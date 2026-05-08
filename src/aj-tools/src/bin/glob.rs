use aj_agent::tool::ToolDefinition;
use aj_tools::testing::DummyToolContext;
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
    let mut ctx = DummyToolContext::default();
    let outcome = tool.execute(&mut ctx, input).await?;
    if outcome.is_error {
        match &outcome.details {
            aj_agent::tool::ToolDetails::Text { body, .. } => {
                eprintln!("Error: {body}");
            }
            other => eprintln!("Error: {other:?}"),
        }
        std::process::exit(1);
    }

    match outcome.details {
        aj_agent::tool::ToolDetails::Text { body, .. } => println!("{body}"),
        other => println!("{other:?}"),
    }

    Ok(())
}
