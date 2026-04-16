//! Tests for the editor's undo stack (Ctrl+-).
//!
//! Coverage for the coalescing rules (word continuation is one unit;
//! each space is its own unit; newline breaks coalescing), for deletes
//! and kills being individually undoable, for programmatic insertions
//! and pastes being atomic undo units, and for history browsing
//! interactions where entering browse mode is an undoable step that
//! restores to the pre-browse draft exactly.

mod support;

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

fn type_string(e: &mut Editor, text: &str) {
    for c in text.chars() {
        if c == '\n' {
            e.handle_input(&Key::shift_enter());
        } else {
            e.handle_input(&Key::char(c));
        }
    }
}

fn undo(e: &mut Editor) {
    e.handle_input(&Key::ctrl('-'));
}

// ---------------------------------------------------------------------------
// Empty stack / coalescing
// ---------------------------------------------------------------------------

#[test]
fn does_nothing_when_undo_stack_is_empty() {
    let mut e = editor();
    undo(&mut e);
    assert_eq!(e.get_text(), "");
}

#[test]
fn coalesces_consecutive_word_characters_into_one_undo_unit() {
    let mut e = editor();
    type_string(&mut e, "hello world");
    assert_eq!(e.get_text(), "hello world");

    // First undo removes " world" (the snapshot taken before the space
    // captured the "hello" state).
    undo(&mut e);
    assert_eq!(e.get_text(), "hello");

    // Second undo removes "hello" (the initial snapshot).
    undo(&mut e);
    assert_eq!(e.get_text(), "");
}

#[test]
fn undoes_spaces_one_at_a_time() {
    let mut e = editor();
    type_string(&mut e, "hello  ");
    assert_eq!(e.get_text(), "hello  ");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello ");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello");

    undo(&mut e);
    assert_eq!(e.get_text(), "");
}

#[test]
fn undoes_newlines_and_signals_next_word_to_capture_state() {
    let mut e = editor();
    type_string(&mut e, "hello\nworld");
    assert_eq!(e.get_text(), "hello\nworld");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello\n");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello");

    undo(&mut e);
    assert_eq!(e.get_text(), "");
}

// ---------------------------------------------------------------------------
// Deletion reversals
// ---------------------------------------------------------------------------

#[test]
fn undoes_backspace() {
    let mut e = editor();
    type_string(&mut e, "hello");
    e.handle_input(&Key::backspace());
    assert_eq!(e.get_text(), "hell");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello");
}

#[test]
fn undoes_forward_delete() {
    let mut e = editor();
    type_string(&mut e, "hello");
    e.handle_input(&Key::ctrl('a'));
    e.handle_input(&Key::right());
    e.handle_input(&Key::delete());
    assert_eq!(e.get_text(), "hllo");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello");
}

#[test]
fn undoes_ctrl_w_delete_word_backward() {
    let mut e = editor();
    type_string(&mut e, "hello world");

    e.handle_input(&Key::ctrl('w'));
    assert_eq!(e.get_text(), "hello ");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello world");
}

#[test]
fn undoes_alt_d_delete_word_forward() {
    // Sibling coverage to `undoes_ctrl_w_delete_word_backward`. Alt+D
    // kills the word to the right of the cursor (Emacs-style
    // `kill-word`) and must be undoable as a single unit even though
    // it's a multi-character deletion. The `Input` component has a
    // matching test; this one pins down the same invariant for the
    // multi-line `Editor`.
    let mut e = editor();
    type_string(&mut e, "hello world");
    e.handle_input(&Key::ctrl('a')); // cursor at start

    e.handle_input(&Key::alt('d'));
    assert_eq!(e.get_text(), " world");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello world");

    // Typing after undo places the cursor back where it was — the
    // pre-kill position at the start of the line.
    e.handle_input(&Key::char('!'));
    assert_eq!(e.get_text(), "!hello world");
}

#[test]
fn undoes_ctrl_k_delete_to_line_end() {
    let mut e = editor();
    type_string(&mut e, "hello world");
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..6 {
        e.handle_input(&Key::right());
    }

    e.handle_input(&Key::ctrl('k'));
    assert_eq!(e.get_text(), "hello ");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello world");

    // Typing after undo places the cursor where it was — between "hello " and "world".
    e.handle_input(&Key::char('|'));
    assert_eq!(e.get_text(), "hello |world");
}

#[test]
fn undoes_ctrl_u_delete_to_line_start() {
    let mut e = editor();
    type_string(&mut e, "hello world");
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..6 {
        e.handle_input(&Key::right());
    }

    e.handle_input(&Key::ctrl('u'));
    assert_eq!(e.get_text(), "world");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello world");
}

#[test]
fn undoes_yank() {
    let mut e = editor();
    type_string(&mut e, "hello ");
    e.handle_input(&Key::ctrl('w')); // kill "hello "
    e.handle_input(&Key::ctrl('y')); // yank back
    assert_eq!(e.get_text(), "hello ");

    // One undo reverses the yank, leaving the post-kill empty state.
    undo(&mut e);
    assert_eq!(e.get_text(), "");
}

// ---------------------------------------------------------------------------
// Paste and insert_text_at_cursor (atomicity)
// ---------------------------------------------------------------------------

#[test]
fn undoes_single_line_paste_atomically() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..5 {
        e.handle_input(&Key::right());
    }

    e.handle_input(&aj_tui::keys::InputEvent::Paste("beep boop".to_string()));
    assert_eq!(e.get_text(), "hellobeep boop world");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello world");

    e.handle_input(&Key::char('|'));
    assert_eq!(e.get_text(), "hello| world");
}

#[test]
fn undoes_multi_line_paste_atomically() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..5 {
        e.handle_input(&Key::right());
    }

    e.handle_input(&aj_tui::keys::InputEvent::Paste(
        "line1\nline2\nline3".to_string(),
    ));
    assert_eq!(e.get_text(), "helloline1\nline2\nline3 world");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello world");

    e.handle_input(&Key::char('|'));
    assert_eq!(e.get_text(), "hello| world");
}

#[test]
fn undoes_insert_text_at_cursor_atomically() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..5 {
        e.handle_input(&Key::right());
    }

    e.insert_text_at_cursor("/tmp/image.png");
    assert_eq!(e.get_text(), "hello/tmp/image.png world");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello world");

    e.handle_input(&Key::char('|'));
    assert_eq!(e.get_text(), "hello| world");
}

#[test]
fn insert_text_at_cursor_handles_multiline_text() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..5 {
        e.handle_input(&Key::right());
    }

    e.insert_text_at_cursor("line1\nline2\nline3");
    assert_eq!(e.get_text(), "helloline1\nline2\nline3 world");

    // Cursor ends at end of inserted region: line 2, col 5 ("line3".len()).
    assert_eq!(e.cursor(), (2, 5));

    undo(&mut e);
    assert_eq!(e.get_text(), "hello world");
}

#[test]
fn insert_text_at_cursor_normalizes_crlf_and_cr() {
    let mut e = editor();
    e.set_text("");

    e.insert_text_at_cursor("a\r\nb\r\nc");
    assert_eq!(e.get_text(), "a\nb\nc");

    undo(&mut e);
    assert_eq!(e.get_text(), "");

    e.insert_text_at_cursor("x\ry\rz");
    assert_eq!(e.get_text(), "x\ny\nz");
}

// ---------------------------------------------------------------------------
// setText as an undoable action
// ---------------------------------------------------------------------------

#[test]
fn undoes_set_text_to_empty_string() {
    let mut e = editor();
    type_string(&mut e, "hello world");
    assert_eq!(e.get_text(), "hello world");

    e.set_text("");
    assert_eq!(e.get_text(), "");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello world");
}

// ---------------------------------------------------------------------------
// Submit
// ---------------------------------------------------------------------------

#[test]
fn clears_undo_stack_on_submit() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let submitted: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let captured = Rc::clone(&submitted);

    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.set_focused(true);
    e.on_submit = Some(Box::new(move |text| {
        *captured.borrow_mut() = text.to_string();
    }));

    type_string(&mut e, "hello");
    e.handle_input(&Key::enter());

    assert_eq!(submitted.borrow().as_str(), "hello");
    assert_eq!(e.get_text(), "");

    // Stack was cleared on submit.
    undo(&mut e);
    assert_eq!(e.get_text(), "");
}

// ---------------------------------------------------------------------------
// History interactions
// ---------------------------------------------------------------------------

#[test]
fn exits_history_browsing_mode_on_undo() {
    let mut e = editor();

    e.add_to_history("hello");
    assert_eq!(e.get_text(), "");

    type_string(&mut e, "world");
    assert_eq!(e.get_text(), "world");

    e.handle_input(&Key::ctrl('w'));
    assert_eq!(e.get_text(), "");

    // Up: enter history browsing, shows "hello".
    e.handle_input(&Key::up());
    assert_eq!(e.get_text(), "hello");

    // First undo: restore to pre-history-browse state ("").
    undo(&mut e);
    assert_eq!(e.get_text(), "");

    // Second undo: restore to pre-Ctrl+W state ("world").
    undo(&mut e);
    assert_eq!(e.get_text(), "world");
}

#[test]
fn undo_restores_to_pre_history_state_even_after_multiple_navigations() {
    let mut e = editor();

    e.add_to_history("first");
    e.add_to_history("second");
    e.add_to_history("third");

    type_string(&mut e, "current");
    assert_eq!(e.get_text(), "current");

    e.handle_input(&Key::ctrl('w'));
    assert_eq!(e.get_text(), "");

    // Walk the entire history.
    e.handle_input(&Key::up());
    assert_eq!(e.get_text(), "third");
    e.handle_input(&Key::up());
    assert_eq!(e.get_text(), "second");
    e.handle_input(&Key::up());
    assert_eq!(e.get_text(), "first");

    // First undo: back to pre-browse state (""), not an intermediate.
    undo(&mut e);
    assert_eq!(e.get_text(), "");

    // Second undo: pre-Ctrl+W state ("current").
    undo(&mut e);
    assert_eq!(e.get_text(), "current");
}

// ---------------------------------------------------------------------------
// Cursor movement breaks coalescing
// ---------------------------------------------------------------------------

#[test]
fn cursor_movement_starts_new_undo_unit() {
    let mut e = editor();
    type_string(&mut e, "hello world");

    // Move cursor left 5 (to after "hello ").
    for _ in 0..5 {
        e.handle_input(&Key::left());
    }

    type_string(&mut e, "lol");
    assert_eq!(e.get_text(), "hello lolworld");

    undo(&mut e);
    assert_eq!(e.get_text(), "hello world");

    e.handle_input(&Key::char('|'));
    assert_eq!(e.get_text(), "hello |world");
}

// ---------------------------------------------------------------------------
// No-op deletion doesn't push a snapshot
// ---------------------------------------------------------------------------

#[test]
fn no_op_delete_operations_do_not_push_undo_snapshots() {
    let mut e = editor();
    type_string(&mut e, "hello");
    assert_eq!(e.get_text(), "hello");

    e.handle_input(&Key::ctrl('w')); // deletes "hello"
    assert_eq!(e.get_text(), "");
    e.handle_input(&Key::ctrl('w')); // no-op
    e.handle_input(&Key::ctrl('w')); // no-op

    // Single undo restores "hello".
    undo(&mut e);
    assert_eq!(e.get_text(), "hello");
}

// ---------------------------------------------------------------------------
// Autocomplete-gated tests (source integration pending)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn does_not_trigger_autocomplete_during_single_line_paste() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use aj_tui::autocomplete::{
        AutocompleteProvider, AutocompleteSuggestions, CompletionApplied, SuggestOpts,
    };
    use aj_tui::keys::InputEvent;
    use async_trait::async_trait;

    struct CountingProvider {
        calls: Arc<AtomicUsize>,
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
            self.calls.fetch_add(1, Ordering::SeqCst);
            None
        }
        fn apply_completion(
            &self,
            lines: &[String],
            cursor_line: usize,
            cursor_col: usize,
            _item: &aj_tui::autocomplete::AutocompleteItem,
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
    e.set_autocomplete_provider(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
    }));
    e.handle_input(&InputEvent::Paste(
        "look at @node_modules/react/index.js please".to_string(),
    ));
    // Let any spuriously-spawned task run to completion so the
    // counter stabilises before we assert on it.
    e.wait_for_pending_autocomplete().await;

    assert_eq!(e.get_text(), "look at @node_modules/react/index.js please");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "pasted text must not trigger autocomplete suggestions"
    );
    assert!(!e.is_showing_autocomplete());
}

#[tokio::test]
async fn undoes_autocomplete() {
    use std::sync::Arc;

    use aj_tui::autocomplete::{
        AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CompletionApplied,
        SuggestOpts,
    };
    use async_trait::async_trait;

    struct DistProvider;
    #[async_trait]
    impl AutocompleteProvider for DistProvider {
        async fn get_suggestions(
            &self,
            lines: &[String],
            _cursor_line: usize,
            cursor_col: usize,
            _opts: SuggestOpts,
        ) -> Option<AutocompleteSuggestions> {
            let prefix = &lines[0][..cursor_col];
            if prefix == "di" {
                Some(AutocompleteSuggestions {
                    items: vec![AutocompleteItem::new("dist/", "dist/")],
                    prefix: "di".to_string(),
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
    }

    let mut e = editor();
    e.set_autocomplete_provider(Arc::new(DistProvider));

    // Type "di".
    e.handle_input(&Key::char('d'));
    e.wait_for_pending_autocomplete().await;
    e.handle_input(&Key::char('i'));
    e.wait_for_pending_autocomplete().await;
    assert_eq!(e.get_text(), "di");

    // Tab auto-applies the single suggestion. Under the async
    // pipeline, Tab dispatches a request and the auto-apply happens
    // only once the worker delivers a result.
    e.handle_input(&Key::tab());
    e.wait_for_pending_autocomplete().await;
    assert_eq!(e.get_text(), "dist/");
    assert!(!e.is_showing_autocomplete());

    // Undo restores the pre-completion "di".
    undo(&mut e);
    assert_eq!(e.get_text(), "di");
}
