//! Per-agent pending input: the prompts a host accepted that their agent
//! has not taken yet.
//!
//! Every prompt goes through here, whether its agent is idle or busy. The
//! host saves the input, puts it in the queue, and wakes the agent if it is
//! idle. The agent takes it at a drain point, where it becomes a transcript
//! message. Two kinds differ in *when* a busy agent takes them:
//!
//! - **Steering** is taken mid-turn, right after the current tool batch
//!   and before the next inference.
//! - **Follow-up** is taken at the top of the agent's next turn, which
//!   the host starts with a wake.
//!
//! [`MessageQueues`] is the shared handle, modeled on
//! [`crate::TaskRegistry`]: an `Arc<Mutex<..>>` keyed by [`AgentId`]. The
//! host edits it without the agent lock, and the agent's turn drains it
//! while holding `&mut self`. Every method is `&self` and the lock is never
//! held across an `.await`. The handle carries no event bus. The agent
//! emits [`crate::events::AgentEvent::QueueUpdate`] after it drains, and
//! the host publishes one for an edit that leaves a message waiting behind
//! a running turn. Clients render the pending-message box from those
//! events, never from this handle.
//!
//! Each agent has **one message in one slot** (the "one message, one kind"
//! invariant): an edit coalesces into the single pending entry, text joined
//! by a newline. This holds for an idle agent too. A prompt to an agent
//! that already has a pending message, one restored at an open for
//! example, joins that message and is delivered with it. The `Vec` shape
//! matches the `QueueUpdate` wire contract, and draining loops over it
//! regardless.
//!
//! Each pending message has a stable id that becomes the id of the
//! transcript message it is delivered as. A session store that saves
//! [`MessageQueues::pending`] therefore retires the saved copy in the same
//! commit that appends the message, so a crash between the drain and the
//! append neither loses nor duplicates it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use aj_models::types::{Message, UserContent, UserMessage};

use crate::events::AgentId;
use crate::message::AgentMessage;

/// Which queue a pending message lives in, and thus when the agent
/// delivers it. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingKind {
    /// Delivered at the top of the agent's next turn.
    FollowUp,
    /// Injected mid-turn, right after the current tool batch.
    Steering,
}

/// A pending user message and the id its transcript message will carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingInput {
    pub id: String,
    /// The content of the user message this becomes: its text and any
    /// attachments, in the order they were sent.
    pub content: Vec<UserContent>,
}

impl PendingInput {
    /// A new input under a fresh id.
    pub fn new(content: Vec<UserContent>) -> Self {
        Self {
            id: format!("{:032x}", rand::random::<u128>()),
            content,
        }
    }

    /// The message's text: its text blocks, joined by newlines.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for block in &self.content {
            if let UserContent::Text(part) = block {
                append_line(&mut text, &part.text);
            }
        }
        text
    }

    /// Whether the message carries anything but text.
    pub fn has_attachments(&self) -> bool {
        let is_text = |block: &UserContent| matches!(block, UserContent::Text(_));
        !self.content.iter().all(is_text)
    }

    /// Add `content` to the end of the message. Text that follows text
    /// joins it on a new line, so text-only edits keep one text block.
    fn append(&mut self, content: &[UserContent]) {
        let mut more = content.iter();
        if let (Some(UserContent::Text(last)), Some(UserContent::Text(first))) =
            (self.content.last_mut(), content.first())
        {
            append_line(&mut last.text, &first.text);
            more.next();
        }
        self.content.extend(more.cloned());
    }

    /// The user message this input is delivered as, carrying its id.
    pub fn into_message(self) -> AgentMessage {
        let mut message = AgentMessage::wire(Message::User(UserMessage::new(self.content)));
        message.set_id(self.id);
        message
    }
}

/// One agent's pending context and user messages. An agent has at most one
/// user entry across steering and follow-up. Their `Vec` shape matches the
/// `QueueUpdate` wire contract.
#[derive(Clone, Default)]
struct AgentQueues {
    context: Option<String>,
    steering: Vec<PendingInput>,
    follow_up: Vec<PendingInput>,
}

/// One change to an agent's pending message (see [`MessageQueues::edit`]).
#[derive(Clone, Copy, Debug)]
pub enum QueueEdit<'a> {
    /// Append content without changing the message's kind, creating a
    /// follow-up if nothing is pending. This is a prompt: to a busy agent
    /// it waits for the turn to end, to an idle one for the wake that
    /// follows. Sending more never escalates an existing message to
    /// steering.
    FollowUp(&'a [UserContent]),
    /// Append text and make the message steering.
    Steer(&'a str),
    /// Make a pending follow-up steering, with no new text.
    Promote,
    /// Remove the message.
    Withdraw,
}

impl AgentQueues {
    /// Apply `edit` and return the text it removed.
    fn apply(&mut self, edit: QueueEdit<'_>) -> Option<String> {
        match edit {
            QueueEdit::FollowUp(content) => {
                if let Some(head) = self.steering.first_mut() {
                    head.append(content);
                } else if let Some(head) = self.follow_up.first_mut() {
                    head.append(content);
                } else {
                    self.follow_up.push(PendingInput::new(content.to_vec()));
                }
            }
            QueueEdit::Steer(text) => {
                let mut combined = take_pending(self).unwrap_or_else(|| PendingInput::new(vec![]));
                combined.append(&[UserContent::text(text)]);
                self.steering = vec![combined];
            }
            QueueEdit::Promote => {
                if self.steering.is_empty() && !self.follow_up.is_empty() {
                    self.steering = std::mem::take(&mut self.follow_up);
                }
            }
            QueueEdit::Withdraw => return take_pending(self).map(|input| input.text()),
        }
        None
    }

    /// The pending message with its kind and id, as one coalesced input.
    fn pending(&self) -> Option<(PendingKind, PendingInput)> {
        let (kind, slot) = if self.steering.is_empty() {
            (PendingKind::FollowUp, &self.follow_up)
        } else {
            (PendingKind::Steering, &self.steering)
        };
        let mut input = slot.first()?.clone();
        for more in &slot[1..] {
            input.append(&more.content);
        }
        Some((kind, input))
    }
}

/// Display view of an agent's pending message: its kind and coalesced
/// text. `kind` is `None` when nothing is queued.
#[derive(Clone, Debug, Default)]
pub struct QueueSnapshot {
    pub kind: Option<PendingKind>,
    pub text: String,
}

/// Shared, cheaply-cloneable handle to every agent's steering and
/// follow-up queues, keyed by [`AgentId`]. See the module docs for the
/// ownership and concurrency contract.
#[derive(Clone, Default)]
pub struct MessageQueues {
    inner: Arc<Mutex<HashMap<AgentId, AgentQueues>>>,
}

impl MessageQueues {
    /// Replace or clear the pending context snapshot without creating wakeable
    /// or editable user work. Only the latest snapshot is delivered.
    pub fn set_context(&self, agent: AgentId, text: Option<String>) {
        self.lock().entry(agent).or_default().context = text;
    }

    pub(crate) fn take_context(&self, agent: AgentId) -> Option<String> {
        self.lock().get_mut(&agent).and_then(|q| q.context.take())
    }

    /// Apply `edit` to `agent`'s pending message, and keep the change only
    /// if `commit` accepts it. `commit` is given the pending message before
    /// and after the edit and is where a session saves it: an accepted
    /// message is one that is saved. Returns the text a withdrawal removed.
    ///
    /// `commit` runs under the queue lock, so the agent cannot drain the
    /// message between the save and the change it describes.
    pub fn edit<E>(
        &self,
        agent: AgentId,
        edit: QueueEdit<'_>,
        commit: impl FnOnce(
            Option<&(PendingKind, PendingInput)>,
            Option<&(PendingKind, PendingInput)>,
        ) -> Result<(), E>,
    ) -> Result<Option<String>, E> {
        let mut map = self.lock();
        let q = map.entry(agent).or_default();
        let mut edited = q.clone();
        let taken = edited.apply(edit);
        let (before, after) = (q.pending(), edited.pending());
        if before != after {
            commit(before.as_ref(), after.as_ref())?;
        }
        *q = edited;
        Ok(taken)
    }

    fn edit_unsaved(&self, agent: AgentId, edit: QueueEdit<'_>) -> Option<String> {
        let unsaved = |_: Option<&_>, _: Option<&_>| Ok::<(), std::convert::Infallible>(());
        match self.edit(agent, edit, unsaved) {
            Ok(taken) => taken,
            Err(never) => match never {},
        }
    }

    /// [`QueueEdit::FollowUp`] of plain text for a queue nothing saves.
    pub fn append_follow_up(&self, agent: AgentId, text: &str) {
        self.edit_unsaved(agent, QueueEdit::FollowUp(&[UserContent::text(text)]));
    }

    /// [`QueueEdit::Steer`] for a queue nothing saves.
    pub fn append_steering(&self, agent: AgentId, text: &str) {
        self.edit_unsaved(agent, QueueEdit::Steer(text));
    }

    /// [`QueueEdit::Promote`] for a queue nothing saves.
    pub fn promote(&self, agent: AgentId) {
        self.edit_unsaved(agent, QueueEdit::Promote);
    }

    /// [`QueueEdit::Withdraw`] for a queue nothing saves, returning the
    /// removed text.
    pub fn take_pending(&self, agent: AgentId) -> Option<String> {
        self.edit_unsaved(agent, QueueEdit::Withdraw)
    }

    /// Drop `agent`'s pending user input from memory without discarding
    /// internal context. A saved copy is untouched.
    pub fn clear(&self, agent: AgentId) {
        let _ = self.take_pending(agent);
    }

    /// Take `agent`'s queued steering messages. This is the agent's
    /// mid-turn drain point.
    pub fn drain_steering(&self, agent: AgentId) -> Vec<PendingInput> {
        let mut map = self.lock();
        map.get_mut(&agent)
            .map(|q| std::mem::take(&mut q.steering))
            .unwrap_or_default()
    }

    /// Take `agent`'s queued follow-up messages. This is the drain point
    /// at the top of a turn.
    pub fn drain_follow_up(&self, agent: AgentId) -> Vec<PendingInput> {
        let mut map = self.lock();
        map.get_mut(&agent)
            .map(|q| std::mem::take(&mut q.follow_up))
            .unwrap_or_default()
    }

    /// Whether `agent` has anything queued in either slot. The host's
    /// wake triggers poll this alongside the task-notification queue.
    pub fn has_pending(&self, agent: AgentId) -> bool {
        self.lock()
            .get(&agent)
            .is_some_and(|q| !q.steering.is_empty() || !q.follow_up.is_empty())
    }

    /// Display view of `agent`'s pending message.
    pub fn snapshot(&self, agent: AgentId) -> QueueSnapshot {
        match self.pending(agent) {
            Some((kind, input)) => QueueSnapshot {
                kind: Some(kind),
                text: input.text(),
            },
            None => QueueSnapshot::default(),
        }
    }

    /// `agent`'s pending message with its kind and id, as one coalesced
    /// input. This is the whole durable state of the agent's queue.
    pub fn pending(&self, agent: AgentId) -> Option<(PendingKind, PendingInput)> {
        self.lock().get(&agent)?.pending()
    }

    /// Put back a pending message a previous process had accepted. It
    /// replaces whatever `agent` has queued.
    pub fn restore(&self, agent: AgentId, kind: PendingKind, input: PendingInput) {
        let mut map = self.lock();
        let q = map.entry(agent).or_default();
        (q.steering, q.follow_up) = match kind {
            PendingKind::Steering => (vec![input], Vec::new()),
            PendingKind::FollowUp => (Vec::new(), vec![input]),
        };
    }

    /// Current queues for `agent` as wire user messages, for the
    /// `QueueUpdate` event payload the agent emits after draining.
    pub fn event_messages(&self, agent: AgentId) -> (Vec<AgentMessage>, Vec<AgentMessage>) {
        let map = self.lock();
        let Some(q) = map.get(&agent) else {
            return (Vec::new(), Vec::new());
        };
        (
            to_user_messages(&q.steering),
            to_user_messages(&q.follow_up),
        )
    }

    /// Every agent that has something queued, in unspecified order.
    ///
    /// An agent whose queues were drained keeps its (now empty) map entry,
    /// so this filters on content rather than on presence.
    pub fn queued_agents(&self) -> Vec<AgentId> {
        self.lock()
            .iter()
            .filter(|(_, q)| !q.steering.is_empty() || !q.follow_up.is_empty())
            .map(|(agent, _)| *agent)
            .collect()
    }

    /// Total pending messages across every agent, as
    /// `(steering, follow_up)`.
    pub fn pending_counts(&self) -> (usize, usize) {
        self.lock()
            .values()
            .fold((0, 0), |(steering, follow_up), q| {
                (steering + q.steering.len(), follow_up + q.follow_up.len())
            })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<AgentId, AgentQueues>> {
        self.inner.lock().expect("message queues mutex poisoned")
    }
}

/// Newline-join `text` onto `buf`, leaving `buf` untouched-but-equal
/// to `text` when it was empty.
fn append_line(buf: &mut String, text: &str) {
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(text);
}

/// Take whichever slot holds the pending message as a single joined
/// input, keeping the first message's id, and clear both slots.
fn take_pending(q: &mut AgentQueues) -> Option<PendingInput> {
    let slot = if q.steering.is_empty() {
        std::mem::take(&mut q.follow_up)
    } else {
        std::mem::take(&mut q.steering)
    };
    q.steering.clear();
    q.follow_up.clear();
    let mut inputs = slot.into_iter();
    let mut first = inputs.next()?;
    for more in inputs {
        first.append(&more.content);
    }
    Some(first)
}

fn to_user_messages(inputs: &[PendingInput]) -> Vec<AgentMessage> {
    inputs
        .iter()
        .map(|input| input.clone().into_message())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::AgentMessageKind;

    const MAIN: AgentId = AgentId::Main;

    #[test]
    fn context_is_neither_user_work_nor_editable_input() {
        let q = MessageQueues::default();
        q.set_context(MAIN, Some("hint".into()));
        assert!(!q.has_pending(MAIN));
        assert!(q.queued_agents().is_empty());
        assert_eq!(q.pending_counts(), (0, 0));
        assert!(q.snapshot(MAIN).kind.is_none());
        assert_eq!(q.take_pending(MAIN), None);
        q.append_follow_up(MAIN, "user");
        assert_eq!(q.take_pending(MAIN).as_deref(), Some("user"));
        assert_eq!(q.take_context(MAIN).as_deref(), Some("hint"));
        q.set_context(MAIN, Some("stale".into()));
        q.append_steering(MAIN, "keep");
        q.set_context(MAIN, None);
        assert_eq!(q.take_context(MAIN), None);
        assert_eq!(
            q.drain_steering(MAIN)
                .into_iter()
                .map(|input| input.text())
                .collect::<Vec<_>>(),
            vec!["keep"]
        );
    }

    #[test]
    fn append_follow_up_coalesces_and_keeps_kind() {
        let q = MessageQueues::default();
        q.append_follow_up(MAIN, "first");
        q.append_follow_up(MAIN, "second");
        let snap = q.snapshot(MAIN);
        assert_eq!(snap.kind, Some(PendingKind::FollowUp));
        assert_eq!(snap.text, "first\nsecond");
    }

    #[test]
    fn append_steering_escalates_existing_follow_up() {
        let q = MessageQueues::default();
        q.append_follow_up(MAIN, "first");
        q.append_steering(MAIN, "now urgent");
        let snap = q.snapshot(MAIN);
        assert_eq!(snap.kind, Some(PendingKind::Steering));
        assert_eq!(snap.text, "first\nnow urgent");
        // Follow-up content moved into the steering slot, leaving it empty.
        assert!(q.drain_follow_up(MAIN).is_empty());
    }

    #[test]
    fn enter_after_steering_stays_steering() {
        let q = MessageQueues::default();
        q.append_steering(MAIN, "steer");
        q.append_follow_up(MAIN, "more");
        let snap = q.snapshot(MAIN);
        assert_eq!(snap.kind, Some(PendingKind::Steering));
        assert_eq!(snap.text, "steer\nmore");
    }

    #[test]
    fn promote_moves_follow_up_without_text() {
        let q = MessageQueues::default();
        q.append_follow_up(MAIN, "later");
        q.promote(MAIN);
        let snap = q.snapshot(MAIN);
        assert_eq!(snap.kind, Some(PendingKind::Steering));
        assert_eq!(snap.text, "later");
    }

    #[test]
    fn promote_is_noop_without_pending() {
        let q = MessageQueues::default();
        q.promote(MAIN);
        assert!(!q.has_pending(MAIN));
    }

    #[test]
    fn take_pending_returns_text_and_empties() {
        let q = MessageQueues::default();
        q.append_steering(MAIN, "yank me");
        assert_eq!(q.take_pending(MAIN).as_deref(), Some("yank me"));
        assert!(!q.has_pending(MAIN));
        assert_eq!(q.take_pending(MAIN), None);
    }

    #[test]
    fn drain_takes_only_its_slot() {
        let q = MessageQueues::default();
        q.append_follow_up(MAIN, "f");
        assert!(q.drain_steering(MAIN).is_empty());
        assert_eq!(
            q.drain_follow_up(MAIN)
                .into_iter()
                .map(|input| input.text())
                .collect::<Vec<_>>(),
            vec!["f".to_string()]
        );
        assert!(!q.has_pending(MAIN));
    }

    #[test]
    fn queues_are_isolated_per_agent() {
        let q = MessageQueues::default();
        q.append_follow_up(MAIN, "main msg");
        q.append_steering(AgentId::Sub(1), "sub msg");
        assert_eq!(q.snapshot(MAIN).text, "main msg");
        assert_eq!(q.snapshot(AgentId::Sub(1)).text, "sub msg");
        // Draining one leaves the other intact.
        let _ = q.drain_follow_up(MAIN);
        assert_eq!(q.snapshot(AgentId::Sub(1)).text, "sub msg");
    }

    /// The session-wide reads see every agent's queue, and a drained
    /// agent (whose map entry survives the drain) counts as empty.
    #[test]
    fn session_wide_reads_span_agents_and_ignore_drained_entries() {
        let q = MessageQueues::default();
        assert_eq!(q.pending_counts(), (0, 0));
        assert!(q.queued_agents().is_empty());

        q.append_follow_up(MAIN, "later");
        q.append_steering(AgentId::Sub(1), "now");
        assert_eq!(q.pending_counts(), (1, 1));
        let mut agents = q.queued_agents();
        agents.sort_by_key(|agent| format!("{agent:?}"));
        assert_eq!(agents, vec![MAIN, AgentId::Sub(1)]);

        let _ = q.drain_follow_up(MAIN);
        assert_eq!(q.pending_counts(), (1, 0));
        assert_eq!(q.queued_agents(), vec![AgentId::Sub(1)]);
    }
    /// A prompt keeps its blocks in order. One that joins a pending
    /// message keeps that message's id and kind, and only text that
    /// follows text is joined into one block.
    #[test]
    fn a_prompt_with_an_attachment_joins_the_pending_message() {
        let q = MessageQueues::default();
        let image = UserContent::image("aGk=", "image/png");
        q.append_steering(MAIN, "restored");
        let id = q.pending(MAIN).unwrap().1.id;
        let prompt = [UserContent::text("look"), image.clone()];
        q.edit_unsaved(MAIN, QueueEdit::FollowUp(&prompt));
        q.append_follow_up(MAIN, "and this");

        let (kind, input) = q.pending(MAIN).unwrap();
        assert_eq!((kind, input.id.as_str()), (PendingKind::Steering, &*id));
        let expected = [
            UserContent::text("restored\nlook"),
            image,
            UserContent::text("and this"),
        ];
        assert_eq!(input.content, expected);
        assert!(input.has_attachments());
        assert_eq!(input.text(), "restored\nlook\nand this");
        let delivered = q.drain_steering(MAIN).remove(0).into_message();
        let AgentMessageKind::Wire(Message::User(user)) = &delivered.kind else {
            panic!("a user message");
        };
        assert_eq!(user.content, expected);
    }

    #[test]
    fn a_pending_message_keeps_its_id_until_it_is_delivered_under_it() {
        let q = MessageQueues::default();
        q.append_follow_up(MAIN, "one");
        let id = q.pending(MAIN).unwrap().1.id;
        q.append_follow_up(MAIN, "two");
        q.append_steering(MAIN, "three");
        let (kind, input) = q.pending(MAIN).unwrap();
        assert_eq!(kind, PendingKind::Steering);
        assert_eq!(
            (input.id.as_str(), input.text().as_str()),
            (id.as_str(), "one\ntwo\nthree")
        );
        assert_eq!(input.content.len(), 1, "text edits keep one text block");

        // A restored message is delivered under the id it was saved with.
        let restored = MessageQueues::default();
        restored.restore(MAIN, kind, input);
        let delivered = restored.drain_steering(MAIN).remove(0).into_message();
        assert_eq!(delivered.id(), id);
        assert!(restored.pending(MAIN).is_none());
    }
}
