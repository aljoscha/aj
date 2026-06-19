//! Test-only HTTP doubles shared by the OAuth provider test suites.
//!
//! A tiny HTTP/1.1 server that answers token-endpoint POSTs with a
//! canned response while capturing the request, just enough to
//! round-trip token exchanges without pulling in a mocking crate.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use super::callback::find_subsequence;

/// A captured token-endpoint request. Carries the body and the
/// `Content-Type` so tests can lock the wire format (JSON vs. form).
#[derive(Clone)]
pub(super) struct CapturedRequest {
    pub body: String,
    pub content_type: Option<String>,
}

/// Tiny HTTP/1.1 server that answers each request with a fixed status
/// and body, capturing each request for later inspection.
pub(super) struct MockTokenServer {
    pub url: String,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    _counter: Arc<AtomicUsize>,
}

impl MockTokenServer {
    pub async fn start(response_body: &str, status: u16) -> Self {
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
                    // Read until the head terminator, then read
                    // `Content-Length` more bytes for the body.
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
                    let body = String::from_utf8_lossy(&buf[head_end..head_end + content_length])
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

    /// Wait briefly for the spawned task to record a request and return
    /// the most recent capture.
    pub async fn captured(&self) -> CapturedRequest {
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
