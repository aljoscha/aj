//! `todo_read` / `todo_write` builtins — session-scoped task list.
//!
//! Migrated to [`aj_agent::tool::ToolDefinition`] per
//! `docs/aj-next-plan.md` §2.2. Both tools return a [`ToolOutcome`]
//! whose `details` is [`ToolDetails::Todos`] (the structured snapshot
//! the UI renders) and whose `content` is a text rendering of the
//! list (what the LLM reads on the wire). The structured payload
//! lets renderers / persistence consumers reflect status changes
//! without re-parsing the text body.
//!
//! Validation errors in `todo_write` (more than one in-progress item)
//! come back as `is_error: true` outcomes with a [`ToolDetails::Text`]
//! describing the violation, matching the recoverable-error pattern
//! used by the other migrated tools (see `read_file`, `ls`, `glob`,
//! `grep`). The model can correct its call without aborting the turn.

use aj_agent::tool::{ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// Re-export the canonical todo types from `aj-agent` so external
// callers (and the new `aj_agent::tool::ToolDetails::Todos` shape)
// share one definition. The legacy on-disk and wire-schema rendering
// is identical: `TodoStatus` uses `kebab-case`, `TodoPriority` uses
// `lowercase`.
pub use aj_agent::tool::{TodoItem, TodoPriority, TodoStatus};

/// Render a list of todo items into the human-friendly text shape
/// the LLM and the legacy CLI both consume.
///
/// Public so the legacy bridge in [`crate::bridge`] can use the same
/// output when rendering [`ToolDetails::Todos`] through the existing
/// `display_tool_result` channel; once the bus drives rendering
/// (§2.3+), this helper stays here as the canonical text projection
/// for the wire content.
pub fn format_todo_list(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "TODO list is empty.".to_string();
    }

    let mut result = String::new();

    for todo in todos {
        let status_symbol = match todo.status {
            TodoStatus::Todo => "-",
            TodoStatus::InProgress => "›",
            TodoStatus::Completed => "✓",
        };

        let priority_text = match todo.priority {
            TodoPriority::High => " (high)",
            TodoPriority::Medium => " (medium)",
            TodoPriority::Low => " (low)",
        };

        // Sanitise the content before wrapping it with styling so any
        // stray ANSI / carriage-return / control bytes the model may
        // have placed in the todo can't corrupt the renderer's width
        // math or the model's own context the next time it reads the
        // list back. The strikethrough SGR added below survives
        // because it's emitted *after* sanitisation.
        let content = crate::sanitize::sanitize_terminal_output(&todo.content);

        // Strike through completed items via the SGR strikethrough
        // attribute (`ESC[9m` on, `ESC[29m` off). Renderers that
        // strip ANSI fall back to the plain text content; renderers
        // that honor it dim the entry visually.
        let content = if todo.status == TodoStatus::Completed {
            format!("\x1b[9m{}\x1b[29m", content)
        } else {
            content
        };

        result.push_str(&format!("{status_symbol} {content}{priority_text}\n"));
    }

    result
}

const TODO_READ_DESCRIPTION: &str = r#"
Read the current todo list for the session.

WHEN TO USE:
- Use VERY frequently to track tasks and give user visibility into progress
- For complex multi-step tasks (more than 3 steps)
- NOT for trivial tasks or conversational interactions

USAGE GUIDELINES:
- Use extensively both for planning and tracking progress
- Check the list frequently to see current status
- Essential for maintaining task visibility and avoiding forgotten work
"#;

const TODO_WRITE_DESCRIPTION: &str = r#"
Update the todo list for the current session. To be used proactively and often
to track progress and pending tasks.

WHEN TO USE:
- Use VERY frequently - EXTREMELY helpful for planning and breaking down complex tasks
- For multi-step tasks (more than 3 steps)
- NOT for trivial tasks or conversational interactions
- CRITICAL: If you don't use this when planning, you may forget important tasks

USAGE GUIDELINES:
- Mark todos as completed IMMEDIATELY when done - don't batch completions
- Only one TODO item can be "in-progress" at a time, others must be "todo" or "completed"
- Use extensively for task planning and progress tracking
- Break larger complex tasks into smaller manageable steps
- Update frequently to maintain visibility into progress

WORKFLOW:
1. Plan task by writing initial TODO list
2. Mark items "in-progress" as you work on them
3. Mark "completed" as soon as each task is finished
4. Update the list frequently throughout the work
"#;

#[derive(Clone)]
pub struct TodoReadTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct TodoReadInput {}

impl ToolDefinition for TodoReadTool {
    type Input = TodoReadInput;

    fn name(&self) -> &'static str {
        "todo_read"
    }

    fn description(&self) -> &'static str {
        TODO_READ_DESCRIPTION
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        _input: Self::Input,
    ) -> anyhow::Result<ToolOutcome> {
        // Snapshot the session's current todo list. The wire content
        // is the text rendering the LLM reads; `details::Todos` is the
        // structured snapshot the UI / persistence consumers use.
        let items = ctx.get_todo_list();
        let formatted = format_todo_list(&items);

        Ok(ToolOutcome {
            content: vec![UserContent::text(formatted)],
            details: ToolDetails::Todos { items },
            is_error: false,
        })
    }
}

#[derive(Clone)]
pub struct TodoWriteTool;

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct TodoWriteInput {
    /// The list of todo items. This replaces any existing todos.
    pub todos: Vec<TodoItem>,
}

impl ToolDefinition for TodoWriteTool {
    type Input = TodoWriteInput;

    fn name(&self) -> &'static str {
        "todo_write"
    }

    fn description(&self) -> &'static str {
        TODO_WRITE_DESCRIPTION
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> anyhow::Result<ToolOutcome> {
        // Validate that there's at most one in-progress item. Returning
        // a recoverable `is_error: true` outcome (instead of bubbling
        // an `Err`) lets the model correct its call without aborting
        // the turn — matches the pattern in `read_file`/`ls`/`glob`/
        // `grep` for input validation.
        let in_progress_count = input
            .todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::InProgress)
            .count();

        if in_progress_count > 1 {
            let msg = format!(
                "Only one TODO item can be in progress at a time, found {in_progress_count}"
            );
            return Ok(ToolOutcome {
                content: vec![UserContent::text(msg.clone())],
                details: ToolDetails::Text {
                    summary: "todo_write: validation error".to_string(),
                    body: msg,
                },
                is_error: true,
            });
        }

        // Replace the session's todo list with the new snapshot. The
        // structured `Todos { items }` payload is the post-update
        // state; the wire content surfaces both a confirmation line
        // and the rendered list so the model has full context.
        ctx.set_todo_list(input.todos.clone());

        let update_result = format!("Updated todo list with {} items.", input.todos.len());
        let formatted_todos = format_todo_list(&input.todos);
        let wire_text = format!("{update_result}\n{formatted_todos}");

        Ok(ToolOutcome {
            content: vec![UserContent::text(wire_text)],
            details: ToolDetails::Todos { items: input.todos },
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::DummyToolContext;

    fn extract_text(content: &[UserContent]) -> String {
        content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text.as_str()),
                UserContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn item(id: &str, content: &str, status: TodoStatus, priority: TodoPriority) -> TodoItem {
        TodoItem {
            id: id.to_string(),
            content: content.to_string(),
            status,
            priority,
        }
    }

    /// `todo_read` on an empty session returns the `Todos` snapshot
    /// alongside the canonical empty-list text body.
    #[tokio::test]
    async fn todo_read_returns_empty_snapshot() {
        let mut ctx = DummyToolContext::default();
        let outcome = TodoReadTool
            .execute(&mut ctx, TodoReadInput {})
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("TODO list is empty"),
            "wire content: {wire:?}"
        );

        match outcome.details {
            ToolDetails::Todos { items } => assert!(items.is_empty(), "items: {items:?}"),
            other => panic!("expected Todos details, got {other:?}"),
        }
    }

    /// A populated todo list round-trips through both channels: the
    /// wire content carries the pretty text rendering and `details`
    /// carries the structured snapshot the UI uses.
    #[tokio::test]
    async fn todo_read_returns_populated_snapshot() {
        let mut ctx = DummyToolContext::default();
        ctx.todos = vec![
            item(
                "1",
                "write spec",
                TodoStatus::InProgress,
                TodoPriority::High,
            ),
            item("2", "ship feature", TodoStatus::Todo, TodoPriority::Medium),
        ];

        let outcome = TodoReadTool
            .execute(&mut ctx, TodoReadInput {})
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        let wire = extract_text(&outcome.content);
        assert!(wire.contains("write spec"), "wire content: {wire:?}");
        assert!(wire.contains("ship feature"), "wire content: {wire:?}");
        // The status/priority markers are part of the canonical text
        // rendering — locking them here keeps a regression visible.
        assert!(wire.contains("(high)"), "wire content: {wire:?}");
        assert!(wire.contains("(medium)"), "wire content: {wire:?}");

        match outcome.details {
            ToolDetails::Todos { items } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].content, "write spec");
                assert_eq!(items[0].status, TodoStatus::InProgress);
                assert_eq!(items[0].priority, TodoPriority::High);
                assert_eq!(items[1].content, "ship feature");
                assert_eq!(items[1].status, TodoStatus::Todo);
            }
            other => panic!("expected Todos details, got {other:?}"),
        }
    }

    /// `todo_write` replaces the session's list and surfaces the new
    /// snapshot through both channels.
    #[tokio::test]
    async fn todo_write_replaces_list_and_returns_snapshot() {
        let mut ctx = DummyToolContext::default();
        ctx.todos = vec![item("old", "stale", TodoStatus::Todo, TodoPriority::Low)];

        let new_todos = vec![
            item("a", "first", TodoStatus::InProgress, TodoPriority::High),
            item("b", "second", TodoStatus::Completed, TodoPriority::Low),
        ];

        let outcome = TodoWriteTool
            .execute(
                &mut ctx,
                TodoWriteInput {
                    todos: new_todos.clone(),
                },
            )
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        // The session list now matches the request; the prior
        // contents were replaced wholesale.
        assert_eq!(ctx.todos.len(), 2);
        assert_eq!(ctx.todos[0].id, "a");
        assert_eq!(ctx.todos[1].id, "b");

        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("Updated todo list with 2 items."),
            "wire content: {wire:?}"
        );
        assert!(wire.contains("first"), "wire content: {wire:?}");
        assert!(wire.contains("second"), "wire content: {wire:?}");

        match outcome.details {
            ToolDetails::Todos { items } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].id, "a");
                assert_eq!(items[1].id, "b");
            }
            other => panic!("expected Todos details, got {other:?}"),
        }
    }

    /// Writing an empty list still returns a `Todos { items: [] }`
    /// snapshot — the wire content carries the confirmation line plus
    /// the empty-list marker.
    #[tokio::test]
    async fn todo_write_accepts_empty_list() {
        let mut ctx = DummyToolContext::default();
        ctx.todos = vec![item("x", "leftover", TodoStatus::Todo, TodoPriority::Low)];

        let outcome = TodoWriteTool
            .execute(&mut ctx, TodoWriteInput { todos: vec![] })
            .await
            .expect("execute");

        assert!(!outcome.is_error);
        assert!(ctx.todos.is_empty());

        let wire = extract_text(&outcome.content);
        assert!(
            wire.contains("Updated todo list with 0 items."),
            "wire content: {wire:?}"
        );
        assert!(
            wire.contains("TODO list is empty"),
            "wire content: {wire:?}"
        );

        match outcome.details {
            ToolDetails::Todos { items } => assert!(items.is_empty(), "items: {items:?}"),
            other => panic!("expected Todos details, got {other:?}"),
        }
    }

    /// More than one `in-progress` item is a recoverable validation
    /// error: the outcome carries `is_error: true` with `Text`
    /// details so the model can correct its call without aborting
    /// the turn. The session's list is left untouched.
    #[tokio::test]
    async fn todo_write_rejects_multiple_in_progress() {
        let mut ctx = DummyToolContext::default();
        let original = vec![item("keep", "keep me", TodoStatus::Todo, TodoPriority::Low)];
        ctx.todos = original.clone();

        let outcome = TodoWriteTool
            .execute(
                &mut ctx,
                TodoWriteInput {
                    todos: vec![
                        item("a", "first", TodoStatus::InProgress, TodoPriority::High),
                        item("b", "second", TodoStatus::InProgress, TodoPriority::Medium),
                    ],
                },
            )
            .await
            .expect("execute");

        assert!(outcome.is_error);
        // The session's list is unchanged when validation fails.
        assert_eq!(ctx.todos.len(), original.len());
        assert_eq!(ctx.todos[0].id, "keep");

        match outcome.details {
            ToolDetails::Text { body, .. } => {
                assert!(
                    body.contains("Only one TODO item can be in progress"),
                    "body: {body:?}"
                );
                assert!(body.contains("found 2"), "body: {body:?}");
            }
            other => panic!("expected Text details on validation error, got {other:?}"),
        }
    }
}
