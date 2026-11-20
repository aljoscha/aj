//! Whether an agent's turn is running, as its thread stores it.
//!
//! A turn is stored as `working` as it starts, by the journal
//! on the turn's `AgentStart`, and as `idle` when it ends, however it
//! ends, by whoever drives it. A turn that never starts, as when a wake
//! finds nothing to deliver, stores neither. A process that dies in
//! between leaves `working`
//! behind, which is how a later process knows the turn was cut short rather
//! than cancelled or failed. A log that comes to be on such a thread
//! without running its turn reports it as [`TurnState::Interrupted`] (see
//! [`crate::SessionLog::set_head`] and
//! [`crate::SessionLog::pick_up_turn`]). That reading is not stored, so
//! it is made again by every open and every return to the branch until a
//! turn starts there.
//!
//! The main agent's state belongs to the thread it is on. On a
//! draft it has none and no turn runs: accepting the first input creates
//! the thread, and a turn that starts on a draft without one creates
//! it as `working` (see [`crate::SessionLog::start_turn`]).
//!
//! A sub-agent's state belongs to its own thread, its run on the
//! selected branch, which its spawn
//! entry starts. The run that entry opens is a turn, so the thread
//! is stored as `working` with the entry.

use aj_agent::events::AgentId;

use crate::SessionError;

/// The state of an agent's turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TurnState {
    #[default]
    Idle,
    Working,
    /// The turn is stored as working and its process ended before it did.
    Interrupted,
}

impl TurnState {
    /// The stored state: `idle` or `working`. An interrupted turn is the
    /// working turn its process left behind.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working | Self::Interrupted => "working",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "idle" => Some(Self::Idle),
            "working" => Some(Self::Working),
            _ => None,
        }
    }
}

/// One session's turn states, on the session log's connection. The
/// session's owner holds its writer lock for this handle's lifetime.
pub struct TurnStore {
    store: crate::log::SharedStore,
    session_id: String,
}

impl TurnStore {
    pub(crate) fn new(store: crate::log::SharedStore, session_id: String) -> Self {
        Self { store, session_id }
    }

    /// The state of `agent`'s turn. A sub-agent's is that of its run on
    /// the selected branch as this process stored or picked it up, and
    /// idle before either.
    pub fn get(&self, agent: AgentId) -> TurnState {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        match agent {
            AgentId::Main => writer.turn,
            AgentId::Sub(n) => writer.sub_turn(n),
        }
    }

    /// The sub-agents of the selected branch whose turn this process
    /// started and has not ended, in ascending order.
    pub fn working_subs(&self) -> Vec<usize> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        let mut working: Vec<usize> = writer
            .sub_runs
            .keys()
            .copied()
            .filter(|n| writer.sub_turn(*n) == TurnState::Working)
            .collect();
        working.sort_unstable();
        working
    }

    /// Store `turn` for `agent`: `Working` as a turn starts, before its
    /// first entry, and `Idle` as it ends. A refused save leaves the state
    /// as it was, and the caller does not go on. One the database was too
    /// busy to take reports [`SessionError::is_busy`].
    ///
    /// The write is skipped only when this writer knows the store holds
    /// `turn` already: it stored or read that state itself. A sub-agent's
    /// state it has not read is unknown, not idle, so it is written.
    ///
    /// An agent without a thread holds no turn, so a state other
    /// than the idle it is in is an error. A turn of the main agent starts
    /// through [`crate::SessionLog::start_turn`], which gives a draft
    /// its thread.
    pub fn set(&self, agent: AgentId, turn: TurnState) -> Result<(), SessionError> {
        let mut writer = self.store.lock().expect("SQLite mutex poisoned");
        let id = &self.session_id;
        match agent {
            AgentId::Main => {
                // The main agent's state is read whenever the log comes to
                // be on a thread, so this is what the store holds.
                if writer.turn.as_str() == turn.as_str() {
                    writer.turn = turn;
                    return Ok(());
                }
                let Some(thread_id) = writer.main else {
                    let agent = "the main agent".to_owned();
                    return Err(crate::sqlite::SqliteError::NoThread(agent).into());
                };
                writer.writable(id)?.set_turn(id, thread_id, turn)?;
                writer.turn = turn;
            }
            AgentId::Sub(n) => {
                let Some(thread_id) = writer.sub_runs.get(&n).copied() else {
                    if turn == TurnState::Idle {
                        // No run on this branch, so no turn to end.
                        return Ok(());
                    }
                    return Err(crate::sqlite::SqliteError::Invalid(format!(
                        "sub-agent {n} has no run on this branch to hold a turn"
                    ))
                    .into());
                };
                let known = writer.sub_turns.get(&thread_id);
                if known.is_some_and(|known| known.as_str() == turn.as_str()) {
                    writer.sub_turns.insert(thread_id, turn);
                    return Ok(());
                }
                writer.writable(id)?.set_turn(id, thread_id, turn)?;
                writer.sub_turns.insert(thread_id, turn);
            }
        }
        Ok(())
    }
}
