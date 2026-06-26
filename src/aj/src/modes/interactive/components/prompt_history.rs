//! Prompt-history search overlay (`/history`).
//!
//! Pairs a search box with a read-only [`SelectList`] of prompts the user
//! has submitted before. `Enter` recalls the highlighted prompt into the
//! editor (it is *not* submitted); `Esc` cancels.
//!
//! The overlay searches one of two scopes, toggled in-place with the
//! `aj.history.toggle_scope` chord (default `Ctrl+T`):
//!
//! - **This workspace** (the default): prompts from the current
//!   project's sessions directory.
//! - **All workspaces**: prompts from every project under
//!   `~/.aj/sessions`, each tagged with its project label.
//!
//! Both scopes are scanned on a blocking thread (a [`StreamingScan`] per
//! scope), not on the TUI event loop: the overlay opens immediately
//! (showing a loading indicator) and the list fills in incrementally as
//! the scan streams batches (one per session file, newest-first). The
//! current-workspace scan starts as soon as the overlay is built; the
//! all-workspaces scan is deferred until the first toggle so it costs
//! nothing when the user never leaves the workspace scope.
//!
//! The shared [`FilterableSelect`] owns the search box, the scope status
//! line, and the loading body; this component owns the scope state, the
//! per-scope entry accumulators, and the toggle chord.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use aj_session::ConversationPersistence;
use aj_tui::component::Component;
use aj_tui::components::filterable_select::FilterableSelect;
use aj_tui::components::select_list::{
    FilterMode, SelectItem, SelectList, SelectListLayout, SelectListTheme,
};
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;
use aj_tui::tui::RenderHandle;

use crate::config::keybindings::ACTION_HISTORY_TOGGLE_SCOPE;
use crate::modes::interactive::components::outcome::OutcomeSlot;
use crate::modes::interactive::components::streaming_scan::StreamingScan;
use crate::modes::interactive::editor_ext::scan_file_user_prompts;

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
    /// Project label (the `~/.aj/sessions` subdirectory name). `None`
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
pub type PromptHistoryOutcomeHandle = OutcomeSlot<PromptHistoryOutcome>;

/// A streaming scan: given an `emit` sink, drives the scan and calls
/// `emit` once per session file. Boxed so the all-workspaces scan can
/// be stored until the first toggle.
type Scan = Box<dyn FnOnce(&mut dyn FnMut(Vec<PromptHistoryEntry>)) + Send>;

/// Prompt-history search component.
pub struct PromptHistorySearchComponent {
    inner: FilterableSelect,
    scope: Scope,
    /// Per-scope background scans. The all-workspaces scan is spawned on
    /// the first toggle (consuming `all_scan_factory`), so it costs
    /// nothing until then.
    workspace_scan: StreamingScan<PromptHistoryEntry>,
    all_scan: Option<StreamingScan<PromptHistoryEntry>>,
    all_scan_factory: Option<Scan>,
    /// Entries per scope, accumulated as batches arrive. Kept so a scope
    /// toggle can rebuild the list from the other scope's already-loaded
    /// rows without re-scanning.
    workspace_entries: Vec<PromptHistoryEntry>,
    all_entries: Vec<PromptHistoryEntry>,
    render_handle: RenderHandle,
    outcome: PromptHistoryOutcomeHandle,
}

impl PromptHistorySearchComponent {
    /// Build the overlay and kick off the current-workspace scan on a
    /// blocking thread. The list starts empty (showing a loading
    /// indicator) and fills in as the scan streams batches. `all_scan`
    /// produces the all-workspaces set on demand, scanned the first
    /// time the user toggles to that scope.
    pub fn new(
        theme: SelectListTheme,
        max_visible_rows: usize,
        render_handle: RenderHandle,
        workspace_scan: impl FnOnce(&mut dyn FnMut(Vec<PromptHistoryEntry>)) + Send + 'static,
        all_scan: impl FnOnce(&mut dyn FnMut(Vec<PromptHistoryEntry>)) + Send + 'static,
    ) -> Self {
        let status_style = Arc::clone(&theme.description);
        let list = SelectList::new(Vec::new(), max_visible_rows, theme, list_layout());
        let mut inner = FilterableSelect::new("search: ", list, status_style).with_status_line();
        inner.set_loading_message("Loading prompt history…");

        let outcome = PromptHistoryOutcomeHandle::new();
        let confirm = outcome.clone();
        inner.on_select = Some(Box::new(move |item| {
            confirm.set(PromptHistoryOutcome::Recalled {
                text: item.value.clone(),
            });
        }));
        let cancel = outcome.clone();
        inner.on_cancel = Some(Box::new(move || {
            cancel.set(PromptHistoryOutcome::Cancelled)
        }));

        let mut component = Self {
            inner,
            scope: Scope::Workspace,
            workspace_scan: StreamingScan::spawn(workspace_scan, render_handle.clone()),
            all_scan: None,
            all_scan_factory: Some(Box::new(all_scan)),
            workspace_entries: Vec::new(),
            all_entries: Vec::new(),
            render_handle,
            outcome,
        };
        component.sync_status();
        component
    }

    /// Hand the host a clone of the outcome slot.
    pub fn outcome_handle(&self) -> PromptHistoryOutcomeHandle {
        self.outcome.clone()
    }

    /// Whether the overlay is currently showing the all-workspaces
    /// scope, for the border key-hint.
    pub fn showing_all_workspaces(&self) -> bool {
        self.scope == Scope::All
    }

    /// Entries backing the currently-selected scope.
    fn current_entries(&self) -> &[PromptHistoryEntry] {
        match self.scope {
            Scope::Workspace => &self.workspace_entries,
            Scope::All => &self.all_entries,
        }
    }

    /// Whether the visible scope's background scan is still running.
    fn is_current_loading(&self) -> bool {
        match self.scope {
            Scope::Workspace => self.workspace_scan.is_loading(),
            Scope::All => self
                .all_scan
                .as_ref()
                .is_some_and(StreamingScan::is_loading),
        }
    }

    /// Drain both scans' delivered batches: accumulate each into its
    /// scope, stream the visible scope's new rows into the live list
    /// (coalesced into one [`SelectList::extend_items`]), then refresh
    /// the status line and loading indicator.
    fn drain(&mut self) {
        let workspace = self.workspace_scan.drain();
        if !workspace.is_empty() {
            if self.scope == Scope::Workspace {
                let items = build_items(&workspace);
                self.inner.list_mut().extend_items(items);
            }
            self.workspace_entries.extend(workspace);
        }

        // `as_mut().map(drain)` releases the `all_scan` borrow before we
        // touch the other fields below.
        if let Some(all) = self.all_scan.as_mut().map(StreamingScan::drain)
            && !all.is_empty()
        {
            if self.scope == Scope::All {
                let items = build_items(&all);
                self.inner.list_mut().extend_items(items);
            }
            self.all_entries.extend(all);
        }

        self.sync_status();
    }

    /// Refresh the scope status line and the loading-body flag from the
    /// current scope's state.
    fn sync_status(&mut self) {
        let loading = self.is_current_loading();
        let mut text = match self.scope {
            Scope::Workspace => "Showing: this workspace".to_string(),
            Scope::All => "Showing: all workspaces".to_string(),
        };
        // While the visible scope is still streaming, advertise it so a
        // partial list doesn't look complete. The toggle chord itself is
        // advertised on the overlay border, not here.
        if loading {
            text.push_str("  \u{2022}  loading\u{2026}");
        }
        self.inner.set_status_line(Some(text));
        self.inner.set_loading(loading);
    }

    /// Rebuild the list for the current scope, re-applying the active
    /// search filter and restoring the highlighted row when it survives.
    /// Used on scope toggle, where the whole item set changes.
    fn rebuild_list(&mut self) {
        let selected_value = self.inner.selected_item().map(|item| item.value.clone());
        let items = build_items(self.current_entries());
        // `set_items` re-applies the list's retained filter (kept in sync
        // with the search box on every keystroke), so the new scope shows
        // the same query's matches.
        self.inner.list_mut().set_items(items);
        if let Some(value) = selected_value {
            self.inner.list_mut().select_by_value(&value);
        }
    }

    /// Flip the scope, spawning the all-workspaces scan the first time
    /// it's needed, then rebuild the list for the new scope.
    fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            Scope::Workspace => {
                self.request_all_load();
                Scope::All
            }
            Scope::All => Scope::Workspace,
        };
        self.rebuild_list();
        self.sync_status();
    }

    /// Spawn the all-workspaces scan on the first toggle. The factory is
    /// consumed here, so repeated toggles never re-scan.
    fn request_all_load(&mut self) {
        if let Some(factory) = self.all_scan_factory.take() {
            self.all_scan = Some(StreamingScan::spawn(factory, self.render_handle.clone()));
        }
    }
}

/// Layout for the prompt list. A capped prefix column holds the
/// project label (all-workspaces scope only); the prompt itself fills
/// the remaining width since no right-hand column competes for space.
///
/// Prompt bodies are long and multi-line, so fuzzy subsequence matching
/// is far too permissive here (almost everything matches a multi-word
/// query). Use a substring "contains all words" filter over the whole
/// prompt instead.
fn list_layout() -> SelectListLayout {
    SelectListLayout {
        max_prefix_column_width: Some(PROJECT_LABEL_MAX),
        wrap_selection: false,
        filter_mode: FilterMode::SubstringAllTokens,
        empty_message: "No matching prompts".to_string(),
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
        self.drain();
        self.inner.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        self.drain();

        // The scope toggle is this overlay's own chord; intercept it
        // before the shared selector sees the key.
        let kb = keybindings::get();
        if kb.matches(event, ACTION_HISTORY_TOGGLE_SCOPE) {
            self.toggle_scope();
            return true;
        }

        self.inner.handle_input(event)
    }

    fn set_focused(&mut self, focused: bool) {
        self.inner.set_focused(focused);
    }

    fn set_available_height(&mut self, rows: usize) {
        self.inner.set_available_height(rows);
    }

    fn is_focused(&self) -> bool {
        self.inner.is_focused()
    }
}

// ---------------------------------------------------------------------------
// Scanning: extract submitted prompts from on-disk session logs.
// ---------------------------------------------------------------------------

/// Stream the current workspace's submitted prompts, newest-first and
/// deduplicated, invoking `emit` once per session file (each call
/// carrying that file's new prompts). Capped at [`MAX_ENTRIES`] across
/// the whole scan.
pub fn workspace_history_streaming(
    persistence: &ConversationPersistence,
    emit: &mut dyn FnMut(Vec<PromptHistoryEntry>),
) {
    let mut seen = HashSet::new();
    let mut remaining = MAX_ENTRIES;
    collect_dir(
        persistence.sessions_dir(),
        None,
        &mut seen,
        &mut remaining,
        emit,
    );
}

/// Stream submitted prompts across every project under `sessions_base`
/// (`~/.aj/sessions`), deduplicated and each tagged with its project
/// (subdirectory) label, invoking `emit` once per session file.
///
/// Projects are visited in reverse-lexicographic directory order and
/// files within a project newest-first, so a prompt's tag reflects
/// the first project (in that order) whose files contain it. The
/// directory order is unrelated to recency — it exists only to make
/// the dedup deterministic — so the tag on a prompt shared across
/// projects is stable but not a "most recent workspace" guarantee.
pub fn all_workspaces_history_streaming(
    sessions_base: &Path,
    emit: &mut dyn FnMut(Vec<PromptHistoryEntry>),
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
    // Directory names are unrelated to recency, but a stable order
    // keeps the dedup deterministic. Reverse lexicographic so the
    // listing roughly mirrors the newest-first feel within a project.
    projects.sort();
    projects.reverse();

    let mut seen = HashSet::new();
    let mut remaining = MAX_ENTRIES;
    for dir in &projects {
        if remaining == 0 {
            break;
        }
        let project = dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        collect_dir(dir, project, &mut seen, &mut remaining, emit);
    }
}

/// Walk every `*.jsonl` file in `dir`, newest file first, invoking
/// `emit` once per file with that file's new prompts (newest-first,
/// skipping bodies already in `seen`). `project` tags every entry.
/// `remaining` is the shared [`MAX_ENTRIES`] budget, decremented as
/// entries are produced; the walk stops once it hits zero.
fn collect_dir(
    dir: &Path,
    project: Option<String>,
    seen: &mut HashSet<String>,
    remaining: &mut usize,
    emit: &mut dyn FnMut(Vec<PromptHistoryEntry>),
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
        if *remaining == 0 {
            return;
        }
        // Within a file prompts are chronological; reverse so the
        // most recent prompt in this file lands first.
        let mut prompts = load_file_prompts(path);
        prompts.reverse();
        let mut batch = Vec::new();
        for text in prompts {
            if *remaining == 0 {
                break;
            }
            if seen.insert(text.clone()) {
                batch.push(PromptHistoryEntry {
                    text,
                    project: project.clone(),
                });
                *remaining -= 1;
            }
        }
        emit(batch);
    }
}

/// Extract the user-submitted prompt texts from a single session file,
/// in chronological (file) order. Delegates to the shared
/// [`scan_file_user_prompts`] (cheap pre-filter, failure isolation) and
/// applies this overlay's own trim: a fully-trimmed display string,
/// dropping blanks.
fn load_file_prompts(path: &Path) -> Vec<String> {
    scan_file_user_prompts(path)
        .into_iter()
        .filter_map(|text| {
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .collect()
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
            10,
            RenderHandle::detached(),
            move |emit| emit(workspace),
            move |emit| emit(all),
        )
    }

    /// Drain a streaming scan into a single vector (test convenience).
    fn collect(
        scan: impl FnOnce(&mut dyn FnMut(Vec<PromptHistoryEntry>)),
    ) -> Vec<PromptHistoryEntry> {
        let mut out = Vec::new();
        scan(&mut |batch| out.extend(batch));
        out
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
    fn batches_accumulate_in_arrival_order() {
        // Each `emit` is a separate batch; the list appends them rather
        // than replacing, preserving arrival (newest-first) order.
        let mut c = PromptHistorySearchComponent::new(
            identity_theme(),
            10,
            RenderHandle::detached(),
            |emit| {
                emit(vec![entry("newest", None)]);
                emit(vec![entry("older", None)]);
            },
            |_emit| {},
        );
        let body = c.render(80).join("\n");
        let newest = body.find("newest").expect("newest shown");
        let older = body.find("older").expect("older shown");
        assert!(newest < older, "expected newest before older, got: {body}");
    }

    #[test]
    fn toggle_scope_loads_all_lazily_once() {
        crate::config::keybindings::install_global_manager_defaults();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_loader = Arc::clone(&calls);
        let mut c = PromptHistorySearchComponent::new(
            identity_theme(),
            10,
            RenderHandle::detached(),
            |emit| emit(vec![entry("workspace prompt", None)]),
            move |emit| {
                calls_for_loader.fetch_add(1, Ordering::Relaxed);
                emit(vec![entry("other workspace prompt", Some("other-proj"))]);
            },
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

    use std::fs::File;
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
        let entries = collect(|emit| workspace_history_streaming(&persistence, emit));
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

        let entries = collect(|emit| all_workspaces_history_streaming(&base, emit));
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
        assert!(collect(|emit| all_workspaces_history_streaming(&base, emit)).is_empty());
    }
}
