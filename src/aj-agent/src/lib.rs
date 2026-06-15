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
pub mod hooks;
pub mod message;
pub mod projection;
pub mod queue;
pub mod tool;
pub mod types;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use aj_conf::{AgentEnv, ConfigThinkingLevel};
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::streaming::{AssistantMessageEvent, AssistantMessageEventStream};
use aj_models::tools::Tool;
use aj_models::types::{
    AssistantContent, AssistantMessage, Context, ErrorCategory, Message, SimpleStreamOptions,
    Speed, StopReason, StreamOptions, ThinkingLevel, ToolCall,
    ToolDefinition as UnifiedToolDefinition, ToolResultMessage, Usage, UserContent, UserMessage,
};
use aj_models::ThinkingConfig;

use crate::bus::{EventBus, Listener, SubscriptionHandle};
use crate::events::{AgentEvent, AgentId, AgentSettings};
use crate::message::AgentMessage;
use crate::projection::transcript_to_messages;
use crate::queue::{MessageQueues, PendingKind};
use crate::tool::{
    ErasedToolDefinition, SpawnMode, SpawnResult, SpawnedAgent, StartedTask, TaskEventSink, TaskId,
    TaskKind, TaskNotice, TaskOutputSource, TaskRead, TaskStatus, TodoItem, ToolContext,
    ToolDetails, ToolOutcome,
};
use crate::types::TokenUsage;
use anyhow::anyhow;
use futures::StreamExt;
use std::sync::Arc;
use tokio_retry2::strategy::{jitter, ExponentialBackoff};
use tokio_util::sync::CancellationToken;

/// One-shot session seed applied at construction time: the resumed
/// transcript, the fully-assembled system prompt, and the sub-agent
/// counter floor derived from sub-agent subtrees already persisted
/// on the session's log. Passed to [`Agent::seed_session`].
#[derive(Debug, Default)]
pub struct AgentSeed {
    /// Replaces the agent's in-memory transcript. On resume the
    /// binary opens the log, linearizes the user thread, and passes
    /// the resulting messages so the next turn sees the prior
    /// conversation. Empty for a fresh session.
    pub transcript: Vec<AgentMessage>,
    /// The fully-assembled system prompt for the session: either
    /// reused verbatim from the log (cache-warm resume) or freshly
    /// assembled via [`Agent::assemble_system_prompt`]. Inference
    /// reads it directly, so it must be seeded before any turn
    /// runs. `None` leaves the agent's prompt unset (the seed
    /// targets a fresh agent, where it is unset already).
    pub assembled_system_prompt: Option<String>,
    /// Floor for sub-agent ids: subsequently minted ids are
    /// strictly greater than this value, so freshly spawned
    /// sub-agents never collide with subtrees already persisted in
    /// the log. `0` for a fresh session.
    pub sub_agent_counter: usize,
}

pub struct Agent {
    env: AgentEnv,
    /// The fully-assembled system prompt for the current run.
    /// Populated by [`Agent::seed_session`] (resume path or fresh
    /// assembly) before any turn runs; inference reads it directly.
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
    /// Inference speed mode reported on sub-agent spawn events and
    /// inherited by spawned sub-agents. The speed's wire effect
    /// (provider-specific headers) is baked into `stream_options` by
    /// the binary; this field only tracks the user-facing knob.
    /// `None` means standard.
    speed: Option<Speed>,
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
    /// In-memory transcript: every [`AgentMessage`] this agent has
    /// seen, in append order. Replaces the agent's reach into
    /// [`aj_session::ConversationLog`] (per `docs/aj-next-plan.md`
    /// §2.4b): the binary owns the log, resumes it on startup, and
    /// seeds the transcript via [`Agent::seed_session`] before the
    /// first turn.
    transcript: Vec<AgentMessage>,
    /// Optional hook fired before every tool call. Set via
    /// [`Agent::set_before_tool_call`]; `None` means "skip the
    /// hook" — the tool's own `execute` runs unconditionally with
    /// the model-supplied arguments.
    before_tool_call: Option<hooks::BeforeToolCallHook>,
    /// Optional hook fired after every tool call returns. Set via
    /// [`Agent::set_after_tool_call`]; can rewrite the outcome's
    /// `content`, `details`, or `is_error` before the bus event
    /// and the wire projection fire.
    after_tool_call: Option<hooks::AfterToolCallHook>,
    /// Optional hook consulted after every assistant turn finishes
    /// its tool batch. Set via [`Agent::set_should_stop_after_turn`];
    /// returning `true` ends the turn without a follow-up inference.
    should_stop_after_turn: Option<hooks::ShouldStopAfterTurnHook>,
    /// Defense-in-depth gate for the `image_block` config flag.
    /// When `true`, [`aj_models::transform::block_user_images`] is
    /// applied to the wire-bound message vector before it reaches
    /// the provider so the model never receives image bytes,
    /// regardless of its declared vision capability. The on-disk
    /// transcript is unaffected; flipping this back to `false`
    /// later in the same thread restores image visibility for
    /// future turns. Set via [`Agent::set_block_images`].
    block_images: bool,
    /// Shared registry into which this agent inserts each sub-agent it
    /// spawns, keyed by `Sub(n)` index, so the handle outlives the
    /// initial `agent` tool call. Default-empty; the binary injects a
    /// shared instance onto the main agent via
    /// [`Agent::set_sub_agent_registry`]. Sub-agents never read it
    /// (they can't spawn), and one-shot callers leave it empty.
    sub_agent_registry: SubAgentRegistry,
    /// Shared registry of background tasks this agent (and its
    /// sub-agents) started. Default; the binary injects a shared
    /// instance via [`Agent::set_task_registry`] so it can observe
    /// and kill tasks. Tasks in a default registry die with the
    /// process — same caveats as [`SubAgentRegistry`].
    task_registry: TaskRegistry,
    /// Shared steering / follow-up message queues, keyed by
    /// [`AgentId`]. The binary injects a shared instance and threads a
    /// clone into each spawned sub-agent (see
    /// [`SessionContextWrapper::spawn_agent`]) so the TUI can enqueue
    /// while a turn runs; the turn driver drains them at the in-loop
    /// steering point and the run-top (wake) point. A default handle
    /// is used by print mode and tests, where nothing enqueues.
    message_queues: MessageQueues,
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
            assembled_system_prompt: None,
            tool_definitions,
            tools: api_tools,
            disabled_tools,
            provider,
            model_info,
            stream_options,
            session_state,
            default_thinking,
            speed: None,
            agent_id: AgentId::Main,
            bus: EventBus::new(),
            cancellation: CancellationToken::new(),
            transcript: Vec::new(),
            before_tool_call: None,
            after_tool_call: None,
            should_stop_after_turn: None,
            block_images: false,
            sub_agent_registry: SubAgentRegistry::default(),
            task_registry: TaskRegistry::default(),
            message_queues: MessageQueues::default(),
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

    /// Replace this agent's cancellation token.
    ///
    /// Used by [`SessionContextWrapper::spawn_agent`] to make a
    /// sub-agent inherit a child token derived from the parent's
    /// per `docs/aj-next-plan.md` §1.6, and by
    /// [`Agent::prompt`] / [`Agent::continue_run`] /
    /// [`Agent::run_single_turn`] to install the per-turn token the
    /// binary owns and can `cancel()` from a different code path
    /// (e.g. the TUI's Ctrl+C handler) without locking the agent.
    ///
    /// Idempotent. Must be called before the turn (or sub-agent
    /// turn) starts; in-flight inferences continue with the token
    /// they captured at the top of [`Self::execute_turn`].
    pub fn set_cancellation(&mut self, token: CancellationToken) {
        self.cancellation = token;
    }

    /// Toggle the defense-in-depth `image_block` gate.
    ///
    /// When `true`, [`aj_models::transform::block_user_images`] is
    /// applied to the wire-bound message vector before every
    /// inference so [`aj_models::types::UserContent::Image`] blocks
    /// are replaced with a placeholder text block. The on-disk
    /// transcript is not touched, so flipping back to `false`
    /// later in the same thread restores image visibility for
    /// future turns. Sub-agents inherit the parent's value at
    /// spawn time.
    pub fn set_block_images(&mut self, block: bool) {
        self.block_images = block;
    }

    /// Inject the shared sub-agent registry.
    ///
    /// The binary calls this on the main agent so the agent and the
    /// binary share one map: the agent inserts each sub-agent on spawn
    /// (see [`SessionContextWrapper::spawn_agent`]) and the binary
    /// resolves handles to drive continuations. Sub-agents never need
    /// one set — they can't spawn.
    pub fn set_sub_agent_registry(&mut self, registry: SubAgentRegistry) {
        self.sub_agent_registry = registry;
    }

    /// Inject the shared background-task registry.
    ///
    /// The binary calls this on the main agent so the agent and the
    /// binary share one map; the agent threads a clone into every tool
    /// call and every spawned sub-agent (so sub-agent-owned tasks land
    /// in the same map the binary observes). Callers that never inject
    /// one get the default registry, whose tasks die with the process.
    pub fn set_task_registry(&mut self, registry: TaskRegistry) {
        self.task_registry = registry;
    }

    /// Inject the shared steering / follow-up message queues.
    ///
    /// The binary calls this on the main agent so the agent and the
    /// binary share one handle; the agent threads a clone into every
    /// spawned sub-agent so the TUI can steer or queue for any agent
    /// it views. Callers that never inject one get a default handle
    /// that nothing ever enqueues onto.
    pub fn set_message_queues(&mut self, queues: MessageQueues) {
        self.message_queues = queues;
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
    pub fn messages(&self) -> &[AgentMessage] {
        &self.transcript
    }

    /// Apply a session seed: replace the transcript, set the
    /// assembled system prompt (a `None` prompt leaves the field
    /// unset), and seed the sub-agent counter so subsequent
    /// [`SessionState::next_sub_agent_id`] calls mint ids strictly
    /// greater than the seeded floor.
    ///
    /// Contract: call at most once, on a freshly constructed agent,
    /// before it is shared or drives its first turn.
    pub fn seed_session(&mut self, seed: AgentSeed) {
        self.transcript = seed.transcript;
        if let Some(prompt) = seed.assembled_system_prompt {
            self.assembled_system_prompt = Some(prompt);
        }
        self.session_state
            .seed_sub_agent_counter(seed.sub_agent_counter);
    }

    /// Install a hook fired before every tool call, replacing any
    /// previous hook. Passing the closure inside `Some(...)` enables
    /// the hook; passing `None` clears it. See
    /// [`hooks::BeforeToolCallHook`] for the contract and
    /// [`hooks::BeforeToolCallOutcome`] for the supported decisions
    /// (proceed with mutated args, or short-circuit the call with a
    /// pre-baked outcome).
    pub fn set_before_tool_call(&mut self, hook: Option<hooks::BeforeToolCallHook>) {
        self.before_tool_call = hook;
    }

    /// Install a hook fired after every tool call returns, replacing
    /// any previous hook. The hook may mutate the [`ToolOutcome`] in
    /// place before [`crate::events::AgentEvent::ToolExecutionEnd`]
    /// fires and the wire `tool_result` block is built. See
    /// [`hooks::AfterToolCallHook`] for the contract; typical use is
    /// redaction, auto-truncation, or rewriting `is_error`.
    pub fn set_after_tool_call(&mut self, hook: Option<hooks::AfterToolCallHook>) {
        self.after_tool_call = hook;
    }

    /// Install a hook consulted after each assistant turn completes
    /// its tool batch, replacing any previous hook. Returning `true`
    /// short-circuits the turn — the agent emits no follow-up
    /// inference and returns control to the caller. Use case:
    /// context-window guards, per-turn budget enforcement.
    pub fn set_should_stop_after_turn(&mut self, hook: Option<hooks::ShouldStopAfterTurnHook>) {
        self.should_stop_after_turn = hook;
    }

    /// Borrow the assembled system prompt. Returns `None` until
    /// [`Agent::seed_session`] supplies one.
    pub fn assembled_system_prompt(&self) -> Option<&str> {
        self.assembled_system_prompt.as_deref()
    }

    /// Assemble the system prompt from the env's base prompt plus
    /// the agent's environment context. The binary calls this when
    /// no persisted prompt exists on the log and uses the result
    /// as the freshly-frozen system prompt for the session.
    pub fn assemble_system_prompt(&self) -> String {
        let mut text = self.env.system_prompt.content.clone();

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

        // Skills are progressive disclosure: the listing carries only
        // name/description/location and the model loads the full
        // SKILL.md with `read_file` when a task matches. Without a
        // `read_file` tool the listing is unreachable, so it is
        // omitted entirely.
        if self.tools.iter().any(|t| t.name == "read_file") {
            if let Some(block) = aj_conf::skills::format_skills_for_prompt(&self.env.skills) {
                text.push_str("\n\n");
                text.push_str(&block);
            }
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
    /// `--scripted` flag synthesises a minimal [`ModelInfo`] inline;
    /// real providers see the registry entry the binary plucked out
    /// at startup.
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
    /// `None` means "no extended thinking". The selector overlays in
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
    /// thinking effort without restarting the session. Takes
    /// effect on the next inference; in-flight turns continue with
    /// whatever they were already configured for.
    pub fn set_default_thinking(&mut self, level: Option<ThinkingConfig>) {
        self.default_thinking = level;
    }

    /// Replace the agent's inference speed mode. `None` means
    /// standard. The wire effect (provider-specific headers) travels
    /// in the [`StreamOptions`] passed to [`Agent::set_provider`];
    /// this knob keeps the user-facing value observable so
    /// sub-agent spawn events report it accurately and spawned
    /// sub-agents inherit it.
    pub fn set_speed(&mut self, speed: Option<Speed>) {
        self.speed = speed;
    }

    /// Append `message` as a user-role text input to the transcript
    /// and run one assistant turn against it.
    ///
    /// Emits [`AgentEvent::MessageStart`] / [`AgentEvent::MessageEnd`]
    /// for the user message before driving
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
    ///
    /// `cancel` is the per-turn [`CancellationToken`] the binary
    /// fires from a different code path (e.g. Ctrl+C in the TUI)
    /// to abort the in-flight turn. On cancellation `prompt`
    /// emits a synthetic
    /// `AssistantMessage { stop_reason: Aborted, ... }` `MessageEnd`
    /// — plus `is_error: true` tool-result `MessageEnd`s for any
    /// in-flight tool calls — and returns
    /// [`TurnError::Aborted`]; the transcript is left internally
    /// consistent so subsequent prompts work without manual repair.
    /// Callers that don't need cancellation pass
    /// [`CancellationToken::new()`] (a fresh token that's never
    /// fired).
    pub async fn prompt(
        &mut self,
        message: String,
        cancel: CancellationToken,
    ) -> Result<(), TurnError> {
        self.cancellation = cancel;
        self.run_top_level_turn(Some(message)).await
    }

    /// Wake the agent on pending work it can react to without a fresh
    /// user prompt: queued task-completion notices and messages the
    /// user queued while the agent was busy (steering / follow-up).
    /// Drains them into the transcript and runs a turn.
    ///
    /// Returns [`WakeOutcome::Empty`] — emitting no events at all —
    /// when there is nothing pending, which makes the binary's wake
    /// triggers idempotent: several may fire for the same work and the
    /// losers are cheap no-ops. Otherwise this is
    /// [`Agent::continue_run`] minus the ends-in-user-message
    /// precondition — the drained notices and queued messages are
    /// themselves the user-role messages that make the transcript
    /// valid for inference.
    pub async fn wake(&mut self, cancel: CancellationToken) -> Result<WakeOutcome, TurnError> {
        // Only the owner drains its own queues, and every drain point
        // holds `&mut self`, so nothing can consume them between this
        // check and the drain inside `run_top_level_turn_inner`.
        if !self.task_registry.has_notices(self.agent_id)
            && !self.message_queues.has_pending(self.agent_id)
        {
            return Ok(WakeOutcome::Empty);
        }
        self.cancellation = cancel;
        self.run_top_level_turn(None)
            .await
            .map(|()| WakeOutcome::Ran)
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
    /// [`AgentEvent::AgentStart`] / [`AgentEvent::AgentEnd`] events,
    /// and `cancel` is honoured the same way.
    pub async fn continue_run(&mut self, cancel: CancellationToken) -> Result<(), TurnError> {
        let last_is_user_or_tool_result = matches!(
            self.transcript.last().and_then(|m| m.as_wire()),
            Some(Message::User(_)) | Some(Message::ToolResult(_))
        );
        if !last_is_user_or_tool_result {
            return Err(TurnError::Fatal(anyhow!(
                "continue_run requires the transcript to end in a user-role message"
            )));
        }
        self.cancellation = cancel;
        self.run_top_level_turn(None).await
    }

    /// Shared driver for [`Agent::prompt`] / [`Agent::continue_run`]
    /// / [`Agent::wake`].
    ///
    /// `prompt` is `Some` for [`Agent::prompt`] (a fresh user
    /// message is appended before inference) and `None` for
    /// [`Agent::continue_run`] and [`Agent::wake`] (the existing
    /// transcript — including any just-drained notices — is fed back
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
        // Notices that arrived while the agent was idle land before
        // the user's new message, in arrival order.
        self.drain_task_notices().await?;

        // Messages the user queued while the agent was busy are
        // delivered here on the wake / continuation path: steering
        // first (more urgent), then follow-up. On a fresh user prompt
        // both queues are empty (the queue box only appears while the
        // agent is busy), so these are no-ops.
        self.drain_queued_messages(PendingKind::Steering).await?;
        self.drain_queued_messages(PendingKind::FollowUp).await?;

        if let Some(text) = prompt {
            // Append the user message to the in-memory transcript
            // and emit a `MessageStart` / `MessageEnd` pair so
            // listeners (renderers + the persistence listener) see
            // a complete lifecycle for the user input. The
            // transcript update happens before the bus emits so
            // the in-memory state can never trail the bus.
            let user_message = AgentMessage::wire(Message::User(UserMessage::text(text)));
            self.transcript.push(user_message.clone());
            self.bus
                .emit(AgentEvent::MessageStart {
                    agent_id: self.agent_id,
                    message: user_message.clone(),
                })
                .await
                .map_err(TurnError::Fatal)?;
            self.bus
                .emit(AgentEvent::MessageEnd {
                    agent_id: self.agent_id,
                    message: user_message,
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
        // Same prompt-top drain point as the top-level path: a
        // sub-agent that backgrounded a command hears about it on its
        // next continuation even when the task finished between runs.
        self.drain_task_notices().await?;

        // Append the prompt as the sub-agent's first user message.
        // The persistence listener chains this entry onto the
        // sub-agent's spawn entry, which the `SubAgentStart` hook
        // anchored under the parent's spawning assistant message
        // (see `aj_session::listener::persistence_listener`).
        let user_message = AgentMessage::wire(Message::User(UserMessage::text(prompt)));
        self.transcript.push(user_message.clone());
        self.bus
            .emit(AgentEvent::MessageStart {
                agent_id: self.agent_id,
                message: user_message.clone(),
            })
            .await?;
        self.bus
            .emit(AgentEvent::MessageEnd {
                agent_id: self.agent_id,
                message: user_message,
            })
            .await?;

        self.execute_turn().await?;

        // Extract the last assistant message text from the
        // sub-agent's own transcript.
        let last_assistant = self
            .transcript
            .iter()
            .rev()
            .find_map(|m| match m.as_wire() {
                Some(Message::Assistant(a)) => Some(a),
                _ => None,
            })
            .ok_or_else(|| anyhow!("sub-agent produced no assistant text output"))?;

        let last_assistant_text: String = last_assistant
            .content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text.as_str()),
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
    /// [`AgentEvent::MessageEnd`] events. The persistence
    /// listener subscribed on the bus translates them into
    /// `aj_session::ConversationView` appends, one JSONL line per
    /// event, so the on-disk state stays at-most one event behind
    /// reality (see `docs/aj-next-plan.md` §2.3b).
    ///
    /// Cancellation is honoured at three checkpoints per
    /// `docs/aj-next-plan.md` §1.8:
    ///
    /// 1. **Streaming inference.** The `response_stream.next()`
    ///    poll is `select!`-ed against [`Self::cancellation`]; on
    ///    cancel the running partial is projected onto a synthetic
    ///    `AssistantMessage { stop_reason: Aborted }` and emitted
    ///    through the normal `MessageUpdate` /  `MessageEnd`
    ///    sequence so listeners see a clean shutdown.
    /// 2. **Provider-side cancel.** The token also rides on
    ///    [`SimpleStreamOptions::base.cancel`] so the provider's
    ///    own SSE loop tears down the HTTP request and emits an
    ///    `AssistantMessageEvent::Error { reason: Aborted }`
    ///    terminal. Either path (1) or (2) wins the race; both
    ///    end in `TurnError::Aborted`.
    /// 3. **Tool execution.** Each `execute_tool().await` is
    ///    `select!`-ed against the token. On cancel we synthesize
    ///    `is_error: true` tool-result messages for the running
    ///    tool *and* every remaining tool call in the batch so
    ///    the transcript never carries a `tool_use` without a
    ///    matching `tool_result`.
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
            // Pre-iteration cancel check (cheap atomic). Lets us
            // skip an inference when cancel fired between turns
            // (e.g. while we were in the tool batch below).
            if self.cancellation.is_cancelled() {
                return Err(TurnError::Aborted);
            }

            let mut response_stream = self.run_inference_streaming();
            // Cheap clone — `CancellationToken` is `Arc`-backed and
            // the same handle is shared with the provider task via
            // `run_inference_streaming`'s `options.cancel`.
            let cancel = self.cancellation.clone();

            // Bracket the streaming inference with `MessageStart` /
            // `MessageEnd` per `docs/aj-next-plan.md` §1.1.
            // `MessageStart` carries an identity-stamped empty
            // assistant message so renderers can open their assistant
            // slot before the first content event arrives; the
            // matching `MessageEnd` fires after the stream terminates
            // and carries the finalized message.
            self.bus
                .emit(AgentEvent::MessageStart {
                    agent_id: self.agent_id,
                    message: AgentMessage::wire(Message::Assistant(self.empty_assistant_message())),
                })
                .await
                .map_err(TurnError::Fatal)?;

            // Terminal `AssistantMessage` captured from the stream's
            // `Done` (success) or `Error` (failure) event. The
            // unified streaming protocol guarantees exactly one
            // terminal event per stream, so once this is `Some` we
            // break out and stop polling.
            let mut final_message: Option<AssistantMessage> = None;
            let mut final_was_error = false;
            // Running snapshot of the latest partial, used to
            // synthesize the aborted terminal when our local
            // `select!` wins the cancel race against the provider's
            // own abort path.
            let mut latest_partial = self.empty_assistant_message();
            let mut aborted_during_stream = false;

            loop {
                tokio::select! {
                    biased;

                    // Cancel arm wins ties so a `cancel()` fired
                    // between iterations always exits, even if the
                    // provider has events queued.
                    _ = cancel.cancelled() => {
                        aborted_during_stream = true;
                        break;
                    }

                    maybe_event = response_stream.next() => {
                        let Some(event) = maybe_event else { break };

                        // Capture the terminal frames before forwarding so we
                        // can break out of the loop with the finalized
                        // message. The forwarded `MessageUpdate` still flows
                        // through for every event so listeners see the
                        // complete streaming protocol per the spec.
                        match &event {
                            AssistantMessageEvent::Done { message, .. } => {
                                final_message = Some(message.clone());
                                final_was_error = false;
                            }
                            AssistantMessageEvent::Error { error, .. } => {
                                final_message = Some(error.clone());
                                final_was_error = true;
                            }
                            _ => {}
                        }
                        latest_partial = event.partial().clone();

                        // Forward the provider event as a `MessageUpdate` on
                        // the bus. Renderers consume the inner
                        // `AssistantMessageEvent` directly (drives text /
                        // thinking / tool-call blocks); persistence listeners
                        // can ignore these since the finalized
                        // `MessageEnd` event below carries the finalized
                        // after the stream terminates.
                        let partial = event.partial().clone();
                        let is_terminal = event.is_terminal();
                        self.bus
                            .emit(AgentEvent::MessageUpdate {
                                agent_id: self.agent_id,
                                message: AgentMessage::wire(Message::Assistant(partial)),
                                event,
                            })
                            .await
                            .map_err(TurnError::Fatal)?;

                        if is_terminal {
                            break;
                        }
                    }
                }
            }

            // Resolve the terminal message. Three cases:
            //
            // 1. We saw a Done/Error event — use it directly.
            // 2. The stream ended without a terminal (channel closed
            //    silently) — fall back to `result()`, which
            //    synthesizes a transient-flavoured error.
            // 3. Our `select!` cancel arm fired — synthesize the
            //    aborted terminal from `latest_partial` and forward
            //    the matching `MessageUpdate` so streaming listeners
            //    see the terminal event.
            let final_message = if aborted_during_stream {
                let aborted_event = AssistantMessageEvent::aborted(latest_partial.clone());
                let aborted_message = aborted_event.partial().clone();
                self.bus
                    .emit(AgentEvent::MessageUpdate {
                        agent_id: self.agent_id,
                        message: AgentMessage::wire(Message::Assistant(aborted_message.clone())),
                        event: aborted_event,
                    })
                    .await
                    .map_err(TurnError::Fatal)?;
                aborted_message
            } else {
                match final_message {
                    Some(m) => m,
                    None => {
                        // The stream ended without emitting Done / Error;
                        // pull the synthesized terminal from the
                        // side-channel.
                        final_was_error = true;
                        response_stream.result().await
                    }
                }
            };
            drop(response_stream);

            // Emit `MessageEnd` so renderers can finalize their
            // assistant slot (close in-flight blocks, mark the turn
            // complete). Fires for success, error, and abort
            // terminations alike; the abort branches below consume
            // the message before the retry/recoverable handling.
            self.bus
                .emit(AgentEvent::MessageEnd {
                    agent_id: self.agent_id,
                    message: AgentMessage::wire(Message::Assistant(final_message.clone())),
                })
                .await
                .map_err(TurnError::Fatal)?;

            if aborted_during_stream {
                // Push the aborted partial onto the transcript so
                // resume sees the same shape the live session
                // did. The wire-transform layer
                // (`aj_models::transform::transform_messages`, rule
                // 5) drops `stop_reason == Aborted` assistant
                // messages — and their orphaned `tool_call` IDs —
                // before sending the next inference, so the model
                // never sees the half-formed turn.
                self.transcript
                    .push(AgentMessage::wire(Message::Assistant(final_message)));
                return Err(TurnError::Aborted);
            }

            if final_was_error {
                let assistant_err = final_message.error.clone();
                // Provider-side cancellation (Phase 1 in the model
                // layer) surfaces here as a terminal `Error` event
                // with `category == Aborted`. Route it onto the
                // same `TurnError::Aborted` path the streaming-side
                // `select!` uses so callers see one cancellation
                // shape regardless of which side won the race.
                let is_aborted_err = assistant_err
                    .as_ref()
                    .is_some_and(|e| e.category == ErrorCategory::Aborted);
                if is_aborted_err {
                    self.transcript
                        .push(AgentMessage::wire(Message::Assistant(final_message)));
                    return Err(TurnError::Aborted);
                }
                // Auto-retry the transport-transient categories with
                // backoff. `Transient` covers a stream that dropped
                // before its terminal frame (a truncated turn,
                // `docs/models-spec.md` §10.3): retrying re-issues the
                // turn instead of surfacing a cut-off answer as final.
                // `Overloaded` (provider 529/503) retries for the same
                // reason. `RateLimit` is also retryable per §10.4 but
                // must honour `retry_after_ms`, which this fixed backoff
                // does not; it surfaces as a recoverable error instead.
                let is_retryable = assistant_err.as_ref().is_some_and(|e| {
                    matches!(
                        e.category,
                        ErrorCategory::Overloaded | ErrorCategory::Transient
                    )
                });
                if is_retryable {
                    if retry_strategy.is_none() {
                        retry_strategy = Some(Self::create_retry_strategy());
                    }
                    let retry_sleep = retry_strategy.as_mut().expect("known to be some").next();
                    if let Some(retry_sleep) = retry_sleep {
                        let err_text = assistant_err
                            .as_ref()
                            .map(|e| e.message.clone())
                            .unwrap_or_else(|| "model stream failed".to_string());
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
                        // Retry sleep is `select!`-ed against cancel
                        // so a Ctrl+C during the backoff window
                        // doesn't have to wait out the timer.
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return Err(TurnError::Aborted),
                            _ = tokio::time::sleep(retry_sleep) => {}
                        }
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
            let turn_usage = response.usage.clone();

            // Collect tool calls off the finalized assistant
            // content.
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

            // Append the finalized assistant message to the
            // transcript. Persistence listeners subscribe to the
            // matching [`AgentEvent::MessageEnd`] event emitted
            // above before the loop body resumed (see the
            // bracketing earlier in this function) so the on-disk
            // record lands without a separate persistence event.
            self.transcript
                .push(AgentMessage::wire(Message::Assistant(response.clone())));

            let usage = TokenUsage {
                accumulated_input: self.session_state.accumulated_usage.input,
                turn_input: turn_usage.input,
                accumulated_output: self.session_state.accumulated_usage.output,
                turn_output: turn_usage.output,
                accumulated_cache_write: self.session_state.accumulated_usage.cache_write,
                turn_cache_write: turn_usage.cache_write,
                accumulated_cache_read: self.session_state.accumulated_usage.cache_read,
                turn_cache_read: turn_usage.cache_read,
            };
            self.bus
                .emit(AgentEvent::TurnUsage {
                    agent_id: self.agent_id,
                    usage,
                })
                .await
                .map_err(TurnError::Fatal)?;

            accumulate_usage(&mut self.session_state.accumulated_usage, &turn_usage);

            // Execute tool calls if any
            if has_tool_use {
                let mut pending = tool_calls.into_iter();
                while let Some((tool_id, tool_name, tool_input)) = pending.next() {
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

                    // Consult the before-tool-call hook (if installed).
                    // The hook can rewrite `tool_input` or short-circuit
                    // the call with a pre-baked outcome (permission
                    // denial, policy block). We clone the `Arc` here so
                    // the borrow doesn't conflict with the `&mut self`
                    // `execute_tool` call below; closures are cheap to
                    // clone since they ride behind `Arc`.
                    let before_hook = self.before_tool_call.clone();
                    let (tool_input, short_circuit_outcome) = match before_hook {
                        Some(hook) => {
                            let ctx = hooks::ToolCallContext {
                                call_id: &tool_id,
                                tool_name: &tool_name,
                            };
                            match hook(ctx, tool_input.clone()).await {
                                hooks::BeforeToolCallOutcome::Proceed { args } => (args, None),
                                hooks::BeforeToolCallOutcome::ShortCircuit { outcome } => {
                                    (tool_input, Some(outcome))
                                }
                            }
                        }
                        None => (tool_input, None),
                    };

                    // Run the tool unless the before-hook short-
                    // circuited it, racing against cancel. On cancel
                    // we drop the tool future (bash tears down its
                    // process tree; other tools just exit) and
                    // synthesize a cancelled outcome so the
                    // transcript still pairs `tool_use` with
                    // `tool_result`.
                    //
                    // Tool-input parse failures surface as a
                    // [`AssistantContent::ToolCall`] with
                    // `arguments == Value::Null`; the tool's own
                    // deserializer rejects the payload and the call
                    // bubbles up here as an `Err`. We fold that
                    // into a synthesized `ToolOutcome` with
                    // `is_error: true` so the failure rides on the
                    // same `Message::ToolResult` shape every other
                    // tool error does.
                    let outcome_or_cancel: Option<ToolOutcome> = if let Some(outcome) =
                        short_circuit_outcome
                    {
                        Some(outcome)
                    } else {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => None,
                            res = self.execute_tool(&tool_id, &tool_name, tool_input.clone()) => {
                                Some(match res {
                                    Ok(outcome) => outcome,
                                    Err(err) => {
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
                                })
                            }
                        }
                    };

                    let aborted_this_tool = outcome_or_cancel.is_none();
                    let mut outcome =
                        outcome_or_cancel.unwrap_or_else(|| cancelled_tool_outcome(&tool_name));

                    // Consult the after-tool-call hook (if installed).
                    // The hook can rewrite `outcome.content`,
                    // `outcome.details`, or `outcome.is_error` before
                    // the bus event and the wire projection fire.
                    // Same `Arc` clone dance as the before-hook so the
                    // `&mut outcome` borrow stays clean.
                    //
                    // We skip the hook on cancellation so a misbehaving
                    // hook can't swallow the abort: the cancelled
                    // outcome lands verbatim, the matching `TurnError::Aborted`
                    // is returned below.
                    if !aborted_this_tool {
                        if let Some(hook) = self.after_tool_call.clone() {
                            let ctx = hooks::ToolCallContext {
                                call_id: &tool_id,
                                tool_name: &tool_name,
                            };
                            hook(ctx, &mut outcome).await;
                        }
                    }

                    self.finalize_tool_result(&tool_id, &tool_name, outcome)
                        .await?;

                    if aborted_this_tool {
                        // Synthesize matching `tool_result` entries
                        // for every still-pending tool call so the
                        // transcript stays internally consistent —
                        // no dangling `tool_use` without a matching
                        // `tool_result`. Each emits its own
                        // ToolExecutionStart / MessageStart /
                        // MessageEnd / ToolExecutionEnd bracket so
                        // listeners get a uniform shape.
                        for (pending_id, pending_name, pending_input) in pending {
                            self.bus
                                .emit(AgentEvent::ToolExecutionStart {
                                    agent_id: self.agent_id,
                                    call_id: pending_id.clone(),
                                    tool: pending_name.clone(),
                                    args: pending_input,
                                })
                                .await
                                .map_err(TurnError::Fatal)?;
                            let cancelled = cancelled_tool_outcome(&pending_name);
                            self.finalize_tool_result(&pending_id, &pending_name, cancelled)
                                .await?;
                        }
                        return Err(TurnError::Aborted);
                    }
                }

                // Consult the should-stop-after-turn hook (if
                // installed). Returning `true` ends the turn here
                // with no follow-up inference, even though tool
                // calls completed successfully. Typical use:
                // context-window guards, per-turn budget caps.
                if let Some(hook) = self.should_stop_after_turn.clone() {
                    if hook().await {
                        break;
                    }
                }

                // Notices that arrived while the tool batch ran reach
                // the model immediately before the next inference
                // step.
                self.drain_task_notices().await?;

                // Steering messages the user queued during the tool
                // batch are injected here, right after the tool
                // results and before the next inference — the urgent
                // path. Follow-up messages are left for the wake drain
                // when the turn ends.
                self.drain_queued_messages(PendingKind::Steering).await?;

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

    /// Drain this agent's queued task-completion notices into the
    /// transcript.
    ///
    /// Each notice becomes one user-role message whose body is wrapped
    /// in a `<task-notification>` tag (marking it as harness-injected,
    /// not a user reply), emitted with the same `MessageStart` /
    /// `MessageEnd` pair the prompt path uses so it persists and
    /// renders like any other message.
    async fn drain_task_notices(&mut self) -> Result<(), TurnError> {
        let notices = self.task_registry.drain_notices(self.agent_id);
        for notice in notices {
            // An agent-backed task's detached driver parked the
            // child's accumulated usage on the registry entry; fold
            // it into session state here, where we hold `&mut self`,
            // so no shared mutability is needed.
            if let TaskKind::Agent { agent_id, .. } = &notice.kind {
                if let Some(usage) = self.task_registry.usage(notice.task_id) {
                    self.session_state.record_sub_agent_usage(*agent_id, usage);
                }
            }
            // The tag delimiters sit on their own lines so the body
            // renders as regular markdown between them instead of
            // being glued to the tag text.
            let text = format!(
                "<task-notification>\n{}\n</task-notification>",
                notice.body.trim_end()
            );
            let message = AgentMessage::wire(Message::User(UserMessage::text(text)));
            self.transcript.push(message.clone());
            self.bus
                .emit(AgentEvent::MessageStart {
                    agent_id: self.agent_id,
                    message: message.clone(),
                })
                .await
                .map_err(TurnError::Fatal)?;
            self.bus
                .emit(AgentEvent::MessageEnd {
                    agent_id: self.agent_id,
                    message,
                })
                .await
                .map_err(TurnError::Fatal)?;
        }
        Ok(())
    }

    /// Drain `kind`'s queued messages for this agent into the
    /// transcript as user messages, emitting a `MessageStart` /
    /// `MessageEnd` pair for each plus a trailing `QueueUpdate`.
    /// Returns whether anything was drained.
    ///
    /// Unlike [`Self::drain_task_notices`] these are genuine user
    /// messages with no `<task-notification>` wrapper: the user typed
    /// them while the agent was busy, so they read like any prompt.
    async fn drain_queued_messages(&mut self, kind: PendingKind) -> Result<bool, TurnError> {
        let texts = match kind {
            PendingKind::Steering => self.message_queues.drain_steering(self.agent_id),
            PendingKind::FollowUp => self.message_queues.drain_follow_up(self.agent_id),
        };
        if texts.is_empty() {
            return Ok(false);
        }
        for text in texts {
            let message = AgentMessage::wire(Message::User(UserMessage::text(text)));
            self.transcript.push(message.clone());
            self.bus
                .emit(AgentEvent::MessageStart {
                    agent_id: self.agent_id,
                    message: message.clone(),
                })
                .await
                .map_err(TurnError::Fatal)?;
            self.bus
                .emit(AgentEvent::MessageEnd {
                    agent_id: self.agent_id,
                    message,
                })
                .await
                .map_err(TurnError::Fatal)?;
        }
        // Announce the post-drain queue state so view listeners can
        // clear the pending-message box. Renderers re-read the handle
        // rather than trusting this payload (see `crate::queue`), but
        // the snapshot keeps the wire event honest for out-of-process
        // consumers.
        let (steering, follow_up) = self.message_queues.event_messages(self.agent_id);
        self.bus
            .emit(AgentEvent::QueueUpdate {
                agent_id: self.agent_id,
                steering,
                follow_up,
            })
            .await
            .map_err(TurnError::Fatal)?;
        Ok(true)
    }

    /// Project a finalized [`ToolOutcome`] onto a unified
    /// [`Message::ToolResult`] entry, append it to the transcript,
    /// and emit the matching `MessageStart` / `MessageEnd` /
    /// `ToolExecutionEnd` bracket on the bus. Shared between the
    /// success and cancellation paths of [`Self::execute_turn`] so
    /// the persisted shape is identical regardless of why the
    /// outcome was produced.
    async fn finalize_tool_result(
        &mut self,
        tool_id: &str,
        tool_name: &str,
        outcome: ToolOutcome,
    ) -> Result<(), TurnError> {
        // Project the outcome onto a unified
        // [`Message::ToolResult`] entry. The structured `details`
        // ride twice: once on the per-call
        // [`AgentEvent::ToolExecutionEnd`] event below (for live
        // renderers) and once as the `details: Option<Value>` field
        // on the [`ToolResultMessage`] we append to the transcript
        // and emit through `MessageEnd` (for resumed sessions and
        // persistence). The latter is serialized via
        // `serde_json::to_value` so it survives the on-disk JSONL
        // round-trip.
        let details_value = serde_json::to_value(&outcome.details).ok();
        // Snapshot the wire content as an `Arc<[UserContent]>` for the
        // `ToolExecutionEnd` event; cloning the Arc is O(1) and keeps
        // image-bearing results cheap to fan out across the bus.
        let content_arc: std::sync::Arc<[UserContent]> =
            std::sync::Arc::from(outcome.content.clone().into_boxed_slice());
        let tool_result = ToolResultMessage {
            tool_call_id: tool_id.to_string(),
            tool_name: tool_name.to_string(),
            content: outcome.content.clone(),
            details: details_value,
            is_error: outcome.is_error,
            timestamp: 0,
        };
        let tool_result_message = AgentMessage::wire(Message::ToolResult(tool_result));
        self.transcript.push(tool_result_message.clone());
        self.bus
            .emit(AgentEvent::MessageStart {
                agent_id: self.agent_id,
                message: tool_result_message.clone(),
            })
            .await
            .map_err(TurnError::Fatal)?;
        self.bus
            .emit(AgentEvent::MessageEnd {
                agent_id: self.agent_id,
                message: tool_result_message,
            })
            .await
            .map_err(TurnError::Fatal)?;

        self.bus
            .emit(AgentEvent::ToolExecutionEnd {
                agent_id: self.agent_id,
                call_id: tool_id.to_string(),
                tool: tool_name.to_string(),
                result: outcome.details,
                content: content_arc,
                is_error: outcome.is_error,
            })
            .await
            .map_err(TurnError::Fatal)?;
        Ok(())
    }

    /// Creates a retry strategy for handling overloaded API errors.
    fn create_retry_strategy() -> impl Iterator<Item = Duration> {
        ExponentialBackoff::from_millis(100)
            .max_delay(Duration::from_secs(2))
            .take(10)
            .map(jitter)
    }

    /// Build an empty [`AssistantMessage`] stamped with the agent's
    /// active provider / api / model identity. Used as the
    /// `MessageStart` payload before any provider event arrives, so
    /// renderers can open their assistant slot with a structurally
    /// complete message even though the content is empty.
    fn empty_assistant_message(&self) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: self.model_info.api.clone(),
            provider: self.model_info.provider.clone(),
            model: self.model_info.id.clone(),
            response_id: None,
            usage: aj_models::types::Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        }
    }

    /// Run a single streaming inference against the agent's
    /// in-memory transcript and return the resulting
    /// [`AssistantMessageEventStream`].
    ///
    /// Projects the agent's [`AgentMessage`] transcript onto the
    /// unified [`aj_models::types::Message`] sequence the
    /// [`Provider`] trait expects, projects the agent's
    /// `Vec<Tool>` onto the unified
    /// [`aj_models::types::ToolDefinition`] shape, builds a
    /// [`Context`] / [`SimpleStreamOptions`] pair, and hands them
    /// to [`Provider::stream_simple`]. The agent does not block
    /// on the stream here: it's returned to the caller, which
    /// polls it inside [`Self::execute_turn`]'s outer retry loop.
    fn run_inference_streaming(&self) -> AssistantMessageEventStream {
        let thinking = self.default_thinking.clone();

        tracing::debug!(?thinking, "thinking effort");

        let system_prompt = self
            .assembled_system_prompt
            .clone()
            .expect("system prompt must be resolved before inference");

        let messages = transcript_to_messages(&self.transcript);
        // Defense-in-depth `image_block` gate: scrub image bytes
        // from the wire-bound vector before they reach the
        // provider. Runs ahead of `transform_messages` (which the
        // provider applies); since `block_user_images` replaces
        // every `UserContent::Image` block with a text placeholder,
        // the subsequent non-vision downgrade in `transform_messages`
        // becomes a no-op on these blocks. The transcript itself is
        // untouched so persistence and future turns retain the bytes.
        let messages = if self.block_images {
            let mut m = messages;
            aj_models::transform::block_user_images(&mut m);
            m
        } else {
            messages
        };
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

        // Thread the agent's per-turn cancellation token into the
        // provider so a `cancel()` tears down the in-flight HTTP
        // request and SSE loop instead of waiting for the response
        // to finish. The provider emits an
        // `AssistantMessageEvent::Error { reason: Aborted, ... }`
        // terminal on cancel — see [`AssistantMessageEvent::aborted`].
        // The same token is also `select!`-ed in `execute_turn` so
        // the agent stops polling the moment cancel fires, regardless
        // of how quickly the provider task winds down.
        let mut base = self.stream_options.clone();
        base.cancel = Some(self.cancellation.clone());

        let options = SimpleStreamOptions {
            base,
            reasoning: thinking.as_ref().map(thinking_config_to_level),
        };

        self.provider
            .stream_simple(&self.model_info, &context, &options)
    }

    async fn execute_tool(
        &mut self,
        call_id: &str,
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
            disabled_tools: &self.disabled_tools,
            provider: Arc::clone(&self.provider),
            model_info: Arc::clone(&self.model_info),
            stream_options: self.stream_options.clone(),
            sub_agent_tools,
            parent_bus: self.bus.clone(),
            parent_agent_id: self.agent_id,
            cancellation: self.cancellation.child_token(),
            block_images: self.block_images,
            default_thinking: self.default_thinking.clone(),
            speed: self.speed,
            sub_agent_registry: self.sub_agent_registry.clone(),
            task_registry: self.task_registry.clone(),
            message_queues: self.message_queues.clone(),
            call_id: call_id.to_string(),
        };

        let outcome = (tool_def.func)(&mut session_ctx_wrapper, tool_input).await?;
        Ok(outcome)
    }
}

/// A live, re-promptable agent handle shared between the runtime and
/// the binary. Wrapping in a `tokio::sync::Mutex` lets a turn lock the
/// agent across `.await` points while other agents run concurrently.
pub type SharedAgent = Arc<tokio::sync::Mutex<Agent>>;

/// Registry of retained sub-agents keyed by their `Sub(n)` index.
///
/// Cheaply cloneable; all clones share one map. The inner
/// [`std::sync::Mutex`] guards only map lookups and inserts and is
/// never held across `.await` — a `SharedAgent`'s own
/// `tokio::sync::Mutex` is what callers lock to drive a turn.
///
/// The binary injects one instance onto the main agent so both share
/// the same map: the main agent inserts each sub-agent on spawn and the
/// binary resolves handles to drive continuations. Sub-agents never set
/// one — they can't spawn, since the `agent` tool is filtered out of
/// their toolset. Callers that never inject one (print mode, tests) get
/// the default-empty registry; retained sub-agents then live for the
/// lifetime of the owning `Agent` and drop with it.
#[derive(Clone, Default)]
pub struct SubAgentRegistry {
    inner: Arc<StdMutex<BTreeMap<usize, SharedAgent>>>,
}

impl SubAgentRegistry {
    /// Retain `agent` under key `n`. `Sub(n)` indices are minted
    /// monotonically per session, so each key is inserted exactly once.
    pub fn insert(&self, n: usize, agent: SharedAgent) {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .insert(n, agent);
    }

    /// Resolve the live handle for `Sub(n)`, if one is retained.
    pub fn get(&self, n: usize) -> Option<SharedAgent> {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .get(&n)
            .cloned()
    }

    /// Retained sub-agent indices in ascending order.
    pub fn ids(&self) -> Vec<usize> {
        self.inner
            .lock()
            .expect("registry mutex poisoned")
            .keys()
            .copied()
            .collect()
    }
}

/// One live background task tracked by the [`TaskRegistry`].
struct TaskEntry {
    owner: AgentId,
    kind: TaskKind,
    label: String,
    status: TaskStatus,
    started_at: Instant,
    /// Child of the registry's root token. Cancelled by
    /// [`TaskRegistry::kill`] or transitively by
    /// [`TaskRegistry::shutdown`]; the task's driver observes it and
    /// flips the status via [`TaskRegistry::set_status`].
    cancel: CancellationToken,
    output: Arc<dyn TaskOutputSource>,
    /// Usage accumulated by an agent-backed task's run, recorded by
    /// the driver at finish. The detached driver has no access to the
    /// owner's `SessionState`, so the value parks here until the
    /// completion notice drains and the owner folds it in.
    usage: Option<Usage>,
}

/// Display snapshot of one task, for the picker and the footer.
#[derive(Clone, Debug)]
pub struct TaskSummary {
    /// Task id.
    pub id: TaskId,
    /// Agent that owns the task and receives its notices.
    pub owner: AgentId,
    /// What kind of work the task runs.
    pub kind: TaskKind,
    /// Display label (command / task description).
    pub label: String,
    /// Current lifecycle status.
    pub status: TaskStatus,
    /// When the task was registered.
    pub started_at: Instant,
}

/// Outcome of [`Agent::wake`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeOutcome {
    /// The notice queue was empty; nothing ran and no events were
    /// emitted.
    Empty,
    /// Queued notices were drained into the transcript and a turn ran
    /// against them.
    Ran,
}

#[derive(Default)]
struct TaskRegistryInner {
    entries: BTreeMap<TaskId, TaskEntry>,
    /// Per-owner completion-notice queues, drained by the owning
    /// agent at its drain points in arrival order.
    notices: HashMap<AgentId, VecDeque<TaskNotice>>,
    /// Last minted task id; ids start at 1 and are monotonic per
    /// session.
    last_id: TaskId,
}

/// Shared registry of background tasks, sibling of
/// [`SubAgentRegistry`].
///
/// Cheaply cloneable; all clones share one map. The inner mutex guards
/// short map operations only and is never held across `.await`. Task
/// cancel tokens are children of `root_cancel`, which is
/// session-scoped: the binary cancels it on shutdown so background
/// tasks survive turn end but not process exit.
#[derive(Clone, Default)]
pub struct TaskRegistry {
    inner: Arc<StdMutex<TaskRegistryInner>>,
    root_cancel: CancellationToken,
    /// Signalled on every status flip; [`TaskRegistry::wait_terminal`]
    /// waiters wake and re-check under the lock. One registry-wide
    /// `Notify` (rather than a channel per entry) keeps `TaskEntry`
    /// simple — status flips happen once per task lifetime, so the
    /// herd of re-checking waiters is tiny.
    status_changed: Arc<tokio::sync::Notify>,
}

impl TaskRegistry {
    /// Mint a task id and insert a `Running` entry for it. Returns the
    /// id plus the task's cancel token (a child of the registry root)
    /// for the detached driver.
    pub fn register(
        &self,
        owner: AgentId,
        kind: TaskKind,
        label: String,
        output: Arc<dyn TaskOutputSource>,
    ) -> (TaskId, CancellationToken) {
        let cancel = self.root_cancel.child_token();
        let mut inner = self.inner.lock().expect("task registry mutex poisoned");
        inner.last_id += 1;
        let id = inner.last_id;
        inner.entries.insert(
            id,
            TaskEntry {
                owner,
                kind,
                label,
                status: TaskStatus::Running,
                started_at: Instant::now(),
                cancel: cancel.clone(),
                output,
                usage: None,
            },
        );
        (id, cancel)
    }

    /// Current status plus a stateless output snapshot for task `id`.
    /// `None` for unknown ids.
    pub fn read(&self, id: TaskId) -> Option<(TaskStatus, TaskRead)> {
        // Snapshot outside the lock: the source's own locking is its
        // business and we keep registry lock hold times minimal.
        let (status, output) = {
            let inner = self.inner.lock().expect("task registry mutex poisoned");
            let entry = inner.entries.get(&id)?;
            (entry.status, Arc::clone(&entry.output))
        };
        Some((status, output.snapshot()))
    }

    /// Cancel task `id`'s token. The driver observes the cancellation
    /// and flips the status. Returns `false` for unknown ids.
    pub fn kill(&self, id: TaskId) -> bool {
        let inner = self.inner.lock().expect("task registry mutex poisoned");
        match inner.entries.get(&id) {
            Some(entry) => {
                entry.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Record task `id`'s new status. Called by the driver (through
    /// [`TaskEventSink::finished`]) when the task terminates. No-op
    /// for unknown ids.
    pub fn set_status(&self, id: TaskId, status: TaskStatus) {
        {
            let mut inner = self.inner.lock().expect("task registry mutex poisoned");
            if let Some(entry) = inner.entries.get_mut(&id) {
                entry.status = status;
            }
        }
        self.status_changed.notify_waiters();
    }

    /// Current status of task `id`, `None` for unknown ids.
    pub fn status(&self, id: TaskId) -> Option<TaskStatus> {
        let inner = self.inner.lock().expect("task registry mutex poisoned");
        inner.entries.get(&id).map(|entry| entry.status)
    }

    /// Record the usage accumulated by task `id`'s agent run. No-op
    /// for unknown ids.
    pub fn record_usage(&self, id: TaskId, usage: Usage) {
        let mut inner = self.inner.lock().expect("task registry mutex poisoned");
        if let Some(entry) = inner.entries.get_mut(&id) {
            entry.usage = Some(usage);
        }
    }

    /// Usage recorded for task `id`, if any. `None` for unknown ids,
    /// bash tasks, and agent tasks whose run hasn't finished.
    pub fn usage(&self, id: TaskId) -> Option<Usage> {
        let inner = self.inner.lock().expect("task registry mutex poisoned");
        inner.entries.get(&id).and_then(|entry| entry.usage.clone())
    }

    /// Display snapshot of task `id`, `None` for unknown ids.
    pub fn summary(&self, id: TaskId) -> Option<TaskSummary> {
        let inner = self.inner.lock().expect("task registry mutex poisoned");
        inner.entries.get(&id).map(|entry| TaskSummary {
            id,
            owner: entry.owner,
            kind: entry.kind.clone(),
            label: entry.label.clone(),
            status: entry.status,
            started_at: entry.started_at,
        })
    }

    /// Resolve once task `id` reaches a terminal status, returning
    /// that status. Resolves immediately for already-terminal tasks;
    /// returns `None` for unknown ids. Callers bound the wait
    /// themselves (`select!` against a timeout or a cancellation
    /// token).
    pub async fn wait_terminal(&self, id: TaskId) -> Option<TaskStatus> {
        loop {
            // Create the `Notified` future before checking status: a
            // `Notified` receives `notify_waiters` wakeups from the
            // moment it is created, so a flip landing between the
            // check and the await cannot be lost.
            let notified = self.status_changed.notified();
            match self.status(id) {
                None => return None,
                Some(status) if status.is_terminal() => return Some(status),
                Some(_) => {}
            }
            notified.await;
        }
    }

    /// Snapshot of every tracked task, in id order.
    pub fn snapshot(&self) -> Vec<TaskSummary> {
        let inner = self.inner.lock().expect("task registry mutex poisoned");
        inner
            .entries
            .iter()
            .map(|(id, entry)| TaskSummary {
                id: *id,
                owner: entry.owner,
                kind: entry.kind.clone(),
                label: entry.label.clone(),
                status: entry.status,
                started_at: entry.started_at,
            })
            .collect()
    }

    /// Queue a completion notice on its owner's queue.
    pub fn push_notice(&self, notice: TaskNotice) {
        let mut inner = self.inner.lock().expect("task registry mutex poisoned");
        inner
            .notices
            .entry(notice.owner)
            .or_default()
            .push_back(notice);
    }

    /// Take all queued notices for `owner`, in arrival order.
    pub fn drain_notices(&self, owner: AgentId) -> Vec<TaskNotice> {
        let mut inner = self.inner.lock().expect("task registry mutex poisoned");
        inner
            .notices
            .remove(&owner)
            .map(Vec::from)
            .unwrap_or_default()
    }

    /// Whether `owner` has notices queued. The binary's wake triggers
    /// poll this.
    pub fn has_notices(&self, owner: AgentId) -> bool {
        let inner = self.inner.lock().expect("task registry mutex poisoned");
        inner.notices.get(&owner).is_some_and(|q| !q.is_empty())
    }

    /// Cancel the session-scoped root token, which cancels every
    /// task's child token. Callers then await driver quiescence with
    /// a bounded grace via [`TaskRegistry::quiesce`].
    pub fn shutdown(&self) {
        self.root_cancel.cancel();
    }

    /// Wait until every tracked task reaches a terminal status,
    /// bounded by `grace`.
    ///
    /// Returns `true` when the registry is quiescent (every entry
    /// terminal, or nothing tracked), `false` when the grace expired
    /// with a task still running. Callers fire
    /// [`TaskRegistry::shutdown`] first; drivers respond promptly to
    /// the root cancel (SIGKILL on the process group + reap, or a
    /// cancelled child run), so an expiry means a wedged driver and
    /// the caller should proceed with teardown anyway.
    pub async fn quiesce(&self, grace: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            // Create the `Notified` future before the check so a flip
            // landing between the check and the await cannot be lost
            // (same pattern as `wait_terminal`).
            let notified = self.status_changed.notified();
            if self.all_terminal() {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.all_terminal();
            }
        }
    }

    fn all_terminal(&self) -> bool {
        let inner = self.inner.lock().expect("task registry mutex poisoned");
        inner.entries.values().all(|e| e.status.is_terminal())
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

#[cfg(test)]
mod session_state_tests {
    use std::path::PathBuf;

    use super::SessionState;

    /// Covers the seam behind [`crate::AgentSeed::sub_agent_counter`]:
    /// a counter seeded to `n` mints ids strictly greater than `n`,
    /// so a resumed session never reuses persisted sub-agent ids.
    #[test]
    fn seeded_counter_mints_strictly_greater_ids() {
        let mut state = SessionState::new(PathBuf::from("/test"));
        state.seed_sub_agent_counter(3);
        assert_eq!(state.next_sub_agent_id(), 4);
        assert_eq!(state.next_sub_agent_id(), 5);
    }
}

#[cfg(test)]
mod task_registry_tests {
    use std::sync::Arc;

    use tokio_util::sync::CancellationToken;

    use super::TaskRegistry;
    use crate::events::AgentId;
    use crate::tool::{TaskKind, TaskNotice, TaskOutputSource, TaskRead, TaskStatus};

    struct StubOutput;

    impl TaskOutputSource for StubOutput {
        fn snapshot(&self) -> TaskRead {
            TaskRead {
                stdout_tail: "tail".to_string(),
                stdout_total_bytes: 4,
                ..TaskRead::default()
            }
        }
    }

    fn register(
        registry: &TaskRegistry,
        owner: AgentId,
        command: &str,
    ) -> (usize, CancellationToken) {
        registry.register(
            owner,
            TaskKind::Bash {
                command: command.to_string(),
            },
            command.to_string(),
            Arc::new(StubOutput),
        )
    }

    fn notice(owner: AgentId, task_id: usize, body: &str) -> TaskNotice {
        TaskNotice {
            owner,
            task_id,
            kind: TaskKind::Bash {
                command: "cmd".to_string(),
            },
            label: "cmd".to_string(),
            status: TaskStatus::Exited(Some(0)),
            body: body.to_string(),
        }
    }

    #[test]
    fn register_read_kill_snapshot() {
        let registry = TaskRegistry::default();
        let (id, cancel) = register(&registry, AgentId::Main, "sleep 5");
        assert_eq!(id, 1);

        let (status, read) = registry.read(id).expect("registered task is readable");
        assert_eq!(status, TaskStatus::Running);
        assert_eq!(read.stdout_tail, "tail");
        assert_eq!(read.stdout_total_bytes, 4);
        assert!(registry.read(999).is_none());

        let summaries = registry.snapshot();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, 1);
        assert_eq!(summaries[0].owner, AgentId::Main);
        assert_eq!(summaries[0].label, "sleep 5");
        assert_eq!(summaries[0].status, TaskStatus::Running);

        // `kill` cancels the task's token; the driver is responsible
        // for flipping the status, mirrored here via `set_status`.
        assert!(registry.kill(id));
        assert!(cancel.is_cancelled());
        assert!(!registry.kill(999));
        registry.set_status(id, TaskStatus::Killed);
        let (status, _) = registry.read(id).expect("killed task stays readable");
        assert_eq!(status, TaskStatus::Killed);
        assert_eq!(registry.snapshot()[0].status, TaskStatus::Killed);
    }

    #[test]
    fn task_ids_are_monotonic_across_owners() {
        let registry = TaskRegistry::default();
        let (a, _) = register(&registry, AgentId::Main, "a");
        let (b, _) = register(&registry, AgentId::Sub(1), "b");
        let (c, _) = register(&registry, AgentId::Main, "c");
        assert_eq!((a, b, c), (1, 2, 3));
    }

    #[test]
    fn shutdown_cancels_all_task_tokens() {
        let registry = TaskRegistry::default();
        let (_, cancel_a) = register(&registry, AgentId::Main, "a");
        let (_, cancel_b) = register(&registry, AgentId::Sub(3), "b");
        assert!(!cancel_a.is_cancelled());
        assert!(!cancel_b.is_cancelled());

        registry.shutdown();
        assert!(cancel_a.is_cancelled());
        assert!(cancel_b.is_cancelled());

        // Tasks registered after shutdown are born cancelled — the
        // root is already fired.
        let (_, cancel_c) = register(&registry, AgentId::Main, "c");
        assert!(cancel_c.is_cancelled());
    }

    #[test]
    fn notices_queue_per_owner_in_arrival_order() {
        let registry = TaskRegistry::default();
        assert!(!registry.has_notices(AgentId::Main));
        assert!(registry.drain_notices(AgentId::Main).is_empty());

        registry.push_notice(notice(AgentId::Main, 1, "first"));
        registry.push_notice(notice(AgentId::Sub(2), 2, "sub"));
        registry.push_notice(notice(AgentId::Main, 3, "second"));

        assert!(registry.has_notices(AgentId::Main));
        assert!(registry.has_notices(AgentId::Sub(2)));

        let drained: Vec<String> = registry
            .drain_notices(AgentId::Main)
            .into_iter()
            .map(|n| n.body)
            .collect();
        assert_eq!(drained, vec!["first", "second"]);

        // Draining one owner leaves the other's queue intact, and a
        // second drain returns nothing.
        assert!(!registry.has_notices(AgentId::Main));
        assert!(registry.drain_notices(AgentId::Main).is_empty());
        assert_eq!(registry.drain_notices(AgentId::Sub(2)).len(), 1);
    }

    /// `read` is a pure observation: an explicit terminal read does
    /// not consume the completion notice — the lifecycle is
    /// unconditional (task ends → notice queued → notice drained).
    #[test]
    fn terminal_read_does_not_consume_notices() {
        let registry = TaskRegistry::default();
        let (id, _) = register(&registry, AgentId::Main, "echo hi");
        registry.set_status(id, TaskStatus::Exited(Some(0)));
        registry.push_notice(notice(AgentId::Main, id, "done"));

        let (status, _) = registry.read(id).expect("terminal task stays readable");
        assert!(status.is_terminal());
        assert!(registry.has_notices(AgentId::Main));
        assert_eq!(registry.drain_notices(AgentId::Main).len(), 1);
    }

    #[tokio::test]
    async fn wait_terminal_resolves_on_status_flip() {
        let registry = TaskRegistry::default();
        let (id, _) = register(&registry, AgentId::Main, "sleep 5");

        // Unknown ids resolve immediately with `None`.
        assert_eq!(registry.wait_terminal(999).await, None);

        let flipper = {
            let registry = registry.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                registry.set_status(id, TaskStatus::Exited(Some(0)));
            })
        };
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            registry.wait_terminal(id),
        )
        .await
        .expect("wait_terminal resolves once the status flips");
        assert_eq!(status, Some(TaskStatus::Exited(Some(0))));
        flipper.await.expect("flipper joined");

        // Already-terminal tasks resolve immediately.
        assert_eq!(
            registry.wait_terminal(id).await,
            Some(TaskStatus::Exited(Some(0)))
        );
    }

    #[tokio::test]
    async fn quiesce_resolves_when_all_entries_are_terminal() {
        let registry = TaskRegistry::default();
        // An empty registry is trivially quiescent.
        assert!(registry.quiesce(std::time::Duration::from_millis(10)).await);

        let (a, _) = register(&registry, AgentId::Main, "a");
        let (b, _) = register(&registry, AgentId::Sub(1), "b");
        registry.set_status(a, TaskStatus::Exited(Some(0)));
        registry.set_status(b, TaskStatus::Killed);
        assert!(registry.quiesce(std::time::Duration::from_millis(10)).await);

        // A driver flipping its status during the wait resolves the
        // quiesce before the grace expires.
        let (c, _) = register(&registry, AgentId::Main, "c");
        let flipper = {
            let registry = registry.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                registry.set_status(c, TaskStatus::Killed);
            })
        };
        assert!(registry.quiesce(std::time::Duration::from_secs(5)).await);
        flipper.await.expect("flipper joined");
    }

    #[tokio::test(start_paused = true)]
    async fn quiesce_times_out_gracefully_on_a_wedged_driver() {
        let registry = TaskRegistry::default();
        // The entry never flips terminal — a wedged driver. Quiesce
        // must return `false` once the grace expires rather than
        // hang.
        let (_, _cancel) = register(&registry, AgentId::Main, "wedged");
        assert!(!registry.quiesce(std::time::Duration::from_secs(2)).await);
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
    /// Parent's `image_block` setting; propagated to spawned
    /// sub-agents so the defense-in-depth gate stays uniform.
    block_images: bool,
    /// Parent's default thinking level; propagated to spawned
    /// sub-agents so they reason at the same effort as the parent
    /// (and so non-reasoning models never receive an explicit
    /// `disabled` they reject) rather than always defaulting off.
    default_thinking: Option<ThinkingConfig>,
    /// Parent's inference speed mode; reported on the
    /// `SubAgentStart` event and propagated to spawned sub-agents.
    speed: Option<Speed>,
    /// Shared registry the parent agent uses to retain spawned
    /// sub-agents. Cloned from the parent so [`Self::spawn_agent`]
    /// inserts the new handle into the same map the binary resolves
    /// continuations against.
    sub_agent_registry: SubAgentRegistry,
    /// Shared background-task registry, cloned from the parent so
    /// tasks started by this tool call (and by spawned sub-agents)
    /// land in the same map the binary observes.
    task_registry: TaskRegistry,
    /// Shared steering / follow-up message queues, cloned from the
    /// parent so a spawned sub-agent drains the same per-agent slots
    /// the binary's TUI enqueues onto.
    message_queues: MessageQueues,
    /// Id of the tool call this wrapper was built for. Captured on the
    /// [`TaskEventSink`] so task lifecycle events correlate with the
    /// originating transcript cell.
    call_id: String,
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
        mode: SpawnMode,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<SpawnResult>> + Send + 'b>>
    {
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
            // The bundle identity carried on the event mirrors the
            // parent's bundle, which is exactly what the child is
            // built from below.
            self.parent_bus
                .emit(AgentEvent::SubAgentStart {
                    parent: self.parent_agent_id,
                    child: child_id,
                    task: task.clone(),
                    settings: AgentSettings {
                        provider: self.model_info.provider.clone(),
                        model_id: self.model_info.id.clone(),
                        thinking: aj_models::thinking_config_name(self.default_thinking.as_ref())
                            .to_string(),
                        speed: aj_models::speed_name(self.speed).to_string(),
                    },
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
            // hierarchy talks to the same backend. The thinking level
            // is applied separately below via `set_default_thinking`
            // because `with_provider` takes a `ConfigThinkingLevel`
            // while the parent already holds a resolved
            // `ThinkingConfig`.
            let mut sub_agent = Agent::with_provider(
                self.env.clone(),
                sub_agent_tools,
                disabled_tools,
                Arc::clone(&self.provider),
                Arc::clone(&self.model_info),
                self.stream_options.clone(),
                None,
            );
            sub_agent.set_agent_id(child_id);
            // Sub-agents inherit the parent's assembled system
            // prompt verbatim so the session has a single,
            // consistent prompt across the hierarchy. The transcript
            // and counter parts of the seed stay at their defaults:
            // the child starts with an empty history and mints no
            // persisted-id collisions of its own.
            sub_agent.seed_session(AgentSeed {
                assembled_system_prompt: Some(self.assembled_system_prompt.clone()),
                ..AgentSeed::default()
            });
            // Sub-agents inherit the parent's `image_block` setting
            // so the defense-in-depth gate stays uniform across the
            // hierarchy.
            sub_agent.set_block_images(self.block_images);
            // Sub-agents inherit the parent's thinking level so they
            // reason at the same effort and so a `None` default never
            // gets serialized as an explicit `disabled` for models
            // that reject it.
            sub_agent.set_default_thinking(self.default_thinking.clone());
            // Sub-agents inherit the parent's speed so their own
            // spawn events (and the spawn entry persisted off them)
            // report the speed they actually run at.
            sub_agent.set_speed(self.speed);
            // Share the background-task registry so tasks the
            // sub-agent starts land in the same map the binary
            // observes, with notices scoped to the sub-agent's own
            // id.
            sub_agent.set_task_registry(self.task_registry.clone());
            // Share the steering / follow-up queues so the user can
            // steer or queue for this sub-agent from its view; the
            // sub-agent drains its own `Sub(n)`-keyed slots.
            sub_agent.set_message_queues(self.message_queues.clone());
            // Share the parent's bus per `docs/aj-next-plan.md`
            // §1.6: every event the sub-agent emits during its
            // run reaches the listeners the binary registered on
            // the parent (rendering, persistence), tagged with
            // `Sub(n)`. Without this the sub-agent runs on its
            // own bus and the binary's bridge listener never sees
            // its activity.
            sub_agent.set_bus(self.parent_bus.clone());

            // Wire the run's cancellation per mode. A blocking run is
            // nested in the parent's turn, so it derives from the
            // parent's token (via `child_token` so a future
            // per-sub-agent cancel stays possible). A background run
            // must outlive the parent's turn, so it hangs off the
            // background task's token instead: `task_stop`, the TUI's
            // kill action, and shutdown all reach the run through it,
            // while cancelling the parent's turn deliberately does
            // not.
            let background = match mode {
                SpawnMode::Blocking => {
                    sub_agent.set_cancellation(self.cancellation.child_token());
                    None
                }
                SpawnMode::Background => {
                    let output = Arc::new(AgentTaskOutput::default());
                    let output_dyn: Arc<dyn TaskOutputSource> =
                        Arc::<AgentTaskOutput>::clone(&output);
                    let kind = TaskKind::Agent {
                        agent_id,
                        task: task.clone(),
                    };
                    let started = self.start_background_task(
                        kind.clone(),
                        format!("agent {agent_id}"),
                        output_dyn,
                    );
                    sub_agent.set_cancellation(started.cancel.clone());
                    Some((started, kind, output))
                }
            };

            // Retain the sub-agent in the shared registry (both
            // modes) so the binary can drive later continuations.
            let shared: SharedAgent = Arc::new(tokio::sync::Mutex::new(sub_agent));
            self.sub_agent_registry
                .insert(agent_id, Arc::clone(&shared));

            if let Some((started, kind, output)) = background {
                let StartedTask { id, cancel, events } = started;
                events.started(kind.clone()).await;
                tokio::spawn(drive_background_agent(BackgroundAgentRun {
                    shared,
                    task,
                    kind,
                    agent_id,
                    task_id: id,
                    parent: self.parent_agent_id,
                    parent_bus: self.parent_bus.clone(),
                    registry: self.task_registry.clone(),
                    cancel,
                    events,
                    output,
                }));
                return Ok(SpawnResult::Started {
                    agent_id,
                    task_id: id,
                });
            }

            // Blocking mode: run the initial turn through the
            // retained handle. The handle stays in the registry after
            // the run; the parent's tool result is still the first
            // report, so the `agent` tool contract is unchanged.
            let (result, sub_agent_usage) = {
                let mut guard = shared.lock().await;
                let result = guard.run_single_turn(task).await;
                let usage = guard.session_state.accumulated_usage.clone();
                (result, usage)
            };

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
            result.map(|report| SpawnResult::Completed(SpawnedAgent { agent_id, report }))
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

    fn task_registry(&self) -> TaskRegistry {
        self.task_registry.clone()
    }

    fn start_background_task(
        &mut self,
        kind: TaskKind,
        label: String,
        output: Arc<dyn TaskOutputSource>,
    ) -> StartedTask {
        let (id, cancel) =
            self.task_registry
                .register(self.parent_agent_id, kind, label.clone(), output);
        let events = TaskEventSink::new(
            self.parent_bus.clone(),
            self.task_registry.clone(),
            self.parent_agent_id,
            id,
            self.call_id.clone(),
            label,
        );
        StartedTask { id, cancel, events }
    }
}

/// [`TaskOutputSource`] for agent-backed background tasks.
///
/// Agent runs have no process streams to tail; the only output is the
/// final report, which the driver stores here right before flipping
/// the task terminal so a terminal `task_output` read always sees it.
#[derive(Default)]
struct AgentTaskOutput {
    report: StdMutex<Option<String>>,
}

impl AgentTaskOutput {
    fn set_report(&self, report: String) {
        *self
            .report
            .lock()
            .expect("agent task output mutex poisoned") = Some(report);
    }
}

impl TaskOutputSource for AgentTaskOutput {
    fn snapshot(&self) -> TaskRead {
        TaskRead {
            report: self
                .report
                .lock()
                .expect("agent task output mutex poisoned")
                .clone(),
            ..TaskRead::default()
        }
    }
}

/// Everything the detached driver of a background agent spawn needs,
/// moved off [`SessionContextWrapper::spawn_agent`] into
/// [`drive_background_agent`].
struct BackgroundAgentRun {
    shared: SharedAgent,
    task: String,
    kind: TaskKind,
    agent_id: usize,
    task_id: TaskId,
    parent: AgentId,
    parent_bus: EventBus,
    registry: TaskRegistry,
    cancel: CancellationToken,
    events: TaskEventSink,
    output: Arc<AgentTaskOutput>,
}

/// Drive a background agent spawn to completion: run the child's
/// initial turn, park its usage on the registry entry, emit
/// `SubAgentEnd`, and finish the task with the report as the notice
/// body.
async fn drive_background_agent(run: BackgroundAgentRun) {
    let BackgroundAgentRun {
        shared,
        task,
        kind,
        agent_id,
        task_id,
        parent,
        parent_bus,
        registry,
        cancel,
        events,
        output,
    } = run;

    let (result, usage) = {
        let mut guard = shared.lock().await;
        let result = guard.run_single_turn(task).await;
        let usage = guard.session_state.accumulated_usage.clone();
        (result, usage)
    };
    // The owner folds this into `SessionState.sub_agent_usage` when
    // the completion notice drains; a detached driver has no `&mut`
    // access to the owner's session state.
    registry.record_usage(task_id, usage);

    // Emit `SubAgentEnd` regardless of success — same contract as the
    // blocking path; listeners need to clean up nested-transcript
    // framing on errors too. Listener failures are logged and
    // swallowed like the sink does: a run finishing outside any turn
    // has no turn to abort.
    //
    // NOTE: this emit happens after the child's own `AgentEnd` and
    // after the lock is released, so a wake on the sub-agent (for a
    // task it owns itself) can interleave as `AgentEnd(Sub)` →
    // `AgentStart(Sub)` → `SubAgentEnd`. Benign: the pump's running
    // set keys off the `Agent*` pair, and persistence tolerates
    // post-`SubAgentEnd` continuations.
    let report = match &result {
        Ok(text) => text.clone(),
        Err(err) => format!("sub-agent failed: {err:#}"),
    };
    let emit_result = parent_bus
        .emit(AgentEvent::SubAgentEnd {
            parent,
            child: AgentId::Sub(agent_id),
            report: report.clone(),
        })
        .await;
    if let Err(err) = emit_result {
        tracing::warn!(task_id, "sub-agent end listener failed: {err:#}");
    }

    // Status mapping: agent runs have no process exit code, so we
    // borrow the process conventions the shared status rendering
    // reads naturally — a completed run is `Exited(Some(0))`, a
    // failed one `Exited(Some(1))`, and a run cancelled through the
    // task token (task_stop / TUI kill / shutdown) is `Killed`. A
    // run that completed before a late cancel still counts as
    // completed. Any *error* under a fired token classifies as
    // `Killed` — a genuine run failure racing a concurrent kill
    // loses its error text, which is acceptable: the kill was
    // requested and the run is gone either way.
    let (status, report_text) = match &result {
        Ok(text) => (TaskStatus::Exited(Some(0)), Some(text.clone())),
        Err(_) if cancel.is_cancelled() => (TaskStatus::Killed, None),
        Err(_) => (TaskStatus::Exited(Some(1)), Some(report)),
    };
    // Store the report before the status flips terminal so a
    // terminal `task_output` read never observes a finished task
    // without its report. A killed run stores none — the status line
    // already says everything.
    if let Some(text) = &report_text {
        output.set_report(text.clone());
    }

    let outcome_text = report_text.as_deref().unwrap_or("killed");
    let label = events.label().to_string();
    let notice = TaskNotice {
        owner: events.owner(),
        task_id,
        kind,
        label: label.clone(),
        status,
        body: format!("Background task #{task_id} finished: {label} — {outcome_text}"),
    };
    events.finished(status, notice).await;
}

/// Inspect a freshly-seeded transcript for `tool_call` blocks that
/// never received a matching `tool_result`. This is the in-memory
/// counterpart of `aj_session::repair_interrupted_tool_uses`: the
/// binary calls the session-side helper to write recovery entries
/// to disk, then re-seeds the agent; if the binary instead seeds
/// without repairing, [`scan_dangling_tool_uses`] surfaces the
/// invariant violation here. Used by the agent's tests; not part
/// of the run-time path.
#[cfg(test)]
fn scan_dangling_tool_uses(transcript: &[AgentMessage]) -> std::collections::HashSet<String> {
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut resolved: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in transcript {
        match msg.as_wire() {
            Some(Message::Assistant(a)) => {
                for c in &a.content {
                    if let AssistantContent::ToolCall(tc) = c {
                        used.insert(tc.id.clone());
                    }
                }
            }
            Some(Message::ToolResult(tr)) => {
                resolved.insert(tr.tool_call_id.clone());
            }
            _ => {}
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
/// `Aborted` mirrors `Recoverable` from the binary's perspective —
/// the session stays alive and the user can re-prompt — but
/// distinguishes "the user cancelled this turn" from "the model
/// returned an error", per `docs/aj-next-plan.md` §1.8.
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
    /// The current turn was cancelled via the agent's
    /// [`CancellationToken`]. Before returning the agent has already
    /// emitted the synthetic
    /// `AssistantMessage { stop_reason: Aborted }` `MessageEnd` and
    /// any `is_error: true` tool-result `MessageEnd`s needed to keep
    /// the transcript internally consistent, so callers can treat
    /// this exactly like `Recoverable` and continue the session.
    #[error("turn aborted by client")]
    Aborted,
}

impl From<anyhow::Error> for TurnError {
    fn from(e: anyhow::Error) -> Self {
        TurnError::Fatal(e)
    }
}

/// Map the agent / binary's [`ThinkingConfig`] policy onto the
/// unified [`ThinkingLevel`] the [`Provider`] trait consumes.
///
/// The mapping is one-to-one: what the user sets is what the provider
/// receives. Levels a model can't honour are rejected by the provider
/// (see [`aj_models::registry::validate_thinking_level`]) rather than
/// silently downgraded here.
fn thinking_config_to_level(level: &ThinkingConfig) -> ThinkingLevel {
    match level {
        ThinkingConfig::Low => ThinkingLevel::Low,
        ThinkingConfig::Medium => ThinkingLevel::Medium,
        ThinkingConfig::High => ThinkingLevel::High,
        ThinkingConfig::XHigh => ThinkingLevel::XHigh,
        ThinkingConfig::Max => ThinkingLevel::Max,
    }
}

/// Sum `other` into `acc`. Counters are added; the cost subfield
/// is summed dimension-by-dimension. `total_tokens` is recomputed
/// off the per-dimension counters so it stays internally
/// consistent with `input + output + cache_read + cache_write`.
fn accumulate_usage(acc: &mut Usage, other: &Usage) {
    acc.input += other.input;
    acc.output += other.output;
    acc.cache_read += other.cache_read;
    acc.cache_write += other.cache_write;
    acc.total_tokens += other.total_tokens;
    acc.cost.input += other.cost.input;
    acc.cost.output += other.cost.output;
    acc.cost.cache_read += other.cost.cache_read;
    acc.cost.cache_write += other.cost.cache_write;
    acc.cost.total += other.cost.total;
}

/// Build the canonical `is_error: true` [`ToolOutcome`] used when a
/// tool's `execute()` future is cancelled mid-flight, or when a
/// later tool in the same batch never got a chance to start. The
/// text body matches `bash`'s "Command cancelled" line so renderers
/// don't have to special-case the agent's synth vs a tool's own
/// cancel report; the structured `details` carry the same string
/// for persistence.
fn cancelled_tool_outcome(tool_name: &str) -> ToolOutcome {
    let body = format!("{tool_name}: cancelled by user");
    ToolOutcome {
        content: vec![UserContent::text(body.clone())],
        details: ToolDetails::Text {
            summary: format!("{tool_name}: cancelled"),
            body,
        },
        is_error: true,
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
    use std::sync::{Arc, Mutex};

    use aj_conf::{AgentEnv, SystemPrompt, SystemPromptSource};
    use aj_models::provider::Provider;
    use aj_models::registry::{InputModality, ModelCost, ModelInfo};
    use aj_models::scripted::{ExhaustedBehavior, ProviderScript, ScriptedProvider};
    use aj_models::streaming::{AssistantMessageEvent, DoneReason};
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, StopReason, StreamOptions, TextContent,
        ToolCall, UserMessage,
    };
    use tokio_util::sync::CancellationToken;

    use crate::bus::listener_from_sync;
    use crate::events::{AgentEvent, AgentId};
    use crate::message::AgentMessage;
    use crate::queue::MessageQueues;
    use crate::tool::{
        ErasedToolDefinition, TaskKind, TaskNotice, TaskStatus, ToolContext, ToolDefinition,
        ToolDetails, ToolOutcome,
    };
    use crate::{Agent, AgentSeed, TaskRegistry};

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

    /// Build a script that simulates a provider acknowledging cancellation
    /// by emitting an `Error { reason: Aborted, ... }` terminal carrying a
    /// partial message with `stop_reason = Aborted` and
    /// `error.category = Aborted`. Mirrors the actual provider behaviour
    /// when `StreamOptions::cancel` fires mid-stream so the agent's
    /// error-category branch in `execute_turn` sees the same shape it
    /// does in production.
    fn aborted_script(mut partial: AssistantMessage) -> Vec<AssistantMessageEvent> {
        use aj_models::streaming::ErrorReason;
        use aj_models::types::{AssistantError, ErrorCategory};
        partial.stop_reason = StopReason::Aborted;
        partial.error = Some(AssistantError::new(
            ErrorCategory::Aborted,
            "client cancelled the request",
        ));
        vec![
            AssistantMessageEvent::Start {
                partial: partial.clone(),
            },
            AssistantMessageEvent::Error {
                reason: ErrorReason::Aborted,
                error: partial,
            },
        ]
    }

    /// Build a script whose terminal event is a retryable transient
    /// `Error` — the shape a provider emits for a stream that dropped
    /// before its terminal frame (a truncated turn,
    /// `docs/models-spec.md` §10.3, via `AssistantMessageEvent::truncated`).
    /// The agent's retry layer should re-issue the turn rather than
    /// surface this as a finished answer.
    fn transient_error_script() -> Vec<AssistantMessageEvent> {
        use aj_models::streaming::ErrorReason;
        use aj_models::types::{AssistantError, ErrorCategory};
        let mut partial = AssistantMessage::empty();
        partial.api = SCRIPT_API.to_string();
        partial.provider = SCRIPT_PROVIDER.to_string();
        partial.model = SCRIPT_MODEL.to_string();
        partial.stop_reason = StopReason::Error;
        partial.error = Some(AssistantError::new(
            ErrorCategory::Transient,
            "stream ended without a terminal event",
        ));
        vec![
            AssistantMessageEvent::Start {
                partial: partial.clone(),
            },
            AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error: partial,
            },
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
        /// Unified message lifecycle event. `kind` records the
        /// inner wire-message variant (`User` / `Assistant` /
        /// `ToolResult`) so test assertions stay legible.
        Message {
            agent_id: AgentId,
            phase: &'static str,
            kind: &'static str,
        },
        /// `MessageUpdate` carrying an `AssistantMessageEvent`. The
        /// inner event kind (`text_delta`, `thinking_start`, …) is
        /// captured as a `&'static str` so the locked sequence
        /// remains comparable.
        MessageStream {
            agent_id: AgentId,
            event_kind: &'static str,
        },
        TurnUsage(AgentId),
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
                content: _,
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
                ..
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
            AgentEvent::TurnUsage { agent_id, .. } => EventLabel::TurnUsage(*agent_id),
            AgentEvent::TurnEnd { .. } => EventLabel::Other("TurnEnd"),
            AgentEvent::MessageStart { agent_id, message } => EventLabel::Message {
                agent_id: *agent_id,
                phase: "start",
                kind: agent_message_kind_label(message),
            },
            AgentEvent::MessageUpdate {
                agent_id, event, ..
            } => EventLabel::MessageStream {
                agent_id: *agent_id,
                event_kind: assistant_event_kind_label(event),
            },
            AgentEvent::MessageEnd { agent_id, message } => EventLabel::Message {
                agent_id: *agent_id,
                phase: "end",
                kind: agent_message_kind_label(message),
            },
            AgentEvent::ToolExecutionUpdate { .. } => EventLabel::Other("ToolExecutionUpdate"),
            AgentEvent::QueueUpdate { .. } => EventLabel::Other("QueueUpdate"),
            AgentEvent::TaskStart { .. } => EventLabel::Other("TaskStart"),
            AgentEvent::TaskOutput { .. } => EventLabel::Other("TaskOutput"),
            AgentEvent::TaskEnd { .. } => EventLabel::Other("TaskEnd"),
        }
    }

    /// Return a stable `&'static str` for the wire-message kind
    /// inside an [`AgentMessage`] so the test labels stay readable.
    fn agent_message_kind_label(message: &crate::message::AgentMessage) -> &'static str {
        use crate::message::AgentMessageKind;
        match &message.kind {
            AgentMessageKind::Wire(Message::User(_)) => "User",
            AgentMessageKind::Wire(Message::Assistant(_)) => "Assistant",
            AgentMessageKind::Wire(Message::ToolResult(_)) => "ToolResult",
        }
    }

    /// Return a stable `&'static str` for an `AssistantMessageEvent`
    /// variant.
    fn assistant_event_kind_label(
        event: &aj_models::streaming::AssistantMessageEvent,
    ) -> &'static str {
        use aj_models::streaming::AssistantMessageEvent::*;
        match event {
            Start { .. } => "start",
            TextStart { .. } => "text_start",
            TextDelta { .. } => "text_delta",
            TextEnd { .. } => "text_end",
            ThinkingStart { .. } => "thinking_start",
            ThinkingDelta { .. } => "thinking_delta",
            ThinkingEnd { .. } => "thinking_end",
            ToolCallStart { .. } => "tool_call_start",
            ToolCallDelta { .. } => "tool_call_delta",
            ToolCallEnd { .. } => "tool_call_end",
            Done { .. } => "done",
            Error { .. } => "error",
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
            system_prompt: SystemPrompt {
                content: "irrelevant".to_string(),
                source: SystemPromptSource::Builtin,
            },
            context_files: Vec::new(),
            skills: Vec::new(),
            skill_diagnostics: Vec::new(),
        }
    }

    fn build_agent(
        scripts: Vec<Vec<AssistantMessageEvent>>,
        tools: Vec<ErasedToolDefinition>,
    ) -> Agent {
        build_agent_with_transcript(scripts, tools, Vec::new())
    }

    fn build_agent_with_transcript(
        scripts: Vec<Vec<AssistantMessageEvent>>,
        tools: Vec<ErasedToolDefinition>,
        transcript: Vec<AgentMessage>,
    ) -> Agent {
        let scripts = scripts
            .into_iter()
            .map(ProviderScript::from_events)
            .collect();
        build_agent_scripts_with_transcript(scripts, tools, transcript)
    }

    /// Like [`build_agent`], but takes full [`ProviderScript`]s so
    /// tests can script per-step delays (e.g. a child inference that
    /// stalls until cancelled).
    fn build_agent_scripts(
        scripts: Vec<ProviderScript>,
        tools: Vec<ErasedToolDefinition>,
    ) -> Agent {
        build_agent_scripts_with_transcript(scripts, tools, Vec::new())
    }

    fn build_agent_scripts_with_transcript(
        scripts: Vec<ProviderScript>,
        tools: Vec<ErasedToolDefinition>,
        transcript: Vec<AgentMessage>,
    ) -> Agent {
        // Strict-mode scripted provider: panic if the agent runs more
        // inferences than the test scripted, which would indicate a
        // regression that adds an unexpected loop iteration.
        let provider: Arc<dyn Provider> =
            Arc::new(ScriptedProvider::new(scripts).on_exhausted(ExhaustedBehavior::Panic));
        let model_info = Arc::new(scripted_model_info());
        let env = empty_env(std::env::temp_dir());
        let mut agent = Agent::with_provider(
            env,
            tools,
            Vec::new(),
            provider,
            model_info,
            StreamOptions::default(),
            None,
        );
        agent.seed_session(AgentSeed {
            transcript,
            assembled_system_prompt: Some("test system prompt".to_string()),
            ..AgentSeed::default()
        });
        agent
    }

    /// Stub with the builtin `read_file` tool's name, so prompt-assembly
    /// tests can flip the skills listing's read-tool gate without pulling
    /// in `aj-tools`.
    #[derive(Clone)]
    struct ReadFileStubTool;

    impl ToolDefinition for ReadFileStubTool {
        type Input = PingInput;

        fn name(&self) -> &'static str {
            "read_file"
        }

        fn description(&self) -> &'static str {
            "Test stub"
        }

        async fn execute(
            &self,
            _ctx: &mut dyn ToolContext,
            _input: PingInput,
        ) -> anyhow::Result<ToolOutcome> {
            unreachable!("never executed in prompt-assembly tests")
        }
    }

    #[test]
    fn assemble_system_prompt_lists_skills_behind_read_file_gate() {
        let skill = |name: &str, enabled: bool, dmi: bool| aj_conf::skills::Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            path: std::path::PathBuf::from(format!("/skills/{name}/SKILL.md")),
            enabled,
            disable_model_invocation: dmi,
        };
        let mut env = empty_env(std::env::temp_dir());
        env.skills = vec![
            skill("alpha", true, false),
            skill("beta", false, false),
            skill("gamma", true, true),
        ];

        let agent_with_tools = |env: AgentEnv, tools: Vec<ErasedToolDefinition>| {
            let provider: Arc<dyn Provider> = Arc::new(
                ScriptedProvider::from_event_vecs(vec![]).on_exhausted(ExhaustedBehavior::Panic),
            );
            Agent::with_provider(
                env,
                tools,
                Vec::new(),
                provider,
                Arc::new(scripted_model_info()),
                StreamOptions::default(),
                None,
            )
        };

        // With a read_file tool: only the enabled, model-visible skill
        // is listed.
        let agent = agent_with_tools(env.clone(), vec![ReadFileStubTool.into()]);
        let prompt = agent.assemble_system_prompt();
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>alpha</name>"));
        assert!(!prompt.contains("beta"));
        assert!(!prompt.contains("gamma"));
        // The listing precedes the trailing <env> block.
        assert!(
            prompt.find("</available_skills>").unwrap() < prompt.find("<env>").unwrap(),
            "skills listing must come before the env block"
        );

        // Without a read_file tool the listing is omitted.
        let agent = agent_with_tools(env, vec![PingTool.into()]);
        let prompt = agent.assemble_system_prompt();
        assert!(!prompt.contains("<available_skills>"));
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
        let recorded_clone = Arc::clone(&recorded);
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
            // The sub-agent's user prompt brackets with
            // MessageStart/End around the User wire-message; the
            // persistence listener subscribes to MessageEnd
            // directly.
            EventLabel::Message {
                agent_id: AgentId::Sub(1),
                phase: "start",
                kind: "User",
            },
            EventLabel::Message {
                agent_id: AgentId::Sub(1),
                phase: "end",
                kind: "User",
            },
            EventLabel::TurnStart(AgentId::Sub(1)),
            // First inference: MessageStart opens the assistant
            // slot, the streaming protocol's Start event flows
            // through MessageUpdate (script step 0 emits Start
            // + Done), then MessageEnd carries the finalized
            // tool-use message (persistence subscribes to
            // MessageEnd directly).
            EventLabel::Message {
                agent_id: AgentId::Sub(1),
                phase: "start",
                kind: "Assistant",
            },
            EventLabel::MessageStream {
                agent_id: AgentId::Sub(1),
                event_kind: "start",
            },
            EventLabel::MessageStream {
                agent_id: AgentId::Sub(1),
                event_kind: "done",
            },
            EventLabel::Message {
                agent_id: AgentId::Sub(1),
                phase: "end",
                kind: "Assistant",
            },
            EventLabel::TurnUsage(AgentId::Sub(1)),
            EventLabel::ToolExecutionStart {
                agent_id: AgentId::Sub(1),
                call_id: "tu-1".to_string(),
                tool: "ping".to_string(),
            },
            // After each tool call: MessageStart/End around the
            // ToolResult wire-message, then ToolExecutionEnd with
            // the structured renderer payload.
            EventLabel::Message {
                agent_id: AgentId::Sub(1),
                phase: "start",
                kind: "ToolResult",
            },
            EventLabel::Message {
                agent_id: AgentId::Sub(1),
                phase: "end",
                kind: "ToolResult",
            },
            EventLabel::ToolExecutionEnd {
                agent_id: AgentId::Sub(1),
                call_id: "tu-1".to_string(),
                tool: "ping".to_string(),
                summary: "ping".to_string(),
                body: "pong".to_string(),
                is_error: false,
            },
            // Second inference: same Start/stream/End bracket as
            // the first; the model returned plain text this time.
            EventLabel::Message {
                agent_id: AgentId::Sub(1),
                phase: "start",
                kind: "Assistant",
            },
            EventLabel::MessageStream {
                agent_id: AgentId::Sub(1),
                event_kind: "start",
            },
            EventLabel::MessageStream {
                agent_id: AgentId::Sub(1),
                event_kind: "done",
            },
            EventLabel::Message {
                agent_id: AgentId::Sub(1),
                phase: "end",
                kind: "Assistant",
            },
            EventLabel::TurnUsage(AgentId::Sub(1)),
            EventLabel::AgentEnd(AgentId::Sub(1)),
        ];
        assert_eq!(events, expected, "unexpected event sequence: {events:#?}");
    }

    #[tokio::test]
    async fn tool_result_persistence_event_carries_structured_details_by_id() {
        // Every tool call produces a `MessageEnd { ToolResult }`
        // event whose `details` field carries the structured
        // [`ToolDetails`] payload serialized to `Value`. A
        // downstream persistence listener relies on this so the
        // on-disk record can pin both the LLM-facing content (used
        // by the next inference) and the renderer payload (used by
        // resumed sessions to rehydrate diffs / todo snapshots /
        // bash exit codes / sub-agent reports without re-running
        // the tool).
        use crate::message::AgentMessageKind;
        let scripts = vec![
            finalize_script(finalize_tool_use("tu-only", "ping")),
            finalize_script(finalize_text("done")),
        ];
        let ping: ErasedToolDefinition = PingTool.into();
        let mut agent = build_agent(scripts, vec![ping]);
        agent.set_agent_id(AgentId::Sub(42));

        let captured: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            // Capture every `MessageEnd` carrying a `ToolResult`
            // so the test can assert on the rich payload.
            if let AgentEvent::MessageEnd { message, .. } = event {
                if matches!(
                    &message.kind,
                    AgentMessageKind::Wire(Message::ToolResult(_))
                ) {
                    captured_clone.lock().unwrap().push(event.clone());
                }
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
            "expected exactly one ToolResult MessageEnd event: {events:#?}"
        );

        let AgentEvent::MessageEnd { message, .. } = &events[0] else {
            panic!("captured non-MessageEnd event: {:#?}", events[0]);
        };
        let AgentMessageKind::Wire(Message::ToolResult(tr)) = &message.kind else {
            panic!("captured MessageEnd with non-ToolResult body: {message:#?}");
        };

        assert_eq!(tr.tool_call_id, "tu-only");
        assert_eq!(tr.tool_name, "ping");
        assert!(!tr.is_error);

        // The details field carries the structured `ToolDetails`
        // serialized to JSON; deserializing it back gets us the
        // original payload the tool returned.
        let details_value = tr
            .details
            .as_ref()
            .expect("details Value present on ToolResult");
        let payload: ToolDetails = serde_json::from_value(details_value.clone())
            .expect("details deserialize back to ToolDetails");
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
        let recorded_clone = Arc::clone(&recorded);
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
                EventLabel::Message {
                    agent_id: AgentId::Sub(7),
                    phase: "start",
                    kind: "User",
                },
                EventLabel::Message {
                    agent_id: AgentId::Sub(7),
                    phase: "end",
                    kind: "User",
                },
                EventLabel::TurnStart(AgentId::Sub(7)),
                EventLabel::Message {
                    agent_id: AgentId::Sub(7),
                    phase: "start",
                    kind: "Assistant",
                },
                EventLabel::MessageStream {
                    agent_id: AgentId::Sub(7),
                    event_kind: "start",
                },
                EventLabel::MessageStream {
                    agent_id: AgentId::Sub(7),
                    event_kind: "done",
                },
                EventLabel::Message {
                    agent_id: AgentId::Sub(7),
                    phase: "end",
                    kind: "Assistant",
                },
                EventLabel::TurnUsage(AgentId::Sub(7)),
                EventLabel::AgentEnd(AgentId::Sub(7)),
            ],
            "unexpected event sequence: {events:#?}"
        );
    }

    #[tokio::test]
    async fn prompt_emits_user_message_lifecycle_events() {
        // The top-level entry point appends the user prompt to the
        // transcript and emits a `MessageStart` / `MessageEnd` pair
        // for it before the assistant turn loop begins. This is the
        // contract the binary's persistence listener relies on to
        // write the user's typed input to disk.
        let scripts = vec![finalize_script(finalize_text("done"))];

        let mut agent = build_agent(scripts, Vec::new());

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = Arc::clone(&recorded);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        agent
            .prompt("hello agent".to_string(), CancellationToken::new())
            .await
            .expect("prompt");

        let events = recorded.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                EventLabel::AgentStart(AgentId::Main),
                EventLabel::Message {
                    agent_id: AgentId::Main,
                    phase: "start",
                    kind: "User",
                },
                EventLabel::Message {
                    agent_id: AgentId::Main,
                    phase: "end",
                    kind: "User",
                },
                EventLabel::TurnStart(AgentId::Main),
                EventLabel::Message {
                    agent_id: AgentId::Main,
                    phase: "start",
                    kind: "Assistant",
                },
                EventLabel::MessageStream {
                    agent_id: AgentId::Main,
                    event_kind: "start",
                },
                EventLabel::MessageStream {
                    agent_id: AgentId::Main,
                    event_kind: "done",
                },
                EventLabel::Message {
                    agent_id: AgentId::Main,
                    phase: "end",
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
        // transcript, without re-emitting a `MessageStart`/`MessageEnd`
        // for the user prompt (the prior `prompt` call already wrote
        // it). Here we seed a transcript ending in a user-role
        // message and verify `continue_run` drives one assistant
        // turn without firing any extra User message events.
        let scripts = vec![finalize_script(finalize_text("retried"))];

        // Seed a user-role last message — typically the prompt the
        // user already submitted before the previous turn errored
        // out.
        let mut agent = build_agent_with_transcript(
            scripts,
            Vec::new(),
            vec![AgentMessage::wire(Message::User(UserMessage::text(
                "retry me",
            )))],
        );

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = Arc::clone(&recorded);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        agent
            .continue_run(CancellationToken::new())
            .await
            .expect("continue_run");

        let events = recorded.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                EventLabel::AgentStart(AgentId::Main),
                // No User-message bracketing here — that's the
                // distinguishing feature of `continue_run` vs
                // `prompt`.
                EventLabel::TurnStart(AgentId::Main),
                EventLabel::Message {
                    agent_id: AgentId::Main,
                    phase: "start",
                    kind: "Assistant",
                },
                EventLabel::MessageStream {
                    agent_id: AgentId::Main,
                    event_kind: "start",
                },
                EventLabel::MessageStream {
                    agent_id: AgentId::Main,
                    event_kind: "done",
                },
                EventLabel::Message {
                    agent_id: AgentId::Main,
                    phase: "end",
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
        // user-role (or tool-result) message before inference.
        // `continue_run` enforces that precondition with a fatal
        // error rather than letting the model API surface an
        // obscure 4xx.
        // Seed an assistant-role last message.
        let mut agent = build_agent_with_transcript(
            Vec::new(),
            Vec::new(),
            vec![AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: "hi".into(),
                    text_signature: None,
                })],
                ..AssistantMessage::empty()
            }))],
        );

        let err = agent
            .continue_run(CancellationToken::new())
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
            .continue_run(CancellationToken::new())
            .await
            .expect_err("continue_run must reject empty transcript");
        assert!(
            matches!(err, crate::TurnError::Fatal(_)),
            "expected Fatal error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn prompt_returns_aborted_on_provider_side_cancel() {
        // The provider emits an `Error { reason: Aborted, ... }`
        // terminal (mirroring what the real providers do when their
        // own `select_cancel` arm fires). The agent should observe
        // the `ErrorCategory::Aborted` and return `TurnError::Aborted`
        // rather than the generic `TurnError::Recoverable` it uses
        // for other errors.
        //
        // This is the "provider won the cancel race" half of the
        // contract; the agent-side `select!` on cancel is covered
        // separately.
        let scripts = vec![aborted_script(finalize_text(""))];

        let mut agent = build_agent(scripts, Vec::new());
        let err = agent
            .prompt("hello".to_string(), CancellationToken::new())
            .await
            .expect_err("aborted-flavoured terminal should bubble up");
        assert!(
            matches!(err, crate::TurnError::Aborted),
            "expected TurnError::Aborted, got: {err:?}"
        );

        // Transcript invariant: the aborted assistant message is
        // pushed so resume sees the same shape the live session
        // did, even though `transform_messages` rule 5 drops it
        // before the next inference.
        let messages = agent.messages();
        assert!(matches!(
            messages.last().and_then(|m| m.as_wire()),
            Some(Message::Assistant(a)) if a.stop_reason == StopReason::Aborted
        ));
    }

    #[tokio::test]
    async fn prompt_returns_aborted_when_token_fired_before_call() {
        // Pre-cancelling the token before calling `prompt` should
        // short-circuit through the pre-iteration check in
        // `execute_turn` — no inference runs, no events emitted past
        // the lifecycle bracket, the call returns `Aborted`.
        //
        // We script one inference defensively; if the agent ever
        // actually polls the provider (regression: forgot to honour
        // the pre-iteration cancel check), the scripted provider's
        // strict mode would still let the test pass since the
        // script exists — so we additionally assert the recorded
        // event sequence never enters the `MessageStart Assistant`
        // phase.
        let scripts = vec![finalize_script(finalize_text("should not run"))];
        let mut agent = build_agent(scripts, Vec::new());

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = Arc::clone(&recorded);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        let token = CancellationToken::new();
        token.cancel();

        let err = agent
            .prompt("hello".to_string(), token)
            .await
            .expect_err("pre-cancelled token must abort the turn");
        assert!(
            matches!(err, crate::TurnError::Aborted),
            "expected TurnError::Aborted, got: {err:?}"
        );

        let events = recorded.lock().unwrap().clone();
        let saw_assistant_start = events.iter().any(|ev| {
            matches!(
                ev,
                EventLabel::Message {
                    phase: "start",
                    kind: "Assistant",
                    ..
                }
            )
        });
        assert!(
            !saw_assistant_start,
            "pre-cancelled prompt must not open an assistant slot, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn cancel_mid_stream_pushes_aborted_partial_and_allows_followup() {
        // End-to-end smoke test for the §1.8 cancellation invariant:
        // firing the token mid-stream should leave the transcript in
        // a shape that lets a second `prompt` call succeed without
        // any manual repair. We use a scripted provider whose first
        // step is an immediate `Start` and whose second step is
        // gated by a long delay, then cancel the token shortly after
        // launching the prompt. The follow-up turn uses a normal
        // scripted Done so we can verify the agent is still healthy.
        use aj_models::scripted::ProviderScript;

        let mut partial = AssistantMessage::empty();
        partial.api = SCRIPT_API.to_string();
        partial.provider = SCRIPT_PROVIDER.to_string();
        partial.model = SCRIPT_MODEL.to_string();

        let mut final_msg = partial.clone();
        final_msg.stop_reason = StopReason::Stop;

        let slow_script = ProviderScript::new()
            .push_immediate(AssistantMessageEvent::Start {
                partial: partial.clone(),
            })
            // Long enough that the cancel races in before this lands.
            .push(
                std::time::Duration::from_secs(60),
                AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: final_msg,
                },
            );
        let followup_script =
            ProviderScript::from_events(finalize_script(finalize_text("followup ok")));

        let provider: Arc<dyn Provider> = Arc::new(
            ScriptedProvider::new(vec![slow_script, followup_script])
                .on_exhausted(ExhaustedBehavior::Panic),
        );
        let model_info = Arc::new(scripted_model_info());
        let env = empty_env(std::env::temp_dir());
        let mut agent = Agent::with_provider(
            env,
            Vec::new(),
            Vec::new(),
            provider,
            model_info,
            StreamOptions::default(),
            None,
        );
        agent.seed_session(AgentSeed {
            assembled_system_prompt: Some("test system prompt".to_string()),
            ..AgentSeed::default()
        });

        let cancel = CancellationToken::new();
        let cancel_for_fire = cancel.clone();
        let fire_handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_for_fire.cancel();
        });

        let err = agent
            .prompt("first".to_string(), cancel)
            .await
            .expect_err("mid-stream cancel should abort the turn");
        fire_handle.await.expect("cancel firer joined");
        assert!(
            matches!(err, crate::TurnError::Aborted),
            "expected TurnError::Aborted, got: {err:?}"
        );

        // Transcript invariant: ends in an `Aborted`-flavoured
        // assistant message paired with the user prompt. No
        // dangling tool calls (the partial had none).
        let messages = agent.messages();
        let kinds: Vec<&'static str> = messages.iter().map(agent_message_kind_label).collect();
        assert_eq!(
            kinds,
            vec!["User", "Assistant"],
            "transcript should be [user, aborted-assistant] after cancel"
        );
        let last_assistant = match messages.last().and_then(|m| m.as_wire()) {
            Some(Message::Assistant(a)) => a,
            _ => panic!("expected trailing assistant message"),
        };
        assert_eq!(last_assistant.stop_reason, StopReason::Aborted);

        // Follow-up prompt with a fresh (un-fired) cancel token
        // succeeds — proves the aborted message didn't poison the
        // agent's state. `transform_messages` rule 5 drops the
        // aborted assistant before the next inference, so the
        // scripted provider sees only the new user message and
        // responds normally.
        agent
            .prompt("second".to_string(), CancellationToken::new())
            .await
            .expect("follow-up prompt should succeed");
    }

    #[tokio::test]
    async fn truncated_turn_is_retried_then_succeeds() {
        // A provider stream that drops before its terminal frame
        // surfaces as a transient `Error` (docs/models-spec.md §10.3,
        // `AssistantMessageEvent::truncated`). The agent's retry layer
        // must re-issue the turn rather than accept the truncated turn
        // as final. Strict-mode provider: exactly two inferences are
        // scripted (the truncation, then the recovery), so a missing
        // retry would surface `Recoverable` and a spurious extra
        // inference would panic.
        let scripts = vec![
            transient_error_script(),
            finalize_script(finalize_text("recovered")),
        ];
        let mut agent = build_agent(scripts, Vec::new());

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = Arc::clone(&recorded);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        let final_text = agent
            .run_single_turn("hello".to_string())
            .await
            .expect("transient truncation should be retried into a successful turn");
        assert_eq!(final_text, "recovered");

        // Exactly one StreamRetry was emitted for the truncated attempt.
        let retries: Vec<u32> = recorded
            .lock()
            .unwrap()
            .iter()
            .filter_map(|l| match l {
                EventLabel::StreamRetry(_, attempt) => Some(*attempt),
                _ => None,
            })
            .collect();
        assert_eq!(retries, vec![1], "expected exactly one stream retry");

        // Only the recovered turn — not the truncated one — landed on
        // the transcript.
        let messages = agent.messages();
        let last_assistant = match messages.last().and_then(|m| m.as_wire()) {
            Some(Message::Assistant(a)) => a,
            _ => panic!("expected trailing assistant message"),
        };
        assert_eq!(last_assistant.stop_reason, StopReason::Stop);
    }

    #[test]
    fn scan_dangling_tool_uses_finds_unmatched_ids() {
        // Sanity check on the test-only helper: a transcript
        // ending in an assistant tool_call without a matching
        // tool_result reports the dangling id.
        use aj_models::types::ToolResultMessage;

        let transcript = vec![
            AgentMessage::wire(Message::User(UserMessage::text("hi"))),
            AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: "tu-1".to_string(),
                    name: "ping".to_string(),
                    arguments: serde_json::json!({}),
                })],
                ..AssistantMessage::empty()
            })),
        ];
        let dangling = super::scan_dangling_tool_uses(&transcript);
        assert!(dangling.contains("tu-1"));

        let mut transcript_with_resolution = transcript.clone();
        transcript_with_resolution.push(AgentMessage::wire(Message::ToolResult(
            ToolResultMessage::text("tu-1", "ping", "done", false),
        )));
        assert!(super::scan_dangling_tool_uses(&transcript_with_resolution).is_empty());
    }

    #[tokio::test]
    async fn before_tool_call_hook_can_mutate_tool_input() {
        // The hook flips the tool's input from `{}` to `{"flag": true}`;
        // a tool that records its input in its outcome body proves the
        // mutated args reached `execute`.
        use std::sync::Arc;

        use crate::hooks::{BeforeToolCallHook, BeforeToolCallOutcome, ToolCallContext};

        #[derive(Clone)]
        struct EchoTool;

        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct EchoInput {
            #[serde(default)]
            flag: bool,
        }

        impl ToolDefinition for EchoTool {
            type Input = EchoInput;
            fn name(&self) -> &'static str {
                "echo"
            }
            fn description(&self) -> &'static str {
                "Test tool that echoes its flag input"
            }
            async fn execute(
                &self,
                _ctx: &mut dyn ToolContext,
                input: EchoInput,
            ) -> anyhow::Result<ToolOutcome> {
                Ok(ToolOutcome {
                    content: vec![aj_models::types::UserContent::text(format!(
                        "flag={}",
                        input.flag
                    ))],
                    details: ToolDetails::Text {
                        summary: "echo".to_string(),
                        body: format!("flag={}", input.flag),
                    },
                    is_error: false,
                })
            }
        }

        let scripts = vec![
            finalize_script(finalize_tool_use("tu-1", "echo")),
            finalize_script(finalize_text("done")),
        ];
        let mut agent = build_agent(scripts, vec![EchoTool.into()]);

        let hook: BeforeToolCallHook = Arc::new(|_ctx: ToolCallContext, _args| {
            Box::pin(async move {
                BeforeToolCallOutcome::Proceed {
                    args: serde_json::json!({ "flag": true }),
                }
            })
        });
        agent.set_before_tool_call(Some(hook));

        // Capture every `ToolExecutionEnd` so we can assert the
        // mutated args reached the tool's execute body.
        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let bodies_clone = Arc::clone(&bodies);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            if let AgentEvent::ToolExecutionEnd {
                result: ToolDetails::Text { body, .. },
                ..
            } = event
            {
                bodies_clone.lock().unwrap().push(body.clone());
            }
        }));

        agent
            .run_single_turn("run echo".to_string())
            .await
            .expect("run_single_turn");

        let recorded = bodies.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec!["flag=true"],
            "before-tool hook should have flipped the flag on the tool's input",
        );
    }

    #[tokio::test]
    async fn before_tool_call_hook_can_short_circuit() {
        // The hook returns `ShortCircuit` so the tool's `execute`
        // never runs; the synthesized outcome flows through the rest
        // of the loop normally.
        use std::sync::Arc;

        use crate::hooks::{BeforeToolCallHook, BeforeToolCallOutcome, ToolCallContext};

        #[derive(Clone)]
        struct ShouldNotRunTool;

        #[derive(serde::Deserialize, schemars::JsonSchema)]
        struct EmptyInput {}

        impl ToolDefinition for ShouldNotRunTool {
            type Input = EmptyInput;
            fn name(&self) -> &'static str {
                "denied"
            }
            fn description(&self) -> &'static str {
                "Tool the before-hook is expected to deny"
            }
            async fn execute(
                &self,
                _ctx: &mut dyn ToolContext,
                _input: EmptyInput,
            ) -> anyhow::Result<ToolOutcome> {
                panic!("execute should not run when the before-hook short-circuits");
            }
        }

        let scripts = vec![
            finalize_script(finalize_tool_use("tu-1", "denied")),
            finalize_script(finalize_text("done")),
        ];
        let mut agent = build_agent(scripts, vec![ShouldNotRunTool.into()]);

        let hook: BeforeToolCallHook = Arc::new(|_ctx: ToolCallContext, _args| {
            Box::pin(async move {
                BeforeToolCallOutcome::ShortCircuit {
                    outcome: ToolOutcome {
                        content: vec![aj_models::types::UserContent::text("blocked".to_string())],
                        details: ToolDetails::Text {
                            summary: "denied: blocked by policy".to_string(),
                            body: "blocked".to_string(),
                        },
                        is_error: true,
                    },
                }
            })
        });
        agent.set_before_tool_call(Some(hook));

        let outcomes: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let outcomes_clone = Arc::clone(&outcomes);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            if let AgentEvent::ToolExecutionEnd {
                result: ToolDetails::Text { summary, .. },
                is_error,
                ..
            } = event
            {
                outcomes_clone
                    .lock()
                    .unwrap()
                    .push((summary.clone(), *is_error));
            }
        }));

        agent
            .run_single_turn("attempt denied".to_string())
            .await
            .expect("run_single_turn");

        let recorded = outcomes.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![("denied: blocked by policy".to_string(), true)]
        );
    }

    #[tokio::test]
    async fn after_tool_call_hook_can_rewrite_outcome() {
        // The hook flips `is_error` from `false` to `true` and
        // rewrites the body — the ping tool would have returned a
        // success outcome, but the after-hook overrides it.
        use std::sync::Arc;

        use crate::hooks::{AfterToolCallHook, ToolCallContext};

        let scripts = vec![
            finalize_script(finalize_tool_use("tu-1", "ping")),
            finalize_script(finalize_text("done")),
        ];
        let mut agent = build_agent(scripts, vec![PingTool.into()]);

        let hook: AfterToolCallHook = Arc::new(|_ctx: ToolCallContext, outcome| {
            Box::pin(async move {
                outcome.is_error = true;
                if let ToolDetails::Text { body, .. } = &mut outcome.details {
                    *body = "[redacted]".to_string();
                }
            })
        });
        agent.set_after_tool_call(Some(hook));

        let outcomes: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
        let outcomes_clone = Arc::clone(&outcomes);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            if let AgentEvent::ToolExecutionEnd {
                result: ToolDetails::Text { body, .. },
                is_error,
                ..
            } = event
            {
                outcomes_clone
                    .lock()
                    .unwrap()
                    .push((body.clone(), *is_error));
            }
        }));

        agent
            .run_single_turn("run ping".to_string())
            .await
            .expect("run_single_turn");

        let recorded = outcomes.lock().unwrap().clone();
        assert_eq!(recorded, vec![("[redacted]".to_string(), true)]);
    }

    #[tokio::test]
    async fn should_stop_after_turn_hook_ends_turn_before_next_inference() {
        // The hook returns `true` so the agent breaks out of the
        // turn loop after the first tool batch — no second
        // inference is run, and the strict-mode scripted provider
        // would panic if a second inference were attempted.
        use std::sync::Arc;

        use crate::hooks::ShouldStopAfterTurnHook;

        // Only one script: the tool_use. If the hook fails to
        // short-circuit, the loop would call `stream_simple` a
        // second time and the strict-mode provider panics.
        let scripts = vec![finalize_script(finalize_tool_use("tu-1", "ping"))];
        let mut agent = build_agent(scripts, vec![PingTool.into()]);

        let hook: ShouldStopAfterTurnHook = Arc::new(|| Box::pin(async { true }));
        agent.set_should_stop_after_turn(Some(hook));

        // Sanity check: the run completes without panicking.
        agent
            .run_single_turn("run ping".to_string())
            .await
            .expect("run_single_turn");
    }

    /// Minimal stand-in for the production `agent` builtin: forwards
    /// the task to [`ToolContext::spawn_agent`] with a configured
    /// [`SpawnMode`] and reports the result. Lets the retention and
    /// background-spawn tests exercise the real `spawn_agent` path
    /// without depending on `aj-tools`.
    #[derive(Clone)]
    struct SpawnTool {
        mode: crate::tool::SpawnMode,
        /// `(agent_id, task_id)` pairs recorded off
        /// [`crate::tool::SpawnResult::Started`] results.
        started: Arc<Mutex<Vec<(usize, usize)>>>,
    }

    impl SpawnTool {
        fn blocking() -> Self {
            Self {
                mode: crate::tool::SpawnMode::Blocking,
                started: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn background() -> (Self, Arc<Mutex<Vec<(usize, usize)>>>) {
            let started = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    mode: crate::tool::SpawnMode::Background,
                    started: Arc::clone(&started),
                },
                started,
            )
        }
    }

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct SpawnInput {
        // The scripted tool-use carries empty arguments, so default the
        // task; the spawned sub-agent's prompt content is irrelevant to
        // retention.
        #[serde(default)]
        task: String,
    }

    impl ToolDefinition for SpawnTool {
        type Input = SpawnInput;

        fn name(&self) -> &'static str {
            "agent"
        }

        fn description(&self) -> &'static str {
            "Spawn a sub-agent"
        }

        async fn execute(
            &self,
            ctx: &mut dyn ToolContext,
            input: SpawnInput,
        ) -> anyhow::Result<ToolOutcome> {
            match ctx.spawn_agent(input.task, self.mode).await? {
                crate::tool::SpawnResult::Completed(spawned) => Ok(ToolOutcome {
                    content: vec![aj_models::types::UserContent::text(spawned.report.clone())],
                    details: ToolDetails::Text {
                        summary: format!("sub-agent {}", spawned.agent_id),
                        body: spawned.report,
                    },
                    is_error: false,
                }),
                crate::tool::SpawnResult::Started { agent_id, task_id } => {
                    self.started.lock().unwrap().push((agent_id, task_id));
                    let text = format!("agent {agent_id} started in background (task #{task_id})");
                    Ok(ToolOutcome {
                        content: vec![aj_models::types::UserContent::text(text.clone())],
                        details: ToolDetails::Text {
                            summary: text,
                            body: String::new(),
                        },
                        is_error: false,
                    })
                }
            }
        }
    }

    /// After a spawn, the parent's injected registry retains the
    /// sub-agent and the retained handle's transcript ends on the
    /// assistant report — i.e. the sub-agent is live and re-promptable,
    /// not dropped when `spawn_agent` returns.
    #[tokio::test]
    async fn spawn_agent_retains_sub_agent_in_registry() {
        use crate::message::{AgentMessage, AgentMessageKind};
        use crate::SubAgentRegistry;

        // The parent and the sub-agent share one `ScriptedProvider`,
        // so scripts are consumed in run order across both:
        //   0. parent emits the `agent` tool call,
        //   1. the sub-agent's single-turn text report,
        //   2. the parent's final text after the tool result.
        let scripts = vec![
            finalize_script(finalize_tool_use("tu-1", "agent")),
            finalize_script(finalize_text("sub report")),
            finalize_script(finalize_text("parent done")),
        ];

        let mut agent = build_agent(scripts, vec![SpawnTool::blocking().into()]);
        let registry = SubAgentRegistry::default();
        agent.set_sub_agent_registry(registry.clone());

        agent
            .run_single_turn("delegate work".to_string())
            .await
            .expect("run_single_turn");

        // The spawn allocated `Sub(1)`; its handle must outlive the
        // tool call and stay in the shared registry.
        assert_eq!(registry.ids(), vec![1]);
        let sub = registry.get(1).expect("sub-agent retained under id 1");

        let guard = sub.lock().await;
        let last = guard
            .messages()
            .last()
            .expect("sub-agent transcript is non-empty");
        assert!(
            matches!(
                last,
                AgentMessage {
                    kind: AgentMessageKind::Wire(Message::Assistant(_)),
                    ..
                }
            ),
            "sub-agent transcript should end on the assistant report, got {last:?}"
        );
    }

    /// A sub-agent retained in the registry is live and re-promptable:
    /// re-prompting its handle directly (the capability the binary
    /// exercises for steering) appends the new user message and the
    /// continuation assistant message onto its existing transcript.
    #[tokio::test]
    async fn re_prompting_retained_sub_agent_extends_its_transcript() {
        use crate::SubAgentRegistry;

        // One shared `ScriptedProvider` serves scripts in run order
        // across the parent and the sub-agent:
        //   0. parent emits the `agent` tool call,
        //   1. the sub-agent's initial single-turn report,
        //   2. the parent's final text after the tool result,
        //   3. the sub-agent's continuation reply to the re-prompt.
        let scripts = vec![
            finalize_script(finalize_tool_use("tu-1", "agent")),
            finalize_script(finalize_text("sub report")),
            finalize_script(finalize_text("parent done")),
            finalize_script(finalize_text("continuation")),
        ];

        let mut agent = build_agent(scripts, vec![SpawnTool::blocking().into()]);
        let registry = SubAgentRegistry::default();
        agent.set_sub_agent_registry(registry.clone());

        agent
            .run_single_turn("delegate work".to_string())
            .await
            .expect("run_single_turn");

        let sub = registry.get(1).expect("sub-agent retained under id 1");

        // Transcript length right after the initial spawn, so we can
        // assert the re-prompt grows it by exactly two messages.
        let len_after_spawn = {
            let guard = sub.lock().await;
            guard.messages().len()
        };

        // Re-prompt the retained handle directly.
        {
            let mut guard = sub.lock().await;
            guard
                .prompt("follow up".to_string(), CancellationToken::new())
                .await
                .expect("re-prompt");
        }

        let guard = sub.lock().await;
        let messages = guard.messages();

        assert_eq!(
            messages.len(),
            len_after_spawn + 2,
            "re-prompt should append the user message and the continuation reply"
        );

        // The transcript ends on the continuation assistant text.
        let last_text: String = match messages.last().and_then(|m| m.as_wire()) {
            Some(Message::Assistant(a)) => a
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect(),
            other => panic!("expected trailing assistant message, got {other:?}"),
        };
        assert_eq!(last_text, "continuation");

        // The re-prompt's user message appears in the transcript.
        let has_follow_up = messages.iter().any(|m| match m.as_wire() {
            Some(Message::User(u)) => u.content.iter().any(|c| match c {
                aj_models::types::UserContent::Text(t) => t.text == "follow up",
                _ => false,
            }),
            _ => false,
        });
        assert!(
            has_follow_up,
            "transcript should contain the re-prompt user message"
        );
    }

    // ---- Background agent spawns ------------------------------------------

    /// Poll `cond` until it holds (bounded), yielding so detached
    /// drivers make progress.
    async fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// A background spawn returns a `Started` result with both ids,
    /// retains the handle like a blocking spawn, still emits
    /// `SubAgentEnd`, delivers the child's report as a completion
    /// notice, and folds the child's usage into the parent's session
    /// state when the notice drains.
    #[tokio::test]
    async fn background_spawn_delivers_report_notice_and_folds_usage() {
        use crate::SubAgentRegistry;

        // Script consumption is deterministic: the stop-after-turn
        // hook ends the parent's turn right after the tool batch, so
        // only the detached child consumes script 1, and the wake
        // afterwards consumes script 2.
        let mut sub_report = finalize_text("sub report");
        sub_report.usage.input = 7;
        sub_report.usage.output = 3;
        sub_report.usage.total_tokens = 10;
        let scripts = vec![
            finalize_script(finalize_tool_use("tu-1", "agent")),
            finalize_script(sub_report),
            finalize_script(finalize_text("reacted")),
        ];

        let (tool, started) = SpawnTool::background();
        let mut agent = build_agent(scripts, vec![tool.into()]);
        let sub_registry = SubAgentRegistry::default();
        agent.set_sub_agent_registry(sub_registry.clone());
        let tasks = TaskRegistry::default();
        agent.set_task_registry(tasks.clone());
        let stop_hook: crate::hooks::ShouldStopAfterTurnHook =
            Arc::new(|| Box::pin(async { true }));
        agent.set_should_stop_after_turn(Some(stop_hook));

        let sub_agent_ends: Arc<Mutex<Vec<(AgentId, AgentId, String)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let sub_agent_ends_clone = Arc::clone(&sub_agent_ends);
        // The binary's submit/wake gating keys off the `Agent*`
        // bracketing of the child's run; record it so the test pins
        // that contract for background initial runs.
        let sub_lifecycle: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let sub_lifecycle_clone = Arc::clone(&sub_lifecycle);
        let _handle = agent.subscribe(listener_from_sync(move |event| match event {
            AgentEvent::SubAgentEnd {
                parent,
                child,
                report,
            } => {
                sub_agent_ends_clone
                    .lock()
                    .unwrap()
                    .push((*parent, *child, report.clone()));
            }
            AgentEvent::AgentStart { agent_id, .. } if *agent_id == AgentId::Sub(1) => {
                sub_lifecycle_clone.lock().unwrap().push("start");
            }
            AgentEvent::AgentEnd { agent_id, .. } if *agent_id == AgentId::Sub(1) => {
                sub_lifecycle_clone.lock().unwrap().push("end");
            }
            _ => {}
        }));

        agent
            .prompt("delegate".to_string(), CancellationToken::new())
            .await
            .expect("prompt");

        // The tool observed the Started result with both ids.
        assert_eq!(*started.lock().unwrap(), vec![(1, 1)]);
        // The handle is retained exactly like a blocking spawn's.
        assert_eq!(sub_registry.ids(), vec![1]);

        // The detached driver outlives the parent's turn; wait for
        // the completion notice it queues.
        wait_until(|| tasks.has_notices(AgentId::Main), "completion notice").await;
        assert_eq!(tasks.status(1), Some(TaskStatus::Exited(Some(0))));
        assert_eq!(
            *sub_agent_ends.lock().unwrap(),
            vec![(AgentId::Main, AgentId::Sub(1), "sub report".to_string())]
        );
        // The detached run bracketed itself with `AgentStart(Sub 1)` /
        // `AgentEnd(Sub 1)` on the shared bus — while it ran, the
        // pump's running set covered it, so user submits to the
        // sub-agent stayed gated exactly like a blocking spawn's.
        assert_eq!(*sub_lifecycle.lock().unwrap(), vec!["start", "end"]);
        // The terminal read surfaces the report — what task_output
        // renders for finished agent tasks.
        let (_, read) = tasks.read(1).expect("task readable");
        assert_eq!(read.report.as_deref(), Some("sub report"));
        // Usage stays parked on the registry until the drain.
        assert!(agent.sub_agent_usage().is_empty());

        // Wake drains the notice into the transcript and folds usage.
        let outcome = agent
            .wake(CancellationToken::new())
            .await
            .expect("wake runs");
        assert_eq!(outcome, crate::WakeOutcome::Ran);
        let usage = agent
            .sub_agent_usage()
            .get(&1)
            .expect("usage folded at drain");
        assert_eq!((usage.input, usage.output, usage.total_tokens), (7, 3, 10));

        let notice_text = agent
            .messages()
            .iter()
            .filter_map(user_text)
            .find(|t| t.starts_with("<task-notification>"))
            .expect("notice injected into transcript");
        assert_eq!(
            notice_text,
            "<task-notification>\nBackground task #1 finished: agent 1 — sub report\
             \n</task-notification>"
        );
    }

    /// Killing an agent-backed task cancels the child's run through
    /// the task token: the run aborts, the task ends `Killed` with a
    /// kill notice, and the sub-agent handle stays retained (it's
    /// re-promptable per the steering contract).
    #[tokio::test]
    async fn killing_background_agent_task_cancels_child_run() {
        use crate::SubAgentRegistry;

        // The child's script stalls before its terminal event so the
        // kill lands mid-inference; on cancel the scripted provider
        // emits an aborted terminal and the run errors out.
        let scripts = vec![
            ProviderScript::from_events(finalize_script(finalize_tool_use("tu-1", "agent"))),
            ProviderScript::new().push(
                std::time::Duration::from_secs(30),
                AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: finalize_text("never delivered"),
                },
            ),
        ];

        let (tool, started) = SpawnTool::background();
        let mut agent = build_agent_scripts(scripts, vec![tool.into()]);
        let sub_registry = SubAgentRegistry::default();
        agent.set_sub_agent_registry(sub_registry.clone());
        let tasks = TaskRegistry::default();
        agent.set_task_registry(tasks.clone());
        let stop_hook: crate::hooks::ShouldStopAfterTurnHook =
            Arc::new(|| Box::pin(async { true }));
        agent.set_should_stop_after_turn(Some(stop_hook));

        agent
            .prompt("delegate".to_string(), CancellationToken::new())
            .await
            .expect("prompt");
        assert_eq!(*started.lock().unwrap(), vec![(1, 1)]);

        // Kill through the registry — the same path task_stop and the
        // TUI's kill action take.
        assert!(tasks.kill(1));
        let status =
            tokio::time::timeout(std::time::Duration::from_secs(10), tasks.wait_terminal(1))
                .await
                .expect("driver reacts to the kill promptly");
        assert_eq!(status, Some(TaskStatus::Killed));
        // A killed run stores no report — the status line already
        // says everything a terminal read needs.
        let (_, read) = tasks.read(1).expect("task readable");
        assert_eq!(read.report, None);

        let notices = tasks.drain_notices(AgentId::Main);
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].body,
            "Background task #1 finished: agent 1 — killed"
        );
        // The kill stops the run, not the agent: the handle stays in
        // the registry.
        assert!(sub_registry.get(1).is_some());
    }

    // ---- Task-notice drain points ----------------------------------------

    /// Extract the concatenated text of a user-role [`AgentMessage`],
    /// `None` for other roles.
    fn user_text(message: &AgentMessage) -> Option<String> {
        match message.as_wire() {
            Some(Message::User(u)) => Some(
                u.content
                    .iter()
                    .filter_map(|c| match c {
                        aj_models::types::UserContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    fn bash_notice(owner: AgentId, task_id: usize, body: &str) -> TaskNotice {
        TaskNotice {
            owner,
            task_id,
            kind: TaskKind::Bash {
                command: "cargo build".to_string(),
            },
            label: "cargo build".to_string(),
            status: TaskStatus::Exited(Some(0)),
            body: body.to_string(),
        }
    }

    /// Idle drain point: a notice queued while the agent is idle is
    /// injected as a `<task-notification>` user message *before* the
    /// user's new prompt, with a persistence-shaped `MessageStart` /
    /// `MessageEnd` pair like any other message.
    #[tokio::test]
    async fn queued_notice_drains_before_prompt_user_message() {
        let scripts = vec![finalize_script(finalize_text("ok"))];
        let mut agent = build_agent(scripts, Vec::new());
        let registry = TaskRegistry::default();
        agent.set_task_registry(registry.clone());
        registry.push_notice(bash_notice(
            AgentId::Main,
            3,
            "Background task #3 finished: cargo build — exit code 0",
        ));

        let starts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ends: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let starts_clone = Arc::clone(&starts);
        let ends_clone = Arc::clone(&ends);
        let _handle = agent.subscribe(listener_from_sync(move |event| match event {
            AgentEvent::MessageStart { message, .. } => {
                if let Some(text) = user_text(message) {
                    starts_clone.lock().unwrap().push(text);
                }
            }
            AgentEvent::MessageEnd { message, .. } => {
                if let Some(text) = user_text(message) {
                    ends_clone.lock().unwrap().push(text);
                }
            }
            _ => {}
        }));

        agent
            .prompt("hello".to_string(), CancellationToken::new())
            .await
            .expect("prompt");

        let expected = vec![
            "<task-notification>\nBackground task #3 finished: cargo build — exit code 0\
             \n</task-notification>"
                .to_string(),
            "hello".to_string(),
        ];
        assert_eq!(*ends.lock().unwrap(), expected);
        // Every drained notice gets the full MessageStart/MessageEnd
        // bracket — the same shape the persistence listener consumes.
        assert_eq!(*starts.lock().unwrap(), expected);

        // The queue is consumed and the notice is on the transcript.
        assert!(!registry.has_notices(AgentId::Main));
        let first = user_text(&agent.messages()[0]).expect("first transcript message is user");
        assert!(first.starts_with("<task-notification>"));
    }

    /// Tool that queues a completion notice for `Main` through the
    /// shared registry, standing in for a background driver finishing
    /// while a tool batch runs.
    #[derive(Clone)]
    struct FinishTaskTool;

    impl ToolDefinition for FinishTaskTool {
        type Input = PingInput;

        fn name(&self) -> &'static str {
            "finish_task"
        }

        fn description(&self) -> &'static str {
            "Test tool that queues a task notice"
        }

        async fn execute(
            &self,
            ctx: &mut dyn ToolContext,
            _input: PingInput,
        ) -> anyhow::Result<ToolOutcome> {
            ctx.task_registry().push_notice(TaskNotice {
                owner: AgentId::Main,
                task_id: 1,
                kind: TaskKind::Bash {
                    command: "sleep 1".to_string(),
                },
                label: "sleep 1".to_string(),
                status: TaskStatus::Exited(Some(0)),
                body: "Background task #1 finished: sleep 1 — exit code 0".to_string(),
            });
            Ok(ToolOutcome {
                content: vec![aj_models::types::UserContent::text("queued".to_string())],
                details: ToolDetails::Text {
                    summary: "finish_task".to_string(),
                    body: "queued".to_string(),
                },
                is_error: false,
            })
        }
    }

    /// Mid-run drain point: a notice queued while a tool batch runs is
    /// injected after the batch's tool results and before the next
    /// inference step.
    #[tokio::test]
    async fn notice_queued_mid_run_drains_after_tool_batch() {
        let scripts = vec![
            finalize_script(finalize_tool_use("tu-1", "finish_task")),
            finalize_script(finalize_text("done")),
        ];
        let mut agent = build_agent(scripts, vec![FinishTaskTool.into()]);
        let registry = TaskRegistry::default();
        agent.set_task_registry(registry.clone());

        let captured: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            captured_clone.lock().unwrap().push(event.clone());
        }));

        agent
            .prompt("go".to_string(), CancellationToken::new())
            .await
            .expect("prompt");

        let events = captured.lock().unwrap().clone();
        let tool_end_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
            .expect("tool batch finished");
        let notice_end_idx = events
            .iter()
            .position(|e| match e {
                AgentEvent::MessageEnd { message, .. } => {
                    user_text(message).is_some_and(|t| t.starts_with("<task-notification>"))
                }
                _ => false,
            })
            .expect("notice user message emitted");
        let second_inference_idx = events
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(
                    e,
                    AgentEvent::MessageStart { message, .. }
                        if matches!(message.as_wire(), Some(Message::Assistant(_)))
                )
            })
            .map(|(i, _)| i)
            .nth(1)
            .expect("second inference started");

        assert!(
            tool_end_idx < notice_end_idx && notice_end_idx < second_inference_idx,
            "notice must land after the tool batch and before the next inference \
             (tool_end={tool_end_idx}, notice={notice_end_idx}, \
             inference={second_inference_idx})"
        );

        // Body shape: the pre-rendered body verbatim inside the tag,
        // with the tag delimiters on their own lines.
        let AgentEvent::MessageEnd { message, .. } = &events[notice_end_idx] else {
            unreachable!()
        };
        assert_eq!(
            user_text(message).unwrap(),
            "<task-notification>\nBackground task #1 finished: sleep 1 — exit code 0\
             \n</task-notification>"
        );
        // The matching MessageStart fired right before the end.
        assert!(matches!(
            &events[notice_end_idx - 1],
            AgentEvent::MessageStart { message, .. }
                if user_text(message).is_some_and(|t| t.starts_with("<task-notification>"))
        ));
        assert!(!registry.has_notices(AgentId::Main));
    }

    /// Sub-agent runs share the prompt-top drain: notices owned by the
    /// sub-agent land at the start of its run, while other owners'
    /// queues are untouched.
    #[tokio::test]
    async fn sub_agent_drains_its_own_notices_at_run_start() {
        let scripts = vec![finalize_script(finalize_text("sub done"))];
        let mut agent = build_agent(scripts, Vec::new());
        agent.set_agent_id(AgentId::Sub(2));
        let registry = TaskRegistry::default();
        agent.set_task_registry(registry.clone());

        registry.push_notice(bash_notice(AgentId::Sub(2), 5, "task #5 done"));
        registry.push_notice(bash_notice(AgentId::Main, 6, "task #6 done"));

        let ends: Arc<Mutex<Vec<(AgentId, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let ends_clone = Arc::clone(&ends);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            if let AgentEvent::MessageEnd { agent_id, message } = event {
                if let Some(text) = user_text(message) {
                    ends_clone.lock().unwrap().push((*agent_id, text));
                }
            }
        }));

        agent
            .run_single_turn("work".to_string())
            .await
            .expect("run_single_turn");

        assert_eq!(
            *ends.lock().unwrap(),
            vec![
                (
                    AgentId::Sub(2),
                    "<task-notification>\ntask #5 done\n</task-notification>".to_string()
                ),
                (AgentId::Sub(2), "work".to_string()),
            ]
        );
        // The other owner's queue is untouched.
        assert!(registry.has_notices(AgentId::Main));
        assert!(!registry.has_notices(AgentId::Sub(2)));
    }

    // ---- Agent::wake ------------------------------------------------------

    /// A wake drains the queued notices and runs a full turn with the
    /// same event bracketing every top-level run gets: AgentStart,
    /// the notice's user-message pair, TurnStart, the assistant
    /// stream, TurnUsage, AgentEnd.
    #[tokio::test]
    async fn wake_drains_notices_and_runs_bracketed_turn() {
        let scripts = vec![finalize_script(finalize_text("reacted"))];
        let mut agent = build_agent(scripts, Vec::new());
        let registry = TaskRegistry::default();
        agent.set_task_registry(registry.clone());
        registry.push_notice(bash_notice(
            AgentId::Main,
            2,
            "Background task #2 finished: cargo build — exit code 0",
        ));

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = Arc::clone(&recorded);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        let outcome = agent
            .wake(CancellationToken::new())
            .await
            .expect("wake runs the turn");
        assert_eq!(outcome, crate::WakeOutcome::Ran);

        let events = recorded.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                EventLabel::AgentStart(AgentId::Main),
                EventLabel::Message {
                    agent_id: AgentId::Main,
                    phase: "start",
                    kind: "User",
                },
                EventLabel::Message {
                    agent_id: AgentId::Main,
                    phase: "end",
                    kind: "User",
                },
                EventLabel::TurnStart(AgentId::Main),
                EventLabel::Message {
                    agent_id: AgentId::Main,
                    phase: "start",
                    kind: "Assistant",
                },
                EventLabel::MessageStream {
                    agent_id: AgentId::Main,
                    event_kind: "start",
                },
                EventLabel::MessageStream {
                    agent_id: AgentId::Main,
                    event_kind: "done",
                },
                EventLabel::Message {
                    agent_id: AgentId::Main,
                    phase: "end",
                    kind: "Assistant",
                },
                EventLabel::TurnUsage(AgentId::Main),
                EventLabel::AgentEnd(AgentId::Main),
            ],
            "unexpected event sequence: {events:#?}"
        );

        // The drained notice is the turn's user-role message.
        let first = user_text(&agent.messages()[0]).expect("first message is the notice");
        assert!(first.starts_with("<task-notification>\nBackground task #2"));
        assert!(!registry.has_notices(AgentId::Main));
    }

    /// An empty queue makes wake a pure no-op: `Empty` outcome, no
    /// events, no inference (the strict-mode provider would panic on
    /// an unscripted inference).
    #[tokio::test]
    async fn wake_on_empty_queue_returns_empty_without_events() {
        let mut agent = build_agent(Vec::new(), Vec::new());
        let registry = TaskRegistry::default();
        agent.set_task_registry(registry.clone());

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = Arc::clone(&recorded);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        let outcome = agent
            .wake(CancellationToken::new())
            .await
            .expect("empty wake succeeds");
        assert_eq!(outcome, crate::WakeOutcome::Empty);
        assert!(
            recorded.lock().unwrap().is_empty(),
            "empty wake must not emit events: {:#?}",
            recorded.lock().unwrap()
        );
        assert!(agent.messages().is_empty());
    }

    // ---- steering / follow-up queues --------------------------------------

    /// Tool that enqueues a steering message for `Main` through the
    /// shared [`MessageQueues`] while it runs, standing in for the user
    /// pressing Alt+Enter during a tool batch.
    #[derive(Clone)]
    struct SteerTool {
        queues: MessageQueues,
    }

    impl ToolDefinition for SteerTool {
        type Input = PingInput;

        fn name(&self) -> &'static str {
            "steer_tool"
        }

        fn description(&self) -> &'static str {
            "Test tool that enqueues a steering message"
        }

        async fn execute(
            &self,
            _ctx: &mut dyn ToolContext,
            _input: PingInput,
        ) -> anyhow::Result<ToolOutcome> {
            self.queues.append_steering(AgentId::Main, "steer now");
            Ok(ToolOutcome {
                content: vec![aj_models::types::UserContent::text("ok".to_string())],
                details: ToolDetails::Text {
                    summary: "steer_tool".to_string(),
                    body: "ok".to_string(),
                },
                is_error: false,
            })
        }
    }

    /// In-loop steering drain: a message queued while a tool batch runs
    /// is injected as a user message after the batch's tool results and
    /// before the next inference, and a `QueueUpdate` announces the
    /// now-empty queue.
    #[tokio::test]
    async fn steering_queued_mid_batch_drains_after_tool_batch() {
        let queues = MessageQueues::default();
        let scripts = vec![
            finalize_script(finalize_tool_use("tu-1", "steer_tool")),
            finalize_script(finalize_text("done")),
        ];
        let mut agent = build_agent(
            scripts,
            vec![SteerTool {
                queues: queues.clone(),
            }
            .into()],
        );
        agent.set_message_queues(queues.clone());

        let captured: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            captured_clone.lock().unwrap().push(event.clone());
        }));

        agent
            .prompt("go".to_string(), CancellationToken::new())
            .await
            .expect("prompt");

        let events = captured.lock().unwrap().clone();
        let tool_end_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
            .expect("tool batch finished");
        let steer_end_idx = events
            .iter()
            .position(|e| match e {
                AgentEvent::MessageEnd { message, .. } => {
                    user_text(message).is_some_and(|t| t == "steer now")
                }
                _ => false,
            })
            .expect("steering user message emitted");
        let second_inference_idx = events
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(
                    e,
                    AgentEvent::MessageStart { message, .. }
                        if matches!(message.as_wire(), Some(Message::Assistant(_)))
                )
            })
            .map(|(i, _)| i)
            .nth(1)
            .expect("second inference started");
        assert!(
            tool_end_idx < steer_end_idx && steer_end_idx < second_inference_idx,
            "steering must land after the tool batch and before the next inference \
             (tool_end={tool_end_idx}, steer={steer_end_idx}, \
             inference={second_inference_idx})"
        );

        // A QueueUpdate announcing the drained (now-empty) queue follows
        // the injected message.
        let queue_update_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::QueueUpdate { .. }))
            .expect("QueueUpdate emitted on drain");
        assert!(steer_end_idx < queue_update_idx);
        let AgentEvent::QueueUpdate {
            steering,
            follow_up,
            ..
        } = &events[queue_update_idx]
        else {
            unreachable!()
        };
        assert!(steering.is_empty() && follow_up.is_empty());
        assert!(!queues.has_pending(AgentId::Main));
    }

    /// Wake delivers a queued follow-up: with no task notices but a
    /// pending follow-up, `wake` runs a bracketed turn whose user
    /// message is the queued text, then the queue is empty. This is the
    /// path the binary's turn-completion trigger drives when a turn
    /// finishes with a follow-up pending.
    #[tokio::test]
    async fn wake_delivers_queued_follow_up() {
        let queues = MessageQueues::default();
        let scripts = vec![finalize_script(finalize_text("on it"))];
        let mut agent = build_agent(scripts, Vec::new());
        agent.set_message_queues(queues.clone());
        queues.append_follow_up(AgentId::Main, "then clean up the temp files");

        let ends: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let ends_clone = Arc::clone(&ends);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            if let AgentEvent::MessageEnd { message, .. } = event {
                if let Some(text) = user_text(message) {
                    ends_clone.lock().unwrap().push(text);
                }
            }
        }));

        let outcome = agent
            .wake(CancellationToken::new())
            .await
            .expect("wake runs the queued follow-up");
        assert_eq!(outcome, crate::WakeOutcome::Ran);

        assert_eq!(
            *ends.lock().unwrap(),
            vec!["then clean up the temp files".to_string()]
        );
        let first = user_text(&agent.messages()[0]).expect("first message is the follow-up");
        assert_eq!(first, "then clean up the temp files");
        assert!(!queues.has_pending(AgentId::Main));
    }
}
