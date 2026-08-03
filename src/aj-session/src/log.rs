//! Append-only conversation log + read-only inference view.
//!
//! Each session is one `.jsonl` file under the project's sessions
//! directory. `ConversationLog` holds the in-memory image and writes
//! every append to disk before mutating the in-memory maps, so a
//! crashed process never leaves the two diverging beyond the last
//! line (which [`ConversationLog::resume`] repairs with a warning).
//!
//! `ConversationView` is a short-lived, crate-internal mutation handle
//! that tracks a head pointer and routes appends to a specific thread
//! (the user's main conversation, or one sub-agent subtree). It writes
//! one JSONL line per call; the write reaches the OS before the call
//! returns, so the entry survives a crash of *this* process. It is
//! deliberately not `fsync`'d, so a host crash or power loss can still
//! lose the most recent line(s). [`ConversationLog::resume`] drops a
//! torn final line with a warning before reopening the log for append.
//!
//! [`Conversation`] is the read-only linearized projection consumed
//! by the wire layer. It carries the materialized [`AgentMessage`]
//! entries (filtered through a [`ThreadFilter`]) plus a small set of
//! helpers (`last_message`, `messages`, etc.) the binary uses to
//! decide thinking efforts and resume state.

use std::collections::{BTreeSet, HashMap};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
};

use aj_agent::events::AgentSettings;
use aj_agent::message::AgentMessage;
use aj_models::types::Message;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tool_details::{compact_message, expand_message};

#[derive(Debug, Error)]
pub enum ConversationError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("conversation log is corrupt: {0}")]
    Corrupt(String),
    #[error("invalid append to conversation log: {0}")]
    InvalidAppend(String),
    #[error("invalid conversation head: {0}")]
    InvalidHead(String),
}

/// A unique identifier for a [ConversationEntry] within a single
/// [ConversationLog]. Parent-child links between entries use this id.
///
/// Ids are only unique within one log file and are not meaningful outside
/// of it. They are random, collision-resistant tokens (minted by
/// `ConversationLog::mint_id`, or adopted from a message's own id), not a
/// counter. Within one process the mint check rules out duplicates; across
/// two processes appending to the same file a collision is possible but
/// vanishingly unlikely (a 128-bit draw), rather than the certainty a
/// shared counter would produce.
pub type EntryId = String;

/// Append position and id of one log entry.
///
/// `seq` is 1-based: the entry's index in the log's append order plus
/// one, so `0` reads as "nothing appended yet" and needs no `Option`.
/// It is stable only within one materialization of the log: a torn tail
/// is truncated on resume and buffered non-punctuation entries are lost
/// with the process, so positions must never be persisted or compared
/// across runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryRef {
    pub seq: u64,
    pub id: EntryId,
}

/// Which thread within a conversation log an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadKind {
    /// Part of the user-driven conversation (root + any branches).
    User,
    /// Part of a subagent exchange. Disambiguated by `agent_id`.
    Subagent,
    /// Log-level metadata that is not part of any conversation thread
    /// (e.g. the [ConversationEntryKind::SystemPrompt] root entry).
    /// `Meta` entries are skipped by [ThreadFilter] walks but still
    /// participate in the parent_id chain so subsequent thread entries
    /// can attach to them.
    Meta,
}

/// An entry in a conversation log. One line in the `.jsonl` file.
///
/// The framing fields (`id`, `parent_id`, `thread`, `agent_id`) live at the
/// top level of the serialized line alongside the payload, thanks to
/// `#[serde(flatten)]` on `entry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEntry {
    /// Unique within the file. A random, collision-resistant token, not
    /// an ordered counter: append order is tracked separately (by
    /// `ConversationLog`'s `order`), so ids need not sort.
    pub id: EntryId,

    /// The immediate predecessor in this entry's thread. `None` only for
    /// the very first entry of the file (the user root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<EntryId>,

    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,

    /// Which thread this entry belongs to.
    pub thread: ThreadKind,

    /// Present only when `thread == Subagent`. Scopes the subagent
    /// subtree within the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<usize>,

    /// The payload. Continues to use `#[serde(tag = "type")]` so its
    /// `type` discriminator sits at the top level of the line.
    #[serde(flatten)]
    pub entry: ConversationEntryKind,
}

/// The different types of conversation entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationEntryKind {
    /// A wire-level message (user / assistant / tool_result), wrapped
    /// in [`AgentMessage`]. The message is nested under a `message`
    /// key (rather than flattened) so its own `timestamp` field
    /// doesn't collide with the framing `timestamp` on
    /// [`ConversationEntry`].
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
    ModelChange { provider: String, model_id: String },
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
    /// speed (unlike `thinking_display`, a view-only preference that
    /// stays in config).
    VerbosityChange { verbosity: String },
    /// The structural root of a sub-agent thread, written when the
    /// sub-agent is spawned and anchored at the parent thread's head
    /// (the assistant message carrying the spawning tool call). It
    /// carries the task, the child's run mode, and its settings snapshot,
    /// so the log is self-describing about what each sub-agent ran with and
    /// replay can synthesize the spawn event without look-ahead.
    SubAgentSpawn {
        task: String,
        /// Whether the sub was spawned to run in the background, concurrent
        /// with the parent's turn, rather than blocking it. `#[serde(default)]`
        /// so logs written before mode tracking still deserialize (missing ->
        /// `false`, i.e. foreground).
        #[serde(default)]
        background: bool,
        settings: AgentSettings,
    },
    /// A compaction checkpoint: the thread's history before
    /// `first_kept_entry_id` was summarized into `summary`. Projection
    /// ([`Conversation::agent_messages`] / [`Conversation::messages`])
    /// replaces that prefix with a single synthetic summary message and
    /// keeps everything from `first_kept_entry_id` onward verbatim. The
    /// summarized entries stay on disk — compaction changes only the
    /// projection, never deletes lines.
    Compaction {
        /// LLM-generated structured summary that stands in for the
        /// summarized prefix.
        summary: String,
        /// First retained entry. Everything strictly before it on this
        /// thread (back to the previous compaction boundary, or the
        /// thread root) is represented by `summary`.
        first_kept_entry_id: EntryId,
        /// Estimated context tokens before this compaction ran. Carried
        /// for the UI ("freed ~N tokens") and telemetry; not used by
        /// projection.
        tokens_before: u64,
        /// Files read / modified in the summarized range, surfaced so
        /// the model knows what was touched without parsing the prose.
        /// `None` when extraction found nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<crate::compaction::CompactionDetails>,
    },
}

impl ConversationEntryKind {
    /// Whether appending this kind triggers a flush of the log's
    /// pending-write buffer to disk.
    ///
    /// Punctuation entries represent real interaction (a user prompt,
    /// an assistant turn, a tool result) — anything we want durable
    /// per-line as the agent loop runs, and anything whose existence
    /// proves the session is worth keeping. Non-punctuation entries
    /// are meta (the system prompt, settings records, and sub-agent
    /// spawn roots) and buffer in-memory until a punctuation flushes
    /// them.
    ///
    /// Net effect: a session the user opens but abandons before
    /// submitting anything leaves no file on disk; the system prompt
    /// alone is not enough to materialize one.
    ///
    /// A `Compaction` checkpoint is likewise punctuation: it must be
    /// durable on its own so that resuming a compacted-then-abandoned
    /// session still sees the reduced context.
    pub fn is_punctuation(&self) -> bool {
        match self {
            Self::Message { .. } | Self::Compaction { .. } => true,
            Self::SystemPrompt { .. }
            | Self::ModelChange { .. }
            | Self::ThinkingChange { .. }
            | Self::SpeedChange { .. }
            | Self::VerbosityChange { .. }
            | Self::SubAgentSpawn { .. } => false,
        }
    }
}

/// A filter specifying which entries of a [ConversationLog] to walk over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadFilter {
    pub thread: ThreadKind,
    /// Required when `thread == Subagent`, ignored when `thread == User`.
    pub agent_id: Option<usize>,
}

impl ThreadFilter {
    pub const USER: Self = Self {
        thread: ThreadKind::User,
        agent_id: None,
    };

    pub fn subagent(agent_id: usize) -> Self {
        Self {
            thread: ThreadKind::Subagent,
            agent_id: Some(agent_id),
        }
    }

    fn matches(&self, entry: &ConversationEntry) -> bool {
        match self.thread {
            ThreadKind::User => matches!(entry.thread, ThreadKind::User),
            ThreadKind::Subagent => {
                matches!(entry.thread, ThreadKind::Subagent) && entry.agent_id == self.agent_id
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
/// [`Conversation::settings`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSettings {
    /// Last (provider, model_id) recorded on this path: the most
    /// recent [`ConversationEntryKind::ModelChange`] entry, falling
    /// back to the most recent assistant message's (provider, model)
    /// for logs that carry no settings entries.
    pub model: Option<(String, String)>,
    /// Last recorded thinking level string, from the most recent
    /// [`ConversationEntryKind::ThinkingChange`] entry. `None` means
    /// "nothing recorded" (inherit the current default) — distinct
    /// from `Some("off")`.
    pub thinking: Option<String>,
    /// Last recorded speed string, from the most recent
    /// [`ConversationEntryKind::SpeedChange`] entry. `None` means
    /// "nothing recorded".
    pub speed: Option<String>,
    /// Last recorded verbosity string, from the most recent
    /// [`ConversationEntryKind::VerbosityChange`] entry. `None` means
    /// "nothing recorded" (inherit the current default) — distinct
    /// from `Some("default")`, which pins the server default.
    pub verbosity: Option<String>,
}

/// A linearized, read-only view of (a slice of) a conversation log. Produced
/// by [ConversationLog::linearize] and passed to the model for inference.
///
/// The view carries both the underlying [`ConversationEntry`] sequence
/// (for callers that need entry-level provenance, e.g. resume-time
/// repair walks and history rendering) and a pre-extracted
/// [`Message`] projection for the wire layer, which only cares
/// about messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    conversation_id: String,
    entries: Vec<ConversationEntry>,
}

impl Conversation {
    /// Construct a read-only view from a conversation id and a linear list
    /// of entries. Crate-internal: external callers obtain a `Conversation`
    /// from [`ConversationLog::linearize`], never by hand.
    pub(crate) fn from_entries(conversation_id: String, entries: Vec<ConversationEntry>) -> Self {
        Self {
            conversation_id,
            entries,
        }
    }

    /// Get all entries in this linearized view.
    pub fn entries(&self) -> &[ConversationEntry] {
        &self.entries
    }

    /// Get the number of message entries only (excluding system prompt).
    pub fn message_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.entry, ConversationEntryKind::Message { .. }))
            .count()
    }

    /// Borrow every wire-level message in this view, in chronological
    /// order. Honors the latest compaction (see
    /// [`Self::projected_agent_messages`]): the summarized prefix is
    /// replaced by one synthetic summary message. Non-message entries
    /// (system prompt, settings) are skipped — the wire layer only
    /// cares about turn-by-turn conversation.
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
    /// replaced by a single synthetic summary message.
    ///
    /// The last compaction wins — its summary already folds in any
    /// earlier compaction and its `first_kept_entry_id` points past the
    /// earlier boundary, so the latest summary plus its retained tail
    /// reconstruct the full reduced context.
    fn projected_agent_messages(&self) -> Vec<AgentMessage> {
        let last_compaction = self
            .entries
            .iter()
            .enumerate()
            .rev()
            .find_map(|(c, entry)| match &entry.entry {
                ConversationEntryKind::Compaction {
                    summary,
                    first_kept_entry_id,
                    ..
                } => Some((c, summary.clone(), first_kept_entry_id.clone())),
                _ => None,
            });

        let Some((c, summary, first_kept)) = last_compaction else {
            return self
                .entries
                .iter()
                .filter_map(|entry| match &entry.entry {
                    ConversationEntryKind::Message { message } => {
                        Some(expand_message(message.clone()))
                    }
                    _ => None,
                })
                .collect();
        };

        // `first_kept` should be on this linearized chain; if it is
        // missing (a corrupt or hand-edited log) fall back to the
        // compaction marker's own index so we drop nothing extra.
        let k = self
            .entries
            .iter()
            .position(|entry| entry.id == first_kept)
            .unwrap_or_else(|| {
                tracing::warn!(
                    "compaction first_kept_entry_id {first_kept} missing from linearized view; \
                     projecting from the compaction marker so nothing extra is dropped"
                );
                c
            });

        let mut out: Vec<AgentMessage> = Vec::new();
        out.push(crate::compaction::summary_message(&summary));
        for entry in &self.entries[k..] {
            if let ConversationEntryKind::Message { message } = &entry.entry {
                out.push(expand_message(message.clone()));
            }
        }
        out
    }

    /// Extract the session settings recorded on this path. One
    /// forward scan over [`Self::entries`], keeping the last value
    /// seen per axis. `ModelChange` entries and assistant-role
    /// messages both update the model; a `SubAgentSpawn` snapshot
    /// updates all three axes; whichever comes later on the path
    /// wins.
    pub fn settings(&self) -> SessionSettings {
        let mut settings = SessionSettings {
            model: None,
            thinking: None,
            speed: None,
            verbosity: None,
        };
        for entry in &self.entries {
            match &entry.entry {
                ConversationEntryKind::ModelChange { provider, model_id } => {
                    settings.model = Some((provider.clone(), model_id.clone()));
                }
                ConversationEntryKind::ThinkingChange { level } => {
                    settings.thinking = Some(level.clone());
                }
                ConversationEntryKind::SpeedChange { speed } => {
                    settings.speed = Some(speed.clone());
                }
                ConversationEntryKind::VerbosityChange { verbosity } => {
                    settings.verbosity = Some(verbosity.clone());
                }
                ConversationEntryKind::SubAgentSpawn { settings: snap, .. } => {
                    settings.model = Some((snap.provider.clone(), snap.model_id.clone()));
                    settings.thinking = Some(snap.thinking.clone());
                    settings.speed = Some(snap.speed.clone());
                    settings.verbosity = Some(snap.verbosity.clone());
                }
                ConversationEntryKind::Message { message } => {
                    if let Some(Message::Assistant(a)) = message.as_stored_wire() {
                        settings.model = Some((a.provider.clone(), a.model.clone()));
                    }
                }
                ConversationEntryKind::SystemPrompt { .. } => {}
                // Compaction does not change settings: it keeps the
                // retained tail's last assistant model plus any
                // pre-boundary settings entries.
                ConversationEntryKind::Compaction { .. } => {}
            }
        }
        settings
    }

    /// Get the last message in the view, if any.
    ///
    /// Returns the provider-facing projection, so a task-notification tail
    /// yields its projected user message rather than being skipped.
    pub fn last_message(&self) -> Option<Message> {
        self.entries.iter().rev().find_map(|entry| {
            let ConversationEntryKind::Message { message } = &entry.entry else {
                return None;
            };
            expand_message(message.clone()).to_projected_wire()
        })
    }
}

/// A cloneable, read-only image of a log's entry tree.
///
/// Every read-side query lives here, and [`ConversationLog`] delegates to
/// the copy it owns. Taking a snapshot under the log lock and answering
/// an expensive read from it (a full projection, say) is what keeps that
/// read from stalling the session's next append.
#[derive(Debug, Clone)]
pub struct LogSnapshot {
    session_id: String,
    entries: HashMap<EntryId, ConversationEntry>,
    /// Insertion order: ids in the order they were appended. The index of
    /// an id here, plus one, is the entry's [`EntryRef::seq`].
    order: Vec<EntryId>,
    /// The user-thread entry the next user-thread append anchors at.
    ///
    /// This is the explicit head that replaces the implicit
    /// "most recently appended user entry" convention. Every
    /// user-thread [`ConversationLog::append`] advances it (messages,
    /// settings records, compaction, repair); sub-agent and meta appends
    /// leave it untouched. [`ConversationLog::set_head`] moves it to an
    /// earlier entry to start a sibling branch. `None` only while the
    /// user thread is empty (no user-thread entry yet); the next append
    /// then anchors at the system-prompt meta entry when one exists, or
    /// becomes the file root. There is no persisted head pointer:
    /// `create` starts it `None` and `resume` recovers it via
    /// [`Self::latest_leaf`], because the most recently appended entry
    /// is always on the branch that was last written to.
    head: Option<EntryId>,
}

/// An append-only, event-sourced log of a conversation and all its subagent
/// and branch offshoots, held in memory and mirrored to a single JSONL file
/// on disk.
///
/// Entries are written to disk before they are inserted into the in-memory
/// maps, so a failed write never leaves the two diverging. A process crash
/// truncates at most the last line, which [ConversationLog::resume] drops
/// with a warning before reopening the log for append.
///
/// Concurrent writers are tolerated rather than locked out: the same session
/// can be resumed in two processes at once (`aj continue <id>` twice). Entry
/// ids are random (see `mint_id`), so the two writers practically never mint
/// the same id. Each entry's JSON and newline are passed to `write_all` as one
/// buffer on an `O_APPEND` file, minimizing opportunities for interleaving.
/// `write_all` can still issue multiple writes after a short write, so this is
/// not an all-or-nothing commit. The writers both anchor to the same head, so
/// they grow two sibling branches: on the next resume one becomes the head
/// and the other writer's tail is left off the linearized path (still on disk,
/// just not replayed). We accept that over a lock.
pub struct ConversationLog {
    /// The entry tree. Reads go through here, appends mutate it after the
    /// line has reached the file.
    core: LogSnapshot,
    path: PathBuf,
    /// Lazily opened: `None` for a freshly-[ConversationLog::create]'d log
    /// that has never had a real ("punctuation") entry appended, `Some`
    /// once we've committed one (or for a [ConversationLog::resume]'d log
    /// from the outset). Keeping creation lazy means a session the user
    /// abandons before typing anything leaves no file in the sessions
    /// directory.
    file: Option<File>,
    /// Pre-serialized lines for entries that have been [Self::append]ed
    /// in memory but whose persistence is deferred until the next
    /// "punctuation" append (see [`ConversationEntryKind::is_punctuation`]).
    /// Drained in order — followed by the punctuation line itself —
    /// on the next punctuation append. Resume initialises this empty:
    /// anything on disk is already committed, by definition.
    pending_writes: Vec<String>,
}

impl LogSnapshot {
    /// The id under which this log is listed by `aj list-sessions`.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Walk back from `head` along parent_id pointers, keeping only
    /// entries matching `filter`. Returns the entries in chronological
    /// (root-first) order, wrapped in a read-only [Conversation] view
    /// that can be handed to the model.
    ///
    /// A broken chain (a `parent_id` pointing at an entry not in the log)
    /// yields a *partial* view rather than an error: the walk stops at the
    /// break, so the root and everything above it are dropped. Append
    /// validates that a parent exists, so this only arises on a corrupt or
    /// hand-edited file (or the sibling-branch case two concurrent writers
    /// produce). We warn so the truncation is observable but keep going,
    /// matching the resume/compaction tolerance for damaged logs.
    pub fn linearize(&self, head: &EntryId, filter: ThreadFilter) -> Conversation {
        let mut out: Vec<ConversationEntry> = Vec::new();
        let mut cursor: Option<EntryId> = Some(head.clone());
        while let Some(id) = cursor {
            let Some(entry) = self.entries.get(&id) else {
                tracing::warn!("linearize: entry {id} missing from log, returning a partial chain");
                break;
            };
            if filter.matches(entry) {
                out.push(entry.clone());
            }
            cursor = entry.parent_id.clone();
        }
        out.reverse();
        Conversation::from_entries(self.session_id.clone(), out)
    }

    /// Most-recently-appended entry matching `filter`, or `None` if none
    /// exist. Used to pick the default "current" head when resuming.
    pub fn latest_leaf(&self, filter: ThreadFilter) -> Option<EntryId> {
        for id in self.order.iter().rev() {
            if let Some(entry) = self.entries.get(id) {
                if filter.matches(entry) {
                    return Some(id.clone());
                }
            }
        }
        None
    }

    /// The user-thread head: the entry the next user-thread append
    /// anchors at. `None` only while the user thread is empty. See the
    /// `head` field for the full contract.
    pub fn head(&self) -> Option<&EntryId> {
        self.head.as_ref()
    }

    /// Total number of entries in the log (across all threads and branches).
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Append position of the most recent entry, `0` when the log is
    /// empty. This is the high-water mark of the 1-based positions
    /// [`ConversationLog::append`] hands out (see [`EntryRef`]).
    pub fn last_seq(&self) -> u64 {
        u64::try_from(self.order.len()).expect("log length fits u64")
    }

    /// The largest `agent_id` recorded on any entry in the log, or `None`
    /// if no subagent entries exist. Used on resume to seed the session's
    /// subagent counter so freshly-spawned subagents don't reuse ids from
    /// the prior session.
    pub fn max_agent_id(&self) -> Option<usize> {
        self.entries.values().filter_map(|e| e.agent_id).max()
    }

    /// Every sub-agent id that appears in the log, on any branch and
    /// whether its run has finished or not.
    ///
    /// A host sweeping the sub-agent boxes a backfill may have left
    /// unconcluded needs the full set, not just the runs the projection
    /// left open.
    pub fn sub_agent_ids(&self) -> BTreeSet<usize> {
        self.entries.values().filter_map(|e| e.agent_id).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Returns the entry at `index` in append order.
    ///
    /// An order slot whose map entry is missing is treated the same as an
    /// out-of-bounds index. Append-order scans must advance past either case.
    pub(crate) fn entry_in_append_order(&self, index: usize) -> Option<&ConversationEntry> {
        self.order.get(index).and_then(|id| self.entries.get(id))
    }

    /// Look up an entry by id. Used by path-aware replay to walk
    /// parent pointers from the head.
    pub(crate) fn get(&self, id: &EntryId) -> Option<&ConversationEntry> {
        self.entries.get(id)
    }

    /// Returns all entries in the order they were appended.
    pub fn entries_in_order(&self) -> Vec<&ConversationEntry> {
        self.order
            .iter()
            .filter_map(|id| self.entries.get(id))
            .collect()
    }

    /// The persisted system prompt for this session, if one was recorded
    /// at session creation. Resumed sessions created before system-prompt
    /// persistence was added will return `None`.
    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt_entry().map(|e| match &e.entry {
            ConversationEntryKind::SystemPrompt { text } => text.as_str(),
            // `system_prompt_entry` only returns SystemPrompt entries.
            _ => unreachable!("system_prompt_entry returned non-SystemPrompt entry"),
        })
    }

    /// The id of the persisted system-prompt entry, if any. Used as the
    /// parent for the first conversation-thread entry so the parent
    /// chain remains rooted.
    pub fn system_prompt_id(&self) -> Option<&EntryId> {
        self.system_prompt_entry().map(|e| &e.id)
    }

    /// The parent id for the next append on `filter`'s thread.
    ///
    /// The user thread anchors at the explicit [`Self::head`]; a
    /// sub-agent thread anchors at its own [`Self::latest_leaf`], since
    /// sub threads are linear per `agent_id` and branching does not
    /// apply to them. Either falls back to the system-prompt root when
    /// the thread has no entry yet, mirroring
    /// [`ConversationView::parent_for_next_append`].
    fn parent_for_thread_append(&self, filter: ThreadFilter) -> Option<EntryId> {
        let leaf = match filter.thread {
            ThreadKind::User => self.head.clone(),
            // Sub threads are linear per `agent_id`, so they anchor at
            // their own leaf. Branching does not apply to them.
            ThreadKind::Subagent => self.latest_leaf(filter),
            // Settings and compaction appends only ever target the user
            // or a sub-agent thread. A `Meta` filter is a misuse: a
            // `ThreadFilter` is never constructed with `thread: Meta`
            // (see `ThreadFilter::matches`), so reaching here means a
            // caller built an invalid filter.
            ThreadKind::Meta => {
                unreachable!("settings/compaction appends never target the meta thread")
            }
        };
        leaf.or_else(|| self.system_prompt_id().cloned())
    }

    /// Locate the (single) system-prompt entry by scanning the log. The
    /// system prompt is the root entry on threads that have one, so this
    /// is effectively `O(1)` in the common case but stays correct even
    /// if the log layout ever grows additional meta entries before it.
    fn system_prompt_entry(&self) -> Option<&ConversationEntry> {
        self.entries
            .values()
            .find(|e| matches!(e.entry, ConversationEntryKind::SystemPrompt { .. }))
    }
}

impl ConversationLog {
    /// Reserve a fresh session id and backing path, but don't touch disk
    /// yet. The file is created lazily on the first [ConversationLog::append]
    /// of a punctuation entry (see
    /// [`ConversationEntryKind::is_punctuation`]) so a session the user
    /// abandons before that point — typically: launches the TUI, never
    /// submits a message — leaves no file on disk. The system prompt
    /// alone is not enough; it buffers in memory and is flushed
    /// alongside the first punctuation entry.
    pub fn create(
        persistence: &crate::persistence::ConversationPersistence,
    ) -> Result<Self, ConversationError> {
        let sessions_dir = persistence.sessions_dir();
        if !sessions_dir.exists() {
            fs::create_dir_all(sessions_dir)?;
        }

        // Session id / filename: millisecond-resolution timestamp. If a
        // collision somehow occurs within the same millisecond we retry
        // with `_N` suffixes.
        let base = Utc::now().format("%Y-%m-%d-%H-%M-%S-%3f").to_string();
        let (session_id, path) = Self::mint_unique_path(sessions_dir, &base)?;

        Ok(Self {
            core: LogSnapshot {
                session_id,
                entries: HashMap::new(),
                order: Vec::new(),
                // A fresh log has no user-thread entry yet.
                head: None,
            },
            path,
            file: None,
            pending_writes: Vec::new(),
        })
    }

    fn mint_unique_path(
        sessions_dir: &std::path::Path,
        base: &str,
    ) -> Result<(String, PathBuf), ConversationError> {
        // The reservation has to be an atomic filesystem operation. A
        // plain existence check cannot see a competing `create`, because
        // `create` writes nothing: the log file appears only on the first
        // punctuation append, so two processes minting in the same
        // millisecond would both find the path free and then share one
        // session. Claiming the id by `create_new` on its lock-file path
        // is atomic, and it composes with the later lock:
        // [`SessionLock::try_acquire`] opens that path with `create(true)`,
        // so the claim file it finds is the one this minted. An abandoned
        // claim leaves an empty lock file and nothing else.
        let take = |stem: &str| -> Result<Option<(String, PathBuf)>, ConversationError> {
            let candidate = sessions_dir.join(format!("{stem}.jsonl"));
            if candidate.exists() {
                return Ok(None);
            }
            match crate::lock::claim_session_id(sessions_dir, stem) {
                Ok(()) => Ok(Some((stem.to_string(), candidate))),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
                Err(err) => Err(ConversationError::Io(err)),
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
        Err(ConversationError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("could not mint a unique session filename near {base}"),
        )))
    }

    /// Load an existing log from disk and reopen its file in append mode
    /// so subsequent appends pick up where the previous session left off.
    ///
    /// If the final line of the file is truncated or otherwise malformed,
    /// it is truncated from disk with a warning. A valid final record without
    /// a newline is preserved and terminated before another record is
    /// appended. A parse failure on any non-final line is a real corruption
    /// and surfaces as an error.
    pub fn resume(
        persistence: &crate::persistence::ConversationPersistence,
        session_id: &str,
    ) -> Result<Self, ConversationError> {
        let path = persistence.session_path(session_id);

        let file = File::open(&path)?;
        // The captured length defines this resume's boundary. Appends after this
        // snapshot belong to a later resume.
        let snapshot_len = file.metadata()?.len();
        let mut reader = BufReader::new(file.take(snapshot_len));
        let mut pending_line = String::new();
        let mut current_line = String::new();
        let mut pending_line_number = None;
        let mut pending_line_start = None;
        let mut physical_line_number = 0;
        let mut next_line_start = 0_u64;
        let mut snapshot_ends_with_newline = snapshot_len == 0;
        let mut corruption = None;

        let mut entries: HashMap<EntryId, ConversationEntry> = HashMap::new();
        let mut order: Vec<EntryId> = Vec::new();

        loop {
            current_line.clear();
            let current_line_start = next_line_start;
            let bytes_read = reader.read_line(&mut current_line)?;
            if bytes_read == 0 {
                break;
            }
            next_line_start += u64::try_from(bytes_read).expect("line length fits u64");
            physical_line_number += 1;
            snapshot_ends_with_newline = current_line.ends_with('\n');

            if corruption.is_some() {
                continue;
            }

            if current_line.ends_with('\n') {
                current_line.pop();
                if current_line.ends_with('\r') {
                    current_line.pop();
                }
            }
            if current_line.trim().is_empty() {
                continue;
            }

            if let Some(line_number) = pending_line_number {
                match serde_json::from_str::<ConversationEntry>(&pending_line) {
                    Ok(entry) => {
                        order.push(entry.id.clone());
                        entries.insert(entry.id.clone(), entry);
                    }
                    Err(err) => {
                        corruption = Some((line_number, err));
                        continue;
                    }
                }
            }

            std::mem::swap(&mut pending_line, &mut current_line);
            pending_line_number = Some(physical_line_number);
            pending_line_start = Some(current_line_start);
        }

        let unread_snapshot_bytes = reader.get_ref().limit();
        if unread_snapshot_bytes != 0 {
            return Err(ConversationError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "session log shrank while reading {}: {unread_snapshot_bytes} snapshot bytes missing",
                    path.display()
                ),
            )));
        }

        if let Some((line_number, err)) = corruption {
            return Err(ConversationError::Corrupt(format!(
                "{}:line {line_number}: {err}",
                path.display()
            )));
        }

        let mut truncate_to = None;
        if pending_line_number.is_some() {
            match serde_json::from_str::<ConversationEntry>(&pending_line) {
                Ok(entry) => {
                    order.push(entry.id.clone());
                    entries.insert(entry.id.clone(), entry);
                }
                Err(err) => {
                    tracing::warn!(
                        "dropping truncated trailing entry in {}: {err}",
                        path.display()
                    );
                    truncate_to = pending_line_start;
                }
            }
        }

        drop(reader);
        if let Some(len) = truncate_to {
            OpenOptions::new().write(true).open(&path)?.set_len(len)?;
        }
        let mut file = OpenOptions::new().append(true).open(&path)?;
        if truncate_to.is_none() && !snapshot_ends_with_newline {
            file.write_all(b"\n")?;
        }

        // Backfill each message entry's in-memory id from its on-disk entry
        // id, so replay, reseeding, and the reducer see ids for free. We do
        // this over the `entries` map (the one handed to `linearize` and the
        // projections), covering both the loop-inserted and trailing entries.
        // Old 8-hex files simply adopt their entry id as the message id.
        for (entry_id, entry) in entries.iter_mut() {
            if let ConversationEntryKind::Message { message } = &mut entry.entry {
                message.set_id(entry_id.clone());
            }
        }

        let mut log = Self {
            core: LogSnapshot {
                session_id: session_id.to_string(),
                entries,
                order,
                head: None,
            },
            path,
            file: Some(file),
            // Anything on disk is by definition already committed.
            pending_writes: Vec::new(),
        };
        // Recover the head from the last-written user entry. The most
        // recently appended entry is always on the branch that was last
        // written to, so its user-thread leaf is the head the next
        // append should anchor at.
        log.core.head = log.latest_leaf(ThreadFilter::USER);
        Ok(log)
    }

    /// Append one entry to the log. Returns the new entry's
    /// [`EntryRef`]: its 1-based append position and its id.
    ///
    /// Message entries pass through the session storage codec before they are
    /// serialized. The caller has transferred ownership, so this can compact
    /// duplicate tool-detail bodies without changing live agent messages.
    /// Other entry kinds are serialized unchanged.
    ///
    /// Durability depends on the entry's kind (see
    /// [`ConversationEntryKind::is_punctuation`]):
    ///
    /// - For a **punctuation** entry, this drains any buffered
    ///   non-punctuation lines into the file (creating it on first
    ///   use) and then writes the new entry's line, in order, before
    ///   returning. After `Ok(_)`, the entry and everything that
    ///   preceded it have been written to the OS — they survive a
    ///   crash of this process, though they are not `fsync`'d, so a
    ///   power loss can still lose the tail. This write-before-return
    ///   is what `repair_interrupted_tool_uses` relies on.
    /// - For a **non-punctuation** entry, this serializes the line
    ///   and queues it in `pending_writes` without touching disk.
    ///   It becomes durable only when a subsequent punctuation
    ///   append flushes the buffer. A log that only ever sees
    ///   non-punctuation appends never creates a file on disk —
    ///   that's the property that prevents accumulating empty
    ///   sessions (where the user opens the TUI but never submits
    ///   a message).
    ///
    /// The in-memory state (`entries`, `order`) is updated identically
    /// for both paths, so all read-side queries (`latest_leaf`,
    /// `system_prompt_id`, `linearize`, …) behave the same way
    /// regardless of whether the entry has been flushed yet.
    ///
    /// NOTE: the crash-consistency unit is the finalized line. A turn
    /// aborted before its assistant `MessageEnd` line is written is
    /// simply absent on resume (good), but an assistant message whose
    /// line *was* written with truncated content is replayed verbatim,
    /// i.e. treated as authoritative. The log can't tell a complete
    /// turn from a content-truncated one once both are well-formed JSON.
    /// Two layers keep such a turn out of the model's context anyway:
    /// `repair_interrupted_tool_uses` synthesizes results for dangling
    /// tool_calls on resume, and `aj_models::transform::transform_messages`
    /// drops assistant turns whose `stop_reason` is `Error`/`Aborted`
    /// before each inference. The residual hole (a content-truncated turn
    /// persisted with a clean `stop_reason`) is narrow, since a truncated
    /// stream finalizes as a retryable error rather than a clean stop.
    pub fn append(
        &mut self,
        parent_id: Option<EntryId>,
        thread: ThreadKind,
        agent_id: Option<usize>,
        entry: ConversationEntryKind,
    ) -> Result<EntryRef, ConversationError> {
        // Cheap invariant checks. Panics here would indicate an agent-side
        // bug; prefer surfacing as errors.
        match thread {
            ThreadKind::User if agent_id.is_some() => {
                return Err(ConversationError::InvalidAppend(
                    "user-thread entry must not carry an agent_id".to_string(),
                ));
            }
            ThreadKind::Subagent if agent_id.is_none() => {
                return Err(ConversationError::InvalidAppend(
                    "subagent-thread entry must carry an agent_id".to_string(),
                ));
            }
            ThreadKind::Meta if agent_id.is_some() => {
                return Err(ConversationError::InvalidAppend(
                    "meta entry must not carry an agent_id".to_string(),
                ));
            }
            _ => {}
        }
        if let Some(parent) = &parent_id {
            if !self.core.entries.contains_key(parent) {
                return Err(ConversationError::InvalidAppend(format!(
                    "parent entry {parent} not found in log"
                )));
            }
        } else if !self.core.order.is_empty() {
            return Err(ConversationError::InvalidAppend(
                "log already has a root entry; additional entries must have a parent".to_string(),
            ));
        }

        let mut entry = entry;
        if let ConversationEntryKind::Message { message } = &mut entry {
            compact_message(message);
        }

        // Adopt the message's own id as the entry id for `Message` entries.
        // This lives in `append` rather than the persistence listener so
        // every writer, repair included, stays consistent. Non-message
        // entries and message entries with no id (deserialized outside the
        // backfill path) get a log-minted id.
        let id = match &entry {
            ConversationEntryKind::Message { message } if !message.id().is_empty() => {
                message.id().to_string()
            }
            _ => self.mint_id(),
        };
        // A duplicate id would silently diverge the parent chain, so we
        // error loudly rather than paper over it. For an adopted 128-bit
        // id, this append collides with probability ~M/2^128 (M = entries
        // already in the log): negligible, not impossible. `mint_id`
        // already excludes existing ids.
        if self.core.entries.contains_key(&id) {
            return Err(ConversationError::InvalidAppend(format!(
                "duplicate entry id {id}: already present in log"
            )));
        }
        let record = ConversationEntry {
            id: id.clone(),
            parent_id: parent_id.clone(),
            timestamp: Some(Utc::now()),
            thread,
            agent_id,
            entry,
        };

        let json = serde_json::to_string(&record)?;

        if record.entry.is_punctuation() {
            // Drain any buffered lines first so they hit disk before
            // this punctuation, matching in-memory `order` exactly.
            // The buffer is only non-empty for `create`'d logs that
            // have seen a non-punctuation append (today: a system
            // prompt) and not yet a punctuation; `resume`'d logs
            // initialise it empty.
            let queued: Vec<String> = self.pending_writes.drain(..).collect();
            let file = self.ensure_open()?;
            // Pass each entry as one buffer (line + trailing newline) rather
            // than making separate body and newline calls. This narrows the
            // interleaving window, but `write_all` may still issue multiple
            // writes after a short write. Resume repairs such a torn tail.
            for line in &queued {
                file.write_all(format!("{line}\n").as_bytes())?;
            }
            file.write_all(format!("{json}\n").as_bytes())?;
        } else {
            self.pending_writes.push(json);
        }

        self.core.order.push(id.clone());
        self.core.entries.insert(id.clone(), record);
        // Advance the explicit head on every user-thread append, once
        // the entry is committed to the in-memory maps. This single
        // point covers every user-thread writer (messages via the
        // persistence listener, settings, compaction, repair).
        // Sub-agent and meta appends must not touch the head.
        if thread == ThreadKind::User {
            self.core.head = Some(id.clone());
        }
        Ok(EntryRef {
            seq: self.last_seq(),
            id,
        })
    }

    /// Open the backing file on first use (lazy init for `create`'d
    /// logs) and return a mutable reference to it. Only ever called
    /// from [`Self::append`] on a punctuation entry, so the file is
    /// created exactly when there's real content to write — never
    /// for a session that only saw a deferred system-prompt append.
    /// `resume`'d logs always return a `Some`-initialized file.
    fn ensure_open(&mut self) -> Result<&mut File, ConversationError> {
        if self.file.is_none() {
            let f = OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(&self.path)?;
            self.file = Some(f);
        }
        Ok(self.file.as_mut().expect("file just opened above"))
    }

    /// Mint a fresh entry id: a random 128-bit value as 32 hex digits,
    /// re-drawn until it doesn't collide with an id already in this log.
    ///
    /// Ids are random rather than a per-process counter so two processes
    /// appending to the same file (the same session resumed in two
    /// terminals) practically can't mint the same id and corrupt the
    /// parent chain. The `contains_key` check rules out a collision with
    /// ids this process already holds, so it fully guards the
    /// within-process draw. Two concurrent processes don't see each
    /// other's fresh ids, so a cross-process collision is possible at
    /// ~1/2^128 per overlapping mint, which we accept over taking a lock.
    ///
    /// The 32-hex format matches the ids `AgentMessage` mints for itself,
    /// so all ids written by current code are uniform. Old 8-hex ids
    /// coexist in the same file: parent links copy strings verbatim.
    fn mint_id(&self) -> EntryId {
        loop {
            let id = format!("{:032x}", rand::random::<u128>());
            if !self.core.entries.contains_key(&id) {
                return id;
            }
        }
    }

    /// Move the user-thread head to `id`, the anchor for the next
    /// user-thread append. Used to start a sibling branch at an earlier
    /// point, or to switch the active branch.
    ///
    /// `id` must exist and be either a user-thread entry or the
    /// system-prompt meta entry. A sub-agent entry is rejected:
    /// anchoring the user-thread head there would splice the main
    /// conversation onto a sub thread, which [`Self::append`]'s own
    /// checks would not catch (they validate the new entry's thread,
    /// not the parent's). A non-system-prompt meta entry and a missing
    /// id are rejected for the same "the head must be a real branch
    /// point" reason. Rejections return
    /// [`ConversationError::InvalidHead`], whose message is fit to
    /// surface to the user.
    pub fn set_head(&mut self, id: EntryId) -> Result<(), ConversationError> {
        let entry = self.core.entries.get(&id).ok_or_else(|| {
            ConversationError::InvalidHead(format!("entry {id} is not in this session's log"))
        })?;
        let valid = match entry.thread {
            ThreadKind::User => true,
            ThreadKind::Meta => matches!(entry.entry, ConversationEntryKind::SystemPrompt { .. }),
            ThreadKind::Subagent => false,
        };
        if !valid {
            return Err(ConversationError::InvalidHead(format!(
                "entry {id} is not a user-thread or system-prompt entry"
            )));
        }
        self.core.head = Some(id);
        Ok(())
    }

    /// Drain the buffered non-punctuation writes to disk, in append order.
    ///
    /// Non-punctuation entries (settings records, spawn roots) normally
    /// buffer in memory until the next punctuation append flushes them
    /// (see [`ConversationEntryKind::is_punctuation`]). Branch rebuilds
    /// re-resume the log from disk, so any still-buffered entries would be
    /// lost unless flushed first. This forces that flush.
    ///
    /// No-op when the log has never materialized a file (`file` is `None`):
    /// forcing a file open here would defeat the abandoned-empty-session
    /// property (a session that only ever buffered a system prompt leaves
    /// nothing on disk). The buffered lines stay in memory in that case.
    pub fn flush_pending(&mut self) -> Result<(), ConversationError> {
        if self.file.is_none() || self.pending_writes.is_empty() {
            return Ok(());
        }
        let queued: Vec<String> = self.pending_writes.drain(..).collect();
        // The file is present (checked above), so write directly rather than
        // through `ensure_open`, which would otherwise create one.
        let file = self.file.as_mut().expect("file present, checked above");
        for line in &queued {
            file.write_all(format!("{line}\n").as_bytes())?;
        }
        Ok(())
    }

    /// A cloneable image of the entry tree, for reads that should not
    /// hold the log lock (see [`LogSnapshot`]).
    pub fn snapshot(&self) -> LogSnapshot {
        self.core.clone()
    }

    /// The entry tree behind this log, for in-crate readers that already
    /// hold the log.
    pub(crate) fn core(&self) -> &LogSnapshot {
        &self.core
    }

    /// Path on disk of the backing file.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The id under which this log is listed by `aj list-sessions`.
    pub fn session_id(&self) -> &str {
        self.core.session_id()
    }

    /// See [`LogSnapshot::linearize`].
    pub fn linearize(&self, head: &EntryId, filter: ThreadFilter) -> Conversation {
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

    /// See [`LogSnapshot::len`].
    pub fn len(&self) -> usize {
        self.core.len()
    }

    /// See [`LogSnapshot::last_seq`].
    pub fn last_seq(&self) -> u64 {
        self.core.last_seq()
    }

    pub fn is_empty(&self) -> bool {
        self.core.is_empty()
    }

    /// See [`LogSnapshot::max_agent_id`].
    pub fn max_agent_id(&self) -> Option<usize> {
        self.core.max_agent_id()
    }

    /// See [`LogSnapshot::entries_in_order`].
    pub fn entries_in_order(&self) -> Vec<&ConversationEntry> {
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
    /// Disk semantics: the system prompt is a non-punctuation entry
    /// (see [`ConversationEntryKind::is_punctuation`]), so this call
    /// updates only the in-memory state — `system_prompt()`,
    /// `system_prompt_id()`, and `parent_for_next_append` work
    /// immediately — and queues the serialized line in
    /// `pending_writes`. The line hits disk alongside the first
    /// punctuation append (typically the first user message). A log
    /// that never sees a punctuation append leaves no file behind.
    pub fn set_system_prompt(&mut self, text: String) -> Result<EntryRef, ConversationError> {
        if !self.core.order.is_empty() {
            return Err(ConversationError::InvalidAppend(
                "system prompt can only be set on an empty log".to_string(),
            ));
        }
        self.append(
            None,
            ThreadKind::Meta,
            None,
            ConversationEntryKind::SystemPrompt { text },
        )
    }

    /// Record a model change on the thread selected by `filter`. See
    /// [`Self::append_settings_entry`] for anchoring and durability.
    pub fn append_model_change(
        &mut self,
        filter: ThreadFilter,
        provider: &str,
        model_id: &str,
    ) -> Result<EntryRef, ConversationError> {
        self.append_settings_entry(
            filter,
            ConversationEntryKind::ModelChange {
                provider: provider.to_string(),
                model_id: model_id.to_string(),
            },
        )
    }

    /// Record a thinking-effort change on the thread selected by
    /// `filter`. See [`Self::append_settings_entry`].
    pub fn append_thinking_change(
        &mut self,
        filter: ThreadFilter,
        level: &str,
    ) -> Result<EntryRef, ConversationError> {
        self.append_settings_entry(
            filter,
            ConversationEntryKind::ThinkingChange {
                level: level.to_string(),
            },
        )
    }

    /// Record a speed change on the thread selected by `filter`. See
    /// [`Self::append_settings_entry`].
    pub fn append_speed_change(
        &mut self,
        filter: ThreadFilter,
        speed: &str,
    ) -> Result<EntryRef, ConversationError> {
        self.append_settings_entry(
            filter,
            ConversationEntryKind::SpeedChange {
                speed: speed.to_string(),
            },
        )
    }

    /// Record an output-verbosity change on the thread selected by
    /// `filter`. See [`Self::append_settings_entry`].
    pub fn append_verbosity_change(
        &mut self,
        filter: ThreadFilter,
        verbosity: &str,
    ) -> Result<EntryRef, ConversationError> {
        self.append_settings_entry(
            filter,
            ConversationEntryKind::VerbosityChange {
                verbosity: verbosity.to_string(),
            },
        )
    }

    /// Record a compaction checkpoint on `filter`'s thread, anchored at
    /// the thread's current leaf. Punctuation: flushes immediately (see
    /// [`ConversationEntryKind::is_punctuation`]). `first_kept_entry_id`
    /// must be an existing entry in the log.
    pub fn append_compaction(
        &mut self,
        filter: ThreadFilter,
        summary: String,
        first_kept_entry_id: EntryId,
        tokens_before: u64,
        details: Option<crate::compaction::CompactionDetails>,
    ) -> Result<EntryRef, ConversationError> {
        if !self.core.entries.contains_key(&first_kept_entry_id) {
            return Err(ConversationError::InvalidAppend(format!(
                "compaction first_kept_entry_id {first_kept_entry_id} not found in log"
            )));
        }
        let parent = self.core.parent_for_thread_append(filter);
        self.append(
            parent,
            filter.thread,
            filter.agent_id,
            ConversationEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
                details,
            },
        )
    }

    /// Seed sub-agent `agent_id`'s thread with its
    /// [`ConversationEntryKind::SubAgentSpawn`] root, anchored at
    /// `parent_head` (the parent thread's head at spawn time — the
    /// assistant message carrying the spawning tool call). After this
    /// the sub thread has a leaf, so its messages chain via
    /// [`Self::latest_leaf`]. Non-punctuation: buffers until the next
    /// punctuation append (see
    /// [`ConversationEntryKind::is_punctuation`]).
    pub fn append_subagent_spawn(
        &mut self,
        agent_id: usize,
        parent_head: EntryId,
        task: &str,
        background: bool,
        settings: &AgentSettings,
    ) -> Result<EntryRef, ConversationError> {
        self.append(
            Some(parent_head),
            ThreadKind::Subagent,
            Some(agent_id),
            ConversationEntryKind::SubAgentSpawn {
                task: task.to_string(),
                background,
                settings: settings.clone(),
            },
        )
    }

    /// Append a settings entry on `filter`'s thread, anchored at the
    /// thread's current head and falling back to the system-prompt
    /// root when the thread is empty. Settings entries are
    /// non-punctuation, so they buffer until the next punctuation
    /// append (see [`ConversationEntryKind::is_punctuation`]).
    fn append_settings_entry(
        &mut self,
        filter: ThreadFilter,
        entry: ConversationEntryKind,
    ) -> Result<EntryRef, ConversationError> {
        let parent = self.core.parent_for_thread_append(filter);
        self.append(parent, filter.thread, filter.agent_id, entry)
    }
}

/// A mutation handle into a [ConversationLog] that tracks where the next
/// append attaches (`head`) and which thread it belongs to.
///
/// Crate-internal: the append API is an implementation detail behind
/// [`crate::persistence_listener`] and [`crate::repair_interrupted_tool_uses`],
/// not a public surface. Each `add_*` method serializes and writes one
/// line to disk before advancing the head, so every individual event
/// reaches the OS as soon as the call returns (surviving a crash of this
/// process, though not `fsync`'d, so a power loss can lose the most
/// recent line).
pub(crate) struct ConversationView<'a> {
    log: &'a mut ConversationLog,
    head: Option<EntryId>,
    thread: ThreadKind,
    agent_id: Option<usize>,
}

impl<'a> ConversationView<'a> {
    /// Build a new user-thread view seeded from the log's explicit
    /// head (see [`ConversationLog::head`]). On a fresh log the head is
    /// `None` and the first append creates the root (or anchors at the
    /// system-prompt entry); on a resumed or in-progress log it is the
    /// active branch's tip. Every user-thread append advances both this
    /// view's cached head and [`ConversationLog::head`] to the new
    /// entry, so they stay consistent.
    pub(crate) fn user(log: &'a mut ConversationLog) -> Self {
        let head = log.head().cloned();
        Self {
            log,
            head,
            thread: ThreadKind::User,
            agent_id: None,
        }
    }

    /// Build a new subagent-thread view whose next append will attach to
    /// `parent_head`. When starting a fresh subagent subtree this is the
    /// user-thread assistant message carrying the spawning `tool_use`;
    /// once inside the subtree it's the latest entry of that subagent's
    /// own thread. `parent_head` must be an existing entry in the log.
    pub(crate) fn subagent(
        log: &'a mut ConversationLog,
        parent_head: EntryId,
        agent_id: usize,
    ) -> Self {
        Self {
            log,
            head: Some(parent_head),
            thread: ThreadKind::Subagent,
            agent_id: Some(agent_id),
        }
    }

    /// Current head -- the id that will become `parent_id` on the next
    /// append, or `None` if the log is still empty.
    #[cfg(test)]
    pub(crate) fn head(&self) -> Option<&EntryId> {
        self.head.as_ref()
    }

    /// Append a wire-level message to this thread. The log compacts duplicate
    /// text details in the owned message, then writes one JSONL line before
    /// advancing the head.
    pub(crate) fn add_message(
        &mut self,
        message: AgentMessage,
    ) -> Result<EntryRef, ConversationError> {
        let entry = ConversationEntryKind::Message { message };
        let parent = self.parent_for_next_append();
        let appended = self.log.append(parent, self.thread, self.agent_id, entry)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::ConversationPersistence;
    use aj_models::types::{
        AssistantContent, AssistantMessage, TextContent, ToolCall, ToolResultMessage, UserContent,
        UserMessage,
    };

    /// Allocate a unique scratch directory for one test's persistence
    /// state. Uses the process id, the test thread id, and a nanosecond
    /// timestamp so tests running concurrently never collide.
    fn fresh_sessions_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "aj-session-log-test-{pid}-{tid:?}-{nanos}",
            pid = std::process::id(),
            tid = std::thread::current().id(),
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
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

    fn resume_fixture() -> (ConversationPersistence, String, Vec<String>) {
        let persistence = ConversationPersistence::new(fresh_sessions_dir());
        let session_id = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("fixture prompt".to_string())
                .expect("set system prompt");
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("fixture message"))
                .expect("add user message");
            log.session_id().to_string()
        };
        let records = std::fs::read_to_string(persistence.session_path(&session_id))
            .expect("read fixture log")
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        (persistence, session_id, records)
    }

    /// `flush_pending` drains buffered non-punctuation entries to disk so a
    /// re-resume sees them, and is a no-op on an unmaterialized log (it must
    /// not create a file, preserving the abandoned-empty-session property).
    #[test]
    fn flush_pending_persists_buffered_entries_and_noops_when_unmaterialized() {
        let persistence = ConversationPersistence::new(fresh_sessions_dir());

        // Unmaterialized log: only a buffered (non-punctuation) system prompt.
        // Flushing must not create a file on disk.
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("prompt".to_string())
            .expect("set system prompt");
        let path = log.path().to_path_buf();
        log.flush_pending()
            .expect("flush is a no-op on an unmaterialized log");
        assert!(
            !path.exists(),
            "flush must not materialize an abandoned session"
        );

        // Materialize with a punctuation entry, then buffer multiple settings
        // entries and flush them explicitly. Buffering more than one exercises
        // the drain's append ordering, not just a single write.
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("add user message");
        }
        let model_id = log
            .append_model_change(ThreadFilter::USER, "prov", "model")
            .expect("buffer a model change")
            .id;
        let thinking_id = log
            .append_thinking_change(ThreadFilter::USER, "high")
            .expect("buffer a thinking change")
            .id;
        let session_id = log.session_id().to_string();
        log.flush_pending().expect("flush drains the buffer");
        drop(log);

        // Both flushed settings entries are on disk after resume, in the order
        // they were buffered.
        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume log");
        let flushed: Vec<EntryId> = resumed
            .entries_in_order()
            .iter()
            .filter(|e| {
                matches!(
                    e.entry,
                    ConversationEntryKind::ModelChange { .. }
                        | ConversationEntryKind::ThinkingChange { .. }
                )
            })
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(
            flushed,
            vec![model_id, thinking_id],
            "both flushed settings entries are on disk in append order after resume"
        );
    }

    #[test]
    fn resume_ignores_blank_and_whitespace_lines_including_crlf() {
        let (persistence, session_id, records) = resume_fixture();
        let path = persistence.session_path(&session_id);
        let expected_ids = records
            .iter()
            .map(|record| {
                serde_json::from_str::<ConversationEntry>(record)
                    .expect("fixture record")
                    .id
            })
            .collect::<Vec<_>>();
        std::fs::write(
            &path,
            format!("\r\n \t\r\n{}\r\n\n  \n{}\n\t \r\n", records[0], records[1]),
        )
        .expect("rewrite fixture log");

        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume log");

        assert_eq!(resumed.core.order, expected_ids);
        assert_eq!(resumed.core.entries.len(), 2);
    }

    #[test]
    fn resume_drops_malformed_trailing_record_followed_by_whitespace() {
        let (persistence, session_id, records) = resume_fixture();
        let path = persistence.session_path(&session_id);
        std::fs::write(
            &path,
            format!("{}\n{}\n{{\"id\":\n \t\r\n\r\n", records[0], records[1]),
        )
        .expect("rewrite fixture log");

        let mut resumed = ConversationLog::resume(&persistence, &session_id).expect("resume log");

        assert_eq!(resumed.core.order.len(), 2);
        assert_eq!(resumed.core.entries.len(), 2);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read repaired log"),
            format!("{}\n{}\n", records[0], records[1])
        );

        ConversationView::user(&mut resumed)
            .add_message(user_text("after repair"))
            .expect("append after repair");
        drop(resumed);

        let resumed_again =
            ConversationLog::resume(&persistence, &session_id).expect("resume repaired log");
        assert_eq!(resumed_again.core.order.len(), 3);
        assert_eq!(resumed_again.core.entries.len(), 3);
    }

    #[test]
    fn resume_terminates_valid_final_record_before_appending() {
        let (persistence, session_id, records) = resume_fixture();
        let path = persistence.session_path(&session_id);
        std::fs::write(&path, format!("{}\n{}", records[0], records[1]))
            .expect("remove trailing newline");

        let mut resumed = ConversationLog::resume(&persistence, &session_id).expect("resume log");

        assert_eq!(resumed.core.order.len(), 2);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read repaired log"),
            format!("{}\n{}\n", records[0], records[1])
        );

        ConversationView::user(&mut resumed)
            .add_message(user_text("after boundary repair"))
            .expect("append after boundary repair");
        drop(resumed);

        let resumed_again =
            ConversationLog::resume(&persistence, &session_id).expect("resume repaired log");
        assert_eq!(resumed_again.core.order.len(), 3);
        assert_eq!(resumed_again.core.entries.len(), 3);
    }

    /// `create` has to reserve its id with an atomic filesystem
    /// operation. Nothing is written to the log file until the first
    /// punctuation append, so an existence check cannot see a competing
    /// create, and two sessions minted in the same millisecond would
    /// otherwise share one file.
    #[test]
    fn create_mints_distinct_ids_and_claims_them_on_disk() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());

        let logs: Vec<ConversationLog> = (0..12)
            .map(|_| ConversationLog::create(&persistence).expect("create log"))
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
                crate::lock::lock_path(&dir, log.session_id()).exists(),
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
        let persistence = ConversationPersistence::new(dir.clone());
        let first = ConversationLog::create(&persistence).expect("create log");

        let claimed = first.session_id().to_string();
        let (minted, _) = ConversationLog::mint_unique_path(&dir, &claimed)
            .expect("minting near a claimed base succeeds");
        assert_ne!(minted, claimed, "the claimed id is refused");
        assert_eq!(minted, format!("{claimed}_1"), "and the next one is taken");
    }

    #[test]
    fn resume_reports_physical_line_for_malformed_non_final_record() {
        let (persistence, session_id, records) = resume_fixture();
        let path = persistence.session_path(&session_id);
        std::fs::write(
            &path,
            format!(
                "{}\r\n \t\r\n{{\"id\":\r\n\r\n{}\r\n",
                records[0], records[1]
            ),
        )
        .expect("rewrite fixture log");

        let err = match ConversationLog::resume(&persistence, &session_id) {
            Ok(_) => panic!("non-final malformed record must fail"),
            Err(err) => err,
        };

        match err {
            ConversationError::Corrupt(message) => assert!(
                message.starts_with(&format!("{}:line 3:", path.display())),
                "unexpected corruption message: {message}"
            ),
            other => panic!("expected corrupt log, got {other}"),
        }
    }

    #[test]
    fn resume_surfaces_invalid_utf8_as_invalid_data_io_error() {
        let (persistence, session_id, records) = resume_fixture();
        let path = persistence.session_path(&session_id);
        let mut contents = records[0].as_bytes().to_vec();
        contents.extend_from_slice(b"\n\xff\n");
        std::fs::write(&path, contents).expect("rewrite fixture log");

        let err = match ConversationLog::resume(&persistence, &session_id) {
            Ok(_) => panic!("invalid UTF-8 must fail"),
            Err(err) => err,
        };

        match err {
            ConversationError::Io(err) => {
                assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
            }
            other => panic!("expected IO error, got {other}"),
        }
    }

    #[test]
    fn resume_io_error_wins_over_earlier_non_final_corruption() {
        let (persistence, session_id, records) = resume_fixture();
        let path = persistence.session_path(&session_id);
        let mut contents = records[0].as_bytes().to_vec();
        contents.extend_from_slice(b"\n{\"id\":\n");
        contents.extend_from_slice(records[1].as_bytes());
        contents.extend_from_slice(b"\n\xff\n");
        std::fs::write(&path, contents).expect("rewrite fixture log");

        let err = match ConversationLog::resume(&persistence, &session_id) {
            Ok(_) => panic!("invalid UTF-8 must win over earlier corruption"),
            Err(err) => err,
        };

        match err {
            ConversationError::Io(err) => {
                assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
            }
            other => panic!("expected IO error, got {other}"),
        }
    }

    #[test]
    fn set_system_prompt_records_root_entry_in_memory() {
        // In-memory contract: after `set_system_prompt` the entry is
        // immediately visible to all read-side queries
        // (`system_prompt`, `system_prompt_id`, `len`, `entries`).
        // The deferred-disk-write behaviour is exercised separately
        // by [`set_system_prompt_alone_does_not_create_file`] and
        // [`first_punctuation_append_flushes_buffered_system_prompt`].
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");

        let id = log
            .set_system_prompt("hello world".to_string())
            .expect("set_system_prompt on empty log")
            .id;

        assert_eq!(log.system_prompt(), Some("hello world"));
        assert_eq!(log.system_prompt_id(), Some(&id));

        assert_eq!(log.len(), 1);
        let entry = log.core.entries.get(&id).expect("entry exists");
        assert!(matches!(entry.thread, ThreadKind::Meta));
        assert!(entry.parent_id.is_none());
        assert!(matches!(
            entry.entry,
            ConversationEntryKind::SystemPrompt { .. }
        ));
    }

    #[test]
    fn set_system_prompt_alone_does_not_create_file() {
        // A session that only sees a system-prompt append must leave
        // no file in the sessions directory — that's the property
        // that prevents accumulating empty sessions when the user
        // opens the TUI and quits before submitting anything.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string()).expect("set sp");

        let path = persistence.session_path(log.session_id());
        assert!(
            !path.exists(),
            "system-prompt-only log must not materialise a file on disk; found {}",
            path.display()
        );
    }

    #[test]
    fn first_punctuation_append_flushes_buffered_system_prompt() {
        // Sequencing contract: the buffered system-prompt line hits
        // disk *before* the punctuation line that flushes it, so the
        // on-disk order matches the in-memory `order` exactly. We
        // resume from disk and check both entries are present in the
        // right order.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);

        let session_id = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("the prompt".to_string())
                .expect("set sp");

            let path = persistence.session_path(log.session_id());
            assert!(!path.exists(), "file must not exist before flush");

            {
                let mut view = ConversationView::user(&mut log);
                view.add_message(user_text("hi"))
                    .expect("first user message");
            }

            assert!(path.exists(), "file must exist after first punctuation");
            log.session_id().to_string()
        };

        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume");
        let entries = resumed.entries_in_order();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            entries[0].entry,
            ConversationEntryKind::SystemPrompt { .. }
        ));
        assert!(matches!(
            entries[1].entry,
            ConversationEntryKind::Message { .. }
        ));
    }

    #[test]
    fn set_system_prompt_rejects_non_empty_log() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");

        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi"))
                .expect("first user message");
        }

        let err = log
            .set_system_prompt("too late".to_string())
            .expect_err("must fail on non-empty log");
        assert!(matches!(err, ConversationError::InvalidAppend(_)));
    }

    #[test]
    fn first_user_message_anchors_to_system_prompt_root() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");

        let sp_id = log
            .set_system_prompt("the prompt".to_string())
            .expect("set system prompt")
            .id;

        let user_id = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user msg").id
        };

        let user_entry = log.core.entries.get(&user_id).expect("user entry exists");
        assert_eq!(user_entry.parent_id.as_ref(), Some(&sp_id));
    }

    #[test]
    fn latest_leaf_user_skips_system_prompt() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");

        log.set_system_prompt("p".to_string()).expect("set sp");

        assert!(log.latest_leaf(ThreadFilter::USER).is_none());

        let user_id = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user msg").id
        };

        assert_eq!(log.latest_leaf(ThreadFilter::USER).as_ref(), Some(&user_id));
    }

    #[test]
    fn linearize_user_walks_past_system_prompt() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string()).expect("set sp");

        let user_id = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user msg").id
        };

        let convo = log.linearize(&user_id, ThreadFilter::USER);
        // SystemPrompt must not appear in a User-thread linearization;
        // only the user message should be present.
        assert_eq!(convo.entries().len(), 1);
        assert!(matches!(
            convo.entries()[0].entry,
            ConversationEntryKind::Message { .. }
        ));
        assert_eq!(convo.message_count(), 1);
        assert_eq!(convo.messages().len(), 1);
    }

    #[test]
    fn linearize_returns_partial_chain_on_broken_parent() {
        // A parent_id pointing at a missing entry (a corrupt or
        // hand-edited file) truncates the walk at the break instead of
        // panicking: we get only the entries below the break, and the
        // root above it is dropped.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);

        let (session_id, head_b) = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".to_string()).expect("set sp");
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("a")).expect("a");
            let head_b = view.add_message(user_text("b")).expect("b").id;
            (log.session_id().to_string(), head_b)
        };

        // Tamper: point entry `b`'s parent at a non-existent id.
        let path = persistence.session_path(&session_id);
        let tampered: Vec<String> = std::fs::read_to_string(&path)
            .expect("read log")
            .lines()
            .map(|line| {
                let mut v: serde_json::Value = serde_json::from_str(line).expect("line is json");
                if v["id"] == serde_json::json!(head_b) {
                    v["parent_id"] = serde_json::json!("deadbeef");
                }
                serde_json::to_string(&v).expect("reserialize")
            })
            .collect();
        std::fs::write(&path, format!("{}\n", tampered.join("\n"))).expect("rewrite");

        let log = ConversationLog::resume(&persistence, &session_id).expect("resume log");
        let convo = log.linearize(&head_b, ThreadFilter::USER);

        // Only `b` survives. The broken parent drops `a` and the root.
        assert_eq!(convo.entries().len(), 1);
        match &convo.entries()[0].entry {
            ConversationEntryKind::Message { message } => {
                assert!(matches!(message.as_stored_wire(), Some(Message::User(_))));
            }
            other => panic!("expected user message, got {other:?}"),
        }
    }

    #[test]
    fn resume_preserves_system_prompt() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);

        let session_id = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("persisted prompt".to_string())
                .expect("set sp");
            {
                let mut view = ConversationView::user(&mut log);
                view.add_message(user_text("hi")).expect("user msg");
            }
            log.session_id().to_string()
        };

        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume log");
        assert_eq!(resumed.system_prompt(), Some("persisted prompt"));
        assert!(resumed.system_prompt_id().is_some());
        assert!(resumed.latest_leaf(ThreadFilter::USER).is_some());
    }

    #[test]
    fn subagent_thread_attaches_to_existing_user_chain() {
        // A subagent's first message attaches to the user-thread parent
        // it was spawned from; subagent linearization only collects
        // subagent entries.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string()).expect("set sp");

        let user_id = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user msg").id
        };

        let sub_id = {
            let mut view = ConversationView::subagent(&mut log, user_id.clone(), 1);
            view.add_message(user_text("subtask"))
                .expect("subagent prompt")
                .id
        };

        let convo = log.linearize(&sub_id, ThreadFilter::subagent(1));
        assert_eq!(convo.entries().len(), 1);
    }

    #[test]
    fn direct_message_append_compacts_storage_and_expands_resume_projections() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("sys".into()).expect("set sp");

        let content = "persisted model-facing content\n".repeat(40);
        let body = format!("{content}\n");
        let live_message =
            detailed_text_tool_result("tu-1", "read_file", &content, "read_file large.txt", &body);
        {
            let mut view = ConversationView::user(&mut log);
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
                    ConversationEntryKind::Message { message }
                        if matches!(message.as_stored_wire(), Some(Message::ToolResult(_)))
                )
            })
            .expect("stored tool result");
        let ConversationEntryKind::Message { message } = &raw_entry.entry else {
            unreachable!("matched message entry");
        };
        let raw_details = agent_tool_result_details(message);
        assert_eq!(raw_details["body_ref"]["source"], "content_text");
        assert_eq!(raw_details["body_ref"]["append_newline"], true);
        assert!(raw_details.get("body").is_none());

        let session_id = log.session_id().to_string();
        drop(log);
        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume log");
        let head = resumed.latest_leaf(ThreadFilter::USER).expect("head");
        let conversation = resumed.linearize(&head, ThreadFilter::USER);

        let raw_resumed = conversation
            .entries()
            .iter()
            .find_map(|entry| match &entry.entry {
                ConversationEntryKind::Message { message }
                    if matches!(message.as_stored_wire(), Some(Message::ToolResult(_))) =>
                {
                    Some(agent_tool_result_details(message))
                }
                _ => None,
            })
            .expect("raw resumed tool result");
        assert!(raw_resumed.get("body").is_none());
        assert_eq!(raw_resumed["body_ref"]["source"], "content_text");

        let agent_messages = conversation.agent_messages();
        let projected_agent = agent_messages
            .iter()
            .find(|message| matches!(message.as_stored_wire(), Some(Message::ToolResult(_))))
            .expect("projected agent tool result");
        let projected_agent_details = agent_tool_result_details(projected_agent);
        assert_eq!(projected_agent_details["summary"], "read_file large.txt");
        assert_eq!(projected_agent_details["body"], body);
        assert!(projected_agent_details.get("body_ref").is_none());

        let messages = conversation.messages();
        let projected_wire = messages
            .iter()
            .find(|message| matches!(message, Message::ToolResult(_)))
            .expect("projected wire tool result");
        let projected_wire_details = wire_tool_result_details(projected_wire);
        assert_eq!(projected_wire_details["summary"], "read_file large.txt");
        assert_eq!(projected_wire_details["body"], body);
        assert!(projected_wire_details.get("body_ref").is_none());

        let last = conversation.last_message().expect("last message");
        assert_eq!(wire_tool_result_details(&last)["body"], body);
    }

    #[test]
    fn task_notification_round_trips_as_typed_kind_through_resume() {
        use aj_agent::message::AgentMessageKind;

        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("system prompt");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user message");
            view.add_message(task_notification_msg("exit code 0"))
                .expect("task notification");
        }
        let session_id = log.session_id().to_string();
        drop(log);

        // Resume from disk: the notice deserializes back to the typed
        // kind, not a plain user entry.
        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume log");
        let head = resumed.latest_leaf(ThreadFilter::USER).expect("head");
        let conversation = resumed.linearize(&head, ThreadFilter::USER);

        let agent_messages = conversation.agent_messages();
        match &agent_messages.last().expect("a message").kind {
            AgentMessageKind::TaskNotification(n) => {
                assert_eq!(n.label, "cargo build");
                assert_eq!(n.body, "exit code 0");
            }
            other => panic!("expected TaskNotification, got {other:?}"),
        }

        // It still projects onto the wire as the framed user message the
        // model expects.
        let messages = conversation.messages();
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
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("sys".into()).expect("set sp");

        let mut tr = ToolResultMessage::text("tu-1", "ping", "pong", false);
        tr.details = Some(serde_json::json!({
            "kind": "text",
            "summary": "ping",
            "body": "pong",
        }));
        let tool_result_msg = AgentMessage::wire(Message::ToolResult(tr));

        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user msg");
            view.add_message(assistant_tool_use("tu-1", "ping"))
                .expect("assistant msg");
            view.add_message(tool_result_msg)
                .expect("tool result entry");
        }

        let session_id = log.session_id().to_string();
        drop(log);
        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume log");

        let head = resumed
            .latest_leaf(ThreadFilter::USER)
            .expect("user-thread head exists");
        let convo = resumed.linearize(&head, ThreadFilter::USER);

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
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("sys".into()).expect("set sp");

        // A live message mints its own id; capture it before the append
        // takes ownership.
        let message = user_text("hi");
        let message_id = message.id().to_string();
        assert!(!message_id.is_empty(), "live messages mint an id");

        let entry_id = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(message).expect("user msg").id
        };
        assert_eq!(entry_id, message_id, "append adopts the message id");

        let session_id = log.session_id().to_string();
        drop(log);
        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume");

        let entry = resumed
            .entries_in_order()
            .into_iter()
            .find(|entry| matches!(entry.entry, ConversationEntryKind::Message { .. }))
            .expect("reloaded message entry");
        assert_eq!(entry.id, message_id, "entry id is stable across resume");
        let ConversationEntryKind::Message { message } = &entry.entry else {
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
        // Materialize a session so a file path exists, then overwrite it
        // with a hand-built legacy fixture: 8-hex entry ids and bare wire
        // messages with no `id` field, the real shape of old files.
        let persistence = ConversationPersistence::new(fresh_sessions_dir());
        let session_id = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("sys".into()).expect("set sp");
            {
                let mut view = ConversationView::user(&mut log);
                view.add_message(user_text("materialize")).expect("u");
            }
            log.session_id().to_string()
        };

        let root = ConversationEntry {
            id: "0000aaaa".to_string(),
            parent_id: None,
            timestamp: None,
            thread: ThreadKind::User,
            agent_id: None,
            entry: ConversationEntryKind::Message {
                message: user_text("hi"),
            },
        };
        let child = ConversationEntry {
            id: "1111bbbb".to_string(),
            parent_id: Some("0000aaaa".to_string()),
            timestamp: None,
            thread: ThreadKind::User,
            agent_id: None,
            entry: ConversationEntryKind::Message {
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
            persistence.session_path(&session_id),
            format!("{root_line}\n{child_line}\n"),
        )
        .expect("write legacy fixture");

        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume");
        let mut message_entries = 0;
        for entry in resumed.entries_in_order() {
            if let ConversationEntryKind::Message { message } = &entry.entry {
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
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("sys".into()).expect("set sp");

        let m1 = user_text("first");
        let mut m2 = user_text("second");
        m2.set_id(m1.id().to_string());

        let mut view = ConversationView::user(&mut log);
        view.add_message(m1).expect("append m1");
        let err = view.add_message(m2).expect_err("duplicate id must error");
        assert!(
            matches!(err, ConversationError::InvalidAppend(_)),
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
        let persistence = ConversationPersistence::new(dir);
        let session_id = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".into()).expect("set sp");
            log.append_model_change(ThreadFilter::USER, "anthropic", "claude-x")
                .expect("model change");
            log.append_thinking_change(ThreadFilter::USER, "high")
                .expect("thinking change");
            log.append_speed_change(ThreadFilter::USER, "fast")
                .expect("speed change");
            log.append_verbosity_change(ThreadFilter::USER, "high")
                .expect("verbosity change");
            {
                let mut view = ConversationView::user(&mut log);
                view.add_message(user_text("hi")).expect("user msg");
            }
            log.session_id().to_string()
        };

        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume");
        let entries = resumed.entries_in_order();
        assert_eq!(entries.len(), 6);
        match &entries[1].entry {
            ConversationEntryKind::ModelChange { provider, model_id } => {
                assert_eq!(provider, "anthropic");
                assert_eq!(model_id, "claude-x");
            }
            other => panic!("expected ModelChange, got {other:?}"),
        }
        match &entries[2].entry {
            ConversationEntryKind::ThinkingChange { level } => assert_eq!(level, "high"),
            other => panic!("expected ThinkingChange, got {other:?}"),
        }
        match &entries[3].entry {
            ConversationEntryKind::SpeedChange { speed } => assert_eq!(speed, "fast"),
            other => panic!("expected SpeedChange, got {other:?}"),
        }
        match &entries[4].entry {
            ConversationEntryKind::VerbosityChange { verbosity } => assert_eq!(verbosity, "high"),
            other => panic!("expected VerbosityChange, got {other:?}"),
        }
    }

    #[test]
    fn settings_only_log_does_not_create_file_until_punctuation() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        log.append_model_change(ThreadFilter::USER, "openai", "gpt-x")
            .expect("model change");
        log.append_thinking_change(ThreadFilter::USER, "off")
            .expect("thinking change");

        let path = persistence.session_path(log.session_id());
        assert!(
            !path.exists(),
            "settings-only log must not materialise a file"
        );

        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user msg");
        }
        assert!(path.exists(), "file must exist after first punctuation");

        let resumed = ConversationLog::resume(&persistence, log.session_id()).expect("resume");
        let entries = resumed.entries_in_order();
        assert!(matches!(
            entries[0].entry,
            ConversationEntryKind::SystemPrompt { .. }
        ));
        assert!(matches!(
            entries[1].entry,
            ConversationEntryKind::ModelChange { .. }
        ));
        assert!(matches!(
            entries[2].entry,
            ConversationEntryKind::ThinkingChange { .. }
        ));
        assert!(matches!(
            entries[3].entry,
            ConversationEntryKind::Message { .. }
        ));
    }

    #[test]
    fn settings_entries_in_linearize_but_skipped_by_messages() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        log.append_model_change(ThreadFilter::USER, "anthropic", "claude-x")
            .expect("model change");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user msg");
        }

        let head = log.latest_leaf(ThreadFilter::USER).expect("head");
        let convo = log.linearize(&head, ThreadFilter::USER);
        assert_eq!(convo.entries().len(), 2);
        assert!(matches!(
            convo.entries()[0].entry,
            ConversationEntryKind::ModelChange { .. }
        ));
        assert_eq!(convo.message_count(), 1);
        assert_eq!(convo.messages().len(), 1);
    }

    #[test]
    fn settings_last_wins_per_axis() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        log.append_model_change(ThreadFilter::USER, "anthropic", "claude-x")
            .expect("mc1");
        log.append_thinking_change(ThreadFilter::USER, "low")
            .expect("tc1");
        log.append_speed_change(ThreadFilter::USER, "standard")
            .expect("sc1");
        log.append_model_change(ThreadFilter::USER, "openai", "gpt-y")
            .expect("mc2");
        log.append_thinking_change(ThreadFilter::USER, "off")
            .expect("tc2");
        log.append_speed_change(ThreadFilter::USER, "fast")
            .expect("sc2");
        log.append_verbosity_change(ThreadFilter::USER, "default")
            .expect("vc1");
        log.append_verbosity_change(ThreadFilter::USER, "high")
            .expect("vc2");

        let head = log.latest_leaf(ThreadFilter::USER).expect("head");
        let settings = log.linearize(&head, ThreadFilter::USER).settings();
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
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("u");
            view.add_message(assistant_from("anthropic", "claude-a"))
                .expect("a1");
            view.add_message(user_text("more")).expect("u2");
            view.add_message(assistant_from("openai", "gpt-b"))
                .expect("a2");
        }

        let head = log.latest_leaf(ThreadFilter::USER).expect("head");
        let settings = log.linearize(&head, ThreadFilter::USER).settings();
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
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("u");
            view.add_message(assistant_from("anthropic", "claude-a"))
                .expect("a");
        }
        log.append_model_change(ThreadFilter::USER, "openai", "gpt-b")
            .expect("mc");

        let head = log.latest_leaf(ThreadFilter::USER).expect("head");
        let settings = log.linearize(&head, ThreadFilter::USER).settings();
        assert_eq!(
            settings.model,
            Some(("openai".to_string(), "gpt-b".to_string()))
        );
    }

    #[test]
    fn settings_assistant_message_after_model_change_wins() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        log.append_model_change(ThreadFilter::USER, "openai", "gpt-b")
            .expect("mc");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("u");
            view.add_message(assistant_from("anthropic", "claude-a"))
                .expect("a");
        }

        let head = log.latest_leaf(ThreadFilter::USER).expect("head");
        let settings = log.linearize(&head, ThreadFilter::USER).settings();
        assert_eq!(
            settings.model,
            Some(("anthropic".to_string(), "claude-a".to_string()))
        );
    }

    #[test]
    fn subagent_settings_entries_excluded_from_user_linearize() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        let user_id = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("u").id
        };
        let sub_id = {
            let mut view = ConversationView::subagent(&mut log, user_id, 1);
            view.add_message(user_text("subtask"))
                .expect("sub prompt")
                .id
        };
        log.append_model_change(ThreadFilter::subagent(1), "openai", "gpt-sub")
            .expect("sub mc");
        log.append_thinking_change(ThreadFilter::subagent(1), "low")
            .expect("sub tc");

        // Sub-agent thread sees its own settings.
        let sub_head = log
            .latest_leaf(ThreadFilter::subagent(1))
            .expect("sub head");
        let sub_settings = log
            .linearize(&sub_head, ThreadFilter::subagent(1))
            .settings();
        assert_eq!(
            sub_settings.model,
            Some(("openai".to_string(), "gpt-sub".to_string()))
        );
        assert_eq!(sub_settings.thinking.as_deref(), Some("low"));
        let _ = sub_id;

        // The user-thread scan does not.
        let user_head = log.latest_leaf(ThreadFilter::USER).expect("user head");
        let user_settings = log.linearize(&user_head, ThreadFilter::USER).settings();
        assert_eq!(user_settings.model, None);
        assert_eq!(user_settings.thinking, None);
    }

    #[test]
    fn append_settings_anchors_to_system_prompt_root_and_chains() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        let sp_id = log.set_system_prompt("p".into()).expect("set sp").id;

        let mc_id = log
            .append_model_change(ThreadFilter::USER, "anthropic", "claude-x")
            .expect("model change")
            .id;
        let mc_entry = log.core.entries.get(&mc_id).expect("entry exists");
        assert_eq!(mc_entry.parent_id.as_ref(), Some(&sp_id));
        assert!(matches!(mc_entry.thread, ThreadKind::User));
        assert!(mc_entry.agent_id.is_none());

        // The next message chains onto the settings entry.
        let user_id = {
            let head = log.latest_leaf(ThreadFilter::USER);
            assert_eq!(head.as_ref(), Some(&mc_id));
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user msg").id
        };
        let user_entry = log.core.entries.get(&user_id).expect("entry exists");
        assert_eq!(user_entry.parent_id.as_ref(), Some(&mc_id));
    }

    fn spawn_settings() -> aj_agent::events::AgentSettings {
        aj_agent::events::AgentSettings {
            provider: "anthropic".to_string(),
            model_id: "claude-x".to_string(),
            thinking: "high".to_string(),
            speed: "fast".to_string(),
            verbosity: "high".to_string(),
        }
    }

    #[test]
    fn subagent_spawn_round_trips_through_resume() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let session_id = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".into()).expect("set sp");
            let user_id = {
                let mut view = ConversationView::user(&mut log);
                view.add_message(user_text("hi")).expect("u").id
            };
            log.append_subagent_spawn(1, user_id, "subtask", true, &spawn_settings())
                .expect("spawn entry");
            {
                let sub_head = log
                    .latest_leaf(ThreadFilter::subagent(1))
                    .expect("sub leaf");
                let mut view = ConversationView::subagent(&mut log, sub_head, 1);
                view.add_message(user_text("subtask")).expect("sub prompt");
            }
            log.session_id().to_string()
        };

        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume");
        let sub_head = resumed
            .latest_leaf(ThreadFilter::subagent(1))
            .expect("sub leaf");
        let convo = resumed.linearize(&sub_head, ThreadFilter::subagent(1));
        match &convo.entries()[0].entry {
            ConversationEntryKind::SubAgentSpawn {
                task,
                background,
                settings,
            } => {
                assert_eq!(task, "subtask");
                assert!(*background, "background mode round-trips through resume");
                assert_eq!(*settings, spawn_settings());
            }
            other => panic!("expected SubAgentSpawn, got {other:?}"),
        }
    }

    #[test]
    fn subagent_spawn_without_background_defaults_to_foreground() {
        // A log written before mode tracking has no `background` key on the
        // spawn line. Resume deserializes whole `ConversationEntry` lines, and
        // the spawn kind is `#[serde(flatten)]`-ed into that wrapper over an
        // internally-tagged enum. That flatten + tag + `#[serde(default)]`
        // combination is the real read path (and a known serde trap), so we
        // exercise it through the wrapper rather than the kind in isolation: a
        // missing `background` must yield foreground, not an error.
        let record = ConversationEntry {
            id: "0000abcd".to_string(),
            parent_id: Some("00000001".to_string()),
            timestamp: None,
            thread: ThreadKind::Subagent,
            agent_id: Some(1),
            entry: ConversationEntryKind::SubAgentSpawn {
                task: "t".to_string(),
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
        let restored: ConversationEntry =
            serde_json::from_value(json).expect("legacy line deserializes");
        match restored.entry {
            ConversationEntryKind::SubAgentSpawn { background, .. } => {
                assert!(!background, "missing background must default to foreground");
            }
            other => panic!("expected SubAgentSpawn, got {other:?}"),
        }
    }

    #[test]
    fn subagent_spawn_is_not_punctuation() {
        // Spawn entries buffer like the other meta entries: they
        // must not materialize the log file on their own.
        let spawn = ConversationEntryKind::SubAgentSpawn {
            task: "t".to_string(),
            background: false,
            settings: spawn_settings(),
        };
        assert!(!spawn.is_punctuation());
    }

    #[test]
    fn subagent_spawn_snapshot_feeds_settings() {
        // settings() on a sub-agent linearize picks up all three
        // axes from the spawn snapshot.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        let user_id = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("u").id
        };
        log.append_subagent_spawn(1, user_id, "subtask", false, &spawn_settings())
            .expect("spawn entry");

        let sub_head = log
            .latest_leaf(ThreadFilter::subagent(1))
            .expect("sub leaf");
        let settings = log
            .linearize(&sub_head, ThreadFilter::subagent(1))
            .settings();
        assert_eq!(
            settings.model,
            Some(("anthropic".to_string(), "claude-x".to_string()))
        );
        assert_eq!(settings.thinking.as_deref(), Some("high"));
        assert_eq!(settings.speed.as_deref(), Some("fast"));

        // The user-thread scan does not see the spawn snapshot.
        let user_head = log.latest_leaf(ThreadFilter::USER).expect("user head");
        let user_settings = log.linearize(&user_head, ThreadFilter::USER).settings();
        assert_eq!(user_settings.model, None);
    }

    #[test]
    fn assistant_and_tool_result_count_toward_messages() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("u");
            view.add_message(assistant_text("hello")).expect("a");
            view.add_message(tool_result("tu-1", "ping", "ok"))
                .expect("tr");
        }
        let head = log.latest_leaf(ThreadFilter::USER).expect("head exists");
        let convo = log.linearize(&head, ThreadFilter::USER);
        assert_eq!(convo.message_count(), 3);
    }

    #[test]
    fn append_compaction_flushes_and_round_trips() {
        // A `Compaction` entry is punctuation: appending it must
        // materialize the file immediately and survive a resume with
        // all its fields intact.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);

        let (session_id, first_kept) = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".into()).expect("set sp");

            let first_kept = {
                let mut view = ConversationView::user(&mut log);
                view.add_message(user_text("one")).expect("u1");
                view.add_message(assistant_text("a1")).expect("a1");
                view.add_message(user_text("two")).expect("u2").id
            };

            let details = crate::compaction::CompactionDetails {
                read_files: vec!["/tmp/a".into()],
                modified_files: vec!["/tmp/b".into()],
            };
            log.append_compaction(
                ThreadFilter::USER,
                "the summary".into(),
                first_kept.clone(),
                1234,
                Some(details),
            )
            .expect("append compaction");

            let path = persistence.session_path(log.session_id());
            assert!(
                path.exists(),
                "compaction is punctuation; file must exist right after append"
            );

            (log.session_id().to_string(), first_kept)
        };

        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume");
        let head = resumed.latest_leaf(ThreadFilter::USER).expect("head");
        let convo = resumed.linearize(&head, ThreadFilter::USER);
        let last = convo.entries().last().expect("entries present");
        match &last.entry {
            ConversationEntryKind::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
                details,
            } => {
                assert_eq!(summary, "the summary");
                assert_eq!(first_kept_entry_id, &first_kept);
                assert_eq!(*tokens_before, 1234);
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
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("u");
        }
        let err = log
            .append_compaction(ThreadFilter::USER, "s".into(), "no-such-id".into(), 0, None)
            .expect_err("must reject unknown first_kept id");
        assert!(matches!(err, ConversationError::InvalidAppend(_)));
    }

    #[test]
    fn agent_messages_drops_prefix_and_prepends_summary_after_compaction() {
        // Projection after a compaction: the summarized prefix is gone,
        // replaced by one synthetic wrapped-summary message, and the
        // retained tail (from `first_kept_entry_id` on) is verbatim.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");

        let kept_user = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("old one")).expect("u1");
            view.add_message(assistant_text("old reply")).expect("a1");
            let kept = view.add_message(user_text("kept question")).expect("u2");
            view.add_message(assistant_text("kept reply")).expect("a2");
            kept.id
        };

        log.append_compaction(ThreadFilter::USER, "SUMMARY".into(), kept_user, 999, None)
            .expect("compaction");

        let head = log.latest_leaf(ThreadFilter::USER).expect("head");
        let convo = log.linearize(&head, ThreadFilter::USER);
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
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");

        let content = "retained model-facing result\n".repeat(40);
        let body = format!("{content}\n");
        let first_kept = {
            let mut view = ConversationView::user(&mut log);
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
        log.append_compaction(ThreadFilter::USER, "SUMMARY".into(), first_kept, 999, None)
            .expect("compaction");

        let head = log.latest_leaf(ThreadFilter::USER).expect("head");
        let conversation = log.linearize(&head, ThreadFilter::USER);
        let raw_details = conversation
            .entries()
            .iter()
            .find_map(|entry| match &entry.entry {
                ConversationEntryKind::Message { message }
                    if matches!(message.as_stored_wire(), Some(Message::ToolResult(_))) =>
                {
                    Some(agent_tool_result_details(message))
                }
                _ => None,
            })
            .expect("raw retained tool result");
        assert!(raw_details.get("body").is_none());
        assert_eq!(raw_details["body_ref"]["source"], "content_text");

        let agent_messages = conversation.agent_messages();
        let projected_agent = agent_messages
            .iter()
            .find(|message| matches!(message.as_stored_wire(), Some(Message::ToolResult(_))))
            .expect("retained agent tool result");
        let agent_details = agent_tool_result_details(projected_agent);
        assert_eq!(agent_details["summary"], "read_file retained.txt");
        assert_eq!(agent_details["body"], body);
        assert!(agent_details.get("body_ref").is_none());

        let messages = conversation.messages();
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
        let persistence = ConversationPersistence::new(fresh_sessions_dir());
        let mut log = ConversationLog::create(&persistence).expect("create log");

        let mut refs = vec![
            log.set_system_prompt("p".to_string())
                .expect("system prompt"),
            log.append_model_change(ThreadFilter::USER, "prov", "m")
                .expect("model change"),
            log.append_thinking_change(ThreadFilter::USER, "high")
                .expect("thinking change"),
            log.append_speed_change(ThreadFilter::USER, "fast")
                .expect("speed change"),
            log.append_verbosity_change(ThreadFilter::USER, "low")
                .expect("verbosity change"),
        ];
        // Non-punctuation entries only buffer in memory, yet their
        // position is handed out right away: it is the in-memory append
        // index, not a disk offset.
        assert!(
            !log.path().exists(),
            "settings entries must not materialize a file"
        );
        assert_eq!(log.last_seq(), 5);

        let user = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user message")
        };
        refs.push(user.clone());
        let assistant = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_tool_use("tu-1", "ping"))
                .expect("assistant message")
        };
        refs.push(assistant.clone());
        let spawn = log
            .append_subagent_spawn(1, assistant.id.clone(), "task", false, &spawn_settings())
            .expect("spawn root");
        refs.push(spawn.clone());
        refs.push(
            log.append_compaction(
                ThreadFilter::USER,
                "sum".to_string(),
                user.id.clone(),
                42,
                None,
            )
            .expect("compaction"),
        );
        refs.push(
            log.append(
                Some(spawn.id.clone()),
                ThreadKind::Subagent,
                Some(1),
                ConversationEntryKind::Message {
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
        let persistence = ConversationPersistence::new(fresh_sessions_dir());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        assert_eq!(log.last_seq(), 0, "an empty log has nothing appended yet");

        let prompt = log
            .set_system_prompt("p".to_string())
            .expect("system prompt");
        assert_eq!(log.last_seq(), prompt.seq);

        let user = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user message")
        };
        assert_eq!(log.last_seq(), user.seq);
        assert_eq!(log.last_seq(), 2);
    }

    #[test]
    fn snapshot_answers_reads_like_the_log_and_ignores_later_appends() {
        let persistence = ConversationPersistence::new(fresh_sessions_dir());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string())
            .expect("system prompt");
        let user = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("user message")
        };
        let assistant = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_text("ho"))
                .expect("assistant message")
        };
        log.append_subagent_spawn(1, assistant.id.clone(), "task", false, &spawn_settings())
            .expect("spawn root");

        let snapshot = log.snapshot();
        let ids = |conv: &Conversation| -> Vec<EntryId> {
            conv.entries().iter().map(|e| e.id.clone()).collect()
        };
        assert_eq!(snapshot.session_id(), log.session_id());
        assert_eq!(snapshot.len(), log.len());
        assert_eq!(snapshot.last_seq(), log.last_seq());
        assert_eq!(snapshot.head(), log.head());
        assert_eq!(
            snapshot.latest_leaf(ThreadFilter::USER),
            log.latest_leaf(ThreadFilter::USER)
        );
        assert_eq!(
            snapshot.latest_leaf(ThreadFilter::subagent(1)),
            log.latest_leaf(ThreadFilter::subagent(1))
        );
        assert_eq!(
            ids(&snapshot.linearize(&assistant.id, ThreadFilter::USER)),
            ids(&log.linearize(&assistant.id, ThreadFilter::USER))
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
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("later")).expect("later message")
        };
        assert_eq!(snapshot.last_seq(), 4);
        assert_eq!(log.last_seq(), 5);
        assert!(snapshot.entry_in_append_order(4).is_none());
        assert_eq!(snapshot.head(), Some(&assistant.id));
        assert_eq!(log.head(), Some(&later.id));
        assert!(
            !ids(&log.linearize(&later.id, ThreadFilter::USER)).is_empty(),
            "the live log still linearizes its new head"
        );
        assert_eq!(
            ids(&snapshot.linearize(&assistant.id, ThreadFilter::USER)),
            vec![user.id.clone(), assistant.id.clone()],
        );
    }

    #[test]
    fn create_hands_out_a_distinct_path_per_call_in_one_process() {
        // The same-millisecond case, deterministically: two mints from one
        // timestamp base. `create` is lazy, so neither path has a file on
        // disk for the existence check to catch.
        let dir = fresh_sessions_dir();
        let (first, first_path) =
            ConversationLog::mint_unique_path(&dir, "2026-01-01-00-00-00-000").expect("first mint");
        let (second, second_path) =
            ConversationLog::mint_unique_path(&dir, "2026-01-01-00-00-00-000")
                .expect("second mint");
        assert_ne!(first, second, "two mints from one base must differ");
        assert_ne!(first_path, second_path);

        let persistence = ConversationPersistence::new(dir);
        let mut ids = std::collections::HashSet::new();
        let mut logs = Vec::new();
        for _ in 0..50 {
            let log = ConversationLog::create(&persistence).expect("create log");
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
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");

        let mut ids = std::collections::HashSet::new();
        {
            let mut view = ConversationView::user(&mut log);
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
    fn two_resumers_mint_distinct_ids_and_reresume_cleanly() {
        // Guards against id-collision corruption when one session is
        // resumed twice (`aj continue <id>` in two terminals). Two
        // resumers that both seed from the same on-disk state must mint
        // distinct ids and leave a file that re-resumes without a parse
        // error. A shared counter would mint identical ids here (both
        // seed the same value), overwriting one append and breaking the
        // parent chain.
        //
        // The two resumers append sequentially, so this exercises the
        // id-uniqueness guarantee, not the line-tearing one (which
        // depends on real concurrent `O_APPEND` writes).
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);

        let session_id = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".into()).expect("set sp");
            {
                let mut view = ConversationView::user(&mut log);
                view.add_message(user_text("hi")).expect("first user msg");
            }
            log.session_id().to_string()
        };

        let (id_a, id_b) = {
            let mut log_a = ConversationLog::resume(&persistence, &session_id).expect("resume a");
            let mut log_b = ConversationLog::resume(&persistence, &session_id).expect("resume b");

            let id_a = {
                let mut view = ConversationView::user(&mut log_a);
                view.add_message(user_text("from a")).expect("a msg").id
            };
            let id_b = {
                let mut view = ConversationView::user(&mut log_b);
                view.add_message(user_text("from b")).expect("b msg").id
            };
            (id_a, id_b)
        };

        // Independent 32-bit draws, so this can in principle collide at
        // ~1/2^32. Negligible, and exactly the cross-process risk the
        // contract documents.
        assert_ne!(id_a, id_b, "two resumers must not mint the same id");

        // The merged file (system prompt, "hi", and both resumers'
        // appends) parses cleanly and contains both new entries.
        let resumed =
            ConversationLog::resume(&persistence, &session_id).expect("re-resume merged file");
        assert_eq!(resumed.len(), 4);
        let ids: std::collections::HashSet<&str> = resumed
            .entries_in_order()
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert!(ids.contains(id_a.as_str()));
        assert!(ids.contains(id_b.as_str()));
    }

    #[test]
    fn head_advances_on_user_append_and_ignores_subagent_append() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string()).expect("set sp");

        // A fresh log has no user-thread head; the system prompt is meta.
        assert!(log.head().is_none());

        let u1 = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("u1").id
        };
        assert_eq!(log.head(), Some(&u1), "user append advances the head");

        let a1 = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(assistant_tool_use("tu-1", "agent"))
                .expect("a1")
                .id
        };
        assert_eq!(log.head(), Some(&a1));

        // A sub-agent spawn and its messages must not touch the head.
        let spawn = log
            .append_subagent_spawn(1, a1.clone(), "task", false, &spawn_settings())
            .expect("spawn")
            .id;
        assert_eq!(log.head(), Some(&a1), "sub-agent spawn leaves the head");
        {
            let mut view = ConversationView::subagent(&mut log, spawn, 1);
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
        let persistence = ConversationPersistence::new(dir);

        let (session_id, u1, tail) = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".to_string()).expect("set sp");
            let (u1, tail) = {
                let mut view = ConversationView::user(&mut log);
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
                let mut view = ConversationView::user(&mut log);
                view.add_message(user_text("branch")).expect("branch").id
            };
            // The new entry anchors at the earlier head, not the tail.
            let entry = log.core().get(&branched).expect("branched entry");
            assert_eq!(entry.parent_id.as_ref(), Some(&u1));
            (log.session_id().to_string(), u1, tail)
        };

        // The abandoned tail is still on disk after re-resume.
        let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume");
        assert!(
            resumed.core().get(&tail).is_some(),
            "abandoned tail stays on disk"
        );
        assert!(resumed.core().get(&u1).is_some());
    }

    #[test]
    fn set_head_validates_entry_thread() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        let sp = log.set_system_prompt("p".to_string()).expect("set sp").id;

        let user_id = {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_text("hi")).expect("u").id
        };
        let spawn = log
            .append_subagent_spawn(1, user_id.clone(), "task", false, &spawn_settings())
            .expect("spawn")
            .id;
        let sub_msg = {
            let mut view = ConversationView::subagent(&mut log, spawn.clone(), 1);
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
            Err(ConversationError::InvalidHead(_))
        ));
        assert!(matches!(
            log.set_head(spawn),
            Err(ConversationError::InvalidHead(_))
        ));
        assert!(matches!(
            log.set_head("does-not-exist".to_string()),
            Err(ConversationError::InvalidHead(_))
        ));
        assert_eq!(log.head(), Some(&sp), "rejected set_head leaves the head");
    }

    #[test]
    fn resume_initializes_head_and_set_head_shortens_linearize() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);

        let (session_id, u1, u2) = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("p".to_string()).expect("set sp");
            let (u1, u2) = {
                let mut view = ConversationView::user(&mut log);
                let u1 = view.add_message(user_text("one")).expect("u1").id;
                let u2 = view.add_message(user_text("two")).expect("u2").id;
                (u1, u2)
            };
            (log.session_id().to_string(), u1, u2)
        };

        let mut resumed = ConversationLog::resume(&persistence, &session_id).expect("resume");
        // Resume initializes the head to the latest user leaf.
        assert_eq!(resumed.head(), Some(&u2));
        let head = resumed.head().cloned().expect("head");
        assert_eq!(
            resumed.linearize(&head, ThreadFilter::USER).message_count(),
            2
        );

        // Moving the head back to the first message yields the shorter path.
        resumed.set_head(u1.clone()).expect("set head to u1");
        let head = resumed.head().cloned().expect("head");
        let convo = resumed.linearize(&head, ThreadFilter::USER);
        assert_eq!(convo.message_count(), 1);
        match &convo.entries()[0].entry {
            ConversationEntryKind::Message { message } => {
                assert!(matches!(message.as_stored_wire(), Some(Message::User(_))));
            }
            other => panic!("expected the first user message, got {other:?}"),
        }
    }
}
