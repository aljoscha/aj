use aj_tools::tools::grep::{GrepInput, GrepTool};
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

    let input = GrepInput { path, include, pattern };
    let tool = GrepTool;
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
