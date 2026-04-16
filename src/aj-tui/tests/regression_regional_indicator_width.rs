//! Regression coverage for regional-indicator (flag emoji) width.
//!
//! During streaming, a two-codepoint flag like "🇨🇳" often appears first as a
//! single regional-indicator codepoint "🇨". If we measure that intermediate
//! as width 1 while the terminal renders it at width 2, differential
//! rendering can drift and leave stale characters on screen. These tests pin
//! the behavior so that any future width-measurement refactor doesn't
//! regress it.

use aj_tui::ansi::{visible_width, wrap_text_with_ansi};

#[test]
fn treats_partial_flag_grapheme_as_full_width_to_avoid_streaming_drift() {
    let partial_flag = "🇨";
    let list_line = "      - 🇨";

    assert_eq!(visible_width(partial_flag), 2);
    assert_eq!(visible_width(list_line), 10);
}

#[test]
fn wraps_intermediate_partial_flag_list_line_before_overflow() {
    // Width 9 cannot fit "      - 🇨" if the flag is width 2 (8 + 2 = 10).
    // The line must wrap to avoid a terminal-side auto-wrap mismatch.
    let wrapped = wrap_text_with_ansi("      - 🇨", 9);

    assert_eq!(wrapped.len(), 2);
    assert_eq!(visible_width(&wrapped[0]), 7);
    assert_eq!(visible_width(&wrapped[1]), 2);
}

#[test]
fn treats_every_regional_indicator_singleton_grapheme_as_width_two() {
    // The full regional-indicator block is U+1F1E6..=U+1F1FF.
    for cp in 0x1F1E6u32..=0x1F1FFu32 {
        let ch = char::from_u32(cp).expect("valid regional indicator codepoint");
        let s = ch.to_string();
        assert_eq!(
            visible_width(&s),
            2,
            "expected {} (U+{:04X}) to be width 2",
            s,
            cp,
        );
    }
}

#[test]
fn keeps_full_flag_pairs_at_width_two() {
    let samples = ["🇯🇵", "🇺🇸", "🇬🇧", "🇨🇳", "🇩🇪", "🇫🇷"];
    for flag in samples {
        assert_eq!(visible_width(flag), 2, "expected {} to be width 2", flag);
    }
}

#[test]
fn keeps_common_streaming_emoji_intermediates_at_stable_width() {
    // A mix of base emoji, skin-tone-modified emoji, variation selectors,
    // and ZWJ sequences. All should measure as width 2 regardless of
    // whether the joiner has arrived yet.
    let samples = [
        "👍",
        "👍🏻",
        "✅",
        "⚡",
        "⚡\u{FE0F}",
        "👨",
        "👨\u{200D}💻",
        "🏳\u{FE0F}\u{200D}🌈",
    ];
    for sample in samples {
        assert_eq!(
            visible_width(sample),
            2,
            "expected {:?} to be width 2",
            sample,
        );
    }
}
