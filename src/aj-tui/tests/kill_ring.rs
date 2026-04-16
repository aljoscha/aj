//! Tests for the kill ring.
//!
//! The ring backs cut-and-yank editing in the text input and editor
//! components: `push` records a killed region, `peek` reads the most
//! recent entry, and `rotate` cycles so yank-pop style commands can
//! walk through history.

use aj_tui::kill_ring::KillRing;

#[test]
fn push_fresh_adds_a_new_entry() {
    let mut ring = KillRing::new();
    ring.push("first", false, false);
    ring.push("second", false, false);

    assert_eq!(ring.len(), 2);
    assert_eq!(ring.peek(), Some("second"));
}

#[test]
fn push_with_accumulate_appends_to_the_tail_entry() {
    let mut ring = KillRing::new();
    ring.push("hello", false, false);
    ring.push(" world", false, true);

    assert_eq!(ring.len(), 1);
    assert_eq!(ring.peek(), Some("hello world"));
}

#[test]
fn push_with_prepend_accumulates_in_reverse_order() {
    let mut ring = KillRing::new();
    ring.push("world", false, false);
    ring.push("hello ", true, true);

    assert_eq!(ring.len(), 1);
    assert_eq!(ring.peek(), Some("hello world"));
}

#[test]
fn accumulate_onto_an_empty_ring_creates_a_fresh_entry() {
    let mut ring = KillRing::new();
    ring.push("solo", false, true);

    assert_eq!(ring.len(), 1);
    assert_eq!(ring.peek(), Some("solo"));
}

#[test]
fn empty_text_is_ignored_across_all_modes() {
    let mut ring = KillRing::new();
    ring.push("", false, false);
    ring.push("", true, true);
    assert!(ring.is_empty());

    ring.push("real", false, false);
    ring.push("", false, true);
    ring.push("", true, true);
    assert_eq!(ring.len(), 1);
    assert_eq!(ring.peek(), Some("real"));
}

#[test]
fn rotate_walks_backwards_through_entries_and_wraps() {
    let mut ring = KillRing::new();
    for entry in ["a", "b", "c"] {
        ring.push(entry, false, false);
    }

    assert_eq!(ring.peek(), Some("c"));
    ring.rotate();
    assert_eq!(ring.peek(), Some("b"));
    ring.rotate();
    assert_eq!(ring.peek(), Some("a"));
    ring.rotate();
    assert_eq!(ring.peek(), Some("c"), "wraps back to newest");
}

#[test]
fn rotate_with_fewer_than_two_entries_is_a_noop() {
    let mut ring = KillRing::new();
    ring.rotate();
    assert!(ring.is_empty());

    ring.push("only", false, false);
    ring.rotate();
    assert_eq!(ring.peek(), Some("only"));
}

#[test]
fn alternating_backward_and_forward_kills_accumulate_on_the_same_entry() {
    // Backward kill of " world" onto "hello" gives "hello" + " world"
    // when appended, but real usage prepends because the deletion came
    // from before the cursor. Verify both directions compose.
    let mut ring = KillRing::new();
    ring.push("world", false, false);
    ring.push("hello ", true, true); // backward → prepend
    ring.push("!", false, true); // forward → append
    assert_eq!(ring.peek(), Some("hello world!"));
}
