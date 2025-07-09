use aj_tools::ToolDefinition;
use aj_tools::testing::{DummySessionContext, DummyTurnContext};
use aj_tools::tools::glob::{GlobInput, GlobTool};
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() != 3 {
        eprintln!("Usage: {} <path> <pattern>", args[0]);
        eprintln!("  path:    Absolute path to start search from");
        eprintln!("  pattern: Glob pattern to match (e.g., '*.rs', '**/*.toml')");
        std::process::exit(1);
    }

    let path = args[1].clone();
    let pattern = args[2].clone();

    let input = GlobInput { path, pattern };
    let tool = GlobTool;
    let mut session_ctx = DummySessionContext;
    let mut turn_ctx = DummyTurnContext;

    match tool.execute(&mut session_ctx, &mut turn_ctx, input).await {
        Ok(result) => println!("{result}"),
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }

    Ok(())
}
