//! Integration tests for `aj_tui::word_wrap::word_wrap_line` and
//! `word_wrap_line_with_segments`.
//!
//! Covers the pure-function contract in 16 cases total (11 using default
//! segmentation, 5 passing pre-segmented atomic units). The
//! content-preserving invariant (every chunk's `text` equals the
//! corresponding slice of the input) is exercised implicitly by
//! comparing `chunk.text` plus, in several cases, reconstructing the
//! line from `(start_index, end_index)`.
//!
//! Test-suite partitioning note: `tests/editor_word_wrap.rs` exercises
//! word wrapping *through the Editor component's render path*, and this
//! file tests the underlying pure function directly. The split is
//! intentional because the pure function is also its own public surface
//! (`aj_tui::word_wrap`) and deserves its own regression file; the two
//! files together cover the full matrix.

use aj_tui::ansi::visible_width;
use aj_tui::word_wrap::{TextChunk, TextSegment, word_wrap_line, word_wrap_line_with_segments};

fn text(chunks: &[TextChunk]) -> Vec<String> {
    chunks.iter().map(|c| c.text.clone()).collect()
}

fn reconstruct(line: &str, chunks: &[TextChunk]) -> String {
    chunks
        .iter()
        .map(|c| &line[c.start_index..c.end_index])
        .collect()
}

fn assert_all_fit(chunks: &[TextChunk], max_width: usize) {
    for c in chunks {
        let w = visible_width(&c.text);
        assert!(
            w <= max_width,
            "chunk overflowed: width={w}, max={max_width}, text={:?}",
            c.text,
        );
    }
}

// ---------------------------------------------------------------------------
// Single-word wrap boundary (non-ws char ending at width)
// ---------------------------------------------------------------------------

#[test]
fn wraps_word_to_next_line_when_it_ends_exactly_at_terminal_width() {
    // "hello " (6) + "world" (5) = 11 exactly, but since "world" is
    // non-whitespace ending AT the width, the break happens after the
    // space: "hello " stays on line 1, "world test" goes to line 2.
    let chunks = word_wrap_line("hello world test", 11);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "hello ");
    assert_eq!(chunks[1].text, "world test");
}

#[test]
fn keeps_whitespace_at_terminal_width_boundary_on_same_line() {
    // "hello world " is 12 chars including trailing space; the space
    // stays on the first line, "test" goes to the second.
    let chunks = word_wrap_line("hello world test", 12);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "hello world ");
    assert_eq!(chunks[1].text, "test");
}

// ---------------------------------------------------------------------------
// Unbreakable runs
// ---------------------------------------------------------------------------

#[test]
fn force_breaks_an_unbreakable_word_of_width_followed_by_space() {
    let chunks = word_wrap_line("aaaaaaaaaaaa aaaa", 12);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "aaaaaaaaaaaa");
    assert_eq!(chunks[1].text, " aaaa");
}

#[test]
fn wraps_to_next_line_when_word_fits_width_but_not_remaining_space() {
    let chunks = word_wrap_line("      aaaaaaaaaaaa", 12);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "      ");
    assert_eq!(chunks[1].text, "aaaaaaaaaaaa");
}

// ---------------------------------------------------------------------------
// Multi-space interior runs
// ---------------------------------------------------------------------------

#[test]
fn keeps_multi_space_plus_word_together_when_they_fit() {
    let chunks = word_wrap_line("Lorem ipsum dolor sit amet,    consectetur", 30);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,    consectetur");
}

#[test]
fn keeps_multi_space_plus_word_together_when_they_fill_width_exactly() {
    let chunks = word_wrap_line("Lorem ipsum dolor sit amet,              consectetur", 30);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,              consectetur");
}

#[test]
fn splits_when_word_plus_multi_space_plus_word_exceeds_width() {
    let chunks = word_wrap_line("Lorem ipsum dolor sit amet,               consectetur", 30);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,               ");
    assert_eq!(chunks[2].text, "consectetur");
}

#[test]
fn breaks_long_whitespace_at_line_boundary() {
    let chunks = word_wrap_line(
        "Lorem ipsum dolor sit amet,                         consectetur",
        30,
    );
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,                         ");
    assert_eq!(chunks[2].text, "consectetur");
}

#[test]
fn breaks_long_whitespace_at_line_boundary_second_variant() {
    let chunks = word_wrap_line(
        "Lorem ipsum dolor sit amet,                          consectetur",
        30,
    );
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,                         ");
    assert_eq!(chunks[2].text, " consectetur");
}

#[test]
fn breaks_whitespace_spanning_full_lines() {
    let chunks = word_wrap_line(
        "Lorem ipsum dolor sit amet,                                     consectetur",
        30,
    );
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].text, "Lorem ipsum dolor sit ");
    assert_eq!(chunks[1].text, "amet,                         ");
    assert_eq!(chunks[2].text, "            consectetur");
}

// ---------------------------------------------------------------------------
// Wide graphemes at the wrap boundary
// ---------------------------------------------------------------------------

#[test]
fn force_breaks_before_wide_char_when_backtrack_would_not_help() {
    // " " (1) + "a"*186 (186) + "你" (2) = 189 visible width. With
    // max_width = 187 the backtrack candidate would still leave 186 + 2
    // = 188 > 187, so the algorithm must force-break right before the
    // wide char instead.
    let line = format!(" {}你", "a".repeat(186));
    let chunks = word_wrap_line(&line, 187);

    for chunk in &chunks {
        let w = visible_width(&chunk.text);
        assert!(
            w <= 187,
            "chunk overflowed: width={w} text={:?}",
            chunk.text
        );
    }

    // No content is lost.
    assert_eq!(reconstruct(&line, &chunks), line, "offsets lost content");
    let joined_text: String = text(&chunks).concat();
    assert_eq!(joined_text, line, "chunk text lost content");
}

// ---------------------------------------------------------------------------
// Pre-segmented atomic units (paste markers)
//
// A "[paste #N +K lines]" style marker is logically one editing unit
// even though it consists of many graphemes. Callers pre-segment the
// input so that the wrapper treats the marker as atomic; the wrapper
// only splits inside it as a last resort (when the marker alone
// exceeds max_width).
// ---------------------------------------------------------------------------

const MARKER_1: &str = "[paste #1 +20 lines]";
const MARKER_2: &str = "[paste #2 +30 lines]";

#[test]
fn splits_oversized_atomic_segment_across_multiple_chunks() {
    let line = format!("A{MARKER_1}B");
    let segments = [
        TextSegment {
            text: "A",
            start_index: 0,
        },
        TextSegment {
            text: MARKER_1,
            start_index: 1,
        },
        TextSegment {
            text: "B",
            start_index: 1 + MARKER_1.len(),
        },
    ];

    let chunks = word_wrap_line_with_segments(&line, 10, &segments);
    assert_all_fit(&chunks, 10);
    assert_eq!(reconstruct(&line, &chunks), line);
}

#[test]
fn splits_oversized_atomic_segment_at_start_of_line() {
    let line = format!("{MARKER_1}B");
    let segments = [
        TextSegment {
            text: MARKER_1,
            start_index: 0,
        },
        TextSegment {
            text: "B",
            start_index: MARKER_1.len(),
        },
    ];

    let chunks = word_wrap_line_with_segments(&line, 10, &segments);
    assert_all_fit(&chunks, 10);
    assert_eq!(reconstruct(&line, &chunks), line);

    // "B" ends up on the last chunk — either alone or trailing the
    // marker's last grapheme row.
    assert!(chunks.last().unwrap().text.contains('B'));
}

#[test]
fn splits_oversized_atomic_segment_at_end_of_line() {
    let line = format!("A{MARKER_1}");
    let segments = [
        TextSegment {
            text: "A",
            start_index: 0,
        },
        TextSegment {
            text: MARKER_1,
            start_index: 1,
        },
    ];

    let chunks = word_wrap_line_with_segments(&line, 10, &segments);
    assert_all_fit(&chunks, 10);
    // "A" is small enough to stand alone on the first chunk.
    assert_eq!(chunks[0].text, "A");
    assert_eq!(reconstruct(&line, &chunks), line);
}

#[test]
fn splits_consecutive_oversized_atomic_segments() {
    let line = format!("{MARKER_1}{MARKER_2}");
    let segments = [
        TextSegment {
            text: MARKER_1,
            start_index: 0,
        },
        TextSegment {
            text: MARKER_2,
            start_index: MARKER_1.len(),
        },
    ];

    let chunks = word_wrap_line_with_segments(&line, 10, &segments);
    assert_all_fit(&chunks, 10);
    assert_eq!(reconstruct(&line, &chunks), line);
}

#[test]
fn wraps_normally_after_oversized_atomic_segment() {
    let line = format!("{MARKER_1} hello world");

    let mut segments = vec![
        TextSegment {
            text: MARKER_1,
            start_index: 0,
        },
        TextSegment {
            text: " ",
            start_index: MARKER_1.len(),
        },
    ];
    // Graphemes of "hello world" start just after the initial space.
    let graphemes: [&str; 11] = ["h", "e", "l", "l", "o", " ", "w", "o", "r", "l", "d"];
    for (i, g) in graphemes.iter().enumerate() {
        segments.push(TextSegment {
            text: g,
            start_index: MARKER_1.len() + 1 + i,
        });
    }

    let chunks = word_wrap_line_with_segments(&line, 10, &segments);
    assert_all_fit(&chunks, 10);
    // Normal word-wrap resumes after the marker: "world" ends up on
    // the final chunk.
    assert_eq!(
        chunks.last().map(|c| c.text.as_str()),
        Some("world"),
        "unexpected final chunk: {chunks:?}",
    );
    assert_eq!(reconstruct(&line, &chunks), line);
}
