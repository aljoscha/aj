//! Binary-side usage-page helpers.
//!
//! The fetching machinery (the [`UsageSource`] trait and its
//! implementations) lives in `aj-models`; this module holds the
//! binary's UX around it: [`collect_usage`] turns every registered
//! source into render-ready rows for the `/usage` overlay, and the
//! formatting helpers render utilization and reset times the way the
//! overlay shows them.
//!
//! [`UsageSource`]: aj_models::usage::UsageSource

use std::sync::Arc;

use chrono::{Datelike, Local, TimeZone, Utc};

use aj_models::auth::{AccountSnapshot, AuthStorage};
use aj_models::usage::{ProviderUsage, UsageReport, UsageSource, default_usage_sources};

/// Per-account timeout. The Anthropic source's HTTP request already
/// caps itself at 5 s; this outer bound also covers credential
/// resolution (an OAuth refresh round-trip) so one stuck account can't
/// hold the whole page indefinitely.
const SOURCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One provider account's resolved usage status, ready to render.
#[derive(Debug, Clone)]
pub struct ProviderUsageStatus {
    pub provider_id: String,
    /// The exact stored account label. `None` is the effective unlabeled
    /// credential or an unconfigured provider.
    pub account: Option<String>,
    pub outcome: UsageOutcome,
}

/// What the `/usage` page shows for one provider account.
#[derive(Debug, Clone)]
pub enum UsageOutcome {
    /// Usage numbers were fetched; render one row per window.
    Usage(ProviderUsage),
    /// Credentials exist but can't report usage (provider-supplied
    /// reason, e.g. "only available with a subscription login").
    Unsupported { reason: String },
    /// No credentials configured for this provider.
    NotConfigured,
    /// No usage source implemented for this provider yet.
    NoSource,
    /// The fetch failed; the message is shown verbatim.
    Error(String),
}

/// Providers surfaced on the `/usage` page even without a usage
/// source, so the page self-documents that it covers all providers
/// and not just Anthropic. Mirrors the `/auth` page's known set.
const KNOWN_PROVIDERS: &[&str] = &["anthropic", "openai", "openai-codex", "openrouter"];

/// Fetch usage for every provider account concurrently. Account inventory is
/// captured before any fetch begins so an OAuth refresh cannot block discovery
/// of a fresh sibling behind the shared credential-file lock.
pub async fn collect_usage(auth: &AuthStorage) -> Vec<ProviderUsageStatus> {
    collect_usage_from_sources(auth, default_usage_sources(), SOURCE_TIMEOUT).await
}

async fn collect_usage_from_sources(
    auth: &AuthStorage,
    sources: Vec<Arc<dyn UsageSource>>,
    source_timeout: std::time::Duration,
) -> Vec<ProviderUsageStatus> {
    let mut providers: Vec<(String, Option<Arc<dyn UsageSource>>)> = sources
        .into_iter()
        .map(|source| (source.provider_id().to_string(), Some(source)))
        .collect();
    for provider_id in KNOWN_PROVIDERS {
        if !providers.iter().any(|(id, _)| id == provider_id) {
            providers.push(((*provider_id).to_string(), None));
        }
    }

    let mut discoveries = tokio::task::JoinSet::new();
    for (provider_id, source) in providers {
        let auth = auth.clone();
        discoveries.spawn(async move {
            let accounts =
                match tokio::time::timeout(source_timeout, auth.accounts(&provider_id)).await {
                    Ok(accounts) => accounts
                        .map(|accounts| {
                            accounts.map_or_else(Vec::new, |accounts| accounts.into_snapshots())
                        })
                        .map_err(|err| err.to_string()),
                    Err(_) => Err("timed out".to_string()),
                };
            let runtime_override = auth.has_runtime_override(&provider_id).await;
            (
                provider_id,
                source,
                accounts.map(|accounts| (accounts, runtime_override)),
            )
        });
    }

    let mut discovered = Vec::new();
    while let Some(result) = discoveries.join_next().await {
        match result {
            Ok(provider) => discovered.push(provider),
            Err(err) => tracing::warn!("usage account discovery task panicked: {err}"),
        }
    }

    let mut statuses = Vec::new();
    let mut tasks = tokio::task::JoinSet::new();
    for (provider_id, source, accounts) in discovered {
        let (accounts, runtime_override) = match accounts {
            Ok(accounts) => accounts,
            Err(message) => {
                statuses.push(ProviderUsageStatus {
                    provider_id,
                    account: None,
                    outcome: source
                        .map_or(UsageOutcome::NoSource, |_| UsageOutcome::Error(message)),
                });
                continue;
            }
        };

        if let Some(source) = source {
            let mut accounts: Vec<Option<AccountSnapshot>> =
                accounts.into_iter().map(Some).collect();
            if accounts.is_empty() || runtime_override {
                accounts.push(None);
            }
            for account in accounts {
                let source = Arc::clone(&source);
                let auth = auth.clone();
                tasks.spawn(async move {
                    let account_label = account.as_ref().map(|account| account.label().to_string());
                    let outcome = match tokio::time::timeout(
                        source_timeout,
                        source.fetch(&auth, account.as_ref()),
                    )
                    .await
                    {
                        Ok(Ok(UsageReport::Usage(usage))) => UsageOutcome::Usage(usage),
                        Ok(Ok(UsageReport::Unsupported { reason })) => {
                            UsageOutcome::Unsupported { reason }
                        }
                        Ok(Ok(UsageReport::NotConfigured)) => UsageOutcome::NotConfigured,
                        Ok(Err(err)) => UsageOutcome::Error(err.to_string()),
                        Err(_) => UsageOutcome::Error("timed out".to_string()),
                    };
                    ProviderUsageStatus {
                        provider_id: source.provider_id().to_string(),
                        account: account_label,
                        outcome,
                    }
                });
            }
        } else {
            if accounts.is_empty() || runtime_override {
                statuses.push(ProviderUsageStatus {
                    provider_id: provider_id.clone(),
                    account: None,
                    outcome: UsageOutcome::NoSource,
                });
            }
            statuses.extend(accounts.into_iter().map(|account| ProviderUsageStatus {
                provider_id: provider_id.clone(),
                account: Some(account.label().to_string()),
                outcome: UsageOutcome::NoSource,
            }));
        }
    }

    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(status) => statuses.push(status),
            Err(err) => tracing::warn!("usage fetch task panicked: {err}"),
        }
    }

    statuses.sort_by(|a, b| {
        a.provider_id
            .cmp(&b.provider_id)
            .then_with(|| a.account.cmp(&b.account))
    });
    statuses
}

/// Render a window's status, e.g.
/// `"12% used · resets 17:00 (Europe/Berlin)"`.
pub fn format_window_status(used: f64, resets_at: Option<i64>, now_ms: i64) -> String {
    let percent = (used * 100.0).round().clamp(0.0, 100.0);
    match resets_at {
        Some(reset_ms) => format!(
            "{percent:.0}% used · resets {}",
            format_reset(reset_ms, now_ms)
        ),
        None => format!("{percent:.0}% used"),
    }
}

/// Render a reset timestamp relative to `now`, in local time with
/// the machine's timezone appended: `"17:00 (Europe/Berlin)"` within
/// the same day, `"Mon 09:00 (Europe/Berlin)"` within a week,
/// `"Jun 15 (Europe/Berlin)"` beyond that, `"now"` when already past.
fn format_reset(reset_ms: i64, now_ms: i64) -> String {
    if reset_ms <= now_ms {
        return "now".to_string();
    }
    let Some(reset_utc) = Utc.timestamp_millis_opt(reset_ms).single() else {
        return "unknown".to_string();
    };
    let reset = reset_utc.with_timezone(&Local);
    let now = Utc
        .timestamp_millis_opt(now_ms)
        .single()
        .map(|dt| dt.with_timezone(&Local))
        .unwrap_or_else(Local::now);

    // The zone name covers DST by itself; the offset — used only by
    // the fallback label — is taken from the reset instant rather
    // than from now, so a DST transition between the two still
    // renders correctly.
    let tz = local_tz_label(reset.offset().local_minus_utc());
    if reset.date_naive() == now.date_naive() {
        format!("{} ({tz})", reset.format("%H:%M"))
    } else if reset_ms - now_ms < 7 * 24 * 3600 * 1000 {
        format!("{} ({tz})", reset.format("%a %H:%M"))
    } else {
        format!("{} {} ({tz})", month_abbrev(reset.month()), reset.day())
    }
}

/// The machine's timezone as an IANA name (e.g. `"Europe/Berlin"`),
/// falling back to a UTC-offset label built from `offset_secs` when
/// the name can't be determined.
fn local_tz_label(offset_secs: i32) -> String {
    match iana_time_zone::get_timezone() {
        // "Etc/UTC" is the zoneinfo spelling; plain "UTC" reads
        // better.
        Ok(name) if name == "Etc/UTC" => "UTC".to_string(),
        Ok(name) => name,
        Err(_) => utc_offset_label(offset_secs),
    }
}

/// Short label for a UTC offset in seconds: `"UTC"`, `"UTC+2"`,
/// `"UTC-7:30"`.
fn utc_offset_label(offset_secs: i32) -> String {
    if offset_secs == 0 {
        return "UTC".to_string();
    }
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let total_minutes = offset_secs.abs() / 60;
    let (hours, minutes) = (total_minutes / 60, total_minutes % 60);
    if minutes == 0 {
        format!("UTC{sign}{hours}")
    } else {
        format!("UTC{sign}{hours}:{minutes:02}")
    }
}

/// English month abbreviation, independent of locale settings.
fn month_abbrev(month: u32) -> &'static str {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS[usize::try_from(month.saturating_sub(1))
        .unwrap_or(0)
        .min(11)]
}

/// Current wall-clock time in unix milliseconds.
pub fn now_unix_ms() -> i64 {
    Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use aj_models::auth::{AccountSnapshot, AuthCredential};
    use aj_models::usage::{UsageError, UsageWindow};
    use async_trait::async_trait;
    use chrono::DateTime;
    use tempfile::TempDir;

    use super::*;

    fn ms(dt: DateTime<Local>) -> i64 {
        dt.timestamp_millis()
    }

    /// The machine-local timezone label for `dt`, so the exact-string
    /// assertions below stay portable across test machines in any
    /// timezone.
    fn tz(dt: DateTime<Local>) -> String {
        local_tz_label(dt.offset().local_minus_utc())
    }

    #[test]
    fn utc_offset_labels() {
        assert_eq!(utc_offset_label(0), "UTC");
        assert_eq!(utc_offset_label(2 * 3600), "UTC+2");
        assert_eq!(utc_offset_label(-7 * 3600 - 30 * 60), "UTC-7:30");
        assert_eq!(utc_offset_label(5 * 3600 + 45 * 60), "UTC+5:45");
    }

    #[test]
    fn window_status_without_reset() {
        assert_eq!(format_window_status(0.125, None, 0), "13% used");
    }

    #[test]
    fn reset_same_day_shows_time_only() {
        let now = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).unwrap();
        let reset = Local.with_ymd_and_hms(2026, 6, 10, 17, 0, 0).unwrap();
        assert_eq!(
            format_window_status(0.5, Some(ms(reset)), ms(now)),
            format!("50% used · resets 17:00 ({})", tz(reset))
        );
    }

    #[test]
    fn reset_within_week_shows_weekday() {
        let now = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).unwrap();
        // 2026-06-15 is a Monday.
        let reset = Local.with_ymd_and_hms(2026, 6, 15, 9, 0, 0).unwrap();
        assert_eq!(
            format_reset(ms(reset), ms(now)),
            format!("Mon 09:00 ({})", tz(reset))
        );
    }

    #[test]
    fn reset_beyond_week_shows_date() {
        let now = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).unwrap();
        let reset = Local.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap();
        assert_eq!(
            format_reset(ms(reset), ms(now)),
            format!("Jul 1 ({})", tz(reset))
        );
    }

    #[test]
    fn reset_in_past_shows_now() {
        assert_eq!(format_reset(1000, 2000), "now");
    }

    /// Without credentials in the environment, collect still returns
    /// a row per known provider so the page never comes up empty.
    #[tokio::test]
    async fn collect_covers_known_providers() {
        let dir = TempDir::with_prefix("aj-usage-collect-").expect("create temp dir");
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), Default::default());
        let statuses = collect_usage(&auth).await;
        let ids: Vec<&str> = statuses.iter().map(|s| s.provider_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["anthropic", "openai", "openai-codex", "openrouter"]
        );
    }

    struct FakeUsageSource {
        calls: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl UsageSource for FakeUsageSource {
        fn provider_id(&self) -> &str {
            "anthropic"
        }

        async fn fetch(
            &self,
            _auth: &AuthStorage,
            account: Option<&AccountSnapshot>,
        ) -> Result<UsageReport, UsageError> {
            let account = account.map(|account| account.label().to_string());
            self.calls.lock().unwrap().push(account.clone());
            match account.as_deref() {
                Some("work") => std::future::pending().await,
                Some("personal") => Ok(UsageReport::Usage(ProviderUsage {
                    windows: vec![UsageWindow {
                        label: "5h limit".to_string(),
                        used: 0.25,
                        resets_at: Some(123),
                    }],
                    notes: vec!["personal note".to_string()],
                    reset_credits: None,
                })),
                None => Ok(UsageReport::Unsupported {
                    reason: "runtime override".to_string(),
                }),
                Some(other) => panic!("unexpected account {other}"),
            }
        }
    }

    #[tokio::test]
    async fn collect_times_out_one_account_without_hiding_its_sibling_or_override() {
        let dir = TempDir::with_prefix("aj-usage-accounts-").expect("create temp dir");
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), Default::default());
        for label in ["personal", "work"] {
            auth.insert_account(
                "anthropic",
                label,
                AuthCredential::ApiKey {
                    key: format!("{label}-key"),
                },
            )
            .await
            .expect("seed account");
        }
        auth.set_runtime_api_key("anthropic", "override".to_string())
            .await;
        for label in ["personal", "work"] {
            auth.insert_account(
                "openrouter",
                label,
                AuthCredential::ApiKey {
                    key: format!("{label}-router-key"),
                },
            )
            .await
            .expect("seed source-less account");
        }
        auth.set_runtime_api_key("openrouter", "router-override".to_string())
            .await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let all_statuses = collect_usage_from_sources(
            &auth,
            vec![Arc::new(FakeUsageSource {
                calls: Arc::clone(&calls),
            })],
            std::time::Duration::from_millis(100),
        )
        .await;
        let statuses = all_statuses
            .iter()
            .filter(|status| status.provider_id == "anthropic")
            .collect::<Vec<_>>();

        assert_eq!(
            statuses
                .iter()
                .map(|status| status.account.as_deref())
                .collect::<Vec<_>>(),
            vec![None, Some("personal"), Some("work")]
        );
        let UsageOutcome::Usage(personal) = &statuses[1].outcome else {
            panic!("personal account lost its usage report")
        };
        assert_eq!(personal.windows[0].used, 0.25);
        assert_eq!(personal.notes, ["personal note"]);
        assert!(matches!(
            &statuses[2].outcome,
            UsageOutcome::Error(message) if message == "timed out"
        ));
        let mut calls = calls.lock().unwrap().clone();
        calls.sort();
        assert_eq!(
            calls,
            vec![None, Some("personal".into()), Some("work".into())]
        );
        let openrouter = all_statuses
            .iter()
            .filter(|status| status.provider_id == "openrouter")
            .collect::<Vec<_>>();
        assert_eq!(
            openrouter
                .iter()
                .map(|status| status.account.as_deref())
                .collect::<Vec<_>>(),
            vec![None, Some("personal"), Some("work")]
        );
        assert!(
            openrouter
                .iter()
                .all(|status| matches!(status.outcome, UsageOutcome::NoSource))
        );
    }
}
