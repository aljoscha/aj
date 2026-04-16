//! Tests for the editor's "sticky visual column" behavior when moving
//! vertically through lines of unequal length.
//!
//! When the cursor leaves a long line for a shorter one, the editor
//! clamps the cursor's column to the shorter line's end — but remembers
//! the original visual column so the next vertical move back up (or
//! further down through more short lines) lands back at the original
//! column if it fits. This matches the Emacs / readline / modern-editor
//! behavior that users expect.
//!
//! Almost every editing or explicit-horizontal-movement action resets
//! the sticky column so fresh vertical navigation starts from the
//! cursor's current position. These tests pin both directions: that the
//! sticky is preserved across short-line traversal, and that each
//! non-vertical action clears it.

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

/// Editor with `padding_x = 0`. Resize tests assume this so layout-width
/// math matches the documented sticky-column scenarios exactly.
fn editor_zero_pad() -> Editor {
    let mut e = editor();
    e.set_padding_x(0);
    e
}

/// Position the cursor at `(line, col)` by feeding Up/Down/Ctrl+A/Right
/// events, matching how a user would navigate there. Needed because the
/// editor exposes no `set_cursor` API — cursor position is always the
/// result of input.
fn position_cursor(e: &mut Editor, line: usize, col: usize) {
    // Move to line 0 first.
    for _ in 0..20 {
        e.handle_input(&Key::up());
    }
    // Then down to the target line.
    for _ in 0..line {
        e.handle_input(&Key::down());
    }
    // Move to column 0, then right by `col`.
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..col {
        e.handle_input(&Key::right());
    }
}

// ---------------------------------------------------------------------------
// Preserving the sticky column across short lines
// ---------------------------------------------------------------------------

#[test]
fn preserves_target_column_when_moving_up_through_a_shorter_line() {
    let mut e = editor();
    // Line 0: "2222222222x222" (x at col 10)
    // Line 1: "" (empty)
    // Line 2: "1111111111_111111111111" (_ at col 10)
    e.set_text("2222222222x222\n\n1111111111_111111111111");

    // Cursor starts at end of text.
    assert_eq!(e.cursor(), (2, 23));

    // Go to line 2, col 10 (on '_').
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..10 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (2, 10));

    // Up once: land on the empty line, clamped to col 0.
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (1, 0));

    // Up again: land on line 0 at col 10 (on 'x') — sticky col preserved.
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 10));
}

#[test]
fn preserves_target_column_when_moving_down_through_a_shorter_line() {
    let mut e = editor();
    e.set_text("1111111111_111\n\n2222222222x222222222222");

    // Navigate to (line 0, col 10) on '_'.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..10 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (0, 10));

    // Down through empty line.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (1, 0));

    // Down to line 2 — sticky col 10 lands on 'x'.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 10));
}

#[test]
fn handles_multiple_consecutive_up_down_movements() {
    let mut e = editor();
    e.set_text("1234567890\nab\ncd\nef\n1234567890");

    // Start at line 4, col 7.
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..7 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (4, 7));

    // Up through short lines — col clamped to each line's end, sticky preserved.
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (3, 2));
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (2, 2));
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (1, 2));
    e.handle_input(&Key::up());
    // Line 0 is long enough — restore to col 7.
    assert_eq!(e.cursor(), (0, 7));

    // Down through the same — sticky persists.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (1, 2));
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 2));
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (3, 2));
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (4, 7));
}

// ---------------------------------------------------------------------------
// Actions that reset the sticky column
// ---------------------------------------------------------------------------

#[test]
fn left_arrow_resets_sticky_column() {
    let mut e = editor();
    e.set_text("1234567890\n\n1234567890");

    // Line 2, col 5.
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..5 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (2, 5));

    // Up through empty line.
    e.handle_input(&Key::up()); // (1, 0)
    e.handle_input(&Key::up()); // (0, 5) sticky
    assert_eq!(e.cursor(), (0, 5));

    // Left: resets sticky.
    e.handle_input(&Key::left());
    assert_eq!(e.cursor(), (0, 4));

    // Down twice: new sticky captured from col 4.
    e.handle_input(&Key::down());
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 4));
}

#[test]
fn right_arrow_resets_sticky_column() {
    let mut e = editor();
    e.set_text("1234567890\n\n1234567890");

    // Navigate to line 0, col 5.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..5 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (0, 5));

    // Down through empty line — sticky 5.
    e.handle_input(&Key::down());
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 5));

    // Right: resets sticky.
    e.handle_input(&Key::right());
    assert_eq!(e.cursor(), (2, 6));

    // Up twice: new sticky from col 6.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 6));
}

#[test]
fn typing_resets_sticky_column() {
    let mut e = editor();
    e.set_text("1234567890\n\n1234567890");

    // Line 2, col 8.
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..8 {
        e.handle_input(&Key::right());
    }

    // Up through empty line — sticky 8.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 8));

    // Type: resets sticky.
    e.handle_input(&Key::char('X'));
    assert_eq!(e.cursor(), (0, 9));

    // Down twice: new sticky from col 9.
    e.handle_input(&Key::down());
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 9));
}

#[test]
fn backspace_resets_sticky_column() {
    let mut e = editor();
    e.set_text("1234567890\n\n1234567890");

    e.handle_input(&Key::ctrl('a'));
    for _ in 0..8 {
        e.handle_input(&Key::right());
    }

    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 8));

    // Backspace: resets sticky and deletes the char before cursor.
    e.handle_input(&Key::backspace());
    assert_eq!(e.cursor(), (0, 7));

    e.handle_input(&Key::down());
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 7));
}

#[test]
fn ctrl_a_resets_sticky_column() {
    let mut e = editor();
    e.set_text("1234567890\n\n1234567890");

    e.handle_input(&Key::ctrl('a'));
    for _ in 0..8 {
        e.handle_input(&Key::right());
    }

    // Up establishes sticky col 8.
    e.handle_input(&Key::up());

    // Ctrl+A: resets sticky.
    e.handle_input(&Key::ctrl('a'));
    assert_eq!(e.cursor(), (1, 0));

    // Up: new sticky from col 0.
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 0));
}

#[test]
fn ctrl_e_resets_sticky_column() {
    let mut e = editor();
    e.set_text("12345\n\n1234567890");

    e.handle_input(&Key::ctrl('a'));
    for _ in 0..3 {
        e.handle_input(&Key::right());
    }

    // Up through empty line — sticky 3.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 3));

    // Ctrl+E: resets sticky and moves to end of line.
    e.handle_input(&Key::ctrl('e'));
    assert_eq!(e.cursor(), (0, 5));

    // Down twice: new sticky from col 5.
    e.handle_input(&Key::down());
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 5));
}

#[test]
fn ctrl_left_word_movement_resets_sticky_column() {
    let mut e = editor();
    e.set_text("hello world\n\nhello world");

    // Starts at (2, 11).
    assert_eq!(e.cursor(), (2, 11));

    // Up through empty line — sticky 11.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 11));

    // Ctrl+Left: word movement, resets sticky.
    e.handle_input(&Key::ctrl_left());
    assert_eq!(e.cursor(), (0, 6));

    // Down twice: new sticky from col 6.
    e.handle_input(&Key::down());
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 6));
}

#[test]
fn ctrl_right_word_movement_resets_sticky_column() {
    let mut e = editor();
    e.set_text("hello world\n\nhello world");

    // Navigate to (0, 0).
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    e.handle_input(&Key::ctrl('a'));
    assert_eq!(e.cursor(), (0, 0));

    // Down through empty line — sticky 0.
    e.handle_input(&Key::down());
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 0));

    // Ctrl+Right: word movement, resets sticky.
    e.handle_input(&Key::ctrl_right());
    assert_eq!(e.cursor(), (2, 5));

    // Up twice: new sticky from col 5.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 5));
}

#[test]
fn undo_resets_sticky_column() {
    let mut e = editor();
    e.set_text("1234567890\n\n1234567890");

    // Go to line 0, col 8.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..8 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (0, 8));

    // Down through empty line — sticky 8.
    e.handle_input(&Key::down());
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 8));

    // Type X: moves cursor to col 9, clears sticky.
    e.handle_input(&Key::char('X'));
    assert_eq!(e.get_text(), "1234567890\n\n12345678X90");
    assert_eq!(e.cursor(), (2, 9));

    // Up twice: new sticky col 9.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 9));

    // Undo: restores text and cursor position, resets sticky.
    e.handle_input(&Key::ctrl('-'));
    assert_eq!(e.get_text(), "1234567890\n\n1234567890");
    assert_eq!(e.cursor(), (2, 8));

    // Up twice: new sticky from restored col 8, not old col 9.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 8));
}

// ---------------------------------------------------------------------------
// set_text and end-of-text capture
// ---------------------------------------------------------------------------

#[test]
fn set_text_resets_sticky_column() {
    let mut e = editor();
    e.set_text("1234567890\n\n1234567890");

    e.handle_input(&Key::ctrl('a'));
    for _ in 0..8 {
        e.handle_input(&Key::right());
    }
    e.handle_input(&Key::up());

    // set_text should wipe sticky.
    e.set_text("abcdefghij\n\nabcdefghij");
    assert_eq!(e.cursor(), (2, 10)); // At end of new text.

    // Up twice: new sticky from col 10.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 10));
}

#[test]
fn right_at_end_of_last_line_still_allows_vertical_move_to_land_on_col() {
    let mut e = editor();
    // Line 0: 20 chars with 'x' at col 10
    // Line 1: empty
    // Line 2: 10 chars ending at col 10
    e.set_text("111111111x1111111111\n\n333333333_");

    // Go to line 0 end.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    e.handle_input(&Key::ctrl('e'));
    assert_eq!(e.cursor(), (0, 20));

    // Down to line 2, clamped to col 10 (line end).
    e.handle_input(&Key::down());
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 10));

    // Right at end-of-text: doesn't move, but whatever the sticky was
    // is cleared so the next Up recaptures from col 10.
    e.handle_input(&Key::right());
    assert_eq!(e.cursor(), (2, 10));

    // Up twice: fresh sticky from col 10, lands on 'x' (col 10 of line 0).
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 10));
}

// ---------------------------------------------------------------------------
// Placeholder helper used by resize tests below to keep their setup
// terse. `position_cursor` via the public Up/Down path is the only way
// to anchor the cursor deterministically.
// ---------------------------------------------------------------------------

#[test]
fn position_cursor_helper_lands_where_requested() {
    // Sanity check on the local helper — if this breaks, the harder
    // tests in this file start misbehaving in confusing ways.
    let mut e = editor();
    e.set_text("aaaa\nbbbb\ncccc");
    position_cursor(&mut e, 1, 2);
    assert_eq!(e.cursor(), (1, 2));
}

// ---------------------------------------------------------------------------
// Tests that exercise the editor's layout_width cache
//
// The editor records the width every call to `render(width)` passed in
// and consults it for subsequent vertical-move math. These tests
// "resize" by calling `render(n)` between movements and assert that
// sticky-column semantics re-project onto the new wrap geometry.
// ---------------------------------------------------------------------------

#[test]
fn moves_through_wrapped_visual_lines_without_getting_stuck() {
    let mut e = editor_zero_pad();
    // Line 0: short (5 chars)
    // Line 1: 30 chars, wraps across multiple visual lines at narrow width.
    e.set_text("short\n123456789012345678901234567890");
    let _ = e.render(15); // layout_width = 14

    // Cursor starts at end of line 1.
    assert_eq!(e.cursor().0, 1);

    // Up steps through wrapped VLs of line 1, then reaches line 0.
    e.handle_input(&Key::up());
    assert_eq!(e.cursor().0, 1);
    e.handle_input(&Key::up());
    assert_eq!(e.cursor().0, 1);
    e.handle_input(&Key::up());
    assert_eq!(e.cursor().0, 0);
}

#[test]
fn resize_clamps_sticky_on_same_line() {
    let mut e = editor_zero_pad();
    e.set_text("12345678901234567890\n\n12345678901234567890");
    // layout_width default (80) — everything in one VL each.

    // Line 2, col 15.
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..15 {
        e.handle_input(&Key::right());
    }

    // Up through empty line — sticky col 15 established.
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 15));

    // Narrow render to simulate resize. content_width = 12,
    // layout_width = 11 (padding=0 reserves one for cursor marker).
    // Line 0 wraps into VLs [0..11, 11..20]; cursor moves from
    // visual col 15 on line 0's original VL to visual col 4 on the
    // second wrapped VL (15 - 11 = 4). Subsequent Down computations
    // operate against that reduced col.
    let _ = e.render(12);

    // Down onto empty line: sticky captured as 4.
    // Down onto line 2: applies sticky 4 → land at col 4.
    e.handle_input(&Key::down());
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 4));
}

#[test]
fn resize_clamps_sticky_when_target_is_different_line() {
    let mut e = editor_zero_pad();
    e.set_text("short\n12345678901234567890");

    // Line 1, col 15.
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..15 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (1, 15));

    // Up to line 0 — clamped to col 5 (end of "short"), sticky = 15.
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 5));

    // Narrow. layout_width = 9.
    let _ = e.render(10);

    // Down: preferred col 15 can't fit in VL of width 9 → case 4/5,
    // keep preferred, land at target-VL end (col 8, since first VL
    // of line 1 is not the last and clamps to length-1 = 8).
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (1, 8));

    // Up: back to line 0 col 5 (only 5 chars). Preferred still 15.
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 5));

    // Restore original width.
    let _ = e.render(80);

    // Down: preferred 15 now fits line 1 (20 chars) → case 3, land
    // exactly on preferred col 15.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (1, 15));
}

#[test]
fn rewrapped_lines_target_fits_current_visual_column() {
    let mut e = editor_zero_pad();
    e.set_text("abcdefghijklmnopqr\n123456789012345678");

    position_cursor(&mut e, 0, 18);
    assert_eq!(e.cursor(), (0, 18));

    // Narrow: layout_width = 9. Line 0 (18 chars) wraps into
    // VLs [0..9, 9..18]. Cursor is on the second VL (last of line 0)
    // at visual col 9 (= length).
    let _ = e.render(10);

    // Down: target VL(1, 0, 9) is not last → targetMax = 8.
    // cursorInMiddle (9 < 9) false. Not-in-middle branch with
    // targetTooShort (8 < 9) true → case 2, preferred=9, land at 8.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (1, 8));

    // Widen — layout_width = 80. VLs collapse back to full lines.
    let _ = e.render(80);

    // Up: currentVCol = 8, cursorInMiddle (8 < 18) yes → first
    // branch. Target fits (18 < 8 no) → case 1/6, clear preferred,
    // return 8.
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (0, 8));

    // Preferred was cleared by the rewrapped-fits branch — another
    // down captures fresh sticky at col 8, which fits. No clamp.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (1, 8));
}

#[test]
fn rewrapped_lines_target_shorter_than_current_visual_column() {
    let mut e = editor_zero_pad();
    e.set_text("abcdefghijklmnopqr\n123456789012345678\nab");

    position_cursor(&mut e, 0, 18);
    assert_eq!(e.cursor(), (0, 18));

    // Narrow to 10 (layoutWidth=9) and move down: clamps as above.
    let _ = e.render(10);
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (1, 8));

    // Widen to 80.
    let _ = e.render(80);

    // Down to short line "ab": target (2 chars) shorter than current
    // visual col 8 → case 2 (rewrapped-shorter): preferred replaced
    // with 8, land at col 2.
    e.handle_input(&Key::down());
    assert_eq!(e.cursor(), (2, 2));

    // Up to line 1: preferred 8 restores, case 3.
    e.handle_input(&Key::up());
    assert_eq!(e.cursor(), (1, 8));
}
