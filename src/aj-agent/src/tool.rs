//! Tool contract: trait, context, outcome, and structured rendering details.
//!
//! Tools are the agent's external effectors. Each implementation supplies a
//! [`ToolDefinition`] describing its name, schema, and execution; the agent
//! drives them through a [`ToolContext`] reference and folds the resulting
//! [`ToolOutcome`] into both the wire transcript and the typed event
//! stream.
//!
//! See `docs/aj-next-plan.md` §1.2 and §1.3.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use aj_models::types::UserContent;
use schemars::generate::SchemaSettings;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Execution mode
// ---------------------------------------------------------------------------

/// Per-tool execution mode.
///
/// The agent batches all tool calls from a single assistant turn. If any
/// tool in the batch declares [`ExecutionMode::Sequential`], the entire
/// batch runs serially in source order; otherwise tools execute
/// concurrently. Tools that mutate the filesystem or run arbitrary
/// commands should opt into [`ExecutionMode::Sequential`] to avoid
/// interleaving.
///
/// Default: [`ExecutionMode::Parallel`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Forces serial execution of the whole batch when any tool in the
    /// batch picks this mode.
    Sequential,
    /// Eligible for concurrent execution when every tool in the batch
    /// picks this mode.
    Parallel,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        Self::Parallel
    }
}

// ---------------------------------------------------------------------------
// Tool details — closed enum keyed by rendering shape
// ---------------------------------------------------------------------------

/// Structured payload describing a tool result for UI rendering and
/// persistence.
///
/// `ToolDetails` is intentionally a closed enum with one variant per
/// **rendering shape**, not per tool. Multiple tools may map onto the
/// same variant (e.g. `read_file` and `agent` both render as
/// [`ToolDetails::Text`]). The closed shape makes persistence cheap
/// (round-trips through JSONL without per-tool deserialization) and
/// keeps event listeners decoupled from concrete tool implementations.
///
/// New tools whose rendering doesn't fit any variant fall back to
/// [`ToolDetails::Json`]; new dedicated variants are added when a
/// rendering pattern repeats.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolDetails {
    /// Default rendering: a one-line summary plus optional body.
    /// Used by `read_file`, plain `agent` text replies, and anything
    /// without a dedicated variant.
    Text {
        /// Short headline displayed in collapsed views.
        summary: String,
        /// Full body shown when expanded. May be empty.
        body: String,
    },
    /// File-edit rendering: shows a unified diff between `before`
    /// and `after`. Used by `write_file`, `edit_file`,
    /// `edit_file_multi`.
    Diff {
        /// Display path of the edited file.
        path: String,
        /// File content prior to the edit.
        before: String,
        /// File content after the edit.
        after: String,
    },
    /// Command-output rendering for `bash`. `stdout`/`stderr` carry a
    /// bounded rolling tail; if either stream exceeded the cap the
    /// implementation spills the full output to a temp file and
    /// surfaces its path through `full_output_path`.
    Bash {
        /// The exact command line executed.
        command: String,
        /// Captured (and possibly truncated) standard output.
        stdout: String,
        /// Captured (and possibly truncated) standard error.
        stderr: String,
        /// Process exit code, when the process exited normally.
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// True when at least one of `stdout` / `stderr` was truncated.
        #[serde(default)]
        truncated: bool,
        /// Path to a temp file containing the full uncaptured output,
        /// when truncation occurred.
        #[serde(skip_serializing_if = "Option::is_none")]
        full_output_path: Option<PathBuf>,
    },
    /// Sub-agent run report — emitted by the `agent` tool when it
    /// runs as a child agent and returns a final report.
    SubAgentReport {
        /// Index of the sub-agent that produced this report.
        agent_id: usize,
        /// Task description supplied by the parent.
        task: String,
        /// Final assistant text returned by the sub-agent.
        report: String,
    },
    /// Todo-list rendering for `todo_read` / `todo_write`. The
    /// implementations also emit a [`ToolDetails::Text`] summary for
    /// the LLM; this variant is the structured snapshot the UI
    /// renders.
    Todos {
        /// Snapshot of the current todo list.
        items: Vec<TodoItem>,
    },
    /// Escape hatch for tools that don't warrant their own variant.
    /// New tools start here and graduate to a dedicated variant once
    /// a rendering pattern repeats.
    Json(Value),
}

// ---------------------------------------------------------------------------
// Todo item
// ---------------------------------------------------------------------------

/// A single todo entry, used by [`ToolDetails::Todos`] and the
/// `todo_read` / `todo_write` tool implementations.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TodoItem {
    /// Stable identifier within the session.
    pub id: String,
    /// Human-readable description.
    pub content: String,
    /// Priority hint for ordering.
    pub priority: TodoPriority,
    /// Current status.
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TodoPriority {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TodoStatus {
    Todo,
    InProgress,
    Completed,
}

// ---------------------------------------------------------------------------
// Tool outcome
// ---------------------------------------------------------------------------

/// Structured result returned from [`ToolDefinition::execute`].
///
/// The agent splits this into two channels: `content` rides on the
/// wire `tool_result` message back to the model, while `details` is
/// projected onto the [`crate::events::AgentEvent::ToolExecutionEnd`]
/// event for UI rendering and persistence.
#[derive(Clone, Debug)]
pub struct ToolOutcome {
    /// Content sent back to the model as the tool_result message.
    /// Maps directly onto the wire `ToolResultMessage.content`.
    pub content: Vec<UserContent>,
    /// Structured payload for UI rendering. Tool-specific.
    pub details: ToolDetails,
    /// Whether the execution should be reported to the model as an
    /// error tool_result.
    pub is_error: bool,
}

/// Result of [`ToolContext::spawn_agent`].
///
/// Carries enough information for callers (currently the `agent`
/// builtin) to construct a [`ToolDetails::SubAgentReport`] without
/// looking up the freshly-allocated sub-agent id elsewhere. The
/// `agent_id` matches the `Sub(_)` index surfaced on the bus through
/// [`crate::events::AgentEvent::SubAgentStart`] /
/// [`crate::events::AgentEvent::SubAgentEnd`].
#[derive(Clone, Debug)]
pub struct SpawnedAgent {
    /// The sub-agent's id within the current session.
    pub agent_id: usize,
    /// Final assistant text returned by the sub-agent.
    pub report: String,
}

// ---------------------------------------------------------------------------
// Tool trait + erased shape
// ---------------------------------------------------------------------------

/// A typed tool implementation.
///
/// Concrete tools implement this trait with a strongly-typed `Input`
/// and convert into an [`ErasedToolDefinition`] for storage in a
/// heterogeneous collection.
pub trait ToolDefinition: Send + Sync {
    /// The tool's input shape. Deserialized from the model's JSON
    /// arguments.
    type Input: JsonSchema + DeserializeOwned + Send;

    /// The tool name advertised to the model. Must match the name
    /// reported in [`crate::events::AgentEvent::ToolExecutionStart`]
    /// events.
    fn name(&self) -> &'static str;

    /// Free-text description shown to the model.
    fn description(&self) -> &'static str;

    /// JSON Schema for [`Self::Input`]. Default implementation derives
    /// from `JsonSchema`.
    fn input_schema(&self) -> Value {
        derive_schema::<Self::Input>()
    }

    /// Per-tool execution mode. Default [`ExecutionMode::Parallel`].
    /// Tools that mutate the filesystem or run arbitrary commands
    /// should override to [`ExecutionMode::Sequential`].
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::default()
    }

    /// Run the tool. Errors should be surfaced as `is_error: true`
    /// outcomes when the model can recover; bubbling up an `Err`
    /// causes the agent to synthesize a generic error tool_result
    /// and abort the turn.
    fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> impl Future<Output = anyhow::Result<ToolOutcome>> + Send;
}

/// Boxed-future signature used by [`ErasedToolDefinition::func`].
///
/// Held behind an `Arc` so [`ErasedToolDefinition`] is cheaply
/// cloneable: the agent clones the parent's tool list for each
/// sub-agent it spawns (filtered to drop the `agent` tool itself),
/// and bumping a refcount per tool keeps that path allocation-free.
pub type ErasedToolFn = Arc<
    dyn for<'a> Fn(
            &'a mut dyn ToolContext,
            Value,
        )
            -> Pin<Box<dyn Future<Output = anyhow::Result<ToolOutcome>> + Send + 'a>>
        + Send
        + Sync,
>;

/// Type-erased tool definition for working with heterogeneous
/// collections of tools (the agent stores `Vec<ErasedToolDefinition>`).
///
/// Convert from a [`ToolDefinition`] via the blanket
/// `From<T> for ErasedToolDefinition` impl.
#[derive(Clone)]
pub struct ErasedToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub execution_mode: ExecutionMode,
    pub func: ErasedToolFn,
}

impl<T> From<T> for ErasedToolDefinition
where
    T: ToolDefinition + Clone + 'static,
{
    fn from(tool: T) -> Self {
        let name = tool.name().to_string();
        let description = tool.description().to_string();
        let input_schema = tool.input_schema();
        let execution_mode = tool.execution_mode();
        ErasedToolDefinition {
            name,
            description,
            input_schema,
            execution_mode,
            func: Arc::new(move |ctx, raw_input| {
                let parsed: Result<T::Input, _> = serde_json::from_value(raw_input);
                let tool = tool.clone();
                Box::pin(async move {
                    let input = parsed?;
                    tool.execute(ctx, input).await
                })
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool context
// ---------------------------------------------------------------------------

/// Runtime context passed to [`ToolDefinition::execute`].
///
/// Provides access to agent-scoped state (working directory, todo
/// list), sub-agent spawning, progress updates, and a cancellation
/// token tools must honor for long-running work.
pub trait ToolContext: Send {
    /// Current working directory for the session.
    fn working_directory(&self) -> PathBuf;

    /// Current todo list snapshot.
    fn get_todo_list(&self) -> Vec<TodoItem>;

    /// Replace the session's todo list.
    fn set_todo_list(&mut self, todos: Vec<TodoItem>);

    /// Spawn a sub-agent on the current bus.
    ///
    /// Resolves to a [`SpawnedAgent`] carrying the freshly-allocated
    /// sub-agent id and the child's final assistant text. The child
    /// shares the parent's event bus tagged with a fresh
    /// [`crate::events::AgentId::Sub`] and inherits a child
    /// cancellation token derived from the parent's
    /// [`Self::cancellation`].
    fn spawn_agent<'a>(
        &'a mut self,
        task: String,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<SpawnedAgent>> + Send + 'a>>;

    /// Emit a partial [`ToolDetails`] snapshot through the bus as a
    /// [`crate::events::AgentEvent::ToolExecutionUpdate`]. Tools that
    /// produce many updates self-throttle (~10/s); the agent does not
    /// debounce.
    fn emit_update(&mut self, partial: ToolDetails);

    /// Cancellation token tools must observe for long-running work.
    /// Cancellation propagates from `Agent::cancel` and from a parent
    /// agent's cancellation when this is a sub-agent.
    fn cancellation(&self) -> CancellationToken;
}

// ---------------------------------------------------------------------------
// Schema helper
// ---------------------------------------------------------------------------

/// Derive a JSON schema suitable for use as a tool's `input_schema`.
///
/// Strips the `title` and ensures `properties` / `required` are
/// always present so the resulting object validates as a function
/// parameter schema for both Anthropic and OpenAI.
pub fn derive_schema<T: JsonSchema>() -> Value {
    let generator = SchemaSettings::default()
        .with(|s| {
            // The meta schema link bloats the output without helping
            // the model.
            s.meta_schema = None;
        })
        .into_generator();
    let mut schema = generator.into_root_schema_for::<T>();

    // The title is just the type name and adds noise.
    schema.remove("title");

    if schema.get("properties").is_none() {
        schema.insert("properties".to_string(), json!({}));
    }
    if schema.get("required").is_none() {
        schema.insert("required".to_string(), json!([]));
    }

    serde_json::to_value(&schema).expect("invalid schema object")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_details_round_trips_each_variant() {
        // The persistence listener writes ToolDetails alongside each
        // tool_result message; this locks the {"kind": "..."} framing
        // so the upcoming `aj-session` migration walker can rely on
        // a stable shape.
        let cases = [
            ToolDetails::Text {
                summary: "hi".into(),
                body: "body".into(),
            },
            ToolDetails::Diff {
                path: "a.txt".into(),
                before: "old".into(),
                after: "new".into(),
            },
            ToolDetails::Bash {
                command: "ls".into(),
                stdout: "out".into(),
                stderr: String::new(),
                exit_code: Some(0),
                truncated: false,
                full_output_path: None,
            },
            ToolDetails::SubAgentReport {
                agent_id: 1,
                task: "do thing".into(),
                report: "done".into(),
            },
            ToolDetails::Todos { items: Vec::new() },
            ToolDetails::Json(json!({"x": 1})),
        ];
        for case in cases {
            let json = serde_json::to_value(&case).expect("serialize");
            assert!(json.get("kind").is_some(), "missing kind tag: {json}");
            let _back: ToolDetails = serde_json::from_value(json).expect("deserialize round trip");
        }
    }

    #[test]
    fn execution_mode_default_is_parallel() {
        assert_eq!(ExecutionMode::default(), ExecutionMode::Parallel);
    }
}
