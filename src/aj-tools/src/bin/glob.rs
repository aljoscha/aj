use aj_tools::tools::glob::{GlobInput, GlobTool};
use aj_tools::tools::todo::TodoItem;
use aj_tools::{SessionContext, ToolDefinition, TurnContext};
use anyhow;
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

    fn spawn_agent(
        &self,
        _task: String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, anyhow::Error>> + Send + '_>,
    > {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "spawn_agent not supported in dummy implementation"
            ))
        })
    }
}

struct DummyTurnContext;
impl TurnContext for DummyTurnContext {}

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
        Ok(result) => println!("{}", result),
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }

    Ok(())
}
