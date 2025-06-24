use aj::ui_cli::AjCli;
use aj_agent::Agent;
use aj_conf::{AgentEnv, SYSTEM_PROMPT};
use aj_tools::get_builtin_tools;
use aj_ui::AjUi;
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

    let ui = AjCli::new();
    let result = run(ui).await;

    let ui = match result {
        Ok(ui) => ui,
        Err((ui, err)) => {
            ui.display_error(&err.to_string());
            ui
        }
    };

    ui.display_notice("Shutting down, bye...");
}

async fn run(ui: AjCli) -> Result<AjCli, (AjCli, anyhow::Error)> {
    let env = AgentEnv::new();
    let mut agent = Agent::new(env, SYSTEM_PROMPT, get_builtin_tools(), ui);

    match agent.run().await {
        Ok(()) => Ok(agent.into_ui()),
        Err(err) => Err((agent.into_ui(), err)),
    }
}
