//! End-to-end image rendering through the [`Image`] component:
//! given a virtual terminal with iTerm2 / Kitty capabilities,
//! the component emits the expected multi-row protocol shape
//! that the [`Tui`] diff renderer can paint without tripping
//! width validation.
//!
//! [`Image`]: aj_tui::components::image::Image
//! [`Tui`]: aj_tui::tui::Tui

use aj_tui::capabilities::{
    ImageProtocol, TerminalCapabilities, reset_capabilities_cache, set_capabilities,
};
use aj_tui::component::Component;
use aj_tui::components::image::Image;
use aj_tui::image_protocol::extract_kitty_image_ids;
use serial_test::serial;

fn make_image() -> Image {
    Image::new(
        "ZGF0YQ==".to_string(),
        "image/png".to_string(),
        (100, 50),
        (40, 20),
        Some((10, 10)),
    )
}

#[test]
#[serial]
fn iterm2_image_renders_as_multi_row_with_cursor_up_prefix_on_last_row() {
    set_capabilities(TerminalCapabilities {
        hyperlinks: false,
        true_color: false,
        images: Some(ImageProtocol::ITerm2),
    });

    let mut img = make_image();
    let lines = img.render(80);
    assert!(lines.len() >= 2, "image must reserve >=2 rows: {lines:?}");
    // Empty leading rows; OSC 1337 on the last row.
    let last = lines.last().expect("non-empty");
    let n = lines.len();
    for head in &lines[..n - 1] {
        assert!(head.is_empty(), "leading row should be empty: {head:?}");
    }
    assert!(
        last.starts_with(&format!("\x1b[{}A", n - 1)),
        "iTerm2 last row must lift the cursor by N-1: {last:?}",
    );
    assert!(
        last.contains("\x1b]1337;File=inline=1;"),
        "iTerm2 row must carry the OSC 1337 escape: {last:?}",
    );
    assert!(last.ends_with('\x07'));

    reset_capabilities_cache();
}

#[test]
#[serial]
fn kitty_image_renders_with_escape_on_first_row_and_stable_id() {
    set_capabilities(TerminalCapabilities {
        hyperlinks: false,
        true_color: false,
        images: Some(ImageProtocol::Kitty),
    });

    let mut img = make_image();
    let first = img.render(80);
    let n = first.len();
    assert!(n >= 2, "Kitty image must reserve >=2 rows: {first:?}");
    assert!(
        first[0].contains("\x1b_G"),
        "Kitty row 0 must carry the graphics escape: {:?}",
        first[0],
    );
    for tail in &first[1..] {
        assert!(tail.is_empty(), "trailing rows must be empty: {tail:?}");
    }

    let ids = extract_kitty_image_ids(&first[0]);
    assert_eq!(ids.len(), 1, "expected exactly one image id, got {ids:?}");
    let id0 = ids[0];

    // Re-rendering the same component reuses the allocated ID so
    // the diff engine can match the placement across frames.
    let second = img.render(80);
    let ids2 = extract_kitty_image_ids(&second[0]);
    assert_eq!(ids2, vec![id0]);

    reset_capabilities_cache();
}

#[test]
#[serial]
fn fallback_to_textual_placeholder_without_image_capability() {
    set_capabilities(TerminalCapabilities {
        hyperlinks: false,
        true_color: false,
        images: None,
    });

    let mut img = make_image();
    let lines = img.render(80);
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("[image: image/png · 100x50]"),
        "fallback should annotate mime + dims: {lines:?}",
    );

    reset_capabilities_cache();
}
