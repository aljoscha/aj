//! Append-only conversation log + read-only inference view.
//!
//! Each session is one `.jsonl` file under the project's sessions
//! directory. `ConversationLog` holds the in-memory image and writes
//! every append to disk before mutating the in-memory maps, so a
//! crashed process never leaves the two diverging beyond the last
//! line (which [`ConversationLog::resume`] tolerates with a warning).
//!
//! [`ConversationView`] is a short-lived mutation handle that tracks
//! a head pointer and routes appends to a specific thread (the
//! user's main conversation, or one sub-agent subtree). It writes
//! one JSONL line per call; the write reaches the OS before the call
//! returns, so the entry survives a crash of *this* process. It is
//! deliberately not `fsync`'d, so a host crash or power loss can still
//! lose the most recent line(s) — [`ConversationLog::resume`] tolerates
//! a torn final line with a warning.
//!
//! [`Conversation`] is the read-only linearized projection consumed
//! by the wire layer. It carries the materialized [`AgentMessage`]
//! entries (filtered through a [`ThreadFilter`]) plus a small set of
//! helpers (`last_message`, `messages`, etc.) the binary uses to
//! decide thinking efforts and resume state.

use aj_agent::events::AgentSettings;
use aj_agent::message::AgentMessage;
use aj_models::types::Message;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
};
use thiserror::Error;

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
}

/// A unique identifier for a [ConversationEntry] within a single
/// [ConversationLog]. Parent-child links between entries use this id.
///
/// Ids are only unique within one log file and are not meaningful outside
/// of it. They are random, collision-resistant tokens (minted by
/// `ConversationLog::mint_id`), not a counter. Within one process the mint
/// check rules out duplicates; across two processes appending to the same
/// file a collision is possible but vanishingly unlikely (a 32-bit draw),
/// rather than the certainty a shared counter would produce.
pub type EntryId = String;

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
    /// "xhigh", "max". Stored as a string so the on-disk format stays
    /// stable if the effort enum evolves; unknown values are tolerated
    /// on restore.
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
    /// carries the task and the child's settings snapshot, so the
    /// log is self-describing about what each sub-agent ran with and
    /// replay can synthesize the spawn event without look-ahead.
    SubAgentSpawn {
        task: String,
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
/// by [ConversationLog::linearize] / [ConversationView::as_conversation] and
/// passed to the model for inference.
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
    /// of entries.
    pub fn from_entries(conversation_id: String, entries: Vec<ConversationEntry>) -> Self {
        Self {
            conversation_id,
            entries,
        }
    }

    /// Get the conversation ID (the filename stem of the log).
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    /// Get all entries in this linearized view.
    pub fn entries(&self) -> &[ConversationEntry] {
        &self.entries
    }

    /// Get the number of total entries in the view.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the view is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
            .filter_map(|m| m.as_wire().cloned())
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
    /// reconstruct the full reduced context (see
    /// `docs/compaction-spec.md` §3.3).
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
                    ConversationEntryKind::Message { message } => Some(message.clone()),
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
                out.push(message.clone());
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
                    if let Some(Message::Assistant(a)) = message.as_wire() {
                        settings.model = Some((a.provider.clone(), a.model.clone()));
                    }
                }
                ConversationEntryKind::SystemPrompt { .. } => {}
                // Compaction does not change settings; the retained tail
                // still carries the last assistant model, and any
                // settings entries before the boundary remain on the
                // path.
                ConversationEntryKind::Compaction { .. } => {}
            }
        }
        settings
    }

    /// Get the last message in the view, if any.
    pub fn last_message(&self) -> Option<Message> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.entry {
                ConversationEntryKind::Message { message: m } => m.as_wire().cloned(),
                _ => None,
            })
    }
}

/// An append-only, event-sourced log of a conversation and all its subagent
/// and branch offshoots, held in memory and mirrored to a single JSONL file
/// on disk.
///
/// Entries are written to disk before they are inserted into the in-memory
/// maps, so a failed write never leaves the two diverging. A process crash
/// truncates at most the last line, which [ConversationLog::resume] tolerates
/// with a warning.
///
/// Concurrent writers are tolerated rather than locked out: the same session
/// can be resumed in two processes at once (`aj continue <id>` twice). Entry
/// ids are random (see `mint_id`), so the two writers practically never mint
/// the same id, and each entry line is appended with its own `O_APPEND`
/// write, so concurrent appends interleave whole lines instead of tearing
/// one. Neither writer corrupts the file. They do both anchor to the same
/// head, though, so they grow two sibling branches: on the next resume one
/// becomes the head and the other writer's tail is left off the linearized
/// path (still on disk, just not replayed). We accept that over a lock.
pub struct ConversationLog {
    path: PathBuf,
    session_id: String,
    entries: HashMap<EntryId, ConversationEntry>,
    /// Insertion order: ids in the order they were appended. Used to find
    /// the most recently written entry matching a filter.
    order: Vec<EntryId>,
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
            path,
            session_id,
            entries: HashMap::new(),
            order: Vec::new(),
            file: None,
            pending_writes: Vec::new(),
        })
    }

    fn mint_unique_path(
        sessions_dir: &std::path::Path,
        base: &str,
    ) -> Result<(String, PathBuf), ConversationError> {
        let candidate = sessions_dir.join(format!("{base}.jsonl"));
        if !candidate.exists() {
            return Ok((base.to_string(), candidate));
        }
        for n in 1..1000 {
            let stem = format!("{base}_{n}");
            let candidate = sessions_dir.join(format!("{stem}.jsonl"));
            if !candidate.exists() {
                return Ok((stem, candidate));
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
    /// it is dropped with a warning. A parse failure on any non-final
    /// line is a real corruption and surfaces as an error.
    pub fn resume(
        persistence: &crate::persistence::ConversationPersistence,
        session_id: &str,
    ) -> Result<Self, ConversationError> {
        let path = persistence.session_path(session_id);

        let reader = BufReader::new(File::open(&path)?);
        let lines: Vec<String> = reader.lines().collect::<Result<_, _>>()?;

        let last_non_empty = lines.iter().rposition(|l| !l.trim().is_empty());

        let mut entries: HashMap<EntryId, ConversationEntry> = HashMap::new();
        let mut order: Vec<EntryId> = Vec::new();

        for (i, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry = match serde_json::from_str::<ConversationEntry>(line) {
                Ok(e) => e,
                Err(err) => {
                    if Some(i) == last_non_empty {
                        tracing::warn!(
                            "dropping truncated trailing entry in {}: {err}",
                            path.display()
                        );
                        break;
                    } else {
                        return Err(ConversationError::Corrupt(format!(
                            "{}:line {}: {err}",
                            path.display(),
                            i + 1
                        )));
                    }
                }
            };

            order.push(entry.id.clone());
            entries.insert(entry.id.clone(), entry);
        }

        let file = OpenOptions::new().append(true).open(&path)?;

        Ok(Self {
            path,
            session_id: session_id.to_string(),
            entries,
            order,
            file: Some(file),
            // Anything on disk is by definition already committed.
            pending_writes: Vec::new(),
        })
    }

    /// The id under which this log is listed by `aj list-sessions`.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Append one entry to the log. Returns the new entry's id.
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
    pub fn append(
        &mut self,
        parent_id: Option<EntryId>,
        thread: ThreadKind,
        agent_id: Option<usize>,
        entry: ConversationEntryKind,
    ) -> Result<EntryId, ConversationError> {
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
            if !self.entries.contains_key(parent) {
                return Err(ConversationError::InvalidAppend(format!(
                    "parent entry {parent} not found in log"
                )));
            }
        } else if !self.order.is_empty() {
            return Err(ConversationError::InvalidAppend(
                "log already has a root entry; additional entries must have a parent".to_string(),
            ));
        }

        let id = self.mint_id();
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
            // Write each entry as one buffer (line + trailing newline)
            // rather than as separate line and newline writes. Under
            // `O_APPEND` the kernel makes a single append write atomic
            // against other appenders, so a second process writing the
            // same file (the same session resumed twice) interleaves
            // whole lines instead of tearing one mid-line. A very large
            // line can still split across writes, but that's the same
            // exposure as any append-only log, and far less likely than
            // the id collisions that random ids remove.
            for line in &queued {
                file.write_all(format!("{line}\n").as_bytes())?;
            }
            file.write_all(format!("{json}\n").as_bytes())?;
        } else {
            self.pending_writes.push(json);
        }

        self.order.push(id.clone());
        self.entries.insert(id.clone(), record);
        Ok(id)
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

    /// Mint a fresh entry id: a random 32-bit value as 8 hex digits,
    /// re-drawn until it doesn't collide with an id already in this log.
    ///
    /// Ids are random rather than a per-process counter so two processes
    /// appending to the same file (the same session resumed in two
    /// terminals) practically can't mint the same id and corrupt the
    /// parent chain. The `contains_key` check rules out a collision with
    /// ids this process already holds, so it fully guards the
    /// within-process draw. Two concurrent processes don't see each
    /// other's fresh ids, so a cross-process collision is possible at
    /// ~1/2^32 per overlapping mint, which we accept over taking a lock.
    fn mint_id(&self) -> EntryId {
        loop {
            let id = format!("{:08x}", rand::random::<u32>());
            if !self.entries.contains_key(&id) {
                return id;
            }
        }
    }

    /// Walk back from `head` along parent_id pointers, keeping only
    /// entries matching `filter`. Returns the entries in chronological
    /// (root-first) order, wrapped in a read-only [Conversation] view
    /// that can be handed to the model.
    pub fn linearize(&self, head: &EntryId, filter: ThreadFilter) -> Conversation {
        let mut out: Vec<ConversationEntry> = Vec::new();
        let mut cursor: Option<EntryId> = Some(head.clone());
        while let Some(id) = cursor {
            let Some(entry) = self.entries.get(&id) else {
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

    /// Total number of entries in the log (across all threads and branches).
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// The largest `agent_id` recorded on any entry in the log, or `None`
    /// if no subagent entries exist. Used on resume to seed the session's
    /// subagent counter so freshly-spawned subagents don't reuse ids from
    /// the prior session.
    pub fn max_agent_id(&self) -> Option<usize> {
        self.entries.values().filter_map(|e| e.agent_id).max()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Path on disk of the backing file.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// All entries in the order they were appended. Used by
    /// [`crate::replay`] to walk a freshly-resumed log without having
    /// to re-derive the head/parent chain.
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
    pub fn set_system_prompt(&mut self, text: String) -> Result<EntryId, ConversationError> {
        if !self.order.is_empty() {
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
    ) -> Result<EntryId, ConversationError> {
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
    ) -> Result<EntryId, ConversationError> {
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
    ) -> Result<EntryId, ConversationError> {
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
    ) -> Result<EntryId, ConversationError> {
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
    ) -> Result<EntryId, ConversationError> {
        if !self.entries.contains_key(&first_kept_entry_id) {
            return Err(ConversationError::InvalidAppend(format!(
                "compaction first_kept_entry_id {first_kept_entry_id} not found in log"
            )));
        }
        let parent = self
            .latest_leaf(filter)
            .or_else(|| self.system_prompt_id().cloned());
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
        settings: &AgentSettings,
    ) -> Result<EntryId, ConversationError> {
        self.append(
            Some(parent_head),
            ThreadKind::Subagent,
            Some(agent_id),
            ConversationEntryKind::SubAgentSpawn {
                task: task.to_string(),
                settings: settings.clone(),
            },
        )
    }

    /// Append a settings entry on `filter`'s thread, anchored at the
    /// thread's current leaf and falling back to the system-prompt
    /// root when the thread is empty (mirroring
    /// [`ConversationView::parent_for_next_append`]). Settings
    /// entries are non-punctuation, so they buffer until the next
    /// punctuation append (see
    /// [`ConversationEntryKind::is_punctuation`]).
    fn append_settings_entry(
        &mut self,
        filter: ThreadFilter,
        entry: ConversationEntryKind,
    ) -> Result<EntryId, ConversationError> {
        let parent = self
            .latest_leaf(filter)
            .or_else(|| self.system_prompt_id().cloned());
        self.append(parent, filter.thread, filter.agent_id, entry)
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

/// A mutation handle into a [ConversationLog] that tracks where the next
/// append attaches (`head`) and which thread it belongs to.
///
/// Each `add_*` method serializes and writes one line to disk before
/// advancing the head, so every individual event reaches the OS as soon
/// as the call returns (surviving a crash of this process; not
/// `fsync`'d, so a power loss can lose the most recent line).
pub struct ConversationView<'a> {
    log: &'a mut ConversationLog,
    head: Option<EntryId>,
    thread: ThreadKind,
    agent_id: Option<usize>,
}

impl<'a> ConversationView<'a> {
    /// Build a new user-thread view attached to the given head. Pass
    /// `None` for a fresh log (the next append will create the root);
    /// pass the result of `latest_leaf(ThreadFilter::USER)` when resuming.
    pub fn user(log: &'a mut ConversationLog, head: Option<EntryId>) -> Self {
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
    pub fn subagent(log: &'a mut ConversationLog, parent_head: EntryId, agent_id: usize) -> Self {
        Self {
            log,
            head: Some(parent_head),
            thread: ThreadKind::Subagent,
            agent_id: Some(agent_id),
        }
    }

    /// Current head -- the id that will become `parent_id` on the next
    /// append, or `None` if the log is still empty.
    pub fn head(&self) -> Option<&EntryId> {
        self.head.as_ref()
    }

    /// Materialize a read-only linear [Conversation] for the model. Walks
    /// parent pointers from `head` back, keeping only entries that
    /// belong to this view's thread (so main-conversation inference
    /// never sees subagent entries, and vice versa).
    pub fn as_conversation(&self) -> Conversation {
        let filter = ThreadFilter {
            thread: self.thread,
            agent_id: self.agent_id,
        };
        match &self.head {
            Some(head) => self.log.linearize(head, filter),
            None => Conversation::from_entries(self.log.session_id().to_string(), Vec::new()),
        }
    }

    /// Append a wire-level message to this thread. Writes one JSONL
    /// line to disk before advancing the head.
    pub fn add_message(&mut self, message: AgentMessage) -> Result<EntryId, ConversationError> {
        let entry = ConversationEntryKind::Message { message };
        let parent = self.parent_for_next_append();
        let id = self.log.append(parent, self.thread, self.agent_id, entry)?;
        self.head = Some(id.clone());
        Ok(id)
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
            .expect("set_system_prompt on empty log");

        assert_eq!(log.system_prompt(), Some("hello world"));
        assert_eq!(log.system_prompt_id(), Some(&id));

        assert_eq!(log.len(), 1);
        let entry = log.entries.get(&id).expect("entry exists");
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
                let mut view = ConversationView::user(&mut log, None);
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
            let mut view = ConversationView::user(&mut log, None);
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
            .expect("set system prompt");

        let user_id = {
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_text("hi")).expect("user msg")
        };

        let user_entry = log.entries.get(&user_id).expect("user entry exists");
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
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_text("hi")).expect("user msg")
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
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_text("hi")).expect("user msg")
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
    fn resume_preserves_system_prompt() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);

        let session_id = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("persisted prompt".to_string())
                .expect("set sp");
            {
                let mut view = ConversationView::user(&mut log, None);
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
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_text("hi")).expect("user msg")
        };

        let sub_id = {
            let mut view = ConversationView::subagent(&mut log, user_id.clone(), 1);
            view.add_message(user_text("subtask"))
                .expect("subagent prompt")
        };

        let convo = log.linearize(&sub_id, ThreadFilter::subagent(1));
        assert_eq!(convo.entries().len(), 1);
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
            let mut view = ConversationView::user(&mut log, None);
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
                let head = log.latest_leaf(ThreadFilter::USER);
                let mut view = ConversationView::user(&mut log, head);
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
            let head = log.latest_leaf(ThreadFilter::USER);
            let mut view = ConversationView::user(&mut log, head);
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
            let head = log.latest_leaf(ThreadFilter::USER);
            let mut view = ConversationView::user(&mut log, head);
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
            let mut view = ConversationView::user(&mut log, None);
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
            let mut view = ConversationView::user(&mut log, None);
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
            let head = log.latest_leaf(ThreadFilter::USER);
            let mut view = ConversationView::user(&mut log, head);
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
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_text("hi")).expect("u")
        };
        let sub_id = {
            let mut view = ConversationView::subagent(&mut log, user_id, 1);
            view.add_message(user_text("subtask")).expect("sub prompt")
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
        let sp_id = log.set_system_prompt("p".into()).expect("set sp");

        let mc_id = log
            .append_model_change(ThreadFilter::USER, "anthropic", "claude-x")
            .expect("model change");
        let mc_entry = log.entries.get(&mc_id).expect("entry exists");
        assert_eq!(mc_entry.parent_id.as_ref(), Some(&sp_id));
        assert!(matches!(mc_entry.thread, ThreadKind::User));
        assert!(mc_entry.agent_id.is_none());

        // The next message chains onto the settings entry.
        let user_id = {
            let head = log.latest_leaf(ThreadFilter::USER);
            assert_eq!(head.as_ref(), Some(&mc_id));
            let mut view = ConversationView::user(&mut log, head);
            view.add_message(user_text("hi")).expect("user msg")
        };
        let user_entry = log.entries.get(&user_id).expect("entry exists");
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
                let mut view = ConversationView::user(&mut log, None);
                view.add_message(user_text("hi")).expect("u")
            };
            log.append_subagent_spawn(1, user_id, "subtask", &spawn_settings())
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
            ConversationEntryKind::SubAgentSpawn { task, settings } => {
                assert_eq!(task, "subtask");
                assert_eq!(*settings, spawn_settings());
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
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_text("hi")).expect("u")
        };
        log.append_subagent_spawn(1, user_id, "subtask", &spawn_settings())
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
            let mut view = ConversationView::user(&mut log, None);
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
                let mut view = ConversationView::user(&mut log, None);
                view.add_message(user_text("one")).expect("u1");
                view.add_message(assistant_text("a1")).expect("a1");
                view.add_message(user_text("two")).expect("u2")
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
            let mut view = ConversationView::user(&mut log, None);
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
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_text("old one")).expect("u1");
            view.add_message(assistant_text("old reply")).expect("a1");
            let kept = view.add_message(user_text("kept question")).expect("u2");
            view.add_message(assistant_text("kept reply")).expect("a2");
            kept
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
    fn appended_ids_are_unique_within_a_log() {
        // The mint-and-retry path must never hand out a duplicate id
        // within one log, even across many appends.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("set sp");

        let mut ids = std::collections::HashSet::new();
        {
            let mut view = ConversationView::user(&mut log, None);
            for i in 0..200 {
                let id = view
                    .add_message(user_text(&format!("m{i}")))
                    .expect("append message");
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
                let mut view = ConversationView::user(&mut log, None);
                view.add_message(user_text("hi")).expect("first user msg");
            }
            log.session_id().to_string()
        };

        let (id_a, id_b) = {
            let mut log_a = ConversationLog::resume(&persistence, &session_id).expect("resume a");
            let mut log_b = ConversationLog::resume(&persistence, &session_id).expect("resume b");

            let head_a = log_a.latest_leaf(ThreadFilter::USER);
            let id_a = {
                let mut view = ConversationView::user(&mut log_a, head_a);
                view.add_message(user_text("from a")).expect("a msg")
            };
            let head_b = log_b.latest_leaf(ThreadFilter::USER);
            let id_b = {
                let mut view = ConversationView::user(&mut log_b, head_b);
                view.add_message(user_text("from b")).expect("b msg")
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
}
