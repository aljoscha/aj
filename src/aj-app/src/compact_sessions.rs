//! `aj compact-sessions`: rewrites stored JSONL sessions into the smaller form
//! a current build would have written.
//!
//! Three things are rewritten:
//!
//! - Tool-result details that repeat the model-facing content become
//!   references into it, as [`aj_session::compact_tool_details`] stores them.
//! - Edit details that hold the file before and after become the display
//!   diff that is stored today.
//! - A goal entry whose only successor is an entry of the same goal, with
//!   nothing but usage changed, is dropped. An active goal used to append one
//!   every second. The last of each sequence stays, so every remaining entry sees
//!   the goal it saw before, with usage at most one sequence older.
//!
//! A session is rewritten only when all of this holds, and is otherwise left
//! byte for byte as it was:
//!
//! - its lock is free, so no running `aj` has it open;
//! - every line is an entry that ends in a newline;
//! - read through [`ProjectSessions::read_entries`], the rewritten
//!   file has the same entries, last entry and goal as the original, apart
//!   from the dropped goal entries;
//! - the original did not change while it was being rewritten.
//!
//! The file is replaced by a rename and keeps its modification time, so the
//! session list keeps its order.
//!
//! Only JSONL files are rewritten. A session already imported into the
//! database keeps the entries it was imported with, so compact before
//! migrating.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use aj_conf::Config;

use aj_models::types::UserContent;
use aj_session::{
    ProjectSessions, SessionLock, compact_tool_details, is_valid_session_id, resolve_tool_details,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::Value;

/// Each dropped entry and the parent its children move to.
type Dropped = HashMap<String, Option<String>>;

/// The fields of an entry that decide whether it can be dropped.
#[derive(Deserialize)]
struct Head {
    id: String,
    parent_id: Option<String>,
    thread: String,
    #[serde(rename = "type")]
    kind: String,
    goal: Option<Value>,
    first_kept_entry_id: Option<String>,
    #[serde(default)]
    retained_user_entry_ids: Vec<String>,
}

/// What rewriting one session did.
#[derive(Default)]
struct Counts {
    bytes: u64,
    goals: usize,
    diffs: usize,
    references: usize,
}

/// What a run did, or without `write` would do.
#[derive(Debug, Default)]
pub struct Report {
    pub sessions: usize,
    pub rewritten: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub goals_dropped: usize,
    pub edits_diffed: usize,
    pub details_referenced: usize,
    /// The sessions left as they were, each with the reason.
    pub skipped: Vec<(PathBuf, String)>,
}

/// Compacts the sessions under `directory`, the sessions root by default,
/// and prints what it did. Without `write` nothing is changed.
pub fn handle_compact_sessions(directory: Option<PathBuf>, write: bool) -> Result<()> {
    let root = match directory {
        Some(directory) => directory,
        None => Config::get_config_dir()?.join("sessions"),
    };
    let report = compact_sessions(&root, write)?;
    let verb = if write { "rewrote" } else { "would rewrite" };
    let saved = (report.bytes_before - report.bytes_after) * 1000 / report.bytes_before.max(1);
    println!(
        "{verb} {} of {} sessions: {} MB -> {} MB ({}.{}% smaller)",
        report.rewritten,
        report.sessions,
        report.bytes_before / 1_000_000,
        report.bytes_after / 1_000_000,
        saved / 10,
        saved % 10,
    );
    println!(
        "  {} goal entries dropped, {} edits turned into diffs, {} details turned into references",
        report.goals_dropped, report.edits_diffed, report.details_referenced
    );
    if !report.skipped.is_empty() {
        println!("left as they were:");
        for (path, reason) in &report.skipped {
            println!("  {}: {reason}", path.display());
        }
    }
    if !write && report.rewritten > 0 {
        println!("pass --write to rewrite them");
    }
    Ok(())
}

/// Compacts every session in `root` and in the project directories inside it.
pub fn compact_sessions(root: &Path, write: bool) -> Result<Report> {
    let mut projects = vec![root.to_path_buf()];
    for entry in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            projects.push(path);
        }
    }
    projects.sort();

    let mut report = Report::default();
    for project in projects {
        let mut paths: Vec<_> = fs::read_dir(&project)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "jsonl")
                    && session_id(path).is_some_and(is_valid_session_id)
            })
            .collect();
        paths.sort();
        for path in paths {
            let size = fs::metadata(&path)?.len();
            report.sessions += 1;
            report.bytes_before += size;
            match compact_session(&project, &path, write) {
                Ok(counts) if counts.bytes < size => {
                    report.rewritten += 1;
                    report.bytes_after += counts.bytes;
                    report.goals_dropped += counts.goals;
                    report.edits_diffed += counts.diffs;
                    report.details_referenced += counts.references;
                }
                Ok(_) => report.bytes_after += size,
                Err(reason) => {
                    report.bytes_after += size;
                    report.skipped.push((path, format!("{reason:#}")));
                }
            }
        }
    }
    Ok(report)
}

fn session_id(path: &Path) -> Option<&str> {
    path.file_stem()?.to_str()
}

fn compact_session(project: &Path, path: &Path, write: bool) -> Result<Counts> {
    let id = session_id(path).context("not a session file")?;
    let persistence = ProjectSessions::new(project.to_path_buf());
    let _lock = if write {
        Some(SessionLock::try_acquire(&persistence, id, "compact-sessions")?.context("in use")?)
    } else {
        None
    };
    let stat = fs::metadata(path)?;
    let dropped = repeated_goal_records(path)?;

    if !write {
        return rewrite(path, &dropped, std::io::sink());
    }

    // A directory of its own, so the rewritten file can be loaded under the
    // session's id before it replaces anything.
    let scratch = project.join(format!(".compact-{}", std::process::id()));
    fs::create_dir_all(&scratch)?;
    let result = (|| {
        let target = scratch.join(path.file_name().context("not a session file")?);
        let mut out = BufWriter::new(File::create(&target)?);
        let counts = rewrite(path, &dropped, &mut out)?;
        let file = out.into_inner().map_err(|err| err.into_error())?;
        if counts.bytes >= stat.len() {
            return Ok(counts);
        }
        file.set_modified(stat.modified()?)?;
        file.sync_all()?;
        drop(file);

        let original = digest(&persistence, id, &dropped)?;
        // Nothing is dropped from the rewritten file: a parent it still names
        // must be an entry it holds.
        let rewritten = digest(&ProjectSessions::new(scratch.clone()), id, &Dropped::new())?;
        if original != rewritten {
            bail!("the rewritten session does not load as the original");
        }
        let now = fs::metadata(path)?;
        if now.len() != stat.len() || now.modified()? != stat.modified()? {
            bail!("changed while it was being rewritten");
        }
        fs::rename(&target, path)?;
        File::open(project)?.sync_all()?;
        Ok(counts)
    })();
    let _ = fs::remove_dir_all(&scratch);
    result
}

/// The goal entries that only repeat the goal their successor records.
fn repeated_goal_records(path: &Path) -> Result<Dropped> {
    struct Record {
        id: String,
        parent: Option<String>,
        /// The goal without its usage, for a goal entry on the user thread.
        goal: Option<String>,
    }

    let mut records = Vec::new();
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut referenced = HashSet::new();
    for line in lines(path)? {
        let head: Head = serde_json::from_slice(&line?)?;
        if let Some(parent) = &head.parent_id {
            children
                .entry(parent.clone())
                .or_default()
                .push(records.len());
        }
        referenced.extend(head.first_kept_entry_id);
        referenced.extend(head.retained_user_entry_ids);
        let goal = match head.goal {
            Some(Value::Object(mut goal))
                if head.kind == "goal_change" && head.thread == "user" =>
            {
                goal.remove("tokens_used");
                goal.remove("time_used_seconds");
                Some(serde_json::to_string(&goal)?)
            }
            _ => None,
        };
        records.push(Record {
            id: head.id,
            parent: head.parent_id,
            goal,
        });
    }

    let mut dropped = Dropped::new();
    for record in &records {
        let Some(goal) = &record.goal else { continue };
        let Some([only]) = children.get(&record.id).map(Vec::as_slice) else {
            continue;
        };
        if records[*only].goal.as_ref() == Some(goal) && !referenced.contains(&record.id) {
            dropped.insert(record.id.clone(), record.parent.clone());
        }
    }
    Ok(dropped)
}

/// Writes the session at `path` to `out` in its smaller form.
fn rewrite(path: &Path, dropped: &Dropped, mut out: impl Write) -> Result<Counts> {
    let mut counts = Counts::default();
    for line in lines(path)? {
        let line = line?;
        let text = std::str::from_utf8(&line)?;
        let head: Head = serde_json::from_str(text)?;
        if dropped.contains_key(&head.id) {
            counts.goals += 1;
            continue;
        }
        let moved = head
            .parent_id
            .as_ref()
            .is_some_and(|parent| dropped.contains_key(parent));
        let mut written = None;
        if moved || text.contains("\"details\"") {
            let mut entry: Value = serde_json::from_str(text)?;
            let mut changed = moved;
            if moved {
                entry["parent_id"] = surviving_parent(head.parent_id, dropped).into();
            }
            if let Some(change) = compact_details(&mut entry) {
                match change {
                    Change::Diff => counts.diffs += 1,
                    Change::Reference => counts.references += 1,
                }
                changed = true;
            }
            if changed {
                let mut bytes = serde_json::to_vec(&entry)?;
                bytes.push(b'\n');
                written = Some(bytes);
            }
        }
        let bytes = written.as_deref().unwrap_or(&line);
        out.write_all(bytes)?;
        counts.bytes += u64::try_from(bytes.len())?;
    }
    out.flush()?;
    Ok(counts)
}

fn surviving_parent(mut parent: Option<String>, dropped: &Dropped) -> Option<String> {
    while let Some(next) = parent.as_ref().and_then(|id| dropped.get(id)) {
        parent = next.clone();
    }
    parent
}

enum Change {
    Diff,
    Reference,
}

/// The tool result an entry holds, directly or as a nested tool's activity.
fn tool_result(entry: &mut Value) -> Option<&mut serde_json::Map<String, Value>> {
    let message = entry.get_mut("message")?;
    let message = if message.get("role").and_then(Value::as_str) == Some("tool_activity") {
        message.get_mut("message")?
    } else {
        message
    };
    let message = message.as_object_mut()?;
    (message.get("details")?.is_object() && message.get("content")?.is_array()).then_some(message)
}

fn content(result: &serde_json::Map<String, Value>) -> Option<Vec<UserContent>> {
    serde_json::from_value(result.get("content")?.clone()).ok()
}

/// Replaces an entry's details with their smaller form, when that form reads
/// back as the same details.
fn compact_details(entry: &mut Value) -> Option<Change> {
    let result = tool_result(entry)?;
    let content = content(result)?;
    let details = result.get("details")?;
    let before = serde_json::to_value(resolve_tool_details(details, &content)?).ok()?;

    let whole_files = details.get("kind").and_then(Value::as_str) == Some("diff")
        && details.get("lines").is_none();
    let mut smaller = if whole_files {
        before.clone()
    } else {
        details.clone()
    };
    if let Some(compacted) = compact_tool_details(&smaller, &content) {
        smaller = compacted;
    }
    if smaller == *details {
        return None;
    }
    let after = serde_json::to_value(resolve_tool_details(&smaller, &content)?).ok()?;
    if after != before {
        return None;
    }
    result.insert("details".to_string(), smaller);
    Some(if whole_files {
        Change::Diff
    } else {
        Change::Reference
    })
}

/// What a session's file reads as: a hash of each entry that is not dropped,
/// with details resolved and parents moved past dropped entries, then the
/// last entry and the goal on its ancestry.
fn digest(
    persistence: &ProjectSessions,
    id: &str,
    dropped: &Dropped,
) -> Result<(Vec<u64>, Value, Value)> {
    let mut hashes = Vec::new();
    let mut kept: HashMap<String, Value> = HashMap::new();
    let mut last = None;
    for entry in persistence.read_entries(id)? {
        let mut entry = serde_json::to_value(entry)?;
        let entry_id = entry["id"]
            .as_str()
            .context("an entry without an id")?
            .to_string();
        if dropped.contains_key(&entry_id) {
            continue;
        }
        let parent = entry["parent_id"].as_str().map(str::to_string);
        entry["parent_id"] = surviving_parent(parent, dropped).into();
        if let Some(result) = tool_result(&mut entry)
            && let Some(content) = content(result)
            && let Some(details) = resolve_tool_details(&result["details"], &content)
        {
            result.insert("details".to_string(), serde_json::to_value(details)?);
        }
        let mut hasher = DefaultHasher::new();
        entry.to_string().hash(&mut hasher);
        hashes.push(hasher.finish());
        // Only what the goal walk reads is kept.
        if let Some(entry) = entry.as_object_mut() {
            entry.remove("message");
        }
        kept.insert(entry_id.clone(), entry);
        last = Some(entry_id);
    }

    let mut goal = Value::Null;
    let mut cursor = last.clone();
    while let Some(entry) = cursor.as_ref().and_then(|id| kept.get(id)) {
        if entry["type"] == "goal_change" && entry["thread"] == "user" {
            goal = entry["goal"].clone();
            break;
        }
        cursor = entry["parent_id"].as_str().map(str::to_string);
    }
    Ok((hashes, last.into(), goal))
}

/// The file's lines, each with its newline. A last line without one is an
/// interrupted append, which is for `aj` to repair.
fn lines(path: &Path) -> Result<impl Iterator<Item = Result<Vec<u8>>>> {
    let mut reader = BufReader::with_capacity(1 << 20, File::open(path)?);
    Ok(std::iter::from_fn(move || {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => None,
            Ok(_) if line.ends_with(b"\n") => Some(Ok(line)),
            Ok(_) => Some(Err(anyhow!("the last line has no newline"))),
            Err(err) => Some(Err(err.into())),
        }
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ID: &str = "2026-01-02-03-04-05-006";

    fn entry(id: &str, parent: &str, rest: Value) -> Value {
        let mut entry = json!({
            "id": id,
            "parent_id": parent,
            "timestamp": "2026-01-02T03:04:05Z",
            "thread": "user",
        });
        entry
            .as_object_mut()
            .unwrap()
            .extend(rest.as_object().unwrap().clone());
        entry
    }

    fn goal(id: &str, parent: &str, status: &str, seconds: u64) -> Value {
        let goal = json!({
            "id": "goal-1",
            "objective": "ship it",
            "status": status,
            "token_budget": null,
            "tokens_used": seconds * 10,
            "time_used_seconds": seconds,
        });
        entry(id, parent, json!({ "type": "goal_change", "goal": goal }))
    }

    fn tool_result(id: &str, parent: &str, tool: &str, text: &str, details: Value) -> Value {
        let message = json!({
            "role": "tool_result",
            "tool_call_id": format!("call-{id}"),
            "tool_name": tool,
            "content": [{ "type": "text", "text": text }],
            "details": details,
            "is_error": false,
            "timestamp": 0,
        });
        entry(id, parent, json!({ "type": "message", "message": message }))
    }

    /// A session as older builds stored it: a goal recorded every second,
    /// a command's output twice, and an edit as two whole files.
    fn old_session(project: &Path) -> PathBuf {
        let output = "line of output\n".repeat(40);
        let before = "unchanged\n".repeat(200);
        let after = format!("{before}added\n");
        let records = [
            json!({
                "id": "prompt",
                "timestamp": "2026-01-02T03:04:05Z",
                "thread": "meta",
                "type": "system_prompt",
                "text": "be brief",
            }),
            goal("g1", "prompt", "active", 1),
            goal("g2", "g1", "active", 2),
            goal("g3", "g2", "active", 3),
            // A change of status is not a repeat, so `g3` stays for it.
            goal("g4", "g3", "paused", 3),
            tool_result(
                "bash",
                "g4",
                "bash",
                &output,
                json!({
                    "kind": "bash",
                    "command": "make",
                    "stdout": output,
                    "stderr": "",
                    "exit_code": 0,
                    "truncated": false,
                }),
            ),
            tool_result(
                "edit",
                "bash",
                "edit_file",
                "edited",
                json!({ "kind": "diff", "path": "a.txt", "before": before, "after": after }),
            ),
        ];
        fs::create_dir_all(project).unwrap();
        let path = project.join(format!("{ID}.jsonl"));
        let lines: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(&path, lines).unwrap();
        path
    }

    fn records(path: &Path) -> HashMap<String, Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .map(|record| (record["id"].as_str().unwrap().to_string(), record))
            .collect()
    }

    #[test]
    fn rewrites_an_old_session_into_one_that_loads_the_same() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let path = old_session(&project);
        let original = fs::read(&path).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let persistence = ProjectSessions::new(project.clone());
        let loaded = digest(&persistence, ID, &Dropped::new()).unwrap();
        let dropped = Dropped::from([
            ("g1".to_string(), Some("prompt".to_string())),
            ("g2".to_string(), Some("g1".to_string())),
        ]);
        let expected = digest(&persistence, ID, &dropped).unwrap();
        assert_eq!(expected.0.len(), loaded.0.len() - 2);

        // Without `write` the report says what would happen and nothing does.
        let report = compact_sessions(root.path(), false).unwrap();
        assert_eq!((report.sessions, report.rewritten), (1, 1));
        assert_eq!(fs::read(&path).unwrap(), original);

        // Neither does a session that is open.
        let lock = SessionLock::try_acquire(&persistence, ID, "a-running-aj")
            .unwrap()
            .unwrap();
        let report = compact_sessions(root.path(), true).unwrap();
        assert_eq!(report.rewritten, 0);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(fs::read(&path).unwrap(), original);
        drop(lock);

        let report = compact_sessions(root.path(), true).unwrap();
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(
            (
                report.goals_dropped,
                report.edits_diffed,
                report.details_referenced
            ),
            (2, 1, 1)
        );
        assert_eq!(report.bytes_after, fs::metadata(&path).unwrap().len());
        assert!(report.bytes_after < report.bytes_before / 2);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);

        let records = records(&path);
        assert!(!records.contains_key("g1") && !records.contains_key("g2"));
        assert_eq!(records["g3"]["parent_id"], json!("prompt"));
        assert_eq!(records["g3"]["goal"]["time_used_seconds"], json!(3));
        let bash = &records["bash"]["message"]["details"];
        assert!(bash.get("stdout").is_none() && bash.get("stdout_ref").is_some());
        let edit = &records["edit"]["message"]["details"];
        assert!(edit.get("before").is_none() && edit.get("lines").is_some());

        // What it loads as is the original without the two goal entries.
        assert_eq!(digest(&persistence, ID, &Dropped::new()).unwrap(), expected);
        assert_eq!(
            fs::read_dir(&project).unwrap().count(),
            2,
            "session and locks"
        );

        // A second run has nothing left to do.
        let report = compact_sessions(root.path(), true).unwrap();
        assert_eq!(report.rewritten, 0);
    }
}
