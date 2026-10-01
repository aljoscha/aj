//! Credential metadata and store mutations for a session's host.
use aj_models::oauth::OAuthCredentials;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialOverview {
    pub oauth_providers: Vec<OAuthProviderInfo>,
    pub statuses: Vec<CredentialStatus>,
    /// Stored identities remain selectable even when a runtime override wins.
    pub stored: std::collections::BTreeMap<String, StoredCredentialMetadata>,
}

/// The account labels a provider stores, never their credentials. An empty
/// label is the unnamed account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCredentialMetadata {
    pub default: String,
    pub accounts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthProviderInfo {
    pub id: String,
    pub name: String,
}

/// One provider-level or labeled-account authentication row, ready to render.
/// Summaries describe the credential source, never bearer material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialStatus {
    pub provider_id: String,
    /// Host display label. Clients use `provider_id` when this is empty.
    #[serde(default)]
    pub provider_name: String,
    /// Exact stored account identity. None denotes a provider-level source
    /// such as a runtime override, an environment key, or nothing configured.
    pub account_label: Option<String>,
    /// Whether this account is the store default.
    pub is_default: bool,
    pub configured: bool,
    /// Short method/source label, such as subscription or an environment key name.
    pub summary: String,
    /// Secondary information such as an OAuth token's remaining lifetime.
    pub detail: Option<String>,
}

/// A mutation of the host's credential store. Every variant retains the
/// store's exact-label checks at lock time.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialMutation {
    /// Commit credentials the client obtained by running the provider's
    /// OAuth flow where the user's browser is.
    Store {
        provider: String,
        target: CredentialStore,
        credentials: OAuthCredentials,
    },
    Logout {
        provider: String,
        account_label: String,
    },
    SetDefault {
        provider: String,
        account_label: String,
    },
    LogoutWithNewDefault {
        provider: String,
        account_label: String,
        new_default: String,
    },
    LogoutAll {
        provider: String,
        expected_accounts: Vec<String>,
    },
}

/// Where a stored login lands. Creation is insert-only and replacement names
/// its exact target, so a login started for one account can never land on
/// another.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialStore {
    /// `None` is a provider's first login, stored as the unnamed account and
    /// refused if the provider gained a credential meanwhile. `Some(label)`
    /// adds an account, including the unnamed account for `Some("")`.
    New { label: Option<String> },
    /// Replace that exact account. An empty label is the unnamed account.
    Replace { label: String },
}

/// A storage refusal is an outcome, distinct from a failed transport whose
/// write outcome may be unknown. RemovingDefault lets callers offer resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CredentialOutcome {
    Applied,
    RemovingDefault {
        provider: String,
        account_label: String,
    },
    Failed {
        code: String,
        message: String,
    },
}

impl crate::request::Sealed for CredentialMutation {
    fn decode(body: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(body)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn status_names_accept_old_replies_and_roundtrip_host_labels() {
        let credential = serde_json::json!({
            "provider_id": "anthropic", "account_label": null,
            "is_default": false, "configured": true,
            "summary": "subscription", "detail": null
        });
        let usage = serde_json::json!({
            "provider_id": "anthropic", "account": null, "outcome": "NoSource"
        });
        let mut credential: super::CredentialStatus = serde_json::from_value(credential).unwrap();
        let mut usage: crate::ProviderUsageStatus = serde_json::from_value(usage).unwrap();
        assert!(credential.provider_name.is_empty());
        assert!(usage.provider_name.is_empty());
        credential.provider_name = "Anthropic subscription".into();
        usage.provider_name = "Anthropic subscription".into();
        assert_eq!(
            credential,
            serde_json::from_str(&serde_json::to_string(&credential).unwrap()).unwrap()
        );
        assert_eq!(
            usage,
            serde_json::from_str(&serde_json::to_string(&usage).unwrap()).unwrap()
        );
    }
}
