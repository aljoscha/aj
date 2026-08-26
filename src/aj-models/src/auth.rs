//! Auth storage & API-key resolution.
//!
//! [`AuthStorage`] is the single entry point both the CLI (`aj /login`,
//! flag plumbing) and the agent (per-request key fetch) hit when they
//! need a provider's bearer token. It owns:
//!
//! - **Persistence.** Credentials live in `~/.aj/auth.json` as a flat
//!   `{ provider_id: AuthCredential }` map. Each mutation is performed
//!   under a sidecar lockfile so two `aj` processes can't clobber each
//!   other's writes when refreshing tokens at the same time.
//! - **Runtime overrides.** A CLI `--api-key` flag bypasses the file
//!   entirely; that path lives in memory and is never written.
//! - **OAuth provider registry.** The two OAuth flows we ship
//!   ([`AnthropicOAuth`], [`OpenAIOAuth`]) are looked up by id when a
//!   refresh is needed, so the storage layer can mint new access
//!   tokens without the caller knowing about provider specifics.
//! - **Resolution chain.** [`AuthStorage::get_api_key`] walks the
//!   priority list: runtime override, then stored API key, then
//!   stored OAuth (auto-refreshing if expired), then env vars. A
//!   stored credential wins over the environment, so a deliberate
//!   login stays authoritative and a stray exported key can't shadow
//!   it. The explicit per-run override is the runtime `--api-key`.
//!
//! The on-disk shape is the same `{ "type": "...", ... }` discriminated
//! union the rest of the project uses, so `auth.json` stays easy to
//! eyeball and migrations stay simple.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::oauth::anthropic::AnthropicOAuth;
use crate::oauth::openai::OpenAIOAuth;
use crate::oauth::{OAuthCallbacks, OAuthCredentials, OAuthError, OAuthProvider, now_unix_ms};

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// In-memory shape of the entire `auth.json` file.
type AuthData = HashMap<String, StoredEntry>;

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
    #[error("provider {provider:?} has no account labeled {label:?}")]
    UnknownAccount { provider: String, label: String },
    /// Removing the default account of a set that still has others. The
    /// caller picks a new default first: silently re-pointing the
    /// default would move what unlabeled resolution bills against.
    #[error("account {label:?} is provider {provider:?}'s default; set another default first")]
    RemovingDefault { provider: String, label: String },
    /// An account label the store refuses (empty, or one that would
    /// collide with the name a bare entry adopts on conversion).
    #[error("invalid account label: {0}")]
    InvalidLabel(String),
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
#[derive(Clone)]
pub struct AuthStorage {
    /// Path to `auth.json` (typically `~/.aj/auth.json`).
    path: PathBuf,
    /// Shared mutable state. `Arc`'d so clones see the same overrides.
    state: Arc<Mutex<State>>,
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
        }
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
        let _lock = FileLock::acquire(&self.path).await?;
        let data = read_auth_file(&self.path)?;
        Ok(data.get(provider_id).and_then(|entry| entry.resolve(None)))
    }

    /// Read one account of `provider_id`'s labeled set. A bare entry
    /// holds no labels, so any `label` against it reads `None`.
    pub async fn get_account(
        &self,
        provider_id: &str,
        label: &str,
    ) -> Result<Option<AuthCredential>, AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let data = read_auth_file(&self.path)?;
        Ok(data
            .get(provider_id)
            .and_then(|entry| entry.resolve(Some(label))))
    }

    /// The labeled set stored for `provider_id`, or `None` when the
    /// provider holds a bare credential or nothing.
    pub async fn accounts(&self, provider_id: &str) -> Result<Option<ProviderAccounts>, AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let data = read_auth_file(&self.path)?;
        Ok(match data.get(provider_id) {
            Some(StoredEntry::Accounts { default, accounts }) => {
                let mut accounts: Vec<(String, AuthCredential)> = accounts
                    .iter()
                    .map(|(label, cred)| (label.clone(), cred.clone()))
                    .collect();
                accounts.sort_by(|a, b| a.0.cmp(&b.0));
                Some(ProviderAccounts {
                    default: default.clone(),
                    accounts,
                })
            }
            _ => None,
        })
    }

    /// Persist a credential for `provider_id`. Atomic w.r.t. concurrent
    /// reads/writes via the file lock.
    ///
    /// Against a bare or absent entry this replaces the whole slot,
    /// exactly as it always did. Against a labeled set it replaces the
    /// DEFAULT account's credential and preserves the rest: this method's
    /// callers (a login, a key paste) predate accounts and must not be
    /// able to flatten a set they do not know exists.
    pub async fn set(
        &self,
        provider_id: &str,
        credential: AuthCredential,
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = read_auth_file(&self.path)?;
        let entry = match data.remove(provider_id) {
            Some(StoredEntry::Accounts {
                default,
                mut accounts,
            }) => {
                accounts.insert(default.clone(), credential);
                StoredEntry::Accounts { default, accounts }
            }
            _ => StoredEntry::from_credential(credential),
        };
        data.insert(provider_id.to_string(), entry);
        write_auth_file(&self.path, &data)
    }

    /// Persist a credential under `label` in `provider_id`'s labeled set,
    /// creating or growing the set as needed.
    ///
    /// A bare entry converts on first growth: the existing credential is
    /// kept under [`DEFAULT_ACCOUNT_LABEL`] and STAYS the store default,
    /// so adding an account never silently moves what unlabeled
    /// resolution bills against. `label` may not be empty, and may not be
    /// the adopted name while a bare entry still holds it (that write
    /// would overwrite the credential the conversion exists to keep; use
    /// [`AuthStorage::set`] to replace it deliberately).
    pub async fn set_account(
        &self,
        provider_id: &str,
        label: &str,
        credential: AuthCredential,
    ) -> Result<(), AuthError> {
        if label.is_empty() {
            return Err(AuthError::InvalidLabel("empty label".to_string()));
        }
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = read_auth_file(&self.path)?;
        let entry = match data.remove(provider_id) {
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
                if label == DEFAULT_ACCOUNT_LABEL {
                    // Put the bare entry back before refusing, `remove`
                    // above already took it out of the map.
                    data.insert(provider_id.to_string(), bare);
                    write_auth_file(&self.path, &data)?;
                    return Err(AuthError::InvalidLabel(format!(
                        "{label:?} is the name the existing credential adopts; \
                         pick another, or replace it with set()"
                    )));
                }
                let existing = bare.resolve(None).expect("a bare entry resolves unlabeled");
                StoredEntry::Accounts {
                    default: DEFAULT_ACCOUNT_LABEL.to_string(),
                    accounts: HashMap::from([
                        (DEFAULT_ACCOUNT_LABEL.to_string(), existing),
                        (label.to_string(), credential),
                    ]),
                }
            }
        };
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
        let mut data = read_auth_file(&self.path)?;
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
        let mut data = read_auth_file(&self.path)?;
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
        }
        write_auth_file(&self.path, &data)
    }

    /// Remove the credential stored for `provider_id`. No-op if none
    /// exists. Removes a labeled set whole, every account of it: this is
    /// the logout-everything path. Atomic w.r.t. concurrent operations
    /// via the file lock.
    pub async fn remove(&self, provider_id: &str) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = read_auth_file(&self.path)?;
        if data.remove(provider_id).is_some() {
            write_auth_file(&self.path, &data)?;
        }
        Ok(())
    }

    /// List all provider ids currently in `auth.json`.
    pub async fn list(&self) -> Result<Vec<String>, AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let data = read_auth_file(&self.path)?;
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
            read_auth_file(&self.path)?.remove(provider_id)
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

    /// Run an OAuth login flow and persist the resulting credentials.
    /// On success, `auth.json` gains a fresh OAuth entry under
    /// `provider_id`. Errors propagate from the underlying flow.
    pub async fn login(
        &self,
        provider_id: &str,
        callbacks: &dyn OAuthCallbacks,
    ) -> Result<(), AuthError> {
        self.login_account(provider_id, None, callbacks).await
    }

    /// Run an OAuth login flow and persist the result under `label` in
    /// `provider_id`'s labeled set (`None` keeps [`AuthStorage::login`]'s
    /// unlabeled write, which replaces the bare entry or a set's default
    /// slot). The flow runs before any write, so a failed or cancelled
    /// login leaves the store untouched.
    pub async fn login_account(
        &self,
        provider_id: &str,
        label: Option<&str>,
        callbacks: &dyn OAuthCallbacks,
    ) -> Result<(), AuthError> {
        let provider = self.lookup_oauth_provider(provider_id).await?;
        let creds = provider.login(callbacks).await?;
        match label {
            Some(label) => {
                self.set_account(provider_id, label, AuthCredential::OAuth(creds))
                    .await
            }
            None => self.set(provider_id, AuthCredential::OAuth(creds)).await,
        }
    }

    /// Remove any stored credential for `provider_id`, regardless of
    /// type (API key or OAuth). Convenience wrapper over
    /// [`AuthStorage::remove`] used by the CLI's `/logout` path.
    pub async fn logout(&self, provider_id: &str) -> Result<(), AuthError> {
        self.remove(provider_id).await
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

        let mut data = read_auth_file(&self.path)?;
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

/// Write `data` to `auth.json`, creating the parent directory if
/// missing. On Unix the file is created with mode 0600 and the parent
/// with 0700 so a stray `world-readable` doesn't leak credentials.
fn write_auth_file(path: &Path, data: &AuthData) -> Result<(), AuthError> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o700);
                let _ = std::fs::set_permissions(parent, perms);
            }
        }
    }

    let content = serde_json::to_string_pretty(data).map_err(AuthError::Serialize)?;
    std::fs::write(path, content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }

    Ok(())
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

        // Make sure the parent exists so `create_dir(lock_path)` has
        // somewhere to land. Ignored on success/already-exists.
        if let Some(parent) = lock_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let start = std::time::Instant::now();
        let mut backoff = INITIAL_BACKOFF;
        loop {
            match std::fs::create_dir(&lock_path) {
                Ok(()) => return Ok(Self { path: lock_path }),
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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

    /// Set / get / remove against an empty file — the storage should
    /// create the file lazily and return what we just wrote.
    #[tokio::test]
    async fn set_get_remove_persists_to_file() {
        let (_dir, path) = scratch_path("crud");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        assert_eq!(storage.list().await.unwrap(), Vec::<String>::new());
        assert!(!storage.has("anthropic").await.unwrap());

        storage
            .set(
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

        storage.remove("anthropic").await.unwrap();
        assert!(!storage.has("anthropic").await.unwrap());
    }

    /// Runtime override beats env vars and stored credentials.
    #[tokio::test]
    async fn get_api_key_runtime_override_wins() {
        let (_dir, path) = scratch_path("override");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        storage
            .set(
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
            .set(
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
            .set(
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
            .set(
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
            .set(
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
            .set(
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
            .set(
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
            .set("openai", AuthCredential::ApiKey { key: "sk-1".into() })
            .await
            .unwrap();
        storage
            .set(
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

        // A write to *any* provider causes the read-modify-write
        // cycle inside `set` to round-trip the migrated map.
        storage
            .set("anthropic", AuthCredential::ApiKey { key: "sk-x".into() })
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

    /// Concurrent writers serialize via the file lock — ten parallel
    /// `set` calls should all land without losing entries.
    #[tokio::test]
    async fn concurrent_writes_serialize_via_lock() {
        let (_dir, path) = scratch_path("concurrent");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        let mut handles = Vec::new();
        for i in 0..10u8 {
            let s = storage.clone();
            handles.push(tokio::spawn(async move {
                s.set(
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
        storage.set("prov-x", api_key("first")).await.unwrap();
        storage
            .set_account("prov-x", "work", api_key("second"))
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
            .set_account("prov-x", "personal", api_key("k1"))
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
        storage.set("prov-x", api_key("k1")).await.unwrap();
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
            .set_account("prov-x", "personal", api_key("k1"))
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
            .set_account("prov-x", "personal", api_key("k1"))
            .await
            .unwrap();
        storage
            .set_account("prov-x", "work", api_key("k2"))
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
        storage.set("prov-x", api_key("k1")).await.unwrap();

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
            .set_account("prov-x", "work", api_key("k2"))
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
            .set_account("prov-x", "work", api_key("stored"))
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
            .set_account(
                "stub",
                "personal",
                AuthCredential::OAuth(OAuthCredentials::new("p-r", "p-a", i64::MAX)),
            )
            .await
            .unwrap();
        storage
            .set_account(
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

    /// An unlabeled write against a labeled set replaces the default
    /// slot and preserves the rest: pre-accounts callers cannot flatten
    /// a set they do not know exists.
    #[tokio::test]
    async fn an_unlabeled_set_replaces_only_the_default_slot() {
        let (_dir, path) = scratch_path("unlabeled-set");
        let storage = AuthStorage::with_providers(path, HashMap::new());
        storage
            .set_account("prov-x", "personal", api_key("k1"))
            .await
            .unwrap();
        storage
            .set_account("prov-x", "work", api_key("k2"))
            .await
            .unwrap();

        storage.set("prov-x", api_key("k1-replaced")).await.unwrap();

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
            Some("k1-replaced"),
        );
        assert_eq!(
            storage
                .get_api_key("prov-x", Some("work"))
                .await
                .unwrap()
                .map(|resolved| resolved.key)
                .as_deref(),
            Some("k2"),
            "the sibling account survived the unlabeled write",
        );
    }

    /// Labels the store refuses: empty anywhere, and the adopted name
    /// while a bare entry still holds it (that write would overwrite
    /// the credential the conversion exists to keep).
    #[tokio::test]
    async fn invalid_labels_are_refused_and_leave_the_store_intact() {
        let (_dir, path) = scratch_path("bad-labels");
        let storage = AuthStorage::with_providers(path, HashMap::new());

        assert!(matches!(
            storage.set_account("prov-x", "", api_key("k")).await,
            Err(AuthError::InvalidLabel(_)),
        ));

        storage.set("prov-x", api_key("keep-me")).await.unwrap();
        assert!(matches!(
            storage
                .set_account("prov-x", DEFAULT_ACCOUNT_LABEL, api_key("clobber"))
                .await,
            Err(AuthError::InvalidLabel(_)),
        ));
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
            .set_account("prov-x", "personal", api_key("k1"))
            .await
            .unwrap();
        storage
            .set_account("prov-x", "work", api_key("k2"))
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
}
