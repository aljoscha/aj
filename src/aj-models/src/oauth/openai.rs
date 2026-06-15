//! OpenAI OAuth flow (ChatGPT Plus/Pro / Codex), per
//! `docs/models-spec.md` §9.4.
//!
//! Authorization Code + PKCE, with a few OpenAI-specific quirks:
//!
//! - **Random state.** OpenAI's authorization server expects a fresh
//!   random `state` value, which we mint as 16 random bytes,
//!   hex-encoded. (We use hex here rather than the base64url
//!   `state` the Anthropic flow uses only because the upstreams differ
//!   in what they round-trip cleanly; both are opaque CSRF tokens.)
//! - **Form-encoded token requests.** The token endpoint takes
//!   `application/x-www-form-urlencoded` bodies, not JSON.
//! - **JWT access tokens.** The returned `access_token` is itself a
//!   JWT. We base64url-decode the payload segment and read
//!   `claims["https://api.openai.com/auth"].chatgpt_account_id` to
//!   discover the ChatGPT account the user is logging in with.
//!   That id is persisted in [`OAuthCredentials::extra`] under the
//!   `accountId` key (see §9.1).
//! - **Distinct callback route.** The redirect URI registered with
//!   the upstream is `http://localhost:1455/auth/callback`; both port
//!   and path are fixed by the upstream.
//! - **Extra authorize params.** `id_token_add_organizations=true`,
//!   `codex_cli_simplified_flow=true`, and a caller-chosen
//!   `originator` identifier are required for the simplified Codex
//!   CLI flow.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use rand::TryRngCore;
use reqwest::Client;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::page::{error_page, success_page};
use super::pkce::generate_pkce;
use super::{OAuthAuthInfo, OAuthCallbacks, OAuthCredentials, OAuthError, OAuthProvider};

// ---------------------------------------------------------------------------
// Endpoint constants
// ---------------------------------------------------------------------------

/// Public OAuth client id registered with OpenAI for the Codex CLI
/// simplified flow. Public — the security of the flow comes from
/// PKCE, not from hiding the id.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Hosted authorize endpoint the user visits in their browser.
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";

/// Token-exchange endpoint. Same URL handles both the
/// `authorization_code` and `refresh_token` grants.
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Local IP the callback HTTP server binds to. We bind the literal
/// loopback address so we don't depend on `localhost` resolving to
/// `127.0.0.1` on the user's machine; the redirect URI advertised to
/// the upstream server still uses `localhost` because that's what
/// the registered redirect requires.
const CALLBACK_HOST: &str = "127.0.0.1";

/// Fixed by the upstream authorization server's allowed redirect
/// URIs; not an implementation choice.
const CALLBACK_PORT: u16 = 1455;

/// Path the upstream redirects the browser to.
const CALLBACK_PATH: &str = "/auth/callback";

/// Full redirect URI sent in the authorization and token-exchange
/// requests. Must be exactly the value registered upstream.
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// Scope string requested at authorize time. The Codex CLI flow
/// expects exactly this set; trimming or extending it causes the
/// upstream to either refuse or downgrade.
const SCOPES: &str = "openid profile email offline_access";

/// JWT claim namespace OpenAI uses to publish provider-specific
/// identity info. The `chatgpt_account_id` field inside this
/// namespace is the value we persist.
const JWT_CLAIM_NAMESPACE: &str = "https://api.openai.com/auth";

/// Default value for the `originator` query parameter on the
/// authorize request. Can be overridden on the provider so embedders
/// can identify themselves separately if they want to.
const DEFAULT_ORIGINATOR: &str = "aj";

/// Bound on every HTTP call we make. Long enough to cover the
/// network round-trip even on slow connections, short enough that a
/// hung server doesn't strand the user staring at a blinking cursor.
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Refresh tokens this many milliseconds before the upstream
/// `expires_in` would otherwise lapse. Guards against clock skew
/// between our machine and the auth server, and against slow
/// network calls during which the token would expire mid-request.
const REFRESH_SAFETY_MARGIN_MS: i64 = 5 * 60 * 1000;

/// Cap on how much HTTP request data we'll buffer while reading the
/// callback's request line and headers. The browser sends a single
/// short GET; anything larger is malformed.
const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;

/// Length, in bytes, of the random `state` value. 16 bytes (128 bits)
/// is well above the threshold needed for CSRF protection.
const STATE_BYTES: usize = 16;

/// JSON key used to persist the ChatGPT account id inside
/// [`OAuthCredentials::extra`]. Spelled in camelCase to match the
/// `auth.json` shape documented in §9.1 (`{ ..., accountId: "..." }`).
pub const ACCOUNT_ID_FIELD: &str = "accountId";

// ---------------------------------------------------------------------------
// Public provider type
// ---------------------------------------------------------------------------

/// OpenAI OAuth provider, suitable for storing in a
/// `Box<dyn OAuthProvider>` registry.
///
/// Holds a single `reqwest::Client` configured with our timeout and a
/// useful user-agent. Cloning is cheap; the underlying client uses
/// reference-counted internals.
pub struct OpenAIOAuth {
    client: Client,
    /// Token-exchange URL. Configurable so tests can point at a local
    /// mock; in production it's always `TOKEN_URL`.
    token_url: String,
    /// Value sent as the `originator` query parameter on the
    /// authorize URL. Defaults to `"aj"`.
    originator: String,
}

impl Default for OpenAIOAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAIOAuth {
    /// Build a provider talking to the production token endpoint with
    /// the default originator (`"aj"`).
    pub fn new() -> Self {
        Self::with_token_url(TOKEN_URL)
    }

    /// Build a provider with a caller-supplied originator. Useful for
    /// embedders that want their own identifier on the authorize URL
    /// while still using the default token endpoint.
    pub fn with_originator(originator: impl Into<String>) -> Self {
        let mut s = Self::with_token_url(TOKEN_URL);
        s.originator = originator.into();
        s
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
            originator: DEFAULT_ORIGINATOR.to_string(),
        }
    }
}

#[async_trait]
impl OAuthProvider for OpenAIOAuth {
    fn id(&self) -> &str {
        // Per spec §7.4.1, OAuth credentials live under the `openai-codex`
        // provider id, separate from the `openai` provider id used for
        // plain `OPENAI_API_KEY` credentials. The JWT issued by this flow
        // is only valid against `chatgpt.com/backend-api`, so the two
        // credential pools deliberately do not overlap.
        "openai-codex"
    }

    fn name(&self) -> &str {
        "ChatGPT Plus/Pro (Codex Subscription)"
    }

    fn uses_callback_server(&self) -> bool {
        true
    }

    async fn login(&self, callbacks: &dyn OAuthCallbacks) -> Result<OAuthCredentials, OAuthError> {
        login_with(&self.client, &self.token_url, &self.originator, callbacks).await
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

/// Run the full OpenAI login flow against the supplied HTTP client
/// and token URL. Pulled out of the trait impl so tests can drive it
/// without constructing a full provider.
async fn login_with(
    client: &Client,
    token_url: &str,
    originator: &str,
    callbacks: &dyn OAuthCallbacks,
) -> Result<OAuthCredentials, OAuthError> {
    let pkce = generate_pkce()?;
    let state = generate_state()?;

    // Bind the callback listener *before* we hand the user the
    // authorize URL: if the user is fast enough to click through and
    // the redirect arrives at our port before we're listening, we'd
    // miss the callback.
    let listener = TcpListener::bind((CALLBACK_HOST, CALLBACK_PORT))
        .await
        .map_err(|e| OAuthError::Other(format!("bind {CALLBACK_HOST}:{CALLBACK_PORT}: {e}")))?;

    let auth_url = build_authorize_url(&pkce.challenge, &state, originator);
    callbacks.on_auth(OAuthAuthInfo {
        url: &auth_url,
        instructions: Some(
            "Complete login in your browser. If your browser is on a different machine, paste the final redirect URL here.",
        ),
    });

    let code = obtain_code(callbacks, listener, &state).await?;

    callbacks.on_progress("Exchanging authorization code for tokens...");
    exchange_authorization_code(client, token_url, &code, &pkce.verifier).await
}

/// Refresh an existing OpenAI OAuth token. Exposed as a free function
/// for the same reason as [`login_with`].
async fn refresh_with(
    client: &Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<OAuthCredentials, OAuthError> {
    let body = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CLIENT_ID),
    ];
    post_token_request(client, token_url, &body).await
}

/// Drive the race between the local callback server, the optional
/// manual-code-input path, and (as a last resort) the prompt
/// fallback. Returns the validated authorization code.
///
/// The select! is `biased` to prefer the callback server: if the
/// browser redirect already produced a usable code, that's
/// authoritative — we don't want a half-typed manual entry to
/// override it.
async fn obtain_code(
    callbacks: &dyn OAuthCallbacks,
    listener: TcpListener,
    expected_state: &str,
) -> Result<String, OAuthError> {
    let mut code: Option<String> = None;

    if callbacks.supports_manual_code_input() {
        let server_fut = await_callback(listener, expected_state);
        tokio::pin!(server_fut);
        tokio::select! {
            biased;
            res = &mut server_fut => {
                code = Some(res?);
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
                }
                // Empty / unparseable manual input: drop through to
                // the prompt fallback below. Dropping `server_fut`
                // closes the listener — the prompt is the only way
                // forward from here.
            }
        }
    } else {
        code = Some(await_callback(listener, expected_state).await?);
    }

    if code.is_none() {
        let input = callbacks
            .on_prompt("Paste the authorization code or full redirect URL:")
            .await?;
        let parsed = parse_authorization_input(input.trim());
        if let Some(s) = parsed.state.as_deref()
            && s != expected_state
        {
            return Err(OAuthError::StateMismatch);
        }
        code = parsed.code;
    }

    code.ok_or(OAuthError::MissingCode)
}

// ---------------------------------------------------------------------------
// Local callback server
// ---------------------------------------------------------------------------

/// Accept connections on `listener` until one of them carries a
/// valid OAuth callback. Connections that hit the wrong route, are
/// missing parameters, or fail the state check are answered with an
/// HTTP error and the loop continues — those are either stale
/// browser tabs or misrouted requests, not the callback we care
/// about.
async fn await_callback(listener: TcpListener, expected_state: &str) -> Result<String, OAuthError> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| OAuthError::Other(format!("accept: {e}")))?;
        match handle_callback_connection(&mut stream, expected_state).await? {
            Some(code) => return Ok(code),
            None => continue,
        }
    }
}

/// Service a single TCP connection from the upstream server's
/// browser redirect. Returns `Ok(Some(code))` on success,
/// `Ok(None)` for noise (wrong path, malformed request, state
/// mismatch — write a 4xx and let the caller keep listening), or
/// `Err(...)` for fatal errors (the upstream returned an `error=`
/// param, indicating the user denied the flow or the server failed).
async fn handle_callback_connection(
    stream: &mut TcpStream,
    expected_state: &str,
) -> Result<Option<String>, OAuthError> {
    let head = match read_http_request_head(stream).await {
        Ok(h) => h,
        Err(_) => return Ok(None),
    };

    let request_line = head.split("\r\n").next().unwrap_or("");
    let path_and_query = request_line.split(' ').nth(1).unwrap_or("");

    let parsed = match parse_callback_request(path_and_query) {
        Some(p) => p,
        None => {
            let _ = write_response(stream, 400, &error_page("Invalid request.", None)).await;
            return Ok(None);
        }
    };

    if parsed.path != CALLBACK_PATH {
        let _ = write_response(stream, 404, &error_page("Callback route not found.", None)).await;
        return Ok(None);
    }

    if let Some(err) = parsed.error {
        let detail = format!("Error: {err}");
        let _ = write_response(
            stream,
            400,
            &error_page("OpenAI authentication did not complete.", Some(&detail)),
        )
        .await;
        return Err(OAuthError::Authorization(err));
    }

    let (code, state) = match (parsed.code, parsed.state) {
        (Some(c), Some(s)) => (c, s),
        _ => {
            let _ = write_response(
                stream,
                400,
                &error_page("Missing code or state parameter.", None),
            )
            .await;
            return Ok(None);
        }
    };

    if state != expected_state {
        // Don't surface state mismatches as fatal — a stale browser
        // tab from a previous attempt could legitimately hit us with
        // an old state. Reject it and keep listening for the real
        // callback.
        let _ = write_response(stream, 400, &error_page("State mismatch.", None)).await;
        return Ok(None);
    }

    let _ = write_response(
        stream,
        200,
        &success_page("OpenAI authentication completed. You can close this window."),
    )
    .await;
    Ok(Some(code))
}

/// Read bytes from the stream until we see the HTTP head terminator
/// (`\r\n\r\n`) or hit our size cap. We deliberately ignore the
/// request body — the browser sends a GET with no body, and the
/// OAuth callback only ever uses query parameters.
async fn read_http_request_head(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if find_subsequence(&buf, b"\r\n\r\n").is_some() {
            break;
        }
        if buf.len() >= MAX_REQUEST_HEAD_BYTES {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Find a subsequence in a byte slice. Used to detect the end of
/// the HTTP head; pulled out for readability.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Send a minimal HTTP/1.1 response with the supplied status code
/// and body, then close the connection. Errors are intentionally
/// ignored — by the time we're writing, we already know whether the
/// callback was good; a write failure is a network blip we can't do
/// anything useful with.
async fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        len = body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

/// Parsed callback request. Either `code`+`state` are populated (the
/// happy path), or `error` is populated (the upstream rejected the
/// request), or none of them are populated (an unrelated GET hit
/// our port and we should keep waiting).
struct CallbackParams {
    path: String,
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Parse the `path?query` portion of an HTTP request line into a
/// [`CallbackParams`]. Returns `None` for genuinely malformed input;
/// missing query parameters are represented as `None` fields.
fn parse_callback_request(path_and_query: &str) -> Option<CallbackParams> {
    let url = reqwest::Url::parse(&format!("http://localhost{path_and_query}")).ok()?;
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }
    Some(CallbackParams {
        path: url.path().to_string(),
        code,
        state,
        error,
    })
}

// ---------------------------------------------------------------------------
// URL construction & input parsing
// ---------------------------------------------------------------------------

/// Build the authorize URL the user opens in their browser. All
/// parameters are required by OpenAI's OAuth server; trimming any of
/// them produces a 400 from the upstream.
fn build_authorize_url(challenge: &str, state: &str, originator: &str) -> String {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).expect("AUTHORIZE_URL is a valid URL");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", originator);
    url.into()
}

/// Generate a fresh OAuth `state` value. 16 random bytes hex-encoded
/// — 32 ASCII characters — which is large enough that a CSRF
/// attacker has no realistic chance of guessing the value.
fn generate_state() -> Result<String, OAuthError> {
    let mut bytes = [0u8; STATE_BYTES];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|err| OAuthError::Other(format!("CSPRNG failed: {err}")))?;
    let mut hex = String::with_capacity(STATE_BYTES * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)
}

/// Result of parsing user-pasted manual input.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedAuth {
    code: Option<String>,
    state: Option<String>,
}

/// Parse a string the user pasted in response to the manual-code
/// prompt. Tolerates four common shapes:
///
/// - A full redirect URL (`http://localhost:1455/auth/callback?code=X&state=Y`)
/// - The `code=X&state=Y` query fragment alone
/// - The `code#state` shorthand the upstream sometimes presents
/// - A bare authorization code (no state)
///
/// Empty input returns both fields as `None` so the caller can fall
/// through to the prompt fallback.
fn parse_authorization_input(input: &str) -> ParsedAuth {
    let value = input.trim();
    if value.is_empty() {
        return ParsedAuth {
            code: None,
            state: None,
        };
    }

    // Full URL form.
    if let Ok(url) = reqwest::Url::parse(value) {
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if code.is_some() || state.is_some() {
            return ParsedAuth { code, state };
        }
    }

    // `code#state` shorthand.
    if let Some((code, state)) = value.split_once('#')
        && !code.is_empty()
        && !state.is_empty()
    {
        return ParsedAuth {
            code: Some(code.to_string()),
            state: Some(state.to_string()),
        };
    }

    // Bare query string. We re-parse via `reqwest::Url` so percent
    // decoding and `+` handling match the URL form above.
    if value.contains("code=")
        && let Ok(url) = reqwest::Url::parse(&format!("http://localhost?{value}"))
    {
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if code.is_some() {
            return ParsedAuth { code, state };
        }
    }

    // Bare code, no state.
    ParsedAuth {
        code: Some(value.to_string()),
        state: None,
    }
}

// ---------------------------------------------------------------------------
// Token exchange
// ---------------------------------------------------------------------------

/// Wire shape of a successful token-exchange response. Fields not
/// listed here (`token_type`, `id_token`, `scope`, ...) are ignored —
/// we don't use them, and ignoring them silently keeps us
/// forward-compatible with future upstream additions.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    /// Lifetime in seconds. We convert this into an absolute
    /// timestamp at parse time so callers don't have to remember it
    /// was relative.
    expires_in: i64,
}

/// Exchange an authorization code for credentials. Used by
/// [`login_with`] after the callback or manual-input path produced a
/// code.
async fn exchange_authorization_code(
    client: &Client,
    token_url: &str,
    code: &str,
    verifier: &str,
) -> Result<OAuthCredentials, OAuthError> {
    let body = [
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", REDIRECT_URI),
    ];
    post_token_request(client, token_url, &body).await
}

/// Shared core of the two token-endpoint POSTs (`authorization_code`
/// and `refresh_token`): both endpoints take a form-encoded body and
/// return the same response shape. The returned credentials carry
/// the ChatGPT account id in the `extra` map under [`ACCOUNT_ID_FIELD`].
async fn post_token_request(
    client: &Client,
    token_url: &str,
    body: &[(&str, &str)],
) -> Result<OAuthCredentials, OAuthError> {
    let resp = client
        .post(token_url)
        .header("Accept", "application/json")
        .form(body)
        .send()
        .await
        .map_err(|e| OAuthError::Http(format!("{token_url}: {e}")))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| OAuthError::Http(format!("read body: {e}")))?;

    if !status.is_success() {
        return Err(OAuthError::Server {
            status: status.as_u16(),
            body: text,
        });
    }

    let token: TokenResponse = serde_json::from_str(&text).map_err(|e| {
        OAuthError::Parse(format!(
            "token response: {e}; body={}",
            crate::oauth::redacted_body_summary(&text)
        ))
    })?;
    token_to_credentials(token)
}

/// Convert an upstream token response into our persistable
/// [`OAuthCredentials`] shape. Computes the absolute expiration
/// timestamp here so callers don't have to track relative time, and
/// lifts the ChatGPT account id out of the JWT access token into
/// `extra[ACCOUNT_ID_FIELD]` (see §9.4 step 5).
fn token_to_credentials(t: TokenResponse) -> Result<OAuthCredentials, OAuthError> {
    let account_id = extract_account_id(&t.access_token).ok_or_else(|| {
        OAuthError::Parse("access token JWT missing chatgpt_account_id claim".into())
    })?;
    let mut extra = HashMap::new();
    extra.insert(
        ACCOUNT_ID_FIELD.to_string(),
        serde_json::Value::String(account_id),
    );
    Ok(OAuthCredentials {
        refresh: t.refresh_token,
        access: t.access_token,
        expires: now_unix_ms() + t.expires_in * 1000 - REFRESH_SAFETY_MARGIN_MS,
        extra,
    })
}

/// Decode the JWT payload of an OpenAI access token and pull out the
/// `chatgpt_account_id` claim. Returns `None` if the token is not a
/// well-formed JWT, the payload doesn't decode, the namespace claim
/// is missing, or the `chatgpt_account_id` field isn't a non-empty
/// string.
///
/// We don't *verify* the JWT signature — that's the upstream's job
/// (we just received the token from their token endpoint over TLS).
/// We only base64-decode the payload to read a claim we need.
///
/// Exposed `pub(crate)` so the Codex provider in
/// [`crate::openai::codex`] can re-decode the JWT at request time and
/// stamp the `chatgpt-account-id` header for every outgoing request.
pub(crate) fn extract_account_id(access_token: &str) -> Option<String> {
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    // Strip any base64 padding to feed `URL_SAFE_NO_PAD`. JWTs are
    // canonically unpadded, but tolerating padding here costs us
    // nothing and protects against minor encoder drift.
    let payload_b64 = parts[1].trim_end_matches('=');
    let payload_bytes = URL_SAFE_NO_PAD.decode(payload_b64.as_bytes()).ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
    let auth = payload.get(JWT_CLAIM_NAMESPACE)?;
    let id = auth.get("chatgpt_account_id")?.as_str()?;
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

/// Current unix time in milliseconds, expressed as an `i64` to match
/// the persisted format in [`OAuthCredentials::expires`].
fn now_unix_ms() -> i64 {
    Utc::now().timestamp_millis()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpStream;
    use tokio::sync::Mutex;

    use super::*;

    /// Build a JWT-shaped string with the given JSON payload. The
    /// header and signature segments are fixed dummies — the
    /// extractor only ever looks at the payload.
    fn make_jwt(payload: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\",\"typ\":\"JWT\"}");
        let body = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header}.{body}.{sig}")
    }

    /// Build a JWT carrying the OpenAI namespace claim with the given
    /// account id. Convenience for the token-exchange tests.
    fn jwt_with_account(account: &str) -> String {
        make_jwt(&serde_json::json!({
            JWT_CLAIM_NAMESPACE: { "chatgpt_account_id": account }
        }))
    }

    /// `parse_authorization_input` must accept all four supported
    /// input shapes and return the right combination of fields. The
    /// four cases live in one test so the contract is easy to read.
    #[test]
    fn parse_authorization_input_handles_all_shapes() {
        let parsed =
            parse_authorization_input("http://localhost:1455/auth/callback?code=ABC&state=DEF");
        assert_eq!(parsed.code.as_deref(), Some("ABC"));
        assert_eq!(parsed.state.as_deref(), Some("DEF"));

        let parsed = parse_authorization_input("code=ABC&state=DEF");
        assert_eq!(parsed.code.as_deref(), Some("ABC"));
        assert_eq!(parsed.state.as_deref(), Some("DEF"));

        let parsed = parse_authorization_input("ABC#DEF");
        assert_eq!(parsed.code.as_deref(), Some("ABC"));
        assert_eq!(parsed.state.as_deref(), Some("DEF"));

        let parsed = parse_authorization_input("ABC123");
        assert_eq!(parsed.code.as_deref(), Some("ABC123"));
        assert!(parsed.state.is_none());

        let parsed = parse_authorization_input("   ");
        assert!(parsed.code.is_none());
        assert!(parsed.state.is_none());
    }

    /// Authorize URL must contain every parameter OpenAI's server
    /// requires, with the documented values. Drift here breaks the
    /// flow silently (the upstream returns 400 with no useful body).
    #[test]
    fn authorize_url_contains_required_params() {
        let url_str = build_authorize_url("CHALLENGE", "STATE", "aj");
        let url = reqwest::Url::parse(&url_str).expect("authorize URL must parse");
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("auth.openai.com"));
        assert_eq!(url.path(), "/oauth/authorize");

        let pairs: HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(pairs.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(pairs.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            pairs.get("redirect_uri").map(String::as_str),
            Some(REDIRECT_URI)
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
        assert_eq!(pairs.get("state").map(String::as_str), Some("STATE"));
        assert_eq!(
            pairs.get("id_token_add_organizations").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            pairs.get("codex_cli_simplified_flow").map(String::as_str),
            Some("true")
        );
        assert_eq!(pairs.get("originator").map(String::as_str), Some("aj"));
    }

    /// `parse_callback_request` should split the path from the query
    /// and surface the three OAuth-relevant params, ignoring others.
    #[test]
    fn parse_callback_request_extracts_known_params() {
        let parsed = parse_callback_request("/auth/callback?code=A&state=B&extra=C")
            .expect("valid path parses");
        assert_eq!(parsed.path, "/auth/callback");
        assert_eq!(parsed.code.as_deref(), Some("A"));
        assert_eq!(parsed.state.as_deref(), Some("B"));
        assert!(parsed.error.is_none());

        let parsed =
            parse_callback_request("/auth/callback?error=access_denied").expect("error parses");
        assert_eq!(parsed.error.as_deref(), Some("access_denied"));
        assert!(parsed.code.is_none());
    }

    /// `extract_account_id` must read the `chatgpt_account_id` claim
    /// from the OpenAI namespace and reject every malformed shape.
    #[test]
    fn extract_account_id_decodes_jwt_payload() {
        let token = jwt_with_account("acc-123");
        assert_eq!(extract_account_id(&token), Some("acc-123".to_string()));

        // Wrong number of segments.
        assert!(extract_account_id("not.a-jwt").is_none());
        assert!(extract_account_id("only-one-segment").is_none());

        // Payload doesn't decode.
        assert!(extract_account_id("aaaa.!!!.bbbb").is_none());

        // Payload decodes but lacks the namespace.
        let no_ns = make_jwt(&serde_json::json!({"sub": "user-1"}));
        assert!(extract_account_id(&no_ns).is_none());

        // Namespace present, no `chatgpt_account_id`.
        let ns_no_field = make_jwt(&serde_json::json!({
            JWT_CLAIM_NAMESPACE: { "other": "value" }
        }));
        assert!(extract_account_id(&ns_no_field).is_none());

        // `chatgpt_account_id` present but not a string.
        let wrong_type = make_jwt(&serde_json::json!({
            JWT_CLAIM_NAMESPACE: { "chatgpt_account_id": 42 }
        }));
        assert!(extract_account_id(&wrong_type).is_none());

        // Empty string is rejected.
        let empty = make_jwt(&serde_json::json!({
            JWT_CLAIM_NAMESPACE: { "chatgpt_account_id": "" }
        }));
        assert!(extract_account_id(&empty).is_none());
    }

    /// State generator produces 32 hex chars (16 bytes) and does not
    /// repeat itself across calls. Matches the spec's "random" rule.
    #[test]
    fn state_is_random_hex() {
        let s1 = generate_state().unwrap();
        let s2 = generate_state().unwrap();
        assert_eq!(s1.len(), STATE_BYTES * 2);
        assert!(s1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(s1, s2);
    }

    /// Token response → `OAuthCredentials` conversion must produce
    /// an absolute expiry that lies in the future, with the safety
    /// margin baked in, and surface the JWT-derived account id in
    /// the `extra` map under the `accountId` key.
    #[test]
    fn token_to_credentials_extracts_account_id_and_computes_expiry() {
        let access = jwt_with_account("acc-xyz");
        let t = TokenResponse {
            access_token: access.clone(),
            refresh_token: "refresh".into(),
            expires_in: 3600,
        };
        let before = now_unix_ms();
        let creds = token_to_credentials(t).expect("valid token");
        let after = now_unix_ms();

        assert_eq!(creds.access, access);
        assert_eq!(creds.refresh, "refresh");
        let lifetime_ms = 3600 * 1000;
        assert!(creds.expires >= before + lifetime_ms - REFRESH_SAFETY_MARGIN_MS);
        assert!(creds.expires <= after + lifetime_ms - REFRESH_SAFETY_MARGIN_MS);
        assert_eq!(
            creds.extra.get(ACCOUNT_ID_FIELD),
            Some(&serde_json::Value::String("acc-xyz".into()))
        );

        // Persistence shape must match §9.1: extra fields flatten to
        // sibling keys, not a nested `extra` object.
        let json = serde_json::to_value(&creds).unwrap();
        assert_eq!(json["accountId"], "acc-xyz");
        assert!(json.get("extra").is_none());
    }

    /// Tokens whose access JWT lacks `chatgpt_account_id` are
    /// rejected with `OAuthError::Parse`. Without an account id we
    /// can't drive the OpenAI Responses API, so persisting partial
    /// credentials would set the user up for a confusing failure
    /// later.
    #[test]
    fn token_to_credentials_rejects_jwt_without_account_id() {
        let access = make_jwt(&serde_json::json!({"sub": "user-1"}));
        let t = TokenResponse {
            access_token: access,
            refresh_token: "refresh".into(),
            expires_in: 3600,
        };
        match token_to_credentials(t) {
            Err(OAuthError::Parse(msg)) => {
                assert!(msg.contains("chatgpt_account_id"));
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    /// End-to-end token exchange: spin up a tiny HTTP server that
    /// answers one POST with a canned JSON body, point the provider
    /// at it, and verify we round-trip the credentials correctly and
    /// that the wire request is form-urlencoded with the expected
    /// fields.
    #[tokio::test]
    async fn exchange_authorization_code_round_trips() {
        let access = jwt_with_account("acc-1");
        let response_body =
            format!(r#"{{"access_token":"{access}","refresh_token":"ref","expires_in":7200}}"#);
        let mock = MockTokenServer::start(&response_body, 200).await;

        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap();
        let creds = exchange_authorization_code(&client, &mock.url, "the-code", "the-verifier")
            .await
            .expect("exchange should succeed");

        assert_eq!(creds.access, access);
        assert_eq!(creds.refresh, "ref");
        assert_eq!(
            creds.extra.get(ACCOUNT_ID_FIELD),
            Some(&serde_json::Value::String("acc-1".into()))
        );

        // Inspect the captured request: body must be
        // form-urlencoded (not JSON) and content type must match.
        let captured = mock.captured().await;
        assert!(
            captured
                .content_type
                .as_deref()
                .unwrap_or("")
                .starts_with("application/x-www-form-urlencoded"),
            "unexpected content type: {:?}",
            captured.content_type
        );

        let pairs: HashMap<String, String> =
            reqwest::Url::parse(&format!("http://x?{}", captured.body))
                .unwrap()
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
        assert_eq!(
            pairs.get("grant_type").map(String::as_str),
            Some("authorization_code")
        );
        assert_eq!(pairs.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(pairs.get("code").map(String::as_str), Some("the-code"));
        assert_eq!(
            pairs.get("code_verifier").map(String::as_str),
            Some("the-verifier")
        );
        assert_eq!(
            pairs.get("redirect_uri").map(String::as_str),
            Some(REDIRECT_URI)
        );
    }

    /// Refresh path: same shape, different grant type.
    #[tokio::test]
    async fn refresh_with_round_trips() {
        let access = jwt_with_account("acc-2");
        let response_body =
            format!(r#"{{"access_token":"{access}","refresh_token":"ref2","expires_in":7200}}"#);
        let mock = MockTokenServer::start(&response_body, 200).await;

        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .unwrap();
        let creds = refresh_with(&client, &mock.url, "old-refresh")
            .await
            .expect("refresh should succeed");
        assert_eq!(creds.access, access);
        assert_eq!(creds.refresh, "ref2");
        assert_eq!(
            creds.extra.get(ACCOUNT_ID_FIELD),
            Some(&serde_json::Value::String("acc-2".into()))
        );

        let captured = mock.captured().await;
        let pairs: HashMap<String, String> =
            reqwest::Url::parse(&format!("http://x?{}", captured.body))
                .unwrap()
                .query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
        assert_eq!(
            pairs.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(pairs.get("client_id").map(String::as_str), Some(CLIENT_ID));
        assert_eq!(
            pairs.get("refresh_token").map(String::as_str),
            Some("old-refresh")
        );
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

    /// Drive the local callback server end-to-end: bind on a random
    /// port, run a synthetic browser GET to the OpenAI-style
    /// `/auth/callback` route, and verify the returned code plus
    /// the success HTML.
    #[tokio::test]
    async fn callback_server_accepts_valid_redirect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move { await_callback(listener, "STATE").await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let response = send_get(port, "/auth/callback?code=THE-CODE&state=STATE").await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("Authentication successful"));

        let code = server.await.unwrap().expect("callback should succeed");
        assert_eq!(code, "THE-CODE");
    }

    /// Wrong-path requests (including the Anthropic-style `/callback`
    /// route, which would be a likely mistype) get a 404 and the
    /// listener keeps waiting until a real `/auth/callback` arrives.
    #[tokio::test]
    async fn callback_server_ignores_wrong_path() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move { await_callback(listener, "STATE").await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let resp1 = send_get(port, "/callback?code=C&state=STATE").await;
        assert!(resp1.starts_with("HTTP/1.1 404"));

        let resp2 = send_get(port, "/auth/callback?code=C&state=STATE").await;
        assert!(resp2.starts_with("HTTP/1.1 200"));

        let code = server.await.unwrap().expect("eventually succeeds");
        assert_eq!(code, "C");
    }

    /// State mismatches (a stale browser tab from a previous attempt)
    /// must not be fatal — the listener answers 400 and keeps
    /// waiting for the real callback.
    #[tokio::test]
    async fn callback_server_keeps_waiting_on_state_mismatch() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move { await_callback(listener, "STATE").await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let resp1 = send_get(port, "/auth/callback?code=C&state=WRONG").await;
        assert!(resp1.starts_with("HTTP/1.1 400"));

        let resp2 = send_get(port, "/auth/callback?code=C&state=STATE").await;
        assert!(resp2.starts_with("HTTP/1.1 200"));

        let code = server.await.unwrap().expect("eventually succeeds");
        assert_eq!(code, "C");
    }

    /// Upstream `error=` redirects propagate as
    /// `OAuthError::Authorization` so the host can show the user
    /// what went wrong (denied scopes, server downtime, etc.).
    #[tokio::test]
    async fn callback_server_propagates_upstream_error() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move { await_callback(listener, "STATE").await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let resp = send_get(port, "/auth/callback?error=access_denied").await;
        assert!(resp.starts_with("HTTP/1.1 400"));
        assert!(resp.contains("access_denied"));

        match server.await.unwrap() {
            Err(OAuthError::Authorization(msg)) => assert_eq!(msg, "access_denied"),
            other => panic!("expected Authorization error, got {other:?}"),
        }
    }

    /// `OAuthProvider` trait wiring smoke-test: build an
    /// `OpenAIOAuth`, ask for its id/name/flags and use it as a
    /// trait object (the same shape the auth-storage layer needs).
    /// The id is the spec §7.4.1 split — OAuth credentials live under
    /// `openai-codex`, not `openai` — so the storage layer never
    /// mistakes a JWT for a plain `OPENAI_API_KEY`.
    #[test]
    fn openai_oauth_implements_provider_metadata() {
        let provider = OpenAIOAuth::new();
        assert_eq!(provider.id(), "openai-codex");
        assert!(provider.name().contains("ChatGPT"));
        assert!(provider.uses_callback_server());
        // Default `get_api_key` returns the access token verbatim.
        let creds = OAuthCredentials::new("r", "an-access-token", 0);
        assert_eq!(provider.get_api_key(&creds), "an-access-token");

        let _: Box<dyn OAuthProvider> = Box::new(OpenAIOAuth::new());
    }

    /// Refresh through the trait object reaches the test mock,
    /// confirming the trait dispatch path is wired up correctly.
    #[tokio::test]
    async fn provider_refresh_token_uses_trait_path() {
        let access = jwt_with_account("acc-trait");
        let response_body =
            format!(r#"{{"access_token":"{access}","refresh_token":"ref-x","expires_in":3600}}"#);
        let mock = MockTokenServer::start(&response_body, 200).await;
        let provider = {
            let mut p = OpenAIOAuth::with_token_url(mock.url.clone());
            p.originator = "aj".into();
            p
        };
        let old = OAuthCredentials::new("ref-old", "acc-old", 0);
        let new = provider
            .refresh_token(&old)
            .await
            .expect("refresh through trait succeeds");
        assert_eq!(new.access, access);
        assert_eq!(new.refresh, "ref-x");
        assert_eq!(
            new.extra.get(ACCOUNT_ID_FIELD),
            Some(&serde_json::Value::String("acc-trait".into()))
        );
    }

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Send a synthetic GET to the local callback server and return
    /// the raw response as a `String`.
    async fn send_get(port: u16, path_and_query: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let req = format!(
            "GET {path_and_query} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        let _ = stream.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Captured request from the mock token server. We grab both the
    /// body and the Content-Type so tests can lock the wire format.
    #[derive(Clone)]
    struct CapturedRequest {
        body: String,
        content_type: Option<String>,
    }

    /// Tiny HTTP/1.1 server that answers exactly N requests with a
    /// fixed status + body, capturing each request body and
    /// Content-Type for later inspection. Just enough machinery to
    /// round-trip token exchanges in tests without pulling in a
    /// mocking crate.
    struct MockTokenServer {
        url: String,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
        _counter: Arc<AtomicUsize>,
    }

    impl MockTokenServer {
        async fn start(response_body: &str, status: u16) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let url = format!("http://127.0.0.1:{port}/oauth/token");
            let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let counter = Arc::new(AtomicUsize::new(0));
            let response_body = response_body.to_string();

            {
                let captured = Arc::clone(&captured);
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    loop {
                        let (mut stream, _) = match listener.accept().await {
                            Ok(s) => s,
                            Err(_) => return,
                        };
                        let mut buf = Vec::with_capacity(1024);
                        let mut tmp = [0u8; 1024];
                        let head_end = loop {
                            let n = stream.read(&mut tmp).await.unwrap_or(0);
                            if n == 0 {
                                break None;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                            if let Some(idx) = find_subsequence(&buf, b"\r\n\r\n") {
                                break Some(idx + 4);
                            }
                        };
                        let head_end = head_end.unwrap_or(buf.len());
                        let head_str = String::from_utf8_lossy(&buf[..head_end]).into_owned();

                        let mut content_length: usize = 0;
                        let mut content_type: Option<String> = None;
                        for line in head_str.lines() {
                            let mut parts = line.splitn(2, ':');
                            let key = match parts.next() {
                                Some(k) => k.trim(),
                                None => continue,
                            };
                            let val = match parts.next() {
                                Some(v) => v.trim(),
                                None => continue,
                            };
                            if key.eq_ignore_ascii_case("Content-Length") {
                                content_length = val.parse().unwrap_or(0);
                            } else if key.eq_ignore_ascii_case("Content-Type") {
                                content_type = Some(val.to_string());
                            }
                        }

                        while buf.len() - head_end < content_length {
                            let n = stream.read(&mut tmp).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        let body =
                            String::from_utf8_lossy(&buf[head_end..head_end + content_length])
                                .into_owned();
                        captured
                            .lock()
                            .await
                            .push(CapturedRequest { body, content_type });

                        let reason = match status {
                            200 => "OK",
                            400 => "Bad Request",
                            _ => "Error",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\n\
                             Content-Type: application/json\r\n\
                             Content-Length: {len}\r\n\
                             Connection: close\r\n\
                             \r\n{response_body}",
                            len = response_body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.shutdown().await;
                        counter.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }

            Self {
                url,
                captured,
                _counter: counter,
            }
        }

        async fn captured(&self) -> CapturedRequest {
            for _ in 0..50 {
                {
                    let lock = self.captured.lock().await;
                    if let Some(b) = lock.last() {
                        return b.clone();
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("no request was captured by the mock server");
        }
    }
}
