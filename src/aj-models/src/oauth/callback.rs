//! The local HTTP callback server shared by the OAuth provider flows.
//!
//! Both the Anthropic and OpenAI flows bind a loopback
//! listener, wait for the browser redirect, validate the `state`
//! parameter, and answer with a success/error page. The only
//! provider-specific bits are the callback path and the provider word
//! shown on those pages, captured in [`CallbackConfig`]. Everything
//! else, the accept loop, the request-head reader, the response
//! writer, and the query parsing, lives here so a hardening fix lands
//! once rather than in two drifting copies.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::OAuthError;
use super::page::{error_page, success_page};

/// Cap on how much HTTP request data we'll buffer while reading the
/// callback's request line and headers. The browser sends a single
/// short GET, so this is generous headroom.
///
/// On hitting the cap we stop reading and parse whatever we have
/// rather than rejecting. Only the request line is consulted and it
/// arrives before any headers, so oversized headers can't hide it. A
/// request line somehow larger than the cap just fails to parse, which
/// answers 400 and keeps the listener waiting.
const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;

/// Per-provider parameters for the callback server. The path is the
/// route the upstream redirects to. The name is the provider word
/// rendered on the success/error pages (e.g. `"Anthropic"`).
pub(super) struct CallbackConfig<'a> {
    pub path: &'a str,
    pub provider_name: &'a str,
}

/// Accept connections on `listener` until one carries a valid OAuth
/// callback, returning the authorization code. Connections that hit
/// the wrong route, are missing parameters, or fail the state check
/// are answered with an HTTP error and the loop continues, since those
/// are either stale browser tabs or misrouted requests, not the
/// callback we care about.
///
/// The returned code's `state` has already been validated to equal
/// `expected_state`, so callers don't carry it back out.
pub(super) async fn await_callback(
    listener: TcpListener,
    expected_state: &str,
    config: &CallbackConfig<'_>,
) -> Result<String, OAuthError> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| OAuthError::Other(format!("accept: {e}")))?;
        match handle_callback_connection(&mut stream, expected_state, config).await? {
            Some(code) => return Ok(code),
            None => continue,
        }
    }
}

/// Service a single TCP connection from the upstream server's browser
/// redirect. Returns `Ok(Some(code))` on success, `Ok(None)` for noise
/// (wrong path, malformed request, state mismatch, write a 4xx and let
/// the caller keep listening), or `Err(...)` for fatal errors (the
/// upstream returned an `error=` param, indicating the user denied the
/// flow or the server failed).
async fn handle_callback_connection(
    stream: &mut TcpStream,
    expected_state: &str,
    config: &CallbackConfig<'_>,
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

    if parsed.path != config.path {
        let _ = write_response(stream, 404, &error_page("Callback route not found.", None)).await;
        return Ok(None);
    }

    if let Some(err) = parsed.error {
        let detail = format!("Error: {err}");
        let _ = write_response(
            stream,
            400,
            &error_page(
                &format!("{} authentication did not complete.", config.provider_name),
                Some(&detail),
            ),
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
        // Don't surface state mismatches as fatal, a stale browser tab
        // from a previous attempt could legitimately hit us with an old
        // state. Reject it and keep listening for the real callback.
        let _ = write_response(stream, 400, &error_page("State mismatch.", None)).await;
        return Ok(None);
    }

    let _ = write_response(
        stream,
        200,
        &success_page(&format!(
            "{} authentication completed. You can close this window.",
            config.provider_name
        )),
    )
    .await;
    Ok(Some(code))
}

/// Read bytes from the stream until we see the HTTP head terminator
/// (`\r\n\r\n`) or hit our size cap. We deliberately ignore the request
/// body, the browser sends a GET with no body, and the OAuth callback
/// only ever uses query parameters.
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

/// Find a subsequence in a byte slice. Used to detect the end of the
/// HTTP head; pulled out for readability.
pub(super) fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Send a minimal HTTP/1.1 response with the supplied status code and
/// body, then close the connection. Errors are intentionally ignored,
/// by the time we're writing we already know whether the callback was
/// good; a write failure is a network blip we can't do anything useful
/// with.
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
/// request), or none of them are populated (an unrelated GET hit our
/// port and we should keep waiting).
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpStream;

    use super::*;

    /// A callback config for the tests. The path and provider name are
    /// arbitrary, the server logic is provider-agnostic.
    const TEST_CONFIG: CallbackConfig<'static> = CallbackConfig {
        path: "/callback",
        provider_name: "Test",
    };

    /// `parse_callback_request` should split the path from the query
    /// and surface the three OAuth-relevant params, ignoring others. It
    /// is path-agnostic, both provider routes parse the same way.
    #[test]
    fn parse_callback_request_extracts_known_params() {
        let parsed =
            parse_callback_request("/callback?code=A&state=B&extra=C").expect("valid path parses");
        assert_eq!(parsed.path, "/callback");
        assert_eq!(parsed.code.as_deref(), Some("A"));
        assert_eq!(parsed.state.as_deref(), Some("B"));
        assert!(parsed.error.is_none());

        let parsed =
            parse_callback_request("/auth/callback?error=access_denied").expect("error parses");
        assert_eq!(parsed.path, "/auth/callback");
        assert_eq!(parsed.error.as_deref(), Some("access_denied"));
        assert!(parsed.code.is_none());

        // Wrong path is still a valid request, we just don't act on it.
        // The caller decides what to do with the path.
        let parsed = parse_callback_request("/elsewhere").expect("any path parses");
        assert_eq!(parsed.path, "/elsewhere");
    }

    /// Drive the local callback server end-to-end: bind on a random
    /// port, run a synthetic browser GET, and verify the returned code
    /// plus the success HTML.
    #[tokio::test]
    async fn callback_server_accepts_valid_redirect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server =
            tokio::spawn(async move { await_callback(listener, "VERIFIER", &TEST_CONFIG).await });

        // Synthesize a browser GET. We send it with a small delay so
        // the server task has a chance to call `accept`.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let response = send_get(port, "/callback?code=THE-CODE&state=VERIFIER").await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("Authentication successful"));

        let code = server.await.unwrap().expect("callback should succeed");
        assert_eq!(code, "THE-CODE");
    }

    /// Wrong-path requests get a 404 and the listener keeps waiting
    /// until a real callback arrives on the configured route.
    #[tokio::test]
    async fn callback_server_ignores_wrong_path() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server =
            tokio::spawn(async move { await_callback(listener, "VERIFIER", &TEST_CONFIG).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let resp1 = send_get(port, "/favicon.ico").await;
        assert!(resp1.starts_with("HTTP/1.1 404"));

        let resp2 = send_get(port, "/callback?code=C&state=VERIFIER").await;
        assert!(resp2.starts_with("HTTP/1.1 200"));

        let code = server.await.unwrap().expect("eventually succeeds");
        assert_eq!(code, "C");
    }

    /// State mismatches (a stale browser tab from a previous attempt)
    /// must not be fatal, the listener answers 400 and keeps waiting
    /// for the real callback.
    #[tokio::test]
    async fn callback_server_keeps_waiting_on_state_mismatch() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server =
            tokio::spawn(async move { await_callback(listener, "VERIFIER", &TEST_CONFIG).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let resp1 = send_get(port, "/callback?code=C&state=WRONG").await;
        assert!(resp1.starts_with("HTTP/1.1 400"));

        let resp2 = send_get(port, "/callback?code=C&state=VERIFIER").await;
        assert!(resp2.starts_with("HTTP/1.1 200"));

        let code = server.await.unwrap().expect("eventually succeeds");
        assert_eq!(code, "C");
    }

    /// Upstream `error=` redirects propagate as
    /// `OAuthError::Authorization` so the host can show the user what
    /// went wrong (denied scopes, server downtime, etc.).
    #[tokio::test]
    async fn callback_server_propagates_upstream_error() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server =
            tokio::spawn(async move { await_callback(listener, "VERIFIER", &TEST_CONFIG).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let resp = send_get(port, "/callback?error=access_denied").await;
        assert!(resp.starts_with("HTTP/1.1 400"));
        assert!(resp.contains("access_denied"));

        match server.await.unwrap() {
            Err(OAuthError::Authorization(msg)) => assert_eq!(msg, "access_denied"),
            other => panic!("expected Authorization error, got {other:?}"),
        }
    }

    /// Send a synthetic GET to the local callback server and return the
    /// raw response as a `String`. Drives the listener without a real
    /// browser.
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
}
