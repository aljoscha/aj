//! Provider-agnostic plan-usage reporting.
//!
//! A [`UsageSource`] knows how to fetch account-level usage numbers
//! (rate-limit windows like "current session" or "current week") for
//! one provider, resolving its own credentials through
//! [`AuthStorage`]. The binary's `/usage` page walks
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

use crate::auth::{AuthError, AuthStorage};

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
    /// Earned rate-limit reset credits the account can spend to clear
    /// its current windows early. `None` when the provider has no such
    /// mechanism, `Some(0)` when it has one but none are available.
    /// Providers with a reset mechanism report `Some(_)`. The UI pairs
    /// the count with a matching [`RateLimitResetSource`] by provider id
    /// before offering the action.
    pub reset_credits: Option<u32>,
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
}

/// A per-provider usage fetcher.
#[async_trait]
pub trait UsageSource: Send + Sync {
    /// Provider id this source reports on, matching the ids used by
    /// [`AuthStorage`] (e.g. `"anthropic"`).
    fn provider_id(&self) -> &str;

    /// Fetch the current usage report, resolving credentials through
    /// `auth` (including OAuth refresh, same as the messages path).
    async fn fetch(&self, auth: &AuthStorage) -> Result<UsageReport, UsageError>;
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
/// [`ProviderUsage::reset_credits`] implement this, and the UI pairs
/// the two by provider id: it offers the action for a provider whose
/// report shows credits available and that has a source here.
#[async_trait]
pub trait RateLimitResetSource: Send + Sync {
    /// Provider id this source acts on, matching the ids used by
    /// [`AuthStorage`] and [`UsageSource::provider_id`].
    fn provider_id(&self) -> &str;

    /// Spend one earned reset credit, resolving credentials through
    /// `auth`.
    ///
    /// `idempotency_key` de-duplicates retries of one logical attempt:
    /// callers pass a fresh key per attempt and reuse it when retrying
    /// that same attempt, so a network retry can't double-spend.
    async fn consume_reset_credit(
        &self,
        auth: &AuthStorage,
        idempotency_key: &str,
    ) -> Result<ResetOutcome, UsageError>;
}

/// Usage sources shipped out of the box: Anthropic (Claude
/// Pro/Max) and OpenAI Codex (ChatGPT subscription).
pub fn default_usage_sources() -> Vec<Arc<dyn UsageSource>> {
    vec![
        Arc::new(anthropic::AnthropicUsageSource),
        Arc::new(codex::OpenAICodexUsageSource),
    ]
}

/// Rate-limit reset sources shipped out of the box. Only OpenAI Codex
/// (ChatGPT subscription) exposes earned reset credits today, so it is
/// the sole entry. The list is the discovery point the UI walks to find
/// which providers can spend a credit.
pub fn default_reset_sources() -> Vec<Arc<dyn RateLimitResetSource>> {
    vec![Arc::new(codex::OpenAICodexUsageSource)]
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
    use anthropic_sdk::usage::{OAuthExtraUsage, OAuthUsage, OAuthUsageWindow};
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

        async fn fetch(&self, auth: &AuthStorage) -> Result<UsageReport, UsageError> {
            let Some(key) = auth.get_api_key(self.provider_id()).await? else {
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

    /// Map the wire response to the generic report: known windows in
    /// display order, plus a usage-credits note when enabled.
    ///
    /// Anthropic doesn't report window lengths numerically, but its
    /// field names pin them down: `five_hour` is the 5-hour session
    /// window and every `seven_day*` field is a weekly window. We pass
    /// those known lengths through [`window_label`] so the rows read
    /// the same as Codex's, and append the per-model scope (Sonnet,
    /// Opus, ...) as a qualifier where Anthropic splits the weekly
    /// budget by model.
    fn map_usage(usage: &OAuthUsage) -> ProviderUsage {
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

        let mut windows = Vec::new();
        for (window, minutes, qualifier) in labeled {
            let Some(window) = window else { continue };
            // A window without a utilization number carries no
            // information; skip it rather than rendering "?% used".
            let Some(utilization) = window.utilization else {
                continue;
            };
            let base = super::window_label(Some(*minutes));
            let label = match qualifier {
                Some(qualifier) => format!("{base} ({qualifier})"),
                None => base,
            };
            windows.push(UsageWindow {
                label,
                used: (utilization / 100.0).clamp(0.0, 1.0),
                resets_at: window.resets_at.as_deref().and_then(parse_reset),
            });
        }

        let mut notes = Vec::new();
        if let Some(extra) = &usage.extra_usage {
            if let Some(note) = extra_usage_note(extra) {
                notes.push(note);
            }
        }

        ProviderUsage {
            windows,
            notes,
            // Anthropic has no rate-limit reset-credit mechanism.
            reset_credits: None,
        }
    }

    /// Parse an ISO 8601 reset timestamp to unix milliseconds.
    fn parse_reset(value: &str) -> Option<i64> {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|dt| dt.timestamp_millis())
    }

    /// Render the usage-credit state as one note line, or `None` when
    /// credits are disabled / unreported.
    fn extra_usage_note(extra: &OAuthExtraUsage) -> Option<String> {
        if extra.is_enabled != Some(true) {
            return None;
        }
        let used = format_money(extra.used_credits.unwrap_or(0.0), extra.currency.as_deref());
        let limit = match extra.monthly_limit {
            Some(limit) => format_money(limit, extra.currency.as_deref()),
            None => "unlimited".to_string(),
        };
        Some(format!("Extra usage credits: {used} of {limit} spent"))
    }

    /// Format an amount of cents as money, e.g. `$12.34` for USD or
    /// `12.34 EUR` otherwise.
    fn format_money(cents: f64, currency: Option<&str>) -> String {
        let amount = cents / 100.0;
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
        fn extra_usage_note_formats_money() {
            let extra: OAuthExtraUsage = serde_json::from_str(
                r#"{"is_enabled": true, "monthly_limit": 5000, "used_credits": 123, "currency": "USD"}"#,
            )
            .unwrap();
            assert_eq!(
                extra_usage_note(&extra).unwrap(),
                "Extra usage credits: $1.23 of $50.00 spent"
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
                "Extra usage credits: 2.00 EUR of unlimited spent"
            );

            let disabled: OAuthExtraUsage =
                serde_json::from_str(r#"{"is_enabled": false}"#).unwrap();
            assert!(extra_usage_note(&disabled).is_none());
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
        ProviderUsage, RateLimitResetSource, ResetOutcome, UsageError, UsageReport, UsageSource,
        UsageWindow,
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
    pub struct OpenAICodexUsageSource;

    #[async_trait]
    impl UsageSource for OpenAICodexUsageSource {
        fn provider_id(&self) -> &str {
            PROVIDER_ID
        }

        async fn fetch(&self, auth: &AuthStorage) -> Result<UsageReport, UsageError> {
            let (client, token, account_id) = match resolve(auth).await? {
                Resolved::Ready(client, token, account_id) => (client, token, account_id),
                Resolved::NotConfigured => return Ok(UsageReport::NotConfigured),
                Resolved::Unsupported => {
                    return Ok(UsageReport::Unsupported {
                        reason: "only available with a ChatGPT subscription login".to_string(),
                    });
                }
            };

            let response = authorize(client.get(USAGE_URL), &token, &account_id)
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
            Ok(UsageReport::Usage(map_usage(&payload)))
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
            idempotency_key: &str,
        ) -> Result<ResetOutcome, UsageError> {
            // These credential errors are defensive: the UI only offers
            // the action for a provider whose usage report came back with
            // credits available, which already required a usable login.
            let (client, token, account_id) = match resolve(auth).await? {
                Resolved::Ready(client, token, account_id) => (client, token, account_id),
                Resolved::NotConfigured => {
                    return Err(UsageError::Fetch(
                        "no ChatGPT subscription login configured".to_string(),
                    ));
                }
                Resolved::Unsupported => {
                    return Err(UsageError::Fetch(
                        "only available with a ChatGPT subscription login".to_string(),
                    ));
                }
            };

            let request = ConsumeRequest {
                redeem_request_id: idempotency_key,
            };
            let response = authorize(client.post(CONSUME_URL), &token, &account_id)
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
    async fn resolve(auth: &AuthStorage) -> Result<Resolved, UsageError> {
        let Some(token) = auth.get_api_key(PROVIDER_ID).await? else {
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
    /// surface as the structured [`ProviderUsage::reset_credits`] count.
    fn map_usage(payload: &UsagePayload) -> ProviderUsage {
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
        let reset_credits = payload
            .rate_limit_reset_credits
            .as_ref()
            .map(|c| u32::try_from(c.available_count.max(0)).unwrap_or(u32::MAX));

        ProviderUsage {
            windows,
            notes,
            reset_credits,
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
        use super::*;

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
            let report = map_usage(&payload);

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
            let report = map_usage(&payload);
            // has_credits is false, so no credit-balance note; the two
            // available reset credits surface as the structured count.
            assert!(report.notes.is_empty());
            assert_eq!(report.reset_credits, Some(2));
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
            let report = map_usage(&payload);

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
            assert_eq!(report.reset_credits, Some(0));
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
            let report = map_usage(&payload);
            assert_eq!(report.reset_credits, None);
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

    use super::*;

    fn scratch_storage(tag: &str) -> AuthStorage {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("aj-usage-test-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        AuthStorage::with_providers(dir.join("auth.json"), HashMap::new())
    }

    /// No credential at all → `NotConfigured`, no network involved.
    #[tokio::test]
    async fn anthropic_source_reports_not_configured() {
        let auth = scratch_storage("not-configured");
        let source = anthropic::AnthropicUsageSource;
        // NOTE: env vars could interfere here, but tests don't run
        // with ANTHROPIC_* keys set in CI.
        if std::env::var("ANTHROPIC_API_KEY").is_ok()
            || std::env::var("ANTHROPIC_OAUTH_TOKEN").is_ok()
        {
            return;
        }
        let report = source.fetch(&auth).await.unwrap();
        assert_eq!(report, UsageReport::NotConfigured);
    }

    /// A plain API key → `Unsupported`, no network involved.
    #[tokio::test]
    async fn anthropic_source_reports_unsupported_for_api_key() {
        let auth = scratch_storage("api-key");
        auth.set(
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
        match source.fetch(&auth).await.unwrap() {
            UsageReport::Unsupported { reason } => {
                assert!(reason.contains("subscription"), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }
}
