use nu_ansi_term::Color::Red;
use tracing_subscriber::EnvFilter;

use aj_agent::{Agent, StdinUserMessage};
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
        println!("{}: {err}", Red.paint("Error:"));
    }
}

async fn run() -> Result<(), anyhow::Error> {
    let mut agent = Agent::new(StdinUserMessage, get_builtin_tools());

    agent.run().await?;

    Ok(())
}
