//! On-disk session state for `aj`.
//!
//! `aj-session` owns the persisted conversation log: an append-only
//! JSONL file per session, with framing for branches and sub-agents.
//! Two layers split the responsibility:
//!
//! - [`log`] owns the in-memory `ConversationLog`, its append API
//!   (`ConversationView`), and the typed entry payload
//!   (`ConversationEntry`, `ConversationEntryKind`). It also exposes
//!   the read-only `Conversation` view used by the wire layer to
//!   build inference requests.
//! - [`persistence`] discovers existing session files in a project
//!   directory (`ConversationPersistence`) and surfaces metadata for
//!   session listing.
//! - [`replay`] projects a persisted log onto the typed
//!   [`aj_agent::events::AgentEvent`] stream so frontends can drive
//!   the same renderer pipeline for both live and resumed sessions.
//! - [`compaction`] is the pure planning library for context
//!   compaction: token estimation, cut-point selection, summary
//!   prompt templates, and file-op extraction over log entries.
//! - [`tree`] projects the log's branch structure onto a
//!   segment-collapsed [`SessionTree`] for the tree-view overlay.
//! - [`lock`] is the advisory single-writer lock a host takes while it
//!   holds a session live.
//! - [`id`] is the session-id grammar, which is what makes an id that
//!   arrives from off-process safe to turn into a path.

/// How many lines a blocking file scan reads between cancellation polls.
///
/// The preview and prompt-history scans run on the blocking pool and
/// can't be aborted by the runtime, so they poll a caller-supplied
/// `cancel` predicate to bail early when the consumer goes away. We poll
/// once per this many lines rather than per line: the check is cheap but
/// not free, and this keeps it off the hot parse path while still
/// bounding the worst-case stall to a few milliseconds of parsing even
/// on a single huge session file.
pub(crate) const SCAN_CANCEL_CHECK_LINES: usize = 1024;

pub mod compaction;
pub mod id;
pub mod listener;
pub mod lock;
pub mod log;
pub mod persistence;
#[cfg(test)]
mod pre_env_codec_fixture;
pub mod prompt_history;
pub mod repair;
pub mod replay;
pub mod stats;
pub mod tag;
mod tool_details;
pub mod tree;

pub use compaction::{
    CompactionDetails, CompactionPlan, ContextEstimate, estimate_context_tokens,
    prepare_compaction, should_compact,
};
pub use id::is_valid_session_id;
pub use listener::{
    AppendHandoff, PersistenceFence, fenced_persisting_forwarder, persistence_listener,
    persisting_forwarder,
};
pub use lock::{LockHolder, LockMetadata, SessionLock};
#[cfg(any(test, feature = "test-support"))]
pub use log::test_support::{AppendFault, AppendFaultFixture};
pub use log::{
    Conversation, ConversationEntry, ConversationEntryKind, ConversationError, ConversationLog,
    EntryId, EntryRef, LogSnapshot, PersistenceFailure, PersistenceFailureSender, SessionEnvError,
    SessionSettings, TailRepair, ThreadFilter, ThreadKind, validate_session_env,
};
pub use persistence::{ConversationPersistence, SessionMetadata, SessionPreview, SidecarMetadata};
pub use prompt_history::{
    PromptEntry, all_workspaces_history, all_workspaces_history_streaming, scan_file_user_prompts,
    workspace_history, workspace_history_streaming,
};
pub use repair::repair_interrupted_tool_uses;
pub use replay::{
    Backfill, TaggedEvent, project_suffix, project_thread, replay, replay_deferring_subs,
};
pub use stats::{SessionStats, UsageBucket};
pub use tag::{MAX_TAG_BYTES, TagError, normalize_tag};
pub use tool_details::resolve_tool_details;
pub use tree::{SessionTree, TreeSegment};
