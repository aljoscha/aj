//! Component-level tests for the single-line `TruncatedText` widget. No
//! virtual terminal needed — we call `render(width)` directly and assert on
//! the returned lines.

mod support;

use aj_tui::ansi::visible_width;
use aj_tui::component::Component;
use aj_tui::components::truncated_text::TruncatedText;

use nu_ansi_term::Color;

use support::strip_ansi;

fn render(text: &str, padding_x: usize, padding_y: usize, width: usize) -> Vec<String> {
    let mut t = TruncatedText::new(text);
    t.set_padding_x(padding_x);
    t.set_padding_y(padding_y);
    t.render(width)
}

#[test]
fn pads_output_lines_to_exactly_match_width() {
    let lines = render("Hello world", 1, 0, 50);

    // One content line, no vertical padding.
    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 50);
}

#[test]
fn pads_output_with_vertical_padding_lines_to_width() {
    let lines = render("Hello", 0, 2, 40);

    // 2 top padding + 1 content + 2 bottom padding = 5 total.
    assert_eq!(lines.len(), 5);

    // All lines — including the vertical-padding blanks — should be exactly
    // the target width.
    for line in &lines {
        assert_eq!(visible_width(line), 40);
    }
}

#[test]
fn truncates_long_text_and_pads_to_width() {
    let long = "This is a very long piece of text that will definitely exceed the available width";
    let lines = render(long, 1, 0, 30);

    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 30);

    // Should contain ellipsis after ANSI codes are stripped.
    let stripped = strip_ansi(&lines[0]);
    assert!(
        stripped.contains("..."),
        "expected ellipsis in {:?}",
        stripped
    );
}

#[test]
fn preserves_ansi_codes_in_output_and_pads_correctly() {
    let styled = format!(
        "{} {}",
        Color::Red.paint("Hello"),
        Color::Blue.paint("world")
    );
    let lines = render(&styled, 1, 0, 40);

    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 40);
    assert!(lines[0].contains("\x1b["));
}

#[test]
fn truncates_styled_text_and_adds_reset_code_before_ellipsis() {
    let long_styled = Color::Red
        .paint("This is a very long red text that will be truncated")
        .to_string();
    let lines = render(&long_styled, 1, 0, 20);

    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 20);
    assert!(lines[0].contains("\x1b[0m..."));
}

#[test]
fn handles_text_that_fits_exactly() {
    // With padding_x = 1, available width is 30 - 2 = 28; "Hello world" fits.
    let lines = render("Hello world", 1, 0, 30);

    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 30);

    let stripped = strip_ansi(&lines[0]);
    assert!(!stripped.contains("..."));
}

#[test]
fn handles_empty_text() {
    let lines = render("", 1, 0, 30);

    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 30);
}

#[test]
fn stops_at_newline_and_only_shows_first_line() {
    let multiline = "First line\nSecond line\nThird line";
    let lines = render(multiline, 1, 0, 40);

    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 40);

    let stripped = strip_ansi(&lines[0]);
    assert!(stripped.contains("First line"));
    assert!(!stripped.contains("Second line"));
    assert!(!stripped.contains("Third line"));
}

#[test]
fn truncates_first_line_even_with_newlines_in_text() {
    let long_multi = "This is a very long first line that needs truncation\nSecond line";
    let lines = render(long_multi, 1, 0, 25);

    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 25);

    let stripped = strip_ansi(&lines[0]);
    assert!(stripped.contains("..."));
    assert!(!stripped.contains("Second line"));
}
