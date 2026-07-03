//! A shared, pluggable word-motion engine.
//!
//! Word motion moves or deletes the cursor by "word". What counts as a word is
//! decided by a pluggable [`WordClassifier`] that sorts each grapheme into one
//! of three [`CharClass`]es. The engine walks graphemes, not scalars, so a base
//! character keeps its combining marks and a ZWJ sequence stays whole.
//!
//! # Two-phase contract
//!
//! A word jump is two phases. First skip a leading run of
//! [`Separator`](CharClass::Separator). Then skip a maximal run of the landable
//! class of the next unit, stopping as soon as the class changes.
//! [`Separator`](CharClass::Separator) is the only class a jump always skips
//! first and never lands in. [`Punctuation`](CharClass::Punctuation) and
//! [`Word`](CharClass::Word) are landable.
//!
//! The distinction is what makes one engine cover both common word feels. A
//! classifier that never yields [`Punctuation`](CharClass::Punctuation) folds
//! punctuation into the separator skip, so a jump over `foo...bar` treats the
//! dots as part of the gap and lands past `bar`: the readline feel
//! ([`ReadlineWords`]). A classifier that yields all three stops between the
//! dots and `bar`, because the punctuation run is its own landable word: the
//! emacs feel ([`EmacsWords`]).
//!
//! [`word_left`] and [`word_right`] run both phases. The forward phases are also
//! exposed on their own as [`skip_separators`] and [`skip_class`].
//!
//! NOTE: The two forward phases are separate functions, not just an internal
//! detail of [`word_right`], so a caller can splice work between them. A
//! multi-line editor that collapses large pastes into a marker checks for a
//! marker at the position right after the separator skip before deciding how far
//! the class skip should run. Splicing there keeps one copy of each phase
//! instead of forking the whole traversal.

use unicode_segmentation::UnicodeSegmentation;
use vaxis_ucd::GeneralCategory;

/// The class a [`WordClassifier`] assigns to a grapheme.
///
/// [`Separator`](CharClass::Separator) is skipped at the start of every jump and
/// is never a landing class. [`Punctuation`](CharClass::Punctuation) and
/// [`Word`](CharClass::Word) are landable: a jump stops when the class changes
/// between two adjacent landable graphemes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharClass {
    /// The leading run a word jump always skips before it can land.
    Separator,
    /// A landable class. A run of punctuation is its own word.
    Punctuation,
    /// A landable class. The ordinary word constituents.
    Word,
}

/// Sorts a grapheme into a [`CharClass`] for the word-motion engine.
///
/// Implementations receive one grapheme cluster at a time, as produced by
/// grapheme segmentation, so a base character and its combining marks are
/// classified together rather than one scalar at a time.
pub trait WordClassifier {
    /// Classify a single grapheme cluster.
    fn classify(&self, grapheme: &str) -> CharClass;
}

/// Byte offset of the previous word boundary in `text`, scanning backward from
/// `cursor`.
///
/// Returns `0` when the cursor is already at the start of `text`. Runs the
/// backward two phases: skip a run of trailing separators, then skip a run of
/// graphemes in whichever landable class the last remaining grapheme belongs to.
/// The returned offset always lands on a grapheme boundary.
pub fn word_left<C: WordClassifier + ?Sized>(text: &str, cursor: usize, classifier: &C) -> usize {
    if cursor == 0 {
        return 0;
    }
    let before = &text[..cursor];
    let graphemes: Vec<(usize, &str)> = before.grapheme_indices(true).collect();
    let mut idx = graphemes.len();
    let mut new_col = cursor;

    // Phase 1: skip a run of trailing separators.
    while idx > 0 && classifier.classify(graphemes[idx - 1].1) == CharClass::Separator {
        idx -= 1;
        new_col = graphemes[idx].0;
    }

    // Phase 2: skip a maximal run of the class of the last remaining grapheme.
    // Separators were consumed above, so this class is landable.
    if idx > 0 {
        let landing = classifier.classify(graphemes[idx - 1].1);
        while idx > 0 && classifier.classify(graphemes[idx - 1].1) == landing {
            idx -= 1;
            new_col = graphemes[idx].0;
        }
    }
    new_col
}

/// Byte offset of the next word boundary in `text`, scanning forward from
/// `cursor`.
///
/// Equivalent to `skip_class(text, skip_separators(text, cursor, c), c)`. Use
/// the two helpers directly when you need to inspect the position right after
/// the separator skip before continuing the class skip.
pub fn word_right<C: WordClassifier + ?Sized>(text: &str, cursor: usize, classifier: &C) -> usize {
    let after_separators = skip_separators(text, cursor, classifier);
    skip_class(text, after_separators, classifier)
}

/// Byte offset reached by skipping a run of [`Separator`](CharClass::Separator)
/// graphemes forward from `cursor`.
///
/// Returns `cursor` unchanged when it already sits on a landable grapheme or
/// past the end of `text`.
pub fn skip_separators<C: WordClassifier + ?Sized>(
    text: &str,
    cursor: usize,
    classifier: &C,
) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let mut col = cursor;
    for (_, g) in text[cursor..].grapheme_indices(true) {
        if classifier.classify(g) != CharClass::Separator {
            break;
        }
        col += g.len();
    }
    col
}

/// Byte offset reached by skipping a run of graphemes that share the landable
/// class of the grapheme at `cursor`.
///
/// The caller is expected to have already moved past any preceding separators
/// via [`skip_separators`]. If `cursor` itself points at a
/// [`Separator`](CharClass::Separator) this returns `cursor` unchanged rather
/// than treating the separator run as a landable class.
pub fn skip_class<C: WordClassifier + ?Sized>(text: &str, cursor: usize, classifier: &C) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    let after = &text[cursor..];
    let mut graphemes = after.grapheme_indices(true);
    let Some((_, first)) = graphemes.next() else {
        return cursor;
    };
    let landing = classifier.classify(first);
    if landing == CharClass::Separator {
        return cursor;
    }
    let mut col = cursor + first.len();
    for (_, g) in graphemes {
        if classifier.classify(g) != landing {
            break;
        }
        col += g.len();
    }
    col
}

/// Two-class word model with the readline feel: word constituents versus
/// everything else.
///
/// A grapheme is [`Word`](CharClass::Word) when its base scalar is a word
/// constituent by Unicode General Category (a letter, a number, a mark,
/// connector punctuation, or `_`). Whitespace and all punctuation map to
/// [`Separator`](CharClass::Separator), so the engine skips punctuation together
/// with whitespace. This model never yields
/// [`Punctuation`](CharClass::Punctuation), which is what gives it the readline
/// feel: a run of punctuation is a gap between words, not a word of its own.
#[derive(Debug, Default, Clone, Copy)]
pub struct ReadlineWords;

impl WordClassifier for ReadlineWords {
    fn classify(&self, grapheme: &str) -> CharClass {
        // Classify by the base scalar. Combining marks that follow are word
        // constituents too, so the whole cluster's class is the base's.
        match grapheme.chars().next() {
            Some(c) if is_word_codepoint(u32::from(c)) => CharClass::Word,
            _ => CharClass::Separator,
        }
    }
}

/// Three-class word model with the emacs feel: a run of punctuation is its own
/// word.
///
/// Whitespace maps to [`Separator`](CharClass::Separator), the ASCII punctuation
/// bag maps to [`Punctuation`](CharClass::Punctuation), and everything else
/// (letters, digits, non-Latin scripts, emoji, marks) maps to
/// [`Word`](CharClass::Word). Because punctuation is landable, a jump stops on
/// punctuation runs.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmacsWords;

impl WordClassifier for EmacsWords {
    fn classify(&self, grapheme: &str) -> CharClass {
        // Whitespace is checked before punctuation so a cluster carrying both a
        // whitespace and a punctuation scalar is treated as a separator.
        if is_whitespace_grapheme(grapheme) {
            CharClass::Separator
        } else if is_punctuation_grapheme(grapheme) {
            CharClass::Punctuation
        } else {
            CharClass::Word
        }
    }
}

/// Whether `cp` is a readline-style word constituent by Unicode General
/// Category: a letter, a number, a mark, connector punctuation, or `_`.
/// Everything else, including dashes, dots, and path separators, is not.
fn is_word_codepoint(cp: u32) -> bool {
    if cp == u32::from('_') {
        return true;
    }
    matches!(
        vaxis_ucd::general_category(cp),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
            | GeneralCategory::DecimalNumber
            | GeneralCategory::LetterNumber
            | GeneralCategory::OtherNumber
            | GeneralCategory::NonspacingMark
            | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark
            | GeneralCategory::ConnectorPunctuation
    )
}

/// Whether `grapheme` contains any whitespace scalar. Empty input is `false`.
///
/// Returns true if any scalar in the cluster is whitespace. Callers feed
/// grapheme-segmenter output (single-scalar inputs in practice), but the
/// any-scalar rule keeps behavior predictable for a multi-scalar cluster that
/// carries a whitespace component.
fn is_whitespace_grapheme(grapheme: &str) -> bool {
    grapheme.chars().any(char::is_whitespace)
}

/// Whether `grapheme` contains any scalar from the ASCII punctuation bag.
///
/// The set is the classic word-segmentation punctuation bag. Returns true if any
/// scalar in the cluster matches.
fn is_punctuation_grapheme(grapheme: &str) -> bool {
    grapheme.chars().any(is_punctuation_char)
}

/// True iff `c` is one of the ASCII punctuation scalars in the
/// word-segmentation bag. Split out so [`is_punctuation_grapheme`] can run it
/// across every scalar of a multi-scalar cluster.
fn is_punctuation_char(c: char) -> bool {
    matches!(
        c,
        '(' | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | '<'
            | '>'
            | '.'
            | ','
            | ';'
            | ':'
            | '\''
            | '"'
            | '!'
            | '?'
            | '+'
            | '-'
            | '='
            | '*'
            | '/'
            | '\\'
            | '|'
            | '&'
            | '%'
            | '^'
            | '$'
            | '#'
            | '@'
            | '~'
            | '`'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- EmacsWords: three-class, punctuation is its own word ----

    #[test]
    fn emacs_left_at_start_returns_zero() {
        assert_eq!(word_left("foo bar", 0, &EmacsWords), 0);
    }

    #[test]
    fn emacs_left_skips_word_run() {
        // Cursor at end of "foo bar", boundary before "bar".
        assert_eq!(word_left("foo bar", 7, &EmacsWords), 4);
    }

    #[test]
    fn emacs_left_eats_trailing_whitespace_then_word() {
        // Trailing whitespace folds into the same jump as the preceding word.
        assert_eq!(word_left("foo  ", 5, &EmacsWords), 0);
    }

    #[test]
    fn emacs_left_treats_punctuation_run_as_its_own_word() {
        // From end of "foo bar...", the first jump lands before "...".
        assert_eq!(word_left("foo bar...", 10, &EmacsWords), 7);
        // Second jump lands before "bar".
        assert_eq!(word_left("foo bar...", 7, &EmacsWords), 4);
        // Third jump lands at start.
        assert_eq!(word_left("foo bar...", 4, &EmacsWords), 0);
    }

    #[test]
    fn emacs_left_treats_emoji_as_word_character() {
        // U+1F600 is four UTF-8 bytes per occurrence.
        let s = "foo 😀😀";
        assert_eq!(word_left(s, s.len(), &EmacsWords), 4);
    }

    #[test]
    fn emacs_right_at_end_returns_len() {
        assert_eq!(word_right("foo", 3, &EmacsWords), 3);
    }

    #[test]
    fn emacs_right_skips_leading_whitespace_then_word() {
        // From start of "   foo bar", boundary after "   foo".
        assert_eq!(word_right("   foo bar", 0, &EmacsWords), 6);
    }

    #[test]
    fn emacs_right_walks_word_then_punctuation_then_word_runs() {
        let s = "foo bar... baz";
        assert_eq!(word_right(s, 0, &EmacsWords), 3, "end of foo");
        assert_eq!(word_right(s, 3, &EmacsWords), 7, "end of bar");
        assert_eq!(word_right(s, 7, &EmacsWords), 10, "end of ...");
        assert_eq!(word_right(s, 10, &EmacsWords), 14, "end of baz");
    }

    #[test]
    fn emacs_right_treats_emoji_as_word_character() {
        let s = "😀😀 foo";
        assert_eq!(word_right(s, 0, &EmacsWords), 8); // both emoji consumed.
    }

    #[test]
    fn skip_separators_then_class_matches_word_right() {
        let s = "   foo bar... baz";
        let after_separators = skip_separators(s, 0, &EmacsWords);
        assert_eq!(after_separators, 3);
        assert_eq!(skip_class(s, after_separators, &EmacsWords), 6);
    }

    #[test]
    fn skip_separators_at_landable_is_a_noop() {
        assert_eq!(skip_separators("foo bar", 0, &EmacsWords), 0);
    }

    #[test]
    fn skip_class_on_separator_is_a_noop() {
        // The contract is "caller already skipped separators". When they didn't,
        // the function returns the cursor unchanged rather than treating the
        // separator as a landable class.
        assert_eq!(skip_class("   foo", 0, &EmacsWords), 0);
    }

    // ---- ReadlineWords: two-class, punctuation is a separator ----
    //
    // These mirror the landing points of TextField's word-motion tests, phrased
    // as byte offsets from the shared engine.

    #[test]
    fn readline_left_stops_at_word_boundary() {
        // "hello-world": first jump lands before "world", second at the start.
        assert_eq!(word_left("hello-world", 11, &ReadlineWords), 6);
        assert_eq!(word_left("hello-world", 6, &ReadlineWords), 0);
    }

    #[test]
    fn readline_right_stops_at_end_of_word() {
        // "hello-world": stop at end of "hello", then skip "-" and stop at end.
        assert_eq!(word_right("hello-world", 0, &ReadlineWords), 5);
        assert_eq!(word_right("hello-world", 5, &ReadlineWords), 11);
    }

    #[test]
    fn readline_left_with_path_separators() {
        assert_eq!(word_left("/usr/local/bin", 14, &ReadlineWords), 11);
        assert_eq!(word_left("/usr/local/bin", 11, &ReadlineWords), 5);
        assert_eq!(word_left("/usr/local/bin", 5, &ReadlineWords), 1);
        assert_eq!(word_left("/usr/local/bin", 1, &ReadlineWords), 0);
    }

    #[test]
    fn readline_right_with_dots() {
        // "foo.bar.baz": each jump skips a dot and lands at the end of a word.
        assert_eq!(word_right("foo.bar.baz", 0, &ReadlineWords), 3);
        assert_eq!(word_right("foo.bar.baz", 3, &ReadlineWords), 7);
        assert_eq!(word_right("foo.bar.baz", 7, &ReadlineWords), 11);
    }

    #[test]
    fn readline_underscores_are_word_chars() {
        // "hello_world-test": the underscore stays inside the word, the hyphen
        // does not.
        assert_eq!(word_left("hello_world-test", 16, &ReadlineWords), 12);
        assert_eq!(word_left("hello_world-test", 12, &ReadlineWords), 0);
    }

    #[test]
    fn readline_with_non_ascii_text() {
        // "café-latte": é is a word char, the hyphen is a separator.
        let s = "café-latte";
        let hyphen = s.find('-').expect("has a hyphen");
        assert_eq!(word_left(s, s.len(), &ReadlineWords), hyphen + 1);
        assert_eq!(word_left(s, hyphen + 1, &ReadlineWords), 0);
        assert_eq!(word_right(s, 0, &ReadlineWords), hyphen);
    }

    #[test]
    fn readline_non_ascii_punctuation_is_a_separator() {
        // U+2014 EM DASH is dash punctuation, so it is not a word constituent.
        let s = "hello\u{2014}world";
        let dash = s.find('\u{2014}').expect("has an em dash");
        let after_dash = dash + '\u{2014}'.len_utf8();
        assert_eq!(word_left(s, s.len(), &ReadlineWords), after_dash);
        assert_eq!(word_right(s, 0, &ReadlineWords), dash);
    }

    #[test]
    fn readline_with_spaces() {
        assert_eq!(word_left("hello world", 11, &ReadlineWords), 6);
        assert_eq!(word_left("hello world", 6, &ReadlineWords), 0);
    }

    #[test]
    fn classify_boxed_dyn_classifier_works() {
        // The engine accepts an unsized classifier, so a boxed trait object can
        // drive it. A widget storing a pluggable classifier relies on this.
        let classifier: Box<dyn WordClassifier> = Box::new(ReadlineWords);
        assert_eq!(word_left("hello world", 11, classifier.as_ref()), 6);
    }
}
