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

use aj_models::auth::AuthStorage;
use aj_models::usage::{ProviderUsage, UsageReport, default_usage_sources};

/// Per-source timeout. The Anthropic source's HTTP request already
/// caps itself at 5 s; this outer bound also covers credential
/// resolution (an OAuth refresh round-trip) so one stuck source can't
/// hold the whole page in its loading state.
const SOURCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One provider's resolved usage status, ready to render.
#[derive(Debug, Clone)]
pub struct ProviderUsageStatus {
    pub provider_id: String,
    pub outcome: UsageOutcome,
}

/// What the `/usage` page shows for one provider.
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
const KNOWN_PROVIDERS: &[&str] = &["anthropic", "openai", "openai-codex"];

/// Fetch usage from every registered source concurrently and append
/// "no usage source" rows for the remaining known providers. Rows
/// are sorted by provider id for a stable display order.
pub async fn collect_usage(auth: &AuthStorage) -> Vec<ProviderUsageStatus> {
    let sources = default_usage_sources();

    let mut tasks = tokio::task::JoinSet::new();
    for source in &sources {
        let source = Arc::clone(source);
        let auth = auth.clone();
        tasks.spawn(async move {
            let outcome = match tokio::time::timeout(SOURCE_TIMEOUT, source.fetch(&auth)).await {
                Ok(Ok(UsageReport::Usage(usage))) => UsageOutcome::Usage(usage),
                Ok(Ok(UsageReport::Unsupported { reason })) => UsageOutcome::Unsupported { reason },
                Ok(Ok(UsageReport::NotConfigured)) => UsageOutcome::NotConfigured,
                Ok(Err(err)) => UsageOutcome::Error(err.to_string()),
                Err(_) => UsageOutcome::Error("timed out".to_string()),
            };
            ProviderUsageStatus {
                provider_id: source.provider_id().to_string(),
                outcome,
            }
        });
    }

    let mut statuses: Vec<ProviderUsageStatus> = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(status) => statuses.push(status),
            Err(err) => tracing::warn!("usage fetch task panicked: {err}"),
        }
    }

    for id in KNOWN_PROVIDERS {
        if !statuses.iter().any(|s| s.provider_id == *id) {
            statuses.push(ProviderUsageStatus {
                provider_id: id.to_string(),
                outcome: UsageOutcome::NoSource,
            });
        }
    }

    statuses.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    statuses
}

/// Render a window's status, e.g. `"12% used · resets 17:00 (UTC+2)"`.
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
/// the machine's UTC offset appended: `"17:00 (UTC+2)"` within the
/// same day, `"Mon 09:00 (UTC+2)"` within a week, `"Jun 15 (UTC+2)"`
/// beyond that, `"now"` when already past.
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

    // The offset is taken from the reset instant, not from now, so a
    // DST transition between the two renders the wall-clock time the
    // reset will actually happen at.
    let tz = utc_offset_label(reset.offset().local_minus_utc());
    if reset.date_naive() == now.date_naive() {
        format!("{} ({tz})", reset.format("%H:%M"))
    } else if reset_ms - now_ms < 7 * 24 * 3600 * 1000 {
        format!("{} ({tz})", reset.format("%a %H:%M"))
    } else {
        format!("{} {} ({tz})", month_abbrev(reset.month()), reset.day())
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
    use chrono::DateTime;

    use super::*;

    fn ms(dt: DateTime<Local>) -> i64 {
        dt.timestamp_millis()
    }

    /// The machine-local offset label for `dt`, so the exact-string
    /// assertions below stay portable across test machines in any
    /// timezone.
    fn tz(dt: DateTime<Local>) -> String {
        utc_offset_label(dt.offset().local_minus_utc())
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
        let dir = std::env::temp_dir().join(format!("aj-usage-collect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let auth = AuthStorage::with_providers(dir.join("auth.json"), Default::default());
        let statuses = collect_usage(&auth).await;
        let ids: Vec<&str> = statuses.iter().map(|s| s.provider_id.as_str()).collect();
        assert_eq!(ids, vec!["anthropic", "openai", "openai-codex"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
