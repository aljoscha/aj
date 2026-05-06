use std::sync::{Arc, Mutex};

use aj_ui::{AjUi, TokenUsage, UsageSummary};
use console::{Color, style};
use rustyline::config::Config;
use rustyline::config::EditMode;
use rustyline::history::MemHistory;
use rustyline::{Cmd, Editor, KeyEvent};

use crate::cli_common::AjCliCommon;
use crate::prompt_history::PromptHistory;

/// Cli-based implementation of [AjUi].
///
/// The prompt history is held in-memory and shared with any clones (made
/// via [`AjUi::shallow_clone`]) so that the agent's UI and the harness's
/// UI both see the same up-arrow stack. Bootstrap from the on-disk
/// JSONL conversation logs happens once at startup; this struct never
/// touches a separate history file.
#[derive(Clone)]
pub struct AjCli {
    history: Arc<Mutex<PromptHistory>>,
    common: AjCliCommon,
}

impl AjCli {
    pub fn new(history: Arc<Mutex<PromptHistory>>) -> Self {
        Self {
            history,
            common: AjCliCommon::new(None, true, true),
        }
    }

    /// Construct an [`AjCli`] with an empty in-memory prompt history.
    /// Useful for example/test binaries that don't actually need
    /// persistent history.
    pub fn with_empty_history() -> Self {
        Self::new(Arc::new(Mutex::new(PromptHistory::new(
            crate::prompt_history::DEFAULT_MAX_ENTRIES,
        ))))
    }
}

pub(crate) const DARK_GRAY: Color = Color::Color256(239);
pub(crate) const LIGHT_GRAY: Color = Color::Color256(248);

impl AjUi for AjCli {
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
        let config = Config::builder().edit_mode(EditMode::Emacs).build();

        let mut rl: Editor<(), MemHistory> =
            Editor::with_history(config, MemHistory::new()).unwrap();

        // Snapshot the in-memory history into the editor's MemHistory.
        // We do this on every readline call so the user sees prompts
        // submitted earlier in this same session as well as those
        // bootstrapped from the JSONL logs at startup.
        {
            let history = self.history.lock().unwrap();
            history.install(&mut rl);
        }

        rl.bind_sequence(KeyEvent::ctrl('S'), Cmd::Newline);

        let prompt = format!("{}: ", style("you").bold().fg(Color::Blue));

        match rl.readline(&prompt) {
            Ok(line) => {
                if line.trim().is_empty() {
                    println!();
                    return None;
                }

                // Record the freshly submitted prompt in the shared
                // in-memory history. The next readline call (and the
                // shallow-cloned UI inside the agent) will see it.
                {
                    let mut history = self.history.lock().unwrap();
                    history.record(&line);
                }

                println!();
                Some(line)
            }
            Err(rustyline::error::ReadlineError::Interrupted) => None, // Ctrl-C
            Err(rustyline::error::ReadlineError::Eof) => None,         // Ctrl-D
            Err(_) => None,
        }
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

    fn display_tool_result(&mut self, tool_name: &str, input: &str, result: &str) {
        self.common.display_tool_result(tool_name, input, result);
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
        Box::new(crate::cli_sub_agent::SubAgentCli::new(agent_number))
    }

    fn shallow_clone(&mut self) -> Box<dyn AjUi> {
        // Cheap Arc clone — both the original and the shallow clone
        // share the same underlying PromptHistory and so observe each
        // other's submissions.
        Box::new(crate::cli::AjCli::new(Arc::clone(&self.history)))
    }
}
