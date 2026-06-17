//! Per-agent pending-message queues: steering and follow-up.
//!
//! While an agent is mid-turn the user can line up another message.
//! Two queues hold these, distinguished by *when* the agent delivers
//! them:
//!
//! - **Steering** is injected mid-turn, right after the current tool
//!   batch and before the next inference (the urgent path).
//! - **Follow-up** is delivered once the turn would otherwise end, via
//!   the binary's wake mechanism.
//!
//! [`MessageQueues`] is the shared handle, modeled on
//! [`crate::TaskRegistry`]: an `Arc<Mutex<..>>` keyed by [`AgentId`],
//! mutated off the agent lock (the TUI input thread enqueues; the
//! agent's turn driver drains while holding `&mut self`). Every method
//! is `&self` and the lock is never held across an `.await`. The
//! handle carries no event bus: the agent emits
//! [`crate::events::AgentEvent::QueueUpdate`] after it drains, and the
//! TUI re-syncs its own view directly when it enqueues, so a stale
//! event can never clobber newer state.
//!
//! The TUI keeps each agent to **one message in one slot** (the "one
//! message, one kind" invariant): `append_follow_up` and
//! `append_steering` coalesce by newline-joining into the single
//! pending entry. The `Vec` shape is retained to match the
//! `QueueUpdate` wire contract and to leave room for other producers;
//! draining loops over it regardless.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use aj_models::types::{Message, UserMessage};

use crate::events::AgentId;
use crate::message::AgentMessage;

/// Which queue a pending message lives in, and thus when the agent
/// delivers it. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingKind {
    /// Delivered when the turn would otherwise end (the wake path).
    FollowUp,
    /// Injected mid-turn, right after the current tool batch.
    Steering,
}

/// One agent's pending messages. The TUI keeps at most one entry
/// across both slots; the `Vec` shape matches the `QueueUpdate` wire
/// contract.
#[derive(Default)]
struct AgentQueues {
    steering: Vec<String>,
    follow_up: Vec<String>,
}

/// Display view of an agent's pending message for the TUI box: its
/// kind and coalesced text. `kind` is `None` when nothing is queued.
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
    /// Append `text` to `agent`'s pending message without changing its
    /// kind, creating a follow-up if nothing is pending. The plain
    /// Enter path: queueing more never escalates an existing message
    /// to steering.
    pub fn append_follow_up(&self, agent: AgentId, text: &str) {
        let mut map = self.lock();
        let q = map.entry(agent).or_default();
        if let Some(head) = q.steering.first_mut() {
            append_line(head, text);
        } else if let Some(head) = q.follow_up.first_mut() {
            append_line(head, text);
        } else {
            q.follow_up.push(text.to_string());
        }
    }

    /// Append `text` to `agent`'s pending message and make it steering,
    /// folding any pending follow-up content into the steering slot
    /// first. The Alt+Enter-with-text path.
    pub fn append_steering(&self, agent: AgentId, text: &str) {
        let mut map = self.lock();
        let q = map.entry(agent).or_default();
        let mut combined = take_pending_text(q);
        append_line(&mut combined, text);
        q.steering = vec![combined];
    }

    /// Promote a pending follow-up to steering with no new text (the
    /// empty-editor Alt+Enter path). No-op when nothing is pending or
    /// it is already steering.
    pub fn promote(&self, agent: AgentId) {
        let mut map = self.lock();
        let Some(q) = map.get_mut(&agent) else {
            return;
        };
        if q.steering.is_empty() && !q.follow_up.is_empty() {
            q.steering = std::mem::take(&mut q.follow_up);
        }
    }

    /// Remove and return `agent`'s pending message text regardless of
    /// kind (the yank). `None` when nothing is pending.
    pub fn take_pending(&self, agent: AgentId) -> Option<String> {
        let mut map = self.lock();
        let q = map.get_mut(&agent)?;
        let text = take_pending_text(q);
        if text.is_empty() { None } else { Some(text) }
    }

    /// Drop `agent`'s pending message without returning it.
    pub fn clear(&self, agent: AgentId) {
        self.lock().remove(&agent);
    }

    /// Take `agent`'s queued steering messages — the agent's mid-turn
    /// drain point.
    pub fn drain_steering(&self, agent: AgentId) -> Vec<String> {
        let mut map = self.lock();
        map.get_mut(&agent)
            .map(|q| std::mem::take(&mut q.steering))
            .unwrap_or_default()
    }

    /// Take `agent`'s queued follow-up messages — the agent's
    /// would-stop (wake) drain point.
    pub fn drain_follow_up(&self, agent: AgentId) -> Vec<String> {
        let mut map = self.lock();
        map.get_mut(&agent)
            .map(|q| std::mem::take(&mut q.follow_up))
            .unwrap_or_default()
    }

    /// Whether `agent` has anything queued in either slot. The binary's
    /// wake triggers poll this alongside the task-notice queue.
    pub fn has_pending(&self, agent: AgentId) -> bool {
        self.lock()
            .get(&agent)
            .is_some_and(|q| !q.steering.is_empty() || !q.follow_up.is_empty())
    }

    /// Display view of `agent`'s pending message for the TUI box.
    pub fn snapshot(&self, agent: AgentId) -> QueueSnapshot {
        let map = self.lock();
        let Some(q) = map.get(&agent) else {
            return QueueSnapshot::default();
        };
        if !q.steering.is_empty() {
            QueueSnapshot {
                kind: Some(PendingKind::Steering),
                text: q.steering.join("\n"),
            }
        } else if !q.follow_up.is_empty() {
            QueueSnapshot {
                kind: Some(PendingKind::FollowUp),
                text: q.follow_up.join("\n"),
            }
        } else {
            QueueSnapshot::default()
        }
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
/// string and clear both slots. Empty string when nothing is pending.
fn take_pending_text(q: &mut AgentQueues) -> String {
    let text = if !q.steering.is_empty() {
        q.steering.join("\n")
    } else {
        q.follow_up.join("\n")
    };
    q.steering.clear();
    q.follow_up.clear();
    text
}

fn to_user_messages(texts: &[String]) -> Vec<AgentMessage> {
    texts
        .iter()
        .map(|t| AgentMessage::wire(Message::User(UserMessage::text(t.clone()))))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: AgentId = AgentId::Main;

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
        assert_eq!(q.drain_follow_up(MAIN), vec!["f".to_string()]);
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
}
