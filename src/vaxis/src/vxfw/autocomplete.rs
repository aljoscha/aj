//! The generic autocomplete mechanism for [`TextArea`](crate::vxfw::TextArea):
//! the provider traits and the data types they exchange.
//!
//! This module carries no application knowledge. It knows nothing about files,
//! commands, or fuzzy matching. It defines the contract a concrete provider
//! implements and the shapes a completion request and its result take. The
//! widget owns the popup, the trigger detection, and the async pipeline. A
//! provider owns "what completes here and how".
//!
//! # Two shapes: one-shot vs. streaming
//!
//! A provider exposes two paths, and the widget picks one per request:
//!
//! - [`AutocompleteProvider::get_suggestions`] is the one-shot path. The
//!   provider runs its work as a single future to completion and returns a
//!   finalized [`AutocompleteSuggestions`]. This fits closed, in-memory
//!   candidate sets: a fixed keyword list, a single directory read, and so on.
//! - [`AutocompleteProvider::try_start_session`] is the streaming path. The
//!   provider returns an [`AutocompleteSession`] whose matcher produces results
//!   incrementally, typically by feeding a background worker into a running
//!   matcher. The widget then drives the session with
//!   [`AutocompleteSession::update`] on each keystroke and reads results from
//!   [`AutocompleteSession::snapshot`] on each pump. This fits open candidate
//!   sets that are expensive to gather but cheap to re-match against a growing
//!   needle.
//!
//! A provider may implement both. The widget calls `try_start_session` first
//! and falls back to `get_suggestions` when it returns `None`. A provider that
//! only cares about the one-shot path leaves `try_start_session` at its default.
//!
//! # Async and cancellation
//!
//! [`AutocompleteProvider::get_suggestions`] is async because a provider may do
//! significant work (a filesystem walk, a remote lookup). It takes a
//! [`tokio_util::sync::CancellationToken`] through [`SuggestOpts`]. A provider
//! that does more than a few microseconds of work must honor the token
//! promptly. Cancellation is best-effort: once the token fires, a provider may
//! return partial results or `None`.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// A single completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    /// Text inserted into the buffer when this item is applied.
    pub value: String,
    /// Short human-readable label shown in the suggestion list.
    pub label: String,
    /// Optional secondary text, such as a fuller path or a hint. How a widget
    /// presents it (beside the label, on its own, or not at all) is the
    /// widget's choice.
    pub description: Option<String>,
}

impl AutocompleteItem {
    /// Builds an item from its `value` (inserted on apply) and `label` (shown).
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    /// Adds optional secondary text, such as a fuller path or a hint.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// The result of a successful suggestion request.
#[derive(Debug, Clone)]
pub struct AutocompleteSuggestions {
    /// Ranked candidates, most relevant first.
    pub items: Vec<AutocompleteItem>,
    /// The substring of input the widget considers "already typed".
    /// [`AutocompleteProvider::apply_completion`] replaces exactly
    /// `prefix.len()` characters ending at the cursor.
    pub prefix: String,
}

/// The lines and cursor state returned by
/// [`AutocompleteProvider::apply_completion`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionApplied {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

/// Options passed to [`AutocompleteProvider::get_suggestions`].
pub struct SuggestOpts {
    /// Cancellation token. Honored by providers that do any work that may run
    /// for more than a few microseconds. Cancellation is best-effort: the
    /// provider may return partial results or `None` once the token fires.
    pub cancel: CancellationToken,
    /// `true` when the caller explicitly asked for suggestions (a Tab press),
    /// so the provider should be more eager. For example, it may return an
    /// empty-prefix suggestion list for the current context.
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

/// A completion backend: what completes at a cursor position, and how to splice
/// a chosen candidate back into the buffer.
///
/// Held by the widget as `Arc<dyn AutocompleteProvider>` because the widget
/// hands a cloned reference to every spawned worker task. `Send + Sync` is what
/// makes that share safe across threads.
///
/// See the module docs for the one-shot vs. streaming distinction and the
/// cancellation contract.
#[async_trait]
pub trait AutocompleteProvider: Send + Sync {
    /// Computes the suggestion list for the given cursor position. Returns
    /// `None` when no completion is appropriate: no prefix match, an empty
    /// candidate set, a cancelled request.
    async fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        opts: SuggestOpts,
    ) -> Option<AutocompleteSuggestions>;

    /// Splices the selected item's `value` into `lines` at the cursor,
    /// replacing exactly `prefix` characters before the cursor.
    ///
    /// Synchronous by design: a pure in-memory string operation that runs on
    /// the UI thread between keystrokes.
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionApplied;

    /// Tries to open a streaming [`AutocompleteSession`] for the current cursor
    /// context.
    ///
    /// A provider that can serve the context incrementally returns
    /// `Some(session)`. The widget then bypasses [`Self::get_suggestions`] for
    /// this context, driving the session via [`AutocompleteSession::update`] on
    /// keystrokes and polling [`AutocompleteSession::tick`] /
    /// [`AutocompleteSession::snapshot`] on each pump.
    ///
    /// Returning `None` (the default) signals that the provider has nothing
    /// streaming to offer here, and the widget falls back to the one-shot path.
    ///
    /// `notify` is a callback the session invokes from its worker threads
    /// whenever new information is available. The widget wires it to a wake so
    /// the popup refreshes live as results stream in.
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

    /// Whether the widget should fire a completion request at this cursor
    /// position when the user explicitly asks for one (Tab).
    ///
    /// Defaults to `true`. A provider that stacks or extends another can
    /// override it to suppress the popup in contexts it owns.
    fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        let _ = (lines, cursor_line, cursor_col);
        true
    }
}

/// A streaming source of completion candidates.
///
/// The widget owns one of these when a provider hands back a streaming context
/// from [`AutocompleteProvider::try_start_session`]. The session is the single
/// place incremental work lives: it holds its own matcher, its own background
/// worker, and any cancellation state. Dropping the session stops that work.
///
/// # Lifecycle
///
/// 1. `try_start_session` constructs the session and hands it to the widget.
/// 2. Per keystroke inside the trigger context, the widget calls
///    [`Self::update`] with the new cursor position. The session either absorbs
///    the change (the user is narrowing) or reports [`SessionInvalid`], in
///    which case the widget drops it and starts a fresh one.
/// 3. Per pump, the widget calls [`Self::tick`] with a short time budget, then
///    reads the current match list via [`Self::snapshot`].
/// 4. When the popup closes, the widget drops the session, cancelling any
///    in-flight work.
pub trait AutocompleteSession: Send {
    /// Substring of the current line that
    /// [`AutocompleteProvider::apply_completion`] will replace when a
    /// suggestion is chosen. Tracks the user's typed token across
    /// [`Self::update`] calls.
    fn prefix(&self) -> &str;

    /// Informs the session of a new cursor position within the same trigger
    /// context. Returns `Ok` when the session absorbed the change and
    /// `Err(SessionInvalid)` when it cannot, in which case the widget drops the
    /// session and opens a new one.
    fn update(
        &mut self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> Result<(), SessionInvalid>;

    /// Pumps the session's matcher / worker state for up to `budget_ms`
    /// milliseconds. Returns a [`SessionStatus`] the widget uses to decide
    /// whether to rebuild its displayed list and whether to expect more
    /// updates.
    fn tick(&mut self, budget_ms: u64) -> SessionStatus;

    /// Current top-ranked matches, a bounded list suitable to feed the popup.
    /// Read after [`Self::tick`] reports `changed`, or when the widget first
    /// attaches the session.
    fn snapshot(&mut self) -> Vec<AutocompleteItem>;
}

/// Outcome of [`AutocompleteSession::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStatus {
    /// `true` when the match snapshot is newer than the one the caller last
    /// read. The caller should re-fetch via [`AutocompleteSession::snapshot`].
    pub changed: bool,
    /// `true` when the worker is still producing items or the matcher is still
    /// churning. The caller should schedule another tick. A stable `false`
    /// means nothing changes without a new [`AutocompleteSession::update`].
    pub running: bool,
}

/// Marker returned by [`AutocompleteSession::update`] when the session can no
/// longer serve the new context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionInvalid;

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed-list, synchronous stub provider: no filesystem, no fuzzy
    /// matching. It matches a leading-`@` token against an in-memory candidate
    /// list and splices a chosen candidate over that token. Enough to exercise
    /// the trait contract without any app dependency.
    struct StubProvider {
        candidates: Vec<&'static str>,
    }

    impl StubProvider {
        /// The `@`-token ending at the cursor, if the cursor sits inside one.
        fn at_token(lines: &[String], cursor_line: usize, cursor_col: usize) -> Option<String> {
            let before = &lines[cursor_line][..cursor_col];
            let at = before.rfind('@')?;
            let token = &before[at..];
            if token.contains(char::is_whitespace) {
                return None;
            }
            Some(token.to_string())
        }
    }

    #[async_trait]
    impl AutocompleteProvider for StubProvider {
        async fn get_suggestions(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            _opts: SuggestOpts,
        ) -> Option<AutocompleteSuggestions> {
            let token = Self::at_token(lines, cursor_line, cursor_col)?;
            let needle = &token[1..];
            let items: Vec<AutocompleteItem> = self
                .candidates
                .iter()
                .filter(|c| c.starts_with(needle))
                .map(|c| AutocompleteItem::new(format!("@{c}"), *c))
                .collect();
            if items.is_empty() {
                return None;
            }
            Some(AutocompleteSuggestions {
                items,
                prefix: token,
            })
        }

        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> CompletionApplied {
            let line = &lines[cursor_line];
            let split = cursor_col - prefix.len();
            let mut new_lines = lines.to_vec();
            new_lines[cursor_line] =
                format!("{}{}{}", &line[..split], item.value, &line[cursor_col..]);
            CompletionApplied {
                lines: new_lines,
                cursor_line,
                cursor_col: split + item.value.len(),
            }
        }
    }

    // The per-module "doctest" the framework meta-test enforces. Exercises the
    // one-shot request path and the apply splice through the stub provider.
    #[tokio::test]
    async fn autocomplete() {
        let provider = StubProvider {
            candidates: vec!["readme.md", "readline.rs"],
        };
        let lines = vec!["see @read".to_string()];

        let suggestions = provider
            .get_suggestions(&lines, 0, lines[0].len(), SuggestOpts::default())
            .await
            .expect("stub returns matches for `@read`");
        assert_eq!(suggestions.prefix, "@read");
        assert_eq!(suggestions.items.len(), 2);

        let applied =
            provider.apply_completion(&lines, 0, lines[0].len(), &suggestions.items[0], "@read");
        assert_eq!(applied.lines[0], "see @readme.md");
        assert_eq!(applied.cursor_col, "see @readme.md".len());

        // A token that matches nothing yields no suggestions.
        let miss = vec!["see @zzz".to_string()];
        assert!(
            provider
                .get_suggestions(&miss, 0, miss[0].len(), SuggestOpts::default())
                .await
                .is_none()
        );
    }
}
