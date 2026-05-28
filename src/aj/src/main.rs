//! Binary entry point for the `aj` CLI.
//!
//! Loads `~/.aj/.env`, parses CLI args (see
//! [`aj::cli::args::Args`]), and dispatches to either
//! [`aj::modes::print`] or [`aj::modes::interactive`].
//! Subcommands (`list-sessions`, `continue`, `update-models`)
//! short-circuit before mode dispatch.

use aj::cli::args::{Args, Command};
use aj::modes::{interactive::InteractiveMode, print};
use aj_conf::Config;
use aj_session::ConversationPersistence;
use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(true)
        .init();

    // `~/.aj/.env` first (highest priority for env-driven config),
    // then a project-local `.env` if present. CLI flags layered on
    // top via clap's `env = ...` per-arg attribute.
    if let Ok(dotenv_path) = Config::get_dotenv_file_path() {
        tracing::info!("loading .env from {:?}", dotenv_path);
        dotenv::from_path(dotenv_path).ok();
    } else {
        tracing::info!("no .env in config directory");
    }
    dotenv::dotenv().ok();

    let args = Args::parse();

    match args.command {
        Some(Command::UpdateModels) => handle_update_models_command().await,
        Some(Command::ListSessions) => handle_list_sessions(),
        Some(Command::Continue {
            session_id: _,
            prompt: _,
        }) => {
            // `continue` always lands in interactive mode (or
            // print mode if the user passed `--print`). The mode
            // itself decides how to resume; we just dispatch.
            dispatch_session_mode(args).await
        }
        None => dispatch_session_mode(args).await,
    }
}

/// Dispatch to the interactive or print mode based on `--print`.
///
/// Per `docs/aj-next-plan.md` §4.2 the same binary serves both;
/// the only difference is which subscriber drives the agent's bus.
async fn dispatch_session_mode(args: Args) -> Result<()> {
    if args.print {
        print::run(args).await
    } else {
        InteractiveMode::from_args(args)?.run().await
    }
}

/// `aj list-sessions`: list existing conversation sessions
/// for the current project, latest first.
///
/// Output: one row per session, formatted as `<session_id>
/// (modified: <utc-ts>, <size>)`. The underlying iteration,
/// pre-refactor-format filtering, and size formatting all live
/// in [`ConversationPersistence::list_sessions`] (`aj-session`);
/// this function is a thin presentation wrapper.
fn handle_list_sessions() -> Result<()> {
    let sessions_dir = Config::get_sessions_dir_path()?;
    let conversation_persistence = ConversationPersistence::new(sessions_dir);
    let sessions = conversation_persistence.list_sessions()?;

    if sessions.is_empty() {
        println!("No conversation sessions found for this project.");
        return Ok(());
    }

    for session in sessions {
        println!(
            "{} (modified: {}, {})",
            session.session_id, session.modified, session.size_display
        );
    }

    Ok(())
}

/// `aj update-models`: refresh the on-disk model catalog at
/// `~/.aj/models.json` from `models.dev`. The `/model` selector
/// overlay reads that catalog at startup, so running this command
/// is how users surface freshly-released models to the picker
/// without restarting from a different catalog source.
///
/// The output is a one-line summary (added / removed /
/// price-changes counts plus total + destination path) suitable
/// for scripting.
async fn handle_update_models_command() -> Result<()> {
    let summary = aj_models::refresh::refresh_user_cache().await?;
    println!("{}", summary.one_line());
    Ok(())
}
