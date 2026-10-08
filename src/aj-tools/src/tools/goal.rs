//! Application-owned goal pursuit tools.
use aj_agent::goal::{GoalAction, GoalError};
use aj_agent::tool::{ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct CreateGoalTool;
#[derive(Clone)]
pub struct GetGoalTool;
#[derive(Clone)]
pub struct UpdateGoalTool;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateGoalInput {
    /// Objective to pursue. Create only when the user explicitly requests a goal.
    pub objective: String,
    /// Optional positive token budget for pursuing this goal.
    #[schemars(range(min = 1))]
    pub token_budget: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetGoalInput {}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalUpdateStatus {
    Complete,
    Blocked,
    Paused,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateGoalInput {
    pub status: GoalUpdateStatus,
}

fn outcome(text: String, is_error: bool) -> ToolOutcome {
    ToolOutcome {
        structured_content: None,
        content: vec![UserContent::text(text.clone())],
        details: ToolDetails::Text {
            summary: "Goal".into(),
            body: text,
        },
        is_error,
    }
}

async fn act(future: aj_agent::goal::GoalFuture) -> ToolOutcome {
    match future.await {
        Ok(goal) => outcome(
            serde_json::json!({
                "remaining_tokens": goal.as_ref().and_then(|goal| goal.remaining_tokens()),
                "goal": goal,
            })
            .to_string(),
            false,
        ),
        Err(error) => outcome(error.to_string(), true),
    }
}

impl ToolDefinition for CreateGoalTool {
    type Input = CreateGoalInput;
    fn code_mode_exposure(&self) -> aj_agent::tool::CodeModeExposure {
        aj_agent::tool::CodeModeExposure::DirectOnly
    }
    fn execution_mode(&self) -> aj_agent::tool::ExecutionMode {
        aj_agent::tool::ExecutionMode::Control
    }
    fn name(&self) -> &'static str {
        "create_goal"
    }
    fn description(&self) -> &'static str {
        "Create a persistent goal only when the user explicitly asks you to create or pursue a goal. Do not infer permission from an ordinary task request. Set token_budget only when an explicit positive token budget was requested. An unfinished goal cannot be replaced. The interactive host continues pursuit across turns until stopped. Unavailable in print mode and subagents."
    }
    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> Result<ToolOutcome, aj_agent::BoxError> {
        if input.objective.trim().is_empty() || input.token_budget == Some(0) {
            return Ok(outcome(
                GoalError::Invalid(
                    "objective must be nonempty and token_budget must be positive".into(),
                )
                .to_string(),
                true,
            ));
        }
        Ok(act(ctx.goal(GoalAction::Create {
            objective: input.objective,
            token_budget: input.token_budget,
        }))
        .await)
    }
}

impl ToolDefinition for GetGoalTool {
    type Input = GetGoalInput;
    fn code_mode_exposure(&self) -> aj_agent::tool::CodeModeExposure {
        aj_agent::tool::CodeModeExposure::DirectOnly
    }
    fn execution_mode(&self) -> aj_agent::tool::ExecutionMode {
        aj_agent::tool::ExecutionMode::Control
    }
    fn name(&self) -> &'static str {
        "get_goal"
    }
    fn description(&self) -> &'static str {
        "Read the current goal, status, token budget, usage and remaining tokens. The goal field is null when there is no goal. Does not create or resume pursuit. Unavailable in print mode and subagents."
    }
    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        _: Self::Input,
    ) -> Result<ToolOutcome, aj_agent::BoxError> {
        Ok(act(ctx.goal(GoalAction::Get)).await)
    }
}

impl ToolDefinition for UpdateGoalTool {
    type Input = UpdateGoalInput;
    fn code_mode_exposure(&self) -> aj_agent::tool::CodeModeExposure {
        aj_agent::tool::CodeModeExposure::DirectOnly
    }
    fn execution_mode(&self) -> aj_agent::tool::ExecutionMode {
        aj_agent::tool::ExecutionMode::Control
    }
    fn name(&self) -> &'static str {
        "update_goal"
    }
    fn description(&self) -> &'static str {
        "Update the current goal status to complete, blocked, or paused. Mark complete only when the full objective is achieved and verified, then report final usage from the result. Mark blocked only after the same genuine blocking condition repeats for at least three consecutive goal turns and no meaningful safe progress is possible without user input or external change. Resuming a blocked goal starts a fresh audit. Once the threshold is met, mark blocked instead of repeatedly reporting no progress. Mark paused only at the user's explicit request. Budget limits take precedence over pausing. Never mark complete merely because the budget is exhausted or work is stopping. Cannot edit the objective, budget, or resume pursuit. Unavailable in print mode and subagents."
    }
    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> Result<ToolOutcome, aj_agent::BoxError> {
        let action = match input.status {
            GoalUpdateStatus::Complete => GoalAction::Complete,
            GoalUpdateStatus::Blocked => GoalAction::Block,
            GoalUpdateStatus::Paused => GoalAction::Pause,
        };
        Ok(act(ctx.goal(action)).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::DummyToolContext;

    #[tokio::test]
    async fn absent_host_is_a_clear_recoverable_tool_result() {
        let mut ctx = DummyToolContext::default();
        let result = GetGoalTool
            .execute(&mut ctx, GetGoalInput {})
            .await
            .unwrap();
        assert!(result.is_error);
        let ToolDetails::Text { body, .. } = result.details else {
            panic!()
        };
        assert!(body.contains("interactive session host"));
        assert!(body.contains("print mode"));
    }

    #[tokio::test]
    async fn creation_rejects_empty_objective_and_zero_budget() {
        for (objective, token_budget) in [(" ", None), ("work", Some(0))] {
            let result = CreateGoalTool
                .execute(
                    &mut DummyToolContext::default(),
                    CreateGoalInput {
                        objective: objective.into(),
                        token_budget,
                    },
                )
                .await
                .unwrap();
            assert!(result.is_error);
            let ToolDetails::Text { body, .. } = result.details else {
                panic!()
            };
            assert!(body.contains("must be positive"));
        }
    }

    #[test]
    fn model_cannot_resume_or_edit_goals() {
        for status in ["active", "budget_limited", "usage_limited", "resume"] {
            assert!(
                serde_json::from_value::<UpdateGoalInput>(serde_json::json!({"status": status}))
                    .is_err()
            );
        }
        assert!(
            serde_json::from_value::<UpdateGoalInput>(
                serde_json::json!({"status": "complete", "objective": "replacement"})
            )
            .is_err()
        );
    }
}
