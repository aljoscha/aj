//! Generator for the Unicode property tables that `vaxis` needs.
//!
//! Consumes UCD data files (`UnicodeData.txt`, `EastAsianWidth.txt`,
//! `emoji-data.txt`, `GraphemeBreakProperty.txt`) and emits Rust tables for
//! the four properties `gwidth` and grapheme breaking rely on:
//! `east_asian_width`, `general_category`, `is_emoji_presentation`, and
//! `grapheme_break`. No generation logic yet.
