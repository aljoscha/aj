//! `agent` builtin — spawns a sub-agent to perform a focused task and
//! returns its final report.
//!
//! Migrated to [`aj_agent::tool::ToolDefinition`] per
//! `docs/aj-next-plan.md` §2.2. Returns a [`ToolOutcome`] with
//! [`ToolDetails::SubAgentReport`] so the renderer / persistence
//! listener gets the structured triple `(agent_id, task, report)`
//! that the closed-enum design promises in §1.2. The sub-agent's
//! final assistant text is also surfaced as `content` so the parent
//! model sees the report on the wire.
//!
//! The tool delegates to [`ToolContext::spawn_agent`], which mirrors
//! `SubAgentStart` / `SubAgentEnd` correlation events on the parent
//! bus and allocates a fresh [`crate::events::AgentId::Sub`] id. That
//! id is surfaced through [`SpawnedAgent`] so the listener can route
//! nested transcripts under the parent's tool-execution component.
//!
//! Errors from the spawned agent (model failures, tool errors that
//! bubble up to its turn loop, etc.) propagate as `Err`; the agent
//! runtime catches them and synthesizes a generic error tool_result
//! with `is_error: true` — same as the legacy contract.
//!
//! Execution mode stays at the trait default ([`Parallel`]). Multiple
//! `agent` calls in one batch can run concurrently because each one
//! gets a fresh sub-agent with its own bus framing; nothing about
//! spawning interleaves on shared state.
//!
//! [`Parallel`]: aj_agent::tool::ExecutionMode::Parallel

use aj_agent::tool::{
    SpawnMode, SpawnResult, ToolContext, ToolDefinition, ToolDetails, ToolOutcome,
};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

Set run_in_background: true to keep working while the sub-agent runs: the call
returns immediately with a task id, and the sub-agent's report is delivered to
you as a completion notice when it finishes. Don't babysit a background
sub-agent with task_output calls — the notice arrives on its own; use
task_stop(id) if you need to stop it early.
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

    /// Run the sub-agent in the background. The call returns immediately with
    /// a task id; the report arrives as a completion notice when the run
    /// finishes. Use task_output to check on it or task_stop to stop it.
    #[serde(default)]
    pub run_in_background: bool,
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
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> anyhow::Result<ToolOutcome> {
        // The sub-agent's id is allocated inside `spawn_agent`; we
        // need it for the structured `SubAgentReport` payload below.
        // Bubble the error up if the spawn fails — the agent runtime
        // catches it and synthesizes a generic error tool_result so
        // the model still sees the failure.
        let task = input.task.clone();
        let mode = if input.run_in_background {
            SpawnMode::Background
        } else {
            SpawnMode::Blocking
        };
        match ctx.spawn_agent(task.clone(), mode).await? {
            // Wire content goes back to the parent model verbatim —
            // it's the text the sub-agent produced and what the parent
            // expects to read as the tool result. The `details`
            // payload is the structured triple the renderer /
            // persistence listener uses to group nested transcripts
            // under this tool call.
            SpawnResult::Completed(spawned) => Ok(ToolOutcome {
                content: vec![UserContent::text(spawned.report.clone())],
                details: ToolDetails::SubAgentReport {
                    agent_id: spawned.agent_id,
                    task,
                    report: spawned.report,
                },
                is_error: false,
            }),
            // A background spawn needs no rich details variant: the
            // `SubAgentStart` event already created the transcript
            // box, and the report reaches the transcript through the
            // completion notice.
            SpawnResult::Started { agent_id, task_id } => {
                let summary = format!("agent {agent_id} started in background (task #{task_id})");
                let wire = format!(
                    "{summary}. You will be notified when it completes and \
                     delivers its report; use task_output({task_id}) to check on it or \
                     task_stop({task_id}) to stop it."
                );
                Ok(ToolOutcome {
                    content: vec![UserContent::text(wire)],
                    details: ToolDetails::Text {
                        summary,
                        body: String::new(),
                    },
                    is_error: false,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_agent::TaskRegistry;
    use aj_agent::tool::{SpawnedAgent, StartedTask, TaskKind, TaskOutputSource, TodoItem};
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    /// A `ToolContext` that records the task and mode it was asked to
    /// spawn and returns a canned [`SpawnResult`]. Pure unit-test
    /// plumbing — none of the production code paths reach these
    /// methods.
    struct StubSpawnContext {
        last_task: Option<String>,
        last_mode: Option<SpawnMode>,
        response: SpawnResult,
        tasks: crate::testing::DummyToolContext,
    }

    impl ToolContext for StubSpawnContext {
        fn working_directory(&self) -> PathBuf {
            PathBuf::from("/tmp")
        }

        fn get_todo_list(&self) -> Vec<TodoItem> {
            Vec::new()
        }

        fn set_todo_list(&mut self, _todos: Vec<TodoItem>) {}

        fn spawn_agent<'a>(
            &'a mut self,
            task: String,
            mode: SpawnMode,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<SpawnResult>> + Send + 'a>>
        {
            self.last_task = Some(task);
            self.last_mode = Some(mode);
            let response = self.response.clone();
            Box::pin(async move { Ok(response) })
        }

        fn emit_update<'a>(
            &'a mut self,
            _partial: ToolDetails,
        ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }

        fn cancellation(&self) -> CancellationToken {
            CancellationToken::new()
        }

        fn task_registry(&self) -> TaskRegistry {
            self.tasks.task_registry()
        }

        fn start_background_task(
            &mut self,
            kind: TaskKind,
            label: String,
            output: Arc<dyn TaskOutputSource>,
        ) -> StartedTask {
            self.tasks.start_background_task(kind, label, output)
        }
    }

    /// A `ToolContext` whose `spawn_agent` always fails. Lets us
    /// exercise the bubbled-error path without standing up a real
    /// agent runtime.
    struct FailingSpawnContext {
        tasks: crate::testing::DummyToolContext,
    }

    impl ToolContext for FailingSpawnContext {
        fn working_directory(&self) -> PathBuf {
            PathBuf::from("/tmp")
        }

        fn get_todo_list(&self) -> Vec<TodoItem> {
            Vec::new()
        }

        fn set_todo_list(&mut self, _todos: Vec<TodoItem>) {}

        fn spawn_agent<'a>(
            &'a mut self,
            _task: String,
            _mode: SpawnMode,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<SpawnResult>> + Send + 'a>>
        {
            Box::pin(async move { Err(anyhow::anyhow!("model exploded")) })
        }

        fn emit_update<'a>(
            &'a mut self,
            _partial: ToolDetails,
        ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async {})
        }

        fn cancellation(&self) -> CancellationToken {
            CancellationToken::new()
        }

        fn task_registry(&self) -> TaskRegistry {
            self.tasks.task_registry()
        }

        fn start_background_task(
            &mut self,
            kind: TaskKind,
            label: String,
            output: Arc<dyn TaskOutputSource>,
        ) -> StartedTask {
            self.tasks.start_background_task(kind, label, output)
        }
    }

    /// On success, the tool wires the sub-agent's report into both
    /// channels: the wire `content` carries the report text so the
    /// parent model can read it, and `details` is the structured
    /// `SubAgentReport` triple used for rendering / persistence.
    #[tokio::test]
    async fn execute_returns_sub_agent_report_outcome() {
        let mut ctx = StubSpawnContext {
            last_task: None,
            last_mode: None,
            tasks: Default::default(),
            response: SpawnResult::Completed(SpawnedAgent {
                agent_id: 7,
                report: "investigation complete".to_string(),
            }),
        };

        let outcome = AgentTool
            .execute(
                &mut ctx,
                AgentInput {
                    task: "scan the codebase".to_string(),
                    description: None,
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");

        // The task we forwarded to spawn_agent must round-trip through
        // the input verbatim — the tool hasn't started rewriting it.
        assert_eq!(ctx.last_task.as_deref(), Some("scan the codebase"));
        // The default is a blocking spawn.
        assert_eq!(ctx.last_mode, Some(SpawnMode::Blocking));

        assert!(!outcome.is_error);

        // Wire content is the report text so the parent model sees
        // exactly what the sub-agent produced.
        let wire = outcome
            .content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text.as_str()),
                UserContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(wire, "investigation complete");

        match outcome.details {
            ToolDetails::SubAgentReport {
                agent_id,
                task,
                report,
            } => {
                assert_eq!(agent_id, 7);
                assert_eq!(task, "scan the codebase");
                assert_eq!(report, "investigation complete");
            }
            other => panic!("expected SubAgentReport, got {other:?}"),
        }
    }

    /// The optional `description` field is part of the model-facing
    /// schema but doesn't change the structured outcome — we keep it
    /// for backwards compatibility with the existing tool contract
    /// even though the new renderer doesn't read it.
    #[tokio::test]
    async fn description_input_does_not_affect_outcome() {
        let mut ctx = StubSpawnContext {
            last_task: None,
            last_mode: None,
            tasks: Default::default(),
            response: SpawnResult::Completed(SpawnedAgent {
                agent_id: 1,
                report: "ok".to_string(),
            }),
        };

        let outcome = AgentTool
            .execute(
                &mut ctx,
                AgentInput {
                    task: "do thing".to_string(),
                    description: Some("a friendly headline".to_string()),
                    run_in_background: false,
                },
            )
            .await
            .expect("execute");

        match outcome.details {
            ToolDetails::SubAgentReport { task, report, .. } => {
                assert_eq!(task, "do thing");
                assert_eq!(report, "ok");
            }
            other => panic!("expected SubAgentReport, got {other:?}"),
        }
    }

    /// `spawn_agent` failures propagate as `Err` so the agent runtime
    /// can fold them into its standard error-tool_result synthesis —
    /// matching the legacy contract.
    #[tokio::test]
    async fn spawn_failure_bubbles_up_as_error() {
        let mut ctx = FailingSpawnContext {
            tasks: Default::default(),
        };
        let result = AgentTool
            .execute(
                &mut ctx,
                AgentInput {
                    task: "doomed".to_string(),
                    description: None,
                    run_in_background: false,
                },
            )
            .await;

        assert!(result.is_err(), "spawn failure should bubble up");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("model exploded"),
            "unexpected error string: {err}"
        );
    }

    /// `run_in_background: true` requests a background spawn and maps
    /// the `Started` result onto the started outcome: a plain-text
    /// summary naming both ids, with the "you will be notified" hint
    /// on the wire.
    #[tokio::test]
    async fn background_input_returns_started_outcome() {
        let mut ctx = StubSpawnContext {
            last_task: None,
            last_mode: None,
            tasks: Default::default(),
            response: SpawnResult::Started {
                agent_id: 2,
                task_id: 5,
            },
        };

        let outcome = AgentTool
            .execute(
                &mut ctx,
                AgentInput {
                    task: "long research".to_string(),
                    description: None,
                    run_in_background: true,
                },
            )
            .await
            .expect("execute");

        assert_eq!(ctx.last_mode, Some(SpawnMode::Background));
        assert!(!outcome.is_error);

        let wire = outcome
            .content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t.text.as_str()),
                UserContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
            wire.starts_with("agent 2 started in background (task #5)"),
            "wire: {wire:?}"
        );
        assert!(wire.contains("You will be notified"), "wire: {wire:?}");

        match outcome.details {
            ToolDetails::Text { summary, .. } => {
                assert_eq!(summary, "agent 2 started in background (task #5)");
            }
            other => panic!("expected Text details, got {other:?}"),
        }
    }
}
