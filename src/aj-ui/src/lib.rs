use serde::{Deserialize, Serialize};

/// Ui abstraction for AJ. The agent and all tools must use this when displaying
/// information to the user or requesting input from the user.
pub trait AjUi: Send + Sync {
    fn display_notice(&self, notice: &str);
    fn display_error(&self, error: &str);

    fn get_user_input(&self) -> Option<String>;

    fn agent_text_start(&self, text: &str);
    fn agent_text_update(&self, diff: &str);
    fn agent_text_stop(&self, text: &str);

    fn agent_thinking_start(&self, thinking: &str);
    fn agent_thinking_update(&self, diff: &str);
    fn agent_thinking_stop(&self);

    fn display_tool_result(&self, tool_name: &str, input: &str, output: &str);
    fn display_tool_result_diff(&self, tool_name: &str, input: &str, before: &str, after: &str);
    fn display_tool_error(&self, tool_name: &str, input: &str, error: &str);

    fn ask_permission(&self, message: &str) -> bool;

    fn display_token_usage(&self, usage: &TokenUsage);

    fn display_token_usage_summary(&self, summary: &UsageSummary);

    fn get_subagent_ui(&self, agent_number: usize) -> impl AjUi;
}

/// Token usage information for display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub accumulated_input: u64,
    pub turn_input: u64,
    pub accumulated_output: u64,
    pub turn_output: u64,
    pub accumulated_cache_creation: u64,
    pub turn_cache_creation: u64,
    pub accumulated_cache_read: u64,
    pub turn_cache_read: u64,
}

/// Usage summary for display including main agent and sub-agents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub main_agent_usage: SubAgentUsage,
    pub sub_agent_usage: Vec<SubAgentUsage>,
    pub total_usage: SubAgentUsage,
}

/// Usage information for a single agent (main or sub-agent)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentUsage {
    pub agent_id: Option<usize>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}

/// Trait for asking permission from the user
pub trait AjUiAskPermission: Send + Sync {
    /// Ask the user for permission to perform an action
    fn ask_permission(&self, message: &str) -> bool;
}

/// Represents different types of output that tools can produce for the user
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserOutput {
    /// Display a notice message to the user
    Notice(String),
    /// Display an error message to the user
    Error(String),
    /// Display the result of a tool execution
    ToolResult {
        tool_name: String,
        input: String,
        output: String,
    },
    /// Display a diff showing before/after changes
    ToolResultDiff {
        tool_name: String,
        input: String,
        before: String,
        after: String,
    },
    /// Display a tool error
    ToolError {
        tool_name: String,
        input: String,
        error: String,
    },
    /// Display token usage information
    TokenUsage(TokenUsage),
    /// Display token usage summary
    TokenUsageSummary(UsageSummary),
}

/// Extension trait to process UserOutput with an AjUi implementation
pub trait ProcessUserOutput {
    /// Process a list of UserOutput items, displaying them to the user
    fn process_user_outputs(&self, outputs: &[UserOutput]);

    /// Process a single UserOutput item
    fn process_user_output(&self, output: &UserOutput);
}

impl<T: AjUi> ProcessUserOutput for T {
    fn process_user_outputs(&self, outputs: &[UserOutput]) {
        for output in outputs {
            self.process_user_output(output);
        }
    }

    fn process_user_output(&self, output: &UserOutput) {
        match output {
            UserOutput::Notice(msg) => {
                self.display_notice(msg);
            }
            UserOutput::Error(msg) => {
                self.display_error(msg);
            }
            UserOutput::ToolResult {
                tool_name,
                input,
                output,
            } => {
                self.display_tool_result(tool_name, input, output);
            }
            UserOutput::ToolResultDiff {
                tool_name,
                input,
                before,
                after,
            } => {
                self.display_tool_result_diff(tool_name, input, before, after);
            }
            UserOutput::ToolError {
                tool_name,
                input,
                error,
            } => {
                self.display_tool_error(tool_name, input, error);
            }
            UserOutput::TokenUsage(usage) => {
                self.display_token_usage(usage);
            }
            UserOutput::TokenUsageSummary(summary) => {
                self.display_token_usage_summary(summary);
            }
        }
    }
}

impl<T: AjUi> AjUiAskPermission for T {
    fn ask_permission(&self, message: &str) -> bool {
        self.ask_permission(message)
    }
}
