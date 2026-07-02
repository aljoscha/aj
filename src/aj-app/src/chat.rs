//! Backend-neutral chat model plus the [`AgentEvent`] reducer.
//!
//! [`ChatState`] is the data model behind an interactive chat view:
//! per-agent transcripts of typed entries, streaming bookkeeping, the
//! background-task table, footer accounting, and the display flags. The
//! [`reduce`] function folds one [`AgentEvent`] into the model, updating
//! the shared [`AgentLifecycle`] sets alongside it. It answers "what is
//! there to render". Turning entries into widgets, layout, styling, and
//! scroll position stay with the consuming view.
//!
//! The reducer preserves the domain rules of `aj`'s imperative event
//! pump, but it mutates data instead of a live component tree, so it is
//! unit-testable with no terminal. Replay emits the same events as a
//! live run (`MessageStart` + `MessageEnd` with no updates for
//! finalized messages, `ToolExecutionEnd` with no `Start`), and the
//! reducer handles both paths, so a resumed session feeds history
//! through the same `reduce` call as live events.
//!
//! [`AgentEvent`]: aj_agent::events::AgentEvent
//! [`AgentLifecycle`]: crate::session::AgentLifecycle

mod model;
mod reducer;

pub use model::{
    AssistantEntry, ChatState, CompactionEntry, Entry, EntryId, EntryKind, NoticeEntry,
    NoticeLevel, SubAgentEntry, SubAgentStatus, TaskInfo, ToolEntry, ToolStatus, Transcript,
    TurnUsageEntry, UserEntry,
};
pub use reducer::{Redraw, reduce};
