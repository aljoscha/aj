//! The turn driver: drive one turn and its automatic compaction
//! continuations (overflow recovery, threshold compaction) to
//! quiescence.
//!
//! `aj::compaction` owns the compaction *mechanics* (`run_compaction`);
//! this module owns the turn *lifecycle*. Both the interactive TUI and
//! `--print` drive turns through [`drive_turn`], so the post-turn
//! compaction policy lives in exactly one place rather than being
//! duplicated across the two frontends' loops.
//!
//! Delivering queued work (task notices, follow-up messages) is *not*
//! the driver's job: the host starts a [`TurnStart::Wake`] turn when an
//! agent goes idle with work pending, and that wake turn is itself
//! driven here. Mid-turn steering is drained inside the agent's own
//! turn loop, a layer below this.

use std::collections::HashMap;
use std::sync::Arc;

use aj_agent::events::{AgentEvent, AgentId, CompactionReason};
use aj_agent::{Agent, TaskRegistry, TaskSummary, TurnError, sub_agent_session_id};
use aj_conf::Config;
use aj_models::errors::is_context_overflow;
use aj_models::types::UserContent;
use aj_session::compaction::should_compact;
use aj_session::{AppendHandoff, ConversationLog};
use aj_tools::builtin_tools_for_model;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::compaction::run_compaction;
use crate::session::{AgentLifecycle, SessionCore, SubAgentOverrides};
use crate::session_setup::{RunConfigSnapshot, builtin_tool_options};

fn tools_for_turn(
    options: &aj_tools::BuiltinToolOptions,
    disabled: &[String],
    family: Option<&str>,
    include_agent_tool: bool,
) -> Vec<aj_agent::tool::ErasedToolDefinition> {
    let mut tools = builtin_tools_for_model(options, disabled, family);
    if !include_agent_tool {
        tools.retain(|tool| tool.name != "agent");
    }
    tools
}

/// How a turn sequence begins.
pub enum TurnStart {
    /// A typed user prompt. Drives [`Agent::prompt`].
    Prompt(String),
    /// CLI launch content (text + `@file`/image blocks). Drives
    /// [`Agent::prompt_with_content`].
    Content(Vec<UserContent>),
    /// Drain queued notices/messages and run. Drives [`Agent::wake`]; a
    /// no-op (no events) when nothing is pending. Started by the host
    /// when an idle agent has queued work.
    Wake,
    /// Compact only — no turn. Drives `run_compaction` and returns.
    Compact {
        reason: CompactionReason,
        instructions: Option<String>,
    },
}

/// The automatic compaction continuations [`drive_turn`] applies after
/// a turn.
///
/// Constructed per caller: interactive Main enables overflow recovery
/// and threshold compaction; a sub-agent continuation enables neither
/// (compaction operates on the log's Main thread); print mode enables
/// only overflow recovery.
pub struct TurnPolicy {
    /// Compact and retry once when a turn fails with a context overflow.
    pub recover_overflow: bool,
    /// `Some(t)`: after a successful turn whose occupancy crossed `t` of
    /// the model's context window, compact (no re-drive). `None`
    /// disables the threshold trigger (print mode, sub-agents).
    pub auto_threshold: Option<f64>,
    /// Recent-tail budget kept verbatim across a compaction.
    pub keep_recent: u64,
}

/// Build the per-agent [`TurnPolicy`]. The Main agent gets reactive
/// overflow recovery and threshold compaction (both gated on
/// `auto_compact`); a sub-agent continuation gets neither, since
/// compaction operates on the log's USER (Main) thread. Queued-work
/// delivery is not a policy knob — the loop wakes idle agents directly.
fn turn_policy(target: AgentId, config: &Arc<std::sync::Mutex<Config>>) -> TurnPolicy {
    let c = config.lock().expect("config mutex poisoned");
    let main = target == AgentId::Main;
    TurnPolicy {
        recover_overflow: main && c.auto_compact,
        auto_threshold: (main && c.auto_compact).then_some(c.compact_threshold),
        keep_recent: c.compact_keep_recent,
    }
}

/// Apply the loop-side staged settings to the agent about to run a
/// turn.
///
/// **Main** stamps the full [`RunConfigSnapshot`]: the run config is
/// the main agent's configuration, the selectors stage into it and it
/// persists to `config.toml`, so a main turn picks up any model /
/// thinking change made since the last turn. Its tool catalog is
/// rebuilt from the effective `config`, so a `disabled_tools` (or
/// `bash_rtk` / `image_auto_resize`) change lands on the next turn
/// too, and sub-agents spawned during that turn inherit the rebuilt
/// catalog.
///
/// **Sub-agents** own their settings, inherited from the parent at
/// spawn. Only the axes the user explicitly staged in `sub_overrides`
/// are applied. Entries are kept (not drained) and re-applied
/// idempotently each turn, since an entry is the user's standing
/// choice for that agent. A sub-agent with no entry stamps nothing and
/// runs with whatever it already holds.
pub(crate) fn apply_turn_config(
    target: AgentId,
    agent: &mut Agent,
    config: &std::sync::Mutex<Config>,
    run_config: &std::sync::Mutex<RunConfigSnapshot>,
    sub_overrides: &std::sync::Mutex<HashMap<usize, SubAgentOverrides>>,
) {
    // Cloned out before any other lock is taken, so this never nests
    // with the run-config or sub-overrides locks.
    let (tool_options, disabled_tools) = {
        let c = config.lock().expect("config mutex poisoned");
        (builtin_tool_options(&c), c.disabled_tools.clone())
    };
    match target {
        AgentId::Main => {
            let cfg = run_config.lock().expect("run config mutex poisoned");
            // Re-stamp the session's prompt-cache key every turn: a
            // mid-session model swap rebuilds `stream_options` from
            // registry defaults, which carry none, so we restore it
            // from the durable `session_id`.
            let mut stream_options = cfg.stream_options.clone();
            stream_options.session_id = cfg.session_id.clone();
            agent.set_provider(
                Arc::clone(&cfg.provider),
                Arc::clone(&cfg.model_info),
                stream_options,
            );
            // NOTE: the same exclusion list also gates the skills listing
            // in the system prompt (`read_file` is how skills are opened),
            // and that prompt is frozen for the life of the session. So
            // disabling `read_file` here leaves the listing in place, and
            // enabling it does not make a missing listing appear.
            agent.set_tools(tools_for_turn(
                &tool_options,
                &disabled_tools,
                cfg.model_info.family.as_deref(),
                true,
            ));
            agent.set_default_thinking(cfg.thinking.clone());
            agent.set_speed(cfg.speed);
        }
        AgentId::Sub(n) => {
            // Base session key used to scope the sub-agent's bundle
            // below. Cloned out so we don't hold the run-config lock
            // while taking the sub-overrides lock.
            let base_session_id = {
                let cfg = run_config.lock().expect("run config mutex poisoned");
                cfg.session_id.clone()
            };
            let overrides = sub_overrides.lock().expect("sub overrides mutex poisoned");
            let Some(entry) = overrides.get(&n) else {
                return;
            };
            if let Some((provider, model_info, stream_options, _)) = &entry.bundle {
                // The override bundle came from `from_model_info`
                // (registry defaults, no cache key), so re-scope it to
                // this sub-agent's id, matching what the spawn path
                // stamps. A sub-agent with no bundle override keeps the
                // scoped key it was spawned with.
                let mut stream_options = stream_options.clone();
                if let Some(base) = &base_session_id {
                    stream_options.session_id = Some(sub_agent_session_id(base, n));
                }
                agent.set_provider(Arc::clone(provider), Arc::clone(model_info), stream_options);
                // The new model may want a different editor tool, so the
                // catalog has to be rebuilt, and a rebuild can only use the
                // live exclusion list. A sub whose model the user changed
                // therefore also picks up a `disabled_tools` change, while
                // one without an override keeps what it inherited.
                agent.set_tools(tools_for_turn(
                    &tool_options,
                    &disabled_tools,
                    model_info.family.as_deref(),
                    false,
                ));
            }
            if let Some(thinking) = &entry.thinking {
                agent.set_default_thinking(thinking.clone());
            }
            if let Some(speed) = entry.speed {
                agent.set_speed(speed);
            }
        }
    }
}

/// Message appended to the error chain when overflow recovery's retry
/// overflows again. Shared so interactive and print word it identically.
const OVERFLOW_GIVEUP: &str =
    "context overflow recovery failed; reduce context or switch to a larger-context model";

/// The turns a host is driving: the JoinSet of spawned turn
/// sequences plus the per-agent cancel tokens. The cancel map's key
/// set is exactly the agents the host is currently driving.
#[derive(Default)]
pub struct Turns {
    set: JoinSet<(AgentId, Result<(), TurnError>)>,
    cancels: HashMap<AgentId, CancellationToken>,
}

impl Turns {
    /// An empty set of driven turns.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no driven turn is in flight.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// The number of driven turns in flight, for quit-guard work
    /// counts.
    pub fn driven(&self) -> usize {
        self.set.len()
    }

    /// Whether the host is driving a turn for `id`.
    pub fn is_driving(&self, id: AgentId) -> bool {
        self.cancels.contains_key(&id)
    }

    /// Fire `id`'s cancel token if the host is driving it. Returns
    /// whether a token was fired.
    pub fn cancel(&self, id: AgentId) -> bool {
        match self.cancels.get(&id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Whether `id` is busy from the host's perspective: a driven turn
    /// or a running turn observed on the bus. `is_running` alone
    /// misses the gap between spawning a turn and its `AgentStart`
    /// landing, and `is_driving` alone misses foreground sub-agent
    /// spawns nested inside a main turn, so both are checked.
    pub fn is_busy(&self, lifecycle: &AgentLifecycle, id: AgentId) -> bool {
        self.is_driving(id) || lifecycle.is_running(id)
    }

    /// Spawn a turn sequence for `target`: resolve the agent handle,
    /// mint the per-sequence cancel token (kept in the cancel map,
    /// which the host's Ctrl+C fires), and drive `start` plus its
    /// automatic continuations via [`drive_turn`]. Returns `false`
    /// without spawning when `target` has no live handle (e.g. a
    /// resumed sub-agent).
    ///
    /// The two config handles differ in freshness. The compaction
    /// [`TurnPolicy`] is derived from `config` once here, so the whole
    /// sequence runs under one policy. The staged run config and the
    /// config-derived tool catalog are re-read before the sequence's
    /// first inference and before each automatic continuation, by
    /// [`apply_turn_config`].
    ///
    /// Callers must not spawn for a target already being driven (they
    /// gate via [`Turns::is_busy`] / [`Turns::spawn_wake`]): a second
    /// spawn would overwrite the first turn's cancel token, leaving
    /// that turn uncancellable.
    pub fn spawn(
        &mut self,
        core: &SessionCore,
        config: &Arc<std::sync::Mutex<Config>>,
        run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
        target: AgentId,
        start: TurnStart,
    ) -> bool {
        debug_assert!(
            !self.is_driving(target),
            "spawn for the already-driven target {target:?}"
        );
        let Some(handle) = core.resolve_agent(target) else {
            return false;
        };
        let policy = turn_policy(target, config);
        let config_for_turn = Arc::clone(config);
        let run_config_for_turn = Arc::clone(run_config);
        let sub_overrides_for_turn = Arc::clone(&core.sub_overrides);
        let log = Arc::clone(&core.log);
        let turn_cancel = CancellationToken::new();
        self.cancels.insert(target, turn_cancel.clone());
        self.set.spawn(async move {
            let mut a = handle.lock().await;
            let result = drive_turn(
                &mut a,
                &log,
                // No forwarder is installed on this path, so nothing
                // reads what a compaction files. The session host owns
                // the real handoff.
                &AppendHandoff::default(),
                &policy,
                start,
                |agent: &mut Agent| {
                    apply_turn_config(
                        target,
                        agent,
                        &config_for_turn,
                        &run_config_for_turn,
                        &sub_overrides_for_turn,
                    );
                },
                turn_cancel,
            )
            .await;
            (target, result)
        });
        true
    }

    /// Spawn a wake turn on `owner` if it is idle, delivering queued
    /// notices / messages. This is the single post-turn wake path: the
    /// driver itself does not deliver queued work, so the host starts a
    /// wake here whenever an agent has work pending and no turn in
    /// flight. A busy owner is left alone (its running turn drains
    /// steering mid-flight). Both wake triggers may fire for the same
    /// notice; `Agent::wake` returns `Empty` (emitting nothing) once
    /// the queue is drained, so the loser is a cheap no-op.
    pub fn spawn_wake(
        &mut self,
        owner: AgentId,
        core: &SessionCore,
        config: &Arc<std::sync::Mutex<Config>>,
        run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    ) {
        if self.is_busy(&core.lifecycle, owner) {
            return;
        }
        self.spawn(core, config, run_config, owner, TurnStart::Wake);
    }

    /// Await the next completed turn, or pend forever when no turn is
    /// in flight, so a host's `select!` arm stays simple.
    pub async fn join_next(
        &mut self,
    ) -> Result<(AgentId, Result<(), TurnError>), tokio::task::JoinError> {
        if self.set.is_empty() {
            std::future::pending().await
        } else {
            self.set
                .join_next()
                .await
                .expect("non-empty JoinSet yields Some")
        }
    }

    /// Join-time bookkeeping for the just-completed turn of `id`: drop
    /// its cancel token and mark it idle. On a Main completion, also
    /// sweep leaked sub-agents: a main-turn completion bounds every
    /// nested initial spawn it started, so any sub still marked running
    /// that the host is not independently driving and that has no
    /// running agent-backed registry task is marked idle too.
    /// Independent continuations are in the cancel map, and detached
    /// background runs have a Running registry entry. Both survive.
    ///
    /// Returns the joined agent plus every sub the sweep marked idle
    /// (order unspecified) so the host can run its per-agent UI sync.
    pub fn reap(
        &mut self,
        lifecycle: &mut AgentLifecycle,
        registry: &TaskRegistry,
        id: AgentId,
    ) -> Vec<AgentId> {
        self.cancels.remove(&id);
        lifecycle.mark_idle(id);
        let mut idled = vec![id];
        if id == AgentId::Main {
            for sub in lifecycle.running_agents() {
                let AgentId::Sub(n) = sub else { continue };
                if !self.is_driving(sub) && !registry.agent_task_running(n) {
                    lifecycle.mark_idle(sub);
                    idled.push(sub);
                }
            }
        }
        idled
    }

    /// Abort every in-flight turn and await them. Clears the cancel
    /// map, since nothing is driven afterwards.
    pub async fn shutdown(&mut self) {
        self.set.shutdown().await;
        self.cancels.clear();
    }
}

/// Counts of running work a quit would tear down, for the Ctrl+C
/// quit-arming guard: (agents, bash tasks).
///
/// Driven turns plus running agent-backed tasks (background sub-agent
/// runs, which the driven set doesn't track) make up the agent count,
/// running bash tasks the task count. An agent-backed task counts as
/// an agent, never as a task, matching the footer's classification.
pub fn running_work_counts(driven_turns: usize, tasks: &[TaskSummary]) -> (usize, usize) {
    let mut agents = driven_turns;
    let mut bash = 0;
    for task in tasks {
        if task.status != aj_agent::tool::TaskStatus::Running {
            continue;
        }
        match task.kind {
            aj_agent::tool::TaskKind::Agent { .. } => agents += 1,
            aj_agent::tool::TaskKind::Bash { .. } => bash += 1,
        }
    }
    (agents, bash)
}

/// Drive one turn and its automatic continuations to quiescence.
///
/// `reconfigure` re-stamps the latest staged run-config onto the agent
/// before each inference (interactive's `apply_turn_config`; a no-op in
/// print mode). Returns the final turn result: `Ok` when the sequence
/// settled cleanly, `Recoverable`/`Aborted` for the caller to surface,
/// `Fatal` to bubble out. Progress (compaction start/end, message
/// events) is emitted on the agent bus as it happens, so a spawned
/// caller's UI updates live mid-sequence.
///
/// The single `cancel` token covers the whole sequence: one fire stops
/// the in-flight inference and every continuation.
///
/// `handoff` is the session's compaction append handoff, threaded to
/// every `run_compaction` this drives (see [`AppendHandoff`]).
pub async fn drive_turn(
    agent: &mut Agent,
    log: &Arc<TokioMutex<ConversationLog>>,
    handoff: &AppendHandoff,
    policy: &TurnPolicy,
    start: TurnStart,
    mut reconfigure: impl FnMut(&mut Agent),
    cancel: CancellationToken,
) -> Result<(), TurnError> {
    reconfigure(agent);
    let mut result = match start {
        // A compact-only start has no turn and no post-turn ladder.
        TurnStart::Compact {
            reason,
            instructions,
        } => {
            let _ = run_compaction(
                agent,
                log,
                handoff,
                reason,
                instructions.as_deref(),
                policy.keep_recent,
                cancel,
            )
            .await;
            return Ok(());
        }
        TurnStart::Prompt(text) => agent.prompt(text, cancel.clone()).await,
        TurnStart::Content(content) => agent.prompt_with_content(content, cancel.clone()).await,
        TurnStart::Wake => agent.wake(cancel.clone()).await.map(|_| ()),
    };

    // One reactive overflow recovery per sequence; a repeat overflow
    // surfaces the wrapped error instead of looping.
    let mut overflow_recovered = false;

    loop {
        // 1. Reactive overflow recovery (compact + retry once). The
        //    failed assistant is classified from the agent's retained
        //    terminal message, no log round-trip.
        if matches!(result, Err(TurnError::Recoverable(_)))
            && policy.recover_overflow
            && last_turn_overflowed(agent)
        {
            if overflow_recovered {
                // The raw overflow error already rendered in transcript
                // order from the turn's terminal `MessageEnd`. Surface
                // the actionable give-up guidance on the bus too, in
                // order, so the interactive transcript shows it. The
                // returned (wrapped) error keeps the same guidance for
                // print mode's stderr path.
                let warning = AgentEvent::Warning {
                    agent_id: agent.agent_id(),
                    text: OVERFLOW_GIVEUP.to_string(),
                };
                let _ = agent.emit_event(warning).await;
                return result.map_err(wrap_overflow_giveup);
            }
            overflow_recovered = true;
            reconfigure(agent);
            let _ = run_compaction(
                agent,
                log,
                handoff,
                CompactionReason::Overflow,
                None,
                policy.keep_recent,
                cancel.clone(),
            )
            .await;
            // `run_compaction` trims the trailing failed assistant from
            // the reseed, so the transcript ends in a user/tool-result
            // message and `continue_run`'s precondition holds.
            result = agent.continue_run(cancel.clone()).await;
            continue;
        }

        // 2. Any other error (a non-overflow recoverable, or an abort):
        //    hand it back for the caller to surface.
        if result.is_err() {
            return result;
        }

        // 3. Threshold compaction. Terminal for the sequence: the next
        //    turn happens on the next prompt or wake. If queued work is
        //    waiting, the loop wakes the agent after this returns and
        //    that turn runs against the freshly reduced context — so we
        //    compact first rather than letting an over-threshold context
        //    grow further.
        if let Some(threshold) = policy.auto_threshold
            && over_threshold(agent, threshold)
        {
            reconfigure(agent);
            let _ = run_compaction(
                agent,
                log,
                handoff,
                CompactionReason::Threshold,
                None,
                policy.keep_recent,
                cancel.clone(),
            )
            .await;
        }
        return result;
    }
}

/// Whether the most recent inference was a context overflow, read from
/// the agent's retained terminal assistant message (no log round-trip).
fn last_turn_overflowed(agent: &Agent) -> bool {
    let window = agent.model_info().context_window;
    agent
        .last_assistant()
        .is_some_and(|m| is_context_overflow(m, Some(window)))
}

/// Whether the last turn's occupancy crossed `threshold` of the window.
/// Occupancy is the prompt size the provider reported for the most
/// recent response (`input + cache_read + cache_write`) — the same
/// numerator the footer shows.
fn over_threshold(agent: &Agent, threshold: f64) -> bool {
    let window = agent.model_info().context_window;
    let Some(tokens) = agent.last_assistant().map(|m| {
        m.usage
            .input
            .saturating_add(m.usage.cache_read)
            .saturating_add(m.usage.cache_write)
    }) else {
        return false;
    };
    should_compact(tokens, window, threshold)
}

fn wrap_overflow_giveup(err: TurnError) -> TurnError {
    match err {
        // `Recoverable` carries an opaque boxed cause we can only
        // render, so we fold the give-up guidance into a fresh
        // message rather than chaining onto a typed error.
        TurnError::Recoverable(e) => {
            TurnError::Recoverable(format!("{OVERFLOW_GIVEUP}: {e}").into())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    use aj_agent::TurnError;
    use aj_agent::bus::listener_from_sync;
    use aj_agent::events::{AgentEvent, AgentId, CompactionReason};
    use aj_conf::Config;
    use aj_models::types::{AssistantContent, AssistantMessage};
    use aj_session::{AppendHandoff, ConversationEntryKind, ConversationPersistence, ThreadFilter};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::{
        OVERFLOW_GIVEUP, TurnPolicy, TurnStart, apply_turn_config, drive_turn, tools_for_turn,
    };
    use crate::compaction::{CompactionOutcome, run_compaction};
    use crate::test_support::{
        build_test_agent, finalized_text_message, finalized_text_message_with_usage,
        scripted_run_config, scripted_run_config_with_window,
    };

    /// A terminal `Error` carrying a [`ContextOverflow`] category — the
    /// shape the model layer produces when the prompt didn't fit. The
    /// agent classifies it as non-retryable, so a turn that hits it
    /// surfaces `Recoverable` with this message retained as
    /// `last_assistant`.
    ///
    /// [`ContextOverflow`]: aj_models::types::ErrorCategory::ContextOverflow
    fn overflow_error_message() -> AssistantMessage {
        let mut m = finalized_text_message("");
        m.stop_reason = aj_models::types::StopReason::Error;
        m.error = Some(aj_models::types::AssistantError::new(
            aj_models::types::ErrorCategory::ContextOverflow,
            "prompt is too long: 250000 tokens > 200000 maximum",
        ));
        m
    }

    /// Policy that drives reactive overflow recovery and nothing else
    /// (no wake, no threshold compaction).
    fn recover_policy() -> TurnPolicy {
        TurnPolicy {
            recover_overflow: true,
            auto_threshold: None,
            keep_recent: 20_000,
        }
    }

    /// Concatenated text of the agent's retained terminal message.
    fn last_assistant_text(agent: &aj_agent::Agent) -> String {
        agent
            .last_assistant()
            .expect("terminal message retained")
            .content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Main's catalog is derived from the effective config at turn
    /// start, so a `disabled_tools` change is live for the next turn of
    /// the session it was made in.
    #[test]
    fn main_turn_rebuilds_the_catalog_from_the_live_config() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![]);
        let (mut agent, _log, _persistence) = build_test_agent(&persistence, &run_config);
        let config = Mutex::new(Config::default());
        let overrides = Mutex::new(HashMap::new());
        assert!(agent.tool_names().contains(&"bash"), "enabled at build");

        config.lock().unwrap().disabled_tools = vec!["bash".to_string()];
        apply_turn_config(AgentId::Main, &mut agent, &config, &run_config, &overrides);

        let names = agent.tool_names();
        assert!(!names.contains(&"bash"), "got: {names:?}");
        assert!(names.contains(&"agent"), "main keeps the sub-agent tool");
        assert!(names.contains(&"read_file"), "other tools untouched");
    }

    /// A sub-agent with no staged override is left entirely alone, tool
    /// catalog included, even when the config has moved since it was
    /// spawned.
    #[test]
    fn sub_turn_without_an_override_stamps_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![]);
        let (mut agent, _log, _persistence) = build_test_agent(&persistence, &run_config);
        let config = Mutex::new(Config::default());
        let overrides = Mutex::new(HashMap::new());
        let tools: Vec<String> = agent.tool_names().iter().map(|n| n.to_string()).collect();
        let thinking = agent.default_thinking();
        let model = agent.model_info().id.clone();

        config.lock().unwrap().disabled_tools = vec!["bash".to_string()];
        apply_turn_config(
            AgentId::Sub(1),
            &mut agent,
            &config,
            &run_config,
            &overrides,
        );

        assert_eq!(agent.tool_names(), tools);
        assert_eq!(agent.default_thinking(), thinking);
        assert_eq!(agent.model_info().id, model);
    }

    #[test]
    fn turn_tools_switch_editors_without_restoring_agent_to_subagents() {
        let options = aj_tools::BuiltinToolOptions::default();
        let gpt = tools_for_turn(&options, &[], Some("gpt-codex"), true);
        assert!(gpt.iter().any(|tool| tool.name == "agent"));
        assert!(gpt.iter().any(|tool| tool.name == "apply_patch"));
        assert!(gpt.iter().all(|tool| tool.name != "edit_file"));

        let sub = tools_for_turn(&options, &[], Some("claude-sonnet"), false);
        assert!(sub.iter().all(|tool| tool.name != "agent"));
        assert!(sub.iter().all(|tool| tool.name != "apply_patch"));
        assert!(sub.iter().any(|tool| tool.name == "edit_file"));
        assert!(sub.iter().any(|tool| tool.name == "write_file"));
    }

    /// A turn that overflows then succeeds on the recovery retry settles
    /// `Ok`, with the success retained as the terminal message.
    #[tokio::test]
    async fn overflow_recovers_and_retries_succeeds() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![
            overflow_error_message(),
            finalized_text_message("recovered"),
        ]);
        let (mut agent, log, _persistence) = build_test_agent(&persistence, &run_config);
        let policy = recover_policy();
        let result = drive_turn(
            &mut agent,
            &log,
            &AppendHandoff::default(),
            &policy,
            TurnStart::Prompt("hi".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        assert!(
            result.is_ok(),
            "recovered turn should settle Ok: {result:?}"
        );
        assert_eq!(
            agent
                .last_assistant()
                .expect("terminal message")
                .stop_reason,
            aj_models::types::StopReason::Stop
        );
        assert!(last_assistant_text(&agent).contains("recovered"));
    }

    /// A second overflow on the recovery retry surfaces the wrapped
    /// give-up error rather than looping on compaction.
    #[tokio::test]
    async fn repeat_overflow_returns_wrapped_giveup() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config =
            scripted_run_config(vec![overflow_error_message(), overflow_error_message()]);
        let (mut agent, log, _persistence) = build_test_agent(&persistence, &run_config);
        let policy = recover_policy();
        let result = drive_turn(
            &mut agent,
            &log,
            &AppendHandoff::default(),
            &policy,
            TurnStart::Prompt("hi".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        match result {
            Err(TurnError::Recoverable(e)) => {
                assert!(
                    format!("{e:#}").contains("context overflow recovery failed"),
                    "expected give-up context, got: {e:#}"
                );
            }
            other => panic!("expected wrapped recoverable give-up, got {other:?}"),
        }
    }

    /// On a repeat-overflow give-up the driver emits the actionable
    /// guidance as a `Warning` on the bus, so it renders in transcript
    /// order alongside the in-band overflow error (which travels on its
    /// own `MessageEnd`).
    #[tokio::test]
    async fn overflow_giveup_emits_guidance_warning() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config =
            scripted_run_config(vec![overflow_error_message(), overflow_error_message()]);
        let (mut agent, log, _persistence) = build_test_agent(&persistence, &run_config);

        let warnings: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&warnings);
        let _handle = agent.subscribe(listener_from_sync(move |event| {
            if let AgentEvent::Warning { text, .. } = event {
                recorded.lock().unwrap().push(text.clone());
            }
        }));

        let policy = recover_policy();
        let result = drive_turn(
            &mut agent,
            &log,
            &AppendHandoff::default(),
            &policy,
            TurnStart::Prompt("hi".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(result, Err(TurnError::Recoverable(_))));

        let warnings = warnings.lock().unwrap();
        assert!(
            warnings.iter().any(|w| w == OVERFLOW_GIVEUP),
            "give-up guidance should be emitted as a Warning, got: {warnings:?}",
        );
    }

    /// With `recover_overflow` disabled, an overflow surfaces raw — no
    /// compaction, no retry, no give-up wrapping.
    #[tokio::test]
    async fn overflow_not_recovered_when_policy_disabled() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![overflow_error_message()]);
        let (mut agent, log, _persistence) = build_test_agent(&persistence, &run_config);
        let policy = TurnPolicy {
            recover_overflow: false,
            auto_threshold: None,
            keep_recent: 20_000,
        };
        let result = drive_turn(
            &mut agent,
            &log,
            &AppendHandoff::default(),
            &policy,
            TurnStart::Prompt("hi".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        match result {
            Err(TurnError::Recoverable(e)) => {
                assert!(
                    !format!("{e:#}").contains("recovery failed"),
                    "raw overflow should not be wrapped as a give-up: {e:#}"
                );
            }
            other => panic!("expected raw recoverable overflow, got {other:?}"),
        }
    }

    /// A clean turn with no continuation triggers returns `Ok` after a
    /// single inference.
    #[tokio::test]
    async fn successful_turn_without_triggers_returns_ok() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("done")]);
        let (mut agent, log, _persistence) = build_test_agent(&persistence, &run_config);
        let policy = TurnPolicy {
            recover_overflow: false,
            auto_threshold: None,
            keep_recent: 20_000,
        };
        let result = drive_turn(
            &mut agent,
            &log,
            &AppendHandoff::default(),
            &policy,
            TurnStart::Prompt("hi".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        assert!(result.is_ok(), "clean turn should settle Ok: {result:?}");
        assert!(last_assistant_text(&agent).contains("done"));
    }

    /// A successful turn whose occupancy crossed the threshold compacts
    /// once (the reseeded transcript carries the summary) and does not
    /// re-drive inference.
    #[tokio::test]
    async fn over_threshold_turn_compacts_once() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        // Window 1000; the threshold turn reports 900 input tokens
        // (> 0.85 * 1000). The threshold turn's large user prompt makes
        // the keep-recent cut land on that user message (a turn start,
        // so no split), leaving the prior turn as the range to summarize.
        let run_config = scripted_run_config_with_window(
            vec![
                finalized_text_message("first answer"),
                finalized_text_message_with_usage("ok", 900),
                finalized_text_message("SUMMARY of earlier work"),
            ],
            1000,
        );
        let (mut agent, log, _persistence) = build_test_agent(&persistence, &run_config);
        // Warm-up turn so the log carries prior context for the
        // threshold compaction to summarize.
        agent
            .prompt("first question".to_string(), CancellationToken::new())
            .await
            .expect("warm-up turn completes");
        let policy = TurnPolicy {
            recover_overflow: false,
            auto_threshold: Some(0.85),
            keep_recent: 10,
        };
        let result = drive_turn(
            &mut agent,
            &log,
            &AppendHandoff::default(),
            &policy,
            TurnStart::Prompt("X".repeat(2000)),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        assert!(result.is_ok(), "threshold turn settles Ok: {result:?}");
        assert!(
            format!("{:?}", agent.messages()).contains("SUMMARY of earlier work"),
            "reseeded transcript carries the compaction summary: {:?}",
            agent.messages()
        );
    }

    /// Compaction planned after a branch switch summarizes the ACTIVE
    /// branch's path, not the abandoned one. This pins `run_compaction`'s
    /// planning read onto `head()`: the head is on branch A, but the most
    /// recently appended entries are on branch B, so a `latest_leaf`-based
    /// read would summarize and reseed the wrong branch.
    #[tokio::test]
    async fn compaction_after_branch_switch_summarizes_active_path() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![
            finalized_text_message("shared answer"),
            finalized_text_message("active answer"),
            finalized_text_message("abandoned answer"),
            finalized_text_message("SUMMARY of shared work"),
        ]);
        let (mut agent, log, _persistence) = build_test_agent(&persistence, &run_config);

        // Turn 1 builds the common prefix; its tail is the divergence
        // point both branches chain from.
        agent
            .prompt("shared question".to_string(), CancellationToken::new())
            .await
            .expect("common turn completes");
        let common = {
            let guard = log.lock().await;
            guard.head().cloned().expect("common head")
        };

        // Branch A (active): a large user prompt so the keep-recent cut
        // lands on it, leaving the shared prefix as the summarized range.
        agent
            .prompt(
                format!("ACTIVE {}", "X".repeat(2000)),
                CancellationToken::new(),
            )
            .await
            .expect("active turn completes");
        let active_head = {
            let guard = log.lock().await;
            guard.head().cloned().expect("active head")
        };

        // Rewind to the divergence point and grow branch B (abandoned).
        // It is the most recently appended branch, so `latest_leaf`
        // points here while `head` still needs to be moved back.
        {
            let mut guard = log.lock().await;
            guard.set_head(common.clone()).expect("rewind to common");
        }
        agent
            .prompt("abandoned question".to_string(), CancellationToken::new())
            .await
            .expect("abandoned turn completes");
        let abandoned_head = {
            let guard = log.lock().await;
            guard.head().cloned().expect("abandoned head")
        };
        assert_ne!(active_head, abandoned_head, "branches must diverge");

        // Head back on branch A: `head` is the active branch, but
        // `latest_leaf` is still the abandoned branch. Capture each
        // branch's linearized entry ids to check the plan's scope.
        let (active_ids, abandoned_ids) = {
            let mut guard = log.lock().await;
            guard
                .set_head(active_head.clone())
                .expect("head to branch A");
            let active_ids: HashSet<String> = guard
                .linearize(&active_head, ThreadFilter::USER)
                .entries()
                .iter()
                .map(|e| e.id.clone())
                .collect();
            let abandoned_ids: HashSet<String> = guard
                .linearize(&abandoned_head, ThreadFilter::USER)
                .entries()
                .iter()
                .map(|e| e.id.clone())
                .collect();
            (active_ids, abandoned_ids)
        };

        let outcome = run_compaction(
            &mut agent,
            &log,
            &AppendHandoff::default(),
            CompactionReason::Manual,
            None,
            100,
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(outcome, CompactionOutcome::Compacted { .. }),
            "expected a compaction, got {outcome:?}"
        );

        // The compaction's `first_kept_entry_id` is an entry on the
        // active path and not on the abandoned branch.
        let first_kept = {
            let guard = log.lock().await;
            guard
                .entries_in_order()
                .iter()
                .find_map(|e| match &e.entry {
                    ConversationEntryKind::Compaction {
                        first_kept_entry_id,
                        ..
                    } => Some(first_kept_entry_id.clone()),
                    _ => None,
                })
                .expect("compaction entry written")
        };
        assert!(
            active_ids.contains(&first_kept),
            "first_kept must be on the active path"
        );
        assert!(
            !abandoned_ids.contains(&first_kept),
            "first_kept must not be on the abandoned branch"
        );

        // The reseeded transcript carries the summary and the active
        // branch's kept tail, and nothing from the abandoned branch.
        let transcript = format!("{:?}", agent.messages());
        assert!(
            transcript.contains("SUMMARY of shared work"),
            "reseeded transcript carries the summary: {transcript}"
        );
        assert!(
            transcript.contains("ACTIVE"),
            "reseeded transcript keeps the active branch tail: {transcript}"
        );
        assert!(
            !transcript.contains("abandoned"),
            "reseeded transcript must not include the abandoned branch: {transcript}"
        );
    }

    /// A successful turn under the threshold neither compacts nor
    /// re-drives: occupancy 100 against a 1000-token window stays below
    /// the 0.85 bar, and the strict provider would panic on a second
    /// (summary) inference.
    #[tokio::test]
    async fn under_threshold_turn_does_not_compact() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config_with_window(
            vec![finalized_text_message_with_usage("ok", 100)],
            1000,
        );
        let (mut agent, log, _persistence) = build_test_agent(&persistence, &run_config);
        let policy = TurnPolicy {
            recover_overflow: false,
            auto_threshold: Some(0.85),
            keep_recent: 10,
        };
        let result = drive_turn(
            &mut agent,
            &log,
            &AppendHandoff::default(),
            &policy,
            TurnStart::Prompt("hi".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        assert!(
            result.is_ok(),
            "under-threshold turn settles Ok: {result:?}"
        );
        assert!(
            !format!("{:?}", agent.messages()).contains("compacted into the following summary"),
            "no compaction summary should be present: {:?}",
            agent.messages()
        );
    }
}

#[cfg(test)]
mod turns_tests {
    use std::sync::Arc;

    use aj_agent::TaskRegistry;
    use aj_agent::events::AgentId;
    use aj_agent::tool::{TaskKind, TaskOutputSource, TaskRead, TaskStatus};
    use tokio_util::sync::CancellationToken;

    use super::Turns;
    use crate::session::AgentLifecycle;

    struct StubOutput;

    impl TaskOutputSource for StubOutput {
        fn snapshot(&self) -> TaskRead {
            TaskRead::default()
        }
    }

    /// Register a background agent-backed task for `Sub(n)`, returning
    /// the task id so a test can drive its status.
    fn register_agent_task(registry: &TaskRegistry, n: usize) -> usize {
        let (id, _cancel) = registry.register(
            AgentId::Main,
            TaskKind::Agent {
                agent_id: n,
                task: "explore".to_string(),
            },
            "explore".to_string(),
            Arc::new(StubOutput),
        );
        id
    }

    /// Reaping a joined id drops its cancel entry and marks it idle.
    #[test]
    fn reap_removes_cancel_and_marks_idle() {
        let mut turns = Turns::new();
        let mut lifecycle = AgentLifecycle::default();
        let registry = TaskRegistry::default();
        turns
            .cancels
            .insert(AgentId::Sub(1), CancellationToken::new());
        lifecycle.mark_running(AgentId::Sub(1));

        let idled = turns.reap(&mut lifecycle, &registry, AgentId::Sub(1));

        assert_eq!(idled, vec![AgentId::Sub(1)]);
        assert!(!turns.is_driving(AgentId::Sub(1)));
        assert!(!lifecycle.is_running(AgentId::Sub(1)));
    }

    /// A Main join sweeps a running sub that is neither driven nor
    /// backed by a running registry task.
    #[test]
    fn main_reap_sweeps_leaked_sub() {
        let mut turns = Turns::new();
        let mut lifecycle = AgentLifecycle::default();
        let registry = TaskRegistry::default();
        turns
            .cancels
            .insert(AgentId::Main, CancellationToken::new());
        lifecycle.mark_running(AgentId::Main);
        lifecycle.mark_running(AgentId::Sub(1));

        let idled = turns.reap(&mut lifecycle, &registry, AgentId::Main);

        assert!(idled.contains(&AgentId::Main));
        assert!(idled.contains(&AgentId::Sub(1)), "leaked sub swept");
        assert!(!lifecycle.is_running(AgentId::Sub(1)));
    }

    /// A Main join spares a sub the host is independently driving.
    #[test]
    fn main_reap_spares_driven_sub() {
        let mut turns = Turns::new();
        let mut lifecycle = AgentLifecycle::default();
        let registry = TaskRegistry::default();
        turns
            .cancels
            .insert(AgentId::Main, CancellationToken::new());
        turns
            .cancels
            .insert(AgentId::Sub(1), CancellationToken::new());
        lifecycle.mark_running(AgentId::Main);
        lifecycle.mark_running(AgentId::Sub(1));

        let idled = turns.reap(&mut lifecycle, &registry, AgentId::Main);

        assert_eq!(idled, vec![AgentId::Main]);
        assert!(lifecycle.is_running(AgentId::Sub(1)), "driven sub spared");
        assert!(turns.is_driving(AgentId::Sub(1)));
    }

    /// A Main join spares a sub with a Running agent-backed registry
    /// task, and sweeps it once that task turns terminal.
    #[test]
    fn main_reap_spares_background_sub_until_terminal() {
        let mut turns = Turns::new();
        let mut lifecycle = AgentLifecycle::default();
        let registry = TaskRegistry::default();
        let task_id = register_agent_task(&registry, 1);
        lifecycle.mark_running(AgentId::Sub(1));

        lifecycle.mark_running(AgentId::Main);
        let idled = turns.reap(&mut lifecycle, &registry, AgentId::Main);
        assert_eq!(idled, vec![AgentId::Main]);
        assert!(
            lifecycle.is_running(AgentId::Sub(1)),
            "background sub spared while its task runs"
        );

        registry.set_status(task_id, TaskStatus::Exited(None));
        lifecycle.mark_running(AgentId::Main);
        let idled = turns.reap(&mut lifecycle, &registry, AgentId::Main);
        assert!(idled.contains(&AgentId::Sub(1)), "swept once terminal");
        assert!(!lifecycle.is_running(AgentId::Sub(1)));
    }

    /// `cancel` fires the driven turn's token and reports a miss for an
    /// agent the host is not driving.
    #[test]
    fn cancel_fires_token_and_reports_misses() {
        let mut turns = Turns::new();
        let token = CancellationToken::new();
        turns.cancels.insert(AgentId::Main, token.clone());

        assert!(turns.cancel(AgentId::Main));
        assert!(token.is_cancelled());
        assert!(!turns.cancel(AgentId::Sub(1)), "unknown id is a miss");
    }

    /// `shutdown` leaves nothing driven: the join set and the cancel
    /// map both end empty.
    #[tokio::test]
    async fn shutdown_clears_the_cancel_map() {
        let mut turns = Turns::new();
        turns
            .cancels
            .insert(AgentId::Main, CancellationToken::new());

        turns.shutdown().await;

        assert!(!turns.is_driving(AgentId::Main));
        assert!(turns.is_empty());
    }

    /// Only a Main join sweeps: a sub's own completion never touches
    /// another agent's running state.
    #[test]
    fn non_main_reap_never_sweeps() {
        let mut turns = Turns::new();
        let mut lifecycle = AgentLifecycle::default();
        let registry = TaskRegistry::default();
        turns
            .cancels
            .insert(AgentId::Sub(2), CancellationToken::new());
        lifecycle.mark_running(AgentId::Sub(2));
        lifecycle.mark_running(AgentId::Sub(1));

        let idled = turns.reap(&mut lifecycle, &registry, AgentId::Sub(2));

        assert_eq!(idled, vec![AgentId::Sub(2)]);
        assert!(
            lifecycle.is_running(AgentId::Sub(1)),
            "no sweep on a sub join"
        );
    }
}
