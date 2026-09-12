//! Log preview facts, independent of transport and on-disk representation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The previews a batch read answered. A session the host does not have is
/// left out rather than failing the batch. `incomplete` names hosts a gateway
/// could not read, each once.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPreviews {
    pub previews: Vec<SessionPreview>,
    pub incomplete: Vec<crate::HostFailure>,
}

/// A complete best-effort log scan, including archived sessions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPreview {
    pub session_id: String,
    pub modified: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub last_message_at: DateTime<Utc>,
    pub size_bytes: u64,
    /// All message entries, including tool results and messages on other branches.
    pub message_count: usize,
    /// The first user text block verbatim, never truncated for display.
    pub first_user_message: Option<String>,
    pub tag: Option<String>,
    pub archived: bool,
}
