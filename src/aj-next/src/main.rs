//! Binary entry point for the `aj-next` CLI, a vaxis-based sibling to `aj`.
//!
//! Loads `~/.aj/.env`, parses the shared CLI surface
//! ([`aj_app::cli::args::Args`]), and dispatches. The non-interactive
//! subcommands and print mode reuse `aj-app` directly, exactly as `aj` does.
//! The interactive branch is the vaxis frontend (see `interactive`).

use aj_app::cli::args::{Args, Command};
use aj_conf::Config;
use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod interactive;
mod tool_cell;
mod transcript;

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr by default. Set `AJ_LOG_FILE` to redirect them to a
    // file instead (appended, without ANSI colors), which keeps the
    // interactive TUI from fighting with log output over the terminal.
    let builder = tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env());
    match std::env::var_os("AJ_LOG_FILE") {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            builder
                .with_ansi(false)
                .with_writer(std::sync::Arc::new(file))
                .init();
        }
        None => builder.with_ansi(true).init(),
    }

    // `~/.aj/.env` first (highest priority for env-driven config), then a
    // project-local `.env` if present. CLI flags layer on top via clap's
    // `env = ...` per-arg attribute.
    if let Ok(dotenv_path) = Config::get_dotenv_file_path() {
        tracing::info!("loading .env from {:?}", dotenv_path);
        dotenv::from_path(dotenv_path).ok();
    } else {
        tracing::info!("no .env in config directory");
    }
    dotenv::dotenv().ok();

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
