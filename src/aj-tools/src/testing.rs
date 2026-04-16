//! Testing utilities for aj-tools.

use std::path::PathBuf;

use crate::tools::todo::TodoItem;
use crate::{SessionContext, TurnContext};
use aj_ui::{AjUi, TokenUsage, UsageSummary};

/// A dummy implementation of SessionContext for testing and CLI tools.
pub struct DummySessionContext;

impl SessionContext for DummySessionContext {
    fn working_directory(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

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

/// A dummy implementation of AjUi for testing and CLI tools.
pub struct DummyPermissionHandler;

impl AjUi for DummyPermissionHandler {
    fn agent_text_start(&mut self, _text: &str) {}
    fn agent_text_update(&mut self, _diff: &str) {}
    fn agent_text_stop(&mut self, _text: &str) {}

    fn user_text_start(&mut self, _text: &str) {}
    fn user_text_update(&mut self, _diff: &str) {}
    fn user_text_stop(&mut self, _text: &str) {}

    fn agent_thinking_start(&mut self, _thinking: &str) {}
    fn agent_thinking_update(&mut self, _diff: &str) {}
    fn agent_thinking_stop(&mut self) {}

    fn display_notice(&mut self, _notice: &str) {}
    fn display_warning(&mut self, _warning: &str) {}
    fn display_error(&mut self, _error: &str) {}

    fn ask_permission(&mut self, _message: &str) -> bool {
        // Always grant permission in dummy implementation
        true
    }

    fn display_tool_result(&mut self, _tool_name: &str, _input: &str, _output: &str) {}
    fn display_tool_result_diff(
        &mut self,
        _tool_name: &str,
        _input: &str,
        _before: &str,
        _after: &str,
    ) {
    }
    fn display_tool_error(&mut self, _tool_name: &str, _input: &str, _error: &str) {}

    fn display_token_usage(&mut self, _usage: &TokenUsage) {}
    fn display_token_usage_summary(&mut self, _summary: &UsageSummary) {}

    fn get_subagent_ui(&mut self, _agent_number: usize) -> Box<dyn AjUi> {
        Box::new(DummyPermissionHandler)
    }

    fn shallow_clone(&mut self) -> Box<dyn AjUi> {
        Box::new(DummyPermissionHandler)
    }

    fn get_user_input(&mut self) -> Option<String> {
        None
    }
}
