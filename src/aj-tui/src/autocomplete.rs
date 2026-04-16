//! Autocomplete providers for path completion, slash commands, and fuzzy
//! file search.
//!
//! The primary type is [`CombinedAutocompleteProvider`]: a provider that
//! dispatches to one of three completion modes based on the text before
//! the cursor:
//!
//! 1. **`@`-prefixed fuzzy file search** — walks the working directory (or
//!    a scoped sub-tree) and ranks matches by filename similarity.
//! 2. **Slash commands** — a configurable list of top-level commands and
//!    their optional argument completers.
//! 3. **Direct path completion** — `./`, `~/`, or absolute-path prefixes
//!    resolved against `readdir` of the parent directory.
//!
//! [`CombinedAutocompleteProvider::apply_completion`] takes a selected
//! [`AutocompleteItem`] and splices it back into the input lines at the
//! cursor, handling slash-command trailing space, directory vs. file
//! suffixes, and closing quotes for quoted paths.
//!
//! # Async & cancellation
//!
//! The [`AutocompleteProvider`] trait is async and takes a
//! [`tokio_util::sync::CancellationToken`]. Implementations that walk
//! the filesystem must honor the token promptly: they run inside a
//! [`tokio::task::spawn_blocking`] worker, so callers can abort a
//! long-running walk by cancelling the token. The default
//! [`CombinedAutocompleteProvider`] uses
//! [`ignore::WalkBuilder::build_parallel`] for the `@`-fuzzy search, so
//! a single cancelled request drops out of every worker thread at once.
//!
//! # No external binaries — `ignore`-crate traversal only
//!
//! All filesystem walking goes through the [`ignore`] crate (the library
//! that backs both `ripgrep` and `fd`). We intentionally do **not** shell
//! out to the `fd` binary, and the module does not probe for it on `PATH`.
//! Reasons:
//!
//! - **No external dependency.** The feature works on any box that can
//!   run the crate, including CI sandboxes, single-binary deployments,
//!   and Windows without MSYS.
//! - **No subprocess plumbing.** Spawn, stdout buffering, exit-code
//!   handling, and signal cleanup all go away.
//! - **Deterministic tests.** Every test runs unconditionally — there is
//!   no `skipIf(!is_fd_installed)` branch.
//! - **Same ignore semantics.** `ignore::WalkBuilder` honors
//!   `.gitignore`, `.ignore`, global gitignore, and hidden-file rules
//!   out of the box, matching what `fd` would have done.
//!
//! If a future requirement seems to call for spawning `fd` (or any other
//! external binary), prefer extending the in-process implementation first.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ignore::{WalkBuilder, WalkState};
use tokio_util::sync::CancellationToken;

use crate::fuzzy::fuzzy_filter;

/// Characters that separate path-like tokens in the input line.
///
/// Encountering any of these while scanning right-to-left marks the end of
/// the current completion prefix.
const PATH_DELIMITERS: &[char] = &[' ', '\t', '"', '\'', '='];

/// Cap on the number of entries returned from the single-directory
/// `read_dir` walk used by the direct-path-completion branch.
const DIRECT_PATH_ENTRY_CAP: usize = 500;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    /// Text inserted into the buffer when this item is applied.
    pub value: String,
    /// Short human-readable label shown in the suggestion list.
    pub label: String,
    /// Optional second line / hint shown under the label.
    pub description: Option<String>,
}

impl AutocompleteItem {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// A top-level slash command registered with a provider.
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    /// Optional one-line hint shown after the command name in the
    /// suggestion list (e.g. `<file>` or `[--flag]`).
    pub argument_hint: Option<String>,
    /// Optional closure that produces argument-completion candidates when
    /// the user has typed a space after the command name. `None` means the
    /// command takes no completable arguments.
    #[allow(clippy::type_complexity)]
    pub get_argument_completions: Option<Box<dyn Fn(&str) -> Vec<AutocompleteItem> + Send + Sync>>,
}

impl SlashCommand {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            argument_hint: None,
            get_argument_completions: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_argument_hint(mut self, hint: impl Into<String>) -> Self {
        self.argument_hint = Some(hint.into());
        self
    }

    pub fn with_argument_completions<F>(mut self, completer: F) -> Self
    where
        F: Fn(&str) -> Vec<AutocompleteItem> + Send + Sync + 'static,
    {
        self.get_argument_completions = Some(Box::new(completer));
        self
    }
}

/// The result of a successful suggestion request.
#[derive(Debug, Clone)]
pub struct AutocompleteSuggestions {
    /// Ranked candidates, most relevant first.
    pub items: Vec<AutocompleteItem>,
    /// The substring of input that callers should consider "already typed"
    /// — `apply_completion` replaces exactly `prefix.len()` characters
    /// ending at the cursor.
    pub prefix: String,
}

/// The lines+cursor state returned by [`AutocompleteProvider::apply_completion`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionApplied {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

/// Options passed to [`AutocompleteProvider::get_suggestions`].
pub struct SuggestOpts {
    /// Cancellation token. Honored by implementations that do any work
    /// that may run for more than a few microseconds — in particular,
    /// any filesystem walk. Cancellation is best-effort: the provider
    /// may return partial results or `None` once the token fires.
    pub cancel: CancellationToken,
    /// `true` when the caller explicitly asked for suggestions (e.g.
    /// via Tab), so the provider should be more eager — for example,
    /// returning an empty-prefix suggestion list for the current
    /// directory.
    pub force: bool,
}

impl Default for SuggestOpts {
    fn default() -> Self {
        Self {
            cancel: CancellationToken::new(),
            force: false,
        }
    }
}

/// Trait implemented by all autocomplete backends. See
/// [`CombinedAutocompleteProvider`] for the default implementation.
///
/// # Two shapes: one-shot vs. streaming
///
/// The trait exposes two paths into a provider, and callers (typically
/// [`crate::components::editor::Editor`]) pick one per request:
///
/// - [`Self::get_suggestions`] is the **one-shot** path. The provider
///   runs whatever work it needs (synchronously in the `async` sense
///   — a single future to completion), returns a finalized
///   [`AutocompleteSuggestions`], and is done. This is right for
///   closed, in-memory candidate sets: slash commands, direct path
///   completion on a single `readdir`, etc.
///
/// - [`Self::try_start_session`] is the **streaming** path. The
///   provider returns an [`AutocompleteSession`] object whose
///   internal matcher produces results incrementally — typically by
///   walking the filesystem on a background thread and streaming
///   entries into a running fuzzy matcher (see
///   [`FuzzyFileSession`] for the canonical implementation). The
///   editor then drives the session with [`AutocompleteSession::update`]
///   on each keystroke and reads results from
///   [`AutocompleteSession::snapshot`] on each render. This is right
///   for open candidate sets that are expensive to gather but cheap
///   to re-match against a growing needle — the `@`-fuzzy-file
///   search being the motivating case.
///
/// A provider may implement both; the editor calls
/// `try_start_session` first and falls back to `get_suggestions`
/// when it returns `None`. Implementors that only care about the
/// one-shot path can leave `try_start_session` at its default
/// (which always returns `None`).
///
/// Async because implementations may do significant filesystem work.
/// Implementations should return `None` promptly when
/// [`SuggestOpts::cancel`] fires; see the module-level docs for the
/// cancellation contract.
#[async_trait]
pub trait AutocompleteProvider: Send + Sync {
    /// Compute the suggestion list for the given cursor position. Returns
    /// `None` if no completion is appropriate (no prefix match, empty
    /// candidate set, cancelled request, etc.).
    async fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        opts: SuggestOpts,
    ) -> Option<AutocompleteSuggestions>;

    /// Splice the selected item's `value` into `lines` at the cursor,
    /// replacing exactly `prefix` characters before the cursor.
    ///
    /// This is synchronous by design: it's a pure in-memory string
    /// operation that runs on the UI thread between keystrokes.
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionApplied;

    /// Try to open a streaming [`AutocompleteSession`] for the current
    /// cursor context.
    ///
    /// Providers that can serve the context incrementally — walking
    /// large filesystems, remote indexes, anything that benefits from
    /// "work keeps flowing while the user keeps typing" — return
    /// `Some(session)`. The editor then bypasses `get_suggestions`
    /// entirely for this context, instead driving the returned
    /// session via [`AutocompleteSession::update`] on keystrokes and
    /// polling [`AutocompleteSession::tick`] /
    /// [`AutocompleteSession::snapshot`] on render.
    ///
    /// Returning `None` (the default) signals that the provider has
    /// nothing streaming to offer for this position; the editor
    /// falls back to the one-shot [`Self::get_suggestions`] path.
    ///
    /// `notify` is a callback the session can invoke from worker
    /// threads whenever new information is available (new items
    /// injected, matcher progressed). The editor hooks this up to
    /// its render-wake channel so the popup refreshes live as
    /// results stream in.
    fn try_start_session(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<Box<dyn AutocompleteSession>> {
        let _ = (lines, cursor_line, cursor_col, notify);
        None
    }
}

/// A streaming source of completion candidates.
///
/// The editor owns one of these when a provider hands back a
/// streaming context from [`AutocompleteProvider::try_start_session`].
/// The session is the single place where incremental work lives: it
/// holds onto its own matcher, its own background walker, and any
/// cancellation state. When the session is dropped, that work stops.
///
/// # Lifecycle
///
/// 1. `try_start_session` constructs the session, starts walking /
///    matching, and hands it to the editor.
/// 2. Per keystroke inside the trigger context, the editor calls
///    [`Self::update`] with the new cursor position. The session
///    either absorbs the change (typical: the user is narrowing the
///    query) or reports [`SessionInvalid`] (e.g. the user typed
///    a directory separator that re-roots the walk and the walker
///    can't be cheaply redirected). In the latter case the editor
///    drops the session and starts a fresh one.
/// 3. Per render, the editor calls [`Self::tick`] with a short time
///    budget so the matcher can absorb any queued work, then reads
///    the current match list via [`Self::snapshot`].
/// 4. When the popup closes (dismiss, selection applied, context
///    lost), the editor drops the session, which cancels any
///    in-flight walk.
pub trait AutocompleteSession: Send {
    /// Substring of the current line that [`AutocompleteProvider::apply_completion`]
    /// will replace when a suggestion is chosen. Tracks the user's
    /// typed token (e.g. `@foo/bar`) across
    /// [`Self::update`] calls.
    fn prefix(&self) -> &str;

    /// Inform the session of a new cursor position within the same
    /// trigger context. Returns `Ok` when the session absorbed the
    /// change (re-parsing the needle, adjusting prefix state, etc.)
    /// and `Err(SessionInvalid)` when it can't — the editor then
    /// drops the session and opens a new one by calling
    /// [`AutocompleteProvider::try_start_session`] again.
    fn update(
        &mut self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> Result<(), SessionInvalid>;

    /// Pump any internal matcher / walker state for up to `budget_ms`
    /// milliseconds. Returns a status the editor uses to decide
    /// whether to rebuild its displayed list and whether to expect
    /// further updates.
    fn tick(&mut self, budget_ms: u64) -> SessionStatus;

    /// Current top-ranked matches. Called after [`Self::tick`]
    /// reports `changed`, or when the editor first attaches the
    /// session. Returns a bounded list suitable to feed directly
    /// into the popup.
    fn snapshot(&mut self) -> Vec<AutocompleteItem>;
}

/// Outcome of [`AutocompleteSession::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStatus {
    /// `true` when the match snapshot is newer than the one the
    /// caller last read. Callers should re-fetch via
    /// [`AutocompleteSession::snapshot`] when this is set.
    pub changed: bool,
    /// `true` when either the underlying walker hasn't finished
    /// pushing items or the matcher is still churning on the current
    /// pattern. Callers should schedule another tick later; a stable
    /// `false` means nothing will change without a new
    /// [`AutocompleteSession::update`] call.
    pub running: bool,
}

/// Marker returned by [`AutocompleteSession::update`] when the
/// session can no longer serve the new context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionInvalid;

/// Either a registered slash command or a plain completion item.
///
/// `Item` entries are rendered verbatim in the slash-command list; their
/// `value` is treated as the command name for matching purposes.
pub enum CommandEntry {
    Command(SlashCommand),
    Item(AutocompleteItem),
}

impl From<SlashCommand> for CommandEntry {
    fn from(cmd: SlashCommand) -> Self {
        CommandEntry::Command(cmd)
    }
}

impl From<AutocompleteItem> for CommandEntry {
    fn from(item: AutocompleteItem) -> Self {
        CommandEntry::Item(item)
    }
}

impl CommandEntry {
    fn name(&self) -> &str {
        match self {
            CommandEntry::Command(c) => &c.name,
            CommandEntry::Item(i) => &i.value,
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            CommandEntry::Command(c) => c.description.as_deref(),
            CommandEntry::Item(i) => i.description.as_deref(),
        }
    }

    fn argument_hint(&self) -> Option<&str> {
        match self {
            CommandEntry::Command(c) => c.argument_hint.as_deref(),
            CommandEntry::Item(_) => None,
        }
    }
}

/// The default provider: dispatches between slash commands, fuzzy `@` file
/// search, and direct path completion.
pub struct CombinedAutocompleteProvider {
    commands: Vec<CommandEntry>,
    fs: FsConfig,
}

/// Cheap-to-clone filesystem config used by the async worker threads.
/// Split out of [`CombinedAutocompleteProvider`] because the provider
/// itself holds `Box<dyn Fn>` slash-command argument completers that
/// aren't `Clone`.
#[derive(Clone)]
struct FsConfig {
    base_path: PathBuf,
    /// Cap on matching candidates collected by the walker before
    /// scoring. Matches are filtered inline inside the walk callback,
    /// so this bounds the number of files that pass the query test —
    /// not the number walked. A repo of any size can be traversed
    /// safely; only the match count is bounded.
    walker_limit: usize,
    /// Cap on suggestions returned to the caller after scoring.
    suggestion_limit: usize,
}

impl CombinedAutocompleteProvider {
    /// Create a provider with the given slash commands and working
    /// directory.
    pub fn new(commands: Vec<CommandEntry>, base_path: impl Into<PathBuf>) -> Self {
        Self {
            commands,
            fs: FsConfig {
                base_path: base_path.into(),
                walker_limit: 100,
                suggestion_limit: 20,
            },
        }
    }

    fn clone_config(&self) -> FsConfig {
        self.fs.clone()
    }

    /// Whether the caller should trigger file completion at this cursor
    /// position. Used by the editor to decide whether to show suggestions
    /// on Tab. Returns `false` for top-level slash commands (so Tab inside
    /// `/mo` doesn't accidentally open the file picker).
    pub fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        let current = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        let before = safe_slice(current, 0, cursor_col);
        let trimmed = before.trim_start();
        !(trimmed.starts_with('/') && !trimmed.contains(' '))
    }

    // -- Prefix extraction --

    /// Extract an `@`-prefixed file-attach token ending at the cursor, if
    /// any.
    fn extract_at_prefix(&self, text: &str) -> Option<String> {
        if let Some(quoted) = extract_quoted_prefix(text)
            && quoted.starts_with("@\"")
        {
            return Some(quoted);
        }

        let last_delim = find_last_delimiter(text);
        let token_start = last_delim.map_or(0, |i| i + 1);
        let token = &text[token_start..];
        if token.starts_with('@') {
            Some(token.to_string())
        } else {
            None
        }
    }

    /// Extract a path-like prefix ending at the cursor. Returns `Some("")`
    /// when the cursor is positioned such that file completion should be
    /// offered (e.g. after a space with `force = true`).
    fn extract_path_prefix(&self, text: &str, force: bool) -> Option<String> {
        if let Some(quoted) = extract_quoted_prefix(text) {
            return Some(quoted);
        }

        let last_delim = find_last_delimiter(text);
        let path_prefix = match last_delim {
            Some(i) => &text[i + 1..],
            None => text,
        };

        if force {
            return Some(path_prefix.to_string());
        }

        if path_prefix.contains('/')
            || path_prefix.starts_with('.')
            || path_prefix.starts_with("~/")
        {
            return Some(path_prefix.to_string());
        }

        if path_prefix.is_empty() && text.ends_with(' ') {
            return Some(String::new());
        }

        None
    }

    // -- Completion producers --

    fn slash_command_suggestions(&self, prefix: &str) -> Option<AutocompleteSuggestions> {
        let items: Vec<CommandSuggestionBuild> = self
            .commands
            .iter()
            .map(|entry| CommandSuggestionBuild {
                name: entry.name().to_string(),
                description: entry.description().map(str::to_string),
                argument_hint: entry.argument_hint().map(str::to_string),
            })
            .collect();

        let filtered = fuzzy_filter(items, prefix, |item| item.name.as_str());
        if filtered.is_empty() {
            return None;
        }

        let items = filtered
            .into_iter()
            .map(|item| {
                let description = match (item.argument_hint, item.description) {
                    (Some(hint), Some(desc)) if !desc.is_empty() => {
                        Some(format!("{hint} — {desc}"))
                    }
                    (Some(hint), _) => Some(hint),
                    (None, Some(desc)) if !desc.is_empty() => Some(desc),
                    _ => None,
                };
                AutocompleteItem {
                    value: item.name.clone(),
                    label: item.name,
                    description,
                }
            })
            .collect();

        Some(AutocompleteSuggestions {
            items,
            prefix: format!("/{prefix}"),
        })
    }
}

impl FsConfig {
    // -- Filesystem resolution --

    fn expand_home(&self, path: &str) -> PathBuf {
        if path == "~" {
            return home_dir().unwrap_or_else(|| PathBuf::from(path));
        }
        if let Some(rest) = path.strip_prefix("~/") {
            let mut home = home_dir().unwrap_or_else(|| PathBuf::from(path));
            // Strip any trailing '/' from rest to avoid double separators.
            let trimmed_rest = rest.trim_end_matches('/');
            if !trimmed_rest.is_empty() {
                home.push(trimmed_rest);
            }
            let mut out = home.to_string_lossy().to_string();
            if path.ends_with('/') && !out.ends_with('/') {
                out.push('/');
            }
            return PathBuf::from(out);
        }
        PathBuf::from(path)
    }

    fn file_suggestions(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let parsed = parse_path_prefix(prefix);
        let mut expanded_prefix = parsed.raw_prefix.clone();
        if expanded_prefix.starts_with('~') {
            expanded_prefix = self
                .expand_home(&expanded_prefix)
                .to_string_lossy()
                .into_owned();
        }

        let raw_prefix = parsed.raw_prefix.as_str();
        let is_root_prefix = matches!(raw_prefix, "" | "./" | "../" | "~" | "~/" | "/")
            || (parsed.is_at_prefix && raw_prefix.is_empty());

        let (search_dir, search_prefix): (PathBuf, String) = if is_root_prefix {
            let dir = if raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                PathBuf::from(&expanded_prefix)
            } else {
                self.base_path.join(&expanded_prefix)
            };
            (dir, String::new())
        } else if raw_prefix.ends_with('/') {
            let dir = if raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                PathBuf::from(&expanded_prefix)
            } else {
                self.base_path.join(&expanded_prefix)
            };
            (dir, String::new())
        } else {
            let path_ref = Path::new(&expanded_prefix);
            let dir_comp = path_ref.parent().unwrap_or_else(|| Path::new(""));
            let file_comp = path_ref
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let dir_buf = if raw_prefix.starts_with('~') || expanded_prefix.starts_with('/') {
                dir_comp.to_path_buf()
            } else {
                self.base_path.join(dir_comp)
            };
            (dir_buf, file_comp)
        };

        let Ok(entries) = std::fs::read_dir(&search_dir) else {
            return Vec::new();
        };

        let search_prefix_lower = search_prefix.to_lowercase();
        let mut suggestions = Vec::<AutocompleteItem>::new();

        for entry in entries.flatten() {
            // Cap the number of directory entries we process so a rogue
            // 100k-entry directory doesn't block the worker. Results are
            // sorted (directories first) after this loop; truncating
            // before sorting is fine for UX because the popup already
            // caps visible items and the user will narrow further with
            // more typed characters.
            if suggestions.len() >= DIRECT_PATH_ENTRY_CAP {
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.to_lowercase().starts_with(&search_prefix_lower) {
                continue;
            }

            // Resolve directory status, following symlinks so "symlink → dir"
            // still sorts with directories.
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let mut is_directory = file_type.is_dir();
            if !is_directory && file_type.is_symlink() {
                if let Ok(meta) = std::fs::metadata(entry.path()) {
                    is_directory = meta.is_dir();
                }
            }

            let display_prefix = raw_prefix;
            let relative = relative_for_display(display_prefix, &name);
            let relative = to_display_path(&relative);
            let path_value = if is_directory {
                format!("{relative}/")
            } else {
                relative
            };
            let value = build_completion_value(
                &path_value,
                is_directory,
                parsed.is_at_prefix,
                parsed.is_quoted_prefix,
            );

            suggestions.push(AutocompleteItem {
                value,
                label: if is_directory {
                    format!("{name}/")
                } else {
                    name
                },
                description: None,
            });
        }

        suggestions.sort_by(|a, b| {
            let a_dir = a.value.ends_with('/') || a.value.ends_with("/\"");
            let b_dir = b.value.ends_with('/') || b.value.ends_with("/\"");
            match (a_dir, b_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.label.cmp(&b.label),
            }
        });

        suggestions
    }

    fn fuzzy_file_suggestions(
        &self,
        query: &str,
        is_quoted_prefix: bool,
        cancel: &CancellationToken,
    ) -> Vec<AutocompleteItem> {
        // Always walk from the project base. The streaming session
        // path does the same: `match_paths()` scoring promotes hits
        // at path delimiter boundaries, so a prefix like
        // `src/aj-tui/foo` naturally ranks those paths above scattered
        // subsequence matches without needing a separate walker root.
        let base_dir = self.base_path.clone();

        let mut scored = walk_for_suggestions_parallel(&base_dir, query, self.walker_limit, cancel);
        if cancel.is_cancelled() {
            return Vec::new();
        }

        scored.sort_by(|a, b| b.1.cmp(&a.1));
        scored.truncate(self.suggestion_limit);

        let mut out = Vec::with_capacity(scored.len());
        for (entry, _) in scored {
            let path_without_slash = if entry.is_directory {
                entry.path_display.trim_end_matches('/').to_string()
            } else {
                entry.path_display.clone()
            };
            let display_path = path_without_slash.clone();
            let entry_name = Path::new(&path_without_slash)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path_without_slash.clone());
            let completion_path = if entry.is_directory {
                format!("{display_path}/")
            } else {
                display_path.clone()
            };
            let value = build_completion_value(
                &completion_path,
                entry.is_directory,
                true,
                is_quoted_prefix,
            );
            let label = if entry.is_directory {
                format!("{entry_name}/")
            } else {
                entry_name
            };
            out.push(AutocompleteItem {
                value,
                label,
                description: Some(display_path),
            });
        }

        out
    }
}

#[async_trait]
impl AutocompleteProvider for CombinedAutocompleteProvider {
    async fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        opts: SuggestOpts,
    ) -> Option<AutocompleteSuggestions> {
        let current = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        let before = safe_slice(current, 0, cursor_col);
        let force = opts.force;

        // 1. `@`-prefixed fuzzy file search. Runs the parallel `ignore`
        //    walk on a blocking worker so the UI task stays responsive
        //    and so cancellation drops out of every walker thread at
        //    once.
        if let Some(at_prefix) = self.extract_at_prefix(before) {
            let parsed = parse_path_prefix(&at_prefix);
            let provider = self.clone_config();
            let raw_prefix = parsed.raw_prefix;
            let is_quoted = parsed.is_quoted_prefix;
            let cancel = opts.cancel.clone();
            let suggestions = tokio::task::spawn_blocking(move || {
                provider.fuzzy_file_suggestions(&raw_prefix, is_quoted, &cancel)
            })
            .await
            .unwrap_or_default();
            if opts.cancel.is_cancelled() || suggestions.is_empty() {
                return None;
            }
            return Some(AutocompleteSuggestions {
                items: suggestions,
                prefix: at_prefix,
            });
        }

        // 2. Slash commands and their arguments. Purely in-memory —
        //    runs inline.
        if !force && before.starts_with('/') {
            let rest = &before[1..];
            match rest.find(' ') {
                None => {
                    // Still typing the command name.
                    let prefix = rest;
                    return self.slash_command_suggestions(prefix).map(|mut s| {
                        s.prefix = before.to_string();
                        s
                    });
                }
                Some(space_rel) => {
                    let command_name = &rest[..space_rel];
                    let argument = &rest[space_rel + 1..];
                    let cmd_match = self
                        .commands
                        .iter()
                        .find(|entry| entry.name() == command_name);
                    let items = match cmd_match {
                        Some(CommandEntry::Command(SlashCommand {
                            get_argument_completions: Some(f),
                            ..
                        })) => f(argument),
                        _ => return None,
                    };
                    if items.is_empty() {
                        return None;
                    }
                    return Some(AutocompleteSuggestions {
                        items,
                        prefix: argument.to_string(),
                    });
                }
            }
        }

        // 3. Direct path completion. A single `read_dir` is not usually
        //    a cancellation hot spot, but we still offload it to a
        //    blocking worker so we don't stall the UI on slow NFS or
        //    FUSE mounts.
        let path_match = self.extract_path_prefix(before, force)?;
        let provider = self.clone_config();
        let path_match_cloned = path_match.clone();
        let suggestions =
            tokio::task::spawn_blocking(move || provider.file_suggestions(&path_match_cloned))
                .await
                .unwrap_or_default();
        if opts.cancel.is_cancelled() || suggestions.is_empty() {
            return None;
        }
        Some(AutocompleteSuggestions {
            items: suggestions,
            prefix: path_match,
        })
    }

    fn try_start_session(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<Box<dyn AutocompleteSession>> {
        // Streaming only covers the `@`-fuzzy-file context. Slash
        // commands and direct path completion stay on the one-shot
        // `get_suggestions` path — their candidate sets are small
        // and already fast, so the streaming machinery would just
        // add bookkeeping for no win.
        let current = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        let before = safe_slice(current, 0, cursor_col);
        let at_prefix = self.extract_at_prefix(before)?;

        let parsed = parse_path_prefix(&at_prefix);
        if !parsed.is_at_prefix {
            return None;
        }

        // One walker rooted at the configured project base, regardless
        // of directory separators in the prefix. Nucleo's
        // `match_paths()` scoring already rewards hits at path
        // delimiter boundaries, so a prefix like `src/aj-tui/foo`
        // naturally promotes paths containing those segments in order.
        // This mirrors helix's file picker and removes the per-`/`
        // session invalidation dance the previous scoped model needed.
        let session = FuzzyFileSession::new(
            self.fs.base_path.clone(),
            at_prefix,
            parsed.raw_prefix,
            parsed.is_quoted_prefix,
            notify,
        );
        Some(Box::new(session))
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionApplied {
        let current = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        let split = cursor_col.saturating_sub(prefix.chars().count());
        let before_prefix = safe_slice(current, 0, split);
        let after_cursor_raw = safe_slice(current, cursor_col, current.chars().count());

        let is_quoted_prefix = prefix.starts_with('"') || prefix.starts_with("@\"");
        let has_leading_quote_after = after_cursor_raw.starts_with('"');
        let has_trailing_quote_in_item = item.value.ends_with('"');
        let after_cursor: &str =
            if is_quoted_prefix && has_trailing_quote_in_item && has_leading_quote_after {
                &after_cursor_raw[1..]
            } else {
                after_cursor_raw
            };

        let is_slash_command = prefix.starts_with('/')
            && before_prefix.trim().is_empty()
            && !prefix[1..].contains('/');
        if is_slash_command {
            let new_line = format!("{before_prefix}/{} {after_cursor}", item.value);
            let mut new_lines = lines.to_vec();
            new_lines[cursor_line] = new_line;
            return CompletionApplied {
                lines: new_lines,
                cursor_line,
                cursor_col: before_prefix.chars().count() + item.value.chars().count() + 2,
            };
        }

        if prefix.starts_with('@') {
            let is_directory = item.label.ends_with('/');
            let suffix = if is_directory { "" } else { " " };
            let new_line = format!("{before_prefix}{}{suffix}{after_cursor}", item.value);
            let has_trailing_quote = item.value.ends_with('"');
            let cursor_offset = if is_directory && has_trailing_quote {
                item.value.chars().count() - 1
            } else {
                item.value.chars().count()
            };
            let mut new_lines = lines.to_vec();
            new_lines[cursor_line] = new_line;
            return CompletionApplied {
                lines: new_lines,
                cursor_line,
                cursor_col: before_prefix.chars().count() + cursor_offset + suffix.chars().count(),
            };
        }

        // Command-argument context: `/cmd foo|` — detect by presence of
        // `/` and a space in the text before the cursor.
        let text_before_cursor = safe_slice(current, 0, cursor_col);
        if text_before_cursor.contains('/') && text_before_cursor.contains(' ') {
            let new_line = format!("{before_prefix}{}{after_cursor}", item.value);
            let is_directory = item.label.ends_with('/');
            let has_trailing_quote = item.value.ends_with('"');
            let cursor_offset = if is_directory && has_trailing_quote {
                item.value.chars().count() - 1
            } else {
                item.value.chars().count()
            };
            let mut new_lines = lines.to_vec();
            new_lines[cursor_line] = new_line;
            return CompletionApplied {
                lines: new_lines,
                cursor_line,
                cursor_col: before_prefix.chars().count() + cursor_offset,
            };
        }

        // Plain path completion.
        let new_line = format!("{before_prefix}{}{after_cursor}", item.value);
        let is_directory = item.label.ends_with('/');
        let has_trailing_quote = item.value.ends_with('"');
        let cursor_offset = if is_directory && has_trailing_quote {
            item.value.chars().count() - 1
        } else {
            item.value.chars().count()
        };
        let mut new_lines = lines.to_vec();
        new_lines[cursor_line] = new_line;
        CompletionApplied {
            lines: new_lines,
            cursor_line,
            cursor_col: before_prefix.chars().count() + cursor_offset,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Intermediate item used for scoring slash commands by name.
struct CommandSuggestionBuild {
    name: String,
    description: Option<String>,
    argument_hint: Option<String>,
}

#[derive(Debug, Clone)]
struct Entry {
    /// Display-style forward-slash path relative to the walk root.
    path_display: String,
    is_directory: bool,
}

struct ParsedPrefix {
    raw_prefix: String,
    is_at_prefix: bool,
    is_quoted_prefix: bool,
}

/// Byte-offset helpers that treat a `char`-indexed view of `s`.
fn safe_slice(s: &str, start: usize, end: usize) -> &str {
    let mut byte_start = s.len();
    let mut byte_end = s.len();
    for (idx, (byte_idx, _)) in s.char_indices().enumerate() {
        if idx == start {
            byte_start = byte_idx;
        }
        if idx == end {
            byte_end = byte_idx;
            break;
        }
    }
    if start == 0 {
        byte_start = 0;
    }
    if end <= start {
        return "";
    }
    &s[byte_start..byte_end]
}

fn to_display_path(value: &str) -> String {
    value.replace('\\', "/")
}

fn find_last_delimiter(text: &str) -> Option<usize> {
    text.char_indices()
        .rev()
        .find(|(_, c)| PATH_DELIMITERS.contains(c))
        .map(|(i, _)| i)
}

fn find_unclosed_quote_start(text: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut quote_start = None;
    for (i, c) in text.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
            if in_quotes {
                quote_start = Some(i);
            }
        }
    }
    if in_quotes { quote_start } else { None }
}

fn is_token_start(text: &str, index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let prev = text[..index].chars().next_back();
    prev.map_or(true, |c| PATH_DELIMITERS.contains(&c))
}

fn extract_quoted_prefix(text: &str) -> Option<String> {
    let quote_start = find_unclosed_quote_start(text)?;

    // `@"foo` — the `@` just before the quote binds to the prefix.
    if quote_start > 0 {
        let before = &text[..quote_start];
        if before.ends_with('@') {
            let at_index = quote_start - 1;
            if is_token_start(text, at_index) {
                return Some(text[at_index..].to_string());
            }
            return None;
        }
    }

    if !is_token_start(text, quote_start) {
        return None;
    }
    Some(text[quote_start..].to_string())
}

fn parse_path_prefix(prefix: &str) -> ParsedPrefix {
    if let Some(rest) = prefix.strip_prefix("@\"") {
        return ParsedPrefix {
            raw_prefix: rest.to_string(),
            is_at_prefix: true,
            is_quoted_prefix: true,
        };
    }
    if let Some(rest) = prefix.strip_prefix('"') {
        return ParsedPrefix {
            raw_prefix: rest.to_string(),
            is_at_prefix: false,
            is_quoted_prefix: true,
        };
    }
    if let Some(rest) = prefix.strip_prefix('@') {
        return ParsedPrefix {
            raw_prefix: rest.to_string(),
            is_at_prefix: true,
            is_quoted_prefix: false,
        };
    }
    ParsedPrefix {
        raw_prefix: prefix.to_string(),
        is_at_prefix: false,
        is_quoted_prefix: false,
    }
}

fn build_completion_value(
    path: &str,
    _is_directory: bool,
    is_at_prefix: bool,
    is_quoted_prefix: bool,
) -> String {
    let needs_quotes = is_quoted_prefix || path.contains(' ');
    let prefix = if is_at_prefix { "@" } else { "" };
    if !needs_quotes {
        return format!("{prefix}{path}");
    }
    format!("{prefix}\"{path}\"")
}

fn relative_for_display(display_prefix: &str, name: &str) -> String {
    if display_prefix.ends_with('/') {
        return format!("{display_prefix}{name}");
    }
    if display_prefix.contains('/') || display_prefix.contains('\\') {
        if let Some(rest) = display_prefix.strip_prefix("~/") {
            let parent = Path::new(rest).parent().unwrap_or_else(|| Path::new(""));
            if parent.as_os_str().is_empty() || parent == Path::new(".") {
                return format!("~/{name}");
            }
            let mut out = PathBuf::from(parent);
            out.push(name);
            return format!("~/{}", out.to_string_lossy());
        }
        if let Some(rest) = display_prefix.strip_prefix('/') {
            let parent = Path::new(rest).parent().unwrap_or_else(|| Path::new(""));
            if parent.as_os_str().is_empty() {
                return format!("/{name}");
            }
            return format!("/{}/{}", parent.to_string_lossy(), name);
        }
        let parent = Path::new(display_prefix)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let mut joined = PathBuf::from(parent);
        joined.push(name);
        let mut rel = joined.to_string_lossy().to_string();
        if display_prefix.starts_with("./") && !rel.starts_with("./") {
            rel = format!("./{rel}");
        }
        return rel;
    }
    if display_prefix.starts_with('~') {
        return format!("~/{name}");
    }
    name.to_string()
}

fn home_dir() -> Option<PathBuf> {
    // `std::env::home_dir` was un-deprecated in 1.86; use it directly to
    // avoid pulling in `dirs` for one lookup.
    #[allow(deprecated)]
    {
        std::env::home_dir()
    }
}

fn score_entry(file_path: &str, query: &str, is_directory: bool) -> u32 {
    let file_name = Path::new(file_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lower_file = file_name.to_lowercase();
    let lower_query = query.to_lowercase();
    let lower_path = file_path.to_lowercase();

    let mut score = if lower_file == lower_query {
        100
    } else if lower_file.starts_with(&lower_query) {
        80
    } else if lower_file.contains(&lower_query) {
        50
    } else if lower_path.contains(&lower_query) {
        30
    } else {
        0
    };

    if is_directory && score > 0 {
        score += 10;
    }
    score
}

/// Walk `root` in parallel, scoring each visited entry against `query`
/// and collecting up to `max_results` matching entries.
///
/// This is the hot path for `@`-fuzzy file search on very large trees:
/// we use [`ignore::WalkBuilder::build_parallel`] (the same engine
/// `ripgrep` and `fd` use) so the walk fans out across CPU cores.
///
/// Scoring and filtering happen inside the walk callback, not in a
/// post-pass. That matters because `build_parallel` returns entries
/// in a non-deterministic order: capping the raw walk at N and *then*
/// filtering would discard every entry after the Nth — including any
/// matches that happened to be visited late. Put differently: with
/// a 100-entry walk cap and 150 files in the tree, a query that only
/// matches files #120-#150 would silently return nothing.
///
/// Instead, each worker computes the score inline, skips non-matches
/// (without consuming a "slot" in the result cap), and records
/// `(Entry, score)` for matches. The cap applies to matches, not to
/// walked entries, so the walker runs to completion on any repo that
/// fits in memory.
///
/// When `query` is empty, every entry scores 1 — the caller (an empty
/// `@` trigger that wants "show me every file") still gets the same
/// paged view it used to.
///
/// Cancellation and the result cap are both honored via
/// [`ignore::WalkState::Quit`]: the visitor checks `cancel` and the
/// atomic `taken` count at the top of every callback. Because
/// `build_parallel()` runs visitors on its own thread pool, a single
/// `Quit` return from any worker stops the entire walk promptly.
///
/// Because results arrive in non-deterministic order, the caller is
/// expected to sort them by score before presenting. The result cap
/// may be slightly exceeded if multiple workers race on the increment;
/// this is fine because the caller truncates to its own tighter
/// suggestion cap after scoring.
fn walk_for_suggestions_parallel(
    root: &Path,
    query: &str,
    max_results: usize,
    cancel: &CancellationToken,
) -> Vec<(Entry, u32)> {
    if cancel.is_cancelled() {
        return Vec::new();
    }
    let walker = WalkBuilder::new(root)
        .hidden(false) // include dotfiles (matches `fd --hidden`)
        .git_ignore(true)
        .ignore(true)
        .parents(true)
        .build_parallel();

    let out = Arc::new(std::sync::Mutex::new(Vec::<(Entry, u32)>::with_capacity(
        max_results,
    )));
    let taken = Arc::new(AtomicUsize::new(0));
    let root_buf = root.to_path_buf();
    let query = query.to_string();

    walker.run(|| {
        let out = Arc::clone(&out);
        let taken = Arc::clone(&taken);
        let cancel = cancel.clone();
        let root_buf = root_buf.clone();
        let query = query.clone();
        Box::new(move |result| {
            // Fast path: if we've already hit the cap or been
            // cancelled, stop every worker.
            if cancel.is_cancelled() || taken.load(Ordering::Relaxed) >= max_results {
                return WalkState::Quit;
            }
            let dir_entry = match result {
                Ok(d) => d,
                Err(_) => return WalkState::Continue,
            };
            let path = dir_entry.path();
            if path == root_buf.as_path() {
                return WalkState::Continue;
            }
            if path_has_git_component(path) {
                // Skip the whole `.git` subtree, not just the entry.
                return WalkState::Skip;
            }
            let rel = match path.strip_prefix(&root_buf) {
                Ok(r) => r,
                Err(_) => return WalkState::Continue,
            };
            let is_directory = dir_entry.file_type().is_some_and(|t| t.is_dir());
            let mut display = to_display_path(&rel.to_string_lossy());
            if is_directory && !display.ends_with('/') {
                display.push('/');
            }

            // Score inline. A miss walks on without consuming a slot
            // so the walker reaches entries that a blind cap would
            // have discarded.
            let score = if query.is_empty() {
                1
            } else {
                score_entry(&display, &query, is_directory)
            };
            if score == 0 {
                return WalkState::Continue;
            }

            // Reserve a slot via the shared counter before touching
            // the vec. If we overshoot (another worker beat us to the
            // last slot), quit gracefully rather than pushing past
            // the cap.
            let idx = taken.fetch_add(1, Ordering::Relaxed);
            if idx >= max_results {
                return WalkState::Quit;
            }

            if let Ok(mut locked) = out.lock() {
                locked.push((
                    Entry {
                        path_display: display,
                        is_directory,
                    },
                    score,
                ));
            }
            WalkState::Continue
        })
    });

    Arc::try_unwrap(out)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_else(|arc| {
            // Some worker is still holding a clone of the Arc — fall
            // back to locking and cloning. This is extremely unlikely
            // in practice because `walker.run()` joins all worker
            // threads before returning.
            arc.lock().map(|g| g.clone()).unwrap_or_default()
        })
}

fn path_has_git_component(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(os) => os == ".git",
        _ => false,
    })
}

// ---------------------------------------------------------------------------
// Streaming `@`-fuzzy session
// ---------------------------------------------------------------------------

/// One entry stored in the nucleo matcher. The `path` is the full
/// display-style relative path (e.g. `"src/autocomplete.rs"` or
/// `"src/aj-tui/"`) and is what nucleo fuzzy-matches the user's
/// needle against. `is_directory` is retained so the snapshot
/// builder can format labels and values correctly.
#[derive(Clone)]
struct FileEntry {
    path: String,
    is_directory: bool,
}

/// Cap on the number of matches returned to the UI per tick.
const FUZZY_SESSION_SUGGESTION_LIMIT: usize = 20;

/// Streaming session for `@`-prefixed fuzzy file search.
///
/// Owns:
///
/// - a [`nucleo::Nucleo`] worker pool that absorbs newly-injected
///   entries and re-scores the current pattern on its own threads;
/// - a [`tokio::task::JoinHandle`] for the background walker that
///   feeds the matcher;
/// - a [`CancellationToken`] wired into the walker — dropping the
///   session cancels the walker promptly.
///
/// The session lives for the duration of one `@`-popup: it's created
/// when the user first types `@` and dropped when the popup closes.
///
/// # Invalidation
///
/// The walker's root is fixed at construction time (the provider's
/// `base_path`) and the only things that retire a session are:
///
/// - the user leaving the `@`-context entirely (cursor moves before
///   the `@`, the `@` itself is deleted, etc.);
/// - the quoted/unquoted shape of the token flipping.
///
/// Typing `/` inside the prefix does **not** invalidate the session —
/// the new characters are folded into the nucleo pattern, and
/// `match_paths()` scoring takes care of promoting matches at path
/// delimiter boundaries.
pub struct FuzzyFileSession {
    /// Nucleo worker. Single column matcher — the full relative
    /// path is the haystack.
    matcher: nucleo::Nucleo<FileEntry>,
    /// Cancel handle for the walker task. Fires on drop.
    cancel: CancellationToken,
    /// The full `@`-prefixed token the session will replace when
    /// a suggestion is applied. Tracked across
    /// [`Self::update`] calls so [`apply_completion`] knows how
    /// many chars to consume.
    at_prefix: String,
    /// Whether the prefix started with `@"` (drives value quoting
    /// in [`Self::snapshot`]).
    is_quoted_prefix: bool,
    /// Last pattern string the session handed to nucleo. Used to
    /// detect the `is_append` optimization where the new pattern
    /// extends the old one (typing more characters) and nucleo
    /// can skip items that already failed.
    last_pattern: String,
}

impl FuzzyFileSession {
    /// Open a new streaming session walking from `base_path`
    /// (possibly scoped by `display_base`) and matching the initial
    /// `query` across the columns of injected entries.
    ///
    /// `at_prefix` is the full token the editor will replace on
    /// selection; `is_quoted_prefix` signals whether the user's
    /// prefix started with `@"` so the formatter emits matching
    /// quotes.
    ///
    /// `notify` fires whenever nucleo has new information — the
    /// editor hooks it up to its render-wake channel so the popup
    /// redraws live as items stream in.
    fn new(
        base_path: PathBuf,
        at_prefix: String,
        query: String,
        is_quoted_prefix: bool,
        notify: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let cancel = CancellationToken::new();

        // Single-column matcher. The column holds the full display
        // path so nucleo's fuzzy algorithm sees both the file name
        // and any parent-directory segments.
        let mut matcher = nucleo::Nucleo::<FileEntry>::new(
            nucleo::Config::DEFAULT.match_paths(),
            notify,
            None, // default thread count: one per hardware thread
            1,
        );

        // Seed the pattern from the initial typed query so matches
        // are already filtered when the user sees the first
        // snapshot. `is_append=false` because there's no previous
        // pattern state.
        matcher.pattern.reparse(
            0,
            &query,
            nucleo::pattern::CaseMatching::Smart,
            nucleo::pattern::Normalization::Smart,
            false,
        );

        let walker_task = spawn_walker_task(base_path, matcher.injector(), cancel.clone());
        // Dropping the JoinHandle does not abort the task. We rely
        // on the cancel token instead, which the session's `Drop`
        // trips. Detaching keeps the session `!Sync` obligation-
        // free for callers.
        drop(walker_task);

        Self {
            matcher,
            cancel,
            at_prefix,
            is_quoted_prefix,
            last_pattern: query,
        }
    }

    /// Build an [`AutocompleteItem`] for a single match, applying
    /// the same formatting rules as the one-shot code path so
    /// snapshots look identical whether they came from
    /// [`CombinedAutocompleteProvider::get_suggestions`] or from a
    /// streaming session.
    fn item_from_entry(&self, entry: &FileEntry) -> AutocompleteItem {
        let path_without_slash = if entry.is_directory {
            entry.path.trim_end_matches('/').to_string()
        } else {
            entry.path.clone()
        };
        let display_path = path_without_slash.clone();
        let entry_name = Path::new(&path_without_slash)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_without_slash.clone());
        let completion_path = if entry.is_directory {
            format!("{display_path}/")
        } else {
            display_path.clone()
        };
        let value = build_completion_value(
            &completion_path,
            entry.is_directory,
            true,
            self.is_quoted_prefix,
        );
        let label = if entry.is_directory {
            format!("{entry_name}/")
        } else {
            entry_name
        };
        AutocompleteItem {
            value,
            label,
            description: Some(display_path),
        }
    }
}

impl Drop for FuzzyFileSession {
    fn drop(&mut self) {
        // Tell the walker task to bail out of its loop. The nucleo
        // worker pool is stopped implicitly when `self.matcher`
        // drops.
        self.cancel.cancel();
    }
}

impl AutocompleteSession for FuzzyFileSession {
    fn prefix(&self) -> &str {
        &self.at_prefix
    }

    fn update(
        &mut self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> Result<(), SessionInvalid> {
        // Re-extract the at-prefix from the current cursor position.
        // If we're no longer in an `@`-context or the quoted/raw
        // shape flipped, the session can't serve the new state — hand
        // control back to the editor so it can restart us. Slash
        // characters inside the prefix no longer invalidate: the
        // walker is rooted at the project base once for the session's
        // lifetime, and nucleo absorbs the updated pattern in place.
        let current = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        let before = safe_slice(current, 0, cursor_col);

        let new_at_prefix = extract_at_prefix_from_text(before).ok_or(SessionInvalid)?;
        let parsed = parse_path_prefix(&new_at_prefix);
        if !parsed.is_at_prefix || parsed.is_quoted_prefix != self.is_quoted_prefix {
            return Err(SessionInvalid);
        }

        let new_query = parsed.raw_prefix.clone();
        let is_append = new_query.starts_with(&self.last_pattern);
        self.matcher.pattern.reparse(
            0,
            &new_query,
            nucleo::pattern::CaseMatching::Smart,
            nucleo::pattern::Normalization::Smart,
            is_append,
        );

        self.at_prefix = new_at_prefix;
        self.last_pattern = new_query;
        Ok(())
    }

    fn tick(&mut self, budget_ms: u64) -> SessionStatus {
        let status = self.matcher.tick(budget_ms);
        // `status.running` from nucleo means the matcher is still
        // absorbing queued work *or* there are active injectors
        // (the walker is still running). Either way the caller
        // should schedule another tick.
        SessionStatus {
            changed: status.changed,
            running: status.running || self.matcher.active_injectors() > 0,
        }
    }

    fn snapshot(&mut self) -> Vec<AutocompleteItem> {
        // Nucleo's `matched_items` iterator yields matches in score
        // order (descending), and its `match_paths()` config
        // already rewards matches at path-delimiter boundaries —
        // so a query like `src` naturally scores `src/` above a
        // file like `src.txt`, and a query like `auto` scores
        // `autocomplete.rs` above a scattered subsequence match in
        // `tests/support/`. We trust nucleo's ranking and just
        // cap to the popup's display window.
        let snap = self.matcher.snapshot();
        let take = u32::try_from(FUZZY_SESSION_SUGGESTION_LIMIT).unwrap_or(u32::MAX);
        let end = snap.matched_item_count().min(take);

        snap.matched_items(..end)
            .map(|item| self.item_from_entry(item.data))
            .collect()
    }
}

/// Find the `@`-prefixed token ending at the cursor, if any. Shared
/// between the session constructor and [`FuzzyFileSession::update`]
/// so both use the same tokenization rules.
fn extract_at_prefix_from_text(text: &str) -> Option<String> {
    if let Some(quoted) = extract_quoted_prefix(text)
        && quoted.starts_with("@\"")
    {
        return Some(quoted);
    }
    let last_delim = find_last_delimiter(text);
    let token_start = last_delim.map_or(0, |i| i + 1);
    let token = &text[token_start..];
    if token.starts_with('@') {
        Some(token.to_string())
    } else {
        None
    }
}

/// Spawn the background walker task that feeds the session's nucleo
/// injector.
///
/// Matches the helix file-picker architecture: a **sequential**
/// walker (not `build_parallel`) pushes each entry into the
/// thread-safe injector as it's discovered. Running the walker
/// sequentially pairs naturally with nucleo's own worker pool —
/// parallelising the walk too would oversubscribe the CPU without
/// helping latency, because most filesystems serialise directory
/// reads anyway.
fn spawn_walker_task(
    base_dir: PathBuf,
    injector: nucleo::Injector<FileEntry>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let walker = WalkBuilder::new(&base_dir)
            .hidden(false)
            .git_ignore(true)
            .ignore(true)
            .parents(true)
            .filter_entry(|entry| entry.file_name() != ".git")
            .build();

        for result in walker {
            if cancel.is_cancelled() {
                return;
            }
            let Ok(dir_entry) = result else {
                continue;
            };
            let path = dir_entry.path();
            if path == base_dir {
                continue;
            }
            if path_has_git_component(path) {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&base_dir) else {
                continue;
            };
            let is_directory = dir_entry.file_type().is_some_and(|t| t.is_dir());
            let mut display = to_display_path(&rel.to_string_lossy());
            if is_directory && !display.ends_with('/') {
                display.push('/');
            }
            let display_for_match = display.clone();
            injector.push(
                FileEntry {
                    path: display,
                    is_directory,
                },
                |_entry, cols| {
                    cols[0] = nucleo::Utf32String::from(display_for_match.as_str());
                },
            );
        }
    })
}

// ---------------------------------------------------------------------------
// Tests for small helpers that don't need a temp filesystem
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_prefix() {
        let p = parse_path_prefix("src/");
        assert_eq!(p.raw_prefix, "src/");
        assert!(!p.is_at_prefix);
        assert!(!p.is_quoted_prefix);
    }

    #[test]
    fn parses_at_prefix() {
        let p = parse_path_prefix("@foo");
        assert_eq!(p.raw_prefix, "foo");
        assert!(p.is_at_prefix);
        assert!(!p.is_quoted_prefix);
    }

    #[test]
    fn parses_quoted_prefix() {
        let p = parse_path_prefix("\"my folder/");
        assert_eq!(p.raw_prefix, "my folder/");
        assert!(!p.is_at_prefix);
        assert!(p.is_quoted_prefix);
    }

    #[test]
    fn parses_at_quoted_prefix() {
        let p = parse_path_prefix("@\"my folder/");
        assert_eq!(p.raw_prefix, "my folder/");
        assert!(p.is_at_prefix);
        assert!(p.is_quoted_prefix);
    }

    #[test]
    fn builds_completion_value_for_plain_path() {
        assert_eq!(
            build_completion_value("src/main.rs", false, false, false),
            "src/main.rs"
        );
    }

    #[test]
    fn builds_completion_value_with_at_prefix() {
        assert_eq!(
            build_completion_value("src/main.rs", false, true, false),
            "@src/main.rs"
        );
    }

    #[test]
    fn builds_completion_value_quotes_when_path_has_spaces() {
        assert_eq!(
            build_completion_value("my folder/", true, false, false),
            "\"my folder/\""
        );
    }

    #[test]
    fn builds_completion_value_quotes_when_prefix_is_quoted() {
        assert_eq!(
            build_completion_value("plain.txt", false, false, true),
            "\"plain.txt\""
        );
    }

    #[test]
    fn finds_last_delimiter_at_last_space() {
        assert_eq!(find_last_delimiter("hey foo"), Some(3));
        assert_eq!(find_last_delimiter("abc"), None);
    }

    #[test]
    fn finds_unclosed_quote_when_trailing() {
        assert_eq!(find_unclosed_quote_start("hello \"world"), Some(6));
        assert_eq!(find_unclosed_quote_start("\"closed\""), None);
    }
}
