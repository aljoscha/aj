//! Testing utilities for aj-tools.
//!
//! Provides a no-op [`ToolContext`] implementation so individual
//! tools can be exercised from CLI bins and integration tests
//! without standing up a full agent runtime.

use std::path::PathBuf;
use std::sync::Arc;

use aj_agent::TaskRegistry;
use aj_agent::bus::EventBus;
use aj_agent::events::AgentId;
use aj_agent::tool::{
    SpawnMode, SpawnResult, StartedTask, TaskEventSink, TaskKind, TaskOutputSource, TodoItem,
    ToolContext, ToolDetails,
};
use tokio_util::sync::CancellationToken;

/// No-op [`ToolContext`] for exercising new-shape
/// [`aj_agent::tool::ToolDefinition`] implementations from unit tests.
///
/// `working_directory` defaults to the process's `cwd`; override the
/// public field if a test needs a fixed directory. Todos are stored
/// in-memory; `spawn_agent` returns an error; `emit_update` discards
/// snapshots; `cancellation` returns a fresh token that never fires.
/// Background tasks register against the (default) `task_registry`
/// and emit on a bus with no subscribers, owned by `agent_id`, which
/// defaults to [`AgentId::Main`].
pub struct DummyToolContext {
    /// Working directory returned by [`ToolContext::working_directory`].
    pub working_directory: PathBuf,
    /// Backing storage for [`ToolContext::get_todo_list`] /
    /// [`ToolContext::set_todo_list`].
    pub todos: Vec<TodoItem>,
    /// Cancellation token surfaced by [`ToolContext::cancellation`].
    pub cancellation: CancellationToken,
    /// Registry surfaced by [`ToolContext::task_registry`] and used
    /// by [`ToolContext::start_background_task`].
    pub task_registry: TaskRegistry,
    /// Bus the task event sinks emit on. Fresh (no subscribers) by
    /// default; tests can subscribe to observe task events.
    pub bus: EventBus,
    /// Identity surfaced by [`ToolContext::agent_id`] and recorded as
    /// the owner of tasks started through this context.
    pub agent_id: AgentId,
}

impl DummyToolContext {
    /// Run as `agent_id` instead of the default [`AgentId::Main`], so
    /// tests can exercise ownership scoping across agents.
    pub fn with_agent_id(mut self, agent_id: AgentId) -> Self {
        self.agent_id = agent_id;
        self
    }
}

impl Default for DummyToolContext {
    fn default() -> Self {
        Self {
            working_directory: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            todos: Vec::new(),
            cancellation: CancellationToken::new(),
            task_registry: TaskRegistry::default(),
            bus: EventBus::new(),
            agent_id: AgentId::Main,
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
        _mode: SpawnMode,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<SpawnResult, aj_agent::BoxError>> + Send + 'a>,
    > {
        Box::pin(async move { Err("spawn_agent not supported in DummyToolContext".into()) })
    }

    fn emit_update<'a>(
        &'a mut self,
        _partial: ToolDetails,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        // No-op: discards snapshots so tools can be exercised without a
        // bus subscriber.
        Box::pin(async {})
    }

    fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    fn task_registry(&self) -> TaskRegistry {
        self.task_registry.clone()
    }

    fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    fn start_background_task(
        &mut self,
        kind: TaskKind,
        label: String,
        output: Arc<dyn TaskOutputSource>,
    ) -> StartedTask {
        let (id, cancel) = self.task_registry.register(
            self.agent_id,
            "dummy-call".to_string(),
            kind,
            label.clone(),
            output,
        );
        let events = TaskEventSink::new(
            self.bus.clone(),
            self.task_registry.clone(),
            self.agent_id,
            id,
            "dummy-call".to_string(),
            label,
        );
        StartedTask { id, cancel, events }
    }
}
