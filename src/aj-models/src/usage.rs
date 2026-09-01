//! Provider-agnostic plan-usage reporting.
//!
//! A [`UsageSource`] knows how to fetch account-level usage numbers
//! (rate-limit windows like "current session" or "current week") for
//! one provider and one optional labeled account, resolving credentials
//! through [`AuthStorage`]. The binary's `/usage` page walks
//! [`default_usage_sources`] and renders every report on one page, so
//! adding usage display for a new provider means implementing the
//! trait and appending it to the default list — no UI changes.
//!
//! The report model is deliberately generic: windows are labeled
//! rows, not provider-specific enums, so each source maps its
//! provider's concepts (Anthropic's `five_hour`/`seven_day`, another
//! provider's primary/secondary windows) to human-readable labels
//! itself.

use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::auth::{AuthCredential, AuthError, AuthStorage};

/// One rate-limit window, ready to render.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    /// Human-readable window name, e.g. "5h limit".
    pub label: String,
    /// Fraction of the window used, `0.0..=1.0`.
    pub used: f64,
    /// When the window resets, unix milliseconds. `None` when the
    /// provider doesn't report a reset time.
    pub resets_at: Option<i64>,
}

/// A provider's full usage report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderUsage {
    /// Rate-limit windows in the source's preferred display order.
    pub windows: Vec<UsageWindow>,
    /// Free-form extra lines, e.g. a usage-credit balance.
    pub notes: Vec<String>,
    /// Earned rate-limit reset credits and the authorization target issued by
    /// the same report. `None` when the provider has no reset mechanism.
    pub reset_offer: Option<ResetCreditOffer>,
}

/// An account's available reset credits coupled to the exact report target
/// required to spend one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetCreditOffer {
    available: u32,
    target: ResetCreditTarget,
}

impl ResetCreditOffer {
    pub fn new(available: u32, target: ResetCreditTarget) -> Self {
        Self { available, target }
    }

    pub fn available(&self) -> u32 {
        self.available
    }

    pub fn target(&self) -> &ResetCreditTarget {
        &self.target
    }
}

/// The exact provider account and provider-defined authorization identity
/// attached to one reset-credit report.
///
/// The authorization identity is deliberately private. Callers carry this
/// value back to [`RateLimitResetSource`] as one unit rather than assembling
/// optional account and identity strings at the destructive boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetCreditTarget {
    provider_id: String,
    account: Option<String>,
    authorization_identity: String,
}

impl ResetCreditTarget {
    /// Build a target from the provider response and exact stored account that
    /// produced it.
    pub fn new(
        provider_id: impl Into<String>,
        account: Option<String>,
        authorization_identity: impl Into<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            account,
            authorization_identity: authorization_identity.into(),
        }
    }

    /// Provider whose reset source owns this target.
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Exact raw stored account label, or `None` for a bare credential.
    pub fn account(&self) -> Option<&str> {
        self.account.as_deref()
    }

    fn authorization_identity(&self) -> &str {
        &self.authorization_identity
    }

    fn changed_error(&self) -> UsageError {
        UsageError::ResetTargetChanged {
            provider_id: self.provider_id.clone(),
            account: self.account.clone(),
        }
    }
}

/// Outcome of asking one source for usage numbers.
#[derive(Debug, Clone, PartialEq)]
pub enum UsageReport {
    /// Usage numbers were fetched successfully.
    Usage(ProviderUsage),
    /// Credentials exist but can't report usage — e.g. a plain API
    /// key where the provider only exposes usage to subscription
    /// accounts. `reason` is shown to the user verbatim.
    Unsupported { reason: String },
    /// No credentials configured for this provider.
    NotConfigured,
}

/// Errors from fetching a usage report.
#[derive(Debug, Error)]
pub enum UsageError {
    /// Credential resolution failed (corrupt `auth.json`, OAuth
    /// refresh failure, ...).
    #[error("auth error: {0}")]
    Auth(#[from] AuthError),
    /// The usage request itself failed (network, HTTP error, or an
    /// unparseable response).
    #[error("{0}")]
    Fetch(String),
    /// The account selected from a usage report is absent, no longer a
    /// subscription, or now names a different upstream subscription.
    #[error("the selected account for provider {provider_id} changed since usage was fetched")]
    ResetTargetChanged {
        provider_id: String,
        account: Option<String>,
    },
}

/// One labeled account from the credential-store snapshot taken when the
/// usage page opened.
///
/// The snapshot lets a fresh sibling resolve without waiting behind another
/// account's stalled OAuth refresh. Expired OAuth credentials still re-enter
/// the store's locked exact-account refresh path.
#[derive(Clone, Copy)]
pub struct UsageAccount<'a> {
    /// User-typed label that identifies the stored slot and rendered row.
    label: &'a str,
    /// Credential captured beside `label` in the same locked store read.
    credential: &'a AuthCredential,
}

impl<'a> UsageAccount<'a> {
    /// Build the account view from one `(label, credential)` pair returned by
    /// [`AuthStorage::accounts`]. Collection lives in `aj-app`, so this is
    /// public across the crate boundary while the credential stays opaque to
    /// usage-source implementations.
    #[doc(hidden)]
    pub fn from_store_snapshot(label: &'a str, credential: &'a AuthCredential) -> Self {
        Self { label, credential }
    }

    /// User-typed account label carried into the rendered status row.
    pub fn label(self) -> &'a str {
        self.label
    }

    /// Resolve this exact snapshot account to the bearer token a usage source
    /// should send. The token is secret and must never be logged or rendered.
    pub async fn resolve_key(
        self,
        auth: &AuthStorage,
        provider_id: &str,
    ) -> Result<Option<String>, UsageError> {
        Ok(auth
            .get_account_api_key_from_snapshot(provider_id, self.label, self.credential)
            .await?
            .map(|resolved| resolved.key))
    }
}

/// A per-provider usage fetcher.
#[async_trait]
pub trait UsageSource: Send + Sync {
    /// Provider id this source reports on, matching the ids used by
    /// [`AuthStorage`] (e.g. `"anthropic"`).
    fn provider_id(&self) -> &str;

    /// Fetch the current usage report for `account`, resolving credentials
    /// through `auth` including OAuth refresh. `None` preserves the bare,
    /// store-default, runtime-override, and environment behavior used before
    /// labeled account sets existed. `Some` resolves exactly that account
    /// from the page-opening store snapshot and cannot be shadowed by
    /// process-level credential sources.
    async fn fetch(
        &self,
        auth: &AuthStorage,
        account: Option<UsageAccount<'_>>,
    ) -> Result<UsageReport, UsageError>;
}

/// Outcome of spending one earned rate-limit reset credit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResetOutcome {
    /// A credit was consumed and the eligible windows were reset.
    Reset,
    /// The idempotency key already completed a reset. Idempotent
    /// success: treat it like [`ResetOutcome::Reset`].
    AlreadyRedeemed,
    /// No window currently needs a reset, so nothing was spent.
    NothingToReset,
    /// The account has no earned reset credits available.
    NoCredit,
}

/// A provider capability: spend an earned credit to clear the current
/// rate-limit windows before their scheduled reset.
///
/// Kept separate from [`UsageSource`] so read-only usage reporting
/// stays provider-agnostic. Only providers that expose reset credits in
/// [`ProviderUsage::reset_offer`] implement this, and the UI pairs
/// the two by provider id: it offers the action for a provider whose
/// report shows credits available and that has a source here.
#[async_trait]
pub trait RateLimitResetSource: Send + Sync {
    /// Provider id this source acts on, matching the ids used by
    /// [`AuthStorage`] and [`UsageSource::provider_id`].
    fn provider_id(&self) -> &str;

    /// Spend one earned reset credit for the exact target issued with a usage
    /// report. The source re-resolves the selected account and verifies its
    /// provider-defined authorization identity before sending a request.
    ///
    /// `idempotency_key` de-duplicates retries of one logical attempt:
    /// callers pass a fresh key per attempt and reuse it when retrying
    /// that same attempt, so a network retry can't double-spend.
    async fn consume_reset_credit(
        &self,
        auth: &AuthStorage,
        target: &ResetCreditTarget,
        idempotency_key: &str,
    ) -> Result<ResetOutcome, UsageError>;
}

/// Resolve the credential one usage row must query.
///
/// A labeled row is an inventory view over `auth.json`, so a runtime
/// `--api-key` or environment variable must not make several account rows
/// query one unrelated credential. Bare and unconfigured providers retain
/// the ordinary resolution chain.
async fn resolve_usage_key(
    auth: &AuthStorage,
    provider_id: &str,
    account: Option<UsageAccount<'_>>,
) -> Result<Option<String>, UsageError> {
    match account {
        Some(account) => account.resolve_key(auth, provider_id).await,
        None => Ok(auth
            .get_api_key(provider_id, None)
            .await?
            .map(|resolved| resolved.key)),
    }
}

/// Resolve a reset action's selected account from current store state. Reset
/// happens after the usage snapshot was rendered, so it deliberately re-reads
/// the selected slot rather than spending against a stale credential.
async fn resolve_selected_usage_key(
    auth: &AuthStorage,
    provider_id: &str,
    account: Option<&str>,
) -> Result<Option<String>, UsageError> {
    let resolved = match account {
        Some(account) => auth.get_account_api_key(provider_id, account).await?,
        None => auth.get_api_key(provider_id, None).await?,
    };
    Ok(resolved.map(|resolved| resolved.key))
}

/// Usage sources shipped out of the box: Anthropic (Claude
/// Pro/Max) and OpenAI Codex (ChatGPT subscription).
pub fn default_usage_sources() -> Vec<Arc<dyn UsageSource>> {
    vec![
        Arc::new(anthropic::AnthropicUsageSource),
        Arc::new(codex::OpenAICodexUsageSource::new()),
    ]
}

/// Rate-limit reset sources shipped out of the box. Only OpenAI Codex
/// (ChatGPT subscription) exposes earned reset credits today, so it is
/// the sole entry. The list is the discovery point the UI walks to find
/// which providers can spend a credit.
pub fn default_reset_sources() -> Vec<Arc<dyn RateLimitResetSource>> {
    vec![Arc::new(codex::OpenAICodexUsageSource::new())]
}

/// Concrete, provider-independent label for a usage window of the
/// given length in minutes, e.g. `"5h limit"` or `"Weekly limit"`.
///
/// Anthropic and Codex both expose 5-hour and weekly rolling windows,
/// so deriving the label from the window length keeps the two
/// providers' rows reading identically in the same overlay. We match
/// each familiar bucket with a 5% tolerance to absorb servers that
/// report e.g. 4h59m or 6d23h. Lengths that don't map to a known
/// bucket (or an unknown length) fall back to a generic `"Usage
/// limit"`.
pub fn window_label(window_minutes: Option<i64>) -> String {
    const HOUR: i64 = 60;
    const DAY: i64 = 24 * HOUR;
    const BUCKETS: &[(i64, &str)] = &[
        (5 * HOUR, "5h limit"),
        (DAY, "Daily limit"),
        (7 * DAY, "Weekly limit"),
        (30 * DAY, "Monthly limit"),
        (365 * DAY, "Annual limit"),
    ];

    let Some(minutes) = window_minutes.filter(|m| *m > 0) else {
        return "Usage limit".to_string();
    };
    for (expected, label) in BUCKETS {
        // Integer 5% tolerance band, no float conversion needed.
        let lower = expected * 95 / 100;
        let upper = expected * 105 / 100;
        if (lower..=upper).contains(&minutes) {
            return (*label).to_string();
        }
    }
    "Usage limit".to_string()
}

pub mod anthropic {
    //! Usage source for Anthropic Claude.ai subscription accounts.

    use anthropic_sdk::client::Client;
    use anthropic_sdk::usage::{
        OAuthExtraUsage, OAuthMoney, OAuthSpend, OAuthUsage, OAuthUsageLimit, OAuthUsageWindow,
    };
    use async_trait::async_trait;
    use chrono::DateTime;

    use super::{ProviderUsage, UsageError, UsageReport, UsageSource, UsageWindow};
    use crate::auth::AuthStorage;

    /// Reports plan rate-limit utilization via the Claude.ai
    /// `GET /api/oauth/usage` endpoint. Only subscription (OAuth)
    /// credentials can query it; API keys report
    /// [`UsageReport::Unsupported`].
    pub struct AnthropicUsageSource;

    #[async_trait]
    impl UsageSource for AnthropicUsageSource {
        fn provider_id(&self) -> &str {
            "anthropic"
        }

        async fn fetch(
            &self,
            auth: &AuthStorage,
            account: Option<super::UsageAccount<'_>>,
        ) -> Result<UsageReport, UsageError> {
            let Some(key) = super::resolve_usage_key(auth, self.provider_id(), account).await?
            else {
                return Ok(UsageReport::NotConfigured);
            };
            // Same OAuth-token sniff the SDK client uses to pick its
            // auth mode; anything else is a plain API key, which the
            // usage endpoint rejects.
            if !key.starts_with("sk-ant-oat") {
                return Ok(UsageReport::Unsupported {
                    reason: "only available with a subscription login (API key configured)"
                        .to_string(),
                });
            }

            let client = Client::new(None, key);
            let usage = client
                .oauth_usage()
                .await
                .map_err(|err| UsageError::Fetch(err.to_string()))?;
            Ok(UsageReport::Usage(map_usage(&usage)))
        }
    }

    /// Map the wire response to the generic report.
    ///
    /// `limits` is provider-defined and carries scoped windows without
    /// requiring us to know model names in advance. Legacy top-level
    /// windows remain as a fallback when `limits` is absent or empty.
    fn map_usage(usage: &OAuthUsage) -> ProviderUsage {
        let windows = usage
            .limits
            .as_deref()
            .map(map_limits)
            .filter(|windows| !windows.is_empty())
            .unwrap_or_else(|| map_legacy_windows(usage));

        let mut notes = Vec::new();
        if let Some(note) = usage.spend.as_ref().and_then(spend_note) {
            notes.push(note);
        }
        if let Some(note) = usage.extra_usage.as_ref().and_then(extra_usage_note) {
            if !notes.iter().any(|existing| existing == &note) {
                notes.push(note);
            }
        }

        ProviderUsage {
            windows,
            notes,
            // Anthropic has no rate-limit reset-credit mechanism.
            reset_offer: None,
        }
    }

    fn map_legacy_windows(usage: &OAuthUsage) -> Vec<UsageWindow> {
        const FIVE_HOURS_MINS: i64 = 5 * 60;
        const SEVEN_DAYS_MINS: i64 = 7 * 24 * 60;
        let labeled: &[(&Option<OAuthUsageWindow>, i64, Option<&str>)] = &[
            (&usage.five_hour, FIVE_HOURS_MINS, None),
            (&usage.seven_day, SEVEN_DAYS_MINS, Some("all models")),
            (&usage.seven_day_sonnet, SEVEN_DAYS_MINS, Some("Sonnet")),
            (&usage.seven_day_opus, SEVEN_DAYS_MINS, Some("Opus")),
            (
                &usage.seven_day_oauth_apps,
                SEVEN_DAYS_MINS,
                Some("OAuth apps"),
            ),
        ];

        labeled
            .iter()
            .filter_map(|(window, minutes, qualifier)| {
                let window = window.as_ref()?;
                let utilization = window.utilization?;
                let base = super::window_label(Some(*minutes));
                let label = match qualifier {
                    Some(qualifier) => format!("{base} ({qualifier})"),
                    None => base,
                };
                Some(UsageWindow {
                    label,
                    used: percent_to_fraction(utilization),
                    resets_at: window.resets_at.as_deref().and_then(parse_reset),
                })
            })
            .collect()
    }

    fn map_limits(limits: &[OAuthUsageLimit]) -> Vec<UsageWindow> {
        limits.iter().filter_map(limit_window).collect()
    }

    fn limit_window(limit: &OAuthUsageLimit) -> Option<UsageWindow> {
        let percent = limit.percent?;
        Some(UsageWindow {
            label: limit_label(limit),
            used: percent_to_fraction(percent),
            resets_at: limit.resets_at.as_deref().and_then(parse_reset),
        })
    }

    fn limit_label(limit: &OAuthUsageLimit) -> String {
        let base = limit_base_label(limit);
        match limit_qualifier(limit) {
            Some(qualifier) => format!("{base} ({qualifier})"),
            None => base,
        }
    }

    fn limit_base_label(limit: &OAuthUsageLimit) -> String {
        const FIVE_HOURS_MINS: i64 = 5 * 60;
        const DAY_MINS: i64 = 24 * 60;
        const SEVEN_DAYS_MINS: i64 = 7 * DAY_MINS;
        const THIRTY_DAYS_MINS: i64 = 30 * DAY_MINS;

        let kind = limit.kind.as_deref();
        let group = limit.group.as_deref();
        if matches!(kind, Some("session")) || matches!(group, Some("session")) {
            super::window_label(Some(FIVE_HOURS_MINS))
        } else if kind.is_some_and(|kind| kind.starts_with("weekly"))
            || matches!(group, Some("weekly"))
        {
            super::window_label(Some(SEVEN_DAYS_MINS))
        } else if kind.is_some_and(|kind| kind.starts_with("daily"))
            || matches!(group, Some("daily"))
        {
            super::window_label(Some(DAY_MINS))
        } else if kind.is_some_and(|kind| kind.starts_with("monthly"))
            || matches!(group, Some("monthly"))
        {
            super::window_label(Some(THIRTY_DAYS_MINS))
        } else if let Some(kind) = kind.or(group) {
            format!("{} limit", humanize_identifier(kind))
        } else {
            super::window_label(None)
        }
    }

    fn limit_qualifier(limit: &OAuthUsageLimit) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(scope) = &limit.scope {
            if let Some(model) = &scope.model {
                if let Some(name) = model.display_name.as_deref().or(model.id.as_deref()) {
                    parts.push(name.to_string());
                }
            }
            if let Some(surface) = scope.surface.as_deref() {
                parts.push(humanize_identifier(surface));
            }
        }
        if parts.is_empty() && matches!(limit.kind.as_deref(), Some("weekly_all")) {
            parts.push("all models".to_string());
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }

    fn percent_to_fraction(percent: f64) -> f64 {
        (percent / 100.0).clamp(0.0, 1.0)
    }

    fn humanize_identifier(value: &str) -> String {
        let words: Vec<String> = value
            .split(['_', '-', ' '])
            .filter(|word| !word.is_empty())
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => {
                        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                    }
                    None => String::new(),
                }
            })
            .collect();
        if words.is_empty() {
            value.to_string()
        } else {
            words.join(" ")
        }
    }

    /// Parse an ISO 8601 reset timestamp to unix milliseconds.
    fn parse_reset(value: &str) -> Option<i64> {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|dt| dt.timestamp_millis())
    }

    fn spend_note(spend: &OAuthSpend) -> Option<String> {
        if spend.enabled == Some(false) {
            return Some(match spend.disabled_reason.as_deref() {
                Some(reason) => format!("Usage credits: off ({})", disabled_reason_text(reason)),
                None => "Usage credits: off".to_string(),
            });
        }

        let used = spend.used.as_ref().and_then(format_money_amount);
        let limit = spend
            .limit
            .as_ref()
            .or(spend.cap.as_ref())
            .and_then(format_money_amount);
        match (used, limit, spend.percent) {
            (Some(used), Some(limit), _) => Some(format!("Usage credits: {used} of {limit} spent")),
            (Some(used), None, _) => Some(format!("Usage credits: {used} spent")),
            (None, None, Some(percent)) => Some(format!(
                "Usage credits: {:.0}% used",
                percent.clamp(0.0, 100.0)
            )),
            (None, None, None) => spend
                .disabled_reason
                .as_deref()
                .map(|reason| format!("Usage credits: {}", disabled_reason_text(reason))),
            (None, Some(limit), _) => Some(format!("Usage credits: limit {limit}")),
        }
    }

    /// Render a provider `disabled_reason` code as readable text.
    ///
    /// The codes are internal identifiers, so several of them read as
    /// broken English once de-snake-cased. We phrase the known ones and
    /// humanize the rest, which keeps a code we've never seen readable
    /// instead of hidden.
    fn disabled_reason_text(reason: &str) -> String {
        let text = match reason {
            // Despite the name, this is the spend cap being hit for the
            // current period, not an admin switching credits off. The
            // "until" refers to a reset date the usage endpoint never
            // sends (only the `anthropic-ratelimit-unified-overage-reset`
            // response header carries it).
            "org_level_disabled_until" | "org_spend_cap_reached" => "monthly spend limit reached",
            "org_level_disabled" | "org_service_level_disabled" => {
                "turned off by your organization"
            }
            "member_level_disabled" | "member_zero_credit_limit" => "disabled by your admin",
            "seat_tier_level_disabled" | "seat_tier_zero_credit_limit" => {
                "not included in your seat"
            }
            "group_zero_credit_limit" => "your group's limit is zero",
            "overage_not_provisioned" | "no_limits_configured" => "not set up",
            other => return humanize_identifier(other).to_lowercase(),
        };
        text.to_string()
    }

    fn extra_usage_note(extra: &OAuthExtraUsage) -> Option<String> {
        if extra.is_enabled != Some(true) {
            return extra
                .disabled_reason
                .as_deref()
                .map(|reason| format!("Usage credits: off ({})", disabled_reason_text(reason)));
        }
        let decimals = extra.decimal_places.unwrap_or(2);
        let used = format_money_minor(
            extra.used_credits.unwrap_or(0.0),
            extra.currency.as_deref(),
            decimals,
        );
        let limit = match extra.monthly_limit {
            Some(limit) => format_money_minor(limit, extra.currency.as_deref(), decimals),
            None => "unlimited".to_string(),
        };
        Some(format!("Usage credits: {used} of {limit} spent"))
    }

    fn format_money_amount(money: &OAuthMoney) -> Option<String> {
        Some(format_money_minor(
            money.amount_minor?,
            money.currency.as_deref(),
            money.exponent.unwrap_or(2),
        ))
    }

    fn format_money_minor(amount_minor: f64, currency: Option<&str>, exponent: u32) -> String {
        let divisor = 10_f64.powi(i32::try_from(exponent).unwrap_or(2));
        let amount = amount_minor / divisor;
        match currency {
            None | Some("USD") => format!("${amount:.2}"),
            Some(other) => format!("{amount:.2} {other}"),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn maps_windows_in_display_order_and_skips_empty() {
            let usage: OAuthUsage = serde_json::from_str(
                r#"{
                    "five_hour": {"utilization": 50.0, "resets_at": "2026-06-10T17:00:00+00:00"},
                    "seven_day": {"utilization": null},
                    "seven_day_opus": {"utilization": 5.0}
                }"#,
            )
            .unwrap();
            let report = map_usage(&usage);
            let labels: Vec<&str> = report.windows.iter().map(|w| w.label.as_str()).collect();
            assert_eq!(labels, vec!["5h limit", "Weekly limit (Opus)"]);
            assert_eq!(report.windows[0].used, 0.5);
            assert!(report.windows[0].resets_at.is_some());
            assert!(report.windows[1].resets_at.is_none());
        }

        #[test]
        fn maps_provider_limits_without_model_allowlist() {
            let usage: OAuthUsage = serde_json::from_str(
                r#"{
                    "five_hour": {"utilization": 1.0},
                    "limits": [
                        {
                            "group": "session",
                            "kind": "session",
                            "percent": 27,
                            "resets_at": "2026-07-03T16:10:00.481021+00:00"
                        },
                        {
                            "group": "weekly",
                            "kind": "weekly_all",
                            "percent": 55,
                            "resets_at": "2026-07-06T21:00:00.481042+00:00"
                        },
                        {
                            "group": "weekly",
                            "kind": "weekly_scoped",
                            "percent": 100,
                            "resets_at": "2026-07-06T21:00:00.481308+00:00",
                            "scope": {
                                "model": {"display_name": "Fable", "id": null},
                                "surface": null
                            },
                            "severity": "critical",
                            "is_active": true
                        }
                    ]
                }"#,
            )
            .unwrap();

            let report = map_usage(&usage);
            let labels: Vec<&str> = report.windows.iter().map(|w| w.label.as_str()).collect();
            assert_eq!(
                labels,
                vec![
                    "5h limit",
                    "Weekly limit (all models)",
                    "Weekly limit (Fable)"
                ]
            );
            assert_eq!(report.windows[0].used, 0.27);
            assert_eq!(report.windows[2].used, 1.0);
            assert!(report.windows[2].resets_at.is_some());
        }

        #[test]
        fn provider_limits_fall_back_when_empty() {
            let usage: OAuthUsage = serde_json::from_str(
                r#"{
                    "five_hour": {"utilization": 50.0},
                    "limits": [{"kind": "weekly_scoped"}]
                }"#,
            )
            .unwrap();

            let report = map_usage(&usage);
            let labels: Vec<&str> = report.windows.iter().map(|w| w.label.as_str()).collect();
            assert_eq!(labels, vec!["5h limit"]);
        }

        #[test]
        fn spend_note_formats_credit_shape() {
            let spend: OAuthSpend = serde_json::from_str(
                r#"{
                    "enabled": true,
                    "used": {"amount_minor": 123, "currency": "USD", "exponent": 2},
                    "limit": {"amount_minor": 5000, "currency": "USD", "exponent": 2}
                }"#,
            )
            .unwrap();
            assert_eq!(
                spend_note(&spend).unwrap(),
                "Usage credits: $1.23 of $50.00 spent"
            );

            let disabled: OAuthSpend =
                serde_json::from_str(r#"{"enabled": false, "disabled_reason": "out_of_credits"}"#)
                    .unwrap();
            assert_eq!(
                spend_note(&disabled).unwrap(),
                "Usage credits: off (out of credits)"
            );
        }

        #[test]
        fn extra_usage_note_formats_money() {
            let extra: OAuthExtraUsage = serde_json::from_str(
                r#"{"is_enabled": true, "monthly_limit": 5000, "used_credits": 123, "currency": "USD"}"#,
            )
            .unwrap();
            assert_eq!(
                extra_usage_note(&extra).unwrap(),
                "Usage credits: $1.23 of $50.00 spent"
            );
        }

        #[test]
        fn extra_usage_note_unlimited_and_disabled() {
            let unlimited: OAuthExtraUsage = serde_json::from_str(
                r#"{"is_enabled": true, "monthly_limit": null, "used_credits": 200, "currency": "EUR"}"#,
            )
            .unwrap();
            assert_eq!(
                extra_usage_note(&unlimited).unwrap(),
                "Usage credits: 2.00 EUR of unlimited spent"
            );

            let disabled: OAuthExtraUsage = serde_json::from_str(
                r#"{"is_enabled": false, "disabled_reason": "out_of_credits"}"#,
            )
            .unwrap();
            assert_eq!(
                extra_usage_note(&disabled).unwrap(),
                "Usage credits: off (out of credits)"
            );
        }

        #[test]
        fn disabled_reason_phrases_known_codes_and_humanizes_the_rest() {
            let spend: OAuthSpend = serde_json::from_str(
                r#"{"enabled": false, "disabled_reason": "org_level_disabled_until"}"#,
            )
            .unwrap();
            assert_eq!(
                spend_note(&spend).unwrap(),
                "Usage credits: off (monthly spend limit reached)"
            );

            let extra: OAuthExtraUsage = serde_json::from_str(
                r#"{"is_enabled": false, "disabled_reason": "org_level_disabled_until"}"#,
            )
            .unwrap();
            assert_eq!(
                extra_usage_note(&extra).unwrap(),
                "Usage credits: off (monthly spend limit reached)"
            );

            // The plain code is an admin switching credits off, the
            // `_until` code is the spend cap for the period. Same prefix,
            // different meaning.
            assert_eq!(
                disabled_reason_text("org_level_disabled"),
                "turned off by your organization"
            );
            assert_eq!(
                disabled_reason_text("seat_tier_zero_credit_limit"),
                "not included in your seat"
            );
            assert_eq!(disabled_reason_text("out_of_credits"), "out of credits");
            assert_eq!(disabled_reason_text("brand_new_code"), "brand new code");
        }
    }
}

pub mod codex {
    //! Usage source for OpenAI Codex (ChatGPT subscription) accounts.

    use async_trait::async_trait;
    use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
    use serde::{Deserialize, Serialize};
    use std::time::Duration;

    use super::{
        ProviderUsage, RateLimitResetSource, ResetCreditOffer, ResetCreditTarget, ResetOutcome,
        UsageError, UsageReport, UsageSource, UsageWindow,
    };
    use crate::auth::AuthStorage;
    use crate::oauth::openai::extract_account_id;

    /// Provider id this source reports on, matching the OAuth pool the
    /// Codex Responses provider uses (see `auth.rs`).
    const PROVIDER_ID: &str = "openai-codex";

    /// Account usage endpoint on the ChatGPT backend. The same JSON
    /// shape backs the `wham/usage` path; the leading host is fixed
    /// because the OAuth JWT is only valid against `chatgpt.com`.
    const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

    /// Endpoint that spends one earned rate-limit reset credit. Same
    /// host and auth as [`USAGE_URL`]. A POST whose body carries the
    /// caller's idempotency key.
    const CONSUME_URL: &str =
        "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";

    /// Tight timeout so a stalled request can't hang the `/usage`
    /// overlay (the outer collection also caps each source, but the
    /// HTTP-level bound keeps connection setup honest too).
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// Reports plan rate-limit utilization via the ChatGPT backend's
    /// account usage endpoint. Requires the OAuth JWT minted by the
    /// Codex login flow; the token carries the `chatgpt_account_id`
    /// claim that the endpoint requires as a header.
    pub struct OpenAICodexUsageSource {
        usage_url: String,
        consume_url: String,
    }

    impl OpenAICodexUsageSource {
        pub fn new() -> Self {
            Self {
                usage_url: USAGE_URL.to_string(),
                consume_url: CONSUME_URL.to_string(),
            }
        }

        #[cfg(test)]
        fn with_base_url(base_url: &str) -> Self {
            Self {
                usage_url: format!("{base_url}/backend-api/wham/usage"),
                consume_url: format!(
                    "{base_url}/backend-api/wham/rate-limit-reset-credits/consume"
                ),
            }
        }
    }

    impl Default for OpenAICodexUsageSource {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl UsageSource for OpenAICodexUsageSource {
        fn provider_id(&self) -> &str {
            PROVIDER_ID
        }

        async fn fetch(
            &self,
            auth: &AuthStorage,
            account: Option<super::UsageAccount<'_>>,
        ) -> Result<UsageReport, UsageError> {
            let account_label = account.map(|account| account.label().to_string());
            let token = super::resolve_usage_key(auth, PROVIDER_ID, account).await?;
            let (client, token, account_id) = match resolve(token)? {
                Resolved::Ready(client, token, account_id) => (client, token, account_id),
                Resolved::NotConfigured => return Ok(UsageReport::NotConfigured),
                Resolved::Unsupported => {
                    return Ok(UsageReport::Unsupported {
                        reason: "only available with a ChatGPT subscription login".to_string(),
                    });
                }
            };

            let response = authorize(client.get(&self.usage_url), &token, &account_id)
                .send()
                .await
                .map_err(|err| UsageError::Fetch(err.to_string()))?;

            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|err| UsageError::Fetch(err.to_string()))?;
            if !status.is_success() {
                return Err(UsageError::Fetch(format!(
                    "usage request failed ({status}): {}",
                    truncate_body(&body)
                )));
            }

            let payload: UsagePayload = serde_json::from_str(&body).map_err(|err| {
                UsageError::Fetch(format!("could not parse usage response: {err}"))
            })?;
            let usage = map_usage(
                &payload,
                ResetCreditTarget::new(PROVIDER_ID, account_label, account_id),
            );
            Ok(UsageReport::Usage(usage))
        }
    }

    #[async_trait]
    impl RateLimitResetSource for OpenAICodexUsageSource {
        fn provider_id(&self) -> &str {
            PROVIDER_ID
        }

        async fn consume_reset_credit(
            &self,
            auth: &AuthStorage,
            target: &ResetCreditTarget,
            idempotency_key: &str,
        ) -> Result<ResetOutcome, UsageError> {
            if target.provider_id() != PROVIDER_ID {
                return Err(target.changed_error());
            }
            let Some(token) =
                super::resolve_selected_usage_key(auth, PROVIDER_ID, target.account()).await?
            else {
                return Err(target.changed_error());
            };
            let (client, token, account_id) = match resolve(Some(token))? {
                Resolved::Ready(client, token, account_id) => (client, token, account_id),
                Resolved::NotConfigured | Resolved::Unsupported => {
                    return Err(target.changed_error());
                }
            };
            if target.authorization_identity() != account_id {
                return Err(target.changed_error());
            }

            let request = ConsumeRequest {
                redeem_request_id: idempotency_key,
            };
            let response = authorize(client.post(&self.consume_url), &token, &account_id)
                .header(CONTENT_TYPE, "application/json")
                .json(&request)
                .send()
                .await
                .map_err(|err| UsageError::Fetch(err.to_string()))?;

            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|err| UsageError::Fetch(err.to_string()))?;
            if !status.is_success() {
                return Err(UsageError::Fetch(format!(
                    "reset request failed ({status}): {}",
                    truncate_body(&body)
                )));
            }

            let parsed: ConsumeResponse = serde_json::from_str(&body).map_err(|err| {
                UsageError::Fetch(format!("could not parse reset response: {err}"))
            })?;
            parsed.outcome().ok_or_else(|| {
                UsageError::Fetch(format!("unexpected reset response code: {}", parsed.code))
            })
        }
    }

    /// Resolved Codex credentials, or why they can't be used against
    /// the ChatGPT backend.
    enum Resolved {
        /// A built client plus the OAuth token and its account id.
        Ready(reqwest::Client, String, String),
        /// No credential stored for the Codex pool.
        NotConfigured,
        /// A credential exists but lacks the `chatgpt_account_id` claim
        /// (e.g. a plain API key), which the endpoints require.
        Unsupported,
    }

    /// Resolve the Codex OAuth token, its account id, and a built HTTP
    /// client, shared by the usage read and the reset-credit consume.
    fn resolve(token: Option<String>) -> Result<Resolved, UsageError> {
        let Some(token) = token else {
            return Ok(Resolved::NotConfigured);
        };
        // Both endpoints authenticate the account via the
        // `chatgpt_account_id` JWT claim. A token without it can't call
        // them.
        let Some(account_id) = extract_account_id(&token) else {
            return Ok(Resolved::Unsupported);
        };
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| UsageError::Fetch(err.to_string()))?;
        Ok(Resolved::Ready(client, token, account_id))
    }

    /// Attach the shared auth headers (bearer token, account id,
    /// user-agent) to a request builder.
    fn authorize(
        builder: reqwest::RequestBuilder,
        token: &str,
        account_id: &str,
    ) -> reqwest::RequestBuilder {
        builder
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header("ChatGPT-Account-Id", account_id)
            .header(USER_AGENT, user_agent())
    }

    /// Consume request body: the caller's idempotency key.
    #[derive(Serialize)]
    struct ConsumeRequest<'a> {
        redeem_request_id: &'a str,
    }

    /// Consume response: a single `code` string we map to a
    /// [`ResetOutcome`].
    #[derive(Debug, Deserialize)]
    struct ConsumeResponse {
        code: String,
    }

    impl ConsumeResponse {
        /// Map the wire `code` to a [`ResetOutcome`], or `None` for an
        /// unrecognized code so the caller can surface it.
        fn outcome(&self) -> Option<ResetOutcome> {
            match self.code.as_str() {
                "reset" => Some(ResetOutcome::Reset),
                "already_redeemed" => Some(ResetOutcome::AlreadyRedeemed),
                "nothing_to_reset" => Some(ResetOutcome::NothingToReset),
                "no_credit" => Some(ResetOutcome::NoCredit),
                _ => None,
            }
        }
    }

    /// `User-Agent` matching the Codex Responses provider:
    /// `aj/<version> (<os> <arch>)`.
    fn user_agent() -> String {
        format!(
            "aj/{} ({} {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    }

    /// Map the wire payload into the generic report. Windows come from
    /// the primary/secondary rolling limits, the per-feature
    /// `additional_rate_limits`, and the workspace monthly credit cap.
    /// The credits balance rides along as a note. Earned reset credits
    /// surface as a structured [`ProviderUsage::reset_offer`].
    fn map_usage(payload: &UsagePayload, reset_target: ResetCreditTarget) -> ProviderUsage {
        let mut windows = Vec::new();

        if let Some(rate_limit) = payload.rate_limit.as_ref() {
            windows.extend(rate_limit.windows(None));
        }
        for additional in payload.additional_rate_limits.iter().flatten() {
            let qualifier = additional
                .limit_name
                .as_deref()
                .filter(|name| !name.trim().is_empty());
            if let Some(rate_limit) = additional.rate_limit.as_ref() {
                windows.extend(rate_limit.windows(qualifier));
            }
        }
        if let Some(window) = payload
            .spend_control
            .as_ref()
            .and_then(|spend| spend.individual_limit.as_ref())
            .and_then(SpendControlLimit::window)
        {
            windows.push(window);
        }

        let mut notes = Vec::new();
        if let Some(note) = payload.credits.as_ref().and_then(Credits::note) {
            notes.push(note);
        }

        // Negative counts shouldn't happen, but clamp defensively so a
        // bad payload can't wrap into a huge unsigned value.
        let reset_offer = payload.rate_limit_reset_credits.as_ref().map(|credits| {
            ResetCreditOffer::new(
                u32::try_from(credits.available_count.max(0)).unwrap_or(u32::MAX),
                reset_target,
            )
        });

        ProviderUsage {
            windows,
            notes,
            reset_offer,
        }
    }

    #[derive(Debug, Deserialize)]
    struct UsagePayload {
        rate_limit: Option<RateLimit>,
        #[serde(default)]
        additional_rate_limits: Option<Vec<AdditionalRateLimit>>,
        credits: Option<Credits>,
        spend_control: Option<SpendControl>,
        rate_limit_reset_credits: Option<ResetCredits>,
    }

    #[derive(Debug, Deserialize)]
    struct RateLimit {
        primary_window: Option<Window>,
        secondary_window: Option<Window>,
    }

    impl RateLimit {
        /// Build display windows for the primary and secondary limits,
        /// labeling each from its own length and appending `qualifier`
        /// (the metered feature name, for `additional_rate_limits`).
        fn windows(&self, qualifier: Option<&str>) -> Vec<UsageWindow> {
            [self.primary_window.as_ref(), self.secondary_window.as_ref()]
                .into_iter()
                .flatten()
                .filter_map(|window| window.to_usage_window(qualifier))
                .collect()
        }
    }

    #[derive(Debug, Deserialize)]
    struct Window {
        used_percent: Option<f64>,
        limit_window_seconds: Option<i64>,
        /// Window reset, unix seconds.
        reset_at: Option<i64>,
    }

    impl Window {
        fn to_usage_window(&self, qualifier: Option<&str>) -> Option<UsageWindow> {
            let used_percent = self.used_percent?;
            let base = super::window_label(self.limit_window_seconds.map(|secs| secs / 60));
            let label = match qualifier {
                Some(qualifier) => format!("{base} ({qualifier})"),
                None => base,
            };
            Some(UsageWindow {
                label,
                used: (used_percent / 100.0).clamp(0.0, 1.0),
                resets_at: self.reset_at.map(seconds_to_millis),
            })
        }
    }

    #[derive(Debug, Deserialize)]
    struct AdditionalRateLimit {
        limit_name: Option<String>,
        rate_limit: Option<RateLimit>,
    }

    #[derive(Debug, Deserialize)]
    struct Credits {
        #[serde(default)]
        has_credits: bool,
        #[serde(default)]
        unlimited: bool,
        balance: Option<String>,
    }

    impl Credits {
        /// One note line describing the credit balance, or `None` when
        /// the account has no credit tracking (matching the windows-only
        /// view those accounts get).
        fn note(&self) -> Option<String> {
            if !self.has_credits {
                return None;
            }
            if self.unlimited {
                return Some("Credits: unlimited".to_string());
            }
            let balance = self.balance.as_deref()?.trim();
            (!balance.is_empty()).then(|| format!("Credits: {balance}"))
        }
    }

    #[derive(Debug, Deserialize)]
    struct SpendControl {
        individual_limit: Option<SpendControlLimit>,
    }

    #[derive(Debug, Deserialize)]
    struct SpendControlLimit {
        remaining_percent: Option<i64>,
        /// Reset, unix seconds.
        reset_at: Option<i64>,
    }

    impl SpendControlLimit {
        /// The workspace monthly credit cap as a usage window. We render
        /// it like a rate-limit window (percent used + reset) so it sits
        /// naturally alongside the rolling limits.
        fn window(&self) -> Option<UsageWindow> {
            // remaining_percent is server-clamped to 0..=100; the u8
            // conversion is therefore lossless and avoids a silent `as`.
            let remaining = self.remaining_percent?.clamp(0, 100);
            let used = f64::from(u8::try_from(100 - remaining).unwrap_or(0)) / 100.0;
            Some(UsageWindow {
                label: "Monthly credit limit".to_string(),
                used,
                resets_at: self.reset_at.map(seconds_to_millis),
            })
        }
    }

    #[derive(Debug, Deserialize)]
    struct ResetCredits {
        #[serde(default)]
        available_count: i64,
    }

    /// Convert a unix-seconds timestamp to the unix-milliseconds the
    /// generic [`UsageWindow`] carries.
    fn seconds_to_millis(seconds: i64) -> i64 {
        seconds * 1000
    }

    /// Cap an error body so a large or HTML response can't flood the
    /// overlay. Keeps the leading bytes, which carry the useful detail.
    fn truncate_body(body: &str) -> String {
        const MAX: usize = 200;
        if body.len() <= MAX {
            return body.to_string();
        }
        let mut end = MAX;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &body[..end])
    }

    #[cfg(test)]
    mod tests {
        use base64::Engine as _;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        use super::*;

        fn reset_target() -> ResetCreditTarget {
            ResetCreditTarget::new(PROVIDER_ID, Some("work".to_string()), "upstream-work")
        }

        fn account_jwt(account_id: &str) -> String {
            let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&serde_json::json!({
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": account_id
                    }
                }))
                .unwrap(),
            );
            format!("{header}.{payload}.signature")
        }

        async fn answer_request(
            listener: &tokio::net::TcpListener,
            body: &str,
        ) -> std::io::Result<String> {
            let (mut stream, _) = listener.accept().await?;
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            Ok(String::from_utf8_lossy(&request).into_owned())
        }

        /// A real Team-plan response captured from the live endpoint.
        const TEAM_PLAN_RESPONSE: &str = r#"{
            "plan_type": "team",
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "primary_window": {
                    "used_percent": 100,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 17192,
                    "reset_at": 1781872115
                },
                "secondary_window": {
                    "used_percent": 27,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 603992,
                    "reset_at": 1782458915
                }
            },
            "additional_rate_limits": null,
            "credits": {
                "has_credits": false,
                "unlimited": false,
                "balance": null
            },
            "spend_control": { "reached": false, "individual_limit": null },
            "rate_limit_reset_credits": { "available_count": 2 }
        }"#;

        #[test]
        fn maps_primary_and_secondary_windows_with_concrete_labels() {
            let payload: UsagePayload = serde_json::from_str(TEAM_PLAN_RESPONSE).unwrap();
            let report = map_usage(&payload, reset_target());

            let labels: Vec<&str> = report.windows.iter().map(|w| w.label.as_str()).collect();
            assert_eq!(labels, vec!["5h limit", "Weekly limit"]);
            assert_eq!(report.windows[0].used, 1.0);
            assert_eq!(report.windows[1].used, 0.27);
            // reset_at is unix seconds on the wire, unix millis in the model.
            assert_eq!(report.windows[0].resets_at, Some(1781872115 * 1000));
        }

        #[test]
        fn team_plan_reports_reset_credits_and_no_credit_note() {
            let payload: UsagePayload = serde_json::from_str(TEAM_PLAN_RESPONSE).unwrap();
            let report = map_usage(&payload, reset_target());
            // has_credits is false, so no credit-balance note; the two
            // available reset credits surface as the structured count.
            assert!(report.notes.is_empty());
            assert_eq!(
                report.reset_offer,
                Some(ResetCreditOffer::new(2, reset_target()))
            );
        }

        #[test]
        fn maps_credits_spend_control_and_additional_limits() {
            let payload: UsagePayload = serde_json::from_str(
                r#"{
                    "rate_limit": {
                        "primary_window": {
                            "used_percent": 10.5,
                            "limit_window_seconds": 18000,
                            "reset_at": 1000
                        }
                    },
                    "additional_rate_limits": [
                        {
                            "limit_name": "gpt-5-codex",
                            "metered_feature": "codex_other",
                            "rate_limit": {
                                "primary_window": {
                                    "used_percent": 50,
                                    "limit_window_seconds": 604800,
                                    "reset_at": 2000
                                }
                            }
                        }
                    ],
                    "credits": { "has_credits": true, "unlimited": false, "balance": "1234" },
                    "spend_control": {
                        "reached": false,
                        "individual_limit": {
                            "remaining_percent": 40,
                            "reset_at": 3000
                        }
                    },
                    "rate_limit_reset_credits": { "available_count": 0 }
                }"#,
            )
            .unwrap();
            let report = map_usage(&payload, reset_target());

            let labels: Vec<&str> = report.windows.iter().map(|w| w.label.as_str()).collect();
            assert_eq!(
                labels,
                vec![
                    "5h limit",
                    "Weekly limit (gpt-5-codex)",
                    "Monthly credit limit"
                ]
            );
            // remaining_percent 40 => 60% used.
            assert_eq!(report.windows[2].used, 0.6);
            // available_count 0 keeps the note list to just the balance,
            // and surfaces the supported-but-empty reset count.
            assert_eq!(report.notes, vec!["Credits: 1234".to_string()]);
            assert_eq!(
                report.reset_offer,
                Some(ResetCreditOffer::new(0, reset_target()))
            );
        }

        #[test]
        fn missing_reset_credits_field_maps_to_none() {
            let payload: UsagePayload = serde_json::from_str(
                r#"{
                    "rate_limit": {
                        "primary_window": {
                            "used_percent": 10,
                            "limit_window_seconds": 18000,
                            "reset_at": 1000
                        }
                    }
                }"#,
            )
            .unwrap();
            let report = map_usage(&payload, reset_target());
            assert_eq!(report.reset_offer, None);
        }

        #[tokio::test]
        async fn report_target_authorizes_only_the_subscription_that_issued_it() {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind local usage endpoint");
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let usage = answer_request(
                    &listener,
                    r#"{"rate_limit":{"primary_window":{"used_percent":50,"limit_window_seconds":18000,"reset_at":1000}},"rate_limit_reset_credits":{"available_count":1}}"#,
                )
                .await
                .expect("answer usage request");
                assert!(usage.starts_with("GET /backend-api/wham/usage "), "{usage}");
                assert!(usage.contains("chatgpt-account-id: account-a"), "{usage}");

                let reset = answer_request(&listener, r#"{"code":"reset"}"#)
                    .await
                    .expect("answer reset request");
                assert!(
                    reset.starts_with("POST /backend-api/wham/rate-limit-reset-credits/consume "),
                    "{reset}"
                );
                assert!(reset.contains("chatgpt-account-id: account-a"), "{reset}");
                let (_, body) = reset
                    .split_once("\r\n\r\n")
                    .expect("reset request has a body boundary");
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(body).unwrap(),
                    serde_json::json!({ "redeem_request_id": "same-account-key" }),
                    "the production request carries the attempt's idempotency key"
                );

                match tokio::time::timeout(
                    Duration::from_secs(1),
                    answer_request(&listener, r#"{"code":"reset"}"#),
                )
                .await
                {
                    Ok(Ok(request)) => Some(request),
                    Ok(Err(err)) => panic!("third request failed: {err}"),
                    Err(_) => None,
                }
            });

            let dir = tempfile::TempDir::new().expect("auth tempdir");
            let auth = AuthStorage::new(dir.path().join("auth.json"));
            let insert = |account_id: &str| {
                crate::auth::AuthCredential::OAuth(crate::oauth::OAuthCredentials::new(
                    format!("refresh-{account_id}"),
                    account_jwt(account_id),
                    i64::MAX,
                ))
            };
            auth.insert_account("openai-codex", "work", insert("account-a"))
                .await
                .expect("seed account A");
            let accounts = auth
                .accounts("openai-codex")
                .await
                .unwrap()
                .expect("labeled set");
            let (label, credential) = accounts
                .accounts
                .iter()
                .find(|(label, _)| label == "work")
                .expect("work snapshot");
            let source = OpenAICodexUsageSource::with_base_url(&base_url);
            let report = source
                .fetch(
                    &auth,
                    Some(super::super::UsageAccount::from_store_snapshot(
                        label, credential,
                    )),
                )
                .await
                .expect("fetch account A report");
            let UsageReport::Usage(usage) = report else {
                panic!("expected usage report")
            };
            let target = usage
                .reset_offer
                .expect("report carries reset offer")
                .target;

            auth.remove_account("openai-codex", "work").await.unwrap();
            auth.insert_account("openai-codex", "work", insert("account-a"))
                .await
                .expect("refresh token for the same subscription");
            assert_eq!(
                source
                    .consume_reset_credit(&auth, &target, "same-account-key")
                    .await
                    .expect("same upstream account remains authorized"),
                ResetOutcome::Reset
            );

            auth.remove_account("openai-codex", "work").await.unwrap();
            auth.insert_account("openai-codex", "work", insert("account-b"))
                .await
                .expect("rebind label to account B");
            let stale = source
                .consume_reset_credit(&auth, &target, "stale-account-key")
                .await;
            assert!(matches!(
                stale,
                Err(UsageError::ResetTargetChanged {
                    ref provider_id,
                    account: Some(ref account),
                }) if provider_id == "openai-codex" && account == "work"
            ));

            assert!(
                server.await.expect("local endpoint task").is_none(),
                "the stale target sent a request before being refused"
            );
        }

        #[tokio::test]
        async fn missing_or_non_subscription_reset_account_is_typed_as_changed() {
            let dir = tempfile::TempDir::new().expect("auth tempdir");
            let auth = AuthStorage::new(dir.path().join("auth.json"));
            let target = ResetCreditTarget::new(PROVIDER_ID, Some("work".to_string()), "account-a");
            let source = OpenAICodexUsageSource::new();

            let missing = source
                .consume_reset_credit(&auth, &target, "missing-key")
                .await;
            assert!(matches!(
                missing,
                Err(UsageError::ResetTargetChanged {
                    ref provider_id,
                    account: Some(ref account),
                }) if provider_id == PROVIDER_ID && account == "work"
            ));

            auth.insert_account(
                PROVIDER_ID,
                "work",
                crate::auth::AuthCredential::ApiKey {
                    key: "not-a-subscription".to_string(),
                },
            )
            .await
            .expect("rebind to API key");
            let unsupported = source.consume_reset_credit(&auth, &target, "api-key").await;
            assert!(matches!(
                unsupported,
                Err(UsageError::ResetTargetChanged {
                    ref provider_id,
                    account: Some(ref account),
                }) if provider_id == PROVIDER_ID && account == "work"
            ));
        }

        #[test]
        fn consume_response_maps_every_known_code() {
            let cases = [
                ("reset", Some(ResetOutcome::Reset)),
                ("already_redeemed", Some(ResetOutcome::AlreadyRedeemed)),
                ("nothing_to_reset", Some(ResetOutcome::NothingToReset)),
                ("no_credit", Some(ResetOutcome::NoCredit)),
                ("something_new", None),
            ];
            for (code, expected) in cases {
                let body = format!(r#"{{"code": "{code}"}}"#);
                let parsed: ConsumeResponse = serde_json::from_str(&body).unwrap();
                assert_eq!(parsed.outcome(), expected, "code {code}");
            }
        }

        #[test]
        fn consume_request_serializes_idempotency_key() {
            let body = serde_json::to_value(ConsumeRequest {
                redeem_request_id: "key-123",
            })
            .unwrap();
            assert_eq!(body, serde_json::json!({ "redeem_request_id": "key-123" }));
        }

        #[test]
        fn truncate_body_caps_long_input_on_char_boundary() {
            assert_eq!(truncate_body("short"), "short");

            let long = "a".repeat(500);
            let capped = truncate_body(&long);
            assert_eq!(capped.chars().filter(|c| *c == 'a').count(), 200);
            assert!(capped.ends_with('…'));

            // A multi-byte char straddling the cap is dropped whole, never
            // split into an invalid slice.
            let multibyte = format!("{}é", "a".repeat(199));
            let capped = truncate_body(&multibyte);
            assert!(capped.ends_with('…'));
            assert!(!capped.contains('é'));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tempfile::TempDir;

    use super::*;
    use crate::oauth::{OAuthCallbacks, OAuthCredentials, OAuthError, OAuthProvider};

    struct StallingRefreshProvider {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl OAuthProvider for StallingRefreshProvider {
        fn id(&self) -> &str {
            "anthropic"
        }

        fn name(&self) -> &str {
            "Stalling refresh"
        }

        async fn login(
            &self,
            _callbacks: &dyn OAuthCallbacks,
        ) -> Result<OAuthCredentials, OAuthError> {
            unreachable!("the fixture never logs in")
        }

        async fn refresh_token(
            &self,
            _credentials: &OAuthCredentials,
        ) -> Result<OAuthCredentials, OAuthError> {
            self.started.notify_one();
            self.release.notified().await;
            // Deliberately not an Anthropic OAuth prefix, so the concrete
            // source returns Unsupported after refresh without network I/O.
            Ok(OAuthCredentials::new(
                "refreshed-r",
                "refreshed-api-key",
                i64::MAX,
            ))
        }
    }

    struct FailingRefreshProvider {
        id: String,
    }

    #[async_trait]
    impl OAuthProvider for FailingRefreshProvider {
        fn id(&self) -> &str {
            &self.id
        }

        fn name(&self) -> &str {
            "Failing refresh"
        }

        async fn login(
            &self,
            _callbacks: &dyn OAuthCallbacks,
        ) -> Result<OAuthCredentials, OAuthError> {
            unreachable!("the fixture never logs in")
        }

        async fn refresh_token(
            &self,
            _credentials: &OAuthCredentials,
        ) -> Result<OAuthCredentials, OAuthError> {
            Err(OAuthError::Other("scripted refresh failure".to_string()))
        }
    }

    /// An empty credential store in a scratch directory, plus the guard that
    /// removes it. The storage reads and writes under that directory, so the
    /// caller has to hold the guard for as long as it uses the storage.
    fn scratch_storage(tag: &str) -> (TempDir, AuthStorage) {
        let dir = TempDir::with_prefix(format!("aj-usage-test-{tag}-")).expect("create temp dir");
        let storage = AuthStorage::with_providers(dir.path().join("auth.json"), HashMap::new());
        (dir, storage)
    }

    async fn two_account_storage(tag: &str, provider_id: &str) -> (TempDir, AuthStorage) {
        let dir = TempDir::with_prefix(format!("aj-usage-test-{tag}-")).expect("create temp dir");
        let provider: Arc<dyn OAuthProvider> = Arc::new(FailingRefreshProvider {
            id: provider_id.to_string(),
        });
        let auth = AuthStorage::with_providers(
            dir.path().join("auth.json"),
            HashMap::from([(provider_id.to_string(), provider)]),
        );
        auth.insert_account(
            provider_id,
            "personal",
            crate::auth::AuthCredential::ApiKey {
                key: "personal-key".to_string(),
            },
        )
        .await
        .unwrap();
        auth.insert_account(
            provider_id,
            "work",
            crate::auth::AuthCredential::OAuth(OAuthCredentials::new("old-r", "old-a", 1)),
        )
        .await
        .unwrap();
        (dir, auth)
    }

    fn snapshot<'a>(accounts: &'a crate::auth::ProviderAccounts, label: &str) -> UsageAccount<'a> {
        let (label, credential) = accounts
            .accounts
            .iter()
            .find(|(candidate, _)| candidate == label)
            .expect("snapshot contains the account");
        UsageAccount::from_store_snapshot(label, credential)
    }

    /// No credential at all → `NotConfigured`, no network involved.
    #[tokio::test]
    async fn anthropic_source_reports_not_configured() {
        let (_dir, auth) = scratch_storage("not-configured");
        let source = anthropic::AnthropicUsageSource;
        // NOTE: env vars could interfere here, but tests don't run
        // with ANTHROPIC_* keys set in CI.
        if std::env::var("ANTHROPIC_API_KEY").is_ok()
            || std::env::var("ANTHROPIC_OAUTH_TOKEN").is_ok()
        {
            return;
        }
        let report = source.fetch(&auth, None).await.unwrap();
        assert_eq!(report, UsageReport::NotConfigured);
    }

    /// A plain API key → `Unsupported`, no network involved.
    #[tokio::test]
    async fn anthropic_source_reports_unsupported_for_api_key() {
        let (_dir, auth) = scratch_storage("api-key");
        auth.insert_bare(
            "anthropic",
            crate::auth::AuthCredential::ApiKey {
                key: "sk-ant-api-key".into(),
            },
        )
        .await
        .unwrap();
        // A runtime override shields the test from ambient env keys.
        auth.set_runtime_api_key("anthropic", "sk-ant-api-key".into())
            .await;
        let source = anthropic::AnthropicUsageSource;
        match source.fetch(&auth, None).await.unwrap() {
            UsageReport::Unsupported { reason } => {
                assert!(reason.contains("subscription"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn labeled_usage_resolution_reads_each_store_account_despite_an_override() {
        let (_dir, auth) = scratch_storage("account-keys");
        for (label, key) in [("personal", "personal-key"), ("work", "work-key")] {
            auth.insert_account(
                "anthropic",
                label,
                crate::auth::AuthCredential::ApiKey {
                    key: key.to_string(),
                },
            )
            .await
            .unwrap();
        }
        auth.set_runtime_api_key("anthropic", "override-key".to_string())
            .await;
        assert_eq!(
            auth.get_api_key("anthropic", Some("work"))
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("override-key"),
            "the inference resolver still honors the process override"
        );
        let accounts = auth
            .accounts("anthropic")
            .await
            .unwrap()
            .expect("a labeled set");
        let snapshot = |label: &str| {
            let (label, credential) = accounts
                .accounts
                .iter()
                .find(|(candidate, _)| candidate == label)
                .expect("snapshot contains the account");
            UsageAccount::from_store_snapshot(label, credential)
        };

        assert_eq!(
            resolve_usage_key(&auth, "anthropic", Some(snapshot("personal")))
                .await
                .unwrap()
                .as_deref(),
            Some("personal-key"),
            "the personal usage row resolves its stored account"
        );
        assert_eq!(
            resolve_usage_key(&auth, "anthropic", Some(snapshot("work")))
                .await
                .unwrap()
                .as_deref(),
            Some("work-key"),
            "each usage row resolves the stored account it names"
        );
        assert_eq!(
            resolve_selected_usage_key(&auth, "anthropic", Some("work"))
                .await
                .unwrap()
                .as_deref(),
            Some("work-key"),
            "a later account action re-reads the selected store slot without the override"
        );
    }

    #[tokio::test]
    async fn concrete_sources_route_each_account_before_any_usage_request() {
        let cases: [(&str, Box<dyn UsageSource>); 2] = [
            ("anthropic", Box::new(anthropic::AnthropicUsageSource)),
            (
                "openai-codex",
                Box::new(codex::OpenAICodexUsageSource::new()),
            ),
        ];
        for (provider_id, source) in cases {
            let (_dir, auth) = two_account_storage("source-routing", provider_id).await;
            let accounts = auth.accounts(provider_id).await.unwrap().expect("a set");

            let personal = source
                .fetch(&auth, Some(snapshot(&accounts, "personal")))
                .await
                .expect("the API-key account resolves without a network request");
            assert!(
                matches!(personal, UsageReport::Unsupported { .. }),
                "{provider_id}'s personal API-key row is unsupported"
            );
            let work = source.fetch(&auth, Some(snapshot(&accounts, "work"))).await;
            assert!(
                matches!(work, Err(UsageError::Auth(AuthError::OAuth(_)))),
                "{provider_id}'s work row must reach its own failing OAuth refresh, got {work:?}"
            );
        }
    }

    #[tokio::test]
    async fn concrete_source_keeps_a_fresh_sibling_out_of_a_stalled_refresh_lock() {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let provider: Arc<dyn OAuthProvider> = Arc::new(StallingRefreshProvider {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        });
        let dir = TempDir::with_prefix("aj-usage-test-stalled-source-").expect("create temp dir");
        let auth = AuthStorage::with_providers(
            dir.path().join("auth.json"),
            HashMap::from([("anthropic".to_string(), provider)]),
        );
        auth.insert_account(
            "anthropic",
            "expired",
            crate::auth::AuthCredential::OAuth(OAuthCredentials::new("old-r", "old-a", 1)),
        )
        .await
        .unwrap();
        auth.insert_account(
            "anthropic",
            "fresh",
            crate::auth::AuthCredential::ApiKey {
                key: "fresh-api-key".to_string(),
            },
        )
        .await
        .unwrap();
        let accounts = auth.accounts("anthropic").await.unwrap().expect("a set");
        let credential = |label: &str| {
            accounts
                .accounts
                .iter()
                .find(|(candidate, _)| candidate == label)
                .expect("snapshot contains the account")
                .clone()
        };
        let expired = credential("expired");
        let fresh = credential("fresh");

        let refresh_started = started.notified();
        let refresh_auth = auth.clone();
        let refreshing = tokio::spawn(async move {
            let (label, credential) = expired;
            anthropic::AnthropicUsageSource
                .fetch(
                    &refresh_auth,
                    Some(UsageAccount::from_store_snapshot(&label, &credential)),
                )
                .await
        });
        refresh_started.await;

        let (label, credential) = fresh;
        let sibling = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            anthropic::AnthropicUsageSource.fetch(
                &auth,
                Some(UsageAccount::from_store_snapshot(&label, &credential)),
            ),
        )
        .await
        .expect("the concrete fresh sibling must bypass another account's refresh lock")
        .unwrap();
        assert!(matches!(sibling, UsageReport::Unsupported { .. }));

        release.notify_one();
        let refreshed = refreshing.await.unwrap().unwrap();
        assert!(matches!(refreshed, UsageReport::Unsupported { .. }));
    }

    #[tokio::test]
    async fn concrete_codex_reset_routes_the_selected_account_before_any_request() {
        let (_dir, auth) = two_account_storage("reset-routing", "openai-codex").await;
        let source = codex::OpenAICodexUsageSource::new();
        let target =
            ResetCreditTarget::new("openai-codex", Some("work".to_string()), "work-upstream-id");

        let result = source
            .consume_reset_credit(&auth, &target, "idempotency-key")
            .await;
        assert!(
            matches!(result, Err(UsageError::Auth(AuthError::OAuth(_)))),
            "the reset must reach work's failing OAuth refresh rather than the default API key: {result:?}"
        );
    }
}
