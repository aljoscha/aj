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
//! one JSONL line per call, so every individual event is durable as
//! soon as the call returns.
//!
//! [`Conversation`] is the read-only linearized projection consumed
//! by the wire layer. It carries the materialized [`AgentMessage`]
//! entries (filtered through a [`ThreadFilter`]) plus a small set of
//! helpers (`last_message`, `messages`, etc.) the binary uses to
//! decide thinking efforts and resume state.

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
/// Ids are only unique within one log file; they are a counter assigned at
/// append time and are not meaningful outside of that file.
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
    /// Unique within the file. Monotonic so lexicographic sort matches
    /// append order.
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
}

impl ConversationEntryKind {
    /// Whether appending this kind triggers a flush of the log's
    /// pending-write buffer to disk.
    ///
    /// Punctuation entries represent real interaction (a user prompt,
    /// an assistant turn, a tool result) — anything we want durable
    /// per-line as the agent loop runs, and anything whose existence
    /// proves the session is worth keeping. Non-punctuation entries
    /// are meta (currently only the system prompt) and buffer
    /// in-memory until a punctuation flushes them.
    ///
    /// Net effect: a session the user opens but abandons before
    /// submitting anything leaves no file on disk; the system prompt
    /// alone is not enough to materialize one.
    pub fn is_punctuation(&self) -> bool {
        match self {
            Self::Message { .. } => true,
            Self::SystemPrompt { .. } => false,
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
    /// order. Non-message entries (system prompt) are skipped — the
    /// wire layer only cares about turn-by-turn conversation, and
    /// out-of-band metadata travels through other channels.
    pub fn messages(&self) -> Vec<Message> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.entry {
                ConversationEntryKind::Message { message: m } => m.as_wire().cloned(),
                _ => None,
            })
            .collect()
    }

    /// Borrow every [`AgentMessage`] in this view, in chronological
    /// order. The transcript-shaped projection used to seed the
    /// agent on resume.
    pub fn agent_messages(&self) -> Vec<AgentMessage> {
        self.entries
            .iter()
            .filter_map(|entry| match &entry.entry {
                ConversationEntryKind::Message { message: m } => Some(m.clone()),
                _ => None,
            })
            .collect()
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
pub struct ConversationLog {
    path: PathBuf,
    session_id: String,
    entries: HashMap<EntryId, ConversationEntry>,
    /// Insertion order: ids in the order they were appended. Used to find
    /// the most recently written entry matching a filter.
    order: Vec<EntryId>,
    /// Per-log counter, used to mint new entry ids. Survives resumes.
    next_counter: u64,
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
            next_counter: 0,
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
        let mut next_counter: u64 = 0;

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

            // Bump counter so new ids continue monotonically after
            // resume. Uses the same scheme as [Self::next_id]; see
            // [Self::parse_id_counter].
            if let Some(n) = Self::parse_id_counter(&entry.id) {
                if n >= next_counter {
                    next_counter = n + 1;
                }
            }
            order.push(entry.id.clone());
            entries.insert(entry.id.clone(), entry);
        }

        let file = OpenOptions::new().append(true).open(&path)?;

        Ok(Self {
            path,
            session_id: session_id.to_string(),
            entries,
            order,
            next_counter,
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
    ///   preceded it are durable. This preserves the per-line
    ///   durability the agent loop relies on for `repair_interrupted_tool_uses`.
    /// - For a **non-punctuation** entry, this serializes the line
    ///   and queues it in `pending_writes` without touching disk.
    ///   It becomes durable only when a subsequent punctuation
    ///   append flushes the buffer. A log that only ever sees
    ///   non-punctuation appends never creates a file on disk —
    ///   that's the property that prevents accumulating empty
    ///   sessions (where the user opens the TUI but never submits
    ///   a message).
    ///
    /// The in-memory state (`entries`, `order`, `next_counter`) is
    /// updated identically for both paths, so all read-side queries
    /// (`latest_leaf`, `system_prompt_id`, `linearize`, …) behave the
    /// same way regardless of whether the entry has been flushed yet.
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

        let id = self.next_id();
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
            for line in &queued {
                writeln!(file, "{line}")?;
            }
            writeln!(file, "{json}")?;
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

    /// Mint a fresh entry id from [Self::next_counter]. The on-disk id
    /// format is tied to [Self::parse_id_counter] -- if you change this
    /// scheme (e.g. to ULIDs), update the parser too so resume can
    /// continue the sequence without collisions.
    fn next_id(&mut self) -> EntryId {
        let id = format!("{:08}", self.next_counter);
        self.next_counter += 1;
        id
    }

    /// Parse an id produced by [Self::next_id] back into its counter
    /// value, or `None` if the id doesn't match the current scheme.
    /// Used on resume to continue minting ids monotonically past
    /// whatever's already in the log.
    fn parse_id_counter(id: &str) -> Option<u64> {
        id.parse::<u64>().ok()
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
/// advancing the head, so every individual event is durable as soon as the
/// call returns.
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
        AssistantContent, AssistantMessage, TextContent, ToolCall, ToolResultMessage, UserMessage,
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
}
