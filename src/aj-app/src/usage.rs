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

use std::future::Future;
use std::sync::Arc;

use chrono::{Datelike, Local, TimeZone, Utc};

use aj_models::auth::AuthStorage;
use aj_models::usage::{
    ProviderUsage, RateLimitResetSource, ResetCreditTarget, ResetOutcome, UsageAccount, UsageError,
    UsageReport, UsageSource, default_reset_sources, default_usage_sources,
};

/// Per-account timeout. The Anthropic source's HTTP request already
/// caps itself at 5 s; this outer bound also covers credential
/// resolution (an OAuth refresh round-trip) so one stuck account can't
/// hold the whole page in its loading state indefinitely.
const SOURCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One provider account's resolved usage status, ready to render.
#[derive(Debug, Clone)]
pub struct ProviderUsageStatus {
    pub provider_id: String,
    /// The labeled stored account this row reports. `None` retains the
    /// pre-accounts row for a bare or unconfigured provider.
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

/// Host-owned provider-usage operations used by the interactive page.
///
/// The service keeps credential access and destructive reset sources behind
/// one application boundary. A frontend receives it from its control backend,
/// never by pairing its own credential store with a viewed session.
#[derive(Clone)]
pub struct UsageService {
    auth: AuthStorage,
    reset_sources: Vec<Arc<dyn RateLimitResetSource>>,
}

impl UsageService {
    /// Build the standard host service over its authoritative credential
    /// store.
    pub fn standard(auth: AuthStorage) -> Self {
        Self::new(auth, default_reset_sources())
    }

    /// Build a service with explicit reset sources. This is the provider
    /// extension seam and lets tests exercise the real page without network
    /// access.
    pub fn new(auth: AuthStorage, reset_sources: Vec<Arc<dyn RateLimitResetSource>>) -> Self {
        Self {
            auth,
            reset_sources,
        }
    }

    /// Collect the complete per-account usage page from this host's store.
    pub async fn collect(&self) -> Vec<ProviderUsageStatus> {
        collect_usage(&self.auth).await
    }

    /// Whether this host has a destructive reset implementation for a
    /// provider whose report offered credits.
    pub fn can_reset(&self, provider_id: &str) -> bool {
        self.reset_sources
            .iter()
            .any(|source| source.provider_id() == provider_id)
    }

    /// Spend one reset credit against the exact target issued by a report.
    pub async fn consume_reset_credit(
        &self,
        target: &ResetCreditTarget,
        idempotency_key: &str,
    ) -> Result<ResetOutcome, UsageError> {
        let Some(source) = self
            .reset_sources
            .iter()
            .find(|source| source.provider_id() == target.provider_id())
        else {
            return Err(UsageError::Fetch(
                "resetting is not supported for this provider".to_string(),
            ));
        };
        source
            .consume_reset_credit(&self.auth, target, idempotency_key)
            .await
    }
}

/// Providers surfaced on the `/usage` page even without a usage
/// source, so the page self-documents that it covers all providers
/// and not just Anthropic. Mirrors the `/auth` page's known set.
const KNOWN_PROVIDERS: &[&str] = &["anthropic", "openai", "openai-codex", "openrouter"];

/// Fetch usage from every registered source and every labeled account,
/// concurrently, then append "no usage source" rows for the remaining
/// known providers. Rows are sorted by provider id and account label for
/// a stable display order.
pub async fn collect_usage(auth: &AuthStorage) -> Vec<ProviderUsageStatus> {
    collect_usage_from_sources(auth, default_usage_sources()).await
}

async fn collect_usage_from_sources(
    auth: &AuthStorage,
    sources: Vec<Arc<dyn UsageSource>>,
) -> Vec<ProviderUsageStatus> {
    collect_usage_with_discovery(auth, sources, |auth, provider_id| async move {
        auth.accounts(&provider_id).await
    })
    .await
}

async fn collect_usage_with_discovery<F, Fut>(
    auth: &AuthStorage,
    sources: Vec<Arc<dyn UsageSource>>,
    discover: F,
) -> Vec<ProviderUsageStatus>
where
    F: Fn(AuthStorage, String) -> Fut + Clone + Send + 'static,
    Fut: Future<
            Output = Result<Option<aj_models::auth::ProviderAccounts>, aj_models::auth::AuthError>,
        > + Send
        + 'static,
{
    let mut providers = Vec::new();
    for source in sources {
        let provider_id = source.provider_id().to_string();
        if !providers.iter().any(|(current, _)| current == &provider_id) {
            providers.push((provider_id, Some(source)));
        }
    }
    for provider_id in KNOWN_PROVIDERS {
        if !providers.iter().any(|(current, _)| current == provider_id) {
            providers.push(((*provider_id).to_string(), None));
        }
    }

    let mut discoveries = tokio::task::JoinSet::new();
    for (provider_id, source) in providers {
        let auth = auth.clone();
        let discover = discover.clone();
        discoveries.spawn(async move {
            let has_runtime_override =
                source.is_some() && auth.has_runtime_override(&provider_id).await;
            let accounts =
                tokio::time::timeout(SOURCE_TIMEOUT, discover(auth, provider_id.clone())).await;
            (provider_id, source, has_runtime_override, accounts)
        });
    }

    let mut statuses = Vec::new();
    let mut discovered_accounts = Vec::new();
    while let Some(discovered) = discoveries.join_next().await {
        let Ok((provider_id, source, has_runtime_override, result)) = discovered else {
            tracing::warn!("usage account discovery task panicked");
            continue;
        };
        let mut accounts = match result {
            Ok(Ok(Some(accounts))) if accounts.accounts.is_empty() => vec![None],
            Ok(Ok(Some(accounts))) => accounts.accounts.into_iter().map(Some).collect(),
            Ok(Ok(None)) => vec![None],
            Ok(Err(err)) => {
                statuses.push(ProviderUsageStatus {
                    provider_id,
                    account: None,
                    outcome: source.as_ref().map_or(UsageOutcome::NoSource, |_| {
                        UsageOutcome::Error(err.to_string())
                    }),
                });
                continue;
            }
            Err(_) => {
                statuses.push(ProviderUsageStatus {
                    provider_id,
                    account: None,
                    outcome: source.as_ref().map_or(UsageOutcome::NoSource, |_| {
                        UsageOutcome::Error("timed out".to_string())
                    }),
                });
                continue;
            }
        };
        if has_runtime_override && accounts.iter().all(Option::is_some) {
            // A process override is an effective provider credential outside
            // the stored account namespace. Preserve its provider-level row
            // beside the labeled inventory rather than duplicating it under
            // every account label.
            accounts.push(None);
        }
        if let Some(source) = source {
            discovered_accounts.push((source, accounts));
        } else {
            statuses.extend(accounts.into_iter().map(|account| ProviderUsageStatus {
                provider_id: provider_id.clone(),
                account: account.map(|(label, _)| label),
                outcome: UsageOutcome::NoSource,
            }));
        }
    }

    // Complete the page-wide store snapshot before any source can begin an
    // OAuth refresh under the shared auth-file lock. A stalled refresh for
    // one provider must not prevent a later provider from discovering its
    // already-fresh accounts.
    let mut tasks = tokio::task::JoinSet::new();
    for (source, accounts) in discovered_accounts {
        for account in accounts {
            let source = Arc::clone(&source);
            let auth = auth.clone();
            tasks.spawn(async move {
                let account_label = account.as_ref().map(|(label, _)| label.clone());
                let usage_account = account.as_ref().map(|(label, credential)| {
                    UsageAccount::from_store_snapshot(label, credential)
                });
                let outcome =
                    match tokio::time::timeout(SOURCE_TIMEOUT, source.fetch(&auth, usage_account))
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

    use aj_models::auth::AuthCredential;
    use aj_models::usage::{ResetCreditOffer, ResetCreditTarget, UsageError, UsageWindow};
    use async_trait::async_trait;
    use chrono::DateTime;
    use tempfile::TempDir;

    use super::*;

    struct FakeUsageSource {
        calls: Arc<Mutex<Vec<Option<String>>>>,
        fail_account: Option<String>,
        delays: Option<(u64, u64)>,
    }

    struct CoordinatedUsageSource {
        provider_id: &'static str,
        started: Arc<tokio::sync::Notify>,
        release: Option<Arc<tokio::sync::Notify>>,
    }

    #[async_trait]
    impl UsageSource for CoordinatedUsageSource {
        fn provider_id(&self) -> &str {
            self.provider_id
        }

        async fn fetch(
            &self,
            _auth: &AuthStorage,
            account: Option<UsageAccount<'_>>,
        ) -> Result<UsageReport, UsageError> {
            assert!(
                account.is_some(),
                "the source receives its discovered snapshot"
            );
            self.started.notify_one();
            if let Some(release) = &self.release {
                release.notified().await;
            }
            Ok(UsageReport::Unsupported {
                reason: "scripted".to_string(),
            })
        }
    }

    #[async_trait]
    impl UsageSource for FakeUsageSource {
        fn provider_id(&self) -> &str {
            "anthropic"
        }

        async fn fetch(
            &self,
            _auth: &AuthStorage,
            account: Option<UsageAccount<'_>>,
        ) -> Result<UsageReport, UsageError> {
            let account = account.map(UsageAccount::label);
            self.calls
                .lock()
                .expect("calls mutex")
                .push(account.map(str::to_string));
            if let Some((personal, work)) = self.delays {
                let delay = match account {
                    Some("personal") => personal,
                    Some("work") => work,
                    _ => 0,
                };
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            }
            if self
                .fail_account
                .as_deref()
                .is_some_and(|failed| Some(failed) == account)
            {
                return Err(UsageError::Fetch(format!(
                    "{} failed",
                    account.unwrap_or("bare")
                )));
            }
            let (used, note, reset_credits) = match account {
                Some("personal") => (0.25, "personal note", Some(1)),
                Some("work") => (0.75, "work note", Some(2)),
                _ => (0.5, "bare note", None),
            };
            Ok(UsageReport::Usage(ProviderUsage {
                windows: vec![UsageWindow {
                    label: "5h limit".to_string(),
                    used,
                    resets_at: None,
                }],
                notes: vec![note.to_string()],
                reset_offer: reset_credits.map(|available| {
                    ResetCreditOffer::new(
                        available,
                        ResetCreditTarget::new(
                            "anthropic",
                            account.map(str::to_string),
                            account
                                .map(|account| format!("identity-{account}"))
                                .unwrap_or_else(|| "identity-bare".to_string()),
                        ),
                    )
                }),
            }))
        }
    }

    async fn two_account_auth(tag: &str) -> (TempDir, AuthStorage) {
        let dir = TempDir::with_prefix(format!("aj-usage-{tag}-")).expect("create temp dir");
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), Default::default());
        for (label, key) in [("personal", "personal-key"), ("work", "work-key")] {
            auth.insert_account(
                "anthropic",
                label,
                AuthCredential::ApiKey {
                    key: key.to_string(),
                },
            )
            .await
            .expect("seed account");
        }
        (dir, auth)
    }

    fn fake_source(
        calls: Arc<Mutex<Vec<Option<String>>>>,
        fail_account: Option<&str>,
        delays: Option<(u64, u64)>,
    ) -> Arc<dyn UsageSource> {
        Arc::new(FakeUsageSource {
            calls,
            fail_account: fail_account.map(str::to_string),
            delays,
        })
    }

    fn account_statuses(statuses: &[ProviderUsageStatus]) -> Vec<&ProviderUsageStatus> {
        statuses
            .iter()
            .filter(|status| status.provider_id == "anthropic")
            .collect()
    }

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

    #[tokio::test]
    async fn collect_emits_no_source_per_labeled_account() {
        let dir = TempDir::with_prefix("aj-usage-no-source-").expect("create temp dir");
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), Default::default());
        for label in ["personal", "work"] {
            auth.insert_account(
                "openai",
                label,
                AuthCredential::ApiKey {
                    key: format!("{label}-key"),
                },
            )
            .await
            .expect("seed source-less account");
        }

        let statuses = collect_usage_from_sources(&auth, Vec::new()).await;
        let rows = statuses
            .iter()
            .filter(|status| status.provider_id == "openai")
            .collect::<Vec<_>>();
        assert_eq!(
            rows.iter()
                .filter_map(|status| status.account.as_deref())
                .collect::<Vec<_>>(),
            vec!["personal", "work"]
        );
        assert!(
            rows.iter()
                .all(|status| matches!(status.outcome, UsageOutcome::NoSource)),
            "each stored account retains the existing NoSource outcome: {rows:?}"
        );
        assert!(
            rows.iter().all(|status| status.account.is_some()),
            "the labeled set must not collapse to one provider row: {rows:?}"
        );
    }

    #[tokio::test]
    async fn all_provider_discovery_completes_before_any_account_fetch() {
        let dir = TempDir::with_prefix("aj-usage-discovery-barrier-").expect("create temp dir");
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), Default::default());
        for provider_id in ["anthropic", "openai-codex"] {
            auth.insert_account(
                provider_id,
                "work",
                AuthCredential::ApiKey {
                    key: format!("{provider_id}-key"),
                },
            )
            .await
            .expect("seed provider account");
        }

        let blocked_discovery_started = Arc::new(tokio::sync::Notify::new());
        let release_discovery = Arc::new(tokio::sync::Notify::new());
        let stalled_fetch_started = Arc::new(tokio::sync::Notify::new());
        let release_stalled_fetch = Arc::new(tokio::sync::Notify::new());
        let fresh_fetch_finished = Arc::new(tokio::sync::Notify::new());
        let sources: Vec<Arc<dyn UsageSource>> = vec![
            Arc::new(CoordinatedUsageSource {
                provider_id: "anthropic",
                started: Arc::clone(&stalled_fetch_started),
                release: Some(Arc::clone(&release_stalled_fetch)),
            }),
            Arc::new(CoordinatedUsageSource {
                provider_id: "openai-codex",
                started: Arc::clone(&fresh_fetch_finished),
                release: None,
            }),
        ];
        let discovering = {
            let auth = auth.clone();
            let blocked_discovery_started = Arc::clone(&blocked_discovery_started);
            let release_discovery = Arc::clone(&release_discovery);
            tokio::spawn(async move {
                collect_usage_with_discovery(&auth, sources, move |auth, provider_id| {
                    let blocked_discovery_started = Arc::clone(&blocked_discovery_started);
                    let release_discovery = Arc::clone(&release_discovery);
                    async move {
                        if provider_id == "openai-codex" {
                            blocked_discovery_started.notify_one();
                            release_discovery.notified().await;
                        }
                        auth.accounts(&provider_id).await
                    }
                })
                .await
            })
        };

        blocked_discovery_started.notified().await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                stalled_fetch_started.notified(),
            )
            .await
            .is_err(),
            "no source fetch starts while another provider is still discovering accounts"
        );

        release_discovery.notify_one();
        stalled_fetch_started.notified().await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fresh_fetch_finished.notified(),
        )
        .await
        .expect("the fresh provider uses its retained snapshot while its sibling stalls");
        release_stalled_fetch.notify_one();
        let statuses = discovering.await.expect("collector task");
        assert!(
            statuses.iter().any(|status| {
                status.provider_id == "anthropic" && status.account.as_deref() == Some("work")
            }) && statuses.iter().any(|status| {
                status.provider_id == "openai-codex" && status.account.as_deref() == Some("work")
            }),
            "both discovered account rows survive: {statuses:?}"
        );
    }

    #[tokio::test]
    async fn collect_fetches_every_labeled_account_independently() {
        let (_dir, auth) = two_account_auth("two-accounts").await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let statuses =
            collect_usage_from_sources(&auth, vec![fake_source(Arc::clone(&calls), None, None)])
                .await;

        let rows = account_statuses(&statuses);
        assert_eq!(
            rows.iter()
                .filter_map(|status| status.account.as_deref())
                .collect::<Vec<_>>(),
            vec!["personal", "work"],
            "a labeled set contributes one stable row per account"
        );
        let reports: Vec<(f64, &str, Option<u32>, Option<&str>)> = rows
            .iter()
            .map(|status| match &status.outcome {
                UsageOutcome::Usage(usage) => (
                    usage.windows[0].used,
                    usage.notes[0].as_str(),
                    usage.reset_offer.as_ref().map(ResetCreditOffer::available),
                    usage
                        .reset_offer
                        .as_ref()
                        .and_then(|offer| offer.target().account()),
                ),
                other => panic!("expected per-account usage, got {other:?}"),
            })
            .collect();
        assert_eq!(
            reports,
            vec![
                (0.25, "personal note", Some(1), Some("personal")),
                (0.75, "work note", Some(2), Some("work"))
            ],
            "each account retains its complete windows, notes, reset credits, and action identity"
        );
        let mut fetched = calls.lock().expect("calls mutex").clone();
        fetched.sort();
        assert_eq!(
            fetched,
            vec![Some("personal".to_string()), Some("work".to_string())],
            "the source receives each account label rather than only the store default"
        );
    }

    #[tokio::test]
    async fn runtime_override_keeps_its_provider_row_beside_labeled_accounts() {
        let (_dir, auth) = two_account_auth("runtime-override").await;
        auth.set_runtime_api_key("anthropic", "override-key".to_string())
            .await;
        let calls = Arc::new(Mutex::new(Vec::new()));

        let statuses =
            collect_usage_from_sources(&auth, vec![fake_source(Arc::clone(&calls), None, None)])
                .await;
        let rows = account_statuses(&statuses);
        assert_eq!(
            rows.iter()
                .map(|status| status.account.as_deref())
                .collect::<Vec<_>>(),
            vec![None, Some("personal"), Some("work")],
            "the effective override stays provider-level beside stored inventory"
        );
        let mut fetched = calls.lock().expect("calls mutex").clone();
        fetched.sort();
        assert_eq!(
            fetched,
            vec![None, Some("personal".to_string()), Some("work".to_string()),],
            "None preserves the ordinary override resolution path"
        );
    }

    #[tokio::test]
    async fn one_account_error_preserves_its_siblings_usage_row() {
        let (_dir, auth) = two_account_auth("sibling-error").await;
        let statuses = collect_usage_from_sources(
            &auth,
            vec![fake_source(
                Arc::new(Mutex::new(Vec::new())),
                Some("work"),
                None,
            )],
        )
        .await;
        let rows = account_statuses(&statuses);

        let personal = rows
            .iter()
            .find(|status| status.account.as_deref() == Some("personal"))
            .expect("one account's error must not blank its sibling row");
        assert!(
            matches!(&personal.outcome, UsageOutcome::Usage(usage) if usage.windows[0].used == 0.25),
            "the sibling account's usage numbers survive: {:?}",
            personal.outcome
        );
        let work = rows
            .iter()
            .find(|status| status.account.as_deref() == Some("work"))
            .expect("the failing account keeps its own row");
        assert!(
            matches!(&work.outcome, UsageOutcome::Error(message) if message == "work failed"),
            "the error belongs only to the account that failed: {:?}",
            work.outcome
        );
    }

    #[tokio::test(start_paused = true)]
    async fn labeled_account_fetches_run_concurrently() {
        let (_dir, auth) = two_account_auth("concurrent").await;
        let started = tokio::time::Instant::now();
        let statuses = collect_usage_from_sources(
            &auth,
            vec![fake_source(
                Arc::new(Mutex::new(Vec::new())),
                None,
                Some((1, 2)),
            )],
        )
        .await;

        assert_eq!(
            tokio::time::Instant::now().duration_since(started),
            std::time::Duration::from_secs(2),
            "two account fetches take the slower fetch, not their sum"
        );
        assert_eq!(
            account_statuses(&statuses).len(),
            2,
            "both delayed account fetches completed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn each_account_fetch_has_its_own_timeout_and_row() {
        let (_dir, auth) = two_account_auth("timeout").await;
        let started = tokio::time::Instant::now();
        let statuses = collect_usage_from_sources(
            &auth,
            vec![fake_source(
                Arc::new(Mutex::new(Vec::new())),
                None,
                Some((1, 20)),
            )],
        )
        .await;
        let rows = account_statuses(&statuses);

        assert_eq!(
            tokio::time::Instant::now().duration_since(started),
            SOURCE_TIMEOUT,
            "the slow account is capped by its own timeout"
        );
        let personal = rows
            .iter()
            .find(|status| status.account.as_deref() == Some("personal"))
            .expect("the fast sibling keeps its row");
        assert!(matches!(&personal.outcome, UsageOutcome::Usage(_)));
        let work = rows
            .iter()
            .find(|status| status.account.as_deref() == Some("work"))
            .expect("the timed-out account keeps its row");
        assert!(
            matches!(&work.outcome, UsageOutcome::Error(message) if message == "timed out"),
            "the timeout belongs to the slow account: {:?}",
            work.outcome
        );
    }
}
