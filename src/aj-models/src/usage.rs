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
    /// Human-readable window name, e.g. "Current session".
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

/// Usage sources shipped out of the box. Today: Anthropic.
pub fn default_usage_sources() -> Vec<Arc<dyn UsageSource>> {
    vec![Arc::new(anthropic::AnthropicUsageSource)]
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
    fn map_usage(usage: &OAuthUsage) -> ProviderUsage {
        let labeled: &[(&Option<OAuthUsageWindow>, &str)] = &[
            (&usage.five_hour, "Current session"),
            (&usage.seven_day, "Current week (all models)"),
            (&usage.seven_day_sonnet, "Current week (Sonnet)"),
            (&usage.seven_day_opus, "Current week (Opus)"),
            (&usage.seven_day_oauth_apps, "Current week (OAuth apps)"),
        ];

        let mut windows = Vec::new();
        for (window, label) in labeled {
            let Some(window) = window else { continue };
            // A window without a utilization number carries no
            // information; skip it rather than rendering "?% used".
            let Some(utilization) = window.utilization else {
                continue;
            };
            windows.push(UsageWindow {
                label: (*label).to_string(),
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

        ProviderUsage { windows, notes }
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
            assert_eq!(labels, vec!["Current session", "Current week (Opus)"]);
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
