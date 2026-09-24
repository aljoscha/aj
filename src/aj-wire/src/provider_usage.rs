//! Provider usage facts and reset actions. No credentials cross this boundary.

use aj_models::usage::{ProviderUsage, RateLimitResetTarget, ResetOutcome};
use serde::{Deserialize, Serialize};

/// One provider account's resolved usage status, ready to render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderUsageStatus {
    pub provider_id: String,
    /// Host display label. Clients use `provider_id` when this is empty.
    #[serde(default)]
    pub provider_name: String,
    /// The exact stored account label. `None` is the effective unlabeled
    /// credential or an unconfigured provider.
    pub account: Option<String>,
    pub outcome: UsageOutcome,
}

/// What the `/usage` page shows for one provider account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UsageOutcome {
    /// Usage numbers were fetched. Render one row per window.
    Usage(ProviderUsage),
    /// Credentials exist but can't report usage (provider-supplied
    /// reason, e.g. "only available with a subscription login").
    Unsupported { reason: String },
    /// No credentials configured for this provider.
    NotConfigured,
    /// No usage source implemented for this provider yet.
    NoSource,
    /// The fetch failed. The message is shown verbatim.
    Error(String),
}

/// Account reports and providers whose host can spend reset credits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderUsageReport {
    pub statuses: Vec<ProviderUsageStatus>,
    pub reset_providers: Vec<String>,
}

/// One confirmed attempt. Retries must carry the same target and key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageResetRequest {
    pub target: RateLimitResetTarget,
    pub idempotency_key: String,
}

/// Provider refusal, separate from transport or session-addressing failures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UsageResetFailure {
    /// The account identity changed. Fetch a new offer instead of retrying.
    StaleTarget,
    /// A provider failure. Retrying retains the original target and key.
    Error(String),
}

/// Provider result, carried independently of HTTP and session-address refusals.
pub type UsageResetResponse = Result<ResetOutcome, UsageResetFailure>;
