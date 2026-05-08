use aj_agent::tool::ToolDefinition;
use aj_tools::testing::DummyToolContext;
use aj_tools::tools::ls::{LsInput, LsTool};
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 || args.len() > 4 {
        eprintln!(
            "Usage: {} <path> [--recursive] [--ignore=pattern1,pattern2,...]",
            args[0]
        );
        eprintln!("  path:       Absolute path to directory to list");
        eprintln!("  --recursive: Enable recursive directory traversal");
        eprintln!("  --ignore:   Comma-separated glob patterns to ignore");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  {} /home/user", args[0]);
        eprintln!("  {} /home/user --recursive", args[0]);
        eprintln!("  {} /home/user --ignore='*.tmp,*.log'", args[0]);
        eprintln!(
            "  {} /home/user --recursive --ignore='*.tmp,*.log'",
            args[0]
        );
        std::process::exit(1);
    }

    let path = args[1].clone();
    let mut recursive = false;
    let mut ignore_patterns: Option<Vec<String>> = None;

    // Parse remaining flags. `--ignore=...` accepts a comma-separated
    // list of glob patterns; `--recursive` is a bare flag.
    for arg in &args[2..] {
        if arg == "--recursive" {
            recursive = true;
        } else if arg.starts_with("--ignore") {
            if let Some(equals_pos) = arg.find('=') {
                let patterns_str = &arg[equals_pos + 1..];
                ignore_patterns = Some(
                    patterns_str
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .collect(),
                );
            } else {
                eprintln!("Error: --ignore requires a value (e.g., --ignore='*.tmp,*.log')");
                std::process::exit(1);
            }
        } else {
            eprintln!("Error: Unknown argument: {arg}");
            std::process::exit(1);
        }
    }

    let input = LsInput {
        path,
        ignore: ignore_patterns,
        recursive: if recursive { Some(true) } else { None },
    };

    let tool = LsTool;
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
