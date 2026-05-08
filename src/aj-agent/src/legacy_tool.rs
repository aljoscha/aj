//! Legacy tool contract — bridge surface used by the existing
//! [`crate::Agent`] runtime and the `aj-tools` crate while we migrate
//! to the event-driven contract in [`crate::tool`].
//!
//! The shapes here mirror the trait that used to live in
//! `aj-tools/src/lib.rs`. They keep `&mut dyn AjUi` alive on the tool
//! signature so the existing CLI rendering and tool implementations
//! continue to work unchanged through Phase 0. The per-tool migration
//! to [`crate::tool::ToolDefinition`] / [`crate::tool::ToolContext`]
//! happens in §2.2 of `docs/aj-next-plan.md`; once every tool moves
//! and the bus drives rendering, this module disappears.
//!
//! Centralizing these types here lets `aj-tools` depend only on
//! `aj-agent` (per the §2.0(c) dependency flip) while still presenting
//! the same legacy trait to callers via re-exports.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

// Tools still talk to the user through `AjUi`; expose the trait
// (and the rendering payloads it carries) from here so downstream
// crates can `use aj_agent::legacy_tool::{AjUi, TokenUsage, ...}`
// without needing a direct `aj-ui` dependency.
pub use aj_ui::{AjUi, TokenUsage, UsageSummary, UserOutput};

use crate::tool::{derive_schema, TodoItem};

/// Result of executing a legacy tool.
///
/// Carries the textual `return_value` that gets folded into the wire
/// `tool_result` message, plus an optional list of `UserOutput`s that
/// the persistence and rendering layers consume. New code should
/// prefer [`crate::tool::ToolOutcome`].
pub struct ToolResult {
    /// Textual return value that flows back to the model as the tool
    /// result content.
    pub return_value: String,
    /// User-facing outputs accumulated during execution.
    pub user_outputs: Vec<aj_ui::UserOutput>,
}

impl ToolResult {
    /// Construct a [`ToolResult`] with just a textual return value and
    /// no user outputs.
    pub fn new(return_value: String) -> Self {
        Self {
            return_value,
            user_outputs: Vec::new(),
        }
    }

    /// Construct a [`ToolResult`] with a textual return value and a
    /// pre-populated list of user outputs.
    pub fn with_outputs(return_value: String, user_outputs: Vec<aj_ui::UserOutput>) -> Self {
        Self {
            return_value,
            user_outputs,
        }
    }
}

/// Legacy tool trait.
///
/// Implementors receive a [`SessionContext`], a [`TurnContext`], and
/// `&mut dyn AjUi` for live rendering. Returns a [`ToolResult`] whose
/// `return_value` becomes the wire `tool_result` content.
pub trait ToolDefinition {
    /// Strongly-typed input shape, parsed from the model's JSON
    /// arguments before [`Self::execute`] runs.
    type Input: JsonSchema + DeserializeOwned + Send;

    /// Tool name advertised to the model.
    fn name(&self) -> &'static str;

    /// Free-text description shown to the model.
    fn description(&self) -> &'static str;

    /// Execute the tool.
    fn execute(
        &self,
        session_ctx: &mut dyn SessionContext,
        turn_ctx: &mut dyn TurnContext,
        ui: &mut dyn AjUi,
        input: Self::Input,
    ) -> impl std::future::Future<Output = Result<ToolResult, anyhow::Error>> + Send;

    /// JSON Schema for [`Self::Input`]. Defaults to
    /// [`crate::tool::derive_schema`].
    fn input_schema(&self) -> Value {
        derive_schema::<Self::Input>()
    }
}

/// Boxed-future signature stored inside [`ErasedToolDefinition`].
///
/// Wrapped in `Arc` so the whole [`ErasedToolDefinition`] is cheaply
/// cloneable; the agent uses that to hand a sub-agent the same tool
/// list as its parent without re-running [`get_builtin_tools`]
/// (which lives in `aj-tools` and is therefore unreachable from
/// `aj-agent` after the dependency flip).
pub type ToolFn = Arc<
    dyn for<'a> Fn(
            &'a mut dyn SessionContext,
            &'a mut dyn TurnContext,
            &'a mut dyn AjUi,
            Value,
        ) -> Pin<
            Box<dyn std::future::Future<Output = Result<ToolResult, anyhow::Error>> + Send + 'a>,
        > + Send
        + Sync,
>;

/// Type-erased legacy tool definition.
///
/// Cloneable: every field is `Clone` and `func` is held behind an
/// `Arc` so cloning just bumps a refcount. Sub-agent spawning relies
/// on this to copy the parent's toolset minus the `agent` tool.
#[derive(Clone)]
pub struct ErasedToolDefinition {
    /// Tool name advertised to the model.
    pub name: String,
    /// Free-text description shown to the model.
    pub description: String,
    /// JSON Schema for the tool's input.
    pub input_schema: Value,
    /// Type-erased execution closure.
    pub func: ToolFn,
}

impl<T: ToolDefinition + Send + Sync + Clone + 'static> From<T> for ErasedToolDefinition {
    fn from(tool: T) -> Self {
        ErasedToolDefinition {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            input_schema: tool.input_schema(),
            func: Arc::new(move |session_ctx, turn_ctx, ui, input| {
                let typed_input: T::Input = match serde_json::from_value(input) {
                    Ok(input) => input,
                    Err(e) => return Box::pin(async move { Err(e.into()) }),
                };
                let tool_clone = tool.clone();
                Box::pin(async move {
                    tool_clone
                        .execute(session_ctx, turn_ctx, ui, typed_input)
                        .await
                })
            }),
        }
    }
}

/// Access to state scoped to one agent session or thread.
pub trait SessionContext: Send {
    /// Current working directory for the session.
    fn working_directory(&self) -> PathBuf;

    /// Current todo-list snapshot.
    fn get_todo_list(&self) -> Vec<TodoItem>;

    /// Replace the session's todo list.
    fn set_todo_list(&mut self, todos: Vec<TodoItem>);

    /// Spawn a sub-agent.
    ///
    /// Resolves to the child's final assistant text. The
    /// implementation handles bus / cancellation propagation; tools
    /// only see the resulting string.
    fn spawn_agent(
        &mut self,
        task: String,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, anyhow::Error>> + Send + '_>>;
}

/// Access to state scoped to a single turn through the agent loop.
///
/// Currently empty; reserved for per-turn metadata that today flows
/// through [`crate::TurnContext`].
pub trait TurnContext: Send {}
