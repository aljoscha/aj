use aj_ui::{AjUi, TokenUsage, UsageSummary};
use console::{Color, style};

use crate::cli::{
    DARK_GRAY, LIGHT_GRAY, format_token_usage, format_usage_summary, render_markdown,
};

/// A wrapper UI for sub-agents that prefixes output with "(sub agent N)" and
/// only shows headings for tool use.
#[derive(Clone)]
pub struct SubAgentCli {
    prefix: String,
}

impl SubAgentCli {
    pub fn new(agent_number: usize) -> Self {
        Self {
            prefix: format!("(sub agent {agent_number})"),
        }
    }
}

impl AjUi for SubAgentCli {
    fn display_notice(&self, notice: &str) {
        println!("{} {}", self.prefix, notice);
    }

    fn display_error(&self, error: &str) {
        println!("{} Error: {}", self.prefix, error);
    }

    fn get_user_input(&self) -> Option<String> {
        // Sub-agents cannot interact with user
        None
    }

    fn agent_text_start(&self, _text: &str) {
        println!(
            "{}:",
            style(&self.prefix.to_string()).bold().fg(Color::Yellow)
        );
    }

    fn agent_text_update(&self, _diff: &str) {
        // Sub-agents don't show streaming updates
    }

    fn agent_text_stop(&self, text: &str) {
        render_markdown(text);
        println!();
    }

    fn agent_thinking_start(&self, thinking: &str) {
        print!(
            "{} {}: {}",
            self.prefix,
            style("aj is thinking").bold().fg(DARK_GRAY),
            style(thinking).fg(LIGHT_GRAY).on_bright()
        );
    }

    fn agent_thinking_update(&self, diff: &str) {
        print!("{diff}");
    }

    fn agent_thinking_stop(&self) {
        println!("\n");
    }

    fn display_tool_result(&self, tool_name: &str, input: &str, _output: &str) {
        // Only show the tool execution heading, not the full output
        println!(
            "{} {}({})",
            self.prefix,
            style(tool_name).bold().fg(Color::Green),
            input
        );
    }

    fn display_tool_result_diff(&self, tool_name: &str, input: &str, _before: &str, _after: &str) {
        // Only show the tool execution heading, not the full diff
        println!(
            "{} {}({})",
            self.prefix,
            style(tool_name).bold().fg(Color::Green),
            input
        );
    }

    fn display_tool_error(&self, tool_name: &str, input: &str, error: &str) {
        println!(
            "{} {}({})",
            self.prefix,
            style(tool_name).bold().fg(Color::Green),
            input
        );
        println!(
            "{} {}: {}",
            self.prefix,
            style("tool_error").bold().fg(Color::Red),
            error
        );
    }

    fn ask_permission(&self, _message: &str) -> bool {
        // Sub-agents cannot ask for permission
        false
    }

    fn display_token_usage(&self, usage: &TokenUsage) {
        let usage_string = format_token_usage(usage);
        println!(
            "{} {}\n",
            style(self.prefix.to_string()).dim(),
            style(usage_string).dim()
        )
    }

    fn display_token_usage_summary(&self, summary: &UsageSummary) {
        let usage_string = format_usage_summary(summary);
        println!(
            "{} {}\n",
            style(self.prefix.to_string()).dim(),
            style(usage_string).dim()
        )
    }

    fn get_subagent_ui(&self, agent_number: usize) -> impl AjUi {
        SubAgentCli::new(agent_number)
    }
}
