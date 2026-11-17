//! User-authored models through app resolution, HTTP, and the Responses adapter.
use std::collections::HashMap;
use std::time::Duration;

use aj_app::model::{ModelSelection, SessionAccounts, resolve};
use aj_models::auth::AuthStorage;
use aj_models::registry::{CATALOG_SCHEMA_VERSION, Catalog, ModelRegistry, OverridesFile};
use aj_models::types::{
    AssistantContent, Context, Message, SimpleStreamOptions, StopReason, ThinkingLevel,
    ToolDefinition, ToolResultMessage, UserMessage,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn custom_responses_config_streams_tools_and_replays_results_without_auth() {
    tokio::time::timeout(Duration::from_secs(10), exercise())
        .await
        .unwrap();
}

async fn exercise() {
    let scratch = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let path = scratch.path().join("models-custom.json");
    std::fs::write(&path, json!({"models": [{
        "id": "qwen3.8-27b", "name": "Qwen 3.8 27B", "provider": "local",
        "provider_name": "llama.cpp", "api": "openai-responses",
        "base_url": format!("{url}/"), "supports_tools": true,
        "input": ["text", "image"], "output": ["text"], "reasoning": true,
        "reasoning_options": [{"type":"toggle"}, {"type":"effort","values":["low","medium","xhigh"]}],
        "context_window": 49152, "max_tokens": 16384
    }]}).to_string()).unwrap();
    let mut registry = ModelRegistry::from_catalog_with_overrides(
        Catalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            updated_at: 0,
            source: "test".into(),
            models: vec![],
        },
        OverridesFile { overrides: vec![] },
        "test",
    );
    registry.load_custom_file(&path).unwrap();
    let auth = AuthStorage::with_providers(scratch.path().join("auth.json"), HashMap::new());
    let mut model = resolve(
        &registry,
        &auth,
        &ModelSelection {
            api: Some("local".into()),
            name: Some("qwen3.8-27b".into()),
            url: None,
        },
        None,
    )
    .unwrap();
    SessionAccounts::default().install_model(&mut model.stream_options, &auth, &model.model_info);
    let server = tokio::spawn(async move {
        for (index, fixture) in [
            include_str!("../../openai-sdk/tests/fixtures/llama-cpp-tool.sse"),
            include_str!("../../openai-sdk/tests/fixtures/llama-cpp-text.sse"),
        ]
        .into_iter()
        .enumerate()
        {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                assert!(bytes.len() < 65536);
                assert_ne!(socket.read_buf(&mut bytes).await.unwrap(), 0);
                if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let headers = std::str::from_utf8(&bytes[..header_end])
                .unwrap()
                .to_lowercase();
            assert!(headers.starts_with("post /v1/responses http/1.1"));
            assert!(!headers.contains("\r\nauthorization:"));
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length: ")
                        .map(|v| v.parse().unwrap())
                })
                .unwrap();
            assert!(length < 65536);
            while bytes.len() < header_end + length {
                assert_ne!(socket.read_buf(&mut bytes).await.unwrap(), 0);
            }
            let body: Value = serde_json::from_slice(&bytes[header_end..]).unwrap();
            assert_eq!(body["model"], "qwen3.8-27b");
            assert_eq!(body["reasoning"]["effort"], "low");
            assert_eq!(body["tools"][0]["name"], "lookup_weather");
            if index == 1 {
                let input = body["input"].as_array().unwrap();
                let call = input.iter().find(|i| i["type"] == "function_call").unwrap();
                let result = input
                    .iter()
                    .find(|i| i["type"] == "function_call_output")
                    .unwrap();
                assert_eq!(call["call_id"], result["call_id"]);
                assert_eq!(result["output"], "Sunny");
            }
            socket.write_all(format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                fixture.len(), fixture,
            ).as_bytes()).await.unwrap();
        }
    });
    let mut context = Context::new("Use the weather tool.");
    context
        .messages
        .push(Message::User(UserMessage::text("Weather in Berlin?")));
    context.tools.push(ToolDefinition {
        name: "lookup_weather".into(), description: "Look up weather".into(),
        parameters: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
        input_format: None,
    });
    let options = SimpleStreamOptions {
        base: model.stream_options,
        reasoning: ThinkingLevel::Low,
    };
    let stream = model
        .provider
        .stream_simple(&model.model_info, &context, &options);
    let first = stream.result().await;
    assert_eq!(first.stop_reason, StopReason::ToolUse, "{first:?}");
    let call = first
        .content
        .iter()
        .find_map(|c| match c {
            AssistantContent::ToolCall(c) => Some(c.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(call.name, "lookup_weather");
    assert_eq!(call.arguments, json!({"city":"Berlin"}));
    context.messages.push(Message::Assistant(first));
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage::text(
            call.id, call.name, "Sunny", false,
        )));
    let stream = model
        .provider
        .stream_simple(&model.model_info, &context, &options);
    let second = stream.result().await;
    assert_eq!(second.stop_reason, StopReason::Stop, "{second:?}");
    assert!(
        second
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::Text(t) if t.text == "hello"))
    );
    assert!(second.usage.total_tokens > 0);
    server.await.unwrap();
}
