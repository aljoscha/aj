//! Project-level discovery of conversation session files.
//!
//! [`ConversationPersistence`] is the owner of a project's sessions
//! directory. It lists existing sessions (for `aj list-sessions` and
//! `aj continue`) and resolves a session id to its on-disk path so
//! [`crate::log::ConversationLog`] can open / create the right file.

use aj_models::types::{Message, UserContent};
use chrono::{DateTime, NaiveDateTime, Utc};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use crate::log::{ConversationEntry, ConversationEntryKind, ConversationError};

/// Handles persistence operations for conversations, including listing
/// existing session files and resolving their paths.
#[derive(Clone)]
pub struct ConversationPersistence {
    sessions_dir: PathBuf,
}

impl ConversationPersistence {
    /// Create a new [ConversationPersistence] instance with the given
    /// sessions directory.
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    pub fn sessions_dir(&self) -> &std::path::Path {
        &self.sessions_dir
    }

    pub(crate) fn session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.jsonl"))
    }

    /// Get metadata about all conversation sessions, sorted by creation
    /// time (latest first).
    ///
    /// Files whose first line does not parse as the new
    /// [ConversationEntry] shape (e.g. pre-refactor sessions) are skipped
    /// with a `tracing::info!` note, and so is a file that cannot be read at
    /// all: one bad file must not fail the listing.
    pub fn list_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
        let mut sessions = self.enumerate_sessions()?;
        sessions.retain(|metadata| {
            let path = self.session_path(&metadata.session_id);
            match self.is_current_format(&metadata.session_id) {
                Some(true) => true,
                Some(false) => {
                    tracing::info!(
                        "skipping pre-refactor session file {} (old on-disk format)",
                        path.display()
                    );
                    false
                }
                None => {
                    tracing::warn!("skipping unreadable session file {}", path.display());
                    false
                }
            }
        });
        Ok(sessions)
    }

    /// Every `.jsonl` file in the sessions directory, latest first, with the
    /// facts a `stat` yields.
    ///
    /// No file is opened, so this says nothing about a log's format:
    /// [`Self::is_current_format`] is that gate, applied separately. The split
    /// is what lets a caller that refreshes a listing often cache the gate's
    /// verdict, which is a read, while re-running the enumeration, which is
    /// not.
    ///
    /// A file that vanishes or turns unreadable between the directory read and
    /// its `stat` is skipped rather than failing the enumeration: a listing
    /// must not break over one file, least of all one that is no longer there.
    pub fn enumerate_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.sessions_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // A stem the grammar rejects is not a session this store can be
            // asked about later (see [`crate::id`]), so listing it would put a
            // row in every directory that no attach could ever resolve.
            if !crate::id::is_valid_session_id(session_id) {
                continue;
            }
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(err) => {
                    // Vanished or turned unreadable since the directory read.
                    tracing::debug!("skipping session file {}: {err}", path.display());
                    continue;
                }
            };
            if !metadata.is_file() {
                continue;
            }
            let Ok(modified) = metadata.modified() else {
                tracing::debug!(
                    "skipping session file {}: no modification time",
                    path.display()
                );
                continue;
            };
            sessions.push(SessionMetadata::new(
                session_id.to_string(),
                modified.into(),
                metadata.len(),
            ));
        }

        // Filenames are timestamps, so reverse-lexicographic is latest first.
        sessions.sort_by(|left, right| right.session_id.cmp(&left.session_id));
        Ok(sessions)
    }

    /// The `stat` facts for one session's log, `None` when the store holds no
    /// readable log under that id.
    ///
    /// The single-id form of [`Self::enumerate_sessions`], for the membership
    /// question a lookup asks. It costs one `stat` rather than a directory
    /// read, which is what keeps "is this id one of mine" off the size of the
    /// store.
    ///
    /// An id the grammar rejects answers `None` without touching the
    /// filesystem: it cannot name a log in this store, and turning it into a
    /// path is exactly what must not happen (see [`crate::id`]).
    pub fn session_metadata(&self, session_id: &str) -> Option<SessionMetadata> {
        if !crate::id::is_valid_session_id(session_id) {
            return None;
        }
        let path = self.session_path(session_id);
        let metadata = fs::metadata(&path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        Some(SessionMetadata::new(
            session_id.to_string(),
            metadata.modified().ok()?.into(),
            metadata.len(),
        ))
    }

    /// Whether `session_id`'s log is in the current on-disk format, or `None`
    /// when the log could not be read at all.
    ///
    /// An empty log counts as current (it was just created and nothing has been
    /// written yet). Otherwise its first non-empty line must parse as a
    /// [`ConversationEntry`]. The line is read as bytes, so a log that is not
    /// valid UTF-8 earns a verdict (it is not the current format) rather than a
    /// read failure.
    ///
    /// The `None` case is separate because a caller that caches the verdict
    /// must not cache it: the format is a durable property of the log's
    /// content, while a failure to open the file says nothing about the log and
    /// can clear on its own.
    pub fn is_current_format(&self, session_id: &str) -> Option<bool> {
        let path = self.session_path(session_id);
        let file = File::open(&path).ok()?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                // An empty file is fine.
                Ok(0) => return Some(true),
                Ok(_) => {
                    let trimmed = line.trim_ascii();
                    if trimmed.is_empty() {
                        continue;
                    }
                    return Some(serde_json::from_slice::<ConversationEntry>(trimmed).is_ok());
                }
                Err(_) => return None,
            }
        }
    }

    /// Get the latest conversation session ID, if any exist.
    pub fn get_latest_session_id(&self) -> Result<Option<String>, ConversationError> {
        let sessions = self.list_sessions()?;
        Ok(sessions.first().map(|t| t.session_id.clone()))
    }

    /// List sessions with rich per-session previews — first user
    /// message, message count, modified time, file size.
    ///
    /// Walks the sessions directory in the same latest-first order
    /// as [`Self::list_sessions`], but for each file opens the JSONL
    /// and scans line by line to count `Message` entries and capture
    /// the first user-role textual block. `on_progress(loaded, total)`
    /// fires once per file as previews complete so a caller showing
    /// a "Loading X/Y" indicator can update incrementally. Files
    /// whose first line does not parse as the new [`ConversationEntry`]
    /// shape are skipped (consistent with [`Self::list_sessions`]).
    ///
    /// Note on streaming: this function returns the previews in one
    /// `Vec` after every file has been scanned. The callback is the
    /// streaming surface for progress reporting; callers that want to
    /// render rows as they are scanned (rather than blocking on the
    /// full walk) use [`Self::list_session_previews_streaming`]
    /// instead.
    pub fn list_session_previews(
        &self,
        mut on_progress: impl FnMut(usize, usize),
    ) -> Result<Vec<SessionPreview>, ConversationError> {
        let candidates = self.preview_candidates()?;
        let total = candidates.len();
        let mut previews = Vec::with_capacity(total);
        for (i, (session_id, path)) in candidates.into_iter().enumerate() {
            if let Some(preview) = read_preview(session_id, &path, &|| false) {
                previews.push(preview);
            }
            // Tick progress for every file, including the pre-refactor
            // ones that produced no row, so the counter reaches `total`.
            on_progress(i + 1, total);
        }
        Ok(previews)
    }

    /// Stream per-session previews to `emit`, one file's preview per
    /// call, in the same latest-first order as
    /// [`Self::list_session_previews`]. Each call carries a
    /// single-element batch so a UI rendering the list incrementally
    /// can append rows as the scan progresses rather than blocking on
    /// the whole walk.
    ///
    /// Mirrors the failure tolerance of [`Self::list_session_previews`]:
    /// a pre-refactor or unreadable file is skipped (no row emitted),
    /// and a missing or unreadable sessions directory emits nothing.
    /// `cancel` is polled between files and periodically within a file so
    /// the scan, which runs on the blocking pool and can't be aborted,
    /// bails promptly once the consumer (the selector overlay) goes away.
    /// It should be sticky: once it returns true the scan stops, and a file
    /// interrupted mid-read is dropped rather than emitted as a partial row.
    /// Pass `&|| false` for an uninterruptible scan.
    pub fn list_session_previews_streaming(
        &self,
        cancel: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(Vec<SessionPreview>),
    ) {
        let candidates = match self.preview_candidates() {
            Ok(c) => c,
            Err(err) => {
                tracing::debug!(
                    "could not enumerate sessions dir {}: {err}",
                    self.sessions_dir.display()
                );
                return;
            }
        };
        for (session_id, path) in candidates {
            if cancel() {
                break;
            }
            if let Some(preview) = read_preview(session_id, &path, cancel) {
                // A mid-file cancel leaves `read_preview` with a partial
                // count, so re-check before emitting: a sticky `cancel` is
                // true here and we drop the partial rather than show a row
                // with a truncated message count.
                if cancel() {
                    break;
                }
                emit(vec![preview]);
            }
        }
    }

    /// Enumerate the session files worth previewing, newest-first.
    ///
    /// Every `.jsonl` file is a candidate. The current-format check runs
    /// inline in the per-file walk ([`read_session_preview_file`]), so
    /// each file is opened once rather than once to check the format and
    /// again to read the preview. A pre-refactor file is dropped during
    /// that walk, so the progress total counts it but no row appears for
    /// it, and the counter still reaches the total.
    fn preview_candidates(&self) -> Result<Vec<(String, PathBuf)>, ConversationError> {
        Ok(self
            .enumerate_sessions()?
            .into_iter()
            .map(|metadata| {
                let path = self.session_path(&metadata.session_id);
                (metadata.session_id, path)
            })
            .collect())
    }
}

/// Read a preview for `path`.
///
/// `Ok(None)` means a pre-refactor file (its first non-empty line is not
/// the current [`ConversationEntry`] shape). It is dropped from the
/// listing, matching the format gate [`ConversationPersistence::list_sessions`]
/// applies. A read error (the file vanished or became unreadable between
/// enumeration and the open) also drops it, the same way `list_sessions`
/// does, so the two listings stay consistent.
fn read_preview(
    session_id: String,
    path: &std::path::Path,
    cancel: &dyn Fn() -> bool,
) -> Option<SessionPreview> {
    match read_session_preview_file(&session_id, path, cancel) {
        Ok(Some(preview)) => Some(preview),
        Ok(None) => {
            tracing::info!(
                "skipping pre-refactor session file {} (old on-disk format)",
                path.display()
            );
            None
        }
        Err(err) => {
            tracing::warn!("skipping unreadable session file {}: {err}", path.display());
            None
        }
    }
}

/// Metadata about a conversation session.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    pub session_id: String,
    pub modified_at: DateTime<Utc>,
    /// File size in bytes. Paired with `modified_at` it fingerprints the
    /// file, which is what lets a caller cache anything it derived from
    /// the file's contents (see
    /// [`ConversationPersistence::is_current_format`]).
    pub size_bytes: u64,
}

impl SessionMetadata {
    /// Assemble a row from what a `stat` of the session's log yields.
    pub fn new(session_id: String, modified_at: DateTime<Utc>, size_bytes: u64) -> Self {
        Self {
            session_id,
            modified_at,
            size_bytes,
        }
    }

    /// The modification time, formatted for the `list-sessions` output.
    ///
    /// Derived on demand rather than held: a listing that only fingerprints
    /// files (the session host's directory refresh) enumerates the whole store
    /// on a timer and never renders a row.
    pub fn modified_display(&self) -> String {
        self.modified_at.format("%Y-%m-%d %H:%M:%S UTC").to_string()
    }

    /// The file size, formatted for the `list-sessions` output. Size stands in
    /// for conversation length.
    pub fn size_display(&self) -> String {
        if self.size_bytes < 1024 {
            format!("{}B", self.size_bytes)
        } else if self.size_bytes < 1024 * 1024 {
            format!("{}KB", self.size_bytes / 1024)
        } else {
            format!("{}MB", self.size_bytes / (1024 * 1024))
        }
    }
}

/// Richer per-session snapshot used by the interactive session
/// selector overlay.
///
/// Unlike [`SessionMetadata`] (which is purely a filesystem-stat
/// payload), [`SessionPreview`] opens the JSONL and walks far enough
/// to count `Message` entries and capture the first user-role text
/// block. Producing one preview is therefore O(file size) per
/// session; [`ConversationPersistence::list_session_previews`] streams
/// progress through a callback so a UI rendering the list can show
/// a `Loading X/Y` indicator while the walk completes.
#[derive(Debug, Clone)]
pub struct SessionPreview {
    /// Filename stem of the session file (e.g.
    /// `2025-05-11-14-22-03-512`).
    pub session_id: String,
    /// Modification time read from the file system. Held as a
    /// real [`DateTime`] (not a pre-formatted string) so the
    /// renderer can choose whatever date/age formatting it likes.
    pub modified: DateTime<Utc>,
    /// Session creation time. Parsed from `session_id` (which is
    /// minted as a millisecond-precision UTC timestamp on
    /// [`crate::log::ConversationLog::create`]). Falls back to
    /// `modified` when the id doesn't parse, so hand-renamed files or a
    /// future filename format this build doesn't recognise still
    /// produce a structurally complete row.
    pub created_at: DateTime<Utc>,
    /// Time of the most recently appended message-kind entry.
    /// Captured during the JSONL walk in
    /// [`read_session_preview_file`] as the largest
    /// [`ConversationEntry::timestamp`] seen on a
    /// [`ConversationEntryKind::Message`] entry, so out-of-order
    /// writes (e.g. a tool result that completes after a streaming
    /// assistant message finalised) still resolve to the true
    /// most-recent message rather than the last line of the file.
    /// Falls back to `modified` when no entry carries a timestamp
    /// (logs predating the timestamping work) or no message-kind
    /// entry has been appended yet.
    pub last_message_at: DateTime<Utc>,
    /// On-disk size in bytes. Cheap to surface from the
    /// `fs::metadata` we already had to call.
    pub size_bytes: u64,
    /// Number of [`ConversationEntryKind::Message`] entries in the
    /// log. User, assistant, and tool_result messages all
    /// contribute; non-message entries (`SystemPrompt`) are
    /// skipped.
    pub message_count: usize,
    /// First user-role textual content block in the file, if any.
    /// `None` for a freshly-minted session that hasn't yet seen a
    /// user prompt. The string carries the verbatim text — the
    /// renderer applies its own truncation policy.
    pub first_user_message: Option<String>,
}

/// Open `path`, walk every JSONL line, and assemble a
/// [`SessionPreview`].
///
/// Returns `Ok(None)` when the first non-empty line does not parse as a
/// [`ConversationEntry`], i.e. a pre-refactor file the listing should
/// drop. This is the current-format gate applied inline so the file is
/// opened once (the standalone [`ConversationPersistence::is_current_format`]
/// check stays for `list_sessions`, which doesn't otherwise read the
/// file). A later line that fails to parse is skipped (matching the
/// resume-time tolerance for truncated trailing lines). The walk is
/// one-pass: we read every line so `message_count` is accurate, but we
/// stop updating `first_user_message` once we have one.
fn read_session_preview_file(
    session_id: &str,
    path: &std::path::Path,
    cancel: &dyn Fn() -> bool,
) -> Result<Option<SessionPreview>, ConversationError> {
    let metadata = fs::metadata(path)?;
    let modified = metadata
        .modified()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now());
    let size_bytes = metadata.len();

    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut message_count = 0usize;
    let mut first_user_message: Option<String> = None;
    // Track the largest message-kind timestamp seen so far. Tracking
    // the max (not the last) lets the field tolerate out-of-order
    // writes: a tool result that lands after a streaming assistant
    // message finalised, for example.
    let mut last_message_at: Option<DateTime<Utc>> = None;
    let mut seen_first_entry = false;

    for (lineno, line_res) in reader.lines().enumerate() {
        // Cooperative cancellation: this runs on the blocking pool, so we
        // poll `cancel` and stop reading once the consumer is gone. We may
        // break with a partial count; the streaming caller re-checks
        // `cancel` before emitting and drops it, so no truncated row shows.
        if lineno % crate::SCAN_CANCEL_CHECK_LINES == 0 && cancel() {
            break;
        }
        // A best-effort `Ok(_)`-only path: an IO error mid-file
        // shouldn't mask the entries we already accumulated. Same
        // policy as the resume tolerance for truncated lines.
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let entry = match serde_json::from_str::<ConversationEntry>(&line) {
            Ok(entry) => entry,
            Err(_) if !seen_first_entry => {
                // First non-empty line isn't the current entry shape: a
                // pre-refactor file. Skip the whole file.
                return Ok(None);
            }
            // A later torn/garbage line: skip it, keep what we have.
            Err(_) => continue,
        };
        seen_first_entry = true;
        if let ConversationEntryKind::Message { message: msg } = &entry.entry {
            message_count += 1;
            if first_user_message.is_none() {
                if let Some(Message::User(u)) = msg.as_stored_wire() {
                    if let Some(text) = first_user_text(&u.content) {
                        first_user_message = Some(text);
                    }
                }
            }
            if let Some(ts) = entry.timestamp {
                last_message_at = Some(match last_message_at {
                    Some(prev) if prev >= ts => prev,
                    _ => ts,
                });
            }
        }
    }

    // Creation time: derived from the filename stem rather than
    // a per-entry timestamp so a session with no appended messages
    // (a freshly-minted log) still has a meaningful "created"
    // marker for the selector. Fall back to the file mtime if the
    // stem doesn't parse.
    let created_at = parse_session_id_created_at(session_id).unwrap_or(modified);
    // `last_message_at` falls back to the file mtime for two cases:
    // logs predating the per-entry timestamping work (every entry
    // has `timestamp: None`) and freshly-minted sessions with no
    // message-kind entries yet. The fallback matches the value the
    // selector would have rendered as `modified` under the older
    // single-field design.
    let last_message_at = last_message_at.unwrap_or(modified);

    Ok(Some(SessionPreview {
        session_id: session_id.to_string(),
        modified,
        created_at,
        last_message_at,
        size_bytes,
        message_count,
        first_user_message,
    }))
}

/// Parse a session id minted by [`crate::log::ConversationLog::create`]
/// back into the UTC instant it represents.
///
/// The mint format is `%Y-%m-%d-%H-%M-%S-%3f` with an optional
/// `_<N>` collision suffix appended when two `create`s land in the
/// same millisecond. This parser strips the suffix and reads the
/// stem against the same `chrono` format string the minter uses, so
/// the round-trip is exact.
///
/// Returns `None` for any stem that doesn't conform, such as
/// hand-renamed files or future format changes. The caller
/// falls back to file mtime in that case so the row still renders.
pub(crate) fn parse_session_id_created_at(session_id: &str) -> Option<DateTime<Utc>> {
    // Strip a trailing `_<digits>` collision suffix. The mint side
    // never embeds an underscore in the timestamp portion so an
    // underscore unambiguously marks the suffix boundary; we still
    // require the suffix to be all digits to avoid misclassifying
    // an unexpected stem shape as a collision.
    let stem = match session_id.rsplit_once('_') {
        Some((prefix, suffix))
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) =>
        {
            prefix
        }
        _ => session_id,
    };
    NaiveDateTime::parse_from_str(stem, "%Y-%m-%d-%H-%M-%S-%3f")
        .ok()
        .map(|naive| naive.and_utc())
}

/// Return the text from the first [`UserContent::Text`] block in
/// `content`, if any. Used by [`read_session_preview_file`] to
/// capture the user-input preview.
fn first_user_text(content: &[UserContent]) -> Option<String> {
    content.iter().find_map(|b| match b {
        UserContent::Text(t) => {
            let trimmed = t.text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use aj_agent::message::AgentMessage;
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, TextContent, ToolCall, ToolResultMessage,
        UserMessage,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::log::{ConversationLog, ConversationView};

    /// Build a `ConversationPersistence` against a fresh temp dir.
    fn fixture() -> (TempDir, ConversationPersistence) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        (dir, persistence)
    }

    fn user_msg(text: &str) -> AgentMessage {
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

    /// Append one user-text message and one assistant-text message
    /// via the high-level [`ConversationView::add_message`] path.
    fn append_user_then_assistant(log: &mut ConversationLog, u: &str, a: &str) {
        let mut view = ConversationView::user(log);
        view.add_message(user_msg(u)).expect("append user");
        view.add_message(assistant_text(a))
            .expect("append assistant");
    }

    #[test]
    fn list_session_previews_returns_empty_when_dir_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing");
        let persistence = ConversationPersistence::new(path);
        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert!(previews.is_empty());
    }

    #[test]
    fn list_session_previews_captures_first_user_message_and_count() {
        let (_dir, persistence) = fixture();

        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello world", "hi there");

        // A second message on the user thread so the count crosses 2.
        let mut view = ConversationView::user(&mut log);
        view.add_message(user_msg("follow-up"))
            .expect("append second user");

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        assert_eq!(p.session_id, log.session_id());
        assert_eq!(p.message_count, 3);
        assert_eq!(p.first_user_message.as_deref(), Some("hello world"));
        assert!(p.size_bytes > 0);
    }

    /// A file whose stem is not a session id is not a session: the store can
    /// never be asked about it again (the grammar rejects the id at every
    /// lookup), so a row for it would be a directory entry nothing resolves.
    #[test]
    fn enumeration_skips_a_stem_that_is_not_a_session_id() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let real = log.session_id().to_string();
        drop(log);
        for stem in ["not a session", "sneaky.name", "héllo"] {
            std::fs::write(
                persistence.sessions_dir().join(format!("{stem}.jsonl")),
                "{}\n",
            )
            .expect("write a stray log");
        }

        let listed: Vec<String> = persistence
            .enumerate_sessions()
            .expect("enumerate")
            .into_iter()
            .map(|metadata| metadata.session_id)
            .collect();
        assert_eq!(listed, vec![real]);
    }

    /// The single-id membership primitive: one `stat`, and nothing at all for
    /// an id the grammar rejects.
    #[test]
    fn session_metadata_answers_one_id_without_a_directory_read() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let session_id = log.session_id().to_string();
        let size = std::fs::metadata(log.path()).expect("stat").len();
        drop(log);

        let metadata = persistence
            .session_metadata(&session_id)
            .expect("the log is there");
        assert_eq!(metadata.session_id, session_id);
        assert_eq!(metadata.size_bytes, size);

        assert!(
            persistence
                .session_metadata("2026-01-01-00-00-00-000")
                .is_none()
        );
        // The sessions directory itself, reached by climbing out of the store.
        // Nothing may make this resolve, whatever is on disk.
        assert!(persistence.session_metadata("../etc/passwd").is_none());
        assert!(persistence.session_metadata("").is_none());
    }

    /// The format sniff reads bytes, not text, so a log that is not valid
    /// UTF-8 earns a verdict rather than a read failure. That matters because
    /// the caller caches the verdict against the file and treats a failure as
    /// "try again next time", which for a file that will never decode is every
    /// enumeration for the life of the host.
    #[test]
    fn the_format_sniff_reads_bytes_and_always_reaches_a_verdict() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let session_id = log.session_id().to_string();
        let path = log.path().to_path_buf();
        drop(log);

        // The first two bytes of a three-byte character, as a crash mid-append
        // leaves behind. The sniff reads the first line, so it is unaffected.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen the log");
        std::io::Write::write_all(&mut file, &[b'{', 0xe2, 0x82]).expect("append a torn line");
        drop(file);
        assert_eq!(persistence.is_current_format(&session_id), Some(true));

        // A whole log of invalid bytes is a verdict too, not a read failure.
        let blob = persistence
            .sessions_dir()
            .join("2000-01-01-00-00-00-000.jsonl");
        std::fs::write(&blob, [0xff, 0xfe, 0xff]).expect("write a non-utf8 log");
        assert_eq!(
            persistence.is_current_format("2000-01-01-00-00-00-000"),
            Some(false),
        );
    }

    /// The enumeration is the cheap half of a listing: it stats but never
    /// opens, so a pre-refactor file is enumerated like any other and only
    /// `list_sessions` (through the format gate) drops it.
    #[test]
    fn enumeration_keeps_what_the_format_gate_drops() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let session_id = log.session_id().to_string();
        drop(log);
        let old = persistence
            .sessions_dir()
            .join("2000-01-01-00-00-00-000.jsonl");
        std::fs::write(&old, "not json at all\n").expect("write a pre-refactor file");
        // Neither an unrelated extension nor a directory is a session.
        std::fs::write(persistence.sessions_dir().join("host-id"), "id\n").expect("write");
        std::fs::create_dir(persistence.sessions_dir().join("nested.jsonl")).expect("mkdir");

        let enumerated: Vec<String> = persistence
            .enumerate_sessions()
            .expect("enumerate")
            .into_iter()
            .map(|metadata| metadata.session_id)
            .collect();
        assert_eq!(
            enumerated,
            vec![session_id.clone(), "2000-01-01-00-00-00-000".to_string()],
            "both logs are enumerated, latest first",
        );

        let listed: Vec<String> = persistence
            .list_sessions()
            .expect("list")
            .into_iter()
            .map(|metadata| metadata.session_id)
            .collect();
        assert_eq!(
            listed,
            vec![session_id.clone()],
            "the gate drops the old one"
        );
        assert_eq!(persistence.is_current_format(&session_id), Some(true));
        assert_eq!(
            persistence.is_current_format("2000-01-01-00-00-00-000"),
            Some(false),
        );
        assert_eq!(
            persistence.is_current_format("no-such-session"),
            None,
            "a log that cannot be opened has no format verdict",
        );
    }

    /// The enumeration never opens a log, which is what a caller refreshing a
    /// listing on a timer depends on: an unreadable log is still enumerated,
    /// and only the gate (which does open it) has no verdict for it.
    #[cfg(unix)]
    #[test]
    fn enumeration_opens_no_log() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let session_id = log.session_id().to_string();
        let path = log.path().to_path_buf();
        drop(log);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("drop the read bit");
        if File::open(&path).is_ok() {
            // Root ignores the permission bits, so there is nothing to prove
            // here. Skipping beats asserting something that cannot fail.
            return;
        }

        let enumerated: Vec<String> = persistence
            .enumerate_sessions()
            .expect("enumerate")
            .into_iter()
            .map(|metadata| metadata.session_id)
            .collect();
        assert_eq!(
            enumerated,
            vec![session_id.clone()],
            "a log nothing can open is still enumerated, so nothing opened it",
        );
        assert_eq!(persistence.is_current_format(&session_id), None);
        assert!(persistence.list_sessions().expect("list").is_empty());

        // And the verdict comes back the moment the file is readable again,
        // with no change to its size or modification time.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("restore the read bit");
        assert_eq!(persistence.is_current_format(&session_id), Some(true));
    }

    #[test]
    fn list_session_previews_emits_progress_callback_per_file() {
        let (_dir, persistence) = fixture();
        for i in 0..3 {
            let mut log = ConversationLog::create(&persistence).expect("create");
            append_user_then_assistant(&mut log, &format!("prompt {i}"), &format!("reply {i}"));
            // Tiny sleep so the millisecond-resolution mint sees a
            // fresh timestamp for each file.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let progress = RefCell::new(Vec::<(usize, usize)>::new());
        let previews = persistence
            .list_session_previews(|loaded, total| progress.borrow_mut().push((loaded, total)))
            .expect("list");
        assert_eq!(previews.len(), 3);
        let p = progress.into_inner();
        assert_eq!(p, vec![(1, 3), (2, 3), (3, 3)]);
    }

    #[test]
    fn list_session_previews_streaming_matches_batched_order() {
        let (_dir, persistence) = fixture();
        for i in 0..3 {
            let mut log = ConversationLog::create(&persistence).expect("create");
            append_user_then_assistant(&mut log, &format!("prompt {i}"), &format!("reply {i}"));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        // Each emit carries exactly one file's preview, in the same
        // newest-first order the batched listing produces.
        let mut batches = Vec::new();
        persistence.list_session_previews_streaming(&|| false, &mut |b| batches.push(b));
        assert!(
            batches.iter().all(|b| b.len() == 1),
            "expected one preview per batch, got {:?}",
            batches.iter().map(Vec::len).collect::<Vec<_>>()
        );
        let streamed: Vec<String> = batches
            .into_iter()
            .flatten()
            .map(|p| p.session_id)
            .collect();
        let batched: Vec<String> = persistence
            .list_session_previews(|_, _| {})
            .expect("list")
            .into_iter()
            .map(|p| p.session_id)
            .collect();
        assert_eq!(streamed, batched);
    }

    #[test]
    fn list_session_previews_streaming_stops_when_cancelled() {
        let (_dir, persistence) = fixture();
        for i in 0..3 {
            let mut log = ConversationLog::create(&persistence).expect("create");
            append_user_then_assistant(&mut log, &format!("prompt {i}"), &format!("reply {i}"));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        // A predicate that trips after the first file leaves the walk: the
        // between-files check must break before reading the rest.
        let seen = std::cell::Cell::new(0usize);
        let mut batches = Vec::new();
        persistence.list_session_previews_streaming(&|| seen.get() > 0, &mut |b| {
            seen.set(seen.get() + 1);
            batches.push(b);
        });
        assert_eq!(batches.len(), 1, "cancel should stop after the first file");
    }

    #[test]
    fn read_session_preview_file_stops_mid_file() {
        // A single file larger than the in-file poll interval, so the check
        // inside the read loop (not the between-files check) is what bails.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("2024-01-01-00-00-00.jsonl");
        let n = crate::SCAN_CANCEL_CHECK_LINES * 3;
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&path).unwrap();
            for i in 0..n {
                let line = serde_json::to_string(&serde_json::json!({
                    "id": format!("{i:08}"),
                    "thread": "user",
                    "type": "message",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": format!("p{i}")}],
                        "timestamp": 0,
                    },
                }))
                .unwrap();
                writeln!(file, "{line}").unwrap();
            }
        }

        let full = read_session_preview_file("2024-01-01-00-00-00", &path, &|| false)
            .expect("read")
            .expect("valid first line");
        assert_eq!(full.message_count, n);

        // Sticky predicate: false at the line-0 poll, true from line-1024 on.
        let calls = std::cell::Cell::new(0usize);
        let cancel = || {
            let c = calls.get();
            calls.set(c + 1);
            c > 0
        };
        let partial = read_session_preview_file("2024-01-01-00-00-00", &path, &cancel)
            .expect("read")
            .expect("valid first line");
        assert_eq!(
            partial.message_count,
            crate::SCAN_CANCEL_CHECK_LINES,
            "in-file cancel bails at the 1024-line poll, not after the full read"
        );
    }

    #[test]
    fn list_session_previews_streaming_missing_dir_emits_nothing() {
        let dir = TempDir::new().unwrap();
        let persistence = ConversationPersistence::new(dir.path().join("missing"));
        let mut batches = Vec::new();
        persistence.list_session_previews_streaming(&|| false, &mut |b| batches.push(b));
        assert!(batches.is_empty());
    }

    #[test]
    fn list_session_previews_ignores_non_user_first_messages() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        // First message is a tool_result (not a user prompt). The
        // preview should leave `first_user_message` at `None`.
        let mut view = ConversationView::user(&mut log);
        view.add_message(AgentMessage::wire(Message::ToolResult(
            ToolResultMessage::text("x", "ping", "ok", false),
        )))
        .expect("append");

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        assert!(previews[0].first_user_message.is_none());
        assert_eq!(previews[0].message_count, 1);
    }

    #[test]
    fn list_session_previews_skips_pre_refactor_files() {
        let (_dir, persistence) = fixture();
        let bogus = persistence.sessions_dir.join("old.jsonl");
        std::fs::write(&bogus, "not json at all\n").expect("write");

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert!(previews.is_empty(), "got {previews:?}");
    }

    #[test]
    fn list_session_previews_keeps_valid_alongside_pre_refactor_and_counts_all_files() {
        // A pre-refactor file is dropped from the rows but still counts
        // toward the progress total (it's walked in the same single pass
        // as the valid files), so the loaded counter reaches the total
        // even though fewer rows appear.
        let (_dir, persistence) = fixture();
        let sessions_dir = persistence.sessions_dir().to_path_buf();
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        std::fs::write(sessions_dir.join("old.jsonl"), "not json at all\n").expect("write old");

        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");

        let progress = RefCell::new(Vec::<(usize, usize)>::new());
        let previews = persistence
            .list_session_previews(|loaded, total| progress.borrow_mut().push((loaded, total)))
            .expect("list");

        assert_eq!(previews.len(), 1, "only the valid session yields a row");
        assert_eq!(previews[0].session_id, log.session_id());
        let progress = progress.into_inner();
        assert_eq!(progress.last(), Some(&(2, 2)), "both files tick progress");
    }

    #[test]
    fn read_session_preview_file_tolerates_a_torn_later_line() {
        // The first line gates the format (a valid entry here), so a
        // garbage line *after* it is skipped rather than dropping the
        // whole file, matching the resume truncated-line tolerance.
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let path = log.path().to_path_buf();
        let session_id = log.session_id().to_string();
        drop(log);

        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .expect("read log")
            .lines()
            .map(str::to_string)
            .collect();
        // Insert garbage after the first valid line.
        lines.insert(1, "}{ this is not json".to_string());
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("rewrite");

        let preview = read_session_preview_file(&session_id, &path, &|| false)
            .expect("read")
            .expect("a valid first line keeps the file");
        // The two messages survive. Only the torn line is skipped.
        assert_eq!(preview.message_count, 2);
        assert_eq!(preview.first_user_message.as_deref(), Some("hello"));
    }

    #[test]
    fn parse_session_id_created_at_round_trips_minted_id() {
        let parsed = super::parse_session_id_created_at("2025-05-11-14-22-03-512")
            .expect("known-good stem parses");
        let expected = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_milli_opt(14, 22, 3, 512)
            .unwrap()
            .and_utc();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_session_id_created_at_strips_collision_suffix() {
        let suffix = super::parse_session_id_created_at("2025-05-11-14-22-03-512_3")
            .expect("suffixed stem parses");
        let bare = super::parse_session_id_created_at("2025-05-11-14-22-03-512")
            .expect("bare stem parses");
        assert_eq!(suffix, bare);
    }

    #[test]
    fn parse_session_id_created_at_returns_none_for_unrecognised_stem() {
        assert!(super::parse_session_id_created_at("custom-name").is_none());
        assert!(super::parse_session_id_created_at("").is_none());
        assert!(super::parse_session_id_created_at("2025-05-11-14-22-03-512_abc").is_none());
    }

    #[test]
    fn list_session_previews_populates_created_at_from_session_id() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hi", "ok");
        let session_id = log.session_id().to_string();

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        let expected =
            super::parse_session_id_created_at(&session_id).expect("freshly-minted id parses");
        assert_eq!(p.created_at, expected);
    }

    #[test]
    fn list_session_previews_counts_tool_result_entries() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        {
            let mut view = ConversationView::user(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: "tu-1".into(),
                    name: "ping".into(),
                    arguments: serde_json::json!({}),
                })],
                ..AssistantMessage::empty()
            })))
            .expect("a");
            view.add_message(AgentMessage::wire(Message::ToolResult(
                ToolResultMessage::text("tu-1", "ping", "ok", false),
            )))
            .expect("tr");
        }

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        // Three wire-level messages: user, assistant, tool_result.
        assert_eq!(previews[0].message_count, 3);
    }

    #[test]
    fn list_session_previews_falls_back_to_modified_when_no_message_entries() {
        // Legacy on-disk shape: a session file containing only a
        // SystemPrompt entry, with no `Message` entries. New code
        // can't produce this layout (the system prompt buffers and
        // never flushes alone), but files written by older builds
        // still exist on users' disks and the preview walk must
        // render them gracefully. The fallback under test:
        // `last_message_at` defaults to the file mtime when no
        // Message-kind entry contributed a timestamp.
        let (_dir, persistence) = fixture();
        let sessions_dir = persistence.sessions_dir().to_path_buf();
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");

        let session_id = "2024-01-01-00-00-00-000";
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        let line = serde_json::json!({
            "id": "00000000",
            "timestamp": "2024-01-01T00:00:00Z",
            "thread": "meta",
            "type": "system_prompt",
            "text": "legacy abandoned-session prompt",
        });
        std::fs::write(&path, format!("{line}\n")).expect("write legacy file");

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        assert_eq!(p.message_count, 0);
        assert_eq!(p.last_message_at, p.modified);
    }

    #[test]
    fn list_session_previews_uses_largest_message_timestamp() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "world");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut view = ConversationView::user(&mut log);
        view.add_message(user_msg("follow-up"))
            .expect("append user2");

        let previews = persistence.list_session_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        let min_expected = p.created_at + chrono::Duration::milliseconds(10);
        assert!(
            p.last_message_at >= min_expected,
            "last_message_at = {}, expected >= {}",
            p.last_message_at,
            min_expected
        );
    }
}
