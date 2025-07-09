use aj_tools::ToolDefinition;
use aj_tools::testing::{DummySessionContext, DummyTurnContext};
use aj_tools::tools::grep::{GrepInput, GrepTool};
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() != 4 {
        eprintln!("Usage: {} <path> <include> <pattern>", args[0]);
        eprintln!("  path:    Absolute path to start search from");
        eprintln!("  include: File patterns to include (e.g., '*.rs', '*.{{rs,toml}}')");
        eprintln!("  pattern: Regular expression to search for");
        std::process::exit(1);
    }

    let path = args[1].clone();
    let include = args[2].clone();
    let pattern = args[3].clone();

    let input = GrepInput {
        path,
        include: Some(include),
        pattern,
    };
    let tool = GrepTool;
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
