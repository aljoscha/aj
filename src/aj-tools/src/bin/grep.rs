use aj_agent::tool::ToolDefinition;
use aj_tools::testing::DummyToolContext;
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
