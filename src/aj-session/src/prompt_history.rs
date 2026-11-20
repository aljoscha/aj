//! Prompt-history extraction from stored sessions.
//!
//! A session's entries are the authoritative account of every prompt a
//! user submitted in a project, so prompt-history search reads them and
//! keeps no history of its own. A stored session's prompts are one query
//! over its user messages, read at one snapshot. A session file that was
//! never imported is scanned by [`crate::legacy`]. A session that cannot
//! be read yields no prompts and does not end the scan.
//!
//! [`recent_history_streaming`] ranks prompts by their recorded timestamps for
//! history search. The session-ordered [`workspace_history`] collector serves the
//! editor ring's bounded bootstrap. Both share the same prompt extraction rules.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use aj_agent::message::{AgentMessage, AgentMessageKind};
use aj_models::types::{Message, UserContent};

use crate::log::{LogEntry, LogEntryKind, SessionError, ThreadKind};
use crate::persistence::ProjectSessions;

/// One recallable prompt plus the project it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptEntry {
    /// The full prompt text, recalled verbatim into the editor.
    pub text: String,
    /// Project label (the `~/.aj/sessions` subdirectory name). `None`
    /// for the current-workspace scan, where the project is implicit.
    pub project: Option<String>,
}

/// A submitted prompt with its persisted time.
#[derive(Debug, Clone)]
pub struct RecordedPrompt {
    pub entry: PromptEntry,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Retain the newest occurrence of each trimmed text, newest first, capped at
/// `max`. Used by both scans and merges of independently read hosts.
///
/// The sort is stable, so prompts with equal timestamps keep the order they
/// were given in. Callers hand over each session's prompts newest first,
/// which is the only recency a session without per-entry timestamps records.
pub fn retain_recent(prompts: &mut Vec<RecordedPrompt>, max: usize) {
    for prompt in prompts.iter_mut() {
        prompt.entry.text = prompt.entry.text.trim().to_string();
    }
    prompts.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    let mut seen = HashSet::new();
    prompts.retain(|p| !p.entry.text.is_empty() && seen.insert(p.entry.text.clone()));
    prompts.truncate(max);
}

/// How often [`recent_history_streaming`] emits a provisional snapshot.
///
/// Each one is the whole history so far, up to `max` prompts, and a store
/// holds thousands of small logs, so one per file would cost the consumer far
/// more than its readers can see. Readers already paint at about this rate.
const EMIT_INTERVAL: Duration = Duration::from_millis(100);

/// Scan every session, emitting provisional newest-first snapshots capped at `max`.
/// A session's identity or traversal position cannot exclude a newer prompt in another
/// session. Cancellation is cooperative, including within large logs.
///
/// The first session's snapshot is emitted at once and later ones at most every
/// [`EMIT_INTERVAL`]. A scan that is not cancelled ends on the snapshot of
/// everything it read, also when it fails partway. A cancelled one emits
/// nothing further.
pub fn recent_history_streaming(
    persistence: &ProjectSessions,
    all: bool,
    max: usize,
    cancel: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Vec<RecordedPrompt>),
) -> std::io::Result<()> {
    if max == 0 || cancel() {
        return Ok(());
    }
    let mut recent = Vec::new();
    let mut paced = Paced {
        emit,
        last: None,
        pending: false,
    };
    let scanned = scan_recent(persistence, all, max, cancel, &mut recent, &mut paced);
    if !cancel() {
        paced.flush(&recent);
    }
    scanned
}

/// An emitter that holds back snapshots offered within [`EMIT_INTERVAL`] of
/// the last one it passed on.
struct Paced<'a> {
    emit: &'a mut dyn FnMut(Vec<RecordedPrompt>),
    last: Option<Instant>,
    /// Whether a snapshot was held back since the last emit.
    pending: bool,
}

impl Paced<'_> {
    fn offer(&mut self, recent: &[RecordedPrompt]) {
        if self.last.is_some_and(|last| last.elapsed() < EMIT_INTERVAL) {
            self.pending = true;
            return;
        }
        self.send(recent);
    }

    fn flush(&mut self, recent: &[RecordedPrompt]) {
        if self.pending {
            self.send(recent);
        }
    }

    fn send(&mut self, recent: &[RecordedPrompt]) {
        self.pending = false;
        self.last = Some(Instant::now());
        (self.emit)(recent.to_vec());
    }
}

fn scan_recent(
    persistence: &ProjectSessions,
    all: bool,
    max: usize,
    cancel: &dyn Fn() -> bool,
    recent: &mut Vec<RecordedPrompt>,
    paced: &mut Paced<'_>,
) -> std::io::Result<()> {
    let mut projects = if all {
        persistence.projects().map_err(std::io::Error::other)?
    } else {
        vec![persistence.clone()]
    };
    projects.sort_by(|a, b| a.sessions_dir().cmp(b.sessions_dir()));
    for persistence in projects {
        if cancel() {
            return Ok(());
        }
        let project = all.then(|| {
            persistence
                .sessions_dir()
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        let mut sessions = persistence.list_sessions().map_err(std::io::Error::other)?;
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        for session in sessions {
            if cancel() {
                return Ok(());
            }
            let mut prompts = scan_session_prompts(&persistence, &session.session_id, cancel);
            if cancel() {
                return Ok(());
            }
            for prompt in &mut prompts {
                prompt.entry.project = project.clone();
            }
            recent.extend(prompts.into_iter().rev());
            retain_recent(recent, max);
            paced.offer(recent);
        }
    }
    Ok(())
}

/// Collect the current workspace's submitted prompts, newest-first and
/// deduplicated, capped at `max`.
pub fn workspace_history(
    persistence: &ProjectSessions,
    max: usize,
    cancel: &dyn Fn() -> bool,
) -> Vec<PromptEntry> {
    let mut out = Vec::new();
    workspace_history_streaming(persistence, max, cancel, &mut |batch| out.extend(batch));
    out
}

/// Stream the current workspace's submitted prompts to `emit`, one
/// per-session batch at a time, in the same newest-first, deduplicated,
/// `max`-capped order as [`workspace_history`].
///
/// A batch carries a single session's new (not-yet-seen) prompts so
/// a UI rendering the list incrementally can append rows as the scan
/// walks the on-disk logs rather than blocking on the whole walk. Empty
/// batches (a session with no new prompts) are not emitted.
pub fn workspace_history_streaming(
    persistence: &ProjectSessions,
    max: usize,
    cancel: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Vec<PromptEntry>),
) {
    let mut seen = HashSet::new();
    let mut remaining = max;
    collect_dir(persistence, &mut seen, &mut remaining, cancel, emit);
}

/// Walk sessions newest identity first, emitting each session's unseen prompts
/// newest first. SQLite owns an identity once imported, even if its source file
/// remains in the directory.
fn collect_dir(
    persistence: &ProjectSessions,
    seen: &mut HashSet<String>,
    remaining: &mut usize,
    cancel: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(Vec<PromptEntry>),
) {
    if *remaining == 0 || cancel() {
        return;
    }
    let sessions = match persistence.list_sessions() {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::debug!(
                "could not list sessions in {}: {e}",
                persistence.sessions_dir().display()
            );
            return;
        }
    };

    for session in sessions {
        if *remaining == 0 || cancel() {
            return;
        }
        let prompts = scan_session_prompts(persistence, &session.session_id, cancel);
        // A cancelled read is incomplete. Reversing it would expose older
        // prompts before the unread newer ones.
        if cancel() {
            return;
        }
        let prompts = prompts.into_iter().rev().filter_map(|prompt| {
            let text = prompt.entry.text.trim();
            (!text.is_empty()).then(|| text.to_string())
        });
        let mut batch = Vec::new();
        for text in prompts {
            if *remaining == 0 {
                break;
            }
            if seen.insert(text.clone()) {
                batch.push(PromptEntry {
                    text,
                    project: None,
                });
                *remaining -= 1;
            }
        }
        if !batch.is_empty() {
            emit(batch);
        }
    }
}

fn scan_session_prompts(
    persistence: &ProjectSessions,
    session_id: &str,
    cancel: &dyn Fn() -> bool,
) -> Vec<RecordedPrompt> {
    match read_session_prompts(persistence, session_id, cancel) {
        Ok(prompts) => prompts,
        Err(error) => {
            // A failed authoritative read must never resurrect the JSONL source.
            tracing::debug!("skipping unreadable session {session_id}: {error}");
            Vec::new()
        }
    }
}

fn read_session_prompts(
    persistence: &ProjectSessions,
    session_id: &str,
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<RecordedPrompt>, SessionError> {
    if let Some(store) = persistence.sqlite_reader()?
        && let Some(metadata) = store.session_metadata(session_id)?
    {
        let fallback = crate::persistence::parse_session_id_created_at(session_id)
            .unwrap_or(metadata.updated_at);
        let mut prompts = Vec::new();
        // One statement reads one snapshot, so concurrent appends cannot
        // keep the scan alive.
        store.scan(session_id, "user", true, |stored| {
            if let Some(prompt) = recorded_prompt(&stored, fallback) {
                prompts.push(prompt);
            }
            !cancel()
        })?;
        return Ok(prompts);
    }
    Ok(crate::legacy::prompts(persistence, session_id, cancel))
}

pub(crate) fn recorded_prompt(
    entry: &LogEntry,
    fallback: chrono::DateTime<chrono::Utc>,
) -> Option<RecordedPrompt> {
    if !matches!(entry.thread, ThreadKind::Main) {
        return None;
    }
    let LogEntryKind::Message { message } = &entry.entry else {
        return None;
    };
    let text = extract_user_prompt_text(message)?;
    let message_time = match &message.kind {
        AgentMessageKind::Wire(Message::User(user)) if user.timestamp > 0 => {
            chrono::DateTime::from_timestamp_millis(user.timestamp)
        }
        _ => None,
    };
    Some(RecordedPrompt {
        entry: PromptEntry {
            text,
            project: None,
        },
        timestamp: entry.timestamp.or(message_time).unwrap_or(fallback),
    })
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

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    /// A scratch directory for one test, removed when the returned guard
    /// drops. The label rides in the name so a leftover directory from a
    /// crashed run still says which test made it.
    fn scratch_dir(label: &str) -> TempDir {
        TempDir::with_prefix(format!("aj-history-scan-{label}-")).expect("create temp dir")
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

    /// A persisted task notification line: `role:"task_notification"`,
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

    fn write_sqlite(dir: &Path, name: &str, lines: &[String]) {
        write_sqlite_to(&ProjectSessions::new(dir.to_path_buf()), name, lines);
    }

    fn write_sqlite_to(persistence: &ProjectSessions, name: &str, lines: &[String]) {
        let mut entries: Vec<LogEntry> = lines
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for index in 1..entries.len() {
            entries[index].parent_id = Some(entries[index - 1].id.clone());
        }
        let mut store = persistence.sqlite_writer().unwrap();
        let (now, none) = (chrono::Utc::now(), Default::default());
        crate::jsonl::import_entries(&mut store, name, &entries, now, &none).unwrap();
    }

    #[test]
    fn mixed_history_uses_sqlite_authority_and_preserves_both_orders() {
        let root = scratch_dir("mixed");
        let project = root.path().join("project");
        let other = root.path().join("other");
        let database = root.path().join("sessions.sqlite3");
        let persistence =
            ProjectSessions::for_project(database.clone(), project.clone(), "project".into());
        let other_store = ProjectSessions::for_project(database, other, "other".into());
        std::fs::create_dir(&project).unwrap();
        let older = "2024-01-01-00-00-00-000";
        let newer = "2024-02-01-00-00-00-000";
        let mut latest: serde_json::Value =
            serde_json::from_str(&user_line("latest recorded", "1")).unwrap();
        latest["timestamp"] = serde_json::json!("2025-01-01T00:00:00Z");
        write_jsonl(
            &project,
            older,
            &[latest.to_string(), user_line("shared", "2")],
        );
        let mut subagent: serde_json::Value =
            serde_json::from_str(&user_line("not submitted by user", "3")).unwrap();
        subagent["thread"] = serde_json::json!("subagent");
        subagent["agent_id"] = serde_json::json!(1);
        write_sqlite_to(
            &persistence,
            newer,
            &[
                user_line(" shared ", "1"),
                user_line("sqlite", "2"),
                subagent.to_string(),
                notification_line("not a prompt", "4"),
                user_line("  ", "5"),
            ],
        );
        // An imported identity can retain a source that differs from SQLite.
        write_jsonl(&project, newer, &[user_line("stale source", "1")]);
        write_sqlite_to(&other_store, newer, &[user_line("other project", "1")]);
        std::fs::write(
            project.join("sessions.sqlite3"),
            b"ignored intermediate store",
        )
        .unwrap();
        assert_eq!(persistence.list_sessions().unwrap().len(), 2);
        assert!(persistence.in_database(newer).unwrap());
        assert!(project.join(format!("{newer}.jsonl")).exists());

        let workspace = workspace_history(&persistence, 10, &|| false);
        assert_eq!(
            workspace
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            ["sqlite", "shared", "latest recorded"]
        );
        assert!(workspace.iter().all(|p| p.project.is_none()));
        assert_eq!(
            workspace_history(&persistence, 1, &|| false),
            workspace[..1]
        );

        let mut recent = Vec::new();
        recent_history_streaming(&persistence, true, 10, &|| false, &mut |p| recent = p).unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|p| (p.entry.text.as_str(), p.entry.project.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("latest recorded", Some("project")),
                ("other project", Some("other")),
                ("sqlite", Some("project")),
                ("shared", Some("project"))
            ]
        );
        assert_eq!(
            recent[2].timestamp,
            crate::persistence::parse_session_id_created_at(newer).unwrap()
        );
    }

    #[test]
    fn sqlite_history_reads_a_long_session_and_cancels_without_emitting_partial_batches() {
        let dir = scratch_dir("sqlite-pages");
        let count = 769;
        let lines: Vec<_> = (0..count)
            .map(|i| user_line(&format!("prompt {i}"), &i.to_string()))
            .collect();
        write_sqlite(dir.path(), "session", &lines);
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let history = workspace_history(&persistence, count, &|| false);
        assert_eq!(history.len(), count);
        assert_eq!(
            history.first().unwrap().text,
            format!("prompt {}", count - 1)
        );
        assert_eq!(history.last().unwrap().text, "prompt 0");

        let polls = std::cell::Cell::new(0);
        let cancel = || {
            polls.set(polls.get() + 1);
            polls.get() >= 4
        };
        let partial = workspace_history(&persistence, count, &cancel);
        assert!(polls.get() >= 4, "must poll within a session");
        assert!(
            partial.is_empty(),
            "cancelled session must not emit older prompts"
        );
    }

    #[test]
    fn recent_history_preserves_extraction_and_deterministic_legacy_fallback() {
        let dir = scratch_dir("recorded");
        let stem = "2024-01-01-00-00-00-000";
        let mut blocks: serde_json::Value =
            serde_json::from_str(&user_line(" first ", "1")).unwrap();
        blocks["message"]["content"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type": "text", "text": "second "}));
        let mut sub = blocks.clone();
        sub["thread"] = serde_json::json!("subagent");
        serde_json::from_value::<LogEntry>(sub.clone()).expect("valid subagent fixture");
        write_jsonl(
            dir.path(),
            stem,
            &[
                blocks.to_string(),
                user_line(" ", "2"),
                sub.to_string(),
                notification_line("not a prompt", "3"),
                "broken JSON".to_string(),
                user_line("last", "4"),
            ],
        );
        let path = dir.path().join(format!("{stem}.jsonl"));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\xff\n")
            .unwrap();
        let mut first = Vec::new();
        recent_history_streaming(
            &ProjectSessions::new(dir.path().to_path_buf()),
            false,
            2000,
            &|| false,
            &mut |p| first = p,
        )
        .unwrap();
        assert_eq!(
            first
                .iter()
                .map(|p| p.entry.text.as_str())
                .collect::<Vec<_>>(),
            ["last", "first \nsecond"]
        );
        let expected = crate::persistence::parse_session_id_created_at(stem).unwrap();
        assert!(first.iter().all(|p| p.timestamp == expected));
        let mut second = Vec::new();
        recent_history_streaming(
            &ProjectSessions::new(dir.path().to_path_buf()),
            false,
            2000,
            &|| false,
            &mut |p| second = p,
        )
        .unwrap();
        assert_eq!(
            first.iter().map(|p| &p.entry.text).collect::<Vec<_>>(),
            second.iter().map(|p| &p.entry.text).collect::<Vec<_>>(),
            "equal fallback timestamps keep newest-line-first order across scans"
        );
        assert_eq!(
            crate::legacy::scan_file_user_prompts_cancellable(&path, &|| false),
            [" first \nsecond ", " ", "last"]
        );
    }

    #[test]
    fn recent_history_emits_before_completion_and_cancels_between_files() {
        let dir = scratch_dir("ranked-cancel");
        write_jsonl(dir.path(), "a", &[user_line("first", "1")]);
        write_jsonl(dir.path(), "z", &[user_line("second", "2")]);
        let cancelled = std::cell::Cell::new(false);
        let mut batches = Vec::new();
        recent_history_streaming(
            &ProjectSessions::new(dir.path().to_path_buf()),
            false,
            2000,
            &|| cancelled.get(),
            &mut |p| {
                batches.push(p);
                cancelled.set(true);
            },
        )
        .unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0][0].entry.text, "first");
    }

    /// A store of many logs costs its consumer a handful of snapshots rather
    /// than one per file: the first at once, the last complete.
    #[test]
    fn recent_history_paces_snapshots_and_ends_complete() {
        const FILES: usize = 64;
        let dir = scratch_dir("ranked-paced");
        for index in 0..FILES {
            let text = format!("prompt {index}");
            write_jsonl(dir.path(), &format!("{index:03}"), &[user_line(&text, "1")]);
        }
        let mut batches: Vec<Vec<RecordedPrompt>> = Vec::new();
        recent_history_streaming(
            &ProjectSessions::new(dir.path().to_path_buf()),
            false,
            2000,
            &|| false,
            &mut |p| batches.push(p),
        )
        .unwrap();
        // Scanning this store takes a few milliseconds. The bound leaves room
        // for a machine a hundred times slower.
        assert!(
            batches.len() < FILES / 4,
            "{} snapshots for {FILES} files: the scan emits per file",
            batches.len(),
        );
        assert_eq!(
            batches.first().map(Vec::len),
            Some(1),
            "the first file's snapshot is not held back",
        );
        assert_eq!(
            batches.last().map(Vec::len),
            Some(FILES),
            "the final snapshot holds every file's prompt",
        );
    }

    #[test]
    fn workspace_history_is_newest_first_and_deduped() {
        let dir = scratch_dir("workspace");
        write_jsonl(
            dir.path(),
            "2024-01-01-00-00-00",
            &[user_line("first", "1"), user_line("second", "2")],
        );
        write_jsonl(
            dir.path(),
            "2024-02-01-00-00-00",
            // `second` repeats. The newer occurrence wins and the older
            // one is dropped.
            &[user_line("second", "1"), user_line("third", "2")],
        );

        let persistence = ProjectSessions::new(dir.path().to_path_buf());
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
            dir.path(),
            "2024-01-01-00-00-00",
            &[user_line("first", "1"), user_line("second", "2")],
        );
        write_jsonl(
            dir.path(),
            "2024-02-01-00-00-00",
            // `second` repeats. The newer file's batch drops it as seen.
            &[user_line("second", "1"), user_line("third", "2")],
        );

        let persistence = ProjectSessions::new(dir.path().to_path_buf());
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
        write_jsonl(dir.path(), "2024-01-01-00-00-00", &lines);
        let path = dir.path().join("2024-01-01-00-00-00.jsonl");

        // Sticky predicate: false at the line-0 poll, true from the
        // line-1024 poll onward.
        let calls = std::cell::Cell::new(0usize);
        let cancel = || {
            let c = calls.get();
            calls.set(c + 1);
            c > 0
        };

        let prompts = crate::legacy::scan_file_user_prompts_cancellable(&path, &cancel);
        // Bails at the line-1024 poll, having read lines 0..1023. Without the
        // in-file check it would read all 3072.
        assert_eq!(prompts.len(), crate::SCAN_CANCEL_CHECK_LINES);
    }

    #[test]
    fn workspace_history_streaming_stops_when_cancelled() {
        let dir = scratch_dir("workspace-cancel");
        write_jsonl(
            dir.path(),
            "2024-01-01-00-00-00",
            &[user_line("first", "1")],
        );
        write_jsonl(
            dir.path(),
            "2024-02-01-00-00-00",
            &[user_line("second", "1")],
        );

        // Trip the predicate after the first file's batch: the between-files
        // check must break before reading the older file.
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
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
            dir.path(),
            "2024-01-01-00-00-00",
            &[
                user_line("a", "1"),
                user_line("b", "2"),
                user_line("c", "3"),
            ],
        );
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let entries = workspace_history(&persistence, 2, &|| false);
        assert_eq!(entries.len(), 2, "cap honored: {entries:?}");
    }

    #[test]
    fn task_notifications_are_excluded() {
        let dir = scratch_dir("notices");
        write_jsonl(
            dir.path(),
            "2024-01-01-00-00-00",
            &[
                user_line("real prompt", "1"),
                notification_line("background task done", "2"),
            ],
        );
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        let entries = workspace_history(&persistence, 2000, &|| false);
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["real prompt"], "harness notice excluded");
    }
}
