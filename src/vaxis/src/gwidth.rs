//! Display-width measurement for grapheme clusters.
//!
//! Reproduces upstream's bespoke width logic. It does not call a `wcwidth`
//! library: width is computed from the East Asian Width and General Category
//! properties plus a hardcoded zero-width list and a set of emoji and
//! variation-selector rules pinned by the tests below.

use vaxis_ucd::{EastAsianWidth, GeneralCategory};

use crate::unicode::grapheme_iterator;

/// The method to use when calculating the width of a grapheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Unicode,
    Wcwidth,
    NoZwj,
}

/// Width contribution of a single codepoint, derived from its East Asian
/// Width and General Category.
///
/// Uses a signed intermediate so control characters can contribute the
/// sentinel `-1`, which callers clamp away with `max(0, ...)`. The hardcoded
/// zero-width list catches codepoints (joiners, variation selectors, the BOM)
/// whose General Category alone does not mark them zero-width.
fn eaw_to_width(cp: u32, eaw: EastAsianWidth) -> i16 {
    if cp == 0 {
        return 0;
    }
    if cp < 32 || (0x7f..0xa0).contains(&cp) {
        return -1;
    }

    match vaxis_ucd::general_category(cp) {
        GeneralCategory::NonspacingMark | GeneralCategory::EnclosingMark => return 0,
        _ => {}
    }

    // Zero-width codepoints not covered by General Category.
    if cp == 0x00ad // soft hyphen
        || cp == 0x200b // zero-width space
        || cp == 0x200c // zero-width non-joiner
        || cp == 0x200d // zero-width joiner
        || cp == 0x2060 // word joiner
        || cp == 0x034f // combining grapheme joiner
        || cp == 0xfeff // zero-width no-break space (BOM)
        || (0x180b..=0x180d).contains(&cp) // Mongolian variation selectors
        || (0xfe00..=0xfe0f).contains(&cp) // variation selectors
        || (0xe0100..=0xe01ef).contains(&cp)
    // plane-14 variation selectors
    {
        return 0;
    }

    match eaw {
        EastAsianWidth::Fullwidth | EastAsianWidth::Wide => 2,
        _ => 1,
    }
}

/// Returns the width of `s` in terminal cells, measured by `method`.
pub fn gwidth(s: &str, method: Method) -> u16 {
    match method {
        Method::Unicode => {
            let mut total: u16 = 0;
            for grapheme in grapheme_iterator(s) {
                let mut width: i16 = 0;
                let mut has_emoji_vs = false;
                let mut has_text_vs = false;
                let mut has_emoji_presentation = false;
                let mut ri_count: u8 = 0;

                for ch in grapheme.bytes(s).chars() {
                    let cp = u32::from(ch);

                    if cp == 0xfe0f {
                        has_emoji_vs = true;
                        continue;
                    }
                    if cp == 0xfe0e {
                        has_text_vs = true;
                        continue;
                    }
                    if vaxis_ucd::is_emoji_presentation(cp) {
                        has_emoji_presentation = true;
                    }
                    if (0x1f1e6..=0x1f1ff).contains(&cp) {
                        ri_count += 1;
                    }

                    let w = eaw_to_width(cp, vaxis_ucd::east_asian_width(cp));
                    // Take the max of the non-zero per-codepoint widths.
                    if w > 0 && w > width {
                        width = w;
                    }
                }

                if has_text_vs {
                    // Explicit text presentation keeps the width as-is.
                    width = width.max(1);
                } else if has_emoji_vs || has_emoji_presentation || ri_count == 2 {
                    // Emoji presentation or a flag pair forces width 2.
                    width = width.max(2);
                }

                total += width.max(0).unsigned_abs();
            }
            total
        }
        Method::Wcwidth => {
            let mut total: u16 = 0;
            for ch in s.chars() {
                let cp = u32::from(ch);
                let w: i16 = match cp {
                    // Undo an emoji skin-tone-selector override and treat them
                    // as width 2.
                    0x1f3fb..=0x1f3ff => 2,
                    _ => eaw_to_width(cp, vaxis_ucd::east_asian_width(cp)),
                };
                total += w.max(0).unsigned_abs();
            }
            total
        }
        Method::NoZwj => {
            // Drop ZWJ joins, then sum the Unicode width of each piece.
            s.split('\u{200d}')
                .map(|piece| gwidth(piece, Method::Unicode))
                .sum()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gwidth_a() {
        assert_eq!(gwidth("a", Method::Unicode), 1);
        assert_eq!(gwidth("a", Method::Wcwidth), 1);
        assert_eq!(gwidth("a", Method::NoZwj), 1);
    }

    #[test]
    fn gwidth_emoji_with_zwj() {
        assert_eq!(gwidth("👩\u{200D}🚀", Method::Unicode), 2);
        assert_eq!(gwidth("👩\u{200D}🚀", Method::Wcwidth), 4);
        assert_eq!(gwidth("👩\u{200D}🚀", Method::NoZwj), 4);
    }

    #[test]
    fn gwidth_emoji_with_vs16_selector() {
        let s = "\u{2764}\u{fe0f}";
        assert_eq!(gwidth(s, Method::Unicode), 2);
        assert_eq!(gwidth(s, Method::Wcwidth), 1);
        assert_eq!(gwidth(s, Method::NoZwj), 2);
    }

    #[test]
    fn gwidth_emoji_with_skin_tone_selector() {
        assert_eq!(gwidth("👋🏿", Method::Unicode), 2);
        assert_eq!(gwidth("👋🏿", Method::Wcwidth), 4);
        assert_eq!(gwidth("👋🏿", Method::NoZwj), 2);
    }

    #[test]
    fn gwidth_zero_width_space() {
        assert_eq!(gwidth("\u{200B}", Method::Unicode), 0);
        assert_eq!(gwidth("\u{200B}", Method::Wcwidth), 0);
    }

    #[test]
    fn gwidth_zero_width_non_joiner() {
        assert_eq!(gwidth("\u{200C}", Method::Unicode), 0);
        assert_eq!(gwidth("\u{200C}", Method::Wcwidth), 0);
    }

    #[test]
    fn gwidth_combining_marks() {
        // Hebrew combining mark.
        assert_eq!(gwidth("\u{05B0}", Method::Unicode), 0);
        // Devanagari combining mark.
        assert_eq!(gwidth("\u{093C}", Method::Unicode), 0);
    }

    #[test]
    fn gwidth_flag_emoji_regional_indicators() {
        // US flag.
        assert_eq!(gwidth("🇺🇸", Method::Unicode), 2);
        // UK flag.
        assert_eq!(gwidth("🇬🇧", Method::Unicode), 2);
    }

    #[test]
    fn gwidth_text_variation_selector() {
        // U+2764 (heavy black heart) + U+FE0E (text variation selector),
        // width 1 with text presentation.
        assert_eq!(gwidth("\u{2764}\u{fe0e}", Method::Unicode), 1);
    }

    #[test]
    fn gwidth_keycap_sequence() {
        // Digit 1 + U+FE0F + U+20E3 (combining enclosing keycap), width 2.
        assert_eq!(gwidth("1\u{fe0f}\u{20e3}", Method::Unicode), 2);
    }

    #[test]
    fn gwidth_base_letter_with_combining_mark() {
        // 'a' + combining acute accent (NFD form), width 1.
        assert_eq!(gwidth("a\u{0301}", Method::Unicode), 1);
    }
}
