//! Binary entry point for the next-generation `aj-next` CLI.
//!
//! Loads `~/.aj/.env`, parses CLI args (see
//! [`aj_next::cli::args::Args`]), and dispatches to either
//! [`aj_next::modes::print`] or [`aj_next::modes::interactive`].
//! Subcommands (`list-threads`, `continue`, `models update`)
//! short-circuit before mode dispatch.
//!
//! The dispatch logic itself is intentionally bare in the
//! scaffold — both modes return a "not yet implemented" error
//! today; the next Phase 1 steps replace those bodies. Argument
//! parsing and mode selection are wired up so subsequent steps
//! can plug their implementations into a stable surface.

use aj_conf::Config;
use aj_next::cli::args::{Args, Command, ModelsCommand};
use aj_next::modes::{interactive::InteractiveMode, print};
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
        Some(Command::Models { command }) => handle_models_command(command).await,
        Some(Command::ListThreads) => handle_list_threads(),
        Some(Command::Continue { thread_id: _ }) => {
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

/// `aj-next list-threads`: list existing thread ids for the
/// current project. Stub for the scaffold; the print-mode step
/// wires it to [`aj_session::ConversationPersistence::list_threads`].
fn handle_list_threads() -> Result<()> {
    anyhow::bail!("aj-next list-threads is not yet implemented");
}

/// `aj-next models <subcommand>`. Stub for the scaffold; wired
/// up alongside print mode. The real body awaits the catalog
/// refresh's HTTP fetch, so the signature stays `async`.
#[allow(clippy::unused_async)]
async fn handle_models_command(command: ModelsCommand) -> Result<()> {
    match command {
        ModelsCommand::Update => {
            anyhow::bail!("aj-next models update is not yet implemented");
        }
    }
}
