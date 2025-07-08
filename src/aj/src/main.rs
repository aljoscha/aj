use aj::cli::AjCli;
use aj_agent::Agent;
use aj_conf::{AgentEnv, SYSTEM_PROMPT};
use aj_tools::get_builtin_tools;
use tracing_subscriber::EnvFilter;

/// A harness that's setting up our logging and environment variables and calls
/// into our "real" `run()`.
#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(true)
        .init();

    let history_path = match aj_conf::Config::get_history_file_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Could not get history file path: {}", e);
            return;
        }
    };

    let ui = AjCli::new(Some(history_path));
    let result = run(ui).await;

    match result {
        Ok(()) => (),
        Err(err) => {
            eprintln!("Error running agent: {}", err);
        }
    }

    println!("Shutting down, bye...");
}

async fn run(ui: AjCli) -> Result<(), anyhow::Error> {
    let env = AgentEnv::new();
    let mut agent = Agent::new(env, SYSTEM_PROMPT, get_builtin_tools(), ui);

    agent.run().await
}
