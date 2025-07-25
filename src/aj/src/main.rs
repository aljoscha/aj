use aj::cli::AjCli;
use aj_agent::Agent;
use aj_conf::{AgentEnv, Config, SYSTEM_PROMPT};
use aj_tools::get_builtin_tools;
use aj_ui::AjUi;
use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use std::fs;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "aj")]
#[command(about = "AI-driven agent for software engineering")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Thread management commands
    Threads {
        #[command(subcommand)]
        action: ThreadsAction,
    },
}

#[derive(Subcommand)]
enum ThreadsAction {
    /// List existing conversation threads
    List,
}

/// A harness that's setting up our logging, environment variables, etc. and
/// calls into [Agent::run].
#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(true)
        .init();

    if let Ok(dotenv_path) = Config::get_dotenv_file_path() {
        tracing::info!("loading .env from {:?}", dotenv_path);
        dotenv::from_path(dotenv_path).ok();
    } else {
        tracing::info!("no .env in config directory");
    }

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Threads { action }) => match action {
            ThreadsAction::List => {
                if let Err(e) = list_threads().await {
                    eprintln!("Error listing threads: {e}");
                    std::process::exit(1);
                }
            }
        },
        None => {
            // Default behavior: start interactive session
            let history_path = match Config::get_history_file_path() {
                Ok(path) => path,
                Err(e) => {
                    eprintln!("Could not get history file path: {e}");
                    return;
                }
            };

            let ui = AjCli::new(Some(history_path));
            let env = AgentEnv::new();
            let mut agent = Agent::new(env, SYSTEM_PROMPT, get_builtin_tools(), ui.clone());

            let result = agent.run().await;

            match result {
                Ok(()) => (),
                Err(err) => {
                    ui.display_error(&format!("Error running agent: {err}"));
                }
            }

            ui.display_notice("Shutting down, bye...");
        }
    }
}

async fn list_threads() -> Result<()> {
    let threads_dir = Config::get_threads_dir_path()?;

    if !threads_dir.exists() {
        println!("No threads directory found for this project.");
        return Ok(());
    }

    let entries = fs::read_dir(&threads_dir)?;
    let mut thread_files = Vec::new();

    // Collect all .jsonl files
    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            if let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) {
                thread_files.push((file_stem.to_string(), path));
            }
        }
    }

    if thread_files.is_empty() {
        println!("No conversation threads found for this project.");
        return Ok(());
    }

    // Sort by filename (which corresponds to creation time), latest first
    thread_files.sort_by(|a, b| b.0.cmp(&a.0));

    for (thread_id, path) in thread_files {
        // Get file metadata for additional info
        let metadata = fs::metadata(&path)?;
        let modified = metadata.modified()?;
        let modified_str = DateTime::<Utc>::from(modified)
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string();

        // Use file size as proxy for conversation length
        let file_size = metadata.len();
        let size_display = if file_size < 1024 {
            format!("{file_size}B")
        } else if file_size < 1024 * 1024 {
            format!("{}KB", file_size / 1024)
        } else {
            format!("{}MB", file_size / (1024 * 1024))
        };

        println!("{thread_id} (modified: {modified_str}, {size_display})");
    }

    Ok(())
}
