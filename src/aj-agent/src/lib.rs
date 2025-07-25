use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::pin;
use std::time::Duration;

use aj_conf::AgentEnv;
use aj_tools::tools::todo::TodoItem;
use aj_tools::{
    get_builtin_tools, ErasedToolDefinition, SessionContext, ToolResult,
    TurnContext as ToolTurnContext,
};
use aj_ui::{AjUi, SubAgentUsage, TokenUsage, UsageSummary, UserOutput};
use anthropic_sdk::client::ClientError;
use anthropic_sdk::messages::{
    CacheControl, ContentBlock, ContentBlockParam, Message, Messages, Role, Tool, Usage,
};
use anthropic_sdk::streaming::StreamingEvent;
use anyhow::anyhow;
use futures::{Stream, StreamExt};
use tokio_retry2::strategy::{jitter, ExponentialBackoff};

pub mod conversation;

use conversation::Conversation;

pub struct Agent<UI: AjUi> {
    env: AgentEnv,
    system_prompt: &'static str,
    ui: UI,
    tool_definitions: HashMap<String, ErasedToolDefinition>,
    tools: Vec<Tool>,
    client: anthropic_sdk::client::Client,
    session_state: SessionState,
}

impl<UI: AjUi> Agent<UI> {
    pub fn new(
        env: AgentEnv,
        system_prompt: &'static str,
        tools: Vec<ErasedToolDefinition>,
        ui: UI,
    ) -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").expect(
            "need ANTHROPIC_API_KEY in environment, maybe you forget to set up a .env file",
        );
        let client = anthropic_sdk::client::Client::new(api_key.clone());

        // Convert ErasedToolDefinition to Tool for Anthropic API
        let api_tools: Vec<Tool> = tools
            .iter()
            .map(|tool_def| Tool {
                name: tool_def.name.clone(),
                description: tool_def.description.clone(),
                input_schema: tool_def.input_schema.clone(),
                r#type: None,
                cache_control: None,
            })
            .collect();

        // Convert ErasedToolDefinition to HashMap for lookup
        let tool_definitions: HashMap<String, ErasedToolDefinition> = tools
            .into_iter()
            .map(|tool_def| (tool_def.name.clone(), tool_def))
            .collect();

        Self {
            system_prompt,
            tool_definitions,
            tools: api_tools,
            client,
            session_state: SessionState::new(env.working_directory.clone()),
            env,
            ui,
        }
    }

    pub fn current_turn(&self) -> usize {
        self.session_state.turn_counter()
    }

    pub fn accumulated_usage(&self) -> &Usage {
        self.session_state.accumulated_usage()
    }

    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        let mut conversation = Conversation::new();

        self.ui
            .display_notice("Chat with AJ (use 'ctrl-c' or 'ctrl-d' to quit)");

        loop {
            let need_user_input = {
                match conversation.last_message() {
                    Some(last) => {
                        matches!(last.role, Role::Assistant)
                    }
                    None => true,
                }
            };
            if need_user_input {
                let user_input = self.ui.get_user_input();
                let user_input = if let Some(user_input) = user_input {
                    user_input
                } else {
                    self.display_usage_summary();
                    break;
                };
                conversation.add_user_message(vec![ContentBlockParam::new_text_block(user_input)]);
            }

            self.execute_turn(&mut conversation).await?;
        }

        Ok(())
    }

    pub async fn run_single_turn(&mut self, prompt: String) -> Result<String, anyhow::Error> {
        let mut conversation = Conversation::new();
        conversation.add_user_message(vec![ContentBlockParam::new_text_block(prompt)]);

        let mut last_assistant_text = String::new();

        self.execute_turn(&mut conversation).await?;

        // Extract the last assistant message text
        if let Some(last_msg) = conversation.last_message() {
            if matches!(last_msg.role, Role::Assistant) {
                last_assistant_text.clear();
                for content in &last_msg.content {
                    if let ContentBlockParam::TextBlock { text, .. } = content {
                        last_assistant_text.push_str(text);
                    }
                }
            } else {
                return Err(anyhow!("did not get a response from the model"));
            }
        }

        Ok(last_assistant_text)
    }

    /// Executes a single "turn" of the conversation, this will potentially
    /// include mutliple back-and-forth interactions with the model, in case
    /// there are thinking blocks or tool calls.
    async fn execute_turn(&mut self, conversation: &mut Conversation) -> Result<(), anyhow::Error> {
        self.session_state.turn_counter += 1;
        let mut turn_ctx = TurnContext::new(self.session_state.turn_counter);

        let mut retry_strategy = None;

        'outer: loop {
            let response_stream = self.run_inference_streaming(conversation).await?;

            let mut response: Option<Message> = None;
            let mut turn_usage_update = Usage::default();

            {
                let mut response_stream = pin!(response_stream);
                while let Some(event) = response_stream.next().await {
                    match event {
                        StreamingEvent::MessageStart { message } => {
                            turn_usage_update.add(&message.usage.into_usage_delta());
                        }
                        StreamingEvent::UsageUpdate { usage } => turn_usage_update.add(&usage),
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
                                self.ui.display_error(&format!(
                                    "{}, retrying in {}s...",
                                    error,
                                    retry_sleep.as_secs()
                                ));

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
                        StreamingEvent::ThinkingStart { thinking: _ } => {
                            self.ui.agent_thinking_start("...");
                        }
                        StreamingEvent::ThinkingUpdate {
                            diff: _,
                            snapshot: _,
                        } => {
                            // self.ui.agent_thinking_update(&diff);
                        }
                        StreamingEvent::ThinkingStop => {
                            self.ui.agent_thinking_stop();
                        }
                        StreamingEvent::ParseError { error, raw_data } => {
                            self.ui.display_error(&format!(
                                "Parse error: {error} (raw data: {raw_data})"
                            ));
                        }
                        StreamingEvent::ProtocolError { error } => {
                            self.ui.display_error(&format!("Protocol error: {error}"));
                        }
                    }

                    // We've successfully received an event, reset the retry
                    // strategy. That way, when we get an Overloaded error again
                    // we'll initialize with a fresh retry_strategy.
                    retry_strategy = None
                }
            }

            let response = response.expect("missing message");

            // Collect tool use blocks from the response
            let mut tool_calls = Vec::new();
            let mut has_tool_use = false;

            for content in response.content.iter() {
                if let ContentBlock::ToolUseBlock { id, name, input } = content {
                    tool_calls.push((id.clone(), name.clone(), input.clone()));
                    has_tool_use = true;
                }
            }

            // Add the assistant's message to conversation
            let message_param = response.into_message_param();
            conversation.add_assistant_message(message_param.content);

            let usage = TokenUsage {
                accumulated_input: self.session_state.accumulated_usage.input_tokens,
                turn_input: turn_usage_update.input_tokens,
                accumulated_output: self.session_state.accumulated_usage.output_tokens,
                turn_output: turn_usage_update.output_tokens,
                accumulated_cache_creation: self
                    .session_state
                    .accumulated_usage
                    .cache_creation_input_tokens
                    .unwrap_or(0),
                turn_cache_creation: turn_usage_update.cache_creation_input_tokens.unwrap_or(0),
                accumulated_cache_read: self
                    .session_state
                    .accumulated_usage
                    .cache_read_input_tokens
                    .unwrap_or(0),
                turn_cache_read: turn_usage_update.cache_read_input_tokens.unwrap_or(0),
            };
            self.ui.display_token_usage(&usage);

            self.session_state
                .accumulated_usage
                .add(&turn_usage_update.into_usage_delta());

            // Execute tool calls if any
            if has_tool_use {
                let mut tool_result_contents = Vec::new();

                for (tool_id, tool_name, tool_input) in tool_calls {
                    let tool_result = self
                        .execute_tool(&mut turn_ctx, &tool_id, &tool_name, tool_input.clone())
                        .await;

                    let (tool_result, is_error) = match tool_result {
                        Ok(result) => (result, false),
                        Err(err) => {
                            let user_error_output = UserOutput::ToolError {
                                tool_name: tool_name.clone(),
                                input: tool_input.to_string(),
                                error: err.to_string(),
                            };
                            let tool_result = ToolResult {
                                return_value: format!("{err}"),
                                user_outputs: vec![user_error_output],
                            };
                            (tool_result, true)
                        }
                    };

                    let result_content_block = ContentBlockParam::ToolResultBlock {
                        tool_use_id: tool_id.to_owned(),
                        content: tool_result.return_value,
                        is_error,
                        cache_control: None,
                    };

                    tool_result_contents.push(result_content_block);

                    Self::record_user_output(conversation, &tool_result.user_outputs);
                    self.display_user_output(&tool_result.user_outputs);
                }

                if !tool_result_contents.is_empty() {
                    conversation.add_user_message(tool_result_contents);
                }

                // Continue the conversation loop to get the model's response to tool results
                continue;
            } else {
                // We are now ready to finish this turn.
                break;
            }
        }

        // Save the conversation after completing the turn
        if let Err(e) = conversation.save_to_file() {
            tracing::warn!("Failed to save conversation: {e}");
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
    ) -> Result<impl Stream<Item = StreamingEvent> + '_, ClientError> {
        let mut messages = conversation.to_message_params();

        let last_user_message = messages
            .iter_mut()
            .filter(|m| matches!(m.role, Role::User))
            .next_back();

        if let Some(last_user_message) = last_user_message {
            let last_content = last_user_message.content.iter_mut().last();
            if let Some(last_content) = last_content {
                last_content.set_cache_control(CacheControl::Ephemeral { ttl: None });
            }
        }

        let last_assistant_message = messages
            .iter_mut()
            .filter(|m| matches!(m.role, Role::Assistant))
            .next_back();

        if let Some(last_assistant_message) = last_assistant_message {
            let last_content = last_assistant_message.content.iter_mut().last();
            if let Some(last_content) = last_content {
                last_content.set_cache_control(CacheControl::Ephemeral { ttl: None });
            }
        }

        let thinking = self.determine_thinking(conversation);
        tracing::info!(?thinking, "thinking");
        let messages = Messages {
            model: "claude-sonnet-4-20250514".to_string(),
            system: Some(self.assemble_system_prompt()),
            thinking,
            max_tokens: 32_000,
            messages,
            tools: self.tools.clone(),
            ..Default::default()
        };
        let response = self.client.messages_stream(messages).await?;

        Ok(response)
    }

    /// Determine the thinking configuration based on trigger texts in the user
    /// prompt and session state. Returns thinking configuration based on
    /// specific trigger phrases:
    /// - "think harder" -> 32,000 tokens
    /// - "think hard" -> 10,000 tokens
    /// - "think" -> 4,000 tokens
    /// - default -> None (no thinking)
    fn determine_thinking(
        &self,
        conversation: &Conversation,
    ) -> Option<anthropic_sdk::messages::Thinking> {
        // Get the last user message
        let messages = conversation.to_message_params();
        let last_user_message = messages
            .iter()
            .filter(|m| {
                let is_user = matches!(m.role, Role::User);
                if !is_user {
                    return false;
                }

                // Only sniff out messages that have actual user-input. The last
                // user input determines thinking, and so, for example, when
                // there is back-and-forth with tool results, we need to
                // maintain the thinking flag enabled.
                let is_user_input = m
                    .content
                    .iter()
                    .any(|c| matches!(c, ContentBlockParam::TextBlock { .. }));

                is_user_input
            })
            .next_back();

        let mut thinking_budget = None;

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

            // Check for trigger phrases in order of specificity
            if text_lower.contains("think harder") {
                thinking_budget = Some(32_000);
            } else if text_lower.contains("think hard") {
                thinking_budget = Some(10_000);
            } else if text_lower.contains("think") {
                thinking_budget = Some(4_000);
            }
        }

        // Return thinking configuration if we have a budget
        thinking_budget.map(|budget| anthropic_sdk::messages::Thinking::Enabled {
            budget_tokens: budget,
        })
    }

    /// Assemble the system prompt we pass to the model from the actual system
    /// prompt and additional information we might want or need, such as
    /// information about the environment.
    fn assemble_system_prompt(&self) -> Vec<ContentBlockParam> {
        let mut text = self.system_prompt.to_string();

        if let Some(agent_md_content) = &self.env.agent_md {
            text.push_str(&format!(
                "\n\n{}\n<agent-md>\n{}\n</agent-md>",
                aj_conf::AGENT_MD_PREFIX,
                agent_md_content
            ));
        }

        text.push_str(&format!(
            "\n\nHere's useful information about your environment:\n<env>\n{}\n</env>",
            self.env
        ));

        vec![ContentBlockParam::TextBlock {
            text,
            cache_control: Some(CacheControl::Ephemeral { ttl: None }),
            citations: None,
        }]
    }

    async fn execute_tool(
        &mut self,
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

        // Create a wrapper that provides UI access to the session state
        let mut session_ctx_wrapper = SessionContextWrapper {
            session_ctx: &mut self.session_state,
            ui: &self.ui,
            env: &self.env,
            system_prompt: self.system_prompt,
        };

        let result =
            (tool_def.func)(&mut session_ctx_wrapper, turn_ctx, &self.ui, tool_input).await?;

        Ok(result)
    }

    fn record_user_output(conversation: &mut Conversation, user_outputs: &[UserOutput]) {
        for output in user_outputs {
            conversation.add_user_output(output.clone());
        }
    }

    fn display_user_output(&self, user_outputs: &[UserOutput]) {
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

    fn display_usage_summary(&self) {
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
            accumulated_usage: Usage {
                cache_creation: None,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                input_tokens: 0,
                output_tokens: 0,
                server_tool_use: None,
                service_tier: None,
            },
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

    fn record_sub_agent_usage(&mut self, agent_id: usize, usage: Usage) {
        self.sub_agent_usage.insert(agent_id, usage);
    }
}

/// Wrapper that provides partial access to mutable [Agent] state, while we have
/// partial immutable access to other parts. Used in [Agent::execute_tool].
struct SessionContextWrapper<'a, UI: AjUi> {
    session_ctx: &'a mut SessionState,
    ui: &'a UI,
    env: &'a AgentEnv,
    system_prompt: &'static str,
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
        Box<dyn std::future::Future<Output = Result<String, anyhow::Error>> + Send + '_>,
    > {
        Box::pin(async move {
            // Get the next agent ID
            let agent_id = self.session_ctx.next_sub_agent_id();

            // Create a sub-agent UI wrapper with the agent number
            let sub_ui = self.ui.get_subagent_ui(agent_id);

            // Get tools excluding the agent tool to prevent infinite recursion
            let sub_agent_tools = get_builtin_tools()
                .into_iter()
                .filter(|tool| tool.name != "agent")
                .collect();

            // Create a new agent with the sub-agent UI
            let mut sub_agent = Agent::new(
                self.env.clone(),
                self.system_prompt,
                sub_agent_tools,
                sub_ui,
            );

            // Run the sub-agent with the task
            let result = sub_agent.run_single_turn(task).await;

            // Get the sub-agent's accumulated usage
            let sub_agent_usage = sub_agent.session_state.accumulated_usage.clone();

            // Record the usage in the main session state
            self.session_ctx
                .record_sub_agent_usage(agent_id, sub_agent_usage);

            result
        })
    }
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
