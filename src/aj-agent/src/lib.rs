// Event-driven contract modules consumed by `aj`, `aj-session`,
// and the in-tree `aj` binary. The agent runtime in this file drives
// tools through the [`tool::ToolDefinition`] / [`tool::ToolContext`]
// surface and emits every state transition through its internal
// [`bus::EventBus`]; the binary subscribes a renderer listener and a
// persistence listener (the latter lives in `aj-session` per the
// dependency graph in `docs/aj-next-plan.md` §1) and owns the
// readline loop, log management, and history display.
pub mod bus;
pub mod events;
pub mod message;
mod projection;
pub mod tool;
pub mod types;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use aj_conf::{AgentEnv, ConfigThinkingLevel};
use aj_models::messages::{ApiError, ContentBlockParam, MessageParam, Role, Usage};
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::streaming::{AssistantMessageEvent, AssistantMessageEventStream};
use aj_models::tools::Tool;
use aj_models::types::{
    AssistantContent, AssistantMessage, Context, ErrorCategory, SimpleStreamOptions, StreamOptions,
    ThinkingLevel, ToolCall, ToolDefinition as UnifiedToolDefinition, UserContent,
};
use aj_models::ModelError;
use aj_models::ThinkingConfig;

use crate::bus::{EventBus, Listener, SubscriptionHandle};
use crate::events::{AgentEvent, AgentId, PersistedMessageKind, StreamAction, StreamChannel};
use crate::projection::{
    assistant_message_to_message_param, transcript_to_messages, usage_unified_to_legacy,
};
use crate::tool::{
    ErasedToolDefinition, SpawnedAgent, TodoItem, ToolContext, ToolDetails, ToolOutcome,
};
use crate::types::{TokenUsage, UserOutput};
use anyhow::anyhow;
use futures::StreamExt;
use std::sync::Arc;
use tokio_retry2::strategy::{jitter, ExponentialBackoff};
use tokio_util::sync::CancellationToken;

pub struct Agent {
    env: AgentEnv,
    /// The base system prompt template provided by the host
    /// (compile-time constant, ships with the binary). The full
    /// prompt sent to the model is derived from this plus
    /// environment-dependent context (`AgentEnv`); the binary
    /// resolves it once and pushes it onto the agent through
    /// [`Agent::set_assembled_system_prompt`] so resumed threads
    /// reuse the original assembly verbatim and keep hitting
    /// Anthropic's prompt cache.
    system_prompt: &'static str,
    /// The fully-assembled system prompt for the current run.
    /// Populated by [`Agent::set_assembled_system_prompt`] (resume
    /// path or fresh assembly) before any turn runs; inference
    /// reads it directly.
    assembled_system_prompt: Option<String>,
    tool_definitions: HashMap<String, ErasedToolDefinition>,
    tools: Vec<Tool>,
    /// Names of builtin tools to exclude when spawning subagents.
    /// Mirrors the filter applied to the top-level agent so
    /// subagents inherit the same tool restrictions.
    disabled_tools: Vec<String>,
    /// Unified provider handle used by the inference loop. Supplied
    /// directly by [`Agent::with_provider`] / [`Agent::set_provider`].
    /// The inference loop only ever reaches for this field; sub-agents
    /// inherit the same handle through [`SessionContextWrapper`].
    provider: Arc<dyn Provider>,
    /// Identity / capability metadata stamped onto the [`Context`]
    /// passed to [`Provider::stream_simple`]. Resolved against the
    /// model registry (or synthesised by a `LegacyProviderAdapter`
    /// wrap on the scripted path) and passed in alongside the
    /// provider handle at construction.
    model_info: Arc<ModelInfo>,
    /// Base [`StreamOptions`] applied to every inference call. Carries
    /// the resolved api key, per-call HTTP headers (e.g. an
    /// `anthropic-beta` line for the fast-mode beta), session
    /// correlation id, etc. The agent layers per-turn reasoning on
    /// top inside `run_inference_streaming`; everything else flows
    /// through verbatim.
    stream_options: StreamOptions,
    session_state: SessionState,
    default_thinking: Option<ThinkingConfig>,
    /// Identifier used on every event emitted by this agent. The
    /// top-level instance constructed by the binary keeps the
    /// default [`AgentId::Main`]; sub-agents created via
    /// [`SessionContextWrapper::spawn_agent`] override this so
    /// listeners can route nested transcripts.
    agent_id: AgentId,
    /// Internal event bus. Every state transition the agent goes
    /// through is mirrored here as an [`AgentEvent`]; the binary
    /// subscribes a renderer listener and a persistence listener
    /// (the latter lives in `aj_session::persistence_listener`).
    bus: EventBus,
    /// Cancellation token surfaced to tools through
    /// [`ToolContext::cancellation`]. Today the agent never fires
    /// it: cancellation propagation lands in §1.8 of
    /// `docs/aj-next-plan.md`, but the field is wired through now
    /// so tools observing `select!` against `ctx.cancellation()`
    /// compile cleanly.
    /// Sub-agents inherit a child token derived from their
    /// parent's per `docs/aj-next-plan.md` §1.6 so a single
    /// eventual `cancel()` call reaches the whole hierarchy.
    cancellation: CancellationToken,
    /// In-memory transcript: every wire-level [`MessageParam`] this
    /// agent has seen, in append order. Replaces the agent's reach
    /// into [`aj_session::ConversationLog`] (per
    /// `docs/aj-next-plan.md` §2.4b): the binary owns the log,
    /// resumes it on startup, and seeds the transcript via
    /// [`Agent::seed_messages`] before the first turn.
    transcript: Vec<MessageParam>,
}

impl Agent {
    /// Build an agent off a unified [`Provider`] handle.
    ///
    /// `model_info` is the registry-resolved metadata the agent
    /// stamps onto the [`Context`] passed to
    /// [`Provider::stream_simple`]; `stream_options` carries the
    /// resolved API key, per-call HTTP headers (`anthropic-beta`
    /// values, etc.), session id, and any other knobs the binary
    /// pre-computed at startup.
    ///
    /// Sub-agents spawned by this agent inherit the same `provider`,
    /// `model_info`, and `stream_options` (per
    /// `docs/aj-next-plan.md` §1.6) so the whole hierarchy talks to
    /// the same backend.
    pub fn with_provider(
        env: AgentEnv,
        system_prompt: &'static str,
        tools: Vec<ErasedToolDefinition>,
        disabled_tools: Vec<String>,
        provider: Arc<dyn Provider>,
        model_info: Arc<ModelInfo>,
        stream_options: StreamOptions,
        default_thinking: Option<ConfigThinkingLevel>,
    ) -> Self {
        // Convert ErasedToolDefinition to Tool for Model API
        let api_tools: Vec<Tool> = tools
            .iter()
            .map(|tool_def| Tool {
                name: tool_def.name.clone(),
                description: tool_def.description.clone(),
                input_schema: tool_def.input_schema.clone(),
                r#type: None,
            })
            .collect();

        // Convert ErasedToolDefinition to HashMap for lookup
        let tool_definitions: HashMap<String, ErasedToolDefinition> = tools
            .into_iter()
            .map(|tool_def| (tool_def.name.clone(), tool_def))
            .collect();

        let session_state = SessionState::new(env.working_directory.clone());

        let default_thinking = default_thinking.and_then(|level| match level {
            ConfigThinkingLevel::Off => None,
            ConfigThinkingLevel::Low => Some(ThinkingConfig::Low),
            ConfigThinkingLevel::Medium => Some(ThinkingConfig::Medium),
            ConfigThinkingLevel::High => Some(ThinkingConfig::High),
            ConfigThinkingLevel::XHigh => Some(ThinkingConfig::XHigh),
            ConfigThinkingLevel::Max => Some(ThinkingConfig::Max),
        });

        Self {
            env,
            system_prompt,
            assembled_system_prompt: None,
            tool_definitions,
            tools: api_tools,
            disabled_tools,
            provider,
            model_info,
            stream_options,
            session_state,
            default_thinking,
            agent_id: AgentId::Main,
            bus: EventBus::new(),
            cancellation: CancellationToken::new(),
            transcript: Vec::new(),
        }
    }

    /// Override this agent's [`AgentId`] before driving any turns.
    ///
    /// Used by [`SessionContextWrapper::spawn_agent`] when
    /// constructing a sub-agent so the events it emits carry the
    /// correct [`AgentId::Sub`] tag. Top-level instances built by
    /// the binary keep the default [`AgentId::Main`] and never
    /// call this.
    pub fn set_agent_id(&mut self, id: AgentId) {
        self.agent_id = id;
    }

    /// Replace this agent's event bus.
    ///
    /// Used by [`SessionContextWrapper::spawn_agent`] to make a
    /// sub-agent share the parent's bus per `docs/aj-next-plan.md`
    /// §1.6: every event the child emits then reaches every
    /// listener the binary registered on the parent (rendering,
    /// persistence, future TUI components), tagged by the child's
    /// [`AgentId::Sub`]. Must be called before any turn runs;
    /// subscriptions registered on the bus that's about to be
    /// replaced are silently dropped.
    pub fn set_bus(&mut self, bus: EventBus) {
        self.bus = bus;
    }

    /// Subscribe an async listener to the agent's internal event
    /// bus.
    ///
    /// Returns a [`SubscriptionHandle`] whose drop removes the
    /// listener. Listeners are awaited inline in registration
    /// order; a listener returning `Err` aborts the in-flight
    /// operation with a fatal error. See [`EventBus::subscribe`]
    /// for the full protocol.
    pub fn subscribe(&self, listener: Listener) -> SubscriptionHandle {
        self.bus.subscribe(listener)
    }

    /// Subscribe a channel-style sink for the agent's event bus.
    ///
    /// Registers a non-blocking forwarding listener that pushes
    /// each [`AgentEvent`] into a `tokio::sync::mpsc::UnboundedSender`
    /// and returns the matching receiver alongside the
    /// [`SubscriptionHandle`] that owns the subscription's
    /// lifetime. The listener returns `Ok(())` even if the
    /// receiver has been dropped, so a renderer that hangs up
    /// mid-run does not fail the agent's turn.
    ///
    /// This is the channel sugar from `docs/aj-next-plan.md` §1.4:
    /// the TUI's event pump uses it to decouple itself from the
    /// agent's emit timing. The agent never blocks on a slow
    /// renderer because the forwarder's `send` is non-blocking;
    /// renderers see events at their own pace by polling the
    /// receiver from a `tokio::select!`.
    pub fn subscribe_channel(
        &self,
    ) -> (
        SubscriptionHandle,
        tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = crate::bus::listener_from_sync(move |event: &AgentEvent| {
            // `send` only fails when the receiver has been dropped;
            // we treat that as the consumer no longer being
            // interested, not as an agent-level error. Dropping the
            // event keeps the agent's turn making progress.
            let _ = tx.send(event.clone());
        });
        let handle = self.bus.subscribe(listener);
        (handle, rx)
    }

    /// Borrow the agent's internal event bus.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    pub fn current_turn(&self) -> usize {
        self.session_state.turn_counter()
    }

    pub fn accumulated_usage(&self) -> &Usage {
        self.session_state.accumulated_usage()
    }

    /// Borrow the per-sub-agent accumulated [`Usage`] map. The
    /// binary uses this to compute the end-of-session usage summary
    /// (the agent no longer renders one — the binary owns
    /// presentation).
    pub fn sub_agent_usage(&self) -> &HashMap<usize, Usage> {
        &self.session_state.sub_agent_usage
    }

    /// Borrow the agent's in-memory transcript. The binary uses
    /// this on shutdown to decide whether to print the resume hint
    /// (only when the agent observed at least one message) and on
    /// each loop iteration to decide whether to ask for input
    /// (depending on whether the last message is from the
    /// assistant).
    pub fn messages(&self) -> &[MessageParam] {
        &self.transcript
    }

    /// Replace the in-memory transcript with `messages`. Used on
    /// resume: the binary opens the log, linearizes the user
    /// thread, and pushes the resulting `Vec<MessageParam>` into
    /// the agent so the next turn sees the prior conversation.
    pub fn seed_messages(&mut self, messages: Vec<MessageParam>) {
        self.transcript = messages;
    }

    /// Seed the sub-agent counter so subsequent
    /// [`SessionState::next_sub_agent_id`] calls mint ids strictly
    /// greater than `value`. Used on resume to avoid colliding with
    /// sub-agent subtrees already persisted in the log.
    pub fn seed_sub_agent_counter(&mut self, value: usize) {
        self.session_state.seed_sub_agent_counter(value);
    }

    /// Provide the freshly-assembled (or persisted) system prompt
    /// to the agent. Must be called before any turn runs; inference
    /// reads it directly. Idempotent: subsequent calls overwrite
    /// the previous value, but in practice the binary calls this
    /// exactly once per session.
    pub fn set_assembled_system_prompt(&mut self, prompt: String) {
        self.assembled_system_prompt = Some(prompt);
    }

    /// Borrow the assembled system prompt. Returns `None` until
    /// [`Agent::set_assembled_system_prompt`] runs.
    pub fn assembled_system_prompt(&self) -> Option<&str> {
        self.assembled_system_prompt.as_deref()
    }

    /// Assemble the system prompt from the constant template plus
    /// the agent's environment context. The binary calls this when
    /// no persisted prompt exists on the log and uses the result
    /// as the freshly-frozen system prompt for the session.
    pub fn assemble_system_prompt(&self) -> String {
        let mut text = self.system_prompt.to_string();

        // Stitch in every context file, in order. Each file is
        // wrapped in an `<agents-md>` block so the model can
        // clearly tell where instructions start and end, with the
        // kind-specific prefix text introducing it.
        for file in &self.env.context_files {
            text.push_str(&format!(
                "\n\n{}\n<agents-md>\n{}\n</agents-md>",
                file.kind.prompt_prefix(),
                file.content
            ));
        }

        text.push_str(&format!(
            "\n\nHere's useful information about your environment:\n<env>\n{}\n</env>",
            self.env
        ));

        text
    }

    /// Borrow the registry-resolved [`ModelInfo`] this agent is
    /// currently running against.
    ///
    /// The TUI footer renders `id @ base_url` off this handle so the
    /// scripted and real-provider paths render identically. The
    /// `--scripted` flag synthesises a minimal [`ModelInfo`] off the
    /// legacy [`Model`](aj_models::Model) handle via
    /// [`aj_models::compat::synthetic_model_info`]; real providers
    /// see the registry entry the binary plucked out at startup.
    ///
    /// [`ModelRegistry`]: aj_models::registry::ModelRegistry
    pub fn model_info(&self) -> Arc<ModelInfo> {
        Arc::clone(&self.model_info)
    }

    /// Replace the provider handle, model metadata, and per-call
    /// [`StreamOptions`] mid-session.
    ///
    /// Used by the interactive `/model` selector to swap to a fresh
    /// registry entry without restarting the session. Takes effect on
    /// the next inference; in-flight turns keep their old handle,
    /// sub-agents spawned after the call see the new one.
    pub fn set_provider(
        &mut self,
        provider: Arc<dyn Provider>,
        model_info: Arc<ModelInfo>,
        stream_options: StreamOptions,
    ) {
        self.provider = provider;
        self.model_info = model_info;
        self.stream_options = stream_options;
    }

    /// Borrow the agent's environment. The binary uses this to
    /// render the startup `Context:` notice listing every
    /// agents.md file injected into the system prompt.
    pub fn env(&self) -> &AgentEnv {
        &self.env
    }

    /// Borrow the agent's current default thinking configuration.
    ///
    /// `None` means "no extended thinking unless a trigger word in
    /// the user's message escalates it". The selector overlays in
    /// the new TUI read this to highlight the active level when
    /// opening; the binary passes it into the footer for the
    /// startup banner.
    pub fn default_thinking(&self) -> Option<ThinkingConfig> {
        self.default_thinking.clone()
    }

    /// Replace the agent's default thinking configuration mid-
    /// session.
    ///
    /// Used by the interactive `/thinking` selector to retune the
    /// reasoning budget without restarting the session. Takes
    /// effect on the next inference; in-flight turns continue with
    /// whatever they were already configured for.
    pub fn set_default_thinking(&mut self, level: Option<ThinkingConfig>) {
        self.default_thinking = level;
    }

    /// Append `message` as a user-role text input to the transcript
    /// and run one assistant turn against it.
    ///
    /// Emits [`AgentEvent::MessagePersisted::User`] before driving
    /// the assistant turn loop so the persistence listener writes
    /// the user's input to disk. The turn loop runs one inference,
    /// processes any tool calls the assistant emits (each with its
    /// own [`AgentEvent::ToolExecutionStart`] /
    /// [`AgentEvent::ToolExecutionEnd`] bracket), and loops until
    /// the assistant produces a non-tool turn.
    ///
    /// The whole call is bracketed by [`AgentEvent::AgentStart`] /
    /// [`AgentEvent::AgentEnd`] events tagged with this agent's id
    /// so listeners can scope nested transcripts.
    pub async fn prompt(&mut self, message: String) -> Result<(), TurnError> {
        self.run_top_level_turn(Some(message)).await
    }

    /// Run one assistant turn against the existing transcript
    /// without appending a new user message.
    ///
    /// Used after a recoverable turn error, when the user's input
    /// (or the synthesized tool_result message that closed the
    /// previous tool batch) is already in the transcript and we
    /// want to retry inference without re-injecting a prompt. The
    /// transcript must end in a user-role message; calling this
    /// against an assistant-role last message would send the model
    /// an invalid request and is treated as a fatal misuse here.
    ///
    /// Like [`Agent::prompt`], the call is bracketed by
    /// [`AgentEvent::AgentStart`] / [`AgentEvent::AgentEnd`] events.
    pub async fn continue_run(&mut self) -> Result<(), TurnError> {
        match self.transcript.last() {
            Some(msg) if matches!(msg.role, Role::User) => {}
            _ => {
                return Err(TurnError::Fatal(anyhow!(
                    "continue_run requires the transcript to end in a user-role message"
                )));
            }
        }
        self.run_top_level_turn(None).await
    }

    /// Shared driver for [`Agent::prompt`] / [`Agent::continue_run`].
    ///
    /// `prompt` is `Some` for [`Agent::prompt`] (a fresh user
    /// message is appended before inference) and `None` for
    /// [`Agent::continue_run`] (the existing transcript is fed back
    /// to the model unchanged).
    async fn run_top_level_turn(&mut self, prompt: Option<String>) -> Result<(), TurnError> {
        // Mirror the run as `AgentStart` / `AgentEnd` events on the
        // bus. `AgentEnd.messages` will eventually carry a snapshot
        // of the agent's transcript per `docs/aj-next-plan.md` §1.4;
        // until §2.4 migrates the agent to the unified message
        // types, we ship an empty snapshot so the protocol shape
        // (event ordering, agent_id routing) is exercised without
        // forcing a premature legacy→unified bridge.
        self.bus
            .emit(AgentEvent::AgentStart {
                agent_id: self.agent_id,
            })
            .await
            .map_err(TurnError::Fatal)?;

        let outcome = self.run_top_level_turn_inner(prompt).await;

        self.bus
            .emit(AgentEvent::AgentEnd {
                agent_id: self.agent_id,
                messages: Vec::new(),
            })
            .await
            .map_err(TurnError::Fatal)?;

        outcome
    }

    async fn run_top_level_turn_inner(&mut self, prompt: Option<String>) -> Result<(), TurnError> {
        if let Some(text) = prompt {
            // Append the user message to the in-memory transcript
            // and emit a `MessagePersisted::User` event for the
            // persistence listener. The transcript update happens
            // unconditionally — even if the listener errors out we
            // still want the in-memory state to reflect the
            // intent, so the bus call is at-most one event behind
            // the transcript (per `docs/aj-next-plan.md` §1.4).
            let content = vec![ContentBlockParam::new_text_block(text)];
            self.transcript
                .push(MessageParam::new_user_message(content.clone()));
            self.bus
                .emit(AgentEvent::MessagePersisted {
                    agent_id: self.agent_id,
                    kind: PersistedMessageKind::User { content },
                })
                .await
                .map_err(TurnError::Fatal)?;
        }

        self.execute_turn().await
    }

    /// Run a single sub-agent turn. Used internally by the `agent`
    /// builtin via [`ToolContext::spawn_agent`]; not for top-level
    /// use.
    ///
    /// Appends `prompt` as a user message on the sub-agent's own
    /// transcript, runs the assistant turn loop, and returns the
    /// final assistant text the sub-agent produced.
    pub async fn run_single_turn(&mut self, prompt: String) -> Result<String, anyhow::Error> {
        // Sub-agent runs share the same lifecycle framing as the
        // top-level agent — `AgentStart` / `AgentEnd` events
        // bracket the entire run so listeners that group by
        // `agent_id` see a self-contained nested transcript.
        self.bus
            .emit(AgentEvent::AgentStart {
                agent_id: self.agent_id,
            })
            .await?;

        let outcome = self.run_single_turn_inner(prompt).await;

        self.bus
            .emit(AgentEvent::AgentEnd {
                agent_id: self.agent_id,
                messages: Vec::new(),
            })
            .await?;

        outcome
    }

    async fn run_single_turn_inner(&mut self, prompt: String) -> Result<String, anyhow::Error> {
        // Append the prompt as the sub-agent's first user message.
        // The persistence listener anchors this entry under the
        // parent's spawning assistant message via the
        // `SubAgentStart` hook (see
        // `aj_session::listener::persistence_listener`).
        let content = vec![ContentBlockParam::new_text_block(prompt)];
        self.transcript
            .push(MessageParam::new_user_message(content.clone()));
        self.bus
            .emit(AgentEvent::MessagePersisted {
                agent_id: self.agent_id,
                kind: PersistedMessageKind::User { content },
            })
            .await?;

        self.execute_turn().await?;

        // Extract the last assistant message text from the
        // sub-agent's own transcript.
        let last_msg = self
            .transcript
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::Assistant))
            .ok_or_else(|| anyhow!("sub-agent produced no assistant text output"))?;

        let last_assistant_text: String = last_msg
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlockParam::TextBlock { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();

        Ok(last_assistant_text)
    }

    /// Execute one assistant-message turn against the in-memory
    /// transcript: run inference, process any tool calls, append
    /// each result, loop until the assistant produces a non-tool
    /// turn.
    ///
    /// The turn's writes (assistant message, per-tool user
    /// outputs, tool-result user message) flow out as
    /// [`AgentEvent::MessagePersisted`] events. The persistence
    /// listener subscribed on the bus translates them into
    /// `aj_session::ConversationView` appends, one JSONL line per
    /// event, so the on-disk state stays at-most one event behind
    /// reality (see `docs/aj-next-plan.md` §2.3b).
    async fn execute_turn(&mut self) -> Result<(), TurnError> {
        self.session_state.turn_counter += 1;

        // `TurnStart` mirrors entry to the assistant-message cycle.
        // The matching `TurnEnd` event (which carries the finalized
        // assistant message and tool-result list per
        // `docs/aj-next-plan.md` §1.1) lands in §2.4 once
        // `aj-agent` migrates to the unified message types.
        self.bus
            .emit(AgentEvent::TurnStart {
                agent_id: self.agent_id,
            })
            .await
            .map_err(TurnError::Fatal)?;

        // Number of streaming retries observed for the current
        // inference. Reported on `StreamRetry` events so listeners
        // can render "retrying… (attempt N)" indicators.
        let mut retry_attempt: u32 = 0;
        let mut retry_strategy = None;

        'outer: loop {
            let mut response_stream = self.run_inference_streaming();

            // Terminal `AssistantMessage` captured from the stream's
            // `Done` (success) or `Error` (failure) event. The
            // unified streaming protocol guarantees exactly one
            // terminal event per stream, so once this is `Some` we
            // break out and stop polling.
            let mut final_message: Option<AssistantMessage> = None;
            let mut final_was_error = false;

            while let Some(event) = response_stream.next().await {
                match event {
                    AssistantMessageEvent::Start { .. } => {
                        // The leading `Start` event carries the
                        // identity-stamped empty partial; no
                        // listener-facing emit is required (the
                        // renderer opens its assistant slot once the
                        // first content block arrives below).
                    }
                    AssistantMessageEvent::TextStart { .. } => {
                        // Open the text slot for the renderer with an
                        // empty snapshot. The unified protocol carries
                        // initial text on a follow-up `TextDelta`
                        // rather than on `TextStart` itself, so the
                        // renderer appends as it goes.
                        self.bus
                            .emit(AgentEvent::StreamChunk {
                                agent_id: self.agent_id,
                                channel: StreamChannel::Text,
                                action: StreamAction::Start {
                                    snapshot: String::new(),
                                },
                            })
                            .await
                            .map_err(TurnError::Fatal)?;
                    }
                    AssistantMessageEvent::TextDelta { delta, .. } => {
                        self.bus
                            .emit(AgentEvent::StreamChunk {
                                agent_id: self.agent_id,
                                channel: StreamChannel::Text,
                                action: StreamAction::Update { delta },
                            })
                            .await
                            .map_err(TurnError::Fatal)?;
                    }
                    AssistantMessageEvent::TextEnd { content, .. } => {
                        self.bus
                            .emit(AgentEvent::StreamChunk {
                                agent_id: self.agent_id,
                                channel: StreamChannel::Text,
                                action: StreamAction::Stop { snapshot: content },
                            })
                            .await
                            .map_err(TurnError::Fatal)?;
                    }
                    AssistantMessageEvent::ThinkingStart { .. } => {
                        self.bus
                            .emit(AgentEvent::StreamChunk {
                                agent_id: self.agent_id,
                                channel: StreamChannel::Thinking,
                                action: StreamAction::Start {
                                    snapshot: String::new(),
                                },
                            })
                            .await
                            .map_err(TurnError::Fatal)?;
                    }
                    AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                        self.bus
                            .emit(AgentEvent::StreamChunk {
                                agent_id: self.agent_id,
                                channel: StreamChannel::Thinking,
                                action: StreamAction::Update { delta },
                            })
                            .await
                            .map_err(TurnError::Fatal)?;
                    }
                    AssistantMessageEvent::ThinkingEnd { content, .. } => {
                        self.bus
                            .emit(AgentEvent::StreamChunk {
                                agent_id: self.agent_id,
                                channel: StreamChannel::Thinking,
                                action: StreamAction::Stop { snapshot: content },
                            })
                            .await
                            .map_err(TurnError::Fatal)?;
                    }
                    AssistantMessageEvent::ToolCallStart { .. }
                    | AssistantMessageEvent::ToolCallDelta { .. }
                    | AssistantMessageEvent::ToolCallEnd { .. } => {
                        // Argument-echo deltas during streaming are
                        // not surfaced as events today: the agent
                        // collects tool calls off the finalized
                        // `Done { message }` payload below, then
                        // brackets each invocation with its own
                        // `ToolExecutionStart` / `ToolExecutionEnd`
                        // pair. Live tool-argument echo can be added
                        // by emitting `ToolExecutionUpdate` here once
                        // a renderer needs it.
                    }
                    AssistantMessageEvent::Done { message, .. } => {
                        final_message = Some(message);
                        final_was_error = false;
                        break;
                    }
                    AssistantMessageEvent::Error { reason, error } => {
                        let _ = reason;
                        final_message = Some(error);
                        final_was_error = true;
                        break;
                    }
                }
            }

            // Drain any trailing events the producer queued after the
            // terminal one (the stream contract drops them, but the
            // explicit drop here frees the channel sooner) and fall
            // back to `result()` if we never observed a terminal
            // event in the loop above (channel closed silently —
            // `result()` then synthesizes a transient error).
            let final_message = match final_message {
                Some(m) => m,
                None => {
                    // The stream ended without emitting Done / Error;
                    // pull the synthesized terminal from the
                    // side-channel.
                    final_was_error = true;
                    response_stream.result().await
                }
            };
            drop(response_stream);

            if final_was_error {
                let assistant_err = final_message.error.clone();
                // Retry strictly on `Overloaded` to match the legacy
                // agent's behavior; other retryable categories
                // (RateLimit, Transient) bubble up as recoverable
                // errors today.
                let is_overloaded = assistant_err
                    .as_ref()
                    .is_some_and(|e| e.category == ErrorCategory::Overloaded);
                if is_overloaded {
                    if retry_strategy.is_none() {
                        retry_strategy = Some(Self::create_retry_strategy());
                    }
                    let retry_sleep = retry_strategy.as_mut().expect("known to be some").next();
                    if let Some(retry_sleep) = retry_sleep {
                        let err_text = assistant_err
                            .as_ref()
                            .map(|e| e.message.clone())
                            .unwrap_or_else(|| "overloaded".to_string());
                        let message =
                            format!("{err_text}, retrying in {}s...", retry_sleep.as_secs(),);
                        self.bus
                            .emit(AgentEvent::Error {
                                agent_id: self.agent_id,
                                text: message,
                            })
                            .await
                            .map_err(TurnError::Fatal)?;
                        retry_attempt = retry_attempt.saturating_add(1);
                        self.bus
                            .emit(AgentEvent::StreamRetry {
                                agent_id: self.agent_id,
                                attempt: retry_attempt,
                                delay: retry_sleep,
                                error: err_text,
                            })
                            .await
                            .map_err(TurnError::Fatal)?;
                        tokio::time::sleep(retry_sleep).await;
                        continue 'outer;
                    }
                }

                // Non-retryable / retry-exhausted: surface a
                // recoverable turn error so the binary keeps the
                // session alive and the user can re-prompt.
                let detail = assistant_err
                    .map(|e| e.message)
                    .unwrap_or_else(|| "model stream failed without details".to_string());
                return Err(TurnError::Recoverable(anyhow!(detail)));
            }

            // Reset the retry budget after a successful inference.
            retry_strategy = None;
            retry_attempt = 0;

            let response = final_message;
            let turn_usage_unified = response.usage.clone();
            let turn_usage = usage_unified_to_legacy(&turn_usage_unified);

            // Collect tool calls off the finalized assistant
            // content. The unified `AssistantContent::ToolCall`
            // variant carries `(id, name, arguments)` directly; we
            // mirror the legacy tuple shape so the rest of the loop
            // can stay byte-for-byte identical to the pre-flip flow.
            let tool_calls: Vec<(String, String, serde_json::Value)> = response
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::ToolCall(ToolCall {
                        id,
                        name,
                        arguments,
                    }) => Some((id.clone(), name.clone(), arguments.clone())),
                    _ => None,
                })
                .collect();
            let has_tool_use = !tool_calls.is_empty();

            // Re-project the finalized unified assistant message
            // onto the agent's transcript shape (flat
            // [`ContentBlockParam`] content). The bus emit happens
            // after the transcript update so a listener that errors
            // out can't leave the transcript ahead of the log (the
            // listener failure is fatal anyway — see
            // [`TurnError::Fatal`]).
            let message_param = assistant_message_to_message_param(&response);
            self.transcript.push(message_param.clone());
            self.bus
                .emit(AgentEvent::MessagePersisted {
                    agent_id: self.agent_id,
                    kind: PersistedMessageKind::Assistant {
                        content: message_param.content,
                    },
                })
                .await
                .map_err(TurnError::Fatal)?;

            let usage = TokenUsage {
                accumulated_input: self.session_state.accumulated_usage.input_tokens,
                turn_input: turn_usage.input_tokens,
                accumulated_output: self.session_state.accumulated_usage.output_tokens,
                turn_output: turn_usage.output_tokens,
                accumulated_cache_creation: self
                    .session_state
                    .accumulated_usage
                    .cache_creation_input_tokens
                    .unwrap_or(0),
                turn_cache_creation: turn_usage.cache_creation_input_tokens.unwrap_or(0),
                accumulated_cache_read: self
                    .session_state
                    .accumulated_usage
                    .cache_read_input_tokens
                    .unwrap_or(0),
                turn_cache_read: turn_usage.cache_read_input_tokens.unwrap_or(0),
            };
            self.bus
                .emit(AgentEvent::TurnUsage {
                    agent_id: self.agent_id,
                    usage,
                })
                .await
                .map_err(TurnError::Fatal)?;

            self.session_state.accumulated_usage.add_usage(&turn_usage);

            // Execute tool calls if any
            if has_tool_use {
                let mut tool_result_contents = Vec::new();
                // Mirror of `tool_result_contents` for the
                // structured renderer payload: each loop iteration
                // pushes one wire block and one matching
                // `(tool_use_id, ToolDetails)` entry here, so the
                // batched `MessagePersisted::ToolResult` event at
                // the end of the loop can persist both halves on
                // the same record. Keyed by `tool_use_id` so the
                // persistence layer can correlate without scanning
                // the content vector.
                let mut tool_result_details: HashMap<String, ToolDetails> = HashMap::new();

                for (tool_id, tool_name, tool_input) in tool_calls {
                    // Mirror the start of every tool invocation on the
                    // bus before we do any work — listeners that render
                    // a "running…" placeholder rely on seeing this
                    // event before any update or end.
                    self.bus
                        .emit(AgentEvent::ToolExecutionStart {
                            agent_id: self.agent_id,
                            call_id: tool_id.clone(),
                            tool: tool_name.clone(),
                            args: tool_input.clone(),
                        })
                        .await
                        .map_err(TurnError::Fatal)?;

                    // Run the tool. Success and recoverable error
                    // both come back as a [`ToolOutcome`] the rest
                    // of the loop projects onto the wire
                    // `tool_result` block (text-flattened `content`)
                    // and the bus [`AgentEvent::ToolExecutionEnd`]
                    // event (structured `details` + `is_error`).
                    //
                    // Tool-input parse failures in the unified
                    // streaming protocol surface as a
                    // [`AssistantContent::ToolCall`] with
                    // `arguments == Value::Null`; the tool's own
                    // deserializer rejects the payload, the call
                    // bubbles up here as an `Err`, and the synthetic
                    // `UserOutput::ToolError` persistence event
                    // mirrors the legacy on-disk shape regardless of
                    // whether the failure was a wire parse error or
                    // a per-tool validation rejection.
                    let outcome = match self
                        .execute_tool(&tool_id, &tool_name, tool_input.clone())
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            self.bus
                                .emit(AgentEvent::MessagePersisted {
                                    agent_id: self.agent_id,
                                    kind: PersistedMessageKind::UserOutput {
                                        output: UserOutput::ToolError {
                                            tool_name: tool_name.clone(),
                                            input: tool_input.to_string(),
                                            error: err.to_string(),
                                        },
                                    },
                                })
                                .await
                                .map_err(TurnError::Fatal)?;

                            let body = err.to_string();
                            ToolOutcome {
                                content: vec![UserContent::text(format!("{err}"))],
                                details: ToolDetails::Text {
                                    summary: format!("{tool_name}: error"),
                                    body,
                                },
                                is_error: true,
                            }
                        }
                    };

                    // Project the structured `content` onto the
                    // legacy text-only `ToolResultContent::Text`
                    // shape for the wire. The structured `details`
                    // ride twice: once on the per-call
                    // [`AgentEvent::ToolExecutionEnd`] event below
                    // (for live renderers), and once into
                    // `tool_result_details` (keyed by `tool_use_id`)
                    // so the batched [`PersistedMessageKind::ToolResult`]
                    // event at the end of the loop carries the same
                    // per-call payload through to persistence —
                    // letting a resumed session rehydrate the
                    // structured renderer state instead of falling
                    // back to the wire text-only projection.
                    let return_value: String = outcome
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            UserContent::Text(t) => Some(t.text.as_str()),
                            UserContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");

                    let result_content_block = ContentBlockParam::ToolResultBlock {
                        tool_use_id: tool_id.to_owned(),
                        content: return_value.into(),
                        is_error: outcome.is_error,
                    };

                    tool_result_contents.push(result_content_block);
                    tool_result_details.insert(tool_id.clone(), outcome.details.clone());

                    self.bus
                        .emit(AgentEvent::ToolExecutionEnd {
                            agent_id: self.agent_id,
                            call_id: tool_id.clone(),
                            tool: tool_name.clone(),
                            result: outcome.details,
                            is_error: outcome.is_error,
                        })
                        .await
                        .map_err(TurnError::Fatal)?;
                }

                if !tool_result_contents.is_empty() {
                    // Append the synthesized user-role
                    // tool_result message to the transcript and
                    // mirror it on the bus.
                    self.transcript
                        .push(MessageParam::new_user_message(tool_result_contents.clone()));
                    self.bus
                        .emit(AgentEvent::MessagePersisted {
                            agent_id: self.agent_id,
                            kind: PersistedMessageKind::ToolResult {
                                content: tool_result_contents,
                                details: tool_result_details,
                            },
                        })
                        .await
                        .map_err(TurnError::Fatal)?;
                }

                // Continue the conversation loop to get the model's
                // response to tool results.
                continue;
            } else {
                // We are now ready to finish this turn. Every event
                // that belongs to this turn has already been
                // emitted individually; there is no per-turn save.
                break;
            }
        }

        Ok(())
    }

    /// Creates a retry strategy for handling overloaded API errors.
    fn create_retry_strategy() -> impl Iterator<Item = Duration> {
        ExponentialBackoff::from_millis(100)
            .max_delay(Duration::from_secs(2))
            .take(10)
            .map(jitter)
    }

    /// Run a single streaming inference against the agent's
    /// in-memory transcript and return the resulting
    /// [`AssistantMessageEventStream`].
    ///
    /// Projects the legacy [`MessageParam`] transcript onto the
    /// unified [`aj_models::types::Message`] sequence the
    /// [`Provider`] trait expects, projects the agent's
    /// `Vec<Tool>` onto the unified
    /// [`aj_models::types::ToolDefinition`] shape, builds a
    /// [`Context`] / [`SimpleStreamOptions`] pair, and hands them
    /// to [`Provider::stream_simple`]. The agent does not block
    /// on the stream here: it's returned to the caller, which
    /// polls it inside [`Self::execute_turn`]'s outer retry loop.
    fn run_inference_streaming(&self) -> AssistantMessageEventStream {
        let thinking = self.determine_thinking();

        tracing::debug!(?thinking, "thinking budget");

        let system_prompt = self
            .assembled_system_prompt
            .clone()
            .expect("system prompt must be resolved before inference");

        let messages = transcript_to_messages(&self.transcript);
        let tools: Vec<UnifiedToolDefinition> = self
            .tools
            .iter()
            .map(|t| UnifiedToolDefinition {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            })
            .collect();

        let context = Context {
            system_prompt: Some(system_prompt),
            messages,
            tools,
        };

        let options = SimpleStreamOptions {
            base: self.stream_options.clone(),
            reasoning: thinking.as_ref().map(thinking_config_to_level),
        };

        self.provider
            .stream_simple(&self.model_info, &context, &options)
    }

    /// Determine the thinking configuration based on trigger texts in the user
    /// prompt. Returns thinking configuration based on specific trigger phrases:
    /// - "think maximum" -> 128,000 tokens
    /// - "think harder" -> 32,000 tokens
    /// - "think hard" -> 10,000 tokens
    /// - "think" -> 4,000 tokens
    /// - default -> falls back to configured default thinking level
    fn determine_thinking(&self) -> Option<ThinkingConfig> {
        // Walk back through the transcript for the most recent
        // user-role message that contained any text content.
        // (Tool-result-only user messages don't count: they're
        // synthesized after a tool batch and shouldn't influence
        // thinking budget for the follow-up inference.)
        let last_user_text = self
            .transcript
            .iter()
            .rev()
            .find_map(|message| match message.role {
                Role::User => {
                    let has_text_input = message
                        .content
                        .iter()
                        .any(|c| matches!(c, ContentBlockParam::TextBlock { .. }));
                    if has_text_input {
                        Some(message)
                    } else {
                        None
                    }
                }
                _ => None,
            });

        if let Some(message) = last_user_text {
            // Extract text content from the message
            let text_content = message
                .content
                .iter()
                .filter_map(|content| {
                    if let ContentBlockParam::TextBlock { text, .. } = content {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");

            let text_lower = text_content.to_lowercase();

            // Check for trigger phrases in order of specificity.
            if text_lower.contains("think maximum") {
                return Some(ThinkingConfig::Max);
            } else if text_lower.contains("think hardest") {
                return Some(ThinkingConfig::XHigh);
            } else if text_lower.contains("think harder") {
                return Some(ThinkingConfig::High);
            } else if text_lower.contains("think hard") {
                return Some(ThinkingConfig::Medium);
            } else if text_lower.contains("think") {
                return Some(ThinkingConfig::Low);
            }
        }

        // No trigger word found; fall back to the configured default.
        self.default_thinking.clone()
    }

    async fn execute_tool(
        &mut self,
        _call_id: &str,
        tool_name: &str,
        tool_input: serde_json::Value,
    ) -> Result<ToolOutcome, anyhow::Error> {
        let tool_def = if let Some(tool_def) = self.tool_definitions.get(tool_name) {
            tool_def
        } else {
            return Err(anyhow!("tool not found!"));
        };

        // Build the sub-agent tool template now (cheap clone: every
        // `ErasedToolDefinition` field is `Clone`, with the closure
        // sitting behind an `Arc`). Doing this before borrowing
        // `self` mutably for the wrapper avoids field-aliasing
        // borrows.
        let sub_agent_tools: Vec<ErasedToolDefinition> =
            self.tool_definitions.values().cloned().collect();

        // Build the [`ToolContext`] the tool sees: working
        // directory, todos, sub-agent spawn, cancellation token,
        // no-op `emit_update`. After §2.4b the wrapper no longer
        // touches the conversation log; sub-agent persistence is
        // anchored via the `SubAgentStart` event the wrapper emits
        // before the child runs.
        let mut session_ctx_wrapper = SessionContextWrapper {
            session_ctx: &mut self.session_state,
            env: &self.env,
            assembled_system_prompt: self
                .assembled_system_prompt
                .clone()
                .expect("system prompt must be resolved before sub-agent spawn"),
            system_prompt: self.system_prompt,
            disabled_tools: &self.disabled_tools,
            provider: Arc::clone(&self.provider),
            model_info: Arc::clone(&self.model_info),
            stream_options: self.stream_options.clone(),
            sub_agent_tools,
            parent_bus: self.bus.clone(),
            parent_agent_id: self.agent_id,
            cancellation: self.cancellation.child_token(),
        };

        let outcome = (tool_def.func)(&mut session_ctx_wrapper, tool_input).await?;
        Ok(outcome)
    }
}

/// Mutable state of an [`Agent`] session.
#[derive(Debug)]
pub struct SessionState {
    working_directory: PathBuf,
    todo_list: Vec<TodoItem>,
    turn_counter: usize,
    accumulated_usage: Usage,
    sub_agent_counter: usize,
    sub_agent_usage: HashMap<usize, Usage>,
}

impl SessionState {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            todo_list: Vec::new(),
            turn_counter: 0,
            accumulated_usage: Usage::default(),
            sub_agent_counter: 0,
            sub_agent_usage: HashMap::new(),
        }
    }

    fn working_directory(&self) -> PathBuf {
        self.working_directory.to_owned()
    }

    fn get_todo_list(&self) -> Vec<TodoItem> {
        self.todo_list.clone()
    }

    fn set_todo_list(&mut self, todos: Vec<TodoItem>) {
        self.todo_list = todos;
    }

    pub fn turn_counter(&self) -> usize {
        self.turn_counter
    }

    pub fn accumulated_usage(&self) -> &Usage {
        &self.accumulated_usage
    }

    fn next_sub_agent_id(&mut self) -> usize {
        self.sub_agent_counter += 1;
        self.sub_agent_counter
    }

    /// Seed the subagent counter to `value` so subsequent
    /// [`SessionState::next_sub_agent_id`] calls mint ids strictly
    /// greater than `value`. Used on resume to avoid colliding
    /// with subagent subtrees already persisted in the log.
    fn seed_sub_agent_counter(&mut self, value: usize) {
        self.sub_agent_counter = value;
    }

    fn record_sub_agent_usage(&mut self, agent_id: usize, usage: Usage) {
        self.sub_agent_usage.insert(agent_id, usage);
    }
}

/// Wrapper that provides partial access to mutable [`Agent`] state,
/// while we have partial immutable access to other parts. Used in
/// [`Agent::execute_tool`].
struct SessionContextWrapper<'a> {
    session_ctx: &'a mut SessionState,
    env: &'a AgentEnv,
    /// The fully-assembled system prompt for the current run,
    /// captured at the moment the tool is invoked. Sub-agents
    /// spawned through this wrapper inherit it verbatim so the
    /// session has a single, consistent system prompt across all
    /// agents.
    assembled_system_prompt: String,
    system_prompt: &'static str,
    disabled_tools: &'a [String],
    /// Unified provider handle threaded into sub-agents. Cloned from
    /// the parent's handle so the whole hierarchy talks to the same
    /// backend per `docs/aj-next-plan.md` §1.6.
    provider: Arc<dyn Provider>,
    model_info: Arc<ModelInfo>,
    stream_options: StreamOptions,
    /// Snapshot of the parent's tool list. Sub-agents inherit this
    /// minus the `agent` tool. Cloning per-spawn is cheap because
    /// every `ErasedToolDefinition` field is `Clone` and the
    /// closure is `Arc`-shared.
    sub_agent_tools: Vec<ErasedToolDefinition>,
    /// Clone of the parent agent's event bus. Sub-agents share
    /// this bus per `docs/aj-next-plan.md` §1.6 so every event a
    /// child emits reaches the listeners the binary registered on
    /// the parent, tagged with [`AgentId::Sub`]. The wrapper also
    /// emits [`AgentEvent::SubAgentStart`] / [`AgentEvent::SubAgentEnd`]
    /// correlation events on this bus before / after the child
    /// runs.
    parent_bus: EventBus,
    /// Identifier of the parent agent that owns this wrapper. The
    /// `parent` field of [`AgentEvent::SubAgentStart`] /
    /// [`AgentEvent::SubAgentEnd`].
    parent_agent_id: AgentId,
    /// Cancellation token surfaced through
    /// [`ToolContext::cancellation`]. Derived from the parent
    /// agent's token via [`CancellationToken::child_token`] so a
    /// future `Agent::cancel` reaches in-flight tools and any
    /// sub-agents they spawn.
    cancellation: CancellationToken,
}

impl<'a> ToolContext for SessionContextWrapper<'a> {
    fn working_directory(&self) -> PathBuf {
        self.session_ctx.working_directory()
    }

    fn get_todo_list(&self) -> Vec<TodoItem> {
        self.session_ctx.get_todo_list()
    }

    fn set_todo_list(&mut self, todos: Vec<TodoItem>) {
        self.session_ctx.set_todo_list(todos);
    }

    fn spawn_agent<'b>(
        &'b mut self,
        task: String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<SpawnedAgent>> + Send + 'b>,
    > {
        Box::pin(async move {
            // Get the next agent ID
            let agent_id = self.session_ctx.next_sub_agent_id();
            let child_id = AgentId::Sub(agent_id);

            // Mirror sub-agent lifecycle on the parent's bus so a
            // single listener (e.g. the future TUI's event pump,
            // or the persistence listener that needs to capture
            // the parent anchor for the child's first write) can
            // group nested transcripts under the parent's
            // tool-execution component.
            self.parent_bus
                .emit(AgentEvent::SubAgentStart {
                    parent: self.parent_agent_id,
                    child: child_id,
                    task: task.clone(),
                })
                .await?;

            // Build the sub-agent's tool list by cloning the
            // parent's (the toolset is filtered upstream when the
            // binary calls `Agent::with_provider`), then dropping
            // the `agent` tool itself to prevent infinite recursion.
            // We clone rather than re-call `get_builtin_tools` so
            // `aj-agent` doesn't depend on `aj-tools`.
            let disabled_tools = self.disabled_tools.to_vec();
            let sub_agent_tools: Vec<ErasedToolDefinition> = self
                .sub_agent_tools
                .iter()
                .filter(|tool| tool.name != "agent")
                .cloned()
                .collect();

            // Create a new agent rooted in this session's env and
            // tools. Its transcript starts empty; the prompt the
            // tool invoked us with is appended as the first user
            // message inside `run_single_turn`. Sub-agents share the
            // parent's provider / model_info / stream_options triple
            // (per `docs/aj-next-plan.md` §1.6) so the whole
            // hierarchy talks to the same backend.
            let mut sub_agent = Agent::with_provider(
                self.env.clone(),
                self.system_prompt,
                sub_agent_tools,
                disabled_tools,
                Arc::clone(&self.provider),
                Arc::clone(&self.model_info),
                self.stream_options.clone(),
                None,
            );
            sub_agent.set_agent_id(child_id);
            sub_agent.set_assembled_system_prompt(self.assembled_system_prompt.clone());
            // Share the parent's bus per `docs/aj-next-plan.md`
            // §1.6: every event the sub-agent emits during its
            // run reaches the listeners the binary registered on
            // the parent (rendering, persistence), tagged with
            // `Sub(n)`. Without this the sub-agent runs on its
            // own bus and the binary's bridge listener never sees
            // its activity.
            sub_agent.set_bus(self.parent_bus.clone());

            let result = sub_agent.run_single_turn(task).await;

            // Get the sub-agent's accumulated usage
            let sub_agent_usage = sub_agent.session_state.accumulated_usage.clone();

            // Record the usage in the main session state
            self.session_ctx
                .record_sub_agent_usage(agent_id, sub_agent_usage);

            // Emit `SubAgentEnd` regardless of success — listeners
            // need to clean up nested-transcript framing on errors
            // too. The report carries the child's final assistant
            // text (or the error string) so the parent's listener
            // sees a single complete summary.
            let report = match &result {
                Ok(text) => text.clone(),
                Err(err) => format!("sub-agent failed: {err:#}"),
            };
            self.parent_bus
                .emit(AgentEvent::SubAgentEnd {
                    parent: self.parent_agent_id,
                    child: child_id,
                    report: report.clone(),
                })
                .await?;

            // Surface the freshly-allocated sub-agent id alongside
            // the child's final assistant text. Errors still
            // propagate via `?` so the agent runtime keeps
            // synthesizing a generic tool-error result for failed
            // spawns.
            result.map(|report| SpawnedAgent { agent_id, report })
        })
    }

    fn emit_update(&mut self, _partial: ToolDetails) {
        // No-op for now. The trait's `emit_update` is synchronous
        // but the bus's `emit` is async, so we cannot drive
        // [`AgentEvent::ToolExecutionUpdate`] inline from a sync
        // context without breaking the bus's "listeners are
        // awaited inline" guarantee (firing the listener from a
        // spawned task would let `Update` events arrive after
        // `End`). Today only `bash` calls `emit_update`, the
        // legacy CLI doesn't render it, and dropping the snapshot
        // is functionally equivalent to the pre-§2.4a behavior. A
        // bus-side `try_emit_sync` (or an async `emit_update` on
        // the trait) lands when the TUI needs progress streaming.
    }

    fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

/// Inspect a freshly-seeded transcript for `tool_use` blocks that
/// never received a matching `tool_result`. This is the in-memory
/// counterpart of `aj_session::repair_interrupted_tool_uses`: the
/// binary calls the session-side helper to write recovery entries
/// to disk, then re-seeds the agent; if the binary instead seeds
/// without repairing, [`scan_dangling_tool_uses`] surfaces the
/// invariant violation here. Used by the agent's tests; not part
/// of the run-time path.
#[cfg(test)]
fn scan_dangling_tool_uses(transcript: &[MessageParam]) -> std::collections::HashSet<String> {
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in transcript {
        match msg.role {
            Role::Assistant => {
                for block in &msg.content {
                    if let ContentBlockParam::ToolUseBlock { id, .. } = block {
                        used.insert(id.clone());
                    }
                }
            }
            Role::User => {
                for block in &msg.content {
                    if let ContentBlockParam::ToolResultBlock { tool_use_id, .. } = block {
                        resolved.insert(tool_use_id.clone());
                    }
                }
            }
        }
    }
    used.difference(&resolved).cloned().collect()
}

/// Error returned from [`Agent::prompt`] / [`Agent::continue_run`] /
/// [`Agent::run_single_turn`].
///
/// `Recoverable` errors (model API failures, malformed streaming
/// responses, etc.) are surfaced to the user so they can retry or
/// rephrase, rather than aborting the program. `Fatal` errors
/// (listener-write failures, internal invariant violations) bubble
/// out so the user gets a clean exit instead of silently looping.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    /// An ephemeral error encountered while talking to the model.
    /// The transcript is in a consistent state and the user can
    /// retry by submitting another message.
    #[error("{0:#}")]
    Recoverable(anyhow::Error),
    /// A persistent failure (e.g. failed disk write through the
    /// persistence listener) or an internal invariant violation.
    /// Bubble out to the top level.
    #[error(transparent)]
    Fatal(anyhow::Error),
}

impl From<ModelError> for TurnError {
    fn from(e: ModelError) -> Self {
        TurnError::Recoverable(e.into())
    }
}

impl From<ApiError> for TurnError {
    fn from(e: ApiError) -> Self {
        TurnError::Recoverable(e.into())
    }
}

impl From<anyhow::Error> for TurnError {
    fn from(e: anyhow::Error) -> Self {
        TurnError::Fatal(e)
    }
}

/// Map a legacy [`ThinkingConfig`] onto the unified
/// [`ThinkingLevel`] the [`Provider`] trait consumes.
///
/// Legacy `ThinkingConfig::Max` collapses onto `ThinkingLevel::XHigh`
/// because the unified protocol caps at XHigh — providers that
/// support a higher reasoning budget than XHigh treat the legacy
/// "Max" rung as a synonym for XHigh.
fn thinking_config_to_level(level: &ThinkingConfig) -> ThinkingLevel {
    match level {
        ThinkingConfig::Low => ThinkingLevel::Low,
        ThinkingConfig::Medium => ThinkingLevel::Medium,
        ThinkingConfig::High => ThinkingLevel::High,
        ThinkingConfig::XHigh | ThinkingConfig::Max => ThinkingLevel::XHigh,
    }
}

#[cfg(test)]
mod event_protocol_tests {
    //! Snapshot the event protocol the agent emits on its bus.
    //!
    //! Per `docs/aj-next-plan.md` §2.1 / §2.4b, the bus is the
    //! agent's only output channel. These tests pin the event
    //! sequence for known-shape turns so subsequent commits cannot
    //! silently regress the protocol; the agent runs in isolation
    //! (no log, no UI), with a scripted model, and the test
    //! observes events directly.

    use std::path::PathBuf;
    use std::sync::Mutex;

    use aj_conf::AgentEnv;
    use aj_models::messages::ContentBlockParam;
    use aj_models::provider::Provider;
    use aj_models::registry::{InputModality, ModelCost, ModelInfo};
    use aj_models::scripted::provider::{ExhaustedBehavior, ScriptedProvider};
    use aj_models::streaming::{AssistantMessageEvent, DoneReason};
    use aj_models::types::{
        AssistantContent, AssistantMessage, StopReason, StreamOptions, TextContent, ToolCall,
    };
    use std::sync::Arc;

    use crate::bus::listener_from_sync;
    use crate::events::{AgentEvent, AgentId, PersistedMessageKind};
    use crate::tool::{
        ErasedToolDefinition, ToolContext, ToolDefinition, ToolDetails, ToolOutcome,
    };
    use crate::Agent;

    /// Trivial tool that returns a fixed string. Implements the
    /// [`ToolDefinition`] trait so the test exercises the same
    /// driving path the production builtins go through.
    #[derive(Clone)]
    struct PingTool;

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct PingInput {}

    impl ToolDefinition for PingTool {
        type Input = PingInput;

        fn name(&self) -> &'static str {
            "ping"
        }

        fn description(&self) -> &'static str {
            "Test tool"
        }

        async fn execute(
            &self,
            _ctx: &mut dyn ToolContext,
            _input: PingInput,
        ) -> anyhow::Result<ToolOutcome> {
            Ok(ToolOutcome {
                content: vec![aj_models::types::UserContent::text("pong".to_string())],
                details: ToolDetails::Text {
                    summary: "ping".to_string(),
                    body: "pong".to_string(),
                },
                is_error: false,
            })
        }
    }

    /// Identity stamped on every scripted [`AssistantMessage`] in this
    /// module. Matches what [`ScriptedProvider`]'s demos use so the
    /// agent's TUI / persistence listeners see a coherent provider
    /// identity even in tests.
    const SCRIPT_API: &str = "scripted";
    const SCRIPT_PROVIDER: &str = "scripted";
    const SCRIPT_MODEL: &str = "scripted";

    /// Build a [`ModelInfo`] mirroring what [`ScriptedProvider`] stamps
    /// onto every emitted [`AssistantMessage`] partial. The agent
    /// reads identity off this struct for the TUI footer and the
    /// `/model` selector; the values are only checked for "matches
    /// what the provider claims", so any consistent triple works.
    fn scripted_model_info() -> ModelInfo {
        ModelInfo {
            id: SCRIPT_MODEL.to_string(),
            name: SCRIPT_MODEL.to_string(),
            api: SCRIPT_API.to_string(),
            provider: SCRIPT_PROVIDER.to_string(),
            base_url: "scripted://internal".to_string(),
            reasoning: false,
            supports_xhigh: false,
            supports_adaptive_thinking: false,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 0,
            max_tokens: 0,
            headers: None,
        }
    }

    /// Build a finalized [`AssistantMessage`] with a single tool_call
    /// block, stop_reason = `ToolUse`.
    fn finalize_tool_use(tool_use_id: &str, tool_name: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: tool_use_id.to_string(),
                name: tool_name.to_string(),
                arguments: serde_json::json!({}),
            })],
            api: SCRIPT_API.to_string(),
            provider: SCRIPT_PROVIDER.to_string(),
            model: SCRIPT_MODEL.to_string(),
            response_id: Some("test-msg-1".to_string()),
            usage: Default::default(),
            stop_reason: StopReason::ToolUse,
            error: None,
            timestamp: 0,
        }
    }

    /// Build a finalized [`AssistantMessage`] with a single text
    /// block, stop_reason = `Stop`.
    fn finalize_text(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            api: SCRIPT_API.to_string(),
            provider: SCRIPT_PROVIDER.to_string(),
            model: SCRIPT_MODEL.to_string(),
            response_id: Some("test-msg-2".to_string()),
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        }
    }

    /// Build an [`AssistantMessageEvent`] script for a single inference
    /// that finalizes on the given message. The pair `Start + Done`
    /// matches the spec's minimum-protocol shape: a stream begins with
    /// `Start` and terminates with `Done`, with no per-block streaming
    /// in between. The agent's match arm treats `Start` as a no-op and
    /// drives all rendering off the finalized message blocks, so the
    /// resulting bus events are independent of intermediate block
    /// streaming — exactly the shape these locked-protocol tests want
    /// to pin.
    fn finalize_script(message: AssistantMessage) -> Vec<AssistantMessageEvent> {
        let reason = match message.stop_reason {
            StopReason::Stop => DoneReason::Stop,
            StopReason::Length => DoneReason::Length,
            StopReason::ToolUse => DoneReason::ToolUse,
            other => panic!(
                "finalize_script: unexpected non-success stop reason {other:?}; \
                 use ScriptedProvider's error path for error/aborted cases"
            ),
        };
        vec![
            AssistantMessageEvent::Start {
                partial: message.clone(),
            },
            AssistantMessageEvent::Done { reason, message },
        ]
    }

    /// Compact, comparable representation of an [`AgentEvent`] for
    /// snapshot assertions. We don't `derive(PartialEq)` on the
    /// real enum because some payloads (e.g. the legacy
    /// `AssistantMessage` once it arrives in §2.4) don't implement
    /// `PartialEq` cleanly, and a label per variant keeps test
    /// failures readable.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum EventLabel {
        AgentStart(AgentId),
        AgentEnd(AgentId),
        TurnStart(AgentId),
        Notice(AgentId, String),
        Warning(AgentId, String),
        Error(AgentId, String),
        ToolExecutionStart {
            agent_id: AgentId,
            call_id: String,
            tool: String,
        },
        ToolExecutionEnd {
            agent_id: AgentId,
            call_id: String,
            tool: String,
            summary: String,
            body: String,
            is_error: bool,
        },
        SubAgentStart {
            parent: AgentId,
            child: AgentId,
            task: String,
        },
        SubAgentEnd {
            parent: AgentId,
            child: AgentId,
        },
        StreamRetry(AgentId, u32),
        StreamChunk {
            agent_id: AgentId,
            channel: &'static str,
            action: &'static str,
        },
        TurnUsage(AgentId),
        /// Persistence write request. The label captures only the
        /// kind discriminator (User / Assistant / ToolResult /
        /// UserOutput) so test assertions don't have to care
        /// about the exact content blocks the event carries.
        MessagePersisted {
            agent_id: AgentId,
            kind: &'static str,
        },
        Other(&'static str),
    }

    fn label(event: &AgentEvent) -> EventLabel {
        match event {
            AgentEvent::AgentStart { agent_id } => EventLabel::AgentStart(*agent_id),
            AgentEvent::AgentEnd { agent_id, .. } => EventLabel::AgentEnd(*agent_id),
            AgentEvent::TurnStart { agent_id } => EventLabel::TurnStart(*agent_id),
            AgentEvent::Notice { agent_id, text } => EventLabel::Notice(*agent_id, text.clone()),
            AgentEvent::Warning { agent_id, text } => EventLabel::Warning(*agent_id, text.clone()),
            AgentEvent::Error { agent_id, text } => EventLabel::Error(*agent_id, text.clone()),
            AgentEvent::ToolExecutionStart {
                agent_id,
                call_id,
                tool,
                ..
            } => EventLabel::ToolExecutionStart {
                agent_id: *agent_id,
                call_id: call_id.clone(),
                tool: tool.clone(),
            },
            AgentEvent::ToolExecutionEnd {
                agent_id,
                call_id,
                tool,
                result,
                is_error,
            } => {
                let (summary, body) = match result {
                    ToolDetails::Text { summary, body } => (summary.clone(), body.clone()),
                    other => (format!("{other:?}"), "<non-text variant>".to_string()),
                };
                EventLabel::ToolExecutionEnd {
                    agent_id: *agent_id,
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    summary,
                    body,
                    is_error: *is_error,
                }
            }
            AgentEvent::SubAgentStart {
                parent,
                child,
                task,
            } => EventLabel::SubAgentStart {
                parent: *parent,
                child: *child,
                task: task.clone(),
            },
            AgentEvent::SubAgentEnd { parent, child, .. } => EventLabel::SubAgentEnd {
                parent: *parent,
                child: *child,
            },
            AgentEvent::StreamRetry {
                agent_id, attempt, ..
            } => EventLabel::StreamRetry(*agent_id, *attempt),
            AgentEvent::StreamChunk {
                agent_id,
                channel,
                action,
            } => {
                let channel = match channel {
                    crate::events::StreamChannel::Text => "text",
                    crate::events::StreamChannel::Thinking => "thinking",
                    crate::events::StreamChannel::User => "user",
                };
                let action = match action {
                    crate::events::StreamAction::Start { .. } => "start",
                    crate::events::StreamAction::Update { .. } => "update",
                    crate::events::StreamAction::Stop { .. } => "stop",
                };
                EventLabel::StreamChunk {
                    agent_id: *agent_id,
                    channel,
                    action,
                }
            }
            AgentEvent::TurnUsage { agent_id, .. } => EventLabel::TurnUsage(*agent_id),
            AgentEvent::MessagePersisted { agent_id, kind } => {
                let kind = match kind {
                    PersistedMessageKind::User { .. } => "User",
                    PersistedMessageKind::Assistant { .. } => "Assistant",
                    PersistedMessageKind::ToolResult { .. } => "ToolResult",
                    PersistedMessageKind::UserOutput { .. } => "UserOutput",
                };
                EventLabel::MessagePersisted {
                    agent_id: *agent_id,
                    kind,
                }
            }
            AgentEvent::TurnEnd { .. } => EventLabel::Other("TurnEnd"),
            AgentEvent::MessageStart { .. } => EventLabel::Other("MessageStart"),
            AgentEvent::MessageUpdate { .. } => EventLabel::Other("MessageUpdate"),
            AgentEvent::MessageEnd { .. } => EventLabel::Other("MessageEnd"),
            AgentEvent::ToolExecutionUpdate { .. } => EventLabel::Other("ToolExecutionUpdate"),
            AgentEvent::QueueUpdate { .. } => EventLabel::Other("QueueUpdate"),
        }
    }

    /// Build an [`AgentEnv`] that doesn't pull instructions from the
    /// host — context loading is environment-dependent, and we want
    /// a deterministic event sequence regardless of where the test
    /// runs.
    fn empty_env(working_directory: PathBuf) -> AgentEnv {
        AgentEnv {
            working_directory,
            git_root_directory: None,
            operating_system: "test".to_string(),
            today_date: "2024-01-01".to_string(),
            context_files: Vec::new(),
        }
    }

    fn build_agent(
        scripts: Vec<Vec<AssistantMessageEvent>>,
        tools: Vec<ErasedToolDefinition>,
    ) -> Agent {
        // Strict-mode scripted provider: panic if the agent runs more
        // inferences than the test scripted, which would indicate a
        // regression that adds an unexpected loop iteration.
        let provider: Arc<dyn Provider> = Arc::new(
            ScriptedProvider::from_event_vecs(scripts).on_exhausted(ExhaustedBehavior::Panic),
        );
        let model_info = Arc::new(scripted_model_info());
        let env = empty_env(std::env::temp_dir());
        let mut agent = Agent::with_provider(
            env,
            "irrelevant",
            tools,
            Vec::new(),
            provider,
            model_info,
            StreamOptions::default(),
            None,
        );
        agent.set_assembled_system_prompt("test system prompt".to_string());
        agent
    }

    #[tokio::test]
    async fn run_single_turn_with_tool_call_emits_locked_protocol() {
        // Two scripted inferences:
        //   1. Tool call (id="tu-1", name="ping").
        //   2. Final text response after the tool result is fed
        //      back.
        let scripts = vec![
            finalize_script(finalize_tool_use("tu-1", "ping")),
            finalize_script(finalize_text("done")),
        ];

        let ping: ErasedToolDefinition = PingTool.into();
        let mut agent = build_agent(scripts, vec![ping]);
        // Mirror what `SessionContextWrapper::spawn_agent` does in
        // production: tagging the agent with its sub-agent id keeps
        // bus events aligned to the [`ThreadKind::Subagent`]
        // entries the persistence listener would write.
        agent.set_agent_id(AgentId::Sub(1));

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = recorded.clone();
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        agent
            .run_single_turn("run ping".to_string())
            .await
            .expect("run_single_turn");

        let events = recorded.lock().unwrap().clone();
        let expected = vec![
            EventLabel::AgentStart(AgentId::Sub(1)),
            // The sub-agent's first persistence event is the
            // user-prompt message (the persistence listener uses
            // this to anchor at the parent's spawning entry).
            EventLabel::MessagePersisted {
                agent_id: AgentId::Sub(1),
                kind: "User",
            },
            EventLabel::TurnStart(AgentId::Sub(1)),
            // First inference: the model returned a tool_use.
            EventLabel::MessagePersisted {
                agent_id: AgentId::Sub(1),
                kind: "Assistant",
            },
            EventLabel::TurnUsage(AgentId::Sub(1)),
            EventLabel::ToolExecutionStart {
                agent_id: AgentId::Sub(1),
                call_id: "tu-1".to_string(),
                tool: "ping".to_string(),
            },
            EventLabel::ToolExecutionEnd {
                agent_id: AgentId::Sub(1),
                call_id: "tu-1".to_string(),
                tool: "ping".to_string(),
                summary: "ping".to_string(),
                body: "pong".to_string(),
                is_error: false,
            },
            // After the tool batch finishes, one
            // `MessagePersisted::ToolResult` event for the
            // synthesized user-role tool_result message.
            EventLabel::MessagePersisted {
                agent_id: AgentId::Sub(1),
                kind: "ToolResult",
            },
            // Second inference: the model returned plain text.
            EventLabel::MessagePersisted {
                agent_id: AgentId::Sub(1),
                kind: "Assistant",
            },
            EventLabel::TurnUsage(AgentId::Sub(1)),
            EventLabel::AgentEnd(AgentId::Sub(1)),
        ];
        assert_eq!(events, expected, "unexpected event sequence: {events:#?}");
    }

    #[tokio::test]
    async fn tool_result_persistence_event_carries_structured_details_by_id() {
        // The agent's batched [`PersistedMessageKind::ToolResult`]
        // event must carry the per-call structured `details` keyed
        // by `tool_use_id`, alongside the wire `content`. A
        // downstream persistence listener relies on this so the
        // on-disk record can pin both the LLM-facing text (used by
        // the next inference) and the renderer payload (used by
        // resumed sessions to rehydrate diffs / todo snapshots /
        // bash exit codes / sub-agent reports without re-running
        // the tool).
        let scripts = vec![
            finalize_script(finalize_tool_use("tu-only", "ping")),
            finalize_script(finalize_text("done")),
        ];
        let ping: ErasedToolDefinition = PingTool.into();
        let mut agent = build_agent(scripts, vec![ping]);
        agent.set_agent_id(AgentId::Sub(42));

        let captured: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            // Stash full events (not just labels) so the test can
            // inspect the `details` map contents.
            if let AgentEvent::MessagePersisted {
                kind: PersistedMessageKind::ToolResult { .. },
                ..
            } = event
            {
                captured_clone.lock().unwrap().push(event.clone());
            }
        }));

        agent
            .run_single_turn("run ping".to_string())
            .await
            .expect("run_single_turn");

        let events = captured.lock().unwrap().clone();
        assert_eq!(
            events.len(),
            1,
            "expected exactly one ToolResult persistence event: {events:#?}"
        );

        let AgentEvent::MessagePersisted {
            kind: PersistedMessageKind::ToolResult { content, details },
            ..
        } = &events[0]
        else {
            panic!("captured non-ToolResult event: {:#?}", events[0]);
        };

        // Wire content carries exactly one tool_result block, with
        // the same id the model's `tool_use` block had.
        assert_eq!(content.len(), 1);
        match &content[0] {
            ContentBlockParam::ToolResultBlock {
                tool_use_id,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "tu-only");
                assert!(!is_error);
            }
            other => panic!("expected ToolResultBlock, got {other:#?}"),
        }

        // The details map has a matching entry keyed by the same
        // `tool_use_id`, carrying the structured `ToolDetails`
        // [`PingTool`] returned (the test fixture emits a
        // `Text { summary: "ping", body: "pong" }` outcome).
        assert_eq!(details.len(), 1, "details: {details:#?}");
        let payload = details
            .get("tu-only")
            .expect("details keyed by tool_use_id");
        match payload {
            ToolDetails::Text { summary, body } => {
                assert_eq!(summary, "ping");
                assert_eq!(body, "pong");
            }
            other => panic!("expected ToolDetails::Text, got {other:#?}"),
        }
    }

    #[tokio::test]
    async fn run_single_turn_brackets_with_agent_lifecycle() {
        // Drives `run_single_turn` (the public sub-agent entry
        // point) and verifies the bus brackets every run with an
        // `AgentStart` / `AgentEnd` pair tagged with the agent's
        // id.
        let scripts = vec![finalize_script(finalize_text("ok"))];

        let mut agent = build_agent(scripts, Vec::new());
        agent.set_agent_id(AgentId::Sub(7));

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = recorded.clone();
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        agent
            .run_single_turn("test prompt".to_string())
            .await
            .expect("run_single_turn");

        let events = recorded.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                EventLabel::AgentStart(AgentId::Sub(7)),
                EventLabel::MessagePersisted {
                    agent_id: AgentId::Sub(7),
                    kind: "User",
                },
                EventLabel::TurnStart(AgentId::Sub(7)),
                EventLabel::MessagePersisted {
                    agent_id: AgentId::Sub(7),
                    kind: "Assistant",
                },
                EventLabel::TurnUsage(AgentId::Sub(7)),
                EventLabel::AgentEnd(AgentId::Sub(7)),
            ],
            "unexpected event sequence: {events:#?}"
        );
    }

    #[tokio::test]
    async fn prompt_emits_user_message_persisted_event() {
        // The top-level entry point appends the user prompt to the
        // transcript and emits a `MessagePersisted::User` event
        // before the assistant turn loop begins. This is the
        // contract the binary's persistence listener relies on
        // to write the user's typed input to disk.
        let scripts = vec![finalize_script(finalize_text("done"))];

        let mut agent = build_agent(scripts, Vec::new());

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = recorded.clone();
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        agent
            .prompt("hello agent".to_string())
            .await
            .expect("prompt");

        let events = recorded.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                EventLabel::AgentStart(AgentId::Main),
                EventLabel::MessagePersisted {
                    agent_id: AgentId::Main,
                    kind: "User",
                },
                EventLabel::TurnStart(AgentId::Main),
                EventLabel::MessagePersisted {
                    agent_id: AgentId::Main,
                    kind: "Assistant",
                },
                EventLabel::TurnUsage(AgentId::Main),
                EventLabel::AgentEnd(AgentId::Main),
            ],
            "unexpected event sequence: {events:#?}"
        );

        // The transcript reflects both the user prompt and the
        // assistant reply.
        assert_eq!(agent.messages().len(), 2);
    }

    #[tokio::test]
    async fn continue_run_drives_existing_transcript_without_appending() {
        // `continue_run` is the recovery / continuation entry point:
        // the binary uses it after a recoverable error to retry
        // inference against the user message that's already on the
        // transcript, without re-emitting a `MessagePersisted::User`
        // (the prior `prompt` call already wrote it). Here we seed
        // a transcript ending in a user-role message and verify
        // `continue_run` drives one assistant turn without firing
        // any extra User persistence event.
        use aj_models::messages::ContentBlockParam;

        let scripts = vec![finalize_script(finalize_text("retried"))];

        let mut agent = build_agent(scripts, Vec::new());
        // Seed a user-role last message — typically the prompt the
        // user already submitted before the previous turn errored
        // out.
        agent.seed_messages(vec![aj_models::messages::MessageParam::new_user_message(
            vec![ContentBlockParam::new_text_block("retry me".into())],
        )]);

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = recorded.clone();
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        agent.continue_run().await.expect("continue_run");

        let events = recorded.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                EventLabel::AgentStart(AgentId::Main),
                // No `MessagePersisted::User` here — that's the
                // distinguishing feature of `continue_run` vs `prompt`.
                EventLabel::TurnStart(AgentId::Main),
                EventLabel::MessagePersisted {
                    agent_id: AgentId::Main,
                    kind: "Assistant",
                },
                EventLabel::TurnUsage(AgentId::Main),
                EventLabel::AgentEnd(AgentId::Main),
            ],
            "unexpected event sequence: {events:#?}"
        );

        // The seeded user prompt + the assistant reply are both
        // visible in the transcript.
        assert_eq!(agent.messages().len(), 2);
    }

    #[tokio::test]
    async fn continue_run_rejects_assistant_role_last_message() {
        // The wire layer requires the transcript to end in a
        // user-role message before inference. `continue_run`
        // enforces that precondition with a fatal error rather
        // than letting the model API surface an obscure 4xx.
        use aj_models::messages::{ContentBlockParam, MessageParam, Role};

        // No scripts queued: if the precondition check is missing
        // and the agent runs an inference, the `ScriptedProvider`
        // panics ("ScriptedProvider exhausted"). That'd produce a
        // different failure than the `Fatal` we're checking for —
        // so a successful test here implies the check fired
        // *before* any inference attempt.
        let mut agent = build_agent(Vec::new(), Vec::new());
        // Seed an assistant-role last message.
        agent.seed_messages(vec![MessageParam {
            role: Role::Assistant,
            content: vec![ContentBlockParam::new_text_block("hi".into())],
        }]);

        let err = agent
            .continue_run()
            .await
            .expect_err("continue_run must reject assistant-role last message");
        assert!(
            matches!(err, crate::TurnError::Fatal(_)),
            "expected Fatal error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn continue_run_rejects_empty_transcript() {
        // Empty transcript is the other shape that's invalid: the
        // model needs *something* user-side to respond to. Same
        // fatal-error contract as the assistant-last case.
        let mut agent = build_agent(Vec::new(), Vec::new());

        let err = agent
            .continue_run()
            .await
            .expect_err("continue_run must reject empty transcript");
        assert!(
            matches!(err, crate::TurnError::Fatal(_)),
            "expected Fatal error, got: {err:?}"
        );
    }

    #[test]
    fn scan_dangling_tool_uses_finds_unmatched_ids() {
        // Sanity check on the test-only helper: a transcript
        // ending in an assistant tool_use without a matching
        // tool_result reports the dangling id.
        use aj_models::messages::{ContentBlockParam, MessageParam, Role};

        let transcript = vec![
            MessageParam::new_user_message(vec![ContentBlockParam::new_text_block("hi".into())]),
            MessageParam {
                role: Role::Assistant,
                content: vec![ContentBlockParam::ToolUseBlock {
                    id: "tu-1".to_string(),
                    name: "ping".to_string(),
                    input: serde_json::json!({}),
                    caller: None,
                }],
            },
        ];
        let dangling = super::scan_dangling_tool_uses(&transcript);
        assert!(dangling.contains("tu-1"));

        let mut transcript_with_resolution = transcript.clone();
        transcript_with_resolution.push(MessageParam::new_user_message(vec![
            ContentBlockParam::ToolResultBlock {
                tool_use_id: "tu-1".to_string(),
                content: "done".to_string().into(),
                is_error: false,
            },
        ]));
        assert!(super::scan_dangling_tool_uses(&transcript_with_resolution).is_empty());
    }
}
