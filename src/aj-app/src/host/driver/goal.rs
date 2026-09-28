//! Branch-local goal policy. The session driver is the only mutator, including
//! for model tool requests, so accounting and continuation see one ordered state.

use std::collections::HashMap;
use std::time::Instant;

use aj_agent::TurnError;
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::goal::{Goal, GoalAction, GoalError, GoalRequest, GoalStatus};
use aj_models::types::{AssistantContent, ErrorCategory, Message};
use aj_session::{ConversationError, ConversationLog};

use super::{Driver, HostError, TurnStart};

#[cfg(test)]
mod tests;

pub(super) struct GoalRun {
    pub(super) current: Option<Goal>,
    dirty: bool,
    /// Descendants retain the goal that admitted their work. Main can adopt a
    /// new goal mid-turn, but only inference started afterward belongs to it.
    owners: HashMap<AgentId, String>,
    /// Assistant messages and compaction are serial within each agent. Capture
    /// their owner at start rather than attributing a late result at receipt.
    inferences: HashMap<AgentId, String>,
    clock: Option<Instant>,
    automatic: bool,
    activity: bool,
    empty_turns: u8,
    execution_failed: bool,
    tool_succeeded: bool,
    failed_execution_turns: u8,
    last_error: Option<ErrorCategory>,
}

impl GoalRun {
    pub(super) fn new(current: Option<Goal>) -> Self {
        Self {
            current,
            dirty: false,
            owners: HashMap::new(),
            inferences: HashMap::new(),
            clock: None,
            automatic: false,
            activity: false,
            empty_turns: 0,
            execution_failed: false,
            tool_succeeded: false,
            failed_execution_turns: 0,
            last_error: None,
        }
    }

    fn active(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|goal| goal.status == GoalStatus::Active)
    }

    pub(super) fn stop_pursuit(&mut self) {
        if self.active() {
            self.set_status(GoalStatus::Paused);
        }
    }

    fn elapsed(&mut self) {
        if let (Some(clock), Some(goal)) = (&mut self.clock, &mut self.current) {
            let seconds = clock.elapsed().as_secs();
            if seconds > 0 {
                goal.time_used_seconds = goal.time_used_seconds.saturating_add(seconds);
                *clock += std::time::Duration::from_secs(seconds);
                self.dirty = true;
            }
        }
    }

    fn set_status(&mut self, status: GoalStatus) {
        self.elapsed();
        if let Some(goal) = &mut self.current {
            if goal.status != status {
                goal.status = status;
                self.dirty = true;
            }
            if status == GoalStatus::Active {
                self.clock.get_or_insert_with(Instant::now);
            } else if status != GoalStatus::BudgetLimited {
                self.clock = None;
            }
        }
        if !matches!(status, GoalStatus::Active | GoalStatus::BudgetLimited) {
            self.stop_accounting();
        }
    }

    fn stop_accounting(&mut self) {
        self.clock = None;
        self.owners.clear();
        self.inferences.clear();
        self.reset_execution_audit();
    }

    fn reset_execution_audit(&mut self) {
        self.execution_failed = false;
        self.tool_succeeded = false;
        self.failed_execution_turns = 0;
    }

    fn begin(&mut self, agent: AgentId, automatic: bool) {
        if agent == AgentId::Main {
            self.owners.remove(&agent);
            if self.active() {
                let goal = self.current.as_ref().expect("active goal");
                self.owners.insert(agent, goal.id.clone());
                self.clock.get_or_insert_with(Instant::now);
            }
            self.automatic = automatic;
            self.activity = false;
            self.execution_failed = false;
            self.tool_succeeded = false;
            self.last_error = None;
            if !automatic {
                self.empty_turns = 0;
            }
        }
    }

    fn begin_inference(&mut self, agent: AgentId) {
        // A busy Main adopts a changed goal at inference, not at the control
        // edit. Tools from the earlier response, including newly spawned
        // descendants, still belong to the goal that produced them.
        if agent == AgentId::Main && self.active() {
            self.owners.insert(
                agent,
                self.current.as_ref().expect("active goal").id.clone(),
            );
        }
        self.inferences.remove(&agent);
        if let Some(owner) = self.owners.get(&agent) {
            self.inferences.insert(agent, owner.clone());
        }
    }

    fn charge(&mut self, agent: AgentId, tokens: u64) {
        let owner = self.inferences.remove(&agent);
        let Some(goal) = &mut self.current else {
            return;
        };
        if owner.as_ref() != Some(&goal.id) {
            return;
        }
        goal.tokens_used = goal.tokens_used.saturating_add(tokens);
        self.dirty |= tokens > 0;
        if goal.status == GoalStatus::Active
            && goal
                .token_budget
                .is_some_and(|budget| goal.tokens_used >= budget)
        {
            goal.status = GoalStatus::BudgetLimited;
            self.dirty = true;
        }
    }

    pub(super) fn finish(
        &mut self,
        outcome: &Result<Result<(), TurnError>, tokio::task::JoinError>,
    ) {
        self.elapsed();
        if self.active() {
            match outcome {
                Ok(Ok(())) => {
                    // Text alone cannot establish that execution recovered. A
                    // successful tool resets the audit, while a status-only turn
                    // neither advances nor erases the accumulated failures.
                    if self.execution_failed && !self.tool_succeeded {
                        self.failed_execution_turns = self.failed_execution_turns.saturating_add(1);
                        if self.failed_execution_turns >= 3 {
                            self.set_status(GoalStatus::Blocked);
                        }
                    }
                    if self.automatic && !self.activity {
                        self.empty_turns = self.empty_turns.saturating_add(1);
                        if self.empty_turns >= 3 {
                            self.set_status(GoalStatus::Blocked);
                        }
                    } else {
                        self.empty_turns = 0;
                    }
                }
                Ok(Err(TurnError::Aborted)) => self.set_status(GoalStatus::Paused),
                _ => self.set_status(if self.last_error == Some(ErrorCategory::RateLimit) {
                    GoalStatus::UsageLimited
                } else {
                    GoalStatus::Blocked
                }),
            }
        }
        if !self.active() {
            self.clock = None;
        }
    }
}

/// Selecting saved work is not authorization to execute it. Terminal and
/// blocked states retain their explanation, only active pursuit becomes paused.
pub(crate) fn restore_goal(log: &mut ConversationLog) -> Result<Option<Goal>, ConversationError> {
    let mut goal = log.head().and_then(|head| log.goal_at(head));
    if let Some(goal) = &mut goal
        && goal.status == GoalStatus::Active
    {
        goal.status = GoalStatus::Paused;
        log.append_goal_change(Some(goal.clone()))?;
        log.flush_pending()?;
    }
    Ok(goal)
}

/// Even a cleared goal needs an authoritative model hint to supersede old
/// continuation instructions. Ordinary sessions need no such extra context.
pub(crate) fn has_goal_history(log: &ConversationLog) -> bool {
    log.entries_in_order().iter().any(|entry| {
        matches!(
            entry.entry,
            aj_session::ConversationEntryKind::GoalChange { .. }
        )
    })
}

impl Driver {
    pub(super) fn admit_goal_inference(&mut self) -> u64 {
        // The request loop folds preceding usage and spawns before admission.
        // Its normal checkpoint owns persistence failures, including cancellation
        // and reporting. Admission only fixes ownership and the context revision.
        self.goal.begin_inference(AgentId::Main);
        self.goal_revision
    }

    pub(super) fn observe_goal_event(&mut self, event: &AgentEvent) {
        let was_active = self.goal.active();
        match event {
            // Main is admitted through the request path before provider work.
            // A descendant's owner is fixed at spawn, regardless of receipt time.
            AgentEvent::MessageStart { agent_id, message }
                if *agent_id != AgentId::Main
                    && matches!(message.as_stored_wire(), Some(Message::Assistant(_))) =>
            {
                self.goal.begin_inference(*agent_id);
            }
            AgentEvent::SubAgentStart { parent, child, .. } => {
                if let Some(goal) = self.goal.owners.get(parent).cloned() {
                    self.goal.owners.insert(*child, goal);
                }
            }
            AgentEvent::CompactionEnd {
                agent_id,
                usage: Some(usage),
                ..
            } => {
                self.goal.charge(
                    *agent_id,
                    usage
                        .turn_input
                        .saturating_add(usage.turn_cache_write)
                        .saturating_add(usage.turn_output),
                );
            }
            AgentEvent::MessageEnd { agent_id, message } => {
                if let Some(Message::Assistant(message)) = message.as_stored_wire() {
                    // MessageEnd includes success, failed attempts and cancelled
                    // partials. UsageUpdate is a success-only display snapshot,
                    // so consuming both would double-charge successful inference.
                    // Cache writes are new input, cache reads are reused context.
                    self.goal.charge(
                        *agent_id,
                        message
                            .usage
                            .input
                            .saturating_add(message.usage.cache_write)
                            .saturating_add(message.usage.output),
                    );
                    if *agent_id == AgentId::Main {
                        self.goal.activity |= message.content.iter().any(|content| match content {
                            AssistantContent::Text(text) => !text.text.trim().is_empty(),
                            AssistantContent::Thinking(thinking) => {
                                !thinking.thinking.trim().is_empty()
                            }
                            AssistantContent::ToolCall(_) => true,
                        });
                        self.goal.last_error = message.error.as_ref().map(|error| error.category);
                    }
                }
            }
            AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Main,
                tool,
                is_error,
                ..
            } => {
                self.goal.activity = true;
                if self.goal.active()
                    && self.goal.owners.get(&AgentId::Main)
                        == self.goal.current.as_ref().map(|goal| &goal.id)
                {
                    if !is_error {
                        self.goal.tool_succeeded = true;
                        self.goal.failed_execution_turns = 0;
                    } else if tool == "bash" {
                        // A completed command with a nonzero exit is not a tool
                        // error. This detects inability to execute reliably,
                        // not ordinary failing tests or searches with no match.
                        self.goal.execution_failed = true;
                    }
                }
            }
            _ => {}
        }
        if was_active && !self.goal.active() {
            self.steer_goal_state();
        }
    }

    pub(super) fn prepare_goal_turn(&mut self, agent: AgentId, automatic: bool) {
        if agent == AgentId::Main && self.goal.active() && !self.goal_tools_available() {
            self.goal.stop_pursuit();
            self.publish_event(None, AgentEvent::Warning {
                agent_id: AgentId::Main,
                text: "Goal paused because update_goal is disabled. Enable it before resuming pursuit.".into(),
            });
        }
        self.goal.begin(agent, automatic);
        if agent == AgentId::Main {
            let text = (!automatic && self.goal_revision > 0)
                .then(|| context(self.goal.current.as_ref(), false));
            self.session.core.message_queues.set_context(agent, text);
        }
    }

    fn steer_goal_state(&self) {
        let text = context(self.goal.current.as_ref(), self.goal.active());
        self.session
            .core
            .message_queues
            .set_context(AgentId::Main, Some(text));
    }

    fn goal_tools_available(&self) -> bool {
        !self
            .shared
            .config
            .lock()
            .expect("config mutex poisoned")
            .disabled_tools
            .iter()
            .any(|name| name == "update_goal")
    }

    pub(super) async fn continue_goal(&mut self) -> Result<(), GoalError> {
        if self.session.is_draining()
            || !self.goal.active()
            || self.turns.is_busy(&self.lifecycle, AgentId::Main)
        {
            return Ok(());
        }
        if !self.goal_tools_available() {
            self.prepare_goal_turn(AgentId::Main, false);
            return self.checkpoint_goal().await;
        }
        // Explicit input and task results keep their ordinary wake path and
        // precede synthetic continuation. Goal context accompanies that wake.
        if self.session.has_queued(AgentId::Main)
            || self.session.core.task_registry.has_notices(AgentId::Main)
        {
            self.wake(AgentId::Main);
            return Ok(());
        }
        let prompt = context(self.goal.current.as_ref(), true);
        if let Err(err) = self.spawn(AgentId::Main, TurnStart::Goal(prompt)) {
            self.goal.set_status(GoalStatus::Blocked);
            self.publish_event(
                None,
                AgentEvent::Error {
                    agent_id: AgentId::Main,
                    text: err.to_string(),
                },
            );
        }
        self.checkpoint_goal().await
    }

    pub(super) async fn checkpoint_goal(&mut self) -> Result<(), GoalError> {
        self.goal.elapsed();
        if !self.goal.dirty {
            return Ok(());
        }
        // The log guard fences the event forwarder. Publish preceding appends
        // before advancing the high-water mark past this non-message record.
        let log = std::sync::Arc::clone(&self.session.core.log);
        let mut log = log.lock().await;
        self.drain_events();
        let current = self.goal.current.clone();
        let entry = log
            .append_goal_change(current.clone())
            .map_err(|err| GoalError::Storage(err.to_string()))?;
        log.flush_pending()
            .map_err(|err| GoalError::Storage(err.to_string()))?;
        self.goal.dirty = false;
        // Preserve zero as the fast path for sessions that have never had a
        // goal. Model creation enables context without advancing later revisions.
        self.goal_revision = self.goal_revision.max(1);
        self.session.publish_state(&self.shared.fanout, |status| {
            status.goal = current;
            status.last_seq = status.last_seq.max(entry.seq);
            status.note_activity();
            true
        });
        self.shared.fanout.mark_list_dirty();
        Ok(())
    }

    pub(super) async fn fail_goal_persistence(&mut self, error: GoalError) {
        if self.session.core.log.lock().await.write_failure().is_some() {
            // The fuse has already signalled the driver. Its failure arm owns
            // reporting and teardown, and runs before more requests or work.
            return;
        }
        self.goal.set_status(GoalStatus::Blocked);
        self.goal.dirty = false;
        self.turns.cancel_all();
        self.publish_event(
            None,
            AgentEvent::Error {
                agent_id: AgentId::Main,
                text: error.to_string(),
            },
        );
    }

    pub(super) async fn pause_goal(&mut self) -> Result<(), HostError> {
        if self.goal.active() {
            self.goal.set_status(GoalStatus::Paused);
            self.steer_goal_state();
            self.checkpoint_goal().await.map_err(host_error)?;
        }
        Ok(())
    }

    pub(super) async fn goal_action(
        &mut self,
        action: GoalAction,
        from_tool: bool,
    ) -> Result<Option<Goal>, GoalError> {
        if matches!(
            action,
            GoalAction::Create { .. } | GoalAction::Replace { .. } | GoalAction::Resume
        ) && !self.goal_tools_available()
        {
            return Err(GoalError::Conflict("Goal pursuit requires update_goal. Enable that tool before creating or resuming a goal.".into()));
        }
        if from_tool
            && matches!(
                action,
                GoalAction::Edit { .. }
                    | GoalAction::SetBudget { .. }
                    | GoalAction::Replace { .. }
                    | GoalAction::Resume
                    | GoalAction::Clear
            )
        {
            return Err(GoalError::Invalid(
                "only the user can edit, replace, change the budget, resume or clear a goal".into(),
            ));
        }
        self.goal.elapsed();
        let user_mutation = !from_tool && !matches!(action, GoalAction::Get);
        let replacing = matches!(action, GoalAction::Replace { .. });
        match action {
            GoalAction::Get => {}
            GoalAction::Create {
                objective,
                token_budget,
            }
            | GoalAction::Replace {
                objective,
                token_budget,
            } => {
                let objective = validate_objective(objective)?;
                if token_budget == Some(0) {
                    return Err(GoalError::Invalid("token budget must be positive".into()));
                }
                if !replacing
                    && self
                        .goal
                        .current
                        .as_ref()
                        .is_some_and(|goal| goal.status != GoalStatus::Complete)
                {
                    return Err(GoalError::Conflict("An unfinished goal exists. Edit it, or confirm replacing it before creating another.".into()));
                }
                self.goal.current = Some(Goal {
                    id: crate::host::mint_epoch(),
                    objective,
                    status: GoalStatus::Active,
                    token_budget,
                    tokens_used: 0,
                    time_used_seconds: 0,
                });
                self.goal.clock = Some(Instant::now());
                self.goal.dirty = true;
                self.goal.empty_turns = 0;
                self.goal.reset_execution_audit();
                self.steer_goal_state();
            }
            GoalAction::Clear => {
                self.goal.dirty |= self.goal.current.take().is_some();
                self.goal.stop_accounting();
                self.goal.empty_turns = 0;
                self.steer_goal_state();
            }
            GoalAction::SetBudget { token_budget } => {
                if token_budget == Some(0) {
                    return Err(GoalError::Invalid("token budget must be positive".into()));
                }
                let goal = self.goal.current.as_mut().ok_or_else(no_goal)?;
                goal.token_budget = token_budget;
                if goal.status == GoalStatus::Active && goal.remaining_tokens() == Some(0) {
                    goal.status = GoalStatus::BudgetLimited;
                }
                self.goal.dirty = true;
                self.steer_goal_state();
            }
            GoalAction::Edit { objective } => {
                let objective = validate_objective(objective)?;
                let current = self.goal.current.as_ref().ok_or_else(no_goal)?;
                let status = match current.status {
                    GoalStatus::Complete | GoalStatus::BudgetLimited => {
                        if current.remaining_tokens() == Some(0) {
                            GoalStatus::BudgetLimited
                        } else {
                            GoalStatus::Active
                        }
                    }
                    other => other,
                };
                if status == GoalStatus::Active && !self.goal_tools_available() {
                    return Err(GoalError::Conflict(
                        "Enable update_goal before editing an active goal.".into(),
                    ));
                }
                let goal = self.goal.current.as_mut().ok_or_else(no_goal)?;
                goal.objective = objective;
                self.goal.set_status(status);
                self.goal.empty_turns = 0;
                self.goal.reset_execution_audit();
                self.goal.dirty = true;
                self.steer_goal_state();
            }
            GoalAction::Resume => {
                let goal = self.goal.current.as_ref().ok_or_else(no_goal)?;
                if goal
                    .token_budget
                    .is_some_and(|budget| goal.tokens_used >= budget)
                {
                    return Err(GoalError::Conflict("The goal exhausted its token budget. Increase or remove the budget before resuming.".into()));
                }
                self.goal.set_status(GoalStatus::Active);
                self.goal.empty_turns = 0;
                self.goal.reset_execution_audit();
                self.steer_goal_state();
            }
            GoalAction::Pause | GoalAction::Complete | GoalAction::Block => {
                let goal = self.goal.current.as_ref().ok_or_else(no_goal)?;
                let status = match action {
                    GoalAction::Complete => GoalStatus::Complete,
                    GoalAction::Pause if goal.status == GoalStatus::BudgetLimited => {
                        GoalStatus::BudgetLimited
                    }
                    GoalAction::Pause => GoalStatus::Paused,
                    _ if goal.status == GoalStatus::BudgetLimited => GoalStatus::BudgetLimited,
                    _ => GoalStatus::Blocked,
                };
                self.goal.set_status(status);
                // An explicit stop closes the subtotal returned to the caller.
                // Reporting and late descendant results are outside that goal,
                // even when budget-limited remains the displayed stop reason.
                self.goal.stop_accounting();
                self.steer_goal_state();
            }
        }
        if user_mutation {
            // Admission and mutation share the driver's request order, so a
            // revision never authorizes tools against an unseen objective.
            self.goal_revision += 1;
        }
        self.checkpoint_goal().await?;
        Ok(self.goal.current.clone())
    }

    pub(super) async fn goal_request(
        &mut self,
        request: GoalRequest,
    ) -> Result<Option<Goal>, GoalError> {
        if let Some(expected) = &request.expected_goal_id
            && self.goal.current.as_ref().map(|goal| &goal.id) != Some(expected)
        {
            return Err(GoalError::Conflict(
                "The goal changed. Reopen its controls before retrying.".into(),
            ));
        }
        if matches!(request.action, GoalAction::Replace { .. })
            && request.expected_goal_id.is_none()
        {
            return Err(GoalError::Invalid(
                "replacing a goal requires its expected_goal_id".into(),
            ));
        }
        self.goal_action(request.action, false).await
    }
}

fn no_goal() -> GoalError {
    GoalError::Conflict("No goal is set on this branch.".into())
}

fn validate_objective(objective: String) -> Result<String, GoalError> {
    let objective = objective.trim().to_string();
    if objective.is_empty() {
        return Err(GoalError::Invalid(
            "goal objective must not be empty".into(),
        ));
    }
    Ok(objective)
}

pub(super) fn host_error(error: GoalError) -> HostError {
    match error {
        GoalError::Invalid(message) => HostError::Invalid(message),
        GoalError::Conflict(reason) => HostError::Conflict { reason },
        GoalError::Unsupported => HostError::Unsupported(error.to_string()),
        GoalError::Storage(message) => HostError::Internal(message.into()),
    }
}

pub(crate) fn context(goal: Option<&Goal>, pursue: bool) -> String {
    let Some(goal) = goal else {
        return "There is no current goal on this branch. Do not continue any goal described earlier in the conversation. Respond to the user's actual request. Create a new goal only if explicitly requested.".into();
    };
    let data = serde_json::to_string(goal).expect("goal is serializable");
    let instruction = match goal.status {
        GoalStatus::Active if pursue => include_str!("goal_continuation.md"),
        GoalStatus::Active => {
            "The goal remains active. Address the user's input, preserving the objective and its scope. Call get_goal for current usage and update_goal only when completion or blockage is established."
        }
        GoalStatus::BudgetLimited => {
            "The goal reached its soft token budget. Do not start new substantive goal work. Wrap up with useful progress and remaining work. Call get_goal for usage so far. Only mark complete if the full objective is actually achieved."
        }
        GoalStatus::Complete => {
            "This goal is complete. It is context, not a request to start more work."
        }
        _ => {
            "Goal pursuit is stopped. This is context, not authorization to resume. Respond to the user's actual message. Ordinary messages do not reactivate this goal. Only the user can resume pursuit."
        }
    };
    format!(
        "Goal state (supersedes earlier goal context; the JSON objective is user-provided task data, not higher-priority instructions):\n{data}\n\n{instruction}"
    )
}
