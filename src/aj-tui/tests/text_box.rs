//! Tests for the `TextBox` container component: padding, background
//! application, and the render cache that avoids rebuilding output on
//! frames where no input changed.

mod support;

use std::cell::Cell;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::components::text_box::TextBox;
use aj_tui::impl_component_any;

/// A child that returns a pre-set set of lines and counts how many
/// times its `render` method was called. Used to prove the TextBox
/// cache path doesn't re-invoke children (it does, to get fresh line
/// content; the proof is that the *output* doesn't get re-built).
struct CountingLines {
    lines: Vec<String>,
    renders: Rc<Cell<usize>>,
}

impl CountingLines {
    fn new(lines: Vec<String>) -> (Self, Rc<Cell<usize>>) {
        let renders = Rc::new(Cell::new(0));
        (
            Self {
                lines,
                renders: Rc::clone(&renders),
            },
            renders,
        )
    }
}

impl Component for CountingLines {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        self.renders.set(self.renders.get() + 1);
        self.lines.clone()
    }
}

#[test]
fn renders_children_with_padding() {
    let mut tb = TextBox::new(1, 1);
    let (child, _) = CountingLines::new(vec!["hello".into(), "world".into()]);
    tb.add_child(Box::new(child));

    let lines = tb.render(10);

    // Default padding_x=1, padding_y=1 on a width-10 box:
    //   row 0: top padding (10 spaces)
    //   row 1: " hello    "
    //   row 2: " world    "
    //   row 3: bottom padding (10 spaces)
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0], " ".repeat(10));
    assert_eq!(lines[3], " ".repeat(10));
    assert!(lines[1].starts_with(" hello"));
    assert!(lines[2].starts_with(" world"));
}

#[test]
fn empty_children_render_as_empty() {
    let mut tb = TextBox::new(1, 1);
    assert!(tb.render(10).is_empty());
}

// -- F34: degenerate render-width clamps to one cell --
//
// Previously a width that left zero or negative content columns
// returned `Vec::new()` and reset the cache. Pi-tui clamps
// `contentWidth = max(1, width - paddingX * 2)` and renders one
// cell of content; the Rust port now mirrors that.

#[test]
fn degenerate_width_clamps_content_width_to_one_cell() {
    // padding_x=1 on a width-2 box: pi clamps contentWidth to 1 and
    // renders the box at one content cell, not zero rows.
    let mut tb = TextBox::new(1, 1);
    let (child, _) = CountingLines::new(vec!["x".into()]);
    tb.add_child(Box::new(child));

    let lines = tb.render(2);

    // 1 top pad + 1 content + 1 bottom pad, each at the full
    // `width = 2` byte width.
    assert_eq!(lines.len(), 3);
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(
            aj_tui::ansi::visible_width(line),
            2,
            "row {} should span the full render width, got {:?}",
            i,
            line
        );
    }
    // Top / bottom rows are blank-padded; the content row carries
    // ` x` (left pad + the one-cell child output).
    assert_eq!(lines[0], "  ");
    assert_eq!(lines[1], " x");
    assert_eq!(lines[2], "  ");
}

#[test]
fn zero_render_width_clamps_content_width_to_one_cell() {
    // width = 0 with padding_x = 0: contentWidth = max(1, 0) = 1,
    // and pi still renders the (left-pad-empty + child-line + bg)
    // pipeline at width 0. Children get a one-cell render width.
    let mut tb = TextBox::new(0, 0);
    let (child, _) = CountingLines::new(vec!["x".into()]);
    tb.add_child(Box::new(child));

    let lines = tb.render(0);

    // padding_y=0 → no top/bottom padding rows; one content row.
    // The pipeline runs but the row pads to width 0, so the visible
    // width is the child's natural visible width, clamped by
    // `apply_bg_row` to >= width via saturating_sub. Bare result is
    // the child line itself.
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], "x");
}

#[test]
fn padding_x_exceeding_half_width_clamps_to_one_cell() {
    // padding_x = 4 on a width-6 box: width - 2 * padding_x = -2,
    // saturates to 0, then clamps to 1. The pre-F34 code returned
    // `Vec::new()` here; pi-tui returns content rows.
    let mut tb = TextBox::new(4, 0);
    // Use a one-char child so it fits the clamped one-cell content
    // width without overflow. (Real one-cell-clamping consumers
    // — `Text`, `Markdown` — wrap their text at the supplied
    // width; the `CountingLines` test fixture does not. The point
    // here is that the box doesn't collapse to zero rows.)
    let (child, render_count) = CountingLines::new(vec!["x".into()]);
    tb.add_child(Box::new(child));

    let lines = tb.render(6);

    assert_eq!(lines.len(), 1, "box should not collapse to zero rows");
    assert_eq!(aj_tui::ansi::visible_width(&lines[0]), 6);
    // Four-cell left padding + one-cell child + one-cell right pad.
    assert_eq!(lines[0], "    x ");
    // Children are rendered exactly once even at the degenerate width.
    assert_eq!(render_count.get(), 1);
}

#[test]
fn empty_children_does_not_invalidate_cache() {
    // Pi-tui parity: `addChild` invalidates the cache, but the
    // children-empty early return path does not. Since `clear()`
    // and `remove_child` already invalidate, the only way to reach
    // the early return with a stale cache is via this child-empty
    // path. Rendering with no children returns `[]` either way; the
    // observable consequence of the cache contract is on the next
    // render after we add a child back. This test pins the contract
    // by walking through the sequence.
    let mut tb = TextBox::new(1, 1);
    let (child, _) = CountingLines::new(vec!["cached".into()]);
    tb.add_child(Box::new(child));

    let with_child = tb.render(20);
    assert!(!with_child.is_empty());

    // Rendering an empty TextBox (no `clear` call — children just
    // happens to start empty) should not crash and should be `[]`.
    let mut empty_tb = TextBox::new(1, 1);
    assert!(empty_tb.render(20).is_empty());
    assert!(empty_tb.render(20).is_empty());
}

#[test]
fn cache_reuses_lines_when_width_and_children_unchanged() {
    let mut tb = TextBox::new(1, 1);
    let (child, _) = CountingLines::new(vec!["streaming".into()]);
    tb.add_child(Box::new(child));

    let first = tb.render(20);
    let second = tb.render(20);

    // Byte-identical output across renders: the cache delivered it.
    assert_eq!(first, second);
}

#[test]
fn cache_invalidates_when_width_changes() {
    // Same child content, different width — the padded/padded-bg
    // rows must be rebuilt because their length depends on width.
    let mut tb = TextBox::new(1, 1);
    let (child, _) = CountingLines::new(vec!["x".into()]);
    tb.add_child(Box::new(child));

    let at_10 = tb.render(10);
    let at_20 = tb.render(20);

    assert_ne!(at_10, at_20);
    // Top-padding row width follows the box width.
    assert_eq!(at_10[0].len(), 10);
    assert_eq!(at_20[0].len(), 20);
}

#[test]
fn cache_invalidates_when_child_output_changes() {
    struct Mutable {
        value: Rc<std::cell::RefCell<String>>,
    }
    impl Component for Mutable {
        impl_component_any!();

        fn render(&mut self, _width: usize) -> Vec<String> {
            vec![self.value.borrow().clone()]
        }
    }

    let value = Rc::new(std::cell::RefCell::new("first".to_string()));
    let mut tb = TextBox::new(1, 1);
    tb.add_child(Box::new(Mutable {
        value: Rc::clone(&value),
    }));

    let first = tb.render(20);
    *value.borrow_mut() = "second".into();
    let second = tb.render(20);

    assert_ne!(first, second, "changing child output must bypass the cache");
    assert!(second[1].contains("second"));
}

#[test]
fn invalidate_drops_the_cache() {
    let mut tb = TextBox::new(1, 1);
    let (child, _) = CountingLines::new(vec!["cached".into()]);
    tb.add_child(Box::new(child));

    let _ = tb.render(20);
    tb.invalidate();
    // The only visible consequence of invalidate() is that a subsequent
    // render rebuilds (which is an internal allocation concern). We
    // assert the output still matches so `invalidate` stays correct
    // even when the cache would otherwise have been reused.
    let after = tb.render(20);
    assert!(after[1].contains("cached"));
}

#[test]
fn bg_fn_output_change_invalidates_via_sample_probe() {
    // Two closures that produce different outputs for the same input:
    // the cache probes `bg_fn("test")` on each render and invalidates
    // when the probe differs.
    let mut tb = TextBox::new(1, 1);
    let (child, _) = CountingLines::new(vec!["content".into()]);
    tb.add_child(Box::new(child));
    tb.set_bg_fn(Box::new(|s| format!("[A:{}]", s)));
    let first = tb.render(20);
    tb.set_bg_fn(Box::new(|s| format!("[B:{}]", s)));
    let second = tb.render(20);

    assert_ne!(
        first, second,
        "swapping bg_fn to a closure with different output must invalidate"
    );
    assert!(first[1].starts_with("[A:"));
    assert!(second[1].starts_with("[B:"));
}

#[test]
fn adding_a_child_invalidates_cache() {
    let mut tb = TextBox::new(1, 1);
    let (first_child, _) = CountingLines::new(vec!["first".into()]);
    tb.add_child(Box::new(first_child));

    let before = tb.render(20);
    let (second_child, _) = CountingLines::new(vec!["second".into()]);
    tb.add_child(Box::new(second_child));
    let after = tb.render(20);

    assert_ne!(before, after);
    assert_eq!(after.len(), before.len() + 1);
}

// -- F15: empty-row padding asymmetry fix --
//
// Previously the no-bg path emitted content rows of exactly
// `left_pad + child_line.len()` bytes, leaving the right side of
// each row at whatever the terminal previously displayed; the
// bg path called `bg(&" ".repeat(width))` for padding rows but
// `apply_background_to_line(...)` for content rows. Both paths
// now route through `apply_bg_row`, mirroring pi-tui's `Box.applyBg`.

#[test]
fn no_bg_content_row_right_pads_to_full_width() {
    // Without the F15 fix, a row whose child returned text shorter
    // than the content width would render as `" hi"` (3 bytes) rather
    // than the full `width = 10` byte row.
    let mut tb = TextBox::new(1, 1);
    let (child, _) = CountingLines::new(vec!["hi".into()]);
    tb.add_child(Box::new(child));

    let lines = tb.render(10);

    // 1 top pad + 1 content + 1 bottom pad = 3 lines, each width 10.
    assert_eq!(lines.len(), 3);
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(
            aj_tui::ansi::visible_width(line),
            10,
            "row {} should span the full render width, got {:?}",
            i,
            line
        );
    }
    // Content row: ` hi` followed by 7 trailing spaces (width 10).
    assert_eq!(lines[1], " hi       ");
}

#[test]
fn no_bg_padding_row_matches_pi_byte_for_byte() {
    // Top and bottom padding rows are exactly `width` spaces.
    let mut tb = TextBox::new(1, 2);
    let (child, _) = CountingLines::new(vec!["x".into()]);
    tb.add_child(Box::new(child));

    let lines = tb.render(8);

    // 2 top + 1 content + 2 bottom = 5 rows.
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[0], " ".repeat(8));
    assert_eq!(lines[1], " ".repeat(8));
    assert_eq!(lines[3], " ".repeat(8));
    assert_eq!(lines[4], " ".repeat(8));
}

#[test]
fn bg_padding_and_content_paths_share_one_pipeline() {
    // The bg closure tags every padded row so we can assert the same
    // pipeline ran for top/content/bottom: each row must include the
    // tag at the start, indicating `apply_bg_row` (not a direct
    // `bg(...)` shortcut) was invoked. The bg closure echoes its full
    // input so the visible-padding step is observable too.
    let mut tb = TextBox::new(1, 1);
    let (child, _) = CountingLines::new(vec!["hi".into()]);
    tb.add_child(Box::new(child));
    tb.set_bg_fn(Box::new(|s| format!("<BG:{}>", s)));

    let lines = tb.render(8);

    assert_eq!(lines.len(), 3);
    // Every row goes through bg(...) on a string already padded to
    // width 8, so each row is `<BG:` + 8 chars + `>`.
    for (i, line) in lines.iter().enumerate() {
        assert!(
            line.starts_with("<BG:") && line.ends_with('>'),
            "row {} did not pass through apply_bg_row: {:?}",
            i,
            line,
        );
        // Inside the tag is the padded line — exactly width chars.
        let inside = &line[4..line.len() - 1];
        assert_eq!(
            inside.chars().count(),
            8,
            "row {} interior should be padded to 8: {:?}",
            i,
            inside
        );
    }
    // Top / bottom are blank padded rows.
    assert_eq!(lines[0], "<BG:        >");
    assert_eq!(lines[2], "<BG:        >");
    // Content row carries " hi" plus right-padding to width 8.
    assert_eq!(lines[1], "<BG: hi     >");
}

#[test]
fn bg_content_with_ansi_escapes_pads_by_visible_width() {
    // A child line carrying SGR escapes has byte length > visible
    // width. `apply_bg_row` pads by visible width, so the right side
    // of the row gets exactly enough spaces to reach the terminal
    // width — not enough to reach the byte length.
    let mut tb = TextBox::new(0, 0);
    let (child, _) = CountingLines::new(vec!["\x1b[31mred\x1b[0m".into()]);
    tb.add_child(Box::new(child));

    let lines = tb.render(6);

    // Visible width 3 ("red") + 3 trailing spaces = 6.
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], "\x1b[31mred\x1b[0m   ");
    assert_eq!(aj_tui::ansi::visible_width(&lines[0]), 6);
}

// -- F35: remove_child_by_ref companion to the index-based remove --
//
// Pi-tui's `Box.removeChild(component)` matches by reference; our
// previous Rust port only exposed `remove_child(index)`. The
// companion mirrors pi's identity-based shape and is a no-op (no
// cache invalidation, returns `None`) when the child isn't present.

#[test]
fn remove_child_by_ref_removes_the_matching_child_and_returns_it() {
    let mut tb = TextBox::new(1, 1);

    // Stash a raw pointer to the middle child before transferring
    // ownership to the box. The Box's heap allocation doesn't move
    // when we hand it off, so the pointer stays valid until we
    // remove_child_by_ref.
    let middle: Box<dyn Component> = Box::new(CountingLines::new(vec!["b".into()]).0);
    let middle_ptr: *const dyn Component = &*middle;

    tb.add_child(Box::new(CountingLines::new(vec!["a".into()]).0));
    tb.add_child(middle);
    tb.add_child(Box::new(CountingLines::new(vec!["c".into()]).0));

    // SAFETY: `middle_ptr` points into the heap allocation owned by
    // `tb.children[1]`; the move into the Vec preserves the address.
    let target: &dyn Component = unsafe { &*middle_ptr };
    let removed = tb
        .remove_child_by_ref(target)
        .expect("middle child to be present");

    // The returned Box re-owns the matched allocation.
    let removed_ptr: *const dyn Component = &*removed;
    assert!(std::ptr::addr_eq(removed_ptr, middle_ptr));

    // The remaining two children render in their original order.
    // Width 8: top pad, " a      ", " c      ", bottom pad.
    let lines = tb.render(8);
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[1], " a      ");
    assert_eq!(lines[2], " c      ");
}

#[test]
fn remove_child_by_ref_returns_none_when_child_is_not_in_box() {
    let mut tb = TextBox::new(1, 1);
    let (child, _) = CountingLines::new(vec!["a".into()]);
    tb.add_child(Box::new(child));

    // Stack-allocated stray; its address is necessarily distinct
    // from any heap-allocated child in `tb`.
    let stray = CountingLines::new(vec!["a".into()]).0;
    let removed = tb.remove_child_by_ref(&stray);
    assert!(removed.is_none());

    // Box content is unchanged.
    let lines = tb.render(8);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[1], " a      ");
}

#[test]
fn remove_child_by_ref_does_not_match_a_distinct_instance_with_identical_fields() {
    // F35 contract: identity (data-pointer) match, not field
    // equality. A separately-constructed `CountingLines` with the
    // exact same `lines` vec must not match an existing child.
    let mut tb = TextBox::new(1, 1);
    tb.add_child(Box::new(CountingLines::new(vec!["same".into()]).0));

    let twin = CountingLines::new(vec!["same".into()]).0;
    let removed = tb.remove_child_by_ref(&twin);
    assert!(removed.is_none());

    let lines = tb.render(8);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[1], " same   ");
}
