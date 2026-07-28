//! Binary entry point for the `aj` CLI.
//!
//! Loads `~/.aj/.env`, parses the shared CLI surface
//! ([`aj_app::cli::args::Args`]), and dispatches. The non-interactive
//! subcommands and print mode reuse `aj-app` directly. The interactive branch
//! is the vaxis frontend (see `interactive`).

use aj_app::cli::args::{Args, Command};
use aj_conf::Config;
use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

enum LogDestination<W> {
    Stderr,
    File(W),
}

fn select_log_destination<W, E>(
    path: Option<&std::ffi::OsStr>,
    open: impl FnOnce(&std::ffi::OsStr) -> Result<W, E>,
) -> (LogDestination<W>, Option<E>) {
    let Some(path) = path else {
        return (LogDestination::Stderr, None);
    };
    match open(path) {
        Ok(writer) => (LogDestination::File(writer), None),
        Err(error) => (LogDestination::Stderr, Some(error)),
    }
}

mod agent_picker;
mod autocomplete;
mod bubble;
mod content_overlay;
mod corner_box;
mod footer;
mod frame_stats_box;
mod image_store;
mod interactive;
mod keymap;
mod login;
mod markdown_view;
mod overlay;
mod palette;
mod pending;
mod prompt_history;
mod quit_hint;
mod scroll;
mod selection_copied;
mod session_selector;
mod session_tree;
mod settings_ui;
mod splash;
mod status;
mod subagent_box;
mod task_output;
mod terminal;
#[cfg(test)]
mod test_support;
mod toasts;
mod tool_cell;
mod transcript;
mod usage_overlay;

#[tokio::main]
async fn main() -> Result<()> {
    // `~/.aj/.env` first (highest priority for env-driven config), then a
    // project-local `.env` if present. `dotenv` preserves values already set,
    // so loading in this order implements the documented precedence.
    if let Ok(dotenv_path) = Config::get_dotenv_file_path() {
        dotenv::from_path(dotenv_path).ok();
    }
    dotenv::dotenv().ok();

    // Logs go to stderr by default. Set `AJ_LOG_FILE` to redirect them to a
    // file instead (appended, without ANSI colors), which keeps the
    // interactive TUI from fighting with log output over the terminal.
    let log_path = std::env::var_os("AJ_LOG_FILE");
    let (destination, open_error) = select_log_destination(log_path.as_deref(), |path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    });
    if let Some(error) = open_error {
        eprintln!("aj: warning: could not open AJ_LOG_FILE; logging to stderr: {error}");
    }
    let builder = tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env());
    match destination {
        LogDestination::File(file) => {
            builder
                .with_ansi(false)
                .with_writer(std::sync::Arc::new(file))
                .init();
        }
        LogDestination::Stderr => builder.with_ansi(true).init(),
    }

    let args = Args::parse();

    match args.command {
        Some(Command::UpdateModels) => aj_app::handle_update_models_command().await,
        Some(Command::ListSessions) => aj_app::handle_list_sessions(),
        Some(Command::Continue {
            session_id: _,
            prompt: _,
        }) => dispatch_session_mode(args).await,
        None => dispatch_session_mode(args).await,
    }
}

#[cfg(test)]
mod startup_tests {
    use std::ffi::OsStr;

    use super::{LogDestination, select_log_destination};

    #[test]
    fn logging_defaults_to_stderr_without_a_path() {
        let (destination, error) = select_log_destination::<(), ()>(None, |_| unreachable!());
        assert!(matches!(destination, LogDestination::Stderr));
        assert!(error.is_none());
    }

    #[test]
    fn logging_falls_back_to_stderr_when_logfile_open_fails() {
        let (destination, error) =
            select_log_destination(Some(OsStr::new("bad.log")), |_| Err::<(), _>("open failed"));
        assert!(matches!(destination, LogDestination::Stderr));
        assert_eq!(error, Some("open failed"));
    }

    #[test]
    fn logging_uses_an_opened_logfile() {
        let (destination, error) =
            select_log_destination(Some(OsStr::new("aj.log")), |_| Ok::<_, ()>(7));
        assert!(matches!(destination, LogDestination::File(7)));
        assert!(error.is_none());
    }
}

/// Dispatch to the interactive or print mode based on `--print`.
///
/// Print mode reuses `aj-app`'s headless runner. The interactive branch is
/// the vaxis alt-screen shell. Its futures are `!Send`, which is fine here:
/// `#[tokio::main]` drives this future with a top-level `block_on`.
async fn dispatch_session_mode(args: Args) -> Result<()> {
    if args.print {
        aj_app::print::run(args).await
    } else {
        interactive::run(args).await
    }
}
