//! Session-addressed operations on this host's credential store.
use aj_models::auth::AuthError;
use aj_wire::{
    CredentialMutation, CredentialOutcome, CredentialOverview, CredentialStore, OAuthProviderInfo,
};

use super::{HostError, SessionHost};

impl SessionHost {
    pub async fn credential_overview(
        &self,
        session: &str,
    ) -> Result<CredentialOverview, HostError> {
        self.validate_credential_session(session).await?;
        let auth = &self.inner.shared.auth;
        let mut stored = std::collections::BTreeMap::new();
        for provider in auth.list().await.map_err(|_| credential_read_error())? {
            use aj_models::auth::StoredProviderCredentials;
            use aj_wire::StoredCredentialMetadata;
            let metadata = match auth
                .stored_credentials(&provider)
                .await
                .map_err(|_| credential_read_error())?
            {
                Some(StoredProviderCredentials::Bare(_)) => StoredCredentialMetadata::Bare,
                Some(StoredProviderCredentials::Accounts(set)) => {
                    StoredCredentialMetadata::Accounts {
                        default: set.default,
                        accounts: set.accounts.into_iter().map(|(label, _)| label).collect(),
                    }
                }
                None => continue,
            };
            stored.insert(provider, metadata);
        }
        Ok(CredentialOverview {
            stored,
            oauth_providers: auth
                .oauth_provider_ids()
                .await
                .into_iter()
                .map(|(id, name)| OAuthProviderInfo { id, name })
                .collect(),
            statuses: crate::auth::collect_statuses(auth).await,
        })
    }

    /// Validate routing identity before touching credentials. The session is
    /// only an address: this neither resumes it nor takes its writer lock.
    async fn validate_credential_session(&self, session: &str) -> Result<(), HostError> {
        self.live_or_cold(session).await?;
        Ok(())
    }

    pub async fn mutate_credentials(
        &self,
        session: &str,
        mutation: CredentialMutation,
    ) -> Result<CredentialOutcome, HostError> {
        self.validate_credential_session(session).await?;
        let auth = &self.inner.shared.auth;
        let result = match mutation {
            CredentialMutation::Store {
                provider,
                target: CredentialStore::New { label },
                credentials,
            } => {
                auth.store_new_login(&provider, label.as_deref(), credentials)
                    .await
            }
            CredentialMutation::Store {
                provider,
                target: CredentialStore::Replace { label },
                credentials,
            } => {
                auth.store_replacement_login(&provider, label.as_deref(), credentials)
                    .await
            }
            CredentialMutation::LogoutBare { provider } => auth.remove_bare(&provider).await,
            CredentialMutation::Logout {
                provider,
                account_label,
            } => auth.remove_account(&provider, &account_label).await,
            CredentialMutation::SetDefault {
                provider,
                account_label,
            } => auth.set_default_account(&provider, &account_label).await,
            CredentialMutation::LogoutWithNewDefault {
                provider,
                account_label,
                new_default,
            } => {
                auth.remove_default_account(&provider, &account_label, &new_default)
                    .await
            }
            CredentialMutation::LogoutAll {
                provider,
                expected_accounts,
            } => {
                auth.remove_all_accounts(&provider, &expected_accounts)
                    .await
            }
        };
        Ok(outcome(result))
    }
}

fn credential_read_error() -> HostError {
    HostError::Unsupported("Could not read host credentials".into())
}

fn outcome(result: Result<(), AuthError>) -> CredentialOutcome {
    match result {
        Ok(()) => CredentialOutcome::Applied,
        Err(AuthError::RemovingDefault { provider, label }) => CredentialOutcome::RemovingDefault {
            provider,
            account_label: label,
        },
        Err(err) => {
            // Parser and provider errors may contain credential material. Only
            // storage errors whose display contract is metadata-only travel.
            let (code, message) = match &err {
                AuthError::UnknownProvider(_) => ("unknown_provider", err.to_string()),
                AuthError::UnknownAccount { .. } => ("unknown_account", err.to_string()),
                AuthError::InvalidLabel(_) => ("invalid_label", "Invalid account label".into()),
                AuthError::DuplicateAccount { .. } => ("duplicate_account", err.to_string()),
                AuthError::ProviderAlreadyConfigured { .. } => {
                    ("provider_configured", err.to_string())
                }
                AuthError::ProviderCredentialChanged { .. }
                | AuthError::ProviderAccountsChanged { .. } => {
                    ("credentials_changed", err.to_string())
                }
                _ => (
                    "credential_failure",
                    "Credential operation failed on the host".into(),
                ),
            };
            CredentialOutcome::Failed {
                code: code.into(),
                message,
            }
        }
    }
}
