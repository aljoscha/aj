//! Advisory child-agent tool. The application supplies its resolved model bundle.

use aj_agent::tool::{SpawnAgentConfig, SpawnMode, ToolContext, ToolDefinition, ToolOutcome};

use super::agent::{AgentInput, spawn_outcome};

/// An advisory contract, not a filesystem sandbox. Shell access permits inspection.
pub const ORACLE_PROMPT: &str = "

## Oracle assignment

You are an expert advisor to the calling agent. Investigate the supplied question
using the code and evidence available to you. Your role is advisory: do not edit
files, implement fixes, or take actions that change shared state. Use shell
commands for inspection, not modification. Do not delegate.

Keep the review or investigation within the requested scope. State concrete
evidence, failure scenarios, tradeoffs, and uncertainty. Recommend the smallest
complete solution and explain why. A user-requested review may be general within
its stated scope. Return a concise, self-contained report for the calling agent
to assess and act on.
";

#[derive(Clone, Default)]
pub struct OracleTool {
    config: Option<SpawnAgentConfig>,
}

impl OracleTool {
    /// Supply the resolved model bundle before installing the tool in an agent.
    pub fn new(config: SpawnAgentConfig) -> Self {
        Self {
            config: Some(config),
        }
    }
}

impl ToolDefinition for OracleTool {
    type Input = AgentInput;

    fn name(&self) -> &'static str {
        "oracle"
    }

    fn description(&self) -> &'static str {
        "Consult an expert advisor for reviews and unresolved, consequential questions.

When the user explicitly asks for Oracle, use it for the requested task, including
general or final code review. Otherwise, investigate first and consult it only
when a specific unresolved question could materially change a consequential
decision: competing designs, a suspected invariant violation, or a difficult
failure you have not resolved. Complexity alone is not a reason to consult it.

Do not use Oracle for routine reassurance, codebase searches, or implementation.
It advises rather than edits. You remain responsible for assessing its report,
implementing any changes, and verifying the result.

Each call starts fresh. Oracle does not see your conversation or previous Oracle
invocations. Supply the intended outcome, constraints, relevant file paths, what
you already checked, and the decision needed. For a review, identify the diff and
intended behavior. For a follow-up, name the prior finding and what changed. Ask
it to inspect the actual code and evidence, and keep the scope explicit.

By default the call waits for the final report. Set run_in_background: true to
keep working while Oracle runs. The call then returns a task id, and its report
arrives as a completion notice. Do not wait by sleeping in the foreground:
no notice can arrive while a foreground command is running."
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        input: Self::Input,
    ) -> Result<ToolOutcome, aj_agent::BoxError> {
        let config = self
            .config
            .clone()
            .ok_or("Oracle model has not been configured by the host")?;
        let mode = if input.run_in_background {
            SpawnMode::Background
        } else {
            SpawnMode::Blocking
        };
        let result = ctx
            .spawn_configured_agent(input.task.clone(), mode, config)
            .await?;
        Ok(spawn_outcome(input.task, result))
    }
}
