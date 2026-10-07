use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::provider::provider_for;
use crate::registry::ModelInfo;
use crate::streaming::AssistantMessageEvent;
use crate::types::*;

const SOURCE: &str = "// 雪\nconst x = {value: `a\\b`};\nawait tools.read(x);\n";
const GRAMMAR: &str = "start: /(.|\\n)+/";

fn call(input: &str) -> Value {
    json!({"type":"custom_tool_call", "id":"ctc_1", "call_id":"call_1",
        "name":"evaluate", "input":input, "status":"completed"})
}

fn events(complete: bool, input_done: bool) -> Vec<String> {
    let response = json!({"id":"resp_1", "object":"response", "created_at":0,
        "model":"fixture", "output":[], "parallel_tool_calls":true,
        "tools":[], "status":"completed"});
    let mut events = vec![
        json!({"type":"response.created", "sequence_number":0, "response":response}),
        json!({"type":"response.output_item.added", "sequence_number":1,
            "output_index":0, "item":call("")}),
        json!({"type":"response.custom_tool_call_input.delta", "sequence_number":2,
            "item_id":"ctc_1", "output_index":0, "delta":SOURCE}),
    ];
    if input_done {
        events.push(
            json!({"type":"response.custom_tool_call_input.done", "sequence_number":3,
            "item_id":"ctc_1", "output_index":0, "input":format!("{SOURCE}// done\n")}),
        );
    }
    if complete {
        events.push(
            json!({"type":"response.output_item.done", "sequence_number":4,
            "output_index":0, "item":call(SOURCE)}),
        );
    }
    // A JSON tool also acts as a barrier after input.done, making the updated
    // partial observable before cancellation without depending on timing.
    let wait = json!({"type":"function_call", "id":"fc_2", "call_id":"call_2",
        "name":"wait", "arguments":"{\"id\":7}", "status":"completed"});
    events.push(
        json!({"type":"response.output_item.added", "sequence_number":5,
        "output_index":1, "item":wait}),
    );
    if complete {
        events.push(
            json!({"type":"response.output_item.done", "sequence_number":6,
            "output_index":1, "item":wait}),
        );
        events.push(json!({"type":"response.completed", "sequence_number":7, "response":response}));
    }
    events.into_iter().map(|e| e.to_string()).collect()
}

/// Real HTTP fixture that captures the complete request, including bodies
/// larger than a single TCP read. The response is finite and needs no cleanup.
async fn server(events: Vec<String>) -> (String, tokio::task::JoinHandle<Value>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let (start, length) = loop {
            let mut buf = [0; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&buf[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap();
                break (end + 4, length);
            }
        };
        while bytes.len() < start + length {
            let mut buf = [0; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&buf[..n]);
        }
        let request = serde_json::from_slice(&bytes[start..start + length]).unwrap();
        let body: String = events
            .iter()
            .map(|e| {
                let event = serde_json::from_str::<Value>(e)
                    .ok()
                    .and_then(|v| v["type"].as_str().map(|s| format!("event: {s}\n")))
                    .unwrap_or_default();
                format!("{event}data: {e}\n\n")
            })
            .collect();
        socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        request
    });
    (url, task)
}

fn calls(message: &AssistantMessage) -> Vec<&ToolCall> {
    message
        .content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

pub(crate) async fn verify_json_string(mut model: ModelInfo, options: StreamOptions) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let provider = provider_for(&model.api).unwrap();
        let mut context = Context::new("Use the tool");
        context.tools.push(ToolDefinition {
            name: "evaluate".into(), description: "Accept JSON".into(),
            parameters: json!({"type":"string"}), input_format: None,
        });
        let response = json!({"id":"resp_1", "object":"response", "created_at":0,
            "model":"fixture", "output":[], "parallel_tool_calls":true,
            "tools":[], "status":"completed"});
        let item = json!({"type":"function_call", "id":"fc_1", "call_id":"call_1",
            "name":"evaluate", "arguments":json!(SOURCE).to_string(), "status":"completed"});
        let mut start = item.clone();
        start["arguments"] = json!("");
        let events: Vec<String> = vec![
            json!({"type":"response.created", "sequence_number":0, "response":response}),
            json!({"type":"response.output_item.added", "sequence_number":1, "output_index":0, "item":start}),
            json!({"type":"response.function_call_arguments.delta", "sequence_number":2,
                "item_id":"fc_1", "output_index":0, "delta":json!(SOURCE).to_string()}),
            json!({"type":"response.output_item.done", "sequence_number":3, "output_index":0, "item":item}),
            json!({"type":"response.completed", "sequence_number":4, "response":response}),
        ].into_iter().map(|e| e.to_string()).collect();
        let (url, request) = server(events.clone()).await;
        model.base_url = url;
        let message = provider.stream(&model, &context, &options).result().await;
        assert_eq!(message.stop_reason, StopReason::ToolUse, "{:?}", message.error);
        assert_eq!(request.await.unwrap()["tools"][0]["type"], "function");
        assert_eq!(calls(&message).len(), 1);
        let call = calls(&message)[0];
        assert!(!call.is_raw);
        assert_eq!(call.arguments, json!(SOURCE));
        let result = ToolResultMessage::text(&call.id, &call.name, "ok", false);
        context.messages = vec![Message::Assistant(message), Message::ToolResult(result)];
        context = serde_json::from_slice(&serde_json::to_vec(&context).unwrap()).unwrap();
        context.tools.clear();
        for cross_model in [false, true] {
            if cross_model { model.id = "fallback-model".into(); }
            let (url, request) = server(events.clone()).await;
            model.base_url = url;
            let terminal = provider.stream(&model, &context, &options).result().await;
            assert_eq!(terminal.stop_reason, StopReason::ToolUse, "{:?}", terminal.error);
            let request = request.await.unwrap();
            let input = request["input"].as_array().unwrap();
            assert!(!input.iter().any(|i| i["type"].as_str().unwrap().starts_with("custom_tool_call")));
            let call = input.iter().find(|i| i["type"] == "function_call").unwrap();
            assert_eq!(call["arguments"], json!(SOURCE).to_string());
            assert_eq!(call["call_id"], "call_1");
            let result = input.iter().find(|i| i["type"] == "function_call_output").unwrap();
            assert_eq!(result["call_id"], call["call_id"]);
            assert_eq!(result["output"], "ok");
        }
    }).await.expect("JSON string replay finishes");
}

pub(crate) async fn verify(mut model: ModelInfo, options: StreamOptions) {
    tokio::time::timeout(Duration::from_secs(15), async {
        let provider = provider_for(&model.api).unwrap();
        let mut context = Context::new("Use the tools");
        context.tools = vec![
            ToolDefinition { name: "evaluate".into(), description: "Run source".into(),
                parameters: json!({}), input_format: Some(ToolInputFormat::Grammar {
                    syntax: "lark".into(), definition: GRAMMAR.into(),
                }) },
            ToolDefinition { name: "wait".into(), description: "Wait".into(),
                parameters: json!({"type":"object"}), input_format: None },
        ];
        let (url, request) = server(events(true, true)).await;
        model.base_url = url;
        let mut named_options = options.clone();
        named_options.tool_choice = Some(ToolChoice::Tool { name: "evaluate".into() });
        let message = provider.stream(&model, &context, &named_options).result().await;
        assert_eq!(message.stop_reason, StopReason::ToolUse, "{:?}", message.error);
        let tool_calls = calls(&message);
        assert_eq!(tool_calls.len(), 2);
        assert!(tool_calls[0].is_raw);
        assert!(!tool_calls[1].is_raw);
        assert_eq!(tool_calls[0].arguments, json!(SOURCE));
        assert_eq!(tool_calls[1].arguments, json!({"id":7}));
        let request = request.await.unwrap();
        assert_eq!(request["tools"][0], json!({"type":"custom", "name":"evaluate",
            "description":"Run source", "format":{"type":"grammar", "syntax":"lark", "definition":GRAMMAR}}));
        assert_eq!(request["tools"][1]["type"], "function");
        if model.api == "openai-responses" {
            assert_eq!(request["tool_choice"], json!({"type":"custom", "name":"evaluate"}));
        }

        context.messages.push(Message::Assistant(message.clone()));
        for call in tool_calls {
            context.messages.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: call.id.clone(), tool_name: call.name.clone(),
                content: vec![UserContent::Text(TextContent { text: "ok".into(), text_signature: None })],
                details: None, is_error: false, timestamp: 0,
            }));
        }
        // Persisted context is sufficient. No declaration or in-memory stream
        // state may be needed to recover a call's wire kind on resume.
        context = serde_json::from_slice(&serde_json::to_vec(&context).unwrap()).unwrap();
        context.tools.clear();
        for cross_model in [false, true] {
            if cross_model { model.id = "fallback-model".into(); }
            let (url, request) = server(events(true, false)).await;
            model.base_url = url;
            provider.stream(&model, &context, &options).result().await;
            let request = request.await.unwrap();
            let input = request["input"].as_array().unwrap();
            let call = input.iter().find(|i| i["type"] == "custom_tool_call").unwrap();
            assert_eq!(call["input"], SOURCE);
            assert_eq!(call["call_id"], "call_1");
            if cross_model { assert!(call.get("id").is_none()); }
            let result = input.iter().find(|i| i["type"] == "custom_tool_call_output").unwrap();
            assert_eq!(result["call_id"], call["call_id"]);
            assert_eq!(result["output"], "ok");
            assert!(input.iter().any(|i| i["type"] == "function_call_output"));
        }

        for input_done in [false, true] {
            let path = if model.api == "openai-codex-responses" { "POST /codex/responses" } else { "POST /responses" };
            let server = super::held_sse_server(path, events(false, input_done)).await;
            model.base_url = server.base_url.clone();
            let token = tokio_util::sync::CancellationToken::new();
            let mut options = options.clone();
            options.cancel = Some(token.clone());
            let mut stream = provider.stream(&model, &Context::new("sys"), &options);
            loop {
                match stream.next().await.unwrap() {
                    AssistantMessageEvent::ToolCallStart { content_index: 1, partial } => {
                        let expected = if input_done { format!("{SOURCE}// done\n") } else { SOURCE.into() };
                        assert_eq!(calls(&partial)[0].arguments, json!(expected));
                        token.cancel();
                        let terminal = stream.result().await;
                        assert_eq!(terminal.stop_reason, StopReason::Aborted);
                        assert!(calls(&terminal)[0].is_raw);
                        assert_eq!(calls(&terminal)[0].arguments, json!(expected));
                        break;
                    }
                    event => assert!(!event.is_terminal(), "{event:?}"),
                }
            }
            server.finish().await;
        }
    }).await.expect("raw-source provider boundary tests finish");
}

pub(crate) async fn verify_json_fallback(mut model: ModelInfo, options: StreamOptions) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let provider = provider_for(&model.api).unwrap();
        let mut context = Context::new("Continue");
        context.tools.push(ToolDefinition {
            name: "evaluate".into(),
            description: "Run source".into(),
            parameters: json!({}),
            input_format: Some(ToolInputFormat::Grammar {
                syntax: "lark".into(),
                definition: GRAMMAR.into(),
            }),
        });
        // Declarations are rejected before credential resolution or HTTP.
        let rejected = provider
            .stream(&model, &context, &StreamOptions::default())
            .result()
            .await;
        assert_eq!(
            rejected.error.unwrap().category,
            ErrorCategory::InvalidRequest
        );
        context.tools.clear();
        for is_raw in [true, false] {
            context.messages.clear();
            let mut prior = AssistantMessage::empty();
            prior.api = "openai-codex-responses".into();
            prior.provider = "openai-codex".into();
            prior.model = "source-model".into();
            prior.stop_reason = StopReason::ToolUse;
            prior.content.push(AssistantContent::ToolCall(ToolCall {
                is_raw,
                id: "call_1|ctc_1".into(),
                name: "evaluate".into(),
                arguments: json!(SOURCE),
            }));
            context.messages.push(Message::Assistant(prior));
            context
                .messages
                .push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call_1|ctc_1".into(),
                    tool_name: "evaluate".into(),
                    content: vec![UserContent::Text(TextContent {
                        text: "ok".into(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    timestamp: 0,
                }));
            let fixture = if model.api == "anthropic-messages" {
                include_str!("../../tests/roundtrip/fixtures/anthropic-messages/text_only.sse")
            } else {
                include_str!("../../tests/roundtrip/fixtures/openai-completions/text_only.sse")
            };
            let events = fixture
                .lines()
                .filter_map(|l| l.strip_prefix("data: ").map(str::to_owned))
                .collect();
            let (url, request) = server(events).await;
            model.base_url = url;
            let terminal = provider.stream(&model, &context, &options).result().await;
            assert_eq!(
                terminal.stop_reason,
                StopReason::Stop,
                "{:?}",
                terminal.error
            );
            let request = request.await.unwrap();
            let messages = request["messages"].as_array().unwrap();
            let assistant = messages.iter().find(|m| m["role"] == "assistant").unwrap();
            let expected = if is_raw {
                json!({"input":SOURCE})
            } else {
                json!(SOURCE)
            };
            if model.api == "anthropic-messages" {
                let call = assistant["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["type"] == "tool_use")
                    .unwrap();
                assert_eq!(call["input"], expected);
                let result = messages
                    .iter()
                    .flat_map(|m| m["content"].as_array().into_iter().flatten())
                    .find(|c| c["type"] == "tool_result")
                    .unwrap();
                assert_eq!(result["tool_use_id"], call["id"]);
            } else {
                let call = &assistant["tool_calls"][0];
                let arguments: Value =
                    serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
                assert_eq!(arguments, expected);
                let result = messages.iter().find(|m| m["role"] == "tool").unwrap();
                assert_eq!(result["tool_call_id"], call["id"]);
            }
        }
    })
    .await
    .expect("JSON fallback boundary test finishes");
}
