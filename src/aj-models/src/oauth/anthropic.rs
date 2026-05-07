//! Anthropic OAuth flow (Claude Pro/Max), per `docs/models-spec.md` §9.3.
//!
//! Authorization Code + PKCE. The high-level flow:
//!
//! 1. Generate a PKCE verifier / challenge pair (S256). The verifier
//!    is reused as the OAuth `state` parameter — Anthropic's
//!    authorization server is configured to expect this exact
//!    arrangement and rejects randomly generated states.
//! 2. Bind a local HTTP callback server on `127.0.0.1:53692`. The
//!    redirect URI registered with the upstream server is
//!    `http://localhost:53692/callback`, so the port is fixed; we
//!    have no choice in the matter.
//! 3. Hand the user the authorize URL via [`OAuthCallbacks::on_auth`]
//!    and wait for either the browser callback to hit our server or
//!    — for hosts that support it — a manually pasted code. The two
//!    paths race; whichever arrives first wins. If neither produces
//!    a code we fall back to a single [`OAuthCallbacks::on_prompt`]
//!    call so the user can paste the redirect URL themselves.
//! 4. POST the authorization code to the token endpoint.
//! 5. Compute an absolute expiration timestamp from the returned
//!    `expires_in`, applying a small safety margin so we proactively
//!    refresh just before the upstream token actually lapses.
//!
//! The flow exposes itself both as a free-function API (good for
//! ad-hoc CLI use and tests) and behind the [`OAuthProvider`] trait
//! (for the auth-storage layer in §9.1, which keys by provider id).

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
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

/// Public OAuth client id registered with Anthropic for the Claude
/// Code-style desktop flow. This is intentionally not a secret — the
/// security of the flow comes from PKCE, not from hiding the id.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Hosted authorize endpoint the user visits in their browser.
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";

/// Token-exchange endpoint. Same URL handles both the
/// `authorization_code` and `refresh_token` grants.
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// Local IP the callback HTTP server binds to. We bind the literal
/// loopback address so we don't depend on `localhost` resolving to
/// `127.0.0.1` on the user's machine; the redirect URI advertised to
/// the upstream server still uses `localhost` because that's what
/// the registered redirect requires.
const CALLBACK_HOST: &str = "127.0.0.1";

/// Fixed by the upstream authorization server's allowed redirect
/// URIs; not an implementation choice.
const CALLBACK_PORT: u16 = 53692;

/// Path the upstream redirects the browser to.
const CALLBACK_PATH: &str = "/callback";

/// Full redirect URI sent in the authorization and token-exchange
/// requests. Must be exactly the value registered upstream.
const REDIRECT_URI: &str = "http://localhost:53692/callback";

/// Scope string requested at authorize time. The wide set is what
/// Claude Code asks for; trimming it would cause the upstream server
/// to either refuse or downgrade.
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

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

    // Bind the callback listener *before* we hand the user the
    // authorize URL: if the user is fast enough to click through and
    // the redirect arrives at our port before we're listening, we'd
    // miss the callback.
    let listener = TcpListener::bind((CALLBACK_HOST, CALLBACK_PORT))
        .await
        .map_err(|e| OAuthError::Other(format!("bind {CALLBACK_HOST}:{CALLBACK_PORT}: {e}")))?;

    let auth_url = build_authorize_url(&pkce.challenge, &pkce.verifier);
    callbacks.on_auth(OAuthAuthInfo {
        url: &auth_url,
        instructions: Some(
            "Complete login in your browser. If your browser is on a different machine, paste the final redirect URL here.",
        ),
    });

    let (code, state) = obtain_code_and_state(callbacks, listener, &pkce.verifier).await?;

    callbacks.on_progress("Exchanging authorization code for tokens...");
    exchange_authorization_code(client, token_url, &code, &state, &pkce.verifier).await
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
/// fallback. Returns the validated `(code, state)` pair.
///
/// The select! is `biased` to prefer the callback server: if the
/// browser redirect already produced a usable code, that's
/// authoritative — we don't want a half-typed manual entry to
/// override it.
async fn obtain_code_and_state(
    callbacks: &dyn OAuthCallbacks,
    listener: TcpListener,
    verifier: &str,
) -> Result<(String, String), OAuthError> {
    let mut code: Option<String> = None;
    let mut state: Option<String> = None;

    if callbacks.supports_manual_code_input() {
        let server_fut = await_callback(listener, verifier);
        tokio::pin!(server_fut);
        tokio::select! {
            biased;
            res = &mut server_fut => {
                let (c, s) = res?;
                code = Some(c);
                state = Some(s);
            }
            res = callbacks.on_manual_code_input() => {
                let input = res?;
                let parsed = parse_authorization_input(input.trim());
                if let Some(s) = parsed.state.as_deref()
                    && s != verifier
                {
                    return Err(OAuthError::StateMismatch);
                }
                if let Some(c) = parsed.code {
                    code = Some(c);
                    state = Some(parsed.state.unwrap_or_else(|| verifier.to_string()));
                }
                // Empty / unparseable manual input: drop through to
                // the prompt fallback below. Dropping `server_fut`
                // closes the listener — the prompt is the only way
                // forward from here.
            }
        }
    } else {
        let (c, s) = await_callback(listener, verifier).await?;
        code = Some(c);
        state = Some(s);
    }

    if code.is_none() {
        let input = callbacks
            .on_prompt("Paste the authorization code or full redirect URL:")
            .await?;
        let parsed = parse_authorization_input(input.trim());
        if let Some(s) = parsed.state.as_deref()
            && s != verifier
        {
            return Err(OAuthError::StateMismatch);
        }
        code = parsed.code;
        state = parsed.state.or_else(|| Some(verifier.to_string()));
    }

    let code = code.ok_or(OAuthError::MissingCode)?;
    let state = state.unwrap_or_else(|| verifier.to_string());
    Ok((code, state))
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
async fn await_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<(String, String), OAuthError> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| OAuthError::Other(format!("accept: {e}")))?;
        match handle_callback_connection(&mut stream, expected_state).await? {
            Some(pair) => return Ok(pair),
            None => continue,
        }
    }
}

/// Service a single TCP connection from the upstream server's
/// browser redirect. Returns `Ok(Some((code, state)))` on success,
/// `Ok(None)` for noise (wrong path, malformed request, state
/// mismatch — write a 4xx and let the caller keep listening), or
/// `Err(...)` for fatal errors (the upstream returned an `error=`
/// param, indicating the user denied the flow or the server failed).
async fn handle_callback_connection(
    stream: &mut TcpStream,
    expected_state: &str,
) -> Result<Option<(String, String)>, OAuthError> {
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
            &error_page("Anthropic authentication did not complete.", Some(&detail)),
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
        &success_page("Anthropic authentication completed. You can close this window."),
    )
    .await;
    Ok(Some((code, state)))
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
    // `reqwest::Url` is a re-export of `url::Url`; using it lets us
    // delegate percent-decoding and query parsing to the standard
    // implementation rather than rolling our own.
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
/// parameters are required by Anthropic's OAuth server; trimming any
/// of them produces a 400 from the upstream.
///
/// Note that `state` is intentionally set to the verifier rather
/// than a fresh random value. The Anthropic server validates state
/// against the verifier-derived material at token-exchange time;
/// passing anything else causes the exchange to fail.
fn build_authorize_url(challenge: &str, verifier: &str) -> String {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL).expect("AUTHORIZE_URL is a valid URL");
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", verifier);
    url.into()
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
/// - A full redirect URL (`http://localhost:53692/callback?code=X&state=Y`)
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
/// listed here (`token_type`, `scope`, ...) are ignored — we don't
/// use them, and ignoring them silently keeps us forward-compatible
/// with future upstream additions.
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
/// `(code, state)` pair.
async fn exchange_authorization_code(
    client: &Client,
    token_url: &str,
    code: &str,
    state: &str,
    verifier: &str,
) -> Result<OAuthCredentials, OAuthError> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": verifier,
    });
    post_token_request(client, token_url, body).await
}

/// Shared core of the two token-endpoint POSTs (`authorization_code`
/// and `refresh_token`): both endpoints take a JSON body and return
/// the same response shape.
async fn post_token_request(
    client: &Client,
    token_url: &str,
    body: serde_json::Value,
) -> Result<OAuthCredentials, OAuthError> {
    let resp = client
        .post(token_url)
        .header("Accept", "application/json")
        .json(&body)
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

    let token: TokenResponse = serde_json::from_str(&text)
        .map_err(|e| OAuthError::Parse(format!("token response: {e}; body={text}")))?;
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

    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpStream;
    use tokio::sync::Mutex;

    use super::*;

    /// `parse_authorization_input` must accept all four supported
    /// input shapes and return the right combination of fields. The
    /// four cases live in one test so the contract is easy to read.
    #[test]
    fn parse_authorization_input_handles_all_shapes() {
        // Full URL.
        let parsed =
            parse_authorization_input("http://localhost:53692/callback?code=ABC&state=DEF");
        assert_eq!(parsed.code.as_deref(), Some("ABC"));
        assert_eq!(parsed.state.as_deref(), Some("DEF"));

        // Bare query string.
        let parsed = parse_authorization_input("code=ABC&state=DEF");
        assert_eq!(parsed.code.as_deref(), Some("ABC"));
        assert_eq!(parsed.state.as_deref(), Some("DEF"));

        // `code#state` shorthand.
        let parsed = parse_authorization_input("ABC#DEF");
        assert_eq!(parsed.code.as_deref(), Some("ABC"));
        assert_eq!(parsed.state.as_deref(), Some("DEF"));

        // Bare code, no state.
        let parsed = parse_authorization_input("ABC123");
        assert_eq!(parsed.code.as_deref(), Some("ABC123"));
        assert!(parsed.state.is_none());

        // Empty input.
        let parsed = parse_authorization_input("   ");
        assert!(parsed.code.is_none());
        assert!(parsed.state.is_none());
    }

    /// Authorize URL must contain every parameter Anthropic's server
    /// requires, with the documented values. Drift here breaks the
    /// flow silently (the upstream returns 400 with no useful body).
    #[test]
    fn authorize_url_contains_required_params() {
        let url_str = build_authorize_url("CHALLENGE", "VERIFIER");
        let url = reqwest::Url::parse(&url_str).expect("authorize URL must parse");
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("claude.ai"));
        assert_eq!(url.path(), "/oauth/authorize");

        let pairs: HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(pairs.get("code").map(String::as_str), Some("true"));
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
        // The state-equals-verifier rule in §9.3 is the most
        // surprising part of the flow; explicitly assert it.
        assert_eq!(pairs.get("state").map(String::as_str), Some("VERIFIER"));
    }

    /// `parse_callback_request` should split the path from the query
    /// and surface the three OAuth-relevant params, ignoring others.
    #[test]
    fn parse_callback_request_extracts_known_params() {
        let parsed =
            parse_callback_request("/callback?code=A&state=B&extra=C").expect("valid path parses");
        assert_eq!(parsed.path, "/callback");
        assert_eq!(parsed.code.as_deref(), Some("A"));
        assert_eq!(parsed.state.as_deref(), Some("B"));
        assert!(parsed.error.is_none());

        let parsed = parse_callback_request("/callback?error=access_denied").expect("error parses");
        assert_eq!(parsed.error.as_deref(), Some("access_denied"));
        assert!(parsed.code.is_none());

        // Wrong path is still a valid request — we just don't act on
        // it. The caller decides what to do with the path.
        let parsed = parse_callback_request("/elsewhere").expect("any path parses");
        assert_eq!(parsed.path, "/elsewhere");
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
        )
        .await
        .expect("exchange should succeed");

        assert_eq!(creds.access, "acc");
        assert_eq!(creds.refresh, "ref");
        assert!(creds.extra.is_empty());

        // Inspect the captured request body to lock the wire shape.
        let captured = mock.captured_body().await;
        let body: serde_json::Value =
            serde_json::from_str(&captured).expect("captured body must be JSON");
        assert_eq!(body["grant_type"], "authorization_code");
        assert_eq!(body["client_id"], CLIENT_ID);
        assert_eq!(body["code"], "the-code");
        assert_eq!(body["state"], "the-state");
        assert_eq!(body["redirect_uri"], REDIRECT_URI);
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

        let captured = mock.captured_body().await;
        let body: serde_json::Value = serde_json::from_str(&captured).unwrap();
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

    /// Drive the local callback server end-to-end: bind on a random
    /// port, run a synthetic browser GET, and verify the returned
    /// `(code, state)` pair plus the success HTML.
    #[tokio::test]
    async fn callback_server_accepts_valid_redirect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move { await_callback(listener, "VERIFIER").await });

        // Synthesize a browser GET. We send it with a small delay so
        // the server task has a chance to call `accept`.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let response = send_get(port, "/callback?code=THE-CODE&state=VERIFIER").await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("Authentication successful"));

        let pair = server.await.unwrap().expect("callback should succeed");
        assert_eq!(pair, ("THE-CODE".to_string(), "VERIFIER".to_string()));
    }

    /// Wrong-path requests get a 404 and the listener keeps waiting
    /// until a real `/callback` arrives.
    #[tokio::test]
    async fn callback_server_ignores_wrong_path() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move { await_callback(listener, "VERIFIER").await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let resp1 = send_get(port, "/favicon.ico").await;
        assert!(resp1.starts_with("HTTP/1.1 404"));

        let resp2 = send_get(port, "/callback?code=C&state=VERIFIER").await;
        assert!(resp2.starts_with("HTTP/1.1 200"));

        let pair = server.await.unwrap().expect("eventually succeeds");
        assert_eq!(pair.0, "C");
    }

    /// State mismatches (a stale browser tab from a previous attempt)
    /// must not be fatal — the listener answers 400 and keeps
    /// waiting for the real callback.
    #[tokio::test]
    async fn callback_server_keeps_waiting_on_state_mismatch() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move { await_callback(listener, "VERIFIER").await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let resp1 = send_get(port, "/callback?code=C&state=WRONG").await;
        assert!(resp1.starts_with("HTTP/1.1 400"));

        let resp2 = send_get(port, "/callback?code=C&state=VERIFIER").await;
        assert!(resp2.starts_with("HTTP/1.1 200"));

        let pair = server.await.unwrap().expect("eventually succeeds");
        assert_eq!(pair.1, "VERIFIER");
    }

    /// Upstream `error=` redirects propagate as
    /// `OAuthError::Authorization` so the host can show the user
    /// what went wrong (denied scopes, server downtime, etc.).
    #[tokio::test]
    async fn callback_server_propagates_upstream_error() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move { await_callback(listener, "VERIFIER").await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let resp = send_get(port, "/callback?error=access_denied").await;
        assert!(resp.starts_with("HTTP/1.1 400"));
        assert!(resp.contains("access_denied"));

        match server.await.unwrap() {
            Err(OAuthError::Authorization(msg)) => assert_eq!(msg, "access_denied"),
            other => panic!("expected Authorization error, got {other:?}"),
        }
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

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Send a synthetic GET to the local callback server and return
    /// the raw response as a `String`. Used by the callback-server
    /// tests above to drive the listener without a real browser.
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

    /// Tiny HTTP/1.1 server that answers exactly N requests with a
    /// fixed status + body, capturing each request body for later
    /// inspection. Just enough machinery to round-trip token
    /// exchanges in tests without pulling in a mocking crate.
    struct MockTokenServer {
        url: String,
        captured: Arc<Mutex<Vec<String>>>,
        _counter: Arc<AtomicUsize>,
    }

    impl MockTokenServer {
        async fn start(response_body: &'static str, status: u16) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let url = format!("http://127.0.0.1:{port}/v1/oauth/token");
            let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let counter = Arc::new(AtomicUsize::new(0));

            {
                let captured = captured.clone();
                let counter = counter.clone();
                tokio::spawn(async move {
                    loop {
                        let (mut stream, _) = match listener.accept().await {
                            Ok(s) => s,
                            Err(_) => return,
                        };
                        // Read until we see the head terminator, then
                        // read `Content-Length` more bytes for the body.
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

                        // Parse Content-Length so we know how much body to read.
                        let content_length: usize = head_str
                            .lines()
                            .find_map(|line| {
                                let mut parts = line.splitn(2, ':');
                                let key = parts.next()?.trim();
                                let val = parts.next()?.trim();
                                if key.eq_ignore_ascii_case("Content-Length") {
                                    val.parse().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);

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
                        captured.lock().await.push(body);

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

        async fn captured_body(&self) -> String {
            // Wait briefly for the spawned task to record the body.
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
