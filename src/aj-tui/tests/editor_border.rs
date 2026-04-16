//! Tests for the `Editor`'s top/bottom border rendering.
//!
//! The editor frames its content with a pair of horizontal `─` lines.
//! When the content scrolls out of the visible window, those lines
//! sprout scroll indicators: `─── ↑ N more ` for content above,
//! `─── ↓ N more ` for content below. These tests guard the shape of
//! those lines because the borders are the most visible piece of the
//! editor's chrome and drifting between `─` and `-` or losing the
//! indicators entirely is exactly the kind of regression that won't
//! surface in a content-only assertion.

mod support;

use aj_tui::ansi::visible_width;
use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::tui::RenderHandle;

use support::strip_ansi;
use support::themes::identity_editor_theme;

fn editor() -> Editor {
    let mut e = Editor::new(RenderHandle::detached(), identity_editor_theme());
    e.disable_submit = true;
    e.set_padding_x(0);
    e.set_focused(true);
    e
}

/// Width to use for rendered lines. Wide enough that the scroll
/// indicator plus its `─` padding exercises both code paths.
const WIDTH: usize = 40;

#[test]
fn top_and_bottom_borders_are_full_width_horizontal_lines() {
    let mut e = editor();
    e.set_text("hello");

    let lines = e.render(WIDTH);
    assert!(lines.len() >= 2, "expected at least a top + bottom border");

    let top = strip_ansi(&lines[0]);
    let bottom = strip_ansi(&lines[lines.len() - 1]);

    // Every cell should be the U+2500 BOX DRAWINGS LIGHT HORIZONTAL.
    assert_eq!(
        top,
        "─".repeat(WIDTH),
        "top border should be `─` the full width",
    );
    assert_eq!(
        bottom,
        "─".repeat(WIDTH),
        "bottom border should be `─` the full width",
    );
    assert_eq!(
        visible_width(&top),
        WIDTH,
        "top border visible width should match render width",
    );
    assert_eq!(
        visible_width(&bottom),
        WIDTH,
        "bottom border visible width should match render width",
    );
}

#[test]
fn scrolled_content_shows_down_indicator_on_bottom_border() {
    // Fill the editor with enough lines to exceed the visible window
    // (at padding_x=0 and this width, the editor caps visible lines at
    // either 5 or 30% of terminal height). 30 lines definitely spills.
    let mut e = editor();
    let body: Vec<String> = (0..30).map(|i| format!("line {}", i)).collect();
    e.set_text(&body.join("\n"));
    // Put the cursor at the top so there's content below but none
    // above. The bottom border should have the `↓ N more ` indicator,
    // the top border should still be a plain `─` line.
    e.set_text(&body.join("\n"));
    // After set_text, cursor is at the end; move it back to the top.
    for _ in 0..body.len() {
        e.handle_input(&aj_tui::keys::Key::up());
    }

    let lines = e.render(WIDTH);
    let top = strip_ansi(&lines[0]);
    let bottom = strip_ansi(&lines[lines.len() - 1]);

    assert_eq!(
        top,
        "─".repeat(WIDTH),
        "top border without content above should be plain `─`",
    );
    assert!(
        bottom.starts_with("─── ↓ "),
        "bottom border should start with the down-indicator prefix; got {:?}",
        bottom,
    );
    assert!(
        bottom.ends_with('─'),
        "bottom border should be padded out to the edge with `─`",
    );
    assert_eq!(
        visible_width(&bottom),
        WIDTH,
        "bottom border with indicator should still span the full width",
    );
}

#[test]
fn scrolled_past_top_shows_up_indicator_on_top_border() {
    // Same setup but with the cursor at the bottom: content scrolls up
    // so there's content above and none below.
    let mut e = editor();
    let body: Vec<String> = (0..30).map(|i| format!("line {}", i)).collect();
    e.set_text(&body.join("\n"));

    let lines = e.render(WIDTH);
    let top = strip_ansi(&lines[0]);
    let bottom = strip_ansi(&lines[lines.len() - 1]);

    assert!(
        top.starts_with("─── ↑ "),
        "top border with scrolled content should start with up indicator; got {:?}",
        top,
    );
    assert!(
        top.ends_with('─'),
        "top border with indicator should pad to the edge with `─`",
    );
    assert_eq!(
        visible_width(&top),
        WIDTH,
        "top border with indicator should still span the full width",
    );
    assert_eq!(
        bottom,
        "─".repeat(WIDTH),
        "bottom border without content below should be plain `─`",
    );
}

#[test]
fn border_color_is_applied_to_every_border_line() {
    // Use the default theme which wraps the border in a dim SGR code.
    // We can't assert on the exact code without coupling to the theme,
    // but we *can* assert that the border line contains an SGR escape —
    // the identity theme would not, so a regression that dropped the
    // theme call on one branch would fail here.
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_padding_x(0);
    e.set_focused(true);
    e.set_text("body");

    let lines = e.render(WIDTH);
    let top = &lines[0];
    let bottom = &lines[lines.len() - 1];

    assert!(
        top.contains("\x1b["),
        "top border should carry the theme's SGR codes; got {:?}",
        top,
    );
    assert!(
        bottom.contains("\x1b["),
        "bottom border should carry the theme's SGR codes; got {:?}",
        bottom,
    );
}
