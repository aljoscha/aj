// New event-driven contract modules. These define the surface that
// `aj-next` and the upcoming `aj-session` crate consume; the legacy
// agent runtime below keeps using the older `aj-ui` types and the
// legacy tool trait (now re-homed in `crate::legacy_tool`) until the
// bus migration in §2.1 of the aj-next plan.
pub mod bus;
pub mod events;
pub mod legacy_tool;
pub mod message;
pub mod tool;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::pin::{pin, Pin};
use std::time::Duration;

use aj_conf::{display_path, AgentEnv, ConfigThinkingLevel};
use aj_models::messages::{ApiError, ContentBlock, ContentBlockParam, Message, Role, Usage};
use aj_models::streaming::StreamingEvent;
use aj_models::tools::Tool;
use aj_models::ModelError;
use aj_models::{Model, ThinkingConfig};
use aj_session::{
    Conversation, ConversationEntryKind, ConversationError, ConversationLog, ConversationView,
    EntryId, ThreadFilter, ThreadKind,
};
use aj_ui::{AjUi, SubAgentUsage, TokenUsage, UsageSummary, UserOutput};

use crate::bus::{EventBus, Listener, SubscriptionHandle};
use crate::events::{AgentEvent, AgentId};
use crate::legacy_tool::{
    ErasedToolDefinition, SessionContext, ToolResult, TurnContext as ToolTurnContext,
};
use crate::tool::{SpawnedAgent, TodoItem, ToolDetails};
use anyhow::anyhow;
use futures::{Stream, StreamExt};
use std::sync::Arc;
use tokio_retry2::strategy::{jitter, ExponentialBackoff};

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

    pub async fn run(&mut self, log: &mut ConversationLog) -> Result<(), anyhow::Error> {
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
    async fn run_inner(&mut self, log: &mut ConversationLog) -> Result<(), anyhow::Error> {
        // Resolve the system prompt up front: either reuse the one
        // persisted on the log (cache-friendly resume) or assemble a
        // fresh one from the current environment and persist it as the
        // log's root entry. After this returns, every subsequent
        // append from any thread (including subagents) will anchor to
        // the system-prompt entry, and inference reuses
        // `self.assembled_system_prompt` verbatim.
        self.resolve_system_prompt(log)?;

        // Seed the subagent counter from the log so ids minted in this
        // session don't collide with subagent subtrees already persisted
        // from a prior session.
        if let Some(max_id) = log.max_agent_id() {
            self.session_state.seed_sub_agent_counter(max_id);
        }

        if let Some(head) = log.latest_leaf(ThreadFilter::USER) {
            let conversation = log.linearize(&head, ThreadFilter::USER);
            self.display_conversation_history(&conversation);

            // Because the log now writes to disk after every event,
            // resuming can
            // land us on a state where the last assistant message carries
            // `tool_use` blocks that never got their matching
            // `tool_result`s (process was killed between the two). Sending
            // that to the model would fail, so synthesize "interrupted"
            // tool_results for any dangling tool_use ids before we carry
            // on.
            repair_interrupted_tool_uses(log, &conversation)?;

            self.notice(format!(
                "Resuming conversation {} (use 'ctrl-c' or 'ctrl-d' to quit)",
                log.thread_id()
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
            let head = log.latest_leaf(ThreadFilter::USER);
            let need_user_input = force_user_input
                || match &head {
                    Some(id) => {
                        let conversation = log.linearize(id, ThreadFilter::USER);
                        match conversation.last_message() {
                            Some(last) => matches!(last.role, Role::Assistant),
                            None => true,
                        }
                    }
                    None => true,
                };
            force_user_input = false;

            if need_user_input {
                let user_input = self.ui.get_user_input();
                let user_input = if let Some(user_input) = user_input {
                    user_input
                } else {
                    self.display_usage_summary();
                    // Show the resume hint only if the user has actually
                    // sent at least one message so far.
                    if head.is_some() {
                        let id = log.thread_id();
                        self.notice(format!("Thread: {id} (resume with: aj continue {id})"))
                            .await?;
                    }
                    break;
                };
                let mut view = ConversationView::user(log, head);
                view.add_user_message(vec![ContentBlockParam::new_text_block(user_input)])?;
            }

            match self.execute_turn(log, ThreadKind::User, None).await {
                Ok(()) => {}
                Err(TurnError::Recoverable(err)) => {
                    self.error(format!("{err:#}")).await?;
                    // The pending user message is still on disk. Force a
                    // prompt next iteration so we don't immediately re-send
                    // the same broken request to the model. The user can
                    // type a follow-up (which will be appended to the
                    // conversation) or hit Ctrl-C/D to quit.
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
        log: &mut ConversationLog,
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
        log: &mut ConversationLog,
        parent_head: EntryId,
        agent_id: usize,
        prompt: String,
    ) -> Result<String, anyhow::Error> {
        // Resolve the system prompt before any inference. For sub-agents
        // the parent has already populated the log's SystemPrompt entry,
        // so this just reads it back; the assembled prompt is shared
        // across the whole session.
        self.resolve_system_prompt(log)?;

        {
            let mut view = ConversationView::subagent(log, parent_head, agent_id);
            view.add_user_message(vec![ContentBlockParam::new_text_block(prompt)])?;
        }

        self.execute_turn(log, ThreadKind::Subagent, Some(agent_id))
            .await?;

        // Extract the last assistant message text from the subagent's
        // own linearized history.
        let head = log
            .latest_leaf(ThreadFilter::subagent(agent_id))
            .ok_or_else(|| anyhow!("subagent produced no entries"))?;
        let conversation = log.linearize(&head, ThreadFilter::subagent(agent_id));
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
    /// The entries for this turn (assistant message, per-tool user outputs,
    /// tool-result user message) are appended directly to `log` via
    /// [ConversationView] handles. Each append serializes and writes one
    /// JSONL line to disk before returning, so the on-disk state is never
    /// more than one event behind reality.
    async fn execute_turn(
        &mut self,
        log: &mut ConversationLog,
        thread: ThreadKind,
        agent_id: Option<usize>,
    ) -> Result<(), TurnError> {
        self.session_state.turn_counter += 1;
        let mut turn_ctx = TurnContext::new(self.session_state.turn_counter);

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
            let head = log
                .latest_leaf(filter)
                .ok_or_else(|| anyhow!("execute_turn called on an empty thread"))?;
            let conversation = log.linearize(&head, filter);
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
                                self.ui.display_error(&message);
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
                            self.ui.agent_text_start(&text);
                        }
                        StreamingEvent::TextUpdate { diff, snapshot: _ } => {
                            self.ui.agent_text_update(&diff);
                        }
                        StreamingEvent::TextStop { text } => {
                            self.ui.agent_text_stop(&text);
                        }
                        StreamingEvent::ThinkingStart { thinking } => {
                            self.ui.agent_thinking_start(&thinking);
                        }
                        StreamingEvent::ThinkingUpdate { diff, snapshot: _ } => {
                            self.ui.agent_thinking_update(&diff);
                        }
                        StreamingEvent::ThinkingStop => {
                            self.ui.agent_thinking_stop();
                        }
                        StreamingEvent::ParseError { error, raw_data } => {
                            let message = format!("Parse error: {error} (raw data: {raw_data})");
                            self.ui.display_error(&message);
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
                            self.ui.display_error(&message);
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
                            self.ui.display_error(&message);
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

            // Append the assistant's message to the log. This write hits
            // disk before we touch any tool; anchor_head is the id we'll
            // use as the parent for subagents spawned while handling the
            // tool_use blocks below.
            let message_param = response.into_message_param();
            let assistant_head = {
                let mut view = make_view(log, head, thread, agent_id);
                view.add_assistant_message(message_param.content)?
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
            self.ui.display_token_usage(&usage);

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

                    // For tool_use blocks resurrected from a parse
                    // failure, skip execution and feed the parse error
                    // back as the tool_result.
                    let (tool_result, is_error) =
                        if let Some(parse_err) = tool_use_parse_failures.remove(&tool_id) {
                            let user_error_output = UserOutput::ToolError {
                                tool_name: tool_name.clone(),
                                input: "<malformed json>".to_string(),
                                error: parse_err.clone(),
                            };
                            let tool_result = ToolResult {
                                return_value: format!("Tool input parse error: {parse_err}"),
                                user_outputs: vec![user_error_output],
                                details: None,
                                is_error: true,
                            };
                            (tool_result, true)
                        } else {
                            let tool_result = self
                                .execute_tool(
                                    log,
                                    assistant_head.clone(),
                                    &mut turn_ctx,
                                    &tool_id,
                                    &tool_name,
                                    tool_input.clone(),
                                )
                                .await;

                            match tool_result {
                                Ok(result) => {
                                    let is_error = result.is_error;
                                    (result, is_error)
                                }
                                Err(err) => {
                                    let user_error_output = UserOutput::ToolError {
                                        tool_name: tool_name.clone(),
                                        input: tool_input.to_string(),
                                        error: err.to_string(),
                                    };
                                    let tool_result = ToolResult {
                                        return_value: format!("{err}"),
                                        user_outputs: vec![user_error_output],
                                        details: None,
                                        is_error: true,
                                    };
                                    (tool_result, true)
                                }
                            }
                        };

                    let result_content_block = ContentBlockParam::ToolResultBlock {
                        tool_use_id: tool_id.to_owned(),
                        content: tool_result.return_value.clone().into(),
                        is_error,
                    };

                    tool_result_contents.push(result_content_block);

                    // Persist each user output as its own log entry, anchored
                    // at the current leaf of this thread (which may now
                    // include earlier subagent/tool events from this
                    // iteration).
                    {
                        let current_head = log
                            .latest_leaf(filter)
                            .expect("we just appended an assistant message");
                        let mut view = make_view(log, current_head, thread, agent_id);
                        for out in tool_result.user_outputs.iter() {
                            view.add_user_output(out.clone())?;
                        }
                    }
                    self.display_user_output(&tool_result.user_outputs);

                    // Mirror the tool result on the bus. Migrated
                    // tools surface their own [ToolDetails] through
                    // [ToolResult::details]; legacy tools fall back to
                    // the [tool_details_for_legacy] projection of the
                    // textual `return_value`. Once §2.2 of
                    // `docs/aj-next-plan.md` finishes per-tool
                    // migration, the fallback (and `tool_details_for_legacy`)
                    // can go away.
                    let bus_details = tool_result.details.clone().unwrap_or_else(|| {
                        tool_details_for_legacy(&tool_name, &tool_result.return_value, is_error)
                    });
                    self.bus
                        .emit(AgentEvent::ToolExecutionEnd {
                            agent_id: self.agent_id,
                            call_id: tool_id.clone(),
                            tool: tool_name.clone(),
                            result: bus_details,
                            is_error,
                        })
                        .await
                        .map_err(TurnError::Fatal)?;
                }

                if !tool_result_contents.is_empty() {
                    let current_head = log
                        .latest_leaf(filter)
                        .expect("we just appended at least an assistant message");
                    let mut view = make_view(log, current_head, thread, agent_id);
                    view.add_user_message(tool_result_contents)?;
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
        log: &mut ConversationLog,
        parent_head: EntryId,
        turn_ctx: &mut dyn ToolTurnContext,
        _tool_id: &str,
        tool_name: &str,
        tool_input: serde_json::Value,
    ) -> Result<ToolResult, anyhow::Error> {
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

        // Create a wrapper that provides UI access to the session state
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
        };

        // Create recording wrapper to capture UI output
        let mut recording_ui = RecordingAjUi::new(&mut self.ui);

        let result = (tool_def.func)(
            &mut session_ctx_wrapper,
            turn_ctx,
            &mut recording_ui,
            tool_input,
        )
        .await?;

        // Extract recorded outputs and add them to the result
        // let recorded_outputs = recording_ui.take_recorded_outputs();
        // let result_with_outputs = ToolResult::with_outputs(result.return_value, recorded_outputs);
        // We rebuild the [ToolResult] with empty `user_outputs` (the
        // recorder is currently disabled) but preserve `details` and
        // `is_error` from migrated tools so the bus event in
        // [Agent::execute_turn] gets the rich payload.
        let result_with_outputs = ToolResult {
            return_value: result.return_value,
            user_outputs: Vec::new(),
            details: result.details,
            is_error: result.is_error,
        };

        Ok(result_with_outputs)
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
    /// sibling files.
    log: &'a mut ConversationLog,
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
    /// [SessionContext::spawn_agent] runs. The sub-agent itself owns
    /// a separate bus today; per `docs/aj-next-plan.md` §1.6 those
    /// will eventually unify, but the correlation events on the
    /// parent's bus are sufficient for listeners to know a sub-agent
    /// ran.
    parent_bus: EventBus,
    /// Identifier of the parent agent that owns this wrapper. The
    /// `parent` field of [AgentEvent::SubAgentStart] /
    /// [AgentEvent::SubAgentEnd].
    parent_agent_id: AgentId,
}

impl<'a, UI: AjUi> SessionContext for SessionContextWrapper<'a, UI> {
    fn working_directory(&self) -> PathBuf {
        self.session_ctx.working_directory()
    }

    fn get_todo_list(&self) -> Vec<TodoItem> {
        self.session_ctx.get_todo_list()
    }

    fn set_todo_list(&mut self, todos: Vec<TodoItem>) {
        self.session_ctx.set_todo_list(todos);
    }

    fn spawn_agent(
        &mut self,
        task: String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SpawnedAgent, anyhow::Error>> + Send + '_>,
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

            // Run the sub-agent with the task, anchored at the parent
            // tool-use's assistant message.
            let result = sub_agent
                .run_single_turn(self.log, self.parent_head.clone(), agent_id, task)
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
}

/// Build a [ConversationView] for the given thread identity, anchored at
/// `head`. Used by [Agent::execute_turn] to keep view construction
/// short-lived around each append so the underlying `&mut ConversationLog`
/// can be re-borrowed between events.
fn make_view<'a>(
    log: &'a mut ConversationLog,
    head: EntryId,
    thread: ThreadKind,
    agent_id: Option<usize>,
) -> ConversationView<'a> {
    match thread {
        ThreadKind::User => ConversationView::user(log, Some(head)),
        ThreadKind::Subagent => {
            let agent_id = agent_id.expect("subagent view requires an agent_id");
            ConversationView::subagent(log, head, agent_id)
        }
        // Meta entries are written by [ConversationLog::set_system_prompt],
        // never via a [ConversationView]. Reaching this arm indicates a
        // caller bug.
        ThreadKind::Meta => panic!("make_view called with ThreadKind::Meta"),
    }
}

/// Project a legacy [ToolResult] return value onto a [ToolDetails::Text]
/// for [AgentEvent::ToolExecutionEnd] emission.
///
/// Today every tool returns a `String` `return_value`; we wrap that in
/// the default rendering shape so listeners have a consistent payload
/// to render. Once §2.2 of `docs/aj-next-plan.md` migrates each tool
/// to [crate::tool::ToolOutcome], the per-tool implementation will
/// pick a richer variant (`Diff`, `Bash`, `Todos`, …) and this helper
/// disappears.
fn tool_details_for_legacy(tool_name: &str, return_value: &str, is_error: bool) -> ToolDetails {
    let summary = if is_error {
        format!("{tool_name}: error")
    } else {
        tool_name.to_string()
    };
    ToolDetails::Text {
        summary,
        body: return_value.to_string(),
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

#[derive(Debug, Clone)]
pub struct TurnContext {
    pub turn_id: usize,
}

impl TurnContext {
    pub fn new(turn_id: usize) -> Self {
        Self { turn_id }
    }
}

impl ToolTurnContext for TurnContext {}

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

/// A wrapper around AjUi that records all UserOutput for later retrieval
pub struct RecordingAjUi<'a> {
    inner: &'a mut dyn AjUi,
    recorded_outputs: Vec<UserOutput>,
}

impl<'a> RecordingAjUi<'a> {
    pub fn new(inner: &'a mut dyn AjUi) -> Self {
        Self {
            inner,
            recorded_outputs: Vec::new(),
        }
    }

    pub fn take_recorded_outputs(&mut self) -> Vec<UserOutput> {
        std::mem::take(&mut self.recorded_outputs)
    }

    fn record_output(&mut self, output: UserOutput) {
        self.recorded_outputs.push(output);
    }
}

impl<'a> AjUi for RecordingAjUi<'a> {
    fn display_notice(&mut self, notice: &str) {
        self.inner.display_notice(notice);
        self.record_output(UserOutput::Notice(notice.to_string()));
    }

    fn display_warning(&mut self, warning: &str) {
        self.inner.display_warning(warning);
    }

    fn display_error(&mut self, error: &str) {
        self.inner.display_error(error);
        self.record_output(UserOutput::Error(error.to_string()));
    }

    fn get_user_input(&mut self) -> Option<String> {
        self.inner.get_user_input()
    }

    fn agent_text_start(&mut self, text: &str) {
        self.inner.agent_text_start(text);
    }

    fn agent_text_update(&mut self, diff: &str) {
        self.inner.agent_text_update(diff);
    }

    fn agent_text_stop(&mut self, text: &str) {
        self.inner.agent_text_stop(text);
    }

    fn user_text_start(&mut self, text: &str) {
        self.inner.user_text_start(text);
    }

    fn user_text_update(&mut self, diff: &str) {
        self.inner.user_text_update(diff);
    }

    fn user_text_stop(&mut self, text: &str) {
        self.inner.user_text_stop(text);
    }

    fn agent_thinking_start(&mut self, thinking: &str) {
        self.inner.agent_thinking_start(thinking);
    }

    fn agent_thinking_update(&mut self, diff: &str) {
        self.inner.agent_thinking_update(diff);
    }

    fn agent_thinking_stop(&mut self) {
        self.inner.agent_thinking_stop();
    }

    fn display_tool_result(&mut self, tool_name: &str, input: &str, output: &str) {
        self.inner.display_tool_result(tool_name, input, output);
        self.record_output(UserOutput::ToolResult {
            tool_name: tool_name.to_string(),
            input: input.to_string(),
            output: output.to_string(),
        });
    }

    fn display_tool_result_diff(
        &mut self,
        tool_name: &str,
        input: &str,
        before: &str,
        after: &str,
    ) {
        self.inner
            .display_tool_result_diff(tool_name, input, before, after);
        self.record_output(UserOutput::ToolResultDiff {
            tool_name: tool_name.to_string(),
            input: input.to_string(),
            before: before.to_string(),
            after: after.to_string(),
        });
    }

    fn display_tool_error(&mut self, tool_name: &str, input: &str, error: &str) {
        self.inner.display_tool_error(tool_name, input, error);
        self.record_output(UserOutput::ToolError {
            tool_name: tool_name.to_string(),
            input: input.to_string(),
            error: error.to_string(),
        });
    }

    fn ask_permission(&mut self, message: &str) -> bool {
        self.inner.ask_permission(message)
    }

    fn display_token_usage(&mut self, usage: &TokenUsage) {
        self.inner.display_token_usage(usage);
        self.record_output(UserOutput::TokenUsage(usage.clone()));
    }

    fn display_token_usage_summary(&mut self, summary: &UsageSummary) {
        self.inner.display_token_usage_summary(summary);
        self.record_output(UserOutput::TokenUsageSummary(summary.clone()));
    }

    fn get_subagent_ui(&mut self, _agent_number: usize) -> Box<dyn AjUi> {
        // We could solve these by splitting them into it's own interface and
        // only giving what is allowed to tools. But we have larger seafood to
        // cook...
        panic!("tools are not allowed to spawn subagent ui");
    }

    fn shallow_clone(&mut self) -> Box<dyn AjUi> {
        // We could solve these by splitting them into it's own interface and
        // only giving what is allowed to tools. But we have larger seafood to
        // cook...
        panic!("tools are not allowed to clone ui");
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
    use tempfile::TempDir;

    use crate::bus::listener_from_sync;
    use crate::events::{AgentEvent, AgentId};
    use crate::legacy_tool::{
        ErasedToolDefinition, SessionContext, ToolDefinition as LegacyToolDefinition, ToolResult,
        TurnContext as LegacyTurnContext,
    };
    use crate::tool::ToolDetails;
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

    /// Trivial tool that returns a fixed string. Mirrors the legacy
    /// `ToolDefinition` trait so it can be passed through `Agent::new`
    /// alongside the existing builtins.
    #[derive(Clone)]
    struct PingTool;

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct PingInput {}

    impl LegacyToolDefinition for PingTool {
        type Input = PingInput;

        fn name(&self) -> &'static str {
            "ping"
        }

        fn description(&self) -> &'static str {
            "Test tool"
        }

        fn execute(
            &self,
            _session_ctx: &mut dyn SessionContext,
            _turn_ctx: &mut dyn LegacyTurnContext,
            _ui: &mut dyn AjUi,
            _input: PingInput,
        ) -> impl std::future::Future<Output = Result<ToolResult, anyhow::Error>> + Send {
            async move { Ok(ToolResult::new("pong".to_string())) }
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
            AgentEvent::TurnEnd { .. } => EventLabel::Other("TurnEnd"),
            AgentEvent::MessageStart { .. } => EventLabel::Other("MessageStart"),
            AgentEvent::MessageUpdate { .. } => EventLabel::Other("MessageUpdate"),
            AgentEvent::MessageEnd { .. } => EventLabel::Other("MessageEnd"),
            AgentEvent::ToolExecutionUpdate { .. } => EventLabel::Other("ToolExecutionUpdate"),
            AgentEvent::QueueUpdate { .. } => EventLabel::Other("QueueUpdate"),
        }
    }

    /// Set up a temp directory with an empty conversation log carrying
    /// a fixed system prompt.
    fn fresh_log() -> (TempDir, ConversationLog) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("threads"));
        let mut log = ConversationLog::create(&persistence).expect("ConversationLog::create");
        log.set_system_prompt("test system prompt".to_string())
            .expect("set_system_prompt on fresh log");
        (dir, log)
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
        let (_dir, mut log) = fresh_log();
        let parent_head = log
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
        let model = std::sync::Arc::new(ScriptedModel::new(scripts));

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

        let recorded: std::sync::Arc<Mutex<Vec<EventLabel>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = recorded.clone();
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        agent
            .run_single_turn(&mut log, parent_head, 1, "run ping".to_string())
            .await
            .expect("run_single_turn");

        let events = recorded.lock().unwrap().clone();
        let expected = vec![
            EventLabel::AgentStart(AgentId::Main),
            EventLabel::TurnStart(AgentId::Main),
            EventLabel::ToolExecutionStart {
                agent_id: AgentId::Main,
                call_id: "tu-1".to_string(),
                tool: "ping".to_string(),
            },
            EventLabel::ToolExecutionEnd {
                agent_id: AgentId::Main,
                call_id: "tu-1".to_string(),
                tool: "ping".to_string(),
                summary: "ping".to_string(),
                body: "pong".to_string(),
                is_error: false,
            },
            EventLabel::AgentEnd(AgentId::Main),
        ];
        assert_eq!(events, expected, "unexpected event sequence: {events:#?}");
    }

    #[tokio::test]
    async fn run_single_turn_brackets_with_agent_lifecycle() {
        // Drives `run_single_turn` (the public sub-agent entry point)
        // and verifies the bus brackets every run with an
        // `AgentStart` / `AgentEnd` pair tagged with the agent's id.
        let (_dir, mut log) = fresh_log();

        // run_single_turn anchors on a parent_head from the log; the
        // system_prompt entry (root) is reachable and works as the
        // parent for the sub-agent thread.
        let parent_head = log
            .system_prompt_id()
            .expect("set_system_prompt populates the root entry")
            .clone();

        let scripts = vec![vec![StreamingEvent::FinalizedMessage {
            message: finalize_text("ok"),
        }]];
        let model = std::sync::Arc::new(ScriptedModel::new(scripts));

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

        let recorded: std::sync::Arc<Mutex<Vec<EventLabel>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let recorded_clone = recorded.clone();
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            recorded_clone.lock().unwrap().push(label(event));
        }));

        agent
            .run_single_turn(&mut log, parent_head, 7, "test prompt".to_string())
            .await
            .expect("run_single_turn");

        let events = recorded.lock().unwrap().clone();
        // The exact subset we lock: lifecycle markers and the turn
        // boundary. Everything else (no tool calls, no errors) means
        // there are no other events this run.
        assert_eq!(
            events,
            vec![
                EventLabel::AgentStart(AgentId::Sub(7)),
                EventLabel::TurnStart(AgentId::Sub(7)),
                EventLabel::AgentEnd(AgentId::Sub(7)),
            ],
            "unexpected event sequence: {events:#?}"
        );
    }
}
