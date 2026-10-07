//! One execution boundary for direct and programmatic tool calls.

use super::*;

#[derive(Clone)]
pub(crate) struct ToolRunner {
    pub(crate) context: SessionContextWrapper,
    pub(crate) tools: HashMap<String, ErasedToolDefinition>,
    pub(crate) before_hook: Option<hooks::BeforeToolCallHook>,
    pub(crate) after_hook: Option<hooks::AfterToolCallHook>,
    pub(crate) access: Arc<tokio::sync::RwLock<()>>,
    pub(crate) slots: Arc<tokio::sync::Semaphore>,
}

impl ToolRunner {
    pub(crate) async fn run(
        &self,
        call_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<RunToolResult, TurnError> {
        self.run_with(
            call_id.clone(),
            tool_name.clone(),
            tool_input,
            cancel.clone(),
            |input| self.execute(&call_id, &tool_name, input, cancel.clone()),
        )
        .await
    }

    pub(crate) async fn run_with<F, Fut>(
        &self,
        call_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
        cancel: CancellationToken,
        execute: F,
    ) -> Result<RunToolResult, TurnError>
    where
        F: FnOnce(serde_json::Value) -> Fut,
        Fut: std::future::Future<Output = Result<ToolOutcome, BoxError>>,
    {
        // Mirror the start of every tool invocation on the bus before
        // any work — listeners that render a "running…" placeholder
        // rely on seeing this before any update or end.
        self.context
            .parent_bus
            .emit(AgentEvent::ToolExecutionStart {
                agent_id: self.context.agent_id,
                call_id: call_id.clone(),
                tool: tool_name.clone(),
                args: tool_input.clone(),
            })
            .await
            .map_err(TurnError::Fatal)?;

        // The before-tool-call hook can rewrite the input or
        // short-circuit the call with a pre-baked outcome (permission
        // denial, policy block). We clone the `Arc` so the borrow
        // doesn't conflict with the `execute_tool` call below.
        let before_hook = self.before_hook.clone();
        let (tool_input, short_circuit_outcome) = match before_hook {
            Some(hook) => {
                let ctx = hooks::ToolCallContext {
                    call_id: &call_id,
                    tool_name: &tool_name,
                };
                match hook(ctx, tool_input.clone()).await {
                    hooks::BeforeToolCallOutcome::Proceed { args } => (args, None),
                    hooks::BeforeToolCallOutcome::ShortCircuit { outcome } => {
                        (tool_input, Some(outcome))
                    }
                }
            }
            None => (tool_input, None),
        };

        // Run the tool unless the before-hook short-circuited it,
        // racing against cancel. On cancel we drop the tool future and
        // synthesize a cancelled outcome so the transcript still pairs
        // `tool_use` with `tool_result`. The drop is all the notice a
        // tool gets: it is never polled again, so releasing whatever it
        // holds (a child process group, a file handle) is the tool's
        // own duty on drop, and a tool that leaks there is a tool bug.
        //
        // Tool-input parse failures surface as a `ToolCall` with
        // `arguments == Value::Null`; the tool's own deserializer
        // rejects the payload and the call bubbles up here as an
        // `Err`. We fold that into an `is_error: true` outcome so the
        // failure rides the same `Message::ToolResult` shape every
        // other tool error does.
        let outcome_or_cancel: Option<ToolOutcome> = if let Some(outcome) = short_circuit_outcome {
            Some(outcome)
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                res = execute(tool_input.clone()) => {
                    Some(match res {
                        Ok(outcome) => outcome,
                        Err(err) => ToolOutcome {
                            structured_content: None,
                            content: vec![UserContent::text(format!("{err}"))],
                            details: ToolDetails::Text {
                                summary: format!("{tool_name}: error"),
                                body: err.to_string(),
                            },
                            is_error: true,
                        },
                    })
                }
            }
        };

        let aborted = outcome_or_cancel.is_none();
        let mut outcome = outcome_or_cancel.unwrap_or_else(|| cancelled_tool_outcome(&tool_name));

        // The after-tool-call hook can rewrite the outcome before it
        // is finalized. We skip it on cancellation so a misbehaving
        // hook can't swallow the abort: the cancelled outcome lands
        // verbatim and the caller returns `TurnError::Aborted`.
        if !aborted {
            if let Some(hook) = self.after_hook.clone() {
                let ctx = hooks::ToolCallContext {
                    call_id: &call_id,
                    tool_name: &tool_name,
                };
                hook(ctx, &mut outcome).await;
            }
        }

        Ok(RunToolResult {
            call_id,
            tool_name,
            outcome,
            aborted,
        })
    }

    pub(crate) async fn execute(
        &self,
        call_id: &str,
        tool_name: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolOutcome, BoxError> {
        let tool = self.tools.get(tool_name).ok_or("tool not found!")?;
        if tool.execution_mode == ExecutionMode::Control {
            return self.invoke(tool, call_id, tool_name, input, cancel).await;
        }
        let _slot = self.slots.acquire().await?;
        // A writer excludes every other tool, not just other writers. The
        // outer exec/wait orchestration never acquires a permit itself.
        let (_read, _write) = match tool.execution_mode {
            ExecutionMode::Parallel => (Some(self.access.read().await), None),
            ExecutionMode::Sequential => (None, Some(self.access.write().await)),
            ExecutionMode::Control => unreachable!("control tools bypass resource permits"),
        };
        self.invoke(tool, call_id, tool_name, input, cancel).await
    }

    async fn invoke(
        &self,
        tool: &ErasedToolDefinition,
        call_id: &str,
        tool_name: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<ToolOutcome, BoxError> {
        let mut context = self.context.clone();
        context.call_id = call_id.to_owned();
        context.tool_name = tool_name.to_owned();
        context.tool_args = input.clone();
        context.cancellation = cancel.child_token();
        (tool.func)(&mut context, input).await
    }
}
