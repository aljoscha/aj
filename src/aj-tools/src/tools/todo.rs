use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use termimad::crossterm::style::{Attribute, Stylize};

use crate::{SessionState, ToolDefinition, TurnState};

/// Formats a list of todo items for display
fn format_todo_list(todos: &[TodoItem]) -> String {
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

        let content = if todo.status == TodoStatus::Completed {
            format!("{}", todo.content.clone().attribute(Attribute::CrossedOut))
        } else {
            todo.content.clone()
        };

        result.push_str(&format!("{} {}{}\n", status_symbol, content, priority_text));
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
        session_state: &mut dyn SessionState,
        _turn_state: &mut dyn TurnState,
        _input: Self::Input,
    ) -> Result<String, anyhow::Error> {
        let todos = session_state.get_todo_list();
        let result = format_todo_list(&todos);

        session_state.display_tool_result("todo_read", "", &result);
        Ok(result)
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
        session_state: &mut dyn SessionState,
        _turn_state: &mut dyn TurnState,
        input: Self::Input,
    ) -> Result<String, anyhow::Error> {
        // Validate that there's at most one in-progress item
        let in_progress_count = input
            .todos
            .iter()
            .filter(|todo| todo.status == TodoStatus::InProgress)
            .count();

        if in_progress_count > 1 {
            return Err(anyhow::anyhow!(
                "Only one TODO item can be in progress at a time, found {}",
                in_progress_count
            ));
        }

        // Set the new todo list
        session_state.set_todo_list(input.todos.clone());

        let update_result = format!("Updated todo list with {} items.", input.todos.len());
        let formatted_todos = format_todo_list(&input.todos);
        let result = format!("{}\n{}", update_result, formatted_todos);

        session_state.display_tool_result("todo_write", "", &result);

        Ok(result)
    }
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub priority: TodoPriority,
    pub status: TodoStatus,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum TodoPriority {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
}

#[derive(JsonSchema, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum TodoStatus {
    #[serde(rename = "todo")]
    Todo,
    #[serde(rename = "in-progress")]
    InProgress,
    #[serde(rename = "completed")]
    Completed,
}
