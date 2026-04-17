use aj_ui::UserOutput;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::PathBuf,
};
use thiserror::Error;

use crate::messages::{ContentBlockParam, MessageParam, Role};

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
    /// A message exchanged between user and assistant (maps to `MessageParam`).
    Message(MessageParam),
    /// Information that is displayed to the user.
    UserOutput(UserOutput),
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
        }
    }
}

/// A linearized, read-only view of (a slice of) a conversation log. Produced
/// by [ConversationLog::linearize] / [ConversationView::as_conversation] and
/// passed to the model for inference.
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

    /// Get the number of message entries only (excluding user output).
    pub fn message_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.entry, ConversationEntryKind::Message(_)))
            .count()
    }

    /// Get the last message in the view, if any.
    pub fn last_message(&self) -> Option<&MessageParam> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.entry {
                ConversationEntryKind::Message(msg) => Some(msg),
                _ => None,
            })
    }

    /// Get the last user message in the view, if any. Only returns a
    /// message if it has actual input from the user, meaning a `TextBlock`.
    pub fn last_user_message(&self) -> Option<&MessageParam> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.entry {
                ConversationEntryKind::Message(m) => {
                    let is_user = matches!(m.role, Role::User);
                    if !is_user {
                        return None;
                    }

                    // Only sniff out messages that have actual user-input.
                    // The last user input determines thinking, and so, for
                    // example, when there is back-and-forth with tool
                    // results, we need to maintain the thinking flag
                    // enabled.
                    let is_user_input = m
                        .content
                        .iter()
                        .any(|c| matches!(c, ContentBlockParam::TextBlock { .. }));

                    if is_user_input { Some(m) } else { None }
                }
                _ => None,
            })
    }

    /// Get the last assistant message in the view, if any. Only returns a
    /// message if it has actual text output from the assistant, meaning a
    /// `TextBlock`.
    pub fn last_assistant_message(&self) -> Option<&MessageParam> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.entry {
                ConversationEntryKind::Message(m) => {
                    let is_assistant = matches!(m.role, Role::Assistant);
                    if !is_assistant {
                        return None;
                    }

                    let is_text_output = m
                        .content
                        .iter()
                        .any(|c| matches!(c, ContentBlockParam::TextBlock { .. }));

                    if is_text_output { Some(m) } else { None }
                }
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
    thread_id: String,
    entries: HashMap<EntryId, ConversationEntry>,
    /// Insertion order: ids in the order they were appended. Used to find
    /// the most recently written entry matching a filter.
    order: Vec<EntryId>,
    /// Per-log counter, used to mint new entry ids. Survives resumes.
    next_counter: u64,
    /// Lazily opened: `None` for a freshly-[ConversationLog::create]'d log
    /// that has never been appended to, `Some` once we've committed an
    /// entry (or for a [ConversationLog::resume]'d log from the outset).
    /// Keeping creation lazy means abandoned sessions (user quits before
    /// typing anything) don't leave 0-byte files in the threads directory.
    file: Option<File>,
}

impl ConversationLog {
    /// Reserve a fresh thread id and backing path, but don't touch disk
    /// yet. The file is created lazily on the first [ConversationLog::append]
    /// so a session the user abandons before typing anything leaves no
    /// 0-byte file behind.
    pub fn create(persistence: &ConversationPersistence) -> Result<Self, ConversationError> {
        let threads_dir = persistence.threads_dir();
        if !threads_dir.exists() {
            fs::create_dir_all(threads_dir)?;
        }

        // Thread id / filename: millisecond-resolution timestamp. If a
        // collision somehow occurs within the same millisecond we retry
        // with `_N` suffixes.
        let base = Utc::now().format("%Y-%m-%d-%H-%M-%S-%3f").to_string();
        let (thread_id, path) = Self::mint_unique_path(threads_dir, &base)?;

        Ok(Self {
            path,
            thread_id,
            entries: HashMap::new(),
            order: Vec::new(),
            next_counter: 0,
            file: None,
        })
    }

    fn mint_unique_path(
        threads_dir: &std::path::Path,
        base: &str,
    ) -> Result<(String, PathBuf), ConversationError> {
        let candidate = threads_dir.join(format!("{base}.jsonl"));
        if !candidate.exists() {
            return Ok((base.to_string(), candidate));
        }
        for n in 1..1000 {
            let stem = format!("{base}_{n}");
            let candidate = threads_dir.join(format!("{stem}.jsonl"));
            if !candidate.exists() {
                return Ok((stem, candidate));
            }
        }
        // 1000 collisions in one millisecond is effectively impossible in
        // a single-writer setup; surface as an IO-shaped error via the
        // existing `Io` variant rather than a bespoke one.
        Err(ConversationError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("could not mint a unique thread filename near {base}"),
        )))
    }

    /// Load an existing log from disk and reopen its file in append mode
    /// so subsequent appends pick up where the previous session left off.
    ///
    /// If the final line of the file is truncated or otherwise malformed,
    /// it is dropped with a warning. A parse failure on any non-final
    /// line is a real corruption and surfaces as an error.
    pub fn resume(
        persistence: &ConversationPersistence,
        thread_id: &str,
    ) -> Result<Self, ConversationError> {
        let path = persistence.thread_path(thread_id);

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
            thread_id: thread_id.to_string(),
            entries,
            order,
            next_counter,
            file: Some(file),
        })
    }

    /// The id under which this log is listed by `aj threads list`.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Append one entry to the log. Serializes and writes to disk before
    /// inserting into the in-memory maps so a failed write never leaves
    /// the two diverging. Returns the new entry's id.
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
        let file = self.ensure_open()?;
        writeln!(file, "{json}")?;

        self.order.push(id.clone());
        self.entries.insert(id.clone(), record);
        Ok(id)
    }

    /// Open the backing file on first use (lazy init for `create`'d logs)
    /// and return a mutable reference to it. `resume` always returns a
    /// `Some`-initialized file, so this only opens on the first append
    /// after `create`.
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
        Conversation::from_entries(self.thread_id.clone(), out)
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
            None => Conversation::from_entries(self.log.thread_id().to_string(), Vec::new()),
        }
    }

    /// Append a user message. Writes one JSONL line to disk before
    /// advancing the head.
    pub fn add_user_message(
        &mut self,
        content: Vec<ContentBlockParam>,
    ) -> Result<EntryId, ConversationError> {
        self.add_message(Role::User, content)
    }

    /// Append an assistant message. Writes one JSONL line to disk
    /// before advancing the head.
    pub fn add_assistant_message(
        &mut self,
        content: Vec<ContentBlockParam>,
    ) -> Result<EntryId, ConversationError> {
        self.add_message(Role::Assistant, content)
    }

    fn add_message(
        &mut self,
        role: Role,
        content: Vec<ContentBlockParam>,
    ) -> Result<EntryId, ConversationError> {
        let entry = ConversationEntryKind::Message(MessageParam { role, content });
        let id = self
            .log
            .append(self.head.clone(), self.thread, self.agent_id, entry)?;
        self.head = Some(id.clone());
        Ok(id)
    }

    /// Append a user output (tool result, notice, etc.). Writes one
    /// JSONL line to disk before advancing the head.
    pub fn add_user_output(
        &mut self,
        user_output: UserOutput,
    ) -> Result<EntryId, ConversationError> {
        let entry = ConversationEntryKind::UserOutput(user_output);
        let id = self
            .log
            .append(self.head.clone(), self.thread, self.agent_id, entry)?;
        self.head = Some(id.clone());
        Ok(id)
    }
}

/// Handles persistence operations for conversations, including listing
/// existing thread files and resolving their paths.
#[derive(Clone)]
pub struct ConversationPersistence {
    threads_dir: PathBuf,
}

impl ConversationPersistence {
    /// Create a new [ConversationPersistence] instance with the given
    /// threads directory.
    pub fn new(threads_dir: PathBuf) -> Self {
        Self { threads_dir }
    }

    pub fn threads_dir(&self) -> &std::path::Path {
        &self.threads_dir
    }

    fn thread_path(&self, thread_id: &str) -> PathBuf {
        self.threads_dir.join(format!("{thread_id}.jsonl"))
    }

    /// Get metadata about all conversation threads, sorted by creation
    /// time (latest first).
    ///
    /// Files whose first line does not parse as the new
    /// [ConversationEntry] shape (e.g. pre-refactor threads) are skipped
    /// with a `tracing::info!` note.
    pub fn list_threads(&self) -> Result<Vec<ThreadMetadata>, ConversationError> {
        if !self.threads_dir.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&self.threads_dir)?;
        let mut thread_files = Vec::new();

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                if let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) {
                    thread_files.push(file_stem.to_string());
                }
            }
        }

        // Sort by filename (a timestamp), latest first.
        thread_files.sort_by(|a, b| b.cmp(a));

        let mut threads = Vec::new();

        for thread_id in thread_files {
            let path = self.thread_path(&thread_id);

            if !Self::looks_like_new_format(&path) {
                tracing::info!(
                    "skipping pre-refactor thread file {} (old on-disk format)",
                    path.display()
                );
                continue;
            }

            let metadata = fs::metadata(&path)?;
            let modified = metadata.modified()?;
            let modified_str = DateTime::<Utc>::from(modified)
                .format("%Y-%m-%d %H:%M:%S UTC")
                .to_string();

            // Use file size as proxy for conversation length.
            let file_size = metadata.len();
            let size_display = if file_size < 1024 {
                format!("{file_size}B")
            } else if file_size < 1024 * 1024 {
                format!("{}KB", file_size / 1024)
            } else {
                format!("{}MB", file_size / (1024 * 1024))
            };

            threads.push(ThreadMetadata {
                thread_id,
                modified: modified_str,
                size_display,
            });
        }

        Ok(threads)
    }

    /// Empty files are considered new-format (they were just created and
    /// nothing has been written yet). Otherwise the first non-empty line
    /// must parse as a [ConversationEntry].
    fn looks_like_new_format(path: &std::path::Path) -> bool {
        let Ok(file) = File::open(path) else {
            return false;
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return true, // empty file is fine
                Ok(_) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    return serde_json::from_str::<ConversationEntry>(line.trim_end()).is_ok();
                }
                Err(_) => return false,
            }
        }
    }

    /// Get the latest conversation thread ID, if any exist.
    pub fn get_latest_thread_id(&self) -> Result<Option<String>, ConversationError> {
        let threads = self.list_threads()?;
        Ok(threads.first().map(|t| t.thread_id.clone()))
    }
}

/// Metadata about a conversation thread.
#[derive(Debug, Clone)]
pub struct ThreadMetadata {
    pub thread_id: String,
    pub modified: String,
    pub size_display: String,
}
