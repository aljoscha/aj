//! Shared token-endpoint plumbing for the OAuth provider flows.
//!
//! Both providers POST to a token endpoint (Anthropic with a JSON
//! body, OpenAI with a form body) and get back the same response
//! shape. The HTTP send, status handling, and the body-redaction on a
//! parse failure are identical and security-relevant, so they live
//! here in one place. Each provider builds its own request (choosing
//! the body encoding) and maps the returned [`TokenResponse`] into
//! [`OAuthCredentials`](super::OAuthCredentials).

use chrono::Utc;
use serde::Deserialize;

use super::{OAuthError, redacted_body_summary};

/// Bound on every token-endpoint HTTP call. Long enough to cover the
/// network round-trip even on slow connections, short enough that a
/// hung server doesn't strand the user staring at a blinking cursor.
pub(super) const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Refresh tokens this many milliseconds before the upstream
/// `expires_in` would otherwise lapse. Guards against clock skew
/// between our machine and the auth server, and against slow network
/// calls during which the token would expire mid-request.
///
/// NOTE: this margin must comfortably exceed the worst-case refresh
/// round-trip, which is the auth storage lock wait
/// (`auth::LOCK_TIMEOUT`, 30 s) plus one [`REQUEST_TIMEOUT_SECS`] HTTP
/// call. The 5-minute value clears that with wide headroom. Shrinking
/// it below ~1 min would let a refresh race the actual expiry.
pub(super) const REFRESH_SAFETY_MARGIN_MS: i64 = 5 * 60 * 1000;

/// Wire shape of a successful token-exchange response. Fields not
/// listed here (`token_type`, `id_token`, `scope`, ...) are ignored,
/// we don't use them, and ignoring them silently keeps us
/// forward-compatible with future upstream additions.
#[derive(Debug, Deserialize)]
pub(super) struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    /// Lifetime in seconds. We convert this into an absolute timestamp
    /// at parse time so callers don't have to remember it was relative.
    pub expires_in: i64,
}

/// Send a fully-built token-endpoint POST and parse the response.
///
/// `request` carries the provider's chosen body encoding (JSON vs.
/// form) and headers. `token_url` is passed separately only to label
/// transport errors. A non-2xx response surfaces as
/// [`OAuthError::Server`] with the raw body (RFC 6749 error objects,
/// the key login-failure diagnostic, not credentials). A 2xx body that
/// fails to deserialize surfaces as [`OAuthError::Parse`] with the body
/// *redacted*: a successful token body is itself the credential
/// payload, and `OAuthError`'s `Display` reaches logs and stdout.
pub(super) async fn send_token_request(
    request: reqwest::RequestBuilder,
    token_url: &str,
) -> Result<TokenResponse, OAuthError> {
    let resp = request
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

    serde_json::from_str(&text).map_err(|e| {
        OAuthError::Parse(format!(
            "token response: {e}; body={}",
            redacted_body_summary(&text)
        ))
    })
}

/// Current unix time in milliseconds, expressed as an `i64` to match
/// the persisted format in
/// [`OAuthCredentials::expires`](super::OAuthCredentials::expires).
pub(super) fn now_unix_ms() -> i64 {
    Utc::now().timestamp_millis()
}
