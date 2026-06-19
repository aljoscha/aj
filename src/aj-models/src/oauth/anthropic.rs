//! Anthropic OAuth flow (Claude Pro/Max), per `docs/models-spec.md` §9.3.
//!
//! Authorization Code + PKCE. The high-level flow:
//!
//! 1. Generate a PKCE verifier / challenge pair (S256) and an
//!    independent random `state` value. The authorization server
//!    treats `state` as opaque CSRF-protection material and echoes it
//!    back on the callback; we only check that what comes back matches
//!    what we sent.
//! 2. Bind a local HTTP callback server on `127.0.0.1` on an
//!    OS-assigned ephemeral port, and advertise
//!    `http://localhost:<port>/callback` as the redirect URI. The
//!    upstream accepts any loopback port (RFC 8252 §7.3), so we don't
//!    pin one — that avoids failing when a fixed port happens to be
//!    taken. The same redirect URI is echoed in the token exchange,
//!    where it must match byte-for-byte.
//! 3. Hand the user the authorize URL via [`OAuthCallbacks::on_auth`]
//!    and wait for either the browser callback to hit our server or
//!    — for hosts that support it — a manually pasted code. The two
//!    paths race; whichever arrives first wins. We hand `on_auth` two
//!    URLs: the automatic one (loopback redirect) and a manual one
//!    whose redirect targets a provider-hosted page that shows a
//!    `code#state` string to paste — the latter is what works when the
//!    browser is on a different machine. If neither path produces a
//!    code we fall back to a single [`OAuthCallbacks::on_prompt`] call.
//! 4. POST the authorization code to the token endpoint. The
//!    `redirect_uri` echoed there must match the one in the authorize
//!    request that issued the code, so we track which URL produced it
//!    (loopback vs. hosted) and send the matching redirect.
//! 5. Compute an absolute expiration timestamp from the returned
//!    `expires_in`, applying a small safety margin so we proactively
//!    refresh just before the upstream token actually lapses.
//!
//! The flow exposes itself both as a free-function API (good for
//! ad-hoc CLI use and tests) and behind the [`OAuthProvider`] trait
//! (for the auth-storage layer in §9.1, which keys by provider id).
//!
//! The local callback server, manual-paste parsing, and token-endpoint
//! plumbing are shared with the OpenAI flow via [`super::callback`],
//! [`super::paste`], and [`super::token`]; only the endpoint constants,
//! the JSON token-body shape, the redirect tracking, and
//! [`token_to_credentials`] are Anthropic-specific.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use tokio::net::TcpListener;

use super::callback::{CallbackConfig, await_callback};
use super::paste::parse_authorization_input;
use super::pkce::{generate_pkce, random_token};
use super::token::{
    REFRESH_SAFETY_MARGIN_MS, REQUEST_TIMEOUT_SECS, TokenResponse, now_unix_ms, send_token_request,
};
use super::{OAuthAuthInfo, OAuthCallbacks, OAuthCredentials, OAuthError, OAuthProvider};

// ---------------------------------------------------------------------------
// Endpoint constants
// ---------------------------------------------------------------------------

/// Public OAuth client id registered with Anthropic for the Claude
/// Code-style desktop flow. This is intentionally not a secret — the
/// security of the flow comes from PKCE, not from hiding the id.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Hosted authorize endpoint the user visits in their browser. This is
/// the Claude.ai subscription (Pro/Max) authorize host, matching the
/// official Claude Code client.
const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";

/// Token-exchange endpoint. Same URL handles both the
/// `authorization_code` and `refresh_token` grants.
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// Redirect URI for the manual flow. The upstream renders a page here
/// that displays a `code#state` string for the user to paste back,
/// which is how login completes when the browser can't reach our
/// loopback server (SSH/headless). Echoed verbatim in the token
/// exchange when the code came from this path.
const MANUAL_REDIRECT_URL: &str = "https://platform.claude.com/oauth/code/callback";

/// Local IP the callback HTTP server binds to. We bind the literal
/// loopback address so we don't depend on `localhost` resolving to
/// `127.0.0.1` on the user's machine; the redirect URI we advertise
/// upstream still spells the host `localhost`, which the
/// authorization server accepts for loopback redirects.
const CALLBACK_HOST: &str = "127.0.0.1";

/// Path the upstream redirects the browser to.
const CALLBACK_PATH: &str = "/callback";

/// Provider word shown on the local callback server's success/error
/// pages.
const PROVIDER_NAME: &str = "Anthropic";

/// Scope string requested at authorize time. The wide set is what
/// Claude Code asks for; trimming it would cause the upstream server
/// to either refuse or downgrade.
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

// ---------------------------------------------------------------------------
// Public provider type
// ---------------------------------------------------------------------------

/// Anthropic OAuth provider, suitable for storing in a
/// `Box<dyn OAuthProvider>` registry.
///
/// Holds a single `reqwest::Client` configured with our timeout and a
/// useful user-agent. Cloning is cheap; the underlying client uses
/// reference-counted internals.
pub struct AnthropicOAuth {
    client: Client,
    /// Token-exchange URL. Configurable so tests can point at a local
    /// mock; in production it's always `TOKEN_URL`.
    token_url: String,
}

impl Default for AnthropicOAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl AnthropicOAuth {
    /// Build a provider talking to the production token endpoint.
    pub fn new() -> Self {
        Self::with_token_url(TOKEN_URL)
    }

    /// Build a provider with a caller-supplied token URL. Used by
    /// tests to redirect to a local mock; not exposed publicly.
    fn with_token_url(token_url: impl Into<String>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .user_agent(concat!("aj/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("default reqwest client should build");
        Self {
            client,
            token_url: token_url.into(),
        }
    }
}

#[async_trait]
impl OAuthProvider for AnthropicOAuth {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn name(&self) -> &str {
        "Anthropic (Claude Pro/Max)"
    }

    fn uses_callback_server(&self) -> bool {
        true
    }

    async fn login(&self, callbacks: &dyn OAuthCallbacks) -> Result<OAuthCredentials, OAuthError> {
        login_with(&self.client, &self.token_url, callbacks).await
    }

    async fn refresh_token(
        &self,
        credentials: &OAuthCredentials,
    ) -> Result<OAuthCredentials, OAuthError> {
        refresh_with(&self.client, &self.token_url, &credentials.refresh).await
    }
}

// ---------------------------------------------------------------------------
// Login orchestration
// ---------------------------------------------------------------------------

/// Run the full Anthropic login flow against the supplied HTTP client
/// and token URL. Pulled out of the trait impl so tests can drive it
/// without constructing a full provider.
async fn login_with(
    client: &Client,
    token_url: &str,
    callbacks: &dyn OAuthCallbacks,
) -> Result<OAuthCredentials, OAuthError> {
    let pkce = generate_pkce()?;
    let state = random_token()?;

    // Bind the callback listener *before* we hand the user the
    // authorize URL: if the user is fast enough to click through and
    // the redirect arrives at our port before we're listening, we'd
    // miss the callback. Port 0 lets the OS pick a free ephemeral
    // port; the upstream accepts any loopback port so we read the
    // assigned one back and build the redirect URI from it.
    let listener = TcpListener::bind((CALLBACK_HOST, 0))
        .await
        .map_err(|e| OAuthError::Other(format!("bind {CALLBACK_HOST}: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| OAuthError::Other(format!("read local callback addr: {e}")))?
        .port();
    let loopback_redirect = redirect_uri_for(port);

    // Build both authorize URLs up front (matching the official
    // client): the automatic one redirects to our loopback server, the
    // manual one to a provider-hosted page that shows a code to paste.
    // The host decides which to surface based on whether it can reach a
    // browser; we accept a code from either path.
    let auto_url = build_authorize_url(&pkce.challenge, &state, &loopback_redirect);
    let manual_url = build_authorize_url(&pkce.challenge, &state, MANUAL_REDIRECT_URL);
    callbacks.on_auth(OAuthAuthInfo {
        url: &auto_url,
        manual_url: Some(&manual_url),
        instructions: Some(
            "Complete login in your browser. If your browser is on a different machine, paste the code it shows here.",
        ),
    });

    let (code, returned_state, redirect_uri) =
        obtain_code_and_state(callbacks, listener, &state, &loopback_redirect).await?;

    callbacks.on_progress("Exchanging authorization code for tokens...");
    exchange_authorization_code(
        client,
        token_url,
        &code,
        &returned_state,
        &pkce.verifier,
        &redirect_uri,
    )
    .await
}

/// Redirect URI advertised to the upstream and echoed in the token
/// exchange, built from the ephemeral callback port. Spelled with the
/// host `localhost` (not the bound `127.0.0.1`) because that's the
/// conventional loopback redirect form.
fn redirect_uri_for(port: u16) -> String {
    format!("http://localhost:{port}{CALLBACK_PATH}")
}

/// Refresh an existing Anthropic OAuth token. Exposed as a free
/// function for the same reason as [`login_with`].
async fn refresh_with(
    client: &Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<OAuthCredentials, OAuthError> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh_token,
    });
    post_token_request(client, token_url, body).await
}

/// Drive the race between the local callback server, the optional
/// manual-code-input path, and (as a last resort) the prompt
/// fallback. Returns the validated `(code, state, redirect_uri)`
/// triple.
///
/// The `redirect_uri` is the one bound to the returned code: the
/// `loopback_redirect` when the code arrived on our callback server,
/// or the hosted [`MANUAL_REDIRECT_URL`] when it was pasted. We need it
/// because the token exchange must echo the exact redirect from the
/// authorize request that issued the code. A pasted *loopback* URL
/// (the user copied the failed redirect from their address bar) is
/// detected and mapped back to the loopback redirect.
///
/// The state returned alongside a server-delivered code is always
/// `expected_state`: the shared callback server validates it before
/// returning, so we don't carry it back out.
///
/// The select! is `biased` to prefer the callback server: if the
/// browser redirect already produced a usable code, that's
/// authoritative — we don't want a half-typed manual entry to
/// override it.
async fn obtain_code_and_state(
    callbacks: &dyn OAuthCallbacks,
    listener: TcpListener,
    expected_state: &str,
    loopback_redirect: &str,
) -> Result<(String, String, String), OAuthError> {
    let config = CallbackConfig {
        path: CALLBACK_PATH,
        provider_name: PROVIDER_NAME,
    };
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;
    let mut redirect: Option<String> = None;

    if callbacks.supports_manual_code_input() {
        let server_fut = await_callback(listener, expected_state, &config);
        tokio::pin!(server_fut);
        tokio::select! {
            biased;
            res = &mut server_fut => {
                code = Some(res?);
                state = Some(expected_state.to_string());
                redirect = Some(loopback_redirect.to_string());
            }
            res = callbacks.on_manual_code_input() => {
                let input = res?;
                let parsed = parse_authorization_input(input.trim());
                if let Some(s) = parsed.state.as_deref()
                    && s != expected_state
                {
                    return Err(OAuthError::StateMismatch);
                }
                if let Some(c) = parsed.code {
                    code = Some(c);
                    state = Some(parsed.state.unwrap_or_else(|| expected_state.to_string()));
                    redirect = Some(redirect_for_input(input.trim(), loopback_redirect));
                }
                // Empty / unparseable manual input: drop through to
                // the prompt fallback below. Dropping `server_fut`
                // closes the listener — the prompt is the only way
                // forward from here.
            }
        }
    } else {
        code = Some(await_callback(listener, expected_state, &config).await?);
        state = Some(expected_state.to_string());
        redirect = Some(loopback_redirect.to_string());
    }

    if code.is_none() {
        let input = callbacks
            .on_prompt("Paste the code shown after login (or the full redirect URL):")
            .await?;
        let parsed = parse_authorization_input(input.trim());
        if let Some(s) = parsed.state.as_deref()
            && s != expected_state
        {
            return Err(OAuthError::StateMismatch);
        }
        if parsed.code.is_some() {
            redirect = Some(redirect_for_input(input.trim(), loopback_redirect));
        }
        code = parsed.code;
        state = parsed.state.or_else(|| Some(expected_state.to_string()));
    }

    let code = code.ok_or(OAuthError::MissingCode)?;
    let state = state.unwrap_or_else(|| expected_state.to_string());
    let redirect = redirect.unwrap_or_else(|| MANUAL_REDIRECT_URL.to_string());
    Ok((code, state, redirect))
}

/// Pick the `redirect_uri` to echo at token-exchange time for a
/// pasted input. If the user pasted the full loopback redirect URL
/// (copied from the failed browser page), the code is bound to the
/// loopback redirect; otherwise it came from the hosted manual page,
/// so it's bound to [`MANUAL_REDIRECT_URL`].
fn redirect_for_input(input: &str, loopback_redirect: &str) -> String {
    if let Ok(url) = reqwest::Url::parse(input)
        && matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
    {
        return loopback_redirect.to_string();
    }
    MANUAL_REDIRECT_URL.to_string()
}

// ---------------------------------------------------------------------------
// URL construction
// ---------------------------------------------------------------------------

/// Build the authorize URL the user opens in their browser. All
/// parameters are required by Anthropic's OAuth server; trimming any
/// of them produces a 400 from the upstream.
///
/// `state` is an opaque CSRF token (see the module docs) and
/// `redirect_uri` must be the exact value later echoed in the token
/// exchange.
fn build_authorize_url(challenge: &str, state: &str, redirect_uri: &str) -> String {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).expect("AUTHORIZE_URL is a valid URL");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.into()
}

// ---------------------------------------------------------------------------
// Token exchange
// ---------------------------------------------------------------------------

/// Exchange an authorization code for credentials. Used by
/// [`login_with`] after the callback or manual-input path produced a
/// `(code, state)` pair.
async fn exchange_authorization_code(
    client: &Client,
    token_url: &str,
    code: &str,
    state: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredentials, OAuthError> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
    });
    post_token_request(client, token_url, body).await
}

/// Anthropic's token-endpoint POST: a JSON body for both the
/// `authorization_code` and `refresh_token` grants. The send /
/// status / redaction handling is shared via [`send_token_request`].
async fn post_token_request(
    client: &Client,
    token_url: &str,
    body: serde_json::Value,
) -> Result<OAuthCredentials, OAuthError> {
    let request = client
        .post(token_url)
        .header("Accept", "application/json")
        .json(&body);
    let token = send_token_request(request, token_url).await?;
    Ok(token_to_credentials(token))
}

/// Convert an upstream token response into our persistable
/// [`OAuthCredentials`] shape. Computes the absolute expiration
/// timestamp here so callers don't have to track relative time.
fn token_to_credentials(t: TokenResponse) -> OAuthCredentials {
    OAuthCredentials {
        refresh: t.refresh_token,
        access: t.access_token,
        expires: now_unix_ms() + t.expires_in * 1000 - REFRESH_SAFETY_MARGIN_MS,
        extra: HashMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::test_support::MockTokenServer;

    /// Authorize URL must contain every parameter Anthropic's server
    /// requires, with the documented values. Drift here breaks the
    /// flow silently (the upstream returns 400 with no useful body).
    #[test]
    fn authorize_url_contains_required_params() {
        let redirect = redirect_uri_for(49152);
        let url_str = build_authorize_url("CHALLENGE", "STATE", &redirect);
        let url = reqwest::Url::parse(&url_str).expect("authorize URL must parse");
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("claude.com"));
        assert_eq!(url.path(), "/cai/oauth/authorize");

        let pairs: HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(pairs.get("code").map(String::as_str), Some("true"));
        assert_eq!(pairs.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            pairs.get("redirect_uri").map(String::as_str),
            Some(redirect.as_str())
        );
        assert_eq!(pairs.get("scope").map(String::as_str), Some(SCOPES));
        assert_eq!(
            pairs.get("code_challenge").map(String::as_str),
            Some("CHALLENGE")
        );
        assert_eq!(
            pairs.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        // `state` is an independent opaque value, passed through to
        // the authorize URL verbatim.
        assert_eq!(pairs.get("state").map(String::as_str), Some("STATE"));
    }

    /// The exchange `redirect_uri` for pasted input must match the
    /// authorize request that issued the code: a loopback URL maps
    /// back to the loopback redirect, anything else (bare code,
    /// `code#state`, or a hosted URL) to the manual hosted redirect.
    #[test]
    fn redirect_for_input_distinguishes_loopback_from_manual() {
        let loopback = redirect_uri_for(49152);

        // Pasted loopback redirect URL → loopback redirect.
        assert_eq!(
            redirect_for_input("http://localhost:49152/callback?code=A&state=B", &loopback),
            loopback
        );
        assert_eq!(
            redirect_for_input("http://127.0.0.1:49152/callback?code=A", &loopback),
            loopback
        );

        // Bare code / `code#state` from the hosted page → manual.
        assert_eq!(redirect_for_input("ABC123", &loopback), MANUAL_REDIRECT_URL);
        assert_eq!(
            redirect_for_input("ABC#DEF", &loopback),
            MANUAL_REDIRECT_URL
        );
    }

    /// Token response → `OAuthCredentials` conversion must produce
    /// an absolute expiry that lies in the future, with the safety
    /// margin baked in.
    #[test]
    fn token_to_credentials_computes_absolute_expiry() {
        let t = TokenResponse {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_in: 3600,
        };
        let before = now_unix_ms();
        let creds = token_to_credentials(t);
        let after = now_unix_ms();

        assert_eq!(creds.access, "access");
        assert_eq!(creds.refresh, "refresh");
        // The expiry must lie within the [before, after] window plus
        // the lifetime, minus the safety margin. Use an inclusive
        // bound on both ends to tolerate test scheduling jitter.
        let lifetime_ms = 3600 * 1000;
        assert!(creds.expires >= before + lifetime_ms - REFRESH_SAFETY_MARGIN_MS);
        assert!(creds.expires <= after + lifetime_ms - REFRESH_SAFETY_MARGIN_MS);
        assert!(creds.extra.is_empty());
    }

    /// End-to-end token exchange: spin up a tiny HTTP server that
    /// answers one POST with a canned JSON body, point the provider
    /// at it, and verify we round-trip the credentials correctly.
    #[tokio::test]
    async fn exchange_authorization_code_round_trips() {
        let mock = MockTokenServer::start(
            r#"{"access_token":"acc","refresh_token":"ref","expires_in":7200}"#,
            200,
        )
        .await;

        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap();
        let creds = exchange_authorization_code(
            &client,
            &mock.url,
            "the-code",
            "the-state",
            "the-verifier",
            "http://localhost:49152/callback",
        )
        .await
        .expect("exchange should succeed");

        assert_eq!(creds.access, "acc");
        assert_eq!(creds.refresh, "ref");
        assert!(creds.extra.is_empty());

        // Inspect the captured request body to lock the wire shape.
        let captured = mock.captured().await;
        let body: serde_json::Value =
            serde_json::from_str(&captured.body).expect("captured body must be JSON");
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["client_id"], CLIENT_ID);
        assert_eq!(body["code"], "the-code");
        assert_eq!(body["state"], "the-state");
        assert_eq!(body["redirect_uri"], "http://localhost:49152/callback");
        assert_eq!(body["code_verifier"], "the-verifier");
    }

    /// Refresh path: same shape, different grant type.
    #[tokio::test]
    async fn refresh_with_round_trips() {
        let mock = MockTokenServer::start(
            r#"{"access_token":"acc2","refresh_token":"ref2","expires_in":7200}"#,
            200,
        )
        .await;

        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap();
        let creds = refresh_with(&client, &mock.url, "old-refresh")
            .await
            .expect("refresh should succeed");
        assert_eq!(creds.access, "acc2");
        assert_eq!(creds.refresh, "ref2");

        let captured = mock.captured().await;
        let body: serde_json::Value = serde_json::from_str(&captured.body).unwrap();
        assert_eq!(body["grant_type"], "refresh_token");
        assert_eq!(body["client_id"], CLIENT_ID);
        assert_eq!(body["refresh_token"], "old-refresh");
    }

    /// Non-2xx responses surface as `OAuthError::Server`, carrying
    /// the upstream status and body so the caller can log it.
    #[tokio::test]
    async fn token_endpoint_errors_propagate_as_server_error() {
        let mock = MockTokenServer::start(r#"{"error":"invalid_grant"}"#, 400).await;
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap();

        let err = refresh_with(&client, &mock.url, "bad-token")
            .await
            .expect_err("non-2xx must surface as Err");
        match err {
            OAuthError::Server { status, body } => {
                assert_eq!(status, 400);
                assert!(body.contains("invalid_grant"));
            }
            other => panic!("expected Server error, got {other:?}"),
        }
    }

    /// A 2xx token body that fails to deserialize must not leak the
    /// live token material into the error string — `OAuthError`'s
    /// `Display` reaches logs/stdout.
    #[tokio::test]
    async fn parse_error_redacts_token_body() {
        // Valid JSON carrying live tokens but missing `expires_in`, so
        // it fails to deserialize on a 2xx and hits the `Parse` arm.
        let mock = MockTokenServer::start(
            r#"{"access_token":"SECRET-ACCESS","refresh_token":"SECRET-REFRESH"}"#,
            200,
        )
        .await;
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap();

        let err = refresh_with(&client, &mock.url, "old-refresh")
            .await
            .expect_err("a malformed 2xx body must surface as Err");
        let msg = err.to_string();
        assert!(
            !msg.contains("SECRET-ACCESS") && !msg.contains("SECRET-REFRESH"),
            "Parse error leaked token material: {msg}"
        );
        assert!(
            msg.contains("(redacted)"),
            "Parse error should note the body was redacted: {msg}"
        );
    }

    /// `OAuthProvider` trait wiring smoke-test: build an
    /// `AnthropicOAuth`, ask for its id/name/flags and use it as a
    /// trait object (the same shape the auth-storage layer needs).
    #[test]
    fn anthropic_oauth_implements_provider_metadata() {
        let provider = AnthropicOAuth::new();
        assert_eq!(provider.id(), "anthropic");
        assert!(provider.name().contains("Anthropic"));
        assert!(provider.uses_callback_server());
        // Default `get_api_key` returns the access token verbatim.
        let creds = OAuthCredentials::new("r", "an-access-token", 0);
        assert_eq!(provider.get_api_key(&creds), "an-access-token");

        let _: Box<dyn OAuthProvider> = Box::new(AnthropicOAuth::new());
    }

    /// Refresh through the trait object reaches the test mock,
    /// confirming the trait dispatch path is wired up correctly.
    #[tokio::test]
    async fn provider_refresh_token_uses_trait_path() {
        let mock = MockTokenServer::start(
            r#"{"access_token":"acc-x","refresh_token":"ref-x","expires_in":3600}"#,
            200,
        )
        .await;
        let provider = AnthropicOAuth::with_token_url(mock.url.clone());
        let old = OAuthCredentials::new("ref-old", "acc-old", 0);
        let new = provider
            .refresh_token(&old)
            .await
            .expect("refresh through trait succeeds");
        assert_eq!(new.access, "acc-x");
        assert_eq!(new.refresh, "ref-x");
    }
}
