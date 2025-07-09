//! Testing utilities for aj-tools.

use std::path::PathBuf;

use crate::tools::todo::TodoItem;
use crate::{SessionContext, TurnContext};

/// A dummy implementation of SessionContext for testing and CLI tools.
pub struct DummySessionContext;

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
        &mut self,
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

/// A dummy implementation of TurnContext for testing and CLI tools.
pub struct DummyTurnContext;

impl TurnContext for DummyTurnContext {}

