//! Behavior tests for `OverlayOptions`: widths (absolute and percent,
//! with `min_width`), anchor positioning, margins, explicit row/col,
//! offsets, `max_height`, and overlay stacking.

mod support;

use aj_tui::component::Component;
use aj_tui::impl_component_any;
use aj_tui::tui::{OverlayAnchor, OverlayMargin, OverlayOptions, SizeValue, Tui};

use support::{StaticLines, StaticOverlay, VirtualTerminal, render_now};

// ---------------------------------------------------------------------------
// Overlay fixtures
// ---------------------------------------------------------------------------

struct EmptyContent;
impl Component for EmptyContent {
    impl_component_any!();
    fn render(&mut self, _width: usize) -> Vec<String> {
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// Width
// ---------------------------------------------------------------------------

#[test]
fn overlay_uses_percentage_of_terminal_width() {
    let terminal = VirtualTerminal::new(100, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, recorded) = StaticOverlay::new(["test"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Percent(50.0)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    assert_eq!(*recorded.borrow(), Some(50));
}

#[test]
fn min_width_wins_when_percentage_would_be_smaller() {
    let terminal = VirtualTerminal::new(100, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, recorded) = StaticOverlay::new(["test"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Percent(10.0)),
            min_width: Some(30),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    assert_eq!(*recorded.borrow(), Some(30));
}

// ---------------------------------------------------------------------------
// Anchor
// ---------------------------------------------------------------------------

#[test]
fn anchor_top_left_puts_overlay_in_the_corner() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["TOP-LEFT"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert!(
        viewport[0].starts_with("TOP-LEFT"),
        "expected TOP-LEFT at row 0 col 0; got {:?}",
        viewport[0],
    );
}

#[test]
fn anchor_bottom_right_places_overlay_at_the_far_corner() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["BTM-RIGHT"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::BottomRight,
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    let last = &viewport[23];
    assert!(
        last.trim_end().ends_with("BTM-RIGHT"),
        "expected BTM-RIGHT at end of last row; got {:?}",
        last,
    );
}

#[test]
fn anchor_top_center_places_overlay_horizontally_centered_on_the_first_row() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["CENTERED"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::TopCenter,
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let row = &terminal.viewport()[0];
    let col = row
        .find("CENTERED")
        .expect("centered overlay should render");
    assert!(
        (30..=40).contains(&col),
        "expected CENTERED to land near column 35; got col {} in {:?}",
        col,
        row,
    );
}

// ---------------------------------------------------------------------------
// Margin
// ---------------------------------------------------------------------------

#[test]
fn uniform_margin_shifts_anchor_origin() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["MARGIN"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(10)),
            margin: OverlayMargin::uniform(5),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert_eq!(
        viewport[5].find("MARGIN"),
        Some(5),
        "expected MARGIN at row 5, col 5; got row={:?}",
        viewport[5],
    );
    assert!(!viewport[0].contains("MARGIN"));
    assert!(!viewport[4].contains("MARGIN"));
}

#[test]
fn per_side_margin_shifts_only_the_side_that_anchors() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["MARGIN"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(10)),
            margin: OverlayMargin {
                top: 2,
                left: 3,
                right: 0,
                bottom: 0,
            },
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert_eq!(
        viewport[2].find("MARGIN"),
        Some(3),
        "expected MARGIN at row 2 col 3; got row={:?}",
        viewport[2],
    );
}

// ---------------------------------------------------------------------------
// Offsets
// ---------------------------------------------------------------------------

#[test]
fn offset_x_and_offset_y_shift_from_the_anchor_origin() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["OFFSET"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(10)),
            offset_x: 10,
            offset_y: 5,
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert_eq!(
        viewport[5].find("OFFSET"),
        Some(10),
        "expected OFFSET at row 5 col 10; got row={:?}",
        viewport[5],
    );
}

// ---------------------------------------------------------------------------
// Absolute row/col
// ---------------------------------------------------------------------------

#[test]
fn absolute_row_and_col_override_the_anchor() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["ABSOLUTE"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::BottomRight,
            row: Some(SizeValue::Absolute(3)),
            col: Some(SizeValue::Absolute(5)),
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert_eq!(
        viewport[3].find("ABSOLUTE"),
        Some(5),
        "expected ABSOLUTE at row 3 col 5; got row={:?}",
        viewport[3],
    );
}

#[test]
fn row_zero_percent_anchors_to_the_top_when_used_as_an_override() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["TOP"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(10)),
            row: Some(SizeValue::Percent(0.0)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    assert!(
        terminal.viewport()[0].contains("TOP"),
        "expected TOP on row 0; viewport[0] = {:?}",
        terminal.viewport()[0],
    );
}

// ---------------------------------------------------------------------------
// Max height
// ---------------------------------------------------------------------------

#[test]
fn max_height_truncates_overlay_lines() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["Line 1", "Line 2", "Line 3", "Line 4", "Line 5"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            max_height: Some(SizeValue::Absolute(3)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let joined = terminal.viewport_text();
    assert!(joined.contains("Line 1"));
    assert!(joined.contains("Line 2"));
    assert!(joined.contains("Line 3"));
    assert!(!joined.contains("Line 4"));
    assert!(!joined.contains("Line 5"));
}

// ---------------------------------------------------------------------------
// Stacking + hiding
// ---------------------------------------------------------------------------

#[test]
fn later_overlays_paint_on_top_of_earlier_ones() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    tui.add_child(Box::new(EmptyContent));

    let (overlay1, _) = StaticOverlay::new(["FIRST-OVERLAY"]);
    tui.show_overlay(
        Box::new(overlay1),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(20)),
            ..Default::default()
        },
    );

    let (overlay2, _) = StaticOverlay::new(["SECOND"]);
    tui.show_overlay(
        Box::new(overlay2),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        },
    );

    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert!(
        viewport[0].starts_with("SECOND"),
        "SECOND should overwrite the start of FIRST; got {:?}",
        viewport[0],
    );
}

#[test]
fn overlays_at_different_positions_coexist() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    tui.add_child(Box::new(EmptyContent));

    let (a, _) = StaticOverlay::new(["TOP-LEFT"]);
    tui.show_overlay(
        Box::new(a),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(15)),
            ..Default::default()
        },
    );
    let (b, _) = StaticOverlay::new(["BTM-RIGHT"]);
    tui.show_overlay(
        Box::new(b),
        OverlayOptions {
            anchor: OverlayAnchor::BottomRight,
            width: Some(SizeValue::Absolute(15)),
            ..Default::default()
        },
    );

    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert!(viewport[0].contains("TOP-LEFT"));
    assert!(viewport[23].contains("BTM-RIGHT"));
}

#[test]
fn hiding_an_overlay_reveals_the_one_it_covered() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    tui.add_child(Box::new(EmptyContent));

    let (first, _) = StaticOverlay::new(["FIRST"]);
    tui.show_overlay(
        Box::new(first),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        },
    );

    let (second, _) = StaticOverlay::new(["SECOND"]);
    let top_handle = tui.show_overlay(
        Box::new(second),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        },
    );

    render_now(&mut tui);
    assert!(
        terminal.viewport()[0].contains("SECOND"),
        "SECOND should be visible initially",
    );

    tui.hide_overlay(&top_handle);
    render_now(&mut tui);

    assert!(
        terminal.viewport()[0].contains("FIRST"),
        "FIRST should be visible after hiding SECOND; got {:?}",
        terminal.viewport()[0],
    );
}

// ---------------------------------------------------------------------------
// Safety: no crash on too-wide or complex input
// ---------------------------------------------------------------------------

#[test]
fn overlay_with_lines_wider_than_declared_width_does_not_crash() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    // Intentionally exercising a compositor path that may leak
    // over-width rows into the final frame; the assertion is just
    // "render completes", so opt out of the strict line-width check.
    tui.set_strict_line_widths(false);
    let (overlay, _) = StaticOverlay::new([&"X".repeat(100)]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(20)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert_eq!(viewport.len(), 24);
}

// ---------------------------------------------------------------------------
// Base content + overlay
// ---------------------------------------------------------------------------

#[test]
fn overlay_composites_onto_styled_base_content_without_crashing() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    // Base content that fills every row with a bold red line.
    let styled_line = format!("\x1b[1m\x1b[38;2;255;0;0m{}\x1b[0m", "X".repeat(80));
    tui.add_child(Box::new(StaticLines::new(vec![styled_line.clone(); 3])));

    let (overlay, _) = StaticOverlay::new(["OVERLAY"]);
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::Center,
            width: Some(SizeValue::Absolute(20)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    assert!(
        terminal.viewport().iter().any(|r| r.contains("OVERLAY")),
        "overlay text should be visible on top of styled base content",
    );
}

// ---------------------------------------------------------------------------
// Overflow / boundary defensive behavior
//
// These guard against a class of crashes where declared width, wide
// characters, or ANSI-laden base content interact with the overlay
// compositor. The assertion surface is deliberately loose ("should
// not crash and should produce some output") because the exact output
// depends on the wrapper's clipping/padding choices — the interesting
// property is that nothing panics.
// ---------------------------------------------------------------------------

#[test]
fn overlay_with_complex_ansi_content_does_not_crash() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    // Intentionally exercising compositor paths with SGR+OSC-heavy
    // content where the width accounting can drift; see the companion
    // tests above.
    tui.set_strict_line_widths(false);

    // Mix of SGR (bg + fg), OSC 8 hyperlink, and long tail content —
    // exactly the kind of line streamed tool output could produce.
    let mut complex = String::new();
    complex
        .push_str("\x1b[48;2;40;50;40m \x1b[38;2;128;128;128mSome styled content\x1b[39m\x1b[49m");
    complex.push_str("\x1b]8;;http://example.com\x07link\x1b]8;;\x07");
    for _ in 0..10 {
        complex.push_str(" more content ");
    }
    let (overlay, _) = StaticOverlay::new([complex.clone(), complex.clone(), complex]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(60)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert!(!viewport.is_empty());
}

#[test]
fn overlay_with_wide_characters_at_boundary_does_not_crash() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    // Mix of CJK (width 2) chars. Odd declared width forces the
    // compositor to handle a cell-boundary split on a wide char.
    let wide = "中文日本語한글テスト漢字";
    let (overlay, _) = StaticOverlay::new([wide]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(15)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    assert!(!terminal.viewport().is_empty());
}

#[test]
fn overlay_positioned_at_right_edge_does_not_crash() {
    // col 60 + width 20 == col 80 (the terminal's right edge). Overlay
    // content is much wider than 20 chars; the interesting property is
    // that the compositor doesn't panic when positioning wide content
    // right up against the edge. Clipping behavior is a separate
    // property.
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    // Intentionally feeding the compositor wider-than-declared content;
    // see the companion test above.
    tui.set_strict_line_widths(false);
    let (overlay, _) = StaticOverlay::new(["X".repeat(50)]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            col: Some(SizeValue::Absolute(60)),
            width: Some(SizeValue::Absolute(20)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    assert!(!terminal.viewport().is_empty());
}

#[test]
fn overlay_on_base_content_with_osc_hyperlinks_does_not_crash() {
    // Regression guard: base content containing OSC 8 hyperlink
    // sequences used to trip the overlay slicing path.
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    struct HyperlinkContent;
    impl Component for HyperlinkContent {
        impl_component_any!();
        fn render(&mut self, width: usize) -> Vec<String> {
            let link = "\x1b]8;;file:///path/to/file.ts\x07file.ts\x1b]8;;\x07";
            let padding = "X".repeat(width.saturating_sub(30));
            let line = format!("See {} for details {}", link, padding);
            vec![line.clone(), line.clone(), line]
        }
    }

    tui.add_child(Box::new(HyperlinkContent));
    let (overlay, _) = StaticOverlay::new(["OVERLAY-TEXT"]);
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::Center,
            width: Some(SizeValue::Absolute(20)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    assert!(!terminal.viewport().is_empty());
}

// ---------------------------------------------------------------------------
// Percentage positioning
// ---------------------------------------------------------------------------

#[test]
fn row_and_col_percentages_center_the_overlay() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["PCT"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(10)),
            row: Some(SizeValue::Percent(50.0)),
            col: Some(SizeValue::Percent(50.0)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    // PCT should be roughly in the middle row of a 24-row terminal.
    let viewport = terminal.viewport();
    let found = viewport
        .iter()
        .position(|row| row.contains("PCT"))
        .expect("overlay should be placed somewhere in the viewport");
    assert!(
        (10..=13).contains(&found),
        "expected overlay near vertical center (rows 10-13), found on row {}",
        found,
    );
}

#[test]
fn row_one_hundred_percent_places_overlay_at_the_last_row() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["BOTTOM"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(10)),
            row: Some(SizeValue::Percent(100.0)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert!(
        viewport[23].contains("BOTTOM"),
        "expected BOTTOM on last row; got {:?}",
        viewport[23],
    );
}

// ---------------------------------------------------------------------------
// max_height as a percentage
// ---------------------------------------------------------------------------

#[test]
fn max_height_as_percentage_truncates_to_that_fraction_of_rows() {
    // 10 rows in a 10-row terminal, 50% max → 5 lines visible.
    let terminal = VirtualTerminal::new(80, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) =
        StaticOverlay::new(["L1", "L2", "L3", "L4", "L5", "L6", "L7", "L8", "L9", "L10"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            max_height: Some(SizeValue::Percent(50.0)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let content = terminal.viewport_text();
    assert!(content.contains("L1"));
    assert!(content.contains("L5"));
    assert!(
        !content.contains("L6"),
        "lines beyond the 50% cap should not render; got\n{}",
        content,
    );
}

// ---------------------------------------------------------------------------
// hide_topmost_overlay reveals the next one in stack order
// ---------------------------------------------------------------------------

#[test]
fn hide_topmost_overlay_reveals_the_one_beneath() {
    // Distinct from hiding_an_overlay_reveals_the_one_it_covered (which
    // uses a specific handle): hide_topmost_overlay is the "pop the
    // modal" operation driven by an Esc handler, and must reveal the
    // next capturing overlay in z-order.
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    tui.add_child(Box::new(EmptyContent));

    let (first, _) = StaticOverlay::new(["FIRST"]);
    tui.show_overlay(
        Box::new(first),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        },
    );
    let (second, _) = StaticOverlay::new(["SECOND"]);
    tui.show_overlay(
        Box::new(second),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(10)),
            ..Default::default()
        },
    );
    render_now(&mut tui);
    assert!(terminal.viewport()[0].starts_with("SECOND"));

    // Pop the topmost.
    assert!(tui.hide_topmost_overlay());
    render_now(&mut tui);

    assert!(
        terminal.viewport()[0].starts_with("FIRST"),
        "after popping SECOND, FIRST should be visible; got {:?}",
        terminal.viewport()[0],
    );
}

// ---------------------------------------------------------------------------
// Height-change stability
// ---------------------------------------------------------------------------

#[test]
fn explicit_row_pins_the_overlay_top_across_content_height_changes() {
    // Settings-demo case: an overlay whose content height fluctuates
    // (e.g. a search-enabled SettingsList) re-positions on every
    // frame under an anchor-based vertical placement, so the top row
    // visibly jumps up and down as the user types. An overlay built
    // with an explicit `row` uses that row as its top, regardless of
    // content height, so the top stays pinned and only the bottom
    // pulls in when content shrinks. Pin this as a regression guard
    // for the inline-settings-like usage pattern.
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct FlexOverlay {
        count: Rc<RefCell<usize>>,
    }
    impl Component for FlexOverlay {
        impl_component_any!();
        fn render(&mut self, _width: usize) -> Vec<String> {
            (0..*self.count.borrow())
                .map(|i| format!("row{}", i))
                .collect()
        }
    }

    let terminal = VirtualTerminal::new(40, 20);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let overlay = FlexOverlay {
        count: Rc::new(RefCell::new(10)),
    };
    let count = Rc::clone(&overlay.count);
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(10)),
            row: Some(SizeValue::Absolute(5)),
            anchor: OverlayAnchor::TopCenter,
            ..Default::default()
        },
    );
    render_now(&mut tui);
    let top_row_tall: String = terminal.viewport()[5].clone();
    assert!(
        top_row_tall.contains("row0"),
        "overlay's first row should land at terminal row 5 when content \
         is tall; row 5 = {:?}",
        top_row_tall,
    );

    // Shrink content. Top row must still be at row 5.
    *count.borrow_mut() = 3;
    render_now(&mut tui);
    let top_row_short: String = terminal.viewport()[5].clone();
    assert!(
        top_row_short.contains("row0"),
        "overlay's first row must stay pinned at terminal row 5 after \
         content shrinks; row 5 = {:?}",
        top_row_short,
    );
    // Grow back. Same invariant.
    *count.borrow_mut() = 8;
    render_now(&mut tui);
    let top_row_medium: String = terminal.viewport()[5].clone();
    assert!(
        top_row_medium.contains("row0"),
        "overlay's first row must stay pinned at terminal row 5 after \
         content grows back; row 5 = {:?}",
        top_row_medium,
    );
}

#[test]
fn center_anchor_shifts_top_row_as_content_height_changes() {
    // Companion negative test to `explicit_row_pins_...`. Documents
    // that `OverlayAnchor::Center` *does* move the top row when
    // content height changes (because it re-centers). Callers who
    // want a stable top should use an explicit `row` or a Top-anchor
    // variant instead.
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct FlexOverlay {
        count: Rc<RefCell<usize>>,
    }
    impl Component for FlexOverlay {
        impl_component_any!();
        fn render(&mut self, _width: usize) -> Vec<String> {
            (0..*self.count.borrow())
                .map(|i| format!("row{}", i))
                .collect()
        }
    }

    let terminal = VirtualTerminal::new(40, 20);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let overlay = FlexOverlay {
        count: Rc::new(RefCell::new(10)),
    };
    let count = Rc::clone(&overlay.count);
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(10)),
            anchor: OverlayAnchor::Center,
            ..Default::default()
        },
    );
    render_now(&mut tui);

    // Find where the overlay's first row ("row0") currently lives.
    let find_row0 = |vp: &[String]| -> usize {
        vp.iter()
            .position(|line| line.contains("row0"))
            .expect("overlay should render somewhere")
    };
    let tall_top = find_row0(&terminal.viewport());

    // Shrink content. With Center anchor, the overlay re-centers and
    // the top row drifts downward by ~ (old_height - new_height) / 2.
    *count.borrow_mut() = 2;
    render_now(&mut tui);
    let short_top = find_row0(&terminal.viewport());
    assert!(
        short_top > tall_top,
        "Center anchor should push the top row downward when content \
         shrinks; tall_top={} short_top={} (expected short_top > tall_top)",
        tall_top,
        short_top,
    );
}

// ---------------------------------------------------------------------------
// Bottom / right clamp (extends past terminal bounds)
// ---------------------------------------------------------------------------

#[test]
fn absolute_row_past_bottom_clamps_overlay_so_it_fits() {
    // An explicit `row` deeper than the terminal would allow for the
    // overlay's content should be clamped so the overlay's bottom
    // row stays inside the visible viewport. Without the clamp, the
    // overlay extends past the terminal bottom and rows that should
    // be visible never appear (the compositor drops them silently
    // because their target row is beyond `lines.len()`).
    //
    // Setup: 10-row terminal, 5-row overlay, requested row=8. The
    // upper bound is `term_height - margin.bottom - effective_height
    // = 10 - 0 - 5 = 5`, so the overlay should land at row 5 with
    // its bottom row on the last visible line (row 9).
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["A", "B", "C", "D", "E"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(5)),
            row: Some(SizeValue::Absolute(8)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    // First row of overlay should be at row 5 (10 - 5), last at row 9.
    let first = viewport
        .iter()
        .position(|l| l.contains('A'))
        .expect("expected overlay row A somewhere in the viewport");
    assert_eq!(
        first, 5,
        "overlay should be clamped to row 5 so all 5 rows fit inside \
         the 10-row terminal; got first={first}",
    );
    assert!(
        viewport[9].contains('E'),
        "expected last overlay row E at the bottom of the viewport \
         (row 9); got {:?}",
        viewport[9],
    );
}

#[test]
fn absolute_col_past_right_clamps_overlay_so_it_fits() {
    // Mirror of `absolute_row_past_bottom_clamps_overlay_so_it_fits`
    // for columns. 20-col terminal, 10-col overlay, requested
    // col=15: upper bound is `20 - 0 - 10 = 10`, so the overlay
    // should land at col 10.
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["XXXXXXXXXX"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(10)),
            row: Some(SizeValue::Absolute(0)),
            col: Some(SizeValue::Absolute(15)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    let pos = viewport[0]
        .find("XXXXXXXXXX")
        .expect("expected overlay text on row 0");
    assert_eq!(
        pos, 10,
        "overlay should be clamped to col 10 so all 10 cols fit \
         inside the 20-col terminal; got pos={pos}",
    );
}

#[test]
fn offset_y_past_bottom_clamps_overlay_so_it_fits() {
    // The same clamp must apply when the overflow comes from
    // `offset_y` rather than an explicit `row`. Anchor TopLeft puts
    // the overlay at row 0; offset_y=20 would push it to row 20 on
    // a 10-row terminal, but the clamp brings it back so the
    // 3-row overlay's bottom row sits at the viewport bottom.
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["1", "2", "3"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(5)),
            offset_y: 20,
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    let first = viewport
        .iter()
        .position(|l| l.contains('1'))
        .expect("expected overlay somewhere in the viewport");
    assert_eq!(
        first, 7,
        "overlay should be clamped to row 7 (= 10 - 3) so all three \
         rows fit; got first={first}",
    );
}

#[test]
fn bottom_margin_pushes_overlay_above_its_extent() {
    // A bottom margin reserves space below the overlay. With a
    // 10-row terminal, 3-row overlay, `margin.bottom = 2`, and an
    // explicit `row: Absolute(9)`, the overlay's bottom row should
    // not encroach on the 2-row reserved area at the bottom.
    //
    // Upper bound = 10 - 2 - 3 = 5, so the overlay lands at row 5
    // with its last row at row 7. Rows 8 and 9 stay clear.
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let (overlay, _) = StaticOverlay::new(["P", "Q", "R"]);

    tui.add_child(Box::new(EmptyContent));
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(5)),
            row: Some(SizeValue::Absolute(9)),
            margin: OverlayMargin {
                bottom: 2,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert!(
        viewport[5].contains('P'),
        "expected first overlay row P at row 5; got {:?}",
        viewport[5],
    );
    assert!(
        viewport[7].contains('R'),
        "expected last overlay row R at row 7; got {:?}",
        viewport[7],
    );
    assert!(
        viewport[8].trim().is_empty() && viewport[9].trim().is_empty(),
        "rows 8 and 9 are reserved by margin.bottom = 2; got {:?} / {:?}",
        viewport[8],
        viewport[9],
    );
}
