//! Pure-function tests for the ANSI-aware truncation and width measurement
//! helpers. No terminal required.

use aj_tui::ansi::{truncate_to_width, visible_width};

const SGR_RESET: &str = "\x1b[0m";

#[test]
fn keeps_output_within_width_for_very_large_unicode_input() {
    let text: String = "🙂界".repeat(100_000);
    let truncated = truncate_to_width(&text, 40, "…", false);

    assert!(visible_width(&truncated) <= 40);
    assert!(truncated.ends_with(&format!("…{}", SGR_RESET)));
}

#[test]
fn preserves_ansi_styling_for_kept_text_and_resets_before_and_after_ellipsis() {
    let text = format!("\x1b[31m{}\x1b[0m", "hello ".repeat(1000));
    let truncated = truncate_to_width(&text, 20, "…", false);

    assert!(visible_width(&truncated) <= 20);
    assert!(truncated.contains("\x1b[31m"));
    assert!(truncated.ends_with(&format!("{}…{}", SGR_RESET, SGR_RESET)));
}

#[test]
fn handles_malformed_ansi_escape_prefixes_without_hanging() {
    let text = format!("abc\x1bnot-ansi {}", "🙂".repeat(1000));
    let truncated = truncate_to_width(&text, 20, "…", false);

    assert!(visible_width(&truncated) <= 20);
}

#[test]
fn clips_wide_ellipsis_safely_and_brackets_it_with_resets() {
    assert_eq!(truncate_to_width("abcdef", 1, "🙂", false), "");
    assert_eq!(
        truncate_to_width("abcdef", 2, "🙂", false),
        "\x1b[0m🙂\x1b[0m",
    );
    assert!(visible_width(&truncate_to_width("abcdef", 2, "🙂", false)) <= 2);
}

#[test]
fn returns_original_text_when_it_already_fits_even_if_ellipsis_is_too_wide() {
    assert_eq!(truncate_to_width("a", 2, "🙂", false), "a");
    assert_eq!(truncate_to_width("界", 2, "🙂", false), "界");
}

#[test]
fn pads_truncated_output_to_requested_width() {
    let truncated = truncate_to_width("🙂界🙂界🙂界", 8, "…", true);
    assert_eq!(visible_width(&truncated), 8);
}

#[test]
fn keeps_a_contiguous_prefix_instead_of_skipping_a_wide_grapheme_and_resuming_later() {
    let truncated = truncate_to_width("🙂\t界 \x1b_abc\x07", 7, "…", true);
    assert_eq!(truncated, "🙂\t\x1b[0m…\x1b[0m ");
}

#[test]
fn adds_a_trailing_reset_when_truncating_without_an_ellipsis() {
    let input = format!("\x1b[31m{}", "hello".repeat(100));
    let truncated = truncate_to_width(&input, 10, "", false);
    assert!(visible_width(&truncated) <= 10);
    assert!(truncated.ends_with(SGR_RESET));
}

#[test]
fn visible_width_counts_tabs_inline_and_skips_ansi_inline() {
    // Tabs count as 3 columns inline.
    assert_eq!(visible_width("\t\x1b[31m界\x1b[0m"), 5);
}
