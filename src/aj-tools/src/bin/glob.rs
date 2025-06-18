use aj_tools::tools::glob::{GlobInput, GlobTool};
use aj_tools::{SessionState, ToolDefinition, TurnState};
use std::env;
use std::path::PathBuf;

struct DummySessionState;
impl SessionState for DummySessionState {
    fn working_directory(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

struct DummyTurnState;
impl TurnState for DummyTurnState {}

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    let mut session_state = DummySessionState;
    let turn_state = DummyTurnState;

    match tool.execute(&mut session_state, &turn_state, input) {
        Ok(result) => println!("{}", result),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}
