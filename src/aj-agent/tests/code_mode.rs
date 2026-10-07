use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::bus::SubscriptionHandle;
use aj_agent::events::AgentEvent;
use aj_agent::hooks::BeforeToolCallOutcome;
use aj_agent::message::AgentMessageKind;
use aj_agent::tool::{
    ErasedToolDefinition, TaskKind, TaskStatus, ToolContext, ToolDefinition, ToolDetails,
    ToolOutcome,
};
use aj_agent::{Agent, BoxError, TaskRegistry};
use aj_models::provider::Provider;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::streaming::AssistantMessageEventStream;
use aj_models::types::{
    AssistantContent, AssistantMessage, Context, Message, SimpleStreamOptions, StopReason,
    StreamOptions, ToolCall, ToolInputFormat, UserContent,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// Deadlines diagnose a stuck test. Ordering and progress are established by latches.
async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), future)
        .await
        .expect("Code Mode operation did not finish")
}

#[derive(Default)]
struct RecordingProvider {
    replies: Mutex<VecDeque<AssistantMessage>>,
    contexts: Mutex<Vec<Context>>,
    release_at_inference: Mutex<Option<CancellationToken>>,
}

impl RecordingProvider {
    fn enqueue(&self, name: &str, arguments: Value) {
        let mut reply = AssistantMessage::empty();
        reply.stop_reason = StopReason::ToolUse;
        reply.content.push(AssistantContent::ToolCall(ToolCall {
            is_raw: name == "exec",
            id: format!("call-{}", rand::random::<u64>()),
            name: name.into(),
            arguments,
        }));
        self.replies.lock().unwrap().push_back(reply);
    }

    fn done(&self) {
        let mut reply = AssistantMessage::empty();
        reply.stop_reason = StopReason::Stop;
        self.replies.lock().unwrap().push_back(reply);
    }
}

impl Provider for RecordingProvider {
    fn stream(&self, _: &ModelInfo, _: &Context, _: &StreamOptions) -> AssistantMessageEventStream {
        panic!("Agent must call stream_simple")
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.contexts.lock().unwrap().push(context.clone());
        if let Some(release) = self.release_at_inference.lock().unwrap().take() {
            release.cancel();
        }
        let mut reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected inference");
        reply.api.clone_from(&model.api);
        reply.provider.clone_from(&model.provider);
        reply.model.clone_from(&model.id);
        ScriptedProvider::from_messages(vec![reply], 1024, Duration::ZERO)
            .on_exhausted(ExhaustedBehavior::Panic)
            .stream_simple(model, context, options)
    }
}

fn model(eligible: bool) -> ModelInfo {
    ModelInfo {
        id: if eligible {
            "gpt-6-astra"
        } else {
            "unsupported-test-model"
        }
        .into(),
        name: "Code Mode integration".into(),
        family: None,
        api: "openai-responses".into(),
        provider: "openai".into(),
        base_url: "scripted://internal".into(),
        reasoning: false,
        reasoning_options: vec![],
        supports_verbosity: false,
        default_verbosity: None,
        speed_modes: vec![],
        default_speed: None,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 128_000,
        max_tokens: 4096,
    }
}

struct Harness {
    agent: Agent,
    provider: Arc<RecordingProvider>,
    events: Arc<Mutex<Vec<AgentEvent>>>,
    registry: TaskRegistry,
    _subscription: SubscriptionHandle,
}

impl Harness {
    fn new(eligible: bool, tools: Vec<ErasedToolDefinition>) -> Self {
        let provider = Arc::new(RecordingProvider::default());
        let mut agent = Agent::with_provider(
            std::env::current_dir().unwrap(),
            tools,
            vec![],
            Arc::<RecordingProvider>::clone(&provider),
            Arc::new(model(eligible)),
            StreamOptions::default(),
            None,
        );
        agent.set_code_mode(true);
        let registry = TaskRegistry::default();
        agent.set_task_registry(registry.clone());
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let subscription = agent.subscribe(Arc::new(move |event| {
            sink.lock().unwrap().push(event.clone());
            Box::pin(async { Ok(()) })
        }));
        Self {
            agent,
            provider,
            events,
            registry,
            _subscription: subscription,
        }
    }

    async fn prompt(&mut self) {
        bounded(
            self.agent
                .prompt("exercise code mode".into(), CancellationToken::new()),
        )
        .await
        .unwrap();
    }

    async fn exec(&mut self, source: &str) {
        self.provider.enqueue("exec", json!(source));
        self.provider.done();
        self.prompt().await;
    }

    fn result(&self, name: &str) -> String {
        self.agent
            .messages()
            .iter()
            .rev()
            .find_map(|message| match message.as_stored_wire() {
                Some(Message::ToolResult(result)) if result.tool_name == name => {
                    Some(text(&result.content))
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {name} result"))
    }

    fn cell(&self) -> (usize, String) {
        let tasks = self.registry.snapshot();
        assert_eq!(tasks.len(), 1, "one live script must register one task");
        let TaskKind::CodeMode { cell_id } = &tasks[0].kind else {
            panic!("expected a Code Mode task")
        };
        assert_eq!(tasks[0].status, TaskStatus::Running);
        (tasks[0].id, cell_id.clone())
    }

    async fn collect(&mut self, cell_id: &str) {
        self.provider.enqueue("wait", json!({"cell_id": cell_id}));
        self.provider.done();
        self.prompt().await;
        bounded(self.registry.wait_for_quiescence()).await;
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.registry.shutdown();
    }
}

fn text(content: &[UserContent]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            UserContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn outcome(value: &str, is_error: bool) -> ToolOutcome {
    ToolOutcome {
        structured_content: Some(json!({"value": value})),
        content: vec![UserContent::text(value)],
        details: ToolDetails::Text {
            summary: value.into(),
            body: value.into(),
        },
        is_error,
    }
}

#[derive(Deserialize, JsonSchema)]
struct EchoInput {
    value: String,
}

#[derive(Clone, Default)]
struct Echo(Arc<Mutex<Vec<String>>>);

impl ToolDefinition for Echo {
    type Input = EchoInput;
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "Echo a value for composition"
    }
    fn output_schema(&self) -> Option<Value> {
        Some(
            json!({"type":"object", "properties":{"value":{"type":"string"}}, "required":["value"]}),
        )
    }
    async fn execute(
        &self,
        _: &mut dyn ToolContext,
        input: EchoInput,
    ) -> Result<ToolOutcome, BoxError> {
        self.0.lock().unwrap().push(input.value.clone());
        if input.value == "throw" {
            return Err("nested failure".into());
        }
        Ok(outcome(&input.value, false))
    }
}

#[derive(Clone, Default)]
struct Gate {
    entered: CancellationToken,
    release: CancellationToken,
    dropped: CancellationToken,
    finished: CancellationToken,
}

impl ToolDefinition for Gate {
    type Input = Value;
    fn name(&self) -> &'static str {
        "gate"
    }
    fn description(&self) -> &'static str {
        "Wait for the test's explicit release"
    }
    async fn execute(&self, _: &mut dyn ToolContext, _: Value) -> Result<ToolOutcome, BoxError> {
        let _drop = self.dropped.clone().drop_guard();
        self.entered.cancel();
        self.release.cancelled().await;
        self.finished.cancel();
        Ok(outcome("released", false))
    }
}

#[tokio::test]
async fn eligible_catalog_exposes_raw_exec_and_json_wait_with_js_discovery() {
    let echo = Echo::default();
    let mut h = Harness::new(true, vec![echo.clone().into()]);
    h.exec(
        r#"
const entry = ALL_TOOLS.find(t => t.name === "echo");
text(entry);
text(await tools[entry.name]({value: "native-js-result"}));
"#,
    )
    .await;
    let contexts = h.provider.contexts.lock().unwrap();
    assert_eq!(contexts.len(), 2);
    let tools = &contexts[0].tools;
    assert_eq!(
        tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["exec", "wait"]
    );
    assert!(matches!(
        tools[0].input_format,
        Some(ToolInputFormat::Grammar { .. })
    ));
    assert!(tools[1].input_format.is_none());
    assert_eq!(
        tools[1].parameters["properties"]["cell_id"]["type"],
        "string"
    );
    let result = h.result("exec");
    assert!(result.contains("Script completed"), "{result}");
    assert!(
        result.contains("value: string"),
        "JS discovery must expose schema: {result}"
    );
    assert!(result.contains("native-js-result"), "{result}");
    assert_eq!(*echo.0.lock().unwrap(), ["native-js-result"]);
}

#[tokio::test]
async fn unsupported_model_notices_and_executes_ordinary_tools() {
    let echo = Echo::default();
    let mut h = Harness::new(false, vec![echo.clone().into()]);
    h.provider.enqueue("echo", json!({"value":"fallback"}));
    h.provider.done();
    h.prompt().await;
    let contexts = h.provider.contexts.lock().unwrap();
    assert_eq!(contexts[0].tools.len(), 1);
    assert_eq!(contexts[0].tools[0].name, "echo");
    assert!(contexts[0].tools[0].input_format.is_none());
    assert_eq!(*echo.0.lock().unwrap(), ["fallback"]);
    assert!(h.events.lock().unwrap().iter().any(|event| matches!(event,
        AgentEvent::Notice { text, .. } if text.contains("Code Mode")
    )));
}

#[tokio::test]
async fn nested_hooks_and_errors_are_durable_activity_not_provider_history() {
    let echo = Echo::default();
    let mut h = Harness::new(true, vec![echo.clone().into()]);
    h.agent.set_before_tool_call(Some(Arc::new(|ctx, mut args| {
        Box::pin(async move {
            if ctx.tool_name == "echo" {
                if args["value"] == "deny" {
                    return BeforeToolCallOutcome::ShortCircuit {
                        outcome: outcome("policy denied", true),
                    };
                }
                if args["value"] == "rewrite" {
                    args["value"] = json!("private-intermediate");
                }
            }
            BeforeToolCallOutcome::Proceed { args }
        })
    })));
    h.agent.set_after_tool_call(Some(Arc::new(|ctx, result| {
        Box::pin(async move {
            if ctx.tool_name == "echo" && !result.is_error {
                result.structured_content = Some(json!({"value":"after-hook"}));
            }
        })
    })));
    h.exec(
        r#"
const result = await tools.echo({value: "rewrite"});
if (result.value !== "after-hook") throw Error("after hook bypassed");
for (const [value, expected] of [["deny", "policy denied"], ["throw", "nested failure"]]) {
  let caught = false;
  try { await tools.echo({value}); } catch (e) {
    if (!String(e).includes(expected)) throw e;
    caught = true;
  }
  if (!caught) throw Error("nested error did not reject");
}
text("hooks-and-errors-verified");
"#,
    )
    .await;
    assert!(
        h.result("exec").contains("hooks-and-errors-verified"),
        "{}",
        h.result("exec")
    );
    assert_eq!(*echo.0.lock().unwrap(), ["private-intermediate", "throw"]);
    let events = h.events.lock().unwrap();
    let activity: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd { message, .. } => match &message.kind {
                AgentMessageKind::ToolActivity(activity) => Some(activity),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        activity.len(),
        6,
        "each nested call needs a durable call/result pair"
    );
    let expected_results = [
        ("private-intermediate", false),
        ("policy denied", true),
        ("nested failure", true),
    ];
    for (pair, (expected, is_error)) in activity.chunks_exact(2).zip(expected_results) {
        assert_eq!(pair[0].cell_id, pair[1].cell_id);
        let Message::Assistant(call) = &pair[0].message else {
            panic!("missing activity call")
        };
        let AssistantContent::ToolCall(call) = &call.content[0] else {
            panic!("missing tool call")
        };
        let Message::ToolResult(result) = &pair[1].message else {
            panic!("missing activity result")
        };
        assert_eq!(call.id, result.tool_call_id);
        assert_eq!(result.tool_name, "echo");
        assert_eq!(result.is_error, is_error);
        assert!(text(&result.content).contains(expected));
    }
    let contexts = h.provider.contexts.lock().unwrap();
    assert_eq!(contexts.len(), 2);
    for message in &contexts[1].messages {
        match message {
            Message::ToolResult(result) => assert_eq!(result.tool_name, "exec"),
            Message::Assistant(message) => {
                for block in &message.content {
                    if let AssistantContent::ToolCall(call) = block {
                        assert_eq!(call.name, "exec");
                    }
                }
            }
            _ => {}
        }
    }
    assert!(
        !serde_json::to_string(&contexts[1])
            .unwrap()
            .contains("private-intermediate")
    );
}

#[tokio::test]
async fn yielded_cell_survives_prompt_completion_and_preserves_store() {
    let gate = Gate::default();
    let mut h = Harness::new(true, vec![gate.clone().into()]);
    h.exec(
        r#"store("saved", {count: 40}); yield_control(); await tools.gate({});
store("saved", {count: load("saved").count + 2}); text("collected-live-cell");"#,
    )
    .await;
    bounded(gate.entered.cancelled()).await;
    assert!(
        !gate.dropped.is_cancelled(),
        "prompt completion killed pending work"
    );
    let (task, cell_id) = h.cell();
    assert!(h.result("exec").contains(&cell_id));
    gate.release.cancel();
    h.collect(&cell_id).await;
    assert!(
        h.result("wait").contains("collected-live-cell"),
        "{}",
        h.result("wait")
    );
    assert_eq!(h.registry.status(task), Some(TaskStatus::Exited(Some(0))));
    h.exec(r#"text(load("saved").count);"#).await;
    assert_eq!(h.result("exec"), "Script completed\n42");
}

#[tokio::test]
async fn removing_tool_between_prompts_revokes_dispatch_from_live_cell() {
    let gate = Gate::default();
    let echo = Echo::default();
    let mut h = Harness::new(true, vec![gate.clone().into(), echo.clone().into()]);
    h.exec(
        r#"yield_control(); await tools.gate({});
try { await tools.echo({value: "forbidden"}); text("revocation bypassed"); }
catch (e) { text(String(e)); }"#,
    )
    .await;
    bounded(gate.entered.cancelled()).await;
    let (_, cell_id) = h.cell();
    h.agent.set_tools(vec![gate.clone().into()]);
    // Release only after the next prompt has installed its dispatch policy.
    *h.provider.release_at_inference.lock().unwrap() = Some(gate.release.clone());
    h.collect(&cell_id).await;
    assert!(
        echo.0.lock().unwrap().is_empty(),
        "a removed tool was dispatched"
    );
    let result = h.result("wait");
    assert!(result.contains("not available"), "{result}");
    assert!(!result.contains("revocation bypassed"), "{result}");
}

#[tokio::test]
async fn registry_stop_cancels_pending_nested_tool_and_quiesces() {
    let gate = Gate::default();
    let mut h = Harness::new(true, vec![gate.clone().into()]);
    h.exec("yield_control(); await tools.gate({});").await;
    bounded(gate.entered.cancelled()).await;
    let (task, _) = h.cell();
    assert!(!gate.dropped.is_cancelled(), "tool must still be pending");
    // This is the public registry boundary used by the task_stop builtin.
    assert!(h.registry.kill(task));
    bounded(gate.dropped.cancelled()).await;
    assert!(
        !gate.finished.is_cancelled(),
        "pending tool completed instead of being cancelled"
    );
    assert_eq!(
        bounded(h.registry.wait_terminal(task)).await,
        Some(TaskStatus::Killed)
    );
    bounded(h.registry.wait_for_quiescence()).await;
}

#[tokio::test]
async fn session_root_shutdown_cancels_pending_nested_tool_and_quiesces() {
    let gate = Gate::default();
    let mut h = Harness::new(true, vec![gate.clone().into()]);
    h.exec("yield_control(); await tools.gate({});").await;
    bounded(gate.entered.cancelled()).await;
    let (task, _) = h.cell();
    assert!(!gate.dropped.is_cancelled(), "tool must still be pending");
    h.registry.shutdown();
    bounded(gate.dropped.cancelled()).await;
    assert!(!gate.finished.is_cancelled());
    bounded(h.registry.wait_for_quiescence()).await;
    assert_eq!(h.registry.status(task), Some(TaskStatus::Killed));
}

#[tokio::test]
async fn promise_all_admits_parallel_tools_before_either_finishes() {
    let first = Gate::default();
    let second = Gate::default();
    let mut second_tool: ErasedToolDefinition = second.clone().into();
    second_tool.name = "second_gate".into();
    let mut h = Harness::new(true, vec![first.clone().into(), second_tool]);
    h.exec("yield_control(); await Promise.all([tools.gate({}), tools.second_gate({})]); text('both finished');").await;
    bounded(first.entered.cancelled()).await;
    bounded(second.entered.cancelled()).await;
    assert!(!first.finished.is_cancelled());
    assert!(!second.finished.is_cancelled());
    let (_, cell) = h.cell();
    first.release.cancel();
    second.release.cancel();
    h.collect(&cell).await;
    assert!(h.result("wait").contains("both finished"));
}

#[derive(Clone)]
struct Stop;

#[derive(Deserialize, JsonSchema)]
struct StopInput {
    id: usize,
}

impl ToolDefinition for Stop {
    type Input = StopInput;
    fn name(&self) -> &'static str {
        "stop"
    }
    fn description(&self) -> &'static str {
        "Stop a cell's task"
    }
    fn execution_mode(&self) -> aj_agent::tool::ExecutionMode {
        aj_agent::tool::ExecutionMode::Control
    }
    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: StopInput,
    ) -> Result<ToolOutcome, BoxError> {
        let id = input.id;
        let registry = ctx.task_registry();
        assert!(registry.kill(id));
        assert_eq!(registry.wait_terminal(id).await, Some(TaskStatus::Killed));
        Ok(outcome("stopped", false))
    }
}

#[tokio::test]
async fn control_call_can_stop_a_cell_holding_a_sequential_resource_permit() {
    let gate = Gate::default();
    let mut exclusive: ErasedToolDefinition = gate.clone().into();
    exclusive.execution_mode = aj_agent::tool::ExecutionMode::Sequential;
    let mut h = Harness::new(true, vec![exclusive, Stop.into()]);
    h.exec("yield_control(); await tools.gate({});").await;
    bounded(gate.entered.cancelled()).await;
    let (task, _) = h.cell();
    h.exec(&format!("text(await tools.stop({{id: {task}}}));"))
        .await;
    assert!(h.result("exec").contains("stopped"));
    assert!(gate.dropped.is_cancelled());
    assert!(!gate.finished.is_cancelled());
    bounded(h.registry.wait_for_quiescence()).await;
}

#[tokio::test]
async fn disabling_code_mode_cancels_open_cells_before_ordinary_calls_resume() {
    let gate = Gate::default();
    let echo = Echo::default();
    let mut h = Harness::new(true, vec![gate.clone().into(), echo.clone().into()]);
    h.exec("yield_control(); await tools.gate({});").await;
    bounded(gate.entered.cancelled()).await;
    let (task, _) = h.cell();
    h.agent.set_code_mode(false);
    h.provider.enqueue("echo", json!({"value":"ordinary"}));
    h.provider.done();
    h.prompt().await;
    assert!(gate.dropped.is_cancelled());
    assert_eq!(h.registry.status(task), Some(TaskStatus::Killed));
    assert_eq!(*echo.0.lock().unwrap(), ["ordinary"]);
    assert!(
        h.provider
            .contexts
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .tools
            .iter()
            .all(|t| t.name != "exec")
    );
}

#[tokio::test]
async fn session_shutdown_owns_a_cell_even_if_the_foreground_prompt_is_dropped() {
    let gate = Gate::default();
    let mut h = Harness::new(true, vec![gate.clone().into()]);
    h.provider.enqueue("exec", json!("await tools.gate({});"));
    let mut prompt = Box::pin(h.agent.prompt("start".into(), CancellationToken::new()));
    bounded(async {
        tokio::select! {
            result = &mut prompt => panic!("prompt finished before the gate: {result:?}"),
            _ = gate.entered.cancelled() => {},
        }
    })
    .await;
    assert!(
        h.registry.snapshot().is_empty(),
        "exec must not have yielded yet"
    );
    drop(prompt);
    assert!(!gate.dropped.is_cancelled());
    h.registry.shutdown();
    bounded(h.registry.wait_for_quiescence()).await;
    assert!(gate.dropped.is_cancelled());
    assert!(!gate.finished.is_cancelled());
    assert!(
        h.events.lock().unwrap().iter().any(|event| matches!(event,
            AgentEvent::MessageEnd { message, .. }
                if matches!(&message.kind, AgentMessageKind::ToolActivity(activity)
                    if matches!(&activity.message, Message::ToolResult(result) if result.is_error))
        )),
        "callback audit must finish before the session writer can be released"
    );
}

#[tokio::test]
async fn committed_store_survives_disabling_and_reenabling_code_mode() {
    let mut h = Harness::new(true, vec![]);
    h.exec(r#"store("saved", {answer: 42});"#).await;
    h.agent.set_code_mode(false);
    h.provider.done();
    h.prompt().await;
    h.agent.set_code_mode(true);
    h.exec(r#"text(load("saved").answer);"#).await;
    assert!(h.result("exec").contains("42"));
    assert_eq!(
        h.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, AgentEvent::CodeModeStore { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn store_acknowledgment_failure_is_fatal_even_after_the_listener_is_removed() {
    let mut h = Harness::new(true, vec![]);
    let failing = h.agent.subscribe(Arc::new(|event| {
        let is_commit = matches!(event, AgentEvent::CodeModeStore { .. });
        Box::pin(async move {
            if is_commit {
                Err("injected store acknowledgment failure".into())
            } else {
                Ok(())
            }
        })
    }));
    h.provider.enqueue("exec", json!(r#"store("saved", 42);"#));
    let error = bounded(h.agent.prompt("save".into(), CancellationToken::new()))
        .await
        .expect_err("an uncertain commit cannot become an ordinary tool error");
    assert!(
        error.to_string().contains("store acknowledgment failure"),
        "{error}"
    );
    assert_eq!(h.provider.contexts.lock().unwrap().len(), 1);
    drop(failing);
    h.agent.set_code_mode(false);
    assert!(
        bounded(h.agent.prompt("continue".into(), CancellationToken::new()))
            .await
            .is_err()
    );
    assert_eq!(h.provider.contexts.lock().unwrap().len(), 1);
    h.registry.shutdown();
    bounded(h.registry.wait_for_quiescence()).await;
}

#[tokio::test]
async fn background_store_failure_during_admission_prevents_the_next_inference() {
    let mut h = Harness::new(true, vec![]);
    let entered = CancellationToken::new();
    let release = CancellationToken::new();
    let starts = std::sync::atomic::AtomicUsize::new(0);
    let registry = h.registry.clone();
    let _listener = h.agent.subscribe(Arc::new(move |event| {
        let store = matches!(event, AgentEvent::CodeModeStore { .. });
        let next = matches!(event, AgentEvent::MessageStart { message, .. }
            if matches!(message.as_stored_wire(), Some(Message::Assistant(_))))
            && starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1;
        let entered = entered.clone();
        let release = release.clone();
        let registry = registry.clone();
        Box::pin(async move {
            if store {
                entered.cancel();
                release.cancelled().await;
                return Err("injected background acknowledgment failure".into());
            }
            if next {
                entered.cancelled().await;
                let task = registry
                    .snapshot()
                    .into_iter()
                    .find(|task| matches!(task.kind, TaskKind::CodeMode { .. }))
                    .expect("the yielded cell must be tracked");
                release.cancel();
                registry.kill(task.id);
                // Completion owns the commit. Terminal task status proves the
                // failed acknowledgment settled before admission can continue.
                registry.wait_terminal(task.id).await;
            }
            Ok(())
        })
    }));
    h.provider
        .enqueue("exec", json!(r#"yield_control(); store("saved", 42);"#));
    h.provider.done();
    let result = bounded(h.agent.prompt("save".into(), CancellationToken::new())).await;
    h.registry.shutdown();
    bounded(h.registry.wait_for_quiescence()).await;
    assert!(result.is_err());
    assert_eq!(
        h.provider.contexts.lock().unwrap().len(),
        1,
        "known store failure must prevent the provider request, not merely reject its response"
    );
}
