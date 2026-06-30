//! Regression guard for the overlay compositor's image-row bypass.
//!
//! `composite_line_at` short-circuits when the base line carries an
//! inline-image escape (Kitty `\x1b_G…\x1b\\` or iTerm2 OSC 1337).
//! The base64 payload looks like printable text to the segment
//! walker, so without the bypass an overlay landing on the image row
//! would slice the payload at the overlay's start column and splice
//! arbitrary overlay bytes into the middle of the escape — corrupting
//! both the on-wire frame and the diff engine's `previous_lines`
//! byte-equality cache.
//!
//! These tests pin the contract by asserting that the image escape
//! reaches the terminal unchanged and that the overlay payload (`OVR`)
//! never appears in the captured writes when it would otherwise land
//! on the image row. A companion test confirms overlay rows that don't
//! intersect the image still composite normally.

use aj_tui_testkit as support;

use aj_tui::component::Component;
use aj_tui::image_protocol::{iterm2_sequence, kitty_sequence};
use aj_tui::impl_component_any;
use aj_tui::tui::{OverlayOptions, SizeValue, Tui};

use support::{StaticLines, VirtualTerminal, render_now};

/// One-line overlay rendered verbatim.
struct SingleLineOverlay(&'static str);

impl Component for SingleLineOverlay {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<aj_tui::Line> {
        vec![self.0.to_string().into()]
    }
}

/// Three-line overlay rendered verbatim.
struct ThreeLineOverlay([&'static str; 3]);

impl Component for ThreeLineOverlay {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<aj_tui::Line> {
        self.0.iter().map(|s| s.to_string().into()).collect()
    }
}

/// Locate the parameter terminator (`;`) that ends a Kitty escape's
/// parameter list. Returns the byte offset of the `;` relative to
/// the start of the input. Panics on malformed input — the tests
/// construct escapes with [`kitty_sequence`] so this stays
/// deterministic.
fn kitty_params_end(escape: &str) -> usize {
    escape
        .find(';')
        .expect("kitty escape must have a `;` ending its parameter list")
}

#[test]
fn overlay_does_not_corrupt_kitty_image_row() {
    let terminal = VirtualTerminal::new(80, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    // Single-row Kitty image: one escape, no empty padding rows.
    // The `C=1` parameter in `kitty_sequence` keeps the cursor put,
    // which is what the differential renderer assumes.
    let image_row = kitty_sequence("ZGF0YQ==", 4, 1, 99);
    tui.add_child(Box::new(StaticLines::new([
        "PLAIN_BASE".to_string(),
        image_row.clone(),
        "AFTER".to_string(),
    ])));

    // Overlay drops directly onto the image row at column 2.
    tui.show_overlay(
        Box::new(SingleLineOverlay("OVR")),
        OverlayOptions {
            row: Some(SizeValue::Absolute(1)),
            col: Some(SizeValue::Absolute(2)),
            width: Some(SizeValue::Absolute(3)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let writes = terminal.writes_joined();

    // The Kitty escape must survive byte-for-byte: opener, full
    // parameter list, base64 payload, and the `\x1b\\` terminator.
    assert!(
        writes.contains(&image_row),
        "Kitty image escape did not survive the overlay composite intact.\n\
         Expected substring: {image_row:?}\n\
         Got writes: {writes:?}",
    );

    // The overlay payload would only appear in the writes if the
    // compositor had spliced it into the image row. Neither base
    // text row contains `OVR`, so its absence is the smoking gun
    // for the bypass working.
    assert!(
        !writes.contains("OVR"),
        "overlay payload `OVR` leaked onto the image row; writes: {writes:?}",
    );

    // Belt-and-braces: even if a future change reshapes writes,
    // assert the base64 payload `ZGF0YQ==` is contiguous in the
    // output (no overlay bytes spliced between the `;` and the
    // closing `\x1b\\`).
    let params_end = kitty_params_end(&image_row);
    let payload = &image_row[params_end + 1..image_row.len() - 2]; // strip `\x1b\\`
    assert!(
        writes.contains(payload),
        "Kitty base64 payload was sliced; expected contiguous {payload:?} in writes: {writes:?}",
    );
}

#[test]
fn overlay_does_not_corrupt_iterm2_image_row() {
    let terminal = VirtualTerminal::new(80, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    // Single-row iTerm2 image: no cursor-up prefix (`rows == 1`),
    // just the OSC 1337 escape terminated with BEL.
    let image_row = iterm2_sequence("ZGF0YQ==", 4, 1, None);
    tui.add_child(Box::new(StaticLines::new([
        "PLAIN_BASE".to_string(),
        image_row.clone(),
        "AFTER".to_string(),
    ])));

    tui.show_overlay(
        Box::new(SingleLineOverlay("OVR")),
        OverlayOptions {
            row: Some(SizeValue::Absolute(1)),
            col: Some(SizeValue::Absolute(2)),
            width: Some(SizeValue::Absolute(3)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let writes = terminal.writes_joined();

    assert!(
        writes.contains(&image_row),
        "iTerm2 image escape did not survive the overlay composite intact.\n\
         Expected substring: {image_row:?}\n\
         Got writes: {writes:?}",
    );
    assert!(
        !writes.contains("OVR"),
        "overlay payload `OVR` leaked onto the image row; writes: {writes:?}",
    );

    // The OSC 1337 payload runs from after the `:` to the BEL.
    let colon = image_row
        .rfind(':')
        .expect("iTerm2 escape must contain a `:` before its payload");
    let payload = &image_row[colon + 1..image_row.len() - 1];
    assert!(
        writes.contains(payload),
        "iTerm2 base64 payload was sliced; expected contiguous {payload:?} in writes: {writes:?}",
    );
}

#[test]
fn overlay_composites_normally_on_rows_around_image() {
    let terminal = VirtualTerminal::new(80, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    // Sandwich the image between two plain rows that the overlay
    // also covers; we want to confirm the bypass is per-row, not
    // wholesale.
    let image_row = kitty_sequence("ZGF0YQ==", 4, 1, 101);
    tui.add_child(Box::new(StaticLines::new([
        "PLAIN_BASE".to_string(),
        image_row.clone(),
        "AFTER".to_string(),
    ])));

    // Three-row overlay spanning rows 0..=2 at column 2.
    tui.show_overlay(
        Box::new(ThreeLineOverlay(["TOP", "MID", "BOT"])),
        OverlayOptions {
            row: Some(SizeValue::Absolute(0)),
            col: Some(SizeValue::Absolute(2)),
            width: Some(SizeValue::Absolute(3)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    // Plain rows have the overlay composited in — we can read the
    // VT100 cell grid for those.
    let row0: String = (0..7)
        .map(|col| {
            terminal
                .cell(0, col)
                .map(|c| c.contents)
                .unwrap_or_default()
        })
        .collect();
    let row2: String = (0..7)
        .map(|col| {
            terminal
                .cell(2, col)
                .map(|c| c.contents)
                .unwrap_or_default()
        })
        .collect();
    assert!(
        row0.contains("TOP"),
        "row 0 should have overlay `TOP` composited in; got {row0:?}",
    );
    assert!(
        row2.contains("BOT"),
        "row 2 should have overlay `BOT` composited in; got {row2:?}",
    );

    // The image row's escape must reach the wire unchanged, and
    // `MID` (the overlay row that landed on the image) must not
    // appear in the captured writes.
    let writes = terminal.writes_joined();
    assert!(
        writes.contains(&image_row),
        "image row escape was mutated by the overlay composite; writes: {writes:?}",
    );
    assert!(
        !writes.contains("MID"),
        "overlay payload `MID` leaked onto the image row; writes: {writes:?}",
    );
}
