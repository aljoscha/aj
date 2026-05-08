use std::sync::{Arc, Mutex};

use aj::SYSTEM_PROMPT;
use aj::cli::AjCli;
use aj::event_bridge::EventBridgeListener;
use aj::prompt_history::{DEFAULT_MAX_ENTRIES, PromptHistory};
use aj_agent::Agent;
use aj_conf::{AgentEnv, Config, ConfigSpeed};
use aj_models::messages::Speed;
use aj_models::{ModelArgs, create_model};
use aj_session::{ConversationLog, ConversationPersistence};
use aj_tools::get_builtin_tools;
use aj_ui::AjUi;
use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "aj")]
#[command(about = "AI-driven agent for software engineering")]
#[command(flatten_help = true)]
struct Cli {
    /// Model API to use.
    #[arg(long, env)]
    model_api: Option<String>,

    /// Model endpoint URL.
    #[arg(long, env)]
    model_url: Option<String>,

    /// Model name to use.
    #[arg(long, env)]
    model_name: Option<String>,

    /// Inference speed mode: `standard` (default) or `fast` (Anthropic
    /// beta `speed` parameter; requires the `fast-inference-2025-10-02`
    /// beta header).
    #[arg(long, env)]
    speed: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
#[command(flatten_help = true)]
enum Commands {
    /// List existing conversation threads.
    ListThreads,
    /// Continue a conversation thread (latest if no id given).
    Continue {
        /// Conversation ID to continue (if not provided, continues latest
        /// thread).
        thread_id: Option<String>,
    },
    /// Manage the bundled model catalog.
    Models {
        #[command(subcommand)]
        command: ModelsCommands,
    },
}

#[derive(Subcommand)]
#[command(flatten_help = true)]
enum ModelsCommands {
    /// Refresh the user model catalog at `~/.aj/models.json` from
    /// `https://models.dev/api.json`. Filters to tool-capable Anthropic
    /// and OpenAI models, applies the bundled overrides, and writes the
    /// result atomically. On any fetch or parse failure the existing
    /// cache is left untouched.
    Update,
}

/// A harness that's setting up our logging, environment variables, etc. and
/// calls into [Agent::run].
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(true)
        .init();

    // Load config.toml first (lowest priority).
    let config = Config::load().unwrap_or_else(|e| {
        tracing::warn!("failed to load config.toml: {e}");
        Config::default()
    });

    // Load .env files (these set env vars, which are medium priority).
    if let Ok(dotenv_path) = Config::get_dotenv_file_path() {
        tracing::info!("loading .env from {:?}", dotenv_path);
        dotenv::from_path(dotenv_path).ok();
    } else {
        tracing::info!("no .env in config directory");
    }

    dotenv::dotenv().ok();

    // Parse CLI flags (highest priority).
    let cli = Cli::parse();

    // `models update` is a standalone catalog-management subcommand; it
    // doesn't need an API key, history, or agent. Short-circuit before
    // any of that setup, both to keep startup fast and to let users run
    // the command without configured credentials.
    if let Some(Commands::Models { command }) = &cli.command {
        return handle_models_command(command).await;
    }

    let threads_dir = Config::get_threads_dir_path()?;

    // Resolve settings with precedence: CLI flags > env vars > config.toml > defaults.
    let env = AgentEnv::new();
    let conversation_persistence = ConversationPersistence::new(threads_dir);

    // Bootstrap the prompt history from the project's JSONL conversation
    // logs. The resulting `Arc<Mutex<_>>` is shared (via shallow_clone)
    // between the harness UI and the cloned UI handed to the agent, so
    // submissions made during the session immediately show up in the
    // editor's up-arrow stack.
    let mut ui = AjCli::new(Arc::new(Mutex::new(PromptHistory::bootstrap(
        &conversation_persistence,
        DEFAULT_MAX_ENTRIES,
    ))));

    let speed = match cli.speed {
        Some(s) => Some(s.parse::<ConfigSpeed>().map_err(anyhow::Error::msg)?),
        None => config.speed,
    }
    .map(|s| match s {
        ConfigSpeed::Standard => Speed::Standard,
        ConfigSpeed::Fast => Speed::Fast,
    });

    let model_args = ModelArgs {
        api: cli
            .model_api
            .or(config.model_api)
            .unwrap_or_else(|| "anthropic".to_string()),
        url: cli.model_url.or(config.model_url),
        model_name: cli.model_name.or(config.model_name),
        speed,
    };
    let model = create_model(model_args)?;

    let mut tools = get_builtin_tools();
    if !config.disabled_tools.is_empty() {
        tools.retain(|tool| !config.disabled_tools.contains(&tool.name));
        tracing::info!(disabled = ?config.disabled_tools, "filtered disabled tools");
    }

    let mut agent = Agent::new(
        env,
        ui.shallow_clone(),
        SYSTEM_PROMPT,
        tools,
        config.disabled_tools.clone(),
        model,
        config.thinking,
    );

    // Register the bus -> AjCli rendering bridge before any turns
    // run, so every event the agent emits during inference flows
    // back into the existing renderer. The listener owns its own
    // shallow-cloned `AjCli` (sharing the prompt-history `Arc`) plus
    // lazily-created `SubAgentCli`s for any sub-agent the session
    // spawns; per `docs/aj-next-plan.md` §1.6 sub-agents share the
    // parent's bus, so all of their events flow through this same
    // listener too. The handle stays alive for the rest of the
    // process: we hold it on the stack until `main` returns.
    let _bridge_handle =
        agent.subscribe(EventBridgeListener::new(ui.shallow_clone()).into_listener());

    match cli.command {
        Some(Commands::ListThreads) => {
            list_threads(&conversation_persistence)?;
        }
        Some(Commands::Continue { thread_id }) => {
            if let Some(thread_id) = thread_id {
                let mut log = ConversationLog::resume(&conversation_persistence, &thread_id)?;
                agent.run(&mut log).await?;
            } else {
                let latest_thread_id = conversation_persistence.get_latest_thread_id()?;
                if let Some(latest_thread_id) = latest_thread_id {
                    let mut log =
                        ConversationLog::resume(&conversation_persistence, &latest_thread_id)?;
                    agent.run(&mut log).await?;
                } else {
                    ui.display_notice("No latest conversation to resume");
                }
            }
        }
        Some(Commands::Models { .. }) => {
            // Handled earlier in `main` before agent setup. Reaching
            // this arm would mean we forgot to short-circuit above.
            unreachable!("models subcommand handled before agent setup");
        }
        None => {
            // Default behavior: start a fresh log and run the agent.
            let mut log = ConversationLog::create(&conversation_persistence)?;
            agent.run(&mut log).await?;
        }
    }

    Ok(())
}

fn list_threads(conversation_persistence: &ConversationPersistence) -> Result<()> {
    let threads = conversation_persistence.list_threads()?;

    if threads.is_empty() {
        println!("No conversation threads found for this project.");
        return Ok(());
    }

    for thread in threads {
        println!(
            "{} (modified: {}, {})",
            thread.thread_id, thread.modified, thread.size_display
        );
    }

    Ok(())
}

/// Dispatch for `aj models <subcommand>`. Currently only `update`, but
/// the layer is in place for future catalog management commands.
async fn handle_models_command(command: &ModelsCommands) -> Result<()> {
    match command {
        ModelsCommands::Update => {
            let summary = aj_models::refresh::refresh_user_cache().await?;
            println!("{}", summary.one_line());
            Ok(())
        }
    }
}
