use aj_ui::{AjUi, TokenUsage, UsageSummary};

use crate::cli_common::AjCliCommon;

/// A wrapper UI for sub-agents that prefixes output with "(sub agent N)" and
/// only shows headings for tool use.
#[derive(Clone)]
pub struct SubAgentCli {
    agent_number: usize,
    common: AjCliCommon,
}

impl SubAgentCli {
    pub fn new(agent_id: usize, skip_permissions: bool) -> Self {
        Self {
            agent_number: agent_id,
            common: AjCliCommon::new(
                Some(format!("(sub agent {agent_id})")),
                false, // Don't show full tool output, only headers
                false, // Don't enable streaming
                skip_permissions,
            ),
        }
    }
}

impl AjUi for SubAgentCli {
    fn display_notice(&mut self, notice: &str) {
        self.common.display_notice(notice);
    }

    fn display_warning(&mut self, warning: &str) {
        self.common.display_warning(warning);
    }

    fn display_error(&mut self, error: &str) {
        self.common.display_error(error);
    }

    fn get_user_input(&mut self) -> Option<String> {
        // Sub-agents cannot interact with user
        None
    }

    fn agent_text_start(&mut self, text: &str) {
        self.common.agent_text_start(text);
    }

    fn agent_text_update(&mut self, diff: &str) {
        self.common.agent_text_update(diff);
    }

    fn agent_text_stop(&mut self, text: &str) {
        self.common.agent_text_stop(text);
    }

    fn user_text_start(&mut self, text: &str) {
        self.common.user_text_start(text);
    }

    fn user_text_update(&mut self, diff: &str) {
        self.common.user_text_update(diff);
    }

    fn user_text_stop(&mut self, text: &str) {
        self.common.user_text_stop(text);
    }

    fn agent_thinking_start(&mut self, thinking: &str) {
        self.common.agent_thinking_start(thinking);
    }

    fn agent_thinking_update(&mut self, diff: &str) {
        self.common.agent_thinking_update(diff);
    }

    fn agent_thinking_stop(&mut self) {
        self.common.agent_thinking_stop();
    }

    fn display_tool_result(&mut self, tool_name: &str, input: &str, output: &str) {
        self.common.display_tool_result(tool_name, input, output);
    }

    fn display_tool_result_diff(
        &mut self,
        tool_name: &str,
        input: &str,
        before: &str,
        after: &str,
    ) {
        self.common
            .display_tool_result_diff(tool_name, input, before, after);
    }

    fn display_tool_error(&mut self, tool_name: &str, input: &str, error: &str) {
        self.common.display_tool_error(tool_name, input, error);
    }

    fn ask_permission(&mut self, message: &str) -> bool {
        self.common.ask_permission(message)
    }

    fn display_token_usage(&mut self, usage: &TokenUsage) {
        self.common.display_token_usage(usage);
    }

    fn display_token_usage_summary(&mut self, summary: &UsageSummary) {
        self.common.display_token_usage_summary(summary);
    }

    fn get_subagent_ui(&mut self, agent_number: usize) -> Box<dyn AjUi> {
        Box::new(SubAgentCli::new(
            agent_number,
            self.common.skip_permissions(),
        ))
    }

    fn shallow_clone(&mut self) -> Box<dyn AjUi> {
        Box::new(SubAgentCli::new(
            self.agent_number,
            self.common.skip_permissions(),
        ))
    }
}
