use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Ui abstraction for AJ. The agent and all tools must use this when displaying
/// information to the user or requesting input from the user.
pub trait AjUi: Send + Sync {
    fn display_notice(&self, notice: &str);
    fn display_error(&self, error: &str);

    fn get_user_input(&self) -> Option<String>;

    fn agent_text_start(&self, text: &str);
    fn agent_text_update(&self, diff: &str);
    fn agent_text_stop(&self, text: &str);

    fn user_text_start(&self, text: &str);
    fn user_text_update(&self, diff: &str);
    fn user_text_stop(&self, text: &str);

    fn agent_thinking_start(&self, thinking: &str);
    fn agent_thinking_update(&self, diff: &str);
    fn agent_thinking_stop(&self);

    fn display_tool_result(&self, tool_name: &str, input: &str, output: &str);
    fn display_tool_result_diff(&self, tool_name: &str, input: &str, before: &str, after: &str);
    fn display_tool_error(&self, tool_name: &str, input: &str, error: &str);

    fn ask_permission(&self, message: &str) -> bool;

    fn display_token_usage(&self, usage: &TokenUsage);

    fn display_token_usage_summary(&self, summary: &UsageSummary);

    fn get_subagent_ui(&self, agent_number: usize) -> Box<dyn AjUi>;
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

impl<T: AjUi> AjUiAskPermission for T {
    fn ask_permission(&self, message: &str) -> bool {
        self.ask_permission(message)
    }
}

impl AjUi for Box<dyn AjUi> {
    fn agent_text_start(&self, text: &str) {
        self.as_ref().agent_text_start(text);
    }

    fn agent_text_update(&self, diff: &str) {
        self.as_ref().agent_text_update(diff);
    }

    fn agent_text_stop(&self, text: &str) {
        self.as_ref().agent_text_stop(text);
    }

    fn user_text_start(&self, text: &str) {
        self.as_ref().user_text_start(text);
    }

    fn user_text_update(&self, diff: &str) {
        self.as_ref().user_text_update(diff);
    }

    fn user_text_stop(&self, text: &str) {
        self.as_ref().user_text_stop(text);
    }

    fn agent_thinking_start(&self, thinking: &str) {
        self.as_ref().agent_thinking_start(thinking);
    }

    fn agent_thinking_update(&self, diff: &str) {
        self.as_ref().agent_thinking_update(diff);
    }

    fn agent_thinking_stop(&self) {
        self.as_ref().agent_thinking_stop();
    }

    fn display_notice(&self, notice: &str) {
        self.as_ref().display_notice(notice);
    }

    fn display_error(&self, error: &str) {
        self.as_ref().display_error(error);
    }

    fn ask_permission(&self, message: &str) -> bool {
        self.as_ref().ask_permission(message)
    }

    fn display_tool_result(&self, tool_name: &str, input: &str, output: &str) {
        self.as_ref().display_tool_result(tool_name, input, output);
    }

    fn display_tool_result_diff(&self, tool_name: &str, input: &str, before: &str, after: &str) {
        self.as_ref()
            .display_tool_result_diff(tool_name, input, before, after);
    }

    fn display_tool_error(&self, tool_name: &str, input: &str, error: &str) {
        self.as_ref().display_tool_error(tool_name, input, error);
    }

    fn display_token_usage(&self, usage: &TokenUsage) {
        self.as_ref().display_token_usage(usage);
    }

    fn display_token_usage_summary(&self, summary: &UsageSummary) {
        self.as_ref().display_token_usage_summary(summary);
    }

    fn get_subagent_ui(&self, agent_number: usize) -> Box<dyn AjUi> {
        self.as_ref().get_subagent_ui(agent_number)
    }

    fn get_user_input(&self) -> Option<String> {
        self.as_ref().get_user_input()
    }
}

/// A wrapper around AjUi that records all UserOutput for later retrieval
pub struct RecordingAjUi<'a> {
    inner: &'a dyn AjUi,
    // We need interior mutability because AjUi methods don't take a mut ref.
    // And we need it to be an Arc/Mutex becaus the ui needs to be sync because
    // we're using async.
    recorded_outputs: Arc<Mutex<Vec<UserOutput>>>,
}

impl<'a> RecordingAjUi<'a> {
    pub fn new(inner: &'a dyn AjUi) -> Self {
        Self {
            inner,
            recorded_outputs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn take_recorded_outputs(&self) -> Vec<UserOutput> {
        let mut outputs = self.recorded_outputs.lock().unwrap();
        std::mem::take(&mut *outputs)
    }

    fn record_output(&self, output: UserOutput) {
        self.recorded_outputs.lock().unwrap().push(output);
    }
}

impl<'a> AjUi for RecordingAjUi<'a> {
    fn display_notice(&self, notice: &str) {
        self.inner.display_notice(notice);
        self.record_output(UserOutput::Notice(notice.to_string()));
    }

    fn display_error(&self, error: &str) {
        self.inner.display_error(error);
        self.record_output(UserOutput::Error(error.to_string()));
    }

    fn get_user_input(&self) -> Option<String> {
        self.inner.get_user_input()
    }

    fn agent_text_start(&self, text: &str) {
        self.inner.agent_text_start(text);
    }

    fn agent_text_update(&self, diff: &str) {
        self.inner.agent_text_update(diff);
    }

    fn agent_text_stop(&self, text: &str) {
        self.inner.agent_text_stop(text);
    }

    fn user_text_start(&self, text: &str) {
        self.inner.user_text_start(text);
    }

    fn user_text_update(&self, diff: &str) {
        self.inner.user_text_update(diff);
    }

    fn user_text_stop(&self, text: &str) {
        self.inner.user_text_stop(text);
    }

    fn agent_thinking_start(&self, thinking: &str) {
        self.inner.agent_thinking_start(thinking);
    }

    fn agent_thinking_update(&self, diff: &str) {
        self.inner.agent_thinking_update(diff);
    }

    fn agent_thinking_stop(&self) {
        self.inner.agent_thinking_stop();
    }

    fn display_tool_result(&self, tool_name: &str, input: &str, output: &str) {
        self.inner.display_tool_result(tool_name, input, output);
        self.record_output(UserOutput::ToolResult {
            tool_name: tool_name.to_string(),
            input: input.to_string(),
            output: output.to_string(),
        });
    }

    fn display_tool_result_diff(&self, tool_name: &str, input: &str, before: &str, after: &str) {
        self.inner
            .display_tool_result_diff(tool_name, input, before, after);
        self.record_output(UserOutput::ToolResultDiff {
            tool_name: tool_name.to_string(),
            input: input.to_string(),
            before: before.to_string(),
            after: after.to_string(),
        });
    }

    fn display_tool_error(&self, tool_name: &str, input: &str, error: &str) {
        self.inner.display_tool_error(tool_name, input, error);
        self.record_output(UserOutput::ToolError {
            tool_name: tool_name.to_string(),
            input: input.to_string(),
            error: error.to_string(),
        });
    }

    fn ask_permission(&self, message: &str) -> bool {
        self.inner.ask_permission(message)
    }

    fn display_token_usage(&self, usage: &TokenUsage) {
        self.inner.display_token_usage(usage);
        self.record_output(UserOutput::TokenUsage(usage.clone()));
    }

    fn display_token_usage_summary(&self, summary: &UsageSummary) {
        self.inner.display_token_usage_summary(summary);
        self.record_output(UserOutput::TokenUsageSummary(summary.clone()));
    }

    fn get_subagent_ui(&self, agent_number: usize) -> Box<dyn AjUi> {
        // For sub-agents, we don't need recording as they handle their own output
        self.inner.get_subagent_ui(agent_number)
    }
}
