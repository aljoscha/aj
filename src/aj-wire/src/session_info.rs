use std::collections::BTreeMap;
use std::path::PathBuf;

use aj_models::types::Usage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::BranchSettings;

/// Raw session facts returned by `GET /v1/sessions/{id}/info`.
/// Counts and usage span all recorded threads and branches.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    /// Backing JSONL path on the host.
    pub path: PathBuf,
    /// Creation time parsed from the session id, if available.
    pub created_at: Option<DateTime<Utc>>,
    /// Most recent timestamped message, if any.
    pub last_activity: Option<DateTime<Utc>>,
    /// Backing file size, absent when the file does not exist.
    pub size_bytes: Option<u64>,
    pub total_entries: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_results: usize,
    pub tool_calls: usize,
    /// Tool name and count, ordered by count descending then name ascending.
    pub tool_call_counts: Vec<(String, usize)>,
    pub subagents: usize,
    pub compactions: usize,
    /// Total recorded usage, including compaction summaries.
    pub usage: Usage,
    /// Assistant usage only, ordered by cost and tokens descending then key ascending.
    pub usage_breakdown: Vec<UsageBucket>,
    /// The compaction-summary share of total usage.
    pub compaction_usage: Usage,
    /// Compaction entries with recorded usage, including explicitly zero usage.
    pub compactions_with_usage: usize,
    /// Recorded settings at the active user branch head, without runtime defaults.
    pub settings: BranchSettings,
    /// Complete active-user-branch environment. None differs from a recorded empty map.
    pub session_env: Option<BTreeMap<String, String>>,
}

/// Recorded assistant usage for one provider, model, and account key.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageBucket {
    pub provider: String,
    pub model: String,
    /// None is unlabelled, while an empty string identifies the unnamed account.
    pub account: Option<String>,
    pub usage: Usage,
    pub responses: usize,
    /// Responses with tokens but no recorded cost.
    pub unpriced_responses: usize,
}
