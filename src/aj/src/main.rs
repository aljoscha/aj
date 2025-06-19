use console::{Color, style};
use tracing_subscriber::EnvFilter;

use aj_agent::{Agent, StdinUserMessage};
use aj_conf::{AgentEnv, SYSTEM_PROMPT};
use aj_tools::get_builtin_tools;

/// A harness that's setting up our logging and environment variables and calls
/// into our "real" `run()`.
#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(true)
        .init();

    let result = run().await;

    if let Err(err) = result {
        println!("{}: {err}", style("Error:").bold().fg(Color::Red));
    }
}

async fn run() -> Result<(), anyhow::Error> {
    let env = AgentEnv::new();
    let mut agent = Agent::new(env, SYSTEM_PROMPT, get_builtin_tools(), StdinUserMessage);

    agent.run().await?;

    Ok(())
}
