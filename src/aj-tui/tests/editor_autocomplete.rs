//! Editor ↔ autocomplete integration tests.
//!
//! Covers the editor's side of the autocomplete contract: how it wires
//! a provider into its input loop, handles Tab as a force-complete
//! request, routes navigation/accept/cancel keys while a popup is
//! open, retains exact-typed values on Enter, and respects the
//! debounce / cancel contract on `@` and `#` autocomplete.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use aj_tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CompletionApplied, SuggestOpts,
};
use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keys::Key;
use aj_tui::tui::RenderHandle;

fn editor() -> Editor {
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e
}

/// Helper: standard apply-completion behavior — replace exactly
/// `prefix.len()` characters before the cursor with the item's value,
/// advancing the cursor to the end of the inserted text.
fn apply_prefix_replace(
    lines: &[String],
    cursor_line: usize,
    cursor_col: usize,
    item: &AutocompleteItem,
    prefix: &str,
) -> CompletionApplied {
    let mut new_lines = lines.to_vec();
    let line = new_lines[cursor_line].clone();
    let before = &line[..cursor_col - prefix.len()];
    let after = &line[cursor_col..];
    new_lines[cursor_line] = format!("{}{}{}", before, item.value, after);
    CompletionApplied {
        lines: new_lines,
        cursor_line,
        cursor_col: cursor_col - prefix.len() + item.value.len(),
    }
}

/// A closure-backed `AutocompleteProvider` that returns `(items,
/// prefix)` given `(lines, cursor_col, force)`. Useful when a test
/// wants to control provider behavior inline without defining a full
/// named type.
struct MockProvider<F>
where
    F: Fn(&[String], usize, usize, bool) -> Option<(Vec<AutocompleteItem>, String)>,
{
    get: F,
}

#[async_trait]
impl<F> AutocompleteProvider for MockProvider<F>
where
    F: Fn(&[String], usize, usize, bool) -> Option<(Vec<AutocompleteItem>, String)>
        + Send
        + Sync
        + 'static,
{
    async fn get_suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        opts: SuggestOpts,
    ) -> Option<AutocompleteSuggestions> {
        let (items, prefix) = (self.get)(lines, cursor_line, cursor_col, opts.force)?;
        Some(AutocompleteSuggestions { items, prefix })
    }

    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> CompletionApplied {
        apply_prefix_replace(lines, cursor_line, cursor_col, item, prefix)
    }
}

/// Convenience: item with no description.
fn item(v: &str) -> AutocompleteItem {
    AutocompleteItem::new(v.to_string(), v.to_string())
}

async fn type_str(e: &mut Editor, s: &str) {
    for c in s.chars() {
        e.handle_input(&Key::char(c));
        e.wait_for_pending_autocomplete().await;
    }
}

// ---------------------------------------------------------------------------
// Tab: force-complete single and multi
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auto_applies_single_force_file_suggestion_without_showing_menu() {
    let mut e = editor();
    e.set_autocomplete_provider(Arc::new(MockProvider {
        get: |lines, _l, col, force| {
            if !force {
                return None;
            }
            let text = &lines[0];
            let prefix = &text[..col];
            if prefix == "Work" {
                Some((vec![item("Workspace/")], "Work".to_string()))
            } else {
                None
            }
        },
    }));

    type_str(&mut e, "Work").await;
    assert_eq!(e.get_text(), "Work");

    // Tab auto-applies the single suggestion.
    e.handle_input(&Key::tab());
    e.wait_for_pending_autocomplete().await;
    assert_eq!(e.get_text(), "Workspace/");
    assert!(!e.is_showing_autocomplete());

    // Undo restores to "Work".
    e.handle_input(&Key::ctrl('-'));
    assert_eq!(e.get_text(), "Work");
}

#[tokio::test]
async fn shows_menu_when_force_file_has_multiple_suggestions() {
    let mut e = editor();
    e.set_autocomplete_provider(Arc::new(MockProvider {
        get: |lines, _l, col, force| {
            if !force {
                return None;
            }
            let prefix = &lines[0][..col];
            if prefix == "src" {
                Some((vec![item("src/"), item("src.txt")], "src".to_string()))
            } else {
                None
            }
        },
    }));

    type_str(&mut e, "src").await;

    // Tab shows menu (multiple suggestions).
    e.handle_input(&Key::tab());
    e.wait_for_pending_autocomplete().await;
    assert_eq!(e.get_text(), "src");
    assert!(e.is_showing_autocomplete());

    // Second Tab accepts the first suggestion.
    e.handle_input(&Key::tab());
    e.wait_for_pending_autocomplete().await;
    assert_eq!(e.get_text(), "src/");
    assert!(!e.is_showing_autocomplete());
}

#[tokio::test]
async fn keeps_suggestions_open_when_typing_in_force_mode() {
    let mut e = editor();
    let all_files = [
        item("readme.md"),
        item("package.json"),
        item("src/"),
        item("dist/"),
    ];
    e.set_autocomplete_provider(Arc::new(MockProvider {
        get: move |lines, _l, col, force| {
            let prefix = &lines[0][..col];
            let should_match = force || prefix.contains('/') || prefix.starts_with('.');
            if !should_match {
                return None;
            }
            let filtered: Vec<AutocompleteItem> = all_files
                .iter()
                .filter(|f| f.value.to_lowercase().starts_with(&prefix.to_lowercase()))
                .cloned()
                .collect();
            if filtered.is_empty() {
                return None;
            }
            Some((filtered, prefix.to_string()))
        },
    }));

    // Tab on empty prompt → force mode, shows all.
    e.handle_input(&Key::tab());
    e.wait_for_pending_autocomplete().await;
    assert!(e.is_showing_autocomplete());

    // Type "r" — narrow but still in force mode.
    e.handle_input(&Key::char('r'));
    e.wait_for_pending_autocomplete().await;
    assert_eq!(e.get_text(), "r");
    assert!(e.is_showing_autocomplete());

    // Type "e" — still narrowing.
    e.handle_input(&Key::char('e'));
    e.wait_for_pending_autocomplete().await;
    assert_eq!(e.get_text(), "re");
    assert!(e.is_showing_autocomplete());

    // Tab accepts the first remaining suggestion ("readme.md").
    e.handle_input(&Key::tab());
    e.wait_for_pending_autocomplete().await;
    assert_eq!(e.get_text(), "readme.md");
    assert!(!e.is_showing_autocomplete());
}

// ---------------------------------------------------------------------------
// Debounce / abort — verify the new async pipeline's contract.
// ---------------------------------------------------------------------------

/// The `@`-attachment context has a 20ms debounce: typing several
/// characters faster than that should result in strictly fewer
/// provider calls than characters typed. Slash-command triggers
/// have no debounce, so they serve as a control group.
#[tokio::test]
async fn debounces_at_autocomplete_while_typing() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aj_tui::autocomplete::{
        AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CompletionApplied,
        SuggestOpts,
    };
    use async_trait::async_trait;

    struct RecordingProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl AutocompleteProvider for RecordingProvider {
        async fn get_suggestions(
            &self,
            lines: &[String],
            _cursor_line: usize,
            cursor_col: usize,
            _opts: SuggestOpts,
        ) -> Option<AutocompleteSuggestions> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let before = &lines[0][..cursor_col];
            if before.contains('@') {
                Some(AutocompleteSuggestions {
                    prefix: before.rsplit(' ').next().unwrap_or("").to_string(),
                    items: vec![AutocompleteItem::new("@file.rs", "file.rs")],
                })
            } else {
                None
            }
        }
        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            item: &AutocompleteItem,
            _prefix: &str,
        ) -> CompletionApplied {
            CompletionApplied {
                lines: lines.to_vec(),
                cursor_line,
                cursor_col: cursor_col + item.value.len(),
            }
        }
    }

    let mut e = editor();
    let calls = Arc::new(AtomicUsize::new(0));
    e.set_autocomplete_provider(Arc::new(RecordingProvider {
        calls: Arc::clone(&calls),
    }));

    // Rapid typing inside an `@`-attachment context. The `@` opens
    // the popup, and each subsequent alphanumeric char refreshes
    // it — but the 20ms debounce coalesces rapid keystrokes into a
    // single dispatch. After the synchronous burst the provider
    // hasn't been called yet, and exactly one call lands once the
    // debounce window expires.
    for ch in "@abcdefgh".chars() {
        e.handle_input(&Key::char(ch));
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "provider must not be called until the @ debounce window expires",
    );
    e.wait_for_pending_autocomplete().await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "exactly one debounced provider call should land after the @ burst",
    );
    // Sanity: the final call should have happened so the popup is
    // visible.
    assert!(
        e.is_showing_autocomplete(),
        "popup must be visible after the final @-attachment keystroke",
    );
}

/// A `#` typed at a token boundary is a symbol-completion trigger
/// just like `@`: the editor triggers on `'@' | '#'` after whitespace
/// or start-of-line via `symbol_follows_whitespace_or_start`. Without
/// this, typing `#1234` would never call the provider.
#[tokio::test]
async fn debounces_hash_autocomplete_while_typing() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aj_tui::autocomplete::{
        AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CompletionApplied,
        SuggestOpts,
    };
    use async_trait::async_trait;

    struct RecordingProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl AutocompleteProvider for RecordingProvider {
        async fn get_suggestions(
            &self,
            lines: &[String],
            _cursor_line: usize,
            cursor_col: usize,
            _opts: SuggestOpts,
        ) -> Option<AutocompleteSuggestions> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let before = &lines[0][..cursor_col];
            Some(AutocompleteSuggestions {
                prefix: before.to_string(),
                items: vec![AutocompleteItem::new("#2983", "#2983")],
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            item: &AutocompleteItem,
            _prefix: &str,
        ) -> CompletionApplied {
            CompletionApplied {
                lines: lines.to_vec(),
                cursor_line,
                cursor_col: cursor_col + item.value.len(),
            }
        }
    }

    let mut e = editor();
    let calls = Arc::new(AtomicUsize::new(0));
    e.set_autocomplete_provider(Arc::new(RecordingProvider {
        calls: Arc::clone(&calls),
    }));

    // `#` opens the popup, the digits refresh it. The 20ms debounce
    // window keeps any provider call pending until the user stops
    // typing for ~20ms, so a tight burst of keystrokes coalesces
    // into one walk: typing `#2983` synchronously must not have
    // triggered a provider call yet, and exactly one call lands
    // once the debounce window expires.
    for ch in "#2983".chars() {
        e.handle_input(&Key::char(ch));
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "provider must not be called until the # debounce window expires",
    );
    e.wait_for_pending_autocomplete().await;

    // Exactly one call after the wait: only the most recent
    // dispatch survives the intervening cancellations.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "exactly one debounced provider call should land after the # burst",
    );
    assert!(
        e.is_showing_autocomplete(),
        "popup must be visible after a # token has been typed",
    );
}

/// A new autocomplete request cancels any in-flight one, even if the
/// earlier one was slow. Verified here by making the provider hold
/// for a controllable duration and observing that the first
/// invocation sees `cancel.is_cancelled() == true` before it returns.
#[tokio::test]
async fn aborts_active_at_autocomplete_when_typing_continues() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use aj_tui::autocomplete::{
        AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CompletionApplied,
        SuggestOpts,
    };
    use async_trait::async_trait;
    use tokio::sync::Notify;

    struct SlowProvider {
        calls: Arc<AtomicUsize>,
        /// Released once the test permits the first call to return.
        release: Arc<Notify>,
        first_call_saw_cancel: Arc<AtomicBool>,
    }
    #[async_trait]
    impl AutocompleteProvider for SlowProvider {
        async fn get_suggestions(
            &self,
            _lines: &[String],
            _cursor_line: usize,
            _cursor_col: usize,
            opts: SuggestOpts,
        ) -> Option<AutocompleteSuggestions> {
            let call_n = self.calls.fetch_add(1, Ordering::SeqCst);
            if call_n == 0 {
                // First call — wait to be released, OR to be cancelled.
                tokio::select! {
                    _ = opts.cancel.cancelled() => {
                        self.first_call_saw_cancel.store(true, Ordering::SeqCst);
                        return None;
                    }
                    _ = self.release.notified() => {}
                }
                return None;
            }
            // Subsequent calls return immediately.
            Some(AutocompleteSuggestions {
                prefix: "@".to_string(),
                items: vec![AutocompleteItem::new("@file.rs", "file.rs")],
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            _item: &AutocompleteItem,
            _prefix: &str,
        ) -> CompletionApplied {
            CompletionApplied {
                lines: lines.to_vec(),
                cursor_line,
                cursor_col,
            }
        }
    }

    let mut e = editor();
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Notify::new());
    let first_call_saw_cancel = Arc::new(AtomicBool::new(false));
    e.set_autocomplete_provider(Arc::new(SlowProvider {
        calls: Arc::clone(&calls),
        release: Arc::clone(&release),
        first_call_saw_cancel: Arc::clone(&first_call_saw_cancel),
    }));

    // Dispatch the first request. Use Tab so the request fires
    // immediately (no `@`-debounce to wait through).
    e.handle_input(&Key::tab());
    // Give the task time to hit the `select!` inside `get_suggestions`.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Fire a second Tab — this cancels the first request before
    // releasing it.
    e.handle_input(&Key::tab());
    // Let the cancellation propagate and the second call run.
    e.wait_for_pending_autocomplete().await;

    // Nobody should be blocked anymore, but make sure:
    release.notify_waiters();
    // Extra yield so any trailing task shutdown completes.
    tokio::task::yield_now().await;

    assert!(
        first_call_saw_cancel.load(Ordering::SeqCst),
        "the first in-flight request must observe cancellation when a \
         second request supersedes it",
    );
    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "both requests must dispatch"
    );
}

// A shared counter fixture, available for tests that want to assert
// on provider call counts. Kept unused here for now in case a future
// test needs it.
#[allow(dead_code)]
fn counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

// ---------------------------------------------------------------------------
// Rendering the autocomplete popup
// ---------------------------------------------------------------------------
//
// These tests verify that a visible autocomplete state actually produces
// popup lines in the rendered output. Earlier tests in this file cover
// the state-machine contract (when does the popup open, what does Tab
// apply, etc.); the rendering side was only verified by inspecting the
// `is_showing_autocomplete()` flag, which missed a regression where the
// popup was fully wired but never drawn.

use aj_tui::components::editor::Editor as EditorType;

fn render_plain(e: &mut EditorType, width: usize) -> Vec<String> {
    e.render(width)
        .iter()
        .map(|l| support::strip_ansi(l))
        .collect()
}

// ---------------------------------------------------------------------------
// Trigger gating: the editor must not ask the provider on every keystroke
// ---------------------------------------------------------------------------
//
// Regression guards for a class of bug where any character insertion —
// including whitespace and plain prose — calls into the provider and,
// for CombinedAutocompleteProvider's direct-path-completion branch,
// produces a popup listing every file in the working directory. The
// fix is in the editor: only invoke the provider for characters that
// plausibly start or continue a completion context (/, @, or
// identifier-chars inside an existing /-or-@ context).

/// A counter-backed provider that records how often `get_suggestions`
/// is called and always returns None.
struct CountingProvider {
    count: Arc<AtomicUsize>,
}

#[async_trait]
impl AutocompleteProvider for CountingProvider {
    async fn get_suggestions(
        &self,
        _lines: &[String],
        _cursor_line: usize,
        _cursor_col: usize,
        _opts: SuggestOpts,
    ) -> Option<AutocompleteSuggestions> {
        self.count.fetch_add(1, Ordering::SeqCst);
        None
    }
    fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        _prefix: &str,
    ) -> CompletionApplied {
        CompletionApplied {
            lines: lines.to_vec(),
            cursor_line,
            cursor_col: cursor_col + item.value.len(),
        }
    }
}

fn counting_editor() -> (Editor, Arc<AtomicUsize>) {
    let mut e = editor();
    let count = Arc::new(AtomicUsize::new(0));
    e.set_autocomplete_provider(Arc::new(CountingProvider {
        count: Arc::clone(&count),
    }));
    (e, count)
}

#[tokio::test]
async fn typing_prose_does_not_call_provider() {
    // Typing ordinary prose in the first line with no slash or @ should
    // never ask the provider for suggestions. In particular a trailing
    // space must not fire a "list every file" query.
    let (mut e, count) = counting_editor();

    for ch in "hello world ".chars() {
        e.handle_input(&Key::char(ch));
        e.wait_for_pending_autocomplete().await;
    }

    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "expected no provider calls for plain prose, got {}",
        count.load(Ordering::SeqCst),
    );
}

#[tokio::test]
async fn typing_a_bare_space_does_not_call_provider() {
    // The specific regression we were chasing: a trailing space used
    // to open the direct-path-completion branch with an empty prefix,
    // showing a file menu on every word break.
    let (mut e, count) = counting_editor();
    e.handle_input(&Key::char(' '));
    e.wait_for_pending_autocomplete().await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn at_sign_after_whitespace_calls_provider() {
    let (mut e, count) = counting_editor();
    e.handle_input(&Key::char('@'));
    e.wait_for_pending_autocomplete().await;
    let after_bare_at = count.load(Ordering::SeqCst);
    assert!(
        after_bare_at >= 1,
        "bare `@` should trigger the @-file popup",
    );

    // Same after a space.
    for ch in "hi @".chars() {
        e.handle_input(&Key::char(ch));
        e.wait_for_pending_autocomplete().await;
    }
    assert!(
        count.load(Ordering::SeqCst) > after_bare_at,
        "`@` after whitespace should trigger the @-file popup again",
    );
}

#[tokio::test]
async fn at_sign_inside_a_word_does_not_call_provider() {
    // `user@host`, `a@b.com`, etc. — not an attachment context.
    let (mut e, count) = counting_editor();
    for ch in "user".chars() {
        e.handle_input(&Key::char(ch));
        e.wait_for_pending_autocomplete().await;
    }
    let before_at = count.load(Ordering::SeqCst);
    e.handle_input(&Key::char('@'));
    e.wait_for_pending_autocomplete().await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        before_at,
        "typing `@` immediately after a word must not open the @-file popup",
    );
}

#[tokio::test]
async fn enter_on_at_file_popup_applies_completion_but_does_not_submit() {
    // Complement to the slash-command case: @-file and other non-slash
    // prefixes apply the completion and *stop*, because the user is
    // mid-message and hasn't indicated they're ready to send.
    use aj_tui::autocomplete::{AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions};

    struct AtProvider;
    #[async_trait]
    impl AutocompleteProvider for AtProvider {
        async fn get_suggestions(
            &self,
            lines: &[String],
            _cursor_line: usize,
            cursor_col: usize,
            _opts: SuggestOpts,
        ) -> Option<AutocompleteSuggestions> {
            let before = &lines[0][..cursor_col];
            let at_idx = before.rfind('@')?;
            let prefix = &before[at_idx..];
            // Match the @-file convention: prefix carries the leading @.
            Some(AutocompleteSuggestions {
                prefix: prefix.to_string(),
                items: vec![AutocompleteItem {
                    value: format!("{}src/main.rs", &prefix[..1]),
                    label: "src/main.rs".to_string(),
                    description: None,
                }],
            })
        }
        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            item: &AutocompleteItem,
            prefix: &str,
        ) -> aj_tui::autocomplete::CompletionApplied {
            let mut new_lines = lines.to_vec();
            let line = new_lines[cursor_line].clone();
            let before = &line[..cursor_col - prefix.len()];
            let after = &line[cursor_col..];
            new_lines[cursor_line] = format!("{}{}{}", before, item.value, after);
            aj_tui::autocomplete::CompletionApplied {
                lines: new_lines,
                cursor_line,
                cursor_col: before.len() + item.value.len(),
            }
        }
    }

    let mut e = editor();
    e.disable_submit = false;
    e.set_autocomplete_provider(Arc::new(AtProvider));

    type_str(&mut e, "look at @").await;
    assert!(e.is_showing_autocomplete());

    e.handle_input(&Key::enter());

    assert_eq!(
        e.take_submitted(),
        None,
        "Enter on an @-file popup should NOT submit — user is still composing",
    );
    assert!(
        !e.is_showing_autocomplete(),
        "popup should be dismissed after Enter",
    );
    assert_eq!(e.get_text(), "look at @src/main.rs");
}

// ---------------------------------------------------------------------------
// Prefix-discriminated autocomplete popup layout.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn at_prefix_autocomplete_keeps_default_layout() {
    // A MockProvider that returns suggestions whenever the line contains
    // an `@`. The returned prefix starts with `@`, so the editor's
    // `select_list_layout_for_prefix` should fall through to the default
    // (32-cell primary column) rather than the slash-command branch.
    let mut e = editor();
    e.set_theme(support::themes::identity_editor_theme());
    e.set_autocomplete_provider(Arc::new(MockProvider {
        get: |lines, _l, col, _force| {
            let before = &lines[0][..col];
            if !before.contains('@') {
                return None;
            }
            Some((
                vec![
                    AutocompleteItem::new("@file.rs", "@file.rs").with_description("first file"),
                    AutocompleteItem::new("@other.rs", "@other.rs").with_description("second file"),
                ],
                "@".to_string(),
            ))
        },
    }));

    e.handle_input(&Key::char('@'));
    e.wait_for_pending_autocomplete().await;
    assert!(
        e.is_showing_autocomplete(),
        "typing `@` at start should open the @-file popup",
    );

    let plain = render_plain(&mut e, 60);
    let row_with_first = plain
        .iter()
        .find(|l| l.contains("@file.rs") && l.contains("first file"))
        .unwrap_or_else(|| {
            panic!(
                "expected a popup row containing `@file.rs`; got {:?}",
                plain
            )
        });

    // Default layout: primary column is the constant 32 cells (with no
    // min/max overrides, both bounds collapse to the
    // `DEFAULT_PRIMARY_COLUMN_WIDTH` constant). `description_start =
    // prefix_marker(2) + primary_column(32) = 34`.
    assert_eq!(
        support::visible_index_of(row_with_first, "first file"),
        34,
        "@-file popup should use the layout default (32-cell primary); \
         description column should be 34, not the slash layout's 14. row: {:?}",
        row_with_first,
    );
}
