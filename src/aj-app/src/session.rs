//! Frontend-agnostic per-session state.
//!
//! A [`SessionCore`] owns everything whose lifetime is one session
//! session and that does not depend on a rendering backend: the agent,
//! its sub-agent registry, the session log, the bus
//! subscriptions, the staged settings, and the agent-lifecycle sets. A
//! frontend wraps a `SessionCore` in its own view type (the `aj` binary
//! adds an event pump plus the install/reconcile view work). Session
//! changes (new session, resume) build a fresh core and replace the old
//! one wholesale instead of mutating shared state back into a pristine
//! shape, so per-session state can never leak across session
//! boundaries.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use aj_agent::events::{AgentId, AgentSettings};
use aj_agent::message::AgentMessage;
use aj_agent::queue::{MessageQueues, PendingInput, PendingKind, QueueEdit};
use aj_agent::tool::TaskId;
use aj_agent::{
    Agent, BoxError, RecordedTask, SharedAgent, SubAgentBuilder, SubAgentConfig, SubAgentRegistry,
    TaskRegistry, TaskStore,
};
use aj_conf::{AgentEnv, Config};
use aj_models::ThinkingConfig;
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::types::{Speed, StreamOptions};
use aj_session::{
    Committed, EntryId, Journal, PendingStore, PersistenceFailure, ProjectSessions, SessionLog,
    TaskRecord, TaskRecords,
};
use aj_tools::tools::bash::{DetachedProcess, watch_background_bash};
use anyhow::Result;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::host::HeadTarget;
use crate::recovery::{Recovered, RecoveredAgent, RecoveredSubAgent};
use crate::session_setup::{
    BuiltAgent, PreparedLog, RestoreContext, RunConfigSnapshot, SessionSource, build_agent,
    freeze_and_seed, prepare_log, restore_model_settings,
};

/// Terminal window title: `"<app title> - <session id> - <cwd basename>"`,
/// dropping the session-id segment (`"<app title> - <cwd basename>"`) when
/// `session_id` is empty. The `app_title` lets each frontend brand the
/// title with its own name.
pub fn window_title(app_title: &str, session_id: &str, cwd: &std::path::Path) -> String {
    let cwd = cwd
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if session_id.is_empty() {
        format!("{app_title} - {cwd}")
    } else {
        format!("{app_title} - {session_id} - {cwd}")
    }
}

/// How a session comes into being and what the user sees announced
/// when it is installed.
pub enum SessionSpec {
    /// Mint a fresh session log.
    Create {
        entry: SessionEntry,
        /// Complete environment stated by this creation. `None` differs from
        /// an explicitly stated empty map in the on-disk creation entry.
        session_env: Option<BTreeMap<String, String>>,
    },
    /// Resume the identified log from disk.
    Resume {
        session_id: String,
        entry: SessionEntry,
    },
}

/// Whether the session is the process's first or replaces a previous
/// one. Decides the header-notice wording.
pub enum SessionEntry {
    Startup,
    Switch,
}

/// Why a session's driver loop returned.
pub enum SessionExit {
    /// The user quit (Ctrl+C / the quit command, or the terminal
    /// stream ended); the process shuts down.
    Quit,
    /// A resume pick: rebuild onto the identified session.
    Switch(String),
    /// New session: rebuild onto a freshly minted session, on `host` when the
    /// peer serves more than one and the user named one (see
    /// [`SessionRequest::New`]).
    New { host: Option<String> },
    /// Branch the current session: move its head to `target` and, when set,
    /// auto-submit `prompt` as the branch's first turn. `prompt` is `None`
    /// for a tree-view switch, which only moves the head.
    Branch {
        target: HeadTarget,
        prompt: Option<crate::composer::PreparedPrompt>,
    },
}

/// A session change requested by a command or selector. The session's
/// driver loop maps it onto a [`SessionExit`] so the host can tear down
/// the current session and build the next one. Only emitted with no
/// turn in flight.
#[derive(Debug, PartialEq, Eq)]
pub enum SessionRequest {
    /// Mint a session, on `host` when the user named one of the peer's hosts.
    ///
    /// `None` leaves the choice to the peer, which is what an absent host field
    /// on the wire asks for. A peer that will not answer it is asked
    /// the question by the frontend before the request is parked.
    New {
        host: Option<String>,
    },
    Resume(String),
    /// Switch the current session's head to `head`, with no prompt. Parked
    /// by the session-tree overlay's confirm, which names a segment tip
    /// directly rather than a message to branch before.
    Branch {
        head: EntryId,
    },
}

impl SessionRequest {
    pub fn into_exit(self) -> SessionExit {
        match self {
            SessionRequest::New { host } => SessionExit::New { host },
            SessionRequest::Resume(id) => SessionExit::Switch(id),
            SessionRequest::Branch { head } => SessionExit::Branch {
                target: HeadTarget::Entry(head),
                prompt: None,
            },
        }
    }
}

/// Loop-side staged settings for one sub-agent. Each axis is
/// `Some(..)` only if the user changed it for this agent; axes left
/// `None` keep whatever the agent itself holds (its spawn-time
/// inheritance). The `Option<Option<..>>` split on thinking/speed
/// matters: `Some(None)` means "explicitly set to off/standard".
///
/// Entries live in [`SessionCore::sub_overrides`] and are re-applied
/// idempotently at every turn start of the agent they belong to. An
/// entry is the user's standing choice for that agent.
#[derive(Default)]
pub struct SubAgentOverrides {
    /// Full bundle swap from a model-selector confirm: provider handle,
    /// model info, stream options, and the `(provider, id)` key.
    pub bundle: Option<(
        Arc<dyn Provider>,
        Arc<ModelInfo>,
        StreamOptions,
        (String, String),
    )>,
    pub thinking: Option<Option<ThinkingConfig>>,
    pub speed: Option<Option<Speed>>,
}

/// The agent-lifecycle truth an event source keeps as it processes
/// `AgentStart`/`AgentEnd` and compaction events.
///
/// `running_agents` is the single source of truth for what is running:
/// `AgentStart` inserts and `AgentEnd` removes. The host's join-time
/// reap ([`crate::turn::Turns::reap`]) is the second writer: it marks
/// the joined agent idle without an `AgentEnd`, and on a Main
/// completion also clears other agents' leaked entries. The per-view
/// spinner, the footer's running-agent count, and per-box status all
/// derive from it.
///
/// `compacting` is kept separately because compaction is
/// host-orchestrated and does not bracket itself with
/// `AgentStart`/`AgentEnd`. An agent can be compacting without being in
/// `running_agents`, and the spinner treats an agent as busy when it is
/// in either set, which is how it animates during the summarizer call.
#[derive(Debug, Default)]
pub struct AgentLifecycle {
    running_agents: HashSet<AgentId>,
    compacting: HashSet<AgentId>,
}

impl AgentLifecycle {
    /// Whether `id` has an open `AgentStart` with no matching
    /// `AgentEnd`.
    pub fn is_running(&self, id: AgentId) -> bool {
        self.running_agents.contains(&id)
    }

    /// Owned snapshot of every agent currently in the running set.
    /// Order is unspecified. `AgentId` is `Copy`, so this hands back a
    /// plain `Vec` the caller can iterate while mutating the set.
    pub fn running_agents(&self) -> Vec<AgentId> {
        self.running_agents.iter().copied().collect()
    }

    /// Record `id` as running.
    pub fn mark_running(&mut self, id: AgentId) {
        self.running_agents.insert(id);
    }

    /// Remove `id` from the running set. Idempotent.
    pub fn mark_idle(&mut self, id: AgentId) {
        self.running_agents.remove(&id);
    }

    /// Whether `id` has an in-flight host-driven compaction.
    pub fn is_compacting(&self, id: AgentId) -> bool {
        self.compacting.contains(&id)
    }

    /// Owned snapshot of every agent currently marked compacting.
    /// Order is unspecified, and the caller can mutate the set while
    /// iterating the returned `Vec`.
    pub fn compacting_agents(&self) -> Vec<AgentId> {
        self.compacting.iter().copied().collect()
    }

    /// Record `id` as compacting.
    pub fn mark_compacting(&mut self, id: AgentId) {
        self.compacting.insert(id);
    }

    /// Clear `id`'s compacting mark. Idempotent.
    pub fn clear_compacting(&mut self, id: AgentId) {
        self.compacting.remove(&id);
    }
}

/// Post-build facts about the Main agent that the frontend needs after a
/// rebuild: the footer seed (settings and context window), read off the
/// freshly-built agent before it is shared behind a lock.
///
/// The context window is read off the agent's `model_info`, which a
/// synchronous caller can no longer do once the agent lives behind an
/// `Arc<TokioMutex>`. [`SessionCore::build`] captures it here so the
/// frontend can seed its footer without re-locking the agent.
pub struct MainAgentSeed {
    pub settings: AgentSettings,
    pub context_window: u64,
}

/// Everything with session lifetime that a rendering backend does not
/// touch, built fresh on every materialization and never reseeded after
/// construction. Dropping the core drops the agent and its bus
/// subscriptions in one go.
pub struct SessionCore {
    /// The session's agent, freshly constructed for this session.
    /// Shared because a submit handler spawns a task that holds it
    /// across `agent.prompt(...).await`.
    pub agent: Arc<TokioMutex<Agent>>,
    /// The environment the agent was built against: base prompt,
    /// AGENTS.md/CLAUDE.md context files, discovered skills, working
    /// directory. The runtime takes only the assembled prompt, so the
    /// core keeps this for the startup context notice, the footer cwd,
    /// and the editor's autocomplete root.
    pub env: AgentEnv,
    /// The sub-agents this process holds a live agent for: the ones it
    /// spawned and the ones it built from their stored runs (see
    /// [`Self::rebuild_sub_agent`]). Injected into `agent`.
    pub registry: SubAgentRegistry,
    /// What a sub-agent built outside a tool call shares with `agent`.
    sub_agents: SubAgentBuilder,
    /// Background-task registry injected into `agent`; shared with the
    /// driver so wake triggers can poll notices and session teardown can
    /// close the task tree. Frontend session switches do not end this
    /// lifetime: the host retains the core until idle release or shutdown.
    pub task_registry: TaskRegistry,
    /// Shared steering / follow-up queues injected into `agent` (and
    /// its sub-agents). The frontend's input handlers enqueue onto them
    /// and the wake triggers poll [`MessageQueues::has_pending`].
    /// Per-session, like the agent itself.
    pub message_queues: MessageQueues,
    /// Where the session saves `message_queues` and undelivered task
    /// notices.
    pub pending: Arc<PendingStore>,
    /// The session's record of the background tasks that outlive the
    /// process.
    pub tasks: Arc<TaskRecords>,
    /// The stored turn state of the main agent and of each sub-agent.
    pub turn: Arc<aj_session::TurnStore>,
    /// Loop-side staged settings overrides, keyed by sub-agent id. The
    /// model and thinking-effort selectors write entries when the user
    /// changes a sub-agent's settings; the turn primitive re-applies
    /// them at every turn start. Sub ids are per-session, so the map
    /// resets naturally with the core. A sub-agent with no entry runs
    /// with whatever it already holds (spawn-time inheritance).
    pub sub_overrides: Arc<StdMutex<HashMap<usize, SubAgentOverrides>>>,
    /// What the session's next turn runs against. Per session, so a
    /// model change or a resumed session's restored bundle cannot leak
    /// into another live session (see [`RunConfigSnapshot`]).
    pub run_config: Arc<StdMutex<RunConfigSnapshot>>,
    /// The session's on-disk session log, shared with the
    /// journal.
    pub log: Arc<TokioMutex<SessionLog>>,
    /// Convenience copy of the log's session id, readable without
    /// locking `log`.
    pub session_id: String,
    /// Notices produced by resume-time settings restoration (what was
    /// restored, or why a recorded value was kept out). Pumped onto the
    /// chat scrollback by the caller after install.
    pub restore_notices: Vec<String>,
    /// The branch's goal as the open found it, already paused if it was
    /// being pursued.
    pub goal: Option<aj_agent::goal::Goal>,
    /// Whether the session ever recorded a goal.
    pub goal_history: bool,
    /// The agents whose turn the last host left unfinished (see
    /// [`crate::recovery::Recovered::interrupted`]).
    pub interrupted: Vec<AgentId>,
    /// What this open owes the first client it serves, each once: that
    /// its resume repaired an interrupted final write, and one notice for
    /// each sub-agent it could not restore.
    open_notices: StdMutex<Vec<String>>,
    /// The task records the open found running, until
    /// [`Self::event_stream`] starts watching them.
    watch: StdMutex<Vec<(AgentId, TaskRecord)>>,
    /// The one writer of `log` while the session is hosted (see [`Journal`]). A host closes
    /// it before it releases the session lock.
    pub(crate) journal: Journal,
}

/// A sub-agent's stored run, as an agent is built from it.
struct SubAgentRun {
    spawned_by_oracle: bool,
    settings: aj_session::SessionSettings,
    transcript: Vec<AgentMessage>,
    code_mode_store: BTreeMap<String, serde_json::Value>,
}

/// Records a session's background tasks in its database.
struct SessionTaskStore(Arc<TaskRecords>);

impl TaskStore for SessionTaskStore {
    fn minted(&self, id: TaskId) -> Result<(), BoxError> {
        Ok(self.0.reserve(id)?)
    }

    fn record(&self, task: &RecordedTask) -> Result<(), BoxError> {
        // Bash is the one tool whose work is recorded.
        let process: DetachedProcess = serde_json::from_value(task.state.clone())?;
        Ok(self.0.start(&TaskRecord {
            id: task.id,
            owner: task.owner,
            call_id: task.call_id.clone(),
            command: task.label.clone(),
            pid: process.pid,
            process_start: process.start_time,
            output_path: process.output_path,
            exit_path: process.exit_path,
            started_at: chrono::Utc::now(),
        })?)
    }

    fn finish(&self, id: TaskId, owner: AgentId, notice: &AgentMessage) -> Result<(), BoxError> {
        Ok(self.0.finish(id, owner, notice)?)
    }
}

/// Change `agent`'s pending message in `queues`, saving the change to
/// `pending` first. An accepted input is one that is saved, a withdrawn
/// one is gone, and a refused save leaves the queue as it was. Returns the
/// text a withdrawal removed.
///
/// This is how the pending message of an agent that has a thread is
/// changed. A prompt for an idle agent goes through [`accept_prompt`],
/// because the main agent may be on a draft.
pub(crate) fn edit_pending(
    pending: &PendingStore,
    queues: &MessageQueues,
    agent: AgentId,
    edit: QueueEdit<'_>,
) -> Result<Option<String>, aj_session::SessionError> {
    let save = |before: Option<&(PendingKind, PendingInput)>,
                after: Option<&(PendingKind, PendingInput)>| match (before, after) {
        (_, Some((kind, input))) => pending.save_input(agent, *kind, input),
        (Some((_, input)), None) => pending.forget(&input.id),
        (None, None) => Ok(()),
    };
    queues.edit(agent, edit, save)
}

/// Accept a prompt for `agent`: save it as the agent's pending input (see
/// [`edit_pending`]). The caller then wakes the agent if no turn of it is
/// running, and that turn takes the message from the queue. Its append
/// removes the saved copy in the same transaction.
///
/// The main agent's first prompt in a new session or at a fork point finds
/// it on a draft. The session or branch is then created in one transaction
/// with the saved input, so an accepted prompt is stored like any other and
/// the turn that takes it has a thread to work on.
pub(crate) async fn accept_prompt(
    journal: &Journal,
    pending: &PendingStore,
    queues: &MessageQueues,
    agent: AgentId,
    content: &[aj_models::types::UserContent],
) -> Result<(), aj_session::SessionError> {
    let edit = QueueEdit::FollowUp(content);
    if agent == AgentId::Main && !pending.has_thread(agent) {
        // Only the task that accepts prompts creates a thread, so
        // the draft seen under the log lock is the one seen above.
        let mut log = journal.lock().await;
        if log.is_draft() {
            let create = |_: Option<&_>, after: Option<&(PendingKind, PendingInput)>| match after {
                Some((kind, input)) => log.accept(*kind, input),
                None => Ok(()),
            };
            return queues.edit(agent, edit, create).map(|_| ());
        }
    }
    edit_pending(pending, queues, agent, edit).map(|_| ())
}

impl SessionCore {
    /// Build the frontend-agnostic half of a session bound to `spec`.
    ///
    /// Performs the setup that doesn't touch a rendering backend: log
    /// resolve (create/resume), the recovery of what a resumed session's
    /// last host left ([`crate::recovery::recover`]) and its installation
    /// ([`Self::install`]), resume-time
    /// settings restoration (when `restore` is supplied, the log's
    /// recorded model/thinking/speed are written back into the shared
    /// run-config snapshot before the agent is built), agent
    /// construction off the run-config snapshot, transcript /
    /// system-prompt / sub-agent-counter seeding, and the bus
    /// subscriptions. A fresh log additionally gets its initial settings
    /// record so a later resume can restore it.
    ///
    /// Returns the core plus a [`MainAgentSeed`]: the Main agent's
    /// footer settings and context window, read off the owned agent
    /// before it is shared. The frontend uses the seed to build its
    /// footer without re-locking the agent.
    ///
    /// On error nothing is shared or installed; the caller's session
    /// loop falls back to the previous session.
    pub fn build(
        config: &Config,
        run_config: RunConfigSnapshot,
        persistence: &ProjectSessions,
        spec: &SessionSpec,
        restore: Option<&RestoreContext>,
    ) -> Result<(SessionCore, MainAgentSeed)> {
        let source = match spec {
            SessionSpec::Create { session_env, .. } => SessionSource::Create {
                session_env: session_env.clone(),
            },
            SessionSpec::Resume { session_id, .. } => SessionSource::Resume {
                session_id: session_id.clone(),
            },
        };
        // Wrapped up front because the steps below share it: `prepare_log`
        // stamps the session's prompt-cache key onto it, and the turn
        // driver reads it at every turn start.
        let run_config = Arc::new(StdMutex::new(run_config));

        // Resolve and settle the log and, on a resume with restoration
        // enabled, write its recorded settings back into the shared run
        // config before the agent is built off it.
        let PreparedLog {
            mut log,
            mut recovered,
            restore_notices,
            session_env,
        } = prepare_log(persistence, &source, &run_config, restore)?;

        // Build a fresh agent off the run-config snapshot, which at this
        // point reflects both runtime model and thinking-effort choices and
        // any settings just restored from the resumed log.
        let settings = {
            let cfg = run_config.lock().expect("run config mutex poisoned");
            cfg.settings()
        };
        let BuiltAgent {
            mut agent,
            env,
            include_skills,
        } = build_agent(
            config,
            &run_config.lock().expect("run config mutex poisoned"),
        );
        agent.set_session_env(session_env.unwrap_or_default());

        // Freeze the system prompt (fresh log) or reuse the persisted
        // one (resume), then seed the agent's transcript, prompt, and
        // sub-agent counter floor.
        freeze_and_seed(
            &mut log,
            &mut agent,
            &mut recovered.main,
            &env,
            include_skills,
            source.creation_env(),
            &run_config.lock().expect("run config mutex poisoned"),
        )?;
        if matches!(source, SessionSource::Create { .. }) {
            let accounts = run_config
                .lock()
                .expect("run config mutex poisoned")
                .accounts
                .snapshot();
            for (provider, account) in accounts {
                log.append_account_change(&provider, Some(&account))?;
            }
        }

        // Fresh, empty registry. A sub-agent of an earlier process enters
        // it when its stored run is picked up.
        let registry = SubAgentRegistry::default();
        agent.set_sub_agent_registry(registry.clone());

        // Fresh task registry, shared with the main loop's wake
        // triggers; its session-scoped cancellation root is fired when
        // the session winds down.
        let task_registry = TaskRegistry::default();
        agent.set_task_registry(task_registry.clone());

        // Fresh, shared steering / follow-up queues: the frontend's
        // input handlers enqueue onto them while a turn runs, the agent
        // drains them, and the frontend reads them to paint the
        // pending-message box.
        let message_queues = MessageQueues::default();
        agent.set_message_queues(message_queues.clone());

        let pending = Arc::new(log.pending());
        let tasks = Arc::new(log.task_records());
        let turn = Arc::new(log.turn_state());
        task_registry.set_store(Arc::new(SessionTaskStore(Arc::clone(&tasks))));

        let session_id = log.session_id().to_string();
        let system_prompt = log.system_prompt().unwrap_or_default().to_string();

        // Read the Main agent's footer seed off the owned agent before
        // it is shared: a synchronous caller can't read `model_info`
        // through the lock later.
        let seed = MainAgentSeed {
            settings,
            context_window: agent.model_info().context_window,
        };

        let log = Arc::new(TokioMutex::new(log));
        // Seeding emits nothing, so nothing was published uncommitted.
        let journal = Journal::new(Arc::clone(&log));
        agent.set_committer(journal.committer());
        let sub_agents = agent.sub_agent_builder();

        let core = SessionCore {
            agent: Arc::new(TokioMutex::new(agent)),
            env,
            registry,
            sub_agents,
            task_registry,
            message_queues,
            pending,
            tasks,
            turn,
            sub_overrides: Arc::new(StdMutex::new(HashMap::new())),
            run_config,
            log,
            session_id,
            restore_notices,
            goal: recovered.goal.take(),
            goal_history: recovered.goal_history,
            interrupted: std::mem::take(&mut recovered.interrupted),
            open_notices: StdMutex::new(std::mem::take(&mut recovered.notices)),
            watch: StdMutex::new(Vec::new()),
            journal,
        };
        core.install(recovered, &system_prompt, config, restore);
        Ok((core, seed))
    }

    /// Install a settled session in this live one: put back each agent's
    /// pending input and undelivered notices, build the sub-agents that
    /// have some, and keep the task id counter above the ids already
    /// minted. Nothing here writes or can fail, and no turn starts:
    /// restored work is delivered at its agent's next ordinary drain
    /// point, once.
    ///
    /// The commands found running are watched from [`Self::event_stream`]
    /// on, so that their `TaskStart` reaches the stream.
    fn install(
        &self,
        recovered: Recovered,
        system_prompt: &str,
        config: &Config,
        restore: Option<&RestoreContext>,
    ) {
        self.task_registry.seed_last_id(recovered.last_task_id);
        let mut watch = Vec::new();
        self.install_agent(AgentId::Main, recovered.main, &mut watch);
        for (n, sub) in recovered.sub_agents {
            let RecoveredSubAgent {
                mut agent,
                spawned_by_oracle,
            } = sub;
            let run = SubAgentRun {
                spawned_by_oracle,
                settings: std::mem::take(&mut agent.settings),
                transcript: std::mem::take(&mut agent.transcript),
                code_mode_store: std::mem::take(&mut agent.code_mode_store),
            };
            self.build_sub_agent(n, run, system_prompt, config, restore);
            self.install_agent(AgentId::Sub(n), agent, &mut watch);
        }
        *self.watch.lock().expect("watch mutex poisoned") = watch;
    }

    fn install_agent(
        &self,
        agent: AgentId,
        recovered: RecoveredAgent,
        watch: &mut Vec<(AgentId, TaskRecord)>,
    ) {
        for (kind, input) in recovered.inputs {
            self.message_queues.restore(agent, kind, input);
        }
        for notice in recovered.task_notifications {
            self.task_registry.restore_notice(agent, notice);
        }
        watch.extend(recovered.tasks.into_iter().map(|task| (agent, task)));
    }

    /// Take the notices this open owes the first client it serves, each
    /// once: that its resume repaired an interrupted final write, and one
    /// for each sub-agent it could not restore.
    pub fn take_open_notices(&self) -> Vec<String> {
        std::mem::take(&mut *self.open_notices.lock().expect("notices mutex poisoned"))
    }

    /// What the session's journal commits from now on, as a stream in
    /// commit order, plus the one-shot storage-failure signal the owning
    /// driver ends the materialization on.
    ///
    /// Call this once, before the first turn: a write committed earlier is
    /// not in the stream. It also starts watching the task records
    /// the open found running, so the stream carries their `TaskStart`.
    /// Their drivers are spawned on the current Tokio runtime and hold the
    /// session's store, so a caller that then gives the session up shuts
    /// the task registry down and waits for it before it releases the
    /// session's lock.
    pub async fn event_stream(
        &mut self,
    ) -> (
        UnboundedReceiver<Committed>,
        tokio::sync::oneshot::Receiver<PersistenceFailure>,
    ) {
        let (failure_signal, failures) = tokio::sync::oneshot::channel();
        self.log.lock().await.install_failure_signal(failure_signal);
        let stream = self.journal.stream();
        let watch = std::mem::take(&mut *self.watch.lock().expect("watch mutex poisoned"));
        for (owner, task) in watch {
            // Nothing else holds an agent before the first turn.
            if let Some(agent) = self.resolve_agent(owner) {
                let agent = agent.lock().await;
                watch_background_bash(&agent, crate::recovery::saved_bash_task(&task));
            }
        }
        (stream, failures)
    }

    /// Build sub-agent `n` from its stored run on the selected branch and
    /// retain it, so it is prompted like one this process spawned. `None`
    /// when the branch shows no run of that sub-agent.
    ///
    /// See [`Self::build_sub_agent`] for what is and is not rebuilt.
    pub fn rebuild_sub_agent(
        &self,
        log: &mut SessionLog,
        n: usize,
        config: &Config,
        restore: Option<&RestoreContext>,
    ) -> Result<Option<SharedAgent>> {
        let Some(sub) = crate::recovery::read_sub_agent_run(log, n)? else {
            return Ok(None);
        };
        let run = SubAgentRun {
            spawned_by_oracle: sub.spawned_by_oracle,
            settings: sub.agent.settings,
            transcript: sub.agent.transcript,
            code_mode_store: sub.agent.code_mode_store,
        };
        let system_prompt = log.system_prompt().unwrap_or_default().to_string();
        Ok(Some(self.build_sub_agent(
            n,
            run,
            &system_prompt,
            config,
            restore,
        )))
    }

    /// Build sub-agent `n` from `run`, its stored run, and retain it.
    /// Reads nothing from the store and cannot fail.
    ///
    /// The agent continues the run: its transcript is the run's model
    /// context, its settings are the spawn record's with the changes
    /// recorded since, and it shares the session's environment, tasks and
    /// queues. Its tools and prompt suffix are those the tool that spawned
    /// it gives a new sub-agent under `config` now, derived once here. A
    /// recorded model this host cannot resolve falls back to that tool's
    /// current one.
    ///
    /// Not rebuilt: the todo list, and what the spawning agent held in
    /// memory only when it spawned (the `image_block` setting comes from
    /// `config` instead). The parent's tool call no longer waits for the
    /// sub-agent, so it has no foreground assignment.
    fn build_sub_agent(
        &self,
        n: usize,
        run: SubAgentRun,
        system_prompt: &str,
        config: &Config,
        restore: Option<&RestoreContext>,
    ) -> SharedAgent {
        let oracle = run.spawned_by_oracle;
        let (mut model, session_id) = {
            let run_config = self.run_config.lock().expect("run config mutex poisoned");
            let model = if oracle {
                run_config.oracle.clone()
            } else {
                run_config.main.clone()
            };
            (model, run_config.session_id.clone())
        };
        // A scripted host resolves no models, as for the main agent.
        if let Some(restore) = restore {
            for notice in restore_model_settings(&mut model, &run.settings, restore) {
                tracing::info!("sub-agent {n}: {notice}");
            }
            let accounts = {
                let run_config = self.run_config.lock().expect("run config mutex poisoned");
                run_config.accounts.clone()
            };
            let provider = model.model_info.provider.clone();
            accounts.install(&mut model.stream_options, &restore.auth, &provider);
        }
        let bundle = if oracle {
            crate::oracle::child_config(config, &model, session_id)
        } else {
            crate::turn::sub_agent_config(config, &model, session_id)
        };
        let agent = self.sub_agents.build(SubAgentConfig {
            agent_id: n,
            bundle,
            restrict_tools: oracle,
            system_prompt: system_prompt.to_string(),
            disabled_tools: config.disabled_tools.clone(),
            block_images: config.image_block,
            code_mode: config.code_mode,
            transcript: run.transcript,
            code_mode_store: run.code_mode_store,
        });
        let agent: SharedAgent = Arc::new(TokioMutex::new(agent));
        self.registry.insert(n, Arc::clone(&agent));
        agent
    }

    /// Resolve an `AgentId` to its live handle: the main agent for
    /// `Main`, a retained sub-agent for `Sub(n)`. `None` for a sub-agent
    /// this process does not hold (see [`Self::rebuild_sub_agent`]).
    pub fn resolve_agent(&self, id: AgentId) -> Option<SharedAgent> {
        match id {
            AgentId::Main => Some(Arc::clone(&self.agent)),
            AgentId::Sub(n) => self.registry.get(n),
        }
    }

    /// Decompose the core into an owned agent, its shared log and its
    /// journal, for headless turn tests that drive the agent directly.
    ///
    /// The agent is uniquely held after [`Self::build`] (the bus
    /// subscriptions keep handles, not agent clones), so unwrapping it
    /// out of the `Arc<TokioMutex>` cannot fail. The agent keeps
    /// committing to the log through its bus.
    #[cfg(any(test, feature = "test-support"))]
    pub fn into_test_agent(self) -> (Agent, Arc<TokioMutex<SessionLog>>, Journal) {
        let agent = Arc::try_unwrap(self.agent)
            .unwrap_or_else(|_| unreachable!("core.agent is uniquely held after build"))
            .into_inner();
        (agent, self.log, self.journal)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn window_title_includes_session_id() {
        assert_eq!(
            window_title("AJ", "abc123", Path::new("/home/user/project")),
            "AJ - abc123 - project"
        );
    }

    #[test]
    fn window_title_drops_empty_session_id() {
        assert_eq!(
            window_title("AJ", "", Path::new("/home/user/project")),
            "AJ - project"
        );
    }

    #[test]
    fn window_title_uses_cwd_basename() {
        // A nested path yields only its last component.
        assert_eq!(
            window_title("AJ", "s", Path::new("/a/b/c/deep/leaf")),
            "AJ - s - leaf"
        );
    }

    #[test]
    fn window_title_brands_with_the_app_title() {
        // The app-title argument is what leads the title, so each frontend
        // brands it with its own name.
        assert!(
            window_title("aj", "s", Path::new("/proj")).starts_with("aj - "),
            "lowercase app title leads the window title"
        );
        assert!(
            window_title("AJ", "s", Path::new("/proj")).starts_with("AJ - "),
            "uppercase app title leads the window title"
        );
    }
}
