//! Append-only session log + read-only inference view.
//!
//! Every opened session lives in SQLite. Resuming a legacy JSONL session
//! imports its file first and leaves the file as it was. `SessionLog`
//! holds, of a reopened session, its selected branch from where the model
//! context or the display tail starts (see [`Window`]). A branch is read
//! by its threads: the selected one, then each one it continues
//! from up to the fork point. Another branch is read when the head moves
//! to it, earlier history a page at a time, and the user's earlier
//! messages when a compaction is planned. The reads themselves live in
//! [`crate::sqlite`].
//!
//! # Drafts
//!
//! A new session and a new branch are drafts until their first input: the
//! store holds nothing of them. The log holds a draft in memory. It is
//! what the creating transaction will write: the entries that precede the
//! first message (the system prompt and settings of a session, what a
//! head switch to an earlier entry produced for a branch), and a session's
//! label and archive marker. Draft entries are filed in memory so reads
//! see them. They take the positions after the stored ones, which the
//! store gives them when it files them.
//!
//! Accepting the first input creates the session or the branch in one
//! transaction with saving the input (see [`SessionLog::accept`]). So
//! does the first entry of work appended there, and a turn that starts
//! there. From then on the main agent has a thread: every append
//! commits, and its pending work, task records and turn state belong
//! to that thread. A draft that is left, by a head switch or by the
//! host ending, is forgotten.
//!
//! A created thread may hold no entry yet. No reader takes it for
//! a branch (see [`crate::threads::Thread::is_empty`]).
//!
//! # Which thread a reopen is on
//!
//! Nothing stores which main thread is selected. A reopen lands on
//! the one that holds pending work, and otherwise on the one last appended
//! to (see [`LogSnapshot::stored`]). A head switch to another
//! thread's last entry writes nothing, so it lasts as long as its
//! host unless something is done there.
//!
//! The store decides which thread each entry belongs to and where
//! it sits in the append order. The log hands an entry over, and files it
//! in memory from the store's answer (see [`crate::threads`]). What the log
//! holds in memory is a cache for reads.
//!
//! A write that finds the database busy has changed nothing. It returns an
//! error [`SessionError::is_busy`] recognizes and leaves the log
//! writable. Any other failed append fuses the log.
//!
//! [`BranchView`] is the read-only linearized projection consumed
//! by the wire layer. It carries the materialized [`AgentMessage`]
//! entries (filtered through a [`ThreadFilter`]) plus a small set of
//! helpers (`last_message`, `messages`, etc.) the binary uses to
//! decide thinking efforts and resume state.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::{fs, path::PathBuf, sync::Arc};

use aj_agent::events::{AgentId, AgentSettings, CompactionReason};
use aj_agent::goal::Goal;
use aj_agent::message::AgentMessage;
use aj_models::types::Message;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::jsonl::TailRepair;
use crate::sqlite::SqliteError;
use crate::tool_details::{compact_message, expand_message};

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// The first persistence failure permanently fused this live log.
    #[error("session log write failed: {0}")]
    WriteFailed(#[from] PersistenceFailure),
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Sqlite(#[from] crate::sqlite::SqliteError),
    #[error("session {0} is in use; close it before migrating")]
    MigrationBusy(String),
    #[error("session migration conflict: {0}")]
    MigrationConflict(String),
    #[error("session log is corrupt: {0}")]
    Corrupt(String),
    #[error("invalid append to session log: {0}")]
    InvalidAppend(String),
    #[error("invalid head: {0}")]
    InvalidHead(String),
    /// The id does not satisfy the store's grammar ([`crate::id`]), so it
    /// could not name a log in this store and is never turned into a path.
    #[error("{0:?} is not a session id")]
    InvalidSessionId(String),
    /// The log's final physical entry is damaged in a way that has no
    /// unique safe repair. Resume left every byte exactly as it found them.
    /// The message is the user-facing refusal; `reason` names the specific
    /// shape for diagnostics.
    #[error(
        "AJ could not safely recover this session automatically ({reason}). \
         The session log was left unchanged."
    )]
    UnsafeTail { reason: String },
    /// The session's journal was closed: its owner let the session go, so
    /// nothing was written.
    #[error("the session is closed")]
    Closed,
}

impl SessionError {
    /// Whether another connection held the session database for longer
    /// than a write waits. Nothing was written and the log is not fused,
    /// so the caller can try again. Every write of a session answers this
    /// the same way: an append, pending work, a task record and the turn
    /// state.
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Sqlite(error) if error.is_busy())
    }

    /// Whether the error says this writer can no longer write its session:
    /// its log is fused. Any other error is about the one thing asked for.
    pub fn ends_the_writer(&self) -> bool {
        matches!(self, Self::WriteFailed(_))
    }

    /// The terminal persistence failure carried by this error, when any.
    pub fn persistence_failure(&self) -> Option<&PersistenceFailure> {
        match self {
            Self::WriteFailed(failure) => Some(failure),
            _ => None,
        }
    }
}

/// The first I/O failure that made one live session log unsafe to write.
///
/// Clones share the original `std::io::Error`, so the listener and driver can
/// report the same failure without reopening or retrying the write.
#[derive(Clone, Debug)]
pub struct PersistenceFailure(Arc<PersistenceFailureInner>);

#[derive(Debug)]
struct PersistenceFailureInner {
    error: std::io::Error,
    durable_log: bool,
}

impl PartialEq for PersistenceFailure {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for PersistenceFailure {}

impl PersistenceFailure {
    fn new(error: std::io::Error, durable_log: bool) -> Self {
        Self(Arc::new(PersistenceFailureInner { error, durable_log }))
    }

    pub fn kind(&self) -> std::io::ErrorKind {
        self.0.error.kind()
    }

    /// Whether the store held the session before the failure, so that a
    /// reopen finds it.
    pub fn can_reopen(&self) -> bool {
        self.0.durable_log
    }
}

impl std::fmt::Display for PersistenceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.error.fmt(f)
    }
}

impl std::error::Error for PersistenceFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0.error)
    }
}

/// The sending half of one hosted session's first-write-failure signal. The
/// log fires it exactly once, from the fuse, so the owning driver learns of a
/// failure whether the writer was a foreground turn or a detached task.
pub type PersistenceFailureSender = tokio::sync::oneshot::Sender<PersistenceFailure>;

/// Why [`SessionLog::commit`] failed.
enum Commit {
    /// The store refused the write and holds nothing of it.
    Refused(SqliteError),
    /// The write is committed and the log could not follow it, so memory
    /// no longer shows the session.
    Unfollowed(SqliteError),
}

#[derive(Debug)]
enum WriteState {
    Writable,
    WriteFailed(PersistenceFailure),
}

/// Why a session environment map cannot be recorded or executed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SessionEnvError {
    #[error("environment key {key:?} is empty")]
    EmptyKey { key: String },
    #[error("environment key {key:?} contains '='")]
    KeyContainsEquals { key: String },
    #[error("environment key {key:?} contains NUL")]
    KeyContainsNul { key: String },
    #[error("environment value for key {key:?} contains NUL")]
    ValueContainsNul { key: String },
}

/// Validate the portable syntax of one complete session environment map.
///
/// Keys compare exactly and case-sensitively. Collisions with inherited or
/// fixed tool environment entries are legal because the layering boundary,
/// rather than validation, decides which value a child receives.
pub fn validate_session_env(env: &BTreeMap<String, String>) -> Result<(), SessionEnvError> {
    for (key, value) in env {
        if key.is_empty() {
            return Err(SessionEnvError::EmptyKey { key: key.clone() });
        }
        if key.contains('=') {
            return Err(SessionEnvError::KeyContainsEquals { key: key.clone() });
        }
        if key.contains('\0') {
            return Err(SessionEnvError::KeyContainsNul { key: key.clone() });
        }
        if value.contains('\0') {
            return Err(SessionEnvError::ValueContainsNul { key: key.clone() });
        }
    }
    Ok(())
}

pub(crate) fn validate_recorded_system_prompt(
    order: &[EntryId],
    entries: &HashMap<EntryId, LogEntry>,
) -> Result<(), SessionError> {
    let prompts: Vec<(usize, &LogEntry)> = order
        .iter()
        .enumerate()
        .filter_map(|(index, id)| {
            let entry = entries.get(id)?;
            matches!(entry.entry, LogEntryKind::SystemPrompt { .. }).then_some((index, entry))
        })
        .collect();
    if prompts.is_empty() {
        return Ok(());
    }
    let [(index, prompt)] = prompts.as_slice() else {
        return Err(SessionError::Corrupt(
            "session log contains more than one system_prompt creation entry".to_string(),
        ));
    };
    let valid = *index == 0
        && prompt.thread == ThreadKind::Meta
        && prompt.agent_id.is_none()
        && prompt.parent_id.is_none();
    if !valid {
        return Err(SessionError::Corrupt(
            "system_prompt must be the single root meta entry".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_recorded_session_env(
    order: &[EntryId],
    entries: &HashMap<EntryId, LogEntry>,
) -> Result<(), SessionError> {
    // Validate every map before resume repairs framing, including branch edits.
    for entry in entries.values() {
        if let LogEntryKind::EnvChange { env } = &entry.entry {
            validate_session_env(env)
                .map_err(|err| SessionError::Corrupt(format!("invalid env_change: {err}")))?;
            if entry.agent_id.is_some() || entry.thread == ThreadKind::Sub {
                return Err(SessionError::Corrupt(
                    "env_change must be user state or legacy meta creation metadata".to_string(),
                ));
            }
        }
    }
    let env_entries: Vec<(usize, &LogEntry)> = order
        .iter()
        .enumerate()
        .filter_map(|(index, id)| {
            let entry = entries.get(id)?;
            (entry.thread == ThreadKind::Meta
                && matches!(entry.entry, LogEntryKind::EnvChange { .. }))
            .then_some((index, entry))
        })
        .collect();
    let Some((index, entry)) = env_entries.first().copied() else {
        return Ok(());
    };
    if env_entries.len() != 1 {
        return Err(SessionError::Corrupt(
            "session log contains more than one env_change creation entry".to_string(),
        ));
    }
    let Some(root_id) = order.first() else {
        return Err(SessionError::Corrupt(
            "env_change exists without a system-prompt root".to_string(),
        ));
    };
    let Some(root) = entries.get(root_id) else {
        return Err(SessionError::Corrupt(
            "env_change root is missing from the entry map".to_string(),
        ));
    };
    let layout_is_valid = index == 1
        && root.thread == ThreadKind::Meta
        && root.agent_id.is_none()
        && root.parent_id.is_none()
        && matches!(root.entry, LogEntryKind::SystemPrompt { .. })
        && entry.thread == ThreadKind::Meta
        && entry.agent_id.is_none()
        && entry.parent_id.as_ref() == Some(root_id);
    if !layout_is_valid {
        return Err(SessionError::Corrupt(
            "env_change must be the single root-parented meta entry immediately after the system prompt"
                .to_string(),
        ));
    }
    let has_session_start = order[index + 1..]
        .iter()
        .filter_map(|id| entries.get(id))
        .any(|entry| entry.entry.is_work());
    if !has_session_start {
        return Err(SessionError::Corrupt(
            "env_change must be followed by an entry that starts the session".to_string(),
        ));
    }
    Ok(())
}

/// A unique identifier for a [LogEntry] within a single
/// [SessionLog]. Parent-child links between entries use this id.
///
/// Ids are only unique within one session and are not meaningful outside
/// of it. They are random, collision-resistant tokens (minted by
/// `SessionLog::mint_id`, or adopted from a message's own id), not a
/// counter. Within one process the mint check rules out duplicates; across
/// two processes appending to the same session a collision is possible but
/// vanishingly unlikely (a 128-bit draw), rather than the certainty a
/// shared counter would produce.
pub type EntryId = String;

/// Position and id of one log entry.
///
/// `seq` is 1-based: the entry's index in the log's append order plus
/// one, so `0` reads as "nothing appended yet" and needs no `Option`.
/// It is the entry's position in the store once the session is
/// stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryRef {
    pub seq: u64,
    pub id: EntryId,
}

/// Which thread within a session log an entry belongs to.
///
/// Entries name the first two `user` and `subagent`, the words the JSONL
/// format has always used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    /// Part of the main thread (root + any branches).
    #[serde(rename = "user")]
    Main,
    /// Part of a sub-agent exchange. Disambiguated by `agent_id`.
    #[serde(rename = "subagent")]
    Sub,
    /// Log-level metadata that is not part of any agent's thread
    /// (e.g. the [LogEntryKind::SystemPrompt] root entry).
    /// `Meta` entries are skipped by [ThreadFilter] walks. A thread
    /// may start under one, and a branch is not followed through one that
    /// itself has a main or sub entry for a parent: no writer produces
    /// that.
    Meta,
}

/// An entry in a session log: one stored row, and one line of an
/// exported `.jsonl` file.
///
/// The framing fields (`id`, `parent_id`, `thread`, `agent_id`) live at the
/// top level of the serialized line alongside the payload, thanks to
/// `#[serde(flatten)]` on `entry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Unique within the file. A random, collision-resistant token, not
    /// an ordered counter: append order is tracked separately (by
    /// `SessionLog`'s `order`), so ids need not sort.
    pub id: EntryId,

    /// The immediate predecessor in this entry's thread. `None` only for
    /// the very first entry of the file (the user root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<EntryId>,

    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,

    /// Which thread this entry belongs to.
    pub thread: ThreadKind,

    /// Present only when `thread == Sub`. Scopes the subagent
    /// subtree within the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<usize>,

    /// The payload. Continues to use `#[serde(tag = "type")]` so its
    /// `type` discriminator sits at the top level of the line.
    #[serde(flatten)]
    pub entry: LogEntryKind,
}

/// The different types of log entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LogEntryKind {
    /// A wire-level message (user / assistant / tool_result), wrapped
    /// in [`AgentMessage`]. The message is nested under a `message`
    /// key (rather than flattened) so its own `timestamp` field
    /// doesn't collide with the framing `timestamp` on
    /// [`LogEntry`].
    Message { message: AgentMessage },
    /// The fully-assembled system prompt for this thread, frozen at
    /// thread creation time. Persisted as a [ThreadKind::Meta] root
    /// entry so resuming the thread later (potentially across UTC date
    /// rollovers, working-directory changes, or context-file edits)
    /// reuses the exact prompt the model already cached, instead of
    /// re-deriving a slightly different one and busting the prompt
    /// cache.
    SystemPrompt { text: String },
    /// The active model changed (or was first recorded). `provider`
    /// and `model_id` key into the model catalog.
    ModelChange {
        provider: String,
        model_id: String,
        /// Capacity of the resolved bundle at the change. Zero is unknown.
        #[serde(default)]
        context_window: u64,
    },
    /// Oracle model choice on the user branch, keyed into the model catalog.
    OracleModelChange { provider: String, model_id: String },
    /// Oracle thinking effort, with the same values as [`Self::ThinkingChange`].
    OracleThinkingChange { level: String },
    /// Oracle speed, with the same values as [`Self::SpeedChange`].
    OracleSpeedChange { speed: String },
    /// Oracle output verbosity, with the same values as [`Self::VerbosityChange`].
    OracleVerbosityChange { verbosity: String },
    /// A session-wide account choice on the user branch. `None` removes the
    /// provider's pin, `Some("")` pins its unnamed account, and any other label
    /// pins that exact account. Sub-agent snapshots do not store account choices.
    AccountChange {
        provider: String,
        account: Option<String>,
    },
    /// The active thinking effort changed (or was first recorded).
    /// `level` is one of "off", "minimal", "low", "medium", "high",
    /// "xhigh", "max". Stored as a string so the on-disk
    /// format stays stable if the effort enum evolves; unknown values
    /// are tolerated on restore.
    ThinkingChange { level: String },
    /// The active speed changed (or was first recorded). `speed` is
    /// "standard" or "fast". Stored as a string so the on-disk format
    /// stays stable; unknown values are tolerated on restore.
    SpeedChange { speed: String },
    /// The active output verbosity changed (or was first recorded).
    /// `verbosity` is "default" (server default), "low", "medium", or
    /// "high". Stored as a string so the on-disk format stays stable;
    /// unknown values are tolerated on restore. Verbosity changes the
    /// produced answer, so it's tracked here alongside model/thinking/
    /// speed. `thinking_display` also affects inference, but remains a
    /// live-only session setting and is deliberately not recorded.
    VerbosityChange { verbosity: String },
    /// A complete environment replacement on the user branch.
    ///
    /// An empty map is explicit, distinct from no entry. Legacy root-parented
    /// Meta entries provide a session baseline. Values remain on disk for
    /// restore but are never included in replay notices.
    EnvChange { env: BTreeMap<String, String> },
    /// A complete goal snapshot on the user branch. `None` explicitly clears it.
    GoalChange { goal: Option<Goal> },
    /// Committed code-mode writes on this agent's thread. Keys overwrite earlier
    /// values on the selected ancestry, independently of transcript compaction.
    CodeModeStore {
        writes: BTreeMap<String, serde_json::Value>,
    },
    /// The context the session was created with, as the user sees it: the base
    /// system prompt, the instruction files stitched into it, and the skills
    /// discovered for it. Written once as root-parented [`ThreadKind::Meta`]
    /// metadata beside the frozen [`Self::SystemPrompt`], so the listing and
    /// the prompt it describes stay in step for the session's life however the
    /// files on disk change. Replay renders it as the session's opening notice.
    Context { context: SessionContext },
    /// The structural root of a sub-agent thread, written when the
    /// sub-agent is spawned and anchored at the parent thread's head
    /// (the assistant message carrying the spawning tool call). It
    /// carries the task, the child's run mode, and its settings snapshot,
    /// so the log is self-describing about what each sub-agent ran with and
    /// replay can synthesize the spawn event without look-ahead.
    SubAgentSpawn {
        task: String,
        /// Originating tool name for display identity. Empty is unspecified.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tool_name: String,
        /// The id of the parent's tool call that spawned the sub-agent.
        /// An open finds the sub-agent an unanswered call waited for by
        /// it. `None` in an entry that predates the field, whose call is
        /// then tied to no sub-agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        /// Whether the sub was spawned to run in the background, concurrent
        /// with the parent's turn, rather than blocking it. `#[serde(default)]`
        /// so logs written before mode tracking still deserialize (missing ->
        /// `false`, i.e. foreground).
        #[serde(default)]
        background: bool,
        settings: AgentSettings,
    },
    /// How a sub-agent's run ended: the last entry the run itself writes
    /// on its thread. `report` is what the parent was handed, the run's
    /// final text or the failure. A later prompt to the same sub-agent
    /// continues the thread and writes no second one. A run its process
    /// abandoned gets one from the next process to open the session,
    /// saying it was interrupted. Runs recorded before this existed, and
    /// runs dropped with their parent's cancelled turn, have none.
    SubAgentEnd {
        conclusion: aj_agent::events::SubAgentConclusion,
        report: String,
    },
    /// A durable transcript error, not a checkpoint. It changes neither model
    /// context nor compaction accounting and does not describe a current hold.
    CompactionFailed {
        reason: CompactionReason,
        tokens_before: u64,
        error: String,
    },
    /// A compaction checkpoint: the thread's history before
    /// `first_kept_entry_id` was summarized into `summary`. Projection
    /// ([`BranchView::agent_messages`] / [`BranchView::messages`])
    /// replaces that prefix with selected original user messages and a
    /// summary, and keeps everything from `first_kept_entry_id` onward
    /// verbatim. All original entries stay on disk. Compaction changes
    /// only the projection, never deletes lines.
    Compaction {
        /// LLM-generated structured summary that stands in for the
        /// summarized prefix.
        summary: String,
        /// First entry in the retained suffix. Earlier history is represented
        /// by `summary` and the selected original user messages.
        first_kept_entry_id: EntryId,
        /// Original user messages retained before the summary, in addition to
        /// the tail. IDs must identify distinct user messages on this ancestry
        /// strictly before `first_kept_entry_id`. An absent field retains none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        retained_user_entry_ids: Vec<EntryId>,
        /// Estimated context tokens before this compaction ran. Carried
        /// for the UI ("freed ~N tokens") and telemetry; not used by
        /// projection.
        tokens_before: u64,
        /// Files read / modified in the summarized range, surfaced so
        /// the model knows what was touched without parsing the prose.
        /// `None` when extraction found nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<crate::compaction::CompactionDetails>,
        /// What the summarizer itself cost, priced by the provider that
        /// produced it, summed over the one or two calls a compaction
        /// makes. This is out-of-band spend: the summarizer exchange is
        /// never a message entry, so the log has no other entry of it,
        /// and a session's totals would omit it entirely.
        #[serde(default)]
        usage: aj_models::types::Usage,
    },
}

impl LogEntryKind {
    /// The usage a completed model response reported, which is what a
    /// projection of the log accounts. An attempt that failed or was
    /// cancelled persists the partial captured at that moment and reports
    /// nothing, and audit calls are not inferences.
    #[cfg(test)]
    pub(crate) fn reported_usage(&self) -> Option<&aj_models::types::Usage> {
        use aj_agent::message::AgentMessageKind;
        match self {
            Self::Message { message } => match &message.kind {
                AgentMessageKind::Wire(Message::Assistant(assistant))
                    if assistant.stop_reason.completed() =>
                {
                    Some(&assistant.usage)
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// What kind of message this is, as the store files it: `None` for a
    /// state entry.
    pub(crate) fn message_type(&self) -> Option<&'static str> {
        use aj_agent::message::AgentMessageKind;
        let Self::Message { message } = self else {
            return None;
        };
        Some(match &message.kind {
            AgentMessageKind::Wire(Message::User(_)) => "user",
            AgentMessageKind::Wire(Message::Assistant(_)) => "assistant",
            AgentMessageKind::Wire(Message::ToolResult(_)) => "tool_result",
            AgentMessageKind::ToolActivity(_) => "tool_activity",
            AgentMessageKind::TaskNotification(_) => "task_notification",
            AgentMessageKind::InternalContext(_) => "internal_context",
        })
    }

    pub(crate) fn is_message(&self) -> bool {
        matches!(self, Self::Message { .. })
    }

    /// Whether this kind is work on a thread: a message, a goal, a
    /// compaction, a Code Mode write. Framing and settings (the system
    /// prompt, state entries) are not.
    ///
    /// Work appended at a draft creates the session or branch with it (see
    /// the [module docs](self)). Framing and settings wait in the draft,
    /// so a session the user opens and abandons before submitting anything
    /// stores nothing.
    pub fn is_work(&self) -> bool {
        match self {
            Self::Message { .. }
            | Self::Compaction { .. }
            | Self::CompactionFailed { .. }
            | Self::CodeModeStore { .. }
            | Self::SubAgentEnd { .. }
            | Self::GoalChange { .. } => true,
            Self::SystemPrompt { .. }
            | Self::ModelChange { .. }
            | Self::OracleModelChange { .. }
            | Self::OracleThinkingChange { .. }
            | Self::OracleSpeedChange { .. }
            | Self::OracleVerbosityChange { .. }
            | Self::AccountChange { .. }
            | Self::ThinkingChange { .. }
            | Self::SpeedChange { .. }
            | Self::VerbosityChange { .. }
            | Self::EnvChange { .. }
            | Self::Context { .. }
            | Self::SubAgentSpawn { .. } => false,
        }
    }
}

/// What a session's system prompt was assembled from, recorded for the user
/// rather than the model: the model's copy is the frozen prompt text itself.
///
/// Paths are display strings as the host shows them (home abbreviated to `~`),
/// since they name files on the host that created the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionContext {
    /// The override file the base prompt was read from, or `None` for the
    /// builtin prompt.
    pub system_prompt: Option<String>,
    /// Instruction files stitched into the prompt, in the order they were.
    pub files: Vec<ContextFileRecord>,
    /// Skills discovered for the session, listed or not.
    pub skills: Vec<ContextSkillRecord>,
}

/// One instruction file in a [`SessionContext`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFileRecord {
    pub path: String,
    /// The file's kind as shown to the user (`project instructions`, ...).
    pub kind: String,
}

/// One discovered skill in a [`SessionContext`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSkillRecord {
    pub path: String,
    pub name: String,
    /// `false` when the user's `disabled_skills` config excluded it.
    pub enabled: bool,
    /// `false` when the skill's own frontmatter kept it out of the model's
    /// listing.
    pub model_invocation: bool,
}

impl SessionContext {
    /// The `Context:` notice: a header and one `  - <path> (<label>)` row per
    /// prompt source, file and skill. A skill excluded from the model's listing
    /// says why, and a skill the user disabled is struck (SGR 9), which the
    /// transcript renders as a struck span and a plain terminal shows as is.
    pub fn notice(&self) -> String {
        let mut lines = vec!["Context:".to_string()];
        lines.push(match &self.system_prompt {
            Some(path) => format!("  - {path} (system prompt)"),
            None => {
                "  - builtin (system prompt; override with ~/.agents/SYSTEM_PROMPT.md)".to_string()
            }
        });
        for file in &self.files {
            lines.push(format!("  - {} ({})", file.path, file.kind));
        }
        for skill in &self.skills {
            let marker = if !skill.enabled {
                ", disabled"
            } else if !skill.model_invocation {
                ", model-invocation disabled"
            } else {
                ""
            };
            let row = format!("{} (skill: {}{marker})", skill.path, skill.name);
            if skill.enabled {
                lines.push(format!("  - {row}"));
            } else {
                lines.push(format!("  - \x1b[9m{row}\x1b[29m"));
            }
        }
        lines.join("\n")
    }
}

/// A filter specifying which entries of a [SessionLog] to walk over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadFilter {
    pub thread: ThreadKind,
    /// Required when `thread == Sub`, ignored when `thread == Main`.
    pub agent_id: Option<usize>,
}

impl ThreadFilter {
    pub const MAIN: Self = Self {
        thread: ThreadKind::Main,
        agent_id: None,
    };

    pub fn sub(agent_id: usize) -> Self {
        Self {
            thread: ThreadKind::Sub,
            agent_id: Some(agent_id),
        }
    }

    /// The filter selecting `agent`'s own entries.
    pub fn of(agent: aj_agent::events::AgentId) -> Self {
        match agent {
            aj_agent::events::AgentId::Main => Self::MAIN,
            aj_agent::events::AgentId::Sub(n) => Self::sub(n),
        }
    }

    fn matches(&self, entry: &LogEntry) -> bool {
        match self.thread {
            ThreadKind::Main => matches!(entry.thread, ThreadKind::Main),
            ThreadKind::Sub => {
                matches!(entry.thread, ThreadKind::Sub) && entry.agent_id == self.agent_id
            }
            // `Meta` is never selected by a filter: meta entries are
            // structural (parent-chain anchors) and don't represent any
            // user-facing thread. Constructing a `ThreadFilter` with
            // `thread: Meta` would be a misuse.
            ThreadKind::Meta => false,
        }
    }
}

/// Session settings recorded on one linearized path, extracted by
/// [`BranchView::settings`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSettings {
    /// Last (provider, model_id) recorded on this path. Model changes,
    /// assistant messages, and sub-agent spawn snapshots each replace it
    /// in path order.
    pub model: Option<(String, String)>,
    /// Last recorded Oracle (provider, model_id). Absence means nothing recorded.
    pub oracle_model: Option<(String, String)>,
    /// Last recorded Oracle effort. Absence is distinct from explicit "off".
    pub oracle_thinking: Option<String>,
    /// Last recorded Oracle speed. Absence means nothing recorded.
    pub oracle_speed: Option<String>,
    /// Last recorded Oracle verbosity. "default" pins the server default.
    pub oracle_verbosity: Option<String>,
    /// Session-wide provider account pins folded on the user path. An absent
    /// provider uses its default, an empty label pins its unnamed account, and
    /// other labels are exact pins. Logs without account entries yield no pins.
    pub accounts: BTreeMap<String, String>,
    /// Last recorded thinking level string, from the most recent
    /// [`LogEntryKind::ThinkingChange`] entry. `None` means
    /// "nothing recorded" (inherit the current default) — distinct
    /// from `Some("off")`.
    pub thinking: Option<String>,
    /// Last recorded speed string, from the most recent
    /// [`LogEntryKind::SpeedChange`] entry. `None` means
    /// "nothing recorded".
    pub speed: Option<String>,
    /// Last recorded verbosity string, from the most recent
    /// [`LogEntryKind::VerbosityChange`] entry. `None` means
    /// "nothing recorded" (inherit the current default) — distinct
    /// from `Some("default")`, which pins the server default.
    pub verbosity: Option<String>,
}

impl SessionSettings {
    /// Project only recorded Oracle choices onto the ordinary settings axes.
    pub fn oracle(&self) -> Self {
        Self {
            model: self.oracle_model.clone(),
            thinking: self.oracle_thinking.clone(),
            speed: self.speed.clone(),
            verbosity: self.oracle_verbosity.clone(),
            ..Default::default()
        }
    }

    /// Fold one recorded entry in path order, without resolving runtime defaults.
    /// The caller selects the thread and ancestry before applying entries.
    pub(crate) fn apply(&mut self, entry: &LogEntryKind) {
        match entry {
            LogEntryKind::ModelChange {
                provider, model_id, ..
            } => {
                self.model = Some((provider.clone(), model_id.clone()));
            }
            LogEntryKind::OracleModelChange { provider, model_id } => {
                self.oracle_model = Some((provider.clone(), model_id.clone()));
            }
            LogEntryKind::OracleThinkingChange { level } => {
                self.oracle_thinking = Some(level.clone());
            }
            LogEntryKind::OracleSpeedChange { speed } => {
                self.oracle_speed = Some(speed.clone());
            }
            LogEntryKind::OracleVerbosityChange { verbosity } => {
                self.oracle_verbosity = Some(verbosity.clone());
            }
            LogEntryKind::AccountChange { provider, account } => match account {
                Some(label) => {
                    self.accounts.insert(provider.clone(), label.clone());
                }
                None => {
                    self.accounts.remove(provider);
                }
            },
            LogEntryKind::ThinkingChange { level } => {
                self.thinking = Some(level.clone());
            }
            LogEntryKind::SpeedChange { speed } => {
                self.speed = Some(speed.clone());
            }
            LogEntryKind::VerbosityChange { verbosity } => {
                self.verbosity = Some(verbosity.clone());
            }
            LogEntryKind::EnvChange { .. }
            | LogEntryKind::CodeModeStore { .. }
            | LogEntryKind::GoalChange { .. }
            | LogEntryKind::SubAgentEnd { .. }
            | LogEntryKind::Context { .. } => {}
            LogEntryKind::SubAgentSpawn { settings: snap, .. } => {
                self.model = Some((snap.provider.clone(), snap.model_id.clone()));
                self.thinking = Some(snap.thinking.clone());
                self.speed = Some(snap.speed.clone());
                self.verbosity = Some(snap.verbosity.clone());
            }
            LogEntryKind::Message { message } => {
                if let Some(Message::Assistant(a)) = message.as_stored_wire() {
                    self.model = Some((a.provider.clone(), a.model.clone()));
                }
            }
            LogEntryKind::SystemPrompt { .. } => {}
            // Compaction does not change settings: it keeps the
            // retained tail's last assistant model plus any
            // pre-boundary state entries.
            LogEntryKind::Compaction { .. } | LogEntryKind::CompactionFailed { .. } => {}
        }
    }
}

/// A linearized, read-only view of (a slice of) a session log. Produced
/// by [SessionLog::linearize] and passed to the model for inference.
///
/// The view carries both the underlying [`LogEntry`] sequence
/// (for callers that need entry-level provenance, e.g. resume-time
/// repair walks and history rendering) and a pre-extracted
/// [`Message`] projection for the wire layer, which only cares
/// about messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchView {
    session_id: String,
    entries: Vec<LogEntry>,
}

impl BranchView {
    /// Construct a read-only view from a session id and a linear list
    /// of entries. Crate-internal: external callers obtain a `BranchView`
    /// from [`SessionLog::linearize`], never by hand.
    pub(crate) fn from_entries(session_id: String, entries: Vec<LogEntry>) -> Self {
        Self {
            session_id,
            entries,
        }
    }

    /// Get all entries in this linearized view.
    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }

    /// Fold committed writes from the full selected thread ancestry. Compaction
    /// changes model context, not the lifetime of stored values.
    pub fn code_mode_store(&self) -> BTreeMap<String, serde_json::Value> {
        let mut store = BTreeMap::new();
        for entry in &self.entries {
            if let LogEntryKind::CodeModeStore { writes } = &entry.entry {
                store.extend(writes.clone());
            }
        }
        store
    }

    /// The last compaction of this view: the index of the first entry it
    /// kept, its summary and the earlier user messages it kept.
    fn kept(&self) -> Option<(usize, &String, &Vec<EntryId>)> {
        let entries = self.entries.iter().enumerate().rev();
        let (c, summary, first_kept, retained) =
            entries
                .into_iter()
                .find_map(|(c, entry)| match &entry.entry {
                    LogEntryKind::Compaction {
                        summary,
                        first_kept_entry_id,
                        retained_user_entry_ids,
                        ..
                    } => Some((c, summary, first_kept_entry_id, retained_user_entry_ids)),
                    _ => None,
                })?;
        // `first_kept` should be on this linearized chain; if it is
        // missing (a corrupt or hand-edited log) fall back to the
        // compaction marker's own index so we drop nothing extra.
        let first = self
            .entries
            .iter()
            .position(|entry| &entry.id == first_kept);
        let k = first.unwrap_or_else(|| {
            tracing::warn!(
                "compaction first_kept_entry_id {first_kept} missing from linearized view; \
                 projecting from the compaction marker so nothing extra is dropped"
            );
            c
        });
        Some((k, summary, retained))
    }

    /// The entries the model context is read from: those from the first
    /// one the last compaction kept, or all of them. What precedes them is
    /// summarized, and of a log that does not hold its whole session only
    /// some of it is here.
    pub fn context_entries(&self) -> &[LogEntry] {
        match self.kept() {
            Some((k, _, _)) => &self.entries[k..],
            None => &self.entries,
        }
    }

    /// Get the number of message entries only (excluding system prompt).
    pub fn message_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.entry.is_message())
            .count()
    }

    /// Borrow every wire-level message in this view, in chronological
    /// order. Honors the latest compaction (see
    /// [`Self::projected_agent_messages`]): the summarized prefix is
    /// replaced by selected users and one synthetic summary message.
    /// Non-message entries (system prompt, settings) are skipped. The wire
    /// layer only cares about the messages.
    pub fn messages(&self) -> Vec<Message> {
        self.projected_agent_messages()
            .iter()
            .filter_map(|m| m.to_projected_wire())
            .collect()
    }

    /// Borrow every [`AgentMessage`] in this view, in chronological
    /// order. The transcript-shaped projection used to seed the agent
    /// on resume. Honors the latest compaction (see
    /// [`Self::projected_agent_messages`]).
    pub fn agent_messages(&self) -> Vec<AgentMessage> {
        self.projected_agent_messages()
    }

    /// Project entries to the agent transcript, honoring the latest
    /// compaction: everything before its `first_kept_entry_id` is
    /// replaced by historical user messages and a synthetic summary message.
    ///
    /// The last compaction wins — its summary already folds in any
    /// earlier compaction and its `first_kept_entry_id` points past the
    /// earlier boundary. Its frozen user selection, summary, and retained
    /// tail reconstruct the full reduced context.
    fn projected_agent_messages(&self) -> Vec<AgentMessage> {
        let message = |entry: &LogEntry| match &entry.entry {
            LogEntryKind::Message { message } => Some(expand_message(message.clone())),
            _ => None,
        };
        let Some((k, summary, retained_users)) = self.kept() else {
            return self.entries.iter().filter_map(message).collect();
        };

        let mut out: Vec<AgentMessage> = Vec::new();
        let users = retained_user_messages(&self.entries[..k], retained_users);
        if !users.is_empty() {
            out.push(AgentMessage::wire(Message::User(
                aj_models::types::UserMessage::text(crate::compaction::RETAINED_USER_CONTEXT),
            )));
            out.extend(
                users
                    .into_iter()
                    .map(|message| expand_message(message.clone())),
            );
        }
        out.push(crate::compaction::summary_message(summary));
        out.extend(self.entries[k..].iter().filter_map(message));
        out
    }

    /// Extract the session settings recorded on this path. One
    /// forward scan over [`Self::entries`], keeping the last value
    /// seen per axis. `ModelChange` entries and assistant-role
    /// messages both update the model; a `SubAgentSpawn` snapshot updates the
    /// inference axes. The complete branch environment is read separately via
    /// [`LogSnapshot::session_env`], without compaction filtering.
    pub fn settings(&self) -> SessionSettings {
        let mut settings = SessionSettings::default();
        for entry in &self.entries {
            settings.apply(&entry.entry);
        }
        settings
    }

    /// Get the last message in the view, if any.
    ///
    /// Returns the provider-facing projection, so a task-notification tail
    /// yields its projected user message rather than being skipped.
    pub fn last_message(&self) -> Option<Message> {
        self.entries.iter().rev().find_map(|entry| {
            let LogEntryKind::Message { message } = &entry.entry else {
                return None;
            };
            expand_message(message.clone()).to_projected_wire()
        })
    }
}

// Resolve only within the supplied prefix, in original chronology. Invalid
// references in a damaged checkpoint cannot import another branch, promote a
// task notification to a user request, or duplicate a message already in the tail.
fn retained_user_messages<'a>(entries: &'a [LogEntry], ids: &[EntryId]) -> Vec<&'a AgentMessage> {
    let selected: BTreeSet<_> = ids.iter().collect();
    entries
        .iter()
        .filter_map(|entry| match &entry.entry {
            LogEntryKind::Message { message }
                if selected.contains(&entry.id)
                    && matches!(message.as_stored_wire(), Some(Message::User(_))) =>
            {
                Some(message)
            }
            _ => None,
        })
        .collect()
}

/// A cloneable, read-only image of a log's entry tree.
///
/// Every read-side query lives here, and [`SessionLog`] delegates to
/// the copy it owns. Taking a snapshot under the log lock and answering
/// an expensive read from it (a full projection, say) is what keeps that
/// read from stalling the session's next append.
///
/// Entries are immutable once published and shared between snapshots. Each
/// snapshot owns its index, append order and head, so appends and branch changes
/// cannot change an existing view. Cloning copies metadata, not log payloads.
#[derive(Debug, Clone)]
pub struct LogSnapshot {
    session_id: String,
    /// The entries this log holds, by position ([`EntryRef::seq`]),
    /// each with the thread it is filed under. A log that only ever
    /// appended holds every entry. A resumed log holds what its window names
    /// (see [`Window`]).
    held: BTreeMap<u64, (usize, Arc<LogEntry>)>,
    /// Position of each held entry.
    positions: HashMap<EntryId, u64>,
    /// Position of the session's last entry, held or not.
    count: u64,
    /// The main-thread entry the next main-thread append anchors at.
    ///
    /// This is the explicit head that replaces the implicit
    /// "most recently appended user entry" convention. Every
    /// main-thread [`SessionLog::append`] advances it (messages,
    /// state entries, compaction, repair); sub-agent and meta appends
    /// leave it untouched. [`SessionLog::set_head`] moves it to an
    /// earlier entry to start a sibling branch. `None` only while the
    /// main thread is empty (no main-thread entry yet); the next append
    /// then anchors at the system-prompt meta entry when one exists, or
    /// becomes the root. It is not stored: `resume` derives it (see
    /// [`Self::stored`]). It may be a draft entry, and the entry an empty
    /// thread continues from.
    head: Option<EntryId>,
    /// Every thread of the session, whether its entries are held or not.
    /// A branch is read by its thread and the threads it continues from.
    threads: crate::threads::Threads,
    window: Window,
}

/// What a log that does not hold its whole session holds, and what stands
/// in for the rest.
///
/// It holds one branch: the one that ends at `anchor`, read by its
/// threads (see [`BranchPlan::read`]). Of that branch it holds
///
/// - every entry from `from` on, with the sub-agent runs that reach past
///   `from` whole
/// - every state entry before `from`: settings, goals, the environment
///   and compactions are read from them, and they are few
/// - the main agent's last response before `from`, which names the model
///   in use there
/// - the user's messages that the compactions from `from` on kept. The
///   last one's are part of the model context.
///
/// and the meta entries, which belong to every branch. `from` is at or
/// before both the start of the branch's model context and the start of
/// its display tail.
///
/// A projection of the branch starts at `from` in the state a projection of
/// the whole session would be in there. `usage` carries what the completed
/// responses of the main agent before `from` that are not held reported.
/// A sub-agent run that ended before `from` is not held and not shown.
///
/// A log that holds everything has `from == 1` and nothing carried.
#[derive(Debug, Clone)]
struct Window {
    from: u64,
    usage: aj_models::types::Usage,
    /// The head this window was read for.
    anchor: Option<EntryId>,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            from: 1,
            usage: Default::default(),
            anchor: None,
        }
    }
}

/// What to read from the store to hold the branch ending at `head`.
#[derive(Clone)]
struct BranchPlan {
    session_id: String,
    threads: crate::threads::Threads,
    /// The head's id, thread and position.
    head: (EntryId, usize, u64),
    /// The log's last position when the plan was made.
    count: u64,
}

/// A pending read of a branch a partial log does not hold (see
/// [`SessionLog::branch_read`]).
pub struct BranchRead {
    database: PathBuf,
    project_id: String,
    plan: BranchPlan,
}

/// What a [`BranchRead`] returned, for [`SessionLog::install_branch`].
pub struct LoadedBranch {
    plan: BranchPlan,
    window: Window,
    /// The entries to hold, each with its position and thread.
    entries: BTreeMap<u64, (usize, LogEntry)>,
    /// Which messages of which threads `entries` holds.
    held: Vec<(usize, (u64, u64))>,
}

impl BranchRead {
    /// Run the read. Blocking database I/O on a connection of its own.
    pub fn read(self) -> Result<LoadedBranch, SessionError> {
        let store =
            crate::sqlite::SqliteStore::open_project_read_only(&self.database, &self.project_id)?;
        Ok(self.plan.read(&store)?)
    }
}

impl BranchPlan {
    /// Read the branch by its threads: the head's up to the head,
    /// then each one it continues from up to the fork point. Every read is
    /// a range of one thread, and nothing of a sibling branch or of
    /// a sub-agent run the window does not reach is passed over.
    fn read(self, store: &crate::sqlite::SqliteStore) -> Result<LoadedBranch, SqliteError> {
        use crate::threads::Owner;
        let session = &self.session_id;
        let threads = &self.threads;
        let head = (self.head.1, self.head.2);
        let path = threads.path(Some(head));
        let ancestry = threads.ancestry(Some(head));
        let end = self.count.saturating_add(1);
        let mut entries = branch_state(store, threads, &ancestry, self.count)?;

        // Where the model context starts: at the first entry the branch's
        // last compaction kept.
        let compaction = entries
            .values()
            .rev()
            .find_map(|(_, entry)| match &entry.entry {
                LogEntryKind::Compaction {
                    first_kept_entry_id,
                    ..
                } if entry.thread == ThreadKind::Main => Some(first_kept_entry_id.clone()),
                _ => None,
            });
        let mut context_start = 1;
        if let Some(first_kept) = compaction
            && let Some(at) = store.locate(session, &first_kept)?
            && let Some(thread) = threads.thread_of(at.thread)
            && on_branch(&ancestry, thread, at.seq)
        {
            context_start = at.seq;
        }
        let tail_start =
            stored_page_start(store, threads, &path, head, Owner::Main, end, DISPLAY_TAIL)?;
        let from = context_start.min(tail_start);

        // The branch from there on, and the sub-agent runs that reach
        // past it.
        let mut held = Vec::new();
        for (thread, upto) in &ancestry {
            if *upto < from {
                continue;
            }
            if let Some(mut span) = span(threads, *thread, *upto) {
                span.after = span.after.max(from - 1);
                for (seq, entry) in store.entries(span, false)? {
                    entries.insert(seq, (*thread, entry));
                }
            }
            held.push((*thread, (from, *upto)));
        }
        for index in runs_reaching(threads, &path, from..end) {
            let run = threads.get(index).expect("a thread of the plan");
            if let Some(span) = span(threads, index, run.tip) {
                for (seq, entry) in store.entries(span, false)? {
                    entries.insert(seq, (index, entry));
                }
            }
            held.push((index, (0, run.tip)));
        }

        // What stands in for the main agent's messages before the window.
        let window = entries.range(from..).map(|(_, (_, entry))| entry);
        let mut wanted = kept_messages(store, session, threads, &ancestry, window)?;
        wanted.extend(last_response(store, threads, &ancestry, from)?);
        wanted.retain(|seq, _| !entries.contains_key(seq));
        for (seq, thread, entry) in read_entries(store, session, wanted)? {
            entries.insert(seq, (thread, entry));
        }
        let met = |seq| entries.contains_key(&seq);
        let window = Window {
            from,
            usage: usage_before(store, threads, &ancestry, from, met)?,
            anchor: Some(self.head.0.clone()),
        };
        Ok(LoadedBranch {
            entries,
            plan: self,
            window,
            held,
        })
    }
}

/// The state entries of the branch whose threads are `ancestry`, each with
/// its position and thread: settings, goals, the environment and
/// compactions are read from them. `count` is the session's last position.
///
/// The meta entries are among them. They are filed beside the root
/// thread's messages and belong to every branch, wherever it leaves
/// the root.
fn branch_state(
    store: &crate::sqlite::SqliteStore,
    threads: &crate::threads::Threads,
    ancestry: &[(usize, u64)],
    count: u64,
) -> Result<BTreeMap<u64, (usize, LogEntry)>, SqliteError> {
    let mut entries = BTreeMap::new();
    for (thread, upto) in ancestry {
        let Some(mut span) = span(threads, *thread, *upto) else {
            continue;
        };
        let root = threads.get(*thread).is_some_and(|t| t.parent.is_none());
        if root {
            span.upto = count;
        }
        for (seq, entry) in store.entries(span, true)? {
            if seq <= *upto || entry.thread == ThreadKind::Meta {
                entries.insert(seq, (*thread, entry));
            }
        }
    }
    Ok(entries)
}

/// Whether the entry of `thread` at `seq` is on the branch whose threads
/// are `ancestry`.
fn on_branch(ancestry: &[(usize, u64)], thread: usize, seq: u64) -> bool {
    let mut ancestry = ancestry.iter();
    ancestry.any(|(on, upto)| *on == thread && seq <= *upto)
}

/// Where the user's messages sit that the compactions among `entries`
/// kept, on the branch whose threads are `ancestry`: position and
/// thread. A compaction names them by id, and they may lie far before it.
/// The last compaction's are part of the model context. An earlier one's
/// are what the occupancy it reports is estimated from.
fn kept_messages<'a>(
    store: &crate::sqlite::SqliteStore,
    session: &str,
    threads: &crate::threads::Threads,
    ancestry: &[(usize, u64)],
    entries: impl Iterator<Item = &'a LogEntry>,
) -> Result<BTreeMap<u64, usize>, SqliteError> {
    let mut kept = BTreeMap::new();
    for entry in entries {
        let LogEntryKind::Compaction {
            retained_user_entry_ids,
            ..
        } = &entry.entry
        else {
            continue;
        };
        for id in retained_user_entry_ids {
            if let Some(at) = store.locate(session, id)?
                && let Some(thread) = threads.thread_of(at.thread)
                && on_branch(ancestry, thread, at.seq)
            {
                kept.insert(at.seq, thread);
            }
        }
    }
    Ok(kept)
}

/// The messages of `thread` up to `upto`, as a stretch of its stored
/// thread. `None` for a thread of draft entries, which the store
/// does not have.
fn span(
    threads: &crate::threads::Threads,
    thread: usize,
    upto: u64,
) -> Option<crate::sqlite::Span> {
    let thread = threads.get(thread)?;
    Some(crate::sqlite::Span {
        thread: thread.id?,
        after: thread.parent.map_or(0, |(_, at)| at),
        upto,
    })
}

/// The sub-agent runs on `path` with a message in `range`. A run with
/// entries on both sides of a window's start is held whole, so its box,
/// report, usage and tool calls project as from a whole log.
fn runs_reaching(
    threads: &crate::threads::Threads,
    path: &crate::threads::Path,
    range: std::ops::Range<u64>,
) -> Vec<usize> {
    let reaches = |(index, thread): &(usize, &crate::threads::Thread)| {
        let started = thread.parent.map_or(1, |(_, at)| at + 1);
        matches!(thread.owner, crate::threads::Owner::Sub(_))
            && path.holds(*index, thread.tip)
            && started < range.end
            && thread.tip >= range.start
    };
    let runs = threads.iter().enumerate().filter(reaches);
    runs.map(|(index, _)| index).collect()
}

/// Where `owner`'s page of history ending before `before` starts on the
/// branch ending at `head`, whose path is `path` (see
/// [`crate::threads::PageStart`]). `1` when nothing earlier can start it.
///
/// The rows the page counts are walked against append order, each
/// thread through its own index, until the page's start is found.
/// Reads no entry.
fn stored_page_start(
    store: &crate::sqlite::SqliteStore,
    threads: &crate::threads::Threads,
    path: &crate::threads::Path,
    head: (usize, u64),
    owner: crate::threads::Owner,
    before: u64,
    limit: u64,
) -> Result<u64, SqliteError> {
    use crate::threads::{Owner, PageStart};
    let last = before.saturating_sub(1);
    let mut spans = Vec::new();
    if owner == Owner::Main {
        for (thread, upto) in threads.ancestry(Some(head)) {
            spans.extend(span(threads, thread, upto.min(last)));
        }
    }
    for (index, thread) in threads.iter().enumerate() {
        let counted = match owner {
            Owner::Main => matches!(thread.owner, Owner::Sub(_)),
            sub => thread.owner == sub,
        };
        if counted && path.holds(index, thread.tip) {
            spans.extend(span(threads, index, thread.tip.min(last)));
        }
    }
    // The session's first entry starts no page.
    for span in &mut spans {
        span.after = span.after.max(1);
    }
    let mut page = PageStart::new(threads, path, owner, limit);
    let mut start = 1;
    store.walk(&spans, true, None, |row| {
        let Some(thread) = threads.thread_of(row.thread) else {
            return true;
        };
        let starts = page.visit(row.seq, thread, row.class());
        if starts {
            start = row.seq;
        }
        !starts
    })?;
    Ok(start)
}

/// The stretches of the main agent's threads before `from` on the
/// branch whose threads are `ancestry`, root first, which is append order.
fn spans_before(
    threads: &crate::threads::Threads,
    ancestry: &[(usize, u64)],
    from: u64,
) -> Vec<crate::sqlite::Span> {
    let last = from.saturating_sub(1);
    let earlier = ancestry.iter().rev();
    earlier
        .filter_map(|(thread, upto)| span(threads, *thread, (*upto).min(last)))
        .collect()
}

/// Where the main agent's last response before `from` sits on the branch
/// whose threads are `ancestry`: its position and thread. It names
/// the model in use there, which the recorded settings follow.
fn last_response(
    store: &crate::sqlite::SqliteStore,
    threads: &crate::threads::Threads,
    ancestry: &[(usize, u64)],
    from: u64,
) -> Result<Option<(u64, usize)>, SqliteError> {
    let mut found = None;
    let spans = spans_before(threads, ancestry, from);
    store.walk(&spans, true, Some("assistant"), |row| {
        found = threads
            .thread_of(row.thread)
            .map(|thread| (row.seq, thread));
        false
    })?;
    Ok(found)
}

/// What the main agent's completed responses before `from` reported, on
/// the branch whose threads are `ancestry`, but for the ones `held` names:
/// a projection meets those and counts them itself (see [`Window`]). It is
/// summed in append order, as a projection of the whole session sums it.
fn usage_before(
    store: &crate::sqlite::SqliteStore,
    threads: &crate::threads::Threads,
    ancestry: &[(usize, u64)],
    from: u64,
    held: impl Fn(u64) -> bool,
) -> Result<aj_models::types::Usage, SqliteError> {
    let mut usage = aj_models::types::Usage::default();
    for span in spans_before(threads, ancestry, from) {
        store.usage(span, |seq, reported| {
            if !held(seq) {
                usage.accumulate(&reported);
            }
        })?;
    }
    Ok(usage)
}

/// Read the entries at `wanted`, each with its position and thread.
fn read_entries(
    store: &crate::sqlite::SqliteStore,
    session: &str,
    wanted: BTreeMap<u64, usize>,
) -> Result<Vec<(u64, usize, LogEntry)>, SqliteError> {
    let positions: Vec<u64> = wanted.keys().copied().collect();
    let found = store.entries_at(session, &positions)?;
    if found.len() != positions.len() {
        return Err(SqliteError::Invalid(format!(
            "session {session:?} changed while it was read"
        )));
    }
    let entries = wanted.into_iter().zip(found);
    Ok(entries
        .map(|((seq, thread), entry)| (seq, thread, entry))
        .collect())
}

/// A sub-agent whose thread holds something an earlier process left
/// (see [`SessionLog::sub_agents_with_work`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeftSubAgent {
    pub agent_id: usize,
    /// Whether its turn is stored as working: the process that ran it
    /// ended before the turn did.
    pub working: bool,
}

/// What a log held before [`SessionLog::install_branch`].
pub struct LeftBranch(LogSnapshot);

/// Where [`SessionLog::find`] found an entry.
pub struct Found {
    pub parent_id: Option<EntryId>,
    /// Its thread and position.
    at: (usize, u64),
}

/// A pending read of a whole session (see [`SessionLog::whole`]).
pub struct WholeRead {
    core: LogSnapshot,
    /// The database and project of a log that does not hold everything.
    source: Option<(PathBuf, String)>,
}

impl WholeRead {
    /// Run the read. Blocking database I/O on a connection of its own,
    /// none at all for a log that already holds its whole session.
    pub fn read(self) -> Result<LogSnapshot, SessionError> {
        let Some((database, project_id)) = self.source else {
            return Ok(self.core);
        };
        let store = crate::sqlite::SqliteStore::open_project_read_only(&database, &project_id)?;
        let mut whole = LogSnapshot::whole(&store, &self.core.session_id)?;
        // What the log has appended and not yet written.
        let unwritten = self.core.held.range(whole.count + 1..);
        for (_, (_, entry)) in unwritten {
            whole.push(Arc::clone(entry));
        }
        whole.head = self.core.head;
        Ok(whole)
    }
}

/// A session's one write connection, with what the handles that share it
/// need to know of the log. The log and the handles for pending work,
/// task records and the turn state write through it.
#[derive(Default)]
pub(crate) struct StoreHandle {
    /// `None` until the session is stored or resumed.
    pub store: Option<crate::sqlite::SqliteStore>,
    /// Whether the database holds the session. It does from the session's
    /// first commit on.
    pub stored: bool,
    /// The main thread the main agent is on. `None` exactly while
    /// the log holds a draft: nothing was accepted for the session or branch
    /// yet, so the agent has no work to file.
    pub main: Option<crate::sqlite::ThreadId>,
    /// The state of the main agent's turn on `main`. Idle on a draft.
    pub turn: crate::TurnState,
    /// The stored thread of each sub-agent's run on the selected
    /// branch, by sub-agent number, as the log resolves it (see
    /// [`LogSnapshot::runs`]). The log replaces it when a run starts and
    /// when the head is switched.
    pub sub_runs: HashMap<usize, crate::sqlite::ThreadId>,
    /// The state of a sub-agent's turn as this writer stored or read it,
    /// by the thread of its run. One it has not read is idle here:
    /// an earlier process's is read when its thread is picked up
    /// (see [`SessionLog::pick_up_turn`]).
    pub sub_turns: HashMap<crate::sqlite::ThreadId, crate::TurnState>,
    /// Set when the session's owner lets go of it (see
    /// [`crate::Journal::close`]). Every later write is refused.
    pub closed: bool,
}

impl StoreHandle {
    /// The store, for a write of the stored session. A session that is
    /// not stored is refused, and so is every write once the writer is
    /// closed.
    pub(crate) fn writable(
        &self,
        session_id: &str,
    ) -> Result<&crate::sqlite::SqliteStore, SqliteError> {
        if self.closed {
            return Err(SqliteError::Closed);
        }
        let store = self.store.as_ref().filter(|_| self.stored);
        store.ok_or_else(|| SqliteError::SessionNotFound(session_id.to_owned()))
    }

    /// The state of sub-agent `n`'s turn on its run of the selected
    /// branch, as this writer stored or read it.
    pub(crate) fn sub_turn(&self, n: usize) -> crate::TurnState {
        let run = self.sub_runs.get(&n);
        run.and_then(|run| self.sub_turns.get(run))
            .copied()
            .unwrap_or_default()
    }

    /// The stored thread of `agent` on the selected branch: the one
    /// the main agent is on, a sub-agent's run. `None` for the main agent
    /// on a draft and for a sub-agent the branch shows no run of.
    pub(crate) fn thread(
        &self,
        agent: aj_agent::events::AgentId,
    ) -> Option<crate::sqlite::ThreadId> {
        match agent {
            aj_agent::events::AgentId::Main => self.main,
            aj_agent::events::AgentId::Sub(n) => self.sub_runs.get(&n).copied(),
        }
    }

    /// `agent` as the store is told whose work a row is. Work is accepted
    /// for an agent that has a thread, so one without is a broken
    /// invariant and not a state to wait out.
    pub(crate) fn owner(
        &self,
        agent: aj_agent::events::AgentId,
    ) -> Result<crate::sqlite::WorkOwner, SqliteError> {
        use crate::sqlite::WorkOwner;
        let thread_id = self.thread(agent).ok_or_else(|| {
            SqliteError::NoThread(match agent {
                aj_agent::events::AgentId::Main => "the main agent".to_owned(),
                aj_agent::events::AgentId::Sub(n) => format!("sub-agent {n}"),
            })
        })?;
        Ok(match agent {
            aj_agent::events::AgentId::Main => WorkOwner::Main(thread_id),
            aj_agent::events::AgentId::Sub(_) => WorkOwner::Sub(thread_id),
        })
    }
}

/// What the log holds for a session or a branch the store does not have yet
/// (see the [module docs](self)). It is what the creating transaction
/// writes, and nothing else.
#[derive(Default)]
struct Draft {
    /// The stored entry a branch continues from. `None` for a session's
    /// first thread.
    from: Option<EntryId>,
    /// The entries that precede the first message, in append order. They
    /// are held by the log's cache as its newest entries.
    entries: Vec<Arc<LogEntry>>,
    /// The label and archive marker of a session that is not stored.
    /// Those of a stored session live in the store.
    annotations: crate::sqlite::SessionAnnotations,
}

pub(crate) type SharedStore = Arc<std::sync::Mutex<StoreHandle>>;

/// How many trailing entries a freshly opened session shows before any
/// earlier history is asked for.
pub const DISPLAY_TAIL: u64 = 400;

/// An append-only tree of entries stored in the session database.
///
/// An entry is committed before it enters the in-memory tree, which files
/// it where the store says it landed. The caller holds the session lock,
/// which is what keeps a second writer out.
///
/// A write the database was too busy to take changed nothing and can be
/// made again (see [`SessionError::is_busy`]). A returned
/// [`SessionError::WriteFailed`] fuses this object: no later append or
/// head mutation can retry the uncertain operation. Recovery belongs to a
/// newly opened object under the session lock.
pub struct SessionLog {
    /// The entry tree, a cache for reads. An append changes it after the
    /// entry has been committed, from the store's answer. The entries of
    /// a draft only live here and in `draft`.
    core: LogSnapshot,
    /// The shared session database.
    path: PathBuf,
    project_id: String,
    /// The session's write connection.
    store: SharedStore,
    /// The session or branch the main agent would work on, while the store
    /// does not have it. `Some` exactly while [`StoreHandle::main`] is `None`.
    draft: Option<Draft>,
    /// Whether work appended at a draft joins it (see [`Self::in_draft`]).
    holding: bool,
    /// A persistence error is terminal for this live object.
    write_state: WriteState,
    /// Installed only by a hosted composition. The log reports from `fuse`, so
    /// listener appends and direct driver writers share one notification edge.
    failure_signal: Option<PersistenceFailureSender>,
    #[cfg(any(test, feature = "test-support"))]
    sqlite_commit_fault: bool,
    /// How long a write waits for another connection's lock (see
    /// [`Self::set_busy_wait`]).
    busy_wait: std::time::Duration,
}

impl LogSnapshot {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            held: BTreeMap::new(),
            positions: HashMap::new(),
            count: 0,
            head: None,
            threads: crate::threads::Threads::default(),
            window: Window::default(),
        }
    }

    /// The stored thread of every sub-agent's run on the selected
    /// branch, by sub-agent number.
    fn sub_threads(&self) -> HashMap<usize, crate::sqlite::ThreadId> {
        let runs = self.runs().into_iter();
        runs.filter_map(|(agent, thread)| Some((agent, self.threads.get(thread)?.id?)))
            .collect()
    }

    /// What a resumed session holds: the branch of the main thread
    /// it lands on (see [`Window`]), with that thread. Which one is
    /// read off stored state: the thread that holds pending work,
    /// because its host accepted that for the thread it was on, and
    /// otherwise the one last appended to.
    ///
    /// A thread that holds no entry is landed on only for its
    /// pending work. The head is then the entry it continues from.
    fn stored(
        store: &crate::sqlite::SqliteStore,
        session_id: &str,
    ) -> Result<(Self, Option<crate::sqlite::ThreadId>), SqliteError> {
        use crate::threads::{Owner, Threads};
        let mut core = Self::new(session_id.to_owned());
        core.threads = Threads::stored(store.threads(session_id)?)?;
        let metadata = store.session_metadata(session_id)?;
        core.count = metadata.map_or(0, |metadata| metadata.last_seq);
        // The session's first entry tells whether its first thread
        // has a message: one that ends at the system prompt has no leaf.
        if let Some(first) = store.entries_at(session_id, &[1])?.pop() {
            core.hold(1, 0, first);
        }
        let pending = store.main_with_pending(session_id)?;
        let pending = pending.and_then(|thread_id| core.threads.thread_of(thread_id));
        let selected = pending.or_else(|| core.last_written(ThreadFilter::MAIN));
        let thread_id = match selected {
            Some(thread) => core.threads.get(thread).and_then(|thread| thread.id),
            // No thread has a message, so the session is on its first.
            None => {
                let mut threads = core.threads.iter();
                let first = threads.find(|thread| thread.owner == Owner::Main);
                first.and_then(|thread| thread.id)
            }
        };
        let head = selected.and_then(|index| {
            let thread = core.threads.get(index)?;
            if thread.is_empty() {
                return Some((thread.tip_id.clone(), thread.parent?));
            }
            let leaf = core.get(&thread.tip_id);
            leaf.is_none_or(|entry| ThreadFilter::MAIN.matches(entry))
                .then(|| (thread.tip_id.clone(), (index, thread.tip)))
        });
        let Some((head, at)) = head else {
            // Nothing to select a window by, so there is no long
            // thread either.
            return Ok((Self::whole(store, session_id)?, thread_id));
        };
        let stored = core.count;
        let branch = core.branch_plan(&head, at).read(store)?;
        core.install(branch, stored);
        core.head = Some(head);
        Ok((core, thread_id))
    }

    /// The whole stored session, read at one snapshot of the store. No head
    /// is selected.
    fn whole(store: &crate::sqlite::SqliteStore, session_id: &str) -> Result<Self, SqliteError> {
        let (threads, entries) = store.whole(session_id)?;
        let mut core = Self::new(session_id.to_owned());
        core.threads = crate::threads::Threads::stored(threads)?;
        for (seq, (thread_id, entry)) in (1..).zip(entries) {
            let thread = core.thread_of(thread_id)?;
            core.hold(seq, thread, entry);
            core.count = seq;
        }
        core.threads.set_held(|_, thread| Some((0, thread.tip)));
        Ok(core)
    }

    /// The place of a stored thread of this session.
    fn thread_of(&self, thread_id: crate::sqlite::ThreadId) -> Result<usize, SqliteError> {
        self.threads.thread_of(thread_id).ok_or_else(|| {
            SqliteError::Invalid(format!(
                "session {:?} changed while it was read",
                self.session_id
            ))
        })
    }

    /// Add `entry`, which the store does not hold, at the end of the
    /// append order and file it as a draft. Its parent is looked for among
    /// the held entries and the thread tips, which is everything a log
    /// that holds its whole session or appends at a head can name.
    fn push(&mut self, entry: Arc<LogEntry>) {
        let parent = entry.parent_id.as_ref().and_then(|id| self.where_is(id));
        let filing = self.place(entry.thread, entry.agent_id, parent);
        self.push_filed(entry, &filing);
    }

    /// Where the next draft entry goes when it has this kind and
    /// sub-agent and its parent sits at `parent`.
    fn place(
        &self,
        thread: ThreadKind,
        agent_id: Option<usize>,
        parent: Option<(usize, u64)>,
    ) -> crate::threads::Filing {
        self.threads.place(thread, agent_id, parent)
    }

    /// Add `entry` at the end of the append order, filed where
    /// [`Self::place`] put it.
    fn push_filed(&mut self, entry: Arc<LogEntry>, filing: &crate::threads::Filing) {
        self.count += 1;
        self.threads.file(filing, self.count, &entry.id);
        self.positions.insert(entry.id.clone(), self.count);
        self.held.insert(self.count, (filing.thread, entry));
    }

    /// Add an entry the store committed, filed where the store says it
    /// landed (see [`crate::threads::Threads::land`]).
    fn land(
        &mut self,
        entry: Arc<LogEntry>,
        landed: &crate::sqlite::Landed,
    ) -> Result<(), SqliteError> {
        let owner = crate::threads::Owner::of(entry.thread, entry.agent_id);
        let thread = self.threads.land(owner, landed, &entry.id)?;
        self.count = landed.seq;
        self.positions.insert(entry.id.clone(), landed.seq);
        self.held.insert(landed.seq, (thread, entry));
        Ok(())
    }

    /// Forget the draft `entries`, which are this log's newest entries,
    /// and the threads they were filed in.
    fn drop_drafts(&mut self, entries: &[Arc<LogEntry>]) {
        for entry in entries {
            if let Some(seq) = self.positions.remove(&entry.id) {
                self.held.remove(&seq);
                self.count -= 1;
            }
        }
        self.threads.drop_drafts();
    }

    /// Hold a stored entry this log did not hold.
    fn hold(&mut self, seq: u64, thread: usize, entry: LogEntry) {
        self.positions.insert(entry.id.clone(), seq);
        self.held.insert(seq, (thread, Arc::new(entry)));
    }

    /// The thread and position of `id`, for a held entry or the
    /// last entry of any thread.
    fn where_is(&self, id: &EntryId) -> Option<(usize, u64)> {
        match self.positions.get(id) {
            Some(seq) => Some((self.held.get(seq)?.0, *seq)),
            None => self.threads.tip(id),
        }
    }

    /// Whether this log holds every entry of its session.
    pub fn is_complete(&self) -> bool {
        u64::try_from(self.held.len()).is_ok_and(|held| held == self.count)
    }

    /// The entries a view of the branch ending at `head` shows (see
    /// [`crate::threads::Threads::path`]).
    pub(crate) fn path(&self, head: Option<&EntryId>) -> crate::threads::Path {
        self.threads.path(head.and_then(|id| self.where_is(id)))
    }

    /// The held entries with their position and thread, in append
    /// order, from position `from` up to but not including `before`.
    pub(crate) fn range(
        &self,
        from: u64,
        before: u64,
    ) -> impl DoubleEndedIterator<Item = (u64, usize, &LogEntry)> {
        let range = (from < before).then(|| self.held.range(from..before));
        let entries = range.into_iter().flatten();
        entries.map(|(seq, (thread, entry))| (*seq, *thread, entry.as_ref()))
    }

    /// The held entries of the branch ending at `head`, last to first:
    /// those of its thread up to it, then of each thread it continues from
    /// up to the fork point. Sub-agent runs along the way are not part of
    /// it. Empty for a head this log cannot place.
    ///
    /// In a log that holds its whole session this is the parent chain. In
    /// a partial one it skips what is not held, so it still reaches the
    /// state entries before the window.
    fn branch(&self, head: &EntryId) -> impl Iterator<Item = &LogEntry> {
        self.branch_at(self.where_is(head))
    }

    /// [`Self::branch`] for the entry at `at`, held or not.
    fn branch_at(&self, at: Option<(usize, u64)>) -> impl Iterator<Item = &LogEntry> {
        let ancestry = self.threads.ancestry(at);
        let upto = at.map_or(0, |(_, seq)| seq);
        self.range(1, upto.saturating_add(1))
            .rev()
            .filter(move |(seq, thread, _)| {
                let mut ancestry = ancestry.iter();
                ancestry.any(|(on, upto)| on == thread && seq <= upto)
            })
            .map(|(_, _, entry)| entry)
    }

    /// Latest user-branch goal on the ancestry ending at `head`, inclusive.
    /// An explicit clear stops inheritance. Meta and sub-agent entries are ignored.
    /// An unknown head or a branch without a goal yields `None`.
    ///
    /// A log that does not hold its whole session answers for an entry it
    /// holds. [`SessionLog::goal_at`] answers for any.
    pub fn goal_at(&self, head: &EntryId) -> Option<Goal> {
        self.goal_from(self.where_is(head)?)
    }

    /// [`Self::goal_at`] for the entry at `at`, held or not.
    fn goal_from(&self, at: (usize, u64)) -> Option<Goal> {
        let mut branch = self.branch_at(Some(at));
        branch.find_map(|entry| match &entry.entry {
            LogEntryKind::GoalChange { goal } if entry.thread == ThreadKind::Main => {
                Some(goal.clone())
            }
            _ => None,
        })?
    }

    /// The id under which this log is listed by `aj list-sessions`.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The branch ending at `head`, keeping only entries matching
    /// `filter`, in chronological (root-first) order, wrapped in a
    /// read-only [BranchView] view that can be handed to the model.
    ///
    /// A broken chain (a `parent_id` pointing at an entry not in the log)
    /// yields a *partial* view rather than an error: the branch starts at
    /// the break, so the root and everything above it are dropped. Append
    /// validates that a parent exists, so this only arises on a corrupt or
    /// hand-edited file (or the sibling-branch case two concurrent writers
    /// produce).
    ///
    /// A log that does not hold its whole session yields what it holds of
    /// the branch. That is the whole model context for a head
    /// [`SessionLog::set_head`] accepts, and of a branch the log was
    /// not made to hold (see [`SessionLog::branch_read`]) it may be
    /// nothing.
    pub fn linearize(&self, head: &EntryId, filter: ThreadFilter) -> BranchView {
        let branch = self.branch(head).filter(|entry| filter.matches(entry));
        let mut out: Vec<LogEntry> = branch.cloned().collect();
        out.reverse();
        BranchView::from_entries(self.session_id.clone(), out)
    }

    /// Recorded main-thread settings on the ancestry ending at `head`, inclusive.
    /// Equivalent to `linearize(head, ThreadFilter::MAIN).settings()` without
    /// cloning message history. Meta and sub-agent entries do not contribute.
    /// An unknown head yields defaults, and a broken ancestry yields the known
    /// suffix, as in [`Self::linearize`]. This does not select the head.
    ///
    /// A log that does not hold its whole session answers for an entry it
    /// holds. [`SessionLog::settings_at`] answers for any.
    pub fn settings_at(&self, head: &EntryId) -> SessionSettings {
        self.where_is(head)
            .map(|at| self.settings_from(at))
            .unwrap_or_default()
    }

    /// [`Self::settings_at`] for the entry at `at`, held or not.
    fn settings_from(&self, at: (usize, u64)) -> SessionSettings {
        let branch = self.branch_at(Some(at));
        let path: Vec<_> = branch
            .filter(|entry| ThreadFilter::MAIN.matches(entry))
            .collect();
        let mut settings = SessionSettings::default();
        for entry in path.into_iter().rev() {
            settings.apply(&entry.entry);
        }
        settings
    }

    /// The last entry of `filter`'s thread, or `None` if it has none. For
    /// the main thread that is the most recently appended main entry of
    /// the session, on any branch, which picks the default "current" head
    /// when resuming. For a sub-agent it is the last entry of its run on
    /// the selected branch (see [`Self::runs`]).
    pub fn latest_leaf(&self, filter: ThreadFilter) -> Option<EntryId> {
        let thread = self.threads.get(self.last_written(filter)?)?;
        Some(thread.tip_id.clone())
    }

    /// The thread [`Self::latest_leaf`] ends. A thread that holds no entry
    /// has no leaf.
    fn last_written(&self, filter: ThreadFilter) -> Option<usize> {
        use crate::threads::Owner;
        match (filter.thread, filter.agent_id) {
            (ThreadKind::Main, _) => {}
            (ThreadKind::Sub, Some(n)) => return self.run_of(n),
            // No entry matches these filters (see `ThreadFilter::matches`).
            (ThreadKind::Sub, None) | (ThreadKind::Meta, _) => return None,
        };
        // The root thread ends at the session's first entry, the system
        // prompt, until a message follows it. That entry is always held.
        let tips = self.threads.iter().enumerate().filter(|(_, thread)| {
            let tip = self.get(&thread.tip_id);
            thread.owner == Owner::Main
                && !thread.is_empty()
                && tip.is_none_or(|entry| filter.matches(entry))
        });
        let (last, _) = tips.max_by_key(|(_, thread)| thread.tip)?;
        Some(last)
    }

    /// The last compaction of the branch ending at `head`: the first entry
    /// it kept and the user's earlier messages it kept. The model context
    /// of the branch is those messages and everything from that entry on.
    fn kept(&self, head: &EntryId) -> Option<(&EntryId, &[EntryId])> {
        self.branch(head).find_map(|entry| match &entry.entry {
            LogEntryKind::Compaction {
                first_kept_entry_id,
                retained_user_entry_ids,
                ..
            } => Some((first_kept_entry_id, retained_user_entry_ids.as_slice())),
            _ => None,
        })
    }

    /// Whether this log holds the whole model context of the branch ending
    /// at `head`.
    fn covers(&self, head: &EntryId) -> bool {
        if self.is_complete() || self.window.anchor.as_ref() == Some(head) {
            return true;
        }
        // A held entry is on the held branch or was appended to it, so
        // its branch's compactions are held too.
        if !self.positions.contains_key(head) {
            return false;
        }
        let start = match self.kept(head) {
            Some((first_kept, retained)) => {
                if !retained.iter().all(|id| self.positions.contains_key(id)) {
                    return false;
                }
                match self.positions.get(first_kept) {
                    Some(start) => *start,
                    None => return false,
                }
            }
            None => 1,
        };
        let ancestry = self.threads.ancestry(self.where_is(head));
        ancestry.into_iter().all(|(index, upto)| {
            let thread = self.threads.get(index).expect("a thread of this log");
            upto < start
                || thread
                    .held
                    .is_some_and(|(lo, hi)| lo <= start && upto <= hi)
        })
    }

    /// What to read to hold the branch ending at `head`, which sits at
    /// `at`.
    fn branch_plan(&self, head: &EntryId, at: (usize, u64)) -> BranchPlan {
        BranchPlan {
            session_id: self.session_id.clone(),
            threads: self.threads.clone(),
            head: (head.clone(), at.0, at.1),
            count: self.count,
        }
    }

    /// Hold a branch read from the store in place of everything this log
    /// holds of the stored session. `stored` is the position of the
    /// last entry the store has: the entries of a draft sit after it and
    /// stay.
    fn install(&mut self, branch: LoadedBranch, stored: u64) {
        let drafts = self.held.split_off(&stored.saturating_add(1));
        for (_, entry) in std::mem::replace(&mut self.held, drafts).into_values() {
            self.positions.remove(&entry.id);
        }
        for (seq, (thread, entry)) in branch.entries {
            self.hold(seq, thread, entry);
        }
        let held = branch.held;
        self.threads.set_held(|index, _| {
            let mut held = held.iter();
            held.find_map(|(thread, range)| (*thread == index).then_some(*range))
        });
        self.window = branch.window;
    }

    /// How many entries this log holds.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// The usage the main agent's completed responses before the window
    /// reported, but for those this log holds. `None` for a log that holds
    /// its whole session.
    pub(crate) fn carried(&self) -> Option<&aj_models::types::Usage> {
        (self.window.from > 1).then_some(&self.window.usage)
    }

    /// Hold `agent`'s page of history before `before`, of at least `limit`
    /// entries, and return where it starts (see [`Self::page_start`]). The
    /// main agent's page moves the window's start back to it. A
    /// sub-agent's runs on the branch are held whole.
    pub(crate) fn load_page(
        &mut self,
        store: &crate::sqlite::SqliteStore,
        agent: AgentId,
        before: u64,
        limit: u64,
    ) -> Result<u64, SqliteError> {
        use crate::threads::Owner;
        let session = self.session_id.clone();
        let Some(head) = self.head.as_ref().and_then(|id| self.where_is(id)) else {
            return Ok(1);
        };
        let path = self.threads.path(Some(head));
        let owner = match agent {
            AgentId::Main => Owner::Main,
            AgentId::Sub(n) => Owner::Sub(n),
        };
        let start = stored_page_start(store, &self.threads, &path, head, owner, before, limit)?;
        let runs = match owner {
            Owner::Main if start >= self.window.from => Vec::new(),
            Owner::Main => {
                let ancestry = self.threads.ancestry(Some(head));
                for (thread, upto) in &ancestry {
                    let last = (*upto).min(before.saturating_sub(1));
                    let Some(mut span) = span(&self.threads, *thread, last) else {
                        continue;
                    };
                    span.after = span.after.max(start - 1);
                    for (seq, entry) in store.entries(span, false)? {
                        self.hold(seq, *thread, entry);
                    }
                }
                let page = self.range(start, before).map(|(_, _, entry)| entry);
                let mut wanted = kept_messages(store, &session, &self.threads, &ancestry, page)?;
                wanted.extend(last_response(store, &self.threads, &ancestry, start)?);
                wanted.retain(|seq, _| !self.held.contains_key(seq));
                for (seq, thread, entry) in read_entries(store, &session, wanted)? {
                    self.hold(seq, thread, entry);
                }
                let held = |seq| self.held.contains_key(&seq);
                self.window = Window {
                    from: start,
                    usage: usage_before(store, &self.threads, &ancestry, start, held)?,
                    anchor: self.window.anchor.take(),
                };
                runs_reaching(&self.threads, &path, start..before)
            }
            // The agent's runs on the branch.
            _ => {
                let threads = self.threads.iter().enumerate();
                let runs = threads
                    .filter(|(index, run)| run.owner == owner && path.holds(*index, run.tip));
                runs.map(|(index, _)| index).collect()
            }
        };
        for index in runs {
            let run = self.threads.get(index).expect("a thread of this log");
            if run.held == Some((0, run.tip)) {
                continue;
            }
            if let Some(span) = span(&self.threads, index, run.tip) {
                for (seq, entry) in store.entries(span, false)? {
                    self.hold(seq, index, entry);
                }
            }
            self.threads.hold_whole(index);
        }
        Ok(start)
    }

    /// Hold every message the user sent on the selected branch. A
    /// compaction chooses the ones to keep among all of them (see
    /// [`crate::compaction::prepare_compaction`]), and a window holds only
    /// those from its start on and the ones the last compaction kept.
    fn load_user_messages(
        &mut self,
        store: &crate::sqlite::SqliteStore,
    ) -> Result<(), SqliteError> {
        let Some(head) = self.head.as_ref().and_then(|id| self.where_is(id)) else {
            return Ok(());
        };
        let ancestry = self.threads.ancestry(Some(head));
        let spans = spans_before(&self.threads, &ancestry, self.window.from);
        let mut wanted = BTreeMap::new();
        store.walk(&spans, false, Some("user"), |row| {
            if !self.held.contains_key(&row.seq)
                && let Some(thread) = self.threads.thread_of(row.thread)
            {
                wanted.insert(row.seq, thread);
            }
            true
        })?;
        for (seq, thread, entry) in read_entries(store, &self.session_id, wanted)? {
            self.hold(seq, thread, entry);
        }
        Ok(())
    }

    /// Position this log's view of the selected branch starts at.
    /// `1` for a log that holds its whole session.
    pub fn window_start(&self) -> u64 {
        self.window.from
    }

    /// Position a freshly opened view starts at: where the last
    /// page of [`DISPLAY_TAIL`] entries of the selected branch starts, or
    /// `1` when the branch is no longer than that.
    pub fn display_start(&self) -> u64 {
        self.page_start(AgentId::Main, u64::MAX, DISPLAY_TAIL)
    }

    /// Where `agent`'s page of history ending before `before` starts: the
    /// nearest message of that agent with at least `limit` entries between
    /// it and `before` (see [`crate::threads::PageStart`]), or the start of
    /// what this log holds when the agent has nothing earlier to cut at.
    ///
    /// A log that does not hold its whole session answers for its window
    /// only. [`crate::ProjectSessions::load_page`] reads further
    /// back.
    pub fn page_start(&self, agent: AgentId, before: u64, limit: u64) -> u64 {
        use crate::threads::{Class, Owner, PageStart};
        let path = self.path(self.head.as_ref());
        let owner = match agent {
            AgentId::Main => Owner::Main,
            AgentId::Sub(n) => Owner::Sub(n),
        };
        let mut page = PageStart::new(&self.threads, &path, owner, limit);
        let mut earlier = self.range(self.window.from.max(2), before).rev();
        earlier
            .find(|(seq, thread, entry)| page.visit(*seq, *thread, Class::of(&entry.entry)))
            .map_or(self.window.from, |(seq, _, _)| seq)
    }

    /// The main-thread head: the entry the next main-thread append
    /// anchors at. `None` only while the main thread is empty. See the
    /// `head` field for the full contract.
    pub fn head(&self) -> Option<&EntryId> {
        self.head.as_ref()
    }

    /// Position of the most recent entry, `0` when the log is
    /// empty. This is the high-water mark of the 1-based positions
    /// [`SessionLog::append`] hands out (see [`EntryRef`]).
    pub fn last_seq(&self) -> u64 {
        self.count
    }

    /// Whether this log holds an entry `id`, on any thread or branch.
    ///
    /// A caller that distinguishes "no such entry" from "that entry cannot
    /// be a head" needs this: [`SessionLog::set_head`] refuses both
    /// with the same error. A log that does not hold its whole session
    /// answers [`SessionLog::find`] for the rest.
    pub fn contains(&self, id: &EntryId) -> bool {
        self.positions.contains_key(id)
    }

    /// The parent of `id`, `None` both when the log does not hold `id` and
    /// when `id` is a root. Pair with [`Self::contains`] to tell those apart.
    ///
    /// This is what resolves "branch before this message": the head moves to
    /// the message's parent, so the branch replaces the message rather than
    /// continuing after it.
    pub fn parent_of(&self, id: &EntryId) -> Option<&EntryId> {
        self.get(id)?.parent_id.as_ref()
    }

    /// The largest `agent_id` recorded on any entry in the log, or `None`
    /// if no subagent entries exist. Used on resume to seed the session's
    /// subagent counter so freshly-spawned subagents don't reuse ids from
    /// the prior session.
    pub fn max_agent_id(&self) -> Option<usize> {
        self.sub_agent_ids().into_iter().max()
    }

    /// Every sub-agent id that appears in the log, on any branch and
    /// whether its run has finished or not.
    ///
    /// A host sweeping the sub-agent boxes a backfill may have left
    /// unconcluded needs the full set, not just the runs the projection
    /// left open.
    pub fn sub_agent_ids(&self) -> BTreeSet<usize> {
        let owners = self.threads.iter().map(|thread| thread.owner);
        owners
            .filter_map(|owner| match owner {
                crate::threads::Owner::Sub(n) => Some(n),
                _ => None,
            })
            .collect()
    }

    /// The thread of every sub-agent's run on the selected branch, by
    /// sub-agent number: the run the branch shows. Another branch may hold
    /// a run of the same number, which is not it.
    ///
    /// This is the one rule for "the thread of sub-agent N". Appends
    /// to a sub-agent's thread, its turn state, its pending work and its
    /// task records all go where it says.
    fn runs(&self) -> HashMap<usize, usize> {
        let path = self.path(self.head.as_ref());
        let mut runs: HashMap<usize, usize> = HashMap::new();
        // In the order the threads started, so of two runs of one number
        // on a branch the later one is kept.
        for (index, thread) in self.threads.iter().enumerate() {
            if let crate::threads::Owner::Sub(agent) = thread.owner
                && path.holds(index, thread.tip)
            {
                runs.insert(agent, index);
            }
        }
        runs
    }

    /// The thread of sub-agent `agent_id`'s run on the selected branch
    /// (see [`Self::runs`]).
    fn run_of(&self, agent_id: usize) -> Option<usize> {
        self.runs().remove(&agent_id)
    }

    /// The sub-agents with an entry at or after `from` on the branch
    /// `path` shows.
    pub(crate) fn subs_reaching(&self, path: &crate::threads::Path, from: u64) -> Vec<usize> {
        let threads = self.threads.iter().enumerate();
        threads
            .filter_map(|(index, thread)| match thread.owner {
                crate::threads::Owner::Sub(n)
                    if thread.tip >= from && path.holds(index, thread.tip) =>
                {
                    Some(n)
                }
                _ => None,
            })
            .collect()
    }

    /// Total number of entries in the session (across all threads and
    /// branches), held or not.
    pub fn len(&self) -> usize {
        usize::try_from(self.count).expect("log length fits usize")
    }

    /// Whether the session has no entries.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Look up a held entry by id.
    pub(crate) fn get(&self, id: &EntryId) -> Option<&LogEntry> {
        let (_, entry) = self.held.get(self.positions.get(id)?)?;
        Some(entry.as_ref())
    }

    /// Returns the held entries in the order they were appended: all of
    /// them for a log that holds its whole session.
    pub fn entries_in_order(&self) -> Vec<&LogEntry> {
        self.held
            .values()
            .map(|(_, entry)| entry.as_ref())
            .collect()
    }

    /// The ids of the held entries in append order.
    #[cfg(test)]
    pub(crate) fn order(&self) -> Vec<EntryId> {
        let entries = self.held.values();
        entries.map(|(_, entry)| entry.id.clone()).collect()
    }

    /// The held entry at `index` in append order.
    #[cfg(test)]
    pub(crate) fn entry_in_append_order(&self, index: usize) -> Option<&LogEntry> {
        self.entries_in_order().get(index).copied()
    }

    /// The persisted system prompt for this session, if one was recorded
    /// at session creation. Resumed sessions created before system-prompt
    /// persistence was added will return `None`.
    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt_entry().map(|e| match &e.entry {
            LogEntryKind::SystemPrompt { text } => text.as_str(),
            // `system_prompt_entry` only returns SystemPrompt entries.
            _ => unreachable!("system_prompt_entry returned non-SystemPrompt entry"),
        })
    }

    /// The id of the persisted system-prompt entry, if any. Used as the
    /// parent for the first main-thread entry so the parent
    /// chain remains rooted.
    pub fn system_prompt_id(&self) -> Option<&EntryId> {
        self.system_prompt_entry().map(|e| &e.id)
    }

    /// The last complete environment on the active head's ancestry.
    ///
    /// An explicit empty map differs from absence. Compaction does not affect
    /// this walk. A legacy Meta creation entry supplies the baseline only when
    /// the active path contains no User EnvChange.
    pub fn session_env(&self) -> Option<&BTreeMap<String, String>> {
        self.session_env_at(self.head.as_ref())
    }

    /// The complete environment inherited at `head`, without selecting it.
    /// `None` reads only the legacy Meta baseline. This follows ancestry even
    /// across compaction, with the same absence/empty distinction as
    /// [`Self::session_env`]. An unknown head returns `None`.
    pub fn session_env_at(&self, head: Option<&EntryId>) -> Option<&BTreeMap<String, String>> {
        match head {
            Some(head) => self.session_env_from(self.where_is(head)?),
            None => self.meta_env(),
        }
    }

    /// [`Self::session_env_at`] for the entry at `at`, held or not.
    fn session_env_from(&self, at: (usize, u64)) -> Option<&BTreeMap<String, String>> {
        let on_branch = self
            .branch_at(Some(at))
            .find_map(|entry| match &entry.entry {
                LogEntryKind::EnvChange { env } if entry.thread == ThreadKind::Main => Some(env),
                _ => None,
            });
        on_branch.or_else(|| self.meta_env())
    }

    /// The legacy creation entry's environment.
    fn meta_env(&self) -> Option<&BTreeMap<String, String>> {
        self.held
            .values()
            .find_map(|(_, entry)| match &entry.entry {
                LogEntryKind::EnvChange { env } if entry.thread == ThreadKind::Meta => Some(env),
                _ => None,
            })
    }

    /// The parent id for the next append on `filter`'s thread.
    ///
    /// The main thread anchors at the explicit [`Self::head`]; a
    /// sub-agent thread anchors at the end of its run on the selected
    /// branch ([`Self::latest_leaf`]). Either falls back to the system-prompt root when
    /// the thread has no entry yet, mirroring
    /// [`AgentLog::parent_for_next_append`].
    fn parent_for_thread_append(&self, filter: ThreadFilter) -> Option<EntryId> {
        let leaf = match filter.thread {
            ThreadKind::Main => self.head.clone(),
            ThreadKind::Sub => self.latest_leaf(filter),
            // State and compaction appends only ever target the user
            // or a sub-agent thread. A `Meta` filter is a misuse: a
            // `ThreadFilter` is never constructed with `thread: Meta`
            // (see `ThreadFilter::matches`), so reaching here means a
            // caller built an invalid filter.
            ThreadKind::Meta => {
                unreachable!("state/compaction appends never target the meta thread")
            }
        };
        leaf.or_else(|| self.system_prompt_id().cloned())
    }

    /// The system-prompt entry. A log that has one has it as its first
    /// entry: append and resume both refuse it anywhere else.
    fn system_prompt_entry(&self) -> Option<&LogEntry> {
        let (_, entry) = self.held.get(&1)?;
        Some(entry.as_ref()).filter(|e| matches!(e.entry, LogEntryKind::SystemPrompt { .. }))
    }
}

impl SessionLog {
    /// Reserve a fresh session id without storing anything. The session is
    /// a draft until its first input (see the [module docs](self)).
    pub fn create(persistence: &crate::persistence::ProjectSessions) -> Result<Self, SessionError> {
        let sessions_dir = persistence.sessions_dir();
        if !sessions_dir.exists() {
            fs::create_dir_all(sessions_dir)?;
        }

        // Session id: millisecond-resolution timestamp. If a collision
        // somehow occurs within the same millisecond we retry with `_N`
        // suffixes.
        let base = Utc::now().format("%Y-%m-%d-%H-%M-%S-%3f").to_string();
        let session_id = Self::mint_session_id(sessions_dir, &base)?;
        let writer = StoreHandle::default();
        Ok(Self::open(
            persistence,
            LogSnapshot::new(session_id),
            writer,
        ))
    }

    /// A log over `core` that writes through `writer`. It holds a draft
    /// when the writer's main agent has no thread.
    fn open(
        persistence: &crate::persistence::ProjectSessions,
        core: LogSnapshot,
        mut writer: StoreHandle,
    ) -> Self {
        writer.sub_runs = core.sub_threads();
        let draft = writer.main.is_none().then(Draft::default);
        Self {
            store: Arc::new(std::sync::Mutex::new(writer)),
            core,
            path: persistence.sqlite_path(),
            project_id: persistence.project_id().to_owned(),
            draft,
            holding: false,
            write_state: WriteState::Writable,
            failure_signal: None,
            #[cfg(any(test, feature = "test-support"))]
            sqlite_commit_fault: false,
            busy_wait: crate::sqlite::BUSY_TIMEOUT,
        }
    }

    fn mint_session_id(sessions_dir: &std::path::Path, base: &str) -> Result<String, SessionError> {
        // The reservation has to be an atomic filesystem operation. A
        // plain existence check cannot see a competing `create`, because
        // `create` writes nothing: the session appears only on the first
        // entry that starts the session, so two processes minting in the same
        // millisecond would both find the id free and then share one
        // session. Claiming the id by `create_new` on its lock-file path
        // is atomic, and it composes with the later lock:
        // [`SessionLock::try_acquire`] opens that path with `create(true)`,
        // so the claim file it finds is the one this minted. An abandoned
        // claim leaves an empty lock file and nothing else. A session file
        // under the id that was never imported is a session too.
        let take = |stem: &str| -> Result<Option<String>, SessionError> {
            if crate::legacy::takes_id(sessions_dir, stem) {
                return Ok(None);
            }
            match crate::lock::claim_session_id(sessions_dir, stem) {
                Ok(()) => Ok(Some(stem.to_string())),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
                Err(err) => Err(SessionError::Io(err)),
            }
        };
        if let Some(minted) = take(base)? {
            return Ok(minted);
        }
        for n in 1..1000 {
            if let Some(minted) = take(&format!("{base}_{n}"))? {
                return Ok(minted);
            }
        }
        // 1000 collisions in one millisecond is effectively impossible in
        // a single-writer setup; surface as an IO-shaped error via the
        // existing `Io` variant rather than a bespoke one.
        Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("could not mint a unique session filename near {base}"),
        )))
    }

    /// Open an existing session for append. The caller holds the session's
    /// writer lock.
    ///
    /// A session the database does not hold yet is imported from its JSONL
    /// file first, with its label and archive sidecars, in one transaction
    /// (see [`crate::legacy::import_on_resume`]). The file and sidecars are
    /// left byte-identical.
    pub fn resume(
        persistence: &crate::persistence::ProjectSessions,
        session_id: &str,
    ) -> Result<Self, SessionError> {
        Self::resume_reporting(persistence, session_id).map(|(log, _)| log)
    }

    /// [`Self::resume`], also returning what an import this open ran did
    /// about an interrupted final entry. A later open imports nothing and
    /// reports nothing.
    pub fn resume_reporting(
        persistence: &crate::persistence::ProjectSessions,
        session_id: &str,
    ) -> Result<(Self, Option<TailRepair>), SessionError> {
        let tail_repair = if persistence.in_database(session_id)? {
            None
        } else {
            crate::legacy::import_on_resume(persistence, session_id)?
        };
        let store = persistence.sqlite_writer()?;
        let (core, main) = LogSnapshot::stored(&store, session_id)?;
        let stored = store.contains_session(session_id)?;
        let turn = match main {
            Some(thread_id) => store.turn(session_id, thread_id)?,
            None => crate::TurnState::Idle,
        };
        let writer = StoreHandle {
            store: Some(store),
            stored,
            main,
            turn,
            ..Default::default()
        };
        let mut log = Self::open(persistence, core, writer);
        if let Some(thread_id) = main {
            let mut writer = log.store.lock().expect("SQLite mutex poisoned");
            let (agent, head) = (aj_agent::events::AgentId::Main, log.core.head.as_ref());
            writer.turn = log.read_turn(&writer, agent, head, (turn, thread_id))?;
        }
        if let (false, Some(draft)) = (stored, log.draft.as_mut()) {
            // A file without entries was not imported, so its sidecars
            // still hold what the session is labelled with.
            draft.annotations = crate::legacy::annotations(persistence, session_id)?;
        }
        Ok((log, tail_repair))
    }

    /// Append one entry to the log. Returns the new entry's
    /// [`EntryRef`]: its 1-based position and its id.
    ///
    /// Message entries pass through the session storage codec before they are
    /// stored. The caller has transferred ownership, so this can compact
    /// duplicate tool-detail bodies without changing live agent messages.
    /// Other entry kinds are stored unchanged.
    ///
    /// The entry is committed before this returns, and only a committed
    /// entry enters the in-memory tree. The one exception is a draft (see
    /// the [module docs](self)): framing and settings appended there wait
    /// in it, and so does everything appended inside [`Self::in_draft`].
    /// Work appended there creates the session or branch: one transaction
    /// commits the waiting entries and the entry. Read-side queries see
    /// waiting entries like committed ones.
    ///
    /// A sub-agent's entry that continues a stored entry is not the
    /// draft's, and is committed. A draft that holds entries is created
    /// with it, so they keep the positions they were read at.
    ///
    /// The store decides which thread the entry belongs to and its
    /// position, and the log files it from that answer. Waiting
    /// entries are filed as drafts by the same rule in memory. When they
    /// are committed that filing is dropped for the store's.
    ///
    /// A database too busy to take the write refuses the append with an
    /// error [`SessionError::is_busy`] recognizes. Nothing changed, in
    /// the store or here, and the append can be made again. Any other
    /// failure of the commit fuses the log.
    ///
    /// NOTE: the persisted content is a finalized entry. A response
    /// aborted before its assistant `MessageEnd` entry is written is
    /// simply absent on resume (good), but an assistant message whose
    /// entry *was* written with incomplete content is replayed verbatim,
    /// i.e. treated as authoritative. The log can't tell a complete message
    /// from a content-truncated one once both are well-formed JSON.
    /// Two layers keep such a message out of the model's context anyway:
    /// `repair_interrupted_tool_uses` synthesizes results for dangling
    /// tool_calls on resume, and `aj_models::transform::transform_messages`
    /// drops assistant messages whose `stop_reason` is `Error`/`Aborted`
    /// before each inference. The residual hole (a content-truncated message
    /// persisted with a clean `stop_reason`) is narrow, since a truncated
    /// stream finalizes as a retryable error rather than a clean stop.
    pub fn append(
        &mut self,
        parent_id: Option<EntryId>,
        thread: ThreadKind,
        agent_id: Option<usize>,
        entry: LogEntryKind,
    ) -> Result<EntryRef, SessionError> {
        self.ensure_writable()?;
        // Cheap invariant checks. Panics here would indicate an agent-side
        // bug; prefer surfacing as errors.
        match thread {
            ThreadKind::Main if agent_id.is_some() => {
                return Err(SessionError::InvalidAppend(
                    "main-thread entry must not carry an agent_id".to_string(),
                ));
            }
            ThreadKind::Sub if agent_id.is_none() => {
                return Err(SessionError::InvalidAppend(
                    "subagent-thread entry must carry an agent_id".to_string(),
                ));
            }
            ThreadKind::Meta if agent_id.is_some() => {
                return Err(SessionError::InvalidAppend(
                    "meta entry must not carry an agent_id".to_string(),
                ));
            }
            _ => {}
        }
        if matches!(entry, LogEntryKind::SystemPrompt { .. }) {
            let valid = thread == ThreadKind::Meta
                && agent_id.is_none()
                && parent_id.is_none()
                && self.core.is_empty();
            if !valid {
                return Err(SessionError::InvalidAppend(
                    "system_prompt must be the root meta entry in an empty log".to_string(),
                ));
            }
        }
        let parent_at = match &parent_id {
            Some(parent) => Some(self.place_of(parent)?.ok_or_else(|| {
                SessionError::InvalidAppend(format!("parent entry {parent} not found in log"))
            })?),
            None => None,
        };
        if parent_id.is_none() && !self.core.is_empty() {
            return Err(SessionError::InvalidAppend(
                "log already has a root entry; additional entries must have a parent".to_string(),
            ));
        }
        if let LogEntryKind::EnvChange { env } = &entry {
            validate_session_env(env)
                .map_err(|err| SessionError::InvalidAppend(err.to_string()))?;
            let root = self.core.system_prompt_id();
            let valid = thread == ThreadKind::Main
                || (thread == ThreadKind::Meta
                    && self.core.last_seq() == 1
                    && parent_id.as_ref() == root);
            if !valid {
                return Err(SessionError::InvalidAppend(
                    "env_change must be user state or the single root-parented meta creation entry"
                        .to_string(),
                ));
            }
        }

        let mut entry = entry;
        if let LogEntryKind::Message { message } = &mut entry {
            compact_message(message);
        }

        // Adopt the message's own id as the entry id for `Message` entries.
        // This lives in `append` rather than the journal so
        // every writer, repair included, stays consistent. Non-message
        // entries and message entries with no id (deserialized outside the
        // backfill path) get a log-minted id.
        let id = match &entry {
            LogEntryKind::Message { message } if !message.id().is_empty() => {
                message.id().to_string()
            }
            _ => self.mint_id(),
        };
        // A duplicate id would silently diverge the parent chain, so we
        // error loudly rather than paper over it. For an adopted 128-bit
        // id, this append collides with probability ~M/2^128 (M = entries
        // already in the log): negligible, not impossible. `mint_id`
        // already excludes held ids. The store refuses an id the session
        // holds anywhere, so only the held ones are looked at here.
        let duplicate = || {
            SessionError::InvalidAppend(format!("duplicate entry id {id}: already present in log"))
        };
        if self.core.contains(&id) {
            return Err(duplicate());
        }
        let entry = Arc::new(LogEntry {
            id: id.clone(),
            parent_id: parent_id.clone(),
            timestamp: Some(Utc::now()),
            thread,
            agent_id,
            entry,
        });

        // Where the entry goes: into the draft, to the store with the
        // draft, or to the store alone.
        let (drafted, alone) = match &self.draft {
            None => (false, true),
            Some(draft) => {
                let on_draft = |id: &EntryId| draft.entries.iter().any(|entry| entry.id == *id);
                // A session that is not stored has nothing but its draft.
                let of_draft = thread != ThreadKind::Sub
                    || !self.is_durable()
                    || parent_id.as_ref().is_some_and(on_draft);
                let waits = of_draft && (self.holding || !entry.entry.is_work());
                (waits, !of_draft && draft.entries.is_empty())
            }
        };
        let committed = if drafted {
            let filing = self.core.place(thread, agent_id, parent_at);
            self.core.push_filed(Arc::clone(&entry), &filing);
            let draft = self.draft.as_mut().expect("an entry of the draft");
            draft.entries.push(entry);
            Ok(())
        } else if alone {
            self.commit(&entry)
        } else {
            self.create_thread(Some(&entry), None, crate::TurnState::Idle)
        };
        match committed {
            Ok(()) => {}
            // Nothing was written, so the log stays as it was.
            Err(Commit::Refused(error)) if error.is_busy() => return Err(error.into()),
            Err(Commit::Refused(SqliteError::EntryExists { .. })) => return Err(duplicate()),
            Err(Commit::Refused(error) | Commit::Unfollowed(error)) => {
                return Err(self.fuse(std::io::Error::other(error)));
            }
        }
        // Advance the explicit head on every main-thread append, once
        // the entry is committed to the in-memory maps. This single
        // point covers every main-thread writer (messages via the
        // journal, state entries, compaction, repair).
        // Sub-agent and meta appends must not touch the head.
        if thread == ThreadKind::Main {
            self.core.head = Some(id.clone());
        }
        Ok(EntryRef {
            seq: self.last_seq(),
            id,
        })
    }

    /// Commit `entry` to the stored session and file it where the store
    /// says it landed. The log holds no draft entry: one would sit at the
    /// position the store gives `entry`.
    fn commit(&mut self, entry: &Arc<LogEntry>) -> Result<(), Commit> {
        let shared = Arc::clone(&self.store);
        let mut shared = shared.lock().expect("SQLite mutex poisoned");
        let id = &self.core.session_id.clone();
        let on = shared.main;
        let store = shared.writable(id).map_err(Commit::Refused)?;
        #[cfg(any(test, feature = "test-support"))]
        if self.sqlite_commit_fault {
            store.refuse_entry_writes();
        }
        let appended = store
            .append(id, &[entry.as_ref()], Utc::now(), on)
            .map_err(Commit::Refused)?;
        let landed = appended.landed.first().expect("one entry, one landing");
        self.follow(&mut shared, entry, landed)?;
        if let Some(now) = appended.head_thread
            && shared.main.is_some_and(|main| main != now)
        {
            // An append that was not at the head started a thread
            // or continued another, and the main agent is on it now. It
            // has a turn state of its own.
            let store = shared.store.as_ref().expect("a stored session");
            shared.turn = store.turn(id, now).map_err(Commit::Unfollowed)?;
            shared.main = Some(now);
        }
        Ok(())
    }

    /// File a committed `entry` in memory where the store says it landed.
    fn follow(
        &mut self,
        shared: &mut StoreHandle,
        entry: &Arc<LogEntry>,
        landed: &crate::sqlite::Landed,
    ) -> Result<(), Commit> {
        let started = matches!(landed.landing, crate::sqlite::Landing::Starts { .. });
        // The store starts a run a host spawns as working (see
        // `SqliteStore::append`).
        if started && matches!(entry.entry, LogEntryKind::SubAgentSpawn { .. }) {
            let run = landed.thread;
            shared.sub_turns.insert(run, crate::TurnState::Working);
        }
        self.core
            .land(Arc::clone(entry), landed)
            .map_err(Commit::Unfollowed)?;
        if started {
            // The thread may be a sub-agent's run, and a new branch
            // shows the runs of its own path.
            shared.sub_runs = self.core.sub_threads();
        }
        Ok(())
    }

    /// Create what the draft is for, in one transaction: the session when
    /// the store does not hold it, the main thread, the draft's
    /// entries, then `entry`, `input` as the thread's pending work,
    /// and `turn` as the state of its turn. Then file what was committed
    /// where the store says it landed. The main agent is on the
    /// thread from then on.
    ///
    /// A refusal leaves the draft as it was.
    fn create_thread(
        &mut self,
        entry: Option<&Arc<LogEntry>>,
        input: Option<&crate::sqlite::PendingRow>,
        turn: crate::TurnState,
    ) -> Result<(), Commit> {
        let shared = Arc::clone(&self.store);
        let mut shared = shared.lock().expect("SQLite mutex poisoned");
        if shared.store.is_none() {
            let store = crate::sqlite::SqliteStore::open_project(&self.path, &self.project_id)
                .map_err(Commit::Refused)?;
            store
                .set_busy_timeout(self.busy_wait)
                .map_err(Commit::Refused)?;
            shared.store = Some(store);
        }
        let draft = self.draft.as_ref().expect("a draft to create");
        let was_stored = shared.stored;
        let store = shared.store.as_mut().expect("opened above");
        #[cfg(any(test, feature = "test-support"))]
        if self.sqlite_commit_fault {
            store.refuse_entry_writes();
        }
        let waiting = draft.entries.iter().map(Arc::as_ref);
        let entries: Vec<&LogEntry> = waiting.chain(entry.map(Arc::as_ref)).collect();
        let creation = crate::sqlite::Creation {
            session: (!was_stored).then_some(&draft.annotations),
            from: draft.from.as_deref(),
            entries: &entries,
            input,
            turn,
        };
        let id = &self.core.session_id.clone();
        let created = store
            .create_thread(id, &creation, Utc::now())
            .map_err(Commit::Refused)?;

        // The draft is committed. Memory follows the store's answer.
        shared.stored = true;
        shared.main = Some(created.thread);
        shared.turn = turn;
        let draft = self.draft.take().expect("checked above");
        self.core.drop_drafts(&draft.entries);
        let from = draft.from.unwrap_or_default();
        let threads = &mut self.core.threads;
        threads
            .create(created.thread, created.from, from)
            .map_err(Commit::Unfollowed)?;
        let committed = draft.entries.iter().chain(entry);
        for (entry, landed) in committed.zip(&created.appended.landed) {
            self.follow(&mut shared, entry, landed)?;
        }
        if let Some(now) = created.appended.head_thread
            && now != created.thread
        {
            // The draft's entries branched among themselves, and the head
            // is on the thread the last of them started.
            let store = shared.store.as_ref().expect("opened above");
            shared.turn = store.turn(id, now).map_err(Commit::Unfollowed)?;
            shared.main = Some(now);
        }
        Ok(())
    }

    /// Whether the main agent is on a draft: a session or a branch the store
    /// does not have (see the [module docs](self)).
    pub fn is_draft(&self) -> bool {
        self.draft.is_some()
    }

    /// Accept the main agent's first input for a draft: create the session
    /// or branch and save `input` as its pending input, in one transaction.
    /// `Ok` means both are stored. An error means neither is, and the
    /// draft is as it was. A database too busy to take the write reports
    /// [`SessionError::is_busy`].
    ///
    /// On a log that holds no draft the input is saved as
    /// [`crate::PendingStore::save_input`] saves it.
    pub fn accept(
        &mut self,
        kind: aj_agent::queue::PendingKind,
        input: &aj_agent::queue::PendingInput,
    ) -> Result<(), SessionError> {
        self.ensure_writable()?;
        if self.draft.is_none() {
            let main = aj_agent::events::AgentId::Main;
            return self.pending().save_input(main, kind, input);
        }
        let row = crate::pending::input_row(kind, input)?;
        self.created(|log| log.create_thread(None, Some(&row), crate::TurnState::Idle))
    }

    /// Store that `agent`'s turn starts: `Working`, on its thread
    /// (see [`crate::TurnStore`]). A turn of the main agent that starts on
    /// a draft creates the session or branch with it.
    pub fn start_turn(&mut self, agent: aj_agent::events::AgentId) -> Result<(), SessionError> {
        if agent == aj_agent::events::AgentId::Main && self.draft.is_some() {
            self.ensure_writable()?;
            return self.created(|log| log.create_thread(None, None, crate::TurnState::Working));
        }
        self.turn_state().set(agent, crate::TurnState::Working)
    }

    /// Run a creation that appends no entry of its own. One the store
    /// refused wrote nothing and left the draft as it was, so it fails the
    /// action it was for and the log stays writable, as after a refused
    /// save of pending work or of a turn state.
    fn created(
        &mut self,
        create: impl FnOnce(&mut Self) -> Result<(), Commit>,
    ) -> Result<(), SessionError> {
        match create(self) {
            Ok(()) => Ok(()),
            Err(Commit::Refused(error)) => Err(error.into()),
            Err(Commit::Unfollowed(error)) => Err(self.fuse(std::io::Error::other(error))),
        }
    }

    /// Run `write` with every entry it appends at a draft joining the
    /// draft, work included. For a head switch, whose own entries (an
    /// answer to an interrupted tool call, a paused goal) precede the
    /// branch's first input and are stored with it. Where the main agent has
    /// a thread, appends commit as always.
    pub fn in_draft<T>(
        &mut self,
        write: impl FnOnce(&mut Self) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let was = std::mem::replace(&mut self.holding, true);
        let result = write(self);
        self.holding = was;
        result
    }

    /// Position of the last entry the store holds. The entries of
    /// a draft sit after it.
    pub fn stored_seq(&self) -> u64 {
        let drafts = self.draft.as_ref().map_or(0, |draft| draft.entries.len());
        self.core.count - u64::try_from(drafts).expect("a draft is short")
    }

    /// Set how long a write of this log waits, blocking its thread, for
    /// another connection's lock on the database before it fails as busy
    /// (see [`SessionError::is_busy`]). Five seconds unless set.
    pub fn set_busy_wait(&mut self, wait: std::time::Duration) -> Result<(), SessionError> {
        if wait == self.busy_wait {
            return Ok(());
        }
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        if let Some(store) = &writer.store {
            store.set_busy_timeout(wait)?;
        }
        self.busy_wait = wait;
        Ok(())
    }

    /// Run `write` with this log's writes waiting at most `wait` for
    /// another connection's lock (see [`Self::set_busy_wait`]). For a
    /// caller that tries a busy write again and so keeps each try short.
    /// Afterwards a write waits as long as it did before.
    pub fn with_busy_wait<T>(
        &mut self,
        wait: std::time::Duration,
        write: impl FnOnce(&mut Self) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let usual = self.busy_wait;
        self.set_busy_wait(wait)?;
        let result = write(self);
        if let Err(error) = self.set_busy_wait(usual) {
            tracing::warn!("could not restore the session database's busy wait: {error}");
        }
        result
    }

    /// Hold the whole session, for a reader that needs all of its
    /// history in this log: an export, a full replay. A no-op for a log
    /// that holds it already. Blocking database I/O.
    pub fn load_all(&mut self) -> Result<(), SessionError> {
        self.core = self.whole().read()?;
        Ok(())
    }

    /// The read that yields the whole session as a snapshot, without
    /// making this log hold it. The read takes no lock on the log, so a
    /// caller can run it off the async threads.
    pub fn whole(&self) -> WholeRead {
        let source =
            (!self.core.is_complete()).then(|| (self.path.clone(), self.project_id.clone()));
        WholeRead {
            core: self.core.clone(),
            source,
        }
    }

    /// The read that makes this log hold the branch ending at `head`:
    /// its model context and its display tail. `None` when it holds the
    /// model context already, as for an earlier entry of the selected
    /// branch after its last compaction, and for an entry the session does
    /// not have.
    ///
    /// The head may only move to `head` once the read's result is passed to
    /// [`Self::install_branch`]. The read itself takes no lock on the log,
    /// so a caller can run it off the async threads, but the log must not
    /// be appended to in between.
    pub fn branch_read(&self, head: &EntryId) -> Result<Option<BranchRead>, SessionError> {
        if self.core.covers(head) {
            return Ok(None);
        }
        let Some(at) = self.place_of(head)? else {
            return Ok(None);
        };
        Ok(Some(BranchRead {
            database: self.path.clone(),
            project_id: self.project_id.clone(),
            plan: self.core.branch_plan(head, at),
        }))
    }

    /// Hold the branch a [`BranchRead`] of this log returned, in place of
    /// the messages held so far. Returns what was held, for a caller that
    /// does not move the head after all (see [`Self::restore_branch`]).
    pub fn install_branch(&mut self, branch: LoadedBranch) -> Result<LeftBranch, SessionError> {
        if branch.plan.count != self.core.last_seq() {
            return Err(SessionError::InvalidHead(format!(
                "the session was appended to while the branch ending at {} was read",
                branch.plan.head.0
            )));
        }
        let left = LeftBranch(self.core.clone());
        self.core.install(branch, self.stored_seq());
        Ok(left)
    }

    /// Go back to holding what [`Self::install_branch`] replaced. The log
    /// must not have been appended to or had its head moved since.
    pub fn restore_branch(&mut self, left: LeftBranch) {
        self.core = left.0;
    }

    /// The last entry of sub-agent `agent_id`'s run on the selected branch,
    /// for a caller that continues the run. `None` when the branch shows no
    /// run of that sub-agent.
    ///
    /// The run is held whole afterwards, so [`Self::linearize`] from the
    /// returned entry yields all of it. A reopened log holds only the runs
    /// that reach into its window, and reads any other here: blocking
    /// database I/O.
    pub fn sub_agent_run(&mut self, agent_id: usize) -> Result<Option<EntryId>, SessionError> {
        let Some(index) = self.core.run_of(agent_id) else {
            return Ok(None);
        };
        let run = self.core.threads.get(index).expect("a thread of this log");
        let (tip, tip_id) = (run.tip, run.tip_id.clone());
        if run.held != Some((0, tip)) {
            let writer = self.store.lock().expect("SQLite mutex poisoned");
            if let Some(store) = writer.store.as_ref() {
                let agent = aj_agent::events::AgentId::Sub(agent_id);
                let end = self.core.count.saturating_add(1);
                self.core.load_page(store, agent, end, 1)?;
            }
        }
        Ok(Some(tip_id))
    }

    /// Hold every message the user sent on the selected branch, for a
    /// caller that plans a compaction: it chooses the ones to keep among
    /// all of them. A reopened log holds only those its model context and
    /// display tail contain, and reads the earlier ones here: blocking
    /// database I/O. They stay held, so the compaction that names some of
    /// them can be appended.
    pub fn hold_user_messages(&mut self) -> Result<(), SessionError> {
        if self.core.is_complete() {
            return Ok(());
        }
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        if let Some(store) = writer.store.as_ref() {
            self.core.load_user_messages(store)?;
        }
        Ok(())
    }

    /// The thread and position of `id`, for any entry of the
    /// session. A log that does not hold `id` asks the store.
    fn place_of(&self, id: &EntryId) -> Result<Option<(usize, u64)>, SessionError> {
        Ok(self.find(id)?.map(|found| found.at))
    }

    /// Look up any entry of the session, held or not. A point read of the
    /// store for an entry this log does not hold.
    pub fn find(&self, id: &EntryId) -> Result<Option<Found>, SessionError> {
        if let Some(entry) = self.core.get(id) {
            let at = self.core.where_is(id).expect("a held entry has a place");
            return Ok(Some(Found {
                parent_id: entry.parent_id.clone(),
                at,
            }));
        }
        if self.core.is_complete() {
            return Ok(None);
        }
        let store = self.store.lock().expect("SQLite mutex poisoned");
        let Some(store) = store.store.as_ref() else {
            return Ok(None);
        };
        let Some(located) = store.locate(&self.core.session_id, id)? else {
            return Ok(None);
        };
        Ok(Some(Found {
            parent_id: located.parent_id,
            at: (self.core.thread_of(located.thread)?, located.seq),
        }))
    }

    /// The log's write connection, for the journal that closes it.
    pub(crate) fn writer(&self) -> SharedStore {
        Arc::clone(&self.store)
    }

    /// The session's stored turn state, on this log's connection.
    pub fn turn_state(&self) -> crate::TurnStore {
        crate::TurnStore::new(Arc::clone(&self.store), self.session_id().to_owned())
    }

    /// The store for this session's pending work, on this log's
    /// connection.
    pub fn pending(&self) -> crate::PendingStore {
        crate::PendingStore::new(Arc::clone(&self.store), self.session_id().to_owned())
    }

    /// `agent`'s saved pending work on its thread of the selected
    /// branch, for a caller that picks that thread up. Empty where
    /// it has none: for the main agent on a draft.
    pub fn restore_pending(
        &self,
        work: &crate::PendingStore,
        agent: aj_agent::events::AgentId,
    ) -> Result<Vec<crate::Pending>, SessionError> {
        match self.stored_thread(agent) {
            Some(thread_id) => work.restore(agent, thread_id),
            None => Ok(Vec::new()),
        }
    }

    /// The store for this session's task records, on this log's
    /// connection.
    pub fn task_records(&self) -> crate::TaskRecords {
        crate::TaskRecords::new(Arc::clone(&self.store), self.session_id().to_owned())
    }

    /// The stored thread of `agent` on the selected branch (see
    /// [`StoreHandle::thread`]).
    fn stored_thread(&self, agent: aj_agent::events::AgentId) -> Option<crate::sqlite::ThreadId> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        writer.thread(agent)
    }

    /// `agent`'s task records on its thread of the selected
    /// branch, chosen as [`Self::restore_pending`] chooses it.
    pub fn restore_tasks(
        &self,
        tasks: &crate::TaskRecords,
        agent: aj_agent::events::AgentId,
    ) -> Result<Vec<crate::TaskRecord>, SessionError> {
        match self.stored_thread(agent) {
            Some(thread_id) => tasks.restore(agent, thread_id),
            None => Ok(Vec::new()),
        }
    }

    /// The sub-agents with a run on the selected branch whose thread
    /// holds something an earlier process left: a task record, saved
    /// pending work, or a turn stored as working. In ascending order.
    pub fn sub_agents_with_work(&self) -> Result<Vec<LeftSubAgent>, SessionError> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        let Some(store) = writer.store.as_ref() else {
            return Ok(Vec::new());
        };
        let path = self.core.path(self.core.head.as_ref());
        let threads = &self.core.threads;
        let mut found = BTreeMap::new();
        for (thread_id, working) in store.sub_threads_with_work(&self.core.session_id)? {
            // A thread of another branch, or one that started after
            // the threads were read, is not this branch's to pick up.
            let Some(index) = threads.thread_of(thread_id) else {
                continue;
            };
            let run = threads.get(index).expect("a thread of this log");
            if let crate::threads::Owner::Sub(agent_id) = run.owner
                && path.holds(index, run.tip)
                && self.core.run_of(agent_id) == Some(index)
            {
                found.insert(agent_id, LeftSubAgent { agent_id, working });
            }
        }
        Ok(found.into_values().collect())
    }

    /// `agent`'s thread on the selected branch, read whole: the
    /// branch for the main agent, its run for a sub-agent (see
    /// [`Self::sub_agent_run`]). `None` where it has none.
    pub fn agent_branch(
        &mut self,
        agent: aj_agent::events::AgentId,
    ) -> Result<Option<BranchView>, SessionError> {
        let end = match agent {
            aj_agent::events::AgentId::Main => self.head().cloned(),
            aj_agent::events::AgentId::Sub(n) => self.sub_agent_run(n)?,
        };
        Ok(end.map(|end| self.linearize(&end, ThreadFilter::of(agent))))
    }

    /// The session's own storage fingerprint, not the shared database's size
    /// or modification time. Other sessions cannot change this answer.
    /// `None` for a session that is not stored.
    pub fn storage_metadata(&self) -> Result<Option<crate::SessionMetadata>, SessionError> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        let Some(store) = writer.store.as_ref().filter(|_| writer.stored) else {
            return Ok(None);
        };
        Ok(store
            .session_metadata(self.session_id())?
            .map(|m| crate::SessionMetadata::new(m.id, m.updated_at, m.last_seq)))
    }

    /// The summed length of the session's stored entries, in bytes. `None`
    /// for a session that is not stored. Reads the session's row headers,
    /// so it is for a caller that shows the size, not for one that polls.
    pub fn stored_bytes(&self) -> Result<Option<u64>, SessionError> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        let Some(store) = writer.store.as_ref().filter(|_| writer.stored) else {
            return Ok(None);
        };
        Ok(Some(store.entry_bytes(self.session_id())?))
    }

    /// Set or clear the session's label, which is expected to have been
    /// through [`crate::tag::normalize_tag`]. The caller holds the session's
    /// writer lock.
    ///
    /// A stored session's label is committed before this returns. One that
    /// is not stored yet keeps it in its draft, which is stored with the
    /// session's first input, so an abandoned session leaves nothing behind.
    pub fn set_tag(&mut self, tag: Option<&str>) -> Result<(), SessionError> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        if writer.closed {
            return Err(SqliteError::Closed.into());
        }
        if let Some(store) = writer.store.as_ref().filter(|_| writer.stored) {
            store.set_tag(&self.core.session_id, tag)?;
        } else {
            crate::sqlite::validate_tag(tag)?;
            let draft = self
                .draft
                .as_mut()
                .expect("a session not stored is a draft");
            draft.annotations.tag = tag.map(str::to_owned);
        }
        Ok(())
    }

    /// Set or clear the session's archive marker. Stored like
    /// [`Self::set_tag`] stores a label.
    pub fn set_archived(&mut self, archived: bool) -> Result<(), SessionError> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        if writer.closed {
            return Err(SqliteError::Closed.into());
        }
        if let Some(store) = writer.store.as_ref().filter(|_| writer.stored) {
            store.set_archived(&self.core.session_id, archived)?;
        } else {
            let draft = self
                .draft
                .as_mut()
                .expect("a session not stored is a draft");
            draft.annotations.archived = archived;
        }
        Ok(())
    }

    fn ensure_writable(&self) -> Result<(), SessionError> {
        match &self.write_state {
            WriteState::Writable => Ok(()),
            WriteState::WriteFailed(failure) => Err(SessionError::WriteFailed(failure.clone())),
        }
    }

    fn fuse(&mut self, error: std::io::Error) -> SessionError {
        let failure = PersistenceFailure::new(error, self.is_durable());
        self.write_state = WriteState::WriteFailed(failure.clone());
        if let Some(signal) = self.failure_signal.take() {
            let _ = signal.send(failure.clone());
        }
        SessionError::WriteFailed(failure)
    }

    /// Mint a fresh entry id: a random 128-bit value as 32 hex digits,
    /// re-drawn until it doesn't collide with an id already in this log.
    ///
    /// Ids are random rather than a per-process counter so two processes
    /// appending to the same session (resumed in two
    /// terminals) practically can't mint the same id and corrupt the
    /// parent chain. The `contains_key` check rules out a collision with
    /// ids this process already holds, so it fully guards the
    /// within-process draw. Two concurrent processes don't see each
    /// other's fresh ids, so a cross-process collision is possible at
    /// ~1/2^128 per overlapping mint, which we accept over taking a lock.
    ///
    /// The 32-hex format matches the ids `AgentMessage` mints for itself,
    /// so all ids written by current code are uniform. Old 8-hex ids
    /// coexist in the same session: parent links copy strings verbatim.
    fn mint_id(&self) -> EntryId {
        loop {
            let id = format!("{:032x}", rand::random::<u128>());
            if !self.core.contains(&id) {
                return id;
            }
        }
    }

    /// Move the main-thread head to `id`, the anchor for the next
    /// main-thread append. Used to start a sibling branch at an earlier
    /// point, or to switch the active branch. Nothing is stored either
    /// way, so a later process does not know of the switch unless
    /// something is done after it.
    ///
    /// A switch to the last entry of a stored thread puts the main
    /// agent on that thread. Its turn state, pending work and
    /// task records are that thread's from then on, and a
    /// sub-agent's are those of its run on the branch switched to. No turn
    /// may be running: one the thread stores as working is taken for
    /// abandoned, and reads as interrupted or is stored idle.
    ///
    /// A switch to any other entry starts the draft of a branch that
    /// continues from it (see the [module docs](self)). The main agent has
    /// no thread until the branch's first input creates it.
    ///
    /// A draft the log held is left behind and forgotten. The draft of a
    /// session that is not stored is the session itself and is kept: the
    /// head moves among its entries.
    ///
    /// `id` must exist and be either a main-thread entry or the
    /// system-prompt meta entry. A sub-agent entry is rejected:
    /// anchoring the main-thread head there would splice the main
    /// thread onto a sub thread, which [`Self::append`]'s own
    /// checks would not catch (they validate the new entry's thread,
    /// not the parent's). A non-system-prompt meta entry and a missing
    /// id are rejected for the same "the head must be a real branch
    /// point" reason. Rejections return
    /// [`SessionError::InvalidHead`], whose message is fit to
    /// surface to the user.
    ///
    /// A branch whose model context this log does not hold is refused
    /// too: the model would be handed a partial history. Read it first (see
    /// [`Self::branch_read`]).
    pub fn set_head(&mut self, id: EntryId) -> Result<(), SessionError> {
        self.ensure_writable()?;
        self.validate_head(&id)?;
        if !self.core.covers(&id) {
            return Err(SessionError::InvalidHead(format!(
                "the branch ending at {id} is not loaded"
            )));
        }
        let mut writer = self.store.lock().expect("SQLite mutex poisoned");
        if !writer.stored {
            self.core.head = Some(id);
            return Ok(());
        }
        let ends = self.core.threads.tip(&id);
        let thread_id = ends.and_then(|(thread, _)| self.core.threads.get(thread)?.id);
        // No turn has started on a draft.
        let mut turn = crate::TurnState::Idle;
        if let Some(thread_id) = thread_id {
            let store = writer.store.as_ref().expect("a stored thread");
            let stored = store.turn(&self.core.session_id, thread_id)?;
            let agent = aj_agent::events::AgentId::Main;
            turn = self.read_turn(&writer, agent, Some(&id), (stored, thread_id))?;
        }
        // A draft the log held is left behind, unless the head only moves
        // among its entries.
        if let Some(left) = self.draft.take() {
            if left.entries.iter().any(|entry| entry.id == id) {
                // The head moves inside the draft, which stays.
                self.draft = Some(left);
                self.core.head = Some(id);
                return Ok(());
            }
            self.core.drop_drafts(&left.entries);
        }
        self.draft = thread_id.is_none().then(|| Draft {
            from: Some(id.clone()),
            ..Default::default()
        });
        writer.main = thread_id;
        writer.turn = turn;
        self.core.head = Some(id);
        // The sub-agents' runs are the new branch's.
        writer.sub_runs = self.core.sub_threads();
        Ok(())
    }

    /// What the `stored` state of `agent`'s turn means for this log, which
    /// has come to be on its thread, ending at `end`, and is not
    /// running the turn. `end` is `None` for a thread without an
    /// entry on its branch.
    ///
    /// A turn stored as working was abandoned by its process. It reads as
    /// [`crate::TurnState::Interrupted`] when the thread leaves input the
    /// model never answered: its last message is not a completed assistant
    /// message, or a tool call has no result, which a repair answers.
    /// Otherwise the process ended between the answer and the mark, and the
    /// turn is stored idle here.
    fn read_turn(
        &self,
        writer: &StoreHandle,
        agent: aj_agent::events::AgentId,
        end: Option<&EntryId>,
        stored: (crate::TurnState, crate::sqlite::ThreadId),
    ) -> Result<crate::TurnState, SqliteError> {
        let (crate::TurnState::Working, thread_id) = stored else {
            return Ok(stored.0);
        };
        let branch = match end {
            Some(end) => self.core.linearize(end, ThreadFilter::of(agent)),
            None => BranchView::from_entries(self.core.session_id.clone(), Vec::new()),
        };
        if crate::repair::turn_unanswered(&branch)
            || !crate::repair::unanswered_tool_calls(&branch).is_empty()
        {
            return Ok(crate::TurnState::Interrupted);
        }
        let session = self.core.session_id.as_str();
        let store = writer.writable(session)?;
        store.set_turn(session, thread_id, crate::TurnState::Idle)?;
        Ok(crate::TurnState::Idle)
    }

    /// Read the turn state an earlier process stored for sub-agent
    /// `agent_id` on its run of the selected branch, for a caller that
    /// picks the run up and is not running the turn. It is read as the
    /// main agent's is when a log comes to be on its thread: a turn
    /// left working reads as interrupted or is stored idle. From then on
    /// [`crate::TurnStore`] answers with it. Idle where the branch shows no
    /// run of that sub-agent.
    pub fn pick_up_turn(&mut self, agent_id: usize) -> Result<crate::TurnState, SessionError> {
        let agent = aj_agent::events::AgentId::Sub(agent_id);
        let Some(end) = self.sub_agent_run(agent_id)? else {
            return Ok(crate::TurnState::Idle);
        };
        let mut writer = self.store.lock().expect("SQLite mutex poisoned");
        let (Some(thread_id), Some(store)) = (writer.thread(agent), writer.store.as_ref()) else {
            return Ok(crate::TurnState::Idle);
        };
        let stored = store.turn(&self.core.session_id, thread_id)?;
        let turn = self.read_turn(&writer, agent, Some(&end), (stored, thread_id))?;
        writer.sub_turns.insert(thread_id, turn);
        Ok(turn)
    }

    /// Validate the target of [`Self::set_head`] without selecting it or
    /// requiring a writable log. Preview and mutation share the same rules
    /// for which entries can anchor the main thread.
    pub fn validate_head(&self, id: &EntryId) -> Result<(), SessionError> {
        let (thread, _) = self.place_of(id)?.ok_or_else(|| {
            SessionError::InvalidHead(format!("entry {id} is not in this session's log"))
        })?;
        let owner = self.core.threads.get(thread).map(|thread| thread.owner);
        // A meta entry is state, which is always held. The system prompt
        // is the only one a branch can start at.
        let meta = self
            .core
            .get(id)
            .is_some_and(|entry| entry.thread == ThreadKind::Meta);
        let valid = owner == Some(crate::threads::Owner::Main)
            && (!meta || self.core.system_prompt_id() == Some(id));
        if !valid {
            return Err(SessionError::InvalidHead(format!(
                "entry {id} is not a main-thread or system-prompt entry"
            )));
        }
        Ok(())
    }

    /// The first persistence error that fused this live object, if any.
    pub fn write_failure(&self) -> Option<&PersistenceFailure> {
        match &self.write_state {
            WriteState::Writable => None,
            WriteState::WriteFailed(failure) => Some(failure),
        }
    }

    /// Install the hosted driver's one-shot failure sender before the first
    /// live event. A construction-time failure is reported immediately.
    pub fn install_failure_signal(&mut self, signal: PersistenceFailureSender) {
        match self.write_failure().cloned() {
            Some(failure) => {
                let _ = signal.send(failure);
            }
            None => self.failure_signal = Some(signal),
        }
    }

    /// A cloneable image of the entry tree, for reads that should not
    /// hold the log lock (see [`LogSnapshot`]).
    pub fn snapshot(&self) -> LogSnapshot {
        self.core.clone()
    }

    /// Recorded main-thread settings at `head`, for any entry of the
    /// session, without selecting it (see [`LogSnapshot::settings_at`]).
    /// For a branch this log does not hold they are read from the store:
    /// blocking database I/O.
    pub fn settings_at(&self, head: &EntryId) -> Result<SessionSettings, SessionError> {
        let state = self.state_of(head)?;
        Ok(state
            .map(|(state, at)| state.settings_from(at))
            .unwrap_or_default())
    }

    /// The goal at `head`, for any entry of the session, without selecting
    /// it (see [`LogSnapshot::goal_at`]). For a branch this log does not
    /// hold it is read from the store: blocking database I/O.
    pub fn goal_at(&self, head: &EntryId) -> Result<Option<Goal>, SessionError> {
        let state = self.state_of(head)?;
        Ok(state.and_then(|(state, at)| state.goal_from(at)))
    }

    /// A tree that holds what settings, the goal and the environment at
    /// `head` are read from, with where `head` sits: this log's own for
    /// an entry it holds, and otherwise one read from the store that holds
    /// nothing else. `None` for an entry the session does not have.
    fn state_of(
        &self,
        head: &EntryId,
    ) -> Result<Option<(std::borrow::Cow<'_, LogSnapshot>, (usize, u64))>, SessionError> {
        use std::borrow::Cow;
        // An entry held in the window is on the held branch or was
        // appended to it, so everything its state is read from is held:
        // the state entries before it and the last response before the
        // window. Of an entry before the window the log may hold neither.
        let held = self.core.positions.get(head);
        if held.is_some_and(|seq| *seq >= self.core.window.from) {
            let at = self.core.where_is(head).expect("a held entry has a place");
            return Ok(Some((Cow::Borrowed(&self.core), at)));
        }
        let Some(at) = self.place_of(head)? else {
            return Ok(None);
        };
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        let Some(store) = writer.store.as_ref() else {
            return Ok(None);
        };
        let session = &self.core.session_id;
        let threads = &self.core.threads;
        let mut branch = LogSnapshot::new(session.clone());
        branch.threads = threads.clone();
        let ancestry = threads.ancestry(Some(at));
        for (seq, (thread, entry)) in branch_state(store, threads, &ancestry, self.core.count)? {
            branch.hold(seq, thread, entry);
        }
        // The main agent's last response names the model in use.
        let response = last_response(store, threads, &ancestry, at.1.saturating_add(1))?;
        for (seq, thread, entry) in read_entries(store, session, response.into_iter().collect())? {
            branch.hold(seq, thread, entry);
        }
        Ok(Some((Cow::Owned(branch), at)))
    }

    /// The entry tree behind this log, for in-crate readers that already
    /// hold the log.
    pub(crate) fn core(&self) -> &LogSnapshot {
        &self.core
    }

    /// The shared session database, not a per-session file. Use
    /// [`Self::storage_metadata`] for session size/time.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The project the session is stored under.
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Whether the store holds the session yet.
    ///
    /// A created log is stored with its first input (see the
    /// [module docs](self)). Until then its
    /// entries live only here and the store does not know the session id.
    /// A caller that would hand the session back to the store, rather than
    /// keep it in memory, has to ask: there would be nothing to hand.
    pub fn is_durable(&self) -> bool {
        self.store.lock().expect("SQLite mutex poisoned").stored
    }

    /// The id under which this log is listed by `aj list-sessions`.
    pub fn session_id(&self) -> &str {
        self.core.session_id()
    }

    /// See [`LogSnapshot::linearize`].
    pub fn linearize(&self, head: &EntryId, filter: ThreadFilter) -> BranchView {
        self.core.linearize(head, filter)
    }

    /// See [`LogSnapshot::latest_leaf`].
    pub fn latest_leaf(&self, filter: ThreadFilter) -> Option<EntryId> {
        self.core.latest_leaf(filter)
    }

    /// See [`LogSnapshot::head`].
    pub fn head(&self) -> Option<&EntryId> {
        self.core.head()
    }

    /// See [`LogSnapshot::session_env`].
    pub fn session_env(&self) -> Option<&BTreeMap<String, String>> {
        self.core.session_env()
    }

    /// The environment inherited at `head`, for any entry of the session
    /// (see [`LogSnapshot::session_env_at`]). For a branch this log does
    /// not hold it is read from the store: blocking database I/O. An entry
    /// the session does not have yields `None`.
    pub fn session_env_at(
        &self,
        head: Option<&EntryId>,
    ) -> Result<Option<BTreeMap<String, String>>, SessionError> {
        let Some(head) = head else {
            return Ok(self.core.session_env_at(None).cloned());
        };
        let state = self.state_of(head)?;
        Ok(state.and_then(|(state, at)| state.session_env_from(at).cloned()))
    }

    /// See [`LogSnapshot::len`].
    pub fn len(&self) -> usize {
        self.core.len()
    }

    /// See [`LogSnapshot::last_seq`].
    pub fn last_seq(&self) -> u64 {
        self.core.last_seq()
    }

    /// See [`LogSnapshot::contains`].
    pub fn contains(&self, id: &EntryId) -> bool {
        self.core.contains(id)
    }

    /// See [`LogSnapshot::parent_of`].
    pub fn parent_of(&self, id: &EntryId) -> Option<&EntryId> {
        self.core.parent_of(id)
    }

    pub fn is_empty(&self) -> bool {
        self.core.is_empty()
    }

    /// See [`LogSnapshot::max_agent_id`].
    pub fn max_agent_id(&self) -> Option<usize> {
        self.core.max_agent_id()
    }

    /// See [`LogSnapshot::sub_agent_ids`].
    pub fn sub_agent_ids(&self) -> BTreeSet<usize> {
        self.core.sub_agent_ids()
    }

    /// See [`LogSnapshot::entries_in_order`].
    pub fn entries_in_order(&self) -> Vec<&LogEntry> {
        self.core.entries_in_order()
    }

    /// See [`LogSnapshot::system_prompt`].
    pub fn system_prompt(&self) -> Option<&str> {
        self.core.system_prompt()
    }

    /// See [`LogSnapshot::system_prompt_id`].
    pub fn system_prompt_id(&self) -> Option<&EntryId> {
        self.core.system_prompt_id()
    }

    /// Record the assembled system prompt as the root [ThreadKind::Meta]
    /// entry of this log. May only be called on an empty log; once the
    /// session has any other entries the system prompt is fixed for its
    /// lifetime. Returns the id of the new entry.
    ///
    /// The system prompt waits in the session's draft (see
    /// [`LogEntryKind::is_work`]): it is stored with the session's
    /// first input.
    pub fn set_system_prompt(&mut self, text: String) -> Result<EntryRef, SessionError> {
        self.ensure_writable()?;
        if !self.core.is_empty() {
            return Err(SessionError::InvalidAppend(
                "system prompt can only be set on an empty log".to_string(),
            ));
        }
        self.append(
            None,
            ThreadKind::Meta,
            None,
            LogEntryKind::SystemPrompt { text },
        )
    }

    /// Record a model change on the thread selected by `filter`. See
    /// [`Self::append_state_entry`] for anchoring and durability.
    pub fn append_model_change(
        &mut self,
        filter: ThreadFilter,
        provider: &str,
        model_id: &str,
        context_window: u64,
    ) -> Result<EntryRef, SessionError> {
        self.append_state_entry(
            filter,
            LogEntryKind::ModelChange {
                provider: provider.to_string(),
                model_id: model_id.to_string(),
                context_window,
            },
        )
    }

    /// Record an Oracle model choice on the active user branch.
    /// See [`Self::append_state_entry`] for anchoring and durability.
    pub fn append_oracle_model_change(
        &mut self,
        provider: &str,
        model_id: &str,
    ) -> Result<EntryRef, SessionError> {
        self.append_state_entry(
            ThreadFilter::MAIN,
            LogEntryKind::OracleModelChange {
                provider: provider.to_string(),
                model_id: model_id.to_string(),
            },
        )
    }

    /// Record Oracle effort on the active user branch. See [`Self::append_state_entry`].
    pub fn append_oracle_thinking_change(&mut self, level: &str) -> Result<EntryRef, SessionError> {
        self.append_state_entry(
            ThreadFilter::MAIN,
            LogEntryKind::OracleThinkingChange {
                level: level.to_string(),
            },
        )
    }

    /// Record Oracle speed on the active user branch. See [`Self::append_state_entry`].
    pub fn append_oracle_speed_change(&mut self, speed: &str) -> Result<EntryRef, SessionError> {
        self.append_state_entry(
            ThreadFilter::MAIN,
            LogEntryKind::OracleSpeedChange {
                speed: speed.to_string(),
            },
        )
    }

    /// Record Oracle verbosity on the active user branch. See [`Self::append_state_entry`].
    pub fn append_oracle_verbosity_change(
        &mut self,
        verbosity: &str,
    ) -> Result<EntryRef, SessionError> {
        self.append_state_entry(
            ThreadFilter::MAIN,
            LogEntryKind::OracleVerbosityChange {
                verbosity: verbosity.to_string(),
            },
        )
    }

    /// Record a session-wide account choice on the active user branch.
    ///
    /// `None` removes the provider's pin (Provider default), `Some("")` pins the
    /// unnamed account, and `Some(label)` pins that exact label.
    pub fn append_account_change(
        &mut self,
        provider: &str,
        account: Option<&str>,
    ) -> Result<EntryRef, SessionError> {
        self.append_state_entry(
            ThreadFilter::MAIN,
            LogEntryKind::AccountChange {
                provider: provider.to_string(),
                account: account.map(str::to_string),
            },
        )
    }

    /// Record a thinking-effort change on the thread selected by
    /// `filter`. See [`Self::append_state_entry`].
    pub fn append_thinking_change(
        &mut self,
        filter: ThreadFilter,
        level: &str,
    ) -> Result<EntryRef, SessionError> {
        self.append_state_entry(
            filter,
            LogEntryKind::ThinkingChange {
                level: level.to_string(),
            },
        )
    }

    /// Record a speed change on the thread selected by `filter`. See
    /// [`Self::append_state_entry`].
    pub fn append_speed_change(
        &mut self,
        filter: ThreadFilter,
        speed: &str,
    ) -> Result<EntryRef, SessionError> {
        self.append_state_entry(
            filter,
            LogEntryKind::SpeedChange {
                speed: speed.to_string(),
            },
        )
    }

    /// Record an output-verbosity change on the thread selected by
    /// `filter`. See [`Self::append_state_entry`].
    pub fn append_verbosity_change(
        &mut self,
        filter: ThreadFilter,
        verbosity: &str,
    ) -> Result<EntryRef, SessionError> {
        self.append_state_entry(
            filter,
            LogEntryKind::VerbosityChange {
                verbosity: verbosity.to_string(),
            },
        )
    }

    /// Replace the complete environment on the active user branch.
    ///
    /// Repeated maps and empty maps are valid.
    pub fn append_env_change(
        &mut self,
        env: BTreeMap<String, String>,
    ) -> Result<EntryRef, SessionError> {
        self.append_state_entry(ThreadFilter::MAIN, LogEntryKind::EnvChange { env })
    }

    /// Replace or clear the goal on the active user branch.
    /// A goal is a submitted task, so this materializes the log even when no
    /// model messages have been recorded.
    pub fn append_goal_change(&mut self, goal: Option<Goal>) -> Result<EntryRef, SessionError> {
        self.append_state_entry(ThreadFilter::MAIN, LogEntryKind::GoalChange { goal })
    }

    /// Record what the session's system prompt was assembled from, beside the
    /// prompt itself (see [`LogEntryKind::Context`]).
    pub fn append_context(&mut self, context: SessionContext) -> Result<EntryRef, SessionError> {
        let parent = self.core.system_prompt_id().cloned();
        self.append(
            parent,
            ThreadKind::Meta,
            None,
            LogEntryKind::Context { context },
        )
    }

    /// Record a compaction checkpoint on `filter`'s thread, anchored at
    /// the thread's current leaf. `first_kept_entry_id`
    /// must be an existing entry in the log. Retained users must be distinct
    /// original user messages on this thread's ancestry before that cut.
    /// The log must hold them, which a reopened one does after
    /// [`Self::hold_user_messages`].
    #[allow(clippy::too_many_arguments)]
    pub fn append_compaction(
        &mut self,
        filter: ThreadFilter,
        summary: String,
        first_kept_entry_id: EntryId,
        retained_user_entry_ids: Vec<EntryId>,
        tokens_before: u64,
        details: Option<crate::compaction::CompactionDetails>,
        usage: aj_models::types::Usage,
    ) -> Result<EntryRef, SessionError> {
        self.ensure_writable()?;
        if !self.core.contains(&first_kept_entry_id) {
            return Err(SessionError::InvalidAppend(format!(
                "compaction first_kept_entry_id {first_kept_entry_id} not found in log"
            )));
        }
        let parent = self.core.parent_for_thread_append(filter);
        if !retained_user_entry_ids.is_empty() {
            let valid = parent.as_ref().is_some_and(|head| {
                let branch = self.core.linearize(head, filter);
                branch
                    .entries()
                    .iter()
                    .position(|e| e.id == first_kept_entry_id)
                    .is_some_and(|cut| {
                        retained_user_messages(&branch.entries()[..cut], &retained_user_entry_ids)
                            .len()
                            == retained_user_entry_ids.len()
                    })
            });
            if !valid {
                return Err(SessionError::InvalidAppend(
                    "retained compaction users must be distinct user messages before the cut on this ancestry".into(),
                ));
            }
        }
        self.append(
            parent,
            filter.thread,
            filter.agent_id,
            LogEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                retained_user_entry_ids,
                tokens_before,
                details,
                usage,
            },
        )
    }

    /// Append a transcript-only compaction failure on the agent's thread.
    /// The error is committed as it is appended.
    pub(crate) fn append_compaction_failed(
        &mut self,
        filter: ThreadFilter,
        reason: CompactionReason,
        tokens_before: u64,
        error: String,
    ) -> Result<EntryRef, SessionError> {
        let parent = self.core.parent_for_thread_append(filter);
        self.append(
            parent,
            filter.thread,
            filter.agent_id,
            LogEntryKind::CompactionFailed {
                reason,
                tokens_before,
                error,
            },
        )
    }

    /// Seed sub-agent `agent_id`'s thread with its
    /// [`LogEntryKind::SubAgentSpawn`] root, anchored at
    /// `parent_head` (the parent thread's head at spawn time — the
    /// assistant message carrying the spawning tool call). After this
    /// the sub thread has a leaf, so its messages chain via
    /// [`Self::latest_leaf`].
    pub fn append_subagent_spawn(
        &mut self,
        agent_id: usize,
        parent_head: EntryId,
        task: &str,
        tool_name: &str,
        call_id: Option<&str>,
        background: bool,
        settings: &AgentSettings,
    ) -> Result<EntryRef, SessionError> {
        let mut settings = settings.clone();
        // Thinking display is session-live state. It follows the creator on
        // creation and reseeds from config on resume, so it must not become
        // part of a persisted sub-agent snapshot either.
        settings.thinking_display.clear();
        self.append(
            Some(parent_head),
            ThreadKind::Sub,
            Some(agent_id),
            LogEntryKind::SubAgentSpawn {
                task: task.to_string(),
                tool_name: tool_name.to_string(),
                call_id: call_id.map(str::to_owned),
                background,
                settings,
            },
        )
    }

    /// Record how sub-agent `agent_id`'s run on the selected branch ended,
    /// at the end of its thread. `None` when the branch shows no such run.
    pub fn append_subagent_end(
        &mut self,
        agent_id: usize,
        conclusion: aj_agent::events::SubAgentConclusion,
        report: &str,
    ) -> Result<Option<EntryRef>, SessionError> {
        let Some(leaf) = self.latest_leaf(ThreadFilter::sub(agent_id)) else {
            return Ok(None);
        };
        let end = LogEntryKind::SubAgentEnd {
            conclusion,
            report: report.to_owned(),
        };
        self.append(Some(leaf), ThreadKind::Sub, Some(agent_id), end)
            .map(Some)
    }

    /// Append a state entry on `filter`'s thread, anchored at the
    /// thread's current head and falling back to the system-prompt
    /// root when the thread is empty.
    fn append_state_entry(
        &mut self,
        filter: ThreadFilter,
        entry: LogEntryKind,
    ) -> Result<EntryRef, SessionError> {
        let parent = self.core.parent_for_thread_append(filter);
        self.append(parent, filter.thread, filter.agent_id, entry)
    }
}

/// A mutation handle into a [SessionLog] that tracks where the next
/// append attaches (`head`) and which thread it belongs to.
///
/// Crate-internal: the append API is an implementation detail behind
/// [`crate::Journal`] and [`crate::repair_interrupted_tool_uses`],
/// not a public surface. Each `add_*` method appends one entry before
/// advancing the head (see [`SessionLog::append`] for when an
/// entry is committed).
pub(crate) struct AgentLog<'a> {
    log: &'a mut SessionLog,
    head: Option<EntryId>,
    thread: ThreadKind,
    agent_id: Option<usize>,
}

impl<'a> AgentLog<'a> {
    /// Build a new main-thread view seeded from the log's explicit
    /// head (see [`SessionLog::head`]). On a fresh log the head is
    /// `None` and the first append creates the root (or anchors at the
    /// system-prompt entry); on a resumed or in-progress log it is the
    /// active branch's tip. Every main-thread append advances both this
    /// view's cached head and [`SessionLog::head`] to the new
    /// entry, so they stay consistent.
    pub(crate) fn main(log: &'a mut SessionLog) -> Self {
        let head = log.head().cloned();
        Self {
            log,
            head,
            thread: ThreadKind::Main,
            agent_id: None,
        }
    }

    /// Build a new subagent-thread view whose next append will attach to
    /// `parent_head`. When starting a fresh subagent subtree this is the
    /// main-thread assistant message carrying the spawning `tool_use`;
    /// once inside the subtree it's the latest entry of that subagent's
    /// own thread. `parent_head` must be an existing entry in the log.
    pub(crate) fn sub(log: &'a mut SessionLog, parent_head: EntryId, agent_id: usize) -> Self {
        Self {
            log,
            head: Some(parent_head),
            thread: ThreadKind::Sub,
            agent_id: Some(agent_id),
        }
    }

    /// The view that appends to `agent`'s thread at its end: for a
    /// sub-agent, the end of its run on the selected branch.
    ///
    /// The main thread may be empty: a fresh log holds only the
    /// system-prompt root, which [`Self::main`] anchors at. A sub-agent's
    /// thread never is once its `SubAgentSpawn` entry seeded it, and one
    /// the branch shows no run of is refused.
    pub(crate) fn of(
        log: &'a mut SessionLog,
        agent: aj_agent::events::AgentId,
    ) -> Result<Self, SessionError> {
        Ok(match agent {
            aj_agent::events::AgentId::Main => Self::main(log),
            aj_agent::events::AgentId::Sub(n) => {
                let head = log.latest_leaf(ThreadFilter::sub(n)).ok_or_else(|| {
                    SessionError::InvalidAppend(format!(
                        "sub-agent {n} thread has no head entry; was SubAgentStart emitted?"
                    ))
                })?;
                Self::sub(log, head, n)
            }
        })
    }

    /// Current head -- the id that will become `parent_id` on the next
    /// append, or `None` if the log is still empty.
    #[cfg(test)]
    pub(crate) fn head(&self) -> Option<&EntryId> {
        self.head.as_ref()
    }

    /// Append a wire-level message to this thread. The log compacts duplicate
    /// text details in the owned message, then commits it before advancing
    /// the head.
    pub(crate) fn add_message(&mut self, message: AgentMessage) -> Result<EntryRef, SessionError> {
        let entry = LogEntryKind::Message { message };
        let parent = self.parent_for_next_append();
        let appended = self.log.append(parent, self.thread, self.agent_id, entry)?;
        self.head = Some(appended.id.clone());
        Ok(appended)
    }

    /// Commit one accepted execution's writes with the same per-entry durability
    /// and thread ownership as a message, without adding model context.
    pub(crate) fn add_code_mode_store(
        &mut self,
        writes: BTreeMap<String, serde_json::Value>,
    ) -> Result<EntryRef, SessionError> {
        let parent = self.parent_for_next_append();
        let appended = self.log.append(
            parent,
            self.thread,
            self.agent_id,
            LogEntryKind::CodeModeStore { writes },
        )?;
        self.head = Some(appended.id.clone());
        Ok(appended)
    }

    /// Determine the `parent_id` for the next append. Normally this is
    /// just the current `head`, but when a thread is being started for
    /// the first time on a log that already has a system-prompt root,
    /// we anchor to that root so the parent chain stays connected.
    fn parent_for_next_append(&self) -> Option<EntryId> {
        self.head
            .clone()
            .or_else(|| self.log.system_prompt_id().cloned())
    }
}

/// Deterministic commit faults for composed persistence tests.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;

    /// Make every later SQLite commit of this log fail inside its
    /// transaction, which then rolls back, as a full disk would.
    pub fn fail_sqlite_commits(log: &mut SessionLog) {
        log.sqlite_commit_fault = true;
    }

    /// A session that `store` holds only as a legacy JSONL file, not yet
    /// imported: the entries `build` appends, one serialized entry per
    /// line, which is what the JSONL backend wrote. Returns its id and the
    /// file's path.
    pub fn jsonl_session(
        store: &crate::ProjectSessions,
        build: impl FnOnce(&mut SessionLog),
    ) -> (String, PathBuf) {
        // Built in a database of its own. Sharing the sessions directory
        // keeps the ids of two such sessions apart.
        let scratch = tempfile::tempdir().expect("scratch directory");
        let source = crate::ProjectSessions::for_project(
            scratch.path().join("sessions.sqlite3"),
            store.sessions_dir().to_owned(),
            "scratch".into(),
        );
        let mut log = SessionLog::create(&source).expect("create the source log");
        build(&mut log);
        let lines: String = log
            .entries_in_order()
            .iter()
            .map(|entry| serde_json::to_string(entry).expect("serialize a record") + "\n")
            .collect();
        let path = crate::legacy::session_path(store, log.session_id());
        fs::write(&path, lines).expect("write the JSONL file");
        (log.session_id().to_owned(), path)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::persistence::ProjectSessions;
    use aj_models::types::{
        AssistantContent, AssistantMessage, TextContent, ToolCall, ToolResultMessage, UserContent,
        UserMessage,
    };
    /// A scratch directory for one test's persistence state, removed when the
    /// returned guard drops. Callers must hold the guard for as long as they
    /// use the directory.
    fn fresh_sessions_dir() -> TempDir {
        TempDir::new().expect("create temp dir")
    }

    fn env_map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn say(log: &mut SessionLog, text: &str) -> Result<EntryRef, SessionError> {
        AgentLog::main(log).add_message(user_text(text))
    }

    /// Move the head to `head`, reading its branch first when the log does
    /// not hold it, as a host does.
    fn switch(log: &mut SessionLog, head: EntryId) {
        if let Some(read) = log.branch_read(&head).expect("plan the read") {
            let branch = read.read().expect("read the branch");
            log.install_branch(branch).expect("hold the branch");
        }
        log.set_head(head).expect("move the head");
    }

    fn user_text(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::User(UserMessage::text(text)))
    }

    fn assistant_text(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            ..AssistantMessage::empty()
        }))
    }

    fn assistant_tool_use(id: &str, name: &str) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                is_raw: false,
                id: id.to_string(),
                name: name.to_string(),
                arguments: serde_json::json!({}),
            })],
            ..AssistantMessage::empty()
        }))
    }

    fn tool_result(id: &str, name: &str, body: &str) -> AgentMessage {
        AgentMessage::wire(Message::ToolResult(ToolResultMessage::text(
            id, name, body, false,
        )))
    }

    fn task_notification_msg(body: &str) -> AgentMessage {
        use aj_agent::message::{TaskNotification, TaskNotificationKind, TaskOutcome};
        AgentMessage::task_notification(TaskNotification::new(
            "cargo build".to_string(),
            TaskNotificationKind::Bash,
            TaskOutcome::Succeeded,
            body.to_string(),
        ))
    }

    fn detailed_text_tool_result(
        id: &str,
        name: &str,
        content: &str,
        summary: &str,
        body: &str,
    ) -> AgentMessage {
        let mut result = ToolResultMessage::text(id, name, content, false);
        result.details = Some(serde_json::json!({
            "kind": "text",
            "summary": summary,
            "body": body,
        }));
        AgentMessage::wire(Message::ToolResult(result))
    }

    fn agent_tool_result_details(message: &AgentMessage) -> &serde_json::Value {
        let Some(Message::ToolResult(result)) = message.as_stored_wire() else {
            panic!("expected tool-result agent message");
        };
        result.details.as_ref().expect("tool details")
    }

    fn wire_tool_result_details(message: &Message) -> &serde_json::Value {
        let Message::ToolResult(result) = message else {
            panic!("expected tool-result wire message");
        };
        result.details.as_ref().expect("tool details")
    }

    /// A materialized log's persistence, its session id, and the entries on
    /// disk, plus the guard that removes the directory. Callers rewrite that
    /// file, so the guard has to outlive their use of the persistence.
    /// A second store under `dir` that holds `persistence`'s session `id`
    /// only as a JSONL file, not yet imported, and that file's path.
    fn as_jsonl(
        dir: &TempDir,
        persistence: &ProjectSessions,
        id: &str,
    ) -> (ProjectSessions, PathBuf) {
        let legacy = ProjectSessions::new(dir.path().join("legacy"));
        let path = persistence
            .export_jsonl(id, legacy.sessions_dir())
            .expect("write the JSONL file");
        (legacy, path)
    }

    /// Whether the store holds `log`'s session yet.
    fn stored(persistence: &ProjectSessions, log: &SessionLog) -> bool {
        persistence
            .in_database(log.session_id())
            .expect("look up the session")
    }

    /// What a write to `log`'s stored session would change.
    fn fingerprint(log: &SessionLog) -> (DateTime<Utc>, u64) {
        let stored = log
            .storage_metadata()
            .expect("read metadata")
            .expect("a stored session");
        (stored.modified_at, stored.revision)
    }

    /// A store whose one session is a JSONL file that has not been imported:
    /// a system prompt and a user message. Returns the file's lines.
    fn resume_fixture() -> (TempDir, ProjectSessions, String, Vec<String>) {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let (session_id, path) = test_support::jsonl_session(&persistence, |log| {
            log.set_system_prompt("fixture prompt".to_string())
                .expect("set system prompt");
            let mut view = AgentLog::main(log);
            view.add_message(user_text("fixture message"))
                .expect("add user message");
        });
        let records = std::fs::read_to_string(path)
            .expect("read fixture log")
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        (dir, persistence, session_id, records)
    }

    /// One thread: its owner, the thread it continues from, by
    /// its place in the list, with the position it continues at, and the ids
    /// of its messages.
    type Run = (crate::threads::Owner, Option<(usize, u64)>, Vec<EntryId>);

    /// The threads the store holds for a session, in the order they
    /// started.
    fn stored_threads(persistence: &ProjectSessions, id: &str) -> Vec<Run> {
        let store = persistence.sqlite_reader().expect("open").expect("a store");
        let rows = store.threads(id).expect("read the threads");
        let place = |thread_id| {
            let mut ids = rows.iter().map(|row| row.id);
            ids.position(|id| id == thread_id).expect("a stored id")
        };
        let mut messages = vec![Vec::new(); rows.len()];
        let (_, records) = store.whole(id).expect("read the records");
        for (thread_id, record) in records {
            messages[place(thread_id)].push(record.id);
        }
        let runs = rows.iter().zip(messages).map(|(row, messages)| {
            let parent = row.parent.map(|(parent, at)| (place(parent), at));
            // A thread ends where the store says it does.
            let last = messages.last().expect("no thread is empty");
            assert_eq!(&row.last_entry_id, last);
            (row.owner, parent, messages)
        });
        runs.collect()
    }

    /// The threads a log that holds its whole session files its entries
    /// under, in the order they started.
    fn held_threads(log: &SessionLog) -> Vec<Run> {
        assert!(log.core.is_complete());
        let threads = log.core.threads.iter().enumerate();
        let runs = threads.map(|(index, thread)| {
            let entries = log.core.range(1, u64::MAX);
            let own = entries.filter(|(_, filed, _)| *filed == index);
            let ids = own.map(|(_, _, entry)| entry.id.clone()).collect();
            (thread.owner, thread.parent, ids)
        });
        runs.collect()
    }

    /// The shape of every thread a log knows, held or not.
    fn thread_shapes(
        log: &SessionLog,
    ) -> Vec<(crate::threads::Owner, Option<(usize, u64)>, u64, EntryId)> {
        let threads = log.core.threads.iter();
        threads
            .map(|thread| {
                (
                    thread.owner,
                    thread.parent,
                    thread.tip,
                    thread.tip_id.clone(),
                )
            })
            .collect()
    }

    /// Append a session with meta entries, a branch started over at the
    /// system prompt, a branch from a message and a sub-agent run that goes
    /// on after it. Returns the threads it must form.
    fn a_branched_session(log: &mut SessionLog) -> Vec<Run> {
        use crate::threads::Owner;
        let root = log.set_system_prompt("prompt".into()).expect("root").id;
        let env = LogEntryKind::EnvChange {
            env: env_map(&[("A", "1")]),
        };
        let created = log
            .append(Some(root.clone()), ThreadKind::Meta, None, env)
            .expect("creation metadata")
            .id;
        let u1 = say(log, "u1").expect("u1").id;
        let a1 = {
            let mut view = AgentLog::main(log);
            view.add_message(assistant_text("a1")).expect("a1").id
        };
        let spawn = log
            .append_subagent_spawn(
                1,
                a1.clone(),
                "task",
                "agent",
                None,
                true,
                &spawn_settings(),
            )
            .expect("spawn")
            .id;
        let s1 = {
            let mut view = AgentLog::sub(log, spawn.clone(), 1);
            view.add_message(user_text("task")).expect("s1").id
        };
        let u2 = say(log, "u2").expect("u2").id;
        log.set_head(root.clone()).expect("back to the root");
        let r1 = say(log, "r1").expect("r1").id;
        log.set_head(u1.clone()).expect("back to u1");
        let b1 = say(log, "b1").expect("b1").id;
        let s2 = {
            let mut view = AgentLog::sub(log, s1.clone(), 1);
            view.add_message(assistant_text("report")).expect("s2").id
        };
        let b2 = say(log, "b2").expect("b2").id;
        vec![
            (Owner::Main, None, vec![root, created, u1, a1, u2]),
            (Owner::Sub(1), Some((0, 4)), vec![spawn, s1, s2]),
            (Owner::Main, Some((0, 1)), vec![r1]),
            (Owner::Main, Some((0, 3)), vec![b1, b2]),
        ]
    }

    /// The root thread with its meta entries, the branches and a
    /// sub-agent run are stored as the log filed them, and a reopened log
    /// knows them by the same parent pointers.
    #[test]
    fn threads_round_trip_through_the_store_with_their_parent_pointers() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        let expected = a_branched_session(&mut log);
        let id = log.session_id().to_owned();

        assert_eq!(held_threads(&log), expected);
        assert_eq!(stored_threads(&persistence, &id), expected);
        // Every thread was adopted under the id the store gave it.
        let adopted = log.core.threads.iter().map(|thread| thread.id);
        let stored = persistence.sqlite_reader().expect("open").expect("a store");
        let rows = stored.threads(&id).expect("threads");
        let ids: Vec<_> = rows.iter().map(|row| Some(row.id)).collect();
        assert_eq!(adopted.collect::<Vec<_>>(), ids);
        let (shapes, head) = (thread_shapes(&log), log.head().cloned());
        drop(log);

        // Reopened, in part and whole, it has the same threads, and goes
        // on filing under them.
        let mut resumed = SessionLog::resume(&persistence, &id).expect("resume");
        assert_eq!(thread_shapes(&resumed), shapes);
        assert_eq!(resumed.head(), head.as_ref());
        resumed.load_all().expect("read the whole session");
        assert_eq!(held_threads(&resumed), expected);
        let b3 = say(&mut resumed, "b3").expect("b3").id;
        let s2 = expected[1].2[2].clone();
        let s3 = {
            let mut view = AgentLog::sub(&mut resumed, s2, 1);
            view.add_message(user_text("more")).expect("s3").id
        };
        let mut expected = expected;
        expected[3].2.push(b3);
        expected[1].2.push(s3);
        assert_eq!(stored_threads(&persistence, &id), expected);
        assert_eq!(held_threads(&resumed), expected);
    }

    /// Importing a JSONL session files its entries as appending them live
    /// did, and exports the same bytes.
    #[test]
    fn an_import_files_records_as_appending_them_does() {
        let dir = fresh_sessions_dir();
        let live = ProjectSessions::new(dir.path().join("live"));
        let mut log = SessionLog::create(&live).expect("create log");
        let expected = a_branched_session(&mut log);
        let id = log.session_id().to_owned();
        drop(log);
        let exported = live
            .export_jsonl(&id, &dir.path().join("exported"))
            .expect("export the live session");

        let imported = ProjectSessions::new(dir.path().join("imported"));
        assert_eq!(imported.import_jsonl(&exported).expect("import"), id);
        assert_eq!(stored_threads(&imported, &id), expected);
        assert_eq!(stored_threads(&imported, &id), stored_threads(&live, &id));
        let again = imported
            .export_jsonl(&id, &dir.path().join("again"))
            .expect("export the imported session");
        assert_eq!(
            std::fs::read(again).expect("read"),
            std::fs::read(exported).expect("read")
        );
        // The import selects the branch the live session was on.
        let head = SessionLog::resume(&live, &id)
            .expect("resume")
            .head()
            .cloned();
        let resumed = SessionLog::resume(&imported, &id).expect("resume");
        assert_eq!(resumed.head(), head.as_ref());
    }

    /// A session that is not stored yet stores nothing for settings alone.
    /// Once it is stored, each entry is committed as it is appended.
    #[test]
    fn a_stored_session_commits_every_record_as_it_is_appended() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());

        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("prompt".to_string())
            .expect("set system prompt");
        assert!(!stored(&persistence, &log), "a prompt starts no session");

        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("add user message");
        }
        let model_id = log
            .append_model_change(ThreadFilter::MAIN, "prov", "model", 0)
            .expect("a model change")
            .id;
        let thinking_id = log
            .append_thinking_change(ThreadFilter::MAIN, "high")
            .expect("a thinking change")
            .id;
        let session_id = log.session_id().to_string();
        // Dropped without any further append, as a crash would leave it.
        drop(log);

        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume log");
        let settings: Vec<EntryId> = resumed
            .entries_in_order()
            .iter()
            .filter(|e| {
                matches!(
                    e.entry,
                    LogEntryKind::ModelChange { .. } | LogEntryKind::ThinkingChange { .. }
                )
            })
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(
            settings,
            vec![model_id, thinking_id],
            "both settings entries were committed, in append order"
        );
    }

    #[test]
    fn resume_ignores_blank_and_whitespace_lines_including_crlf() {
        let (_dir, persistence, session_id, records) = resume_fixture();
        let path = crate::legacy::session_path(&persistence, &session_id);
        let expected_ids = records
            .iter()
            .map(|record| {
                serde_json::from_str::<LogEntry>(record)
                    .expect("fixture record")
                    .id
            })
            .collect::<Vec<_>>();
        std::fs::write(
            &path,
            format!("\r\n \t\r\n{}\r\n\n  \n{}\n\t \r\n", records[0], records[1]),
        )
        .expect("rewrite fixture log");

        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume log");

        assert_eq!(resumed.core.order(), expected_ids);
        assert_eq!(resumed.core.held(), 2);
    }

    /// A malformed entry that carries its framing newline is not an
    /// interrupted final write: an interrupted write cannot have produced its
    /// own delimiter. It refuses as corruption and mutates nothing, even when
    /// only whitespace follows it.
    #[test]
    fn resume_refuses_a_malformed_delimited_final_record_without_changing_bytes() {
        let (_dir, persistence, session_id, records) = resume_fixture();
        let path = crate::legacy::session_path(&persistence, &session_id);
        let bytes = format!("{}\n{}\n{{\"id\":\n \t\r\n\r\n", records[0], records[1]).into_bytes();
        std::fs::write(&path, &bytes).expect("rewrite fixture log");

        let err = SessionLog::resume(&persistence, &session_id)
            .err()
            .expect("a delimited malformed record is corruption");
        match err {
            SessionError::Corrupt(message) => {
                assert!(message.contains("line 3"), "{message}");
            }
            other => panic!("expected corrupt log, got {other}"),
        }
        assert_eq!(
            std::fs::read(&path).expect("source bytes"),
            bytes,
            "refusal changed the source bytes"
        );
    }

    /// A final entry cut between its last byte and its newline is whole.
    /// The import adopts it, reports that once, and leaves the file alone.
    /// A clean file reports nothing.
    #[test]
    fn resume_adopts_a_complete_final_record_without_its_newline() {
        let (_dir, persistence, session_id, _) = resume_fixture();
        let (_, repair) =
            SessionLog::resume_reporting(&persistence, &session_id).expect("clean resume");
        assert_eq!(repair, None);

        let (_dir, persistence, session_id, records) = resume_fixture();
        let path = crate::legacy::session_path(&persistence, &session_id);
        let bytes = format!("{}\n{}", records[0], records[1]).into_bytes();
        std::fs::write(&path, &bytes).expect("remove trailing newline");

        let (mut resumed, repair) =
            SessionLog::resume_reporting(&persistence, &session_id).expect("resume log");
        assert_eq!(repair, Some(TailRepair::CompletedFraming));
        assert_eq!(resumed.core.order().len(), 2);

        AgentLog::main(&mut resumed)
            .add_message(user_text("after import"))
            .expect("append after import");
        drop(resumed);
        assert_eq!(std::fs::read(&path).expect("source bytes"), bytes);

        let (again, repair) =
            SessionLog::resume_reporting(&persistence, &session_id).expect("later resume");
        assert_eq!(repair, None, "a later open reports nothing");
        assert_eq!(again.core.order().len(), 3);
        assert_eq!(again.core.held(), 3);
    }

    /// An interrupted append leaves a strict prefix of one entry without its
    /// newline. Wherever the cut fell, including inside a multi-byte
    /// character, the import leaves the prefix out and reports its length
    /// once. The file keeps it.
    #[test]
    fn resume_drops_an_interrupted_final_record() {
        for which in 0..5 {
            let (_dir, persistence, session_id, records) = resume_fixture();
            let path = crate::legacy::session_path(&persistence, &session_id);
            let parent = serde_json::from_str::<LogEntry>(&records[1])
                .expect("parse fixture record")
                .id;
            let victim = LogEntry {
                id: "victim".to_string(),
                parent_id: Some(parent),
                timestamp: None,
                thread: ThreadKind::Main,
                agent_id: None,
                entry: LogEntryKind::Message {
                    message: user_text("caf\u{e9} with \"escapes\" and {braces}"),
                },
            };
            let line = serde_json::to_string(&victim).expect("serialize victim");
            let inside_e_acute = line.find('\u{e9}').expect("non-ascii payload") + 1;
            assert!(!line.is_char_boundary(inside_e_acute));
            let cut = [1, 8, line.len() / 2, inside_e_acute, line.len() - 1][which];
            let mut bytes = format!("{}\n{}\n", records[0], records[1]).into_bytes();
            bytes.extend_from_slice(&line.as_bytes()[..cut]);
            std::fs::write(&path, &bytes).expect("write cut fixture");

            let (log, repair) = SessionLog::resume_reporting(&persistence, &session_id)
                .unwrap_or_else(|err| panic!("cut {cut}: {err}"));
            assert_eq!(
                repair,
                Some(TailRepair::RemovedIncompleteEntry {
                    bytes: u64::try_from(cut).expect("cut fits u64"),
                }),
                "cut {cut}"
            );
            assert_eq!(log.core.order().len(), 2, "cut {cut}");
            drop(log);
            assert_eq!(
                std::fs::read(&path).expect("source bytes"),
                bytes,
                "cut {cut}"
            );

            let (again, repair) =
                SessionLog::resume_reporting(&persistence, &session_id).expect("later resume");
            assert_eq!(repair, None, "a later open reports nothing");
            assert_eq!(again.core.order().len(), 2, "cut {cut}");
        }
    }

    /// An undelimited final line that no interrupted append could have
    /// produced refuses without touching a byte.
    #[test]
    fn resume_refuses_an_undelimited_final_line_that_is_not_a_record_prefix() {
        let (_dir, persistence, session_id, records) = resume_fixture();
        let path = crate::legacy::session_path(&persistence, &session_id);
        let base = format!("{}\n{}\n", records[0], records[1]);
        let tails: [&[u8]; 5] = [
            b"not json at all",
            b"{\"id\":\"x\",\"type\":\"unknown_record\"}",
            &[records[1].as_bytes(), b"}"].concat(),
            &[records[1].as_bytes(), records[1].as_bytes()].concat(),
            b"{\"id\":\"x\xff\",\"type\":\"message\"",
        ];
        for tail in tails {
            let bytes = [base.as_bytes(), tail].concat();
            std::fs::write(&path, &bytes).expect("write refused fixture");

            let err = SessionLog::resume(&persistence, &session_id)
                .err()
                .unwrap_or_else(|| panic!("{tail:?} must refuse"));
            assert!(
                matches!(err, SessionError::UnsafeTail { .. }),
                "{tail:?}: {err}"
            );
            assert_eq!(
                std::fs::read(&path).expect("source bytes"),
                bytes,
                "{tail:?}: refusal changed the source bytes"
            );
        }
    }

    /// A duplicated line gives one entry two positions unless
    /// resume rejects it, which would project its message twice and hand
    /// the same entry id two seqs.
    #[test]
    fn resume_keeps_the_first_occurrence_of_a_duplicated_entry() {
        let (_dir, persistence, session_id, records) = resume_fixture();
        let path = crate::legacy::session_path(&persistence, &session_id);
        std::fs::write(
            &path,
            format!("{}\n{}\n{}\n", records[0], records[1], records[1]),
        )
        .expect("rewrite fixture log with a duplicated line");

        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume log");

        assert_eq!(resumed.core.order().len(), 2, "the duplicate is dropped");
        assert_eq!(resumed.core.held(), 2);
        assert_eq!(resumed.last_seq(), 2, "positions count the entry once");

        // And the message projects once, at one position.
        let message_ends: Vec<String> = crate::replay::project_suffix(
            &resumed.snapshot(),
            None,
            &std::collections::BTreeSet::new(),
        )
        .filter_map(|tagged| match tagged.event {
            aj_agent::events::AgentEvent::MessageEnd { .. } => {
                Some(tagged.entry.expect("a MessageEnd is durable").id)
            }
            _ => None,
        })
        .collect();
        assert_eq!(message_ends.len(), 1, "got {message_ends:?}");
    }

    #[test]
    fn resume_refuses_a_duplicated_legacy_root_without_changing_bytes() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let session_id = "2000-01-01-00-00-00-000_9";
        let path = crate::legacy::session_path(&persistence, session_id);
        let root = LogEntry {
            id: "legacy-root".to_string(),
            parent_id: None,
            timestamp: None,
            thread: ThreadKind::Main,
            agent_id: None,
            entry: LogEntryKind::Message {
                message: user_text("legacy root"),
            },
        };
        let line = serde_json::to_string(&root).expect("legacy root JSON");
        // The missing final newline also proves corruption refusal does not
        // normalize a legacy source before returning.
        let bytes = format!("{line}\n{line}").into_bytes();
        std::fs::write(&path, &bytes).expect("write duplicated legacy root");

        let err = SessionLog::resume(&persistence, session_id)
            .err()
            .expect("a duplicated physical root is corruption");
        match err {
            SessionError::Corrupt(message) => assert_eq!(
                message,
                format!(
                    "{}:line 2: duplicate creation identity entry id legacy-root",
                    path.display()
                )
            ),
            other => panic!("expected corrupt log, got {other}"),
        }
        assert_eq!(
            std::fs::read(&path).expect("source bytes"),
            bytes,
            "resume changed the refused legacy log"
        );
    }

    /// `create` has to reserve its id with an atomic filesystem
    /// operation. A session is not stored until its first substantive
    /// entry, so an existence check cannot see a competing create, and
    /// two sessions minted in the same millisecond would otherwise share
    /// one id.
    #[test]
    fn create_mints_distinct_ids_and_claims_them_on_disk() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());

        let logs: Vec<SessionLog> = (0..12)
            .map(|_| SessionLog::create(&persistence).expect("create log"))
            .collect();
        let ids: std::collections::HashSet<&str> =
            logs.iter().map(|log| log.session_id()).collect();
        assert_eq!(ids.len(), logs.len(), "every create minted its own id");

        for log in &logs {
            assert!(
                !log.path().exists(),
                "no log file until the first punctuation append"
            );
            assert!(
                crate::lock::lock_path(dir.path(), log.session_id())
                    .expect("a minted id")
                    .exists(),
                "the claim is visible on the filesystem"
            );
        }
    }

    /// NOTE: this covers the cross-process hazard only as far as one
    /// process can. `create_new` is atomic across processes by
    /// definition, so an in-process assertion that a claimed id is
    /// refused is the honest test of it. Spawning a second aj process
    /// from a unit test would buy nothing beyond the same syscall.
    #[test]
    fn create_never_reuses_a_claimed_id() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let first = SessionLog::create(&persistence).expect("create log");

        let claimed = first.session_id().to_string();
        let minted = SessionLog::mint_session_id(dir.path(), &claimed)
            .expect("minting near a claimed base succeeds");
        assert_ne!(minted, claimed, "the claimed id is refused");
        assert_eq!(minted, format!("{claimed}_1"), "and the next one is taken");
    }

    #[test]
    fn resume_reports_physical_line_for_malformed_non_final_record() {
        let (_dir, persistence, session_id, records) = resume_fixture();
        let path = crate::legacy::session_path(&persistence, &session_id);
        std::fs::write(
            &path,
            format!(
                "{}\r\n \t\r\n{{\"id\":\r\n\r\n{}\r\n",
                records[0], records[1]
            ),
        )
        .expect("rewrite fixture log");

        let err = match SessionLog::resume(&persistence, &session_id) {
            Ok(_) => panic!("non-final malformed record must fail"),
            Err(err) => err,
        };

        match err {
            SessionError::Corrupt(message) => assert!(
                message.starts_with(&format!("{}:line 3:", path.display())),
                "unexpected corruption message: {message}"
            ),
            other => panic!("expected corrupt log, got {other}"),
        }
    }

    /// A physical line that is not UTF-8 can never be a current entry, so it
    /// refuses as corruption naming its line, leaving every byte in place.
    #[test]
    fn resume_reports_invalid_utf8_as_corruption_with_its_line() {
        let (_dir, persistence, session_id, records) = resume_fixture();
        let path = crate::legacy::session_path(&persistence, &session_id);
        let mut contents = records[0].as_bytes().to_vec();
        contents.extend_from_slice(b"\n\xff\n");
        std::fs::write(&path, &contents).expect("rewrite fixture log");

        let err = match SessionLog::resume(&persistence, &session_id) {
            Ok(_) => panic!("invalid UTF-8 must fail"),
            Err(err) => err,
        };

        match err {
            SessionError::Corrupt(message) => {
                assert!(
                    message.contains("line 2") && message.contains("not valid UTF-8"),
                    "{message}"
                );
            }
            other => panic!("expected corruption, got {other}"),
        }
        assert_eq!(
            std::fs::read(&path).expect("source bytes"),
            contents,
            "refusal changed the source bytes"
        );
    }

    /// The first malformed line names the failure even when later lines are
    /// also damaged: refusal is deterministic over the earliest defect.
    #[test]
    fn resume_reports_the_earliest_corruption_when_later_lines_are_also_damaged() {
        let (_dir, persistence, session_id, records) = resume_fixture();
        let path = crate::legacy::session_path(&persistence, &session_id);
        let mut contents = records[0].as_bytes().to_vec();
        contents.extend_from_slice(b"\n{\"id\":\n");
        contents.extend_from_slice(records[1].as_bytes());
        contents.extend_from_slice(b"\n\xff\n");
        std::fs::write(&path, &contents).expect("rewrite fixture log");

        let err = match SessionLog::resume(&persistence, &session_id) {
            Ok(_) => panic!("interior corruption must fail"),
            Err(err) => err,
        };

        match err {
            SessionError::Corrupt(message) => {
                assert!(message.contains("line 2"), "{message}");
            }
            other => panic!("expected corruption, got {other}"),
        }
        assert_eq!(
            std::fs::read(&path).expect("source bytes"),
            contents,
            "refusal changed the source bytes"
        );
    }

    /// A duplicated creation identity is corruption at its own line, and a
    /// later damaged line does not displace or reorder that refusal.
    #[test]
    fn resume_reports_a_duplicate_creation_identity_before_later_damage() {
        let (_dir, persistence, session_id, records) = resume_fixture();
        let path = crate::legacy::session_path(&persistence, &session_id);
        let mut contents = records[0].as_bytes().to_vec();
        contents.extend_from_slice(b"\n");
        contents.extend_from_slice(records[0].as_bytes());
        contents.extend_from_slice(b"\n");
        contents.extend_from_slice(records[1].as_bytes());
        contents.extend_from_slice(b"\n\xff\n");
        std::fs::write(&path, &contents).expect("rewrite fixture log");

        let err = match SessionLog::resume(&persistence, &session_id) {
            Ok(_) => panic!("duplicate creation identity must fail"),
            Err(err) => err,
        };

        match err {
            SessionError::Corrupt(message) => {
                assert!(message.contains("duplicate creation identity"), "{message}");
            }
            other => panic!("expected corruption, got {other}"),
        }
        assert_eq!(
            std::fs::read(&path).expect("source bytes"),
            contents,
            "refusal changed the source bytes"
        );
    }

    #[test]
    fn set_system_prompt_records_root_entry_in_memory() {
        // In-memory contract: after `set_system_prompt` the entry is
        // immediately visible to all read-side queries
        // (`system_prompt`, `system_prompt_id`, `len`, `entries`).
        // The deferred-disk-write behaviour is exercised separately
        // by [`set_system_prompt_alone_does_not_create_file`] and
        // [`the_first_message_stores_the_system_prompt_before_itself`].
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");

        let id = log
            .set_system_prompt("hello world".to_string())
            .expect("set_system_prompt on empty log")
            .id;

        assert_eq!(log.system_prompt(), Some("hello world"));
        assert_eq!(log.system_prompt_id(), Some(&id));

        assert_eq!(log.len(), 1);
        let entry = log.core.get(&id).expect("entry exists");
        assert!(matches!(entry.thread, ThreadKind::Meta));
        assert!(entry.parent_id.is_none());
        assert!(matches!(entry.entry, LogEntryKind::SystemPrompt { .. }));
    }

    #[test]
    fn set_system_prompt_alone_does_not_create_file() {
        // A session that only sees a system-prompt append must leave
        // no file in the sessions directory — that's the property
        // that prevents accumulating empty sessions when the user
        // opens the TUI and quits before submitting anything.
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string()).expect("set sp");

        assert!(
            !stored(&persistence, &log),
            "system-prompt-only log must not materialise a session"
        );
    }

    #[test]
    fn the_first_message_stores_the_system_prompt_before_itself() {
        // The system prompt waits for the first message and is stored
        // before it, in the same transaction, so the stored order is the
        // order in memory. Resume and check both entries are present in
        // that order.
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());

        let session_id = {
            let mut log = SessionLog::create(&persistence).expect("create log");
            log.set_system_prompt("the prompt".to_string())
                .expect("set sp");

            assert!(!stored(&persistence, &log), "nothing stored before flush");

            {
                let mut view = AgentLog::main(&mut log);
                view.add_message(user_text("hi"))
                    .expect("first user message");
            }

            assert!(stored(&persistence, &log), "stored after first punctuation");
            log.session_id().to_string()
        };

        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume");
        let entries = resumed.entries_in_order();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            entries[0].entry,
            LogEntryKind::SystemPrompt { .. }
        ));
        assert!(matches!(entries[1].entry, LogEntryKind::Message { .. }));
    }

    #[test]
    fn set_system_prompt_rejects_non_empty_log() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");

        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi"))
                .expect("first user message");
        }

        let err = log
            .set_system_prompt("too late".to_string())
            .expect_err("must fail on non-empty log");
        assert!(matches!(err, SessionError::InvalidAppend(_)));
    }

    #[test]
    fn first_user_message_anchors_to_system_prompt_root() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");

        let sp_id = log
            .set_system_prompt("the prompt".to_string())
            .expect("set system prompt")
            .id;

        let user_id = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("user msg").id
        };

        let user_entry = log.core.get(&user_id).expect("user entry exists");
        assert_eq!(user_entry.parent_id.as_ref(), Some(&sp_id));
    }

    #[test]
    fn latest_leaf_user_skips_system_prompt() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");

        log.set_system_prompt("p".to_string()).expect("set sp");

        assert!(log.latest_leaf(ThreadFilter::MAIN).is_none());

        let user_id = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("user msg").id
        };

        assert_eq!(log.latest_leaf(ThreadFilter::MAIN).as_ref(), Some(&user_id));
    }

    #[test]
    fn linearize_user_walks_past_system_prompt() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string()).expect("set sp");

        let user_id = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("user msg").id
        };

        let convo = log.linearize(&user_id, ThreadFilter::MAIN);
        // SystemPrompt must not appear in a Main-thread linearization;
        // only the user message should be present.
        assert_eq!(convo.entries().len(), 1);
        assert!(matches!(
            convo.entries()[0].entry,
            LogEntryKind::Message { .. }
        ));
        assert_eq!(convo.message_count(), 1);
        assert_eq!(convo.messages().len(), 1);
    }

    /// The store files every entry under its parent, so a JSONL file whose
    /// parent link names no earlier entry (a hand-edited file) cannot be
    /// imported. It is refused and left as it was.
    #[test]
    fn resume_refuses_a_broken_parent_link() {
        let (_dir, persistence, session_id, records) = resume_fixture();
        let path = crate::legacy::session_path(&persistence, &session_id);
        let mut orphan: serde_json::Value = serde_json::from_str(&records[1]).expect("json");
        orphan["parent_id"] = serde_json::json!("deadbeef");
        let bytes = format!("{}\n{orphan}\n", records[0]).into_bytes();
        std::fs::write(&path, &bytes).expect("rewrite");

        let err = SessionLog::resume(&persistence, &session_id)
            .err()
            .expect("an orphan cannot be filed");
        assert!(matches!(err, SessionError::Sqlite(_)), "{err}");
        assert!(!persistence.in_database(&session_id).expect("look up"));
        assert_eq!(std::fs::read(&path).expect("source bytes"), bytes);
    }

    #[test]
    fn resume_preserves_system_prompt() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());

        let session_id = {
            let mut log = SessionLog::create(&persistence).expect("create log");
            log.set_system_prompt("persisted prompt".to_string())
                .expect("set sp");
            {
                let mut view = AgentLog::main(&mut log);
                view.add_message(user_text("hi")).expect("user msg");
            }
            log.session_id().to_string()
        };

        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume log");
        assert_eq!(resumed.system_prompt(), Some("persisted prompt"));
        assert!(resumed.system_prompt_id().is_some());
        assert!(resumed.latest_leaf(ThreadFilter::MAIN).is_some());
    }

    #[test]
    fn subagent_thread_attaches_to_existing_user_chain() {
        // A subagent's first message attaches to the main-thread parent
        // it was spawned from; subagent linearization only collects
        // subagent entries.
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string()).expect("set sp");

        let user_id = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("user msg").id
        };

        let sub_id = {
            let mut view = AgentLog::sub(&mut log, user_id.clone(), 1);
            view.add_message(user_text("subtask"))
                .expect("subagent prompt")
                .id
        };

        let convo = log.linearize(&sub_id, ThreadFilter::sub(1));
        assert_eq!(convo.entries().len(), 1);
    }

    #[test]
    fn direct_message_append_compacts_storage_and_expands_resume_projections() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("sys".into()).expect("set sp");

        let content = "persisted model-facing content\n".repeat(40);
        let body = format!("{content}\n");
        let live_message =
            detailed_text_tool_result("tu-1", "read_file", &content, "read_file large.txt", &body);
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("read it")).expect("user msg");
            view.add_message(assistant_tool_use("tu-1", "read_file"))
                .expect("assistant msg");
            view.add_message(live_message.clone())
                .expect("tool result entry");
        }

        let live_details = agent_tool_result_details(&live_message);
        assert_eq!(live_details["body"], body);
        assert!(live_details.get("body_ref").is_none());

        let raw_entry = log
            .entries_in_order()
            .into_iter()
            .find(|entry| {
                matches!(
                    &entry.entry,
                    LogEntryKind::Message { message }
                        if matches!(message.as_stored_wire(), Some(Message::ToolResult(_)))
                )
            })
            .expect("stored tool result");
        let LogEntryKind::Message { message } = &raw_entry.entry else {
            unreachable!("matched message entry");
        };
        let raw_details = agent_tool_result_details(message);
        assert_eq!(raw_details["body_ref"]["source"], "content_text");
        assert_eq!(raw_details["body_ref"]["append_newline"], true);
        assert!(raw_details.get("body").is_none());

        let session_id = log.session_id().to_string();
        drop(log);
        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume log");
        let head = resumed.latest_leaf(ThreadFilter::MAIN).expect("head");
        let branch = resumed.linearize(&head, ThreadFilter::MAIN);

        let raw_resumed = branch
            .entries()
            .iter()
            .find_map(|entry| match &entry.entry {
                LogEntryKind::Message { message }
                    if matches!(message.as_stored_wire(), Some(Message::ToolResult(_))) =>
                {
                    Some(agent_tool_result_details(message))
                }
                _ => None,
            })
            .expect("raw resumed tool result");
        assert!(raw_resumed.get("body").is_none());
        assert_eq!(raw_resumed["body_ref"]["source"], "content_text");

        let agent_messages = branch.agent_messages();
        let projected_agent = agent_messages
            .iter()
            .find(|message| matches!(message.as_stored_wire(), Some(Message::ToolResult(_))))
            .expect("projected agent tool result");
        let projected_agent_details = agent_tool_result_details(projected_agent);
        assert_eq!(projected_agent_details["summary"], "read_file large.txt");
        assert_eq!(projected_agent_details["body"], body);
        assert!(projected_agent_details.get("body_ref").is_none());

        let messages = branch.messages();
        let projected_wire = messages
            .iter()
            .find(|message| matches!(message, Message::ToolResult(_)))
            .expect("projected wire tool result");
        let projected_wire_details = wire_tool_result_details(projected_wire);
        assert_eq!(projected_wire_details["summary"], "read_file large.txt");
        assert_eq!(projected_wire_details["body"], body);
        assert!(projected_wire_details.get("body_ref").is_none());

        let last = branch.last_message().expect("last message");
        assert_eq!(wire_tool_result_details(&last)["body"], body);
    }

    #[test]
    fn task_notification_round_trips_as_typed_kind_through_resume() {
        use aj_agent::message::AgentMessageKind;

        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("system prompt");
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("user message");
            view.add_message(task_notification_msg("exit code 0"))
                .expect("task notification");
        }
        let session_id = log.session_id().to_string();
        drop(log);

        // Resume from disk: the notice deserializes back to the typed
        // kind, not a plain user entry.
        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume log");
        let head = resumed.latest_leaf(ThreadFilter::MAIN).expect("head");
        let branch = resumed.linearize(&head, ThreadFilter::MAIN);

        let agent_messages = branch.agent_messages();
        match &agent_messages.last().expect("a message").kind {
            AgentMessageKind::TaskNotification(n) => {
                assert_eq!(n.label, "cargo build");
                assert_eq!(n.body, "exit code 0");
            }
            other => panic!("expected TaskNotification, got {other:?}"),
        }

        // It still projects onto the wire as the framed user message the
        // model expects.
        let messages = branch.messages();
        match messages.last().expect("a wire message") {
            Message::User(u) => match &u.content[0] {
                UserContent::Text(t) => assert_eq!(
                    t.text,
                    "<task-notification>\nexit code 0\n</task-notification>"
                ),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected framed user projection, got {other:?}"),
        }
    }

    #[test]
    fn add_message_tool_result_round_trips_through_resume() {
        // ToolResult messages serialize with their structured details
        // preserved on disk and rehydrate equivalently on resume.
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("sys".into()).expect("set sp");

        let mut tr = ToolResultMessage::text("tu-1", "ping", "pong", false);
        tr.details = Some(serde_json::json!({
            "kind": "text",
            "summary": "ping",
            "body": "pong",
        }));
        let tool_result_msg = AgentMessage::wire(Message::ToolResult(tr));

        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("user msg");
            view.add_message(assistant_tool_use("tu-1", "ping"))
                .expect("assistant msg");
            view.add_message(tool_result_msg)
                .expect("tool result entry");
        }

        let session_id = log.session_id().to_string();
        drop(log);
        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume log");

        let head = resumed
            .latest_leaf(ThreadFilter::MAIN)
            .expect("main-thread head exists");
        let convo = resumed.linearize(&head, ThreadFilter::MAIN);

        // Three wire messages: user, assistant, tool_result.
        assert_eq!(convo.message_count(), 3);
        let messages = convo.messages();
        assert_eq!(messages.len(), 3);
        match &messages[2] {
            Message::ToolResult(tr) => {
                assert_eq!(tr.tool_call_id, "tu-1");
                assert!(tr.details.is_some());
                assert_eq!(tr.details.as_ref().unwrap()["summary"], "ping");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn message_id_is_adopted_as_entry_id_and_survives_resume() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("sys".into()).expect("set sp");

        // A live message mints its own id; capture it before the append
        // takes ownership.
        let message = user_text("hi");
        let message_id = message.id().to_string();
        assert!(!message_id.is_empty(), "live messages mint an id");

        let entry_id = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(message).expect("user msg").id
        };
        assert_eq!(entry_id, message_id, "append adopts the message id");

        let session_id = log.session_id().to_string();
        drop(log);
        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume");

        let entry = resumed
            .entries_in_order()
            .into_iter()
            .find(|entry| matches!(entry.entry, LogEntryKind::Message { .. }))
            .expect("reloaded message entry");
        assert_eq!(entry.id, message_id, "entry id is stable across resume");
        let LogEntryKind::Message { message } = &entry.entry else {
            unreachable!("matched a message entry");
        };
        assert_eq!(
            message.id(),
            entry.id,
            "resume backfills the message id from the entry id"
        );
    }

    #[test]
    fn resume_backfills_message_ids_on_legacy_8hex_file() {
        // A hand-built legacy fixture: 8-hex entry ids and bare wire
        // messages with no `id` field, the real shape of old files.
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let session_id = "2000-01-01-00-00-00-000";

        let root = LogEntry {
            id: "0000aaaa".to_string(),
            parent_id: None,
            timestamp: None,
            thread: ThreadKind::Main,
            agent_id: None,
            entry: LogEntryKind::Message {
                message: user_text("hi"),
            },
        };
        let child = LogEntry {
            id: "1111bbbb".to_string(),
            parent_id: Some("0000aaaa".to_string()),
            timestamp: None,
            thread: ThreadKind::Main,
            agent_id: None,
            entry: LogEntryKind::Message {
                message: assistant_text("hello"),
            },
        };
        let root_line = serde_json::to_string(&root).expect("serialize root");
        let child_line = serde_json::to_string(&child).expect("serialize child");

        // The wire message is `#[serde(skip)]` on its id, so the nested
        // `message` object carries none: this is what old files look like.
        let root_json: serde_json::Value = serde_json::from_str(&root_line).unwrap();
        assert!(
            root_json["message"].get("id").is_none(),
            "legacy fixture must not carry an id inside the wire message"
        );

        std::fs::write(
            crate::legacy::session_path(&persistence, session_id),
            format!("{root_line}\n{child_line}\n"),
        )
        .expect("write legacy fixture");

        let resumed = SessionLog::resume(&persistence, session_id).expect("resume");
        let mut message_entries = 0;
        for entry in resumed.entries_in_order() {
            if let LogEntryKind::Message { message } = &entry.entry {
                message_entries += 1;
                assert!(!message.id().is_empty(), "backfilled id is non-empty");
                assert_eq!(
                    message.id(),
                    entry.id,
                    "legacy entry id becomes the message id"
                );
            }
        }
        assert_eq!(message_entries, 2, "both message entries were backfilled");
    }

    #[test]
    fn duplicate_message_id_append_errors() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("sys".into()).expect("set sp");

        let m1 = user_text("first");
        let mut m2 = user_text("second");
        m2.set_id(m1.id().to_string());

        let mut view = AgentLog::main(&mut log);
        view.add_message(m1).expect("append m1");
        let err = view.add_message(m2).expect_err("duplicate id must error");
        assert!(
            matches!(err, SessionError::InvalidAppend(_)),
            "expected InvalidAppend on a duplicate adopted id, got {err}"
        );
    }

    fn assistant_from(provider: &str, model: &str) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: "ok".to_string(),
                text_signature: None,
            })],
            provider: provider.to_string(),
            model: model.to_string(),
            ..AssistantMessage::empty()
        }))
    }

    #[test]
    fn settings_entries_round_trip_through_resume() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let session_id = {
            let mut log = SessionLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".into()).expect("set sp");
            log.append_model_change(ThreadFilter::MAIN, "anthropic", "claude-x", 200_000)
                .expect("model change");
            log.append_thinking_change(ThreadFilter::MAIN, "high")
                .expect("thinking change");
            log.append_speed_change(ThreadFilter::MAIN, "fast")
                .expect("speed change");
            log.append_verbosity_change(ThreadFilter::MAIN, "high")
                .expect("verbosity change");
            {
                let mut view = AgentLog::main(&mut log);
                view.add_message(user_text("hi")).expect("user msg");
            }
            log.session_id().to_string()
        };

        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume");
        let entries = resumed.entries_in_order();
        assert_eq!(entries.len(), 6);
        match &entries[1].entry {
            LogEntryKind::ModelChange {
                provider,
                model_id,
                context_window,
            } => {
                assert_eq!(provider, "anthropic");
                assert_eq!(model_id, "claude-x");
                assert_eq!(*context_window, 200_000);
            }
            other => panic!("expected ModelChange, got {other:?}"),
        }
        match &entries[2].entry {
            LogEntryKind::ThinkingChange { level } => assert_eq!(level, "high"),
            other => panic!("expected ThinkingChange, got {other:?}"),
        }
        match &entries[3].entry {
            LogEntryKind::SpeedChange { speed } => assert_eq!(speed, "fast"),
            other => panic!("expected SpeedChange, got {other:?}"),
        }
        match &entries[4].entry {
            LogEntryKind::VerbosityChange { verbosity } => assert_eq!(verbosity, "high"),
            other => panic!("expected VerbosityChange, got {other:?}"),
        }
    }

    #[test]
    fn legacy_model_change_capacity_is_unknown() {
        let entry: LogEntryKind = serde_json::from_value(serde_json::json!({
            "type": "model_change", "provider": "anthropic", "model_id": "claude-x"
        }))
        .expect("legacy model change");
        assert!(matches!(
            entry,
            LogEntryKind::ModelChange {
                context_window: 0,
                ..
            }
        ));
    }

    #[test]
    fn settings_alone_do_not_store_a_session() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        log.append_model_change(ThreadFilter::MAIN, "openai", "gpt-x", 0)
            .expect("model change");
        log.append_thinking_change(ThreadFilter::MAIN, "off")
            .expect("thinking change");

        assert!(
            !stored(&persistence, &log),
            "settings-only log must not materialise a session"
        );

        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("user msg");
        }
        assert!(stored(&persistence, &log), "stored after first punctuation");

        let resumed = SessionLog::resume(&persistence, log.session_id()).expect("resume");
        let entries = resumed.entries_in_order();
        assert!(matches!(
            entries[0].entry,
            LogEntryKind::SystemPrompt { .. }
        ));
        assert!(matches!(entries[1].entry, LogEntryKind::ModelChange { .. }));
        assert!(matches!(
            entries[2].entry,
            LogEntryKind::ThinkingChange { .. }
        ));
        assert!(matches!(entries[3].entry, LogEntryKind::Message { .. }));
    }

    #[test]
    fn env_change_round_trips_with_its_stable_tag_and_starts_no_session() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let expected = env_map(&[
            ("BEADS_ACTOR", "session-actor"),
            ("SECRET_TOKEN", "hunter2"),
        ]);
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        let env_ref = log
            .append_env_change(expected.clone())
            .expect("append env seed");
        assert!(
            !stored(&persistence, &log),
            "an env seed with no message must not materialize a session"
        );
        let stored = log.core().get(&env_ref.id).expect("env entry in memory");
        assert!(
            !stored.entry.is_work(),
            "EnvChange must buffer like the other creation seeds"
        );

        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("punctuation");
        }
        let session_id = log.session_id().to_string();
        let encoded = persistence
            .read_entries(&session_id)
            .expect("read the stored session")
            .iter()
            .map(|entry| serde_json::to_value(entry).expect("entry JSON"))
            .find(|entry| entry["id"] == env_ref.id)
            .expect("env record in the store");
        assert_eq!(encoded["type"], "env_change");
        assert_eq!(
            encoded["env"],
            serde_json::to_value(&expected).expect("expected env serializes")
        );

        drop(log);
        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume");
        let LogEntryKind::EnvChange { env } = &resumed
            .entries_in_order()
            .into_iter()
            .find(|entry| entry.id == env_ref.id)
            .expect("resumed env entry")
            .entry
        else {
            panic!("env_change tag did not deserialize as EnvChange");
        };
        assert_eq!(env, &expected);
        assert_eq!(resumed.session_env(), Some(&expected));
    }

    #[test]
    fn env_extraction_distinguishes_absent_from_explicitly_empty() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());

        let mut absent = SessionLog::create(&persistence).expect("create absent log");
        absent.set_system_prompt("p".into()).expect("set sp");
        {
            let mut view = AgentLog::main(&mut absent);
            view.add_message(user_text("no env")).expect("user");
        }
        assert_eq!(
            absent.session_env(),
            None,
            "a log with no EnvChange must extract None"
        );

        let mut empty = SessionLog::create(&persistence).expect("create empty log");
        empty.set_system_prompt("p".into()).expect("set sp");
        empty
            .append_env_change(BTreeMap::new())
            .expect("append explicit empty env");
        {
            let mut view = AgentLog::main(&mut empty);
            view.add_message(user_text("empty env")).expect("user");
        }
        assert_eq!(
            empty.session_env(),
            Some(&BTreeMap::new()),
            "a recorded empty EnvChange must extract Some(empty)"
        );
    }

    #[test]
    fn session_env_validation_is_exact_and_resume_refuses_invalid_identity_without_writes() {
        let distinct_case = env_map(&[("AJ_CASE", "upper"), ("aj_case", "lower")]);
        validate_session_env(&distinct_case).expect("supported hosts keep exact key spelling");

        for (env, expected) in [
            (env_map(&[("", "value")]), "is empty"),
            (env_map(&[("BAD=KEY", "value")]), "contains '='"),
            (env_map(&[("BAD\0KEY", "value")]), "contains NUL"),
            (env_map(&[("KEY", "bad\0value")]), "contains NUL"),
        ] {
            let err = validate_session_env(&env).expect_err("invalid map is refused");
            assert!(err.to_string().contains(expected), "{err}");
            let dir = fresh_sessions_dir();
            let persistence = ProjectSessions::new(dir.path().to_path_buf());
            let mut log = SessionLog::create(&persistence).expect("create");
            log.set_system_prompt("p".into()).expect("root");
            log.append_env_change(BTreeMap::new()).expect("valid seed");
            let before = log.len();
            let err = log.append_env_change(env).expect_err("invalid edit");
            assert!(matches!(err, SessionError::InvalidAppend(_)));
            assert_eq!(log.len(), before);
            assert_eq!(log.session_env(), Some(&BTreeMap::new()));
        }

        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let session_id = "2000-01-01-00-00-00-000";
        let path = crate::legacy::session_path(&persistence, session_id);
        let root = LogEntry {
            id: "root".to_string(),
            parent_id: None,
            timestamp: None,
            thread: ThreadKind::Meta,
            agent_id: None,
            entry: LogEntryKind::SystemPrompt { text: "p".into() },
        };
        for thread in [ThreadKind::Main, ThreadKind::Meta] {
            let invalid_env = LogEntry {
                id: "env".to_string(),
                parent_id: Some("root".to_string()),
                timestamp: None,
                thread,
                agent_id: None,
                entry: LogEntryKind::EnvChange {
                    env: env_map(&[("", "value")]),
                },
            };
            let message = LogEntry {
                id: "message".to_string(),
                parent_id: Some("root".to_string()),
                timestamp: None,
                thread: ThreadKind::Main,
                agent_id: None,
                entry: LogEntryKind::Message {
                    message: user_text("hi"),
                },
            };
            let bytes = format!(
                "{}\n{}\n{}",
                serde_json::to_string(&root).expect("root"),
                serde_json::to_string(&invalid_env).expect("env"),
                serde_json::to_string(&message).expect("message")
            )
            .into_bytes();
            std::fs::write(&path, &bytes)
                .expect("write invalid recorded env without final newline");

            let err = SessionLog::resume(&persistence, session_id)
                .err()
                .expect("invalid recorded identity is corruption");
            assert!(
                err.to_string().contains("environment key \"\" is empty"),
                "{err}"
            );
            assert_eq!(
                std::fs::read(&path).expect("source bytes"),
                bytes,
                "validation ran after resume normalized the source file"
            );
        }
    }

    #[test]
    fn append_refuses_a_system_prompt_outside_the_meta_root_boundary() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");

        let err = log
            .append(
                None,
                ThreadKind::Main,
                None,
                LogEntryKind::SystemPrompt {
                    text: "wrong-thread root".to_string(),
                },
            )
            .expect_err("a main-thread system prompt is not a legal creation root");
        assert!(matches!(err, SessionError::InvalidAppend(_)), "{err}");
        assert!(log.is_empty(), "the refused root entered the in-memory log");
        assert!(
            !log.path().exists(),
            "the refused root published a session file"
        );

        let root = log
            .set_system_prompt("valid root".to_string())
            .expect("the refused append leaves the log reusable");
        let stored = log.core().get(&root.id).expect("stored valid root");
        assert_eq!(stored.thread, ThreadKind::Meta);
        assert_eq!(stored.parent_id, None);

        let err = log
            .append(
                Some(root.id.clone()),
                ThreadKind::Meta,
                None,
                LogEntryKind::SystemPrompt {
                    text: "second prompt".to_string(),
                },
            )
            .expect_err("a second root-parented meta system prompt is not legal");
        assert!(matches!(err, SessionError::InvalidAppend(_)), "{err}");
        assert_eq!(log.len(), 1, "the refused second prompt entered the log");
        assert_eq!(
            log.system_prompt(),
            Some("valid root"),
            "the refused prompt displaced the creation root"
        );
    }

    #[test]
    fn resume_refuses_a_non_meta_system_prompt_without_an_env_record() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let session_id = "2000-01-01-00-00-00-000_8";
        let root = LogEntry {
            id: "root".to_string(),
            parent_id: None,
            timestamp: None,
            thread: ThreadKind::Main,
            agent_id: None,
            entry: LogEntryKind::SystemPrompt { text: "p".into() },
        };
        let message = LogEntry {
            id: "message".to_string(),
            parent_id: Some("root".to_string()),
            timestamp: None,
            thread: ThreadKind::Main,
            agent_id: None,
            entry: LogEntryKind::Message {
                message: user_text("punctuation"),
            },
        };
        let bytes = format!(
            "{}\n{}\n",
            serde_json::to_string(&root).expect("root JSON"),
            serde_json::to_string(&message).expect("message JSON"),
        )
        .into_bytes();
        let path = crate::legacy::session_path(&persistence, session_id);
        std::fs::write(&path, &bytes).expect("write malformed prompt layout");

        let err = SessionLog::resume(&persistence, session_id)
            .err()
            .expect("a non-meta system prompt is corruption without EnvChange too");
        assert!(matches!(err, SessionError::Corrupt(_)), "{err}");
        assert_eq!(
            std::fs::read(&path).expect("source bytes"),
            bytes,
            "resume changed malformed system-prompt layout"
        );
    }

    #[test]
    fn resume_refuses_every_malformed_env_creation_layout_without_changing_bytes() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let root = LogEntry {
            id: "root".to_string(),
            parent_id: None,
            timestamp: None,
            thread: ThreadKind::Meta,
            agent_id: None,
            entry: LogEntryKind::SystemPrompt { text: "p".into() },
        };
        let message = LogEntry {
            id: "message".to_string(),
            parent_id: Some("root".to_string()),
            timestamp: None,
            thread: ThreadKind::Main,
            agent_id: None,
            entry: LogEntryKind::Message {
                message: user_text("punctuation"),
            },
        };
        let env_entry = |id: &str, parent: &str, thread: ThreadKind| LogEntry {
            id: id.to_string(),
            parent_id: Some(parent.to_string()),
            timestamp: None,
            thread,
            agent_id: None,
            entry: LogEntryKind::EnvChange {
                env: env_map(&[("BEADS_ACTOR", "session-actor")]),
            },
        };
        let cases = [
            (
                "2000-01-01-00-00-00-000_9",
                vec![
                    root.clone(),
                    env_entry("env", "root", ThreadKind::Meta),
                    env_entry("env", "root", ThreadKind::Meta),
                    message.clone(),
                ],
            ),
            (
                "2000-01-01-00-00-00-000_1",
                vec![
                    root.clone(),
                    message.clone(),
                    env_entry("env", "root", ThreadKind::Meta),
                ],
            ),
            (
                "2000-01-01-00-00-00-000_2",
                vec![
                    root.clone(),
                    env_entry("env", "root", ThreadKind::Sub),
                    message.clone(),
                ],
            ),
            (
                "2000-01-01-00-00-00-000_3",
                vec![
                    root.clone(),
                    env_entry("env", "wrong-parent", ThreadKind::Meta),
                    message.clone(),
                ],
            ),
            (
                "2000-01-01-00-00-00-000_4",
                vec![
                    root.clone(),
                    env_entry("env-1", "root", ThreadKind::Meta),
                    env_entry("env-2", "root", ThreadKind::Meta),
                    message.clone(),
                ],
            ),
            (
                "2000-01-01-00-00-00-000_5",
                vec![root.clone(), env_entry("env", "root", ThreadKind::Meta)],
            ),
            (
                "2000-01-01-00-00-00-000_6",
                vec![
                    root.clone(),
                    env_entry("root", "root", ThreadKind::Meta),
                    message.clone(),
                ],
            ),
            (
                "2000-01-01-00-00-00-000_7",
                vec![
                    root,
                    env_entry("env", "root", ThreadKind::Meta),
                    LogEntry {
                        id: "second-prompt".to_string(),
                        parent_id: Some("root".to_string()),
                        timestamp: None,
                        thread: ThreadKind::Meta,
                        agent_id: None,
                        entry: LogEntryKind::SystemPrompt {
                            text: "ambiguous".to_string(),
                        },
                    },
                    message,
                ],
            ),
        ];

        for (session_id, entries) in cases {
            let bytes = entries
                .iter()
                .map(|entry| serde_json::to_string(entry).expect("entry JSON"))
                .collect::<Vec<_>>()
                .join("\n")
                .into_bytes();
            let path = crate::legacy::session_path(&persistence, session_id);
            std::fs::write(&path, &bytes).expect("write malformed env layout");
            let err = SessionLog::resume(&persistence, session_id)
                .err()
                .expect("malformed creation identity is refused");
            assert!(matches!(err, SessionError::Corrupt(_)), "{err}");
            assert_eq!(
                std::fs::read(&path).expect("source bytes"),
                bytes,
                "resume changed malformed env layout {session_id}"
            );
        }
    }

    #[test]
    fn invalid_recorded_env_is_refused_before_a_torn_tail_is_truncated() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let session_id = "2000-01-01-00-00-00-001";
        let root = LogEntry {
            id: "root".to_string(),
            parent_id: None,
            timestamp: None,
            thread: ThreadKind::Meta,
            agent_id: None,
            entry: LogEntryKind::SystemPrompt { text: "p".into() },
        };
        for thread in [ThreadKind::Main, ThreadKind::Meta] {
            let invalid_env = LogEntry {
                id: "env".to_string(),
                parent_id: Some("root".to_string()),
                timestamp: None,
                thread,
                agent_id: None,
                entry: LogEntryKind::EnvChange {
                    env: env_map(&[("", "value")]),
                },
            };
            // A repairable torn tail: complete canonical envelope anchors, then
            // parser EOF inside the payload. Only the earlier invalid env entry
            // must keep resume from mutating it.
            let bytes = format!(
            "{}\n{}\n{{\"id\":\"torn\",\"parent_id\":\"env\",\"timestamp\":null,\"thread\":\"user\",\"type\":\"message\",\"message\":{{\"ro",
            serde_json::to_string(&root).expect("root"),
            serde_json::to_string(&invalid_env).expect("env")
        )
        .into_bytes();
            let path = crate::legacy::session_path(&persistence, session_id);
            std::fs::write(&path, &bytes).expect("write invalid identity and torn tail");

            let err = SessionLog::resume(&persistence, session_id)
                .err()
                .expect("invalid identity wins over tail repair");
            assert!(
                err.to_string().contains("environment key \"\" is empty"),
                "{err}"
            );
            assert_eq!(
                std::fs::read(&path).expect("source bytes"),
                bytes,
                "tail repair ran before identity validation"
            );
        }
    }

    fn test_goal() -> Goal {
        Goal {
            id: "goal-1".into(),
            objective: "Implement the requested feature".into(),
            status: aj_agent::goal::GoalStatus::Active,
            token_budget: Some(10_000),
            tokens_used: 123,
            time_used_seconds: 45,
        }
    }

    #[test]
    fn goals_inherit_at_the_fork_point_and_round_trip_with_clears() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create");
        let root = log.set_system_prompt("p".into()).expect("root");
        assert_eq!(log.goal_at(&root.id).unwrap(), None);
        let original = test_goal();
        let seed = log
            .append_goal_change(Some(original.clone()))
            .expect("goal");
        assert_eq!(log.parent_of(&seed.id), Some(&root.id));
        assert_eq!(log.head(), Some(&seed.id));
        assert!(
            log.is_durable(),
            "a submitted goal must survive before the first inference"
        );
        let saved = SessionLog::resume(&persistence, log.session_id()).expect("saved goal");
        assert_eq!(saved.goal_at(&seed.id).unwrap(), Some(original.clone()));
        let common = say(&mut log, "common").expect("publish");
        let edited_goal = Goal {
            status: aj_agent::goal::GoalStatus::Paused,
            tokens_used: 456,
            token_budget: None,
            ..original.clone()
        };
        let edited = log
            .append_goal_change(Some(edited_goal.clone()))
            .expect("edit");
        log.set_head(common.id.clone()).expect("fork");
        let sibling = say(&mut log, "sibling").expect("sibling");
        assert_eq!(log.goal_at(&sibling.id).unwrap(), Some(original.clone()));
        let cleared = log.append_goal_change(None).expect("clear");
        assert_eq!(log.parent_of(&cleared.id), Some(&sibling.id));
        assert_eq!(log.goal_at(&cleared.id).unwrap(), None);
        assert_eq!(log.goal_at(&edited.id).unwrap(), Some(edited_goal.clone()));
        let resumed = SessionLog::resume(&persistence, log.session_id()).expect("resume");
        assert_eq!(resumed.head(), Some(&cleared.id));
        for (head, expected) in [
            (&root.id, None),
            (&common.id, Some(original.clone())),
            (&sibling.id, Some(original)),
            (&edited.id, Some(edited_goal)),
            (&cleared.id, None),
        ] {
            assert_eq!(resumed.goal_at(head).unwrap(), expected);
        }
    }

    #[test]
    fn goal_metadata_survives_compaction_without_entering_messages_or_replay() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create");
        log.set_system_prompt("p".into()).expect("root");
        let goal = test_goal();
        let seed = log.append_goal_change(Some(goal.clone())).expect("goal");
        let conv = log.linearize(&seed.id, ThreadFilter::MAIN);
        assert!(conv.messages().is_empty());
        assert!(conv.agent_messages().is_empty());
        assert!(crate::replay::replay(&log).next().is_none());
        say(&mut log, "summarized").expect("prefix");
        let tail = say(&mut log, "retained").expect("tail");
        let compacted = log
            .append_compaction(
                ThreadFilter::MAIN,
                "summary".into(),
                tail.id,
                vec![],
                1000,
                None,
                aj_models::types::Usage::default(),
            )
            .expect("compact");
        let resumed = SessionLog::resume(&persistence, log.session_id()).expect("resume");
        let conv = resumed.linearize(&compacted.id, ThreadFilter::MAIN);
        assert_eq!(
            conv.messages().len(),
            2,
            "only summary and retained message"
        );
        assert_eq!(resumed.goal_at(&compacted.id).unwrap(), Some(goal));
    }

    #[test]
    fn env_replacements_follow_branches_and_resume_as_the_last_record() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create");
        let root = log.set_system_prompt("p".into()).expect("root");
        let original = env_map(&[("KEEP", "old"), ("DROP", "old")]);
        let seed = log.append_env_change(original.clone()).expect("seed");
        let entry = log.core().get(&seed.id).expect("seed entry");
        assert_eq!(entry.thread, ThreadKind::Main);
        assert_eq!(entry.parent_id.as_ref(), Some(&root.id));
        assert_eq!(log.head(), Some(&seed.id));
        let common = say(&mut log, "common").expect("publish");
        let replacement = env_map(&[("KEEP", "new")]);
        log.append_env_change(replacement.clone()).expect("replace");
        let edited = log.append_env_change(replacement.clone()).expect("repeat");
        assert_eq!(log.session_env(), Some(&replacement));
        assert!(!log.session_env().unwrap().contains_key("DROP"));
        log.set_head(common.id.clone()).expect("fork");
        assert_eq!(log.session_env(), Some(&original));
        let empty = log.append_env_change(BTreeMap::new()).expect("clear");
        assert_eq!(log.parent_of(&empty.id), Some(&common.id));
        assert_eq!(log.session_env(), Some(&BTreeMap::new()));
        // The edit waits in the branch's draft. The branch's first message
        // stores both.
        let forked = say(&mut log, "forked").expect("publish");
        let mut resumed =
            SessionLog::resume(&persistence, log.session_id()).expect("resume trailing edit");
        assert_eq!(resumed.head(), Some(&forked.id));
        assert_eq!(resumed.session_env(), Some(&BTreeMap::new()));
        // The environment of a branch the log does not hold is read from
        // the store, and is what the branch says once the log is on it.
        let elsewhere = resumed.session_env_at(Some(&edited.id));
        assert_eq!(elsewhere.expect("read"), Some(replacement.clone()));
        switch(&mut resumed, edited.id);
        assert_eq!(resumed.session_env(), Some(&replacement));
        switch(&mut resumed, root.id);
        assert_eq!(resumed.session_env(), None);
    }

    #[test]
    fn legacy_meta_env_is_a_baseline_not_a_branch_override() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create");
        let root = log.set_system_prompt("p".into()).expect("root");
        let baseline = env_map(&[("BASE", "value")]);
        log.append(
            Some(root.id.clone()),
            ThreadKind::Meta,
            None,
            LogEntryKind::EnvChange {
                env: baseline.clone(),
            },
        )
        .expect("legacy env");
        let common = say(&mut log, "common").expect("publish");
        let original = fingerprint(&log);
        let mut resumed =
            SessionLog::resume(&persistence, log.session_id()).expect("legacy resume");
        assert_eq!(fingerprint(&resumed), original);
        assert_eq!(resumed.session_env(), Some(&baseline));
        let edited = resumed
            .append_env_change(BTreeMap::new())
            .expect("override");
        assert_eq!(resumed.session_env(), Some(&BTreeMap::new()));
        resumed.set_head(common.id).expect("sibling");
        say(&mut resumed, "sibling").expect("publish edit and sibling");
        assert_eq!(resumed.session_env(), Some(&baseline));
        let mut resumed = SessionLog::resume(&persistence, log.session_id()).expect("mixed resume");
        assert_eq!(resumed.session_env(), Some(&baseline));
        switch(&mut resumed, edited.id);
        assert_eq!(resumed.session_env(), Some(&BTreeMap::new()));
        switch(&mut resumed, root.id);
        assert_eq!(resumed.session_env(), Some(&baseline));
    }

    #[test]
    fn resumed_user_env_duplicates_keep_the_first_state_record() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create");
        log.set_system_prompt("p".into()).expect("root");
        say(&mut log, "published").expect("publish");
        let env = log
            .append_env_change(env_map(&[("KEY", "first")]))
            .expect("env");
        let mut duplicate = log.core().get(&env.id).unwrap().clone();
        duplicate.entry = LogEntryKind::EnvChange {
            env: BTreeMap::new(),
        };
        use std::io::Write;
        let (legacy, path) = as_jsonl(&dir, &persistence, log.session_id());
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{}", serde_json::to_string(&duplicate).unwrap()).unwrap();
        let resumed = SessionLog::resume(&legacy, log.session_id()).expect("ordinary duplicate");
        assert_eq!(resumed.len(), log.len());
        assert_eq!(resumed.session_env(), log.session_env());
    }

    #[test]
    fn env_extraction_carries_the_seed_past_a_real_compaction_boundary() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let expected = env_map(&[("BEADS_ACTOR", "original-actor")]);
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        let seed = log
            .append_env_change(expected.clone())
            .expect("append env seed");
        let first_kept = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("summarized question"))
                .expect("old user");
            view.add_message(assistant_text("summarized answer"))
                .expect("old assistant");
            view.add_message(user_text("retained question"))
                .expect("first retained entry")
        };
        assert!(
            seed.seq < first_kept.seq,
            "env seed {} is not strictly before first_kept_entry_id {}; the fixture measures no compaction carry",
            seed.id,
            first_kept.id,
        );
        log.append_compaction(
            ThreadFilter::MAIN,
            "SUMMARY".into(),
            first_kept.id,
            Vec::new(),
            1_000,
            None,
            aj_models::types::Usage::default(),
        )
        .expect("append compaction");

        let head = log
            .latest_leaf(ThreadFilter::MAIN)
            .expect("post-compaction head");
        assert!(
            log.contains(&head),
            "compaction did not produce a real head"
        );
        assert_eq!(
            log.session_env(),
            Some(&expected),
            "compaction hid the ancestral environment"
        );
        let resumed =
            SessionLog::resume(&persistence, log.session_id()).expect("resume compaction");
        assert_eq!(resumed.session_env(), Some(&expected));
    }

    #[test]
    fn env_seed_is_shared_by_a_branch_forked_after_it() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let expected = env_map(&[("BEADS_ACTOR", "original-actor")]);
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        let seed = log
            .append_env_change(expected.clone())
            .expect("append env seed");
        let common = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("common root")).expect("common")
        };
        assert!(
            seed.seq < common.seq,
            "the branch point does not follow the env seed"
        );
        let original_tail = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(assistant_text("original tail"))
                .expect("original")
                .id
        };

        log.set_head(common.id.clone())
            .expect("switch back to fork point");
        let branch_head = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("other branch"))
                .expect("branch")
                .id
        };
        assert_ne!(branch_head, original_tail, "the fixture did not fork");
        assert_eq!(
            log.parent_of(&original_tail),
            Some(&common.id),
            "the original tail does not descend from the fork point"
        );
        assert_eq!(
            log.parent_of(&branch_head),
            Some(&common.id),
            "the new tail extended the original branch instead of forming a sibling"
        );
        assert_eq!(
            log.head(),
            Some(&branch_head),
            "the new branch is not active"
        );
        assert_eq!(
            log.session_env(),
            Some(&expected),
            "a branch lost its ancestral seed"
        );
    }

    #[test]
    fn account_pins_and_resets_round_trip_independently() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create");
        let root = log.set_system_prompt("p".into()).expect("root");
        let mut checkpoints = vec![(root.id, BTreeMap::new())];
        for (provider, account, expected) in [
            (
                "anthropic",
                Some(" Work 界 "),
                vec![("anthropic", " Work 界 ")],
            ),
            (
                "openai",
                Some(""),
                vec![("anthropic", " Work 界 "), ("openai", "")],
            ),
            (
                "anthropic",
                Some("personal"),
                vec![("anthropic", "personal"), ("openai", "")],
            ),
            ("anthropic", None, vec![("openai", "")]),
            ("anthropic", None, vec![("openai", "")]),
            ("openai", None, vec![]),
        ] {
            let previous_head = log
                .head()
                .cloned()
                .or_else(|| log.system_prompt_id().cloned());
            let at = log
                .append_account_change(provider, account)
                .expect("account change");
            assert_eq!(log.head(), Some(&at.id));
            assert_eq!(log.parent_of(&at.id), previous_head.as_ref());
            assert_eq!(log.last_seq(), at.seq);
            let entry = log.entries_in_order().pop().expect("appended entry");
            assert_eq!(entry.thread, ThreadKind::Main);
            assert_eq!(entry.agent_id, None);
            let json = serde_json::to_value(entry).expect("serialize account change");
            assert_eq!(json["type"], "account_change");
            assert_eq!(json["provider"], provider);
            assert_eq!(json["account"], serde_json::json!(account));
            checkpoints.push((
                at.id,
                expected
                    .into_iter()
                    .map(|(p, a)| (p.into(), a.into()))
                    .collect(),
            ));
        }
        assert!(
            !log.path().exists(),
            "account choices are ordinary buffered state"
        );
        let tip = say(&mut log, "publish choices").expect("message");
        let resumed = SessionLog::resume(&persistence, log.session_id()).expect("resume");
        for reader in [&log, &resumed] {
            for (head, expected) in &checkpoints {
                let branch = reader.linearize(head, ThreadFilter::MAIN);
                assert_eq!(&branch.settings().accounts, expected);
                assert!(
                    branch.messages().is_empty(),
                    "account state is not model input"
                );
            }
            assert_eq!(
                reader
                    .linearize(&tip.id, ThreadFilter::MAIN)
                    .message_count(),
                1
            );
        }
    }

    #[test]
    fn account_settings_follow_the_selected_branch_on_resume() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create");
        log.set_system_prompt("p".into()).expect("root");
        let legacy = say(&mut log, "no account history").expect("legacy message");
        log.append_account_change("anthropic", Some("work"))
            .expect("pin");
        log.append_account_change("openai", Some("shared"))
            .expect("other provider");
        let fork = say(&mut log, "fork here").expect("fork point");
        log.append_account_change("anthropic", None).expect("reset");
        let reset_tip = say(&mut log, "default branch").expect("reset branch");
        log.set_head(fork.id.clone()).expect("select fork");
        let unnamed = log
            .append_account_change("anthropic", Some(""))
            .expect("unnamed pin");
        assert_eq!(log.parent_of(&unnamed.id), Some(&fork.id));
        let unnamed_tip = say(&mut log, "unnamed branch").expect("unnamed branch");
        log.append_compaction(
            ThreadFilter::MAIN,
            "summary".into(),
            unnamed_tip.id,
            Vec::new(),
            100,
            None,
            aj_models::types::Usage::default(),
        )
        .expect("compact branch");
        let session_id = log.session_id().to_string();
        let active_tip = log.head().cloned().expect("active tip");
        drop(log);

        let mut resumed = SessionLog::resume(&persistence, &session_id).expect("resume");
        assert_eq!(resumed.head(), Some(&active_tip));
        for (head, expected) in [
            (active_tip, vec![("anthropic", ""), ("openai", "shared")]),
            (reset_tip.id, vec![("openai", "shared")]),
            (fork.id, vec![("anthropic", "work"), ("openai", "shared")]),
            (legacy.id, vec![]),
        ] {
            // A reopened log holds its selected branch. Another is read first.
            if let Some(read) = resumed.branch_read(&head).expect("plan the read") {
                resumed
                    .install_branch(read.read().expect("read the branch"))
                    .expect("hold the branch");
            }
            resumed.set_head(head).expect("select branch");
            let settings = resumed
                .linearize(resumed.head().unwrap(), ThreadFilter::MAIN)
                .settings();
            assert_eq!(
                settings.accounts,
                expected
                    .into_iter()
                    .map(|(p, a)| (p.into(), a.into()))
                    .collect()
            );
        }
    }

    #[test]
    fn settings_entries_in_linearize_but_skipped_by_messages() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        log.append_model_change(ThreadFilter::MAIN, "anthropic", "claude-x", 0)
            .expect("model change");
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("user msg");
        }

        let head = log.latest_leaf(ThreadFilter::MAIN).expect("head");
        let convo = log.linearize(&head, ThreadFilter::MAIN);
        assert_eq!(convo.entries().len(), 2);
        assert!(matches!(
            convo.entries()[0].entry,
            LogEntryKind::ModelChange { .. }
        ));
        assert_eq!(convo.message_count(), 1);
        assert_eq!(convo.messages().len(), 1);
    }

    #[test]
    fn settings_last_wins_per_axis() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        log.append_model_change(ThreadFilter::MAIN, "anthropic", "claude-x", 0)
            .expect("mc1");
        log.append_thinking_change(ThreadFilter::MAIN, "low")
            .expect("tc1");
        log.append_speed_change(ThreadFilter::MAIN, "standard")
            .expect("sc1");
        log.append_model_change(ThreadFilter::MAIN, "openai", "gpt-y", 0)
            .expect("mc2");
        log.append_thinking_change(ThreadFilter::MAIN, "off")
            .expect("tc2");
        log.append_speed_change(ThreadFilter::MAIN, "fast")
            .expect("sc2");
        log.append_verbosity_change(ThreadFilter::MAIN, "default")
            .expect("vc1");
        log.append_verbosity_change(ThreadFilter::MAIN, "high")
            .expect("vc2");

        let head = log.latest_leaf(ThreadFilter::MAIN).expect("head");
        let settings = log.linearize(&head, ThreadFilter::MAIN).settings();
        assert_eq!(
            settings.model,
            Some(("openai".to_string(), "gpt-y".to_string()))
        );
        // "off" was explicitly recorded — distinct from None.
        assert_eq!(settings.thinking.as_deref(), Some("off"));
        assert_eq!(settings.speed.as_deref(), Some("fast"));
        assert_eq!(settings.verbosity.as_deref(), Some("high"));
    }

    #[test]
    fn settings_assistant_message_fallback_for_model() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("u");
            view.add_message(assistant_from("anthropic", "claude-a"))
                .expect("a1");
            view.add_message(user_text("more")).expect("u2");
            view.add_message(assistant_from("openai", "gpt-b"))
                .expect("a2");
        }

        let head = log.latest_leaf(ThreadFilter::MAIN).expect("head");
        let settings = log.linearize(&head, ThreadFilter::MAIN).settings();
        assert_eq!(
            settings.model,
            Some(("openai".to_string(), "gpt-b".to_string()))
        );
        assert_eq!(settings.thinking, None);
        assert_eq!(settings.speed, None);
    }

    #[test]
    fn settings_model_change_after_assistant_message_wins() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("u");
            view.add_message(assistant_from("anthropic", "claude-a"))
                .expect("a");
        }
        log.append_model_change(ThreadFilter::MAIN, "openai", "gpt-b", 0)
            .expect("mc");

        let head = log.latest_leaf(ThreadFilter::MAIN).expect("head");
        let settings = log.linearize(&head, ThreadFilter::MAIN).settings();
        assert_eq!(
            settings.model,
            Some(("openai".to_string(), "gpt-b".to_string()))
        );
    }

    #[test]
    fn settings_assistant_message_after_model_change_wins() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        log.append_model_change(ThreadFilter::MAIN, "openai", "gpt-b", 0)
            .expect("mc");
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("u");
            view.add_message(assistant_from("anthropic", "claude-a"))
                .expect("a");
        }

        let head = log.latest_leaf(ThreadFilter::MAIN).expect("head");
        let settings = log.linearize(&head, ThreadFilter::MAIN).settings();
        assert_eq!(
            settings.model,
            Some(("anthropic".to_string(), "claude-a".to_string()))
        );
    }

    #[test]
    fn subagent_settings_entries_excluded_from_user_linearize() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        let user_id = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("u").id
        };
        let sub_id = {
            let mut view = AgentLog::sub(&mut log, user_id, 1);
            view.add_message(user_text("subtask"))
                .expect("sub prompt")
                .id
        };
        log.append_model_change(ThreadFilter::sub(1), "openai", "gpt-sub", 0)
            .expect("sub mc");
        log.append_thinking_change(ThreadFilter::sub(1), "low")
            .expect("sub tc");

        // Sub-agent thread sees its own settings.
        let sub_head = log.latest_leaf(ThreadFilter::sub(1)).expect("sub head");
        let sub_settings = log.linearize(&sub_head, ThreadFilter::sub(1)).settings();
        assert_eq!(
            sub_settings.model,
            Some(("openai".to_string(), "gpt-sub".to_string()))
        );
        assert_eq!(sub_settings.thinking.as_deref(), Some("low"));
        let _ = sub_id;

        // The main-thread scan does not.
        let user_head = log.latest_leaf(ThreadFilter::MAIN).expect("user head");
        let user_settings = log.linearize(&user_head, ThreadFilter::MAIN).settings();
        assert_eq!(user_settings.model, None);
        assert_eq!(user_settings.thinking, None);
    }

    #[test]
    fn append_settings_anchors_to_system_prompt_root_and_chains() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        let sp_id = log.set_system_prompt("p".into()).expect("set sp").id;

        let mc_id = log
            .append_model_change(ThreadFilter::MAIN, "anthropic", "claude-x", 0)
            .expect("model change")
            .id;
        let mc_entry = log.core.get(&mc_id).expect("entry exists");
        assert_eq!(mc_entry.parent_id.as_ref(), Some(&sp_id));
        assert!(matches!(mc_entry.thread, ThreadKind::Main));
        assert!(mc_entry.agent_id.is_none());

        // The next message chains onto the settings entry.
        let user_id = {
            let head = log.latest_leaf(ThreadFilter::MAIN);
            assert_eq!(head.as_ref(), Some(&mc_id));
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("user msg").id
        };
        let user_entry = log.core.get(&user_id).expect("entry exists");
        assert_eq!(user_entry.parent_id.as_ref(), Some(&mc_id));
    }

    fn spawn_settings() -> aj_agent::events::AgentSettings {
        aj_agent::events::AgentSettings {
            context_window: 0,
            provider: "anthropic".to_string(),
            model_id: "claude-x".to_string(),
            thinking: "high".to_string(),
            thinking_display: String::new(),
            speed: "fast".to_string(),
            verbosity: "high".to_string(),
        }
    }

    #[test]
    fn subagent_spawn_round_trips_through_resume() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let session_id = {
            let mut log = SessionLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".into()).expect("set sp");
            let user_id = {
                let mut view = AgentLog::main(&mut log);
                view.add_message(user_text("hi")).expect("u").id
            };
            log.append_subagent_spawn(
                1,
                user_id,
                "subtask",
                "agent",
                None,
                true,
                &spawn_settings(),
            )
            .expect("spawn entry");
            {
                let sub_head = log.latest_leaf(ThreadFilter::sub(1)).expect("sub leaf");
                let mut view = AgentLog::sub(&mut log, sub_head, 1);
                view.add_message(user_text("subtask")).expect("sub prompt");
            }
            log.session_id().to_string()
        };

        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume");
        let sub_head = resumed.latest_leaf(ThreadFilter::sub(1)).expect("sub leaf");
        let convo = resumed.linearize(&sub_head, ThreadFilter::sub(1));
        match &convo.entries()[0].entry {
            LogEntryKind::SubAgentSpawn {
                task,
                background,
                settings,
                ..
            } => {
                assert_eq!(task, "subtask");
                assert!(*background, "background mode round-trips through resume");
                assert_eq!(*settings, spawn_settings());
            }
            other => panic!("expected SubAgentSpawn, got {other:?}"),
        }
    }

    #[test]
    fn subagent_spawn_without_metadata_defaults_to_ordinary_foreground() {
        // Resume deserializes whole `LogEntry` lines, and
        // the spawn kind is `#[serde(flatten)]`-ed into that wrapper over an
        // internally-tagged enum. That flatten + tag + `#[serde(default)]`
        // combination is the real read path (and a known serde trap), so we
        // exercise it through the wrapper rather than the kind in isolation.
        let record = LogEntry {
            id: "0000abcd".to_string(),
            parent_id: Some("00000001".to_string()),
            timestamp: None,
            thread: ThreadKind::Sub,
            agent_id: Some(1),
            entry: LogEntryKind::SubAgentSpawn {
                task: "t".to_string(),
                tool_name: "oracle".into(),
                call_id: Some("call-9".into()),
                background: true,
                settings: spawn_settings(),
            },
        };
        let mut json = serde_json::to_value(&record).expect("serialize entry");
        assert!(
            json.as_object_mut()
                .expect("entry is a JSON object")
                .remove("background")
                .is_some(),
            "background sits at the flattened top level before removal"
        );
        assert_eq!(
            json.as_object_mut().unwrap().remove("tool_name"),
            Some(serde_json::json!("oracle"))
        );
        assert_eq!(
            json.as_object_mut().unwrap().remove("call_id"),
            Some(serde_json::json!("call-9"))
        );
        let restored: LogEntry = serde_json::from_value(json).expect("legacy line deserializes");
        match restored.entry {
            LogEntryKind::SubAgentSpawn {
                background,
                tool_name,
                call_id,
                ..
            } => {
                assert!(!background, "missing background must default to foreground");
                assert!(tool_name.is_empty(), "missing origin is unspecified");
                assert_eq!(call_id, None, "a missing call names no call");
            }
            other => panic!("expected SubAgentSpawn, got {other:?}"),
        }
    }

    #[test]
    fn subagent_spawn_is_not_punctuation() {
        // Spawn entries buffer like the other meta entries: they
        // must not materialize the log file on their own.
        let spawn = LogEntryKind::SubAgentSpawn {
            task: "t".to_string(),
            tool_name: "agent".into(),
            call_id: None,
            background: false,
            settings: spawn_settings(),
        };
        assert!(!spawn.is_work());
    }

    #[test]
    fn subagent_spawn_snapshot_feeds_settings() {
        // settings() on a sub-agent linearize picks up all three
        // axes from the spawn snapshot.
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        let user_id = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("u").id
        };
        log.append_subagent_spawn(
            1,
            user_id,
            "subtask",
            "agent",
            None,
            false,
            &spawn_settings(),
        )
        .expect("spawn entry");

        let sub_head = log.latest_leaf(ThreadFilter::sub(1)).expect("sub leaf");
        let settings = log.linearize(&sub_head, ThreadFilter::sub(1)).settings();
        assert_eq!(
            settings.model,
            Some(("anthropic".to_string(), "claude-x".to_string()))
        );
        assert_eq!(settings.thinking.as_deref(), Some("high"));
        assert_eq!(settings.speed.as_deref(), Some("fast"));

        // The main-thread scan does not see the spawn snapshot.
        let user_head = log.latest_leaf(ThreadFilter::MAIN).expect("user head");
        let user_settings = log.linearize(&user_head, ThreadFilter::MAIN).settings();
        assert_eq!(user_settings.model, None);
    }

    #[test]
    fn assistant_and_tool_result_count_toward_messages() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("u");
            view.add_message(assistant_text("hello")).expect("a");
            view.add_message(tool_result("tu-1", "ping", "ok"))
                .expect("tr");
        }
        let head = log.latest_leaf(ThreadFilter::MAIN).expect("head exists");
        let convo = log.linearize(&head, ThreadFilter::MAIN);
        assert_eq!(convo.message_count(), 3);
    }

    #[test]
    fn a_compaction_line_carrying_usage_parses_the_recorded_spend() {
        // The positive half, and the only test that pins the on-disk KEY.
        // A round-trip cannot: serde renames serializing and deserializing
        // together, so a renamed field still round-trips in one binary
        // while orphaning the usage on every file already written. This
        // line is the writer's own output with the ids and timestamp
        // normalized, so the names below are the names on disk.
        let line = r#"{"id":"00000001","parent_id":"00000000","timestamp":"2026-07-01T00:00:00Z","thread":"user","type":"compaction","summary":"older summary","first_kept_entry_id":"00000000","tokens_before":4321,"usage":{"input":40000,"output":900,"cache_read":1200,"cache_write":300,"total_tokens":42400,"cost":{"input":0.12,"output":0.0135,"cache_read":0.0004,"cache_write":0.001,"total":0.1349}}}"#;

        let entry: LogEntry =
            serde_json::from_str(line).expect("a compaction line with usage parses");
        match entry.entry {
            LogEntryKind::Compaction { usage, .. } => {
                // Each count by name: a rename inside `Usage` orphans the
                // field it names just as silently as one on the outside.
                assert_eq!(usage.input, 40_000);
                assert_eq!(usage.output, 900);
                assert_eq!(usage.cache_read, 1_200);
                assert_eq!(usage.cache_write, 300);
                assert_eq!(usage.total_tokens, 42_400);
                assert!(
                    (usage.cost.total - 0.1349).abs() < 1e-9,
                    "the recorded dollars survive the read, got {}",
                    usage.cost.total
                );
            }
            other => panic!("expected a Compaction entry, got {other:?}"),
        }
    }

    #[test]
    fn append_compaction_flushes_and_round_trips() {
        // A `Compaction` entry stores the session: appending it must
        // commit at once and survive a resume with all its fields
        // intact.
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());

        let (session_id, first_kept, retained_user) = {
            let mut log = SessionLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".into()).expect("set sp");

            let (first_kept, retained_user) = {
                let mut view = AgentLog::main(&mut log);
                let retained = view.add_message(user_text("one")).expect("u1").id;
                view.add_message(assistant_text("a1")).expect("a1");
                (view.add_message(user_text("two")).expect("u2").id, retained)
            };

            let details = crate::compaction::CompactionDetails {
                read_files: vec!["/tmp/a".into()],
                modified_files: vec!["/tmp/b".into()],
            };
            let mut usage = aj_models::types::Usage {
                input: 40_000,
                output: 900,
                cache_read: 1_000,
                cache_write: 200,
                total_tokens: 42_100,
                ..Default::default()
            };
            usage.cost.total = 0.25;
            log.append_compaction(
                ThreadFilter::MAIN,
                "the summary".into(),
                first_kept.clone(),
                vec![retained_user.clone()],
                1234,
                Some(details),
                usage,
            )
            .expect("append compaction");

            assert!(
                stored(&persistence, &log),
                "compaction is punctuation; it is stored right after append"
            );

            let records = persistence.read_entries(log.session_id()).unwrap();
            let checkpoint = serde_json::to_value(records.last().unwrap()).unwrap();
            assert_eq!(
                checkpoint["retained_user_entry_ids"],
                serde_json::json!([retained_user])
            );
            (log.session_id().to_string(), first_kept, retained_user)
        };

        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume");
        let head = resumed.latest_leaf(ThreadFilter::MAIN).expect("head");
        let convo = resumed.linearize(&head, ThreadFilter::MAIN);
        let last = convo.entries().last().expect("entries present");
        match &last.entry {
            LogEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                retained_user_entry_ids,
                tokens_before,
                details,
                usage,
            } => {
                assert_eq!(summary, "the summary");
                assert_eq!(first_kept_entry_id, &first_kept);
                assert_eq!(retained_user_entry_ids, &[retained_user]);
                assert_eq!(*tokens_before, 1234);
                // The durability edge: the summarizer's spend has no
                // other entry, so a field that does not survive the
                // round trip loses it outright.
                assert_eq!(usage.total_tokens, 42_100);
                assert!((usage.cost.total - 0.25).abs() < 1e-9);
                let details = details.as_ref().expect("details present");
                assert_eq!(details.read_files, vec!["/tmp/a".to_string()]);
                assert_eq!(details.modified_files, vec!["/tmp/b".to_string()]);
            }
            other => panic!("expected Compaction, got {other:?}"),
        }
    }

    #[test]
    fn append_compaction_rejects_unknown_first_kept_id() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("u");
        }
        let err = log
            .append_compaction(
                ThreadFilter::MAIN,
                "s".into(),
                "no-such-id".into(),
                Vec::new(),
                0,
                None,
                aj_models::types::Usage::default(),
            )
            .expect_err("must reject unknown first_kept id");
        assert!(matches!(err, SessionError::InvalidAppend(_)));
    }

    #[test]
    fn retained_compaction_users_cannot_cross_branches_or_duplicate_the_tail() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).unwrap();
        log.set_system_prompt("p".into()).unwrap();
        let (original, assistant, kept) = {
            let mut view = AgentLog::main(&mut log);
            let original = view.add_message(user_text("ORIGINAL")).unwrap().id;
            let assistant = view.add_message(assistant_text("WORK")).unwrap().id;
            let kept = view.add_message(user_text("TAIL")).unwrap().id;
            (original, assistant, kept)
        };
        log.set_head(assistant.clone()).unwrap();
        let sibling = AgentLog::main(&mut log)
            .add_message(user_text("SIBLING"))
            .unwrap()
            .id;
        log.set_head(kept.clone()).unwrap();
        let before = fingerprint(&log);
        for ids in [
            vec!["missing".into()],
            vec![assistant.clone()],
            vec![kept.clone()],
            vec![sibling.clone()],
            vec![original.clone(), original.clone()],
        ] {
            assert!(matches!(
                log.append_compaction(
                    ThreadFilter::MAIN,
                    "SUMMARY".into(),
                    kept.clone(),
                    ids,
                    0,
                    None,
                    aj_models::types::Usage::default(),
                ),
                Err(SessionError::InvalidAppend(_))
            ));
            assert_eq!(log.head(), Some(&kept));
            assert_eq!(fingerprint(&log), before);
        }

        // A malformed selection cannot escape the prefix or duplicate messages.
        let checkpoint = log
            .append(
                Some(kept.clone()),
                ThreadKind::Main,
                None,
                LogEntryKind::Compaction {
                    summary: "SUMMARY".into(),
                    first_kept_entry_id: kept.clone(),
                    retained_user_entry_ids: vec![
                        original.clone(),
                        original,
                        assistant,
                        sibling,
                        kept,
                        "missing".into(),
                    ],
                    tokens_before: 0,
                    details: None,
                    usage: Default::default(),
                },
            )
            .unwrap();
        let messages = log
            .linearize(&checkpoint.id, ThreadFilter::MAIN)
            .agent_messages();
        let json = serde_json::to_string(&messages).unwrap();
        assert_eq!(json.matches("ORIGINAL").count(), 1);
        assert_eq!(json.matches("TAIL").count(), 1);
        assert!(!json.contains("SIBLING") && !json.contains("WORK"));
    }

    #[test]
    fn agent_messages_drops_prefix_and_prepends_summary_after_compaction() {
        // Projection after a compaction: the summarized prefix is gone,
        // replaced by one synthetic wrapped-summary message, and the
        // retained tail (from `first_kept_entry_id` on) is verbatim.
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");

        let kept_user = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("old one")).expect("u1");
            view.add_message(assistant_text("old reply")).expect("a1");
            let kept = view.add_message(user_text("kept question")).expect("u2");
            view.add_message(assistant_text("kept reply")).expect("a2");
            kept.id
        };

        log.append_compaction(
            ThreadFilter::MAIN,
            "SUMMARY".into(),
            kept_user,
            Vec::new(),
            999,
            None,
            aj_models::types::Usage::default(),
        )
        .expect("compaction");

        let head = log.latest_leaf(ThreadFilter::MAIN).expect("head");
        let convo = log.linearize(&head, ThreadFilter::MAIN);
        let messages = convo.messages();

        // Synthetic summary + the two retained messages.
        assert_eq!(messages.len(), 3, "got: {messages:#?}");
        match &messages[0] {
            Message::User(u) => match &u.content[0] {
                UserContent::Text(t) => {
                    assert!(
                        t.text
                            .starts_with(crate::compaction::COMPACTION_SUMMARY_PREFIX)
                    );
                    assert!(t.text.contains("SUMMARY"));
                }
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected synthetic summary user message, got {other:?}"),
        }
        match &messages[1] {
            Message::User(u) => match &u.content[0] {
                UserContent::Text(t) => assert_eq!(t.text, "kept question"),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected kept user message, got {other:?}"),
        }
        match &messages[2] {
            Message::Assistant(a) => match &a.content[0] {
                AssistantContent::Text(t) => assert_eq!(t.text, "kept reply"),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected kept assistant message, got {other:?}"),
        }
    }

    #[test]
    fn compaction_projection_expands_retained_text_details() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");

        let content = "retained model-facing result\n".repeat(40);
        let body = format!("{content}\n");
        let first_kept = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("old request"))
                .expect("old user");
            view.add_message(assistant_text("old reply"))
                .expect("old assistant");
            let first_kept = view
                .add_message(user_text("retained request"))
                .expect("retained user");
            view.add_message(assistant_tool_use("tu-1", "read_file"))
                .expect("retained assistant");
            view.add_message(detailed_text_tool_result(
                "tu-1",
                "read_file",
                &content,
                "read_file retained.txt",
                &body,
            ))
            .expect("retained tool result");
            first_kept.id
        };
        log.append_compaction(
            ThreadFilter::MAIN,
            "SUMMARY".into(),
            first_kept,
            Vec::new(),
            999,
            None,
            aj_models::types::Usage::default(),
        )
        .expect("compaction");

        let head = log.latest_leaf(ThreadFilter::MAIN).expect("head");
        let branch = log.linearize(&head, ThreadFilter::MAIN);
        let raw_details = branch
            .entries()
            .iter()
            .find_map(|entry| match &entry.entry {
                LogEntryKind::Message { message }
                    if matches!(message.as_stored_wire(), Some(Message::ToolResult(_))) =>
                {
                    Some(agent_tool_result_details(message))
                }
                _ => None,
            })
            .expect("raw retained tool result");
        assert!(raw_details.get("body").is_none());
        assert_eq!(raw_details["body_ref"]["source"], "content_text");

        let agent_messages = branch.agent_messages();
        let projected_agent = agent_messages
            .iter()
            .find(|message| matches!(message.as_stored_wire(), Some(Message::ToolResult(_))))
            .expect("retained agent tool result");
        let agent_details = agent_tool_result_details(projected_agent);
        assert_eq!(agent_details["summary"], "read_file retained.txt");
        assert_eq!(agent_details["body"], body);
        assert!(agent_details.get("body_ref").is_none());

        let messages = branch.messages();
        let projected_wire = messages
            .iter()
            .find(|message| matches!(message, Message::ToolResult(_)))
            .expect("retained wire tool result");
        let wire_details = wire_tool_result_details(projected_wire);
        assert_eq!(wire_details["body"], body);
        assert!(wire_details.get("body_ref").is_none());
    }

    #[test]
    fn every_append_returns_its_one_based_position() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");

        let mut refs = vec![
            log.set_system_prompt("p".to_string())
                .expect("system prompt"),
            log.append_model_change(ThreadFilter::MAIN, "prov", "m", 0)
                .expect("model change"),
            log.append_thinking_change(ThreadFilter::MAIN, "high")
                .expect("thinking change"),
            log.append_speed_change(ThreadFilter::MAIN, "fast")
                .expect("speed change"),
            log.append_verbosity_change(ThreadFilter::MAIN, "low")
                .expect("verbosity change"),
        ];
        // Entries that do not store the session wait in memory, yet
        // their position is handed out right away: it is the append
        // index.
        assert!(
            !log.path().exists(),
            "settings entries must not materialize a file"
        );
        assert_eq!(log.last_seq(), 5);

        let user = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("user message")
        };
        refs.push(user.clone());
        let assistant = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(assistant_tool_use("tu-1", "ping"))
                .expect("assistant message")
        };
        refs.push(assistant.clone());
        let spawn = log
            .append_subagent_spawn(
                1,
                assistant.id.clone(),
                "task",
                "agent",
                None,
                false,
                &spawn_settings(),
            )
            .expect("spawn root");
        refs.push(spawn.clone());
        refs.push(
            log.append_compaction(
                ThreadFilter::MAIN,
                "sum".to_string(),
                user.id.clone(),
                Vec::new(),
                42,
                None,
                aj_models::types::Usage::default(),
            )
            .expect("compaction"),
        );
        refs.push(
            log.append(
                Some(spawn.id.clone()),
                ThreadKind::Sub,
                Some(1),
                LogEntryKind::Message {
                    message: user_text("subtask"),
                },
            )
            .expect("raw append"),
        );

        let total = u64::try_from(refs.len()).expect("fits u64");
        assert_eq!(
            refs.iter().map(|r| r.seq).collect::<Vec<_>>(),
            (1..=total).collect::<Vec<_>>(),
            "positions are dense and monotone across entry kinds"
        );
        let order: Vec<EntryId> = log
            .entries_in_order()
            .into_iter()
            .map(|entry| entry.id.clone())
            .collect();
        for entry_ref in &refs {
            let index = usize::try_from(entry_ref.seq).expect("fits usize") - 1;
            assert_eq!(order[index], entry_ref.id, "position indexes its own entry");
        }
        assert_eq!(log.last_seq(), total);
    }

    #[test]
    fn last_seq_starts_at_zero_and_tracks_the_last_append() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        assert_eq!(log.last_seq(), 0, "an empty log has nothing appended yet");

        let prompt = log
            .set_system_prompt("p".to_string())
            .expect("system prompt");
        assert_eq!(log.last_seq(), prompt.seq);

        let user = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("user message")
        };
        assert_eq!(log.last_seq(), user.seq);
        assert_eq!(log.last_seq(), 2);
    }

    #[test]
    fn snapshots_preserve_their_view_across_writes_and_owner_drop() {
        for resumed in [false, true] {
            let dir = fresh_sessions_dir();
            let persistence = ProjectSessions::new(dir.path().to_path_buf());
            let mut log = SessionLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".to_string())
                .expect("system prompt");
            let user = {
                let mut view = AgentLog::main(&mut log);
                view.add_message(user_text("hi")).expect("user message")
            };
            let assistant = {
                let mut view = AgentLog::main(&mut log);
                view.add_message(assistant_text("ho"))
                    .expect("assistant message")
            };
            log.append_subagent_spawn(
                1,
                assistant.id.clone(),
                "task",
                "agent",
                None,
                false,
                &spawn_settings(),
            )
            .expect("spawn root");

            if resumed {
                let session_id = log.session_id().to_string();
                drop(log);
                log = SessionLog::resume(&persistence, &session_id).expect("resume fixture");
            }
            let snapshot = log.snapshot();
            let original =
                serde_json::to_vec(&snapshot.entries_in_order()).expect("snapshot contents");
            let ids = |conv: &BranchView| -> Vec<EntryId> {
                conv.entries().iter().map(|e| e.id.clone()).collect()
            };
            assert_eq!(snapshot.session_id(), log.session_id());
            assert_eq!(snapshot.len(), log.len());
            assert_eq!(snapshot.last_seq(), log.last_seq());
            assert_eq!(snapshot.head(), log.head());
            assert_eq!(
                snapshot.latest_leaf(ThreadFilter::MAIN),
                log.latest_leaf(ThreadFilter::MAIN)
            );
            assert_eq!(
                snapshot.latest_leaf(ThreadFilter::sub(1)),
                log.latest_leaf(ThreadFilter::sub(1))
            );
            assert_eq!(
                ids(&snapshot.linearize(&assistant.id, ThreadFilter::MAIN)),
                ids(&log.linearize(&assistant.id, ThreadFilter::MAIN))
            );
            for index in 0..=log.len() {
                assert_eq!(
                    snapshot.entry_in_append_order(index).map(|e| &e.id),
                    log.core().entry_in_append_order(index).map(|e| &e.id),
                    "append-order slot {index} differs"
                );
            }

            // The snapshot is a value: appends to the log after it was taken
            // are invisible to it, which is what lets a projection run
            // outside the log lock.
            let later = {
                let mut view = AgentLog::main(&mut log);
                view.add_message(user_text("later")).expect("later message")
            };
            assert_eq!(snapshot.last_seq(), 4);
            assert_eq!(log.last_seq(), 5);
            assert!(snapshot.entry_in_append_order(4).is_none());
            assert_eq!(snapshot.head(), Some(&assistant.id));
            assert_eq!(log.head(), Some(&later.id));
            assert!(
                !ids(&log.linearize(&later.id, ThreadFilter::MAIN)).is_empty(),
                "the live log still linearizes its new head"
            );
            assert_eq!(
                ids(&snapshot.linearize(&assistant.id, ThreadFilter::MAIN)),
                vec![user.id.clone(), assistant.id.clone()],
            );
            log.set_head(user.id.clone()).expect("select branch point");
            let branch = AgentLog::main(&mut log)
                .add_message(assistant_text("another branch"))
                .expect("branch message");
            assert_eq!(log.head(), Some(&branch.id));
            let detached = snapshot.clone();
            drop(snapshot);
            drop(log);
            assert_eq!(detached.head(), Some(&assistant.id));
            assert!(!detached.contains(&later.id));
            assert!(!detached.contains(&branch.id));
            assert_eq!(
                serde_json::to_vec(&detached.entries_in_order()).expect("detached contents"),
                original,
                "snapshot contents outlive both the writer and the snapshot they were cloned from",
            );
            assert_eq!(
                ids(&detached.linearize(&assistant.id, ThreadFilter::MAIN)),
                vec![user.id, assistant.id],
            );
        }
    }

    #[test]
    fn create_hands_out_a_distinct_id_per_call_in_one_process() {
        // The same-millisecond case, deterministically: two mints from one
        // timestamp base. `create` is lazy, so neither path has a file on
        // disk for the existence check to catch.
        let dir = fresh_sessions_dir();
        let first =
            SessionLog::mint_session_id(dir.path(), "2026-01-01-00-00-00-000").expect("first mint");
        let second = SessionLog::mint_session_id(dir.path(), "2026-01-01-00-00-00-000")
            .expect("second mint");
        assert_ne!(first, second, "two mints from one base must differ");

        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut ids = std::collections::HashSet::new();
        let mut logs = Vec::new();
        for _ in 0..50 {
            let log = SessionLog::create(&persistence).expect("create log");
            assert!(!log.path().exists(), "create must not touch disk");
            assert!(
                ids.insert(log.session_id().to_string()),
                "create handed out a duplicate session id"
            );
            // Hold every log: a reservation has to outlive the call, not
            // just the loop iteration.
            logs.push(log);
        }
        assert_eq!(logs.len(), 50);
    }

    #[test]
    fn appended_ids_are_unique_within_a_log() {
        // The mint-and-retry path must never hand out a duplicate id
        // within one log, even across many appends.
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");

        let mut ids = std::collections::HashSet::new();
        {
            let mut view = AgentLog::main(&mut log);
            for i in 0..200 {
                let id = view
                    .add_message(user_text(&format!("m{i}")))
                    .expect("append message")
                    .id;
                assert!(ids.insert(id), "minted a duplicate id");
            }
        }
    }

    #[test]
    fn head_advances_on_user_append_and_ignores_subagent_append() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string()).expect("set sp");

        // A fresh log has no main-thread head; the system prompt is meta.
        assert!(log.head().is_none());

        let u1 = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("u1").id
        };
        assert_eq!(log.head(), Some(&u1), "user append advances the head");

        let a1 = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(assistant_tool_use("tu-1", "agent"))
                .expect("a1")
                .id
        };
        assert_eq!(log.head(), Some(&a1));

        // A sub-agent spawn and its messages must not touch the head.
        let spawn = log
            .append_subagent_spawn(
                1,
                a1.clone(),
                "task",
                "agent",
                None,
                false,
                &spawn_settings(),
            )
            .expect("spawn")
            .id;
        assert_eq!(log.head(), Some(&a1), "sub-agent spawn leaves the head");
        {
            let mut view = AgentLog::sub(&mut log, spawn, 1);
            view.add_message(user_text("subtask")).expect("sub u");
            view.add_message(assistant_text("sub done")).expect("sub a");
        }
        assert_eq!(
            log.head(),
            Some(&a1),
            "sub-agent messages leave the user head"
        );
    }

    #[test]
    fn set_head_to_earlier_entry_creates_sibling_branch() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());

        let (session_id, u1, tail) = {
            let mut log = SessionLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".to_string()).expect("set sp");
            let (u1, tail) = {
                let mut view = AgentLog::main(&mut log);
                let u1 = view.add_message(user_text("first")).expect("u1").id;
                let tail = view
                    .add_message(assistant_text("first reply"))
                    .expect("tail")
                    .id;
                (u1, tail)
            };

            // Move the head back to the first user message and branch.
            log.set_head(u1.clone()).expect("set head to u1");
            let branched = {
                let mut view = AgentLog::main(&mut log);
                view.add_message(user_text("branch")).expect("branch").id
            };
            // The new entry anchors at the earlier head, not the tail.
            let entry = log.core().get(&branched).expect("branched entry");
            assert_eq!(entry.parent_id.as_ref(), Some(&u1));
            (log.session_id().to_string(), u1, tail)
        };

        // The abandoned tail is still stored after re-resume. The reopened
        // log holds the branch that replaced it.
        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume");
        assert!(
            resumed.find(&tail).expect("look up").is_some(),
            "abandoned tail stays stored"
        );
        assert!(resumed.core().get(&u1).is_some());
    }

    #[test]
    fn set_head_validates_entry_thread() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let mut log = SessionLog::create(&persistence).expect("create log");
        let sp = log.set_system_prompt("p".to_string()).expect("set sp").id;

        let user_id = {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_text("hi")).expect("u").id
        };
        let spawn = log
            .append_subagent_spawn(
                1,
                user_id.clone(),
                "task",
                "agent",
                None,
                false,
                &spawn_settings(),
            )
            .expect("spawn")
            .id;
        let sub_msg = {
            let mut view = AgentLog::sub(&mut log, spawn.clone(), 1);
            view.add_message(user_text("subtask")).expect("sub u").id
        };

        // Accepts a user entry and the system-prompt meta entry.
        log.set_head(user_id.clone())
            .expect("user entry is a valid head");
        assert_eq!(log.head(), Some(&user_id));
        log.set_head(sp.clone())
            .expect("system-prompt entry is a valid head");
        assert_eq!(log.head(), Some(&sp));

        // Rejects a sub-agent message, its spawn root, and a missing id,
        // leaving the head unchanged.
        assert!(matches!(
            log.set_head(sub_msg),
            Err(SessionError::InvalidHead(_))
        ));
        assert!(matches!(
            log.set_head(spawn),
            Err(SessionError::InvalidHead(_))
        ));
        assert!(matches!(
            log.set_head("does-not-exist".to_string()),
            Err(SessionError::InvalidHead(_))
        ));
        assert_eq!(log.head(), Some(&sp), "rejected set_head leaves the head");
    }

    #[test]
    fn resume_initializes_head_and_set_head_shortens_linearize() {
        let dir = fresh_sessions_dir();
        let persistence = ProjectSessions::new(dir.path().to_path_buf());

        let (session_id, u1, u2) = {
            let mut log = SessionLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".to_string()).expect("set sp");
            let (u1, u2) = {
                let mut view = AgentLog::main(&mut log);
                let u1 = view.add_message(user_text("one")).expect("u1").id;
                let u2 = view.add_message(user_text("two")).expect("u2").id;
                (u1, u2)
            };
            (log.session_id().to_string(), u1, u2)
        };

        let mut resumed = SessionLog::resume(&persistence, &session_id).expect("resume");
        // Resume initializes the head to the latest user leaf.
        assert_eq!(resumed.head(), Some(&u2));
        let head = resumed.head().cloned().expect("head");
        assert_eq!(
            resumed.linearize(&head, ThreadFilter::MAIN).message_count(),
            2
        );

        // Moving the head back to the first message yields the shorter path.
        resumed.set_head(u1.clone()).expect("set head to u1");
        let head = resumed.head().cloned().expect("head");
        let convo = resumed.linearize(&head, ThreadFilter::MAIN);
        assert_eq!(convo.message_count(), 1);
        match &convo.entries()[0].entry {
            LogEntryKind::Message { message } => {
                assert!(matches!(message.as_stored_wire(), Some(Message::User(_))));
            }
            other => panic!("expected the first user message, got {other:?}"),
        }
    }
}
