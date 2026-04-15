use aj::SYSTEM_PROMPT;
use aj::cli::AjCli;
use aj_agent::Agent;
use aj_conf::{AgentEnv, Config};
use aj_models::{ModelArgs, conversation::ConversationPersistence, create_model};
use aj_tools::get_builtin_tools;
use aj_ui::AjUi;
use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "aj")]
#[command(about = "AI-driven agent for software engineering")]
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

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// List conversation threads for this project.
    ListThreads,
    /// Resume a conversation thread.
    Resume {
        /// Conversation ID to resume.
        conversation_id: String,
    },
    /// Resume the latest conversation thread.
    ResumeLatest,
}

#[derive(Subcommand)]
enum ThreadsAction {
    /// List existing conversation threads
    List,
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

    let history_path = match Config::get_history_file_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Could not get history file path: {e}");
            return Err(e.into());
        }
    };

    let threads_dir = Config::get_threads_dir_path()?;

    // Resolve settings with precedence: CLI flags > env vars > config.toml > defaults.
    let mut ui = AjCli::new(Some(history_path));
    let env = AgentEnv::new();
    let conversation_persistence = ConversationPersistence::new(threads_dir);

    let model_args = ModelArgs {
        api: cli
            .model_api
            .or(config.model_api)
            .unwrap_or_else(|| "anthropic".to_string()),
        url: cli.model_url.or(config.model_url),
        model_name: cli.model_name.or(config.model_name),
    };
    let model = create_model(model_args)?;

    let mut agent = Agent::new(
        env,
        ui.shallow_clone(),
        conversation_persistence.clone(),
        SYSTEM_PROMPT,
        get_builtin_tools(),
        model,
        config.thinking,
    );

    match cli.command {
        Some(Commands::ListThreads) => {
            list_threads(&conversation_persistence)?;
        }
        Some(Commands::Resume { conversation_id }) => {
            let latest_conversation =
                conversation_persistence.load_conversation(&conversation_id)?;
            agent.run(Some(latest_conversation)).await?;
        }
        Some(Commands::ResumeLatest) => {
            let latest_thread_id = conversation_persistence.get_latest_thread_id()?;
            if let Some(latest_thread_id) = latest_thread_id {
                let latest_conversation =
                    conversation_persistence.load_conversation(&latest_thread_id)?;
                agent.run(Some(latest_conversation)).await?;
            } else {
                ui.display_notice("No latest conversation to resume");
            }
        }
        None => {
            // Default behavior: run agent with new/empty conversation.
            agent.run(None).await?;
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
