//! Project-level discovery of conversation thread files.
//!
//! [`ConversationPersistence`] is the owner of a project's threads
//! directory. It lists existing threads (for `aj list-threads` and
//! `aj continue`) and resolves a thread id to its on-disk path so
//! [`crate::log::ConversationLog`] can open / create the right file.

use aj_models::wire::{ContentBlockParam, Role};
use chrono::{DateTime, NaiveDateTime, Utc};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use crate::log::{ConversationEntry, ConversationEntryKind, ConversationError};

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

    pub(crate) fn thread_path(&self, thread_id: &str) -> PathBuf {
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

    /// List threads with rich per-thread previews — first user
    /// message, message count, modified time, file size.
    ///
    /// Walks the threads directory in the same latest-first order
    /// as [`Self::list_threads`], but for each file opens the JSONL
    /// and scans line by line to count `Message` entries and capture
    /// the first user-role textual block. `on_progress(loaded, total)`
    /// fires once per file as previews complete so a caller showing
    /// a "Loading X/Y" indicator can update incrementally. Files
    /// whose first line does not parse as the new [`ConversationEntry`]
    /// shape are skipped (consistent with [`Self::list_threads`]).
    ///
    /// Note on streaming: this function still returns the previews
    /// in one `Vec` after every file has been scanned. The callback
    /// is the streaming surface for progress reporting; a future
    /// extension can flip the return type to an `Iterator` / async
    /// stream if a single project ever grows enough threads that
    /// the cumulative scan time becomes user-visible.
    pub fn list_thread_previews(
        &self,
        mut on_progress: impl FnMut(usize, usize),
    ) -> Result<Vec<ThreadPreview>, ConversationError> {
        if !self.threads_dir.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&self.threads_dir)?;
        let mut thread_files: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    thread_files.push(stem.to_string());
                }
            }
        }
        thread_files.sort_by(|a, b| b.cmp(a));

        let mut previews = Vec::with_capacity(thread_files.len());
        // Filter out pre-refactor files up-front so `total` in the
        // progress callback reflects only files we'll actually
        // surface — otherwise the bar would stall partway through.
        let candidate_paths: Vec<(String, PathBuf)> = thread_files
            .into_iter()
            .map(|id| (id.clone(), self.thread_path(&id)))
            .filter(|(_, p)| Self::looks_like_new_format(p))
            .collect();

        let total = candidate_paths.len();
        for (i, (thread_id, path)) in candidate_paths.into_iter().enumerate() {
            // Surface a preview even if a per-file read fails: the
            // selector should still show the entry so the user can
            // see which thread couldn't be parsed.
            let preview = read_thread_preview_file(&thread_id, &path).unwrap_or_else(|err| {
                tracing::warn!(
                    "failed to read preview for thread {}: {err}",
                    path.display()
                );
                ThreadPreview::placeholder(thread_id, &path)
            });
            previews.push(preview);
            on_progress(i + 1, total);
        }

        Ok(previews)
    }
}

/// Metadata about a conversation thread.
#[derive(Debug, Clone)]
pub struct ThreadMetadata {
    pub thread_id: String,
    pub modified: String,
    pub size_display: String,
}

/// Richer per-thread snapshot used by the interactive session
/// selector overlay.
///
/// Unlike [`ThreadMetadata`] (which is purely a filesystem-stat
/// payload), [`ThreadPreview`] opens the JSONL and walks far enough
/// to count `Message` entries and capture the first user-role text
/// block. Producing one preview is therefore O(file size) per
/// thread; [`ConversationPersistence::list_thread_previews`] streams
/// progress through a callback so a UI rendering the list can show
/// a `Loading X/Y` indicator while the walk completes.
#[derive(Debug, Clone)]
pub struct ThreadPreview {
    /// Filename stem of the thread file (e.g.
    /// `2025-05-11-14-22-03-512`).
    pub thread_id: String,
    /// Modification time read from the file system. Held as a
    /// real [`DateTime`] (not a pre-formatted string) so the
    /// renderer can choose whatever date/age formatting it likes.
    pub modified: DateTime<Utc>,
    /// Thread creation time. Parsed from `thread_id` (which is
    /// minted as a millisecond-precision UTC timestamp on
    /// [`crate::log::ConversationLog::create`]). Falls back to
    /// `modified` when the id doesn't parse — placeholder previews,
    /// hand-renamed files, or a future filename format that this
    /// build doesn't recognise still produce a structurally
    /// complete row.
    pub created_at: DateTime<Utc>,
    /// Time of the most recently appended message-kind entry.
    /// Captured during the JSONL walk in
    /// [`read_thread_preview_file`] as the largest
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
    /// log. User and assistant messages both contribute; tool
    /// results are projected on the wire as user-role messages so
    /// they count too. Non-message entries (`SystemPrompt`,
    /// `UserOutput`) are skipped.
    pub message_count: usize,
    /// First user-role textual content block in the file, if any.
    /// `None` for a freshly-minted thread that hasn't yet seen a
    /// user prompt. The string carries the verbatim text — the
    /// renderer applies its own truncation policy.
    pub first_user_message: Option<String>,
}

impl ThreadPreview {
    /// Build a minimal preview for a file we could not parse — only
    /// the id and file-system stat fields are populated. Used as a
    /// fall-back so a corrupt thread file still appears in the
    /// selector instead of silently dropping out of the listing.
    fn placeholder(thread_id: String, path: &std::path::Path) -> Self {
        let (modified, size_bytes) = match fs::metadata(path) {
            Ok(md) => {
                let modified = md
                    .modified()
                    .map(DateTime::<Utc>::from)
                    .unwrap_or_else(|_| Utc::now());
                (modified, md.len())
            }
            Err(_) => (Utc::now(), 0),
        };
        // Parse the creation time from the filename stem; fall back
        // to `modified` so the row still has a complete metadata
        // triple to render.
        let created_at = parse_thread_id_created_at(&thread_id).unwrap_or(modified);
        Self {
            thread_id,
            modified,
            created_at,
            // No message-kind entries parsed: the cheap fallback is
            // the file mtime, matching what `format_age` would have
            // returned before this field existed.
            last_message_at: modified,
            size_bytes,
            message_count: 0,
            first_user_message: None,
        }
    }
}

/// Open `path`, walk every JSONL line, and assemble a
/// [`ThreadPreview`].
///
/// Lines that fail to parse are skipped (matching the resume-time
/// tolerance for truncated trailing lines). The walk is one-pass:
/// we read every line so `message_count` is accurate, but we stop
/// updating `first_user_message` once we have one.
fn read_thread_preview_file(
    thread_id: &str,
    path: &std::path::Path,
) -> Result<ThreadPreview, ConversationError> {
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
    // writes — a tool result that lands after a streaming assistant
    // message finalised, for example.
    let mut last_message_at: Option<DateTime<Utc>> = None;

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
        let Ok(entry) = serde_json::from_str::<ConversationEntry>(&line) else {
            continue;
        };
        // Both the legacy [`ConversationEntryKind::Message`] entries
        // and the structured [`ConversationEntryKind::ToolResult`]
        // entries (introduced by the §3 work — see
        // `docs/aj-next-progress.md`) carry wire-level messages that
        // count toward the thread's user-visible message count and
        // its `last_message_at` recency bucket. The preview only
        // ever needs the user's first text — which lives on a
        // [`Message`] entry — for the row label, so the
        // `first_user_message` capture stays on the
        // [`Message`]-only path.
        match &entry.entry {
            ConversationEntryKind::Message(msg) => {
                message_count += 1;
                if first_user_message.is_none() && matches!(msg.role, Role::User) {
                    if let Some(text) = first_text_block(&msg.content) {
                        first_user_message = Some(text);
                    }
                }
                if let Some(ts) = entry.timestamp {
                    last_message_at = Some(match last_message_at {
                        Some(prev) if prev >= ts => prev,
                        _ => ts,
                    });
                }
            }
            ConversationEntryKind::ToolResult { .. } => {
                message_count += 1;
                if let Some(ts) = entry.timestamp {
                    last_message_at = Some(match last_message_at {
                        Some(prev) if prev >= ts => prev,
                        _ => ts,
                    });
                }
            }
            _ => {}
        }
    }

    // Creation time: derived from the filename stem rather than
    // a per-entry timestamp so a thread with no appended messages
    // (a freshly-minted log) still has a meaningful "created"
    // marker for the selector. Fall back to the file mtime if the
    // stem doesn't parse.
    let created_at = parse_thread_id_created_at(thread_id).unwrap_or(modified);
    // `last_message_at` falls back to the file mtime for two cases:
    // logs predating the per-entry timestamping work (every entry
    // has `timestamp: None`) and freshly-minted threads with no
    // message-kind entries yet. The fallback matches the value the
    // selector would have rendered as `modified` under the older
    // single-field design.
    let last_message_at = last_message_at.unwrap_or(modified);

    Ok(ThreadPreview {
        thread_id: thread_id.to_string(),
        modified,
        created_at,
        last_message_at,
        size_bytes,
        message_count,
        first_user_message,
    })
}

/// Parse a thread id minted by [`crate::log::ConversationLog::create`]
/// back into the UTC instant it represents.
///
/// The mint format is `%Y-%m-%d-%H-%M-%S-%3f` with an optional
/// `_<N>` collision suffix appended when two `create`s land in the
/// same millisecond. This parser strips the suffix and reads the
/// stem against the same `chrono` format string the minter uses, so
/// the round-trip is exact.
///
/// Returns `None` for any stem that doesn't conform — placeholder
/// ids, hand-renamed files, or future format changes. The caller
/// falls back to file mtime in that case so the row still renders.
fn parse_thread_id_created_at(thread_id: &str) -> Option<DateTime<Utc>> {
    // Strip a trailing `_<digits>` collision suffix. The mint side
    // never embeds an underscore in the timestamp portion so an
    // underscore unambiguously marks the suffix boundary; we still
    // require the suffix to be all digits to avoid misclassifying
    // an unexpected stem shape as a collision.
    let stem = match thread_id.rsplit_once('_') {
        Some((prefix, suffix))
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) =>
        {
            prefix
        }
        _ => thread_id,
    };
    NaiveDateTime::parse_from_str(stem, "%Y-%m-%d-%H-%M-%S-%3f")
        .ok()
        .map(|naive| naive.and_utc())
}

/// Return the text from the first [`ContentBlockParam::TextBlock`]
/// in `content`, if any. Used by [`read_thread_preview_file`] to
/// capture the user-input preview without dragging tool-result
/// content into it (tool result wire messages are user-role).
fn first_text_block(content: &[ContentBlockParam]) -> Option<String> {
    content.iter().find_map(|b| match b {
        ContentBlockParam::TextBlock { text, .. } => {
            let trimmed = text.trim();
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

    use aj_models::wire::{ContentBlockParam, MessageParam, Role};
    use tempfile::TempDir;

    use super::*;
    use crate::log::{ConversationLog, ThreadKind};

    /// Build a `ConversationPersistence` against a fresh temp dir.
    fn fixture() -> (TempDir, ConversationPersistence) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        (dir, persistence)
    }

    /// Append one user-text message and one assistant-text message to
    /// `log`, returning the new tail id. Used by the preview tests so
    /// the JSONL contains exactly the entries we want to assert on.
    fn append_user_then_assistant(
        log: &mut ConversationLog,
        user_text: &str,
        assistant_text: &str,
    ) {
        let user_msg = MessageParam {
            role: Role::User,
            content: vec![ContentBlockParam::TextBlock {
                text: user_text.to_string(),
                citations: None,
                signature: None,
            }],
        };
        let user_id = log
            .append(
                None,
                ThreadKind::User,
                None,
                ConversationEntryKind::Message(user_msg),
            )
            .expect("append user");
        let assistant_msg = MessageParam {
            role: Role::Assistant,
            content: vec![ContentBlockParam::TextBlock {
                text: assistant_text.to_string(),
                citations: None,
                signature: None,
            }],
        };
        log.append(
            Some(user_id),
            ThreadKind::User,
            None,
            ConversationEntryKind::Message(assistant_msg),
        )
        .expect("append assistant");
    }

    #[test]
    fn list_thread_previews_returns_empty_when_dir_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing");
        let persistence = ConversationPersistence::new(path);
        let previews = persistence.list_thread_previews(|_, _| {}).expect("list");
        assert!(previews.is_empty());
    }

    #[test]
    fn list_thread_previews_captures_first_user_message_and_count() {
        let (_dir, persistence) = fixture();

        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello world", "hi there");

        // A second user-thread message so the count crosses 2.
        let head = log
            .latest_leaf(crate::ThreadFilter::USER)
            .expect("head exists");
        let user_msg2 = MessageParam {
            role: Role::User,
            content: vec![ContentBlockParam::TextBlock {
                text: "follow-up".to_string(),
                citations: None,
                signature: None,
            }],
        };
        log.append(
            Some(head),
            ThreadKind::User,
            None,
            ConversationEntryKind::Message(user_msg2),
        )
        .expect("append second user");

        let previews = persistence.list_thread_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        assert_eq!(p.thread_id, log.thread_id());
        assert_eq!(p.message_count, 3);
        assert_eq!(p.first_user_message.as_deref(), Some("hello world"));
        assert!(p.size_bytes > 0);
    }

    #[test]
    fn list_thread_previews_emits_progress_callback_per_file() {
        let (_dir, persistence) = fixture();
        // Three separate thread files.
        for i in 0..3 {
            let mut log = ConversationLog::create(&persistence).expect("create");
            append_user_then_assistant(&mut log, &format!("prompt {i}"), &format!("reply {i}"));
            // Tiny sleep so the millisecond-resolution mint sees a
            // fresh timestamp for each file — otherwise `_N` suffix
            // collisions still produce distinct files but ordering
            // becomes a function of suffix rather than time.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let progress = RefCell::new(Vec::<(usize, usize)>::new());
        let previews = persistence
            .list_thread_previews(|loaded, total| progress.borrow_mut().push((loaded, total)))
            .expect("list");
        assert_eq!(previews.len(), 3);
        let p = progress.into_inner();
        // One callback per file, total is the candidate count.
        assert_eq!(p, vec![(1, 3), (2, 3), (3, 3)]);
    }

    #[test]
    fn list_thread_previews_ignores_first_text_when_empty() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        // First user message with no text block (e.g. only tool
        // results). The preview should leave `first_user_message`
        // at `None`.
        let tool_only = MessageParam {
            role: Role::User,
            content: vec![ContentBlockParam::ToolResultBlock {
                tool_use_id: "x".into(),
                content: String::from("ok").into(),
                is_error: false,
            }],
        };
        log.append(
            None,
            ThreadKind::User,
            None,
            ConversationEntryKind::Message(tool_only),
        )
        .expect("append");

        let previews = persistence.list_thread_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        assert!(previews[0].first_user_message.is_none());
        assert_eq!(previews[0].message_count, 1);
    }

    #[test]
    fn list_thread_previews_skips_pre_refactor_files() {
        let (_dir, persistence) = fixture();
        // Drop a bogus .jsonl that won't parse as the new format.
        let bogus = persistence.threads_dir.join("old.jsonl");
        std::fs::write(&bogus, "not json at all\n").expect("write");

        let previews = persistence.list_thread_previews(|_, _| {}).expect("list");
        assert!(previews.is_empty(), "got {previews:?}");
    }

    #[test]
    fn parse_thread_id_created_at_round_trips_minted_id() {
        // The mint format is `%Y-%m-%d-%H-%M-%S-%3f`; parsing a known
        // stem should give back the corresponding UTC instant.
        let parsed = super::parse_thread_id_created_at("2025-05-11-14-22-03-512")
            .expect("known-good stem parses");
        // Build the same instant from components and compare.
        let expected = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_milli_opt(14, 22, 3, 512)
            .unwrap()
            .and_utc();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_thread_id_created_at_strips_collision_suffix() {
        // `_<digits>` suffix marks an intra-millisecond collision —
        // strip it before parsing. The result should be the same as
        // the suffix-less id.
        let suffix = super::parse_thread_id_created_at("2025-05-11-14-22-03-512_3")
            .expect("suffixed stem parses");
        let bare =
            super::parse_thread_id_created_at("2025-05-11-14-22-03-512").expect("bare stem parses");
        assert_eq!(suffix, bare);
    }

    #[test]
    fn parse_thread_id_created_at_returns_none_for_unrecognised_stem() {
        // Hand-renamed files, placeholder ids, or future formats
        // should produce `None` so the caller can fall back to
        // file-mtime.
        assert!(super::parse_thread_id_created_at("custom-name").is_none());
        assert!(super::parse_thread_id_created_at("").is_none());
        // Underscore with non-digit suffix isn't a collision marker.
        assert!(super::parse_thread_id_created_at("2025-05-11-14-22-03-512_abc").is_none());
    }

    #[test]
    fn list_thread_previews_populates_created_at_from_thread_id() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hi", "ok");
        let thread_id = log.thread_id().to_string();

        let previews = persistence.list_thread_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        // The id was minted from `Utc::now()` at create-time, so the
        // parsed `created_at` should land within a few seconds of
        // when the test ran. We assert it parses back to the same
        // instant the id encodes by re-parsing the stem ourselves.
        let expected =
            super::parse_thread_id_created_at(&thread_id).expect("freshly-minted id parses");
        assert_eq!(p.created_at, expected);
    }

    #[test]
    fn list_thread_previews_counts_structured_tool_result_entries_as_messages() {
        // The persistence preview's `message_count` and
        // `last_message_at` must include the structured
        // [`ConversationEntryKind::ToolResult`] entries written
        // through [`ConversationView::add_tool_result`], not just
        // the legacy [`ConversationEntryKind::Message`] entries.
        // Otherwise a session that only used the new variant for
        // its tool results would underreport its message count and
        // surface an artificially-old `last_message_at` in the
        // selector overlay.
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        // Seed a user / assistant exchange via the high-level view
        // so each line lands with a real timestamp.
        {
            let mut view = crate::log::ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("u");
            view.add_assistant_message(vec![ContentBlockParam::ToolUseBlock {
                id: "tu-1".to_string(),
                name: "ping".to_string(),
                input: serde_json::json!({}),
                caller: None,
            }])
            .expect("a");
            view.add_tool_result(
                vec![ContentBlockParam::ToolResultBlock {
                    tool_use_id: "tu-1".to_string(),
                    content: "ok".to_string().into(),
                    is_error: false,
                }],
                std::collections::HashMap::new(),
            )
            .expect("tool result");
        }

        let previews = persistence.list_thread_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        // Three wire-level messages: user prompt, assistant tool-use
        // turn, structured tool result.
        assert_eq!(p.message_count, 3);
    }

    #[test]
    fn list_thread_previews_falls_back_to_modified_for_last_message_at_when_no_entries() {
        let (_dir, persistence) = fixture();
        // Create a log but don't append anything — the file stays
        // empty so there are zero message-kind entries.
        let _log = ConversationLog::create(&persistence).expect("create");
        // The file is created lazily on first append; we need an
        // on-disk file for `list_thread_previews` to see it, so
        // append a single SystemPrompt entry (not a Message-kind
        // entry, so it shouldn't bump `last_message_at`).
        let mut log = _log;
        log.append(
            None,
            ThreadKind::Meta,
            None,
            ConversationEntryKind::SystemPrompt {
                text: "test".to_string(),
            },
        )
        .expect("append system prompt");

        let previews = persistence.list_thread_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        assert_eq!(p.message_count, 0);
        // No message-kind entry was appended → `last_message_at`
        // falls back to the file mtime.
        assert_eq!(p.last_message_at, p.modified);
    }

    #[test]
    fn list_thread_previews_uses_largest_message_timestamp() {
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        // Append two messages with a real time gap so each line gets
        // a distinct timestamp.
        append_user_then_assistant(&mut log, "hello", "world");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let head = log
            .latest_leaf(crate::ThreadFilter::USER)
            .expect("head exists");
        let user2 = MessageParam {
            role: Role::User,
            content: vec![ContentBlockParam::TextBlock {
                text: "follow-up".to_string(),
                citations: None,
                signature: None,
            }],
        };
        log.append(
            Some(head),
            ThreadKind::User,
            None,
            ConversationEntryKind::Message(user2),
        )
        .expect("append user2");

        let previews = persistence.list_thread_previews(|_, _| {}).expect("list");
        assert_eq!(previews.len(), 1);
        let p = &previews[0];
        // The last appended message is the latest in this case, so
        // `last_message_at` should match its timestamp. We can't
        // assert the exact instant (the test doesn't know it) but
        // it must be strictly after the first message's timestamp
        // window — we sanity-check by requiring `last_message_at`
        // to be no earlier than `created_at` + 10ms.
        let min_expected = p.created_at + chrono::Duration::milliseconds(10);
        assert!(
            p.last_message_at >= min_expected,
            "last_message_at = {}, expected >= {}",
            p.last_message_at,
            min_expected
        );
    }
}
