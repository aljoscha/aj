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
    /// with a `tracing::info!` note.
    pub fn list_sessions(&self) -> Result<Vec<SessionMetadata>, ConversationError> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&self.sessions_dir)?;
        let mut session_files = Vec::new();

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                if let Some(file_stem) = path.file_stem().and_then(|s| s.to_str()) {
                    session_files.push(file_stem.to_string());
                }
            }
        }

        // Sort by filename (a timestamp), latest first.
        session_files.sort_by(|a, b| b.cmp(a));

        let mut sessions = Vec::new();

        for session_id in session_files {
            let path = self.session_path(&session_id);

            if !Self::looks_like_new_format(&path) {
                tracing::info!(
                    "skipping pre-refactor session file {} (old on-disk format)",
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

            sessions.push(SessionMetadata {
                session_id,
                modified: modified_str,
                size_display,
            });
        }

        Ok(sessions)
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
            if let Some(preview) = read_preview(session_id, &path) {
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
    pub fn list_session_previews_streaming(&self, emit: &mut dyn FnMut(Vec<SessionPreview>)) {
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
            if let Some(preview) = read_preview(session_id, &path) {
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
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&self.sessions_dir)?;
        let mut session_files: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    session_files.push(stem.to_string());
                }
            }
        }
        // Filenames are timestamps; reverse-lexicographic = newest-first.
        session_files.sort_by(|a, b| b.cmp(a));

        Ok(session_files
            .into_iter()
            .map(|id| (id.clone(), self.session_path(&id)))
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
fn read_preview(session_id: String, path: &std::path::Path) -> Option<SessionPreview> {
    match read_session_preview_file(&session_id, path) {
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
    pub modified: String,
    pub size_display: String,
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
/// opened once (the standalone [`ConversationPersistence::looks_like_new_format`]
/// check stays for `list_sessions`, which doesn't otherwise read the
/// file). A later line that fails to parse is skipped (matching the
/// resume-time tolerance for truncated trailing lines). The walk is
/// one-pass: we read every line so `message_count` is accurate, but we
/// stop updating `first_user_message` once we have one.
fn read_session_preview_file(
    session_id: &str,
    path: &std::path::Path,
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

    for line_res in reader.lines() {
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
                if let Some(Message::User(u)) = msg.as_wire() {
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
        let mut view = ConversationView::user(log, None);
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
        let head = log
            .latest_leaf(crate::ThreadFilter::USER)
            .expect("head exists");
        let mut view = ConversationView::user(&mut log, Some(head));
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
        persistence.list_session_previews_streaming(&mut |b| batches.push(b));
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
    fn list_session_previews_streaming_missing_dir_emits_nothing() {
        let dir = TempDir::new().unwrap();
        let persistence = ConversationPersistence::new(dir.path().join("missing"));
        let mut batches = Vec::new();
        persistence.list_session_previews_streaming(&mut |b| batches.push(b));
        assert!(batches.is_empty());
    }

    #[test]
    fn list_session_previews_ignores_non_user_first_messages() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        // First message is a tool_result (not a user prompt). The
        // preview should leave `first_user_message` at `None`.
        let mut view = ConversationView::user(&mut log, None);
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

        let preview = read_session_preview_file(&session_id, &path)
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
            let mut view = ConversationView::user(&mut log, None);
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
        let head = log
            .latest_leaf(crate::ThreadFilter::USER)
            .expect("head exists");
        let mut view = ConversationView::user(&mut log, Some(head));
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
