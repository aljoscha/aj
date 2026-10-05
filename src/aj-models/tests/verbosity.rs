use std::time::Duration;

use aj_models::provider::complete_simple;
use aj_models::registry::{Catalog, ModelRegistry, bundled_codex_seed};
use aj_models::types::{
    Context, SimpleStreamOptions, StopReason, StreamOptions, ThinkingLevel, Verbosity,
};
use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn verbosity_resolves_at_each_openai_request_boundary() {
    let token = format!(
        "e30.{}.signature",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            json!({"https://api.openai.com/auth": {"chatgpt_account_id": "test-account"}})
                .to_string()
        )
    );
    let seed = bundled_codex_seed().remove(0);
    assert!(seed.supports_verbosity);
    assert_eq!(seed.default_verbosity, Some(Verbosity::Low));

    for (api, path, field) in [
        ("openai-completions", "/chat/completions", "/verbosity"),
        ("openai-responses", "/responses", "/text/verbosity"),
        (
            "openai-codex-responses",
            "/codex/responses",
            "/text/verbosity",
        ),
    ] {
        for supported in [false, true] {
            for default in [None, Some(Verbosity::Low), Some(Verbosity::High)] {
                for requested in [None, Some(Verbosity::Medium)] {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let mut model = seed.clone();
                    model.api = api.into();
                    model.base_url = format!("http://{}", listener.local_addr().unwrap());
                    model.supports_verbosity = supported;
                    model.default_verbosity = default;
                    let options = SimpleStreamOptions {
                        base: StreamOptions {
                            api_key: Some(token.clone()),
                            verbosity: requested,
                            ..Default::default()
                        },
                        reasoning: ThinkingLevel::Medium,
                        ..Default::default()
                    };
                    let context = Context::new("test");
                    let server = async {
                        let (mut socket, _) = listener.accept().await.unwrap();
                        let mut bytes = Vec::new();
                        let header_end = loop {
                            let n = socket.read_buf(&mut bytes).await.unwrap();
                            assert_ne!(n, 0, "request headers must arrive");
                            if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                                break i + 4;
                            }
                        };
                        let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
                        assert!(headers.starts_with(&format!("POST {path} HTTP/1.1")));
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .expect("request has a content length");
                        while bytes.len() < header_end + length {
                            assert_ne!(socket.read_buf(&mut bytes).await.unwrap(), 0);
                        }
                        let body: Value =
                            serde_json::from_slice(&bytes[header_end..header_end + length])
                                .unwrap();
                        // This fixture observes request serialization, not generation. A terminal
                        // auth error closes every adapter without retries or SSE scaffolding.
                        let error = r#"{"error":{"message":"test rejection","type":"invalid_request_error","code":"invalid_api_key"}}"#;
                        socket.write_all(format!(
                            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{error}", error.len()
                        ).as_bytes()).await.unwrap();
                        body
                    };
                    let (body, terminal) = tokio::time::timeout(Duration::from_secs(5), async {
                        tokio::join!(server, complete_simple(&model, &context, &options))
                    })
                    .await
                    .expect("request and terminal complete");
                    assert_eq!(terminal.stop_reason, StopReason::Error);
                    let expected = if supported {
                        requested.or(default)
                    } else {
                        None
                    };
                    assert_eq!(
                        body.pointer(field).cloned(),
                        expected.map(|v| serde_json::to_value(v).unwrap()),
                        "{api}: supported={supported}, default={default:?}, requested={requested:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn stale_cache_loads_catalog_verbosity_defaults() {
    const CHILD: &str = "AJ_TEST_VERBOSITY_CACHE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let registry = ModelRegistry::load();
        for id in ["gpt-6-astra", "gpt-6.1-sol"] {
            let model = registry.get("openai-codex", id).unwrap();
            assert!(model.supports_verbosity);
            assert_eq!(model.default_verbosity, Some(Verbosity::Low));
        }
        return;
    }

    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(home.path().join(".aj")).unwrap();
    let mut models = bundled_codex_seed();
    for model in &mut models {
        model.supports_verbosity = false;
        model.default_verbosity = None;
    }
    let mut cache = Catalog {
        schema_version: 3,
        updated_at: 0,
        source: "stale cache".into(),
        models,
    };
    for schema_version in [3, 4, 5] {
        cache.schema_version = schema_version;
        std::fs::write(
            home.path().join(".aj/models.json"),
            serde_json::to_vec(&cache).unwrap(),
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "stale_cache_loads_catalog_verbosity_defaults",
                "--nocapture",
            ])
            .env("HOME", home.path())
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
