//! Tests for the editor's paste-marker atomic behavior.
//!
//! Large pastes (>10 lines or >1000 characters) are replaced with a
//! short marker token like `[paste #1 +20 lines]` or `[paste #1 1234
//! chars]`. The full content is retained in the editor's paste table
//! and can be recovered with `get_expanded_text()`. Cursor navigation
//! and deletion treat the marker as a single atomic unit so the user
//! doesn't have to page through hundreds of characters to move past a
//! paste.
//!
//! The tests come in two flavors:
//!
//! - **Functional**: navigation, deletion, expansion, submit, etc.
//!   These run today.
//! - **Layout-interaction**: cursor snapping through marker visual
//!   lines on narrow terminals, wrapping at marker boundaries, etc.
//!   These depend on the editor persisting its render width across
//!   `render()` calls and running the visual-line map on the resulting
//!   layout. They are `#[ignore]`'d alongside the equivalent
//!   sticky-column tests in `editor_sticky_column.rs`, and share the
//!   same follow-up.

mod support;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keys::{InputEvent, Key};
use aj_tui::tui::RenderHandle;
use regex::Regex;

fn editor() -> Editor {
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e
}

/// Paste 20 single-line "line" entries (plus a 21st that's trailing
/// whitespace). Returns the editor's current text after insertion.
fn paste_large(e: &mut Editor) -> String {
    // 20 lines of "line", each followed by a newline, then trim the
    // final newline — yields 99 characters with 19 interior newlines.
    // That's 20 lines post-split, triggering the >10-lines branch.
    let big = "line\n".repeat(20).trim_end().to_string();
    e.handle_input(&InputEvent::Paste(big));
    e.get_text()
}

/// Find the single paste marker in the text and return its byte length
/// and the full matched string.
fn find_marker(text: &str) -> (usize, String) {
    let re = Regex::new(r"\[paste #\d+ \+\d+ lines\]").expect("regex compiles");
    let m = re
        .find(text)
        .unwrap_or_else(|| panic!("expected paste marker in {:?}", text));
    (m.as_str().len(), m.as_str().to_string())
}

// ---------------------------------------------------------------------------
// Marker creation
// ---------------------------------------------------------------------------

#[test]
fn creates_a_paste_marker_for_large_pastes() {
    let mut e = editor();
    let text = paste_large(&mut e);
    let (_len, _marker) = find_marker(&text);
}

// ---------------------------------------------------------------------------
// Atomic navigation
// ---------------------------------------------------------------------------

#[test]
fn treats_paste_marker_as_single_unit_for_right_arrow() {
    let mut e = editor();
    e.handle_input(&Key::char('A'));
    paste_large(&mut e);
    e.handle_input(&Key::char('B'));

    let text = e.get_text();
    let (marker_len, _marker) = find_marker(&text);

    e.handle_input(&Key::ctrl('a'));
    assert_eq!(e.cursor(), (0, 0));

    e.handle_input(&Key::right());
    assert_eq!(e.cursor(), (0, 1));

    e.handle_input(&Key::right());
    assert_eq!(e.cursor(), (0, 1 + marker_len));

    e.handle_input(&Key::right());
    assert_eq!(e.cursor(), (0, 1 + marker_len + 1));
}

#[test]
fn treats_paste_marker_as_single_unit_for_left_arrow() {
    let mut e = editor();
    e.handle_input(&Key::char('A'));
    paste_large(&mut e);
    e.handle_input(&Key::char('B'));

    let text = e.get_text();
    let (marker_len, _marker) = find_marker(&text);

    // Cursor is at end.
    e.handle_input(&Key::left());
    assert_eq!(e.cursor(), (0, 1 + marker_len));

    e.handle_input(&Key::left());
    assert_eq!(e.cursor(), (0, 1));

    e.handle_input(&Key::left());
    assert_eq!(e.cursor(), (0, 0));
}

#[test]
fn treats_paste_marker_as_single_unit_for_backspace() {
    let mut e = editor();
    e.handle_input(&Key::char('A'));
    paste_large(&mut e);
    e.handle_input(&Key::char('B'));

    let text = e.get_text();
    let (marker_len, _marker) = find_marker(&text);

    // Position cursor just after the marker (at the 'B').
    e.handle_input(&Key::ctrl('a'));
    e.handle_input(&Key::right()); // past 'A'
    e.handle_input(&Key::right()); // past marker
    assert_eq!(e.cursor(), (0, 1 + marker_len));

    e.handle_input(&Key::backspace());
    assert_eq!(e.get_text(), "AB");
    assert_eq!(e.cursor(), (0, 1));
}

#[test]
fn treats_paste_marker_as_single_unit_for_forward_delete() {
    let mut e = editor();
    e.handle_input(&Key::char('A'));
    paste_large(&mut e);
    e.handle_input(&Key::char('B'));

    // Position cursor just before the marker (after 'A').
    e.handle_input(&Key::ctrl('a'));
    e.handle_input(&Key::right());

    e.handle_input(&Key::delete());
    assert_eq!(e.get_text(), "AB");
    assert_eq!(e.cursor(), (0, 1));
}

#[test]
fn treats_paste_marker_as_single_unit_for_word_movement() {
    let mut e = editor();
    e.handle_input(&Key::char('X'));
    e.handle_input(&Key::char(' '));
    paste_large(&mut e);
    e.handle_input(&Key::char(' '));
    e.handle_input(&Key::char('Y'));

    let text = e.get_text();
    let (marker_len, _marker) = find_marker(&text);

    e.handle_input(&Key::ctrl('a'));

    // Ctrl+Right: skip "X" (one word).
    e.handle_input(&Key::ctrl_right());
    assert_eq!(e.cursor(), (0, 1));

    // Ctrl+Right: skip whitespace + marker atomically.
    e.handle_input(&Key::ctrl_right());
    assert_eq!(e.cursor(), (0, 2 + marker_len));
}

#[test]
fn undo_restores_marker_after_backspace_deletion() {
    let mut e = editor();
    e.handle_input(&Key::char('A'));
    paste_large(&mut e);
    e.handle_input(&Key::char('B'));

    let text_before = e.get_text();

    e.handle_input(&Key::ctrl('a'));
    e.handle_input(&Key::right()); // past A
    e.handle_input(&Key::right()); // past marker

    e.handle_input(&Key::backspace());
    assert_eq!(e.get_text(), "AB");

    e.handle_input(&Key::ctrl('-'));
    assert_eq!(e.get_text(), text_before);
}

#[test]
fn handles_multiple_paste_markers_in_same_line() {
    let mut e = editor();
    paste_large(&mut e);
    e.handle_input(&Key::char(' '));
    paste_large(&mut e);

    let text = e.get_text();
    let re = Regex::new(r"\[paste #\d+ \+\d+ lines\]").expect("regex compiles");
    let markers: Vec<&str> = re.find_iter(&text).map(|m| m.as_str()).collect();
    assert_eq!(markers.len(), 2);
    let m0 = markers[0].len();
    let m1 = markers[1].len();

    e.handle_input(&Key::ctrl('a'));

    e.handle_input(&Key::right()); // skip first marker
    assert_eq!(e.cursor(), (0, m0));

    e.handle_input(&Key::right()); // past space
    assert_eq!(e.cursor(), (0, m0 + 1));

    e.handle_input(&Key::right()); // skip second marker
    assert_eq!(e.cursor(), (0, m0 + 1 + m1));
}

#[test]
fn does_not_treat_manually_typed_marker_like_text_as_atomic() {
    let mut e = editor();
    // Type the marker-like string by hand. No paste entry gets
    // created, so the text is just normal characters.
    let fake = "[paste #99 +5 lines]";
    for c in fake.chars() {
        e.handle_input(&Key::char(c));
    }
    assert_eq!(e.get_text(), fake);

    // Right arrow from col 0 moves just past '['.
    e.handle_input(&Key::ctrl('a'));
    e.handle_input(&Key::right());
    assert_eq!(e.cursor(), (0, 1));
}

// ---------------------------------------------------------------------------
// Expansion and submission
// ---------------------------------------------------------------------------

#[test]
fn expands_large_pasted_content_literally_in_get_expanded_text() {
    let mut e = editor();
    let pasted_text = [
        "line 1",
        "line 2",
        "line 3",
        "line 4",
        "line 5",
        "line 6",
        "line 7",
        "line 8",
        "line 9",
        "line 10",
        "tokens $1 $2 $& $$ $` $' end",
    ]
    .join("\n");

    e.handle_input(&InputEvent::Paste(pasted_text.clone()));

    let text = e.get_text();
    let re = Regex::new(r"\[paste #\d+ \+\d+ lines\]").expect("regex compiles");
    assert!(re.is_match(&text), "expected marker in {:?}", text);

    assert_eq!(e.get_expanded_text(), pasted_text);
}

#[test]
fn submits_large_pasted_content_literally() {
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

    let pasted_text = [
        "line 1",
        "line 2",
        "line 3",
        "line 4",
        "line 5",
        "line 6",
        "line 7",
        "line 8",
        "line 9",
        "line 10",
        "tokens $1 $2 $& $$ $` $' end",
    ]
    .join("\n");

    e.handle_input(&InputEvent::Paste(pasted_text.clone()));
    e.handle_input(&Key::enter());

    assert_eq!(submitted.borrow().as_str(), pasted_text);
}

// ---------------------------------------------------------------------------
// Tab handling in pasted text
// ---------------------------------------------------------------------------

#[test]
fn small_paste_expands_each_tab_to_four_spaces() {
    // A pasted tab represents an indent the user wanted to preserve.
    // Expand to four spaces rather than collapsing to one (matches the
    // Input component's paste-tab handling).
    let mut e = editor();
    e.handle_input(&InputEvent::Paste("a\tb\tc".to_string()));
    assert_eq!(e.get_text(), "a    b    c");
}

#[test]
fn small_paste_with_tabs_and_newlines_preserves_both() {
    // Tab → four spaces per line; newlines still split lines.
    let mut e = editor();
    e.handle_input(&InputEvent::Paste("\tone\n\ttwo".to_string()));
    assert_eq!(e.get_text(), "    one\n    two");
}

#[test]
fn small_paste_strips_non_tab_control_chars() {
    // Tab is special; other control chars (NUL, BEL, etc.) are dropped.
    let mut e = editor();
    e.handle_input(&InputEvent::Paste("a\tb\0\x07c".to_string()));
    assert_eq!(e.get_text(), "a    bc");
}

// ---------------------------------------------------------------------------
// Path-prefix safety: prepend a space when a paste starts with a path
// trigger (`/`, `~`, `.`) and the cursor is preceded by a word-class
// grapheme. Without it, typing `cd` then pasting `/etc/hosts` would
// merge into `cd/etc/hosts` and read like a single token.
// ---------------------------------------------------------------------------

#[test]
fn paste_with_slash_prefix_after_word_prepends_space() {
    let mut e = editor();
    e.handle_input(&Key::char('c'));
    e.handle_input(&Key::char('d'));
    e.handle_input(&InputEvent::Paste("/etc/hosts".to_string()));
    assert_eq!(e.get_text(), "cd /etc/hosts");
}

#[test]
fn paste_with_tilde_prefix_after_word_prepends_space() {
    let mut e = editor();
    e.handle_input(&Key::char('l'));
    e.handle_input(&Key::char('s'));
    e.handle_input(&InputEvent::Paste("~/code".to_string()));
    assert_eq!(e.get_text(), "ls ~/code");
}

#[test]
fn paste_with_dot_prefix_after_word_prepends_space() {
    let mut e = editor();
    e.handle_input(&Key::char('l'));
    e.handle_input(&Key::char('s'));
    e.handle_input(&InputEvent::Paste("./bin".to_string()));
    assert_eq!(e.get_text(), "ls ./bin");
}

#[test]
fn paste_with_path_prefix_after_space_does_not_prepend() {
    // Cursor preceded by whitespace — the space already separates the
    // paste from the preceding word, so no extra space is inserted.
    let mut e = editor();
    e.handle_input(&Key::char('c'));
    e.handle_input(&Key::char('d'));
    e.handle_input(&Key::char(' '));
    e.handle_input(&InputEvent::Paste("/etc/hosts".to_string()));
    assert_eq!(e.get_text(), "cd /etc/hosts");
}

#[test]
fn paste_with_path_prefix_at_start_of_line_does_not_prepend() {
    // Cursor at column 0 has no preceding character — nothing to
    // separate the paste from.
    let mut e = editor();
    e.handle_input(&InputEvent::Paste("/etc/hosts".to_string()));
    assert_eq!(e.get_text(), "/etc/hosts");
}

#[test]
fn paste_with_path_prefix_after_punctuation_does_not_prepend() {
    // Punctuation already breaks the visual word boundary, so a
    // separator space would just add noise.
    let mut e = editor();
    e.handle_input(&Key::char('('));
    e.handle_input(&InputEvent::Paste("/etc".to_string()));
    assert_eq!(e.get_text(), "(/etc");
}

#[test]
fn paste_without_path_prefix_after_word_does_not_prepend() {
    // Only `/`, `~`, `.` triggers the prepend. A plain word paste is
    // a continuation, not a path, and merges naturally.
    let mut e = editor();
    e.handle_input(&Key::char('f'));
    e.handle_input(&Key::char('o'));
    e.handle_input(&Key::char('o'));
    e.handle_input(&InputEvent::Paste("bar".to_string()));
    assert_eq!(e.get_text(), "foobar");
}

#[test]
fn large_paste_with_path_prefix_after_word_separates_marker_with_space() {
    // The large-paste branch replaces the content with a marker, but
    // we still want the visual separation between the preceding word
    // and the marker for consistency. Storing the leading space in
    // the line buffer (not the paste content) lets `get_expanded_text`
    // recover the literal pasted bytes — `cd /etc/...` rather than
    // `cd/etc/...`.
    let mut e = editor();
    e.handle_input(&Key::char('c'));
    e.handle_input(&Key::char('d'));
    let big = "/path\n".repeat(20).trim_end().to_string();
    e.handle_input(&InputEvent::Paste(big.clone()));

    let text = e.get_text();
    assert!(
        text.starts_with("cd ["),
        "expected `cd ` followed by marker, got {:?}",
        text,
    );

    let expanded = e.get_expanded_text();
    assert_eq!(expanded, format!("cd {}", big));
}

// ---------------------------------------------------------------------------
// Layout-interaction tests: wrapping and marker-aware vertical navigation
//
// These exercise the editor's layout_width cache and visual-line map:
// markers wider than the render width must split visually (without
// losing their atomic semantics), and vertical movement through a
// marker-containing line must snap to marker boundaries and skip over
// continuation visual lines of oversized markers.
// ---------------------------------------------------------------------------

/// Paste exactly `n_lines` single-word lines, producing a `+N lines`
/// marker when it crosses the large-paste threshold.
fn paste_n_lines(e: &mut Editor, n_lines: usize) {
    let content = "line\n".repeat(n_lines).trim_end().to_string();
    e.handle_input(&InputEvent::Paste(content));
}

/// Paste a string of `n_chars` literal 'x' characters, producing a
/// `N chars` marker when it crosses the 1000-char threshold.
fn paste_n_chars(e: &mut Editor, n_chars: usize) {
    e.handle_input(&InputEvent::Paste("x".repeat(n_chars)));
}

#[test]
fn does_not_crash_when_paste_marker_is_wider_than_terminal_width() {
    let mut e = editor();
    paste_n_lines(&mut e, 47);

    let text = e.get_text();
    let re = Regex::new(r"\[paste #\d+ \+\d+ lines\]").expect("regex compiles");
    let marker = re.find(&text).expect("paste marker should be created");
    assert!(
        marker.as_str().len() > 8,
        "marker ({} chars) should be wider than width 8",
        marker.as_str().len(),
    );

    // Render at a narrow width — should not panic, and every rendered
    // line must fit. The marker visually splits across multiple VLs
    // while remaining atomic at the cursor-navigation level.
    let lines = e.render(8);
    for line in &lines {
        let vw = aj_tui::ansi::visible_width(line);
        assert!(
            vw <= 8,
            "line exceeds width 8: visible={} text={:?}",
            vw,
            line,
        );
    }
}

#[test]
fn does_not_crash_when_text_plus_marker_exceeds_width_with_cursor_on_marker() {
    let mut e = editor();
    for _ in 0..35 {
        e.handle_input(&Key::char('b'));
    }
    paste_n_lines(&mut e, 27);
    for _ in 0..4 {
        e.handle_input(&Key::char('b'));
    }

    // Move cursor left so it lands on the marker atomically.
    for _ in 0..5 {
        e.handle_input(&Key::left());
    }

    let render_width = 54;
    let lines = e.render(render_width);
    for line in &lines {
        let vw = aj_tui::ansi::visible_width(line);
        assert!(
            vw <= render_width,
            "line exceeds width {}: visible={} text={:?}",
            render_width,
            vw,
            line,
        );
    }
}

#[test]
fn word_wrap_line_re_checks_overflow_after_backtracking() {
    // Reproduces a subtle wrap bug: after backtracking to a wrap
    // opportunity at a space, the remaining run (35 'b's + atomic
    // 21-char marker = 56 chars) must re-check overflow and force-
    // break rather than silently overflowing the visual row.
    let mut e = editor();
    e.handle_input(&Key::char(' '));
    for _ in 0..35 {
        e.handle_input(&Key::char('b'));
    }
    paste_n_lines(&mut e, 27);
    for _ in 0..4 {
        e.handle_input(&Key::char('b'));
    }

    let render_width = 54;
    let lines = e.render(render_width);
    for line in &lines {
        let vw = aj_tui::ansi::visible_width(line);
        assert!(
            vw <= render_width,
            "line exceeds width {}: visible={} text={:?}",
            render_width,
            vw,
            line,
        );
    }
}

#[test]
fn snaps_to_paste_marker_start_when_navigating_down_into_it() {
    let mut e = editor();

    // Line 0: long enough for a sticky column of 10.
    // Line 1: empty.
    // Line 2: "hello " followed by a large-chars paste marker.
    e.set_text("12345678901234567890\n\nhello ");
    paste_n_chars(&mut e, 2000);
    let _ = e.render(80);

    // Sanity-check the chars marker exists.
    let text = e.get_text();
    let re = Regex::new(r"\[paste #\d+ \d+ chars\]").expect("regex compiles");
    assert!(re.is_match(&text), "expected chars marker in {:?}", text);

    // Navigate to line 0, col 10.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..10 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (0, 10));

    // Down to empty line.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (1, 0));

    // Down to paste-marker line. Sticky col 10 falls inside the marker
    // (which starts at col 6), so the cursor snaps to the marker's
    // start rather than landing inside it.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 6));
}

#[test]
fn preserves_sticky_column_when_navigating_through_paste_marker_line() {
    let mut e = editor();

    // Build the five-line document:
    //   0: "1234567890123456"   (16 chars)
    //   1: ""
    //   2: "[paste #1 2000 chars]"
    //   3: ""
    //   4: "abcdefghijklmnop"   (16 chars)
    for ch in "1234567890123456".chars() {
        e.handle_input(&Key::char(ch));
    }
    // Shift+Enter inserts a newline via the `tui.input.newLine`
    // binding. (Plain Enter would match `tui.input.submit`, which is a
    // silent no-op under `disable_submit = true` per pi parity.)
    e.handle_input(&Key::shift_enter());
    e.handle_input(&Key::shift_enter());
    paste_n_chars(&mut e, 2000);
    e.handle_input(&Key::shift_enter());
    e.handle_input(&Key::shift_enter());
    for ch in "abcdefghijklmnop".chars() {
        e.handle_input(&Key::char(ch));
    }
    let _ = e.render(30);

    // Navigate to line 0, col 10.
    for _ in 0..4 {
        e.handle_input(&Key::up());
    }
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..10 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (0, 10));

    // Down through empty line: sticky col 10 established.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (1, 0));

    // Down onto marker line: snap to marker start (col 0).
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 0));

    // Down to next empty line.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (3, 0));

    // Down to last line: sticky col 10 restores.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (4, 10));
}

#[test]
fn does_not_get_stuck_moving_down_from_a_multi_visual_line_paste_marker() {
    let mut e = editor();

    // Line 0: "abcdefgh" + marker(21 chars, +100 lines) + "ijklmnopqr"
    // Line 1: "123456789012345678"
    //
    // The 21-char marker is wider than terminal width 20, so word-wrap
    // splits at the space before "lines]":
    //   VL0: abcdefgh            (line 0, start 0,  len 8)
    //   VL1: [paste #1 +100      (line 0, start 8,  len 15)
    //   VL2: lines]ijklmnopqr    (line 0, start 23, len 16) <- marker tail + content
    //   VL3: 123456789012345678  (line 1)
    for ch in "abcdefgh".chars() {
        e.handle_input(&Key::char(ch));
    }
    paste_n_lines(&mut e, 100);
    for ch in "ijklmnopqr".chars() {
        e.handle_input(&Key::char(ch));
    }
    // Shift+Enter inserts a newline via `tui.input.newLine`; plain
    // Enter is a no-op under `disable_submit = true` (pi parity).
    e.handle_input(&Key::shift_enter());
    for ch in "123456789012345678".chars() {
        e.handle_input(&Key::char(ch));
    }
    let _ = e.render(20);

    let text = e.get_text();
    let re = Regex::new(r"\[paste #\d+ \+\d+ lines\]").expect("regex compiles");
    let marker_match = re.find(&text).expect("paste marker should be created");
    let marker_len = marker_match.as_str().len();
    assert!(
        marker_len > 20,
        "marker ({} chars) should be wider than render width 20",
        marker_len,
    );
    let marker_start = 8;
    let marker_end = marker_start + marker_len;

    // Navigate to line 0, col 6 (on "g"). Preferred col 6 is past the
    // marker tail on VL2, so the cursor should land on the first
    // content character after the marker (col 29 = "i") without
    // snapping back into the marker.
    e.handle_input(&Key::up());
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..6 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (0, 6));

    // Down: lands on the paste-marker start.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (0, marker_start));

    // Down again: preferred col 6 lands at VL col 29 ("i"), past the
    // marker. Cursor stays on line 0.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (0, marker_end));

    // Up: back to paste marker.
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, marker_start));

    // Up again: back to col 6 ("g").
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 6));
}

#[test]
fn skips_marker_continuation_vls_when_preferred_col_falls_in_marker_tail() {
    let mut e = editor();

    // Same layout as the previous test. Start at col 3 ("d"); the
    // preferred col of 3 maps to VL2 visual col 3 which sits inside
    // the "lines]" marker tail. move_to_visual_line detects the
    // continuation VL and skips forward to VL3 (line 1).
    for ch in "abcdefgh".chars() {
        e.handle_input(&Key::char(ch));
    }
    paste_n_lines(&mut e, 100);
    for ch in "ijklmnopqr".chars() {
        e.handle_input(&Key::char(ch));
    }
    // Shift+Enter inserts a newline via `tui.input.newLine`; plain
    // Enter is a no-op under `disable_submit = true` (pi parity).
    e.handle_input(&Key::shift_enter());
    for ch in "123456789012345678".chars() {
        e.handle_input(&Key::char(ch));
    }
    let _ = e.render(20);

    // Navigate to line 0, col 3 ("d").
    e.handle_input(&Key::up());
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..3 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (0, 3));

    // Down: marker.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor().1, 8);

    // Down: skips the marker-tail continuation VL and lands on line 1.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (1, 3));

    // Round-trip back.
    e.handle_input(&Key::up());
    assert_eq!(e.cursor().1, 8);
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 3));
}
