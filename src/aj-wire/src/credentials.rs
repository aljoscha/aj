//! Credential metadata and store mutations for a session's host.
use aj_models::oauth::OAuthCredentials;
use serde::{Deserialize, Serialize};

pub const CREDENTIALS_CAPABILITY: &str = "credentials";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialOverview {
    pub oauth_providers: Vec<OAuthProviderInfo>,
    pub statuses: Vec<CredentialStatus>,
    /// Stored identities remain selectable even when a runtime override wins.
    pub stored: std::collections::BTreeMap<String, StoredCredentialMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredCredentialMetadata {
    Bare,
    Accounts {
        default: String,
        accounts: Vec<String>,
    },
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
    /// Exact account identity. None denotes a bare credential or provider source.
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
/// store's exact-label and storage-shape checks at lock time.
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
    LogoutBare {
        provider: String,
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
    /// `None` is a provider's first, bare credential. `Some(label)` adds an
    /// account, including the unnamed account for `Some("")`.
    New { label: Option<String> },
    /// `None` replaces the bare credential, `Some(label)` that exact account.
    Replace { label: Option<String> },
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
