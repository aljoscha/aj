//! Authorizing with a provider on this machine for a login the session's
//! host stores.
//!
//! The browser is where the user sits, so the OAuth flow runs here whether
//! the host is this process or a remote one. Only the finished credentials
//! travel, as a `store` mutation the host commits under its own lock.

use aj_models::auth::DEFAULT_ACCOUNT_LABEL;
use aj_models::oauth::{OAuthCallbacks, OAuthError, OAuthProvider};
use aj_wire::{CredentialMutation, CredentialStore, StoredCredentialMetadata};
use async_trait::async_trait;

/// The storage intent selected before authorization starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginTarget {
    /// Insert a credential. `existing` are the labels the host already holds
    /// for the provider (see [`LoginTarget::new_account`]); a label is asked
    /// for when there are any.
    NewAccount { existing: Vec<String> },
    /// Replace the selected bare credential or exact labeled account.
    ExistingAccount(Option<String>),
}

impl LoginTarget {
    /// A new account next to whatever the host currently holds. A bare
    /// credential occupies the unnamed label.
    pub fn new_account(stored: Option<&StoredCredentialMetadata>) -> Self {
        let existing = match stored {
            None => Vec::new(),
            Some(StoredCredentialMetadata::Bare) => vec![DEFAULT_ACCOUNT_LABEL.to_string()],
            Some(StoredCredentialMetadata::Accounts { accounts, .. }) => accounts.clone(),
        };
        Self::NewAccount { existing }
    }
}

/// Presentation hooks for a login, including account naming.
#[async_trait]
pub trait LoginCallbacks: OAuthCallbacks {
    /// Ask for a new account's label. `existing` are the host's stored labels,
    /// offered as collision guidance. The host validates the label again under
    /// its write lock.
    async fn prompt_account_label(&self, existing: &[String]) -> Result<String, OAuthError>;
}

/// Run `provider`'s OAuth flow and describe how the host should store the
/// result.
///
/// A duplicate label is refused before the browser opens, since the host
/// would refuse it afterwards anyway. Dropping this future abandons the
/// authorization with nothing stored anywhere.
pub async fn login(
    provider: &dyn OAuthProvider,
    target: LoginTarget,
    callbacks: &dyn LoginCallbacks,
) -> Result<CredentialMutation, OAuthError> {
    let target = match target {
        LoginTarget::NewAccount { existing } if existing.is_empty() => {
            CredentialStore::New { label: None }
        }
        LoginTarget::NewAccount { existing } => {
            let label = callbacks.prompt_account_label(&existing).await?;
            if existing.contains(&label) {
                return Err(OAuthError::Other(format!(
                    "an account named {label:?} already exists"
                )));
            }
            CredentialStore::New { label: Some(label) }
        }
        LoginTarget::ExistingAccount(label) => CredentialStore::Replace { label },
    };
    let credentials = provider.login(callbacks).await?;
    Ok(CredentialMutation::Store {
        provider: provider.id().to_string(),
        target,
        credentials,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_models::oauth::{OAuthAuthInfo, OAuthCredentials};
    use std::sync::Mutex;

    struct Provider {
        logins: Mutex<usize>,
    }
    #[async_trait]
    impl OAuthProvider for Provider {
        fn id(&self) -> &str {
            "fake"
        }
        fn name(&self) -> &str {
            "Fake"
        }
        async fn login(&self, _: &dyn OAuthCallbacks) -> Result<OAuthCredentials, OAuthError> {
            *self.logins.lock().unwrap() += 1;
            Ok(OAuthCredentials::new("r", "a", i64::MAX))
        }
        async fn refresh_token(
            &self,
            _: &OAuthCredentials,
        ) -> Result<OAuthCredentials, OAuthError> {
            unreachable!()
        }
    }

    struct Callbacks {
        label: &'static str,
        prompted: Mutex<Vec<Vec<String>>>,
    }
    #[async_trait]
    impl OAuthCallbacks for Callbacks {
        fn on_auth(&self, _: OAuthAuthInfo<'_>) {}
        async fn on_prompt(&self, _: &str) -> Result<String, OAuthError> {
            unreachable!()
        }
    }
    #[async_trait]
    impl LoginCallbacks for Callbacks {
        async fn prompt_account_label(&self, existing: &[String]) -> Result<String, OAuthError> {
            self.prompted.lock().unwrap().push(existing.to_vec());
            Ok(self.label.to_string())
        }
    }

    fn callbacks(label: &'static str) -> Callbacks {
        Callbacks {
            label,
            prompted: Mutex::new(Vec::new()),
        }
    }

    #[tokio::test]
    async fn first_login_is_bare_and_later_ones_are_labelled() {
        let provider = Provider {
            logins: Mutex::new(0),
        };
        let cb = callbacks("work");
        let first = login(&provider, LoginTarget::new_account(None), &cb)
            .await
            .unwrap();
        assert!(matches!(
            first,
            CredentialMutation::Store {
                target: CredentialStore::New { label: None },
                ..
            }
        ));
        assert!(cb.prompted.lock().unwrap().is_empty());

        let bare = StoredCredentialMetadata::Bare;
        let second = login(&provider, LoginTarget::new_account(Some(&bare)), &cb)
            .await
            .unwrap();
        assert!(matches!(
            second,
            CredentialMutation::Store { target: CredentialStore::New { label: Some(label) }, .. }
                if label == "work"
        ));
        assert_eq!(
            *cb.prompted.lock().unwrap(),
            [vec![DEFAULT_ACCOUNT_LABEL.to_string()]]
        );

        let replace = login(
            &provider,
            LoginTarget::ExistingAccount(Some("work".into())),
            &cb,
        )
        .await
        .unwrap();
        assert!(matches!(
            replace,
            CredentialMutation::Store { target: CredentialStore::Replace { label: Some(label) }, .. }
                if label == "work"
        ));
        assert_eq!(cb.prompted.lock().unwrap().len(), 1);
        assert_eq!(*provider.logins.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn duplicate_label_is_refused_before_the_browser_opens() {
        let provider = Provider {
            logins: Mutex::new(0),
        };
        let stored = StoredCredentialMetadata::Accounts {
            default: "work".into(),
            accounts: vec!["work".into(), "home".into()],
        };
        let err = login(
            &provider,
            LoginTarget::new_account(Some(&stored)),
            &callbacks("home"),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("home"), "{err}");
        assert_eq!(*provider.logins.lock().unwrap(), 0);
    }
}
