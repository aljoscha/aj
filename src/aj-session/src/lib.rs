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
//!
//! See `docs/aj-next-plan.md` §1, §2.0(a), and §2.5.

pub mod listener;
pub mod log;
pub mod persistence;
pub mod repair;
pub mod replay;

pub use listener::persistence_listener;
pub use log::{
    Conversation, ConversationEntry, ConversationEntryKind, ConversationError, ConversationLog,
    ConversationView, EntryId, ThreadFilter, ThreadKind,
};
pub use persistence::{ConversationPersistence, SessionMetadata, SessionPreview};
pub use repair::repair_interrupted_tool_uses;
pub use replay::replay;
