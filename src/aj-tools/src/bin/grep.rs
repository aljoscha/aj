use aj_tools::tools::grep::{GrepInput, GrepTool};
use aj_tools::tools::todo::TodoItem;
use aj_tools::{SessionContext, ToolDefinition, TurnContext};
use std::env;
use std::path::PathBuf;

struct DummySessionContext;
impl SessionContext for DummySessionContext {
    fn working_directory(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    fn display_tool_result(&self, _tool_name: &str, _input: &str, _output: &str) {}

    fn display_tool_result_diff(
        &self,
        _tool_name: &str,
        _input: &str,
        _before: &str,
        _after: &str,
    ) {
    }

    fn display_tool_error(&self, _tool_name: &str, _input: &str, _error: &str) {}

    fn get_todo_list(&self) -> Vec<TodoItem> {
        Vec::new()
    }

    fn set_todo_list(&mut self, _todos: Vec<TodoItem>) {
        // No-op for dummy implementation
    }
}

struct DummyTurnContext;
impl TurnContext for DummyTurnContext {}

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
        Ok(result) => println!("{}", result),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}
