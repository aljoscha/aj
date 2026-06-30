//! Per-codepoint Unicode property tables that `vaxis` needs for width
//! measurement.
//!
//! The three tables (East Asian Width, General Category, Emoji_Presentation)
//! are generated at build time by `build.rs` from the UCD text files vendored
//! under `ucd/`, pinned to a single Unicode version (see `ucd/README.md`). We
//! generate from the authoritative data rather than shipping a hand-written
//! snapshot so the tables stay faithful and refreshable.
//!
//! We deliberately do not generate the Grapheme_Cluster_Break property here.
//! UAX#29 grapheme segmentation is delegated to the `unicode-segmentation`
//! crate, a UCD-generated, well-tested implementation of the standard
//! algorithm pinned to the same Unicode version. This mirrors how the port
//! delegates image decoding to the `image` crate.

use std::cmp::Ordering;

/// The East_Asian_Width property (UAX#11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EastAsianWidth {
    Neutral,
    Ambiguous,
    Halfwidth,
    Fullwidth,
    Narrow,
    Wide,
}

/// The General_Category property: the full UCD set of 30 categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneralCategory {
    UppercaseLetter,
    LowercaseLetter,
    TitlecaseLetter,
    ModifierLetter,
    OtherLetter,
    NonspacingMark,
    SpacingMark,
    EnclosingMark,
    DecimalNumber,
    LetterNumber,
    OtherNumber,
    ConnectorPunctuation,
    DashPunctuation,
    OpenPunctuation,
    ClosePunctuation,
    InitialPunctuation,
    FinalPunctuation,
    OtherPunctuation,
    MathSymbol,
    CurrencySymbol,
    ModifierSymbol,
    OtherSymbol,
    SpaceSeparator,
    LineSeparator,
    ParagraphSeparator,
    Control,
    Format,
    Surrogate,
    PrivateUse,
    Unassigned,
}

include!(concat!(env!("OUT_DIR"), "/tables.rs"));

/// Binary-searches a sorted, disjoint `(start, end, value)` range table for
/// the range containing `cp`, returning `default` when no range covers it.
fn lookup<T: Copy>(table: &[(u32, u32, T)], cp: u32, default: T) -> T {
    match table.binary_search_by(|&(start, end, _)| {
        if cp < start {
            Ordering::Greater
        } else if cp > end {
            Ordering::Less
        } else {
            Ordering::Equal
        }
    }) {
        Ok(i) => table[i].2,
        Err(_) => default,
    }
}

/// Returns the East_Asian_Width of `cp`.
///
/// Codepoints absent from the data file take the file's `@missing` default of
/// Neutral. NOTE: UAX#11 also assigns Wide to unassigned codepoints in certain
/// CJK blocks. We do not apply that derivation, since it is prose in the data
/// file rather than a data row and only affects unassigned codepoints, which
/// never appear in real text.
pub fn east_asian_width(cp: u32) -> EastAsianWidth {
    lookup(EAST_ASIAN_WIDTH, cp, EastAsianWidth::Neutral)
}

/// Returns the General_Category of `cp`, defaulting to `Unassigned` for
/// codepoints absent from the data file.
pub fn general_category(cp: u32) -> GeneralCategory {
    lookup(GENERAL_CATEGORY, cp, GeneralCategory::Unassigned)
}

/// Returns whether `cp` has the Emoji_Presentation property (UTS#51), meaning
/// it defaults to an emoji (wide, colorful) rendering absent a variation
/// selector.
pub fn is_emoji_presentation(cp: u32) -> bool {
    EMOJI_PRESENTATION
        .binary_search_by(|&(start, end)| {
            if cp < start {
                Ordering::Greater
            } else if cp > end {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn east_asian_width_spot_checks() {
        // 'A' is Narrow per the UCD (the Latin block is explicitly Na).
        assert_eq!(east_asian_width(u32::from('A')), EastAsianWidth::Narrow);
        // '世' (U+4E16) is a CJK ideograph: Wide.
        assert_eq!(east_asian_width(u32::from('世')), EastAsianWidth::Wide);
        // U+FF21 FULLWIDTH LATIN CAPITAL LETTER A.
        assert_eq!(east_asian_width(0xFF21), EastAsianWidth::Fullwidth);
        // U+FF61 HALFWIDTH IDEOGRAPHIC FULL STOP.
        assert_eq!(east_asian_width(0xFF61), EastAsianWidth::Halfwidth);
        // U+00A1 INVERTED EXCLAMATION MARK is Ambiguous.
        assert_eq!(east_asian_width(0x00A1), EastAsianWidth::Ambiguous);
        // A codepoint with no listed range falls back to Neutral.
        assert_eq!(east_asian_width(0x0378), EastAsianWidth::Neutral);
    }

    #[test]
    fn general_category_spot_checks() {
        // U+0301 COMBINING ACUTE ACCENT is a nonspacing mark (Mn).
        assert_eq!(general_category(0x0301), GeneralCategory::NonspacingMark);
        // U+20E3 COMBINING ENCLOSING KEYCAP is an enclosing mark (Me).
        assert_eq!(general_category(0x20E3), GeneralCategory::EnclosingMark);
        // '5' is a decimal number (Nd).
        assert_eq!(
            general_category(u32::from('5')),
            GeneralCategory::DecimalNumber
        );
        // '_' is connector punctuation (Pc).
        assert_eq!(
            general_category(u32::from('_')),
            GeneralCategory::ConnectorPunctuation
        );
        // 'A' is an uppercase letter (Lu).
        assert_eq!(
            general_category(u32::from('A')),
            GeneralCategory::UppercaseLetter
        );
        // Reserved codepoints are Unassigned (Cn).
        assert_eq!(general_category(0x0378), GeneralCategory::Unassigned);
    }

    #[test]
    fn emoji_presentation_spot_checks() {
        // U+1F642 SLIGHTLY SMILING FACE defaults to emoji presentation.
        assert!(is_emoji_presentation(0x1F642));
        // 'A' is not an emoji.
        assert!(!is_emoji_presentation(u32::from('A')));
        // U+2764 HEAVY BLACK HEART is text-default (no Emoji_Presentation).
        assert!(!is_emoji_presentation(0x2764));
    }
}
