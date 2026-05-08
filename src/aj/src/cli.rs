use std::sync::{Arc, Mutex};

use aj_agent::types::{TokenUsage, UsageSummary};
use console::{Color, style};
use rustyline::config::Config;
use rustyline::config::EditMode;
use rustyline::history::MemHistory;
use rustyline::{Cmd, Editor, KeyEvent};

use crate::cli_common::AjCliCommon;
use crate::prompt_history::PromptHistory;

/// CLI renderer for the legacy `aj` binary.
///
/// Owns the readline loop and the in-memory prompt history; all
/// other display methods delegate to a shared [`AjCliCommon`].
/// The struct is plain inherent-method dispatch — there is no
/// trait impl to satisfy. Per `docs/aj-next-plan.md` §2.6 the
/// previous `AjUi` indirection is gone; the binary owns one
/// `AjCli` for input + outside-the-turn-loop notices and a
/// separate [`AjCliCommon`] (or several, one per agent id) lives
/// inside the bus listener for in-turn rendering.
///
/// The prompt history is held in-memory and shared with any
/// renderer clones (made via [`AjCli::renderer`]) so that the
/// harness UI and the cloned UI handed to listeners both see the
/// same up-arrow stack. Bootstrap from the on-disk JSONL
/// conversation logs happens once at startup; this struct never
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

    /// Borrow the shared rendering helper. The bus listener (and
    /// any other in-process consumer that needs to render without
    /// pulling in the readline loop) holds its own clone of this
    /// to drive [`AjCliCommon`]'s display methods directly.
    pub fn renderer(&self) -> AjCliCommon {
        self.common.clone()
    }

    pub fn display_notice(&self, notice: &str) {
        self.common.display_notice(notice);
    }

    pub fn display_warning(&self, warning: &str) {
        self.common.display_warning(warning);
    }

    pub fn display_error(&self, error: &str) {
        self.common.display_error(error);
    }

    pub fn display_token_usage(&self, usage: &TokenUsage) {
        self.common.display_token_usage(usage);
    }

    pub fn display_token_usage_summary(&self, summary: &UsageSummary) {
        self.common.display_token_usage_summary(summary);
    }

    pub fn agent_text_stop(&self, text: &str) {
        self.common.agent_text_stop(text);
    }

    pub fn display_tool_result_diff(
        &self,
        tool_name: &str,
        input: &str,
        before: &str,
        after: &str,
    ) {
        self.common
            .display_tool_result_diff(tool_name, input, before, after);
    }

    /// Read one line from the user via rustyline. Returns `None`
    /// for Ctrl-C / Ctrl-D / empty input.
    pub fn get_user_input(&self) -> Option<String> {
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
                // in-memory history. The next readline call (and any
                // cloned `AjCli` inside listeners) will see it.
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
}

pub(crate) const DARK_GRAY: Color = Color::Color256(239);
pub(crate) const LIGHT_GRAY: Color = Color::Color256(248);
