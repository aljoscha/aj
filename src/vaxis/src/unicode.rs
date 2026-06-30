//! Grapheme-cluster iteration over UTF-8.
//!
//! Mirrors upstream's `Grapheme { start, len }` value and its `bytes(str)`
//! accessor, backed by `unicode-segmentation`'s UAX#29 extended grapheme
//! clusters. The print engine in `window` indexes back into the source string
//! by these byte offsets, so we keep the offset shape rather than owning the
//! cluster bytes.

use unicode_segmentation::{GraphemeIndices, UnicodeSegmentation};

/// A grapheme cluster located by byte offset into its source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grapheme {
    pub start: usize,
    pub len: usize,
}

impl Grapheme {
    /// Returns the cluster's bytes, borrowing from `s`.
    ///
    /// `s` must be the same string the grapheme was produced from, otherwise
    /// the byte range is meaningless.
    pub fn bytes<'a>(&self, s: &'a str) -> &'a str {
        &s[self.start..self.start + self.len]
    }
}

/// Iterator over the extended grapheme clusters of a string, yielding
/// [`Grapheme`] offsets in order.
pub struct GraphemeIterator<'a> {
    inner: GraphemeIndices<'a>,
}

impl Iterator for GraphemeIterator<'_> {
    type Item = Grapheme;

    fn next(&mut self) -> Option<Grapheme> {
        self.inner.next().map(|(start, cluster)| Grapheme {
            start,
            len: cluster.len(),
        })
    }
}

/// Creates a grapheme iterator over `s`.
///
/// Uses extended grapheme clusters, so ZWJ emoji sequences, regional-indicator
/// flags, and base-plus-combining sequences each count as one grapheme.
pub fn grapheme_iterator(s: &str) -> GraphemeIterator<'_> {
    GraphemeIterator {
        inner: s.grapheme_indices(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_splits_per_char() {
        let s = "abc";
        let gs: Vec<Grapheme> = grapheme_iterator(s).collect();
        assert_eq!(gs.len(), 3);
        assert_eq!(gs[0], Grapheme { start: 0, len: 1 });
        assert_eq!(gs[1], Grapheme { start: 1, len: 1 });
        assert_eq!(gs[2], Grapheme { start: 2, len: 1 });
        assert_eq!(gs[0].bytes(s), "a");
        assert_eq!(gs[2].bytes(s), "c");
    }

    #[test]
    fn astronaut_zwj_is_one_cluster() {
        // WOMAN + ZWJ + ROCKET renders as a single grapheme.
        let s = "👩\u{200D}🚀";
        let gs: Vec<Grapheme> = grapheme_iterator(s).collect();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].bytes(s), s);
    }

    #[test]
    fn regional_indicator_flag_is_one_cluster() {
        // U+1F1FA U+1F1F8 (regional indicators U S) form the US flag.
        let s = "🇺🇸";
        let gs: Vec<Grapheme> = grapheme_iterator(s).collect();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].bytes(s), s);
    }

    #[test]
    fn base_plus_combining_is_one_cluster() {
        // 'a' + COMBINING ACUTE ACCENT (NFD form of 'á').
        let s = "a\u{0301}";
        let gs: Vec<Grapheme> = grapheme_iterator(s).collect();
        assert_eq!(gs.len(), 1);
        assert_eq!(gs[0].bytes(s), s);
    }
}
