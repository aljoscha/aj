//! Session-owned JavaScript orchestration over the ordinary tool execution boundary.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use aj_models::registry::ModelInfo;
use aj_models::types::{
    AssistantContent, AssistantMessage, Message, ToolCall, ToolInputFormat, ToolResultMessage,
    UserContent,
};
use codex_code_mode_protocol as protocol;
use codex_code_mode_runtime::InProcessCodeModeSession;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::execution::ToolRunner;
use crate::message::AgentMessage;
use crate::tool::{
    TaskEventSink, TaskKind, TaskNotice, TaskOutputSource, TaskRead, TaskStatus, ToolDetails,
    ToolOutcome,
};
use crate::{AgentEvent, BoxError, ErasedToolDefinition};

pub(crate) fn eligible(model: &ModelInfo) -> bool {
    static CATALOG: LazyLock<Value> = LazyLock::new(|| {
        serde_json::from_str(include_str!("../../../vendor/codex-code-mode/models.json"))
            .expect("vendored Codex catalog must be valid JSON")
    });
    aj_models::provider::supports_freeform_tools(&model.api)
        && CATALOG["models"].as_array().is_some_and(|models| {
            models.iter().any(|m| {
                m["slug"].as_str() == Some(model.id.as_str())
                    && matches!(
                        m["tool_mode"].as_str(),
                        Some("code_mode" | "code_mode_only")
                    )
            })
        })
}

fn nested_tools(tools: &HashMap<String, ErasedToolDefinition>) -> Vec<protocol::ToolDefinition> {
    let mut tools: Vec<_> = tools
        .values()
        .filter(|tool| tool.code_mode_exposure == crate::tool::CodeModeExposure::Nested)
        .map(|tool| {
            protocol::augment_tool_definition(protocol::ToolDefinition {
                name: tool.name.clone(),
                tool_name: protocol::ToolName::plain(&tool.name),
                description: tool
                    .freeform
                    .as_ref()
                    .map_or_else(|| tool.description.clone(), |raw| raw.description.clone()),
                kind: if tool.freeform.is_some() {
                    protocol::CodeModeToolKind::Freeform
                } else {
                    protocol::CodeModeToolKind::Function
                },
                input_schema: tool.freeform.is_none().then(|| tool.input_schema.clone()),
                input_schema_max_bytes: None,
                output_schema: Some(
                    tool.output_schema
                        .clone()
                        .unwrap_or_else(crate::tool::derive_schema::<ScriptResult>),
                ),
            })
        })
        .collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

pub(crate) fn catalog(
    tools: &HashMap<String, ErasedToolDefinition>,
) -> Vec<aj_models::types::ToolDefinition> {
    let nested = nested_tools(tools);
    let mut description = protocol::build_exec_tool_description(
        &nested,
        &[],
        &BTreeMap::new(),
        protocol::DEFAULT_EXEC_YIELD_TIME_MS,
        true,
        protocol::ImageDetailVisibility::Hidden,
        protocol::DeferredToolDiscovery::Catalog,
        None,
    );
    // The evaluator's notification callback belongs to the host. AJ delivers
    // explicit notifications at inference boundaries, never as duplicate results.
    description = description.replace(
        "immediately injects an extra `custom_tool_call_output` for the current `exec` call",
        "queues a notification for the next model inference and immediately displays it to the user",
    );
    description.push_str(concat!(
        "\n\nAJ: committed store()/load() values belong to this agent's conversation branch ",
        "and survive session resume and compaction. Writes commit when the script finishes, ",
        "including when it ends with a script error. Cancellation before completion discards ",
        "pending writes, but does not undo tool side effects. Cells survive turns, not process ",
        "restart or branch changes. Interrupting the agent cancels its open cells. Disabling ",
        "Code Mode or switching to an ineligible model cancels them too, retaining committed ",
        "values. Tool availability is checked on each nested call. An open cell stays in the ",
        "task list until collected or terminated, even if evaluation has finished. Use cell ",
        "`wait` to collect it rather than waiting for a task-completion notice. AJ's input-yield ",
        "tool, when present, is named `yield`. AJ supports text and image output, not audio output."
    ));
    // Codex's exec grammar, from core/src/tools/code_mode/execute_spec.rs.
    // The vendored Apache-2.0 license and NOTICE cover this grammar as well.
    let grammar = r#"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;
    let mut result = vec![
        aj_models::types::ToolDefinition {
            name: "exec".into(),
            description,
            parameters: Value::Null,
            input_format: Some(ToolInputFormat::Grammar {
                syntax: "lark".into(),
                definition: grammar.into(),
            }),
        },
        aj_models::types::ToolDefinition {
            name: "wait".into(),
            description: protocol::build_wait_tool_description().into(),
            parameters: crate::tool::derive_schema::<WaitInput>(),
            input_format: None,
        },
    ];
    let mut direct: Vec<_> = tools
        .values()
        .filter(|t| t.code_mode_exposure == crate::tool::CodeModeExposure::DirectOnly)
        .collect();
    direct.sort_by(|a, b| a.name.cmp(&b.name));
    result.extend(direct.into_iter().map(|t| {
        aj_models::types::ToolDefinition {
            name: if t.name == "wait" {
                "yield".into()
            } else {
                t.name.clone()
            },
            description: t
                .freeform
                .as_ref()
                .map_or_else(|| t.description.clone(), |raw| raw.description.clone()),
            parameters: t.input_schema.clone(),
            input_format: t.freeform.as_ref().map(|raw| raw.input_format.clone()),
        }
    }));
    result
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct WaitInput {
    cell_id: String,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

/// The owner is the agent, not an exec invocation. Delegates retain only shared
/// dispatch state, so neither a cell nor a task driver can keep its owner alive.
pub(crate) struct CodeMode {
    runtime: Arc<InProcessCodeModeSession>,
    shared: Arc<Shared>,
    owner_cancel: CancellationToken,
}

/// Committed values outlive evaluator replacement. An acknowledgment failure
/// poisons the owner because an earlier subscriber may already have persisted
/// the update, so retrying or continuing would assume a rollback we cannot make.
#[derive(Default)]
pub(crate) struct Store {
    values: BTreeMap<String, Value>,
    failure: Option<String>,
}

impl Store {
    pub(crate) fn new(values: BTreeMap<String, Value>) -> Self {
        Self {
            values,
            failure: None,
        }
    }

    pub(crate) fn check(&self) -> Result<(), String> {
        self.failure
            .as_ref()
            .map_or(Ok(()), |error| Err(error.clone()))
    }
}

struct Shared {
    prefix: String,
    runner: RwLock<ToolRunner>,
    cells: Mutex<HashMap<protocol::CellId, Cell>>,
    notifications: Mutex<VecDeque<String>>,
    store: Arc<Mutex<Store>>,
}

struct Cell {
    call_id: String,
    closed: CancellationToken,
    task: Option<(usize, watch::Sender<Option<TaskStatus>>)>,
    output: Arc<CellOutput>,
}

#[derive(Default)]
struct CellOutput(Mutex<String>);

impl CellOutput {
    fn append(&self, content: &[UserContent]) {
        let mut output = self.0.lock().expect("cell output mutex poisoned");
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&content_text(content));
    }
}

impl TaskOutputSource for CellOutput {
    fn snapshot(&self) -> TaskRead {
        TaskRead {
            report: Some(self.0.lock().expect("cell output mutex poisoned").clone()),
            ..TaskRead::default()
        }
    }
}

impl CodeMode {
    pub(crate) fn new(runner: ToolRunner, store: Arc<Mutex<Store>>) -> Self {
        let values = store
            .lock()
            .expect("code mode store mutex poisoned")
            .values
            .clone()
            .into_iter()
            .collect();
        let runtime = Arc::new(InProcessCodeModeSession::with_stored_values(values));
        let owner_cancel = CancellationToken::new();
        let shutdown_runtime = Arc::clone(&runtime);
        let owner = owner_cancel.clone();
        let session = runner.context.task_registry.root_cancel.clone();
        // A prompt can be dropped before exec's first observation. The session
        // still owns that cell, independently of foreground-future destruction.
        tokio::spawn(async move {
            tokio::select! {
                _ = owner.cancelled() => {},
                _ = session.cancelled() => {},
            }
            if let Err(error) = shutdown_runtime.shutdown().await {
                tracing::warn!("code mode shutdown failed: {error}");
            }
        });
        Self {
            runtime,
            shared: Arc::new(Shared {
                prefix: format!("{:016x}-", rand::random::<u64>()),
                runner: RwLock::new(runner),
                cells: Mutex::new(HashMap::new()),
                notifications: Mutex::new(VecDeque::new()),
                store,
            }),
            owner_cancel,
        }
    }

    /// Config changes apply to new dispatches. Calls already admitted keep their
    /// owned context, just like an ordinary tool call already in progress.
    pub(crate) fn refresh(&self, runner: ToolRunner) {
        *self
            .shared
            .runner
            .write()
            .expect("code mode runner lock poisoned") = runner;
    }

    pub(crate) fn notifications(&self) -> Vec<String> {
        self.shared
            .notifications
            .lock()
            .expect("code mode notifications lock poisoned")
            .drain(..)
            .collect()
    }

    pub(crate) async fn call(
        &self,
        name: &str,
        call_id: &str,
        input: Value,
    ) -> Result<ToolOutcome, BoxError> {
        self.shared.check_store()?;
        let (response, budget) = if name == "exec" {
            let parsed = protocol::parse_exec_source(
                input.as_str().ok_or("exec expects raw JavaScript source")?,
            )?;
            let runner = self
                .shared
                .runner
                .read()
                .expect("code mode runner lock poisoned")
                .clone();
            if runner.context.task_registry.root_cancel.is_cancelled() {
                return Err("Code Mode session is shutting down".into());
            }
            let tools = nested_tools(&runner.tools);
            let closed = CancellationToken::new();
            let started = self
                .runtime
                .execute(
                    protocol::ExecuteRequest {
                        tool_call_id: call_id.into(),
                        enabled_tools: tools,
                        source: parsed.code,
                        yield_time_ms: parsed.yield_time_ms,
                        max_output_tokens: parsed.max_output_tokens,
                    },
                    Arc::new(Delegate {
                        shared: Arc::clone(&self.shared),
                        closed: closed.clone(),
                        _cleanup: runner.context.task_registry.track_cleanup(),
                    }),
                    None,
                )
                .await?;
            self.shared
                .cells
                .lock()
                .expect("code mode cells lock poisoned")
                .insert(
                    started.cell_id.clone(),
                    Cell {
                        call_id: call_id.into(),
                        closed,
                        task: None,
                        output: Arc::new(CellOutput::default()),
                    },
                );
            (started.initial_response().await?, parsed.max_output_tokens)
        } else {
            let args: WaitInput = serde_json::from_value(input)?;
            let cell_id = protocol::CellId::new(
                args.cell_id
                    .strip_prefix(&self.shared.prefix)
                    .ok_or("cell not found in this live agent session")?
                    .to_owned(),
            );
            let response = if args.terminate {
                self.runtime.terminate(cell_id).await?
            } else {
                self.runtime
                    .wait(
                        protocol::WaitRequest {
                            cell_id,
                            yield_time_ms: args
                                .yield_time_ms
                                .unwrap_or(protocol::DEFAULT_WAIT_YIELD_TIME_MS),
                        },
                        None,
                    )
                    .await?
            };
            (response.into(), args.max_tokens)
        };
        let cell_id = response_id(&response).clone();
        let status = terminal_status(&response);
        let outcome = render(self.shared.public_response(response), budget);
        if let Some(status) = status {
            self.shared.finish(&cell_id, status, &outcome);
        } else {
            self.track_yielded(&cell_id, &outcome);
        }
        Ok(outcome)
    }

    fn track_yielded(&self, cell_id: &protocol::CellId, outcome: &ToolOutcome) {
        let runner = self
            .shared
            .runner
            .read()
            .expect("code mode runner lock poisoned")
            .clone();
        let mut cells = self
            .shared
            .cells
            .lock()
            .expect("code mode cells lock poisoned");
        let Some(cell) = cells.get_mut(cell_id) else {
            return;
        };
        cell.output.append(&outcome.content);
        if cell.task.is_some() {
            return;
        }
        // An open cell includes a completed script whose final result has not
        // yet been collected. The task owns that entire wait/termination lifetime.
        let public_id = self.shared.public_id(cell_id);
        let kind = TaskKind::CodeMode {
            cell_id: public_id.clone(),
        };
        let label = format!("Code Mode cell {public_id} (open until collected)");
        let ctx = &runner.context;
        let (id, cancel, registration) = ctx.task_registry.register_driver(
            ctx.agent_id,
            cell.call_id.clone(),
            kind.clone(),
            label.clone(),
            Arc::<CellOutput>::clone(&cell.output),
        );
        let events = TaskEventSink::new(
            ctx.parent_bus.clone(),
            ctx.task_registry.clone(),
            ctx.agent_id,
            id,
            cell.call_id.clone(),
            label.clone(),
        );
        let (done, mut completed) = watch::channel(None);
        cell.task = Some((id, done));
        let runtime = Arc::clone(&self.runtime);
        let closed = cell.closed.clone();
        let shared = Arc::clone(&self.shared);
        let owner_cancel = self.owner_cancel.clone();
        let cell_id = cell_id.clone();
        registration.spawn(async move {
            events.started(kind.clone()).await;
            let status = tokio::select! {
                _ = cancel.cancelled() => TaskStatus::Killed,
                _ = owner_cancel.cancelled() => TaskStatus::Killed,
                result = completed.wait_for(|status| status.is_some()) => result.ok().and_then(|status| *status).unwrap_or(TaskStatus::Killed),
            };
            if status == TaskStatus::Killed {
                match runtime.terminate(cell_id.clone()).await {
                    Ok(response) => {
                        let outcome = render(shared.public_response(response.into()), None);
                        shared.finish(&cell_id, status, &outcome);
                    }
                    Err(error) => tracing::warn!(%cell_id, "code mode termination failed: {error}"),
                }
            }
            // Retain the writer lease until the actor has drained its callbacks,
            // including when another observer initiated termination.
            closed.cancelled().await;
            let description = match status {
                TaskStatus::Exited(Some(0)) => "completed",
                TaskStatus::Killed => "cancelled",
                TaskStatus::Exited(_) | TaskStatus::CaptureFailed(_) => "failed",
                TaskStatus::Running => "is still running",
            };
            events.finished(status, TaskNotice {
                owner: events.owner(), task_id: id, kind, label,
                status, body: format!("Code Mode cell {public_id} {description}."),
            }).await;
        });
    }

    pub(crate) async fn interrupt(&self) -> Result<(), BoxError> {
        let cells: Vec<_> = self
            .shared
            .cells
            .lock()
            .expect("code mode cells lock poisoned")
            .iter()
            .map(|(id, cell)| (id.clone(), cell.task.as_ref().map(|(id, _)| *id)))
            .collect();
        let registry = self
            .shared
            .runner
            .read()
            .expect("code mode runner lock poisoned")
            .context
            .task_registry
            .clone();
        for (cell_id, task) in cells {
            if let Some(task) = task {
                registry.kill(task);
                registry.wait_terminal(task).await;
            } else {
                let response = self.runtime.terminate(cell_id.clone()).await?;
                let outcome = render(self.shared.public_response(response.into()), None);
                self.shared.finish(&cell_id, TaskStatus::Killed, &outcome);
            }
        }
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> Result<(), BoxError> {
        self.owner_cancel.cancel();
        self.runtime.shutdown().await?;
        Ok(())
    }
}

impl Drop for CodeMode {
    fn drop(&mut self) {
        self.owner_cancel.cancel();
    }
}

impl Shared {
    fn check_store(&self) -> Result<(), String> {
        self.store
            .lock()
            .expect("code mode store mutex poisoned")
            .check()
    }

    fn public_id(&self, id: &protocol::CellId) -> String {
        format!("{}{id}", self.prefix)
    }

    fn public_response(
        &self,
        mut response: protocol::RuntimeResponse,
    ) -> protocol::RuntimeResponse {
        let (protocol::RuntimeResponse::Yielded { cell_id, .. }
        | protocol::RuntimeResponse::Terminated { cell_id, .. }
        | protocol::RuntimeResponse::Result { cell_id, .. }) = &mut response;
        *cell_id = protocol::CellId::new(self.public_id(cell_id));
        response
    }

    fn finish(&self, id: &protocol::CellId, status: TaskStatus, outcome: &ToolOutcome) {
        if let Some(cell) = self
            .cells
            .lock()
            .expect("code mode cells lock poisoned")
            .remove(id)
        {
            cell.output.append(&outcome.content);
            if let Some((_, done)) = cell.task {
                done.send_replace(Some(status));
            }
        }
    }
}

struct Delegate {
    shared: Arc<Shared>,
    closed: CancellationToken,
    // Session shutdown must drain even unobserved store commits before closing
    // the persistence listener or releasing its writer lock.
    _cleanup: crate::TaskCleanupGuard,
}

impl Drop for Delegate {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

impl protocol::CodeModeSessionDelegate for Delegate {
    fn store<'a>(
        &'a self,
        _cell_id: protocol::CellId,
        writes: HashMap<String, Arc<Value>>,
    ) -> protocol::NotificationFuture<'a> {
        Box::pin(async move {
            self.shared.check_store()?;
            let runner = self
                .shared
                .runner
                .read()
                .expect("code mode runner lock poisoned")
                .clone();
            let writes: BTreeMap<_, _> = writes
                .into_iter()
                .map(|(key, value)| (key, (*value).clone()))
                .collect();
            // The runtime serializes commits and blocks new cell snapshots
            // through this await. Only acknowledged writes become live state.
            let result = runner
                .context
                .parent_bus
                .emit(AgentEvent::CodeModeStore {
                    agent_id: runner.context.agent_id,
                    writes: writes.clone(),
                })
                .await;
            let mut store = self
                .shared
                .store
                .lock()
                .expect("code mode store mutex poisoned");
            match result {
                Ok(()) => {
                    store.values.extend(writes);
                    Ok(())
                }
                Err(error) => {
                    let error = format!("Code Mode store persistence failed: {error}");
                    store.failure = Some(error.clone());
                    Err(error)
                }
            }
        })
    }

    fn invoke_tool<'a>(
        &'a self,
        invocation: protocol::CodeModeNestedToolCall,
        cancel: CancellationToken,
    ) -> protocol::ToolInvocationFuture<'a> {
        Box::pin(async move {
            self.shared.check_store()?;
            let runner = self
                .shared
                .runner
                .read()
                .expect("code mode runner lock poisoned")
                .clone();
            let name = invocation.tool_name.name;
            if !runner.tools.get(&name).is_some_and(|tool| {
                tool.code_mode_exposure == crate::tool::CodeModeExposure::Nested
            }) {
                return Err(format!("Tool {name} is not available inside exec"));
            }
            let cell_id = self.shared.public_id(&invocation.cell_id);
            let call_id = format!("code-{:032x}", rand::random::<u128>());
            let input = invocation.input.unwrap_or(Value::Null);
            let mut assistant = AssistantMessage::empty();
            assistant.content.push(AssistantContent::ToolCall(ToolCall {
                is_raw: runner.tools[&name].freeform.is_some() && input.is_string(),
                id: call_id.clone(),
                name: name.clone(),
                arguments: input.clone(),
            }));
            let bus = &runner.context.parent_bus;
            let agent_id = runner.context.agent_id;
            bus.emit(AgentEvent::MessageEnd {
                agent_id,
                message: AgentMessage::tool_activity(
                    cell_id.clone(),
                    Message::Assistant(assistant),
                ),
            })
            .await
            .map_err(|e| e.to_string())?;
            let result = runner
                .run(call_id.clone(), name.clone(), input, cancel)
                .await
                .map_err(|e| e.to_string())?;
            let outcome = result.outcome;
            let value = script_result(&outcome);
            let message = ToolResultMessage {
                tool_call_id: call_id.clone(),
                tool_name: name.clone(),
                content: outcome.content.clone(),
                details: Some(serde_json::to_value(&outcome.details).map_err(|e| e.to_string())?),
                is_error: outcome.is_error,
                timestamp: chrono::Utc::now().timestamp_millis(),
            };
            bus.emit(AgentEvent::MessageEnd {
                agent_id,
                message: AgentMessage::tool_activity(cell_id, Message::ToolResult(message)),
            })
            .await
            .map_err(|e| e.to_string())?;
            bus.emit(AgentEvent::ToolExecutionEnd {
                agent_id,
                call_id,
                tool: name,
                content: outcome.content.into(),
                result: outcome.details,
                is_error: outcome.is_error,
            })
            .await
            .map_err(|e| e.to_string())?;
            value
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        cell_id: protocol::CellId,
        text: String,
        _cancel: CancellationToken,
    ) -> protocol::NotificationFuture<'a> {
        Box::pin(async move {
            let text = format!("Code Mode cell {}: {text}", self.shared.public_id(&cell_id));
            self.shared
                .notifications
                .lock()
                .expect("code mode notifications lock poisoned")
                .push_back(text.clone());
            let runner = self
                .shared
                .runner
                .read()
                .expect("code mode runner lock poisoned")
                .clone();
            runner
                .context
                .parent_bus
                .emit(AgentEvent::Notice {
                    agent_id: runner.context.agent_id,
                    text,
                })
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn cell_closed(&self, _cell_id: &protocol::CellId) {
        self.closed.cancel();
    }
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct ScriptResult {
    content: Vec<ScriptContent>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ScriptContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

fn script_result(outcome: &ToolOutcome) -> Result<Value, String> {
    if outcome.is_error {
        return Err(content_text(&outcome.content));
    }
    if let Some(value) = &outcome.structured_content {
        return Ok(value.clone());
    }
    let content: Vec<_> = outcome
        .content
        .iter()
        .map(|item| match item {
            UserContent::Text(t) => ScriptContent::Text {
                text: t.text.clone(),
            },
            UserContent::Image(i) => ScriptContent::Image {
                data: i.data.clone(),
                mime_type: i.mime_type.clone(),
            },
        })
        .collect();
    serde_json::to_value(ScriptResult { content }).map_err(|error| error.to_string())
}

fn content_text(content: &[UserContent]) -> String {
    content
        .iter()
        .filter_map(|item| match item {
            UserContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn response_id(response: &protocol::RuntimeResponse) -> &protocol::CellId {
    match response {
        protocol::RuntimeResponse::Yielded { cell_id, .. }
        | protocol::RuntimeResponse::Terminated { cell_id, .. }
        | protocol::RuntimeResponse::Result { cell_id, .. } => cell_id,
    }
}

fn terminal_status(response: &protocol::RuntimeResponse) -> Option<TaskStatus> {
    match response {
        protocol::RuntimeResponse::Yielded { .. } => None,
        protocol::RuntimeResponse::Terminated { .. } => Some(TaskStatus::Killed),
        protocol::RuntimeResponse::Result { error_text, .. } => {
            Some(TaskStatus::Exited(Some(if error_text.is_some() {
                1
            } else {
                0
            })))
        }
    }
}

fn render(response: protocol::RuntimeResponse, budget: Option<usize>) -> ToolOutcome {
    let (status, mut items, mut error) = match response {
        protocol::RuntimeResponse::Yielded {
            cell_id,
            content_items,
            ..
        } => (
            format!("Script running with cell ID {cell_id}"),
            content_items,
            None,
        ),
        protocol::RuntimeResponse::Terminated { content_items, .. } => {
            ("Script terminated".into(), content_items, None)
        }
        protocol::RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => (
            if error_text.is_some() {
                "Script failed"
            } else {
                "Script completed"
            }
            .into(),
            content_items,
            error_text,
        ),
    };
    let mut content = vec![UserContent::text(&status)];
    if let Some(error) = &error {
        items.push(protocol::FunctionCallOutputContentItem::InputText {
            text: format!("Script error:\n{error}"),
        });
    }
    // Approximate token budget, in UTF-8 bytes. Keep status and one truncation
    // notice outside the budget so even a zero budget remains understandable.
    let mut remaining = budget
        .unwrap_or(protocol::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
        .saturating_mul(4);
    let mut truncated = false;
    for item in items {
        match item {
            protocol::FunctionCallOutputContentItem::InputText { text } => {
                let end = text.floor_char_boundary(remaining.min(text.len()));
                let output = text[..end].to_owned();
                remaining = remaining.saturating_sub(end);
                truncated |= end < text.len();
                if !output.is_empty() {
                    content.push(UserContent::text(output));
                }
            }
            protocol::FunctionCallOutputContentItem::InputImage { image_url, .. } => {
                if let Some((header, data)) = image_url.split_once(',')
                    && let Some(mime) = header
                        .strip_prefix("data:")
                        .and_then(|h| h.strip_suffix(";base64"))
                {
                    content.push(UserContent::image(data, mime));
                } else {
                    error = Some("Code Mode images must be base64 data URLs".into());
                    content.push(UserContent::text(
                        "Output error: invalid Code Mode image URL",
                    ));
                }
            }
            protocol::FunctionCallOutputContentItem::InputAudio { .. } => {
                error = Some("AJ does not support audio tool output".into());
                content.push(UserContent::text(
                    "Output error: AJ does not support audio tool output",
                ));
            }
        }
    }
    if truncated {
        content.push(UserContent::text("[Code Mode output truncated]"));
    }
    ToolOutcome {
        structured_content: None,
        details: ToolDetails::Text {
            summary: status,
            body: content_text(&content),
        },
        content,
        is_error: error.is_some(),
    }
}
