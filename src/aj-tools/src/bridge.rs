//! Bridge from the new [`aj_agent::tool::ToolDefinition`] shape onto
//! the legacy [`aj_agent::legacy_tool::ToolDefinition`] shape.
//!
//! `docs/aj-next-plan.md` §2.2 migrates each builtin off the legacy
//! `&mut dyn AjUi` contract one at a time. The agent runtime in
//! `aj-agent` still drives tools via the legacy [`ErasedToolDefinition`]
//! collection until §2.3 wires the bus into rendering and §2.4 deletes
//! the legacy contract entirely. While that work is in flight, this
//! module lets a migrated tool present the new structured outcome to
//! the agent without forcing a synchronized rewrite of every call
//! site.
//!
//! Two pieces:
//!
//! - [`LegacyContextBridge`] adapts an `&mut dyn legacy::SessionContext`
//!   into an `&mut dyn tool::ToolContext`. Cancellation is a fresh,
//!   never-fired token (legacy doesn't surface one); `emit_update` is
//!   a no-op (no listener consumes [`crate::tool::ToolDetails`]
//!   updates yet); `spawn_agent` forwards.
//! - [`BridgedTool`] wraps a `T: tool::ToolDefinition` and implements
//!   [`legacy::ToolDefinition`] for it. `execute` builds the bridge
//!   context, calls the inner tool, then renders [`ToolDetails`] back
//!   onto the legacy `AjUi` so the existing CLI keeps printing tool
//!   results during the migration window.
//!
//! Once the bus drives rendering, both pieces — and this module —
//! disappear.

use std::path::PathBuf;
use std::pin::Pin;

use aj_agent::legacy_tool::{
    AjUi, ErasedToolDefinition, SessionContext, ToolDefinition as LegacyToolDefinition,
    ToolResult as LegacyToolResult, TurnContext as LegacyTurnContext,
};
use aj_agent::tool::{
    SpawnedAgent, TodoItem, ToolContext, ToolDefinition as NewToolDefinition, ToolDetails,
};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Adapter that exposes a legacy [`SessionContext`] as a new
/// [`ToolContext`] for migrated tools.
///
/// The bridge owns a never-fired [`CancellationToken`] because the
/// legacy contract doesn't carry one; once the agent plumbs
/// cancellation through [`SessionContext`] it can be surfaced here.
/// `emit_update` is intentionally a no-op: no production listener
/// consumes [`crate::tool::ToolDetails`] updates today, and tools
/// that emit progress (today only `bash`) tolerate a swallowed update.
pub struct LegacyContextBridge<'a> {
    session_ctx: &'a mut dyn SessionContext,
    cancellation: CancellationToken,
}

impl<'a> LegacyContextBridge<'a> {
    /// Wrap a legacy session context for use with a new-shape tool.
    pub fn new(session_ctx: &'a mut dyn SessionContext) -> Self {
        Self {
            session_ctx,
            cancellation: CancellationToken::new(),
        }
    }
}

impl<'a> ToolContext for LegacyContextBridge<'a> {
    fn working_directory(&self) -> PathBuf {
        self.session_ctx.working_directory()
    }

    fn get_todo_list(&self) -> Vec<TodoItem> {
        self.session_ctx.get_todo_list()
    }

    fn set_todo_list(&mut self, todos: Vec<TodoItem>) {
        self.session_ctx.set_todo_list(todos);
    }

    fn spawn_agent<'b>(
        &'b mut self,
        task: String,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<SpawnedAgent>> + Send + 'b>> {
        self.session_ctx.spawn_agent(task)
    }

    fn emit_update(&mut self, _partial: ToolDetails) {
        // Tool-update streaming lands once the bus drives rendering;
        // see `docs/aj-next-plan.md` §1.3 and §6 ("tool-update
        // streaming"). Until then, swallow the snapshot — `bash` is
        // the only intended caller and it tolerates this.
    }

    fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

/// Wraps a new-shape [`NewToolDefinition`] so it can be plugged into
/// the agent's legacy [`ErasedToolDefinition`] collection.
///
/// Use [`legacy_adapt`] in tool catalogs (`get_builtin_tools`) rather
/// than spelling this out:
///
/// ```ignore
/// vec![
///     legacy_adapt(ReadFileTool), // migrated
///     LegacyTool.into(),           // not yet migrated
/// ]
/// ```
#[derive(Clone)]
pub struct BridgedTool<T>(pub T);

impl<T> LegacyToolDefinition for BridgedTool<T>
where
    T: NewToolDefinition + Clone + Send + Sync + 'static,
    T::Input: JsonSchema + DeserializeOwned + Send,
{
    type Input = T::Input;

    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn description(&self) -> &'static str {
        self.0.description()
    }

    fn input_schema(&self) -> Value {
        self.0.input_schema()
    }

    fn execute(
        &self,
        session_ctx: &mut dyn SessionContext,
        _turn_ctx: &mut dyn LegacyTurnContext,
        ui: &mut dyn AjUi,
        input: Self::Input,
    ) -> impl std::future::Future<Output = Result<LegacyToolResult, anyhow::Error>> + Send {
        let inner = self.0.clone();
        let tool_name = self.0.name();
        async move {
            let mut ctx = LegacyContextBridge::new(session_ctx);
            let outcome = inner.execute(&mut ctx, input).await?;
            // Render the structured details onto the legacy UI so the
            // current CLI keeps printing tool output. After §2.3 the
            // bus listener takes over and this fans out via the renderer.
            render_details_via_ui(ui, tool_name, &outcome.details, outcome.is_error);
            Ok(LegacyToolResult::from_outcome(outcome))
        }
    }
}

/// Adapt a new-shape tool into a legacy [`ErasedToolDefinition`] in
/// one call. Equivalent to `BridgedTool(tool).into()` but reads better
/// at the catalog site.
pub fn legacy_adapt<T>(tool: T) -> ErasedToolDefinition
where
    T: NewToolDefinition + Clone + Send + Sync + 'static,
{
    BridgedTool(tool).into()
}

/// Render a [`ToolDetails`] payload onto the legacy [`AjUi`].
///
/// Each variant maps onto the closest matching `display_tool_*` call.
/// `Diff` errors fall back to a textual error display because there
/// is no `display_tool_error_diff`. Variants that don't have a
/// dedicated legacy renderer (`Bash`, `SubAgentReport`, `Todos`,
/// `Json`) come through as a flattened `display_tool_result` — those
/// tools haven't migrated yet and won't take this path until they do.
fn render_details_via_ui(
    ui: &mut dyn AjUi,
    tool_name: &str,
    details: &ToolDetails,
    is_error: bool,
) {
    match details {
        ToolDetails::Text { summary, body } => {
            if is_error {
                ui.display_tool_error(tool_name, summary, body);
            } else {
                ui.display_tool_result(tool_name, summary, body);
            }
        }
        ToolDetails::Diff {
            path,
            before,
            after,
        } => {
            if is_error {
                ui.display_tool_error(tool_name, path, "diff failed");
            } else {
                ui.display_tool_result_diff(tool_name, path, before, after);
            }
        }
        ToolDetails::Bash {
            command,
            stdout,
            stderr,
            exit_code,
            ..
        } => {
            let mut body = String::new();
            if !stdout.is_empty() {
                body.push_str(stdout);
            }
            if !stderr.is_empty() {
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(stderr);
            }
            if let Some(code) = exit_code {
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(&format!("[exit {code}]"));
            }
            if is_error {
                ui.display_tool_error(tool_name, command, &body);
            } else {
                ui.display_tool_result(tool_name, command, &body);
            }
        }
        ToolDetails::SubAgentReport {
            agent_id,
            task,
            report,
        } => {
            let header = format!("sub-agent {agent_id}: {task}");
            if is_error {
                ui.display_tool_error(tool_name, &header, report);
            } else {
                ui.display_tool_result(tool_name, &header, report);
            }
        }
        ToolDetails::Todos { items } => {
            // Reuse the canonical text rendering from the `todo` tool
            // module so the legacy CLI display matches what the
            // pre-migration code produced.
            let body = crate::tools::todo::format_todo_list(items);
            if is_error {
                ui.display_tool_error(tool_name, "", &body);
            } else {
                ui.display_tool_result(tool_name, "", &body);
            }
        }
        ToolDetails::Json(value) => {
            let body = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
            if is_error {
                ui.display_tool_error(tool_name, "", &body);
            } else {
                ui.display_tool_result(tool_name, "", &body);
            }
        }
    }
}
