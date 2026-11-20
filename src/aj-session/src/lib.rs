//! Stored session state for `aj`.
//!
//! `aj-session` owns sessions as they are stored: append-only entries in
//! one SQLite database, with branches and sub-agents. The modules:
//!
//! - [`log`] owns `SessionLog`, the handle of one open session: its
//!   append API (`AgentLog`), the typed entry
//!   (`LogEntry`, `LogEntryKind`), and the read-only
//!   `BranchView` view used to build inference requests. It holds in
//!   memory the part of the session its reads need.
//! - `sqlite` is the store: the schema, its migrations, and every query
//!   and transaction. Nothing outside this crate sees it.
//! - `threads` files entries into threads in memory, by the rule
//!   the store files them by.
//! - [`journal`] is the one writer of a hosted session. It commits an
//!   event's entries before the event is published.
//! - `pending` is the accepted input and the task notifications an agent has
//!   not taken yet, `tasks` the background commands recorded as running,
//!   and `turn` whether an agent's turn was working when its host ended.
//! - [`persistence`] discovers a project's sessions
//!   (`ProjectSessions`) and surfaces metadata for session
//!   listing.
//! - [`replay`] projects a persisted log onto the typed
//!   [`aj_agent::events::AgentEvent`] stream so frontends can drive
//!   the same renderer pipeline for both live and resumed sessions.
//! - [`compaction`] is the pure planning library for context
//!   compaction: token estimation, cut-point selection, summary
//!   prompt templates, and file-op extraction over log entries.
//! - [`tree`] projects the log's branch structure onto a
//!   segment-collapsed [`SessionTree`] for the tree-view overlay.
//! - [`repair`] finds the tool calls an interrupted turn left without a
//!   result, and answers them.
//! - [`stats`] is the digest behind a session-info view, [`prompt_history`]
//!   the prompts a user submitted, and [`tag`] the rules for a label.
//! - [`lock`] is the advisory single-writer lock a host takes while it
//!   holds a session live.
//! - [`id`] is the session-id grammar, which is what makes an id that
//!   arrives from off-process safe to turn into a path.
//! - `jsonl` is the format a session is exported to and imported from.
//!   `legacy` is everything that deals with session files from before the
//!   database that were never imported.

/// How many lines a blocking scan of a session file reads between
/// cancellation polls.
///
/// The preview and prompt-history scans of [`legacy`] run on the blocking
/// pool and can't be aborted by the runtime, so they poll a caller-supplied
/// `cancel` predicate to bail early when the consumer goes away. We poll
/// once per this many lines rather than per line: the check is cheap but
/// not free, and this keeps it off the hot parse path while still
/// bounding the worst-case stall to a few milliseconds of parsing even
/// on a single huge session file.
pub(crate) const SCAN_CANCEL_CHECK_LINES: usize = 1024;

pub mod compaction;
pub mod id;
pub mod journal;
mod jsonl;
mod legacy;
pub mod lock;
pub mod log;
pub mod persistence;
pub mod prompt_history;
pub mod repair;
pub mod replay;
mod sqlite;
pub mod stats;
pub mod tag;
mod tasks;
mod threads;
mod tool_details;
pub mod tree;
mod turn;

pub use compaction::{
    CompactionDetails, CompactionPlan, ContextEstimate, estimate_context_tokens,
    prepare_compaction, should_compact,
};
pub use id::is_valid_session_id;
pub use journal::{Committed, Journal, JournalGuard};
pub use lock::{LockHolder, LockMetadata, SessionLock};
#[cfg(any(test, feature = "test-support"))]
pub use log::test_support::{fail_sqlite_commits, jsonl_session};
pub use log::{
    BranchRead, BranchView, ContextFileRecord, ContextSkillRecord, DISPLAY_TAIL, EntryId, EntryRef,
    Found, LeftBranch, LeftSubAgent, LoadedBranch, LogEntry, LogEntryKind, LogSnapshot,
    PersistenceFailure, PersistenceFailureSender, SessionContext, SessionEnvError, SessionError,
    SessionLog, SessionSettings, ThreadFilter, ThreadKind, WholeRead, validate_session_env,
};
pub use persistence::{ProjectSessions, SessionMetadata, SessionPreview, TagListing};
pub use prompt_history::{PromptEntry, workspace_history, workspace_history_streaming};
pub use repair::{
    answer_tool_call, repair_interrupted_tool_uses, turn_unanswered, unanswered_spawns,
    unanswered_tool_calls,
};
pub use replay::{Backfill, TaggedEvent, project_page, project_suffix, replay};
pub use stats::{SessionStats, UsageBucket};
pub use tag::{MAX_TAG_BYTES, TagError, normalize_tag};
pub use tool_details::{compact_tool_details, resolve_tool_details};
pub use tree::{SessionTree, TreeSegment};

mod pending;
pub use jsonl::TailRepair;
pub use legacy::{MigrationReport, MigrationResult, migrate_all_jsonl};
pub use pending::{Pending, PendingStore};
pub use tasks::{TaskRecord, TaskRecords};
pub use turn::{TurnState, TurnStore};
