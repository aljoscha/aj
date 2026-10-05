//! Oracle contracts through the public application builder and real tool runtime.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::{Agent, AgentSeed};
use aj_app::host::{AttachRequest, Command, HostSetup, SessionHost, SettingsAxis, SettingsChange};
use aj_app::model::{ModelSelection, apply_thinking_display, resolve};
use aj_app::session_setup::{
    ModelConfig, RestoreContext, RunConfigDefaults, RunConfigSnapshot, build_agent,
};
use aj_app::settings::{ConfigLayers, PersistAction};
use aj_app::test_support::{finalized_text_message, scripted_model_info, scripted_run_config};
use aj_conf::{
    Config, ConfigLayer, ConfigSpeed, ConfigThinkingDisplay, ConfigThinkingLevel, ConfigVerbosity,
};
use aj_models::ThinkingConfig;
use aj_models::auth::{AuthCredential, AuthStorage};
use aj_models::provider::Provider;
use aj_models::registry::{
    Catalog, ModelCost, ModelInfo, ModelRegistry, OverridesFile, ReasoningOption, SpeedMode,
    supported_thinking_levels,
};
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::streaming::AssistantMessageEventStream;
use aj_models::types::{
    AssistantContent, AssistantMessage, Context, Message, ReasoningSummary, SimpleStreamOptions,
    Speed, StopReason, StreamOptions, ThinkingLevel, ToolCall, ToolResultMessage, UserContent,
    Verbosity,
};
use aj_session::ConversationPersistence;
use aj_wire::Frame;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const TASK: &str = "Inspect the retry invariant; return evidence only: oracle-task-47";
const REPORT: &str = "oracle-report-83: the retry loses the original deadline";
const HISTORY: &str = "parent-only-history-29 must not reach the advisor";

#[derive(Clone)]
struct Request {
    model: ModelInfo,
    context: Context,
    options: SimpleStreamOptions,
}

struct RecordingProvider {
    script: ScriptedProvider,
    requests: Mutex<Vec<Request>>,
}

impl RecordingProvider {
    fn new(messages: Vec<AssistantMessage>) -> Arc<Self> {
        Arc::new(Self {
            script: ScriptedProvider::from_messages(messages, 0, Duration::ZERO)
                .on_exhausted(ExhaustedBehavior::Panic),
            requests: Mutex::new(Vec::new()),
        })
    }
}

impl Provider for RecordingProvider {
    fn stream(&self, _: &ModelInfo, _: &Context, _: &StreamOptions) -> AssistantMessageEventStream {
        panic!("runtime must supply its thinking choice through stream_simple")
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.requests.lock().unwrap().push(Request {
            model: model.clone(),
            context: context.clone(),
            options: options.clone(),
        });
        self.script.stream_simple(model, context, options)
    }
}

fn consult() -> AssistantMessage {
    let mut message = finalized_text_message("consulting");
    message.content.push(AssistantContent::ToolCall(ToolCall {
        id: "oracle-call-61".into(),
        name: "oracle".into(),
        arguments: json!({"task": TASK}),
    }));
    message.stop_reason = StopReason::ToolUse;
    message
}

fn snapshot(main: Arc<RecordingProvider>, oracle: Arc<RecordingProvider>) -> RunConfigSnapshot {
    let mut run = scripted_run_config(vec![]).lock().unwrap().clone();
    run.main.provider = main;
    run.main.model_info = Arc::new(ModelInfo {
        id: "current-main".into(),
        base_url: "http://127.0.0.1:1/main-override-must-not-leak".into(),
        speed_modes: vec![],
        default_speed: None,
        reasoning: true,
        reasoning_options: vec![ReasoningOption::Effort {
            values: vec![ThinkingLevel::Low, ThinkingLevel::High],
        }],
        ..scripted_model_info()
    });
    run.main.thinking = Some(ThinkingConfig::Low);
    run.main.speed = Some(Speed::Standard);
    run.main.thinking_display = Some(ConfigThinkingDisplay::Detailed);
    run.main.stream_options.api_key = Some("synthetic-main-key".into());
    run.main.stream_options.verbosity = Some(Verbosity::Low);
    run.main.stream_options.speed = run.main.speed.clone();
    run.session_id = Some("oracle-test-session".into());
    run.main.stream_options.session_id = run.session_id.clone();
    apply_thinking_display(&mut run.main.stream_options, run.main.thinking_display);
    run.main.model_key = ("scripted".into(), "current-main".into());
    run.oracle = ModelConfig {
        provider: oracle,
        model_info: Arc::new(ModelInfo {
            id: "independent-oracle".into(),
            base_url: "http://127.0.0.1:1/oracle".into(),
            ..(*run.main.model_info).clone()
        }),
        stream_options: StreamOptions {
            api_key: Some("synthetic-oracle-key".into()),
            speed: Some(Speed::Standard),
            verbosity: Some(Verbosity::High),
            session_id: run.session_id.clone(),
            ..StreamOptions::default()
        },
        thinking: Some(ThinkingConfig::High),
        thinking_display: Some(ConfigThinkingDisplay::Omitted),
        speed: Some(Speed::Standard),
        model_key: ("scripted".into(), "independent-oracle".into()),
    };
    {
        let model = &mut run.oracle;
        apply_thinking_display(&mut model.stream_options, model.thinking_display);
    }
    run
}

fn build(config: &Config, run: &RunConfigSnapshot) -> Agent {
    let mut agent = build_agent(config, run).agent;
    agent.seed_session(AgentSeed {
        transcript: vec![],
        assembled_system_prompt: Some("Shared engineering instructions.".into()),
        sub_agent_counter: 0,
    });
    agent
}

async fn prompt(agent: &mut Agent, text: &str) {
    tokio::time::timeout(
        Duration::from_secs(10),
        agent.prompt(text.into(), CancellationToken::new()),
    )
    .await
    .expect("bounded composed turn")
    .expect("main turn succeeds");
}

fn oracle_result(context: &Context) -> &ToolResultMessage {
    context
        .messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_name == "oracle" => Some(result),
            _ => None,
        })
        .expect("parent inference receives a real oracle tool result")
}

#[tokio::test]
async fn main_and_oracle_require_resolvable_model_defaults() {
    let root = TempDir::new().unwrap();
    let auth = AuthStorage::with_providers(root.path().join("auth.json"), HashMap::new());
    let args = aj_app::cli::args::Args::try_parse_from(["aj"]).unwrap();
    for prefix in ["", "oracle_"] {
        for (key, value, valid) in [
            ("model_url", "https://advisor.example/v1", true),
            ("model_url", "not-a-url", false),
            ("model_url", "file:///tmp/advisor", false),
            ("model_api", "unknown-provider", false),
            ("model_name", "unknown-model", false),
        ] {
            let mut config = Config::default();
            Config::option(&format!("{prefix}{key}"))
                .unwrap()
                .apply_str(value, &mut config)
                .unwrap();
            let result = aj_app::session_setup::compose_host(
                &args,
                ConfigLayers {
                    user: config,
                    project: ConfigLayer::default(),
                    project_path: None,
                    writes: Default::default(),
                },
                &auth,
                &ConversationPersistence::new(root.path().join("sessions")),
                None,
            );
            if valid {
                let composed = result.unwrap();
                let session = composed.host.create().await.unwrap();
                let handles = composed.host.local_handles(&session).await.unwrap();
                {
                    let run = handles.run_config.lock().unwrap();
                    let model = if prefix.is_empty() {
                        &run.main
                    } else {
                        &run.oracle
                    };
                    assert_eq!(model.model_info.base_url, value);
                }
                composed.host.shutdown().await;
            } else {
                let error = match result {
                    Err(error) => format!("{error:#}"),
                    Ok(composed) => {
                        composed.host.shutdown().await;
                        panic!("invalid {prefix}{key} must fail startup");
                    }
                };
                assert!(error.contains(value), "{error}");
            }
        }
    }
}

#[tokio::test]
async fn oracle_credentials_are_checked_before_work_and_login_clears_the_warning() {
    let root = TempDir::new().unwrap();
    let mut server = MockResponses::start().await;
    let auth = AuthStorage::new(root.path().join("auth.json"));
    auth.set_runtime_api_key("anthropic", "main-test-key".into())
        .await;
    let args = aj_app::cli::args::Args::try_parse_from(["aj"]).unwrap();
    let composed = aj_app::session_setup::compose_host(
        &args,
        ConfigLayers {
            user: Config {
                oracle_model_api: Some("openai".into()),
                oracle_model_name: Some("gpt-5.5".into()),
                oracle_model_url: Some(server.url.clone()),
                ..Config::default()
            },
            project: ConfigLayer::default(),
            project_path: None,
            writes: Default::default(),
        },
        &auth,
        &ConversationPersistence::new(root.path().join("sessions")),
        None,
    )
    .unwrap();
    let session = composed.host.create().await.unwrap();
    let main = RecordingProvider::new(vec![consult(), finalized_text_message("main done")]);
    composed
        .host
        .local_handles(&session)
        .await
        .unwrap()
        .run_config
        .lock()
        .unwrap()
        .main
        .provider = Arc::<RecordingProvider>::clone(&main);
    for credentialed in [false, true] {
        if credentialed {
            auth.set_runtime_api_key("openai", "oracle-test-key".into())
                .await;
        }
        let mut stream = composed
            .host
            .attach(&[AttachRequest {
                session: session.clone(),
                cursor: None,
            }])
            .await
            .unwrap();
        let warned = tokio::time::timeout(Duration::from_secs(10), async {
            let mut warned = false;
            loop {
                match stream.recv().await.unwrap() {
                    Frame::Event { event, .. } => {
                        if let Some(AgentEvent::Warning { text, .. }) = event.known() {
                            warned |= text.starts_with("Oracle:") && text.contains("openai");
                        }
                    }
                    Frame::CaughtUp { .. } => return warned,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(warned, !credentialed);
        if credentialed {
            composed
                .host
                .command(
                    &session,
                    Command::Prompt {
                        agent: AgentId::Main,
                        content: vec![UserContent::text("consult Oracle after login")],
                    },
                )
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Frame::Event { event, .. } = stream.recv().await.unwrap()
                        && matches!(
                            event.known(),
                            Some(AgentEvent::AgentEnd {
                                agent_id: AgentId::Main,
                                ..
                            })
                        )
                    {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            let (headers, body) = tokio::time::timeout(Duration::from_secs(2), &mut server.request)
                .await
                .unwrap()
                .unwrap();
            assert!(
                headers
                    .to_lowercase()
                    .contains("authorization: bearer oracle-test-key")
            );
            assert_eq!(body["model"], "gpt-5.5");
            let calls = main.requests.lock().unwrap();
            assert_eq!(calls.len(), 2);
            let result = oracle_result(&calls[1].context);
            assert!(!result.is_error);
            assert!(
                serde_json::to_string(&result.content)
                    .unwrap()
                    .contains(REPORT)
            );
        }
    }
    composed.host.shutdown().await;
}

fn assert_parent(request: &Request) {
    assert_eq!(request.model.id, "current-main");
    assert!(
        request
            .model
            .base_url
            .ends_with("main-override-must-not-leak")
    );
    assert_eq!(request.options.reasoning, ThinkingLevel::Low);
    assert_eq!(request.options.base.speed, Some(Speed::Standard));
    assert_eq!(request.options.base.verbosity, Some(Verbosity::Low));
    assert_eq!(
        request.options.base.api_key.as_deref(),
        Some("synthetic-main-key")
    );
    assert_eq!(
        request.options.base.reasoning_summary,
        Some(ReasoningSummary::Detailed)
    );
    assert!(
        !request
            .context
            .system_prompt
            .as_ref()
            .unwrap()
            .contains("Oracle assignment")
    );
    for name in ["oracle", "agent", "edit_file", "write_file", "todo_write"] {
        assert!(
            request.context.tools.iter().any(|tool| tool.name == name),
            "parent lost {name}"
        );
    }
}

#[tokio::test]
async fn independent_oracle_bundle_runs_an_advisory_isolated_child() {
    let provider = RecordingProvider::new(vec![
        finalized_text_message("ordinary-before"),
        consult(),
        finalized_text_message("parent-assesses-report"),
        finalized_text_message("ordinary-after"),
    ]);
    let oracle = RecordingProvider::new(vec![finalized_text_message(REPORT)]);
    let run = snapshot(Arc::clone(&provider), Arc::clone(&oracle));
    // Tool construction consumes the resolved bundles, not startup defaults.
    let config = Config {
        model_name: Some("stale-startup-model".into()),
        thinking: Some(ConfigThinkingLevel::High),
        speed: Some(ConfigSpeed::Fast),
        verbosity: Some(ConfigVerbosity::High),
        thinking_display: Some(ConfigThinkingDisplay::Omitted),
        oracle_model_api: Some("unavailable-startup-provider".into()),
        oracle_model_name: Some("unavailable-startup-oracle".into()),
        oracle_thinking: Some(ConfigThinkingLevel::Max),
        oracle_verbosity: Some(ConfigVerbosity::Low),
        disabled_tools: vec!["todo_read".into()],
        ..Config::default()
    };
    let mut agent = build(&config, &run);
    prompt(&mut agent, HISTORY).await;
    prompt(&mut agent, "Ask Oracle now").await;
    prompt(&mut agent, "Do unrelated ordinary work").await;

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for index in 0..4 {
        assert_parent(&requests[index]);
    }
    assert!(
        serde_json::to_string(&requests[1].context.messages)
            .unwrap()
            .contains(HISTORY)
    );
    let oracle_requests = oracle.requests.lock().unwrap();
    assert_eq!(oracle_requests.len(), 1);
    let child = &oracle_requests[0];
    assert_eq!(child.model.id, "independent-oracle");
    assert_eq!(child.model.base_url, run.oracle.model_info.base_url);
    assert_eq!(child.options.reasoning, ThinkingLevel::High);
    assert_eq!(child.options.base.speed, Some(Speed::Standard));
    assert_eq!(child.options.base.verbosity, Some(Verbosity::High));
    assert_eq!(
        child.options.base.api_key,
        run.oracle.stream_options.api_key
    );
    assert_eq!(child.options.base.reasoning_summary, None);
    assert!(
        child
            .context
            .system_prompt
            .as_ref()
            .unwrap()
            .ends_with(aj_tools::tools::oracle::ORACLE_PROMPT)
    );
    let system = child.context.system_prompt.as_ref().unwrap();
    assert!(system.starts_with("Shared engineering instructions."));
    for instruction in [
        "Your role is advisory: do not edit",
        "commands for inspection, not modification",
        "Do not delegate",
        "self-contained report for the calling agent",
    ] {
        assert!(
            system.contains(instruction),
            "missing advisory instruction: {instruction}"
        );
    }
    assert_eq!(
        child.context.messages.len(),
        1,
        "child gets only its explicit task"
    );
    let messages = serde_json::to_string(&child.context.messages).unwrap();
    assert!(messages.contains(TASK));
    assert!(!messages.contains(HISTORY));
    for name in [
        "oracle",
        "agent",
        "apply_patch",
        "edit_file",
        "write_file",
        "todo_write",
        "todo_read",
    ] {
        assert!(
            !child.context.tools.iter().any(|tool| tool.name == name),
            "advisor exposed {name}"
        );
    }
    for name in ["read_file", "bash"] {
        assert!(
            child.context.tools.iter().any(|tool| tool.name == name),
            "advisor cannot inspect using {name}"
        );
    }
    let result = oracle_result(&requests[2].context);
    assert!(!result.is_error);
    assert!(
        serde_json::to_string(&result.content)
            .unwrap()
            .contains(REPORT)
    );
    assert!(
        serde_json::to_string(&requests[3].context.messages)
            .unwrap()
            .contains("parent-assesses-report")
    );
}

/// The test owns completion, so Main continuing cannot be explained by a fast advisor.
struct PausedOracle {
    stream: AssistantMessageEventStream,
    called: tokio::sync::Notify,
}

impl Provider for PausedOracle {
    fn stream(&self, _: &ModelInfo, _: &Context, _: &StreamOptions) -> AssistantMessageEventStream {
        panic!("thinking must reach stream_simple")
    }

    fn stream_simple(
        &self,
        _: &ModelInfo,
        _: &Context,
        _: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.called.notify_one();
        self.stream.clone()
    }
}

#[tokio::test]
async fn background_oracle_returns_before_completion_and_delivers_its_report_to_main() {
    let root = TempDir::new().unwrap();
    let mut call = consult();
    let AssistantContent::ToolCall(tool) = call.content.last_mut().unwrap() else {
        panic!("Oracle call")
    };
    tool.arguments["run_in_background"] = json!(true);
    let main = RecordingProvider::new(vec![
        call,
        finalized_text_message("main continues while Oracle works"),
        finalized_text_message("main assesses Oracle's report"),
    ]);
    let paused = Arc::new(PausedOracle {
        stream: AssistantMessageEventStream::new(),
        called: tokio::sync::Notify::new(),
    });
    let args = aj_app::cli::args::Args::try_parse_from(["aj"]).unwrap();
    let composed = aj_app::session_setup::compose_host(
        &args,
        ConfigLayers {
            user: Config::default(),
            project: ConfigLayer::default(),
            project_path: None,
            writes: Default::default(),
        },
        &AuthStorage::with_providers(root.path().join("auth.json"), HashMap::new()),
        &ConversationPersistence::new(root.path().join("sessions")),
        None,
    )
    .unwrap();
    let host = composed.host;
    let session = host.create().await.unwrap();
    let handles = host.local_handles(&session).await.unwrap();
    {
        let mut run = handles.run_config.lock().unwrap();
        run.main.provider = Arc::<RecordingProvider>::clone(&main);
        run.oracle.provider = Arc::<PausedOracle>::clone(&paused);
    }
    let mut attachment = host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .unwrap();
    while !matches!(attachment.recv().await.unwrap(), Frame::CaughtUp { .. }) {}
    host.command(
        &session,
        Command::Prompt {
            agent: AgentId::Main,
            content: vec![UserContent::text("Consult Oracle in the background")],
        },
    )
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(10), async {
        paused.called.notified().await;
        let mut child_started = false;
        loop {
            if let Frame::Event { event, .. } = attachment.recv().await.unwrap() {
                match event.known() {
                    Some(AgentEvent::SubAgentStart {
                        tool_name,
                        background,
                        ..
                    }) => {
                        assert_eq!(tool_name, "oracle");
                        assert!(*background);
                        child_started = true;
                    }
                    Some(AgentEvent::AgentEnd {
                        agent_id: AgentId::Main,
                        ..
                    }) => {
                        assert!(child_started);
                        break;
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("Main finishes its turn while Oracle is still blocked");
    {
        let requests = main.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let result = oracle_result(&requests[1].context);
        assert!(!result.is_error);
        let content = serde_json::to_string(&result.content).unwrap();
        assert!(content.contains("started in background"));
        assert!(!content.contains(REPORT));
    }

    for step in
        aj_models::scripted::script_from_message(finalized_text_message(REPORT), 0, Duration::ZERO)
            .steps
    {
        paused.stream.push(step.event);
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Frame::Event { event, .. } = attachment.recv().await.unwrap()
                && matches!(
                    event.known(),
                    Some(AgentEvent::AgentEnd {
                        agent_id: AgentId::Main,
                        ..
                    })
                )
            {
                break;
            }
        }
    })
    .await
    .expect("Oracle completion wakes Main without another user prompt");
    {
        let requests = main.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let messages = serde_json::to_string(&requests[2].context.messages).unwrap();
        assert!(messages.contains(REPORT));
        assert!(messages.contains("task-notification"));
    }
    host.shutdown().await;
}

// Bounded local HTTP fixture, owned even if an assertion unwinds. No global
// environment mutation or external credentials are needed by the real adapter.
struct MockResponses {
    url: String,
    request: tokio::sync::oneshot::Receiver<(String, Value)>,
    task: tokio::task::JoinHandle<()>,
    requests: tokio::sync::mpsc::UnboundedReceiver<(String, Value)>,
}

impl Drop for MockResponses {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockResponses {
    async fn start() -> Self {
        let response = json!({"id":"resp_oracle", "object":"response", "created_at":0.0,
            "model":"oracle-other", "output":[], "parallel_tool_calls":true, "tools":[], "status":"in_progress"});
        let mut completed = response.clone();
        completed["status"] = json!("completed");
        let events = [
            json!({"type":"response.created", "sequence_number":0, "response":response}),
            json!({"type":"response.output_item.added", "sequence_number":1, "output_index":0,
                "item":{"type":"message", "id":"msg_oracle", "content":[], "role":"assistant", "status":"in_progress"}}),
            json!({"type":"response.output_text.delta", "sequence_number":2, "item_id":"msg_oracle",
                "output_index":0, "content_index":0, "delta":REPORT}),
            json!({"type":"response.completed", "sequence_number":3, "response":completed}),
        ];
        Self::start_sse(
            events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect(),
        )
        .await
    }

    async fn start_anthropic(model: &str) -> Self {
        let events = [
            json!({"type":"message_start", "message":{"id":"msg_oracle", "type":"message",
                "role":"assistant", "content":[], "model":model, "stop_reason":null,
                "stop_sequence":null, "usage":{"input_tokens":12, "output_tokens":0}}}),
            json!({"type":"content_block_start", "index":0,
                "content_block":{"type":"text", "text":"", "citations":[]}}),
            json!({"type":"content_block_delta", "index":0,
                "delta":{"type":"text_delta", "text":REPORT}}),
            json!({"type":"content_block_stop", "index":0}),
            json!({"type":"message_delta", "delta":{"stop_reason":"end_turn", "stop_sequence":null},
                "usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ];
        Self::start_sse(
            events
                .iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().unwrap()
                    )
                })
                .collect(),
        )
        .await
    }

    async fn start_sse(response_body: String) -> Self {
        Self::start_sequence(vec![response_body]).await
    }

    async fn start_sequence(response_bodies: Vec<String>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/oracle-endpoint", listener.local_addr().unwrap());
        let (tx, request) = tokio::sync::oneshot::channel();
        let (requests_tx, requests) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut first = Some(tx);
            for response_body in response_bodies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let (headers, body) = loop {
                    let mut buffer = [0; 8192];
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0, "complete HTTP request");
                    bytes.extend_from_slice(&buffer[..count]);
                    let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .expect("request content length");
                    if bytes.len() >= end + 4 + length {
                        break (
                            headers,
                            serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + length])
                                .unwrap(),
                        );
                    }
                };
                if let Some(tx) = first.take() {
                    tx.send((headers.clone(), body.clone())).unwrap();
                }
                requests_tx.send((headers, body)).unwrap();
                socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}", response_body.len()).as_bytes()).await.unwrap();
            }
        });
        Self {
            url,
            request,
            task,
            requests,
        }
    }
}

async fn restore_context(root: &TempDir, url: String) -> RestoreContext {
    let auth = AuthStorage::with_providers(root.path().join("auth.json"), HashMap::new());
    for (provider, key) in [
        ("scripted", "synthetic-main-catalog-key"),
        ("oracle-provider", "synthetic-oracle-key"),
    ] {
        auth.insert_account(provider, "", AuthCredential::ApiKey { key: key.into() })
            .await
            .unwrap();
    }
    let model = ModelInfo {
        provider: "oracle-provider".into(),
        api: "openai-responses".into(),
        id: "oracle-other".into(),
        base_url: url,
        speed_modes: vec![],
        default_speed: None,
        reasoning: true,
        reasoning_options: vec![ReasoningOption::Effort {
            values: vec![ThinkingLevel::Low, ThinkingLevel::High],
        }],
        supports_verbosity: true,
        default_verbosity: None,
        max_tokens: 4096,
        context_window: 200_000,
        ..scripted_model_info()
    };
    RestoreContext {
        auth,
        registry: Arc::new(ModelRegistry::from_catalog_with_overrides(
            Catalog {
                schema_version: aj_models::registry::CATALOG_SCHEMA_VERSION,
                updated_at: 0,
                source: "oracle-test".into(),
                models: vec![model],
            },
            OverridesFile { overrides: vec![] },
            "oracle-test",
        )),
    }
}

#[tokio::test]
async fn explicit_other_provider_uses_its_endpoint_credentials_and_oracle_axes() {
    let mut server = MockResponses::start().await;
    let root = TempDir::new().unwrap();
    let context = restore_context(&root, server.url.clone()).await;
    let provider = RecordingProvider::new(vec![
        consult(),
        finalized_text_message("main-complete"),
        finalized_text_message("ordinary-after"),
    ]);
    let mut run = snapshot(Arc::clone(&provider), RecordingProvider::new(vec![]));
    let config = Config {
        oracle_model_api: Some("oracle-provider".into()),
        oracle_model_name: Some("oracle-other".into()),
        oracle_thinking: Some(ConfigThinkingLevel::High),
        speed: Some(ConfigSpeed::Standard),
        oracle_verbosity: Some(ConfigVerbosity::High),
        thinking_display: Some(ConfigThinkingDisplay::Omitted),
        ..Config::default()
    };
    let resolved = resolve(
        &context.registry,
        &context.auth,
        &ModelSelection {
            api: config.oracle_model_api.clone(),
            name: config.oracle_model_name.clone(),
            url: config.oracle_model_url.clone(),
        },
        run.main.speed.clone(),
    )
    .unwrap();
    run.oracle = ModelConfig {
        model_key: (
            resolved.model_info.provider.clone(),
            resolved.model_info.id.clone(),
        ),
        provider: resolved.provider,
        model_info: resolved.model_info,
        stream_options: resolved.stream_options,
        thinking: Some(ThinkingConfig::High),
        thinking_display: Some(ConfigThinkingDisplay::Detailed),
        speed: run.main.speed.clone(),
    };
    run.oracle.stream_options.verbosity = Some(Verbosity::High);
    {
        let model = &mut run.oracle;
        apply_thinking_display(&mut model.stream_options, model.thinking_display);
    }
    let mut agent = build(&config, &run);
    let (_subscription, mut events) = agent.subscribe_channel();
    prompt(&mut agent, HISTORY).await;
    prompt(&mut agent, "ordinary unrelated work").await;
    let (headers, body) = tokio::time::timeout(Duration::from_secs(2), &mut server.request)
        .await
        .unwrap()
        .unwrap();
    assert!(
        headers.starts_with("POST /oracle-endpoint/responses HTTP/1.1"),
        "{headers}"
    );
    assert!(
        headers
            .to_lowercase()
            .contains("authorization: bearer synthetic-oracle-key")
    );
    assert!(!headers.contains("synthetic-main"));
    assert_eq!(body["model"], "oracle-other");
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["reasoning"]["summary"], "detailed");
    assert_eq!(body["text"]["verbosity"], "high");
    assert!(body.to_string().contains(TASK));
    assert!(!body.to_string().contains(HISTORY));
    assert!(
        body.get("service_tier").is_none(),
        "tierless compatible Oracle: {body}"
    );
    let mut starts = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let AgentEvent::SubAgentStart {
            settings,
            background,
            ..
        } = event
        {
            assert!(!background);
            starts.push(settings);
        }
    }
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].provider, "oracle-provider");
    assert_eq!(starts[0].model_id, "oracle-other");
    assert_eq!(starts[0].thinking, "high");
    assert_eq!(starts[0].speed, "standard");
    assert_eq!(starts[0].verbosity, "high");
    assert_eq!(starts[0].thinking_display, "detailed");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        3,
        "only parent inference uses the injected main provider"
    );
    for request in requests.iter() {
        assert_parent(request);
    }
    let result = oracle_result(&requests[1].context);
    assert!(!result.is_error, "{result:?}");
    assert!(
        serde_json::to_string(&result.content)
            .unwrap()
            .contains(REPORT)
    );
}

#[tokio::test]
async fn retained_oracle_settings_use_child_identity_and_fall_back_for_unsupported_shared_speed() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let root = TempDir::new().unwrap();
        let mut server = MockResponses::start_anthropic("replacement-oracle").await;
        let mut run = snapshot(
            RecordingProvider::new(vec![]),
            RecordingProvider::new(vec![]),
        );
        let main_info = ModelInfo {
            api: "openai-responses".into(),
            speed_modes: vec![SpeedMode {
                speed: Speed::Fast,
                name: "Fast".into(),
                description: String::new(),
                wire_value: "priority".into(),
                cost: None,
            }],
            default_speed: None,
            context_window: 200_000,
            max_tokens: 4096,
            ..(*run.main.model_info).clone()
        };
        let initial_oracle = ModelInfo {
            provider: "oracle-provider".into(),
            api: "anthropic-messages".into(),
            reasoning_options: vec![ReasoningOption::Effort {
                values: vec![ThinkingLevel::High, ThinkingLevel::Max],
            }],
            context_window: 200_000,
            max_tokens: 4096,
            ..(*run.oracle.model_info).clone()
        };
        let replacement_oracle = ModelInfo {
            id: "replacement-oracle".into(),
            base_url: server.url.clone(),
            reasoning_options: vec![ReasoningOption::Effort {
                values: vec![ThinkingLevel::Low, ThinkingLevel::High],
            }],
            ..initial_oracle.clone()
        };
        // Log-derived settings need the same identity as the request, not the
        // generic scripted identity supplied by finalized_text_message.
        let identified = |mut message: AssistantMessage, model: &ModelInfo| {
            message.api = model.api.clone();
            message.provider = model.provider.clone();
            message.model = model.id.clone();
            message
        };
        let main = RecordingProvider::new(vec![
            identified(consult(), &main_info),
            identified(finalized_text_message("parent-finished"), &main_info),
        ]);
        let oracle = RecordingProvider::new(vec![identified(
            finalized_text_message(REPORT),
            &initial_oracle,
        )]);
        run.main.provider = Arc::<RecordingProvider>::clone(&main);
        run.main.model_info = Arc::new(main_info.clone());
        run.main.speed = Some(Speed::Fast);
        run.main.stream_options.speed = run.main.speed.clone();
        run.oracle.provider = Arc::<RecordingProvider>::clone(&oracle);
        run.oracle.model_info = Arc::new(initial_oracle.clone());
        run.oracle.model_key = (initial_oracle.provider.clone(), initial_oracle.id.clone());
        run.oracle.speed = run.main.speed.clone();
        run.oracle.stream_options.speed = run.oracle.speed.clone();

        assert_eq!(
            supported_thinking_levels(&main_info),
            vec![ThinkingLevel::Low, ThinkingLevel::High]
        );
        assert_eq!(
            supported_thinking_levels(&initial_oracle),
            vec![ThinkingLevel::Off, ThinkingLevel::High, ThinkingLevel::Max]
        );
        assert_eq!(
            supported_thinking_levels(&replacement_oracle),
            vec![ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High]
        );
        let catalog = vec![
            main_info.clone(),
            initial_oracle.clone(),
            replacement_oracle.clone(),
        ];
        let auth = AuthStorage::with_providers(root.path().join("auth.json"), HashMap::new());
        auth.insert_account(
            "oracle-provider",
            "",
            AuthCredential::ApiKey {
                key: "synthetic-oracle-key".into(),
            },
        )
        .await
        .unwrap();
        let restore = RestoreContext {
            auth: auth.clone(),
            registry: Arc::new(ModelRegistry::from_catalog_with_overrides(
                Catalog {
                    schema_version: aj_models::registry::CATALOG_SCHEMA_VERSION,
                    updated_at: 0,
                    source: "oracle-host-test".into(),
                    models: catalog.clone(),
                },
                OverridesFile { overrides: vec![] },
                "oracle-host-test",
            )),
        };
        let config = Config {
            spill_dir: Some(root.path().join("spill").to_string_lossy().into_owned()),
            ..Config::default()
        };
        let host = SessionHost::new(HostSetup {
            config: Arc::new(Mutex::new(config.clone())),
            layers: Arc::new(Mutex::new(ConfigLayers {
                writes: Default::default(),
                user: config,
                project: ConfigLayer::default(),
                project_path: None,
            })),
            catalog: Arc::new(catalog),
            defaults: RunConfigDefaults::fixed(run),
            restore: Some(restore),
            persistence: ConversationPersistence::new(root.path().join("sessions")),
            auth,
            working_directory: root.path().to_path_buf(),
            name: None,
            idle_grace: None,
            live_capacity: None,
            list_coalesce: None,
        })
        .unwrap();
        let session = host.create().await.unwrap();
        let mut attachment = host
            .attach(&[AttachRequest {
                session: session.clone(),
                cursor: None,
            }])
            .await
            .unwrap();
        while !matches!(attachment.recv().await.unwrap(), Frame::CaughtUp { .. }) {}
        host.command(
            &session,
            Command::Prompt {
                agent: AgentId::Main,
                content: vec![UserContent::text("Consult Oracle")],
            },
        )
        .await
        .unwrap();

        let mut child = None;
        let mut parent_ended = false;
        loop {
            match attachment.recv().await.unwrap() {
                Frame::Event { event, .. } => match event.known() {
                    Some(AgentEvent::SubAgentStart {
                        parent,
                        child: id,
                        settings,
                        ..
                    }) => {
                        assert_eq!(*parent, AgentId::Main);
                        assert!(matches!(id, AgentId::Sub(_)));
                        assert!(child.replace(*id).is_none(), "exactly one Oracle child");
                        assert_eq!(settings.provider, initial_oracle.provider);
                        assert_eq!(settings.model_id, initial_oracle.id);
                        assert_eq!(settings.thinking, "high");
                        assert_eq!(settings.speed, "fast");
                    }
                    Some(AgentEvent::AgentEnd {
                        agent_id: AgentId::Main,
                        ..
                    }) => parent_ended = true,
                    _ => {}
                },
                Frame::State { working: false, .. } if parent_ended => break,
                Frame::List { sessions, .. }
                    if parent_ended
                        && sessions
                            .iter()
                            .any(|item| item.id == session && !item.working) =>
                {
                    break;
                }
                _ => {}
            }
        }
        let child = child.expect("consultation retains its child");
        {
            let requests = main.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            for request in requests.iter() {
                assert_eq!(request.model.id, main_info.id);
                assert_eq!(request.options.base.speed, Some(Speed::Fast));
            }
            let result = oracle_result(&requests[1].context);
            assert!(!result.is_error, "{result:?}");
            assert!(
                serde_json::to_string(&result.content)
                    .unwrap()
                    .contains(REPORT)
            );
            let requests = oracle.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].model.id, initial_oracle.id);
            assert_eq!(requests[0].options.base.speed, Some(Speed::Fast));
        }

        // All three commands precede the child's next turn. Low is absent from
        // its initial vocabulary, so validation must see the staged model pick.
        for (axis, promise) in [
            (
                SettingsAxis::Thinking(Some(ThinkingConfig::Max)),
                "Max belongs to the child, not Main",
            ),
            (
                SettingsAxis::Model(replacement_oracle.clone()),
                "the child model can be replaced",
            ),
            (
                SettingsAxis::Thinking(Some(ThinkingConfig::Low)),
                "Low belongs to the staged replacement",
            ),
        ] {
            host.command(
                &session,
                Command::Settings(SettingsChange {
                    agent: child,
                    persist: PersistAction::None,
                    axis,
                }),
            )
            .await
            .expect(promise);
        }
        host.command(
            &session,
            Command::Prompt {
                agent: child,
                content: vec![UserContent::text("Recheck the Oracle evidence")],
            },
        )
        .await
        .unwrap();

        let (headers, body) = (&mut server.request).await.unwrap();
        assert!(
            headers.starts_with("POST /oracle-endpoint/v1/messages HTTP/1.1"),
            "{headers}"
        );
        assert!(
            headers
                .to_lowercase()
                .contains("x-api-key: synthetic-oracle-key")
        );
        assert_eq!(body["model"], replacement_oracle.id);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "low");
        // Anthropic encodes Standard by omitting both fast-mode signals.
        assert!(
            body.get("speed").is_none(),
            "unsupported shared fast speed must use standard: {body}"
        );
        assert!(!headers.to_lowercase().contains("fast-mode-"), "{headers}");
        let tools = body["tools"].as_array().expect("advisor tool definitions");
        for name in [
            "oracle",
            "agent",
            "apply_patch",
            "edit_file",
            "write_file",
            "todo_write",
        ] {
            assert!(
                !tools.iter().any(|tool| tool["name"] == name),
                "advisor exposed {name}"
            );
        }
        for name in ["read_file", "bash"] {
            assert!(
                tools.iter().any(|tool| tool["name"] == name),
                "advisor lost {name}"
            );
        }
        let mut replied = false;
        loop {
            let Frame::Event { event, .. } = attachment.recv().await.unwrap() else {
                continue;
            };
            match event.known() {
                Some(AgentEvent::MessageEnd {
                    agent_id, message, ..
                }) if *agent_id == child => {
                    if let Some(Message::Assistant(message)) = message.as_stored_wire() {
                        assert!(message.error.is_none(), "{message:?}");
                        assert_eq!(message.stop_reason, StopReason::Stop);
                        assert_eq!(message.model, replacement_oracle.id);
                        assert!(message.content.iter().any(|content| matches!(content,
                            AssistantContent::Text(text) if text.text == REPORT)));
                        replied = true;
                    }
                }
                Some(AgentEvent::AgentEnd { agent_id, .. }) if *agent_id == child && replied => {
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(main.requests.lock().unwrap().len(), 2);
        assert_eq!(oracle.requests.lock().unwrap().len(), 1);
        host.shutdown().await;
    })
    .await
    .expect("bounded retained Oracle host turn");
}

// Both adapters see the requested preference. Only advertised modes reach the wire.
#[tokio::test]
async fn shared_speed_survives_resume_and_resolves_independently_on_real_requests() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let root = TempDir::new().unwrap();
        let response = |tool: bool| {
            let item = if tool {
                json!({"type":"function_call", "id":"fc_speed", "call_id":"call_speed",
                    "name":"oracle", "arguments":json!({"task":TASK}).to_string(), "status":"completed"})
            } else {
                json!({"type":"message", "id":"msg_speed", "role":"assistant", "status":"completed",
                    "content":[{"type":"output_text", "text":REPORT, "annotations":[]}]})
            };
            let response = json!({"id":"resp_speed", "object":"response", "created_at":0.0,
                "model":"speed-main", "output":[], "parallel_tool_calls":true, "tools":[], "status":"in_progress"});
            let mut completed = response.clone();
            completed["status"] = json!("completed");
            completed["output"] = json!([item.clone()]);
            completed["usage"] = json!({"input_tokens":100, "output_tokens":10, "total_tokens":110,
                "input_tokens_details":{"cached_tokens":0}, "output_tokens_details":{"reasoning_tokens":0}});
            let events = [
                json!({"type":"response.created", "sequence_number":0, "response":response}),
                json!({"type":"response.output_item.added", "sequence_number":1, "output_index":0, "item":item}),
                json!({"type":"response.output_item.done", "sequence_number":2, "output_index":0, "item":item}),
                json!({"type":"response.completed", "sequence_number":3, "response":completed}),
            ];
            events.iter().map(|event| format!("data: {event}\n\n")).collect::<String>()
        };
        let mut main_server = MockResponses::start_sequence(
            (0..6).map(|index| response(index % 2 == 0)).collect(),
        ).await;
        let mut oracle_server = MockResponses::start_sequence(
            (0..3).map(|_| response(false).replace("speed-main", "speed-oracle")).collect(),
        ).await;
        let standard_cost = ModelCost { input: 2.0, output: 4.0, ..ModelCost::default() };
        let premium_cost = ModelCost { input: 10.0, output: 20.0, ..ModelCost::default() };
        let main_info = ModelInfo {
            provider: "openai".into(), api: "openai-responses".into(),
            id: "speed-main".into(), base_url: main_server.url.clone(),
            reasoning: false, reasoning_options: vec![],
            speed_modes: vec![SpeedMode {
                speed: Speed::Ultrafast, name: "Ultrafast".into(), description: String::new(),
                wire_value: "ultrafast".into(), cost: Some(premium_cost.clone()),
            }],
            default_speed: None, cost: standard_cost.clone(),
            max_tokens: 4096, context_window: 200_000,
            ..scripted_model_info()
        };
        let oracle_info = ModelInfo {
            id: "speed-oracle".into(), base_url: oracle_server.url.clone(),
            speed_modes: vec![], ..main_info.clone()
        };
        assert!(main_info.speed_mode(&Speed::Ultrafast).is_some());
        assert!(oracle_info.speed_mode(&Speed::Ultrafast).is_none());
        let catalog = vec![main_info.clone(), oracle_info.clone()];
        let auth = AuthStorage::with_providers(root.path().join("auth.json"), HashMap::new());
        auth.insert_account("openai", "", AuthCredential::ApiKey { key: "speed-test-key".into() })
            .await.unwrap();
        let restore = RestoreContext {
            auth: auth.clone(),
            registry: Arc::new(ModelRegistry::from_catalog_with_overrides(
                Catalog { schema_version: aj_models::registry::CATALOG_SCHEMA_VERSION, updated_at: 0,
                    source: "shared-speed-test".into(), models: catalog.clone() },
                OverridesFile { overrides: vec![] }, "shared-speed-test",
            )),
        };
        let mut run = scripted_run_config(vec![]).lock().unwrap().clone();
        let bundle = |info: ModelInfo| {
            let resolved = aj_app::model::from_model_info(&auth, info, Some(Speed::Ultrafast)).unwrap();
            ModelConfig {
                model_key: (resolved.model_info.provider.clone(), resolved.model_info.id.clone()),
                provider: resolved.provider, model_info: resolved.model_info,
                stream_options: resolved.stream_options, thinking: None, thinking_display: None,
                speed: Some(Speed::Ultrafast),
            }
        };
        run.main = bundle(main_info);
        run.oracle = bundle(oracle_info);
        let make_host = |default_speed: Speed| {
            let mut defaults = run.clone();
            for model in [&mut defaults.main, &mut defaults.oracle] {
                model.speed = Some(default_speed.clone());
                model.stream_options.speed = Some(default_speed.clone());
            }
            let config = Config { speed: Some(default_speed.as_str().parse().unwrap()),
                spill_dir: Some(root.path().join("spill").to_string_lossy().into_owned()),
                ..Config::default() };
            SessionHost::new(HostSetup {
                config: Arc::new(Mutex::new(config.clone())),
                layers: Arc::new(Mutex::new(ConfigLayers { user: config, project: ConfigLayer::default(),
                    project_path: None, writes: Default::default() })),
                catalog: Arc::new(catalog.clone()), defaults: RunConfigDefaults::fixed(defaults),
                restore: Some(restore.clone()), persistence: ConversationPersistence::new(root.path().join("sessions")),
                auth: auth.clone(), working_directory: root.path().to_path_buf(), name: None,
                idle_grace: None, live_capacity: None, list_coalesce: None,
            }).unwrap()
        };
        let mut host = make_host(Speed::Ultrafast);
        let session = host.create().await.unwrap();
        for phase in 0..3 {
            if phase == 1 {
                host.shutdown().await;
                host = make_host(Speed::Standard);
            }
            if phase == 2 {
                host.command(&session, Command::Settings(SettingsChange {
                    agent: AgentId::Main, persist: PersistAction::None,
                    axis: SettingsAxis::Speed(Some(Speed::Standard)),
                })).await.unwrap();
            }
            let expected = if phase == 2 { Speed::Standard } else { Speed::Ultrafast };
            let handles = host.local_handles(&session).await.unwrap();
            {
                let run = handles.run_config.lock().unwrap();
                for model in [&run.main, &run.oracle] {
                    assert_eq!(model.speed.as_ref(), Some(&expected));
                    assert_eq!(model.stream_options.speed.as_ref(), Some(&expected));
                }
            }
            {
                let log = handles.log.lock().await;
                assert_eq!(log.settings_at(log.head().unwrap()).speed.as_deref(), Some(expected.as_str()));
            }
            let mut attachment = host.attach(&[AttachRequest { session: session.clone(), cursor: None }]).await.unwrap();
            while !matches!(attachment.recv().await.unwrap(), Frame::CaughtUp { .. }) {}
            host.command(&session, Command::Prompt { agent: AgentId::Main,
                content: vec![UserContent::text("Consult Oracle about speed")] }).await.unwrap();
            let mut costs = Vec::new();
            loop {
                if let Frame::Event { event, .. } = attachment.recv().await.unwrap() {
                    match event.known() {
                        Some(AgentEvent::MessageEnd { agent_id, message, .. }) => {
                            if let Some(Message::Assistant(message)) = message.as_stored_wire() {
                                assert!(message.error.is_none(), "{message:?}");
                                assert_eq!((message.usage.input, message.usage.output), (100, 10));
                                costs.push((*agent_id, message.usage.cost.total));
                            }
                        }
                        Some(AgentEvent::AgentEnd { agent_id: AgentId::Main, .. }) => break,
                        _ => {}
                    }
                }
            }
            assert_eq!(costs.len(), 3, "two Main responses and one real Oracle response");
            for (agent, cost) in costs {
                let rates = if agent == AgentId::Main && phase != 2 { &premium_cost } else { &standard_cost };
                let expected_cost = (100.0 * rates.input + 10.0 * rates.output) / 1_000_000.0;
                assert!((cost - expected_cost).abs() < 1e-12, "{agent:?}: {cost} != {expected_cost}");
            }
            for _ in 0..2 {
                let (_, body) = main_server.requests.recv().await.unwrap();
                assert_eq!(body["model"], "speed-main");
                if phase == 2 { assert_eq!(body["service_tier"], "default"); }
                else { assert_eq!(body["service_tier"], "ultrafast"); }
            }
            let (_, body) = oracle_server.requests.recv().await.unwrap();
            assert_eq!(body["model"], "speed-oracle");
            assert_eq!(body["service_tier"], "default", "unsupported shared mode: {body}");
        }
        host.shutdown().await;
    }).await.expect("bounded shared-speed composed regression");
}
