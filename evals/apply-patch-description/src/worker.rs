//! Disposable agent worker and brokered provider implementation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::bus::listener_from_sync;
use aj_agent::hooks::ShouldStopAfterTurnHook;
use aj_agent::tool::{ErasedToolDefinition, ToolDetails, ToolOutcome};
use aj_agent::{Agent, AgentSeed, TaskRegistry, TurnError};
use aj_conf::{AgentEnv, SystemPrompt, SystemPromptSource};
use aj_models::ThinkingConfig;
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::streaming::{AssistantMessageEvent, AssistantMessageEventStream, ErrorReason};
use aj_models::types::{
    AssistantError, AssistantMessage, Context, ErrorCategory, Message, SimpleStreamOptions,
    StopReason, StreamOptions, ThinkingLevel, ToolDefinition, UserMessage,
};
use aj_tools::{BuiltinToolOptions, builtin_tools_for_model};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::descriptions::load;
use crate::protocol::{
    ParentResponse, ProtocolError, ToolOutcomeWire, WorkerInit, WorkerRequest, read_frame,
    write_frame,
};
use crate::runtime::{EventCollector, WorkerResult, WorkerTerminal};

type BoxReader = Box<dyn AsyncRead + Send + Unpin>;
type BoxWriter = Box<dyn AsyncWrite + Send + Unpin>;

/// Multiplexed guest-side client over attached stdin/stdout.
pub struct IpcClient {
    writer: AsyncMutex<BoxWriter>,
    pending: Mutex<HashMap<u64, mpsc::UnboundedSender<ParentResponse>>>,
    next_id: AtomicU64,
    runner_internal: Mutex<Option<String>>,
}

impl IpcClient {
    pub fn new<R, W>(reader: R, writer: W) -> Arc<Self>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let client = Arc::new(Self {
            writer: AsyncMutex::new(Box::new(writer)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            runner_internal: Mutex::new(None),
        });
        Self::spawn_dispatcher(&client, Box::new(reader));
        client
    }

    fn spawn_dispatcher(client: &Arc<Self>, mut reader: BoxReader) {
        let weak = Arc::downgrade(client);
        tokio::spawn(async move {
            loop {
                let response = match read_frame::<_, ParentResponse>(&mut reader).await {
                    Ok(Some(response)) => response,
                    Ok(None) => {
                        fail_all(&weak, "parent closed the broker stream");
                        return;
                    }
                    Err(error) => {
                        fail_all(&weak, &format!("broker response failed: {error}"));
                        return;
                    }
                };
                let Some(client) = weak.upgrade() else {
                    return;
                };
                let sender = client
                    .pending
                    .lock()
                    .expect("IPC pending mutex poisoned")
                    .get(&response.id())
                    .cloned();
                if let Some(sender) = sender {
                    let _ = sender.send(response);
                }
            }
        });
    }

    async fn begin(
        &self,
        request: WorkerRequest,
    ) -> Result<(u64, mpsc::UnboundedReceiver<ParentResponse>), ProtocolError> {
        let id = match &request {
            WorkerRequest::Provider { id, .. } | WorkerRequest::Tool { id, .. } => *id,
            WorkerRequest::Finished { .. } => {
                return Err(ProtocolError(
                    "finished message cannot await a response".into(),
                ));
            }
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        if self
            .pending
            .lock()
            .expect("IPC pending mutex poisoned")
            .insert(id, sender)
            .is_some()
        {
            return Err(ProtocolError(format!("duplicate request id {id}")));
        }
        if let Err(error) = write_frame(&mut *self.writer.lock().await, &request).await {
            self.pending
                .lock()
                .expect("IPC pending mutex poisoned")
                .remove(&id);
            return Err(error);
        }
        Ok((id, receiver))
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn finish_request(&self, id: u64) {
        self.pending
            .lock()
            .expect("IPC pending mutex poisoned")
            .remove(&id);
    }

    fn record_runner_internal(&self, error: String) {
        let mut recorded = self
            .runner_internal
            .lock()
            .expect("IPC runner-internal mutex poisoned");
        if recorded.is_none() {
            *recorded = Some(error);
        }
    }

    fn runner_internal(&self) -> Option<String> {
        self.runner_internal
            .lock()
            .expect("IPC runner-internal mutex poisoned")
            .clone()
    }

    async fn tool(&self, name: String, arguments: Value) -> Result<ToolOutcomeWire, ProtocolError> {
        let id = self.next_id();
        let (_, mut receiver) = self
            .begin(WorkerRequest::Tool {
                id,
                name,
                arguments,
            })
            .await?;
        let result = match receiver.recv().await {
            Some(ParentResponse::ToolResult { outcome, .. }) => Ok(outcome),
            Some(ParentResponse::Failure { error, .. }) => Err(ProtocolError(error)),
            Some(ParentResponse::ProviderEvent { .. }) => Err(ProtocolError(
                "provider event answered a tool request".into(),
            )),
            None => Err(ProtocolError("tool response channel closed".into())),
        };
        self.finish_request(id);
        result
    }

    async fn send_finished(&self, result: WorkerResult) -> Result<(), ProtocolError> {
        write_frame(
            &mut *self.writer.lock().await,
            &WorkerRequest::Finished { result },
        )
        .await
    }
}

fn fail_all(weak: &std::sync::Weak<IpcClient>, error: &str) {
    let Some(client) = weak.upgrade() else {
        return;
    };
    let pending = std::mem::take(&mut *client.pending.lock().expect("IPC pending mutex poisoned"));
    for (id, sender) in pending {
        let _ = sender.send(ParentResponse::Failure {
            id,
            error: error.to_string(),
        });
    }
}

/// Provider installed in the guest Agent. It has no network or credentials.
pub struct IpcProvider {
    client: Arc<IpcClient>,
    requests: Arc<AtomicU32>,
}

impl IpcProvider {
    pub fn new(client: Arc<IpcClient>, requests: Arc<AtomicU32>) -> Self {
        Self { client, requests }
    }

    fn request(
        &self,
        model: &ModelInfo,
        context: &Context,
        reasoning: ThinkingLevel,
        cancel: Option<CancellationToken>,
    ) -> AssistantMessageEventStream {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let output = AssistantMessageEventStream::new();
        let producer = output.clone();
        let client = Arc::clone(&self.client);
        let context = context.clone();
        let model = model.clone();
        tokio::spawn(async move {
            let id = client.next_id();
            let request = WorkerRequest::Provider {
                id,
                context,
                observed_reasoning: reasoning,
            };
            let (_, mut receiver) = match client.begin(request).await {
                Ok(value) => value,
                Err(error) => {
                    producer.push(provider_error(&model, error.to_string()));
                    return;
                }
            };
            let mut last_partial = empty_message(&model);
            loop {
                let response = if let Some(cancel) = &cancel {
                    tokio::select! {
                        response = receiver.recv() => response,
                        () = cancel.cancelled() => {
                            producer.push(AssistantMessageEvent::aborted(last_partial));
                            client.finish_request(id);
                            return;
                        }
                    }
                } else {
                    receiver.recv().await
                };
                match response {
                    Some(ParentResponse::ProviderEvent { event, .. }) => {
                        last_partial = event.partial().clone();
                        let terminal = event.is_terminal();
                        producer.push(event);
                        if terminal {
                            client.finish_request(id);
                            return;
                        }
                    }
                    Some(ParentResponse::Failure { error, .. }) => {
                        producer.push(provider_error(&model, error));
                        client.finish_request(id);
                        return;
                    }
                    Some(ParentResponse::ToolResult { .. }) => {
                        producer.push(provider_error(
                            &model,
                            "tool result answered a provider request".into(),
                        ));
                        client.finish_request(id);
                        return;
                    }
                    None => {
                        producer.push(provider_error(&model, "provider channel closed".into()));
                        client.finish_request(id);
                        return;
                    }
                }
            }
        });
        output
    }
}

impl Provider for IpcProvider {
    fn stream(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &StreamOptions,
    ) -> AssistantMessageEventStream {
        self.request(model, context, ThinkingLevel::Off, options.cancel.clone())
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.request(
            model,
            context,
            options.reasoning,
            options.base.cancel.clone(),
        )
    }
}

fn empty_message(model: &ModelInfo) -> AssistantMessage {
    let mut message = AssistantMessage::empty();
    message.api = model.api.clone();
    message.provider = model.provider.clone();
    message.model = model.id.clone();
    message
}

fn provider_error(model: &ModelInfo, text: String) -> AssistantMessageEvent {
    let mut message = empty_message(model);
    message.stop_reason = StopReason::Error;
    message.error = Some(AssistantError::new(ErrorCategory::Transient, text));
    AssistantMessageEvent::Error {
        reason: ErrorReason::Error,
        error: message,
    }
}

/// Runs the hidden `__worker` command on attached stdio.
pub async fn run_worker() -> Result<(), ProtocolError> {
    let mut stdin = tokio::io::stdin();
    let init: WorkerInit = read_frame(&mut stdin)
        .await?
        .ok_or_else(|| ProtocolError("worker did not receive initialization".into()))?;
    let client = IpcClient::new(stdin, tokio::io::stdout());
    let result = execute_worker(init, Arc::clone(&client)).await;
    client.send_finished(result).await
}

async fn execute_worker(init: WorkerInit, client: Arc<IpcClient>) -> WorkerResult {
    let requests = Arc::new(AtomicU32::new(0));
    let limit_reached = Arc::new(AtomicBool::new(false));
    let provider: Arc<dyn Provider> =
        Arc::new(IpcProvider::new(Arc::clone(&client), Arc::clone(&requests)));
    let model = Arc::new(ModelInfo {
        id: init.model.id,
        name: init.model.name,
        family: init.model.family,
        api: init.model.api,
        provider: init.model.provider,
        base_url: String::new(),
        reasoning: init.model.reasoning,
        reasoning_options: init.model.reasoning_options,
        supports_verbosity: init.model.supports_verbosity,
        input: init.model.input,
        cost: init.model.cost,
        context_window: init.model.context_window,
        max_tokens: init.model.max_tokens,
    });

    let tools = brokered_tools(&client, model.family.as_deref(), init.variant);
    let context_tools = tools
        .iter()
        .map(|tool| ToolDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
        })
        .collect::<Vec<_>>();
    let mut options = StreamOptions {
        session_id: Some(init.session_id),
        max_tokens: Some(init.max_output_tokens),
        ..StreamOptions::default()
    };
    options.api_key = None;
    options.api_key_resolver = None;
    let disabled = vec!["agent".to_string()];
    let mut agent = Agent::with_provider(
        PathBuf::from("/workspace"),
        tools,
        disabled,
        provider,
        model,
        options,
        Some(ThinkingConfig::Low),
    );
    let registry = TaskRegistry::default();
    agent.set_task_registry(registry.clone());

    let max_responses = init.max_model_responses;
    let hook_requests = Arc::clone(&requests);
    let hook_reached = Arc::clone(&limit_reached);
    let stop_hook: ShouldStopAfterTurnHook = Arc::new(move || {
        let should_stop = hook_requests.load(Ordering::SeqCst) >= max_responses;
        if should_stop {
            hook_reached.store(true, Ordering::SeqCst);
        }
        Box::pin(async move { should_stop })
    });
    agent.set_should_stop_after_turn(Some(stop_hook));

    let context = initial_context(&init.utc_date, &init.prompt, &context_tools);
    let assembled = context.system_prompt.unwrap_or_default();
    agent.seed_session(AgentSeed {
        transcript: Vec::new(),
        assembled_system_prompt: Some(assembled),
        sub_agent_counter: 0,
    });

    let collector = Arc::new(Mutex::new(EventCollector::default()));
    let listener_collector = Arc::clone(&collector);
    let subscription = agent.subscribe(listener_from_sync(move |event| {
        listener_collector
            .lock()
            .expect("worker event collector mutex poisoned")
            .observe(event);
    }));
    let outcome = agent.prompt(init.prompt, CancellationToken::new()).await;
    registry.shutdown();
    let registry_quiescent = registry.quiesce(Duration::from_secs(2)).await;
    drop(subscription);
    drop(agent);
    let metrics = Arc::try_unwrap(collector)
        .expect("worker collector still shared")
        .into_inner()
        .expect("worker event collector mutex poisoned")
        .finish();

    let (mut terminal, mut error) = match outcome {
        Ok(()) if limit_reached.load(Ordering::SeqCst) => (WorkerTerminal::TurnLimit, None),
        Ok(()) => (WorkerTerminal::Completed, None),
        Err(TurnError::Aborted) => (WorkerTerminal::Cancelled, Some("agent was aborted".into())),
        Err(TurnError::Recoverable(error)) => {
            (WorkerTerminal::ModelFailed, Some(error.to_string()))
        }
        Err(TurnError::Fatal(error)) => (WorkerTerminal::RunnerInternal, Some(error.to_string())),
    };
    if let Some(internal) = client.runner_internal() {
        terminal = WorkerTerminal::RunnerInternal;
        error = Some(internal);
    }
    WorkerResult {
        terminal: if registry_quiescent {
            terminal
        } else {
            WorkerTerminal::RunnerInternal
        },
        error: if registry_quiescent {
            error
        } else {
            Some("task registry did not quiesce".into())
        },
        metrics,
        registry_quiescent,
    }
}

fn brokered_tools(
    client: &Arc<IpcClient>,
    family: Option<&str>,
    variant: crate::descriptions::DescriptionVariant,
) -> Vec<ErasedToolDefinition> {
    let disabled = vec!["agent".to_string()];
    let mut tools = builtin_tools_for_model(
        &BuiltinToolOptions {
            image_auto_resize: true,
            bash_rtk: false,
        },
        &disabled,
        family,
    );
    for tool in &mut tools {
        if matches!(tool.name.as_str(), "apply_patch" | "bash" | "read_file") {
            let name = tool.name.clone();
            let rpc = Arc::clone(client);
            tool.func = Arc::new(move |_context, arguments| {
                let rpc = Arc::clone(&rpc);
                let name = name.clone();
                Box::pin(async move {
                    let outcome = rpc.tool(name, arguments).await?;
                    let details = serde_json::from_value::<ToolDetails>(outcome.details.clone())
                        .map_err(|error| -> aj_agent::BoxError {
                            let message = format!(
                                "cannot deserialize exact ToolDetails from tool worker: {error}"
                            );
                            rpc.record_runner_internal(message.clone());
                            message.into()
                        })?;
                    Ok(ToolOutcome {
                        content: outcome.content,
                        details,
                        is_error: outcome.is_error,
                    })
                })
            });
        }
        if tool.name == "apply_patch" {
            tool.description = load(variant).content;
        }
    }
    tools
}

/// Builds the exact first provider context without making a model request.
pub fn initial_context(date: &str, prompt: &str, tools: &[ToolDefinition]) -> Context {
    let workspace = PathBuf::from("/workspace");
    let env = AgentEnv {
        working_directory: workspace.clone(),
        git_root_directory: Some(workspace),
        operating_system: std::env::consts::OS.to_string(),
        today_date: date.to_string(),
        system_prompt: SystemPrompt {
            content: aj_app::SYSTEM_PROMPT.to_string(),
            source: SystemPromptSource::Builtin,
        },
        context_files: Vec::new(),
        skills: Vec::new(),
        skill_diagnostics: Vec::new(),
    };
    Context {
        system_prompt: Some(aj_app::system_prompt::assemble_system_prompt(&env, false)),
        messages: vec![Message::User(UserMessage::text(prompt))],
        tools: tools.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use tokio::io::duplex;
    use tokio::sync::oneshot;

    use super::*;
    use aj_models::registry::{InputModality, ModelCost, ReasoningOption};
    use aj_models::streaming::DoneReason;

    fn model() -> ModelInfo {
        ModelInfo {
            id: "model".into(),
            name: "Model".into(),
            family: Some("gpt-test".into()),
            api: "ipc".into(),
            provider: "ipc".into(),
            base_url: String::new(),
            reasoning: true,
            reasoning_options: vec![ReasoningOption::Effort {
                values: vec![ThinkingLevel::Low],
            }],
            supports_verbosity: false,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 1000,
            max_tokens: 100,
        }
    }

    #[test]
    fn tool_details_deserialize_to_the_exact_serialized_variant() {
        let details = ToolDetails::Text {
            summary: "summary".into(),
            body: "body".into(),
        };
        let serialized = serde_json::to_value(&details).unwrap();
        let restored: ToolDetails = serde_json::from_value(serialized.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), serialized);
    }

    #[tokio::test]
    async fn ipc_provider_streams_success_and_captures_fixed_low_effort() {
        let (guest, parent) = duplex(64 * 1024);
        let (guest_read, guest_write) = tokio::io::split(guest);
        let (mut parent_read, mut parent_write) = tokio::io::split(parent);
        let client = IpcClient::new(guest_read, guest_write);
        let provider = IpcProvider::new(client, Arc::new(AtomicU32::new(0)));
        let parent_task = tokio::spawn(async move {
            let request: WorkerRequest = read_frame(&mut parent_read).await.unwrap().unwrap();
            let (id, reasoning, tools) = match request {
                WorkerRequest::Provider {
                    id,
                    observed_reasoning,
                    context,
                } => (id, observed_reasoning, context.tools),
                _ => panic!("expected provider request"),
            };
            assert_eq!(reasoning, ThinkingLevel::Low);
            assert_eq!(tools[0].name, "x");
            let mut message = AssistantMessage::empty();
            message.model = "model".into();
            write_frame(
                &mut parent_write,
                &ParentResponse::ProviderEvent {
                    id,
                    event: AssistantMessageEvent::Done {
                        reason: DoneReason::Stop,
                        message,
                    },
                },
            )
            .await
            .unwrap();
        });
        let mut context = Context::new("system");
        context.tools.push(aj_models::types::ToolDefinition {
            name: "x".into(),
            description: "x".into(),
            parameters: serde_json::json!({}),
        });
        let mut stream = provider.stream_simple(
            &model(),
            &context,
            &SimpleStreamOptions {
                base: StreamOptions::default(),
                reasoning: ThinkingLevel::Low,
            },
        );
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Done { .. })
        ));
        parent_task.await.unwrap();
    }

    #[tokio::test]
    async fn ipc_provider_turns_parent_failure_into_terminal_error() {
        let (guest, parent) = duplex(4096);
        let (guest_read, guest_write) = tokio::io::split(guest);
        let (mut parent_read, mut parent_write) = tokio::io::split(parent);
        let client = IpcClient::new(guest_read, guest_write);
        let provider = IpcProvider::new(client, Arc::new(AtomicU32::new(0)));
        tokio::spawn(async move {
            let request: WorkerRequest = read_frame(&mut parent_read).await.unwrap().unwrap();
            let id = match request {
                WorkerRequest::Provider { id, .. } => id,
                _ => unreachable!(),
            };
            write_frame(
                &mut parent_write,
                &ParentResponse::Failure {
                    id,
                    error: "transport failed".into(),
                },
            )
            .await
            .unwrap();
        });
        let mut stream = provider.stream_simple(
            &model(),
            &Context::new("system"),
            &SimpleStreamOptions {
                base: StreamOptions::default(),
                reasoning: ThinkingLevel::Low,
            },
        );
        assert!(matches!(
            stream.next().await,
            Some(AssistantMessageEvent::Error { .. })
        ));
    }

    #[tokio::test]
    async fn ipc_provider_cancellation_terminates_the_pending_request() {
        let (guest, parent) = duplex(4096);
        let (guest_read, guest_write) = tokio::io::split(guest);
        let (mut parent_read, _parent_write) = tokio::io::split(parent);
        let client = IpcClient::new(guest_read, guest_write);
        let provider = IpcProvider::new(client, Arc::new(AtomicU32::new(0)));
        let (started_tx, started_rx) = oneshot::channel();
        let parent_task = tokio::spawn(async move {
            let request: WorkerRequest = read_frame(&mut parent_read).await.unwrap().unwrap();
            assert!(matches!(request, WorkerRequest::Provider { .. }));
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let cancel = CancellationToken::new();
        let mut stream = provider.stream_simple(
            &model(),
            &Context::new("system"),
            &SimpleStreamOptions {
                base: StreamOptions {
                    cancel: Some(cancel.clone()),
                    ..StreamOptions::default()
                },
                reasoning: ThinkingLevel::Low,
            },
        );
        started_rx.await.unwrap();
        cancel.cancel();
        let event = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .unwrap()
            .unwrap();
        assert!(event.is_terminal());
        assert_eq!(event.partial().stop_reason, StopReason::Aborted);
        parent_task.abort();
    }
}
