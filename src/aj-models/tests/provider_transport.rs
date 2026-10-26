use std::collections::HashMap;
use std::time::Duration;

use aj_models::provider::provider_for;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::types::{
    AssistantMessage, CacheRetention, Context, ErrorCategory, StopReason, StreamOptions,
};
use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const APIS: [(&str, &str); 4] = [
    ("anthropic-messages", "/v1/messages"),
    ("openai-completions", "/chat/completions"),
    ("openai-responses", "/responses"),
    ("openai-codex-responses", "/codex/responses"),
];
const BOUND: Duration = Duration::from_secs(5);
const REJECTION: &str = "transport fixture rejection";

fn model(api: &str, listener: &TcpListener) -> ModelInfo {
    ModelInfo {
        id: "transport-test".into(),
        name: "Transport test".into(),
        family: None,
        api: api.into(),
        provider: "test".into(),
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        reasoning: false,
        reasoning_options: Vec::new(),
        supports_verbosity: false,
        default_verbosity: None,
        speed_modes: Vec::new(),
        default_speed: None,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 1024,
        max_tokens: 64,
    }
}

// Calls 0 and 1 change identity and settings. Call 2 removes optional settings.
fn options(api: &str, call: usize) -> StreamOptions {
    let key = match api {
        "openai-codex-responses" => format!(
            "e30.{}.signature",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                json!({"https://api.openai.com/auth": {
                    "chatgpt_account_id": format!("account-{call}")
                }})
                .to_string()
            )
        ),
        "anthropic-messages" if call == 0 => "sk-ant-oat-test-0".into(),
        _ => format!("test-key-{call}"),
    };
    StreamOptions {
        api_key: Some(key),
        session_id: (call < 2).then(|| format!("session-{call}")),
        cache_retention: CacheRetention::Short,
        headers: (api == "anthropic-messages" && call < 2)
            .then(|| HashMap::from([("anthropic-beta".into(), format!("test-beta-{call}"))])),
        ..Default::default()
    }
}

struct Request {
    headers: HashMap<String, String>,
    body: Value,
}

async fn read_request(socket: &mut TcpStream, path: &str) -> Request {
    let mut bytes = Vec::new();
    let header_end = loop {
        assert!(bytes.len() < 64 * 1024, "bounded fixture request headers");
        assert_ne!(
            socket.read_buf(&mut bytes).await.unwrap(),
            0,
            "request must arrive on this socket"
        );
        if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let raw_headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let mut lines = raw_headers.lines();
    assert_eq!(lines.next().unwrap(), format!("POST {path} HTTP/1.1"));
    let mut headers = HashMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').unwrap();
        assert!(
            headers
                .insert(name.to_ascii_lowercase(), value.trim().to_string())
                .is_none(),
            "unexpected duplicate header: {name}"
        );
    }
    let length: usize = headers
        .get("content-length")
        .expect("request has a content length")
        .parse()
        .unwrap();
    assert!(length < 64 * 1024, "bounded fixture request body");
    while bytes.len() < header_end + length {
        assert_ne!(socket.read_buf(&mut bytes).await.unwrap(), 0);
    }
    assert_eq!(bytes.len(), header_end + length, "no pipelined requests");
    Request {
        headers,
        body: serde_json::from_slice(&bytes[header_end..]).unwrap(),
    }
}

async fn reject(socket: &mut TcpStream) {
    // A finite body lets the SDK drain the response and return the HTTP/1.1
    // connection to its pool without needing generation or SSE fixtures.
    let body = json!({"type": "error", "error": {
        "type": "authentication_error", "code": "invalid_api_key", "message": REJECTION
    }})
    .to_string();
    socket
        .write_all(
            format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

fn assert_rejected(message: AssistantMessage) {
    assert_eq!(message.stop_reason, StopReason::Error);
    let error = message.error.expect("fixture rejection reaches caller");
    assert_eq!(error.category, ErrorCategory::Auth);
    assert_eq!(error.message, REJECTION);
}

fn assert_settings(api: &str, call: usize, options: &StreamOptions, request: &Request) {
    let header = |name: &str| request.headers.get(name).map(String::as_str);
    let key = options.api_key.as_deref().unwrap();
    let oauth = key.starts_with("sk-ant-oat");
    if api == "anthropic-messages" && !oauth {
        assert_eq!(header("x-api-key"), Some(key));
        assert_eq!(header("authorization"), None);
    } else {
        assert_eq!(
            header("authorization"),
            Some(format!("Bearer {key}").as_str())
        );
        assert_eq!(header("x-api-key"), None);
    }
    if api == "anthropic-messages" {
        let betas: Vec<_> = header("anthropic-beta").unwrap().split(',').collect();
        for beta in ["oauth-2025-04-20", "claude-code-20250219"] {
            assert_eq!(betas.contains(&beta), oauth, "{beta}");
        }
        for i in 0..2 {
            let beta = format!("test-beta-{i}");
            assert_eq!(betas.contains(&beta.as_str()), call == i, "{beta}");
        }
        assert_eq!(header("x-app"), oauth.then_some("cli"));
    }
    if api == "openai-codex-responses" {
        assert_eq!(
            header("chatgpt-account-id"),
            Some(format!("account-{call}").as_str())
        );
        for name in ["session_id", "x-client-request-id"] {
            assert_eq!(header(name), options.session_id.as_deref());
        }
    }
    if api == "openai-responses" {
        // Responses only sends correlation headers to api.openai.com, not
        // loopback or proxy hosts. Its cache key still follows each call.
        assert_eq!(header("session_id"), None);
        assert_eq!(header("x-client-request-id"), None);
    }
    if matches!(api, "openai-responses" | "openai-codex-responses") {
        assert_eq!(
            request.body.get("prompt_cache_key").and_then(Value::as_str),
            options.session_id.as_deref()
        );
    }
}

#[tokio::test]
async fn retained_providers_reuse_sockets_without_retaining_request_settings() {
    for (api, path) in APIS {
        tokio::time::timeout(BOUND, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut model = model(api, &listener);
            let context = Context::new("transport test");
            let provider = provider_for(api).unwrap();

            let first_options = options(api, 0);
            let first = provider.stream(&model, &context, &first_options);
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket, path).await;
            assert_settings(api, 0, &first_options, &request);
            reject(&mut socket).await;
            assert_rejected(first.result().await);

            // Reading the same accepted socket is the reuse witness. A second
            // connection cannot satisfy this read, even on the same endpoint.
            for call in 1..3 {
                let options = options(api, call);
                let stream = provider.stream(&model, &context, &options);
                let request = read_request(&mut socket, path).await;
                assert_settings(api, call, &options, &request);
                reject(&mut socket).await;
                assert_rejected(stream.result().await);
            }

            let other = TcpListener::bind("127.0.0.1:0").await.unwrap();
            model.base_url = format!("http://{}", other.local_addr().unwrap());
            let other_options = options(api, 0);
            let stream = provider.stream(&model, &context, &other_options);
            let (mut other_socket, _) = other.accept().await.unwrap();
            let request = read_request(&mut other_socket, path).await;
            assert_settings(api, 0, &other_options, &request);
            reject(&mut other_socket).await;
            assert_rejected(stream.result().await);

            // Keep the original provider and its idle socket alive. A fresh
            // provider must connect rather than borrow a global pool entry.
            model.base_url = format!("http://{}", listener.local_addr().unwrap());
            let fresh_provider = provider_for(api).unwrap();
            let stream = fresh_provider.stream(&model, &context, &first_options);
            let (mut fresh_socket, _) = listener.accept().await.unwrap();
            assert_ne!(
                socket.peer_addr().unwrap(),
                fresh_socket.peer_addr().unwrap()
            );
            let request = read_request(&mut fresh_socket, path).await;
            assert_settings(api, 0, &first_options, &request);
            reject(&mut fresh_socket).await;
            assert_rejected(stream.result().await);
        })
        .await
        .unwrap_or_else(|_| panic!("{api}: sequential transport test timed out"));
    }
}

#[tokio::test]
async fn cancelling_one_request_does_not_cancel_another_on_the_same_provider() {
    for (api, path) in APIS {
        tokio::time::timeout(BOUND, async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let model = model(api, &listener);
            let context = Context::new("transport test");
            let provider = provider_for(api).unwrap();
            let cancel = CancellationToken::new();
            let mut first_options = options(api, 0);
            first_options.cancel = Some(cancel.clone());
            let first = provider.stream(&model, &context, &first_options);
            let (mut first_socket, _) = listener.accept().await.unwrap();
            read_request(&mut first_socket, path).await;

            let second = provider.stream(&model, &context, &options(api, 1));
            let (mut second_socket, _) = listener.accept().await.unwrap();
            read_request(&mut second_socket, path).await;
            assert_ne!(
                first_socket.peer_addr().unwrap(),
                second_socket.peer_addr().unwrap()
            );

            // Both complete requests have reached distinct sockets and neither
            // has a response. Cancellation is staged by I/O, not scheduling delays.
            cancel.cancel();
            let aborted = first.result().await;
            assert_eq!(aborted.stop_reason, StopReason::Aborted);
            assert_eq!(aborted.error.unwrap().category, ErrorCategory::Aborted);
            assert_eq!(
                first_socket.read(&mut [0]).await.unwrap(),
                0,
                "cancellation releases the in-flight HTTP connection"
            );
            reject(&mut second_socket).await;
            assert_rejected(second.result().await);
        })
        .await
        .unwrap_or_else(|_| panic!("{api}: concurrent cancellation test timed out"));
    }
}
