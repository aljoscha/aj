//! Testing utilities for aj-tools.
//!
//! Provides no-op implementations of [`SessionContext`], [`TurnContext`],
//! and [`AjUi`] so individual tools can be exercised from CLI bins
//! and integration tests without standing up a full agent runtime.
//!
//! Also includes [`DummyToolContext`], the new-shape [`ToolContext`]
//! analogue, for testing tools that have migrated to
//! [`aj_agent::tool::ToolDefinition`].

use std::path::PathBuf;

use crate::{AjUi, SessionContext, TokenUsage, TurnContext, UsageSummary};
use aj_agent::tool::{TodoItem, ToolContext, ToolDetails};
use tokio_util::sync::CancellationToken;

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

/// No-op [`ToolContext`] for exercising new-shape
/// [`aj_agent::tool::ToolDefinition`] implementations from unit tests.
///
/// `working_directory` defaults to the process's `cwd`; override the
/// public field if a test needs a fixed directory. Todos are stored
/// in-memory; `spawn_agent` returns an error; `emit_update` discards
/// snapshots; `cancellation` returns a fresh token that never fires.
pub struct DummyToolContext {
    /// Working directory returned by [`ToolContext::working_directory`].
    pub working_directory: PathBuf,
    /// Backing storage for [`ToolContext::get_todo_list`] /
    /// [`ToolContext::set_todo_list`].
    pub todos: Vec<TodoItem>,
    /// Cancellation token surfaced by [`ToolContext::cancellation`].
    pub cancellation: CancellationToken,
}

impl Default for DummyToolContext {
    fn default() -> Self {
        Self {
            working_directory: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            todos: Vec::new(),
            cancellation: CancellationToken::new(),
        }
    }
}

impl ToolContext for DummyToolContext {
    fn working_directory(&self) -> PathBuf {
        self.working_directory.clone()
    }

    fn get_todo_list(&self) -> Vec<TodoItem> {
        self.todos.clone()
    }

    fn set_todo_list(&mut self, todos: Vec<TodoItem>) {
        self.todos = todos;
    }

    fn spawn_agent<'a>(
        &'a mut self,
        _task: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'a>>
    {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "spawn_agent not supported in DummyToolContext"
            ))
        })
    }

    fn emit_update(&mut self, _partial: ToolDetails) {
        // No-op: no production listener consumes ToolExecutionUpdate yet.
    }

    fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}
