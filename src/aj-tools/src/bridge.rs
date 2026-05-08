//! Render a [`ToolDetails`] payload onto the legacy [`AjUi`].
//!
//! After §2.4a of `docs/aj-next-plan.md` the agent drives tools
//! directly through [`aj_agent::tool::ToolDefinition`] —
//! [`AjUi`]-bound bridge types are gone. What remains is the
//! [`ToolDetails`]-to-`AjUi` projection used by the binary's
//! [`crate::event_bridge::EventBridgeListener`] (conceptually:
//! `aj/src/event_bridge.rs`) so the legacy CLI keeps rendering
//! tool results from `AgentEvent::ToolExecutionEnd` events. The
//! function disappears in §2.6 along with the rest of `aj-ui`.

use aj_agent::tool::ToolDetails;
use aj_ui::AjUi;

/// Render a [`ToolDetails`] payload onto the legacy [`AjUi`].
///
/// Each variant maps onto the closest matching `display_tool_*` call.
/// `Diff` errors fall back to a textual error display because there
/// is no `display_tool_error_diff`. Variants without a dedicated
/// legacy renderer (`Bash`, `SubAgentReport`, `Todos`, `Json`) come
/// through as a flattened `display_tool_result`.
pub fn render_details_via_ui(
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
