use anyhow::Result;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{SessionContext, ToolDefinition, TurnContext};

const DESCRIPTION: &str = r#"
Spawn a sub-agent to perform a specific task like research, analysis, or implementation.

The sub-agent acts as a collaborator or researcher that can:
- Research where in the code something is implemented
- Analyze how specific systems work
- Implement smaller-scope tasks independently
- Provide detailed analysis and insights

The sub-agent has access to tools for reading files, searching, and basic file operations but cannot interact with the user directly. It runs a single turn and returns a detailed report.

Use this tool when you need to:
- Investigate complex code patterns across multiple files
- Research implementation details before making changes
- Analyze system architecture or dependencies
- Implement isolated features or components
- Get a fresh perspective on code organization

The sub-agent will return a comprehensive report that you can use to inform your next steps.
"#;

#[derive(Debug, Clone)]
pub struct AgentTool;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentInput {
    /// The task for the sub-agent to perform. Be specific about what needs to
    /// be done and include context. This should be a clear, detailed prompt
    /// that describes what the sub-agent should research, analyze, or
    /// implement. The sub-agent will act as a collaborator or researcher,
    /// providing insights and implementation details.
    pub task: String,

    /// Optional description of the task that can be displayed to the user while
    /// the sub-agent is working. If not provided, a generic description will be
    /// used.
    pub description: Option<String>,
}

impl ToolDefinition for AgentTool {
    type Input = AgentInput;

    fn name(&self) -> &'static str {
        "agent"
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    async fn execute(
        &self,
        session_ctx: &mut dyn SessionContext,
        _turn_ctx: &mut dyn TurnContext,
        input: AgentInput,
    ) -> Result<String> {
        let description = input
            .description
            .unwrap_or_else(|| "Running sub-agent task".to_string());

        // Display that we're spawning a sub-agent
        session_ctx.display_tool_result("agent", &description, "Spawning sub-agent...");

        // Spawn the sub-agent
        let result = session_ctx.spawn_agent(input.task).await?;

        Ok(result)
    }
}
