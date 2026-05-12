//! One-shot migration walker for legacy on-disk thread shapes.
//!
//! Pre-§3 logs recorded tool errors as freestanding
//! [`ConversationEntryKind::UserOutput`](crate::log::ConversationEntryKind::UserOutput)
//! entries carrying [`UserOutput::ToolError`]. Once the §3 work landed
//! the structured [`ConversationEntryKind::ToolResult`] variant became
//! the canonical home for tool-result rendering payloads. Legacy
//! entries still parse — `UserOutput` lives on as a serde-only shape —
//! but they bypass the renderer's `ToolDetails` pipeline so resumed
//! threads show only a faint freestanding-error placeholder instead
//! of the structured payload a live run would surface.
//!
//! [`walk_threads_dir`] is the one-shot walker that promotes those
//! legacy entries to the new shape. It runs once on binary startup
//! and is idempotent: a file that's already been migrated drops a
//! sibling `<name>.jsonl.bak` and is skipped on subsequent passes.
//!
//! Per-file behaviour:
//!
//! - Files with no [`UserOutput::ToolError`] entries are left
//!   untouched and no `.bak` is created — the walker is a no-op on
//!   fresh threads so users without legacy data don't pay any disk
//!   cost.
//! - For each [`UserOutput::ToolError`], walk the parent chain back
//!   to the closest assistant [`ConversationEntryKind::Message`] and
//!   find the matching `tool_use` block by tool name. The matched
//!   `tool_use_id` becomes the key under which the structured
//!   [`ToolDetails::Text`] payload rides.
//! - If a [`ConversationEntryKind::ToolResult`] entry (or a legacy
//!   user-role [`ConversationEntryKind::Message`] with
//!   [`ContentBlockParam::ToolResultBlock`] content) already covers
//!   that `tool_use_id` in the same file, the legacy entry is
//!   **dropped** instead of rewritten — that case is the transitional
//!   double-write the agent emitted during the changeover, where the
//!   structured `ToolResult` already carries the canonical payload.
//! - When no preceding `tool_use` block can be found in the parent
//!   chain (orphan errors from very early thread shapes), the entry
//!   is preserved as-is so no rendering payload is lost; the renderer
//!   continues to fall back to its [`UserOutput::ToolError`] handler.
//!
//! Each migrated file is rewritten atomically: the original is moved
//! to `<name>.jsonl.bak` and a freshly-written `<name>.jsonl.tmp` is
//! renamed into place. Renames within a single directory are atomic
//! on the platforms `aj` supports.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use aj_agent::tool::ToolDetails;
use aj_agent::types::UserOutput;
use aj_models::messages::{ContentBlockParam, MessageParam, Role};

use crate::log::{ConversationEntry, ConversationEntryKind, ConversationError};

/// Summary returned by [`walk_threads_dir`]. Useful for logging and
/// for tests that want to assert on the migration outcome.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationSummary {
    /// Number of `.jsonl` files inspected (including those skipped
    /// because they already had a `.bak` sibling).
    pub files_scanned: usize,
    /// Number of `.jsonl` files that actually had at least one
    /// legacy entry to migrate and were rewritten on disk.
    pub files_migrated: usize,
    /// Number of `.jsonl` files skipped because a sibling `.bak`
    /// already existed.
    pub files_skipped: usize,
    /// Number of legacy [`UserOutput::ToolError`] entries promoted
    /// to structured [`ConversationEntryKind::ToolResult`] entries.
    pub entries_rewritten: usize,
    /// Number of legacy [`UserOutput::ToolError`] entries dropped
    /// because a structured tool-result entry already covered the
    /// same `tool_use_id` in the same file.
    pub entries_dropped: usize,
    /// Number of legacy [`UserOutput::ToolError`] entries left in
    /// place because no preceding `tool_use` could be found in the
    /// parent chain.
    pub entries_orphaned: usize,
}

/// Walk every `.jsonl` file directly under `threads_dir` and migrate
/// legacy [`UserOutput::ToolError`] entries to the structured
/// [`ConversationEntryKind::ToolResult`] shape.
///
/// Idempotent: files with an existing `<stem>.jsonl.bak` sibling are
/// skipped. A file with no legacy entries is left untouched and no
/// `.bak` sibling is created. Per-file errors (parse failures, IO
/// errors) are logged via `tracing::warn!` and the walker carries
/// on with the remaining files.
///
/// Returns a [`MigrationSummary`] describing what happened so callers
/// can log a one-line summary or assert in tests.
pub fn walk_threads_dir(threads_dir: &Path) -> Result<MigrationSummary, ConversationError> {
    let mut summary = MigrationSummary::default();
    if !threads_dir.exists() {
        return Ok(summary);
    }

    for entry in fs::read_dir(threads_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }

        summary.files_scanned += 1;

        let bak = bak_sibling(&path);
        if bak.exists() {
            summary.files_skipped += 1;
            tracing::debug!(
                "migrate: skipping {} (already has .bak sibling)",
                path.display()
            );
            continue;
        }

        if let Err(err) = migrate_file(&path, &mut summary) {
            tracing::warn!("migrate: failed to migrate {}: {err}", path.display());
        }
    }
    Ok(summary)
}

/// `<stem>.jsonl` → `<stem>.jsonl.bak`. The suffix is appended
/// rather than replacing the extension so the original filename
/// stays inside the backup name (so a grep on the thread id finds
/// both files together).
fn bak_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// `<stem>.jsonl` → `<stem>.jsonl.tmp`. Same naming rule as
/// [`bak_sibling`] so atomic rename targets land in the same
/// directory as the original.
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Migrate one `.jsonl` file in place, if it contains any legacy
/// entries. No-op when the file is already clean.
fn migrate_file(path: &Path, summary: &mut MigrationSummary) -> Result<(), ConversationError> {
    let entries = read_entries(path)?;

    // Cheap pre-scan: if there are no legacy entries we skip the
    // file entirely (no .bak, no rewrite). Users without legacy
    // data shouldn't pay any disk cost on every startup.
    let has_legacy = entries.iter().any(|e| {
        matches!(
            &e.entry,
            ConversationEntryKind::UserOutput(UserOutput::ToolError { .. })
        )
    });
    if !has_legacy {
        return Ok(());
    }

    // Build the by-id index used to walk parent_id chains.
    let by_id: HashMap<&str, &ConversationEntry> =
        entries.iter().map(|e| (e.id.as_str(), e)).collect();

    // Collect every `tool_use_id` that already has a recorded
    // tool-result block on disk. Both encodings count: the
    // structured [`ConversationEntryKind::ToolResult`] variant
    // (post-§3) and the legacy user-role [`ConversationEntryKind::Message`]
    // carrying [`ContentBlockParam::ToolResultBlock`] content. A
    // legacy `UserOutput::ToolError` whose `tool_use_id` is already
    // covered is a transitional double-write — drop it on migration.
    let mut resolved: HashSet<String> = HashSet::new();
    for entry in &entries {
        let blocks = match &entry.entry {
            ConversationEntryKind::ToolResult { content, .. } => content.as_slice(),
            ConversationEntryKind::Message(MessageParam {
                role: Role::User,
                content,
            }) => content.as_slice(),
            _ => &[],
        };
        for b in blocks {
            if let ContentBlockParam::ToolResultBlock { tool_use_id, .. } = b {
                resolved.insert(tool_use_id.clone());
            }
        }
    }

    // Plan and apply the rewrite. `claimed` tracks `tool_use_id`s
    // that earlier-pass entries have already consumed, so a batch
    // of N legacy errors sharing a single assistant tool_use turn
    // gets distinct ids (matching the order the agent emitted them
    // in the live run). `needs_rewrite` flips on the first actual
    // change (rewrite or drop) so a file containing only orphan
    // entries is left untouched — orphans aren't a semantic change
    // and writing a `.bak` for them would only confuse the user.
    let mut claimed: HashSet<String> = HashSet::new();
    let mut migrated: Vec<ConversationEntry> = Vec::with_capacity(entries.len());
    let mut needs_rewrite = false;

    for entry in &entries {
        let ConversationEntryKind::UserOutput(UserOutput::ToolError {
            tool_name,
            input: _,
            error,
        }) = &entry.entry
        else {
            migrated.push(entry.clone());
            continue;
        };

        match find_matching_tool_use_id(entry, tool_name, &by_id, &resolved, &claimed) {
            None => {
                // Orphan: no preceding `tool_use` block. Preserve
                // the entry verbatim so the renderer's legacy
                // `UserOutput::ToolError` path keeps surfacing it;
                // a future cleanup can revisit once the legacy
                // renderer goes away.
                summary.entries_orphaned += 1;
                migrated.push(entry.clone());
            }
            Some(id) if resolved.contains(&id) => {
                // Transitional double-write: a structured
                // tool-result entry already carries the canonical
                // payload for this `tool_use_id`. The legacy
                // freestanding error is redundant — drop it.
                summary.entries_dropped += 1;
                needs_rewrite = true;
                // (no push)
            }
            Some(id) => {
                claimed.insert(id.clone());
                summary.entries_rewritten += 1;
                needs_rewrite = true;
                migrated.push(rewrite_as_tool_result(entry, &id, tool_name, error));
            }
        }
    }

    if !needs_rewrite {
        // Only orphan entries — no semantic change worth writing.
        return Ok(());
    }

    write_entries_atomically(path, &migrated)?;
    summary.files_migrated += 1;
    Ok(())
}

/// Read every JSONL line of `path` as a [`ConversationEntry`].
/// Blank lines are tolerated; parse failures abort the file (the
/// surrounding walker logs and moves on).
fn read_entries(path: &Path) -> Result<Vec<ConversationEntry>, ConversationError> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry = serde_json::from_str::<ConversationEntry>(&line)?;
        entries.push(entry);
    }
    Ok(entries)
}

/// Walk the `parent_id` chain back from `entry` until we hit the
/// closest assistant [`ConversationEntryKind::Message`]. Return the
/// first matching `tool_use_id` whose tool name equals `tool_name`
/// and that has not already been claimed by an earlier-migrated
/// legacy entry. `resolved` is consulted so the caller can decide
/// between "drop as duplicate" and "rewrite as structured" once a
/// candidate id is in hand.
fn find_matching_tool_use_id(
    entry: &ConversationEntry,
    tool_name: &str,
    by_id: &HashMap<&str, &ConversationEntry>,
    resolved: &HashSet<String>,
    claimed: &HashSet<String>,
) -> Option<String> {
    // Defensive bound on the walk: a corrupt file with a cyclic
    // parent_id chain shouldn't lock the walker into an infinite
    // loop. Real chains are at most a few hundred entries; 4096 is
    // a comfortable upper bound.
    let mut cursor = entry.parent_id.as_deref()?;
    for _ in 0..4096 {
        let parent = by_id.get(cursor)?;
        match &parent.entry {
            ConversationEntryKind::Message(MessageParam {
                role: Role::Assistant,
                content,
            }) => {
                // Prefer unclaimed-and-unresolved candidates first:
                // a batch of N errors sharing this assistant
                // message gets one id each, picked in source order.
                // If every candidate is already covered, fall
                // through to "duplicate" by returning any matching
                // id (the caller will see it in `resolved` and
                // route the entry to the drop arm).
                let mut first_match: Option<String> = None;
                for b in content {
                    if let ContentBlockParam::ToolUseBlock { id, name, .. } = b {
                        if name != tool_name {
                            continue;
                        }
                        if first_match.is_none() {
                            first_match = Some(id.clone());
                        }
                        if !claimed.contains(id) && !resolved.contains(id) {
                            return Some(id.clone());
                        }
                    }
                }
                return first_match;
            }
            _ => {
                cursor = parent.parent_id.as_deref()?;
            }
        }
    }
    None
}

/// Build a rewritten [`ConversationEntry`] for a legacy
/// [`UserOutput::ToolError`]. Preserves `id`, `parent_id`,
/// `timestamp`, `thread`, and `agent_id` so parent chains and
/// thread framing stay stable across the migration.
fn rewrite_as_tool_result(
    original: &ConversationEntry,
    tool_use_id: &str,
    tool_name: &str,
    error: &str,
) -> ConversationEntry {
    let mut details = HashMap::new();
    details.insert(
        tool_use_id.to_string(),
        ToolDetails::Text {
            summary: format!("{tool_name}: error"),
            body: error.to_string(),
        },
    );
    let content = vec![ContentBlockParam::ToolResultBlock {
        tool_use_id: tool_use_id.to_string(),
        content: error.to_string().into(),
        is_error: true,
    }];
    ConversationEntry {
        id: original.id.clone(),
        parent_id: original.parent_id.clone(),
        timestamp: original.timestamp,
        thread: original.thread,
        agent_id: original.agent_id,
        entry: ConversationEntryKind::ToolResult { content, details },
    }
}

/// Serialize `entries` line-by-line to `<path>.tmp`, then rotate
/// the original to `<path>.bak` and rename `<path>.tmp` into the
/// original's spot. Three-step ordering keeps a recoverable state
/// at every crash point:
///
/// 1. Copy original → `.bak`. If we crash after this, the next
///    walker pass sees `.bak` and skips the file — the user has
///    a recoverable backup and the file is unchanged.
/// 2. Write the new contents to `.tmp` and `fsync` it. If we
///    crash, `.tmp` is leftover; the next walker pass starts from
///    `.bak`-aware idempotency.
/// 3. Atomic rename `.tmp` → original. Same-directory rename is
///    atomic on the POSIX and Windows platforms `aj` runs on.
fn write_entries_atomically(
    path: &Path,
    entries: &[ConversationEntry],
) -> Result<(), ConversationError> {
    let bak = bak_sibling(path);
    let tmp = tmp_sibling(path);

    // Make sure no stale `.tmp` from a prior crashed pass survives —
    // we own the name and we're about to overwrite anyway.
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }

    // Step 1: copy original to `.bak` first. If the copy fails the
    // original is still in place; the walker reports an error and
    // moves on.
    fs::copy(path, &bak)?;

    // Step 2: write the new file under `.tmp` and fsync. A torn
    // write under the original filename would be unrecoverable;
    // doing the work under a sibling name keeps the original
    // intact until step 3.
    let mut tmp_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)?;
    for entry in entries {
        let line = serde_json::to_string(entry)?;
        writeln!(tmp_file, "{line}")?;
    }
    tmp_file.sync_all()?;
    drop(tmp_file);

    // Step 3: atomic same-directory rename. Overwrites the
    // original.
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use aj_models::messages::{ContentBlockParam, Role};
    use serde_json::json;
    use tempfile::TempDir;

    use crate::log::{ConversationLog, ConversationView, ThreadFilter};
    use crate::persistence::ConversationPersistence;

    /// Build a fresh log under a temp dir. Returns the temp dir
    /// (so it stays alive for the test) and the persistence handle
    /// pointing at it.
    fn fixture() -> (TempDir, ConversationPersistence) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        (dir, persistence)
    }

    /// Seed a log with a user prompt followed by an assistant
    /// `tool_use` and then a freestanding
    /// [`UserOutput::ToolError`] entry referencing it. Mirrors the
    /// shape pre-§3 logs took for tool-error reporting.
    fn legacy_thread(
        persistence: &ConversationPersistence,
        tool_use_id: &str,
        tool_name: &str,
        error: &str,
    ) -> String {
        let mut log = ConversationLog::create(persistence).expect("create log");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("user");
            view.add_assistant_message(vec![ContentBlockParam::ToolUseBlock {
                id: tool_use_id.to_string(),
                name: tool_name.to_string(),
                input: json!({}),
                caller: None,
            }])
            .expect("assistant");
            view.add_user_output(UserOutput::ToolError {
                tool_name: tool_name.to_string(),
                input: "<args>".to_string(),
                error: error.to_string(),
            })
            .expect("tool error");
        }
        log.thread_id().to_string()
    }

    /// Read every entry of `thread_id` back from disk.
    fn read_thread(
        persistence: &ConversationPersistence,
        thread_id: &str,
    ) -> Vec<ConversationEntry> {
        let log = ConversationLog::resume(persistence, thread_id).expect("resume");
        log.entries_in_order().into_iter().cloned().collect()
    }

    #[test]
    fn walk_is_noop_when_threads_dir_missing() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        let summary = walk_threads_dir(&missing).expect("walk missing");
        assert_eq!(summary, MigrationSummary::default());
    }

    #[test]
    fn walk_skips_files_with_no_legacy_entries() {
        // A fresh thread that only carries Message entries and a
        // structured ToolResult should be left strictly alone: no
        // rewrite, no `.bak` sibling.
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("u");
            view.add_assistant_message(vec![ContentBlockParam::ToolUseBlock {
                id: "tu-1".into(),
                name: "ping".into(),
                input: json!({}),
                caller: None,
            }])
            .expect("a");
            view.add_tool_result(
                vec![ContentBlockParam::ToolResultBlock {
                    tool_use_id: "tu-1".into(),
                    content: "ok".to_string().into(),
                    is_error: false,
                }],
                HashMap::new(),
            )
            .expect("tr");
        }
        let path = log.path().to_path_buf();
        drop(log);

        let summary = walk_threads_dir(persistence.threads_dir()).expect("walk");
        assert_eq!(summary.files_scanned, 1);
        assert_eq!(summary.files_migrated, 0);
        assert_eq!(summary.entries_rewritten, 0);
        assert_eq!(summary.entries_dropped, 0);
        assert_eq!(summary.entries_orphaned, 0);

        // No .bak sibling was created on a clean file.
        let bak = bak_sibling(&path);
        assert!(!bak.exists(), "{} should not exist", bak.display());
    }

    #[test]
    fn walk_rewrites_legacy_tool_error_as_structured_tool_result() {
        let (_dir, persistence) = fixture();
        let thread_id = legacy_thread(&persistence, "tu-1", "ping", "boom");

        let summary = walk_threads_dir(persistence.threads_dir()).expect("walk");
        assert_eq!(summary.files_scanned, 1);
        assert_eq!(summary.files_migrated, 1);
        assert_eq!(summary.entries_rewritten, 1);
        assert_eq!(summary.entries_dropped, 0);
        assert_eq!(summary.entries_orphaned, 0);

        // The on-disk shape now carries a ToolResult entry where the
        // freestanding UserOutput::ToolError used to be.
        let entries = read_thread(&persistence, &thread_id);
        assert!(
            !entries.iter().any(|e| matches!(
                &e.entry,
                ConversationEntryKind::UserOutput(UserOutput::ToolError { .. })
            )),
            "legacy ToolError should be gone"
        );
        let tool_result = entries
            .iter()
            .find_map(|e| match &e.entry {
                ConversationEntryKind::ToolResult { content, details } => Some((content, details)),
                _ => None,
            })
            .expect("ToolResult entry present");
        let (content, details) = tool_result;
        assert_eq!(content.len(), 1);
        match &content[0] {
            ContentBlockParam::ToolResultBlock {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "tu-1");
                assert_eq!(content.text(), "boom");
                assert!(*is_error);
            }
            other => panic!("expected ToolResultBlock, got {other:?}"),
        }
        let stored = details.get("tu-1").expect("details keyed by tool_use_id");
        match stored {
            ToolDetails::Text { summary, body } => {
                assert_eq!(summary, "ping: error");
                assert_eq!(body, "boom");
            }
            other => panic!("expected Text details, got {other:?}"),
        }

        // The `.bak` sibling exists and carries the pre-migration
        // bytes verbatim.
        let path = persistence.threads_dir().join(format!("{thread_id}.jsonl"));
        let bak = bak_sibling(&path);
        assert!(bak.exists(), "{} should exist", bak.display());
        let bak_text = std::fs::read_to_string(&bak).expect("read bak");
        assert!(
            bak_text.contains("\"ToolError\""),
            "bak should preserve the original legacy entry"
        );
    }

    #[test]
    fn walk_drops_legacy_tool_error_when_structured_result_already_covers_it() {
        // Mirrors the transitional shape today's agent emits: both
        // a legacy `UserOutput::ToolError` and a structured
        // `ConversationEntryKind::ToolResult` carrying the same
        // `tool_use_id`. The legacy entry is redundant; the walker
        // should drop it.
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("u");
            view.add_assistant_message(vec![ContentBlockParam::ToolUseBlock {
                id: "tu-1".into(),
                name: "ping".into(),
                input: json!({}),
                caller: None,
            }])
            .expect("a");
            view.add_user_output(UserOutput::ToolError {
                tool_name: "ping".into(),
                input: "<args>".into(),
                error: "boom".into(),
            })
            .expect("legacy");
            view.add_tool_result(
                vec![ContentBlockParam::ToolResultBlock {
                    tool_use_id: "tu-1".into(),
                    content: "boom".to_string().into(),
                    is_error: true,
                }],
                HashMap::new(),
            )
            .expect("structured");
        }
        let thread_id = log.thread_id().to_string();
        drop(log);

        let summary = walk_threads_dir(persistence.threads_dir()).expect("walk");
        assert_eq!(summary.entries_dropped, 1);
        assert_eq!(summary.entries_rewritten, 0);
        assert_eq!(summary.entries_orphaned, 0);
        assert_eq!(summary.files_migrated, 1);

        // Only the structured ToolResult remains; no UserOutput
        // entry survives the migration.
        let entries = read_thread(&persistence, &thread_id);
        assert!(
            !entries
                .iter()
                .any(|e| matches!(&e.entry, ConversationEntryKind::UserOutput(_)))
        );
        let tool_result_count = entries
            .iter()
            .filter(|e| matches!(e.entry, ConversationEntryKind::ToolResult { .. }))
            .count();
        assert_eq!(
            tool_result_count, 1,
            "the original structured entry survives"
        );
    }

    #[test]
    fn walk_preserves_orphan_tool_error_without_preceding_tool_use() {
        // No assistant `tool_use` precedes the legacy
        // `UserOutput::ToolError`: the walker can't synthesize a
        // sane `tool_use_id`, so it leaves the entry as-is. The
        // file is still considered untouched (no `.bak`).
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("u");
            // No assistant turn; just a freestanding error entry.
            view.add_user_output(UserOutput::ToolError {
                tool_name: "ping".into(),
                input: "<args>".into(),
                error: "boom".into(),
            })
            .expect("legacy");
        }
        let thread_id = log.thread_id().to_string();
        let path = log.path().to_path_buf();
        drop(log);

        let summary = walk_threads_dir(persistence.threads_dir()).expect("walk");
        assert_eq!(summary.entries_orphaned, 1);
        assert_eq!(summary.entries_rewritten, 0);
        assert_eq!(summary.entries_dropped, 0);
        // The migration still rewrites the file (it records the
        // orphan accounting) but the orphan survives verbatim.
        // Asserting on files_migrated would over-specify behaviour;
        // what matters is the entry's preservation.
        let entries = read_thread(&persistence, &thread_id);
        assert!(entries.iter().any(|e| matches!(
            &e.entry,
            ConversationEntryKind::UserOutput(UserOutput::ToolError { .. })
        )));

        // Files containing only orphan entries should NOT be
        // rewritten — they're effectively no-ops. The walker
        // signals this by leaving `files_migrated` at 0 and not
        // creating a `.bak` sibling.
        assert_eq!(summary.files_migrated, 0);
        assert!(!bak_sibling(&path).exists());
    }

    #[test]
    fn walk_is_idempotent() {
        // Running the walker twice in a row must be safe: the
        // second pass sees the `.bak` sibling left by the first
        // pass and skips the file entirely.
        let (_dir, persistence) = fixture();
        let _thread_id = legacy_thread(&persistence, "tu-1", "ping", "boom");

        let first = walk_threads_dir(persistence.threads_dir()).expect("first walk");
        assert_eq!(first.entries_rewritten, 1);
        assert_eq!(first.files_migrated, 1);

        let second = walk_threads_dir(persistence.threads_dir()).expect("second walk");
        assert_eq!(second.entries_rewritten, 0);
        assert_eq!(second.entries_dropped, 0);
        assert_eq!(second.entries_orphaned, 0);
        assert_eq!(second.files_migrated, 0);
        assert_eq!(second.files_skipped, 1);
    }

    #[test]
    fn walk_handles_batch_of_errors_sharing_one_assistant_turn() {
        // Two tool_use blocks on a single assistant message, each
        // followed by its own freestanding ToolError. The walker
        // must claim distinct `tool_use_id`s in source order so
        // both rewrites are well-formed.
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("u");
            view.add_assistant_message(vec![
                ContentBlockParam::ToolUseBlock {
                    id: "tu-1".into(),
                    name: "ping".into(),
                    input: json!({}),
                    caller: None,
                },
                ContentBlockParam::ToolUseBlock {
                    id: "tu-2".into(),
                    name: "ping".into(),
                    input: json!({}),
                    caller: None,
                },
            ])
            .expect("a");
            view.add_user_output(UserOutput::ToolError {
                tool_name: "ping".into(),
                input: "<args1>".into(),
                error: "first".into(),
            })
            .expect("e1");
            view.add_user_output(UserOutput::ToolError {
                tool_name: "ping".into(),
                input: "<args2>".into(),
                error: "second".into(),
            })
            .expect("e2");
        }
        let thread_id = log.thread_id().to_string();
        drop(log);

        let summary = walk_threads_dir(persistence.threads_dir()).expect("walk");
        assert_eq!(summary.entries_rewritten, 2);
        assert_eq!(summary.entries_dropped, 0);
        assert_eq!(summary.entries_orphaned, 0);

        let entries = read_thread(&persistence, &thread_id);
        let mut tool_result_ids: Vec<String> = Vec::new();
        for e in &entries {
            if let ConversationEntryKind::ToolResult { content, .. } = &e.entry {
                for b in content {
                    if let ContentBlockParam::ToolResultBlock { tool_use_id, .. } = b {
                        tool_result_ids.push(tool_use_id.clone());
                    }
                }
            }
        }
        // Both ids appear once each, in source order.
        assert_eq!(
            tool_result_ids,
            vec!["tu-1".to_string(), "tu-2".to_string()]
        );
    }

    #[test]
    fn walk_preserves_parent_chain_after_rewrite() {
        // The rewritten entry must keep the same `parent_id` it
        // had before so subsequent entries (which reference it as
        // their parent) still link cleanly. Test by chaining a
        // user follow-up after the rewritten entry and asserting
        // that `linearize` walks back through it correctly.
        let (_dir, persistence) = fixture();
        let mut log = ConversationLog::create(&persistence).expect("create");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("u");
            view.add_assistant_message(vec![ContentBlockParam::ToolUseBlock {
                id: "tu-1".into(),
                name: "ping".into(),
                input: json!({}),
                caller: None,
            }])
            .expect("a");
            view.add_user_output(UserOutput::ToolError {
                tool_name: "ping".into(),
                input: "<args>".into(),
                error: "boom".into(),
            })
            .expect("legacy");
            view.add_user_message(vec![ContentBlockParam::new_text_block("follow-up".into())])
                .expect("u2");
        }
        let thread_id = log.thread_id().to_string();
        drop(log);

        walk_threads_dir(persistence.threads_dir()).expect("walk");

        // Resume and linearize the user thread; the follow-up
        // user message must still be reachable.
        let log = ConversationLog::resume(&persistence, &thread_id).expect("resume");
        let head = log
            .latest_leaf(ThreadFilter::USER)
            .expect("user head exists");
        let convo = log.linearize(&head, ThreadFilter::USER);
        let last = convo.last_message().expect("at least one message");
        assert!(matches!(last.role, Role::User));
        // The follow-up user text survived.
        let text_blocks: Vec<&str> = last
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlockParam::TextBlock { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_blocks, vec!["follow-up"]);

        // And the rewritten ToolResult is present in the linearized view.
        let has_tool_result = convo
            .entries()
            .iter()
            .any(|e| matches!(e.entry, ConversationEntryKind::ToolResult { .. }));
        assert!(has_tool_result, "rewritten ToolResult should be linearized");
    }
}
