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
/// Each legacy window is `None` when the server omits or nulls it
/// (e.g. a plan without a separate Opus limit). Responses can also
/// include `limits`, which is the preferred source when present because
/// it carries provider-defined scopes without baking model names into
/// the client.
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
    /// Provider-defined rate-limit windows.
    #[serde(default)]
    pub limits: Option<Vec<OAuthUsageLimit>>,
    /// Pay-as-you-go usage credits beyond the plan limits.
    #[serde(default)]
    pub extra_usage: Option<OAuthExtraUsage>,
    /// Usage-credit status shape.
    #[serde(default)]
    pub spend: Option<OAuthSpend>,
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

/// One provider-defined rate-limit window.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthUsageLimit {
    /// Provider grouping, e.g. `"session"` or `"weekly"`.
    #[serde(default)]
    pub group: Option<String>,
    /// Provider-defined window kind, e.g. `"weekly_scoped"`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Percentage of the window used, 0–100.
    #[serde(default)]
    pub percent: Option<f64>,
    /// ISO 8601 timestamp when the window resets.
    #[serde(default)]
    pub resets_at: Option<String>,
    /// Scope that further qualifies this limit.
    #[serde(default)]
    pub scope: Option<OAuthUsageLimitScope>,
    /// Provider severity, e.g. `"normal"` or `"critical"`.
    #[serde(default)]
    pub severity: Option<String>,
    /// Whether this is the currently active limit.
    #[serde(default)]
    pub is_active: Option<bool>,
}

/// Scope metadata for a provider-defined rate-limit window.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthUsageLimitScope {
    /// Model scope, when the limit applies to one model family.
    #[serde(default)]
    pub model: Option<OAuthUsageLimitModel>,
    /// Surface scope, when supplied by the provider.
    #[serde(default)]
    pub surface: Option<String>,
}

/// Model metadata for a provider-defined rate-limit scope.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthUsageLimitModel {
    /// Display name supplied by the provider.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Provider model id, when supplied.
    #[serde(default)]
    pub id: Option<String>,
}

/// Usage-credit status shape.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthSpend {
    /// Whether usage credits are enabled.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Provider reason why credits are disabled.
    #[serde(default)]
    pub disabled_reason: Option<String>,
    /// Percentage of the spend cap used, 0–100.
    #[serde(default)]
    pub percent: Option<f64>,
    /// Credits spent this period.
    #[serde(default)]
    pub used: Option<OAuthMoney>,
    /// Configured spend limit.
    #[serde(default)]
    pub limit: Option<OAuthMoney>,
    /// Spend cap, when distinct from `limit`.
    #[serde(default)]
    pub cap: Option<OAuthMoney>,
    /// Remaining balance, when reported.
    #[serde(default)]
    pub balance: Option<OAuthMoney>,
    /// Provider severity for the spend state.
    #[serde(default)]
    pub severity: Option<String>,
}

/// Money amount reported by Anthropic.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthMoney {
    /// Amount in the currency's minor unit.
    #[serde(default)]
    pub amount_minor: Option<f64>,
    /// ISO 4217 currency code, e.g. `"USD"`.
    #[serde(default)]
    pub currency: Option<String>,
    /// Number of minor-unit decimal places.
    #[serde(default)]
    pub exponent: Option<u32>,
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
    /// Provider reason why credits are disabled.
    #[serde(default)]
    pub disabled_reason: Option<String>,
    /// Number of minor-unit decimal places.
    #[serde(default)]
    pub decimal_places: Option<u32>,
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
            },
            "limits": [
                {
                    "group": "session",
                    "kind": "session",
                    "percent": 12.5,
                    "resets_at": "2026-06-10T17:00:00+00:00",
                    "scope": null,
                    "severity": "normal",
                    "is_active": false
                },
                {
                    "group": "weekly",
                    "kind": "weekly_scoped",
                    "percent": 100,
                    "resets_at": "2026-06-15T09:00:00+00:00",
                    "scope": {
                        "model": {"display_name": "Fable", "id": null},
                        "surface": null
                    },
                    "severity": "critical",
                    "is_active": true
                }
            ],
            "spend": {
                "enabled": false,
                "disabled_reason": "out_of_credits",
                "percent": 0,
                "used": {"amount_minor": 0, "currency": "USD", "exponent": 2}
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
        let limits = usage.limits.unwrap();
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[1].kind.as_deref(), Some("weekly_scoped"));
        assert_eq!(
            limits[1]
                .scope
                .as_ref()
                .and_then(|scope| scope.model.as_ref())
                .and_then(|model| model.display_name.as_deref()),
            Some("Fable")
        );
        let spend = usage.spend.unwrap();
        assert_eq!(spend.disabled_reason.as_deref(), Some("out_of_credits"));
        assert_eq!(spend.used.unwrap().amount_minor, Some(0.0));
    }

    /// An empty object — the server's "no data" shape — parses to all
    /// `None`s instead of erroring.
    #[test]
    fn parses_empty_response() {
        let usage: OAuthUsage = serde_json::from_str("{}").unwrap();
        assert!(usage.five_hour.is_none());
        assert!(usage.limits.is_none());
        assert!(usage.extra_usage.is_none());
    }
}
