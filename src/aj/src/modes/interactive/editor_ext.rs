//! Editor extensions on top of [`aj_tui::EditorComponent`].
//!
//! Plug-ins that turn the bare editor into the prompt surface:
//! `@file` autocomplete (driven by [`aj_tui::autocomplete`]),
//! prompt-history wiring, and multi-line submit handling.
//!
//! Today this module owns the [`PromptHistory`] type, which
//! bootstraps an in-memory prompt history from the project's JSONL
//! session logs and installs it into a freshly-built editor. Live
//! submissions are recorded by the host's submit handler via
//! [`aj_tui::components::editor::Editor::add_to_history`] directly,
//! so no separate "record" path lives here.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use aj_agent::message::{AgentMessage, AgentMessageKind};
use aj_models::types::{Message, UserContent};
use aj_session::{ConversationEntry, ConversationEntryKind, ConversationPersistence, ThreadKind};
use aj_tui::components::editor::Editor;

/// Default cap on the number of prompts retained.
///
/// Set above [`Editor::HISTORY_LIMIT`] (100) so a fresh bootstrap
/// over-supplies the editor's ring and lets the editor's own cap
/// keep only the most recent entries automatically.
pub const DEFAULT_MAX_ENTRIES: usize = 200;

/// In-memory prompt history extracted from on-disk session logs.
///
/// Newest entry is at the back of the queue. [`install`] pushes
/// entries into an [`Editor`] in oldest-first order so that pressing
/// Up once surfaces the most recent prompt and the editor's own
/// dedup / cap apply naturally to the resulting ring.
///
/// Why this lives in-memory rather than on a separate `history.txt`:
///
/// 1. **Brittle to non-UTF-8 content.** A flat history file read
///    with `BufReader::lines()` errors out on the first invalid-UTF-8
///    line, then a load-then-rewrite-whole-file pattern truncates
///    every entry past the corruption point on the next submit.
/// 2. **Concurrent-process clobber.** Two `aj` processes
///    running side by side each read the file, add their own new
///    entry, and rewrite the whole file. Last writer wins; the
///    other terminal's entries are silently lost.
///
/// The conversation log we already maintain is JSONL: every line
/// is independently parseable, arbitrary bytes round-trip via
/// `serde_json` escaping, and each session file is owned by exactly
/// one running process. It is therefore the natural source of truth
/// for "prompts the user has ever submitted in this project".
///
/// [`install`]: PromptHistory::install
pub struct PromptHistory {
    entries: VecDeque<String>,
    max: usize,
}

impl PromptHistory {
    /// Construct an empty history capped at `max` entries
    /// (minimum 1).
    pub fn new(max: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max: max.max(1),
        }
    }

    /// Extract the most recent `max` user-text prompts from the
    /// project's sessions directory, in chronological order.
    ///
    /// Session files are walked newest-first and the scan stops as
    /// soon as `max` prompts are collected, so a project with a large
    /// backlog of old logs only pays for the newest file or two rather
    /// than parsing every log. (This runs on a background thread off
    /// the startup path, but the early stop keeps that thread's work
    /// bounded regardless.)
    ///
    /// Robustness contract:
    ///
    /// - A read error on a single file (permission, missing, IO)
    ///   is logged and that file is skipped; other files still load.
    /// - A line that is not valid UTF-8 is skipped without aborting
    ///   the rest of the file.
    /// - A line that is valid UTF-8 but does not parse as a
    ///   [`ConversationEntry`] is skipped without aborting the rest
    ///   of the file.
    /// - Subagent threads and meta entries are ignored — only
    ///   top-level user messages count as "prompts the human typed".
    /// - Tool-result-only user messages (no text block) contribute
    ///   nothing.
    pub fn bootstrap(persistence: &ConversationPersistence, max: usize) -> Self {
        let mut history = Self::new(max);
        let dir = persistence.sessions_dir();

        if !dir.exists() {
            return history;
        }

        let read_dir = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::debug!("could not read sessions dir {}: {e}", dir.display());
                return history;
            }
        };

        let mut files: Vec<_> = read_dir
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();

        // Filenames are timestamps. Reverse-lex sort puts the newest
        // file first, so collecting newest-first lets the early stop at
        // `max` touch only recent logs. We reverse back to chronological
        // before storing.
        files.sort();
        files.reverse();

        // Consecutive-duplicate suppression is symmetric under reversal
        // (a run of equal prompts collapses the same forwards or
        // backwards), so comparing each prompt against the previously
        // kept one while walking newest-first yields the same deduped
        // chronological sequence a forward walk would, bounded to the
        // most recent `max`.
        let mut newest_first: Vec<String> = Vec::new();
        'outer: for path in &files {
            for text in scan_file_user_prompts(path).into_iter().rev() {
                let Some(norm) = normalize_prompt(&text) else {
                    continue;
                };
                if newest_first.last().map(String::as_str) == Some(norm) {
                    continue;
                }
                newest_first.push(norm.to_string());
                if newest_first.len() >= history.max {
                    break 'outer;
                }
            }
        }

        newest_first.reverse();
        history.entries = newest_first.into();
        history
    }

    /// Seed `editor`'s history ring with these prompts, oldest first.
    ///
    /// Delegates to [`Editor::seed_history`], so the entries land
    /// beneath any prompts already submitted this session (the scan is
    /// backgrounded and can finish after the first submission). After
    /// it returns, pressing Up surfaces the most recent prompt and the
    /// editor's own [`Editor::HISTORY_LIMIT`] cap applies.
    pub fn install(&self, editor: &mut Editor) {
        let entries: Vec<String> = self.entries.iter().cloned().collect();
        editor.seed_history(&entries);
    }

    /// Total entries currently retained.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` iff no entries are retained.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate entries oldest-first.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|s| s.as_str())
    }
}

/// Pull text-block content out of a user message, joining multiple
/// text blocks with a newline. Returns `None` if there is no text
/// content (e.g. a tool-result message or an assistant message).
pub(crate) fn extract_user_prompt_text(msg: &AgentMessage) -> Option<String> {
    let user = match &msg.kind {
        AgentMessageKind::Wire(Message::User(u)) => u,
        _ => return None,
    };
    let parts: Vec<&str> = user
        .content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Read the user-typed prompt texts from one session file, in
/// chronological (file) order. Shared by the editor's Up-arrow ring
/// and the `/history` overlay; each caller applies its own trimming
/// and dedup to the raw joined text returned here.
///
/// A session log is mostly assistant turns and tool results whose
/// bodies dwarf the occasional user prompt. To keep a scan of a large
/// project's logs cheap, each line is first parsed into a tiny
/// [`PromptHead`] capturing only the thread and message role; the
/// expensive full [`ConversationEntry`] parse (which allocates the
/// message-content tree) runs only for lines that really are top-level
/// user messages.
///
/// Honors the failure-isolation contract documented on
/// [`PromptHistory::bootstrap`]: an unreadable file yields no prompts,
/// and non-UTF-8 or unparseable lines are skipped without aborting the
/// rest of the file.
pub(crate) fn scan_file_user_prompts(path: &Path) -> Vec<String> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!("skipping unreadable session file {}: {e}", path.display());
            return Vec::new();
        }
    };

    let mut prompts = Vec::new();
    for (lineno, line) in BufReader::new(file).lines().enumerate() {
        // A non-UTF-8 (or IO-erroring) line is skipped, not fatal:
        // the failure-isolation property a flat-file format lacks.
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let head: PromptHead = match serde_json::from_str(&line) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(
                    "skipping unparseable line {} in {}: {e}",
                    lineno + 1,
                    path.display()
                );
                continue;
            }
        };
        if !head.is_user_prompt() {
            continue;
        }
        // Confirmed a top-level user message; the full parse is what
        // actually pulls the text content out.
        if let Ok(entry) = serde_json::from_str::<ConversationEntry>(&line)
            && let ConversationEntryKind::Message { message: msg } = entry.entry
            && let Some(text) = extract_user_prompt_text(&msg)
        {
            prompts.push(text);
        }
    }
    prompts
}

/// A minimal view of one log line: just enough to tell whether it is a
/// top-level user message. Unlisted fields (including the message
/// `content`) are ignored, so serde walks past the heavy body without
/// allocating it.
#[derive(serde::Deserialize)]
struct PromptHead {
    thread: ThreadKind,
    #[serde(default)]
    message: Option<PromptHeadMessage>,
}

#[derive(serde::Deserialize)]
struct PromptHeadMessage {
    #[serde(default)]
    role: Option<String>,
}

impl PromptHead {
    /// A line is a user prompt when it is on the user thread and its
    /// message role is `user` (the `role` tag of [`Message::User`]).
    /// Assistant / tool-result messages and non-message entries
    /// (system prompt, settings records) are excluded.
    fn is_user_prompt(&self) -> bool {
        matches!(self.thread, ThreadKind::User)
            && self.message.as_ref().and_then(|m| m.role.as_deref()) == Some("user")
    }
}

/// Trim a prompt for storage: drop trailing whitespace (keeping any
/// trailing newline) and leading spaces/tabs. Returns `None` when only
/// whitespace remains.
fn normalize_prompt(text: &str) -> Option<&str> {
    let trimmed = text.trim_end_matches(|c: char| c.is_whitespace() && c != '\n');
    let trimmed = trimmed.trim_start_matches(|c: char| c == ' ' || c == '\t');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Allocate a unique scratch directory under the system temp
    /// dir. Manual cleanup only — tests are expected to assert
    /// behaviour and not pile up a meaningful amount of data.
    fn scratch_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("aj-prompt-history-{label}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_jsonl(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(format!("{name}.jsonl"));
        let mut f = File::create(&path).unwrap();
        for line in lines {
            f.write_all(line.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
        path
    }

    fn user_message_line(text: &str, id: &str) -> String {
        let payload = serde_json::json!({
            "id": id,
            "thread": "user",
            "type": "message",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": text}],
                "timestamp": 0,
            },
        });
        serde_json::to_string(&payload).unwrap()
    }

    fn assistant_message_line(text: &str, id: &str) -> String {
        let payload = serde_json::json!({
            "id": id,
            "thread": "user",
            "type": "message",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": text}],
                "api": "scripted",
                "provider": "scripted",
                "model": "scripted",
                "usage": {
                    "input": 0,
                    "output": 0,
                    "cache_read": 0,
                    "cache_write": 0,
                    "total_tokens": 0,
                    "cost": {"input": 0.0, "output": 0.0, "cache_read": 0.0, "cache_write": 0.0, "total": 0.0},
                },
                "stop_reason": "Stop",
                "timestamp": 0,
            },
        });
        serde_json::to_string(&payload).unwrap()
    }

    fn subagent_user_message_line(text: &str, id: &str) -> String {
        let payload = serde_json::json!({
            "id": id,
            "thread": "subagent",
            "agent_id": 1,
            "type": "message",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": text}],
                "timestamp": 0,
            },
        });
        serde_json::to_string(&payload).unwrap()
    }

    fn bootstrap_for(dir: &Path, max: usize) -> PromptHistory {
        let p = ConversationPersistence::new(dir.to_path_buf());
        PromptHistory::bootstrap(&p, max)
    }

    #[test]
    fn bootstrap_returns_user_prompts_in_chronological_order() {
        let dir = scratch_dir("order");
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[
                &user_message_line("first prompt", "1"),
                &assistant_message_line("answer", "2"),
                &user_message_line("second prompt", "3"),
            ],
        );
        write_jsonl(
            &dir,
            "2024-02-01-00-00-00",
            &[&user_message_line("third prompt", "1")],
        );

        let h = bootstrap_for(&dir, 100);
        let entries: Vec<&str> = h.iter().collect();
        assert_eq!(
            entries,
            vec!["first prompt", "second prompt", "third prompt"]
        );
    }

    #[test]
    fn bootstrap_skips_lines_that_are_not_valid_utf8() {
        let dir = scratch_dir("badutf8");
        let path = dir.join("2024-01-01-00-00-00.jsonl");
        let mut f = File::create(&path).unwrap();
        writeln!(f, "{}", user_message_line("good before", "1")).unwrap();
        // \xc3\x28 is an invalid UTF-8 sequence.
        f.write_all(b"\xc3\x28 not valid utf-8 garbage paste\n")
            .unwrap();
        writeln!(f, "{}", user_message_line("good after", "3")).unwrap();
        drop(f);

        let h = bootstrap_for(&dir, 100);
        let entries: Vec<&str> = h.iter().collect();
        assert_eq!(entries, vec!["good before", "good after"]);
    }

    #[test]
    fn bootstrap_skips_lines_that_are_not_parseable_json() {
        let dir = scratch_dir("badjson");
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[
                &user_message_line("before", "1"),
                "this is not json at all",
                "{not closed",
                &user_message_line("after", "2"),
            ],
        );
        let h = bootstrap_for(&dir, 100);
        let entries: Vec<&str> = h.iter().collect();
        assert_eq!(entries, vec!["before", "after"]);
    }

    #[test]
    fn bootstrap_round_trips_null_bytes_in_prompts() {
        let dir = scratch_dir("nullbytes");
        let weird = "danger\u{0000}ous\u{0000}pasted";
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[&user_message_line(weird, "1")],
        );
        let h = bootstrap_for(&dir, 100);
        let entries: Vec<&str> = h.iter().collect();
        assert_eq!(entries, vec![weird]);
    }

    #[test]
    fn bootstrap_round_trips_multiline_prompts() {
        let dir = scratch_dir("multiline");
        let multi = "line one\nline two\nline three";
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[&user_message_line(multi, "1")],
        );
        let h = bootstrap_for(&dir, 100);
        let entries: Vec<&str> = h.iter().collect();
        assert_eq!(entries, vec![multi]);
    }

    #[test]
    fn bootstrap_dedupes_consecutive_only() {
        let dir = scratch_dir("dedup");
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[
                &user_message_line("a", "1"),
                &user_message_line("a", "2"),
                &user_message_line("b", "3"),
                &user_message_line("a", "4"),
            ],
        );
        let h = bootstrap_for(&dir, 100);
        let entries: Vec<&str> = h.iter().collect();
        assert_eq!(entries, vec!["a", "b", "a"]);
    }

    #[test]
    fn bootstrap_dedupes_consecutive_across_file_boundaries() {
        let dir = scratch_dir("dedup-cross-file");
        // Older file ends with "x"; the newer file starts with "x". In
        // chronological order those two are adjacent, so the duplicate
        // collapses even though it straddles the file boundary. This
        // locks the newest-first scan's central equivalence claim.
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[&user_message_line("a", "1"), &user_message_line("x", "2")],
        );
        write_jsonl(
            &dir,
            "2024-02-01-00-00-00",
            &[&user_message_line("x", "1"), &user_message_line("b", "2")],
        );
        let h = bootstrap_for(&dir, 100);
        let entries: Vec<&str> = h.iter().collect();
        assert_eq!(entries, vec!["a", "x", "b"]);
    }

    #[test]
    fn bootstrap_caps_at_max() {
        let dir = scratch_dir("cap");
        let lines: Vec<String> = (0..500)
            .map(|i| user_message_line(&format!("p{i}"), &i.to_string()))
            .collect();
        let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_jsonl(&dir, "2024-01-01-00-00-00", &line_refs);

        let h = bootstrap_for(&dir, 200);
        assert_eq!(h.len(), 200);
        let entries: Vec<&str> = h.iter().collect();
        assert_eq!(entries.first().copied(), Some("p300"));
        assert_eq!(entries.last().copied(), Some("p499"));
    }

    #[test]
    fn bootstrap_ignores_subagent_threads() {
        let dir = scratch_dir("subagent");
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[
                &user_message_line("real prompt", "1"),
                &subagent_user_message_line("synthetic subagent prompt", "2"),
            ],
        );
        let h = bootstrap_for(&dir, 100);
        let entries: Vec<&str> = h.iter().collect();
        assert_eq!(entries, vec!["real prompt"]);
    }

    #[test]
    fn bootstrap_ignores_assistant_messages_and_tool_results() {
        let dir = scratch_dir("assistant");
        // A tool_result message (not a user prompt).
        let tool_result_only = serde_json::to_string(&serde_json::json!({
            "id": "5",
            "thread": "user",
            "type": "message",
            "message": {
                "role": "tool_result",
                "tool_call_id": "tu_1",
                "tool_name": "ping",
                "content": [{"type": "text", "text": "ok"}],
                "is_error": false,
                "timestamp": 0,
            },
        }))
        .unwrap();

        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[
                &assistant_message_line("hello there", "1"),
                &tool_result_only,
                &user_message_line("the only real prompt", "9"),
            ],
        );
        let h = bootstrap_for(&dir, 100);
        let entries: Vec<&str> = h.iter().collect();
        assert_eq!(entries, vec!["the only real prompt"]);
    }

    #[test]
    fn bootstrap_returns_empty_for_missing_dir() {
        let dir = scratch_dir("missing");
        std::fs::remove_dir(&dir).unwrap();
        let h = bootstrap_for(&dir, 100);
        assert!(h.is_empty());
    }

    #[test]
    fn install_pushes_entries_into_editor_history_oldest_first() {
        // Build a small history, install into a detached editor,
        // and verify the ring carries entries oldest-first by
        // walking Up through them. The editor's public API doesn't
        // expose its `history` vector, but `handle_input(Up)` is
        // observable: pressing Up once surfaces the most-recently-
        // inserted entry, the next Up the prior one, and so on —
        // exactly the contract a user relies on after startup.
        use aj_tui::component::Component;
        use aj_tui::components::editor::EditorTheme;
        use aj_tui::components::select_list::SelectListTheme;
        use aj_tui::keys::Key;
        use aj_tui::tui::RenderHandle;
        use std::sync::Arc;

        let identity_theme = EditorTheme {
            border_color: Arc::new(|s: &str| s.to_string()),
            select_list: SelectListTheme {
                selected_prefix: Arc::new(|s: &str| s.to_string()),
                selected_text: Arc::new(|s: &str| s.to_string()),
                description: Arc::new(|s: &str| s.to_string()),
                scroll_info: Arc::new(|s: &str| s.to_string()),
                no_match: Arc::new(|s: &str| s.to_string()),
                prefix: Arc::new(|s: &str| s.to_string()),
                shortcut: Arc::new(|s: &str| s.to_string()),
            },
        };

        let dir = scratch_dir("install");
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[
                &user_message_line("first", "1"),
                &user_message_line("second", "2"),
                &user_message_line("third", "3"),
            ],
        );
        let h = bootstrap_for(&dir, 100);

        let mut editor = Editor::new(RenderHandle::detached(), identity_theme);
        editor.set_focused(true);
        h.install(&mut editor);

        // Press Up — should pull the most-recently submitted
        // prompt ("third").
        editor.handle_input(&Key::up());
        assert_eq!(editor.get_text(), "third");

        // Up again — "second".
        editor.handle_input(&Key::up());
        assert_eq!(editor.get_text(), "second");

        // Up again — "first" (oldest).
        editor.handle_input(&Key::up());
        assert_eq!(editor.get_text(), "first");
    }
}
