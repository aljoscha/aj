//! OAuth infrastructure shared by all OAuth-capable providers.
//!
//! Defines the core trait surface (`docs/models-spec.md` §9.2) without
//! prescribing how any specific provider implements its flow:
//!
//! - [`OAuthCredentials`] — what gets persisted on disk after a
//!   successful login. Common token fields (`refresh`, `access`,
//!   `expires`) plus a free-form `extra` map for provider-specific
//!   data such as OpenAI's `accountId`.
//! - [`OAuthCallbacks`] — the host-supplied UI hooks the login flow
//!   needs to drive a user through the authorization dance (open a
//!   URL, prompt for a pasted code, report progress).
//! - [`OAuthProvider`] — the trait each provider's flow implements.
//!   Object-safe (it lives behind `&dyn` everywhere) so we can build
//!   a registry of providers and look them up by id.
//!
//! Concrete provider flows (Anthropic in §9.3, OpenAI in §9.4) live in
//! their own submodules. The [`pkce`] submodule provides the
//! verifier/challenge utilities both flows share, and [`page`] renders
//! the success/error HTML each provider's local callback server
//! returns to the browser.

pub mod anthropic;
pub mod openai;
pub mod page;
pub mod pkce;

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Persisted OAuth tokens for a single provider.
///
/// This type is the wire shape both for `~/.aj/auth.json` (see §9.1)
/// and for return values from [`OAuthProvider::login`] /
/// [`OAuthProvider::refresh_token`]. Keep it round-trip-safe: any
/// extra provider-specific fields go in [`OAuthCredentials::extra`]
/// rather than as named fields, so adding a new provider doesn't
/// require a schema migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredentials {
    /// Long-lived refresh token used to mint fresh access tokens
    /// without re-prompting the user.
    pub refresh: String,
    /// Short-lived bearer token sent on each request.
    pub access: String,
    /// Unix milliseconds at which `access` expires. Callers should
    /// refresh slightly before this to avoid clock-skew failures.
    pub expires: i64,
    /// Provider-specific fields (e.g. OpenAI's `account_id`). Stored
    /// flattened into the parent JSON object so the on-disk shape
    /// matches the §9.1 design (`{ refresh, access, expires, ...extra }`).
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

impl OAuthCredentials {
    /// Build credentials with no extra fields. Convenience for providers
    /// that only use the three core fields (currently Anthropic).
    pub fn new(refresh: impl Into<String>, access: impl Into<String>, expires: i64) -> Self {
        Self {
            refresh: refresh.into(),
            access: access.into(),
            expires,
            extra: HashMap::new(),
        }
    }

    /// Returns `true` when the credentials are at or past their
    /// expiration timestamp. Use this to decide whether a refresh is
    /// needed before the next provider call.
    ///
    /// Compared against the supplied `now_unix_ms` rather than reading
    /// the wall clock so callers can apply their own safety margin
    /// (e.g. treat tokens that expire within the next 60 s as expired).
    pub fn is_expired_at(&self, now_unix_ms: i64) -> bool {
        now_unix_ms >= self.expires
    }
}

/// Information the user needs in order to authorize the application.
///
/// Passed to [`OAuthCallbacks::on_auth`] so the host can display the
/// URL and any provider-specific instructions (e.g. "if your browser
/// is on a different machine, paste the redirect URL back here").
#[derive(Debug, Clone)]
pub struct OAuthAuthInfo<'a> {
    /// Authorization URL the user must open.
    pub url: &'a str,
    /// Optional human-readable instructions to display alongside the URL.
    pub instructions: Option<&'a str>,
}

/// Host-supplied UI hooks invoked over the course of an OAuth login flow.
///
/// Every callback is `&self`-borrowed so a single callbacks object can
/// be shared across the entire flow without interior mutability
/// constraints leaking into the trait. Callbacks must be `Send + Sync`
/// because they are invoked from async tasks driving HTTP I/O.
///
/// The trait is object-safe — concrete provider flows take
/// `&dyn OAuthCallbacks` so the host (CLI, TUI, RPC server) is free
/// to plug in whatever UI it has.
#[async_trait]
pub trait OAuthCallbacks: Send + Sync {
    /// Called once with the authorization URL the user should open in
    /// their browser. The host is expected to display the URL (and
    /// optionally print/open it) but must not block — the flow
    /// continues in parallel waiting for the redirect.
    fn on_auth(&self, info: OAuthAuthInfo<'_>);

    /// Prompt the user for a single line of text input.
    ///
    /// Used as the manual-paste fallback when the browser callback
    /// can't reach this machine (SSH, headless, restricted networks).
    /// The returned string is whatever the user typed; the flow is
    /// responsible for parsing it (usually the full redirect URL or
    /// a bare authorization code).
    async fn on_prompt(&self, message: &str) -> Result<String, OAuthError>;

    /// Optional progress message. Default impl is a no-op so simple
    /// hosts can ignore it; richer hosts can render a status line.
    fn on_progress(&self, _message: &str) {}

    /// Optional manual code-input race. When provided, the provider
    /// flow will await this future *concurrently* with the browser
    /// callback server, so a user on a different machine can paste
    /// the code themselves without first hitting the local redirect.
    ///
    /// Default impl returns
    /// [`OAuthError::ManualCodeInputUnsupported`]. Provider flows
    /// must check support via [`OAuthCallbacks::supports_manual_code_input`]
    /// before awaiting; the default-error path is a safety net for
    /// implementations that ignore the capability flag.
    async fn on_manual_code_input(&self) -> Result<String, OAuthError> {
        Err(OAuthError::ManualCodeInputUnsupported)
    }

    /// Indicates whether [`OAuthCallbacks::on_manual_code_input`] is
    /// implemented. Provider flows consult this before racing the
    /// browser callback against manual input. Defaults to `false`.
    fn supports_manual_code_input(&self) -> bool {
        false
    }
}

/// One specific provider's OAuth flow.
///
/// Implementations encapsulate everything provider-specific:
/// authorization URL construction, redirect handling, token exchange,
/// and refresh. The host code (CLI / agent) only sees the trait
/// surface, which keeps the auth-storage layer (§9.1) decoupled from
/// any specific provider's API.
#[async_trait]
pub trait OAuthProvider: Send + Sync {
    /// Stable identifier used as the auth-storage map key
    /// (e.g. `"anthropic"`, `"openai"`).
    fn id(&self) -> &str;

    /// Human-readable provider name shown to the user (e.g.
    /// `"Anthropic (Claude Pro/Max)"`).
    fn name(&self) -> &str;

    /// Indicates that this flow opens a local HTTP callback server.
    /// Hosts can use this hint to pre-emptively warn users that a
    /// localhost port will be bound, or to suppress the manual-paste
    /// prompt for non-callback flows. Defaults to `false`.
    fn uses_callback_server(&self) -> bool {
        false
    }

    /// Run the full interactive login flow.
    ///
    /// On success, returns credentials ready to be persisted via the
    /// auth storage layer. The provider flow may invoke `callbacks`
    /// any number of times.
    async fn login(&self, callbacks: &dyn OAuthCallbacks) -> Result<OAuthCredentials, OAuthError>;

    /// Mint a fresh set of credentials from an existing (possibly
    /// expired) refresh token.
    ///
    /// Returns the new credentials. The auth storage layer overwrites
    /// the prior entry on success; on failure the caller decides
    /// whether to surface a re-login prompt.
    async fn refresh_token(
        &self,
        credentials: &OAuthCredentials,
    ) -> Result<OAuthCredentials, OAuthError>;

    /// Extract the bearer-token string the provider's HTTP client
    /// should use as its API key. For most providers this is just
    /// `credentials.access.clone()`; OAuth-on-API-key providers may
    /// override.
    fn get_api_key(&self, credentials: &OAuthCredentials) -> String {
        credentials.access.clone()
    }
}

/// Errors that can occur during an OAuth flow.
///
/// Variants are deliberately coarse — flows attach human-readable
/// detail strings rather than encoding every upstream HTTP shape.
/// Callers typically log the message and surface "log in again" to
/// the user; nothing programmatic keys off the variant beyond
/// distinguishing user-cancelled from genuine failures.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// PKCE generation failed (CSPRNG unavailable).
    #[error("PKCE generation failed: {0}")]
    Pkce(#[from] pkce::PkceError),

    /// HTTP request failed (network, TLS, timeout).
    #[error("HTTP request failed: {0}")]
    Http(String),

    /// The authorization server returned a non-2xx response.
    #[error("authorization server returned status {status}: {body}")]
    Server { status: u16, body: String },

    /// A response body could not be parsed.
    #[error("invalid response body: {0}")]
    Parse(String),

    /// The authorization callback returned an error parameter.
    #[error("authorization failed: {0}")]
    Authorization(String),

    /// The `state` parameter in the callback didn't match the value
    /// supplied with the authorization request — possible CSRF.
    #[error("OAuth state mismatch")]
    StateMismatch,

    /// The flow ended without obtaining an authorization code.
    #[error("authorization code missing")]
    MissingCode,

    /// The user cancelled the login (closed prompt, hit Ctrl+C, ...).
    #[error("login cancelled")]
    Cancelled,

    /// The host's [`OAuthCallbacks`] declined to provide manual code
    /// input but the flow tried to use it anyway.
    #[error("manual code input not supported by host")]
    ManualCodeInputUnsupported,

    /// Catch-all for provider-specific failure modes that don't fit
    /// the variants above. The flow attaches detail in the string.
    #[error("OAuth flow failed: {0}")]
    Other(String),
}

/// Summarize an HTTP response body for inclusion in an error message
/// without exposing its contents.
///
/// A token endpoint's success (2xx) body *is* the credential payload
/// (`access_token`/`refresh_token`), and [`OAuthError`]'s `Display`
/// reaches logs and stdout, so a raw token-endpoint body must never be
/// interpolated into an error. Callers pair this with the serde error,
/// which names the offending field without echoing its value.
pub(crate) fn redacted_body_summary(body: &str) -> String {
    format!("{} bytes (redacted)", body.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `OAuthCredentials` must round-trip through serde with the `extra`
    /// map flattened into the JSON object — the on-disk shape per
    /// §9.1 is `{ refresh, access, expires, accountId, ... }`, not
    /// `{ refresh, access, expires, extra: { accountId } }`.
    #[test]
    fn credentials_flatten_extra_in_json() {
        let mut creds = OAuthCredentials::new("r", "a", 1234);
        creds.extra.insert(
            "account_id".into(),
            serde_json::Value::String("acc-123".into()),
        );

        let json = serde_json::to_value(&creds).expect("serialize");
        assert_eq!(json["refresh"], "r");
        assert_eq!(json["access"], "a");
        assert_eq!(json["expires"], 1234);
        assert_eq!(json["account_id"], "acc-123");
        assert!(
            json.get("extra").is_none(),
            "extra map should be flattened, not nested: {json}"
        );

        let back: OAuthCredentials = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.refresh, "r");
        assert_eq!(back.access, "a");
        assert_eq!(back.expires, 1234);
        assert_eq!(back.extra.get("account_id").unwrap(), "acc-123");
    }

    /// Empty `extra` maps must not write a key at all, keeping the
    /// disk format minimal for providers without provider-specific
    /// fields (e.g. Anthropic).
    #[test]
    fn credentials_skip_empty_extra() {
        let creds = OAuthCredentials::new("r", "a", 1234);
        let json = serde_json::to_string(&creds).expect("serialize");
        assert_eq!(json, r#"{"refresh":"r","access":"a","expires":1234}"#);
    }

    /// Expiration check: timestamps in the past or equal to `now`
    /// count as expired; future timestamps don't.
    #[test]
    fn expiration_check_is_inclusive_at_boundary() {
        let creds = OAuthCredentials::new("r", "a", 1_000_000);
        assert!(!creds.is_expired_at(999_999));
        assert!(creds.is_expired_at(1_000_000));
        assert!(creds.is_expired_at(1_000_001));
    }

    /// Default trait implementations: callbacks should report no
    /// manual-code-input support out of the box, and the helper
    /// `on_manual_code_input` should fail with the typed variant
    /// rather than returning an empty string.
    #[tokio::test]
    async fn default_callbacks_decline_manual_code_input() {
        struct Bare;

        #[async_trait]
        impl OAuthCallbacks for Bare {
            fn on_auth(&self, _info: OAuthAuthInfo<'_>) {}
            async fn on_prompt(&self, _message: &str) -> Result<String, OAuthError> {
                Ok(String::new())
            }
        }

        let cb = Bare;
        assert!(!cb.supports_manual_code_input());
        match cb.on_manual_code_input().await {
            Err(OAuthError::ManualCodeInputUnsupported) => {}
            other => panic!("expected ManualCodeInputUnsupported, got {:?}", other),
        }
    }

    /// Provider trait must be object-safe (we'll keep providers in a
    /// `HashMap<String, Box<dyn OAuthProvider>>` registry). This test
    /// fails at compile time if the trait drifts to a non-dispatchable
    /// shape.
    #[test]
    fn provider_trait_is_object_safe() {
        struct Stub;

        #[async_trait]
        impl OAuthProvider for Stub {
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
                Ok(OAuthCredentials::new("r", "a", 0))
            }
        }

        let _: Box<dyn OAuthProvider> = Box::new(Stub);
    }
}
