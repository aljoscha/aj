use aj::cli::AjCli;
use aj_agent::Agent;
use aj_conf::{AgentEnv, SYSTEM_PROMPT};
use aj_tools::get_builtin_tools;
use aj_ui::AjUi;
use tracing_subscriber::EnvFilter;

/// A harness that's setting up our logging, environment variables, etc. and
/// calls into [Agent::run].
#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(true)
        .init();

    if let Ok(dotenv_path) = aj_conf::Config::get_dotenv_file_path() {
        tracing::info!("loading .env from {:?}", dotenv_path);
        dotenv::from_path(dotenv_path).ok();
    } else {
        tracing::info!("no .env in config directory");
    }

    let history_path = match aj_conf::Config::get_history_file_path() {
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
