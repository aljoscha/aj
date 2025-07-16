use aj_tools::ToolDefinition;
use aj_tools::testing::{DummyPermissionHandler, DummySessionContext, DummyTurnContext};
use aj_tools::tools::ls::{LsInput, LsTool};
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 || args.len() > 4 {
        eprintln!(
            "Usage: {} <path> [--recursive] [--ignore pattern1,pattern2,...]",
            args[0]
        );
        eprintln!("  path:       Absolute path to directory to list");
        eprintln!("  --recursive: Enable recursive directory traversal");
        eprintln!("  --ignore:   Comma-separated glob patterns to ignore");
        eprintln!();
        eprintln!("Examples:");
        eprintln!("  {} /home/user", args[0]);
        eprintln!("  {} /home/user --recursive", args[0]);
        eprintln!("  {} /home/user --ignore '*.tmp,*.log'", args[0]);
        eprintln!(
            "  {} /home/user --recursive --ignore '*.tmp,*.log'",
            args[0]
        );
        std::process::exit(1);
    }

    let path = args[1].clone();
    let mut recursive = false;
    let mut ignore_patterns: Option<Vec<String>> = None;

    // Parse additional arguments
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
            eprintln!("Error: Unknown argument: {}", arg);
            std::process::exit(1);
        }
    }

    let input = LsInput {
        path,
        ignore: ignore_patterns,
        recursive: if recursive { Some(true) } else { None },
    };

    let tool = LsTool;
    let mut session_ctx = DummySessionContext;
    let mut turn_ctx = DummyTurnContext;
    let permission_handler = DummyPermissionHandler;

    match tool
        .execute(&mut session_ctx, &mut turn_ctx, &permission_handler, input)
        .await
    {
        Ok(result) => println!("{}", result.return_value),
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }

    Ok(())
}
