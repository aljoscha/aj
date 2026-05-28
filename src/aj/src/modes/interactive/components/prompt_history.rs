//! Prompt-history search overlay (`/history`).
//!
//! Pairs a [`TextInput`] for live fuzzy filtering with a read-only
//! [`SelectList`] of prompts the user has submitted before. `Enter`
//! recalls the highlighted prompt into the editor (it is *not*
//! submitted); `Esc` cancels.
//!
//! The overlay searches one of two scopes, toggled in-place with the
//! `aj.history.toggle_scope` chord (default `Ctrl+T`):
//!
//! - **This workspace** (the default): prompts from the current
//!   project's threads directory.
//! - **All workspaces**: prompts from every project under
//!   `~/.aj/threads`, each tagged with its project label.
//!
//! The all-workspaces set is loaded lazily on first toggle via the
//! `all_loader` closure so opening the overlay stays cheap when the
//! user never leaves the workspace scope.
//!
//! Like the command palette, the list is built once per scope and
//! filtered via [`SelectList::set_filter`] on each keystroke rather
//! than rebuilt.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::{Arc, Mutex};

use aj_session::{ConversationEntry, ConversationEntryKind, ConversationPersistence, ThreadKind};
use aj_tui::component::Component;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::components::text_input::TextInput;
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;

use crate::config::keybindings::ACTION_HISTORY_TOGGLE_SCOPE;
use crate::modes::interactive::editor_ext::extract_user_prompt_text;

/// Cap on how many prompts a single scope retains. Generous enough
/// to cover any realistic history while bounding the scan + the
/// in-memory list.
const MAX_ENTRIES: usize = 2000;

/// How much of a prompt's first line is shown in the primary column.
const PRIMARY_MAX_CHARS: usize = 120;

/// Cap on the project-label (prefix) column width in the
/// all-workspaces scope; longer slugs are truncated.
const PROJECT_LABEL_MAX: usize = 18;

/// One recallable prompt plus the project it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHistoryEntry {
    /// The full prompt text. Recalled verbatim into the editor.
    pub text: String,
    /// Project label (the `~/.aj/threads` subdirectory name). `None`
    /// for the current-workspace scope, where the project is implicit.
    pub project: Option<String>,
}

/// Which history scope the overlay is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Workspace,
    All,
}

/// Outcome of a single overlay session.
#[derive(Clone, Debug)]
pub enum PromptHistoryOutcome {
    /// The user picked a prompt; `text` is recalled into the editor.
    Recalled { text: String },
    /// The user pressed Esc.
    Cancelled,
}

/// Cheap-to-clone handle pointing at the overlay's outcome slot.
#[derive(Clone)]
pub struct PromptHistoryOutcomeHandle(Arc<Mutex<Option<PromptHistoryOutcome>>>);

impl PromptHistoryOutcomeHandle {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// Take the current outcome (if any), leaving the slot empty.
    pub fn take(&self) -> Option<PromptHistoryOutcome> {
        self.0
            .lock()
            .expect("prompt-history outcome mutex poisoned")
            .take()
    }

    fn set(&self, value: PromptHistoryOutcome) {
        *self
            .0
            .lock()
            .expect("prompt-history outcome mutex poisoned") = Some(value);
    }
}

/// Lazy loader for the all-workspaces scope. Invoked at most once
/// (the result is cached) the first time the user toggles to that
/// scope.
type AllLoader = Box<dyn Fn() -> Vec<PromptHistoryEntry>>;

/// Prompt-history search component.
pub struct PromptHistorySearchComponent {
    search: TextInput,
    list: SelectList,
    theme: SelectListTheme,
    max_visible_rows: usize,
    scope: Scope,
    workspace_entries: Vec<PromptHistoryEntry>,
    /// Cached all-workspaces entries; `None` until the first toggle.
    all_entries: Option<Vec<PromptHistoryEntry>>,
    all_loader: AllLoader,
    outcome: PromptHistoryOutcomeHandle,
}

impl PromptHistorySearchComponent {
    /// Build the overlay over `workspace_entries` (the current-project
    /// prompts, newest-first). `all_loader` produces the
    /// all-workspaces set on demand.
    pub fn new(
        theme: SelectListTheme,
        workspace_entries: Vec<PromptHistoryEntry>,
        all_loader: AllLoader,
        max_visible_rows: usize,
    ) -> Self {
        let mut search = TextInput::new("search: ");
        search.set_focused(true);

        let mut list = SelectList::new(
            build_items(&workspace_entries),
            max_visible_rows,
            theme.clone(),
            list_layout(),
        );
        list.set_focused(true);

        Self {
            search,
            list,
            theme,
            max_visible_rows,
            scope: Scope::Workspace,
            workspace_entries,
            all_entries: None,
            all_loader,
            outcome: PromptHistoryOutcomeHandle::new(),
        }
    }

    /// Hand the host a clone of the outcome slot.
    pub fn outcome_handle(&self) -> PromptHistoryOutcomeHandle {
        PromptHistoryOutcomeHandle(Arc::clone(&self.outcome.0))
    }

    /// Entries backing the currently-selected scope.
    fn current_entries(&self) -> &[PromptHistoryEntry] {
        match self.scope {
            Scope::Workspace => &self.workspace_entries,
            // `all_entries` is always populated before `scope` flips
            // to `All` (see `toggle_scope`).
            Scope::All => self
                .all_entries
                .as_deref()
                .unwrap_or(&self.workspace_entries),
        }
    }

    /// Flip the scope, lazily loading the all-workspaces set the first
    /// time it's needed, then rebuild the list for the new scope and
    /// re-apply the current search filter.
    fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            Scope::Workspace => {
                if self.all_entries.is_none() {
                    self.all_entries = Some((self.all_loader)());
                }
                Scope::All
            }
            Scope::All => Scope::Workspace,
        };

        let items = build_items(self.current_entries());
        let mut list = SelectList::new(
            items,
            self.max_visible_rows,
            self.theme.clone(),
            list_layout(),
        );
        list.set_focused(true);
        self.list = list;
        self.list.set_filter(self.search.value());
    }

    fn commit_selection(&self) {
        let Some(item) = self.list.selected_item().cloned() else {
            return;
        };
        self.outcome
            .set(PromptHistoryOutcome::Recalled { text: item.value });
    }

    fn commit_cancel(&self) {
        self.outcome.set(PromptHistoryOutcome::Cancelled);
    }

    /// Dim status line advertising the current scope and the toggle
    /// chord, rendered between the search box and the list.
    fn scope_line(&self) -> String {
        let key = aj_tui::keybindings::format_action_shortcut(ACTION_HISTORY_TOGGLE_SCOPE)
            .unwrap_or_else(|| "Ctrl+T".to_string());
        let text = match self.scope {
            Scope::Workspace => format!("this workspace  \u{2022}  {key} all workspaces"),
            Scope::All => format!("all workspaces  \u{2022}  {key} this workspace"),
        };
        (self.theme.description)(&text)
    }
}

/// Layout for the prompt list. A capped prefix column holds the
/// project label (all-workspaces scope only); the prompt itself fills
/// the remaining width since no right-hand column competes for space.
fn list_layout() -> SelectListLayout {
    SelectListLayout {
        max_prefix_column_width: Some(PROJECT_LABEL_MAX),
        ..Default::default()
    }
}

/// Build one [`SelectItem`] per entry.
///
/// - `value` is the full prompt text, recalled verbatim on confirm.
/// - `label` is the prompt's first line, truncated for display.
/// - `prefix` carries the project label in the all-workspaces scope
///   (entries with `project: None` show no prefix column).
/// - `filter_key` is the full prompt plus project so the search
///   matches multi-line bodies and the originating workspace.
fn build_items(entries: &[PromptHistoryEntry]) -> Vec<SelectItem> {
    entries
        .iter()
        .map(|e| {
            let label = truncate_chars(first_line(&e.text), PRIMARY_MAX_CHARS);
            let filter_key = match &e.project {
                Some(p) => format!("{} {}", e.text, p),
                None => e.text.clone(),
            };
            let mut item = SelectItem::new(&e.text, &label).with_filter_key(&filter_key);
            if let Some(project) = &e.project {
                item = item.with_prefix(project);
            }
            item
        })
        .collect()
}

/// First line of `text` for the primary column. Prompts are trimmed
/// at load, so this is non-blank in practice.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// Truncate to `max` characters (not bytes), appending `…` when cut.
fn truncate_chars(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let cut = max.saturating_sub(1).min(chars.len());
    let mut s: String = chars[..cut].iter().collect();
    s.push('…');
    s
}

impl Component for PromptHistorySearchComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.max_visible_rows + 3);
        lines.extend(self.search.render(width));
        lines.push(self.scope_line());
        lines.push(String::new());
        lines.extend(self.list.render(width));
        lines
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();

        if kb.matches(event, ACTION_HISTORY_TOGGLE_SCOPE) {
            drop(kb);
            self.toggle_scope();
            return true;
        }

        if kb.matches(event, "tui.select.cancel") {
            self.commit_cancel();
            return true;
        }

        if kb.matches(event, "tui.input.submit") {
            self.commit_selection();
            return true;
        }

        if kb.matches(event, "tui.select.up")
            || kb.matches(event, "tui.select.down")
            || kb.matches(event, "tui.select.pageUp")
            || kb.matches(event, "tui.select.pageDown")
        {
            drop(kb);
            return self.list.handle_input(event);
        }

        drop(kb);

        let before = self.search.value().to_string();
        let handled = self.search.handle_input(event);
        if handled && self.search.value() != before {
            self.list.set_filter(self.search.value());
        }
        handled
    }

    fn set_focused(&mut self, focused: bool) {
        self.search.set_focused(focused);
        self.list.set_focused(focused);
    }

    fn is_focused(&self) -> bool {
        self.search.is_focused()
    }
}

// ---------------------------------------------------------------------------
// Scanning: extract submitted prompts from on-disk thread logs.
// ---------------------------------------------------------------------------

/// Collect the current workspace's submitted prompts, newest-first,
/// deduplicated.
pub fn workspace_history(persistence: &ConversationPersistence) -> Vec<PromptHistoryEntry> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    collect_dir(persistence.threads_dir(), None, &mut seen, &mut out);
    out
}

/// Collect submitted prompts across every project under
/// `threads_base` (`~/.aj/threads`), deduplicated, each entry tagged
/// with its project (subdirectory) label.
///
/// Projects are visited in reverse-lexicographic directory order and
/// files within a project newest-first, so a prompt's tag reflects
/// the first project (in that order) whose files contain it. The
/// directory order is unrelated to recency — it exists only to make
/// the dedup deterministic — so the tag on a prompt shared across
/// projects is stable but not a "most recent workspace" guarantee.
pub fn all_workspaces_history(threads_base: &Path) -> Vec<PromptHistoryEntry> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    let read_dir = match std::fs::read_dir(threads_base) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::debug!(
                "could not read threads base {}: {e}",
                threads_base.display()
            );
            return out;
        }
    };

    let mut projects: Vec<_> = read_dir
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| p.is_dir())
        .collect();
    // Directory names are unrelated to recency, but a stable order
    // keeps the dedup deterministic. Reverse lexicographic so the
    // listing roughly mirrors the newest-first feel within a project.
    projects.sort();
    projects.reverse();

    for dir in &projects {
        let project = dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        collect_dir(dir, project, &mut seen, &mut out);
        if out.len() >= MAX_ENTRIES {
            break;
        }
    }
    out
}

/// Walk every `*.jsonl` file in `dir`, newest file first, appending
/// each file's prompts newest-first to `out` (skipping bodies already
/// in `seen`). `project` tags every entry produced here.
fn collect_dir(
    dir: &Path,
    project: Option<String>,
    seen: &mut HashSet<String>,
    out: &mut Vec<PromptHistoryEntry>,
) {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::debug!("could not read threads dir {}: {e}", dir.display());
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
        // Within a file prompts are chronological; reverse so the
        // most recent prompt in this file lands first.
        let mut prompts = load_file_prompts(path);
        prompts.reverse();
        for text in prompts {
            if seen.insert(text.clone()) {
                out.push(PromptHistoryEntry {
                    text,
                    project: project.clone(),
                });
                if out.len() >= MAX_ENTRIES {
                    return;
                }
            }
        }
    }
}

/// Extract the user-submitted prompt texts from a single thread file,
/// in chronological (file) order. Mirrors the failure-isolation
/// contract of [`crate::modes::interactive::editor_ext::PromptHistory`]:
/// non-UTF-8 lines, unparseable lines, and non-top-level entries are
/// skipped without aborting the file.
fn load_file_prompts(path: &Path) -> Vec<String> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!("skipping unreadable thread file {}: {e}", path.display());
            return Vec::new();
        }
    };

    let mut prompts = Vec::new();
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let entry: ConversationEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !matches!(entry.thread, ThreadKind::User) {
            continue;
        }
        if let ConversationEntryKind::Message { message } = entry.entry
            && let Some(text) = extract_user_prompt_text(&message)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                prompts.push(trimmed.to_string());
            }
        }
    }
    prompts
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aj_tui::components::select_list::SelectListTheme;
    use aj_tui::keys::Key;

    use super::*;

    fn identity_theme() -> SelectListTheme {
        SelectListTheme {
            selected_prefix: Arc::new(|s| s.to_string()),
            selected_text: Arc::new(|s| s.to_string()),
            description: Arc::new(|s| s.to_string()),
            scroll_info: Arc::new(|s| s.to_string()),
            no_match: Arc::new(|s| s.to_string()),
            prefix: Arc::new(|s| s.to_string()),
            shortcut: Arc::new(|s| s.to_string()),
        }
    }

    fn entry(text: &str, project: Option<&str>) -> PromptHistoryEntry {
        PromptHistoryEntry {
            text: text.to_string(),
            project: project.map(|s| s.to_string()),
        }
    }

    fn component(
        workspace: Vec<PromptHistoryEntry>,
        all: Vec<PromptHistoryEntry>,
    ) -> PromptHistorySearchComponent {
        PromptHistorySearchComponent::new(
            identity_theme(),
            workspace,
            Box::new(move || all.clone()),
            10,
        )
    }

    #[test]
    fn renders_workspace_entries() {
        let mut c = component(
            vec![entry("fix the bug", None), entry("add a test", None)],
            vec![],
        );
        let body = c.render(80).join("\n");
        assert!(body.contains("fix the bug"), "got: {body}");
        assert!(body.contains("add a test"), "got: {body}");
    }

    #[test]
    fn filter_narrows_by_prompt_text() {
        let mut c = component(
            vec![entry("fix the bug", None), entry("add a test", None)],
            vec![],
        );
        for ch in "test".chars() {
            c.handle_input(&Key::char(ch));
        }
        let body = c.render(80).join("\n");
        assert!(body.contains("add a test"), "got: {body}");
        assert!(!body.contains("fix the bug"), "got: {body}");
    }

    #[test]
    fn enter_recalls_full_text() {
        let mut c = component(vec![entry("line one\nline two", None)], vec![]);
        let handle = c.outcome_handle();
        c.handle_input(&Key::enter());
        match handle.take().expect("outcome set") {
            PromptHistoryOutcome::Recalled { text } => assert_eq!(text, "line one\nline two"),
            other => panic!("expected Recalled, got {other:?}"),
        }
    }

    #[test]
    fn esc_cancels() {
        let mut c = component(vec![entry("x", None)], vec![]);
        let handle = c.outcome_handle();
        c.handle_input(&Key::escape());
        assert!(matches!(
            handle.take().expect("outcome set"),
            PromptHistoryOutcome::Cancelled
        ));
    }

    #[test]
    fn toggle_scope_loads_all_lazily_once() {
        crate::config::keybindings::install_global_manager_defaults();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_loader = Arc::clone(&calls);
        let mut c = PromptHistorySearchComponent::new(
            identity_theme(),
            vec![entry("workspace prompt", None)],
            Box::new(move || {
                calls_for_loader.fetch_add(1, Ordering::Relaxed);
                vec![entry("other workspace prompt", Some("other-proj"))]
            }),
            10,
        );

        // Workspace scope only shows the workspace prompt.
        assert!(c.render(80).join("\n").contains("workspace prompt"));
        assert!(!c.render(80).join("\n").contains("other workspace prompt"));

        // Toggle to all: loader runs, the all set shows.
        c.handle_input(&Key::ctrl('t'));
        let body = c.render(80).join("\n");
        assert!(body.contains("other workspace prompt"), "got: {body}");
        assert!(body.contains("other-proj"), "project label shown: {body}");
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // Toggle back and forth: loader is not called again.
        c.handle_input(&Key::ctrl('t'));
        c.handle_input(&Key::ctrl('t'));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    // --- Scanner tests (fs-backed) ---

    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

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
            // `second` repeats; the newer occurrence wins and the
            // older one is dropped.
            &[user_line("second", "1"), user_line("third", "2")],
        );

        let persistence = ConversationPersistence::new(dir);
        let entries = workspace_history(&persistence);
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        // Newest file first, prompts within a file newest-first, then
        // older files; `second` deduped to its newest position.
        assert_eq!(texts, vec!["third", "second", "first"]);
        assert!(entries.iter().all(|e| e.project.is_none()));
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

        let entries = all_workspaces_history(&base);
        // Every prompt is tagged with the project it came from.
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
        assert!(all_workspaces_history(&base).is_empty());
    }
}
