//! Auth storage & API-key resolution per `docs/models-spec.md` §9.1 / §9.5.
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
//!   spec §9.1 priority list — runtime override → env vars → stored
//!   API key → stored OAuth (auto-refreshing if expired).
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
use crate::oauth::{OAuthCallbacks, OAuthCredentials, OAuthError, OAuthProvider};

// ---------------------------------------------------------------------------
// On-disk shape
// ---------------------------------------------------------------------------

/// A single credential entry in `auth.json`.
///
/// Internally-tagged so the JSON object carries a `"type"` field
/// alongside the variant's payload. For the OAuth variant the inner
/// [`OAuthCredentials`] fields are flattened into the same object,
/// matching the §9.1 disk layout
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

/// In-memory shape of the entire `auth.json` file.
type AuthData = HashMap<String, AuthCredential>;

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

    /// Build a storage at the spec's default location, `~/.aj/auth.json`.
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

    /// Read the credential currently stored for `provider_id`, if any.
    ///
    /// Performs a fresh disk read every call so multiple processes can
    /// share the file without a stale-cache problem. Acquires the
    /// file lock so a concurrent write doesn't yield a torn read.
    pub async fn get(&self, provider_id: &str) -> Result<Option<AuthCredential>, AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let data = read_auth_file(&self.path)?;
        Ok(data.get(provider_id).cloned())
    }

    /// Persist a credential for `provider_id`, replacing any existing
    /// entry. Atomic w.r.t. concurrent reads/writes via the file lock.
    pub async fn set(
        &self,
        provider_id: &str,
        credential: AuthCredential,
    ) -> Result<(), AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;
        let mut data = read_auth_file(&self.path)?;
        data.insert(provider_id.to_string(), credential);
        write_auth_file(&self.path, &data)
    }

    /// Remove the credential stored for `provider_id`. No-op if none
    /// exists. Atomic w.r.t. concurrent operations via the file lock.
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
    /// spec §9.1 priority chain:
    ///
    /// 1. Runtime override (CLI `--api-key` flag).
    /// 2. Environment variables (§9.5).
    /// 3. Stored API key in `auth.json`.
    /// 4. Stored OAuth tokens — auto-refreshed under the file lock if
    ///    expired.
    ///
    /// Returns `Ok(None)` when no source has a key. OAuth refresh
    /// failures bubble out as [`AuthError::OAuth`]; callers typically
    /// surface a "log in again" prompt.
    pub async fn get_api_key(&self, provider_id: &str) -> Result<Option<String>, AuthError> {
        // 1. Runtime override.
        if let Some(key) = self
            .state
            .lock()
            .await
            .runtime_overrides
            .get(provider_id)
            .cloned()
        {
            return Ok(Some(key));
        }

        // 2. Environment variables (§9.5). Per spec, env wins over
        //    `auth.json` so a developer can dev-override stored
        //    credentials for a one-off run without editing the file.
        if let Some(key) = get_env_api_key(provider_id) {
            return Ok(Some(key));
        }

        // 3 & 4. Stored credential.
        let cred = self.get(provider_id).await?;
        match cred {
            Some(AuthCredential::ApiKey { key }) => Ok(Some(key)),
            Some(AuthCredential::OAuth(creds)) => {
                let provider = self.lookup_oauth_provider(provider_id).await?;
                let now = current_unix_ms();
                if !creds.is_expired_at(now) {
                    return Ok(Some(provider.get_api_key(&creds)));
                }
                self.refresh_oauth_with_lock(provider_id, &*provider).await
            }
            None => Ok(None),
        }
    }

    /// Run an OAuth login flow and persist the resulting credentials.
    /// On success, `auth.json` gains a fresh OAuth entry under
    /// `provider_id`. Errors propagate from the underlying flow.
    pub async fn login(
        &self,
        provider_id: &str,
        callbacks: &dyn OAuthCallbacks,
    ) -> Result<(), AuthError> {
        let provider = self.lookup_oauth_provider(provider_id).await?;
        let creds = provider.login(callbacks).await?;
        self.set(provider_id, AuthCredential::OAuth(creds)).await
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
    /// upstream call.
    async fn refresh_oauth_with_lock(
        &self,
        provider_id: &str,
        provider: &dyn OAuthProvider,
    ) -> Result<Option<String>, AuthError> {
        let _lock = FileLock::acquire(&self.path).await?;

        let mut data = read_auth_file(&self.path)?;
        let creds = match data.get(provider_id) {
            Some(AuthCredential::OAuth(c)) => c.clone(),
            // Either the entry vanished or it's now an api_key;
            // either way, nothing to refresh.
            _ => return Ok(None),
        };

        // Sibling process may have refreshed while we were waiting
        // for the lock — use the freshened token without burning the
        // refresh-token round-trip again.
        let now = current_unix_ms();
        if !creds.is_expired_at(now) {
            return Ok(Some(provider.get_api_key(&creds)));
        }

        let refreshed = provider.refresh_token(&creds).await?;
        let api_key = provider.get_api_key(&refreshed);
        data.insert(provider_id.to_string(), AuthCredential::OAuth(refreshed));
        write_auth_file(&self.path, &data)?;
        Ok(Some(api_key))
    }
}

// ---------------------------------------------------------------------------
// Default registry / paths
// ---------------------------------------------------------------------------

/// OAuth providers shipped out of the box, matching the auth section
/// of the spec (Anthropic Claude Pro/Max in §9.3 → provider id
/// `"anthropic"`, OpenAI ChatGPT/Codex in §9.4 → provider id
/// `"openai-codex"`). Per §7.4.1 the Codex flow uses a distinct
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
// Environment-variable mapping (§9.5)
// ---------------------------------------------------------------------------

/// Environment variables that can supply an API key for `provider_id`,
/// in order of preference.
///
/// Per §9.5 we cover three providers today: `"anthropic"`
/// (`ANTHROPIC_OAUTH_TOKEN` then `ANTHROPIC_API_KEY`), `"openai"`
/// (`OPENAI_API_KEY`), and `"openai-codex"`
/// (`OPENAI_CODEX_OAUTH_TOKEN`). The codex var carries a short-lived
/// JWT minted by the §9.4 OAuth flow; on its own it cannot be
/// refreshed, so persistent use should rely on a stored OAuth
/// credential rather than this env var. Unknown providers return an
/// empty slice so callers can treat absence as "no env mapping
/// configured".
pub fn find_env_keys(provider_id: &str) -> &'static [&'static str] {
    match provider_id {
        "anthropic" => &["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "openai-codex" => &["OPENAI_CODEX_OAUTH_TOKEN"],
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
/// Applies the §7.4.1 legacy-id migration in-memory before returning:
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
/// destination id is already populated or if the source isn't an
/// OAuth entry.
fn migrate_legacy_openai_oauth(data: &mut AuthData) {
    const LEGACY_ID: &str = "openai";
    const NEW_ID: &str = "openai-codex";

    // Only OAuth credentials migrate. The legacy `openai` slot also
    // legitimately stored hand-written `api_key` entries (plain
    // `OPENAI_API_KEY` paste-ins), and those stay where they are.
    if !matches!(data.get(LEGACY_ID), Some(AuthCredential::OAuth(_))) {
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
// Time
// ---------------------------------------------------------------------------

/// Current Unix time in milliseconds. Pulled out so tests can stub it
/// in via the public [`OAuthCredentials::is_expired_at`] entry point
/// rather than hooking the whole module.
fn current_unix_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Build a tempdir-style scratch path so each test gets its own
    /// `auth.json` and `.lock` to play with. Atomic counter avoids
    /// PID collisions across cargo's parallel test runs.
    fn scratch_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("aj-auth-test-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("auth.json")
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
        let path = scratch_path("crud");
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

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// Runtime override beats env vars and stored credentials.
    #[tokio::test]
    async fn get_api_key_runtime_override_wins() {
        let path = scratch_path("override");
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

        let key = storage.get_api_key("openai").await.unwrap();
        assert_eq!(key.as_deref(), Some("from-runtime"));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// Stored API key is returned when no runtime override or env var
    /// is set. Uses an unknown provider id so env-var resolution
    /// can't accidentally satisfy the request.
    #[tokio::test]
    async fn get_api_key_falls_back_to_stored_key() {
        let path = scratch_path("stored");
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

        let key = storage.get_api_key("custom-provider-xyz").await.unwrap();
        assert_eq!(key.as_deref(), Some("from-file"));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
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

        let path = scratch_path("refresh");
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

        let key = storage.get_api_key("stub").await.unwrap();
        assert_eq!(key.as_deref(), Some("refreshed-a"));

        // Confirm the refreshed creds were persisted.
        match storage.get("stub").await.unwrap() {
            Some(AuthCredential::OAuth(c)) => {
                assert_eq!(c.access, "refreshed-a");
                assert_eq!(c.refresh, "refreshed-r");
                assert_eq!(c.expires, i64::MAX);
            }
            other => panic!("unexpected credential: {other:?}"),
        }

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
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

        let path = scratch_path("cached");
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

        let key = storage.get_api_key("stub").await.unwrap();
        assert_eq!(key.as_deref(), Some("fresh-a"));

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// `get_api_key` returns `None` when nothing is configured at any
    /// layer — runtime, env, or file.
    #[tokio::test]
    async fn get_api_key_returns_none_when_unconfigured() {
        let path = scratch_path("none");
        let storage = AuthStorage::with_providers(path.clone(), HashMap::new());

        let key = storage
            .get_api_key("nonexistent-provider-zzz")
            .await
            .unwrap();
        assert!(key.is_none());

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// `find_env_keys` mirrors the §9.5 table: anthropic prefers the
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

    /// The OAuth-only `openai-codex` pool resolves to its own env var
    /// (per spec §9.5). It deliberately does *not* fall back to
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
        let path = scratch_path("format");
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

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// Default registry includes Anthropic + OpenAI Codex so out-of-the-box
    /// CLI usage can refresh both. Constructed via `AuthStorage::new`
    /// to exercise the same path embedders use. The OpenAI entry lives
    /// under `openai-codex` per spec §7.4.1 — the regular `openai`
    /// provider id is reserved for plain `OPENAI_API_KEY` credentials,
    /// which don't need a refresh flow.
    #[tokio::test]
    async fn default_registry_has_anthropic_and_openai_codex() {
        let path = scratch_path("registry");
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

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A legacy `openai` OAuth entry in `auth.json` is invisibly
    /// migrated to `openai-codex` on read, so a user who logged in
    /// before the §7.4.1 rename keeps their stored refresh token.
    /// `get` against the old id returns `None`; `get` against the
    /// new id returns the migrated credential.
    #[tokio::test]
    async fn read_migrates_legacy_openai_oauth_to_openai_codex() {
        let path = scratch_path("migrate");
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

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// An `api_key` entry under `openai` is *not* migrated — that's a
    /// real `OPENAI_API_KEY` for the public API, distinct from the
    /// Codex OAuth pool. Migrating it would silently break the
    /// regular OpenAI provider's auth lookup.
    #[tokio::test]
    async fn read_preserves_openai_api_key_entries() {
        let path = scratch_path("preserve");
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

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// If the user already has an `openai-codex` entry — e.g. they
    /// hand-edited the file or already migrated in a prior run — the
    /// legacy `openai` slot is left untouched. We never clobber an
    /// existing destination.
    #[tokio::test]
    async fn read_skips_migration_when_target_already_present() {
        let path = scratch_path("collision");
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

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// After the in-memory migration, the next mutating write
    /// (`set` / `remove`) persists the migrated shape to disk, so the
    /// legacy `openai` OAuth key disappears from `auth.json` once any
    /// real auth operation runs.
    #[tokio::test]
    async fn migration_persists_to_disk_on_next_write() {
        let path = scratch_path("persist");
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

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// Concurrent writers serialize via the file lock — ten parallel
    /// `set` calls should all land without losing entries.
    #[tokio::test]
    async fn concurrent_writes_serialize_via_lock() {
        let path = scratch_path("concurrent");
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

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// `try_steal_stale_lock` should leave fresh locks alone but
    /// remove ones whose mtime is older than the supplied threshold.
    /// Drives the helper directly with a near-zero `max_age` so the
    /// test doesn't have to wait for the production
    /// [`STALE_LOCK_AGE`] to elapse.
    #[tokio::test]
    async fn stale_lock_is_stealable() {
        let path = scratch_path("stale");
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

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
