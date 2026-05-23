//! Editor extensions on top of [`aj_tui::EditorComponent`].
//!
//! Plug-ins that turn the bare editor into the prompt surface:
//! slash-command completion, `@file` autocomplete (driven by
//! [`aj_tui::autocomplete`]), prompt-history wiring, and
//! multi-line submit handling.
//!
//! Today this module owns the [`PromptHistory`] type, which
//! bootstraps an in-memory prompt history from the project's JSONL
//! thread logs and installs it into a freshly-built editor. Live
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

/// In-memory prompt history extracted from on-disk thread logs.
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
/// `serde_json` escaping, and each thread file is owned by exactly
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

    /// Walk every `*.jsonl` file in the project's threads directory,
    /// extract user-text prompts in chronological order, and load
    /// them into a fresh [`PromptHistory`].
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
        let dir = persistence.threads_dir();

        if !dir.exists() {
            return history;
        }

        let read_dir = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::debug!("could not read threads dir {}: {e}", dir.display());
                return history;
            }
        };

        let mut files: Vec<_> = read_dir
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();

        // Filenames are timestamps; lex sort = chronological.
        // Oldest first so the most recently submitted prompts end
        // up most recent in the queue.
        files.sort();

        for path in &files {
            history.load_file(path);
        }

        history
    }

    fn load_file(&mut self, path: &Path) {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                tracing::debug!("skipping unreadable thread file {}: {e}", path.display());
                return;
            }
        };

        for (lineno, line) in BufReader::new(file).lines().enumerate() {
            let line = match line {
                Ok(s) => s,
                Err(_) => {
                    // Invalid UTF-8 on this line: skip it, keep
                    // going. This is the failure-isolation
                    // property a flat-file history format lacks.
                    continue;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let entry: ConversationEntry = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(e) => {
                    tracing::debug!(
                        "skipping unparseable line {} in {}: {e}",
                        lineno + 1,
                        path.display()
                    );
                    continue;
                }
            };
            if !matches!(entry.thread, ThreadKind::User) {
                continue;
            }
            if let ConversationEntryKind::Message { message: msg } = entry.entry
                && let Some(text) = extract_user_prompt_text(&msg)
            {
                self.push_internal(text);
            }
        }
    }

    fn push_internal(&mut self, text: String) {
        let trimmed = text.trim_end_matches(|c: char| c.is_whitespace() && c != '\n');
        let trimmed = trimmed.trim_start_matches(|c: char| c == ' ' || c == '\t');
        if trimmed.is_empty() {
            return;
        }
        // Don't materially re-allocate if no trim happened.
        let entry = if trimmed.len() == text.len() {
            text
        } else {
            trimmed.to_string()
        };
        if self.entries.back().is_some_and(|s| s == &entry) {
            return;
        }
        self.entries.push_back(entry);
        while self.entries.len() > self.max {
            self.entries.pop_front();
        }
    }

    /// Push every entry into `editor`, oldest first. Pressing Up
    /// once after this returns surfaces the most recently submitted
    /// prompt; the editor's own [`Editor::HISTORY_LIMIT`] cap and
    /// consecutive-duplicate dedup apply naturally as entries land.
    pub fn install(&self, editor: &mut Editor) {
        for entry in &self.entries {
            editor.add_to_history(entry);
        }
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
fn extract_user_prompt_text(msg: &AgentMessage) -> Option<String> {
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
