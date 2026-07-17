//! Frontend-agnostic per-session state.
//!
//! A [`SessionCore`] owns everything whose lifetime is one conversation
//! session and that does not depend on a rendering backend: the agent,
//! its sub-agent registry, the conversation log, the bus
//! subscriptions, the staged settings, and the agent-lifecycle sets. A
//! frontend wraps a `SessionCore` in its own view type (the `aj` binary
//! adds an event pump plus the install/reconcile view work). Session
//! changes (`/new`, `/resume`) build a fresh core and replace the old
//! one wholesale instead of mutating shared state back into a pristine
//! shape, so per-session state can never leak across session
//! boundaries.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use aj_agent::bus::SubscriptionHandle;
use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_agent::queue::MessageQueues;
use aj_agent::types::UsageSummary;
use aj_agent::{Agent, SharedAgent, SubAgentRegistry, TaskRegistry};
use aj_conf::{AgentEnv, Config};
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::types::{Speed, StreamOptions};
use aj_models::{ThinkingConfig, speed_name, thinking_config_name, verbosity_name};
use aj_session::{ConversationLog, ConversationPersistence, EntryId, persistence_listener};
use anyhow::Result;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::session_setup::{
    BuiltAgent, PreparedLog, RestoreContext, RunConfigSnapshot, SessionSource, build_agent,
    freeze_and_seed, prepare_log,
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
    /// Mint a fresh conversation log.
    Create { entry: SessionEntry },
    /// Resume the identified log from disk.
    Resume {
        session_id: String,
        entry: SessionEntry,
        /// Optional user-thread head to rebuild on, for branching and
        /// tree-view switching. `None` means "use the log's default head"
        /// (its `latest_leaf`), the plain-resume behavior. `None` never
        /// means "branch to the file root": root branching is refused up
        /// front, so every branch target here is a real entry id.
        head: Option<EntryId>,
    },
}

/// Whether the session is the process's first or replaces a previous
/// one. Decides the header-notice wording.
pub enum SessionEntry {
    Startup,
    Switch,
}

/// Why a session's run loop returned.
pub enum SessionExit {
    /// The user quit (Ctrl+C / the quit command, or the terminal
    /// stream ended); the process shuts down.
    Quit,
    /// A resume pick: rebuild onto the identified session.
    Switch(String),
    /// New session: rebuild onto a freshly minted session.
    New,
    /// Branch the current session at an earlier user message: rebuild the
    /// same session onto `head` (the branched-from message's parent) and,
    /// when set, auto-submit `prompt` as the branch's first turn. `prompt`
    /// is `None` for a tree-view switch, which only moves the head.
    Branch {
        head: EntryId,
        prompt: Option<String>,
    },
}

/// A session change requested by a command or selector. The session's
/// run loop maps it onto a [`SessionExit`] so the host can tear down
/// the current session and build the next one. Only emitted with no
/// turn in flight.
pub enum SessionRequest {
    New,
    Resume(String),
    /// Switch the active branch of the current session: rebuild it onto
    /// `head` with no prompt. Parked by the session-tree overlay's confirm,
    /// this reuses the branch rebuild path (see [`SessionExit::Branch`]).
    Branch {
        head: EntryId,
    },
}

impl SessionRequest {
    pub fn into_exit(self) -> SessionExit {
        match self {
            SessionRequest::New => SessionExit::New,
            SessionRequest::Resume(id) => SessionExit::Switch(id),
            SessionRequest::Branch { head } => SessionExit::Branch { head, prompt: None },
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
#[derive(Default)]
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
/// touch, built fresh on every session change and never reseeded after
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
    /// Sub-agent registry injected into `agent`; starts empty, so only
    /// sub-agents spawned in this session are promptable.
    pub registry: SubAgentRegistry,
    /// Background-task registry injected into `agent`; shared with the
    /// main loop so the wake triggers can poll notices and shutdown can
    /// kill the task tree. Per-session; the loop shuts it down on every
    /// exit (quit, fatal error, session switch), so tasks never outlive
    /// their session.
    pub task_registry: TaskRegistry,
    /// Shared steering / follow-up queues injected into `agent` (and
    /// its sub-agents). The frontend's input handlers enqueue onto them
    /// and the wake triggers poll [`MessageQueues::has_pending`].
    /// Per-session, like the agent itself.
    pub message_queues: MessageQueues,
    /// Loop-side staged settings overrides, keyed by sub-agent id. The
    /// `/model` / `/thinking` selectors write entries when the user
    /// changes a sub-agent's settings; the turn primitive re-applies
    /// them at every turn start. Sub ids are per-session, so the map
    /// resets naturally with the core. A sub-agent with no entry runs
    /// with whatever it already holds (spawn-time inheritance).
    pub sub_overrides: Arc<StdMutex<HashMap<usize, SubAgentOverrides>>>,
    /// The session's on-disk conversation log, shared with the
    /// persistence listener.
    pub log: Arc<TokioMutex<ConversationLog>>,
    /// Convenience copy of the log's session id, readable without
    /// locking `log`.
    pub session_id: String,
    /// Receiver side of the bus->channel forwarder feeding the
    /// frontend.
    pub event_rx: UnboundedReceiver<AgentEvent>,
    /// Agent-lifecycle truth. Whoever processes
    /// `AgentStart`/`AgentEnd` and compaction updates it. The frontend
    /// reads it to drive spinners, counts, and busy checks.
    pub lifecycle: AgentLifecycle,
    /// Notices produced by resume-time settings restoration (what was
    /// restored, or why a recorded value was kept out). Pumped onto the
    /// chat scrollback by the caller after install.
    pub restore_notices: Vec<String>,
    /// Keeps the bus->channel forwarder subscribed; dropped with the
    /// core.
    _event_handle: SubscriptionHandle,
    /// Keeps the persistence listener subscribed; dropped with the
    /// core.
    _persistence_handle: SubscriptionHandle,
}

impl SessionCore {
    /// Build the frontend-agnostic half of a session bound to `spec`.
    ///
    /// Performs the setup that doesn't touch a rendering backend: log
    /// resolve (create/resume), interrupted-tool-use repair, resume-time
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
        run_config: &Arc<StdMutex<RunConfigSnapshot>>,
        persistence: &ConversationPersistence,
        spec: &SessionSpec,
        restore: Option<&RestoreContext>,
    ) -> Result<(SessionCore, MainAgentSeed)> {
        let source = match spec {
            SessionSpec::Create { .. } => SessionSource::Create,
            SessionSpec::Resume {
                session_id, head, ..
            } => SessionSource::Resume {
                session_id: session_id.clone(),
                head: head.clone(),
            },
        };

        // Resolve + repair the log and, on a resume with restoration
        // enabled, write its recorded settings back into the shared run
        // config before the agent is built off it.
        let PreparedLog {
            mut log,
            transcript,
            restore_notices,
        } = prepare_log(persistence, &source, config, run_config, restore)?;

        // Build a fresh agent off the run-config snapshot, which at this
        // point reflects both runtime `/model` / `/thinking` choices and
        // any settings just restored from the resumed log.
        let (provider, model_info, stream_options, thinking, speed, verbosity, model_key) = {
            let cfg = run_config.lock().expect("run config mutex poisoned");
            (
                Arc::clone(&cfg.provider),
                Arc::clone(&cfg.model_info),
                cfg.stream_options.clone(),
                cfg.thinking.clone(),
                cfg.speed,
                cfg.stream_options.verbosity,
                cfg.model_key.clone(),
            )
        };
        let BuiltAgent {
            mut agent,
            env,
            include_skills,
        } = build_agent(
            config,
            provider,
            model_info,
            stream_options,
            thinking.clone(),
            speed,
        );

        // Freeze the system prompt (fresh log) or reuse the persisted
        // one (resume), then seed the agent's transcript, prompt, and
        // sub-agent counter floor.
        freeze_and_seed(
            &mut log,
            &mut agent,
            transcript,
            &env,
            include_skills,
            &model_key,
            thinking.as_ref(),
            speed,
            verbosity,
        )?;

        // Fresh, empty registry: only sub-agents spawned in this session
        // become promptable.
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

        // Bus subscriptions: the channel forwarder feeds the frontend in
        // the main loop; the persistence listener writes events into the
        // log. Seeding never emits bus events, so subscription order
        // relative to it is immaterial.
        let (event_handle, event_rx) = agent.subscribe_channel();
        let session_id = log.session_id().to_string();

        // Read the Main agent's footer seed off the owned agent before
        // it is shared: a synchronous caller can't read `model_info`
        // through the lock later.
        let seed = MainAgentSeed {
            settings: AgentSettings {
                provider: model_key.0.clone(),
                model_id: model_key.1.clone(),
                thinking: thinking_config_name(thinking.as_ref()).to_string(),
                speed: speed_name(speed).to_string(),
                verbosity: verbosity_name(verbosity).to_string(),
            },
            context_window: agent.model_info().context_window,
        };

        let log = Arc::new(TokioMutex::new(log));
        let persistence_handle = agent.subscribe(persistence_listener(Arc::clone(&log)));

        let core = SessionCore {
            agent: Arc::new(TokioMutex::new(agent)),
            env,
            registry,
            task_registry,
            message_queues,
            sub_overrides: Arc::new(StdMutex::new(HashMap::new())),
            log,
            session_id,
            event_rx,
            lifecycle: AgentLifecycle::default(),
            restore_notices,
            _event_handle: event_handle,
            _persistence_handle: persistence_handle,
        };
        Ok((core, seed))
    }

    /// Resolve an `AgentId` to its live handle: the main agent for
    /// `Main`, a retained sub-agent for `Sub(n)` (`None` if no live
    /// handle, e.g. a resumed sub-agent).
    pub fn resolve_agent(&self, id: AgentId) -> Option<SharedAgent> {
        match id {
            AgentId::Main => Some(Arc::clone(&self.agent)),
            AgentId::Sub(n) => self.registry.get(n),
        }
    }

    /// Snapshot this session's accumulated token usage for the shutdown
    /// banner. Locks the agent, so call only while no turn is in flight.
    pub async fn usage_summary(&self) -> UsageSummary {
        let agent = self.agent.lock().await;
        crate::shutdown::build_usage_summary(&agent)
    }

    /// Decompose the core into an owned agent plus its shared log and
    /// the persistence subscription, for headless turn tests that drive
    /// the agent directly.
    ///
    /// The agent is uniquely held after [`Self::build`] (the bus
    /// subscriptions keep handles, not agent clones), so unwrapping it
    /// out of the `Arc<TokioMutex>` cannot fail. The persistence handle
    /// is returned so driven turns keep writing to the log. The event
    /// forwarder is not observed on this path, so its handle drops here.
    #[cfg(any(test, feature = "test-support"))]
    pub fn into_test_agent(self) -> (Agent, Arc<TokioMutex<ConversationLog>>, SubscriptionHandle) {
        let agent = Arc::try_unwrap(self.agent)
            .unwrap_or_else(|_| unreachable!("core.agent is uniquely held after build"))
            .into_inner();
        (agent, self.log, self._persistence_handle)
    }

    /// Whether `id` is currently running.
    pub fn is_running(&self, id: AgentId) -> bool {
        self.lifecycle.is_running(id)
    }

    /// Owned snapshot of every agent currently in the running set.
    pub fn running_agents(&self) -> Vec<AgentId> {
        self.lifecycle.running_agents()
    }

    /// Record `id` as running.
    pub fn mark_running(&mut self, id: AgentId) {
        self.lifecycle.mark_running(id);
    }

    /// Remove `id` from the running set. Idempotent.
    pub fn mark_idle(&mut self, id: AgentId) {
        self.lifecycle.mark_idle(id);
    }

    /// Whether `id` has an in-flight host-driven compaction.
    pub fn is_compacting(&self, id: AgentId) -> bool {
        self.lifecycle.is_compacting(id)
    }

    /// Record `id` as compacting.
    pub fn mark_compacting(&mut self, id: AgentId) {
        self.lifecycle.mark_compacting(id);
    }

    /// Clear `id`'s compacting mark. Idempotent.
    pub fn clear_compacting(&mut self, id: AgentId) {
        self.lifecycle.clear_compacting(id);
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
