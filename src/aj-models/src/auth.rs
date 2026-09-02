//! Auth storage & API-key resolution.
//!
//! [`AuthStorage`] is the single entry point both the CLI (`aj /login`,
//! flag plumbing) and the agent (per-request key fetch) hit when they
//! need a provider's bearer token. It owns:
//!
//! - **Persistence.** Credentials live in `~/.aj/auth.json` keyed by provider.
//!   Each value is either one bare credential or a labeled account set. Each
//!   mutation runs under a sidecar lockfile so two `aj` processes cannot
//!   clobber each other's writes when refreshing tokens at the same time.
//! - **Runtime overrides.** A CLI `--api-key` flag bypasses the file
//!   entirely; that path lives in memory and is never written.
//! - **OAuth provider registry.** The two OAuth flows we ship
//!   ([`AnthropicOAuth`], [`OpenAIOAuth`]) are looked up by id when a
//!   refresh is needed, so the storage layer can mint new access
//!   tokens without the caller knowing about provider specifics.
//! - **Resolution chain.** [`AuthStorage::get_api_key`] walks the priority list:
//!   runtime override, then the selected stored credential (bare or account,
//!   API key or auto-refreshed OAuth), then environment variables. A stored
//!   credential wins over the environment, so a deliberate login stays
//!   authoritative and a stray exported key cannot shadow it. The explicit
//!   per-run override is the runtime `--api-key`.
//!
//! Every on-disk provider value is a `{ "type": "...", ... }` discriminated
//! union, so bare credentials and account sets remain explicit and migrations
//! stay simple.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(any(test, feature = "test-support"))]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::oauth::anthropic::AnthropicOAuth;
use crate::oauth::openai::OpenAIOAuth;
use crate::oauth::{OAuthCallbacks, OAuthCredentials, OAuthError, OAuthProvider, now_unix_ms};

mod account_label;
pub use account_label::{
    ACCOUNT_LABEL_UNICODE_VERSION, AccountLabelDisplayMode, AccountLabelValidationError,
    MAX_ACCOUNT_LABEL_BYTES, display_account_label, validate_account_label,
    validate_account_label_edit,
};

// ---------------------------------------------------------------------------
// On-disk shape
// ---------------------------------------------------------------------------

/// A single credential entry in `auth.json`.
///
/// Internally-tagged so the JSON object carries a `"type"` field
/// alongside the variant's payload. For the OAuth variant the inner
/// [`OAuthCredentials`] fields are flattened into the same object,
/// matching the disk layout
/// (`{ "type": "oauth", "refresh": ..., "access": ..., "expires": ..., ...extra }`).
/// Its [`Debug`](std::fmt::Debug) output preserves the credential variant but
/// never includes bearer material.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AuthCredential {
    /// Static API key the user pasted in (no refresh logic).
    #[serde(rename = "api_key")]
    ApiKey {
        /// Raw key string sent as the provider's bearer token.
        key: String,
    },
    /// OAuth-issued tokens with refresh capability.
    #[serde(rename = "oauth")]
    OAuth(OAuthCredentials),
}

impl std::fmt::Debug for AuthCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey { .. } => f
                .debug_struct("ApiKey")
                .field("key", &"<redacted>")
                .finish(),
            Self::OAuth(credentials) => f.debug_tuple("OAuth").field(credentials).finish(),
        }
    }
}

/// The label a bare entry's credential adopts when a second account
/// joins its provider and the entry becomes a labeled set.
pub const DEFAULT_ACCOUNT_LABEL: &str = "default";

/// A provider's slot in `auth.json`: one bare credential, or a labeled
/// set of credentials with a store-default label.
///
/// The bare variants share the `"type"` tag space and the exact bytes of
/// [`AuthCredential`], so every pre-accounts file parses unchanged and a
/// single login still writes the shape it always did. The set adds one
/// tag:
/// `{ "type": "accounts", "default": "personal", "accounts": { "personal": {...} } }`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum StoredEntry {
    #[serde(rename = "api_key")]
    ApiKey { key: String },
    #[serde(rename = "oauth")]
    OAuth(OAuthCredentials),
    #[serde(rename = "accounts")]
    Accounts {
        /// The label unlabeled resolution uses. Every writer keeps it
        /// naming a key of `accounts`; a hand-edited file that breaks
        /// the invariant resolves like a missing account rather than
        /// failing the whole parse, so one damaged entry cannot take
        /// down every provider's credentials.
        default: String,
        accounts: HashMap<String, AuthCredential>,
    },
}

// Unlike the account-set variant, the bare API-key variant does not compose
// through `AuthCredential`, so this storage leaf owns the same redaction.
impl std::fmt::Debug for StoredEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey { .. } => f
                .debug_struct("ApiKey")
                .field("key", &"<redacted>")
                .finish(),
            Self::OAuth(credentials) => f.debug_tuple("OAuth").field(credentials).finish(),
            Self::Accounts { default, accounts } => f
                .debug_struct("Accounts")
                .field("default", default)
                .field("accounts", accounts)
                .finish(),
        }
    }
}

impl StoredEntry {
    fn from_credential(credential: AuthCredential) -> Self {
        match credential {
            AuthCredential::ApiKey { key } => Self::ApiKey { key },
            AuthCredential::OAuth(creds) => Self::OAuth(creds),
        }
    }

    /// Resolve `label` to a credential. `None` means the bare credential
    /// or a set's default account; `Some` only ever matches inside a set,
    /// so a labeled ask against a bare entry is a miss, never a silent
    /// answer from a credential the caller did not name.
    fn resolve(&self, label: Option<&str>) -> Option<AuthCredential> {
        match (self, label) {
            (Self::ApiKey { key }, None) => Some(AuthCredential::ApiKey { key: key.clone() }),
            (Self::OAuth(creds), None) => Some(AuthCredential::OAuth(creds.clone())),
            (Self::ApiKey { .. } | Self::OAuth(_), Some(_)) => None,
            (Self::Accounts { default, accounts }, label) => {
                accounts.get(label.unwrap_or(default)).cloned()
            }
        }
    }

    /// The concrete slot `label` resolves against, recorded before any
    /// await so a refresh writes back to the slot it read, not to
    /// wherever the default points by the time the write happens.
    fn slot(&self, label: Option<&str>) -> Slot {
        match self {
            Self::ApiKey { .. } | Self::OAuth(_) => Slot::Bare,
            Self::Accounts { default, .. } => Slot::Account(label.unwrap_or(default).to_string()),
        }
    }
}

/// Where a resolved credential lives in its provider's entry.
#[derive(Clone, Debug)]
enum Slot {
    /// The provider's bare (unlabeled) entry.
    Bare,
    /// One account of a labeled set.
    Account(String),
}

/// A credential [`AuthStorage::get_api_key`] resolved, and where it came
/// from.
///
/// The source travels with the key because the store is the only place
/// that knows it. An unlabeled ask against a labeled set resolves that
/// set's default, and no caller can name which label that was.
pub struct ResolvedCredential {
    /// The bearer token to send.
    pub key: String,
    /// Which credential answered.
    pub source: CredentialSource,
}

// Written out rather than derived: a derived `Debug` prints the bearer
// token, and one `tracing::debug!` on this type anywhere downstream
// would put a live credential in a log file.
impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("key", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

/// Which of the resolution chain's sources answered.
///
/// Distinct from [`Slot`], which names where a REFRESH writes back and
/// so exists only for stored credentials. These four are what a caller
/// asks about after the fact, and two of them are not in the file at
/// all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// The runtime `--api-key` override. Carries no account identity:
    /// it is an operator instruction for this run, not a credential the
    /// store holds.
    Override,
    /// The provider's bare, unlabeled entry, which is every
    /// pre-feature `auth.json`.
    Bare,
    /// One labeled account of the provider's set, named.
    Account(String),
    /// An environment variable. Reached by unlabeled asks only, and
    /// carries no account identity either.
    Environment,
}

impl CredentialSource {
    /// The account label, for the one source that has one.
    ///
    /// `None` covers three different situations and a caller that needs
    /// to tell them apart must match on the variant instead.
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Account(label) => Some(label),
            Self::Override | Self::Bare | Self::Environment => None,
        }
    }
}

impl Slot {
    /// The source a credential read from this slot came from.
    fn source(&self) -> CredentialSource {
        match self {
            Self::Bare => CredentialSource::Bare,
            Self::Account(label) => CredentialSource::Account(label.clone()),
        }
    }
}

/// A labeled set's contents, for surfaces that render per account.
///
/// `None` from [`AuthStorage::accounts`] means the provider holds a bare
/// credential or nothing; [`AuthStorage::get`] distinguishes those two.
#[derive(Debug, Clone)]
pub struct ProviderAccounts {
    /// The label unlabeled resolution uses.
    pub default: String,
    /// Every account, sorted by label for stable display.
    pub accounts: Vec<(String, AuthCredential)>,
}

/// The complete credential shape stored for one provider.
///
/// Account-management surfaces need the whole shape rather than the one
/// credential [`AuthStorage::get`] resolves. Bare credentials remain unlabeled
/// for backward compatibility, while account sets retain every exact raw key.
#[derive(Debug, Clone)]
pub enum StoredProviderCredentials {
    /// The pre-accounts shape, with no account label.
    Bare(AuthCredential),
    /// A labeled account set and its current default.
    Accounts(ProviderAccounts),
}

/// In-memory shape of the entire `auth.json` file.
type AuthData = HashMap<String, StoredEntry>;

/// Check an absent-key account insertion against the lexical contract and the
/// provider's prospective namespace without mutating either one.
fn check_account_insert(data: &AuthData, provider_id: &str, label: &str) -> Result<(), AuthError> {
    let duplicate = match data.get(provider_id) {
        Some(StoredEntry::Accounts { accounts, .. }) => accounts.contains_key(label),
        Some(StoredEntry::ApiKey { .. } | StoredEntry::OAuth(_)) => label == DEFAULT_ACCOUNT_LABEL,
        None => false,
    };
    if duplicate {
        return Err(AuthError::DuplicateAccount {
            provider: provider_id.to_string(),
            label: label.to_string(),
        });
    }
    validate_account_label(label).map_err(|err| AuthError::InvalidLabel(err.to_string()))?;
    Ok(())
}

/// Apply an insertion already approved by [`check_account_insert`].
fn insert_account_entry(
    existing: Option<StoredEntry>,
    label: &str,
    credential: AuthCredential,
) -> StoredEntry {
    match existing {
        None => StoredEntry::Accounts {
            default: label.to_string(),
            accounts: HashMap::from([(label.to_string(), credential)]),
        },
        Some(StoredEntry::Accounts {
            default,
            mut accounts,
        }) => {
            accounts.insert(label.to_string(), credential);
            StoredEntry::Accounts { default, accounts }
        }
        Some(bare) => {
            let existing = bare.resolve(None).expect("a bare entry resolves unlabeled");
            StoredEntry::Accounts {
                default: DEFAULT_ACCOUNT_LABEL.to_string(),
                accounts: HashMap::from([
                    (DEFAULT_ACCOUNT_LABEL.to_string(), existing),
                    (label.to_string(), credential),
                ]),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors emitted by the auth-storage layer.
///
/// Variants are coarse on purpose — most callers only need to
/// distinguish "I/O died" from "the file is corrupt" from "OAuth
/// refresh failed", so they can decide between retrying, fixing the
/// file, and prompting the user to re-login.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Filesystem error reading or writing `auth.json`.
    #[error("auth storage I/O error: {0}")]
    Io(#[from] io::Error),
    /// `auth.json` exists but isn't valid JSON in our shape.
    #[error("invalid auth.json: {0}")]
    Parse(serde_json::Error),
    /// JSON serialization error when writing `auth.json`.
    #[error("failed to serialize auth.json: {0}")]
    Serialize(serde_json::Error),
    /// Underlying OAuth flow returned an error during login or refresh.
    #[error("OAuth flow failed: {0}")]
    OAuth(#[from] OAuthError),
    /// Stored credentials reference an OAuth provider we don't know
    /// how to refresh. Either the registry is missing an entry or
    /// `auth.json` was hand-edited with a bogus provider id.
    #[error("unknown OAuth provider: {0}")]
    UnknownProvider(String),
    /// An account operation named a label the provider's entry does not
    /// hold.
    #[error("provider {provider:?} has no selected account with that exact label")]
    UnknownAccount { provider: String, label: String },
    /// Removing the default account of a set that still has others. The
    /// caller picks a new default first: silently re-pointing the
    /// default would move what unlabeled resolution bills against.
    #[error("the selected account is provider {provider:?}'s default; set another default first")]
    RemovingDefault { provider: String, label: String },
    /// An account label that fails the shared creation policy, including its
    /// normalization, Unicode repertoire, grapheme, spacing, and length rules.
    #[error("invalid account label: {0}")]
    InvalidLabel(String),
    /// Insert-only account creation named an exact key already occupied in
    /// the provider's prospective account namespace.
    #[error("provider {provider:?} already has an account with that exact label")]
    DuplicateAccount { provider: String, label: String },
    /// A first-login write raced with another process that configured the
    /// provider while OAuth was in flight.
    #[error(
        "provider {provider:?} gained a credential while login was running. Start again to choose an account label"
    )]
    ProviderAlreadyConfigured { provider: String },
    /// An explicit replacement no longer names the required bare or labeled
    /// storage shape. The caller must reopen the account picker.
    #[error("provider {provider:?}'s selected credential changed. Reopen the account picker")]
    ProviderCredentialChanged { provider: String },
    /// A remove-all confirmation no longer names the account set currently
    /// stored for the provider.
    #[error("provider {provider:?}'s accounts changed. Reopen the account picker")]
    ProviderAccountsChanged { provider: String },
    /// Couldn't acquire the file lock within the timeout.
    #[error("auth storage lock timed out")]
    LockTimeout,
    /// `HOME` isn't set, so we can't compute the default `~/.aj/auth.json` path.
    #[error("home directory not found")]
    HomeNotFound,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// In-memory state shared across [`AuthStorage`] clones.
///
/// Wrapped in a tokio [`Mutex`] so async callers can mutate runtime
/// overrides without poisoning the lock, and so we can hold the lock
/// across `.await` points safely.
struct State {
    /// CLI-flag-driven keys that bypass `auth.json`. Higher priority
    /// than anything persisted; never written to disk.
    runtime_overrides: HashMap<String, String>,
    /// Provider-id → flow object, consulted when a stored OAuth
    /// credential needs refreshing. Defaults to Anthropic + OpenAI;
    /// callers can register more.
    oauth_providers: HashMap<String, Arc<dyn OAuthProvider>>,
}

/// Credential storage backed by `auth.json`.
///
/// Cheap to clone — internally an `Arc`, so all clones share one
/// runtime-override map and OAuth registry. The on-disk file is the
/// authoritative store; every read/write hits it directly so two
/// `AuthStorage` instances in the same process stay consistent.
///
/// The configured path must be absolute and its immediate parent is
/// storage-owned. On Unix every locked access enforces mode 0700 on that
/// directory and mode 0600 on an existing file. The auth path itself must not
/// be a symbolic link.
#[derive(Clone)]
pub struct AuthStorage {
    /// Path to `auth.json` (typically `~/.aj/auth.json`).
    path: PathBuf,
    /// Shared mutable state. `Arc`'d so clones see the same overrides.
    state: Arc<Mutex<State>>,
    /// Observable credential-file reads for ownership-boundary tests. The
    /// counter is shared with every clone, like the storage state itself.
    #[cfg(any(test, feature = "test-support"))]
    credential_reads: Arc<AtomicUsize>,
}

impl AuthStorage {
    /// Build a storage rooted at `path` with the default OAuth
    /// provider registry (Anthropic + OpenAI).
    pub fn new(path: PathBuf) -> Self {
        Self::with_providers(path, default_oauth_providers())
    }

    /// Build a storage rooted at `path` with a caller-supplied OAuth
    /// provider registry. Used by tests that want to inject mock
    /// providers (so refresh flows don't hit the real network) and by
    /// embedders that want a different default set.
    pub fn with_providers(
        path: PathBuf,
        oauth_providers: HashMap<String, Arc<dyn OAuthProvider>>,
    ) -> Self {
        Self {
            path,
            state: Arc::new(Mutex::new(State {
                runtime_overrides: HashMap::new(),
                oauth_providers,
            })),
            #[cfg(any(test, feature = "test-support"))]
            credential_reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Number of credential-file reads performed by this storage and all of
    /// its clones. This test-support observer proves remote commands refuse
    /// before crossing the client credential boundary.
    #[cfg(any(test, feature = "test-support"))]
    pub fn credential_read_count(&self) -> usize {
        self.credential_reads.load(Ordering::Relaxed)
    }

    /// Reset the shared credential-read observer after a fixture is arranged.
    #[cfg(any(test, feature = "test-support"))]
    pub fn reset_credential_read_count(&self) {
        self.credential_reads.store(0, Ordering::Relaxed);
    }

    /// Read the authoritative credential file through the one observable
    /// boundary used by every storage read and read-modify-write.
    fn read_credentials(&self) -> Result<AuthData, AuthError> {
        #[cfg(any(test, feature = "test-support"))]
        self.credential_reads.fetch_add(1, Ordering::Relaxed);
        read_auth_file(&self.path)
    }

    /// Build a storage at the default location, `~/.aj/auth.json`.
    ///
    /// Errors if `HOME` isn't set. Doesn't actually create the file —
    /// that happens lazily on first write.
    pub fn at_default_path() -> Result<Self, AuthError> {
        Ok(Self::new(default_path()?))
    }

    /// Path to the underlying `auth.json` file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Register (or replace) an OAuth provider entry in the in-memory
    /// registry. Useful when an embedder ships extra providers
    /// beyond the two we bundle.
    pub async fn register_oauth_provider(&self, provider: Arc<dyn OAuthProvider>) {
        let id = provider.id().to_string();
        self.state.lock().await.oauth_providers.insert(id, provider);
    }

    /// Set a runtime API-key override for `provider_id`. Stays
    /// in-memory — never written to `auth.json`. Highest priority in
    /// the resolution chain. Used to back the `--api-key` CLI flag.
    pub async fn set_runtime_api_key(&self, provider_id: &str, key: String) {
        self.state
            .lock()
            .await
            .runtime_overrides
            .insert(provider_id.to_string(), key);
    }

    /// Drop a runtime override previously set with
    /// [`AuthStorage::set_runtime_api_key`]. No-op if none is set.
    pub async fn remove_runtime_api_key(&self, provider_id: &str) {
        self.state
            .lock()
            .await
            .runtime_overrides
            .remove(provider_id);
    }

    /// Returns `true` if a runtime API-key override (CLI `--api-key`)
    /// is currently installed for `provider_id`. The override's value
    /// is deliberately not exposed — callers only need to know that it
    /// wins the resolution chain, e.g. to label it in a status view.
    pub async fn has_runtime_override(&self, provider_id: &str) -> bool {
        self.state
            .lock()
            .await
            .runtime_overrides
            .contains_key(provider_id)
    }

    /// Read the credential currently stored for `provider_id`, if any:
    /// the bare credential, or the default account of a labeled set.
    ///
    /// Performs a fresh disk read every call so multiple processes can
    /// share the file without a stale-cache problem. Acquires the
    /// file lock so a concurrent write doesn't yield a torn read.
    pub async fn get(&self, provider_id: &str) -> Result<Option<AuthCredential>, AuthError> {
        Ok(match self.stored_credentials(provider_id).await? {
            Some(StoredProviderCredentials::Bare(credential)) => Some(credential),
            Some(StoredProviderCredentials::Accounts(set)) => set
                .accounts
                .into_iter()
                .find_map(|(label, credential)| (label == set.default).then_some(credential)),
            None => None,
        })
    }

    /// Read the provider's complete stored shape under one file lock.
    pub async fn stored_credentials(
        &self,
        provider_id: &str,
    ) -> Result<Option<StoredProviderCredentials>, AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = self.read_credentials()?;
        Ok(data.remove(provider_id).map(|entry| match entry {
            StoredEntry::ApiKey { key } => {
                StoredProviderCredentials::Bare(AuthCredential::ApiKey { key })
            }
            StoredEntry::OAuth(credentials) => {
                StoredProviderCredentials::Bare(AuthCredential::OAuth(credentials))
            }
            StoredEntry::Accounts { default, accounts } => {
                let mut accounts = accounts.into_iter().collect::<Vec<_>>();
                accounts.sort_by(|a, b| a.0.cmp(&b.0));
                StoredProviderCredentials::Accounts(ProviderAccounts { default, accounts })
            }
        }))
    }

    /// Read one account of `provider_id`'s labeled set. A bare entry
    /// holds no labels, so any `label` against it reads `None`.
    pub async fn get_account(
        &self,
        provider_id: &str,
        label: &str,
    ) -> Result<Option<AuthCredential>, AuthError> {
        Ok(match self.stored_credentials(provider_id).await? {
            Some(StoredProviderCredentials::Accounts(set)) => set
                .accounts
                .into_iter()
                .find_map(|(current, credential)| (current == label).then_some(credential)),
            Some(StoredProviderCredentials::Bare(_)) | None => None,
        })
    }

    /// The labeled set stored for `provider_id`, or `None` when the
    /// provider holds a bare credential or nothing.
    pub async fn accounts(&self, provider_id: &str) -> Result<Option<ProviderAccounts>, AuthError> {
        Ok(match self.stored_credentials(provider_id).await? {
            Some(StoredProviderCredentials::Accounts(set)) => Some(set),
            Some(StoredProviderCredentials::Bare(_)) | None => None,
        })
    }

    /// Insert the first bare credential for an unconfigured provider.
    ///
    /// Creation is explicit and insert-only. A provider another process
    /// configures before the locked decision is preserved.
    pub async fn insert_bare(
        &self,
        provider_id: &str,
        credential: AuthCredential,
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = self.read_credentials()?;
        if data.contains_key(provider_id) {
            return Err(AuthError::ProviderAlreadyConfigured {
                provider: provider_id.to_string(),
            });
        }
        data.insert(
            provider_id.to_string(),
            StoredEntry::from_credential(credential),
        );
        write_auth_file(&self.path, &data)
    }

    /// Replace the exact bare slot of an already configured provider.
    ///
    /// A labeled set is never interpreted as a replacement target, including
    /// one whose default is dangling. Account replacement uses an explicit
    /// labeled intent instead.
    pub async fn replace_bare(
        &self,
        provider_id: &str,
        credential: AuthCredential,
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = self.read_credentials()?;
        match data.get_mut(provider_id) {
            Some(entry @ (StoredEntry::ApiKey { .. } | StoredEntry::OAuth(_))) => {
                *entry = StoredEntry::from_credential(credential);
            }
            Some(StoredEntry::Accounts { .. }) | None => {
                return Err(AuthError::ProviderCredentialChanged {
                    provider: provider_id.to_string(),
                });
            }
        }
        write_auth_file(&self.path, &data)
    }

    /// Insert a credential under a newly created account label.
    ///
    /// A bare entry converts on first growth: the existing credential is
    /// kept under [`DEFAULT_ACCOUNT_LABEL`] and STAYS the store default,
    /// so adding an account never silently moves what unlabeled resolution
    /// bills against. Creation is insert-only: an exact existing key or the
    /// prospective `default` key occupied by a bare credential is a duplicate,
    /// never an implicit replacement.
    pub async fn insert_account(
        &self,
        provider_id: &str,
        label: &str,
        credential: AuthCredential,
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = self.read_credentials()?;
        check_account_insert(&data, provider_id, label)?;
        let entry = insert_account_entry(data.remove(provider_id), label, credential);
        data.insert(provider_id.to_string(), entry);
        write_auth_file(&self.path, &data)
    }

    /// Point `provider_id`'s default at `label`. Errors when the entry
    /// is not a labeled set holding that label.
    pub async fn set_default_account(
        &self,
        provider_id: &str,
        label: &str,
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = self.read_credentials()?;
        match data.get_mut(provider_id) {
            Some(StoredEntry::Accounts { default, accounts }) if accounts.contains_key(label) => {
                *default = label.to_string();
            }
            _ => {
                return Err(AuthError::UnknownAccount {
                    provider: provider_id.to_string(),
                    label: label.to_string(),
                });
            }
        }
        write_auth_file(&self.path, &data)
    }

    /// Remove one account from `provider_id`'s labeled set.
    ///
    /// Removing the last account removes the provider's entry entirely.
    /// Removing the default while other accounts remain is refused
    /// ([`AuthError::RemovingDefault`]): the caller picks a new default
    /// first, so the store never re-points billing on its own. A set
    /// that shrinks to one account stays a set, its label was chosen
    /// deliberately and collapsing it back to bare would discard it.
    pub async fn remove_account(&self, provider_id: &str, label: &str) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = self.read_credentials()?;
        let Some(StoredEntry::Accounts { default, accounts }) = data.get_mut(provider_id) else {
            return Err(AuthError::UnknownAccount {
                provider: provider_id.to_string(),
                label: label.to_string(),
            });
        };
        if !accounts.contains_key(label) {
            return Err(AuthError::UnknownAccount {
                provider: provider_id.to_string(),
                label: label.to_string(),
            });
        }
        if label == default {
            if accounts.len() > 1 {
                return Err(AuthError::RemovingDefault {
                    provider: provider_id.to_string(),
                    label: label.to_string(),
                });
            }
            data.remove(provider_id);
        } else {
            accounts.remove(label);
            if accounts.is_empty() {
                data.remove(provider_id);
            }
        }
        write_auth_file(&self.path, &data)
    }

    /// Atomically move a labeled set's default and remove the old default.
    ///
    /// Both raw labels are checked under the same file lock, so a stale picker
    /// cannot briefly point the store at one account and then fail to remove
    /// another.
    pub async fn remove_default_account(
        &self,
        provider_id: &str,
        label: &str,
        new_default: &str,
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = self.read_credentials()?;
        let Some(StoredEntry::Accounts { default, accounts }) = data.get_mut(provider_id) else {
            return Err(AuthError::ProviderAccountsChanged {
                provider: provider_id.to_string(),
            });
        };
        if default != label
            || label == new_default
            || !accounts.contains_key(label)
            || !accounts.contains_key(new_default)
        {
            return Err(AuthError::ProviderAccountsChanged {
                provider: provider_id.to_string(),
            });
        }
        accounts.remove(label);
        *default = new_default.to_string();
        write_auth_file(&self.path, &data)
    }

    /// Remove a labeled provider set only while its exact raw key set still
    /// matches the one the user confirmed.
    pub async fn remove_all_accounts(
        &self,
        provider_id: &str,
        expected_accounts: &[String],
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = self.read_credentials()?;
        let Some(StoredEntry::Accounts { accounts, .. }) = data.get(provider_id) else {
            return Err(AuthError::ProviderAccountsChanged {
                provider: provider_id.to_string(),
            });
        };
        let mut current = accounts.keys().cloned().collect::<Vec<_>>();
        current.sort();
        let mut expected = expected_accounts.to_vec();
        expected.sort();
        if current != expected {
            return Err(AuthError::ProviderAccountsChanged {
                provider: provider_id.to_string(),
            });
        }
        data.remove(provider_id);
        write_auth_file(&self.path, &data)
    }

    /// Remove a provider credential only while it is still the bare shape the
    /// caller selected. A concurrent promotion to labeled accounts is a stale
    /// choice and must never become an implicit remove-all.
    pub async fn remove_bare(&self, provider_id: &str) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = self.read_credentials()?;
        match data.get(provider_id) {
            Some(StoredEntry::ApiKey { .. } | StoredEntry::OAuth(_)) => {
                data.remove(provider_id);
                write_auth_file(&self.path, &data)
            }
            Some(StoredEntry::Accounts { .. }) | None => {
                Err(AuthError::ProviderCredentialChanged {
                    provider: provider_id.to_string(),
                })
            }
        }
    }

    /// List all provider ids currently in `auth.json`.
    pub async fn list(&self) -> Result<Vec<String>, AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let data = self.read_credentials()?;
        Ok(data.into_keys().collect())
    }

    /// Returns `true` if `auth.json` has a stored credential for
    /// `provider_id`. Doesn't consider env vars or runtime overrides;
    /// use [`AuthStorage::has_auth`] for the broader check.
    pub async fn has(&self, provider_id: &str) -> Result<bool, AuthError> {
        Ok(self.get(provider_id).await?.is_some())
    }

    /// Returns `true` if *any* form of auth is configured for
    /// `provider_id` — runtime override, env var, or stored entry.
    /// Doesn't validate the credential or refresh OAuth tokens, so
    /// this is the right call for "should I show a login prompt?".
    pub async fn has_auth(&self, provider_id: &str) -> Result<bool, AuthError> {
        if self
            .state
            .lock()
            .await
            .runtime_overrides
            .contains_key(provider_id)
        {
            return Ok(true);
        }
        if get_env_api_key(provider_id).is_some() {
            return Ok(true);
        }
        self.has(provider_id).await
    }

    /// Resolve a usable bearer token for `provider_id`, walking the
    /// priority chain:
    ///
    /// 1. Runtime override (CLI `--api-key` flag).
    /// 2. Stored credential in `auth.json`: the bare credential or, for a
    ///    labeled set, the `account` slot (`None` = the store default).
    ///    Stored OAuth tokens auto-refresh under the file lock when
    ///    expired.
    /// 3. Environment variables, for unlabeled asks only.
    ///
    /// A stored credential is checked before the environment so a
    /// deliberate `aj login` or hand-edited `auth.json` entry stays
    /// authoritative. A stray exported key (say an `ANTHROPIC_API_KEY`
    /// left in a shell profile) must not silently shadow, and mis-bill
    /// against, a configured subscription. The explicit per-run
    /// override is the runtime `--api-key` in step 1, not ambient env,
    /// and it wins even over an explicit `account`: it is the louder,
    /// more local instruction.
    ///
    /// An `account` that misses (a label the set does not hold, any
    /// label against a bare entry, a dangling default) resolves to
    /// `Ok(None)` with NO environment fallback: serving a credential
    /// other than the one named is exactly the mis-billing this chain
    /// exists to prevent, and the caller names the label in its own
    /// missing-credential message.
    ///
    /// Returns `Ok(None)` when no source has a key. A stored OAuth
    /// credential whose provider id isn't in the registry (e.g. a
    /// hand-edited or renamed id) also resolves to `Ok(None)`: we
    /// can't refresh it, so it's treated as unconfigured and the
    /// caller prompts a fresh login. Env-supplied OAuth tokens
    /// (`ANTHROPIC_OAUTH_TOKEN`, `OPENAI_CODEX_OAUTH_TOKEN`) are
    /// returned verbatim without refresh, so a stale one surfaces as
    /// an upstream 401 mid-request rather than a re-login prompt.
    /// OAuth *refresh* failures bubble out as [`AuthError::OAuth`].
    /// Callers typically surface a "log in again" prompt.
    pub async fn get_api_key(
        &self,
        provider_id: &str,
        account: Option<&str>,
    ) -> Result<Option<ResolvedCredential>, AuthError> {
        // 1. Runtime override.
        if let Some(key) = self
            .state
            .lock()
            .await
            .runtime_overrides
            .get(provider_id)
            .cloned()
        {
            return Ok(Some(ResolvedCredential {
                key,
                source: CredentialSource::Override,
            }));
        }

        // 2. Stored credential, checked before the environment so a
        //    deliberate login stays authoritative (see the doc
        //    comment). Each arm that yields a usable key returns
        //    here.
        let entry = {
            let _lock = FileLock::acquire(&self.path).await?;
            self.read_credentials()?.remove(provider_id)
        };
        if let Some(entry) = entry {
            // The slot is fixed from this read: a refresh writes back to
            // the slot it read, whatever the default points at by then.
            // It is also what the resolved credential reports as its
            // source, which is the only place an unlabeled ask can learn
            // which label the store's default pointed at.
            let slot = entry.slot(account);
            match entry.resolve(account) {
                Some(AuthCredential::ApiKey { key }) => {
                    return Ok(Some(ResolvedCredential {
                        key,
                        source: slot.source(),
                    }));
                }
                Some(AuthCredential::OAuth(creds)) => {
                    // A stored OAuth credential under a provider id we have
                    // no flow for (a hand-edited or renamed id) can be
                    // neither validated nor refreshed. Treat it as
                    // unconfigured and let the caller prompt a fresh login
                    // rather than hard-erroring on what is effectively a typo.
                    let Ok(provider) = self.lookup_oauth_provider(provider_id).await else {
                        return Ok(None);
                    };
                    let now = now_unix_ms();
                    if !creds.is_expired_at(now) {
                        return Ok(Some(ResolvedCredential {
                            key: provider.get_api_key(&creds),
                            source: slot.source(),
                        }));
                    }
                    // A refresh failure bubbles as `AuthError::OAuth`. An
                    // `Ok(None)` means a sibling process cleared or replaced
                    // the slot while we held the lock, so we fall through
                    // rather than inventing a credential.
                    if let Some(key) = self
                        .refresh_oauth_with_lock(provider_id, &slot, &*provider)
                        .await?
                    {
                        return Ok(Some(ResolvedCredential {
                            key,
                            source: slot.source(),
                        }));
                    }
                }
                // The entry exists but the asked slot does not (a missing
                // label, a label against a bare entry, a dangling
                // default). Unconfigured, never a different credential.
                None => return Ok(None),
            }
        }

        // 3. Environment variables, the lowest-priority fallback, and
        //    only for unlabeled asks: env keys carry no account identity.
        if account.is_some() {
            return Ok(None);
        }
        Ok(get_env_api_key(provider_id).map(|key| ResolvedCredential {
            key,
            source: CredentialSource::Environment,
        }))
    }

    /// Run the first OAuth login for an unconfigured provider.
    ///
    /// The absence decision is checked before OAuth and repeated under the
    /// final file lock. A provider another process configures while the
    /// browser flow is open is preserved rather than overwritten.
    pub async fn login(
        &self,
        provider_id: &str,
        callbacks: &dyn OAuthCallbacks,
    ) -> Result<(), AuthError> {
        self.login_account(provider_id, None, callbacks).await
    }

    /// Run insert-only OAuth account creation.
    ///
    /// `None` is the first-login bare shape. `Some(label)` is a labeled
    /// account creation. Both decisions are checked before OAuth and repeated
    /// under the final file lock, so this API never becomes an upsert.
    pub async fn login_account(
        &self,
        provider_id: &str,
        label: Option<&str>,
        callbacks: &dyn OAuthCallbacks,
    ) -> Result<(), AuthError> {
        self.check_login_creation(provider_id, label).await?;
        let provider = self.lookup_oauth_provider(provider_id).await?;
        let creds = provider.login(callbacks).await?;
        self.commit_login_creation(provider_id, label, AuthCredential::OAuth(creds))
            .await
    }

    /// Run OAuth for an explicitly selected existing bare credential or
    /// labeled account.
    ///
    /// The exact target must exist before OAuth. An existing legacy account
    /// remains replaceable without current validation. If a labeled target is
    /// removed while OAuth runs, committing would recreate a key, so the final
    /// locked decision applies the current insertion predicate and prospective
    /// namespace rules before doing so.
    pub async fn replace_login_account(
        &self,
        provider_id: &str,
        label: Option<&str>,
        callbacks: &dyn OAuthCallbacks,
    ) -> Result<(), AuthError> {
        self.check_login_replacement(provider_id, label).await?;
        let provider = self.lookup_oauth_provider(provider_id).await?;
        let creds = provider.login(callbacks).await?;
        self.commit_login_replacement(provider_id, label, AuthCredential::OAuth(creds))
            .await
    }

    async fn check_login_creation(
        &self,
        provider_id: &str,
        label: Option<&str>,
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let data = self.read_credentials()?;
        match label {
            Some(label) => check_account_insert(&data, provider_id, label),
            None if data.contains_key(provider_id) => Err(AuthError::ProviderAlreadyConfigured {
                provider: provider_id.to_string(),
            }),
            None => Ok(()),
        }
    }

    async fn commit_login_creation(
        &self,
        provider_id: &str,
        label: Option<&str>,
        credential: AuthCredential,
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = self.read_credentials()?;
        match label {
            Some(label) => {
                check_account_insert(&data, provider_id, label)?;
                let entry = insert_account_entry(data.remove(provider_id), label, credential);
                data.insert(provider_id.to_string(), entry);
            }
            None if data.contains_key(provider_id) => {
                return Err(AuthError::ProviderAlreadyConfigured {
                    provider: provider_id.to_string(),
                });
            }
            None => {
                data.insert(
                    provider_id.to_string(),
                    StoredEntry::from_credential(credential),
                );
            }
        }
        write_auth_file(&self.path, &data)
    }

    async fn check_login_replacement(
        &self,
        provider_id: &str,
        label: Option<&str>,
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let data = self.read_credentials()?;
        let exists = match (data.get(provider_id), label) {
            (Some(StoredEntry::ApiKey { .. } | StoredEntry::OAuth(_)), None) => true,
            (Some(StoredEntry::Accounts { accounts, .. }), Some(label)) => {
                accounts.contains_key(label)
            }
            _ => false,
        };
        if exists {
            Ok(())
        } else {
            Err(AuthError::ProviderCredentialChanged {
                provider: provider_id.to_string(),
            })
        }
    }

    async fn commit_login_replacement(
        &self,
        provider_id: &str,
        label: Option<&str>,
        credential: AuthCredential,
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = self.read_credentials()?;
        match label {
            None => match data.get_mut(provider_id) {
                Some(entry @ (StoredEntry::ApiKey { .. } | StoredEntry::OAuth(_))) => {
                    *entry = StoredEntry::from_credential(credential);
                }
                _ => {
                    return Err(AuthError::ProviderCredentialChanged {
                        provider: provider_id.to_string(),
                    });
                }
            },
            Some(label) => {
                let still_present = matches!(
                    data.get(provider_id),
                    Some(StoredEntry::Accounts { accounts, .. }) if accounts.contains_key(label)
                );
                if still_present {
                    let Some(StoredEntry::Accounts { accounts, .. }) = data.get_mut(provider_id)
                    else {
                        unreachable!("presence check matched account set")
                    };
                    accounts.insert(label.to_string(), credential);
                } else {
                    check_account_insert(&data, provider_id, label)?;
                    let entry = insert_account_entry(data.remove(provider_id), label, credential);
                    data.insert(provider_id.to_string(), entry);
                }
            }
        }
        write_auth_file(&self.path, &data)
    }

    /// List the registered OAuth providers as `(id, display_name)`
    /// pairs, sorted by id so a UI building a login picker gets a
    /// stable order. The display name is [`OAuthProvider::name`]
    /// (e.g. `"Anthropic (Claude Pro/Max)"`).
    pub async fn oauth_provider_ids(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .state
            .lock()
            .await
            .oauth_providers
            .values()
            .map(|p| (p.id().to_string(), p.name().to_string()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Look up an OAuth provider by id, returning a clone of the
    /// `Arc` so the caller can `.await` against it without holding
    /// the registry lock.
    async fn lookup_oauth_provider(
        &self,
        provider_id: &str,
    ) -> Result<Arc<dyn OAuthProvider>, AuthError> {
        self.state
            .lock()
            .await
            .oauth_providers
            .get(provider_id)
            .cloned()
            .ok_or_else(|| AuthError::UnknownProvider(provider_id.to_string()))
    }

    /// Atomically refresh a stored OAuth credential and return the
    /// new bearer token. Holds the file lock for the entire
    /// read-modify-write so concurrent `aj` processes serialize on
    /// the refresh and avoid double-spending the refresh token.
    ///
    /// Always re-reads `auth.json` under the lock — a sibling process
    /// may have already refreshed by the time we got the lock, in
    /// which case we use *its* token instead of doing another
    /// upstream call. `slot` is the place the caller's read resolved,
    /// and it is where the refreshed token is written back: never the
    /// default-of-the-moment, which a sibling may have re-pointed.
    async fn refresh_oauth_with_lock(
        &self,
        provider_id: &str,
        slot: &Slot,
        provider: &dyn OAuthProvider,
    ) -> Result<Option<String>, AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;

        let mut data = self.read_credentials()?;
        let creds = match (data.get(provider_id), slot) {
            (Some(StoredEntry::OAuth(c)), Slot::Bare) => c.clone(),
            (Some(StoredEntry::Accounts { accounts, .. }), Slot::Account(label)) => {
                match accounts.get(label) {
                    Some(AuthCredential::OAuth(c)) => c.clone(),
                    // The slot vanished or is now an api_key; nothing
                    // to refresh.
                    _ => return Ok(None),
                }
            }
            // The entry changed shape under a sibling's write; nothing
            // to refresh in the slot we read.
            _ => return Ok(None),
        };

        // Sibling process may have refreshed while we were waiting
        // for the lock — use the freshened token without burning the
        // refresh-token round-trip again.
        let now = now_unix_ms();
        if !creds.is_expired_at(now) {
            return Ok(Some(provider.get_api_key(&creds)));
        }

        let refreshed = provider.refresh_token(&creds).await?;
        let api_key = provider.get_api_key(&refreshed);
        match (data.get_mut(provider_id), slot) {
            (Some(entry @ StoredEntry::OAuth(_)), Slot::Bare) => {
                *entry = StoredEntry::OAuth(refreshed);
            }
            (Some(StoredEntry::Accounts { accounts, .. }), Slot::Account(label)) => {
                accounts.insert(label.clone(), AuthCredential::OAuth(refreshed));
            }
            // Unreachable in practice: the shapes were just matched
            // above under the same lock.
            _ => return Ok(None),
        }
        write_auth_file(&self.path, &data)?;
        Ok(Some(api_key))
    }
}

// ---------------------------------------------------------------------------
// Default registry / paths
// ---------------------------------------------------------------------------

/// OAuth providers shipped out of the box (Anthropic Claude Pro/Max →
/// provider id `"anthropic"`, OpenAI ChatGPT/Codex → provider id
/// `"openai-codex"`). The Codex flow uses a distinct
/// provider id from plain `OPENAI_API_KEY` credentials so the
/// `chatgpt.com/backend-api` JWT pool never collides with the
/// `api.openai.com` API-key pool.
fn default_oauth_providers() -> HashMap<String, Arc<dyn OAuthProvider>> {
    let mut map: HashMap<String, Arc<dyn OAuthProvider>> = HashMap::new();
    let anthropic: Arc<dyn OAuthProvider> = Arc::new(AnthropicOAuth::new());
    let openai: Arc<dyn OAuthProvider> = Arc::new(OpenAIOAuth::new());
    map.insert(anthropic.id().to_string(), anthropic);
    map.insert(openai.id().to_string(), openai);
    map
}

/// Compute `~/.aj/auth.json`. Errors if `HOME` isn't set.
fn default_path() -> Result<PathBuf, AuthError> {
    let home = std::env::var("HOME").map_err(|_| AuthError::HomeNotFound)?;
    Ok(PathBuf::from(home).join(".aj").join("auth.json"))
}

// ---------------------------------------------------------------------------
// Environment-variable mapping
// ---------------------------------------------------------------------------

/// Environment variables that can supply an API key for `provider_id`,
/// in order of preference.
///
/// We cover four providers today: `"anthropic"`
/// (`ANTHROPIC_OAUTH_TOKEN` then `ANTHROPIC_API_KEY`), `"openai"`
/// (`OPENAI_API_KEY`), `"openai-codex"` (`OPENAI_CODEX_OAUTH_TOKEN`),
/// and `"openrouter"` (`OPENROUTER_API_KEY`). The codex var carries a
/// short-lived JWT minted by the OAuth flow; on its own it cannot
/// be refreshed, so persistent use should rely on a stored OAuth
/// credential rather than this env var. Unknown providers return an
/// empty slice so callers can treat absence as "no env mapping
/// configured".
pub fn find_env_keys(provider_id: &str) -> &'static [&'static str] {
    match provider_id {
        "anthropic" => &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "openai-codex" => &["OPENAI_CODEX_OAUTH_TOKEN"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        _ => &[],
    }
}

/// Return the first non-empty env var listed by [`find_env_keys`]
/// for `provider_id`, or `None` if no mapped variable is set.
pub fn get_env_api_key(provider_id: &str) -> Option<String> {
    find_env_keys(provider_id)
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

// ---------------------------------------------------------------------------
// File I/O
// ---------------------------------------------------------------------------

/// Read and parse `auth.json`. Treats a missing or empty file as an
/// empty map so first-run flows don't have to special-case file
/// creation themselves.
///
/// Applies the legacy-id migration in-memory before returning:
/// any OAuth-type entry stored under provider id `"openai"` is moved
/// to `"openai-codex"`, matching the renamed [`OpenAIOAuth`] provider
/// id. The migration is silent and idempotent — if the destination
/// id already holds an entry we leave both alone rather than clobber
/// a user's hand-edited file. Plain `api_key` entries under
/// `"openai"` are never touched: those are real `OPENAI_API_KEY`
/// credentials for the public API and don't belong to the Codex
/// credential pool.
///
/// The on-disk file is not rewritten here — that happens the next
/// time any mutating operation re-reads + writes via [`write_auth_file`],
/// at which point the migrated shape is persisted. Until then, both
/// shapes coexist on disk, which is harmless: callers always observe
/// the migrated in-memory view.
fn read_auth_file(path: &Path) -> Result<AuthData, AuthError> {
    let mut data: AuthData = match std::fs::read_to_string(path) {
        Ok(content) => {
            if content.trim().is_empty() {
                return Ok(HashMap::new());
            }
            serde_json::from_str(&content).map_err(AuthError::Parse)?
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(AuthError::Io(e)),
    };
    migrate_legacy_openai_oauth(&mut data);
    Ok(data)
}

/// In-place rewrite of any legacy OAuth-typed `"openai"` entry to
/// `"openai-codex"`. Idempotent; leaves the data alone if the
/// destination id is already populated or if the source isn't a bare
/// OAuth entry. A labeled set under `"openai"` is never legacy: sets
/// postdate the rename, so one there is user-authored and stays put.
fn migrate_legacy_openai_oauth(data: &mut AuthData) {
    const LEGACY_ID: &str = "openai";
    const NEW_ID: &str = "openai-codex";

    // Only OAuth credentials migrate. The legacy `openai` slot also
    // legitimately stored hand-written `api_key` entries (plain
    // `OPENAI_API_KEY` paste-ins), and those stay where they are.
    if !matches!(data.get(LEGACY_ID), Some(StoredEntry::OAuth(_))) {
        return;
    }
    // Don't clobber a user-authored entry under the new id.
    if data.contains_key(NEW_ID) {
        return;
    }
    let cred = data.remove(LEGACY_ID).expect("matched OAuth variant above");
    data.insert(NEW_ID.to_string(), cred);
    tracing::info!(
        "migrated legacy OAuth credentials from `openai` to `openai-codex` in auth.json"
    );
}

/// Write `data` to `auth.json` through a private same-directory temporary file.
///
/// On Unix the containing directory and any existing destination are made
/// private before the temporary file receives credential bytes. The completed
/// file atomically replaces the destination. A failed write leaves the prior
/// store intact, and process interruption leaves either the prior or replacement
/// version rather than a partially rewritten destination. This does not claim
/// power-loss durability, which requires syncing both the file and directory.
fn write_auth_file(path: &Path, data: &AuthData) -> Result<(), AuthError> {
    let content = serde_json::to_string_pretty(data).map_err(AuthError::Serialize)?;
    replace_auth_file(path, content.as_bytes(), write_credentials)
}

/// The production byte writer behind [`write_auth_file`].
///
/// Named rather than inlined as a closure so tests can observe and
/// fault-inject the exact writer whose result [`replace_auth_file`] must
/// propagate, instead of substituting a test closure downstream of it.
fn write_credentials(file: &mut std::fs::File, bytes: &[u8]) -> io::Result<()> {
    #[cfg(test)]
    if let Some(result) = fault::intercept_credential_write(file, bytes) {
        return result;
    }
    file.write_all(bytes)
}

/// Replace `path` only after `write` succeeds.
///
/// The byte writer is separate from publication so any write error drops the
/// private temporary file while the destination remains unchanged.
fn replace_auth_file(
    path: &Path,
    content: &[u8],
    write: impl FnOnce(&mut std::fs::File, &[u8]) -> io::Result<()>,
) -> Result<(), AuthError> {
    prepare_auth_parent(path)?;
    make_existing_auth_file_private(path)?;

    let parent = storage_parent(path)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auth.json");
    let prefix = format!(".{file_name}.tmp-");
    let mut replacement = tempfile::Builder::new()
        .prefix(&prefix)
        .tempfile_in(parent)?;

    #[cfg(unix)]
    set_unix_file_permissions(
        PermissionSite::ReplacementFile,
        replacement.as_file(),
        0o600,
    )?;

    write(replacement.as_file_mut(), content)?;
    replacement
        .persist(path)
        .map_err(|error| AuthError::Io(error.error))?;

    Ok(())
}

/// The directory in which both the destination and its replacement live.
/// A relative path is ambiguous because lexical forms such as `./auth.json`
/// could make hardening its parent alter a caller-owned working directory.
fn storage_parent(path: &Path) -> io::Result<&Path> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "auth storage path must be absolute",
        ));
    }
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "auth storage path must have an explicit parent directory",
            )
        })
}

/// Create the auth directory and, on Unix, repair permissive mode bits before
/// any credential file is opened.
fn prepare_auth_parent(path: &Path) -> io::Result<()> {
    let parent = storage_parent(path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700).create(parent)?;
        set_unix_permissions(PermissionSite::Parent, parent, 0o700)?;
    }

    #[cfg(not(unix))]
    std::fs::create_dir_all(parent)?;

    Ok(())
}

/// Reject symbolic links and non-files. On Unix, repair a permissive existing
/// destination before replacement. A missing destination is the normal
/// first-write case. Every other metadata or permission error is part of the
/// caller-visible storage result.
fn make_existing_auth_file_private(path: &Path) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "auth storage path must not be a symbolic link",
        ));
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "auth storage path must name a regular file",
        ));
    }

    #[cfg(unix)]
    set_unix_permissions(PermissionSite::ExistingFile, path, 0o600)?;

    Ok(())
}

/// One hardening chmod in the credential write path.
///
/// Every site routes through [`set_unix_permissions`] or
/// [`set_unix_file_permissions`] so a test can fail exactly one site's
/// operation and require the storage result to surface it. Swallowing any of
/// these failures would leave credential bytes behind permissive modes.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PermissionSite {
    /// The private mode of the same-directory replacement file, applied
    /// before it receives any credential byte.
    ReplacementFile,
    /// The 0700 mode of the storage directory.
    Parent,
    /// The 0600 repair of a permissive existing destination.
    ExistingFile,
}

#[cfg(unix)]
fn set_unix_permissions(site: PermissionSite, target: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    #[cfg(test)]
    fault::injected_permission_failure(site)?;
    #[cfg(not(test))]
    let _ = site;
    std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))
}

#[cfg(unix)]
fn set_unix_file_permissions(
    site: PermissionSite,
    file: &std::fs::File,
    mode: u32,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    #[cfg(test)]
    fault::injected_permission_failure(site)?;
    #[cfg(not(test))]
    let _ = site;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
}

// ---------------------------------------------------------------------------
// Cross-process file lock
// ---------------------------------------------------------------------------

/// Maximum time we'll wait for the file lock before giving up.
/// 30 s is generous — typical refresh round-trips finish in well
/// under a second.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// If a `.lock` directory exists but its mtime is older than this,
/// assume the holder crashed without cleaning up and steal the lock.
const STALE_LOCK_AGE: Duration = Duration::from_secs(60);

/// Initial backoff between lock-acquisition retries. Doubles on each
/// attempt up to `MAX_BACKOFF`.
const INITIAL_BACKOFF: Duration = Duration::from_millis(20);
const MAX_BACKOFF: Duration = Duration::from_millis(500);

/// Sidecar lock for `auth.json`, implemented as an empty directory
/// next to the file. `mkdir` is atomic on every supported OS, so
/// `create_dir`'s `AlreadyExists` error is the natural "already
/// locked" signal.
///
/// On `Drop` we best-effort `rmdir`; if the process aborts before
/// `Drop` runs, the next acquirer detects the stale lock via mtime
/// and steals it.
struct FileLock {
    path: PathBuf,
}

impl FileLock {
    /// Acquire the lock, retrying with exponential backoff up to
    /// [`LOCK_TIMEOUT`]. Returns [`AuthError::LockTimeout`] if a
    /// sibling holds the lock the whole time (and isn't stale).
    async fn acquire(target_path: &Path) -> Result<Self, AuthError> {
        let lock_path = lock_path_for(target_path);

        // The lock is the first filesystem object an auth operation creates.
        // Harden its parent before creating it so the lock path cannot open a
        // process-default-permission window ahead of the credential write.
        prepare_auth_parent(target_path)?;

        let start = std::time::Instant::now();
        let mut backoff = INITIAL_BACKOFF;
        loop {
            match create_lock_dir(&lock_path) {
                Ok(()) => {
                    let lock = Self { path: lock_path };
                    make_existing_auth_file_private(target_path)?;
                    return Ok(lock);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if try_steal_stale_lock(&lock_path, STALE_LOCK_AGE) {
                        // Try again immediately after stealing; if a
                        // racing acquirer beat us we'll re-enter the
                        // backoff path on the next iteration.
                        continue;
                    }
                    if start.elapsed() > LOCK_TIMEOUT {
                        return Err(AuthError::LockTimeout);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                Err(e) => return Err(AuthError::Io(e)),
            }
        }
    }
}

/// Create a lock directory that is private from its first observable mode.
fn create_lock_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        std::fs::DirBuilder::new().mode(0o700).create(path)
    }

    #[cfg(not(unix))]
    std::fs::create_dir(path)
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Best-effort cleanup. Use sync `std::fs` because `Drop`
        // can't `.await`. The lock path is a directory we created
        // ourselves, so `remove_dir` succeeds unless something has
        // already torn it down — fine to ignore that case.
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// `auth.json` → `auth.json.lock` next to it.
fn lock_path_for(file_path: &Path) -> PathBuf {
    let parent = file_path.parent().unwrap_or_else(|| Path::new("."));
    let name = match file_path.file_name() {
        Some(n) => format!("{}.lock", n.to_string_lossy()),
        None => "auth.lock".to_string(),
    };
    parent.join(name)
}

/// If the lock directory exists and looks abandoned, try to remove
/// it. Returns `true` only when we actually removed something so the
/// caller can retry. Any I/O error is swallowed — worst case we just
/// loop and time out.
///
/// `max_age` is the threshold past which a lock is considered stale.
/// Pulled out as a parameter so tests can drive the steal path with
/// a tiny age without sleeping out the full production threshold.
fn try_steal_stale_lock(lock_path: &Path, max_age: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(lock_path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = modified.elapsed() else {
        return false;
    };
    if age <= max_age {
        return false;
    }
    std::fs::remove_dir(lock_path).is_ok()
}

// ---------------------------------------------------------------------------
// Test fault points
// ---------------------------------------------------------------------------

/// Test-only fault points crossed by the production write path.
///
/// The hooks are thread-local: auth tests run their storage operations on the
/// current-thread tokio runtime, so an installed hook is visible to exactly
/// the operations of its own test, and parallel tests cannot observe one
/// another's injections. Installation returns an owning guard whose drop
/// clears the hook, so a panicking test cannot leak an injection into a later
/// test scheduled on the same thread. Installing replaces any installed hook
/// and every guard's drop clears the slot outright: tests do not nest
/// installations.
///
/// The permission injection fires above the real chmod, so it proves each
/// site's failure propagates but cannot catch a swallow of the chmod syscall
/// itself; an unprivileged test cannot make an owned target's chmod fail. The
/// write seam has no such residual: the device-failure test swaps the
/// replacement handle for `/dev/full` and drives the untouched production
/// write into a real `ENOSPC`.
#[cfg(test)]
mod fault {
    use std::cell::RefCell;
    use std::io;

    /// Interception for [`super::write_credentials`]. Returning `Some`
    /// replaces the production write with the hook's result; returning `None`
    /// falls through to the real writer, which lets an observer record the
    /// write-time state without changing behavior.
    type WriteHook = Box<dyn FnMut(&mut std::fs::File, &[u8]) -> Option<io::Result<()>>>;

    thread_local! {
        static CREDENTIAL_WRITE: RefCell<Option<WriteHook>> = const { RefCell::new(None) };
        #[cfg(unix)]
        static PERMISSION_FAILURE: RefCell<Option<super::PermissionSite>> =
            const { RefCell::new(None) };
    }

    pub(super) fn intercept_credential_write(
        file: &mut std::fs::File,
        bytes: &[u8],
    ) -> Option<io::Result<()>> {
        CREDENTIAL_WRITE.with(|hook| {
            hook.borrow_mut()
                .as_mut()
                .and_then(|hook| hook(file, bytes))
        })
    }

    /// Fail the given hardening chmod site with `PermissionDenied`, standing
    /// in for the real `EPERM` a non-owned target produces.
    #[cfg(unix)]
    pub(super) fn injected_permission_failure(site: super::PermissionSite) -> io::Result<()> {
        PERMISSION_FAILURE.with(|failure| match *failure.borrow() {
            Some(injected) if injected == site => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("injected {site:?} permission failure"),
            )),
            _ => Ok(()),
        })
    }

    /// Clears the installed hook on drop.
    pub(super) struct HookGuard {
        clear: fn(),
    }

    impl Drop for HookGuard {
        fn drop(&mut self) {
            (self.clear)();
        }
    }

    pub(super) fn install_write_hook(hook: WriteHook) -> HookGuard {
        CREDENTIAL_WRITE.with(|slot| *slot.borrow_mut() = Some(hook));
        HookGuard {
            clear: || CREDENTIAL_WRITE.with(|slot| *slot.borrow_mut() = None),
        }
    }

    #[cfg(unix)]
    pub(super) fn install_permission_failure(site: super::PermissionSite) -> HookGuard {
        PERMISSION_FAILURE.with(|slot| *slot.borrow_mut() = Some(site));
        HookGuard {
            clear: || PERMISSION_FAILURE.with(|slot| *slot.borrow_mut() = None),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::TempDir;

    use super::*;
    use async_trait::async_trait;

    /// An `auth.json` path in a scratch directory of its own, so each test
    /// gets its own file and `.lock` to play with, plus the guard that removes
    /// the directory. The guard has to outlive the test's use of the path.
    ///
    /// The label rides in the directory name so a leftover from a crashed run
    /// still says which test made it.
    fn scratch_path(tag: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::with_prefix(format!("aj-auth-test-{tag}-")).expect("create temp dir");
        let path = dir.path().join("auth.json");
        (dir, path)
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;

        std::fs::metadata(path)
            .expect("read fixture metadata")
            .permissions()
            .mode()
            & 0o777
    }

    struct TestCallbacks;

    #[async_trait]
    impl OAuthCallbacks for TestCallbacks {
        fn on_auth(&self, _info: crate::oauth::OAuthAuthInfo<'_>) {}

        async fn on_prompt(&self, _message: &str) -> Result<String, OAuthError> {
            Ok(String::new())
        }
    }

    enum LoginRace {
        None,
        InsertBare { key: String },
        Insert { label: String, key: String },
        Remove { label: String },
    }

    struct RacingLoginProvider {
        path: PathBuf,
        race: StdMutex<LoginRace>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl OAuthProvider for RacingLoginProvider {
        fn id(&self) -> &str {
            "stub"
        }

        fn name(&self) -> &str {
            "Stub"
        }

        async fn login(
            &self,
            _callbacks: &dyn OAuthCallbacks,
        ) -> Result<OAuthCredentials, OAuthError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let race = std::mem::replace(
                &mut *self.race.lock().expect("race mutex poisoned"),
                LoginRace::None,
            );
            let rival = AuthStorage::with_providers(self.path.clone(), HashMap::new());
            match race {
                LoginRace::None => {}
                LoginRace::InsertBare { key } => {
                    rival
                        .insert_bare("stub", AuthCredential::ApiKey { key })
                        .await
                        .expect("concurrent bare insertion");
                }
                LoginRace::Insert { label, key } => {
                    rival
                        .insert_account("stub", &label, AuthCredential::ApiKey { key })
                        .await
                        .expect("concurrent insertion");
                }
                LoginRace::Remove { label } => {
                    rival
                        .remove_account("stub", &label)
                        .await
                        .expect("concurrent removal");
                }
            }
            Ok(OAuthCredentials::new("new-refresh", "new-access", i64::MAX))
        }

        async fn refresh_token(
            &self,
            _credentials: &OAuthCredentials,
        ) -> Result<OAuthCredentials, OAuthError> {
            unreachable!("login tests do not refresh")
        }
    }

    fn racing_storage(path: PathBuf, race: LoginRace) -> (AuthStorage, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn OAuthProvider> = Arc::new(RacingLoginProvider {
            path: path.clone(),
            race: StdMutex::new(race),
            calls: Arc::clone(&calls),
        });
        (
            AuthStorage::with_providers(path, HashMap::from([("stub".to_string(), provider)])),
            calls,
        )
    }

    /// `AuthCredential` round-trip: API-key shape stays a flat
    /// `{ "type": "api_key", "key": ... }`.
    #[test]
    fn credential_api_key_roundtrip() {
        let cred = AuthCredential::ApiKey {
            key: "sk-test".into(),
        };
        let json = serde_json::to_value(&cred).unwrap();
        assert_eq!(json["type"], "api_key");
        assert_eq!(json["key"], "sk-test");

        let back: AuthCredential = serde_json::from_value(json).unwrap();
        match back {
            AuthCredential::ApiKey { key } => assert_eq!(key, "sk-test"),
            _ => panic!("expected ApiKey variant"),
        }
    }

    /// OAuth credentials must serialize with the inner
    /// `OAuthCredentials` fields *flattened* alongside `"type"`, so
    /// `auth.json` looks like
    /// `{ "type": "oauth", "refresh": ..., "access": ..., "expires": ..., "accountId": ... }`
    /// — not nested under an extra key.
    #[test]
    fn credential_oauth_roundtrip_flattens() {
        let mut creds = OAuthCredentials::new("r", "a", 1234);
        creds.extra.insert(
            "accountId".into(),
            serde_json::Value::String("acc-9".into()),
        );
        let cred = AuthCredential::OAuth(creds);

        let json = serde_json::to_value(&cred).unwrap();
        assert_eq!(json["type"], "oauth");
        assert_eq!(json["refresh"], "r");
        assert_eq!(json["access"], "a");
        assert_eq!(json["expires"], 1234);
        assert_eq!(json["accountId"], "acc-9");

        let back: AuthCredential = serde_json::from_value(json).unwrap();
        match back {
            AuthCredential::OAuth(c) => {
                assert_eq!(c.refresh, "r");
                assert_eq!(c.access, "a");
                assert_eq!(c.expires, 1234);
                assert_eq!(c.extra.get("accountId").unwrap(), "acc-9");
            }
            _ => panic!("expected OAuth variant"),
        }
    }

    #[test]
    fn credential_debug_redacts_secrets_through_stored_and_public_containers() {
        const API_KEY: &str = "api-key-debug-sentinel";
        const REFRESH_TOKEN: &str = "refresh-debug-sentinel";
        const ACCESS_TOKEN: &str = "access-debug-sentinel";
        const EXTRA_VALUE: &str = "extra-value-debug-sentinel";

        let mut oauth = OAuthCredentials::new(REFRESH_TOKEN, ACCESS_TOKEN, 1234);
        oauth.extra.insert(
            "future_token_field".into(),
            serde_json::Value::String(EXTRA_VALUE.into()),
        );
        let api_key = AuthCredential::ApiKey {
            key: API_KEY.into(),
        };
        let oauth = AuthCredential::OAuth(oauth);

        let outputs = [
            format!("{api_key:?}"),
            format!("{oauth:?}"),
            format!(
                "{:?}",
                StoredEntry::ApiKey {
                    key: API_KEY.into()
                }
            ),
            format!(
                "{:?}",
                ProviderAccounts {
                    default: "personal".into(),
                    accounts: vec![
                        ("personal".into(), api_key.clone()),
                        ("work".into(), oauth.clone()),
                    ],
                }
            ),
            format!(
                "{:?}",
                StoredProviderCredentials::Accounts(ProviderAccounts {
                    default: "personal".into(),
                    accounts: vec![("personal".into(), api_key), ("work".into(), oauth),],
                })
            ),
            format!(
                "{:?}",
                ResolvedCredential {
                    key: ACCESS_TOKEN.into(),
                    source: CredentialSource::Account("personal".into()),
                }
            ),
        ];

        for output in &outputs {
            for secret in [API_KEY, REFRESH_TOKEN, ACCESS_TOKEN, EXTRA_VALUE] {
                assert!(!output.contains(secret), "credential leaked in {output}");
            }
        }

        assert!(outputs[0].contains("ApiKey"));
        assert!(outputs[0].contains("key"));
        assert!(outputs[1].contains("OAuth(OAuthCredentials"));
        assert!(outputs[2].contains("ApiKey"));
        let accounts = &outputs[3];
        assert!(accounts.contains("ProviderAccounts"));
        assert!(accounts.contains("personal"));
        assert!(accounts.contains("work"));
        assert!(accounts.contains("future_token_field"));
        assert!(outputs[4].contains("Accounts(ProviderAccounts"));
        assert!(outputs[5].contains("Account(\"personal\")"));
    }

    /// Explicit bare insert, replace, and removal persist their exact slot.
    #[tokio::test]
    async fn bare_insert_replace_and_remove_persist_to_file() {
        let (_dir, path) = scratch_path("crud");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        assert_eq!(storage.list().await.unwrap(), Vec::<String>::new());
        assert!(!storage.has("anthropic").await.unwrap());

        storage
            .insert_bare(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "sk-abc".into(),
                },
            )
            .await
            .unwrap();

        // File was created and contains the right shape.
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("\"type\""), "{content}");
        assert!(content.contains("\"sk-abc\""), "{content}");

        assert!(storage.has("anthropic").await.unwrap());
        let mut providers = storage.list().await.unwrap();
        providers.sort();
        assert_eq!(providers, vec!["anthropic".to_string()]);

        match storage.get("anthropic").await.unwrap() {
            Some(AuthCredential::ApiKey { key }) => assert_eq!(key, "sk-abc"),
            other => panic!("unexpected credential: {other:?}"),
        }

        storage
            .replace_bare(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "sk-replaced".into(),
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            storage.get("anthropic").await.unwrap(),
            Some(AuthCredential::ApiKey { key }) if key == "sk-replaced"
        ));

        storage.remove_bare("anthropic").await.unwrap();
        assert!(!storage.has("anthropic").await.unwrap());
    }

    /// Runtime override beats env vars and stored credentials.
    #[tokio::test]
    async fn get_api_key_runtime_override_wins() {
        let (_dir, path) = scratch_path("override");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        storage
            .insert_bare(
                "openai",
                AuthCredential::ApiKey {
                    key: "from-file".into(),
                },
            )
            .await
            .unwrap();

        storage
            .set_runtime_api_key("openai", "from-runtime".into())
            .await;

        let key = storage.get_api_key("openai", None).await.unwrap();
        assert_eq!(
            key.map(|resolved| resolved.key).as_deref(),
            Some("from-runtime")
        );
    }

    /// Stored API key is returned when no runtime override or env var
    /// is set. Uses an unknown provider id so env-var resolution
    /// can't accidentally satisfy the request.
    #[tokio::test]
    async fn get_api_key_falls_back_to_stored_key() {
        let (_dir, path) = scratch_path("stored");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        storage
            .insert_bare(
                "custom-provider-xyz",
                AuthCredential::ApiKey {
                    key: "from-file".into(),
                },
            )
            .await
            .unwrap();

        let key = storage
            .get_api_key("custom-provider-xyz", None)
            .await
            .unwrap();
        assert_eq!(
            key.map(|resolved| resolved.key).as_deref(),
            Some("from-file")
        );
    }

    /// Restores an environment variable to its prior value on drop so an
    /// env-mutating test can't leak state into sibling tests. The
    /// `set_var`/`remove_var` calls are unsound if another thread reads
    /// the environment concurrently, so a caller must (a) run under
    /// `#[serial_test::serial]` and (b) pick a variable no parallel test
    /// touches. `serial_test` only serializes against other `#[serial]`
    /// tests, so (b) is what actually guards against the parallel
    /// non-serial tests in this binary.
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var(key).ok();
            // SAFETY: see the type doc. No parallel test reads or writes
            // this variable, so the set/remove can't race a `getenv`.
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            // SAFETY: see `EnvVarGuard::set`.
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    /// Precedence: a stored `auth.json` credential wins over a set
    /// environment variable, and the env var is consulted only as the
    /// fallback when nothing is stored. Uses `openrouter`, whose single
    /// env mapping (`OPENROUTER_API_KEY`) no other test reads.
    #[tokio::test]
    #[serial_test::serial]
    async fn get_api_key_stored_credential_beats_env_var() {
        let _env = EnvVarGuard::set("OPENROUTER_API_KEY", "from-env");
        let (_dir, path) = scratch_path("stored-beats-env");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        // Nothing stored yet, so the env var is the fallback.
        assert_eq!(
            storage
                .get_api_key("openrouter", None)
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("from-env"),
        );

        // A stored key now wins over the env var.
        storage
            .insert_bare(
                "openrouter",
                AuthCredential::ApiKey {
                    key: "from-file".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            storage
                .get_api_key("openrouter", None)
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("from-file"),
        );
    }

    /// OAuth refresh flow: an expired token gets refreshed via the
    /// registered provider and the new tokens are written back.
    #[tokio::test]
    async fn get_api_key_refreshes_expired_oauth() {
        struct StubProvider;

        #[async_trait]
        impl OAuthProvider for StubProvider {
            fn id(&self) -> &str {
                "stub"
            }
            fn name(&self) -> &str {
                "Stub"
            }
            async fn login(
                &self,
                _callbacks: &dyn OAuthCallbacks,
            ) -> Result<OAuthCredentials, OAuthError> {
                Ok(OAuthCredentials::new("r", "a", 0))
            }
            async fn refresh_token(
                &self,
                _credentials: &OAuthCredentials,
            ) -> Result<OAuthCredentials, OAuthError> {
                // Far-future expiration so the next get_api_key call
                // returns the fresh token without re-refreshing.
                Ok(OAuthCredentials::new(
                    "refreshed-r",
                    "refreshed-a",
                    i64::MAX,
                ))
            }
        }

        let (_dir, path) = scratch_path("refresh");
        let mut providers: HashMap<String, Arc<dyn OAuthProvider>> = HashMap::new();
        providers.insert("stub".into(), Arc::new(StubProvider));
        let storage = AuthStorage::with_providers(path.clone(), providers);

        // Pre-seed an expired token.
        storage
            .insert_bare(
                "stub",
                AuthCredential::OAuth(OAuthCredentials::new("old-r", "old-a", 1)),
            )
            .await
            .unwrap();

        let key = storage.get_api_key("stub", None).await.unwrap();
        assert_eq!(
            key.map(|resolved| resolved.key).as_deref(),
            Some("refreshed-a")
        );

        // Confirm the refreshed creds were persisted.
        match storage.get("stub").await.unwrap() {
            Some(AuthCredential::OAuth(c)) => {
                assert_eq!(c.access, "refreshed-a");
                assert_eq!(c.refresh, "refreshed-r");
                assert_eq!(c.expires, i64::MAX);
            }
            other => panic!("unexpected credential: {other:?}"),
        }
    }

    /// Storage should still serve a non-expired OAuth token without
    /// invoking the refresh callback.
    #[tokio::test]
    async fn get_api_key_uses_cached_oauth_when_fresh() {
        struct PanickyProvider;

        #[async_trait]
        impl OAuthProvider for PanickyProvider {
            fn id(&self) -> &str {
                "stub"
            }
            fn name(&self) -> &str {
                "Stub"
            }
            async fn login(
                &self,
                _callbacks: &dyn OAuthCallbacks,
            ) -> Result<OAuthCredentials, OAuthError> {
                panic!("login should not be called");
            }
            async fn refresh_token(
                &self,
                _credentials: &OAuthCredentials,
            ) -> Result<OAuthCredentials, OAuthError> {
                panic!("refresh_token should not be called for fresh credentials");
            }
        }

        let (_dir, path) = scratch_path("cached");
        let mut providers: HashMap<String, Arc<dyn OAuthProvider>> = HashMap::new();
        providers.insert("stub".into(), Arc::new(PanickyProvider));
        let storage = AuthStorage::with_providers(path.clone(), providers);

        storage
            .insert_bare(
                "stub",
                AuthCredential::OAuth(OAuthCredentials::new("r", "fresh-a", i64::MAX)),
            )
            .await
            .unwrap();

        let key = storage.get_api_key("stub", None).await.unwrap();
        assert_eq!(key.map(|resolved| resolved.key).as_deref(), Some("fresh-a"));
    }

    /// `get_api_key` returns `None` when nothing is configured at any
    /// layer — runtime, env, or file.
    #[tokio::test]
    async fn get_api_key_returns_none_when_unconfigured() {
        let (_dir, path) = scratch_path("none");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        let key = storage
            .get_api_key("nonexistent-provider-zzz", None)
            .await
            .unwrap();
        assert!(key.is_none());
    }

    /// A failing refresh surfaces as `AuthError::OAuth` (so the host
    /// can prompt a re-login) and must leave the stale stored
    /// credential on disk untouched, so a later attempt can retry.
    #[tokio::test]
    async fn get_api_key_surfaces_refresh_failure_and_keeps_stale_cred() {
        struct FailingProvider;

        #[async_trait]
        impl OAuthProvider for FailingProvider {
            fn id(&self) -> &str {
                "stub"
            }
            fn name(&self) -> &str {
                "Stub"
            }
            async fn login(
                &self,
                _callbacks: &dyn OAuthCallbacks,
            ) -> Result<OAuthCredentials, OAuthError> {
                Ok(OAuthCredentials::new("r", "a", 0))
            }
            async fn refresh_token(
                &self,
                _credentials: &OAuthCredentials,
            ) -> Result<OAuthCredentials, OAuthError> {
                Err(OAuthError::Other("refresh token rejected".into()))
            }
        }

        let (_dir, path) = scratch_path("refresh-fail");
        let mut providers: HashMap<String, Arc<dyn OAuthProvider>> = HashMap::new();
        providers.insert("stub".into(), Arc::new(FailingProvider));
        let storage = AuthStorage::with_providers(path.clone(), providers);

        storage
            .insert_bare(
                "stub",
                AuthCredential::OAuth(OAuthCredentials::new("stale-r", "stale-a", 1)),
            )
            .await
            .unwrap();

        match storage.get_api_key("stub", None).await {
            Err(AuthError::OAuth(_)) => {}
            other => panic!("expected AuthError::OAuth, got {other:?}"),
        }

        // The stale credential must still be there for a retry.
        match storage.get("stub").await.unwrap() {
            Some(AuthCredential::OAuth(c)) => {
                assert_eq!(c.refresh, "stale-r");
                assert_eq!(c.access, "stale-a");
            }
            other => panic!("stale credential should be preserved, got {other:?}"),
        }
    }

    /// A stored OAuth credential whose provider id isn't in the
    /// registry (a hand-edited/renamed id) resolves to `Ok(None)`
    /// rather than a hard error: we can't refresh it, so it's treated
    /// as unconfigured and the host prompts a fresh login. Even a
    /// non-expired token takes this path, since we have no flow to
    /// turn it into a usable key.
    #[tokio::test]
    async fn get_api_key_unknown_oauth_provider_resolves_to_none() {
        let (_dir, path) = scratch_path("unknown-oauth");
        // No providers registered, so any OAuth lookup misses.
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        storage
            .insert_bare(
                "typod-provider",
                AuthCredential::OAuth(OAuthCredentials::new("r", "fresh-a", i64::MAX)),
            )
            .await
            .unwrap();

        let key = storage.get_api_key("typod-provider", None).await.unwrap();
        assert!(
            key.is_none(),
            "unknown OAuth provider should resolve to None, got {key:?}"
        );
    }

    /// `find_env_keys` mirrors the table: anthropic prefers the
    /// OAuth token env var, then falls back to the API key.
    #[test]
    fn find_env_keys_anthropic_order() {
        assert_eq!(
            find_env_keys("anthropic"),
            &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"]
        );
    }

    /// OpenAI maps to a single env var.
    #[test]
    fn find_env_keys_openai() {
        assert_eq!(find_env_keys("openai"), &["OPENAI_API_KEY"]);
    }

    #[test]
    fn find_env_keys_openrouter() {
        assert_eq!(find_env_keys("openrouter"), &["OPENROUTER_API_KEY"]);
    }

    /// The OAuth-only `openai-codex` pool resolves to its own env var.
    /// It deliberately does *not* fall back to
    /// `OPENAI_API_KEY` — a regular OpenAI API key is not accepted
    /// against `chatgpt.com/backend-api`, and a Codex JWT is not
    /// accepted against `api.openai.com`, so leaking either across
    /// the boundary would surface as a confusing 401 mid-request.
    #[test]
    fn find_env_keys_openai_codex_is_distinct_from_openai() {
        assert_eq!(find_env_keys("openai-codex"), &["OPENAI_CODEX_OAUTH_TOKEN"]);
        // Sanity-check the inverse: `openai` does not pick up the
        // Codex env var.
        assert!(
            !find_env_keys("openai").contains(&"OPENAI_CODEX_OAUTH_TOKEN"),
            "`openai` provider id must not consume the Codex JWT env var"
        );
    }

    /// Unknown providers report an empty mapping rather than an
    /// error so callers can treat absence uniformly.
    #[test]
    fn find_env_keys_unknown_returns_empty() {
        assert!(find_env_keys("totally-fake-provider").is_empty());
    }

    /// Storing credentials gives the file a serializable shape that
    /// reads back identically (i.e. `{ provider: AuthCredential }`).
    #[tokio::test]
    async fn auth_file_format_is_provider_keyed_map() {
        let (_dir, path) = scratch_path("format");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        storage
            .insert_bare("openai", AuthCredential::ApiKey { key: "sk-1".into() })
            .await
            .unwrap();
        storage
            .insert_bare(
                "anthropic",
                AuthCredential::OAuth(OAuthCredentials::new("r", "a", 100)),
            )
            .await
            .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["openai"]["type"], "api_key");
        assert_eq!(parsed["openai"]["key"], "sk-1");
        assert_eq!(parsed["anthropic"]["type"], "oauth");
        assert_eq!(parsed["anthropic"]["refresh"], "r");
    }

    /// Default registry includes Anthropic + OpenAI Codex so out-of-the-box
    /// CLI usage can refresh both. Constructed via `AuthStorage::new`
    /// to exercise the same path embedders use. The OpenAI entry lives
    /// under `openai-codex` — the regular `openai`
    /// provider id is reserved for plain `OPENAI_API_KEY` credentials,
    /// which don't need a refresh flow.
    #[tokio::test]
    async fn default_registry_has_anthropic_and_openai_codex() {
        let (_dir, path) = scratch_path("registry");
        let storage = AuthStorage::new(path.clone());

        // Looking up by id should succeed; we verify via the
        // private helper indirectly by attempting to refresh — but
        // since we don't want to hit the network, we just confirm
        // the lookup resolves by checking via the public list-style
        // API isn't quite right. Instead verify by registering
        // known ids and seeing them stick.
        let providers = storage.state.lock().await.oauth_providers.clone();
        let mut ids: Vec<&str> = providers.values().map(|p| p.id()).collect();
        ids.sort();
        assert_eq!(ids, vec!["anthropic", "openai-codex"]);
    }

    /// A legacy `openai` OAuth entry in `auth.json` is invisibly
    /// migrated to `openai-codex` on read, so a user who logged in
    /// before the rename keeps their stored refresh token.
    /// `get` against the old id returns `None`; `get` against the
    /// new id returns the migrated credential.
    #[tokio::test]
    async fn read_migrates_legacy_openai_oauth_to_openai_codex() {
        let (_dir, path) = scratch_path("migrate");
        // Hand-write a pre-migration `auth.json` containing an OAuth
        // entry under the legacy `openai` key. This is exactly the
        // shape a previous-version `aj` would have produced.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = serde_json::json!({
            "openai": {
                "type": "oauth",
                "refresh": "legacy-refresh",
                "access": "legacy-access",
                "expires": i64::MAX,
                "accountId": "acc-legacy"
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        // Old id no longer surfaces the entry…
        assert!(storage.get("openai").await.unwrap().is_none());

        // …and the new id holds the migrated OAuth credential.
        match storage.get("openai-codex").await.unwrap() {
            Some(AuthCredential::OAuth(c)) => {
                assert_eq!(c.refresh, "legacy-refresh");
                assert_eq!(c.access, "legacy-access");
                assert_eq!(c.extra.get("accountId").unwrap(), "acc-legacy");
            }
            other => panic!("expected migrated OAuth entry under openai-codex, got {other:?}"),
        }
    }

    /// An `api_key` entry under `openai` is *not* migrated — that's a
    /// real `OPENAI_API_KEY` for the public API, distinct from the
    /// Codex OAuth pool. Migrating it would silently break the
    /// regular OpenAI provider's auth lookup.
    #[tokio::test]
    async fn read_preserves_openai_api_key_entries() {
        let (_dir, path) = scratch_path("preserve");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = serde_json::json!({
            "openai": {"type": "api_key", "key": "sk-keep-me"}
        });
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        match storage.get("openai").await.unwrap() {
            Some(AuthCredential::ApiKey { key }) => assert_eq!(key, "sk-keep-me"),
            other => panic!("expected untouched ApiKey under openai, got {other:?}"),
        }
        assert!(storage.get("openai-codex").await.unwrap().is_none());
    }

    /// If the user already has an `openai-codex` entry — e.g. they
    /// hand-edited the file or already migrated in a prior run — the
    /// legacy `openai` slot is left untouched. We never clobber an
    /// existing destination.
    #[tokio::test]
    async fn read_skips_migration_when_target_already_present() {
        let (_dir, path) = scratch_path("collision");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mixed = serde_json::json!({
            "openai": {
                "type": "oauth",
                "refresh": "legacy-r",
                "access": "legacy-a",
                "expires": 0
            },
            "openai-codex": {
                "type": "oauth",
                "refresh": "new-r",
                "access": "new-a",
                "expires": 1
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&mixed).unwrap()).unwrap();

        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        // Both entries remain.
        match storage.get("openai").await.unwrap() {
            Some(AuthCredential::OAuth(c)) => assert_eq!(c.refresh, "legacy-r"),
            other => panic!("expected legacy entry preserved, got {other:?}"),
        }
        match storage.get("openai-codex").await.unwrap() {
            Some(AuthCredential::OAuth(c)) => assert_eq!(c.refresh, "new-r"),
            other => panic!("expected new entry preserved, got {other:?}"),
        }
    }

    /// After the in-memory migration, the next mutating write
    /// (`set` / `remove`) persists the migrated shape to disk, so the
    /// legacy `openai` OAuth key disappears from `auth.json` once any
    /// real auth operation runs.
    #[tokio::test]
    async fn migration_persists_to_disk_on_next_write() {
        let (_dir, path) = scratch_path("persist");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = serde_json::json!({
            "openai": {
                "type": "oauth",
                "refresh": "legacy-refresh",
                "access": "legacy-access",
                "expires": i64::MAX
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        // A write to *any* provider causes its locked read-modify-write to
        // round-trip the migrated map.
        storage
            .insert_bare("anthropic", AuthCredential::ApiKey { key: "sk-x".into() })
            .await
            .unwrap();

        let on_disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            on_disk.get("openai").is_none(),
            "legacy openai key should be gone from disk, got: {on_disk}"
        );
        assert_eq!(on_disk["openai-codex"]["refresh"], "legacy-refresh");
        assert_eq!(on_disk["anthropic"]["type"], "api_key");
    }

    /// Concurrent writers serialize via the file lock. Ten parallel
    /// insert-only calls for distinct providers all land without losing entries.
    #[tokio::test]
    async fn concurrent_writes_serialize_via_lock() {
        let (_dir, path) = scratch_path("concurrent");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        let mut handles = Vec::new();
        for i in 0..10u8 {
            let s = storage.clone();
            handles.push(tokio::spawn(async move {
                s.insert_bare(
                    &format!("p{i}"),
                    AuthCredential::ApiKey {
                        key: format!("k{i}"),
                    },
                )
                .await
                .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let mut listed = storage.list().await.unwrap();
        listed.sort();
        let expected: Vec<String> = (0..10u8).map(|i| format!("p{i}")).collect();
        assert_eq!(listed, expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn first_write_creates_a_private_parent_and_file() {
        let root =
            TempDir::with_prefix("aj-auth-test-private-create-").expect("create scratch root");
        let parent = root.path().join("credentials");
        let path = parent.join("auth.json");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        storage
            .insert_bare(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "first-secret".into(),
                },
            )
            .await
            .expect("write first credential");

        assert_eq!(mode(&parent), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn lock_acquisition_creates_private_state_before_the_auth_file_exists() {
        let root = TempDir::with_prefix("aj-auth-test-private-lock-").expect("create scratch root");
        let parent = root.path().join("credentials");
        let path = parent.join("auth.json");
        let lock_path = lock_path_for(&path);

        let lock = FileLock::acquire(&path).await.expect("acquire auth lock");
        assert_eq!(mode(&parent), 0o700);
        assert_eq!(mode(&lock_path), 0o700);
        assert!(!path.exists(), "lock acquisition created the auth file");
        drop(lock);
        assert!(
            !lock_path.exists(),
            "lock guard did not release its directory"
        );
    }

    #[tokio::test]
    async fn relative_auth_paths_are_rejected_before_lock_creation() {
        for path in [
            PathBuf::from("aj-auth-relative-path-must-be-rejected.json"),
            PathBuf::from("./aj-auth-dot-path-must-be-rejected.json"),
        ] {
            let storage = AuthStorage::with_providers(path.clone(), HashMap::new());
            assert!(matches!(
                storage.list().await,
                Err(AuthError::Io(error)) if error.kind() == io::ErrorKind::InvalidInput
            ));
            assert!(!path.exists());
            assert!(!lock_path_for(&path).exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_write_repairs_a_permissive_parent_and_existing_file() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            TempDir::with_prefix("aj-auth-test-private-repair-").expect("create scratch root");
        let parent = root.path().join("credentials");
        let path = parent.join("auth.json");
        std::fs::create_dir(&parent).expect("create permissive parent");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777))
            .expect("make parent permissive");
        std::fs::write(
            &path,
            r#"{"existing":{"type":"api_key","key":"old-secret"}}"#,
        )
        .expect("seed existing credentials");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
            .expect("make auth file permissive");

        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());
        storage
            .insert_bare(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "new-secret".into(),
                },
            )
            .await
            .expect("replace through private storage boundary");

        assert_eq!(mode(&parent), 0o700);
        assert_eq!(mode(&path), 0o600);
        let stored = std::fs::read_to_string(&path).expect("read repaired credentials");
        assert!(stored.contains("old-secret"));
        assert!(stored.contains("new-secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_locked_read_repairs_a_permissive_parent_and_existing_file() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDir::with_prefix("aj-auth-test-private-read-").expect("create scratch root");
        let parent = root.path().join("credentials");
        let path = parent.join("auth.json");
        std::fs::create_dir(&parent).expect("create permissive parent");
        std::fs::write(
            &path,
            r#"{"anthropic":{"type":"api_key","key":"existing-secret"}}"#,
        )
        .expect("seed existing credentials");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777))
            .expect("make parent permissive");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
            .expect("make auth file permissive");

        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());
        assert_eq!(
            storage.list().await.expect("read through storage boundary"),
            ["anthropic"]
        );
        assert_eq!(mode(&parent), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn an_injected_partial_write_preserves_the_prior_complete_store() {
        let (_dir, path) = scratch_path("atomic-failure");
        let previous = br#"{"provider":{"type":"api_key","key":"complete-old-secret"}}"#;
        std::fs::write(&path, previous).expect("seed prior complete store");

        let result = replace_auth_file(
            &path,
            br#"{"provider":{"type":"api_key","key":"replacement-secret"}}"#,
            |file, bytes| {
                file.write_all(&bytes[..bytes.len() / 2])?;
                Err(io::Error::other("injected auth write failure"))
            },
        );

        assert!(
            matches!(result, Err(AuthError::Io(error)) if error.to_string() == "injected auth write failure")
        );
        assert_eq!(
            std::fs::read(&path).expect("read store after failed replacement"),
            previous
        );
        let entries = std::fs::read_dir(path.parent().expect("auth parent"))
            .expect("list auth parent")
            .collect::<Result<Vec<_>, _>>()
            .expect("read auth parent entries");
        assert_eq!(entries.len(), 1, "failed replacement left a temp file");
        assert_eq!(entries[0].path(), path);
    }

    #[cfg(unix)]
    #[test]
    fn successful_replacement_is_private_before_writing_and_replaces_the_inode() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            TempDir::with_prefix("aj-auth-test-atomic-success-").expect("create scratch root");
        let parent = root.path().join("credentials");
        let path = parent.join("auth.json");
        let prior_link = root.path().join("prior-auth.json");
        let previous = b"complete-old-secret";
        let replacement = b"complete-new-secret";
        std::fs::create_dir(&parent).expect("create auth parent");
        std::fs::write(&path, previous).expect("seed prior store");
        std::fs::hard_link(&path, &prior_link).expect("retain prior inode");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777))
            .expect("make parent permissive");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
            .expect("make auth file permissive");

        replace_auth_file(&path, replacement, |file, bytes| {
            assert_eq!(mode(&parent), 0o700);
            assert_eq!(mode(&path), 0o600);
            assert_eq!(
                file.metadata().expect("temp metadata").permissions().mode() & 0o777,
                0o600
            );

            let temporary = std::fs::read_dir(&parent)
                .expect("list replacement directory")
                .collect::<Result<Vec<_>, _>>()
                .expect("read replacement entries")
                .into_iter()
                .map(|entry| entry.path())
                .filter(|entry| entry != &path)
                .collect::<Vec<_>>();
            assert_eq!(
                temporary.len(),
                1,
                "replacement must use one same-directory temp file"
            );
            assert!(
                temporary[0]
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".auth.json.tmp-"))
            );

            file.write_all(bytes)
        })
        .expect("atomically replace auth store");

        assert_eq!(std::fs::read(&path).expect("read replacement"), replacement);
        assert_eq!(
            std::fs::read(&prior_link).expect("read retained prior inode"),
            previous,
            "replacement wrote through the destination instead of swapping it"
        );
        assert_eq!(mode(&parent), 0o700);
        assert_eq!(mode(&path), 0o600);
        let entries = std::fs::read_dir(&parent)
            .expect("list final auth parent")
            .collect::<Result<Vec<_>, _>>()
            .expect("read final auth entries");
        assert_eq!(entries.len(), 1, "successful replacement left a temp file");
        assert_eq!(entries[0].path(), path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_storage_mutation_replaces_the_destination_inode() {
        let root =
            TempDir::with_prefix("aj-auth-test-storage-replacement-").expect("create scratch root");
        let parent = root.path().join("credentials");
        let path = parent.join("auth.json");
        let prior_link = root.path().join("prior-auth.json");
        let previous = br#"{"existing":{"type":"api_key","key":"old-secret"}}"#;
        std::fs::create_dir(&parent).expect("create auth parent");
        std::fs::write(&path, previous).expect("seed prior store");
        std::fs::hard_link(&path, &prior_link).expect("retain prior inode");

        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());
        storage
            .insert_bare(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "new-secret".into(),
                },
            )
            .await
            .expect("mutate auth storage");

        assert_eq!(
            std::fs::read(&prior_link).expect("read retained prior inode"),
            previous,
            "AuthStorage rewrote the destination inode in place"
        );
        let stored = std::fs::read_to_string(&path).expect("read replacement store");
        assert!(stored.contains("old-secret"));
        assert!(stored.contains("new-secret"));
        assert_eq!(mode(&path), 0o600);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symbolic_auth_path_is_rejected_and_releases_the_lock() {
        use std::os::unix::fs::symlink;

        let (_dir, path) = scratch_path("permission-failure");
        let target = path.with_file_name("target.json");
        let previous = br#"{"provider":{"type":"api_key","key":"existing-secret"}}"#;
        std::fs::write(&target, previous).expect("seed symlink target");
        symlink(&target, &path).expect("create auth path symlink");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());
        let result = storage.list().await;

        assert!(matches!(
            result,
            Err(AuthError::Io(error)) if error.kind() == io::ErrorKind::InvalidInput
        ));
        assert!(
            std::fs::symlink_metadata(&path)
                .expect("symlink remains")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(&target).expect("read unchanged symlink target"),
            previous
        );
        assert!(
            !lock_path_for(&path).exists(),
            "symlink rejection leaked the acquired lock"
        );
    }

    /// The temporal first-create guarantee: on a missing destination, the
    /// production writer receives credential bytes only through an
    /// already-private temporary file in an already-private parent, never
    /// through the destination path. Observed during the write, not after it,
    /// so a write-then-chmod regression cannot pass on final modes alone.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_first_credential_write_is_private_before_its_first_byte() {
        use std::cell::RefCell;
        use std::os::unix::fs::PermissionsExt;
        use std::rc::Rc;

        let root =
            TempDir::with_prefix("aj-auth-test-temporal-create-").expect("create scratch root");
        let parent = root.path().join("credentials");
        let path = parent.join("auth.json");

        let observed = Rc::new(RefCell::new(None));
        let record = Rc::clone(&observed);
        let observer_path = path.clone();
        let observer_parent = parent.clone();
        let _guard = fault::install_write_hook(Box::new(move |file, _bytes| {
            let file_mode = file
                .metadata()
                .expect("replacement file metadata")
                .permissions()
                .mode()
                & 0o777;
            let same_directory_temp = std::fs::read_dir(&observer_parent)
                .expect("list auth parent during write")
                .filter_map(|entry| {
                    entry
                        .expect("read auth parent entry")
                        .file_name()
                        .into_string()
                        .ok()
                })
                .any(|name| name.starts_with(".auth.json.tmp-"));
            *record.borrow_mut() = Some((
                file_mode,
                mode(&observer_parent),
                observer_path.exists(),
                same_directory_temp,
            ));
            None
        }));

        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());
        storage
            .insert_bare(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "first-secret".into(),
                },
            )
            .await
            .expect("write first credential");

        let (file_mode, parent_mode, destination_existed, same_directory_temp) = observed
            .borrow_mut()
            .take()
            .expect("the first create must cross the production credential writer");
        assert_eq!(
            file_mode, 0o600,
            "credential bytes must land on an already-private file"
        );
        assert_eq!(
            parent_mode, 0o700,
            "credential bytes must land in an already-private parent"
        );
        assert!(
            !destination_existed,
            "first-create bytes must go through the temporary file, not the destination"
        );
        assert!(
            same_directory_temp,
            "the replacement file must live in the destination's own directory"
        );
        assert_eq!(mode(&path), 0o600);
        let stored = std::fs::read_to_string(&path).expect("read first store");
        assert!(stored.contains("first-secret"));
    }

    /// Each hardening chmod is failed in isolation and the storage operation
    /// must surface exactly that failure. The fixture is meaningful because
    /// every other operation succeeds: after clearing the fault the same
    /// insert completes, so a best-effort regression that swallowed the
    /// failure would report success and the assertions below would fail.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_injected_permission_failure_surfaces_at_every_hardening_site() {
        use std::os::unix::fs::PermissionsExt;

        for site in [
            PermissionSite::Parent,
            PermissionSite::ExistingFile,
            PermissionSite::ReplacementFile,
        ] {
            let root = TempDir::with_prefix("aj-auth-test-permission-fault-")
                .expect("create scratch root");
            let parent = root.path().join("credentials");
            let path = parent.join("auth.json");
            let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

            // ExistingFile and ReplacementFile need a destination whose bytes
            // the failed operation must leave untouched. ExistingFile fires
            // only on a permissive destination in need of repair.
            let prior = match site {
                PermissionSite::Parent => None,
                PermissionSite::ExistingFile | PermissionSite::ReplacementFile => {
                    storage
                        .insert_bare(
                            "anthropic",
                            AuthCredential::ApiKey {
                                key: "prior-secret".into(),
                            },
                        )
                        .await
                        .expect("seed prior store");
                    if site == PermissionSite::ExistingFile {
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
                            .expect("make destination permissive");
                    }
                    Some(std::fs::read(&path).expect("read prior store"))
                }
            };

            let guard = fault::install_permission_failure(site);
            let result = storage
                .insert_bare(
                    "openai",
                    AuthCredential::ApiKey {
                        key: "injected-secret".into(),
                    },
                )
                .await;
            let error = match result {
                Err(AuthError::Io(error)) => error,
                other => panic!("{site:?}: the hardening failure must surface, got {other:?}"),
            };
            assert_eq!(
                error.kind(),
                io::ErrorKind::PermissionDenied,
                "{site:?} surfaced the wrong kind: {error}"
            );
            assert!(
                error.to_string().contains("injected"),
                "{site:?} surfaced a different failure: {error}"
            );
            match &prior {
                None => assert!(
                    !path.exists(),
                    "{site:?}: a failed first write must not publish a store"
                ),
                Some(prior) => assert_eq!(
                    &std::fs::read(&path).expect("read store after failure"),
                    prior,
                    "{site:?}: the failed write changed the prior store"
                ),
            }
            assert!(
                !lock_path_for(&path).exists(),
                "{site:?}: the failure leaked the acquired lock"
            );
            if parent.exists() {
                let residue: Vec<_> = std::fs::read_dir(&parent)
                    .expect("list auth parent")
                    .collect::<Result<Vec<_>, _>>()
                    .expect("read auth parent entries")
                    .into_iter()
                    .map(|entry| entry.path())
                    .filter(|entry| entry != &path)
                    .collect();
                assert!(residue.is_empty(), "{site:?} left residue: {residue:?}");
            }

            // replace_auth_file's own hardening calls are shadowed by the
            // lock's on the storage path above; they are the only defense for
            // a future direct caller, so their propagation is pinned here.
            let direct = replace_auth_file(&path, b"{}", write_credentials);
            assert!(
                matches!(
                    direct,
                    Err(AuthError::Io(error)) if error.to_string().contains("injected")
                ),
                "{site:?}: replace_auth_file must surface its own hardening failure"
            );

            drop(guard);
            storage
                .insert_bare(
                    "openai",
                    AuthCredential::ApiKey {
                        key: "replacement-secret".into(),
                    },
                )
                .await
                .unwrap_or_else(|error| {
                    panic!("{site:?}: the fixture must succeed without the fault: {error}")
                });
        }
    }

    /// A failure of the real production writer, reached through
    /// `insert_bare`, must surface and leave the prior complete store
    /// byte-identical. This crosses `write_auth_file`'s own writer rather
    /// than substituting a test closure, so swallowing its result anywhere
    /// on the path publishes a partial store and fails here.
    #[tokio::test]
    async fn a_failed_production_write_preserves_the_prior_complete_store() {
        use std::cell::Cell;
        use std::rc::Rc;

        let (_dir, path) = scratch_path("production-write-failure");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());
        storage
            .insert_bare(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "prior-secret".into(),
                },
            )
            .await
            .expect("seed prior complete store");
        let prior = std::fs::read(&path).expect("read prior store");

        let hook_fired = Rc::new(Cell::new(false));
        let fired = Rc::clone(&hook_fired);
        let guard = fault::install_write_hook(Box::new(move |file, bytes| {
            fired.set(true);
            file.write_all(&bytes[..bytes.len() / 2])
                .expect("partial injected write");
            Some(Err(io::Error::other("injected production write failure")))
        }));

        let result = storage
            .insert_bare(
                "openai",
                AuthCredential::ApiKey {
                    key: "replacement-secret".into(),
                },
            )
            .await;
        assert!(
            hook_fired.get(),
            "the injection must cross write_auth_file's production writer"
        );
        assert!(matches!(
            result,
            Err(AuthError::Io(error))
                if error.to_string() == "injected production write failure"
        ));
        assert_eq!(
            std::fs::read(&path).expect("read store after failed write"),
            prior,
            "a failed write published a partial store"
        );
        let entries = std::fs::read_dir(path.parent().expect("auth parent"))
            .expect("list auth parent")
            .collect::<Result<Vec<_>, _>>()
            .expect("read auth parent entries");
        assert_eq!(entries.len(), 1, "the failed write left a temp file");
        assert_eq!(entries[0].path(), path);

        drop(guard);
        storage
            .insert_bare(
                "openai",
                AuthCredential::ApiKey {
                    key: "replacement-secret".into(),
                },
            )
            .await
            .expect("write succeeds without the fault");
        let stored = std::fs::read_to_string(&path).expect("read replacement store");
        assert!(stored.contains("prior-secret"));
        assert!(stored.contains("replacement-secret"));
    }

    /// A real device-level write failure crossing the completely untouched
    /// production writer: the hook only swaps the replacement handle for
    /// `/dev/full` and falls through, so `write_credentials`' own `write_all`
    /// takes the `ENOSPC`. This is the one seam-free propagation proof:
    /// swallowing the write result anywhere, including inside the production
    /// writer's tail, publishes an empty store and fails the prior-bytes
    /// assertion.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_real_device_write_failure_preserves_the_prior_complete_store() {
        let (_dir, path) = scratch_path("device-write-failure");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());
        storage
            .insert_bare(
                "anthropic",
                AuthCredential::ApiKey {
                    key: "prior-secret".into(),
                },
            )
            .await
            .expect("seed prior complete store");
        let prior = std::fs::read(&path).expect("read prior store");

        let guard = fault::install_write_hook(Box::new(move |file, _bytes| {
            *file = std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .expect("open /dev/full");
            None
        }));

        let result = storage
            .insert_bare(
                "openai",
                AuthCredential::ApiKey {
                    key: "replacement-secret".into(),
                },
            )
            .await;
        match result {
            Err(AuthError::Io(error)) => assert_eq!(
                error.kind(),
                io::ErrorKind::StorageFull,
                "the device failure must surface as-is: {error}"
            ),
            other => panic!("a full device must fail the write, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&path).expect("read store after failed write"),
            prior,
            "a failed device write published a replacement store"
        );
        let entries = std::fs::read_dir(path.parent().expect("auth parent"))
            .expect("list auth parent")
            .collect::<Result<Vec<_>, _>>()
            .expect("read auth parent entries");
        assert_eq!(entries.len(), 1, "the failed write left a temp file");
        assert_eq!(entries[0].path(), path);

        drop(guard);
        storage
            .insert_bare(
                "openai",
                AuthCredential::ApiKey {
                    key: "replacement-secret".into(),
                },
            )
            .await
            .expect("write succeeds without the fault");
    }

    /// `try_steal_stale_lock` should leave fresh locks alone but
    /// remove ones whose mtime is older than the supplied threshold.
    /// Drives the helper directly with a near-zero `max_age` so the
    /// test doesn't have to wait for the production
    /// [`STALE_LOCK_AGE`] to elapse.
    #[tokio::test]
    async fn stale_lock_is_stealable() {
        let (_dir, path) = scratch_path("stale");
        let lock_path = lock_path_for(&path);

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::create_dir(&lock_path).unwrap();

        // Just-created lock: way younger than 60 s, must not be
        // stolen.
        assert!(
            !try_steal_stale_lock(&lock_path, STALE_LOCK_AGE),
            "fresh lock must not be stolen"
        );

        // Wait long enough that a 1 ms threshold considers the lock
        // stale, then confirm the helper steals it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            try_steal_stale_lock(&lock_path, Duration::from_millis(1)),
            "lock past max_age must be stolen"
        );
        assert!(
            !lock_path.exists(),
            "stolen lock directory should be removed"
        );
    }

    // -----------------------------------------------------------------
    // Labeled accounts
    // -----------------------------------------------------------------

    fn api_key(key: &str) -> AuthCredential {
        AuthCredential::ApiKey { key: key.into() }
    }

    #[tokio::test]
    async fn duplicate_creation_fails_before_oauth_and_preserves_the_credential() {
        let (_dir, path) = scratch_path("duplicate-login");
        let (storage, calls) = racing_storage(path, LoginRace::None);
        storage
            .insert_account("stub", "work", api_key("keep"))
            .await
            .unwrap();

        assert!(matches!(
            storage
                .login_account("stub", Some("work"), &TestCallbacks)
                .await,
            Err(AuthError::DuplicateAccount { .. })
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0, "OAuth never started");
        assert!(matches!(
            storage.get_account("stub", "work").await.unwrap(),
            Some(AuthCredential::ApiKey { key }) if key == "keep"
        ));
    }

    #[tokio::test]
    async fn first_login_stays_bare_and_is_insert_only() {
        let (_dir, path) = scratch_path("first-login");
        let (storage, calls) = racing_storage(path, LoginRace::None);
        storage
            .login("stub", &TestCallbacks)
            .await
            .expect("first login succeeds");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(matches!(
            storage.stored_credentials("stub").await.unwrap(),
            Some(StoredProviderCredentials::Bare(AuthCredential::OAuth(credentials)))
                if credentials.access == "new-access"
        ));
    }

    #[tokio::test]
    async fn first_login_race_preserves_the_concurrent_bare_credential() {
        let (_dir, path) = scratch_path("first-login-race");
        let (storage, calls) = racing_storage(
            path,
            LoginRace::InsertBare {
                key: "racing".to_string(),
            },
        );
        assert!(matches!(
            storage.login("stub", &TestCallbacks).await,
            Err(AuthError::ProviderAlreadyConfigured { .. })
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(matches!(
            storage.stored_credentials("stub").await.unwrap(),
            Some(StoredProviderCredentials::Bare(AuthCredential::ApiKey { key }))
                if key == "racing"
        ));
    }

    #[tokio::test]
    async fn cloned_read_observer_counts_reads_and_read_modify_writes() {
        let (_dir, path) = scratch_path("read-observer");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        let clone = storage.clone();
        storage.reset_credential_read_count();

        clone.list().await.expect("read through clone");
        assert_eq!(
            storage.credential_read_count(),
            1,
            "the clone shares the observer"
        );
        clone
            .insert_bare("stub", api_key("calibration"))
            .await
            .expect("read-modify-write through clone");
        assert_eq!(
            storage.credential_read_count(),
            2,
            "a mutating boundary also increments"
        );
        storage.reset_credential_read_count();
        assert_eq!(storage.credential_read_count(), 0);
    }

    #[tokio::test]
    async fn creation_race_is_duplicate_and_preserves_the_concurrent_credential() {
        let (_dir, path) = scratch_path("creation-race");
        let (storage, calls) = racing_storage(
            path,
            LoginRace::Insert {
                label: "work".to_string(),
                key: "racing".to_string(),
            },
        );

        assert!(matches!(
            storage
                .login_account("stub", Some("work"), &TestCallbacks)
                .await,
            Err(AuthError::DuplicateAccount { .. })
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1, "OAuth did run once");
        assert!(matches!(
            storage.get_account("stub", "work").await.unwrap(),
            Some(AuthCredential::ApiKey { key }) if key == "racing"
        ));
    }

    #[tokio::test]
    async fn second_oauth_login_promotes_and_preserves_the_bare_credential() {
        let (_dir, path) = scratch_path("second-login-promotes");
        let (storage, calls) = racing_storage(path, LoginRace::None);
        storage
            .insert_bare("stub", api_key("existing"))
            .await
            .unwrap();

        storage
            .login_account("stub", Some("work"), &TestCallbacks)
            .await
            .expect("second login creates a labeled account");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let set = storage
            .accounts("stub")
            .await
            .unwrap()
            .expect("promoted set");
        assert_eq!(set.default, DEFAULT_ACCOUNT_LABEL);
        assert!(matches!(
            storage
                .get_account("stub", DEFAULT_ACCOUNT_LABEL)
                .await
                .unwrap(),
            Some(AuthCredential::ApiKey { key }) if key == "existing"
        ));
        assert!(matches!(
            storage.get_account("stub", "work").await.unwrap(),
            Some(AuthCredential::OAuth(credentials)) if credentials.access == "new-access"
        ));
    }

    #[tokio::test]
    async fn second_login_default_collision_refuses_before_oauth_without_rewriting_bare_bytes() {
        let (_dir, path) = scratch_path("second-login-default-collision");
        let (storage, calls) = racing_storage(path.clone(), LoginRace::None);
        storage
            .insert_bare("stub", api_key("existing"))
            .await
            .unwrap();
        let before = std::fs::read(&path).unwrap();

        assert!(matches!(
            storage
                .login_account("stub", Some(DEFAULT_ACCOUNT_LABEL), &TestCallbacks)
                .await,
            Err(AuthError::DuplicateAccount { .. })
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(storage.accounts("stub").await.unwrap().is_none());
        assert!(matches!(
            storage.get("stub").await.unwrap(),
            Some(AuthCredential::ApiKey { key }) if key == "existing"
        ));
    }

    #[tokio::test]
    async fn replacement_requires_an_initial_exact_target_before_oauth() {
        let (_dir, path) = scratch_path("missing-replacement");
        let (storage, calls) = racing_storage(path, LoginRace::None);

        assert!(matches!(
            storage
                .replace_login_account("stub", Some("work"), &TestCallbacks)
                .await,
            Err(AuthError::ProviderCredentialChanged { .. })
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 0, "OAuth never started");
    }

    #[tokio::test]
    async fn removed_valid_replacement_target_is_recreated_under_current_rules() {
        let (_dir, path) = scratch_path("valid-replacement-race");
        let (storage, calls) = racing_storage(
            path,
            LoginRace::Remove {
                label: "work".to_string(),
            },
        );
        storage
            .insert_account("stub", "work", api_key("old"))
            .await
            .unwrap();

        storage
            .replace_login_account("stub", Some("work"), &TestCallbacks)
            .await
            .expect("current-valid absent target may be recreated");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(matches!(
            storage.get_account("stub", "work").await.unwrap(),
            Some(AuthCredential::OAuth(credentials)) if credentials.access == "new-access"
        ));
    }

    #[tokio::test]
    async fn legacy_replacement_works_in_place_but_is_not_recreated_after_removal() {
        let legacy = "wo\nrk";

        let (_kept_dir, kept_path) = scratch_path("legacy-replace-kept");
        std::fs::write(
            &kept_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "stub": {
                    "type": "accounts",
                    "default": legacy,
                    "accounts": {
                        (legacy): { "type": "api_key", "key": "old" }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let (kept, _) = racing_storage(kept_path, LoginRace::None);
        kept.replace_login_account("stub", Some(legacy), &TestCallbacks)
            .await
            .expect("existing legacy key remains replaceable");
        assert!(matches!(
            kept.get_account("stub", legacy).await.unwrap(),
            Some(AuthCredential::OAuth(credentials)) if credentials.access == "new-access"
        ));

        let (_removed_dir, removed_path) = scratch_path("legacy-replace-removed");
        std::fs::write(
            &removed_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "stub": {
                    "type": "accounts",
                    "default": legacy,
                    "accounts": {
                        (legacy): { "type": "api_key", "key": "old" }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let (removed, _) = racing_storage(
            removed_path,
            LoginRace::Remove {
                label: legacy.to_string(),
            },
        );
        assert!(matches!(
            removed
                .replace_login_account("stub", Some(legacy), &TestCallbacks)
                .await,
            Err(AuthError::InvalidLabel(_))
        ));
        assert!(
            removed.get("stub").await.unwrap().is_none(),
            "removed legacy target was not recreated"
        );
    }

    #[tokio::test]
    async fn overlength_legacy_key_remains_exactly_replaceable_while_present() {
        let label = "a".repeat(MAX_ACCOUNT_LABEL_BYTES + 1);
        let (_dir, path) = scratch_path("overlength-replace");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "stub": {
                    "type": "accounts",
                    "default": label,
                    "accounts": {
                        (label.clone()): { "type": "api_key", "key": "old" }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let (storage, _) = racing_storage(path, LoginRace::None);
        storage
            .replace_login_account("stub", Some(&label), &TestCallbacks)
            .await
            .expect("present overlength legacy key remains replaceable");
        assert!(matches!(
            storage.get_account("stub", &label).await.unwrap(),
            Some(AuthCredential::OAuth(credentials)) if credentials.access == "new-access"
        ));
    }

    #[tokio::test]
    async fn removed_overlength_replacement_target_is_not_recreated() {
        let label = "a".repeat(MAX_ACCOUNT_LABEL_BYTES + 1);
        let (_dir, path) = scratch_path("overlength-replace-removed");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "stub": {
                    "type": "accounts",
                    "default": label.clone(),
                    "accounts": {
                        (label.clone()): { "type": "api_key", "key": "old" }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let (storage, _) = racing_storage(
            path,
            LoginRace::Remove {
                label: label.clone(),
            },
        );
        assert!(matches!(
            storage
                .replace_login_account("stub", Some(&label), &TestCallbacks)
                .await,
            Err(AuthError::InvalidLabel(_))
        ));
        assert!(storage.get("stub").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn over_limit_legacy_identity_lists_resolves_defaults_refreshes_and_removes_exactly() {
        struct LegacyRefreshProvider;

        #[async_trait]
        impl OAuthProvider for LegacyRefreshProvider {
            fn id(&self) -> &str {
                "stub"
            }

            fn name(&self) -> &str {
                "Stub"
            }

            async fn login(
                &self,
                _callbacks: &dyn OAuthCallbacks,
            ) -> Result<OAuthCredentials, OAuthError> {
                Ok(OAuthCredentials::new(
                    "reactivated-r",
                    "reactivated-a",
                    i64::MAX,
                ))
            }

            async fn refresh_token(
                &self,
                _credentials: &OAuthCredentials,
            ) -> Result<OAuthCredentials, OAuthError> {
                Ok(OAuthCredentials::new(
                    "refreshed-r",
                    "refreshed-a",
                    i64::MAX,
                ))
            }
        }

        let label = format!("{}\u{1000}", "a".repeat(10_921));
        assert_eq!(
            display_account_label(&label, AccountLabelDisplayMode::Ordinary).len(),
            65_536,
            "fixture crosses the terminal inspection boundary"
        );
        let (_dir, path) = scratch_path("over-limit-legacy-matrix");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "stub": {
                    "type": "accounts",
                    "default": "safe",
                    "accounts": {
                        "safe": { "type": "api_key", "key": "safe-key" },
                        (label.clone()): {
                            "type": "oauth",
                            "refresh": "old-r",
                            "access": "old-a",
                            "expires": 0
                        }
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let provider: Arc<dyn OAuthProvider> = Arc::new(LegacyRefreshProvider);
        let storage = AuthStorage::with_providers(
            path.clone(),
            HashMap::from([("stub".to_string(), provider)]),
        );

        let set = storage.accounts("stub").await.unwrap().unwrap();
        assert!(set.accounts.iter().any(|(current, _)| current == &label));
        assert!(storage.get_account("stub", &label).await.unwrap().is_some());
        assert!(matches!(
            storage
                .insert_account("stub", &label, api_key("overwrite"))
                .await,
            Err(AuthError::DuplicateAccount { .. })
        ));
        storage.set_default_account("stub", &label).await.unwrap();
        let resolved = storage
            .get_api_key("stub", None)
            .await
            .unwrap()
            .expect("legacy default refreshes");
        assert_eq!(resolved.key, "refreshed-a");
        assert_eq!(resolved.source, CredentialSource::Account(label.clone()));
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["stub"]["default"], label);
        assert_eq!(
            json["stub"]["accounts"][label.as_str()]["access"],
            "refreshed-a"
        );

        storage
            .replace_login_account("stub", Some(&label), &TestCallbacks)
            .await
            .expect("over-limit legacy identity remains reactivatable");
        assert!(matches!(
            storage.get_account("stub", &label).await.unwrap(),
            Some(AuthCredential::OAuth(credentials)) if credentials.access == "reactivated-a"
        ));

        storage.set_default_account("stub", "safe").await.unwrap();
        storage.remove_account("stub", &label).await.unwrap();
        assert!(storage.get_account("stub", &label).await.unwrap().is_none());
        assert!(storage.get_account("stub", "safe").await.unwrap().is_some());
    }

    /// A pre-accounts auth.json, as literal bytes rather than a
    /// round-trip through our own serializer: a serializer bug that
    /// changed the written shape would keep a round-trip green while
    /// every real user's file stopped parsing.
    #[tokio::test]
    async fn pre_accounts_file_parses_unchanged() {
        let (_dir, path) = scratch_path("legacy-shape");
        std::fs::write(
            &path,
            r#"{
  "anthropic": { "type": "oauth", "refresh": "r1", "access": "a1", "expires": 9999999999999 },
  "openrouter": { "type": "api_key", "key": "sk-or" }
}"#,
        )
        .unwrap();
        let storage = AuthStorage::with_providers(path, HashMap::new());

        match storage.get("anthropic").await.unwrap() {
            Some(AuthCredential::OAuth(c)) => assert_eq!(c.access, "a1"),
            other => panic!("unexpected credential: {other:?}"),
        }
        assert_eq!(
            storage
                .get_api_key("openrouter", None)
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("sk-or"),
        );
        // A bare entry is not a labeled set.
        assert!(storage.accounts("anthropic").await.unwrap().is_none());
    }

    /// Growing a bare entry keeps the existing credential under the
    /// adopted label AND as the default: adding an account never moves
    /// what unlabeled resolution bills against.
    #[tokio::test]
    async fn growing_a_bare_entry_keeps_it_the_default() {
        let (_dir, path) = scratch_path("grow");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());
        storage
            .insert_bare("prov-x", api_key("first"))
            .await
            .unwrap();
        storage
            .insert_account("prov-x", "work", api_key("second"))
            .await
            .unwrap();

        let set = storage.accounts("prov-x").await.unwrap().expect("a set");
        assert_eq!(set.default, DEFAULT_ACCOUNT_LABEL);
        assert_eq!(
            set.accounts
                .iter()
                .map(|(l, _)| l.as_str())
                .collect::<Vec<_>>(),
            vec![DEFAULT_ACCOUNT_LABEL, "work"],
            "sorted labels, both credentials present",
        );
        assert_eq!(
            storage
                .get_api_key("prov-x", None)
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("first"),
            "unlabeled resolution still bills the pre-existing credential",
        );
        assert_eq!(
            storage
                .get_api_key("prov-x", Some("work"))
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("second"),
        );

        // And the disk shape is the documented one.
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["prov-x"]["type"], "accounts");
        assert_eq!(json["prov-x"]["default"], DEFAULT_ACCOUNT_LABEL);
        assert_eq!(json["prov-x"]["accounts"]["work"]["type"], "api_key");
    }

    /// A label the set does not hold resolves to nothing, never to the
    /// default: serving a different credential than the one named is
    /// the mis-billing the chain exists to prevent.
    #[tokio::test]
    async fn an_absent_label_never_falls_back_to_the_default() {
        let (_dir, path) = scratch_path("absent-label");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage
            .insert_account("prov-x", "personal", api_key("k1"))
            .await
            .unwrap();

        assert_eq!(
            storage
                .get_api_key("prov-x", None)
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("k1"),
            "the default resolves, otherwise this test measures nothing",
        );
        let resolved = storage.get_api_key("prov-x", Some("work")).await.unwrap();
        assert!(
            resolved.is_none(),
            "a labeled ask resolves nothing rather than another credential, got {resolved:?}"
        );
        assert!(
            storage
                .get_account("prov-x", "work")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// Any label against a bare entry is a miss.
    #[tokio::test]
    async fn a_label_against_a_bare_entry_misses() {
        let (_dir, path) = scratch_path("label-vs-bare");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage.insert_bare("prov-x", api_key("k1")).await.unwrap();
        let resolved = storage.get_api_key("prov-x", Some("work")).await.unwrap();
        assert!(
            resolved.is_none(),
            "a labeled ask resolves nothing rather than another credential, got {resolved:?}"
        );
    }

    /// Every source names itself, and the one that matters is the
    /// unlabeled ask against a labeled set: it resolves the store's
    /// default, and the label it resolved is knowledge only this call
    /// has. A caller cannot reconstruct it, because a second read could
    /// see a different default.
    #[tokio::test]
    async fn an_unlabeled_ask_reports_the_default_label_it_resolved() {
        let (_dir, path) = scratch_path("source-default");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage
            .insert_account("prov-x", "personal", api_key("k1"))
            .await
            .unwrap();

        let resolved = storage
            .get_api_key("prov-x", None)
            .await
            .unwrap()
            .expect("the default resolves, otherwise this test measures nothing");
        assert_eq!(resolved.key, "k1");
        assert_eq!(
            resolved.source,
            CredentialSource::Account("personal".to_string()),
            "an unlabeled ask reports the default's label, not an absent one"
        );
        assert_eq!(resolved.source.label(), Some("personal"));
    }

    #[tokio::test]
    async fn a_labeled_ask_reports_the_label_it_was_given() {
        let (_dir, path) = scratch_path("source-labeled");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage
            .insert_account("prov-x", "personal", api_key("k1"))
            .await
            .unwrap();
        storage
            .insert_account("prov-x", "work", api_key("k2"))
            .await
            .unwrap();

        let resolved = storage
            .get_api_key("prov-x", Some("work"))
            .await
            .unwrap()
            .expect("the named account resolves");
        assert_eq!(resolved.key, "k2");
        assert_eq!(
            resolved.source,
            CredentialSource::Account("work".to_string())
        );
    }

    /// A bare entry is every pre-feature `auth.json`, and it reports a
    /// source of its own rather than a label nobody typed.
    #[tokio::test]
    async fn a_bare_entry_reports_itself_as_bare() {
        let (_dir, path) = scratch_path("source-bare");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage.insert_bare("prov-x", api_key("k1")).await.unwrap();

        let resolved = storage
            .get_api_key("prov-x", None)
            .await
            .unwrap()
            .expect("the bare entry resolves");
        assert_eq!(resolved.source, CredentialSource::Bare);
        assert_eq!(resolved.source.label(), None);
    }

    /// The override wins even over an explicit label, so it must not
    /// report that label: the turn did not run on that account. It is
    /// not `Bare` either, since nothing in the file served it.
    #[tokio::test]
    async fn the_runtime_override_reports_itself_and_not_the_label_asked_for() {
        let (_dir, path) = scratch_path("source-override");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage
            .insert_account("prov-x", "work", api_key("k2"))
            .await
            .unwrap();
        storage
            .set_runtime_api_key("prov-x", "from-runtime".into())
            .await;

        let resolved = storage
            .get_api_key("prov-x", Some("work"))
            .await
            .unwrap()
            .expect("the override resolves");
        assert_eq!(
            resolved.key, "from-runtime",
            "the override wins over the label, otherwise this test measures nothing"
        );
        assert_eq!(
            resolved.source,
            CredentialSource::Override,
            "a turn the override served is not a turn on the account that was asked for"
        );
    }

    /// An env-served turn is not the unnamed stored credential either,
    /// so it reports its own source and not `Bare`.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_env_key_reports_itself_as_environment() {
        let _env = EnvVarGuard::set("OPENROUTER_API_KEY", "from-env");
        let (_dir, path) = scratch_path("source-env");
        let storage = AuthStorage::with_providers(path, HashMap::new());

        let resolved = storage
            .get_api_key("openrouter", None)
            .await
            .unwrap()
            .expect("the env var serves an unlabeled ask");
        assert_eq!(resolved.key, "from-env");
        assert_eq!(resolved.source, CredentialSource::Environment);
        assert_eq!(resolved.source.label(), None);
    }

    /// A labeled ask never falls through to the environment: env keys
    /// carry no account identity, so serving one for a named account
    /// would bill a credential the caller did not pick.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_labeled_miss_ignores_the_environment() {
        let _env = EnvVarGuard::set("OPENROUTER_API_KEY", "from-env");
        let (_dir, path) = scratch_path("label-no-env");
        let storage = AuthStorage::with_providers(path, HashMap::new());

        assert_eq!(
            storage
                .get_api_key("openrouter", None)
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("from-env"),
            "the env var is set and serves unlabeled asks, \
             otherwise this test measures nothing",
        );
        let resolved = storage
            .get_api_key("openrouter", Some("work"))
            .await
            .unwrap();
        assert!(
            resolved.is_none(),
            "a labeled ask never reaches the environment, got {resolved:?}"
        );
    }

    /// The runtime override is the loudest instruction and wins even
    /// over an explicit account pick.
    #[tokio::test]
    async fn runtime_override_wins_over_a_labeled_ask() {
        let (_dir, path) = scratch_path("override-vs-label");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage
            .insert_account("prov-x", "work", api_key("stored"))
            .await
            .unwrap();
        storage
            .set_runtime_api_key("prov-x", "override".into())
            .await;
        assert_eq!(
            storage
                .get_api_key("prov-x", Some("work"))
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("override"),
        );
    }

    /// An expired token in a NON-default slot refreshes in place: the
    /// refreshed credential lands back in its own slot and the default
    /// account's credential is untouched.
    #[tokio::test]
    async fn refresh_writes_the_labeled_slot_not_the_default() {
        struct StubProvider;

        #[async_trait]
        impl OAuthProvider for StubProvider {
            fn id(&self) -> &str {
                "stub"
            }
            fn name(&self) -> &str {
                "Stub"
            }
            async fn login(
                &self,
                _callbacks: &dyn OAuthCallbacks,
            ) -> Result<OAuthCredentials, OAuthError> {
                Ok(OAuthCredentials::new("r", "a", 0))
            }
            async fn refresh_token(
                &self,
                _credentials: &OAuthCredentials,
            ) -> Result<OAuthCredentials, OAuthError> {
                Ok(OAuthCredentials::new(
                    "refreshed-r",
                    "refreshed-a",
                    i64::MAX,
                ))
            }
        }

        let (_dir, path) = scratch_path("labeled-refresh");
        let mut providers: HashMap<String, Arc<dyn OAuthProvider>> = HashMap::new();
        providers.insert("stub".into(), Arc::new(StubProvider));
        let storage = AuthStorage::with_providers(path, providers);

        // Default account holds a FRESH token, the picked account an
        // expired one, so only the picked slot has any business moving.
        storage
            .insert_account(
                "stub",
                "personal",
                AuthCredential::OAuth(OAuthCredentials::new("p-r", "p-a", i64::MAX)),
            )
            .await
            .unwrap();
        storage
            .insert_account(
                "stub",
                "work",
                AuthCredential::OAuth(OAuthCredentials::new("w-r", "w-a", 1)),
            )
            .await
            .unwrap();
        assert_eq!(
            storage
                .accounts("stub")
                .await
                .unwrap()
                .expect("a set")
                .default,
            "personal",
            "the first account is the default, \
             otherwise this test measures nothing",
        );

        let resolved = storage
            .get_api_key("stub", Some("work"))
            .await
            .unwrap()
            .expect("the refreshed account resolves");
        assert_eq!(resolved.key, "refreshed-a");
        assert_eq!(
            resolved.source,
            CredentialSource::Account("work".to_string()),
            "refresh preserves the source slot with the key"
        );

        match storage.get_account("stub", "work").await.unwrap() {
            Some(AuthCredential::OAuth(c)) => assert_eq!(c.access, "refreshed-a"),
            other => panic!("unexpected credential: {other:?}"),
        }
        match storage.get_account("stub", "personal").await.unwrap() {
            Some(AuthCredential::OAuth(c)) => {
                assert_eq!(c.access, "p-a", "the default slot is untouched")
            }
            other => panic!("unexpected credential: {other:?}"),
        }
    }

    /// Bare creation and replacement never infer a labeled target from the
    /// default at lock acquisition.
    #[tokio::test]
    async fn bare_intents_refuse_a_labeled_set_without_mutation() {
        let (_dir, path) = scratch_path("bare-vs-labeled-set");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage
            .insert_account("prov-x", "personal", api_key("k1"))
            .await
            .unwrap();
        storage
            .insert_account("prov-x", "work", api_key("k2"))
            .await
            .unwrap();

        assert!(matches!(
            storage.insert_bare("prov-x", api_key("insert")).await,
            Err(AuthError::ProviderAlreadyConfigured { .. })
        ));
        assert!(matches!(
            storage.replace_bare("prov-x", api_key("replace")).await,
            Err(AuthError::ProviderCredentialChanged { .. })
        ));

        let set = storage
            .accounts("prov-x")
            .await
            .unwrap()
            .expect("still a set");
        assert_eq!(set.default, "personal");
        assert_eq!(
            storage
                .get_api_key("prov-x", None)
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("k1"),
        );
        assert_eq!(
            storage
                .get_api_key("prov-x", Some("work"))
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("k2"),
            "the sibling account remains untouched",
        );
    }

    /// Labels the store refuses: empty anywhere, and the adopted name
    /// while a bare entry still holds it (that write would overwrite
    /// the credential the conversion exists to keep).
    #[tokio::test]
    async fn invalid_labels_are_refused_and_leave_the_store_intact() {
        let (_dir, path) = scratch_path("bad-labels");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        assert!(matches!(
            storage.insert_account("prov-x", "", api_key("k")).await,
            Err(AuthError::InvalidLabel(_)),
        ));

        storage
            .insert_bare("prov-x", api_key("keep-me"))
            .await
            .unwrap();
        let before = std::fs::read(&path).expect("read bare credential bytes");
        assert!(matches!(
            storage
                .insert_account("prov-x", DEFAULT_ACCOUNT_LABEL, api_key("clobber"))
                .await,
            Err(AuthError::DuplicateAccount { .. }),
        ));
        assert_eq!(
            std::fs::read(&path).expect("read preserved credential bytes"),
            before,
            "prospective default collision performs no write"
        );
        assert!(
            storage.accounts("prov-x").await.unwrap().is_none(),
            "the refused promotion remains bare"
        );
        assert_eq!(
            storage
                .get_api_key("prov-x", None)
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("keep-me"),
            "the refused write left the bare credential in place",
        );
    }

    /// Default-account lifecycle: removing the default while others
    /// remain is refused, re-pointing then removing works, removing
    /// the last account drops the provider's entry.
    #[tokio::test]
    async fn removing_accounts_respects_the_default() {
        let (_dir, path) = scratch_path("remove-accounts");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage
            .insert_account("prov-x", "personal", api_key("k1"))
            .await
            .unwrap();
        storage
            .insert_account("prov-x", "work", api_key("k2"))
            .await
            .unwrap();

        assert!(matches!(
            storage.remove_account("prov-x", "personal").await,
            Err(AuthError::RemovingDefault { .. }),
        ));
        assert!(matches!(
            storage.remove_account("prov-x", "ghost").await,
            Err(AuthError::UnknownAccount { .. }),
        ));
        assert!(matches!(
            storage.set_default_account("prov-x", "ghost").await,
            Err(AuthError::UnknownAccount { .. }),
        ));

        storage.set_default_account("prov-x", "work").await.unwrap();
        storage.remove_account("prov-x", "personal").await.unwrap();
        let set = storage.accounts("prov-x").await.unwrap().expect("a set");
        assert_eq!(set.default, "work");
        assert_eq!(set.accounts.len(), 1, "a one-account set stays a set");

        storage.remove_account("prov-x", "work").await.unwrap();
        assert!(!storage.has("prov-x").await.unwrap());
        assert!(storage.accounts("prov-x").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn default_replacement_and_removal_are_one_locked_transition() {
        let (_dir, path) = scratch_path("replace-default-and-remove");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage
            .insert_account("prov-x", "personal", api_key("k1"))
            .await
            .unwrap();
        storage
            .insert_account("prov-x", "work", api_key("k2"))
            .await
            .unwrap();

        storage
            .remove_default_account("prov-x", "personal", "work")
            .await
            .unwrap();
        let set = storage.accounts("prov-x").await.unwrap().unwrap();
        assert_eq!(set.default, "work");
        assert_eq!(
            set.accounts
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>(),
            vec!["work"]
        );
    }

    #[tokio::test]
    async fn stale_bare_removal_cannot_delete_a_promoted_account_set() {
        let (_dir, path) = scratch_path("stale-bare-removal");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage
            .insert_bare("prov-x", api_key("bare"))
            .await
            .unwrap();
        // A sibling promotes the exact bare credential after the picker read.
        storage
            .insert_account("prov-x", "work", api_key("second"))
            .await
            .unwrap();

        assert!(matches!(
            storage.remove_bare("prov-x").await,
            Err(AuthError::ProviderCredentialChanged { .. })
        ));
        let set = storage.accounts("prov-x").await.unwrap().unwrap();
        assert_eq!(set.default, DEFAULT_ACCOUNT_LABEL);
        assert_eq!(set.accounts.len(), 2);
        assert!(matches!(
            storage
                .get_account("prov-x", DEFAULT_ACCOUNT_LABEL)
                .await
                .unwrap(),
            Some(AuthCredential::ApiKey { key }) if key == "bare"
        ));
        assert!(matches!(
            storage.get_account("prov-x", "work").await.unwrap(),
            Some(AuthCredential::ApiKey { key }) if key == "second"
        ));
    }

    #[tokio::test]
    async fn remove_all_refuses_when_the_exact_account_set_changed() {
        let (_dir, path) = scratch_path("remove-all-stale");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage
            .insert_account("prov-x", "personal", api_key("k1"))
            .await
            .unwrap();
        storage
            .insert_account("prov-x", "work", api_key("k2"))
            .await
            .unwrap();
        let expected = vec!["personal".to_string(), "work".to_string()];
        storage
            .insert_account("prov-x", "later", api_key("k3"))
            .await
            .unwrap();

        assert!(matches!(
            storage.remove_all_accounts("prov-x", &expected).await,
            Err(AuthError::ProviderAccountsChanged { .. })
        ));
        assert_eq!(
            storage
                .accounts("prov-x")
                .await
                .unwrap()
                .unwrap()
                .accounts
                .len(),
            3
        );
    }

    /// A hand-edited default that names no account resolves like a
    /// missing account (no key, no env fallback, no parse failure), so
    /// one damaged entry cannot take down the file or borrow a sibling
    /// credential.
    #[tokio::test]
    async fn a_dangling_default_resolves_to_nothing() {
        let (_dir, path) = scratch_path("dangling-default");
        std::fs::write(
            &path,
            r#"{
  "prov-x": {
    "type": "accounts",
    "default": "gone",
    "accounts": { "work": { "type": "api_key", "key": "k2" } }
  }
}"#,
        )
        .unwrap();
        let storage = AuthStorage::with_providers(path, HashMap::new());

        let removed = storage.get_api_key("prov-x", None).await.unwrap();
        assert!(
            removed.is_none(),
            "the removed default resolves nothing, got {removed:?}"
        );
        assert_eq!(
            storage
                .get_api_key("prov-x", Some("work"))
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("k2"),
            "the intact account still resolves by name",
        );
    }

    #[tokio::test]
    async fn bare_intent_does_not_target_dangling_default_and_last_real_account_cleans_up() {
        let (_dir, path) = scratch_path("dangling-legacy-default");
        std::fs::write(
            &path,
            r#"{
  "prov-x": {
    "type": "accounts",
    "default": "bad\nlabel",
    "accounts": { "work": { "type": "api_key", "key": "keep" } }
  }
}"#,
        )
        .unwrap();
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());
        let before = std::fs::read(&path).unwrap();

        assert!(matches!(
            storage
                .insert_bare("prov-x", api_key("must-not-recreate"))
                .await,
            Err(AuthError::ProviderAlreadyConfigured { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            storage
                .get_account("prov-x", "bad\nlabel")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            storage
                .get_account("prov-x", "work")
                .await
                .unwrap()
                .is_some()
        );

        storage.remove_account("prov-x", "work").await.unwrap();
        assert!(
            storage
                .stored_credentials("prov-x")
                .await
                .unwrap()
                .is_none(),
            "removing the last real account does not leave an empty ghost set"
        );
    }
}
