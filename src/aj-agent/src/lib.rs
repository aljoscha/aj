use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::pin;
use std::time::SystemTime;

use anthropic_sdk::streaming::StreamingEvent;
use anyhow::anyhow;
use console::{style, Color};
use futures::{Stream, StreamExt};
use similar::{ChangeTag, TextDiff};

use aj_conf::AgentEnv;
use aj_tools::{
    ErasedToolDefinition, SessionState as ToolSessionState, TurnState as ToolTurnState,
};
use anthropic_sdk::messages::{
    ContentBlock, ContentBlockParam, Message, MessageParam, Messages, Role, Tool,
};

pub struct Agent<U: GetUserMessage> {
    env: AgentEnv,
    system_prompt: &'static str,
    get_user_message: U,
    tool_definitions: HashMap<String, ErasedToolDefinition>,
    tools: Vec<Tool>,
    client: anthropic_sdk::client::Client,
    session_state: SessionState,
    turn_counter: usize,
}

impl<U: GetUserMessage> Agent<U> {
    pub fn new(
        env: AgentEnv,
        system_prompt: &'static str,
        tools: Vec<ErasedToolDefinition>,
        get_user_message: U,
    ) -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
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
            get_user_message,
            tool_definitions,
            tools: api_tools,
            client,
            session_state: SessionState::new(env.working_directory.clone()),
            env,
            turn_counter: 0,
        }
    }

    pub fn session_state(&self) -> &SessionState {
        &self.session_state
    }

    pub fn session_state_mut(&mut self) -> &mut SessionState {
        &mut self.session_state
    }

    pub fn current_turn(&self) -> usize {
        self.turn_counter
    }

    pub async fn run(&mut self) -> Result<(), anyhow::Error> {
        let mut conversation: Vec<MessageParam> = Vec::new();

        println!("Chat with AJ (use 'ctrl-c' to quit)");

        loop {
            self.turn_counter += 1;
            let mut turn_state = TurnState::new(self.turn_counter);

            let need_user_input = {
                match conversation.last() {
                    Some(last) => {
                        matches!(last.role, Role::Assistant)
                    }
                    None => true,
                }
            };
            if need_user_input {
                print!("{}: ", style("you").fg(Color::Blue));
                let user_input = self.get_user_message.get_user_message();
                let user_input = if let Some(user_input) = user_input {
                    user_input
                } else {
                    break;
                };
                let user_message =
                    MessageParam::new_user_message(vec![ContentBlockParam::new_text_block(
                        user_input,
                    )]);
                conversation.push(user_message);
            }

            let response_stream = self.run_inference_streaming(conversation.clone()).await?;

            let mut response: Option<Message> = None;
            {
                let mut response_stream = pin!(response_stream);
                while let Some(event) = response_stream.next().await {
                    match event {
                        StreamingEvent::MessageStart { message: _ } => (),
                        StreamingEvent::UsageUpdate { usage: _ } => (),
                        StreamingEvent::FinalizedMessage { message } => {
                            response = Some(message);
                        }
                        StreamingEvent::Error { error } => return Err(anyhow!("{}", error)),
                        StreamingEvent::TextStart { text } => {
                            print!("{}: {}", style("aj").fg(Color::Yellow), text);
                        }
                        StreamingEvent::TextUpdate { diff, snapshot: _ } => {
                            print!("{}", diff);
                        }
                        StreamingEvent::TextStop => {
                            println!()
                        }
                        StreamingEvent::ThinkingStart { thinking } => {
                            print!(
                                "{}: {}",
                                style("aj is thinking").fg(Color::Black).bright(),
                                style(thinking).fg(Color::Black).on_bright()
                            );
                        }
                        StreamingEvent::ThinkingUpdate { diff, snapshot: _ } => {
                            print!("{}", diff);
                        }
                        StreamingEvent::ThinkingStop => {
                            println!()
                        }
                        StreamingEvent::ParseError { error, raw_data } => {
                            eprintln!("Parse error: {} (raw data: {})", error, raw_data);
                        }
                        StreamingEvent::ProtocolError { error } => {
                            eprintln!("Protocol error: {}", error);
                        }
                    }
                }
            }

            let response = response.expect("missing message");

            // Collect tool use blocks from the response
            let mut tool_calls = Vec::new();
            let mut has_tool_use = false;

            for content in response.content.iter() {
                match content {
                    ContentBlock::ToolUseBlock { id, name, input } => {
                        tool_calls.push((id.clone(), name.clone(), input.clone()));
                        has_tool_use = true;
                        println!("{}: {}({})", style("tool").fg(Color::Green), name, input,);
                    }
                    _ => {}
                }
            }

            // Add the assistant's message to conversation
            conversation.push(response.into_message_param());

            // Execute tool calls if any
            if has_tool_use {
                let mut tool_result_contents = Vec::new();

                for (tool_id, tool_name, tool_input) in tool_calls {
                    let tool_result =
                        self.execute_tool(&tool_id, &tool_name, tool_input, &mut turn_state);

                    let (result_content, is_error) = match tool_result {
                        Ok(result) => (result, false),
                        Err(err) => {
                            println!("{}: {:?}", style("tool_error").fg(Color::Red), err);
                            (format!("{}", err), true)
                        }
                    };

                    let result_content_block = ContentBlockParam::ToolResultBlock {
                        tool_use_id: tool_id.to_owned(),
                        content: result_content,
                        is_error,
                        cache_control: None,
                    };

                    tool_result_contents.push(result_content_block);
                }

                if tool_result_contents.len() > 0 {
                    let tool_result_message = MessageParam::new_user_message(tool_result_contents);

                    conversation.push(tool_result_message);
                }

                // Continue the conversation loop to get the model's response to tool results
                continue;
            }
        }

        Ok(())
    }

    async fn run_inference_streaming(
        &self,
        conversation: Vec<MessageParam>,
    ) -> Result<impl Stream<Item = StreamingEvent> + use<'_, U>, anyhow::Error> {
        let messages = Messages {
            model: "claude-sonnet-4-20250514".to_string(),
            system: Some(self.assemble_system_prompt()),
            // thinking: Some(anthropic_sdk::messages::Thinking::Enabled {
            //     budget_tokens: 10_000,
            // }),
            max_tokens: 32_000,
            messages: conversation,
            tools: self.tools.clone(),
            ..Default::default()
        };
        let response = self.client.messages_stream(messages).await?;

        Ok(response)
    }

    /// Assemble the system prompt we pass to the model from the actual system
    /// prompt and additional information we might want or need, such as
    /// information about the environment.
    fn assemble_system_prompt(&self) -> String {
        format!(
            "{}\n\nHere's useful information about your environment:\n<env>\n{}\n</env>",
            self.system_prompt, self.env
        )
    }

    fn execute_tool(
        &mut self,
        _tool_id: &str,
        tool_name: &str,
        tool_input: serde_json::Value,
        turn_state: &mut dyn ToolTurnState,
    ) -> Result<String, anyhow::Error> {
        let tool_def = if let Some(tool_def) = self.tool_definitions.get(tool_name) {
            tool_def
        } else {
            return Err(anyhow!("tool not found!"));
        };

        let tool_result = (tool_def.func)(&mut self.session_state, turn_state, tool_input);
        tool_result
    }
}

/// Trait for getting the next message from the user, for passing to the model.
pub trait GetUserMessage {
    fn get_user_message(&self) -> Option<String>;
}

/// A [GetUserMessage] that reads user messages from stdin.
pub struct StdinUserMessage;

impl GetUserMessage for StdinUserMessage {
    fn get_user_message(&self) -> Option<String> {
        use std::io::{self, Write};

        io::stdout().flush().unwrap();

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) => None, // EOF (ctrl-d)
            Ok(_) => Some(input.trim().to_string()),
            Err(_) => None, // Error (ctrl-c or other)
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub working_directory: PathBuf,
    pub accessed_files: HashMap<PathBuf, SystemTime>,
}

impl SessionState {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            accessed_files: HashMap::new(),
        }
    }

    /// Display file contents to the user. For example, for displaying the
    /// results of a read_file tool call.
    ///
    /// Abbreviating of longer than 20 lines.
    pub fn display_file(&self, path: &str, contents: &str) {
        let lines: Vec<&str> = contents.lines().collect();

        println!("{}: {}", style("file").bold(), path);

        if lines.len() <= 20 {
            // Display all lines with line numbers
            for (i, line) in lines.iter().enumerate() {
                println!("{:5>}: {}", i + 1, line);
            }
        } else {
            // Display first 8 lines
            for (i, line) in lines.iter().take(8).enumerate() {
                println!("{:5>}: {}", i + 1, line);
            }

            // Show truncation indicator with count
            let truncated_lines = lines.len() - 16; // Total lines minus first 8 and last 8
            println!(
                "{}",
                style(format!("[... {} lines truncated ...]", truncated_lines)).dim()
            );

            // Display last 8 lines
            let start_line = lines.len() - 8;
            for (i, line) in lines.iter().skip(start_line).enumerate() {
                println!("{:5>}: {}", start_line + i + 1, line);
            }
        }
        println!(); // Add blank line after file display
    }

    /// Display a diff to the user. For example, for displaying the results of
    /// edit/write operations.
    pub fn display_file_modification(&self, path: &str, old_content: &str, new_content: &str) {
        println!("{}: {}", style("diff").bold(), path);

        let diff = TextDiff::from_lines(old_content, new_content);

        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };

            let styled_line = match change.tag() {
                ChangeTag::Delete => style(format!("{} {}", sign, change.value().trim_end()))
                    .bg(Color::Red)
                    .on_bright()
                    .black(),
                ChangeTag::Insert => style(format!("{} {}", sign, change.value().trim_end()))
                    .bg(Color::Green)
                    .on_bright()
                    .black(),
                ChangeTag::Equal => style(format!("{} {}", sign, change.value().trim_end())).dim(),
            };

            println!("{}", styled_line);
        }
        println!(); // Add blank line after diff
    }
}

impl ToolSessionState for SessionState {
    fn working_directory(&self) -> PathBuf {
        self.working_directory.to_owned()
    }

    fn record_file_access(&mut self, path: PathBuf) {
        self.accessed_files.insert(path, SystemTime::now());
    }

    fn get_file_access_time(&self, path: &PathBuf) -> Option<SystemTime> {
        self.accessed_files.get(path).copied()
    }

    fn display_file(&self, path: &str, contents: &str) {
        self.display_file(path, contents);
    }

    fn display_file_modification(&self, path: &str, old_content: &str, new_content: &str) {
        self.display_file_modification(path, old_content, new_content);
    }
}

#[derive(Debug, Clone)]
pub struct TurnState {
    pub turn_id: usize,
}

impl TurnState {
    pub fn new(turn_id: usize) -> Self {
        Self { turn_id }
    }
}

impl ToolTurnState for TurnState {}
