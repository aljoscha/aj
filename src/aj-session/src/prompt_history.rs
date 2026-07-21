//! Prompt-history extraction from persisted session logs.
//!
//! The session log is the authoritative record of every prompt a user
//! has ever submitted in a project, so prompt-history search reads it
//! rather than a separate history file. A `*.jsonl` log is
//! line-independent: arbitrary bytes round-trip via `serde_json`
//! escaping and each session file is owned by exactly one running
//! process, so a corrupt or non-UTF-8 line is skipped without aborting
//! the scan.
//!
//! Two collectors sit on the same per-file scanner:
//!
//! - [`workspace_history`] over the current project's sessions
//!   directory.
//! - [`all_workspaces_history`] over every project under
//!   `~/.aj/sessions`, tagging each prompt with its project.
//!
//! Both are newest-first and deduplicated, capped at `max` entries.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use aj_agent::message::{AgentMessage, AgentMessageKind};
use aj_models::types::{Message, UserContent};

use crate::log::{ConversationEntry, ConversationEntryKind, ThreadKind};
use crate::persistence::ConversationPersistence;

/// One recallable prompt plus the project it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptEntry {
    /// The full prompt text, recalled verbatim into the editor.
    pub text: String,
    /// Project label (the `~/.aj/sessions` subdirectory name). `None`
    /// for the current-workspace scan, where the project is implicit.
    pub project: Option<String>,
}

/// Collect the current workspace's submitted prompts, newest-first and
/// deduplicated, capped at `max`.
pub fn workspace_history(
    persistence: &ConversationPersistence,
    max: usize,
    cancel: &dyn Fn() -> bool,
) -> Vec<PromptEntry> {
    let mut out = Vec::new();
    workspace_history_streaming(persistence, max, cancel, &mut |batch| out.extend(batch));
    out
}

/// Stream the current workspace's submitted prompts to `emit`, one
/// per-file batch at a time, in the same newest-first, deduplicated,
/// `max`-capped order as [`workspace_history`].
///
/// A batch carries a single session file's new (not-yet-seen) prompts so
/// a UI rendering the list incrementally can append rows as the scan
/// walks the on-disk logs rather than blocking on the whole walk. Empty
/// batches (a file with no new prompts) are not emitted.
pub fn workspace_history_streaming(
    persistence: &ConversationPersistence,
    max: usize,
    cancel: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Vec<PromptEntry>),
) {
    let mut seen = HashSet::new();
    let mut remaining = max;
    collect_dir(
        persistence.sessions_dir(),
        None,
        &mut seen,
        &mut remaining,
        cancel,
        emit,
    );
}

/// Collect submitted prompts across every project under `sessions_base`
/// (`~/.aj/sessions`), deduplicated and each tagged with its project
/// (subdirectory) label, newest-first and capped at `max`.
///
/// Projects are visited in reverse-lexicographic directory order and
/// files within a project newest-first, so a prompt's tag reflects the
/// first project (in that order) whose files contain it. The directory
/// order is unrelated to recency. It exists only to make the dedup
/// deterministic, so the tag on a prompt shared across projects is
/// stable but not a "most recent workspace" guarantee.
pub fn all_workspaces_history(sessions_base: &Path, max: usize) -> Vec<PromptEntry> {
    let mut out = Vec::new();
    all_workspaces_history_streaming(sessions_base, max, &|| false, &mut |batch| {
        out.extend(batch)
    });
    out
}

/// Stream submitted prompts across every project under `sessions_base` to
/// `emit`, one per-file batch at a time, in the same order, dedup, and
/// `max`-cap as [`all_workspaces_history`]. See
/// [`workspace_history_streaming`] for the batching contract.
pub fn all_workspaces_history_streaming(
    sessions_base: &Path,
    max: usize,
    cancel: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Vec<PromptEntry>),
) {
    let read_dir = match std::fs::read_dir(sessions_base) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::debug!(
                "could not read sessions base {}: {e}",
                sessions_base.display()
            );
            return;
        }
    };

    let mut projects: Vec<_> = read_dir
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| p.is_dir())
        .collect();
    // Directory names are unrelated to recency, but a stable order keeps
    // the dedup deterministic. Reverse lexicographic so the listing
    // roughly mirrors the newest-first feel within a project.
    projects.sort();
    projects.reverse();

    let mut seen = HashSet::new();
    let mut remaining = max;
    for dir in &projects {
        if remaining == 0 || cancel() {
            break;
        }
        let project = dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        collect_dir(dir, project, &mut seen, &mut remaining, cancel, emit);
    }
}

/// Walk every `*.jsonl` file in `dir`, newest file first, emitting each
/// file's new prompts (newest-first, skipping bodies already in `seen`)
/// as one batch through `emit`. `project` tags every entry. `remaining`
/// is the shared budget, decremented as entries are produced. The walk
/// stops once it hits zero. A file that yields no new prompts emits
/// nothing.
fn collect_dir(
    dir: &Path,
    project: Option<String>,
    seen: &mut HashSet<String>,
    remaining: &mut usize,
    cancel: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Vec<PromptEntry>),
) {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::debug!("could not read sessions dir {}: {e}", dir.display());
            return;
        }
    };

    let mut files: Vec<_> = read_dir
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    // Filenames are timestamps; reverse-lexicographic = newest-first.
    files.sort();
    files.reverse();

    for path in &files {
        if *remaining == 0 || cancel() {
            return;
        }
        // Within a file prompts are chronological; reverse so the most
        // recent prompt in this file lands first.
        let mut prompts = load_file_prompts(path, cancel);
        // A mid-file cancel returns a partial (still chronological) read.
        // Drop it rather than reverse-and-emit an out-of-order partial
        // batch. A sticky `cancel` is true here after an in-file break.
        if cancel() {
            return;
        }
        prompts.reverse();
        let mut batch = Vec::new();
        for text in prompts {
            if *remaining == 0 {
                break;
            }
            if seen.insert(text.clone()) {
                batch.push(PromptEntry {
                    text,
                    project: project.clone(),
                });
                *remaining -= 1;
            }
        }
        if !batch.is_empty() {
            emit(batch);
        }
    }
}

/// Extract the user-submitted prompt texts from a single session file,
/// in chronological (file) order, each fully trimmed with blanks
/// dropped.
fn load_file_prompts(path: &Path, cancel: &dyn Fn() -> bool) -> Vec<String> {
    scan_file_user_prompts_cancellable(path, cancel)
        .into_iter()
        .filter_map(|text| {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .collect()
}

/// Read the user-typed prompt texts from one session file, in
/// chronological (file) order.
///
/// A session log is mostly assistant turns and tool results whose
/// bodies dwarf the occasional user prompt. To keep a scan of a large
/// project's logs cheap, each line is first parsed into a tiny
/// [`PromptHead`] capturing only the thread and message role. The
/// expensive full [`ConversationEntry`] parse (which allocates the
/// message-content tree) runs only for lines that really are top-level
/// user messages.
///
/// Failure-isolation: an unreadable file yields no prompts, and
/// non-UTF-8 or unparseable lines are skipped without aborting the rest
/// of the file.
pub fn scan_file_user_prompts(path: &Path) -> Vec<String> {
    scan_file_user_prompts_cancellable(path, &|| false)
}

/// [`scan_file_user_prompts`] with cooperative cancellation for the
/// blocking-pool scans. `cancel` is polled periodically while reading so a
/// large file doesn't pin the scan after the consumer has gone away.
fn scan_file_user_prompts_cancellable(path: &Path, cancel: &dyn Fn() -> bool) -> Vec<String> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!("skipping unreadable session file {}: {e}", path.display());
            return Vec::new();
        }
    };

    let mut prompts = Vec::new();
    for (lineno, line) in BufReader::new(file).lines().enumerate() {
        // Cooperative cancellation: this runs on the blocking pool, so we
        // poll `cancel` and stop reading once the consumer is gone. The
        // caller (`collect_dir`) re-checks `cancel` and drops the partial
        // read, so no out-of-order batch is emitted.
        if lineno % crate::SCAN_CANCEL_CHECK_LINES == 0 && cancel() {
            break;
        }
        // A non-UTF-8 (or IO-erroring) line is skipped, not fatal: the
        // failure-isolation property a flat-file format lacks.
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
        // Confirmed a top-level user message. The full parse is what
        // actually pulls the text content out. Task-completion notices
        // are stored with `role:"task_notification"`, so `is_user_prompt`
        // already excluded them above and they never reach here.
        if let Ok(entry) = serde_json::from_str::<ConversationEntry>(&line)
            && let ConversationEntryKind::Message { message: msg } = entry.entry
            && let Some(text) = extract_user_prompt_text(&msg)
        {
            prompts.push(text);
        }
    }
    prompts
}

/// Join a user message's text blocks with a newline. `None` when there
/// is no text content (a tool-result message or an assistant message).
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
    /// message role is `user`. Assistant / tool-result messages and
    /// non-message entries (system prompt, settings records) are
    /// excluded.
    fn is_user_prompt(&self) -> bool {
        matches!(self.thread, ThreadKind::User)
            && self.message.as_ref().and_then(|m| m.role.as_deref()) == Some("user")
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn scratch_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("aj-history-scan-{label}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn user_line(text: &str, id: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "id": id,
            "thread": "user",
            "type": "message",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": text}],
                "timestamp": 0,
            },
        }))
        .unwrap()
    }

    /// A persisted task-completion notice line: `role:"task_notification"`,
    /// the on-disk shape the drain writes.
    fn notification_line(body: &str, id: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "id": id,
            "thread": "user",
            "type": "message",
            "message": {
                "role": "task_notification",
                "label": "cargo build",
                "kind": "bash",
                "outcome": {"status": "succeeded"},
                "body": body,
            },
        }))
        .unwrap()
    }

    fn write_jsonl(dir: &Path, name: &str, lines: &[String]) {
        let path = dir.join(format!("{name}.jsonl"));
        let mut f = File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
    }

    #[test]
    fn workspace_history_is_newest_first_and_deduped() {
        let dir = scratch_dir("workspace");
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[user_line("first", "1"), user_line("second", "2")],
        );
        write_jsonl(
            &dir,
            "2024-02-01-00-00-00",
            // `second` repeats. The newer occurrence wins and the older
            // one is dropped.
            &[user_line("second", "1"), user_line("third", "2")],
        );

        let persistence = ConversationPersistence::new(dir);
        let entries = workspace_history(&persistence, 2000, &|| false);
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        // Newest file first, prompts within a file newest-first, then
        // older files; `second` deduped to its newest position.
        assert_eq!(texts, vec!["third", "second", "first"]);
        assert!(entries.iter().all(|e| e.project.is_none()));
    }

    #[test]
    fn workspace_history_streaming_emits_per_file_batches_deduped() {
        let dir = scratch_dir("workspace-stream");
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[user_line("first", "1"), user_line("second", "2")],
        );
        write_jsonl(
            &dir,
            "2024-02-01-00-00-00",
            // `second` repeats. The newer file's batch drops it as seen.
            &[user_line("second", "1"), user_line("third", "2")],
        );

        let persistence = ConversationPersistence::new(dir);
        let mut batches: Vec<Vec<String>> = Vec::new();
        workspace_history_streaming(&persistence, 2000, &|| false, &mut |batch| {
            batches.push(batch.into_iter().map(|e| e.text).collect());
        });
        // One batch per file (newest file first); within a batch newest
        // prompt first; `second` deduped out of the older file's batch.
        assert_eq!(batches, vec![vec!["third", "second"], vec!["first"]]);
    }

    #[test]
    fn scan_file_user_prompts_cancellable_stops_mid_file() {
        // A single file larger than the in-file poll interval: the check
        // inside the read loop (not just the between-files check) must bail.
        let dir = scratch_dir("mid-file-cancel");
        let n = crate::SCAN_CANCEL_CHECK_LINES * 3;
        let lines: Vec<String> = (0..n)
            .map(|i| user_line(&format!("p{i}"), &i.to_string()))
            .collect();
        write_jsonl(&dir, "2024-01-01-00-00-00", &lines);
        let path = dir.join("2024-01-01-00-00-00.jsonl");

        // Sticky predicate: false at the line-0 poll, true from the
        // line-1024 poll onward.
        let calls = std::cell::Cell::new(0usize);
        let cancel = || {
            let c = calls.get();
            calls.set(c + 1);
            c > 0
        };

        let prompts = scan_file_user_prompts_cancellable(&path, &cancel);
        // Bails at the line-1024 poll, having read lines 0..1023. Without the
        // in-file check it would read all 3072.
        assert_eq!(prompts.len(), crate::SCAN_CANCEL_CHECK_LINES);
    }

    #[test]
    fn workspace_history_streaming_stops_when_cancelled() {
        let dir = scratch_dir("workspace-cancel");
        write_jsonl(&dir, "2024-01-01-00-00-00", &[user_line("first", "1")]);
        write_jsonl(&dir, "2024-02-01-00-00-00", &[user_line("second", "1")]);

        // Trip the predicate after the first file's batch: the between-files
        // check must break before reading the older file.
        let persistence = ConversationPersistence::new(dir);
        let seen = std::cell::Cell::new(0usize);
        let mut batches: Vec<Vec<String>> = Vec::new();
        workspace_history_streaming(&persistence, 2000, &|| seen.get() > 0, &mut |batch| {
            seen.set(seen.get() + 1);
            batches.push(batch.into_iter().map(|e| e.text).collect());
        });
        assert_eq!(batches, vec![vec!["second"]]);
    }

    #[test]
    fn workspace_history_respects_the_cap() {
        let dir = scratch_dir("workspace-cap");
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[
                user_line("a", "1"),
                user_line("b", "2"),
                user_line("c", "3"),
            ],
        );
        let persistence = ConversationPersistence::new(dir);
        let entries = workspace_history(&persistence, 2, &|| false);
        assert_eq!(entries.len(), 2, "cap honored: {entries:?}");
    }

    #[test]
    fn all_workspaces_history_tags_and_dedupes_across_projects() {
        let base = scratch_dir("all-base");
        let proj_a = base.join("proj-a");
        let proj_b = base.join("proj-b");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();
        write_jsonl(
            &proj_a,
            "2024-01-01-00-00-00",
            &[user_line("shared prompt", "1"), user_line("only in a", "2")],
        );
        write_jsonl(
            &proj_b,
            "2024-01-01-00-00-00",
            &[user_line("shared prompt", "1"), user_line("only in b", "2")],
        );

        let entries = all_workspaces_history(&base, 2000);
        let by_text: std::collections::HashMap<&str, Option<&str>> = entries
            .iter()
            .map(|e| (e.text.as_str(), e.project.as_deref()))
            .collect();
        assert_eq!(by_text.get("only in a"), Some(&Some("proj-a")));
        assert_eq!(by_text.get("only in b"), Some(&Some("proj-b")));
        // `shared prompt` appears once (deduped across projects).
        let shared_count = entries.iter().filter(|e| e.text == "shared prompt").count();
        assert_eq!(shared_count, 1);
    }

    #[test]
    fn all_workspaces_history_missing_base_is_empty() {
        let base = scratch_dir("missing-base");
        std::fs::remove_dir_all(&base).unwrap();
        assert!(all_workspaces_history(&base, 2000).is_empty());
    }

    #[test]
    fn task_notifications_are_excluded() {
        let dir = scratch_dir("notices");
        write_jsonl(
            &dir,
            "2024-01-01-00-00-00",
            &[
                user_line("real prompt", "1"),
                notification_line("background task done", "2"),
            ],
        );
        let persistence = ConversationPersistence::new(dir);
        let entries = workspace_history(&persistence, 2000, &|| false);
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["real prompt"], "harness notice excluded");
    }
}
