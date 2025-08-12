use std::path::PathBuf;

use aj_ui::{AjUi, TokenUsage, UsageSummary};
use console::{Color, style};
use rustyline::config::Config;
use rustyline::config::EditMode;
use rustyline::history::FileHistory;
use rustyline::{Cmd, Editor, KeyEvent};

pub mod common_cli;
pub mod sub_agent_cli;

use common_cli::AjCliCommon;

/// Cli-based implementation of [AjUi].
#[derive(Clone)]
pub struct AjCli {
    history_path: Option<PathBuf>,
    common: AjCliCommon,
}

impl AjCli {
    pub fn new(history_path: Option<PathBuf>) -> Self {
        Self {
            history_path,
            common: AjCliCommon::new(None, true, true),
        }
    }
}

const DARK_GRAY: Color = Color::Color256(239);
const LIGHT_GRAY: Color = Color::Color256(248);

impl AjUi for AjCli {
    fn display_notice(&self, notice: &str) {
        self.common.display_notice(notice);
    }

    fn display_error(&self, error: &str) {
        self.common.display_error(error);
    }

    fn get_user_input(&self) -> Option<String> {
        let config = Config::builder().edit_mode(EditMode::Emacs).build();

        let mut rl: Editor<(), FileHistory> =
            Editor::with_history(config, FileHistory::new()).unwrap();

        if let Some(history_path) = self.history_path.as_ref() {
            if history_path.exists() {
                let _ = rl.load_history(history_path);
            }
        }

        rl.bind_sequence(KeyEvent::ctrl('S'), Cmd::Newline);

        let prompt = format!("{}: ", style("you").bold().fg(Color::Blue));

        match rl.readline(&prompt) {
            Ok(line) => {
                if line.trim().is_empty() {
                    println!();
                    return None;
                }

                // Add to history if not empty and not a duplicate
                if !line.trim().is_empty() {
                    let _ = rl.add_history_entry(&line);
                }

                if let Some(history_path) = self.history_path.as_ref() {
                    let _ = rl.save_history(history_path);
                }

                println!();
                Some(line)
            }
            Err(rustyline::error::ReadlineError::Interrupted) => None, // Ctrl-C
            Err(rustyline::error::ReadlineError::Eof) => None,         // Ctrl-D
            Err(_) => None,
        }
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

    fn display_tool_result(&self, tool_name: &str, input: &str, result: &str) {
        self.common.display_tool_result(tool_name, input, result);
    }

    fn display_tool_result_diff(&self, tool_name: &str, input: &str, before: &str, after: &str) {
        self.common
            .display_tool_result_diff(tool_name, input, before, after);
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
        sub_agent_cli::SubAgentCli::new(agent_number)
    }
}
