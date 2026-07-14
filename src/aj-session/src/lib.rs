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

pub mod compaction;
pub mod listener;
pub mod log;
pub mod persistence;
pub mod prompt_history;
pub mod repair;
pub mod replay;
pub mod stats;
mod tool_details;

pub use compaction::{
    CompactionDetails, CompactionPlan, ContextEstimate, estimate_context_tokens,
    prepare_compaction, should_compact,
};
pub use listener::persistence_listener;
pub use log::{
    Conversation, ConversationEntry, ConversationEntryKind, ConversationError, ConversationLog,
    EntryId, SessionSettings, ThreadFilter, ThreadKind,
};
pub use persistence::{ConversationPersistence, SessionMetadata, SessionPreview};
pub use prompt_history::{
    PromptEntry, all_workspaces_history, all_workspaces_history_streaming, scan_file_user_prompts,
    workspace_history, workspace_history_streaming,
};
pub use repair::repair_interrupted_tool_uses;
pub use replay::{project_thread, replay, replay_deferring_subs};
pub use stats::SessionStats;
pub use tool_details::resolve_tool_details;
