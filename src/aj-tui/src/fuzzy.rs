//! Fuzzy matching utilities used by list filtering and autocomplete ranking.
//!
//! A thin wrapper over [`nucleo_matcher`] that exposes:
//!
//! - [`FuzzyMatcher`], a reusable matcher. Construct once and call
//!   [`FuzzyMatcher::score`] / [`FuzzyMatcher::filter`] repeatedly.
//! - [`fuzzy_match`] and [`fuzzy_filter`], free-function conveniences backed
//!   by a thread-local [`FuzzyMatcher`]. Use these for ad-hoc calls; prefer
//!   an owned matcher in tight loops so internal buffers are not cleared
//!   and refilled on every call.
//!
//! Scoring convention: higher score is better. `None` means no match. An
//! empty query matches every haystack with score `0`.
//!
//! # Example
//!
//! ```
//! use aj_tui::fuzzy::fuzzy_filter;
//!
//! let items = ["apple", "banana", "cherry"];
//! let matched = fuzzy_filter(items, "an", |s| s);
//! assert_eq!(matched, vec!["banana"]);
//! ```

use std::cell::RefCell;

use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Bonus added to the raw nucleo score when query and text are equal
/// (case-insensitive, ASCII). Ensures that an item whose text matches
/// the query exactly outranks an item where the query is only a
/// prefix/substring of a longer text — nucleo on its own scores both
/// identically because the matched span is the same. 100 is large
/// enough to flip ordering for any realistic gap-vs-bonus difference
/// in nucleo's scoring (single-digit deltas in practice).
const EXACT_MATCH_BONUS: u16 = 100;

/// A reusable fuzzy matcher. Holds internal buffers that are cleared and
/// refilled on each call, amortizing allocation across many matches.
///
/// For a one-off match, use [`fuzzy_match`] or [`fuzzy_filter`].
pub struct FuzzyMatcher {
    inner: Matcher,
    haystack_buf: Vec<char>,
    needle_buf: Vec<char>,
}

impl FuzzyMatcher {
    /// Create a new matcher with the default configuration.
    pub fn new() -> Self {
        Self {
            inner: Matcher::new(Config::DEFAULT),
            haystack_buf: Vec::new(),
            needle_buf: Vec::new(),
        }
    }

    /// Score a match of `query` against `text`. Higher is better.
    ///
    /// Returns `None` if `query` cannot be matched as a subsequence of
    /// `text` (case-insensitive). An empty `query` yields `Some(0)`.
    ///
    /// Adds an exact-match bonus on top of nucleo's score: if `query` and
    /// `text` are equal under case-insensitive (ASCII) comparison, the
    /// score is bumped by `EXACT_MATCH_BONUS`. nucleo on its own does not
    /// distinguish `"cl"` matched against `"cl"` from `"cl"` matched
    /// against `"clone"` — both score identically because the matched
    /// span is the same. For interactive list filtering and autocomplete,
    /// users expect an exact-text match to beat a longer-text partial
    /// match, so the bonus gives exact full matches a substantial boost.
    /// Saturating arithmetic guards against overflow on already-near-
    /// `u16::MAX` scores.
    pub fn score(&mut self, query: &str, text: &str) -> Option<u16> {
        self.haystack_buf.clear();
        self.needle_buf.clear();
        let haystack = Utf32Str::new(text, &mut self.haystack_buf);
        let needle = Utf32Str::new(query, &mut self.needle_buf);
        let base = self.inner.fuzzy_match(haystack, needle)?;
        let bonus = if query.eq_ignore_ascii_case(text) {
            EXACT_MATCH_BONUS
        } else {
            0
        };
        Some(base.saturating_add(bonus))
    }

    /// Score `query` against a set of fields belonging to one item,
    /// best first. Returns `None` if the item does not match.
    ///
    /// `query` is split on whitespace into tokens; each token must
    /// match at least one field (case-insensitive subsequence) or the
    /// whole item is rejected. A token's contribution is its best score
    /// across the fields, and the returned score is the sum of the
    /// per-token contributions.
    ///
    /// Matching each token *within a single field* — rather than
    /// against a concatenation of every field — keeps a token from
    /// being satisfied by characters that straddle two fields. For
    /// example the query `gpt-5.5` will not match an item with id
    /// `gpt-5.1` and name `GPT-5.1` by borrowing the trailing `5` from
    /// the name; against the concatenated `"gpt-5.1 GPT-5.1"` it
    /// otherwise would.
    ///
    /// An empty (or whitespace-only) `query` yields `Some(0)`.
    pub fn score_fields(&mut self, query: &str, fields: &[&str]) -> Option<u32> {
        let tokens: Vec<&str> = query.split_whitespace().collect();
        if tokens.is_empty() {
            return Some(0);
        }
        let mut total: u32 = 0;
        for token in tokens {
            let best = fields.iter().filter_map(|f| self.score(token, f)).max()?;
            total = total.saturating_add(u32::from(best));
        }
        Some(total)
    }

    /// Filter and sort `items` by match quality, best first.
    ///
    /// `query` is split on whitespace into tokens; every token must match
    /// the item's text (via `get_text`) or the item is dropped. The final
    /// score is the sum of per-token scores; ties preserve the original
    /// input order.
    ///
    /// An empty (or whitespace-only) `query` returns `items` unchanged.
    pub fn filter<T, I, F>(&mut self, items: I, query: &str, get_text: F) -> Vec<T>
    where
        I: IntoIterator<Item = T>,
        F: Fn(&T) -> &str,
    {
        let query = query.trim();
        if query.is_empty() {
            return items.into_iter().collect();
        }

        let tokens: Vec<&str> = query.split_whitespace().collect();
        if tokens.is_empty() {
            return items.into_iter().collect();
        }

        let mut scored: Vec<(T, u32, usize)> = Vec::new();
        for (idx, item) in items.into_iter().enumerate() {
            let text = get_text(&item);
            let mut total: u32 = 0;
            let mut all_match = true;
            for token in &tokens {
                match self.score(token, text) {
                    Some(s) => total = total.saturating_add(u32::from(s)),
                    None => {
                        all_match = false;
                        break;
                    }
                }
            }
            if all_match {
                scored.push((item, total, idx));
            }
        }

        // Highest score first; stable tiebreak via original index so that
        // equally-matched items preserve their input order.
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
        scored.into_iter().map(|(item, _, _)| item).collect()
    }
}

impl Default for FuzzyMatcher {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    /// Thread-local matcher for the free-function API. Constructed on first
    /// use; `nucleo_matcher::Matcher` allocates ~135KB up front, so we reuse
    /// a single instance per thread rather than building one per call.
    static SHARED: RefCell<FuzzyMatcher> = RefCell::new(FuzzyMatcher::new());
}

/// Score a fuzzy match using a thread-local [`FuzzyMatcher`].
///
/// See [`FuzzyMatcher::score`] for semantics. For tight loops, prefer
/// constructing a [`FuzzyMatcher`] explicitly.
pub fn fuzzy_match(query: &str, text: &str) -> Option<u16> {
    SHARED.with(|m| m.borrow_mut().score(query, text))
}

/// Filter and sort items by match quality using a thread-local
/// [`FuzzyMatcher`].
///
/// See [`FuzzyMatcher::filter`] for semantics.
pub fn fuzzy_filter<T, I, F>(items: I, query: &str, get_text: F) -> Vec<T>
where
    I: IntoIterator<Item = T>,
    F: Fn(&T) -> &str,
{
    SHARED.with(|m| m.borrow_mut().filter(items, query, get_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_fields_rejects_token_spanning_two_fields() {
        let mut m = FuzzyMatcher::new();
        // The `5.5` query has two `5`s. Against a `gpt-5.1` id and
        // `GPT-5.1` name it must NOT match by borrowing the second `5`
        // from the name — each token is scored within a single field.
        assert!(
            m.score_fields("gpt-5.5", &["openai", "gpt-5.1", "GPT-5.1"])
                .is_none()
        );
        // The genuine 5.5 entry still matches.
        assert!(
            m.score_fields("gpt-5.5", &["openai", "gpt-5.5", "GPT-5.5"])
                .is_some()
        );
    }

    #[test]
    fn score_fields_matches_tokens_across_distinct_fields() {
        let mut m = FuzzyMatcher::new();
        // Multi-word query: each token may match a different field.
        assert!(
            m.score_fields("openai 5.5", &["openai", "gpt-5.5", "GPT-5.5"])
                .is_some()
        );
        // A token that matches no field rejects the whole item.
        assert!(
            m.score_fields("openai claude", &["openai", "gpt-5.5", "GPT-5.5"])
                .is_none()
        );
    }

    #[test]
    fn score_fields_empty_query_matches_with_zero() {
        let mut m = FuzzyMatcher::new();
        assert_eq!(m.score_fields("", &["anything"]), Some(0));
        assert_eq!(m.score_fields("   ", &["anything"]), Some(0));
    }
}
