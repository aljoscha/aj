// Event-driven contract modules consumed by `aj-next`, the
// upcoming `aj-session` crate, and the in-tree `aj` binary. The
// agent runtime in this file drives tools through the new
// [`tool::ToolDefinition`] / [`tool::ToolContext`] surface; the
// legacy `&mut dyn AjUi`-bound tool trait was retired in §2.4a of
// the aj-next plan. `aj_ui::AjUi` itself stays alive for run-level
// orchestration (notices, history display, readline loop, usage
// summary) until §2.4b moves that responsibility to the binary.
pub mod bus;
pub mod events;
pub mod message;
pub mod persistence;
pub mod tool;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::pin::{pin, Pin};
use std::time::Duration;

use aj_conf::{display_path, AgentEnv, ConfigThinkingLevel};
use aj_models::messages::{ApiError, ContentBlock, ContentBlockParam, Message, Role, Usage};
use aj_models::streaming::StreamingEvent;
use aj_models::tools::Tool;
use aj_models::types::UserContent;
use aj_models::ModelError;
use aj_models::{Model, ThinkingConfig};
use aj_session::{
    Conversation, ConversationEntryKind, ConversationError, ConversationLog, ConversationView,
    EntryId, ThreadFilter, ThreadKind,
};
use aj_ui::{AjUi, SubAgentUsage, TokenUsage, UsageSummary, UserOutput};

use crate::bus::{EventBus, Listener, SubscriptionHandle};
use crate::events::{AgentEvent, AgentId, PersistedMessageKind, StreamAction, StreamChannel};
use crate::tool::{
    ErasedToolDefinition, SpawnedAgent, TodoItem, ToolContext, ToolDetails, ToolOutcome,
};
use anyhow::anyhow;
use futures::{Stream, StreamExt};
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;
use tokio_retry2::strategy::{jitter, ExponentialBackoff};
use tokio_util::sync::CancellationToken;

pub struct Agent<UI: AjUi> {
    env: AgentEnv,
    ui: UI,
    /// The base system prompt template provided by the host (compile-time
    /// constant, ships with the binary). The full prompt sent to the
    /// model is derived from this plus environment-dependent context
    /// (`AgentEnv`), and then frozen on the conversation log so that
    /// resumed threads reuse the original assembly verbatim and keep
    /// hitting Anthropic's prompt cache.
    system_prompt: &'static str,
    /// The fully-assembled system prompt for the current run, populated
    /// by [Agent::resolve_system_prompt] on the first turn. Equal to the
    /// thread's persisted [aj_session::ConversationEntryKind::SystemPrompt]
    /// when one exists; otherwise freshly assembled (and persisted on
    /// fresh logs).
    assembled_system_prompt: Option<String>,
    tool_definitions: HashMap<String, ErasedToolDefinition>,
    tools: Vec<Tool>,
    /// Names of builtin tools to exclude when spawning subagents. Mirrors the
    /// filter applied to the top-level agent so subagents inherit the same
    /// tool restrictions.
    disabled_tools: Vec<String>,
    model: Arc<dyn Model>,
    session_state: SessionState,
    default_thinking: Option<ThinkingConfig>,
    /// Identifier used on every event emitted by this agent. The
    /// top-level instance constructed by the binary keeps the default
    /// [AgentId::Main]; sub-agents created via
    /// [SessionContextWrapper::spawn_agent] override this so listeners
    /// can route nested transcripts.
    agent_id: AgentId,
    /// Internal event bus. Every state transition the agent goes
    /// through is mirrored here as an [AgentEvent]; today nothing
    /// production-side subscribes (the CLI still drives off the
    /// `self.ui.*` calls), but tests subscribe to lock the protocol
    /// shape, and §2.3 of `docs/aj-next-plan.md` swaps in production
    /// listeners for rendering and persistence.
    bus: EventBus,
    /// Cancellation token surfaced to tools through
    /// [`ToolContext::cancellation`]. Today the agent never fires
    /// it: cancellation propagation lands in §1.8 of the aj-next
    /// plan, but the field is wired through now so tools observing
    /// `select!` against `ctx.cancellation()` compile cleanly.
    /// Sub-agents inherit a child token derived from their parent's
    /// (`docs/aj-next-plan.md` §1.6) so a single eventual `cancel()`
    /// call reaches the whole hierarchy.
    cancellation: CancellationToken,
}

impl<UI: AjUi> Agent<UI> {
    pub fn new(
        env: AgentEnv,
        ui: UI,
        system_prompt: &'static str,
        tools: Vec<ErasedToolDefinition>,
        disabled_tools: Vec<String>,
        model: Arc<dyn Model>,
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
            ui,
            system_prompt,
            assembled_system_prompt: None,
            tool_definitions,
            tools: api_tools,
            disabled_tools,
            model,
            session_state,
            default_thinking,
            agent_id: AgentId::Main,
            bus: EventBus::new(),
            cancellation: CancellationToken::new(),
        }
    }

    /// Override this agent's [AgentId] before driving any turns.
    ///
    /// Used by [SessionContextWrapper::spawn_agent] when constructing
    /// a sub-agent so the events it emits carry the correct
    /// [AgentId::Sub] tag. Top-level instances built by the binary
    /// keep the default [AgentId::Main] and never call this.
    pub fn set_agent_id(&mut self, id: AgentId) {
        self.agent_id = id;
    }

    /// Replace this agent's event bus.
    ///
    /// Used by [SessionContextWrapper::spawn_agent] to make a
    /// sub-agent share the parent's bus per `docs/aj-next-plan.md`
    /// §1.6: every event the child emits then reaches every listener
    /// the binary registered on the parent (rendering, persistence,
    /// future TUI components), tagged by the child's
    /// [AgentId::Sub]. Must be called before any turn runs;
    /// subscriptions registered on the bus that's about to be
    /// replaced are silently dropped.
    pub fn set_bus(&mut self, bus: EventBus) {
        self.bus = bus;
    }

    /// Subscribe an async listener to the agent's internal event bus.
    ///
    /// Returns a [SubscriptionHandle] whose drop removes the listener.
    /// Listeners are awaited inline in registration order; a listener
    /// returning `Err` aborts the in-flight operation with a fatal
    /// error. See [EventBus::subscribe] for the full protocol.
    pub fn subscribe(&self, listener: Listener) -> SubscriptionHandle {
        self.bus.subscribe(listener)
    }

    /// Borrow the agent's internal event bus.
    ///
    /// Sub-systems (currently only [SessionContextWrapper::spawn_agent])
    /// clone this so events emitted on a sub-agent's bus can later be
    /// forwarded to the parent's listeners — the eventual
    /// "sub-agents share the parent's bus" arrangement from
    /// `docs/aj-next-plan.md` §1.6 lands once the per-tool migration
    /// in §2.2 finishes; for now sub-agents own their own bus and
    /// the parent emits only correlation events.
    pub fn bus(&self) -> &EventBus {
        &self.bus
    }

    pub fn current_turn(&self) -> usize {
        self.session_state.turn_counter()
    }

    pub fn accumulated_usage(&self) -> &Usage {
        self.session_state.accumulated_usage()
    }

    pub async fn run(
        &mut self,
        log: Arc<TokioMutex<ConversationLog>>,
    ) -> Result<(), anyhow::Error> {
        // Mirror the run as `AgentStart` / `AgentEnd` events on the
        // bus. `AgentEnd.messages` will eventually carry a snapshot
        // of the agent's transcript per `docs/aj-next-plan.md` §1.4;
        // until §2.4 migrates the agent to the unified message types,
        // we ship an empty snapshot so the protocol shape (event
        // ordering, agent_id routing) is exercised without forcing a
        // premature legacy→unified bridge.
        self.bus
            .emit(AgentEvent::AgentStart {
                agent_id: self.agent_id,
            })
            .await?;

        let outcome = self.run_inner(log).await;

        self.bus
            .emit(AgentEvent::AgentEnd {
                agent_id: self.agent_id,
                messages: Vec::new(),
            })
            .await?;

        outcome
    }

    /// Body of [Agent::run], split out so [Agent::run] itself can
    /// emit `AgentStart` / `AgentEnd` events around the run regardless
    /// of which exit path the loop takes.
    async fn run_inner(
        &mut self,
        log: Arc<TokioMutex<ConversationLog>>,
    ) -> Result<(), anyhow::Error> {
        // Setup phase. We hold the log lock for the duration of this
        // block so resolve / repair / linearize see a consistent
        // snapshot, but we never call `bus.emit` inside it — the
        // persistence listener also needs the lock and would deadlock.
        let resume_info: Option<(String, Conversation)> = {
            let mut log_guard = log.lock().await;
            // Resolve the system prompt up front: either reuse the
            // one persisted on the log (cache-friendly resume) or
            // assemble a fresh one from the current environment and
            // persist it as the log's root entry. After this returns,
            // every subsequent append from any thread (including
            // subagents) anchors to the system-prompt entry, and
            // inference reuses `self.assembled_system_prompt`
            // verbatim.
            self.resolve_system_prompt(&mut log_guard)?;

            // Seed the subagent counter from the log so ids minted
            // in this session don't collide with subagent subtrees
            // already persisted from a prior session.
            if let Some(max_id) = log_guard.max_agent_id() {
                self.session_state.seed_sub_agent_counter(max_id);
            }

            if let Some(head) = log_guard.latest_leaf(ThreadFilter::USER) {
                let conversation = log_guard.linearize(&head, ThreadFilter::USER);
                // Because the log writes to disk after every event,
                // resuming can land us on a state where the last
                // assistant message carries `tool_use` blocks that
                // never got their matching `tool_result`s (process
                // was killed between the two). Sending that to the
                // model would fail, so synthesize "interrupted"
                // tool_results for any dangling tool_use ids before
                // we carry on.
                repair_interrupted_tool_uses(&mut log_guard, &conversation)?;
                Some((log_guard.thread_id().to_string(), conversation))
            } else {
                None
            }
        };

        // Render conversation history through the legacy UI before
        // any bus emit fires so the user sees it interleaved
        // correctly with the resume notice below. `display_*` is
        // synchronous and doesn't touch the log.
        if let Some((_, conversation)) = &resume_info {
            self.display_conversation_history(conversation);
        }

        if let Some((thread_id, _)) = &resume_info {
            self.notice(format!(
                "Resuming conversation {thread_id} (use 'ctrl-c' or 'ctrl-d' to quit)"
            ))
            .await?;
        } else {
            self.notice("Chat with AJ (use 'ctrl-c' or 'ctrl-d' to quit)")
                .await?;
        }

        self.notice(format!(
            "Model: {}, at {}",
            self.model.model_name(),
            self.model.model_url()
        ))
        .await?;

        self.display_context().await?;

        if std::env::var("AJ_DISABLE_SANDBOX_WARNING").is_err() {
            self.warning(
                "WARNING: AJ has no sandboxing or permission checks. The agent can execute \
                 arbitrary commands on your system. Do not use AJ if you don't understand what \
                 this means. Set AJ_DISABLE_SANDBOX_WARNING=1 to suppress this warning.",
            )
            .await?;
        }

        let mut force_user_input = false;
        loop {
            // Decide whether the next iteration needs user input.
            // Lock briefly to peek at the user thread; drop before
            // we touch the UI.
            let (need_user_input, has_history) = {
                let log_guard = log.lock().await;
                let head = log_guard.latest_leaf(ThreadFilter::USER);
                let need = force_user_input
                    || match &head {
                        Some(id) => {
                            let conversation = log_guard.linearize(id, ThreadFilter::USER);
                            match conversation.last_message() {
                                Some(last) => matches!(last.role, Role::Assistant),
                                None => true,
                            }
                        }
                        None => true,
                    };
                (need, head.is_some())
            };
            force_user_input = false;

            if need_user_input {
                let user_input = self.ui.get_user_input();
                let user_input = if let Some(user_input) = user_input {
                    user_input
                } else {
                    self.display_usage_summary();
                    // Show the resume hint only if the user has
                    // actually sent at least one message so far.
                    if has_history {
                        let id = log.lock().await.thread_id().to_string();
                        self.notice(format!("Thread: {id} (resume with: aj continue {id})"))
                            .await?;
                    }
                    break;
                };
                // Persist the user message directly (the binary will
                // own this write in §2.5; until then, the agent
                // still drives the readline loop and writes its own
                // input). No bus emit yet — we need the lock.
                let mut log_guard = log.lock().await;
                let head = log_guard.latest_leaf(ThreadFilter::USER);
                let mut view = ConversationView::user(&mut log_guard, head);
                view.add_user_message(vec![ContentBlockParam::new_text_block(user_input)])?;
            }

            match self
                .execute_turn(Arc::clone(&log), ThreadKind::User, None)
                .await
            {
                Ok(()) => {}
                Err(TurnError::Recoverable(err)) => {
                    self.error(format!("{err:#}")).await?;
                    // The pending user message is still on disk.
                    // Force a prompt next iteration so we don't
                    // immediately re-send the same broken request to
                    // the model. The user can type a follow-up
                    // (which will be appended to the conversation)
                    // or hit Ctrl-C/D to quit.
                    force_user_input = true;
                    continue;
                }
                Err(TurnError::Fatal(err)) => return Err(err),
            }
        }

        Ok(())
    }

    pub async fn run_single_turn(
        &mut self,
        log: Arc<TokioMutex<ConversationLog>>,
        parent_head: EntryId,
        agent_id: usize,
        prompt: String,
    ) -> Result<String, anyhow::Error> {
        // Sub-agent runs share the same lifecycle framing as the
        // top-level agent — `AgentStart` / `AgentEnd` events bracket
        // the entire run so listeners that group by `agent_id` see a
        // self-contained nested transcript.
        self.bus
            .emit(AgentEvent::AgentStart {
                agent_id: self.agent_id,
            })
            .await?;

        let outcome = self
            .run_single_turn_inner(log, parent_head, agent_id, prompt)
            .await;

        self.bus
            .emit(AgentEvent::AgentEnd {
                agent_id: self.agent_id,
                messages: Vec::new(),
            })
            .await?;

        outcome
    }

    async fn run_single_turn_inner(
        &mut self,
        log: Arc<TokioMutex<ConversationLog>>,
        parent_head: EntryId,
        agent_id: usize,
        prompt: String,
    ) -> Result<String, anyhow::Error> {
        // Resolve the system prompt before any inference. For
        // sub-agents the parent has already populated the log's
        // `SystemPrompt` entry, so this just reads it back; the
        // assembled prompt is shared across the whole session.
        {
            let mut log_guard = log.lock().await;
            self.resolve_system_prompt(&mut log_guard)?;
        }

        // Append the prompt as the sub-agent's first user message
        // anchored at `parent_head` (the spawning assistant message
        // on the parent thread). Direct write — no bus emit so we
        // hold the lock through it.
        {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::subagent(&mut log_guard, parent_head, agent_id);
            view.add_user_message(vec![ContentBlockParam::new_text_block(prompt)])?;
        }

        self.execute_turn(Arc::clone(&log), ThreadKind::Subagent, Some(agent_id))
            .await?;

        // Extract the last assistant message text from the
        // subagent's own linearized history.
        let log_guard = log.lock().await;
        let head = log_guard
            .latest_leaf(ThreadFilter::subagent(agent_id))
            .ok_or_else(|| anyhow!("subagent produced no entries"))?;
        let conversation = log_guard.linearize(&head, ThreadFilter::subagent(agent_id));
        let last_msg = conversation
            .last_assistant_message()
            .ok_or_else(|| anyhow!("subagent produced no assistant text output"))?;

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

    /// Executes a single "turn" of the conversation, this will potentially
    /// include mutliple back-and-forth interactions with the model, in case
    /// there are thinking blocks or tool calls.
    ///
    /// The turn's writes (assistant message, per-tool user outputs,
    /// tool-result user message) flow out as
    /// [`AgentEvent::MessagePersisted`] events. The persistence
    /// listener subscribed on the bus translates them into
    /// [`ConversationView`] appends, one JSONL line per event, so
    /// the on-disk state stays at-most one event behind reality
    /// (see `docs/aj-next-plan.md` §2.3b).
    async fn execute_turn(
        &mut self,
        log: Arc<TokioMutex<ConversationLog>>,
        thread: ThreadKind,
        agent_id: Option<usize>,
    ) -> Result<(), TurnError> {
        self.session_state.turn_counter += 1;

        // `TurnStart` mirrors entry to the assistant-message cycle.
        // The matching `TurnEnd` event (which carries the finalized
        // assistant message and tool-result list per `docs/aj-next-plan.md`
        // §1.1) lands in §2.4 once `aj-agent` migrates to the unified
        // message types; today we have only the legacy `MessageParam`
        // shape, and bridging it through a throwaway converter would
        // be code we'd delete in the same week.
        self.bus
            .emit(AgentEvent::TurnStart {
                agent_id: self.agent_id,
            })
            .await
            .map_err(TurnError::Fatal)?;

        let filter = match thread {
            ThreadKind::User => ThreadFilter::USER,
            ThreadKind::Subagent => {
                ThreadFilter::subagent(agent_id.expect("subagent thread requires agent_id"))
            }
            // `execute_turn` only runs against user/subagent threads;
            // meta entries are structural and never the subject of a turn.
            ThreadKind::Meta => {
                return Err(TurnError::Fatal(anyhow!(
                    "execute_turn called with ThreadKind::Meta"
                )));
            }
        };

        // Number of streaming retries observed for the current
        // inference. Reported on `StreamRetry` events so listeners
        // can render "retrying… (attempt N)" indicators.
        let mut retry_attempt: u32 = 0;
        let mut retry_strategy = None;

        'outer: loop {
            // Lock briefly to snapshot the conversation for inference.
            // The lock is dropped before the inference call so the
            // persistence listener (or anything else) can take it.
            let conversation = {
                let log_guard = log.lock().await;
                let head = log_guard
                    .latest_leaf(filter)
                    .ok_or_else(|| anyhow!("execute_turn called on an empty thread"))?;
                log_guard.linearize(&head, filter)
            };
            let response_stream = self.run_inference_streaming(&conversation).await?;

            let mut response: Option<Message> = None;
            // Tool_use blocks whose `input` JSON failed to parse arrive
            // as ToolUseParseError and are dropped from the assistant
            // content. Collect (id, name, error) so we can resurrect
            // them with a paired `is_error: true` tool_result below.
            let mut tool_use_parse_errors: Vec<(String, String, String)> = Vec::new();

            {
                let mut response_stream = pin!(response_stream);
                while let Some(event) = response_stream.next().await {
                    match event {
                        StreamingEvent::MessageStart { .. } => {}
                        StreamingEvent::UsageUpdate { .. } => {}
                        StreamingEvent::FinalizedMessage { message } => {
                            response = Some(message);
                        }
                        StreamingEvent::Error { error } if error.is_overloaded() => {
                            // We initialize the strategy when we see the first
                            // overloaded error.
                            if retry_strategy.is_none() {
                                retry_strategy = Some(Self::create_retry_strategy());
                            }

                            let retry_sleep =
                                retry_strategy.as_mut().expect("known to be some").next();

                            if let Some(retry_sleep) = retry_sleep {
                                let message =
                                    format!("{}, retrying in {}s...", error, retry_sleep.as_secs());
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
                                        error: error.to_string(),
                                    })
                                    .await
                                    .map_err(TurnError::Fatal)?;

                                tokio::time::sleep(retry_sleep).await;

                                continue 'outer;
                            } else {
                                return Err(error.into());
                            }
                        }
                        StreamingEvent::Error { error } => {
                            return Err(error.into());
                        }
                        StreamingEvent::TextStart { text, citations: _ } => {
                            self.bus
                                .emit(AgentEvent::StreamChunk {
                                    agent_id: self.agent_id,
                                    channel: StreamChannel::Text,
                                    action: StreamAction::Start {
                                        snapshot: text.clone(),
                                    },
                                })
                                .await
                                .map_err(TurnError::Fatal)?;
                        }
                        StreamingEvent::TextUpdate { diff, snapshot: _ } => {
                            self.bus
                                .emit(AgentEvent::StreamChunk {
                                    agent_id: self.agent_id,
                                    channel: StreamChannel::Text,
                                    action: StreamAction::Update {
                                        delta: diff.clone(),
                                    },
                                })
                                .await
                                .map_err(TurnError::Fatal)?;
                        }
                        StreamingEvent::TextStop { text } => {
                            self.bus
                                .emit(AgentEvent::StreamChunk {
                                    agent_id: self.agent_id,
                                    channel: StreamChannel::Text,
                                    action: StreamAction::Stop {
                                        snapshot: text.clone(),
                                    },
                                })
                                .await
                                .map_err(TurnError::Fatal)?;
                        }
                        StreamingEvent::ThinkingStart { thinking } => {
                            self.bus
                                .emit(AgentEvent::StreamChunk {
                                    agent_id: self.agent_id,
                                    channel: StreamChannel::Thinking,
                                    action: StreamAction::Start {
                                        snapshot: thinking.clone(),
                                    },
                                })
                                .await
                                .map_err(TurnError::Fatal)?;
                        }
                        StreamingEvent::ThinkingUpdate { diff, snapshot: _ } => {
                            self.bus
                                .emit(AgentEvent::StreamChunk {
                                    agent_id: self.agent_id,
                                    channel: StreamChannel::Thinking,
                                    action: StreamAction::Update {
                                        delta: diff.clone(),
                                    },
                                })
                                .await
                                .map_err(TurnError::Fatal)?;
                        }
                        StreamingEvent::ThinkingStop => {
                            // Thinking streaming has no terminal text
                            // (the streaming layer doesn't accumulate
                            // a final snapshot for thinking blocks),
                            // so the renderer-bridge listener treats
                            // this as a "thinking finished, flush
                            // newline" signal regardless of snapshot
                            // contents.
                            self.bus
                                .emit(AgentEvent::StreamChunk {
                                    agent_id: self.agent_id,
                                    channel: StreamChannel::Thinking,
                                    action: StreamAction::Stop {
                                        snapshot: String::new(),
                                    },
                                })
                                .await
                                .map_err(TurnError::Fatal)?;
                        }
                        StreamingEvent::ParseError { error, raw_data } => {
                            let message = format!("Parse error: {error} (raw data: {raw_data})");
                            self.bus
                                .emit(AgentEvent::Error {
                                    agent_id: self.agent_id,
                                    text: message,
                                })
                                .await
                                .map_err(TurnError::Fatal)?;
                        }
                        StreamingEvent::ToolUseParseError {
                            id,
                            name,
                            error,
                            raw_data,
                        } => {
                            let message = format!(
                                "Tool use parse error for '{name}' (id={id}): {error} \
                                 (raw data: {raw_data})"
                            );
                            self.bus
                                .emit(AgentEvent::Error {
                                    agent_id: self.agent_id,
                                    text: message,
                                })
                                .await
                                .map_err(TurnError::Fatal)?;
                            tool_use_parse_errors.push((id, name, error));
                        }
                        StreamingEvent::ProtocolError { error } => {
                            let message = format!("Protocol error: {error}");
                            self.bus
                                .emit(AgentEvent::Error {
                                    agent_id: self.agent_id,
                                    text: message,
                                })
                                .await
                                .map_err(TurnError::Fatal)?;
                        }
                    }

                    // We've successfully received an event, reset the retry
                    // strategy. That way, when we get an Overloaded error again
                    // we'll initialize with a fresh retry_strategy.
                    retry_strategy = None;
                    retry_attempt = 0;
                }
            }

            let mut response = response.ok_or_else(|| {
                TurnError::Recoverable(anyhow!(
                    "model stream ended without producing a final message"
                ))
            })?;
            let turn_usage = response.usage.clone();

            // Resurrect tool_use blocks dropped by the streaming layer
            // due to malformed input JSON. We add a synthetic
            // ToolUseBlock with `null` input to keep the assistant
            // message structurally valid; the matching `is_error: true`
            // tool_result is produced in the execution loop below so the
            // model can retry instead of the user being bumped to the
            // prompt.
            //
            // TODO: with incremental tool-call parsing we'd have the
            // partial `input` bytes here and could pass them through
            // instead of `null`.
            let mut tool_use_parse_failures: HashMap<String, String> = HashMap::new();
            for (id, name, error) in tool_use_parse_errors.drain(..) {
                response.content.push(ContentBlock::ToolUseBlock {
                    id: id.clone(),
                    name,
                    input: serde_json::Value::Null,
                    caller: None,
                });
                tool_use_parse_failures.insert(id, error);
            }

            // Collect tool use blocks from the response
            let mut tool_calls = Vec::new();
            let mut has_tool_use = false;

            for content in response.content.iter() {
                if let ContentBlock::ToolUseBlock {
                    id, name, input, ..
                } = content
                {
                    tool_calls.push((id.clone(), name.clone(), input.clone()));
                    has_tool_use = true;
                }
            }

            // Persist the assistant's message via the bus. The
            // persistence listener writes it to disk before this
            // `emit` resolves (listeners are awaited inline), so
            // when we re-read `latest_leaf` immediately after we
            // see the freshly-appended entry. `assistant_head` is
            // the id sub-agents spawned while handling tool_use
            // blocks below anchor at.
            let message_param = response.into_message_param();
            self.bus
                .emit(AgentEvent::MessagePersisted {
                    agent_id: self.agent_id,
                    kind: PersistedMessageKind::Assistant {
                        content: message_param.content,
                    },
                })
                .await
                .map_err(TurnError::Fatal)?;
            let assistant_head = {
                let log_guard = log.lock().await;
                log_guard.latest_leaf(filter).ok_or_else(|| {
                    TurnError::Fatal(anyhow!(
                        "persistence listener failed to append assistant message"
                    ))
                })?
            };

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

                for (tool_id, tool_name, tool_input) in tool_calls {
                    // Mirror the start of every tool invocation on the
                    // bus before we do any work — listeners that render
                    // a "running…" placeholder rely on seeing this
                    // event before any update or end. The matching
                    // `ToolExecutionEnd` is emitted below regardless of
                    // whether the call succeeded, errored, or was a
                    // parse-failure that bypassed execution.
                    self.bus
                        .emit(AgentEvent::ToolExecutionStart {
                            agent_id: self.agent_id,
                            call_id: tool_id.clone(),
                            tool: tool_name.clone(),
                            args: tool_input.clone(),
                        })
                        .await
                        .map_err(TurnError::Fatal)?;

                    // Run the tool (or synthesize an error outcome for
                    // tool_use blocks the streaming layer rejected
                    // with a parse error). The agent now drives the
                    // new [`tool::ToolDefinition`] surface end-to-end:
                    // success and recoverable error both come back as
                    // a [`ToolOutcome`] the rest of the loop projects
                    // onto the wire `tool_result` block (text-flattened
                    // `content`) and the bus
                    // [`AgentEvent::ToolExecutionEnd`] event
                    // (structured `details` + `is_error`).
                    let outcome = if let Some(parse_err) = tool_use_parse_failures.remove(&tool_id)
                    {
                        // Persist a freestanding [`UserOutput::ToolError`]
                        // for the legacy on-disk shape (per the §2.0
                        // reconnaissance: every freestanding user_output
                        // entry on disk is a `ToolError`). The §3
                        // migration walker rewrites these into
                        // structured `ToolDetails` entries.
                        self.bus
                            .emit(AgentEvent::MessagePersisted {
                                agent_id: self.agent_id,
                                kind: PersistedMessageKind::UserOutput {
                                    output: UserOutput::ToolError {
                                        tool_name: tool_name.clone(),
                                        input: "<malformed json>".to_string(),
                                        error: parse_err.clone(),
                                    },
                                },
                            })
                            .await
                            .map_err(TurnError::Fatal)?;

                        let body = format!("Tool input parse error: {parse_err}");
                        ToolOutcome {
                            content: vec![UserContent::text(body.clone())],
                            details: ToolDetails::Text {
                                summary: format!("{tool_name}: error"),
                                body,
                            },
                            is_error: true,
                        }
                    } else {
                        let result = self
                            .execute_tool(
                                Arc::clone(&log),
                                assistant_head.clone(),
                                &tool_id,
                                &tool_name,
                                tool_input.clone(),
                            )
                            .await;

                        match result {
                            Ok(outcome) => outcome,
                            Err(err) => {
                                // Same legacy persistence as the parse-failure path.
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
                        }
                    };

                    // Project the structured `content` onto the
                    // legacy text-only `ToolResultContent::Text` shape
                    // for the wire. The `details` ride directly on
                    // the bus event below; persistence captures both
                    // pieces (the wire content goes into the
                    // synthesized user-role `ToolResult` message; the
                    // structured details land via a future
                    // `MessagePersisted::ToolDetails` once the on-disk
                    // format catches up — see `docs/aj-next-plan.md`
                    // §3 / §1.2).
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
                    self.bus
                        .emit(AgentEvent::MessagePersisted {
                            agent_id: self.agent_id,
                            kind: PersistedMessageKind::ToolResult {
                                content: tool_result_contents,
                            },
                        })
                        .await
                        .map_err(TurnError::Fatal)?;
                }

                // Continue the conversation loop to get the model's response to tool results
                continue;
            } else {
                // We are now ready to finish this turn. Every event that
                // belongs to this turn has already been appended
                // individually; there is no per-turn save.
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

    async fn run_inference_streaming(
        &self,
        conversation: &Conversation,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamingEvent> + Send>>, ModelError> {
        let thinking = self.determine_thinking(conversation);

        tracing::debug!(?thinking, "thinking budget");

        // `resolve_system_prompt` is called at the top of each public
        // entry point (`run`, `run_single_turn`) before any inference,
        // so the cache is always populated by the time we get here.
        let system_prompt = self
            .assembled_system_prompt
            .clone()
            .expect("system prompt must be resolved before inference");

        // The legacy `Model` trait operates on a flat slice of
        // `MessageParam`s — the wire view of the conversation. Project
        // the linearized log down to that shape here; non-message
        // entries (system prompts, tool-output stand-ins) are
        // structural metadata and never reach the wire.
        let messages: Vec<aj_models::messages::MessageParam> =
            conversation.messages().into_iter().cloned().collect();

        let response = self
            .model
            .run_inference_streaming(&messages, system_prompt, self.tools.clone(), thinking)
            .await?;

        Ok(response)
    }

    /// Determine the thinking configuration based on trigger texts in the user
    /// prompt. Returns thinking configuration based on specific trigger phrases:
    /// - "think maximum" -> 128,000 tokens
    /// - "think harder" -> 32,000 tokens
    /// - "think hard" -> 10,000 tokens
    /// - "think" -> 4,000 tokens
    /// - default -> falls back to configured default thinking level
    fn determine_thinking(&self, conversation: &Conversation) -> Option<ThinkingConfig> {
        let last_user_message = conversation.last_user_message();

        if let Some(message) = last_user_message {
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

    /// Assemble the system prompt we pass to the model from the actual system
    /// prompt and additional information we might want or need, such as
    /// information about the environment.
    fn assemble_system_prompt(&self) -> String {
        let mut text = self.system_prompt.to_string();

        // Stitch in every context file, in order. Each file is wrapped in an
        // `<agents-md>` block so the model can clearly tell where instructions
        // start and end, with the kind-specific prefix text introducing it.
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

    /// Determine the system prompt to use for this run, populating
    /// [Self::assembled_system_prompt]. Idempotent: subsequent calls
    /// within the same run are no-ops.
    ///
    /// Resolution rules:
    /// - If the log already carries a persisted `SystemPrompt` entry
    ///   (resumed thread, or a thread the parent agent already
    ///   initialized), use that verbatim. This is the cache-friendly
    ///   path: the prompt sent to the model is byte-for-byte identical
    ///   to the one used on the previous turn, so Anthropic's prompt
    ///   cache stays warm across UTC date rollovers and restarts.
    /// - Else if the log is empty, freshly assemble from the static
    ///   prompt + current env and persist as the root entry. Future
    ///   resumes of this thread will then take the first branch.
    /// - Else (legacy thread file with no persisted system prompt),
    ///   freshly assemble without persisting, falling back to the
    ///   pre-persistence behavior. The assembly may differ from what
    ///   the model originally saw on this thread, but legacy threads
    ///   never had a stable cached prompt anyway.
    fn resolve_system_prompt(
        &mut self,
        log: &mut ConversationLog,
    ) -> Result<(), ConversationError> {
        if self.assembled_system_prompt.is_some() {
            return Ok(());
        }

        if let Some(persisted) = log.system_prompt() {
            self.assembled_system_prompt = Some(persisted.to_string());
            return Ok(());
        }

        let assembled = self.assemble_system_prompt();
        if log.is_empty() {
            log.set_system_prompt(assembled.clone())?;
        }
        self.assembled_system_prompt = Some(assembled);
        Ok(())
    }

    async fn execute_tool(
        &mut self,
        log: Arc<TokioMutex<ConversationLog>>,
        parent_head: EntryId,
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

        // Build the [`ToolContext`] the tool sees: working directory,
        // todos, sub-agent spawn, cancellation token, no-op
        // `emit_update`. The wrapper holds the still-required pieces
        // for sub-agent spawning (parent log handle, parent head id,
        // model, disabled tools, parent bus, parent agent id, sub-
        // agent tool list, and a clone of the parent's UI for the
        // sub-agent's `AjUi` while §2.4a leaves run-level
        // orchestration on `self.ui`).
        let mut session_ctx_wrapper = SessionContextWrapper {
            session_ctx: &mut self.session_state,
            ui: self.ui.shallow_clone(),
            env: &self.env,
            log,
            parent_head,
            system_prompt: self.system_prompt,
            disabled_tools: &self.disabled_tools,
            model: Arc::clone(&self.model),
            sub_agent_tools,
            parent_bus: self.bus.clone(),
            parent_agent_id: self.agent_id,
            cancellation: self.cancellation.child_token(),
        };

        let outcome = (tool_def.func)(&mut session_ctx_wrapper, tool_input).await?;
        Ok(outcome)
    }

    fn display_user_output(&mut self, user_outputs: &[UserOutput]) {
        for output in user_outputs {
            match output {
                UserOutput::Notice(msg) => {
                    self.ui.display_notice(msg);
                }
                UserOutput::Error(msg) => {
                    self.ui.display_error(msg);
                }
                UserOutput::ToolResult {
                    tool_name,
                    input,
                    output,
                } => {
                    self.ui.display_tool_result(tool_name, input, output);
                }
                UserOutput::ToolResultDiff {
                    tool_name,
                    input,
                    before,
                    after,
                } => {
                    self.ui
                        .display_tool_result_diff(tool_name, input, before, after);
                }
                UserOutput::ToolError {
                    tool_name,
                    input,
                    error,
                } => {
                    self.ui.display_tool_error(tool_name, input, error);
                }
                UserOutput::TokenUsage(usage) => {
                    self.ui.display_token_usage(usage);
                }
                UserOutput::TokenUsageSummary(summary) => {
                    self.ui.display_token_usage_summary(summary);
                }
            }
        }
    }

    /// Display the context (system prompt addenda) at startup so the user can
    /// see which agent files (and, in the future, skills) are being injected
    /// into the model's system prompt.
    async fn display_context(&mut self) -> Result<(), anyhow::Error> {
        let text = if self.env.context_files.is_empty() {
            "Context: (none)".to_string()
        } else {
            let mut lines = String::from("Context:");
            for file in &self.env.context_files {
                lines.push_str(&format!(
                    "\n  - {} ({})",
                    display_path(&file.path),
                    file.kind.label()
                ));
            }
            lines
        };
        self.notice(text).await
    }

    /// Display a notice both through the UI and on the bus.
    ///
    /// The bus emit is awaited inline so a listener that returns
    /// `Err` (e.g. a future persistence listener that observes a
    /// disk-write failure) propagates the failure back to the
    /// caller. Today no production subscribers exist; the helper
    /// pairs the legacy UI call with a `Notice` event so tests can
    /// snapshot the protocol shape without touching the binary.
    async fn notice(&mut self, text: impl Into<String>) -> Result<(), anyhow::Error> {
        let text = text.into();
        self.ui.display_notice(&text);
        self.bus
            .emit(AgentEvent::Notice {
                agent_id: self.agent_id,
                text,
            })
            .await
    }

    /// Sibling of [Self::notice] for warnings.
    async fn warning(&mut self, text: impl Into<String>) -> Result<(), anyhow::Error> {
        let text = text.into();
        self.ui.display_warning(&text);
        self.bus
            .emit(AgentEvent::Warning {
                agent_id: self.agent_id,
                text,
            })
            .await
    }

    /// Sibling of [Self::notice] for errors.
    async fn error(&mut self, text: impl Into<String>) -> Result<(), anyhow::Error> {
        let text = text.into();
        self.ui.display_error(&text);
        self.bus
            .emit(AgentEvent::Error {
                agent_id: self.agent_id,
                text,
            })
            .await
    }

    fn display_usage_summary(&mut self) {
        // Create main agent usage
        let main_agent_usage = SubAgentUsage {
            agent_id: None,
            input_tokens: self.session_state.accumulated_usage.input_tokens,
            output_tokens: self.session_state.accumulated_usage.output_tokens,
            cache_creation_tokens: self
                .session_state
                .accumulated_usage
                .cache_creation_input_tokens
                .unwrap_or(0),
            cache_read_tokens: self
                .session_state
                .accumulated_usage
                .cache_read_input_tokens
                .unwrap_or(0),
        };

        // Create sub-agent usage list
        let mut sub_agent_usage = Vec::new();
        let mut total_sub_agent_input = 0;
        let mut total_sub_agent_output = 0;
        let mut total_sub_agent_cache_creation = 0;
        let mut total_sub_agent_cache_read = 0;

        for (agent_id, usage) in &self.session_state.sub_agent_usage {
            let sub_usage = SubAgentUsage {
                agent_id: Some(*agent_id),
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_creation_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
                cache_read_tokens: usage.cache_read_input_tokens.unwrap_or(0),
            };

            total_sub_agent_input += sub_usage.input_tokens;
            total_sub_agent_output += sub_usage.output_tokens;
            total_sub_agent_cache_creation += sub_usage.cache_creation_tokens;
            total_sub_agent_cache_read += sub_usage.cache_read_tokens;

            sub_agent_usage.push(sub_usage);
        }

        // Create total usage
        let total_usage = SubAgentUsage {
            agent_id: None,
            input_tokens: main_agent_usage.input_tokens + total_sub_agent_input,
            output_tokens: main_agent_usage.output_tokens + total_sub_agent_output,
            cache_creation_tokens: main_agent_usage.cache_creation_tokens
                + total_sub_agent_cache_creation,
            cache_read_tokens: main_agent_usage.cache_read_tokens + total_sub_agent_cache_read,
        };

        // Create usage summary
        let summary = UsageSummary {
            main_agent_usage,
            sub_agent_usage,
            total_usage,
        };

        // Display using UI
        self.ui.display_token_usage_summary(&summary);
    }

    fn display_conversation_history(&mut self, conversation: &Conversation) {
        if conversation.is_empty() {
            return;
        }

        for entry in conversation.entries() {
            match &entry.entry {
                ConversationEntryKind::Message(msg) => {
                    match msg.role {
                        Role::User => {
                            // Extract text content from user message
                            let text_content = msg
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

                            if !text_content.is_empty() {
                                self.ui.user_text_start("");
                                self.ui.user_text_stop(&text_content);
                            }
                        }
                        Role::Assistant => {
                            // First, display thinking content
                            for content in &msg.content {
                                if let ContentBlockParam::ThinkingBlock { thinking, .. } = content {
                                    self.ui.agent_thinking_start(thinking);
                                    self.ui.agent_thinking_stop();
                                } else if let ContentBlockParam::RedactedThinkingBlock { data } =
                                    content
                                {
                                    self.ui.agent_thinking_start(&format!(
                                        "[Redacted thinking: {}]",
                                        data
                                    ));
                                    self.ui.agent_thinking_stop();
                                }
                            }

                            // Then, display text content
                            let text_content = msg
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

                            if !text_content.is_empty() {
                                self.ui.agent_text_start("");
                                self.ui.agent_text_stop(&text_content);
                            }
                        }
                    }
                }
                ConversationEntryKind::UserOutput(user_output) => {
                    // Display user output (tool results, etc.)
                    self.display_user_output(std::slice::from_ref(user_output));
                }
                // SystemPrompt entries are model-facing metadata, not
                // shown in the user-visible conversation history.
                ConversationEntryKind::SystemPrompt { .. } => {}
            }
        }

        self.ui
            .display_notice("--- End of conversation history ---");
    }
}

/// Mutable state of an [Agent] session.
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
    /// [SessionState::next_sub_agent_id] calls mint ids strictly greater
    /// than `value`. Used on resume to avoid colliding with subagent
    /// subtrees already persisted in the log.
    fn seed_sub_agent_counter(&mut self, value: usize) {
        self.sub_agent_counter = value;
    }

    fn record_sub_agent_usage(&mut self, agent_id: usize, usage: Usage) {
        self.sub_agent_usage.insert(agent_id, usage);
    }
}

/// Wrapper that provides partial access to mutable [Agent] state, while we have
/// partial immutable access to other parts. Used in [Agent::execute_tool].
struct SessionContextWrapper<'a, UI: AjUi> {
    session_ctx: &'a mut SessionState,
    env: &'a AgentEnv,
    ui: UI,
    /// The one log owned by this session. Subagents append into it
    /// (through their own [ConversationView]) rather than creating
    /// sibling files. Held behind an `Arc<tokio::sync::Mutex<_>>`
    /// per `docs/aj-next-plan.md` §2.3b so the agent and the
    /// persistence listener can both reach it; the wrapper only
    /// passes it through to the spawned sub-agent.
    log: Arc<TokioMutex<ConversationLog>>,
    /// The entry id that subagents spawned from this tool invocation
    /// should anchor at. Typically the assistant message that carried
    /// the spawning `tool_use` block.
    parent_head: EntryId,
    system_prompt: &'static str,
    disabled_tools: &'a [String],
    model: Arc<dyn Model>,
    /// Snapshot of the parent's tool list. Sub-agents inherit this
    /// minus the `agent` tool. Cloning per-spawn is cheap because
    /// every `ErasedToolDefinition` field is `Clone` and the closure
    /// is `Arc`-shared.
    sub_agent_tools: Vec<ErasedToolDefinition>,
    /// Clone of the parent agent's event bus. Used to emit
    /// [AgentEvent::SubAgentStart] / [AgentEvent::SubAgentEnd]
    /// correlation events on the parent's bus when this wrapper's
    /// [ToolContext::spawn_agent] runs. Sub-agents share this bus
    /// per `docs/aj-next-plan.md` §1.6 so every event a child emits
    /// reaches the listeners the binary registered on the parent,
    /// tagged with [`AgentId::Sub`].
    parent_bus: EventBus,
    /// Identifier of the parent agent that owns this wrapper. The
    /// `parent` field of [AgentEvent::SubAgentStart] /
    /// [AgentEvent::SubAgentEnd].
    parent_agent_id: AgentId,
    /// Cancellation token surfaced through [`ToolContext::cancellation`].
    /// Derived from the parent agent's token via
    /// [`CancellationToken::child_token`] so a future `Agent::cancel`
    /// reaches in-flight tools and any sub-agents they spawn.
    cancellation: CancellationToken,
}

impl<'a, UI: AjUi> ToolContext for SessionContextWrapper<'a, UI> {
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
            // single listener (e.g. the future TUI's event pump) can
            // group nested transcripts under the parent's
            // tool-execution component.
            self.parent_bus
                .emit(AgentEvent::SubAgentStart {
                    parent: self.parent_agent_id,
                    child: child_id,
                    task: task.clone(),
                })
                .await?;

            // Create a sub-agent UI wrapper with the agent number
            let sub_ui = self.ui.get_subagent_ui(agent_id);

            // Build the sub-agent's tool list by cloning the parent's
            // (the toolset is filtered upstream when the binary calls
            // `Agent::new`), then dropping the `agent` tool itself to
            // prevent infinite recursion. We clone rather than re-call
            // `get_builtin_tools` so `aj-agent` doesn't depend on
            // `aj-tools`.
            let disabled_tools = self.disabled_tools.to_vec();
            let sub_agent_tools: Vec<ErasedToolDefinition> = self
                .sub_agent_tools
                .iter()
                .filter(|tool| tool.name != "agent")
                .cloned()
                .collect();

            // Create a new agent with the sub-agent UI. It shares this
            // session's log; its entries are appended as `Subagent`
            // entries in the same file, rooted at `self.parent_head`.
            let mut sub_agent = Agent::new(
                self.env.clone(),
                sub_ui,
                self.system_prompt,
                sub_agent_tools,
                disabled_tools,
                Arc::clone(&self.model),
                None,
            );
            sub_agent.set_agent_id(child_id);
            // Share the parent's bus per `docs/aj-next-plan.md` §1.6:
            // every event the sub-agent emits during its run reaches
            // the listeners the binary registered on the parent
            // (rendering, persistence), tagged with `Sub(n)`. Without
            // this the sub-agent runs on its own bus and the binary's
            // bridge listener never sees its activity.
            sub_agent.set_bus(self.parent_bus.clone());

            // Run the sub-agent with the task, anchored at the parent
            // tool-use's assistant message.
            let result = sub_agent
                .run_single_turn(
                    Arc::clone(&self.log),
                    self.parent_head.clone(),
                    agent_id,
                    task,
                )
                .await;

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

            // Surface the freshly-allocated sub-agent id alongside the
            // child's final assistant text. Errors still propagate via
            // `?` so the agent runtime keeps synthesizing a generic
            // tool-error result for failed spawns.
            result.map(|report| SpawnedAgent { agent_id, report })
        })
    }

    fn emit_update(&mut self, _partial: ToolDetails) {
        // No-op for §2.4a. The trait's `emit_update` is synchronous
        // but the bus's `emit` is async, so we cannot drive
        // [`AgentEvent::ToolExecutionUpdate`] inline from a sync
        // context without breaking the bus's "listeners are awaited
        // inline" guarantee (firing the listener from a spawned task
        // would let `Update` events arrive after `End`). Today only
        // `bash` calls `emit_update`, the legacy CLI doesn't render
        // it, and dropping the snapshot is functionally equivalent
        // to the pre-§2.4a behavior. A bus-side `try_emit_sync` (or
        // an async `emit_update` on the trait) lands when the TUI
        // needs progress streaming.
    }

    fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

/// Scan the linearized user thread for `tool_use` blocks that never got a
/// matching `tool_result`. If any are found, synthesize a single user
/// message with one `tool_result` block per dangling id and append it to
/// the log so the conversation is valid input for the model again.
///
/// Without this, a process killed between writing the assistant message
/// and writing the tool_result user message would leave the file in a
/// state that both Anthropic and OpenAI APIs reject on resume.
fn repair_interrupted_tool_uses(
    log: &mut ConversationLog,
    conversation: &Conversation,
) -> Result<(), anyhow::Error> {
    // Collect all tool_use ids from assistant messages and all
    // tool_result ids seen in subsequent user messages. Anything in the
    // first set that isn't in the second set is dangling.
    let mut used: HashSet<String> = HashSet::new();
    let mut resolved: HashSet<String> = HashSet::new();
    for entry in conversation.entries() {
        let ConversationEntryKind::Message(msg) = &entry.entry else {
            continue;
        };
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

    let dangling: Vec<String> = used.difference(&resolved).cloned().collect();
    if dangling.is_empty() {
        return Ok(());
    }

    tracing::warn!(
        "resuming past {} interrupted tool call(s); synthesizing error results",
        dangling.len()
    );

    let tool_result_contents: Vec<ContentBlockParam> = dangling
        .into_iter()
        .map(|tool_use_id| ContentBlockParam::ToolResultBlock {
            tool_use_id,
            content: "Previous session was interrupted before this tool call completed."
                .to_string()
                .into(),
            is_error: true,
        })
        .collect();

    let head = log
        .latest_leaf(ThreadFilter::USER)
        .expect("repair called with a non-empty user thread");
    let mut view = ConversationView::user(log, Some(head));
    view.add_user_message(tool_result_contents)?;
    Ok(())
}

/// Error returned from [Agent::execute_turn].
///
/// `Recoverable` errors (model API failures, malformed streaming responses,
/// etc.) are surfaced to the user so they can retry or rephrase, rather
/// than aborting the program. `Fatal` errors (log persistence failures,
/// internal invariant violations) bubble out so the user gets a clean
/// exit instead of silently looping.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    /// An ephemeral error encountered while talking to the model. The
    /// conversation log is in a consistent state and the user can retry
    /// by submitting another message.
    #[error("{0:#}")]
    Recoverable(anyhow::Error),
    /// A persistent failure (e.g. failed disk write) or an internal
    /// invariant violation. Bubble out to the top level.
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

impl From<ConversationError> for TurnError {
    fn from(e: ConversationError) -> Self {
        TurnError::Fatal(e.into())
    }
}

impl From<anyhow::Error> for TurnError {
    fn from(e: anyhow::Error) -> Self {
        TurnError::Fatal(e)
    }
}

#[cfg(test)]
mod event_protocol_tests {
    //! Snapshot the event protocol the agent emits on its bus.
    //!
    //! Per `docs/aj-next-plan.md` §2.1, the bus runs alongside the
    //! legacy `AjUi` calls during Phase 0; this module locks the
    //! event sequence for known-shape turns so subsequent commits
    //! (per-tool migration, atomic CLI swap, type unification)
    //! cannot silently regress the protocol.

    use std::path::PathBuf;
    use std::sync::Mutex;

    use aj_conf::AgentEnv;
    use aj_models::messages::{
        ContentBlock, Message as LegacyMessage, MessageType, Role, StopReason, Usage,
    };
    use aj_models::streaming::StreamingEvent;
    use aj_models::tools::Tool;
    use aj_models::{Model, ModelError, ThinkingConfig};
    use aj_session::{ConversationLog, ConversationPersistence};
    use aj_ui::{AjUi, TokenUsage, UsageSummary};
    use async_trait::async_trait;
    use futures::stream;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex as TokioMutex;

    use crate::bus::listener_from_sync;
    use crate::events::{AgentEvent, AgentId, PersistedMessageKind};
    use crate::persistence::persistence_listener;
    use crate::tool::{
        ErasedToolDefinition, ToolContext, ToolDefinition, ToolDetails, ToolOutcome,
    };
    use crate::Agent;

    /// Stub AjUi that swallows every interaction. We assert against
    /// the bus, not against the legacy UI; the agent still routes
    /// every notice/warning/error through both during §2.1.
    #[derive(Clone)]
    struct NoopUi;

    impl AjUi for NoopUi {
        fn display_notice(&mut self, _notice: &str) {}
        fn display_warning(&mut self, _warning: &str) {}
        fn display_error(&mut self, _error: &str) {}
        fn get_user_input(&mut self) -> Option<String> {
            None
        }
        fn agent_text_start(&mut self, _text: &str) {}
        fn agent_text_update(&mut self, _diff: &str) {}
        fn agent_text_stop(&mut self, _text: &str) {}
        fn user_text_start(&mut self, _text: &str) {}
        fn user_text_update(&mut self, _diff: &str) {}
        fn user_text_stop(&mut self, _text: &str) {}
        fn agent_thinking_start(&mut self, _thinking: &str) {}
        fn agent_thinking_update(&mut self, _diff: &str) {}
        fn agent_thinking_stop(&mut self) {}
        fn display_tool_result(&mut self, _tool_name: &str, _input: &str, _output: &str) {}
        fn display_tool_result_diff(
            &mut self,
            _tool_name: &str,
            _input: &str,
            _before: &str,
            _after: &str,
        ) {
        }
        fn display_tool_error(&mut self, _tool_name: &str, _input: &str, _error: &str) {}
        fn ask_permission(&mut self, _message: &str) -> bool {
            false
        }
        fn display_token_usage(&mut self, _usage: &TokenUsage) {}
        fn display_token_usage_summary(&mut self, _summary: &UsageSummary) {}
        fn get_subagent_ui(&mut self, _agent_number: usize) -> Box<dyn AjUi> {
            Box::new(NoopUi)
        }
        fn shallow_clone(&mut self) -> Box<dyn AjUi> {
            Box::new(NoopUi)
        }
    }

    /// Fake [Model] that hands back canned [StreamingEvent] streams.
    ///
    /// The agent loops over inferences when the model returns a
    /// `tool_use`, so the test feeds in one stream per inference.
    /// Each `Vec<StreamingEvent>` is consumed in source order; the
    /// test panics if the agent runs more inferences than were
    /// queued, which would indicate a regression that adds an
    /// unexpected loop iteration.
    struct ScriptedModel {
        scripts: Mutex<std::vec::IntoIter<Vec<StreamingEvent>>>,
    }

    impl ScriptedModel {
        fn new(scripts: Vec<Vec<StreamingEvent>>) -> Self {
            Self {
                scripts: Mutex::new(scripts.into_iter()),
            }
        }
    }

    #[async_trait]
    impl Model for ScriptedModel {
        async fn run_inference_streaming(
            &self,
            _messages: &[aj_models::messages::MessageParam],
            _system_prompt: String,
            _tools: Vec<Tool>,
            _thinking: Option<ThinkingConfig>,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = StreamingEvent> + Send>>, ModelError>
        {
            let next = self
                .scripts
                .lock()
                .unwrap()
                .next()
                .expect("ScriptedModel exhausted: agent ran more inferences than scripted");
            Ok(Box::pin(stream::iter(next)))
        }

        fn model_name(&self) -> String {
            "scripted".to_string()
        }

        fn model_url(&self) -> String {
            "fake://test".to_string()
        }
    }

    /// Trivial tool that returns a fixed string. Implements the
    /// new-shape [`ToolDefinition`] so the test exercises the same
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

    /// Build a finalized [LegacyMessage] with a single tool_use block.
    fn finalize_tool_use(tool_use_id: &str, tool_name: &str) -> LegacyMessage {
        LegacyMessage {
            id: "test-msg-1".to_string(),
            r#type: MessageType::Message,
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUseBlock {
                id: tool_use_id.to_string(),
                name: tool_name.to_string(),
                input: serde_json::json!({}),
                caller: None,
            }],
            model: "scripted".to_string(),
            stop_reason: Some(StopReason::ToolUse),
            stop_sequence: None,
            stop_details: None,
            usage: Usage::default(),
            container: None,
            context_management: None,
        }
    }

    /// Build a finalized [LegacyMessage] with a single text block.
    fn finalize_text(text: &str) -> LegacyMessage {
        LegacyMessage {
            id: "test-msg-2".to_string(),
            r#type: MessageType::Message,
            role: Role::Assistant,
            content: vec![ContentBlock::TextBlock {
                text: text.to_string(),
                citations: Vec::new(),
                signature: None,
            }],
            model: "scripted".to_string(),
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            stop_details: None,
            usage: Usage::default(),
            container: None,
            context_management: None,
        }
    }

    /// Compact, comparable representation of an [AgentEvent] for
    /// snapshot assertions. We don't `derive(PartialEq)` on the real
    /// enum because some payloads (e.g. the legacy `AssistantMessage`
    /// once it arrives in §2.4) don't implement `PartialEq` cleanly,
    /// and a label per variant keeps test failures readable.
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
        /// kind discriminator (Assistant / ToolResult / UserOutput)
        /// so test assertions don't have to care about the exact
        /// content blocks the event carries.
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

    /// Set up a temp directory with an empty conversation log carrying
    /// a fixed system prompt. Returns the log behind the same shared
    /// handle the agent expects so each test can register its own
    /// persistence listener and pass the handle into `run_single_turn`.
    fn fresh_log() -> (TempDir, Arc<TokioMutex<ConversationLog>>) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("threads"));
        let mut log = ConversationLog::create(&persistence).expect("ConversationLog::create");
        log.set_system_prompt("test system prompt".to_string())
            .expect("set_system_prompt on fresh log");
        (dir, Arc::new(TokioMutex::new(log)))
    }

    /// Build an [AgentEnv] that doesn't pull instructions from the
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

    #[tokio::test]
    async fn run_single_turn_with_tool_call_emits_locked_protocol() {
        let (_dir, log) = fresh_log();
        let parent_head = log
            .lock()
            .await
            .system_prompt_id()
            .expect("set_system_prompt populates the root entry")
            .clone();

        // Two scripted inferences:
        //   1. Tool call (id="tu-1", name="ping").
        //   2. Final text response after the tool result is fed back.
        let scripts = vec![
            vec![StreamingEvent::FinalizedMessage {
                message: finalize_tool_use("tu-1", "ping"),
            }],
            vec![StreamingEvent::FinalizedMessage {
                message: finalize_text("done"),
            }],
        ];
        let model = Arc::new(ScriptedModel::new(scripts));

        let env = empty_env(std::env::temp_dir());
        let ping: ErasedToolDefinition = PingTool.into();
        let mut agent = Agent::new(
            env,
            NoopUi,
            "irrelevant — log already has a frozen system prompt",
            vec![ping],
            Vec::new(),
            model,
            None,
        );
        // Mirror what `SessionContextWrapper::spawn_agent` does in
        // production: tagging the agent with its sub-agent id keeps
        // bus events and persistence writes aligned to the
        // [`ThreadKind::Subagent`] entries `run_single_turn` will
        // append below.
        agent.set_agent_id(AgentId::Sub(1));

        // Register the persistence listener so `MessagePersisted`
        // events actually reach disk; without this the agent can't
        // recover the assistant message's id via `latest_leaf` after
        // the emit and the run aborts with a Fatal error.
        let _persistence_handle = agent.subscribe(persistence_listener(Arc::clone(&log)));

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = recorded.clone();
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        agent
            .run_single_turn(Arc::clone(&log), parent_head, 1, "run ping".to_string())
            .await
            .expect("run_single_turn");

        let events = recorded.lock().unwrap().clone();
        let expected = vec![
            EventLabel::AgentStart(AgentId::Sub(1)),
            EventLabel::TurnStart(AgentId::Sub(1)),
            // First inference: the model returned a tool_use, so a
            // `MessagePersisted::Assistant` event fires (the
            // listener writes the assistant message to disk before
            // the emit resolves), `TurnUsage` reports the per-turn
            // token counts, and the tool-execution lifecycle runs
            // against the synthesized `ping` tool.
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
            // After the tool batch finishes, the agent emits one
            // `MessagePersisted::ToolResult` event so the listener
            // appends the synthesized user-role tool_result message
            // (which the next inference will see).
            EventLabel::MessagePersisted {
                agent_id: AgentId::Sub(1),
                kind: "ToolResult",
            },
            // Second inference: the model returned plain text, so
            // we get one more `MessagePersisted::Assistant` (the
            // text response itself), `TurnUsage` fires, and the
            // loop exits without another tool batch.
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
    async fn run_single_turn_brackets_with_agent_lifecycle() {
        // Drives `run_single_turn` (the public sub-agent entry point)
        // and verifies the bus brackets every run with an
        // `AgentStart` / `AgentEnd` pair tagged with the agent's id.
        let (_dir, log) = fresh_log();

        // run_single_turn anchors on a parent_head from the log; the
        // system_prompt entry (root) is reachable and works as the
        // parent for the sub-agent thread.
        let parent_head = log
            .lock()
            .await
            .system_prompt_id()
            .expect("set_system_prompt populates the root entry")
            .clone();

        let scripts = vec![vec![StreamingEvent::FinalizedMessage {
            message: finalize_text("ok"),
        }]];
        let model = Arc::new(ScriptedModel::new(scripts));

        let env = empty_env(std::env::temp_dir());
        let mut agent = Agent::new(
            env,
            NoopUi,
            "irrelevant",
            Vec::new(),
            Vec::new(),
            model,
            None,
        );
        agent.set_agent_id(AgentId::Sub(7));

        let _persistence_handle = agent.subscribe(persistence_listener(Arc::clone(&log)));

        let recorded: Arc<Mutex<Vec<EventLabel>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = recorded.clone();
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        agent
            .run_single_turn(Arc::clone(&log), parent_head, 7, "test prompt".to_string())
            .await
            .expect("run_single_turn");

        let events = recorded.lock().unwrap().clone();
        // The exact subset we lock: lifecycle markers, the turn
        // boundary, the assistant-message persistence event, and
        // the per-turn token-usage event. Everything else (no tool
        // calls, no errors) means there are no other events this run.
        assert_eq!(
            events,
            vec![
                EventLabel::AgentStart(AgentId::Sub(7)),
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
}
