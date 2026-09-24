//! Host-owned provider usage collection and reset actions, with display helpers.
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

use aj_models::auth::{AuthStorage, StoredProviderCredentials};
#[cfg(test)]
use aj_models::usage::ProviderUsage;
use aj_models::usage::{UsageError, UsageReport, UsageSource, default_usage_sources};

use crate::auth::{api_provider_name, credential_provider_name};

/// Per-account timeout. The Anthropic source's HTTP request already
/// caps itself at 5 s; this outer bound also covers credential
/// resolution (an OAuth refresh round-trip) so one stuck account can't
/// hold the whole page indefinitely.
const SOURCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub use aj_wire::{ProviderUsageStatus, UsageOutcome};

/// Provider adapters owned by a session host. Credentials are supplied only by
/// that host, including on refresh and reset retries.
pub struct UsageSources {
    pub usage: Vec<Arc<dyn UsageSource>>,
    pub resets: Vec<Arc<dyn aj_models::usage::RateLimitResetSource>>,
}

impl Default for UsageSources {
    fn default() -> Self {
        Self {
            usage: default_usage_sources(),
            resets: aj_models::usage::default_reset_sources(),
        }
    }
}

impl UsageSources {
    /// Collect every account through the supplied host credential store.
    pub async fn collect(&self, auth: &AuthStorage) -> aj_wire::ProviderUsageReport {
        aj_wire::ProviderUsageReport {
            statuses: collect_usage_from_sources(auth, self.usage.clone(), SOURCE_TIMEOUT).await,
            reset_providers: self
                .resets
                .iter()
                .map(|source| source.provider_id().to_string())
                .collect(),
        }
    }

    /// Route a confirmed claim to its provider, which revalidates account identity
    /// against host credentials before consuming a credit.
    pub async fn reset(
        &self,
        auth: &AuthStorage,
        request: &aj_wire::UsageResetRequest,
    ) -> aj_wire::UsageResetResponse {
        use aj_wire::UsageResetFailure;
        let Some(source) = self
            .resets
            .iter()
            .find(|source| source.provider_id() == request.target.provider_id())
        else {
            return Err(UsageResetFailure::Error(
                "Resetting is not supported for this provider.".into(),
            ));
        };
        if request.idempotency_key.trim().is_empty() {
            return Err(UsageResetFailure::Error(
                "A reset idempotency key is required.".into(),
            ));
        }
        source
            .consume_reset_credit(auth, &request.target, &request.idempotency_key)
            .await
            .map_err(|err| match err {
                UsageError::StaleResetTarget => UsageResetFailure::StaleTarget,
                err => UsageResetFailure::Error(usage_error_message(err)),
            })
    }
}

fn usage_error_message(error: UsageError) -> String {
    match error {
        // Credential parser and OAuth errors can contain secrets. Keep them
        // host-local, while preserving the usage adapter's provider diagnostics.
        UsageError::Auth(_) => "Could not resolve host credentials".into(),
        UsageError::Fetch(message) => message,
        UsageError::StaleResetTarget => error.to_string(),
    }
}

/// Providers surfaced on the `/usage` page even without a usage
/// source, so the page self-documents that it covers all providers
/// and not just Anthropic. Mirrors the `/auth` page's known set.
const KNOWN_PROVIDERS: &[&str] = &["anthropic", "openai", "openai-codex", "openrouter"];

/// Fetch usage for every provider account concurrently: one status per
/// stored account label, or one bare status when the provider has no labeled
/// accounts. A runtime `--api-key` override also collapses the provider to one
/// bare status, because the store serves the override for every label and
/// labeled rows would all show the same numbers. Source-less known providers
/// get `NoSource` rows the same way. Statuses are sorted by provider id, then
/// account, for a stable display order.
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

    // Account inventory first, so the fan-out below knows every row it owes
    // before any fetch starts.
    let mut discoveries = tokio::task::JoinSet::new();
    for (provider_id, source) in providers {
        let auth = auth.clone();
        discoveries.spawn(async move {
            let accounts = account_names(&auth, &provider_id, source_timeout).await;
            (provider_id, source, accounts)
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
        let accounts = match accounts {
            Ok(accounts) => accounts,
            Err(message) => {
                statuses.push(ProviderUsageStatus {
                    provider_name: api_provider_name(&provider_id).to_string(),
                    provider_id,
                    account: None,
                    outcome: source
                        .map_or(UsageOutcome::NoSource, |_| UsageOutcome::Error(message)),
                });
                continue;
            }
        };
        let Some(source) = source else {
            statuses.extend(accounts.into_iter().map(|(account, provider_name)| {
                ProviderUsageStatus {
                    provider_id: provider_id.clone(),
                    provider_name,
                    account,
                    outcome: UsageOutcome::NoSource,
                }
            }));
            continue;
        };
        for (account, provider_name) in accounts {
            let source = Arc::clone(&source);
            let auth = auth.clone();
            tasks.spawn(async move {
                let fetch = source.fetch(&auth, account.as_deref());
                let outcome = match tokio::time::timeout(source_timeout, fetch).await {
                    Ok(Ok(UsageReport::Usage(usage))) => UsageOutcome::Usage(usage),
                    Ok(Ok(UsageReport::Unsupported { reason })) => {
                        UsageOutcome::Unsupported { reason }
                    }
                    Ok(Ok(UsageReport::NotConfigured)) => UsageOutcome::NotConfigured,
                    Ok(Err(err)) => UsageOutcome::Error(usage_error_message(err)),
                    Err(_) => UsageOutcome::Error("timed out".to_string()),
                };
                ProviderUsageStatus {
                    provider_id: source.provider_id().to_string(),
                    provider_name,
                    account,
                    outcome,
                }
            });
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

/// Discover labels and credential-kind names without resolving or refreshing tokens.
async fn account_names(
    auth: &AuthStorage,
    provider_id: &str,
    timeout: std::time::Duration,
) -> Result<Vec<(Option<String>, String)>, String> {
    let bare = || vec![(None, api_provider_name(provider_id).to_string())];
    if auth.has_runtime_override(provider_id).await {
        return Ok(bare());
    }
    let stored = match tokio::time::timeout(timeout, auth.stored_credentials(provider_id)).await {
        Ok(Ok(stored)) => stored,
        Ok(Err(err)) => return Err(usage_error_message(err.into())),
        Err(_) => return Err("timed out".to_string()),
    };
    let providers = auth.oauth_provider_ids().await;
    let oauth_name = providers
        .iter()
        .find(|(id, _)| id == provider_id)
        .map(|(_, name)| name.as_str());
    let name =
        |credential: &_| credential_provider_name(provider_id, credential, oauth_name).to_string();
    Ok(match stored {
        Some(StoredProviderCredentials::Bare(credential)) => vec![(None, name(&credential))],
        Some(StoredProviderCredentials::Accounts(set)) if set.accounts.is_empty() => bare(),
        Some(StoredProviderCredentials::Accounts(set)) => set
            .accounts
            .into_iter()
            .map(|(label, credential)| (Some(label), name(&credential)))
            .collect(),
        None => bare(),
    })
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
pub fn format_reset(reset_ms: i64, now_ms: i64) -> String {
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

    use aj_models::auth::{AuthCredential, AuthError};
    use aj_models::usage::{RateLimitResetTarget, UsageWindow};
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

    const CREDENTIAL_SENTINEL: &str = "secret-credential-sentinel";

    async fn corrupt_credentials(auth: &AuthStorage) {
        let malformed = serde_json::json!({
            "openai-codex": {
                "type": "oauth",
                "access": "access-token",
                "refresh": "refresh-token",
                "expires": CREDENTIAL_SENTINEL,
            }
        });
        std::fs::write(auth.path(), serde_json::to_vec(&malformed).unwrap()).unwrap();
        let error = auth.accounts("openai-codex").await.unwrap_err();
        assert!(matches!(error, AuthError::Parse(_)));
        assert!(error.to_string().contains(CREDENTIAL_SENTINEL));
    }

    #[tokio::test]
    async fn discovery_errors_do_not_serialize_credential_contents() {
        let dir = TempDir::with_prefix("aj-usage-discovery-error-").unwrap();
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), Default::default());
        corrupt_credentials(&auth).await;

        let report = UsageSources::default().collect(&auth).await;
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains(CREDENTIAL_SENTINEL)
        );
        for provider in ["anthropic", "openai-codex"] {
            let rows = accounts_of(&report.statuses, provider);
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].outcome,
                UsageOutcome::Error("Could not resolve host credentials".into())
            );
        }
    }

    struct CorruptingUsageSource;

    #[async_trait]
    impl UsageSource for CorruptingUsageSource {
        fn provider_id(&self) -> &str {
            "openai-codex"
        }

        async fn fetch(
            &self,
            auth: &AuthStorage,
            account: Option<&str>,
        ) -> Result<UsageReport, UsageError> {
            // Discovery has finished. Exercise a credential failure during the
            // real adapter's subsequent resolution, not the inventory read.
            corrupt_credentials(auth).await;
            aj_models::usage::codex::OpenAICodexUsageSource
                .fetch(auth, account)
                .await
        }
    }

    #[tokio::test]
    async fn fetch_errors_do_not_serialize_credential_contents() {
        let dir = TempDir::with_prefix("aj-usage-fetch-error-").unwrap();
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), Default::default());
        let sources = UsageSources {
            usage: vec![Arc::new(CorruptingUsageSource)],
            resets: vec![],
        };

        let report = sources.collect(&auth).await;
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains(CREDENTIAL_SENTINEL)
        );
        let rows = accounts_of(&report.statuses, "openai-codex");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].outcome,
            UsageOutcome::Error("Could not resolve host credentials".into())
        );
    }

    #[tokio::test]
    async fn reset_errors_do_not_serialize_credential_contents() {
        let dir = TempDir::with_prefix("aj-usage-reset-error-").unwrap();
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), Default::default());
        corrupt_credentials(&auth).await;
        let request = aj_wire::UsageResetRequest {
            target: RateLimitResetTarget::new("openai-codex", None, "account-id".into()),
            idempotency_key: "reset-attempt".into(),
        };

        let response = UsageSources::default().reset(&auth, &request).await;
        assert!(
            !serde_json::to_string(&response)
                .unwrap()
                .contains(CREDENTIAL_SENTINEL)
        );
        assert_eq!(
            response,
            Err(aj_wire::UsageResetFailure::Error(
                "Could not resolve host credentials".into()
            ))
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
            account: Option<&str>,
        ) -> Result<UsageReport, UsageError> {
            self.calls.lock().unwrap().push(account.map(str::to_string));
            match account {
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
                    reason: "bare credential".to_string(),
                }),
                Some(other) => panic!("unexpected account {other}"),
            }
        }
    }

    async fn seed_accounts(auth: &AuthStorage, provider_id: &str) {
        for label in ["personal", "work"] {
            auth.insert_account(
                provider_id,
                label,
                AuthCredential::ApiKey {
                    key: format!("{label}-key"),
                },
            )
            .await
            .expect("seed account");
        }
    }

    fn accounts_of<'a>(
        statuses: &'a [ProviderUsageStatus],
        provider_id: &str,
    ) -> Vec<&'a ProviderUsageStatus> {
        statuses
            .iter()
            .filter(|status| status.provider_id == provider_id)
            .collect()
    }

    fn labels<'a>(statuses: &[&'a ProviderUsageStatus]) -> Vec<Option<&'a str>> {
        statuses
            .iter()
            .map(|status| status.account.as_deref())
            .collect()
    }

    #[tokio::test]
    async fn collect_fetches_every_account_and_times_out_one_without_hiding_its_sibling() {
        let dir = TempDir::with_prefix("aj-usage-accounts-").expect("create temp dir");
        let auth = AuthStorage::new(dir.path().join("auth.json"));
        seed_accounts(&auth, "anthropic").await;
        seed_accounts(&auth, "openrouter").await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let statuses = collect_usage_from_sources(
            &auth,
            vec![Arc::new(FakeUsageSource {
                calls: Arc::clone(&calls),
            })],
            std::time::Duration::from_millis(100),
        )
        .await;

        let anthropic = accounts_of(&statuses, "anthropic");
        assert_eq!(labels(&anthropic), vec![Some("personal"), Some("work")]);
        assert!(anthropic.iter().all(|row| row.provider_name == "Anthropic"));
        let UsageOutcome::Usage(personal) = &anthropic[0].outcome else {
            panic!("personal account lost its usage report")
        };
        assert_eq!(personal.windows[0].used, 0.25);
        assert_eq!(personal.notes, ["personal note"]);
        assert!(matches!(
            &anthropic[1].outcome,
            UsageOutcome::Error(message) if message == "timed out"
        ));
        let mut calls = calls.lock().unwrap().clone();
        calls.sort();
        assert_eq!(calls, vec![Some("personal".into()), Some("work".into())]);

        let openrouter = accounts_of(&statuses, "openrouter");
        assert_eq!(labels(&openrouter), vec![Some("personal"), Some("work")]);
        assert!(
            openrouter
                .iter()
                .all(|row| row.provider_name == "OpenRouter")
        );
        assert!(
            openrouter
                .iter()
                .all(|status| matches!(status.outcome, UsageOutcome::NoSource))
        );
    }

    #[tokio::test]
    async fn a_runtime_override_collapses_the_provider_to_one_bare_row() {
        let dir = TempDir::with_prefix("aj-usage-override-").expect("create temp dir");
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), Default::default());
        seed_accounts(&auth, "anthropic").await;
        auth.set_runtime_api_key("anthropic", "override".to_string())
            .await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let statuses = collect_usage_from_sources(
            &auth,
            vec![Arc::new(FakeUsageSource {
                calls: Arc::clone(&calls),
            })],
            std::time::Duration::from_millis(100),
        )
        .await;

        let anthropic = accounts_of(&statuses, "anthropic");
        assert_eq!(labels(&anthropic), vec![None]);
        assert!(matches!(
            &anthropic[0].outcome,
            UsageOutcome::Unsupported { reason } if reason == "bare credential"
        ));
        assert_eq!(*calls.lock().unwrap(), vec![None]);
    }
}
