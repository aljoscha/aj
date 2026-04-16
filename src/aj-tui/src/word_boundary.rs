//! Three-class word-boundary helpers shared by the [`Editor`] and
//! single-line [`Input`] components.
//!
//! Both components walk the cursor by "word" using the same three-class
//! segmentation model: a grapheme is whitespace, ASCII punctuation, or
//! "word" (everything else — letters, digits, most non-Latin scripts,
//! emoji, combining marks, etc.). A word jump is "skip whitespace, then
//! skip a run of the class of the next non-whitespace grapheme",
//! mirroring the standard readline / Emacs behavior where `foo bar...`
//! breaks into three words: `foo`, `bar`, `...`.
//!
//! The high-level [`word_boundary_left`] / [`word_boundary_right`]
//! functions are sufficient for plain text. The lower-level
//! [`skip_whitespace_forward`] and [`skip_word_class_forward`] are
//! exposed so the editor can splice paste-marker handling between the
//! whitespace skip and the class skip without duplicating either step.
//!
//! [`Editor`]: crate::components::editor::Editor
//! [`Input`]: crate::components::text_input::Input

use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::{is_punctuation_grapheme, is_whitespace_grapheme};

/// Byte offset of the previous word boundary in `text`, scanning
/// backward from `cursor`.
///
/// Returns `0` when the cursor is already at the start of `text`.
/// Walking backward: skip a run of trailing whitespace, then skip a
/// run of graphemes in whichever of the two remaining classes
/// (punctuation, word) the last remaining grapheme belongs to. The
/// returned offset always lands on a grapheme boundary.
pub fn word_boundary_left(text: &str, cursor: usize) -> usize {
    if cursor == 0 {
        return 0;
    }
    let before = &text[..cursor];
    let graphemes: Vec<(usize, &str)> = before.grapheme_indices(true).collect();
    let mut idx = graphemes.len();
    let mut new_col = cursor;

    // Skip trailing whitespace.
    while idx > 0 && is_whitespace_grapheme(graphemes[idx - 1].1) {
        idx -= 1;
        new_col = graphemes[idx].0;
    }

    if idx > 0 {
        let last = graphemes[idx - 1].1;
        if is_punctuation_grapheme(last) {
            while idx > 0 && is_punctuation_grapheme(graphemes[idx - 1].1) {
                idx -= 1;
                new_col = graphemes[idx].0;
            }
        } else {
            while idx > 0 {
                let g = graphemes[idx - 1].1;
                if is_whitespace_grapheme(g) || is_punctuation_grapheme(g) {
                    break;
                }
                idx -= 1;
                new_col = graphemes[idx].0;
            }
        }
    }
    new_col
}

/// Byte offset of the next word boundary in `text`, scanning forward
/// from `cursor`.
///
/// Equivalent to `skip_word_class_forward(text, skip_whitespace_forward(text, cursor))`.
/// Use the lower-level pair directly if you need to inspect the
/// position right after the whitespace skip — e.g. to check for a
/// paste marker — before continuing the class skip.
pub fn word_boundary_right(text: &str, cursor: usize) -> usize {
    let after_ws = skip_whitespace_forward(text, cursor);
    skip_word_class_forward(text, after_ws)
}

/// Byte offset reached by skipping a run of whitespace graphemes
/// forward from `cursor`. Returns `cursor` unchanged when it already
/// sits on a non-whitespace grapheme or past the end of `text`.
pub fn skip_whitespace_forward(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let after = &text[cursor..];
    let mut col = cursor;
    for (_i, g) in after.grapheme_indices(true) {
        if !is_whitespace_grapheme(g) {
            break;
        }
        col += g.len();
    }
    col
}

/// Byte offset reached by skipping a run of graphemes that share the
/// class (punctuation or word) of the grapheme at `cursor`. The
/// caller is expected to have already moved past any preceding
/// whitespace via [`skip_whitespace_forward`]; if `cursor` itself
/// points at whitespace this function returns `cursor` unchanged
/// (whitespace is neither "punctuation" nor "word" under the
/// three-class model used here).
pub fn skip_word_class_forward(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let after = &text[cursor..];
    let graphemes: Vec<(usize, &str)> = after.grapheme_indices(true).collect();
    if graphemes.is_empty() {
        return cursor;
    }
    let first = graphemes[0].1;
    let mut col = cursor;
    if is_whitespace_grapheme(first) {
        return col;
    }
    if is_punctuation_grapheme(first) {
        for (_i, g) in &graphemes {
            if !is_punctuation_grapheme(g) {
                break;
            }
            col += g.len();
        }
    } else {
        for (_i, g) in &graphemes {
            if is_whitespace_grapheme(g) || is_punctuation_grapheme(g) {
                break;
            }
            col += g.len();
        }
    }
    col
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- word_boundary_left ----

    #[test]
    fn left_at_start_returns_zero() {
        assert_eq!(word_boundary_left("foo bar", 0), 0);
    }

    #[test]
    fn left_skips_word_run() {
        // Cursor at end of "foo bar" → boundary before "bar".
        assert_eq!(word_boundary_left("foo bar", 7), 4);
    }

    #[test]
    fn left_eats_trailing_whitespace_then_word() {
        // Trailing whitespace folds into the same jump as the
        // preceding word.
        assert_eq!(word_boundary_left("foo  ", 5), 0);
    }

    #[test]
    fn left_treats_punctuation_run_as_its_own_word() {
        // From end of "foo bar..." → first jump lands before "...".
        assert_eq!(word_boundary_left("foo bar...", 10), 7);
        // Second jump lands before "bar".
        assert_eq!(word_boundary_left("foo bar...", 7), 4);
        // Third jump lands at start.
        assert_eq!(word_boundary_left("foo bar...", 4), 0);
    }

    #[test]
    fn left_treats_emoji_as_word_character() {
        // 😀 is U+1F600, four UTF-8 bytes per occurrence.
        let s = "foo 😀😀";
        assert_eq!(word_boundary_left(s, s.len()), 4);
    }

    // ---- word_boundary_right ----

    #[test]
    fn right_at_end_returns_len() {
        assert_eq!(word_boundary_right("foo", 3), 3);
    }

    #[test]
    fn right_skips_leading_whitespace_then_word() {
        // From start of "   foo bar" → boundary after "   foo".
        assert_eq!(word_boundary_right("   foo bar", 0), 6);
    }

    #[test]
    fn right_walks_word_then_punctuation_then_word_runs() {
        let s = "foo bar... baz";
        assert_eq!(word_boundary_right(s, 0), 3, "end of foo");
        assert_eq!(word_boundary_right(s, 3), 7, "end of bar");
        assert_eq!(word_boundary_right(s, 7), 10, "end of ...");
        assert_eq!(word_boundary_right(s, 10), 14, "end of baz");
    }

    #[test]
    fn right_treats_emoji_as_word_character() {
        let s = "😀😀 foo";
        assert_eq!(word_boundary_right(s, 0), 8); // both emoji consumed.
    }

    // ---- skip_whitespace_forward / skip_word_class_forward ----

    #[test]
    fn skip_whitespace_then_class_matches_combined_helper() {
        let s = "   foo bar... baz";
        let after_ws = skip_whitespace_forward(s, 0);
        assert_eq!(after_ws, 3);
        assert_eq!(skip_word_class_forward(s, after_ws), 6);
    }

    #[test]
    fn skip_whitespace_at_non_whitespace_is_a_noop() {
        assert_eq!(skip_whitespace_forward("foo bar", 0), 0);
    }

    #[test]
    fn skip_word_class_on_whitespace_is_a_noop() {
        // The contract is "caller already skipped whitespace". When
        // they didn't, the function returns the cursor unchanged
        // rather than treating whitespace as its own class.
        assert_eq!(skip_word_class_forward("   foo", 0), 0);
    }
}
