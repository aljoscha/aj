//! Recovery: what an open does with what the last host left.
//!
//! Every exit of a host is a crash as far as the store can tell, so this
//! is the one path by which a session comes back. [`recover`] reads the
//! stored session, writes what settles it, and returns plain data that
//! says what a live session holds. A host installs that data
//! ([`crate::session::SessionCore`]), and so does print mode.
//!
//! Each settling write is idempotent, and each leaves state the next recovery
//! finishes from. A host that dies inside [`recover`], or after it and
//! before anything was installed, leaves a session the next open recovers
//! to the same result.
//!
//! What a host does with a turn that was interrupted is not decided here.
//! [`recover`] reports those turns and [`RESUME`] is the policy applied to
//! them.

use std::collections::BTreeMap;

use aj_agent::events::{AgentId, SubAgentConclusion};
use aj_agent::goal::{Goal, GoalStatus};
use aj_agent::message::AgentMessage;
use aj_agent::queue::{PendingInput, PendingKind};
use aj_models::types::ToolResultMessage;
use aj_session::{
    LogEntryKind, Pending, SessionError, SessionLog, SessionSettings, TaskRecord, ThreadFilter,
    TurnState, answer_tool_call, repair_interrupted_tool_uses, unanswered_spawns,
    unanswered_tool_calls,
};
use aj_tools::tools::bash::{
    DetachedProcess, SavedBashTask, ended_background_bash, still_running_outcome,
};
use anyhow::Result;

/// What a host does with an interrupted turn once the session is settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    /// The turn waits for the user's `continue`.
    WaitForUser,
    /// The host continues the turn by itself.
    Continue,
}

/// The resume policy, for every agent alike. This is the one place it is
/// decided: [`Recovered::interrupted`] lists the turns it applies to, of
/// the main agent and of sub-agents, and the host's driver applies it to
/// each when it starts. Under [`Resume::WaitForUser`] the `continue`
/// action resumes them all.
///
/// A sub-agent's run that was cut short is not such a turn. The open
/// closes it and tells its parent once that it can be continued by
/// sending the sub-agent a message. Resuming it instead would need the
/// run kept open across hosts, with its report still owed to the parent:
/// a task or an assignment taken up again for an agent built from its
/// stored run.
pub const RESUME: Resume = Resume::WaitForUser;

/// What a user is told about an interrupted turn that waits for them.
pub const TURN_INTERRUPTED_NOTICE: &str = "The last turn did not finish. Run \"continue\" \
     from the command palette to resume it. An interrupted tool call may run again.";

/// How the end record of a run its host abandoned reports it.
const INTERRUPTED_RUN: &str =
    "sub-agent interrupted: the session ended while it ran. Send it a message to continue.";

/// How the end record reports a background run that saved its completion
/// notification and whose host ended before the record was appended.
const FINISHED_RUN: &str = "sub-agent finished: the session ended before its end was recorded. \
     Its report was delivered to its parent.";

/// A settled session, as [`recover`] returns it.
#[derive(Debug, Default)]
pub struct Recovered {
    /// The main agent, on the thread the open landed on.
    pub main: RecoveredAgent,
    /// The sub-agents of that branch the last host left work for: pending
    /// input, an undelivered notification, a running task or an interrupted
    /// turn. A live session builds each at once. Every other sub-agent is built when it is
    /// first used.
    pub sub_agents: BTreeMap<usize, RecoveredSubAgent>,
    /// The branch's goal. A goal the last host was pursuing is paused.
    pub goal: Option<Goal>,
    /// Whether the session ever recorded a goal.
    pub goal_history: bool,
    /// The highest task id the session minted.
    pub last_task_id: usize,
    /// The agents whose turn the last host left unfinished and that wait
    /// for [`RESUME`]: the main agent, and a sub-agent that was in a turn
    /// after its run. Their turns stay stored as working, which is what
    /// the next open reads this from. A sub-agent's run that was cut short
    /// is not among them: it was closed, and its parent told.
    pub interrupted: Vec<AgentId>,
    /// What the user is told once, by the first client of this open.
    pub notices: Vec<String>,
}

/// One agent of a settled session.
#[derive(Debug, Default)]
pub struct RecoveredAgent {
    /// The agent's model context.
    pub transcript: Vec<AgentMessage>,
    /// The Code Mode values its thread committed.
    pub code_mode_store: BTreeMap<String, serde_json::Value>,
    /// The settings its thread recorded.
    pub settings: SessionSettings,
    /// Accepted input the agent has not taken, in the order it was saved.
    pub inputs: Vec<(PendingKind, PendingInput)>,
    /// Task notifications the agent has not been given, in the order they
    /// were saved.
    pub task_notifications: Vec<AgentMessage>,
    /// Task records whose commands were still running when they were looked
    /// at. A live session watches them.
    pub tasks: Vec<TaskRecord>,
}

/// A sub-agent of a settled session.
#[derive(Debug)]
pub struct RecoveredSubAgent {
    pub agent: RecoveredAgent,
    /// The tool that spawned it, which decides its tools and model.
    pub spawned_by_oracle: bool,
}

/// Settle the session `log` was just opened on and say what a live
/// session of it holds. A session that is not stored, or holds nothing
/// yet, settles to an empty result.
///
/// The steps, in order. Each is safe to repeat and to stop after:
///
/// 1. The main agent's tool calls without a result are answered (rule 6).
/// 2. Each sub-agent run left with its turn working is closed, and each
///    sub-agent the last host left work for has its open calls answered.
/// 3. Each task record that ended unwatched has its notification saved and
///    its record removed, in one transaction.
/// 4. A goal that was being pursued is paused.
/// 5. Everything is read back: transcripts, pending work, the commands
///    still running.
///
/// Step 1 comes before step 2 so that an `agent` call is answered as
/// interrupted, not with the end record step 2 appends.
///
/// A sub-agent that cannot be settled does not fail the open. It is left
/// as stored for the next open, and [`Recovered::notices`] names it. An
/// error that means this writer can no longer write the session is
/// returned.
pub fn recover(log: &mut SessionLog) -> Result<Recovered> {
    let mut recovered = Recovered::default();
    let main = AgentId::Main;

    settle_interrupted_calls(log, main)?;

    let mut skipped = Vec::new();
    let mut left = Vec::new();
    for sub in log.sub_agents_with_work()? {
        let n = sub.agent_id;
        let settled = if sub.working {
            settle_working_sub_agent(log, n)
        } else {
            settle_interrupted_calls(log, AgentId::Sub(n)).map(|()| TurnState::Idle)
        };
        match settled {
            Ok(turn) => left.push((n, turn == TurnState::Interrupted)),
            Err(error) => {
                skip_or_fail(n, error)?;
                skipped.push(n);
            }
        }
    }

    settle_tasks(log, main, &mut recovered.main);
    recovered.goal = pause_goal(log)?;
    recovered.goal_history = has_goal_history(log);

    read_agent(log, main, &mut recovered.main)?;
    if log.turn_state().get(main) == TurnState::Interrupted {
        recovered.interrupted.push(main);
    }
    for (n, interrupted) in left {
        let mut agent = RecoveredAgent::default();
        settle_tasks(log, AgentId::Sub(n), &mut agent);
        match read_sub_agent(log, n, agent) {
            Ok(Some(sub)) if sub.has_work() || interrupted => {
                recovered.sub_agents.insert(n, sub);
                if interrupted {
                    recovered.interrupted.push(AgentId::Sub(n));
                }
            }
            Ok(_) => {}
            Err(error) => {
                skip_or_fail(n, error)?;
                skipped.push(n);
            }
        }
    }

    recovered.last_task_id = log.task_records().last_id()?;
    recovered.notices = skipped
        .into_iter()
        .map(|n| {
            format!(
                "Sub-agent {n} could not be restored when this session was opened, so its \
                 interrupted or pending work was not picked up. The rest of the session is \
                 unaffected."
            )
        })
        .collect();
    if RESUME == Resume::WaitForUser {
        let waiting = recovered
            .interrupted
            .iter()
            .filter_map(|agent| match agent {
                AgentId::Sub(n) => Some(format!(
                    "Sub-agent {n}'s last turn did not finish. Run \"continue\" from the command \
                 palette to resume it, or send it a message."
                )),
                // A client shows the main agent's from the session's state.
                AgentId::Main => None,
            });
        recovered.notices.extend(waiting);
    }
    Ok(recovered)
}

impl RecoveredSubAgent {
    /// Whether a live session has something to install for it.
    fn has_work(&self) -> bool {
        let agent = &self.agent;
        !(agent.inputs.is_empty() && agent.task_notifications.is_empty() && agent.tasks.is_empty())
    }
}

/// Read sub-agent `n`'s run on the selected branch as a live session
/// builds an agent from it. `None` when the branch shows no run of it.
pub fn read_sub_agent_run(log: &mut SessionLog, n: usize) -> Result<Option<RecoveredSubAgent>> {
    read_sub_agent(log, n, RecoveredAgent::default())
}

fn read_sub_agent(
    log: &mut SessionLog,
    n: usize,
    mut agent: RecoveredAgent,
) -> Result<Option<RecoveredSubAgent>> {
    let Some(run) = log.agent_branch(AgentId::Sub(n))? else {
        return Ok(None);
    };
    let spawned_by_oracle = run.entries().iter().any(|entry| {
        matches!(&entry.entry,
            LogEntryKind::SubAgentSpawn { tool_name, .. } if tool_name == "oracle")
    });
    agent.transcript = run.agent_messages();
    agent.code_mode_store = run.code_mode_store();
    agent.settings = run.settings();
    read_pending(log, AgentId::Sub(n), &mut agent)?;
    Ok(Some(RecoveredSubAgent {
        agent,
        spawned_by_oracle,
    }))
}

/// Read the main agent's context and pending work into `into`.
fn read_agent(log: &SessionLog, agent: AgentId, into: &mut RecoveredAgent) -> Result<()> {
    if let Some(head) = log.head().cloned() {
        let branch = log.linearize(&head, ThreadFilter::of(agent));
        into.transcript = branch.agent_messages();
        into.code_mode_store = branch.code_mode_store();
        into.settings = branch.settings();
    }
    read_pending(log, agent, into)
}

fn read_pending(log: &SessionLog, agent: AgentId, into: &mut RecoveredAgent) -> Result<()> {
    for work in log.restore_pending(&log.pending(), agent)? {
        match work {
            Pending::Input(_, kind, input) => into.inputs.push((kind, input)),
            Pending::TaskNotification(_, message) => into.task_notifications.push(message),
        }
    }
    Ok(())
}

/// Look at `agent`'s task records. One that ended while nothing
/// watched it, or was lost, has its notification saved and its record removed.
/// One still running goes to `into` for a live session to watch.
///
/// A command whose end cannot be saved stays recorded for the next open,
/// and so does every command after a failed read.
fn settle_tasks(log: &SessionLog, agent: AgentId, into: &mut RecoveredAgent) {
    let tasks = log.task_records();
    let saved = match log.restore_tasks(&tasks, agent) {
        Ok(saved) => saved,
        Err(error) => {
            tracing::warn!("could not read {agent:?}'s background tasks: {error}");
            return;
        }
    };
    for task in saved {
        let bash = saved_bash_task(&task);
        let Some(notice) = ended_background_bash(&bash) else {
            into.tasks.push(task);
            continue;
        };
        match tasks.finish(task.id, agent, &notice.to_message()) {
            Ok(()) => bash.process.discard_exit_file(),
            Err(error) => {
                let id = task.id;
                tracing::warn!(
                    task_id = id,
                    "could not save a background task's end: {error}"
                );
            }
        }
    }
}

/// A task record as the bash tool takes it up.
pub(crate) fn saved_bash_task(task: &TaskRecord) -> SavedBashTask {
    SavedBashTask {
        id: task.id,
        owner: task.owner,
        call_id: task.call_id.clone(),
        command: task.command.clone(),
        running_for: (chrono::Utc::now() - task.started_at)
            .to_std()
            .unwrap_or_default(),
        process: DetachedProcess {
            pid: task.pid,
            start_time: task.process_start,
            output_path: task.output_path.clone(),
            exit_path: task.exit_path.clone(),
        },
    }
}

/// The goal of the selected branch, paused when the last host was
/// pursuing it: opening a session is not authorization to go on with its
/// goal. Terminal and blocked states keep their explanation.
pub(crate) fn pause_goal(log: &mut SessionLog) -> Result<Option<Goal>, SessionError> {
    let mut goal = match log.head() {
        Some(head) => log.goal_at(head)?,
        None => None,
    };
    if let Some(goal) = &mut goal
        && goal.status == GoalStatus::Active
    {
        goal.status = GoalStatus::Paused;
        log.append_goal_change(Some(goal.clone()))?;
    }
    Ok(goal)
}

/// Whether the session ever recorded a goal. Even a cleared goal needs a
/// model hint that supersedes old continuation instructions, and a
/// session without goal history needs none.
pub(crate) fn has_goal_history(log: &SessionLog) -> bool {
    log.entries_in_order()
        .iter()
        .any(|entry| matches!(entry.entry, LogEntryKind::GoalChange { .. }))
}

/// Answer every tool call `agent`'s last host left without a result on
/// its thread of the selected branch, so the branch is valid
/// input for the model again.
///
/// A bash call that launched a recorded background task is answered with
/// the task's id: the host ended between recording the task and reporting
/// the launch. A call that spawned a sub-agent, as the sub-agent's spawn
/// record names it, is told the sub-agent was interrupted and can be
/// continued, or is given the report of a run that ended without the call
/// receiving it. Every other call gets the generic result of
/// [`repair_interrupted_tool_uses`]. That includes a bash call that waited
/// for its command, which ended with the host, and a Code Mode `exec`
/// whose cell ended with it.
///
/// Every answer is appended for a call that is open, so a host that ends
/// anywhere in here answers no call twice, and the next call of this
/// function answers the rest.
fn settle_interrupted_calls(log: &mut SessionLog, agent: AgentId) -> Result<()> {
    let Some(branch) = log.agent_branch(agent)? else {
        return Ok(());
    };
    let open = unanswered_tool_calls(&branch);
    let tasks = log.task_records();
    for saved in log.restore_tasks(&tasks, agent)? {
        if !open
            .iter()
            .any(|(id, tool)| *id == saved.call_id && tool == "bash")
        {
            continue;
        }
        let outcome = still_running_outcome(saved.command, saved.id, saved.output_path)
            .map_err(anyhow::Error::from_boxed)?;
        let result = ToolResultMessage {
            tool_call_id: saved.call_id,
            tool_name: "bash".into(),
            details: serde_json::to_value(&outcome.details).ok(),
            content: outcome.content,
            is_error: outcome.is_error,
            timestamp: 0,
        };
        answer_tool_call(log, agent, result)?;
    }
    let answered = |log: &mut SessionLog| -> Result<_> {
        let branch = log.agent_branch(agent)?;
        Ok(branch.expect("answering keeps the branch"))
    };
    let branch = answered(log)?;
    for (call, tool, sub_agent) in unanswered_spawns(log, &branch) {
        let (text, is_error) = match sub_agent_end(log, sub_agent) {
            // The run ended and only its report did not reach the call.
            Some((conclusion, report)) => (
                format!(
                    "Sub-agent {sub_agent} ended its run, and the session ended before this \
                     call received its report. Its report:\n\n{}",
                    aj_agent::delivered_report(conclusion, &report)
                ),
                conclusion != SubAgentConclusion::Completed,
            ),
            None => (
                format!(
                    "Sub-agent {sub_agent} was interrupted before it reported: the session \
                     ended while this call waited for it. It keeps its branch and can \
                     be continued by sending it a message."
                ),
                true,
            ),
        };
        let result = ToolResultMessage::text(call, tool, text, is_error);
        answer_tool_call(log, agent, result)?;
    }
    let branch = answered(log)?;
    repair_interrupted_tool_uses(log, agent, &branch)?;
    Ok(())
}

/// How sub-agent `n`'s run on the selected branch ended, if it has its end
/// record. A run that cannot be read is taken for one that has none.
fn sub_agent_end(log: &mut SessionLog, n: usize) -> Option<(SubAgentConclusion, String)> {
    let run = match log.agent_branch(AgentId::Sub(n)) {
        Ok(run) => run?,
        Err(error) => {
            tracing::warn!("could not read sub-agent {n}'s run: {error}");
            return None;
        }
    };
    run.entries().iter().find_map(|entry| match &entry.entry {
        LogEntryKind::SubAgentEnd { conclusion, report } => Some((*conclusion, report.clone())),
        _ => None,
    })
}

/// Decide what an error while settling sub-agent `n` means. A session is
/// usable without one sub-agent, so the error is logged and the caller
/// skips the sub-agent. An error that says this writer cannot write the
/// session any more is returned, and fails the open.
fn skip_or_fail(n: usize, error: anyhow::Error) -> Result<()> {
    let fatal = error.chain().any(|cause| {
        cause
            .downcast_ref::<SessionError>()
            .is_some_and(SessionError::ends_the_writer)
    });
    if fatal {
        return Err(error);
    }
    tracing::warn!("sub-agent {n} could not be restored and is skipped: {error:#}");
    Ok(())
}

/// Settle sub-agent `n`, whose turn the last host left stored as working.
///
/// A run without an end record was abandoned by its host. It gets its one
/// `SubAgentEnd`, which says it was interrupted. A foreground run's parent
/// is told by the answer to its tool call ([`settle_interrupted_calls`]).
/// A background run's parent is told by the notification the run announced when
/// it started ([`aj_agent::interrupted_run_notice`]), which is among the
/// parent's pending work already. The stored turn is the signal, not the
/// missing end record alone: a run whose parent's turn was cancelled has
/// none either, and was ended by its host.
///
/// A run with an end record needs no closing: the turn left working is a
/// later one, or an earlier open appended the record and ended before it
/// stored the turn idle. Its open tool calls are answered, and its turn
/// is read as the main agent's is: interrupted while the thread
/// leaves input unanswered, which it stays stored as, and idle otherwise.
/// The reading is returned.
///
/// A run closed here has its open tool calls answered and its turn stored
/// idle, so later opens do not read it.
///
/// Exactly once across opens and a host that ends in between: the notification
/// has one id from the run's start to its delivery, whoever saves it, and
/// a save under that id replaces or is dropped as delivered. The end
/// record is appended after the notification is known to be saved, and the turn
/// is stored idle last.
fn settle_working_sub_agent(log: &mut SessionLog, n: usize) -> Result<TurnState> {
    let Some(run) = log.agent_branch(AgentId::Sub(n))? else {
        return Ok(TurnState::Idle);
    };
    let ended =
        |entry: &aj_session::LogEntry| matches!(entry.entry, LogEntryKind::SubAgentEnd { .. });
    if let Some(end) = run.entries().iter().position(ended) {
        // A later turn starts with a message that is not a tool result.
        // Without one, an earlier open closed the run and ended before it
        // stored the turn idle, and this one finishes that.
        let later_turn = run.entries()[end + 1..].iter().any(|entry| {
            let LogEntryKind::Message { message } = &entry.entry else {
                return false;
            };
            !matches!(
                message.as_stored_wire(),
                Some(aj_models::types::Message::ToolResult(_))
            )
        });
        settle_interrupted_calls(log, AgentId::Sub(n))?;
        if later_turn {
            return Ok(log.pick_up_turn(n)?);
        }
        log.turn_state().set(AgentId::Sub(n), TurnState::Idle)?;
        return Ok(TurnState::Idle);
    }
    {
        let background = run.entries().iter().any(|entry| {
            matches!(
                entry.entry,
                LogEntryKind::SubAgentSpawn {
                    background: true,
                    ..
                }
            )
        });
        let mut report = INTERRUPTED_RUN;
        let stored = |notice: &AgentMessage| serde_json::to_value(notice).ok();
        if background {
            let announced = aj_agent::interrupted_run_notice(n);
            let pending = log.pending();
            let saved = log.restore_pending(&pending, AgentId::Main)?;
            let saved = saved.into_iter().find_map(|work| match work {
                Pending::TaskNotification(_, notice) if notice.id() == announced.id() => {
                    Some(notice)
                }
                _ => None,
            });
            match saved {
                // The run saved its completion and its host ended before
                // the end record. The parent has the report.
                Some(saved) if stored(&saved) != stored(&announced) => report = FINISHED_RUN,
                Some(_) => {}
                // The host ended between the spawn record and the
                // announcement, or the notification was delivered, which the
                // save then finds and does nothing.
                None => pending.save_task_notification(AgentId::Main, &announced)?,
            }
        }
        log.append_subagent_end(n, SubAgentConclusion::Failed, report)?;
    }
    settle_interrupted_calls(log, AgentId::Sub(n))?;
    log.turn_state().set(AgentId::Sub(n), TurnState::Idle)?;
    Ok(TurnState::Idle)
}
