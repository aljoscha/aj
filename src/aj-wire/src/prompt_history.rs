//! User-paced prompt history reads. Gateways merge all-scope replies.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const PROMPT_HISTORY_CAPABILITY: &str = "prompt_history";
pub const PROMPT_HISTORY_LIMIT: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPrompt {
    pub text: String,
    pub project: Option<String>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptHistory {
    pub prompts: Vec<HistoryPrompt>,
    pub incomplete: Vec<crate::HostFailure>,
}
