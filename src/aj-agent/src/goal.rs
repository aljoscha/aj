//! Goal data and the application-owned capability exposed to tools.
//!
//! The runtime neither persists goals nor schedules their continuations.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    BudgetLimited,
    UsageLimited,
    Complete,
}

impl GoalStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::BudgetLimited => "budget limited",
            Self::UsageLimited => "usage limited",
            Self::Complete => "complete",
        }
    }
}

/// A snapshot on the user branch. Usage belongs to this line of work, not
/// to the session-wide spending ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<u64>,
    pub tokens_used: u64,
    pub time_used_seconds: u64,
}

impl Goal {
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.token_budget
            .map(|budget| budget.saturating_sub(self.tokens_used))
    }
}

/// User-facing operations. Model tools expose only Get, Create, Complete,
/// Block and Pause. Only the user can replace a goal, change its budget,
/// edit its objective or resume pursuit.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalAction {
    Get,
    Create {
        objective: String,
        token_budget: Option<u64>,
    },
    Edit {
        objective: String,
    },
    SetBudget {
        token_budget: Option<u64>,
    },
    Replace {
        objective: String,
        token_budget: Option<u64>,
    },
    Pause,
    Resume,
    Clear,
    Complete,
    Block,
}

/// A user command, optionally addressed to the goal the user was viewing.
/// A mismatched identity refuses the whole command, including replacement.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalRequest {
    pub action: GoalAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_goal_id: Option<String>,
}

impl GoalRequest {
    pub fn for_goal(goal_id: impl Into<String>, action: GoalAction) -> Self {
        Self {
            action,
            expected_goal_id: Some(goal_id.into()),
        }
    }
}

impl From<GoalAction> for GoalRequest {
    fn from(action: GoalAction) -> Self {
        Self {
            action,
            expected_goal_id: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GoalError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error(
        "goal pursuit requires an interactive session host and is unavailable in print mode or subagents"
    )]
    Unsupported,
    #[error("goal operation failed: {0}")]
    Storage(String),
}

pub type GoalFuture = Pin<Box<dyn Future<Output = Result<Option<Goal>, GoalError>> + Send>>;
/// The turn token fences interruption. The revision identifies the goal context
/// sampled by the originating inference, so live edits cannot accept stale tools.
pub type GoalControl =
    Arc<dyn Fn(GoalAction, tokio_util::sync::CancellationToken, u64) -> GoalFuture + Send + Sync>;

/// Admit Main inference before provider work. The host settles preceding usage,
/// records its goal owner, and returns the revision of queued goal context.
/// Admission and user goal mutations must share the same serialization boundary.
pub type GoalAdmission = Arc<
    dyn Fn(
            tokio_util::sync::CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<u64, GoalError>> + Send>>
        + Send
        + Sync,
>;
