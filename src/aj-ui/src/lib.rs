use serde::{Deserialize, Serialize};

/// Ui abstraction for AJ. The agent and all tools must use this when displaying
/// information to the user or requesting input from the user.
pub trait AjUi: Send + Sync {
    fn display_notice(&mut self, notice: &str);
    fn display_error(&mut self, error: &str);

    fn get_user_input(&mut self) -> Option<String>;

    fn agent_text_start(&mut self, text: &str);
    fn agent_text_update(&mut self, diff: &str);
    fn agent_text_stop(&mut self, text: &str);

    fn user_text_start(&mut self, text: &str);
    fn user_text_update(&mut self, diff: &str);
    fn user_text_stop(&mut self, text: &str);

    fn agent_thinking_start(&mut self, thinking: &str);
    fn agent_thinking_update(&mut self, diff: &str);
    fn agent_thinking_stop(&mut self);

    fn display_tool_result(&mut self, tool_name: &str, input: &str, output: &str);
    fn display_tool_result_diff(&mut self, tool_name: &str, input: &str, before: &str, after: &str);
    fn display_tool_error(&mut self, tool_name: &str, input: &str, error: &str);

    fn ask_permission(&mut self, message: &str) -> bool;

    fn display_token_usage(&mut self, usage: &TokenUsage);

    fn display_token_usage_summary(&mut self, summary: &UsageSummary);

    fn get_subagent_ui(&mut self, agent_number: usize) -> Box<dyn AjUi>;

    fn shallow_clone(&mut self) -> Box<dyn AjUi>;
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
    fn ask_permission(&mut self, message: &str) -> bool;
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

impl<T: AjUi> AjUiAskPermission for T {
    fn ask_permission(&mut self, message: &str) -> bool {
        self.ask_permission(message)
    }
}

impl AjUi for Box<dyn AjUi> {
    fn agent_text_start(&mut self, text: &str) {
        self.as_mut().agent_text_start(text);
    }

    fn agent_text_update(&mut self, diff: &str) {
        self.as_mut().agent_text_update(diff);
    }

    fn agent_text_stop(&mut self, text: &str) {
        self.as_mut().agent_text_stop(text);
    }

    fn user_text_start(&mut self, text: &str) {
        self.as_mut().user_text_start(text);
    }

    fn user_text_update(&mut self, diff: &str) {
        self.as_mut().user_text_update(diff);
    }

    fn user_text_stop(&mut self, text: &str) {
        self.as_mut().user_text_stop(text);
    }

    fn agent_thinking_start(&mut self, thinking: &str) {
        self.as_mut().agent_thinking_start(thinking);
    }

    fn agent_thinking_update(&mut self, diff: &str) {
        self.as_mut().agent_thinking_update(diff);
    }

    fn agent_thinking_stop(&mut self) {
        self.as_mut().agent_thinking_stop();
    }

    fn display_notice(&mut self, notice: &str) {
        self.as_mut().display_notice(notice);
    }

    fn display_error(&mut self, error: &str) {
        self.as_mut().display_error(error);
    }

    fn ask_permission(&mut self, message: &str) -> bool {
        self.as_mut().ask_permission(message)
    }

    fn display_tool_result(&mut self, tool_name: &str, input: &str, output: &str) {
        self.as_mut().display_tool_result(tool_name, input, output);
    }

    fn display_tool_result_diff(
        &mut self,
        tool_name: &str,
        input: &str,
        before: &str,
        after: &str,
    ) {
        self.as_mut()
            .display_tool_result_diff(tool_name, input, before, after);
    }

    fn display_tool_error(&mut self, tool_name: &str, input: &str, error: &str) {
        self.as_mut().display_tool_error(tool_name, input, error);
    }

    fn display_token_usage(&mut self, usage: &TokenUsage) {
        self.as_mut().display_token_usage(usage);
    }

    fn display_token_usage_summary(&mut self, summary: &UsageSummary) {
        self.as_mut().display_token_usage_summary(summary);
    }

    fn get_subagent_ui(&mut self, agent_number: usize) -> Box<dyn AjUi> {
        self.as_mut().get_subagent_ui(agent_number)
    }

    fn shallow_clone(&mut self) -> Box<dyn AjUi> {
        self.as_mut().shallow_clone()
    }

    fn get_user_input(&mut self) -> Option<String> {
        self.as_mut().get_user_input()
    }
}
