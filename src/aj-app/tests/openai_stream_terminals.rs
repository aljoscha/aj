//! OpenAI terminal precedence through real HTTP, SDK, Agent, and persistence.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::events::AgentEvent;
use aj_app::session_setup::RunConfigSnapshot;
use aj_app::test_support::build_tagged_test_agent;
use aj_models::openai::{OpenAiCompletionsProvider, OpenAiResponsesProvider};
use aj_models::provider::Provider;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::types::{
    AssistantContent, AssistantMessage, ErrorCategory, Message, ServiceTier, StopReason,
    StreamOptions,
};
use aj_session::{ConversationEntryKind, ConversationLog, ConversationPersistence, replay};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct ResponseScript {
    path: &'static str,
    events: Vec<String>,
    short_body: bool,
}

struct CountingSseServer {
    base_url: String,
    requests: Arc<AtomicUsize>,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl CountingSseServer {
    async fn start(scripts: Vec<ResponseScript>) -> Self {
        assert!(!scripts.is_empty(), "fixture needs at least one response");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind counting SSE server");
        let address = listener.local_addr().expect("fixture address");
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&requests);
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => accepted,
                };
                let (mut socket, _) = accepted.expect("accept provider request");
                let index = counted.fetch_add(1, Ordering::SeqCst);
                let script = scripts
                    .get(index)
                    .unwrap_or_else(|| scripts.last().expect("checked non-empty"));
                let request = read_request(&mut socket).await;
                assert!(
                    request.starts_with(script.path),
                    "request did not target {}: {}",
                    script.path,
                    request.lines().next().unwrap_or_default(),
                );

                let body = script
                    .events
                    .iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .collect::<String>();
                let content_length = body.len() + usize::from(script.short_body);
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                             content-length: {content_length}\r\nconnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write response head");
                socket
                    .write_all(body.as_bytes())
                    .await
                    .expect("write SSE body");
            }
        });
        Self {
            base_url: format!("http://{address}"),
            requests,
            stop,
            task,
        }
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    async fn finish(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("counting server stops")
            .expect("counting server task");
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let read = socket
            .read(&mut buffer)
            .await
            .expect("read provider request");
        assert!(read > 0, "provider closed before sending its request");
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let header_end = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| {
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("numeric Content-Length")
                })
            })
            .unwrap_or(0);
        if request.len() >= header_end + content_length {
            return String::from_utf8_lossy(&request).into_owned();
        }
    }
}

fn model(api: &str, base_url: String) -> ModelInfo {
    ModelInfo {
        id: "gpt-test".into(),
        name: "GPT test".into(),
        family: None,
        api: api.into(),
        provider: "openai".into(),
        base_url,
        reasoning: false,
        reasoning_options: Vec::new(),
        supports_verbosity: false,
        input: vec![InputModality::Text],
        cost: ModelCost {
            input: 1.0,
            output: 2.0,
            cache_read: 0.1,
            cache_write: 0.0,
            tiers: Vec::new(),
        },
        context_window: 200_000,
        max_tokens: 16_000,
    }
}

fn run_config(provider: Arc<dyn Provider>, model: ModelInfo) -> Arc<Mutex<RunConfigSnapshot>> {
    Arc::new(Mutex::new(RunConfigSnapshot {
        provider,
        model_info: Arc::new(model),
        stream_options: StreamOptions {
            api_key: Some("fixture-key".into()),
            service_tier: Some(ServiceTier::Flex),
            ..StreamOptions::default()
        },
        thinking: None,
        thinking_display: None,
        speed: None,
        model_key: ("openai".into(), "gpt-test".into()),
        session_id: None,
    }))
}

fn response_prefix(text: &str) -> Vec<String> {
    [
        serde_json::json!({
            "type": "response.created", "sequence_number": 0,
            "response": {
                "id": "resp_1", "object": "response", "created_at": 0.0,
                "model": "gpt-test", "output": [], "parallel_tool_calls": true,
                "tools": [], "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.output_item.added", "sequence_number": 1,
            "output_index": 0,
            "item": {
                "type": "message", "id": "msg_1", "content": [],
                "role": "assistant", "status": "in_progress"
            }
        }),
        serde_json::json!({
            "type": "response.output_text.delta", "sequence_number": 2,
            "item_id": "msg_1", "output_index": 0, "content_index": 0,
            "delta": text
        }),
    ]
    .into_iter()
    .map(|event| event.to_string())
    .collect()
}

fn response_completed(text: &str, input: u64, output: u64) -> Vec<String> {
    let mut events = response_prefix(text);
    events.push(
        serde_json::json!({
            "type": "response.completed", "sequence_number": 3,
            "response": {
                "id": "resp_1", "object": "response", "created_at": 0.0,
                "model": "gpt-test", "output": [], "parallel_tool_calls": true,
                "tools": [], "status": "completed", "service_tier": "flex",
                "usage": {
                    "input_tokens": input, "output_tokens": output,
                    "total_tokens": input + output
                }
            }
        })
        .to_string(),
    );
    events
}

fn response_failed(text: &str, input: u64, output: u64) -> Vec<String> {
    let mut events = response_prefix(text);
    events.push(
        serde_json::json!({
            "type": "response.failed", "sequence_number": 3,
            "response": {
                "id": "resp_1", "object": "response", "created_at": 0.0,
                "model": "gpt-test", "output": [], "parallel_tool_calls": true,
                "tools": [], "status": "failed", "service_tier": "flex",
                "usage": {
                    "input_tokens": input, "output_tokens": output,
                    "total_tokens": input + output
                }
            }
        })
        .to_string(),
    );
    events
}

fn chat_completed(text: &str, usage: Option<(u64, u64)>) -> Vec<String> {
    let mut events = vec![
        serde_json::json!({
            "id": "chatcmpl_1", "object": "chat.completion.chunk", "created": 0,
            "model": "gpt-test", "choices": [{
                "index": 0, "delta": {"role": "assistant", "content": text},
                "finish_reason": null
            }]
        })
        .to_string(),
        serde_json::json!({
            "id": "chatcmpl_1", "object": "chat.completion.chunk", "created": 0,
            "model": "gpt-test", "choices": [{
                "index": 0, "delta": {}, "finish_reason": "stop"
            }]
        })
        .to_string(),
    ];
    if let Some((prompt_tokens, completion_tokens)) = usage {
        events.push(
            serde_json::json!({
                "id": "chatcmpl_1", "object": "chat.completion.chunk", "created": 0,
                "model": "gpt-test", "choices": [],
                "usage": {
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "total_tokens": prompt_tokens + completion_tokens
                }
            })
            .to_string(),
        );
    }
    events
}

fn persisted_assistants(log: &ConversationLog) -> Vec<AssistantMessage> {
    log.entries_in_order()
        .into_iter()
        .filter_map(|entry| match &entry.entry {
            ConversationEntryKind::Message { message } => match message.as_stored_wire() {
                Some(Message::Assistant(message)) => Some(message.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn message_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

fn event_counts(events: &[AgentEvent]) -> (usize, usize) {
    let retries = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::StreamRetry { .. }))
        .count();
    let usage = events
        .iter()
        .filter(|event| matches!(event, AgentEvent::UsageUpdate { .. }))
        .count();
    (retries, usage)
}

#[tokio::test]
async fn a_preterminal_responses_failure_retries_once_and_persists_its_partial() {
    let scripts = vec![
        ResponseScript {
            path: "POST /v1/responses",
            events: response_prefix("partial"),
            short_body: true,
        },
        ResponseScript {
            path: "POST /v1/responses",
            events: response_completed("recovered", 20, 5),
            short_body: false,
        },
    ];
    let server = CountingSseServer::start(scripts).await;
    let model = model("openai-responses", format!("{}/v1", server.base_url));
    let run_config = run_config(Arc::new(OpenAiResponsesProvider), model);
    let root = TempDir::new().expect("session root");
    let persistence = ConversationPersistence::new(root.path().join("sessions"));
    let (mut agent, log, _handle, mut frames) = build_tagged_test_agent(&persistence, &run_config);

    tokio::time::timeout(
        Duration::from_secs(10),
        agent.prompt("hello".into(), CancellationToken::new()),
    )
    .await
    .expect("retried turn completes")
    .expect("second response succeeds");
    assert_eq!(server.request_count(), 2);
    server.finish().await;

    let mut events = Vec::new();
    while let Ok(frame) = frames.try_recv() {
        events.push(frame.event);
    }
    assert_eq!(event_counts(&events), (1, 1));
    let log = log.lock().await;
    let messages = persisted_assistants(&log);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].stop_reason, StopReason::Error);
    assert_eq!(
        messages[0].error.as_ref().map(|error| error.category),
        Some(ErrorCategory::Transient)
    );
    assert_eq!(message_text(&messages[0]), "partial");
    assert!(messages[0].usage.incomplete);
    assert_eq!(messages[1].stop_reason, StopReason::Stop);
    assert_eq!(message_text(&messages[1]), "recovered");
    assert!(!messages[1].usage.incomplete);
    let stats = log.stats();
    assert_eq!(stats.assistant_messages, 2);
    assert!(
        stats.usage.incomplete,
        "durable stats count the failed attempt"
    );
    assert!(stats.usage_breakdown[0].usage.incomplete);
    assert_eq!(agent.accumulated_usage().total_tokens, 25);
    assert!(
        !agent.accumulated_usage().incomplete,
        "live accumulation still counts only the successful retry"
    );
}

#[tokio::test]
async fn a_chat_finish_before_body_failure_is_not_reissued() {
    let scripts = vec![
        ResponseScript {
            path: "POST /v1/chat/completions",
            events: chat_completed("complete", None),
            short_body: true,
        },
        ResponseScript {
            path: "POST /v1/chat/completions",
            events: chat_completed("duplicate", Some((20, 5))),
            short_body: false,
        },
    ];
    let server = CountingSseServer::start(scripts).await;
    let model = model("openai-completions", format!("{}/v1", server.base_url));
    let run_config = run_config(Arc::new(OpenAiCompletionsProvider), model);
    let root = TempDir::new().expect("session root");
    let persistence = ConversationPersistence::new(root.path().join("sessions"));
    let (mut agent, log, _handle, mut frames) = build_tagged_test_agent(&persistence, &run_config);

    tokio::time::timeout(
        Duration::from_secs(10),
        agent.prompt("hello".into(), CancellationToken::new()),
    )
    .await
    .expect("completed Chat turn returns")
    .expect("transport tail cannot replace finish_reason");
    assert_eq!(server.request_count(), 1);
    server.finish().await;

    let mut events = Vec::new();
    while let Ok(frame) = frames.try_recv() {
        events.push(frame.event);
    }
    assert_eq!(event_counts(&events), (0, 1));
    let message_end = events.iter().find_map(|event| match event {
        AgentEvent::MessageEnd { message, .. } => match message.as_stored_wire() {
            Some(Message::Assistant(message)) => Some(message),
            _ => None,
        },
        _ => None,
    });
    let message_end =
        message_end.expect("the successful turn emitted its authoritative MessageEnd");
    assert!(message_end.usage.incomplete);
    let usage_update = events.iter().find_map(|event| match event {
        AgentEvent::UsageUpdate { usage, .. } => Some(usage),
        _ => None,
    });
    let usage_update = usage_update.expect("the successful turn emitted usage");
    assert!(usage_update.turn_incomplete);
    assert!(!usage_update.accumulated_incomplete);
    assert!(agent.accumulated_usage().incomplete);
    let log = log.lock().await;
    let messages = persisted_assistants(&log);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].stop_reason, StopReason::Stop);
    assert_eq!(message_text(&messages[0]), "complete");
    assert!(messages[0].usage.incomplete);
    let stats = log.stats();
    assert_eq!(stats.assistant_messages, 1);
    assert!(stats.usage.incomplete);
    assert!(stats.usage_breakdown[0].usage.incomplete);
    let session_id = log.session_id().to_string();
    drop(log);

    let reloaded = ConversationLog::resume(&persistence, &session_id).expect("reload session");
    assert!(persisted_assistants(&reloaded)[0].usage.incomplete);
    let replayed = replay(&reloaded).find_map(|event| match event {
        AgentEvent::UsageUpdate { usage, .. } => Some(usage),
        _ => None,
    });
    let replayed = replayed.expect("reload replays successful usage");
    assert!(replayed.turn_incomplete);
    assert!(!replayed.accumulated_incomplete);
    assert!(reloaded.stats().usage.incomplete);
}

#[tokio::test]
async fn a_chat_reported_zero_is_complete_across_live_and_durable_views() {
    let server = CountingSseServer::start(vec![ResponseScript {
        path: "POST /v1/chat/completions",
        events: chat_completed("complete", Some((0, 0))),
        short_body: false,
    }])
    .await;
    let model = model("openai-completions", format!("{}/v1", server.base_url));
    let run_config = run_config(Arc::new(OpenAiCompletionsProvider), model);
    let root = TempDir::new().expect("session root");
    let persistence = ConversationPersistence::new(root.path().join("sessions"));
    let (mut agent, log, _handle, mut frames) = build_tagged_test_agent(&persistence, &run_config);

    tokio::time::timeout(
        Duration::from_secs(10),
        agent.prompt("hello".into(), CancellationToken::new()),
    )
    .await
    .expect("reported-zero Chat turn returns")
    .expect("reported zero is a successful response");
    assert_eq!(server.request_count(), 1);
    server.finish().await;

    let mut events = Vec::new();
    while let Ok(frame) = frames.try_recv() {
        events.push(frame.event);
    }
    assert_eq!(event_counts(&events), (0, 1));
    let usage_update = events.iter().find_map(|event| match event {
        AgentEvent::UsageUpdate { usage, .. } => Some(usage),
        _ => None,
    });
    let usage_update = usage_update.expect("reported-zero turn emitted usage");
    assert!(!usage_update.turn_incomplete);
    assert!(!usage_update.accumulated_incomplete);
    assert!(!agent.accumulated_usage().incomplete);
    assert_eq!(agent.accumulated_usage().total_tokens, 0);

    let log = log.lock().await;
    let messages = persisted_assistants(&log);
    assert_eq!(messages.len(), 1);
    assert!(!messages[0].usage.incomplete);
    let stats = log.stats();
    assert!(!stats.usage.incomplete);
    assert!(!stats.usage_breakdown[0].usage.incomplete);
    let session_id = log.session_id().to_string();
    drop(log);

    let reloaded = ConversationLog::resume(&persistence, &session_id).expect("reload session");
    assert!(!persisted_assistants(&reloaded)[0].usage.incomplete);
    let replayed = replay(&reloaded).find_map(|event| match event {
        AgentEvent::UsageUpdate { usage, .. } => Some(usage),
        _ => None,
    });
    let replayed = replayed.expect("reload replays reported-zero usage");
    assert!(!replayed.turn_incomplete);
    assert!(!replayed.accumulated_incomplete);
}

#[tokio::test]
async fn a_responses_completion_before_body_failure_is_not_reissued() {
    let scripts = vec![
        ResponseScript {
            path: "POST /v1/responses",
            events: response_completed("complete", 20, 5),
            short_body: true,
        },
        ResponseScript {
            path: "POST /v1/responses",
            events: response_completed("duplicate", 20, 5),
            short_body: false,
        },
    ];
    let server = CountingSseServer::start(scripts).await;
    let model = model("openai-responses", format!("{}/v1", server.base_url));
    let run_config = run_config(Arc::new(OpenAiResponsesProvider), model);
    let root = TempDir::new().expect("session root");
    let persistence = ConversationPersistence::new(root.path().join("sessions"));
    let (mut agent, log, _handle, mut frames) = build_tagged_test_agent(&persistence, &run_config);

    tokio::time::timeout(
        Duration::from_secs(10),
        agent.prompt("hello".into(), CancellationToken::new()),
    )
    .await
    .expect("completed Responses turn returns")
    .expect("transport tail cannot replace response.completed");
    assert_eq!(server.request_count(), 1);
    server.finish().await;

    let mut events = Vec::new();
    while let Ok(frame) = frames.try_recv() {
        events.push(frame.event);
    }
    assert_eq!(event_counts(&events), (0, 1));
    let log = log.lock().await;
    let messages = persisted_assistants(&log);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].stop_reason, StopReason::Stop);
    assert_eq!(message_text(&messages[0]), "complete");
    assert_eq!(messages[0].usage.total_tokens, 25);
    assert_eq!(log.stats().assistant_messages, 1);
    assert_eq!(log.stats().usage.total_tokens, 25);
}

#[tokio::test]
async fn a_retryable_provider_terminal_persists_spend_but_accumulates_only_success() {
    let scripts = vec![
        ResponseScript {
            path: "POST /v1/responses",
            events: response_failed("failed", 10, 4),
            short_body: false,
        },
        ResponseScript {
            path: "POST /v1/responses",
            events: response_completed("recovered", 20, 5),
            short_body: false,
        },
    ];
    let server = CountingSseServer::start(scripts).await;
    let model = model("openai-responses", format!("{}/v1", server.base_url));
    let run_config = run_config(Arc::new(OpenAiResponsesProvider), model);
    let root = TempDir::new().expect("session root");
    let persistence = ConversationPersistence::new(root.path().join("sessions"));
    let (mut agent, log, _handle, mut frames) = build_tagged_test_agent(&persistence, &run_config);

    tokio::time::timeout(
        Duration::from_secs(10),
        agent.prompt("hello".into(), CancellationToken::new()),
    )
    .await
    .expect("retryable provider terminal retries")
    .expect("second response succeeds");
    assert_eq!(server.request_count(), 2);
    server.finish().await;

    let mut events = Vec::new();
    while let Ok(frame) = frames.try_recv() {
        events.push(frame.event);
    }
    assert_eq!(event_counts(&events), (1, 1));
    let accumulated = agent.accumulated_usage();
    assert_eq!((accumulated.input, accumulated.output), (20, 5));
    assert_eq!(accumulated.total_tokens, 25);
    assert!((accumulated.cost.total - 0.000_015).abs() < 1e-12);

    let log = log.lock().await;
    let messages = persisted_assistants(&log);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].stop_reason, StopReason::Error);
    assert_eq!(message_text(&messages[0]), "failed");
    assert_eq!(messages[0].usage.total_tokens, 14);
    assert!(!messages[0].usage.incomplete);
    assert!((messages[0].usage.cost.total - 0.000_009).abs() < 1e-12);
    assert_eq!(messages[1].stop_reason, StopReason::Stop);
    assert_eq!(messages[1].usage.total_tokens, 25);
    assert!(!messages[1].usage.incomplete);

    let stats = log.stats();
    assert_eq!(stats.assistant_messages, 2);
    assert_eq!((stats.usage.input, stats.usage.output), (30, 9));
    assert_eq!(stats.usage.total_tokens, 39);
    assert!((stats.usage.cost.total - 0.000_024).abs() < 1e-12);
    assert_eq!(stats.usage_breakdown.len(), 1);
    assert_eq!(stats.usage_breakdown[0].responses, 2);
    assert!(!stats.usage.incomplete);
}
