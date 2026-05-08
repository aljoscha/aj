//! In-memory prompt history derived from on-disk thread logs.
//!
//! Replaces the pre-existing `~/.aj/history.txt` (a `rustyline::FileHistory`
//! V2 file). That format had two problems:
//!
//! 1. **Brittle to non-UTF-8 content.** `FileHistory::load_from` reads with
//!    `BufReader::lines()`, which errors out on the first invalid-UTF-8
//!    line. Combined with our load-then-rewrite-whole-file pattern, a
//!    single bad paste truncated every entry past the corruption point on
//!    the next submit.
//! 2. **Concurrent-process clobber.** Two `aj` processes running side by
//!    side each read the file, added their own new entry, and rewrote the
//!    whole file. Last writer wins; the other terminal's entries are
//!    silently lost.
//!
//! The conversation log we already maintain is JSONL: every line is
//! independently parseable, arbitrary bytes round-trip via `serde_json`
//! escaping, and each thread file is owned by exactly one running process.
//! It is therefore the natural source of truth for "prompts the user has
//! ever submitted in this project". We bootstrap an in-memory history
//! from those files at startup and append to it on submit. There is no
//! separate disk format to corrupt.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use aj_models::messages::{ContentBlockParam, MessageParam, Role};
use aj_session::{ConversationEntry, ConversationEntryKind, ConversationPersistence, ThreadKind};
use rustyline::Editor;
use rustyline::history::MemHistory;

/// Default cap on the number of prompts retained.
pub const DEFAULT_MAX_ENTRIES: usize = 200;

/// In-memory prompt history.
///
/// Newest entry is at the back of the queue. `install` pushes entries
/// into a `rustyline` editor in oldest-first order so that pressing Up
/// once surfaces the most recent prompt.
pub struct PromptHistory {
    entries: VecDeque<String>,
    max: usize,
}

impl PromptHistory {
    pub fn new(max: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max: max.max(1),
        }
    }

    /// Walk every `*.jsonl` file in the project's threads directory,
    /// extract user-text prompts in chronological order, and load them
    /// into a fresh [`PromptHistory`].
    ///
    /// Robustness contract:
    ///
    /// - A read error on a single file (permission, missing, IO) is
    ///   logged and that file is skipped; other files still load.
    /// - A line that is not valid UTF-8 is skipped without aborting the
    ///   rest of the file.
    /// - A line that is valid UTF-8 but does not parse as a
    ///   [`ConversationEntry`] is skipped without aborting the rest of
    ///   the file.
    /// - Subagent threads and meta entries are ignored — only top-level
    ///   user messages count as "prompts the human typed".
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

        // Filenames are timestamps; lex sort = chronological. Oldest
        // first so the most recently submitted prompts end up most
        // recent in the queue.
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
                    // Invalid UTF-8 on this line: skip it, keep going.
                    // This is the failure-isolation property the old
                    // history file format lacked.
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
            if let ConversationEntryKind::Message(msg) = entry.entry {
                if let Some(text) = extract_user_prompt_text(&msg) {
                    self.push_internal(text);
                }
            }
        }
    }

    /// Record a freshly submitted prompt.
    pub fn record(&mut self, prompt: &str) {
        self.push_internal(prompt.to_string());
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
        if self.entries.back().map(|s| s == &entry).unwrap_or(false) {
            return;
        }
        self.entries.push_back(entry);
        while self.entries.len() > self.max {
            self.entries.pop_front();
        }
    }

    /// Push every entry into a rustyline editor, oldest first.
    pub fn install(&self, rl: &mut Editor<(), MemHistory>) {
        for entry in &self.entries {
            let _ = rl.add_history_entry(entry.as_str());
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|s| s.as_str())
    }
}

/// Pull text-block content out of a user message, joining multiple text
/// blocks with a newline. Returns `None` if there is no text content
/// (e.g. a tool-result-only user message).
fn extract_user_prompt_text(msg: &MessageParam) -> Option<String> {
    if !matches!(msg.role, Role::User) {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for block in &msg.content {
        if let ContentBlockParam::TextBlock { text, .. } = block {
            parts.push(text.as_str());
        }
    }
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

    /// Allocate a unique scratch directory under the system temp dir.
    /// Manual cleanup only — tests are expected to assert behavior and
    /// not pile up a meaningful amount of data.
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
        // Build an entry by hand to keep the test tied to the on-disk
        // shape rather than the in-memory builder.
        let payload = serde_json::json!({
            "id": id,
            "thread": "user",
            "type": "message",
            "role": "user",
            "content": [{"type": "text", "text": text}],
        });
        serde_json::to_string(&payload).unwrap()
    }

    fn assistant_message_line(text: &str, id: &str) -> String {
        let payload = serde_json::json!({
            "id": id,
            "thread": "user",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
        });
        serde_json::to_string(&payload).unwrap()
    }

    fn subagent_user_message_line(text: &str, id: &str) -> String {
        let payload = serde_json::json!({
            "id": id,
            "thread": "subagent",
            "agent_id": 1,
            "type": "message",
            "role": "user",
            "content": [{"type": "text", "text": text}],
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
        // A user message whose content is only a tool_result block —
        // not a typed prompt.
        let tool_result_only = serde_json::to_string(&serde_json::json!({
            "id": "5",
            "thread": "user",
            "type": "message",
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_1",
                "content": "ok",
                "is_error": false,
            }],
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
    fn record_skips_empty_and_consecutive_dupes() {
        let mut h = PromptHistory::new(100);
        h.record("");
        h.record("   ");
        h.record("alpha");
        h.record("alpha");
        h.record("beta");
        let entries: Vec<&str> = h.iter().collect();
        assert_eq!(entries, vec!["alpha", "beta"]);
    }

    #[test]
    fn install_pushes_into_rustyline_editor_oldest_first() {
        let mut h = PromptHistory::new(100);
        h.record("first");
        h.record("second");
        h.record("third");

        let config = rustyline::config::Config::default();
        let mut rl: Editor<(), MemHistory> =
            Editor::with_history(config, MemHistory::new()).unwrap();
        h.install(&mut rl);

        // rustyline stores entries in insertion order in its underlying
        // history; Up-arrow walks newest-first.
        use rustyline::history::{History, SearchDirection};
        assert_eq!(rl.history().len(), 3);
        let last = rl
            .history()
            .get(2, SearchDirection::Forward)
            .unwrap()
            .unwrap();
        assert_eq!(last.entry, "third");
    }

    #[test]
    fn bootstrap_returns_empty_for_missing_dir() {
        let dir = scratch_dir("missing");
        std::fs::remove_dir(&dir).unwrap();
        let h = bootstrap_for(&dir, 100);
        assert!(h.is_empty());
    }

    #[test]
    fn arc_mutex_record_is_visible_through_shared_handles() {
        // Lock down the contract used by `AjCli::shallow_clone`: a
        // record made via one `Arc<Mutex<PromptHistory>>` handle is
        // visible to all other handles that share the same Arc.
        use std::sync::{Arc, Mutex};

        let shared = Arc::new(Mutex::new(PromptHistory::new(100)));
        let other = Arc::clone(&shared);

        shared.lock().unwrap().record("from handle a");
        other.lock().unwrap().record("from handle b");

        let entries: Vec<String> = shared
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(entries, vec!["from handle a", "from handle b"]);
    }
}
