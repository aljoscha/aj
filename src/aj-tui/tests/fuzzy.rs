//! Tests for the fuzzy matcher and filter.
//!
//! Scoring convention: higher score = better match. `None` means no match.
//! Empty query matches every text with score `0`.

use aj_tui::fuzzy::{fuzzy_filter, fuzzy_match};

// ---------------------------------------------------------------------------
// fuzzy_match
// ---------------------------------------------------------------------------

#[test]
fn empty_query_matches_everything_with_score_zero() {
    assert_eq!(fuzzy_match("", "anything"), Some(0));
}

#[test]
fn query_longer_than_text_does_not_match() {
    assert_eq!(fuzzy_match("longquery", "short"), None);
}

#[test]
fn exact_match_has_positive_score() {
    let score = fuzzy_match("test", "test").expect("exact match should match");
    assert!(score > 0, "expected positive score, got {}", score);
}

#[test]
fn characters_must_appear_in_order() {
    assert!(fuzzy_match("abc", "aXbXc").is_some());
    assert!(fuzzy_match("abc", "cba").is_none());
}

#[test]
fn case_insensitive_matching() {
    assert!(fuzzy_match("ABC", "abc").is_some());
    assert!(fuzzy_match("abc", "ABC").is_some());
}

#[test]
fn consecutive_matches_score_better_than_scattered_matches() {
    let consecutive = fuzzy_match("foo", "foobar").expect("consecutive should match");
    let scattered = fuzzy_match("foo", "f_o_o_bar").expect("scattered should match");
    assert!(
        consecutive > scattered,
        "expected consecutive ({}) > scattered ({})",
        consecutive,
        scattered,
    );
}

#[test]
fn word_boundary_matches_score_better() {
    let at_boundary = fuzzy_match("fb", "foo-bar").expect("boundary should match");
    let not_at_boundary = fuzzy_match("fb", "afbx").expect("non-boundary should match");
    assert!(
        at_boundary > not_at_boundary,
        "expected at_boundary ({}) > not_at_boundary ({})",
        at_boundary,
        not_at_boundary,
    );
}

// ---------------------------------------------------------------------------
// fuzzy_filter
// ---------------------------------------------------------------------------

#[test]
fn empty_query_returns_all_items_unchanged() {
    let items = vec!["apple", "banana", "cherry"];
    let result = fuzzy_filter(items.clone(), "", |s| *s);
    assert_eq!(result, items);
}

#[test]
fn filters_out_non_matching_items() {
    let items = vec!["apple", "banana", "cherry"];
    let result = fuzzy_filter(items, "an", |s| *s);
    assert!(result.contains(&"banana"));
    assert!(!result.contains(&"apple"));
    assert!(!result.contains(&"cherry"));
}

#[test]
fn sorts_results_by_match_quality() {
    let items = vec!["a_p_p", "app", "application"];
    let result = fuzzy_filter(items, "app", |s| *s);

    // "app" is an exact prefix and should rank first. "application" also
    // starts with "app" and ties on raw score; stable tiebreak on input
    // order places "app" ahead since it comes first in the input.
    assert_eq!(result.first().copied(), Some("app"));
}

#[test]
fn prioritizes_exact_matches_over_longer_prefix_matches() {
    // Both "cl" and "clone" match the query "cl", and nucleo on its own
    // assigns them the same score (the matched span is identical). The
    // exact-match bonus in `FuzzyMatcher::score` ensures that a full
    // case-insensitive match beats a longer prefix match. This matters
    // for command palettes / autocomplete where typing the full token
    // of a short item should rank that item first.
    let items = vec!["clone", "cl"];
    let result = fuzzy_filter(items, "cl", |s| *s);
    assert_eq!(result, vec!["cl", "clone"]);
}

#[test]
fn works_with_custom_get_text_function() {
    #[derive(Debug, Clone, PartialEq)]
    struct Item {
        name: String,
        id: u32,
    }

    let items = vec![
        Item {
            name: "foo".to_string(),
            id: 1,
        },
        Item {
            name: "bar".to_string(),
            id: 2,
        },
        Item {
            name: "foobar".to_string(),
            id: 3,
        },
    ];

    let result = fuzzy_filter(items, "foo", |item| &item.name);

    assert_eq!(result.len(), 2);
    let names: Vec<&str> = result.iter().map(|i| i.name.as_str()).collect();
    assert!(names.contains(&"foo"));
    assert!(names.contains(&"foobar"));
}
