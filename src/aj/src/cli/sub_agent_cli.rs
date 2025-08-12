use aj_ui::{AjUi, TokenUsage, UsageSummary};

use crate::cli::common_cli::AjCliCommon;

/// A wrapper UI for sub-agents that prefixes output with "(sub agent N)" and
/// only shows headings for tool use.
#[derive(Clone)]
pub struct SubAgentCli {
    common: AjCliCommon,
}

impl SubAgentCli {
    pub fn new(agent_number: usize) -> Self {
        Self {
            common: AjCliCommon::new(
                Some(format!("(sub agent {agent_number})")),
                false, // Don't show full tool output, only headers
                false, // Don't enable streaming
            ),
        }
    }
}

impl AjUi for SubAgentCli {
    fn display_notice(&self, notice: &str) {
        self.common.display_notice(notice);
    }

    fn display_error(&self, error: &str) {
        self.common.display_error(error);
    }

    fn get_user_input(&self) -> Option<String> {
        // Sub-agents cannot interact with user
        None
    }

    fn agent_text_start(&self, text: &str) {
        self.common.agent_text_start(text);
    }

    fn agent_text_update(&self, diff: &str) {
        self.common.agent_text_update(diff);
    }

    fn agent_text_stop(&self, text: &str) {
        self.common.agent_text_stop(text);
    }

    fn user_text_start(&self, text: &str) {
        self.common.user_text_start(text);
    }

    fn user_text_update(&self, diff: &str) {
        self.common.user_text_update(diff);
    }

    fn user_text_stop(&self, text: &str) {
        self.common.user_text_stop(text);
    }

    fn agent_thinking_start(&self, thinking: &str) {
        self.common.agent_thinking_start(thinking);
    }

    fn agent_thinking_update(&self, diff: &str) {
        self.common.agent_thinking_update(diff);
    }

    fn agent_thinking_stop(&self) {
        self.common.agent_thinking_stop();
    }

    fn display_tool_result(&self, tool_name: &str, input: &str, output: &str) {
        self.common.display_tool_result(tool_name, input, output);
    }

    fn display_tool_result_diff(&self, tool_name: &str, input: &str, before: &str, after: &str) {
        self.common.display_tool_result_diff(tool_name, input, before, after);
    }

    fn display_tool_error(&self, tool_name: &str, input: &str, error: &str) {
        self.common.display_tool_error(tool_name, input, error);
    }

    fn ask_permission(&self, message: &str) -> bool {
        self.common.ask_permission(message)
    }

    fn display_token_usage(&self, usage: &TokenUsage) {
        self.common.display_token_usage(usage);
    }

    fn display_token_usage_summary(&self, summary: &UsageSummary) {
        self.common.display_token_usage_summary(summary);
    }

    fn get_subagent_ui(&self, agent_number: usize) -> impl AjUi {
        SubAgentCli::new(agent_number)
    }
}
