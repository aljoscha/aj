//! End-to-end tests for the rendering engine, using a `VirtualTerminal` as
//! the sink. Assertions check the viewport seen by the (virtual) user and
//! observability counters exposed by the `Tui` (e.g. `full_redraws()`,
//! `max_lines_rendered()`).
//!
//! Engine-feature coverage that's intentionally left for future work
//! (viewport-shift-aware full redraws, fine-grained Termux resize
//! gating) will land alongside the features themselves; where a test
//! file already guards one of those axes, its file-level doc comment
//! calls out the gap explicitly.

mod support;

use aj_tui::tui::Tui;

use support::{MutableLines, VirtualTerminal, wait_for_render};

fn strings<I, S>(lines: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    lines.into_iter().map(Into::into).collect()
}

fn viewport_contains(viewport: &[String], row: usize, needle: &str) -> bool {
    viewport
        .get(row)
        .map(|line| line.contains(needle))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// TUI resize handling
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn height_change_triggers_full_redraw() {
    let _guard = support::with_env(&[("TERMUX_VERSION", None)]);

    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    component.set(["Line 0", "Line 1", "Line 2"]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);
    let initial = tui.full_redraws();

    terminal.resize(40, 15);
    wait_for_render(&mut tui);

    assert!(
        tui.full_redraws() > initial,
        "height change should trigger a full redraw",
    );

    let viewport = terminal.viewport();
    assert!(
        viewport_contains(&viewport, 0, "Line 0"),
        "content preserved after height change; got {:?}",
        viewport.first(),
    );
}

#[test]
fn width_change_triggers_full_redraw() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    component.set(["Line 0", "Line 1", "Line 2"]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);
    let initial = tui.full_redraws();

    terminal.resize(60, 10);
    wait_for_render(&mut tui);

    assert!(
        tui.full_redraws() > initial,
        "width change should trigger a full redraw",
    );
}

// ---------------------------------------------------------------------------
// TUI content shrinkage
// ---------------------------------------------------------------------------

#[test]
fn clears_empty_rows_when_content_shrinks_significantly() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.set_clear_on_shrink(true);
    let component = MutableLines::new();
    component.set(["Line 0", "Line 1", "Line 2", "Line 3", "Line 4", "Line 5"]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);
    let initial = tui.full_redraws();

    component.set(["Line 0", "Line 1"]);
    tui.request_render();
    wait_for_render(&mut tui);

    assert!(
        tui.full_redraws() > initial,
        "content shrinkage should trigger a full redraw",
    );

    let viewport = terminal.viewport();
    assert!(viewport_contains(&viewport, 0, "Line 0"));
    assert!(viewport_contains(&viewport, 1, "Line 1"));
    assert_eq!(viewport[2].trim(), "", "line 2 should be cleared");
    assert_eq!(viewport[3].trim(), "", "line 3 should be cleared");
}

#[test]
fn handles_shrink_to_single_line() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.set_clear_on_shrink(true);
    let component = MutableLines::new();
    component.set(["Line 0", "Line 1", "Line 2", "Line 3"]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);

    component.set(["Only line"]);
    tui.request_render();
    wait_for_render(&mut tui);

    let viewport = terminal.viewport();
    assert!(viewport_contains(&viewport, 0, "Only line"));
    assert_eq!(viewport[1].trim(), "");
}

#[test]
fn handles_shrink_to_empty() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.set_clear_on_shrink(true);
    let component = MutableLines::new();
    component.set(["Line 0", "Line 1", "Line 2"]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);

    component.set(Vec::<String>::new());
    tui.request_render();
    wait_for_render(&mut tui);

    let viewport = terminal.viewport();
    assert_eq!(viewport[0].trim(), "");
    assert_eq!(viewport[1].trim(), "");
}

// ---------------------------------------------------------------------------
// Differential rendering
// ---------------------------------------------------------------------------

#[test]
fn middle_line_changes_spinner_case() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    component.set(["Header", "Working...", "Footer"]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);

    for frame in ["|", "/", "-", "\\"] {
        component.set(["Header", &format!("Working {}", frame), "Footer"]);
        tui.request_render();
        wait_for_render(&mut tui);

        let viewport = terminal.viewport();
        assert!(viewport_contains(&viewport, 0, "Header"));
        assert!(viewport_contains(
            &viewport,
            1,
            &format!("Working {}", frame)
        ));
        assert!(viewport_contains(&viewport, 2, "Footer"));
    }
}

#[test]
fn resets_styles_after_each_rendered_line() {
    let terminal = VirtualTerminal::new(20, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    // First line turns italic on (no explicit off). Expect the next line's
    // first cell to NOT inherit italic styling.
    component.set(strings(["\x1b[3mItalic", "Plain"]));
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);

    let cell = terminal.cell(1, 0).expect("cell (1,0) should exist");
    assert!(!cell.italic, "italic style leaked into plain line");
}

#[test]
fn renders_correctly_when_first_line_changes_but_rest_stays_same() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    component.set(["Line 0", "Line 1", "Line 2", "Line 3"]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);

    component.set(["CHANGED", "Line 1", "Line 2", "Line 3"]);
    tui.request_render();
    wait_for_render(&mut tui);

    let viewport = terminal.viewport();
    assert!(viewport_contains(&viewport, 0, "CHANGED"));
    assert!(viewport_contains(&viewport, 1, "Line 1"));
    assert!(viewport_contains(&viewport, 2, "Line 2"));
    assert!(viewport_contains(&viewport, 3, "Line 3"));
}

#[test]
fn renders_correctly_when_last_line_changes_but_rest_stays_same() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    component.set(["Line 0", "Line 1", "Line 2", "Line 3"]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);

    component.set(["Line 0", "Line 1", "Line 2", "CHANGED"]);
    tui.request_render();
    wait_for_render(&mut tui);

    let viewport = terminal.viewport();
    assert!(viewport_contains(&viewport, 0, "Line 0"));
    assert!(viewport_contains(&viewport, 1, "Line 1"));
    assert!(viewport_contains(&viewport, 2, "Line 2"));
    assert!(viewport_contains(&viewport, 3, "CHANGED"));
}

#[test]
fn renders_correctly_when_multiple_non_adjacent_lines_change() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    component.set(["Line 0", "Line 1", "Line 2", "Line 3", "Line 4"]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);

    component.set(["Line 0", "CHANGED 1", "Line 2", "CHANGED 3", "Line 4"]);
    tui.request_render();
    wait_for_render(&mut tui);

    let viewport = terminal.viewport();
    assert!(viewport_contains(&viewport, 0, "Line 0"));
    assert!(viewport_contains(&viewport, 1, "CHANGED 1"));
    assert!(viewport_contains(&viewport, 2, "Line 2"));
    assert!(viewport_contains(&viewport, 3, "CHANGED 3"));
    assert!(viewport_contains(&viewport, 4, "Line 4"));
}

#[test]
fn tracks_cursor_correctly_when_content_shrinks_with_unchanged_remaining_lines() {
    // Regression: after a shrink, the engine still knows where the cursor
    // is so that a subsequent diff render writes to the correct row.
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();

    component.set(["Line 0", "Line 1", "Line 2", "Line 3", "Line 4"]);
    tui.root.add_child(Box::new(component.clone()));
    wait_for_render(&mut tui);

    // Shrink to 3 lines, all unchanged from the first three.
    component.set(["Line 0", "Line 1", "Line 2"]);
    tui.request_render();
    wait_for_render(&mut tui);

    // Now change line 1. If cursor tracking was off, this would land on
    // the wrong row.
    component.set(["Line 0", "CHANGED", "Line 2"]);
    tui.request_render();
    wait_for_render(&mut tui);

    let viewport = terminal.viewport();
    assert!(
        viewport_contains(&viewport, 1, "CHANGED"),
        "expected CHANGED on row 1, got {:?}",
        viewport.get(1),
    );
}

#[test]
fn handles_transition_from_content_to_empty_and_back_to_content() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();

    component.set(["Line 0", "Line 1", "Line 2"]);
    tui.root.add_child(Box::new(component.clone()));
    wait_for_render(&mut tui);

    component.set(Vec::<String>::new());
    tui.request_render();
    wait_for_render(&mut tui);

    component.set(["New Line 0", "New Line 1"]);
    tui.request_render();
    wait_for_render(&mut tui);

    let viewport = terminal.viewport();
    assert!(viewport_contains(&viewport, 0, "New Line 0"));
    assert!(viewport_contains(&viewport, 1, "New Line 1"));
}

#[test]
fn appends_after_shrink_without_another_full_redraw() {
    // Shrink forces a full redraw; the next-render that *grows* content
    // back up (but not above the new previous count) must stay on the
    // differential path.
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.set_clear_on_shrink(true);
    let component = MutableLines::new();

    component.set((0..8).map(|i| format!("Line {i}")));
    tui.root.add_child(Box::new(component.clone()));
    wait_for_render(&mut tui);

    let initial = tui.full_redraws();

    component.set(["Line 0", "Line 1"]);
    tui.request_render();
    wait_for_render(&mut tui);

    let after_shrink = tui.full_redraws();
    assert!(
        after_shrink > initial,
        "shrink should have triggered a full redraw",
    );

    component.set(["Line 0", "Line 1", "Line 2"]);
    tui.request_render();
    wait_for_render(&mut tui);

    assert_eq!(
        tui.full_redraws(),
        after_shrink,
        "appending after the shrink should stay on the differential path",
    );

    // Viewport shows the three lines, rest blank.
    assert_eq!(
        terminal.viewport_trimmed(),
        vec!["Line 0", "Line 1", "Line 2"],
    );
}

#[test]
fn full_redraw_on_shrink_from_many_lines_preserves_tail_in_viewport() {
    // 12 lines in a 5-row terminal leaves the first 7 in scrollback; the
    // viewport shows lines 7..=11. Shrinking to 7 lines should trigger a
    // full redraw and leave the viewport showing the new tail (lines
    // 2..=6).
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.set_clear_on_shrink(true);
    let component = MutableLines::new();

    component.set((0..12).map(|i| format!("Line {i}")));
    tui.root.add_child(Box::new(component.clone()));
    wait_for_render(&mut tui);

    let initial = tui.full_redraws();

    component.set((0..7).map(|i| format!("Line {i}")));
    tui.request_render();
    wait_for_render(&mut tui);

    assert!(
        tui.full_redraws() > initial,
        "shrink should have triggered a full redraw",
    );
    assert_eq!(
        terminal.viewport(),
        vec!["Line 2", "Line 3", "Line 4", "Line 5", "Line 6"],
    );
}

// ---------------------------------------------------------------------------
// Termux height-change gating
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn height_changes_stay_on_differential_path_in_termux() {
    let _guard = support::with_env(&[("TERMUX_VERSION", Some("1"))]);

    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();

    component.set((0..20).map(|i| format!("Line {i}")));
    tui.root.add_child(Box::new(component.clone()));
    wait_for_render(&mut tui);
    terminal.clear_writes();

    let initial = tui.full_redraws();
    for height in [15, 8, 14, 11] {
        terminal.resize(40, height);
        wait_for_render(&mut tui);
    }

    assert_eq!(
        tui.full_redraws(),
        initial,
        "height changes should not trigger a full redraw in Termux",
    );
    let writes = terminal.writes_joined();
    assert!(
        !writes.contains("\x1b[2J"),
        "height change should not clear the screen in Termux; writes: {:?}",
        writes,
    );
    assert!(
        !writes.contains("\x1b[3J"),
        "height change should not clear scrollback in Termux; writes: {:?}",
        writes,
    );
}

#[test]
#[serial_test::serial]
fn height_changes_still_trigger_full_redraw_outside_termux() {
    let _guard = support::with_env(&[("TERMUX_VERSION", None)]);

    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();

    component.set(["Line 0", "Line 1", "Line 2"]);
    tui.root.add_child(Box::new(component.clone()));
    wait_for_render(&mut tui);
    let initial = tui.full_redraws();

    terminal.resize(40, 15);
    wait_for_render(&mut tui);

    assert!(
        tui.full_redraws() > initial,
        "height change outside Termux should trigger a full redraw",
    );
}

// ---------------------------------------------------------------------------
// max_lines_rendered / clear-on-shrink high-water mark
// ---------------------------------------------------------------------------

#[test]
fn clears_stale_content_when_high_water_mark_was_inflated_by_a_transient_component() {
    // Regression guard: clear-on-shrink has to compare against the
    // historical high-water mark of the rendered content — not just the
    // previous render — or a transient component (a dropdown, a selector
    // overlay, a tool-call log that scrolled away) that temporarily
    // inflated the working area will leave stale rows behind once the
    // user returns to the baseline shape.
    //
    // Scenario:
    //   1. Base layout: 15 chat lines + 3 editor lines = 18 rows.
    //   2. Editor swaps to an 8-row selector: 15 + 8 = 23 rows.
    //   3. Selector closes, editor returns to 3 rows: 15 + 3 = 18 rows.
    //   4. Chat shrinks to 12 lines: 12 + 3 = 15 rows.
    //
    // Step 4 is what the test pins down. 15 rows is below both the
    // current 18-row baseline *and* the 23-row high-water mark. A
    // correct engine takes the full-redraw path and wipes the tail;
    // a broken engine that only checked the last render's length may
    // still take the full-redraw path for the step 3→4 delta, but the
    // scenario is structured so the only way to produce a clean
    // viewport after step 4 is to track the high-water mark accurately.

    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.set_clear_on_shrink(true);

    let chat = MutableLines::new();
    let editor = MutableLines::new();
    tui.root.add_child(Box::new(chat.clone()));
    tui.root.add_child(Box::new(editor.clone()));

    let long_chat: Vec<String> = (0..15).map(|i| format!("Chat {}", i)).collect();
    let short_chat: Vec<String> = (0..12).map(|i| format!("Chat {}", i)).collect();
    let editor_lines = vec![
        "Editor 0".to_string(),
        "Editor 1".to_string(),
        "Editor 2".to_string(),
    ];
    let selector_lines: Vec<String> = (0..8).map(|i| format!("Selector {}", i)).collect();

    // Step 1: 18 rows rendered; high-water = 18.
    chat.set(long_chat.clone());
    editor.set(editor_lines.clone());
    wait_for_render(&mut tui);

    // Step 2: 23 rows rendered; high-water = 23.
    editor.set(selector_lines);
    wait_for_render(&mut tui);

    // Step 3: 18 rows rendered; high-water still 23.
    editor.set(editor_lines.clone());
    wait_for_render(&mut tui);

    let redraws_before_switch = tui.full_redraws();

    // Step 4: 15 rows rendered; with an accurate high-water mark this
    // dips below 23 and must take the full-redraw path.
    chat.set(short_chat);
    wait_for_render(&mut tui);

    assert!(
        tui.full_redraws() > redraws_before_switch,
        "shrinking below the working-area high-water mark should force a full redraw \
         (redraws before: {}, after: {})",
        redraws_before_switch,
        tui.full_redraws(),
    );

    // After step 4 the viewport is 10 rows tall and the content is 15
    // rows total, so the terminal shows the tail of chat (rows 5..=11)
    // plus the three editor rows. None of the old "Chat 12/13/14"
    // entries — which *were* visible in steps 1-3 — should survive.
    let viewport = terminal.viewport();
    for (row, line) in viewport.iter().enumerate() {
        for stale in ["Chat 12", "Chat 13", "Chat 14"] {
            assert!(
                !line.contains(stale),
                "stale {:?} left on viewport row {}: {:?}",
                stale,
                row,
                line,
            );
        }
    }
    assert_eq!(
        viewport,
        vec![
            "Chat 5", "Chat 6", "Chat 7", "Chat 8", "Chat 9", "Chat 10", "Chat 11", "Editor 0",
            "Editor 1", "Editor 2",
        ],
    );
}

#[test]
fn max_lines_rendered_tracks_the_high_water_mark_across_diff_renders() {
    // Whitebox assertion on the engine's internal high-water mark —
    // specifically that diff renders grow it (via `max`) and that a
    // full-clear render resets it.
    let terminal = VirtualTerminal::new(40, 30);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.set_clear_on_shrink(true);
    let component = MutableLines::new();
    tui.root.add_child(Box::new(component.clone()));

    // First render: 5 lines. High-water starts at 5.
    component.set(["a1", "a2", "a3", "a4", "a5"]);
    wait_for_render(&mut tui);
    assert_eq!(tui.max_lines_rendered(), 5);

    // Grow to 8 (diff append): high-water grows to 8.
    component.set(["a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8"]);
    wait_for_render(&mut tui);
    assert_eq!(tui.max_lines_rendered(), 8);

    // Shrink to 6. Below high-water, so clear-on-shrink fires and the
    // full-clear render resets the mark down to 6.
    let redraws_before = tui.full_redraws();
    component.set(["a1", "a2", "a3", "a4", "a5", "a6"]);
    wait_for_render(&mut tui);
    assert!(
        tui.full_redraws() > redraws_before,
        "shrink below high-water should take the full-render path",
    );
    assert_eq!(tui.max_lines_rendered(), 6);
}

#[test]
fn active_overlay_suppresses_clear_on_shrink() {
    // An overlay inflates the composited lines length for positioning
    // purposes; we don't want the high-water mark + clear-on-shrink
    // combination to fire a full redraw every time the overlay's
    // content shifts, because each such redraw replays the whole
    // base layout below. Verify the engine takes the diff path while
    // an overlay is present even when the working area otherwise
    // looks like it shrank.
    use aj_tui::tui::{OverlayAnchor, OverlayOptions, SizeValue};

    let terminal = VirtualTerminal::new(40, 15);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let base =
        MutableLines::with_lines(["b0", "b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8", "b9"]);
    tui.root.add_child(Box::new(base.clone()));
    wait_for_render(&mut tui);

    // Install a visible overlay. Subsequent renders are composited.
    let _handle = tui.show_overlay(
        Box::new(support::StaticLines::new(["OVERLAY"])),
        OverlayOptions {
            width: Some(SizeValue::Absolute(10)),
            anchor: OverlayAnchor::Center,
            ..Default::default()
        },
    );
    wait_for_render(&mut tui);
    let redraws_with_overlay = tui.full_redraws();

    // Shrink the base content while the overlay is still up. Without
    // the overlay guard, clear-on-shrink would force a full redraw
    // every frame the base moves; with the guard, the engine stays on
    // the differential path.
    base.set(["b0", "b1", "b2"]);
    wait_for_render(&mut tui);

    assert_eq!(
        tui.full_redraws(),
        redraws_with_overlay,
        "base-content shrink under an active overlay should not force a full redraw",
    );
}

#[test]
fn diff_path_keeps_cursor_tracking_when_overlay_content_shrinks_then_grows() {
    // Regression guard for the settings-demo artifact: an overlay
    // whose content height fluctuates (e.g. a search-enabled list) is
    // driven through the differential path on every keystroke because
    // the active-overlay guard suppresses `clear_on_shrink`. When old
    // content was taller than new, the diff writer must correctly
    // track the hardware cursor past the new content's last row;
    // otherwise the next render positions its output on the wrong
    // rows, and the user sees the old lines duplicated a few rows
    // above the new ones.
    use aj_tui::component::Component;
    use aj_tui::impl_component_any;
    use aj_tui::tui::{OverlayAnchor, OverlayOptions, SizeValue};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct FlipOverlay {
        shape: Rc<RefCell<&'static str>>,
    }
    impl Component for FlipOverlay {
        impl_component_any!();
        fn render(&mut self, _width: usize) -> Vec<String> {
            match *self.shape.borrow() {
                "tall" => (0..10).map(|i| format!("T{}", i)).collect(),
                _ => vec!["S0".to_string(), "S1".to_string()],
            }
        }
    }

    let terminal = VirtualTerminal::new(40, 20);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let overlay = FlipOverlay {
        shape: Rc::new(RefCell::new("tall")),
    };
    let shape_handle = Rc::clone(&overlay.shape);
    let _handle = tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(20)),
            anchor: OverlayAnchor::TopLeft,
            ..Default::default()
        },
    );
    wait_for_render(&mut tui);

    // Shrink. Diff path: old content taller than new, exercising the
    // "write range clamped to new-len-1, cleanup loop advances the
    // tracked cursor row" branch of `differential_render`.
    *shape_handle.borrow_mut() = "short";
    wait_for_render(&mut tui);

    // Grow back. If the shrink frame's cursor tracking drifted, the
    // fresh rows land below the expected position.
    *shape_handle.borrow_mut() = "tall";
    wait_for_render(&mut tui);

    let viewport: Vec<String> = terminal
        .viewport_trimmed()
        .into_iter()
        .map(|s| s.trim_end().to_string())
        .collect();
    let expected: Vec<String> = (0..10).map(|i| format!("T{}", i)).collect();
    assert_eq!(
        viewport, expected,
        "after shrink+grow the overlay content should land at rows 0..10 \
         with no leaked rows from the shrunk frame; got {:#?}",
        viewport,
    );
}

// ---------------------------------------------------------------------------
// request_full_render
// ---------------------------------------------------------------------------

#[test]
fn request_full_render_re_emits_every_line_on_next_render() {
    // After a normal render, a second render with unchanged content
    // should take the no-op path: the diff engine sees zero changed
    // lines. request_full_render clears that diff state so the next
    // render re-emits every line verbatim.
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let lines = MutableLines::with_lines(["alpha", "beta"]);
    tui.root.add_child(Box::new(lines.clone()));

    wait_for_render(&mut tui);
    assert_eq!(terminal.viewport()[0], "alpha");
    assert_eq!(terminal.viewport()[1], "beta");

    // Baseline: a second render without any changes emits nothing
    // new (no line text re-appears in the writes log). Clear the log
    // so we can measure the next frame in isolation.
    terminal.clear_writes();
    wait_for_render(&mut tui);
    let quiet_writes = terminal.writes_joined();
    assert!(
        !quiet_writes.contains("alpha"),
        "idle re-render must not re-emit unchanged lines; got {:?}",
        quiet_writes,
    );

    // Now request a full render. The next frame should emit every
    // line again, even though the content is byte-identical to the
    // previous frame.
    terminal.clear_writes();
    tui.request_full_render();
    wait_for_render(&mut tui);
    let forced_writes = terminal.writes_joined();
    assert!(
        forced_writes.contains("alpha") && forced_writes.contains("beta"),
        "request_full_render should cause the next render to re-emit all lines; got {:?}",
        forced_writes,
    );
}

#[test]
fn request_full_render_sets_render_requested_flag() {
    let terminal = VirtualTerminal::new(10, 3);
    let mut tui = Tui::new(Box::new(terminal));
    assert!(
        !tui.is_render_requested(),
        "render-requested flag starts cleared",
    );

    tui.request_full_render();

    assert!(
        tui.is_render_requested(),
        "request_full_render should set the render-requested flag so event \
         loops that poll is_render_requested pick up the forced frame",
    );
}

#[test]
fn force_full_render_does_not_set_render_requested_flag() {
    // force_full_render is the lower-level variant: clears diff state
    // but does NOT flag a pending render. Callers that want both
    // should use request_full_render instead.
    let terminal = VirtualTerminal::new(10, 3);
    let mut tui = Tui::new(Box::new(terminal));

    tui.force_full_render();

    assert!(
        !tui.is_render_requested(),
        "force_full_render is independent of the render-requested flag",
    );
}

#[test]
fn diff_grows_below_screen_bottom_scrolls_instead_of_clamping() {
    // Regression guard for the tmux-at-the-bottom flicker: when a new
    // render needs to cursor-down past the last visible row (e.g.,
    // a popup opens and the TUI already fills the screen), the engine
    // must produce `\r\n`s — which scroll the terminal — rather than
    // `\x1b[nB`, which clamps at the last row without scrolling and
    // desynchronizes the tracked logical cursor from the physical
    // cursor.
    //
    // Scenario: terminal height matches the first render's height so
    // the TUI fills the screen bottom-to-bottom. The next render grows
    // by two rows. If the engine emits `\x1b[2B`, the cursor clamps,
    // the newly-written rows overwrite the last two existing rows
    // instead of landing below them, and the viewport ends up with
    // duplicated/skipped content and a stale row surviving in
    // scrollback in the wrong place.

    let terminal = VirtualTerminal::new(40, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let component = MutableLines::new();
    tui.root.add_child(Box::new(component.clone()));

    // Fill the 6-row terminal: 6 rows total. Cursor ends at the
    // bottom.
    component.set(["row 0", "row 1", "row 2", "row 3", "row 4", "row 5"]);
    wait_for_render(&mut tui);
    assert_eq!(
        terminal.viewport(),
        vec!["row 0", "row 1", "row 2", "row 3", "row 4", "row 5"],
    );

    // Grow by two rows. The diff engine computes `first=6` (first
    // new row) and `cursor_at=5` (last row of the previous render)
    // — so it must move the cursor *down one* to land on a row that
    // doesn't yet exist, which requires scrolling. Same again for
    // row 7.
    component.set([
        "row 0", "row 1", "row 2", "row 3", "row 4", "row 5", "row 6", "row 7",
    ]);
    tui.request_render();
    wait_for_render(&mut tui);

    // After growth the terminal has scrolled: the last 6 logical rows
    // are visible (rows 2..=7).
    let v = terminal.viewport();
    assert_eq!(
        v,
        vec!["row 2", "row 3", "row 4", "row 5", "row 6", "row 7"],
        "scrolling growth should shift the viewport to show the latest rows",
    );

    // And no stale duplicate of row 5 (which is what you'd see if
    // \x1b[nB had clamped: the cursor stays on row 5 and the "row 6"
    // write overwrites it, leaving row 5 missing and row 6 written
    // in its place — every subsequent frame is then off by one).
    let all_rows = v.join("\n");
    for r in ["row 0", "row 1"] {
        assert!(
            !all_rows.contains(r),
            "{:?} should have scrolled off the viewport, got:\n{}",
            r,
            all_rows,
        );
    }
}

#[test]
#[should_panic(expected = "exceeds terminal width")]
fn panics_cleanly_when_rendered_line_exceeds_terminal_width() {
    // Regression guard for the "component forgot to truncate" class of
    // bug. Pre-fix, the diff engine would happily write the oversize
    // line and then desync cursor tracking on the next frame; now we
    // short-circuit with a panic that carries the offending row number
    // and the crash-log path, and the panic hook has already restored
    // the terminal.

    // Point the crash log at a test-local path so this test doesn't
    // scribble into the developer's `~/.aj/`.
    let crash_log = std::env::temp_dir().join(format!(
        "aj-tui-test-crash-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    // SAFETY: single-threaded test context (this test doesn't spawn
    // threads and `cargo test` already serializes when the user sets
    // `--test-threads=1`; the panic-on-violation property is what we
    // care about, not the precise crash-log content here).
    unsafe {
        std::env::set_var("AJ_TUI_CRASH_LOG", &crash_log);
    }

    let terminal = VirtualTerminal::new(10, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    tui.root.add_child(Box::new(component.clone()));

    // 20 visible columns in a 10-column terminal.
    component.set(["this line is twenty"]);
    wait_for_render(&mut tui);
}

#[test]
fn stop_uses_crlf_to_move_past_last_rendered_row() {
    // Regression guard for the clamp-at-bottom latent bug in `stop`:
    // the previous implementation used `\x1b[nB` (via
    // `Terminal::move_by`) to advance past the last rendered row, but
    // `CUD` doesn't scroll, so when the TUI's last row coincides with
    // the terminal's last row (common when the shell prompt was at the
    // bottom when the TUI started) the move is a no-op and the
    // restored shell prompt paints on top of the last content row.
    //
    // The fix emits `\r\n`s instead, which scroll when necessary.

    let terminal = VirtualTerminal::new(40, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let component = MutableLines::new();
    tui.root.add_child(Box::new(component.clone()));

    component.set(["row 0", "row 1", "row 2"]);
    wait_for_render(&mut tui);

    tui.stop();

    let joined = terminal.writes_joined();
    // No CUD sequence emitted during stop. (Using `contains` on the
    // full joined trace is fine: prior renders are line-level `\r\n`
    // emissions plus `\x1b[2K`, not CUD.)
    assert!(
        !joined.contains("\x1b[1B") && !joined.contains("\x1b[2B"),
        "stop should not emit \\x1b[nB (CUD clamps at the last visible \
         row); trace was:\n{joined:?}",
    );
    // And the final viewport has a blank line below the content (the
    // row the shell prompt will land on).
    let viewport = terminal.viewport();
    assert!(
        viewport.iter().any(|r| r.contains("row 2")),
        "last rendered row should still be visible after stop: {viewport:?}",
    );
}

#[test]
fn debug_log_records_decision_state_per_render() {
    // Integration smoke test: with AJ_TUI_DEBUG_LOG pointing at a
    // temp file, every call to `render` should append a record
    // capturing the strategy (full vs diff), cursor tracking, and
    // the frame contents. The exact format is informational, not
    // load-bearing — we just grep for landmark fields.
    let log_path = std::env::temp_dir().join(format!(
        "aj-tui-test-debug-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    // Ensure a clean slate.
    let _ = std::fs::remove_file(&log_path);
    // SAFETY: see the width-panic test above — serialized test
    // context and the env var is only read from within this process.
    unsafe {
        std::env::set_var("AJ_TUI_DEBUG_LOG", &log_path);
    }

    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    tui.root.add_child(Box::new(component.clone()));

    component.set(["row 0", "row 1", "row 2"]);
    wait_for_render(&mut tui);

    component.set(["row 0", "row 1 CHANGED", "row 2"]);
    tui.request_render();
    wait_for_render(&mut tui);

    unsafe {
        std::env::remove_var("AJ_TUI_DEBUG_LOG");
    }

    let log = std::fs::read_to_string(&log_path).expect("debug log should exist");
    // First render is picked up as first_render → full path.
    assert!(
        log.contains("strategy: full(first_render)"),
        "expected the first render to be labeled full(first_render); got:\n{log}",
    );
    // Second render changes only row 1 → diff path with first=last=1.
    assert!(
        log.contains("strategy: diff"),
        "expected the second render to be diff; got:\n{log}",
    );
    assert!(
        log.contains("first_changed: 1"),
        "expected first_changed: 1 in second record; got:\n{log}",
    );
    assert!(
        log.contains("last_changed: 1"),
        "expected last_changed: 1 in second record; got:\n{log}",
    );
    // Line contents are snapshotted.
    assert!(
        log.contains("row 1 CHANGED"),
        "expected new_lines snapshot to include 'row 1 CHANGED'; got:\n{log}",
    );

    let _ = std::fs::remove_file(&log_path);
}

#[test]
fn full_clear_mode_defaults_to_whole_screen_and_emits_scrollback_wipe() {
    // Regression guard for the defensive full-clear default: a
    // full-redraw that's supposed to recover from cursor-tracking
    // drift should emit `\x1b[2J\x1b[H\x1b[3J`, not the
    // preserve-shell-output-above `\x1b[{n}A\r\x1b[J` variant.
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    tui.root.add_child(Box::new(component.clone()));

    // First render: first_render=true → full_render(clear=false), no
    // wipe sequence involved.
    component.set(["row 0", "row 1", "row 2"]);
    wait_for_render(&mut tui);

    // Force a width change to land on `full_render(clear=true)`.
    let clear_call_before = terminal
        .writes_joined()
        .matches("\x1b[2J\x1b[H\x1b[3J")
        .count();
    terminal.resize(50, 10);
    tui.request_render();
    wait_for_render(&mut tui);

    let joined = terminal.writes_joined();
    let clear_call_after = joined.matches("\x1b[2J\x1b[H\x1b[3J").count();
    assert!(
        clear_call_after > clear_call_before,
        "WholeScreen full-clear should emit \\x1b[2J\\x1b[H\\x1b[3J; trace:\n{joined:?}",
    );
}

#[test]
fn full_clear_mode_below_cursor_opt_in_preserves_scrollback_wipe_absent() {
    // Verify the opt-out is honored: setting `FullClearMode::BelowCursor`
    // reverts to the pre-port `\x1b[{n}A\r\x1b[J` sequence and
    // scrollback wipes stop being emitted.
    use aj_tui::tui::FullClearMode;

    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.set_full_clear_mode(FullClearMode::BelowCursor);
    let component = MutableLines::new();
    tui.root.add_child(Box::new(component.clone()));

    component.set(["row 0", "row 1", "row 2"]);
    wait_for_render(&mut tui);

    // Trigger a full-clear path via a width change.
    terminal.resize(50, 10);
    tui.request_render();
    wait_for_render(&mut tui);

    let joined = terminal.writes_joined();
    assert!(
        !joined.contains("\x1b[2J\x1b[H\x1b[3J"),
        "BelowCursor mode must not emit the WholeScreen clear sequence; trace:\n{joined:?}",
    );
    // `\x1b[J` (erase below cursor) should be present instead.
    assert!(
        joined.contains("\x1b[J"),
        "BelowCursor mode should emit \\x1b[J to erase below the cursor; trace:\n{joined:?}",
    );
}

#[test]
fn diff_falls_back_to_full_redraw_when_first_change_is_above_viewport() {
    // Regression guard for the `first_changed < viewport_top` full-
    // redraw fallback. Scenario:
    //
    //   1. First render fills the terminal exactly (no scroll).
    //   2. Second render grows past the bottom, scrolling the top
    //      row into scrollback. `previous_viewport_top` advances to
    //      track where logical row 0 now lives relative to the
    //      visible viewport.
    //   3. Third render changes logical row 0 again. Diff says
    //      `firstChanged = 0`, but row 0 is off-screen in scrollback
    //      — diff path physically can't touch it. Engine must fall
    //      back to a full redraw (which resets the whole viewport).
    let terminal = VirtualTerminal::new(40, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    tui.root.add_child(Box::new(component.clone()));

    component.set(["row 0", "row 1", "row 2", "row 3", "row 4", "row 5"]);
    wait_for_render(&mut tui);

    // Grow past the bottom — the \r\n fix makes the terminal scroll
    // and `previous_viewport_top` must advance so the engine knows
    // where "logical row 0" sits relative to the visible area.
    component.set([
        "row 0", "row 1", "row 2", "row 3", "row 4", "row 5", "row 6", "row 7", "row 8",
    ]);
    tui.request_render();
    wait_for_render(&mut tui);

    let redraws_before = tui.full_redraws();

    // Change row 0 (logically) — off-screen, must trigger full
    // redraw fallback.
    component.set([
        "ROW 0 CHANGED",
        "row 1",
        "row 2",
        "row 3",
        "row 4",
        "row 5",
        "row 6",
        "row 7",
        "row 8",
    ]);
    tui.request_render();
    wait_for_render(&mut tui);

    assert!(
        tui.full_redraws() > redraws_before,
        "changing a row that's scrolled into scrollback should force full redraw \
         (before: {redraws_before}, after: {})",
        tui.full_redraws(),
    );
    // And the visible viewport shows the tail rows including the now-
    // updated row 0 wait — with WholeScreen full-redraw the scrolled
    // content is wiped, so the final viewport is the bottom `height`
    // logical rows (rows 3..=8, starting with the updated row 0 at
    // the top of the paint but scrolled to show only the tail).
    let v = terminal.viewport();
    assert_eq!(v.len(), 6);
    assert!(
        v.iter().any(|r| r.contains("row 8")),
        "viewport should show latest row; got {v:?}",
    );
}

#[test]
fn line_reset_closes_open_hyperlink_on_each_row() {
    // Regression guard for apply_line_resets: every rendered non-empty
    // line must be terminated with `\x1b[0m\x1b]8;;\x07` (SGR reset +
    // OSC 8 empty-URL closer), not just `\x1b[0m`. A component that
    // emits a hyperlink and forgets to close it should not bleed the
    // URL attribute into downstream rows.
    let terminal = VirtualTerminal::new(40, 4);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    // Two rows: the first opens an OSC 8 hyperlink and leaves it
    // dangling (no `\x1b]8;;\x1b\\` closer). The second is plain text
    // that, if the framework's line terminator doesn't close the
    // hyperlink, would inherit the URL and render as a clickable row.
    let component = MutableLines::with_lines([
        "\x1b]8;;https://example.com\x1b\\link label", // no closer
        "plain row",
    ]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);

    // The writes log should contain the segment-reset form after the
    // first line, not a bare SGR reset. The OSC 8 closer is `\x1b]8;;\x07`.
    let joined = terminal.writes_joined();
    assert!(
        joined.contains("\x1b]8;;\x07"),
        "expected OSC 8 empty-URL closer in writes log; got:\n{joined:?}",
    );
}

#[test]
fn line_reset_terminates_empty_lines_too() {
    // A row produced as `""` by a component must still pick up the
    // segment-reset terminator. Without it, an in-progress style on
    // the *terminal* (BSU not honored end-to-end, a raw escape from a
    // subprocess, etc.) can bleed through into the otherwise-empty
    // row's cells until something else resets.
    //
    // The check here is structural: the writes log must contain the
    // segment-reset bytes between consecutive `\r\n`s. With the old
    // "skip empty lines" Phase 4, a row whose component output was
    // `""` would emit only `\r\n` for the row terminator with no
    // intervening reset, leaving the cells styled.
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let component = MutableLines::with_lines(["styled row", "", "another"]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);

    // After the empty row, the writes log should contain the
    // segment-reset (`\x1b[0m\x1b]8;;\x07`) for that row, even though
    // it was empty. We assert there are at least three reset
    // sequences in the writes — one per content row including the
    // empty middle one.
    let joined = terminal.writes_joined();
    let reset_count = joined.matches("\x1b[0m\x1b]8;;\x07").count();
    assert!(
        reset_count >= 3,
        "expected at least 3 segment-reset terminators (one per row, \
         empty row included); got {reset_count} in:\n{joined:?}",
    );
}

#[test]
fn force_full_render_clears_screen_on_next_render() {
    // Regression guard: `force_full_render` is the "recover from an
    // out-of-band terminal change" escape hatch. It must wipe the
    // screen on the next render even though resetting `previous_lines`
    // to empty would otherwise make the engine think it's rendering
    // for the first time (and skip the clear to preserve pre-TUI
    // shell output).
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let lines = MutableLines::with_lines(["alpha", "beta"]);
    tui.root.add_child(Box::new(lines.clone()));

    // Prime a render so we're past first-render.
    wait_for_render(&mut tui);
    terminal.clear_writes();

    // Force a clear recovery render.
    tui.force_full_render();
    wait_for_render(&mut tui);

    let joined = terminal.writes_joined();
    assert!(
        joined.contains("\x1b[2J\x1b[H\x1b[3J"),
        "force_full_render should emit the WholeScreen clear sequence \
         on the next render; writes trace:\n{joined:?}",
    );
    // And the content is re-emitted.
    assert!(
        joined.contains("alpha") && joined.contains("beta"),
        "content must be repainted after the clear; writes trace:\n{joined:?}",
    );
}

#[test]
fn first_render_does_not_clear_so_pre_tui_shell_output_is_preserved() {
    // Complement to `force_full_render_clears_screen_on_next_render`:
    // a *genuine* first render must NOT emit the screen-clear
    // sequence. This is the property that keeps shell output visible
    // above a freshly-started TUI, and the whole reason
    // `pending_full_clear` has to exist as a separate signal from
    // `previous_lines.is_empty()`.
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.root
        .add_child(Box::new(MutableLines::with_lines(["hello"])));

    wait_for_render(&mut tui);

    let joined = terminal.writes_joined();
    assert!(
        !joined.contains("\x1b[2J\x1b[H\x1b[3J"),
        "first render must not emit the WholeScreen clear sequence; \
         writes trace:\n{joined:?}",
    );
}

#[test]
fn shrink_at_viewport_bottom_does_not_scroll_kept_content_off_screen() {
    // Regression guard for the all-deletions diff path. Scenario: the
    // previous frame filled the viewport exactly (so the cursor sits
    // at the terminal's last row); the new frame is shorter. Clearing
    // the deleted trailing rows must use clamp-style cursor moves
    // (CUD + \x1b[2K) rather than \r\n, because \r\n scrolls on the
    // last row and would push the still-visible content into
    // scrollback as a side effect of the cleanup.
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    component.set([
        "keep line 0",
        "keep line 1",
        "keep line 2",
        "drop line 3",
        "drop line 4",
    ]);
    tui.root.add_child(Box::new(component.clone()));

    wait_for_render(&mut tui);
    let redraws_before = tui.full_redraws();

    // Shrink to three rows — trailing rows 3 and 4 are the only
    // changes. Upstream's all-deletions path kicks in; our uniform-
    // loop path would have scrolled the terminal twice and pushed
    // "keep line 0" off the top.
    component.set(["keep line 0", "keep line 1", "keep line 2"]);
    tui.request_render();
    wait_for_render(&mut tui);

    // No full redraw should fire for a simple in-viewport shrink.
    assert_eq!(
        tui.full_redraws(),
        redraws_before,
        "in-viewport shrink should take the diff path, not a full redraw",
    );

    let viewport = terminal.viewport();
    assert_eq!(
        viewport[0], "keep line 0",
        "first kept line stayed on row 0"
    );
    assert_eq!(viewport[1], "keep line 1");
    assert_eq!(viewport[2], "keep line 2");
    assert_eq!(viewport[3], "", "deleted row 3 was wiped");
    assert_eq!(viewport[4], "", "deleted row 4 was wiped");
}

#[test]
fn shrink_with_scrolled_content_falls_back_to_full_redraw() {
    // The new frame's end-of-content is above the current viewport
    // top (because prior renders scrolled content into scrollback).
    // The diff path physically can't reach those rows, so the
    // strategy selector must route us to a full redraw instead of
    // trying to clear trailing rows we can't touch.
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();

    // Eight rows on a 5-row terminal: the first render scrolls three
    // rows into scrollback. previous_viewport_top now ~= 3.
    component.set((0..8).map(|i| format!("row {i}")));
    tui.root.add_child(Box::new(component.clone()));
    wait_for_render(&mut tui);

    let redraws_before = tui.full_redraws();

    // Shrink to two rows. In the logical frame, rows 2-7 are deleted.
    // Target row = lines.len() - 1 = 1, which is above the stored
    // previous_viewport_top (~= 3) — diff path can't reach.
    component.set(["row 0", "row 1"]);
    tui.request_render();
    wait_for_render(&mut tui);

    assert!(
        tui.full_redraws() > redraws_before,
        "shrink below visible viewport must trigger full redraw \
         (before: {redraws_before}, after: {})",
        tui.full_redraws(),
    );
    let v = terminal.viewport();
    assert_eq!(v[0], "row 0");
    assert_eq!(v[1], "row 1");
}

#[test]
fn partial_change_with_trailing_deletions_preserves_earlier_rows() {
    // Combined case: one row early in the frame changes AND trailing
    // rows are deleted. The main diff loop handles the change; the
    // post-loop cleanup pass clears the trailing rows without
    // scrolling. Verifies that the cleanup sequence doesn't trample
    // the rewrite the main loop just emitted.
    let terminal = VirtualTerminal::new(20, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    component.set(["a", "b", "c", "d", "e"]);
    tui.root.add_child(Box::new(component.clone()));
    wait_for_render(&mut tui);

    // Change row 1 AND delete rows 3, 4.
    component.set(["a", "B", "c"]);
    tui.request_render();
    wait_for_render(&mut tui);

    let v = terminal.viewport();
    assert_eq!(v[0], "a");
    assert_eq!(v[1], "B", "row 1 was rewritten");
    assert_eq!(v[2], "c");
    assert_eq!(v[3], "", "deleted row 3 wiped");
    assert_eq!(v[4], "", "deleted row 4 wiped");
}

#[test]
fn pure_deletion_whose_target_lies_above_viewport_falls_back_to_full_redraw() {
    // Distinguishing case for the `deletion_only_needs_full` check.
    // Scenario:
    //
    //   - Initial content has blank middle rows, so `compute_diff_range`
    //     can produce a `first` that's *past* the previous viewport top
    //     (bypassing the `diff_above_viewport` fallback).
    //   - The shrink removes the bottom-most non-blank row, leaving a
    //     new frame whose last row is still above the viewport top.
    //
    // Without the dedicated fallback, the diff path would clear only
    // the last-row change at the viewport bottom and leave the user
    // staring at the blank middle rows that scrolled into the
    // viewport — the logical top of the new frame ("a", "b", "c")
    // would never appear.
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();

    // 10 rows total, three rows of content at the top, blanks in the
    // middle, one row of content at the bottom. A 5-row viewport
    // shows rows 5..=9, so prev_viewport_top = 5.
    component.set(["a", "b", "c", "", "", "", "", "", "", "visible-bottom"]);
    tui.root.add_child(Box::new(component.clone()));
    wait_for_render(&mut tui);

    let redraws_before = tui.full_redraws();

    // Shrink to three rows. Only row 9 ("visible-bottom") differs;
    // rows 3-8 were already blank and stay blank in the new frame's
    // absent slots. Diff says first=9; new.len()=3, target_row=2 —
    // above the previous viewport top (5).
    component.set(["a", "b", "c"]);
    tui.request_render();
    wait_for_render(&mut tui);

    assert!(
        tui.full_redraws() > redraws_before,
        "pure-deletion shrink whose target row is above the previous \
         viewport must trigger a full redraw (before: {redraws_before}, \
         after: {})",
        tui.full_redraws(),
    );
    let v = terminal.viewport();
    assert_eq!(v[0], "a", "full redraw should land the new top at row 0");
    assert_eq!(v[1], "b");
    assert_eq!(v[2], "c");
}

#[test]
fn overlay_renders_at_viewport_top_when_base_content_exceeds_terminal_height() {
    // Regression guard for the `viewport_start` offset in
    // composite_overlays. Scenario: base content is taller than the
    // terminal, so the visible viewport shows only the bottom-most
    // `height` rows. A top-anchored overlay must appear at the top of
    // the *visible viewport*, not at logical row 0 (which is off-
    // screen in scrollback).
    //
    // Without viewport_start, the overlay would composite at absolute
    // row 0 — far above the viewport — and the user would see no
    // overlay at all.
    use aj_tui::tui::{OverlayAnchor, OverlayOptions, SizeValue};

    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    // Base content: 12 rows. Visible viewport shows only rows 7..=11.
    let base = MutableLines::with_lines((0..12).map(|i| format!("base-{i:02}")));
    tui.root.add_child(Box::new(base));

    // Top-anchored overlay. `TopLeft` puts it at row 0 of the visible
    // viewport with the declared width.
    let overlay = support::StaticLines::new(["OVERLAY"]);
    let _handle = tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(7)),
            anchor: OverlayAnchor::TopLeft,
            ..Default::default()
        },
    );

    wait_for_render(&mut tui);

    // Overlay must be on the first visible row, NOT buried in logical
    // scrollback.
    let viewport = terminal.viewport();
    let overlay_row = viewport
        .iter()
        .position(|line| line.contains("OVERLAY"))
        .unwrap_or_else(|| panic!("overlay not found in viewport; got {viewport:?}"));
    assert_eq!(
        overlay_row, 0,
        "TopLeft overlay must composite at the top of the visible \
         viewport even when base content is taller than the terminal; \
         got {viewport:?}",
    );
}

#[test]
fn composite_drops_wide_char_that_would_straddle_overlay_boundary() {
    // Regression guard for `composite_line_at` strict boundary +
    // post-composition width clamp. An overlay declares width 4 but
    // renders a 5-column payload that ends with a 2-wide CJK char at
    // columns 3-4. Without strict=true on the overlay slice, the
    // composed line's visible width would overflow the overlay's
    // column range by 1, which would then trip the render engine's
    // phase-4.5 "line wider than terminal" check and panic.
    use aj_tui::impl_component_any;

    struct BoundaryOverlay;
    impl aj_tui::component::Component for BoundaryOverlay {
        impl_component_any!();
        fn render(&mut self, _width: usize) -> Vec<String> {
            // Four cells + one 2-wide CJK: width 6, overlay claims 4.
            vec!["abcあ".to_string()]
        }
    }

    let terminal = VirtualTerminal::new(10, 3);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    use aj_tui::tui::{OverlayAnchor, OverlayOptions, SizeValue};
    let _handle = tui.show_overlay(
        Box::new(BoundaryOverlay),
        OverlayOptions {
            width: Some(SizeValue::Absolute(4)),
            anchor: OverlayAnchor::TopLeft,
            ..Default::default()
        },
    );

    // Strict line-width enforcement stays on; if the overlay slice
    // were permissive, this render would panic.
    wait_for_render(&mut tui);

    // Verify composition produced a reasonable result: the `abc`
    // prefix fits in 3 columns, the wide CJK would start at column 3
    // but its width would push past the 4-column overlay — so strict
    // slicing drops it and the overlay reads as "abc " padded to the
    // declared width.
    let viewport = terminal.viewport();
    assert!(
        viewport[0].starts_with("abc"),
        "overlay start preserved; got {:?}",
        viewport[0],
    );
    assert!(
        !viewport[0].contains('あ'),
        "wide CJK at overlay boundary should be excluded; got {:?}",
        viewport[0],
    );
}

#[test]
#[serial_test::serial]
fn termux_height_shrink_reaches_full_redraw_when_old_top_would_have_hidden_it() {
    // Regression guard for the Termux-path viewport-top recompute.
    //
    // Scenario that distinguishes old-vs-new behavior:
    //
    //   - Start with a 10-row terminal and exactly 10 rows of
    //     content. previous_viewport_top = 0.
    //   - Termux-shrink to 5 rows. The visible viewport is now rows
    //     5..=9; logical rows 0..=4 have fallen above it into what
    //     the tracker should start treating as scrollback.
    //   - Change logical row 0. This row is above the (recomputed)
    //     viewport top, so the engine must fall back to a full
    //     redraw to reach it.
    //
    // Without the recompute, `previous_viewport_top` stays 0 — the
    // `diff_above_viewport` check is `0 < 0 => false`, the diff path
    // runs, and the repaint lands on the wrong physical row.
    let _guard = support::with_env(&[("TERMUX_VERSION", Some("1"))]);

    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    component.set((0..10).map(|i| format!("row-{i}")));
    tui.root.add_child(Box::new(component.clone()));
    wait_for_render(&mut tui);

    // Termux shrink, same content. previous_viewport_top should now
    // reflect the new 5-row viewport: max(0, 0 + 10 - 5) = 5.
    terminal.resize(40, 5);
    tui.request_render();
    wait_for_render(&mut tui);

    let redraws_before = tui.full_redraws();

    // Change logical row 0. With the recompute, diff_above_viewport
    // fires (0 < 5 => true) and the engine full-redraws. Without it,
    // the check is 0 < 0 => false and the diff path runs.
    component.set((0..10).map(|i| {
        if i == 0 {
            "row-0-NEW".to_string()
        } else {
            format!("row-{i}")
        }
    }));
    tui.request_render();
    wait_for_render(&mut tui);

    assert!(
        tui.full_redraws() > redraws_before,
        "change above (recomputed) viewport top must force a full redraw \
         (before: {redraws_before}, after: {})",
        tui.full_redraws(),
    );
}

#[test]
fn hardware_cursor_enabled_preference_suppresses_cursor_even_with_marker() {
    // Regression guard for the preference-vs-state split. A focus-
    // aware component emits CURSOR_MARKER in its render output; by
    // default the engine shows the real hardware cursor there, but
    // setting `hardware_cursor_enabled(false)` must suppress the
    // cursor-show emission even when the marker is present.
    use aj_tui::component::{CURSOR_MARKER, Component};
    use aj_tui::impl_component_any;

    struct MarkerEmitter;
    impl Component for MarkerEmitter {
        impl_component_any!();
        fn render(&mut self, _width: usize) -> Vec<String> {
            vec![format!("before{CURSOR_MARKER}after")]
        }
    }

    // Default preference: cursor shown when a marker is present.
    {
        let terminal = VirtualTerminal::new(20, 3);
        let mut tui = Tui::new(Box::new(terminal.clone()));
        assert!(
            tui.hardware_cursor_enabled(),
            "preference defaults to true for backward compat",
        );
        tui.start().expect("start");
        tui.root.add_child(Box::new(MarkerEmitter));
        wait_for_render(&mut tui);
        assert!(
            tui.hardware_cursor_currently_shown(),
            "marker with default preference should show the hardware cursor",
        );
        assert!(
            terminal.is_cursor_visible(),
            "VT should observe \\x1b[?25h emission",
        );
    }

    // Preference disabled: cursor stays hidden even with marker.
    {
        let terminal = VirtualTerminal::new(20, 3);
        let mut tui = Tui::new(Box::new(terminal.clone()));
        tui.set_hardware_cursor_enabled(false);
        tui.start().expect("start");
        tui.root.add_child(Box::new(MarkerEmitter));
        wait_for_render(&mut tui);
        assert!(
            !tui.hardware_cursor_currently_shown(),
            "disabled preference must prevent cursor-show even with marker",
        );
        assert!(
            !terminal.is_cursor_visible(),
            "VT should observe no \\x1b[?25h emission",
        );
    }
}

#[test]
fn render_tracker_survives_grow_shrink_grow_sequence() {
    // A8 stress test: simulate a component that grows past the
    // terminal bottom (scrolling into scrollback), grows further,
    // shrinks, and grows again. After each step the viewport must
    // show the tail of the logical frame, and the internal tracker
    // must let the next change land on the correct row.
    //
    // This is the scenario the collapsed `\r\n`-for-down approach
    // must handle without drift. If any transition went wrong, one
    // of the assertions below would catch it (either the tail row
    // would be missing from the viewport, or a subsequent edit would
    // land on the wrong row).
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let component = MutableLines::new();
    tui.root.add_child(Box::new(component.clone()));

    // Step 1: initial render, fits exactly.
    component.set((0..5).map(|i| format!("r{i:02}")));
    wait_for_render(&mut tui);
    assert!(
        terminal.viewport().iter().any(|r| r.contains("r04")),
        "step 1: tail visible after initial fit",
    );

    // Step 2: grow past terminal bottom. The diff path uses `\r\n`
    // for down-moves, which scrolls on the last row. Verify the new
    // tail appears.
    component.set((0..9).map(|i| format!("r{i:02}")));
    tui.request_render();
    wait_for_render(&mut tui);
    assert!(
        terminal.viewport().iter().any(|r| r.contains("r08")),
        "step 2: tail visible after first grow; got {:?}",
        terminal.viewport(),
    );

    // Step 3: grow further.
    component.set((0..14).map(|i| format!("r{i:02}")));
    tui.request_render();
    wait_for_render(&mut tui);
    assert!(
        terminal.viewport().iter().any(|r| r.contains("r13")),
        "step 3: tail visible after second grow; got {:?}",
        terminal.viewport(),
    );

    // Step 4: shrink in place (prev 14, new 7). Tail is now r06.
    // With clear_on_shrink off (the default), diff handles the
    // cleanup without a full redraw.
    component.set((0..7).map(|i| format!("r{i:02}")));
    tui.request_render();
    wait_for_render(&mut tui);
    let v = terminal.viewport();
    assert!(
        v.iter().any(|r| r.contains("r06")),
        "step 4: tail visible after shrink; got {v:?}",
    );
    // And the deleted rows are gone (no stray "r13" around).
    assert!(
        !v.iter().any(|r| r.contains("r13")),
        "step 4: r13 must have been wiped; got {v:?}",
    );

    // Step 5: grow again. Tail is r09 now.
    component.set((0..10).map(|i| format!("r{i:02}")));
    tui.request_render();
    wait_for_render(&mut tui);
    assert!(
        terminal.viewport().iter().any(|r| r.contains("r09")),
        "step 5: tail visible after second grow; got {:?}",
        terminal.viewport(),
    );

    // Step 6: single-row in-viewport change. If the tracker is off,
    // this lands on the wrong physical row and the viewport reads
    // something other than the intended edit.
    component.set((0..10).map(|i| {
        if i == 9 {
            "r09-EDITED".to_string()
        } else {
            format!("r{i:02}")
        }
    }));
    tui.request_render();
    wait_for_render(&mut tui);
    let v = terminal.viewport();
    assert!(
        v.iter().any(|r| r.contains("r09-EDITED")),
        "step 6: edit landed on the correct row; got {v:?}",
    );
}

#[test]
fn overlay_composite_pads_right_to_terminal_width() {
    // Regression guard for the A-bis-2 `afterPad` port.
    //
    // When a composited row's base content (minus the overlay window)
    // is shorter than the terminal width, the compositor must pad the
    // right side with spaces so the emitted line has visible width
    // exactly equal to `total_width`. Without that padding, a
    // composited row can end at `start_col + overlay_width +
    // after_width` — short of terminal width — and while current
    // rendering paths emit `\x1b[2K` / `\x1b[2J` before they write
    // the row (which visually clears the stale cells), the contract
    // we want to lock in is "a composited row is always
    // `total_width` cells wide".
    //
    // Guarding the contract at this level catches regressions in
    // downstream rendering paths that might legitimately write a row
    // without a preceding clear (logging sinks, headless diff
    // comparators, etc.).
    use aj_tui::ansi::visible_width;
    use aj_tui::impl_component_any;
    use aj_tui::tui::{OverlayAnchor, OverlayOptions, SizeValue};

    struct Short;
    impl aj_tui::component::Component for Short {
        impl_component_any!();
        fn render(&mut self, _width: usize) -> Vec<String> {
            // Only 5 visible columns on a 20-column terminal; the
            // after segment is empty so `afterPad` is the only
            // thing keeping the composited row at full width.
            vec!["AAAAA".to_string()]
        }
    }

    struct Five;
    impl aj_tui::component::Component for Five {
        impl_component_any!();
        fn render(&mut self, _width: usize) -> Vec<String> {
            vec!["OOOOO".to_string()]
        }
    }

    let terminal = VirtualTerminal::new(20, 3);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.root.add_child(Box::new(Short));

    let _handle = tui.show_overlay(
        Box::new(Five),
        OverlayOptions {
            width: Some(SizeValue::Absolute(5)),
            anchor: OverlayAnchor::TopLeft,
            col: Some(SizeValue::Absolute(5)),
            row: Some(SizeValue::Absolute(0)),
            ..Default::default()
        },
    );
    wait_for_render(&mut tui);

    // Inspect the writes log for the full-render frame. The first
    // rendered row starts at the beginning of the content section
    // (after the SYNC_BEGIN marker) and ends at the next `\r\n`.
    let writes = terminal.writes_joined();
    let sync_begin = "\x1b[?2026h";
    let content_start = writes
        .find(sync_begin)
        .map(|pos| pos + sync_begin.len())
        .unwrap_or(0);
    let slice = &writes[content_start..];
    // First `\r\n` in the content section delimits the first row.
    let row_end = slice
        .find("\r\n")
        .unwrap_or_else(|| panic!("no row separator in writes: {writes:?}"));
    let row = &slice[..row_end];
    let row_width = visible_width(row);
    assert_eq!(
        row_width, 20,
        "composited row must be exactly terminal_width cells wide; \
         got {row_width} from {row:?}",
    );
    // Sanity check that both the base prefix and the overlay landed
    // in the same row.
    assert!(row.contains("AAAAA"), "base prefix in row: {row:?}");
    assert!(row.contains("OOOOO"), "overlay content in row: {row:?}");
}

#[test]
fn pure_append_diff_emits_append_start_shortcut_not_redundant_carriage_return() {
    // Regression guard for the `append_start` shortcut.
    //
    // When a frame strictly appends rows, the engine emits
    // `<move to first_changed - 1>\r\n` before writing the new row
    // — no trailing `\r` after the `\r\n` because `\r\n` already
    // lands us at column 0. The collapsed `\r\n`-for-down logic
    // would otherwise emit `\r\n\r` (one extra byte).
    //
    // The test drives a two-frame scenario: initial frame with N
    // rows, then append one more row, and checks that the *second*
    // frame's writes log contains the `\r\n\x1b[2K` sequence
    // (append-start style) and not `\r\n\r\x1b[2K` (old style).
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    let lines = MutableLines::with_lines(["row-0", "row-1"]);
    tui.root.add_child(Box::new(lines.clone()));

    // First frame: full render. We don't care about its writes.
    wait_for_render(&mut tui);
    terminal.clear_writes();

    // Second frame: append one row. This triggers the appendStart
    // path in `differential_render`.
    lines.push("row-2");
    tui.request_render();
    wait_for_render(&mut tui);

    let writes = terminal.writes_joined();
    // Strip the SYNC_BEGIN prefix to focus on the move-and-write
    // sequence. We expect `\r\n\x1b[2Krow-2` immediately after
    // SYNC_BEGIN — no intermediate `\r`.
    let sync_begin = "\x1b[?2026h";
    let after_sync = writes
        .find(sync_begin)
        .map(|pos| &writes[pos + sync_begin.len()..])
        .unwrap_or(writes.as_str());
    // The append-start sequence is `\r\n\x1b[2K<content>`. If the old
    // collapsed path had run instead, we'd see `\r\n\r\x1b[2K...`.
    assert!(
        after_sync.starts_with("\r\n\x1b[2K"),
        "append-start should emit `\\r\\n\\x1b[2K` after SYNC_BEGIN; \
         got {:?}",
        after_sync.chars().take(40).collect::<String>(),
    );
    assert!(
        !after_sync.starts_with("\r\n\r"),
        "append-start must not emit the redundant `\\r` after `\\r\\n`; \
         got {:?}",
        after_sync.chars().take(40).collect::<String>(),
    );
    // And the appended content arrived.
    assert!(
        terminal.viewport().iter().any(|r| r.contains("row-2")),
        "appended row visible in viewport; got {:?}",
        terminal.viewport(),
    );
}
