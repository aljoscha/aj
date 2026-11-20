//! A session's threads: the log's cache of how the store files its entries.
//!
//! A session's entries form a tree through their parent links. A thread is
//! one line of that tree: the main thread, a branch of it, or a
//! sub-agent's run. A thread that does not start the session points at the
//! entry it continues from, in another thread, and holds none of that
//! thread's entries itself. Reading a branch is reading its thread, then its
//! parent thread up to the fork point, and so on to the root.
//!
//! The session's meta entries (the system prompt, what it was assembled
//! from) are filed in the root thread, which the system prompt starts. They
//! belong to every branch, wherever it leaves the root thread.
//!
//! The store decides which thread an entry belongs to, in the
//! transaction that appends it, and answers where the entry landed.
//! [`Threads`] is the log's cache of that: [`Threads::land`] files an entry
//! from the store's answer.
//!
//! A thread is created for its first input and may hold no entry
//! yet. Its thread is empty: it ends at the entry it continues from, and
//! no reader takes that entry for one of its own.
//!
//! The entries of a draft, a session or a branch the store does not have
//! yet, have no store to ask. They are filed by [`Threads::place`], which
//! states the store's rule in memory. When the draft is created the store
//! files the same entries by its own rule and the drafts' filing is thrown
//! away (see [`crate::log::SessionLog::append`]).

use std::collections::HashMap;

use crate::log::{EntryId, LogEntryKind, ThreadKind};
use crate::sqlite::{Landed, Landing, SqliteError, ThreadId, ThreadRow};

/// Whose entries a thread carries: the main agent's or one sub-agent's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    Main,
    Sub(usize),
}

impl Owner {
    pub(crate) fn of(thread: ThreadKind, agent_id: Option<usize>) -> Self {
        match (thread, agent_id) {
            (ThreadKind::Sub, Some(n)) => Self::Sub(n),
            // A sub-agent entry without its number is refused before it is
            // filed, so this arm sees the main thread and the meta entries
            // that start it.
            _ => Self::Main,
        }
    }
}

/// A thread an entry starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Start {
    pub owner: Owner,
    /// The entry it continues from: its thread and position.
    pub parent: Option<(usize, u64)>,
}

/// Where [`Threads::place`] files an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Filing {
    /// The entry's thread. For an entry that starts one, the place the
    /// thread takes once the entry is filed.
    pub thread: usize,
    pub starts: Option<Start>,
    /// A meta entry filed in a thread that exists. It does not become the
    /// thread's last entry, so the entry before it can still be continued.
    pub beside: bool,
}

/// What a reader choosing entries needs to know of one, without its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// A message that is not a tool's output. A page of history starts at
    /// one of these.
    Message,
    /// A tool result or nested tool activity.
    ToolOutput,
    /// A state entry: settings, a compaction, a sub-agent's spawn, the
    /// system prompt.
    State,
}

impl Class {
    pub(crate) fn of(entry: &LogEntryKind) -> Self {
        entry.message_type().map_or(Self::State, Self::of_type)
    }

    /// The class of a stored row type (see
    /// [`LogEntryKind::message_type`]).
    pub(crate) fn of_type(kind: &str) -> Self {
        if TOOL_OUTPUT_TYPES.contains(&kind) {
            Self::ToolOutput
        } else if MESSAGE_TYPES.contains(&kind) {
            Self::Message
        } else {
            Self::State
        }
    }
}

/// The row types of [`Class::Message`].
pub(crate) const MESSAGE_TYPES: &[&str] =
    &["user", "assistant", "task_notification", "internal_context"];

/// The row types of [`Class::ToolOutput`].
pub(crate) const TOOL_OUTPUT_TYPES: &[&str] = &["tool_result", "tool_activity"];

/// The row types of [`Class::State`]: the serialized name of every
/// [`LogEntryKind`] but `Message`. The store selects state entries
/// by this list and refuses to file one it does not name, so a new kind is
/// added here with its variant. The order is that of the `messages_state`
/// index, which serves a query only while the two lists are the same.
pub(crate) const STATE_TYPES: &[&str] = &[
    "system_prompt",
    "context",
    "model_change",
    "oracle_model_change",
    "oracle_thinking_change",
    "oracle_speed_change",
    "oracle_verbosity_change",
    "account_change",
    "thinking_change",
    "speed_change",
    "verbosity_change",
    "env_change",
    "goal_change",
    "code_mode_store",
    "sub_agent_spawn",
    "sub_agent_end",
    "compaction_failed",
    "compaction",
];

/// One line of a session's entries.
#[derive(Clone, Debug)]
pub(crate) struct Thread {
    pub owner: Owner,
    /// The entry this thread continues from: its thread and position.
    pub parent: Option<(usize, u64)>,
    /// Position and id of the thread's last entry, not counting
    /// the meta entries filed beside it. An empty thread names the entry
    /// it continues from here, and the session's first names none, at
    /// position `0`.
    pub tip: u64,
    pub tip_id: EntryId,
    /// The positions `lo..=hi` at which the log holds every message of
    /// this thread, `None` when it holds none. A log that holds a whole
    /// session holds every thread from `0`.
    pub held: Option<(u64, u64)>,
    /// The id the store keeps this thread under, `None` for a
    /// thread of draft entries.
    pub id: Option<ThreadId>,
}

impl Thread {
    /// Whether the thread holds no entry of its own: it was created and
    /// no entry has landed in it.
    pub(crate) fn is_empty(&self) -> bool {
        self.tip == self.parent.map_or(0, |(_, at)| at)
    }
}

/// A session's threads, in the order they started. A thread is known here
/// by its place in that order.
#[derive(Clone, Debug, Default)]
pub(crate) struct Threads {
    threads: Vec<Thread>,
    /// Each stored thread's place in `threads`, by its id.
    stored: HashMap<ThreadId, usize>,
}

impl Threads {
    /// The threads the store holds for a session, which arrive in the
    /// order they started, holding none of their entries yet.
    pub(crate) fn stored(rows: Vec<ThreadRow>) -> Result<Self, SqliteError> {
        let mut threads = Self::default();
        for row in rows {
            let parent = match row.parent {
                Some((thread_id, at)) => {
                    let thread = threads.thread_of(thread_id).ok_or_else(|| {
                        SqliteError::Invalid(format!(
                            "thread {} continues from one that starts after it",
                            row.id
                        ))
                    })?;
                    Some((thread, at))
                }
                None => None,
            };
            let index = threads.threads.len();
            threads.stored.insert(row.id, index);
            threads.threads.push(Thread {
                owner: row.owner,
                parent,
                tip: row.last_seq,
                tip_id: row.last_entry_id,
                held: None,
                id: Some(row.id),
            });
        }
        Ok(threads)
    }

    /// Where a draft entry of `kind` goes, `agent_id` naming its sub-agent,
    /// in a session or branch the store does not hold yet. `parent` is where its
    /// parent sits, `None` for an entry without one. This is the store's
    /// rule (see the module docs), and a stored entry is never filed by it.
    ///
    /// A meta entry is filed beside the entries of the root thread, the
    /// one the session's first entry starts. Any other entry continues its
    /// parent's thread when it has the same owner and the parent is that
    /// thread's last entry. Otherwise it starts a thread that continues
    /// from the parent: a branch or a sub-agent's run.
    ///
    /// The system prompt stays the root thread's last entry until a message
    /// follows it, whatever meta entries are filed beside it. So the first
    /// message after the prompt continues the root thread, and a later one
    /// that starts over at the prompt is a branch.
    pub(crate) fn place(
        &self,
        kind: ThreadKind,
        agent_id: Option<usize>,
        parent: Option<(usize, u64)>,
    ) -> Filing {
        let owner = Owner::of(kind, agent_id);
        let beside = kind == ThreadKind::Meta && !self.threads.is_empty();
        let continued = match parent {
            _ if beside => Some(0),
            Some((index, at)) => {
                let thread = &self.threads[index];
                (thread.owner == owner && thread.tip == at).then_some(index)
            }
            None => None,
        };
        match continued {
            Some(thread) => Filing {
                thread,
                starts: None,
                beside,
            },
            None => Filing {
                thread: self.threads.len(),
                starts: Some(Start { owner, parent }),
                beside,
            },
        }
    }

    /// File the draft entry appended at `seq` where [`Self::place`] put
    /// it. No entry may have been filed since it was placed.
    pub(crate) fn file(&mut self, filing: &Filing, seq: u64, id: &EntryId) {
        match filing.starts {
            None if filing.beside => {}
            None => {
                let thread = &mut self.threads[filing.thread];
                thread.held = Some(match thread.held {
                    // The first entry of a thread is all of it so far.
                    _ if thread.is_empty() => (0, seq),
                    Some((lo, hi)) if hi == thread.tip => (lo, seq),
                    _ => (seq, seq),
                });
                thread.tip = seq;
                thread.tip_id = id.clone();
            }
            Some(start) => {
                assert_eq!(filing.thread, self.threads.len(), "a stale filing");
                self.threads.push(Thread {
                    owner: start.owner,
                    parent: start.parent,
                    tip: seq,
                    tip_id: id.clone(),
                    held: Some((0, seq)),
                    id: None,
                });
            }
        }
    }

    /// File an entry of `owner` where the store says it landed. Returns
    /// its thread. An answer that names a thread this cache does not
    /// know means the cache is not the session's any more.
    pub(crate) fn land(
        &mut self,
        owner: Owner,
        landed: &Landed,
        id: &EntryId,
    ) -> Result<usize, SqliteError> {
        let known = |thread_id: ThreadId| {
            self.thread_of(thread_id).ok_or_else(|| {
                SqliteError::Invalid(format!(
                    "the store filed entry {id:?} under thread {thread_id}, \
                     which the log does not know"
                ))
            })
        };
        let filing = match landed.landing {
            Landing::Continues | Landing::Beside => Filing {
                thread: known(landed.thread)?,
                starts: None,
                beside: landed.landing == Landing::Beside,
            },
            Landing::Starts { from } => {
                let parent = match from {
                    Some((thread_id, at)) => Some((known(thread_id)?, at)),
                    None => None,
                };
                Filing {
                    thread: self.threads.len(),
                    starts: Some(Start { owner, parent }),
                    beside: false,
                }
            }
        };
        self.file(&filing, landed.seq, id);
        if filing.starts.is_some() {
            self.threads[filing.thread].id = Some(landed.thread);
            self.stored.insert(landed.thread, filing.thread);
        }
        Ok(filing.thread)
    }

    /// Add the empty thread the store created as `thread_id`, which
    /// continues from the entry `tip_id` at `from`. Returns the thread.
    pub(crate) fn create(
        &mut self,
        thread_id: ThreadId,
        from: Option<(ThreadId, u64)>,
        tip_id: EntryId,
    ) -> Result<usize, SqliteError> {
        let parent = match from {
            Some((parent, at)) => {
                let thread = self.thread_of(parent).ok_or_else(|| {
                    SqliteError::Invalid(format!(
                        "the store created thread {thread_id} from thread \
                         {parent}, which the log does not know"
                    ))
                })?;
                Some((thread, at))
            }
            None => None,
        };
        let index = self.threads.len();
        self.stored.insert(thread_id, index);
        self.threads.push(Thread {
            owner: Owner::Main,
            parent,
            tip: parent.map_or(0, |(_, at)| at),
            tip_id,
            held: None,
            id: Some(thread_id),
        });
        Ok(index)
    }

    /// Forget the threads of draft entries. They are the newest: nothing
    /// is committed while a draft's own entries are filed.
    pub(crate) fn drop_drafts(&mut self) {
        while self
            .threads
            .last()
            .is_some_and(|thread| thread.id.is_none())
        {
            self.threads.pop();
        }
    }

    /// The place of the thread stored as `thread_id`, `None` for one that
    /// started
    /// after these threads were read.
    pub(crate) fn thread_of(&self, thread_id: ThreadId) -> Option<usize> {
        self.stored.get(&thread_id).copied()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Thread> {
        self.threads.iter()
    }

    pub(crate) fn get(&self, thread: usize) -> Option<&Thread> {
        self.threads.get(thread)
    }

    /// Where the entry `id` sits if it is the last entry of a thread. An
    /// empty thread has no entry, so the one it continues from is not its.
    pub(crate) fn tip(&self, id: &EntryId) -> Option<(usize, u64)> {
        let mut threads = self.threads.iter().enumerate();
        threads.find_map(|(index, thread)| {
            (thread.tip_id == *id && !thread.is_empty()).then_some((index, thread.tip))
        })
    }

    /// Record which messages of each thread the log holds (see
    /// [`Thread::held`]). Threads `held` does not name hold none.
    pub(crate) fn set_held(&mut self, held: impl Fn(usize, &Thread) -> Option<(u64, u64)>) {
        for (index, thread) in self.threads.iter_mut().enumerate() {
            thread.held = held(index, thread);
        }
    }

    /// Record that the log holds every message of `thread`.
    pub(crate) fn hold_whole(&mut self, thread: usize) {
        let thread = &mut self.threads[thread];
        thread.held = Some((0, thread.tip));
    }

    /// The threads a branch ending at `head` runs through, from its own up
    /// to the root, each with its last position on the branch.
    pub(crate) fn ancestry(&self, head: Option<(usize, u64)>) -> Vec<(usize, u64)> {
        let mut chain = Vec::new();
        let mut cursor = head;
        while let Some((thread, upto)) = cursor {
            chain.push((thread, upto));
            cursor = self.threads[thread].parent;
        }
        chain
    }

    /// What a view of the branch ending at `head` shows: that entry's
    /// thread up to it, each thread it continues from up to the fork point,
    /// and the sub-agent runs that start on those. Sibling branches and
    /// their sub-agents are not in it. Without a head it holds nothing.
    ///
    /// A view also shows every meta entry. Those filed after the point
    /// where the branch leaves the root thread are not on the path, so a
    /// reader with the entry in hand asks its kind as well.
    pub(crate) fn path(&self, head: Option<(usize, u64)>) -> Path {
        // The last position of each thread that is on the path, zero for
        // a thread that is not.
        let mut bounds = vec![0; self.threads.len()];
        for (thread, upto) in self.ancestry(head) {
            bounds[thread] = bounds[thread].max(upto);
        }
        // A thread is created after the one it continues from, so one pass
        // in creation order sees each sub-agent run's anchor first. Only
        // sub-agent runs join this way: a sibling branch also starts on
        // the path, and following it would leak that branch in.
        for (index, thread) in self.threads.iter().enumerate() {
            if let (Owner::Sub(_), Some((parent, at))) = (thread.owner, thread.parent)
                && bounds[parent] >= at
            {
                bounds[index] = u64::MAX;
            }
        }
        Path { bounds }
    }
}

/// The entries one branch shows (see [`Threads::path`]).
#[derive(Clone, Debug)]
pub(crate) struct Path {
    bounds: Vec<u64>,
}

impl Path {
    /// Whether the entry of `thread` at position `seq` is on the
    /// path. A thread that started after the path was taken is not.
    pub(crate) fn holds(&self, thread: usize, seq: u64) -> bool {
        self.bounds.get(thread).is_some_and(|bound| *bound >= seq)
    }
}

/// Finds where a page of one agent's history starts, from the entries
/// before the page's end, visited last to first: the nearest message of that
/// agent with at least `limit` entries between it and the end.
///
/// The main agent's pages count what a view of the branch shows: its own
/// entries and every sub-agent's. A sub-agent's pages count only its own
/// entries, which may be far apart. The path holds only the run of a
/// sub-agent number that the branch shows. Another branch may reuse it.
pub(crate) struct PageStart<'a> {
    threads: &'a Threads,
    path: &'a Path,
    owner: Owner,
    limit: u64,
    counted: u64,
}

impl<'a> PageStart<'a> {
    pub(crate) fn new(threads: &'a Threads, path: &'a Path, owner: Owner, limit: u64) -> Self {
        Self {
            threads,
            path,
            owner,
            limit: limit.max(1),
            counted: 0,
        }
    }

    /// Whether the page starts at this entry.
    pub(crate) fn visit(&mut self, seq: u64, thread: usize, class: Class) -> bool {
        if !self.path.holds(thread, seq) {
            return false;
        }
        let owner = self.threads.get(thread).map(|thread| thread.owner);
        if self.owner != Owner::Main && owner != Some(self.owner) {
            return false;
        }
        self.counted += 1;
        self.counted >= self.limit && owner == Some(self.owner) && class == Class::Message
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ThreadKind::{Main, Meta, Sub};

    /// File entries in append order, each given as its kind, sub-agent
    /// number and parent's position. Returns the threads and the thread of
    /// the entry at each position.
    fn filed(
        entries: &[(ThreadKind, Option<usize>, Option<u64>)],
    ) -> (Threads, std::collections::BTreeMap<u64, usize>) {
        let mut threads = Threads::default();
        let mut filed: std::collections::BTreeMap<u64, usize> = Default::default();
        for (seq, (kind, agent, parent)) in (1..).zip(entries.iter().copied()) {
            let filing = threads.place(kind, agent, parent.map(|at| (filed[&at], at)));
            threads.file(&filing, seq, &seq.to_string());
            filed.insert(seq, filing.thread);
        }
        (threads, filed)
    }

    #[test]
    fn the_root_thread_holds_the_meta_records_and_is_continued_past_them() {
        // Positions:   1 prompt, 2 context, 3 u1 (off the prompt), 4 a1,
        //              5 a late meta entry, 6 u2 (off a1),
        //              7 b1 (off the prompt again), 8 b2.
        let (threads, filed) = filed(&[
            (Meta, None, None),
            (Meta, None, Some(1)),
            (Main, None, Some(1)),
            (Main, None, Some(3)),
            (Meta, None, Some(1)),
            (Main, None, Some(4)),
            (Main, None, Some(1)),
            (Main, None, Some(7)),
        ]);
        let of: Vec<usize> = filed.values().copied().collect();
        assert_eq!(of, [0, 0, 0, 0, 0, 0, 1, 1]);
        let runs: Vec<_> = threads.iter().map(|t| (t.owner, t.parent, t.tip)).collect();
        assert_eq!(
            runs,
            [(Owner::Main, None, 6), (Owner::Main, Some((0, 1)), 8)]
        );
    }

    #[test]
    fn a_path_holds_its_branch_and_the_runs_that_start_on_it_only() {
        // Positions:   1 root, 2 u1, 3 a1, 4 sub 1 (off a1), 5 u2,
        //              6 sub 1 again, 7 b1 (branch off u1),
        //              8 another sub 1 (off b1).
        let (threads, filed) = filed(&[
            (Meta, None, None),
            (Main, None, Some(1)),
            (Main, None, Some(2)),
            (Sub, Some(1), Some(3)),
            (Main, None, Some(3)),
            (Sub, Some(1), Some(4)),
            (Main, None, Some(2)),
            (Sub, Some(1), Some(7)),
        ]);
        let shown = |head: u64| -> Vec<u64> {
            let path = threads.path(Some((filed[&head], head)));
            let seqs = filed
                .iter()
                .filter(|(seq, thread)| path.holds(**thread, **seq));
            seqs.map(|(seq, _)| *seq).collect()
        };
        // The first branch shows its own sub-agent 1 and not the other's.
        assert_eq!(shown(5), [1, 2, 3, 4, 5, 6]);
        // The second starts at u1, so nothing after it on the first shows.
        assert_eq!(shown(7), [1, 2, 7, 8]);
        // A head in the middle of a thread cuts the thread there, and a
        // run that starts later is not on the path yet.
        assert_eq!(shown(2), [1, 2]);
        let nothing = threads.path(None);
        assert!(
            filed
                .iter()
                .all(|(seq, thread)| !nothing.holds(*thread, *seq))
        );
    }
}
