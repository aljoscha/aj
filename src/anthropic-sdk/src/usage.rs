//! Wire types for the `GET /api/oauth/usage` endpoint.
//!
//! The endpoint reports plan rate-limit utilization for Claude.ai
//! subscription (OAuth) accounts — the data behind Claude Code's
//! `/usage` page. It is undocumented and only answers OAuth bearer
//! tokens; plain API keys get a 401.
//!
//! Because the shape is unofficial and may change without notice,
//! every field is optional and unknown fields are ignored, so a
//! server-side addition degrades to "window not shown" rather than a
//! parse failure.

use serde::Deserialize;

/// Response body of `GET /api/oauth/usage`.
///
/// Each window is `None` when the server omits or nulls it (e.g. a
/// plan without a separate Opus limit). The one-time promotional
/// credit window (`cinder_cove`) is deliberately not modeled.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthUsage {
    /// Rolling five-hour session window.
    #[serde(default)]
    pub five_hour: Option<OAuthUsageWindow>,
    /// Rolling seven-day window across all models.
    #[serde(default)]
    pub seven_day: Option<OAuthUsageWindow>,
    /// Seven-day window for third-party OAuth apps.
    #[serde(default)]
    pub seven_day_oauth_apps: Option<OAuthUsageWindow>,
    /// Seven-day window for Opus-class models.
    #[serde(default)]
    pub seven_day_opus: Option<OAuthUsageWindow>,
    /// Seven-day window for Sonnet-class models.
    #[serde(default)]
    pub seven_day_sonnet: Option<OAuthUsageWindow>,
    /// Pay-as-you-go usage credits beyond the plan limits.
    #[serde(default)]
    pub extra_usage: Option<OAuthExtraUsage>,
}

/// One rate-limit window.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthUsageWindow {
    /// Percentage of the window used, 0–100.
    #[serde(default)]
    pub utilization: Option<f64>,
    /// ISO 8601 timestamp when the window resets.
    #[serde(default)]
    pub resets_at: Option<String>,
}

/// Usage-credit (overage) state.
///
/// Money fields are in cents of `currency`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthExtraUsage {
    /// Whether usage credits are enabled for this account.
    #[serde(default)]
    pub is_enabled: Option<bool>,
    /// Monthly credit limit in cents; `None` means unlimited.
    #[serde(default)]
    pub monthly_limit: Option<f64>,
    /// Credits spent this month, in cents.
    #[serde(default)]
    pub used_credits: Option<f64>,
    /// Percentage of the monthly limit used, 0–100.
    #[serde(default)]
    pub utilization: Option<f64>,
    /// ISO 4217 currency code, e.g. `"USD"`.
    #[serde(default)]
    pub currency: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic response parses, including windows we don't model
    /// (`cinder_cove`) and null members.
    #[test]
    fn parses_full_response() {
        let json = r#"{
            "five_hour": {"utilization": 12.5, "resets_at": "2026-06-10T17:00:00+00:00"},
            "seven_day": {"utilization": 34.0, "resets_at": "2026-06-15T09:00:00+00:00"},
            "seven_day_opus": null,
            "seven_day_sonnet": {"utilization": null, "resets_at": null},
            "cinder_cove": {"utilization": 0, "resets_at": null},
            "extra_usage": {
                "is_enabled": true,
                "monthly_limit": 5000,
                "used_credits": 123.0,
                "utilization": 2.46,
                "currency": "USD"
            }
        }"#;
        let usage: OAuthUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.five_hour.as_ref().unwrap().utilization, Some(12.5));
        assert!(usage.seven_day_opus.is_none());
        let sonnet = usage.seven_day_sonnet.unwrap();
        assert!(sonnet.utilization.is_none());
        let extra = usage.extra_usage.unwrap();
        assert_eq!(extra.is_enabled, Some(true));
        assert_eq!(extra.monthly_limit, Some(5000.0));
    }

    /// An empty object — the server's "no data" shape — parses to all
    /// `None`s instead of erroring.
    #[test]
    fn parses_empty_response() {
        let usage: OAuthUsage = serde_json::from_str("{}").unwrap();
        assert!(usage.five_hour.is_none());
        assert!(usage.extra_usage.is_none());
    }
}
