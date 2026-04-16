//! Content-preserving word-wrap for single-line text.
//!
//! Unlike [`crate::ansi::wrap_text_with_ansi`], which is output-oriented
//! and trims whitespace at wrap boundaries, this wrapper preserves the
//! original byte content of the input: each returned [`TextChunk`]
//! carries the exact substring plus its byte offsets into the original
//! line. That makes it usable for cursor-position math in a text editor,
//! where a visible break must not silently reshape the backing string.
//!
//! The algorithm walks graphemes (or caller-supplied atomic segments)
//! left-to-right, counting visible columns and remembering the last
//! whitespace→non-whitespace transition as a "wrap opportunity". On
//! overflow it prefers to break at the last wrap opportunity; failing
//! that (a single atom already exceeds the width) it force-breaks at
//! the current atom. Atoms wider than the target width — e.g. a paste
//! marker in a narrow terminal — recursively split at grapheme
//! granularity but remain atomic for cursor and editing purposes.
//!
//! # API
//!
//! - [`word_wrap_line`] uses default grapheme segmentation.
//! - [`word_wrap_line_with_segments`] lets the caller pre-segment the
//!   input so that certain runs are treated as atomic units even though
//!   they consist of multiple graphemes (used for paste markers).

use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::{is_whitespace_grapheme, visible_width};

/// One chunk produced by the wrapper. `start_index` and `end_index`
/// are byte offsets into the original line such that
/// `line[chunk.start_index..chunk.end_index] == chunk.text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start_index: usize,
    pub end_index: usize,
}

/// One atomic segment passed to [`word_wrap_line_with_segments`]. The
/// wrapper treats each entry as an indivisible unit at the layout level
/// (a paste marker stays whole unless it's wider than `max_width`, in
/// which case it splits at grapheme granularity).
#[derive(Debug, Clone, Copy)]
pub struct TextSegment<'a> {
    pub text: &'a str,
    pub start_index: usize,
}

impl TextChunk {
    fn empty() -> Self {
        Self {
            text: String::new(),
            start_index: 0,
            end_index: 0,
        }
    }

    fn from_slice(line: &str, start: usize, end: usize) -> Self {
        Self {
            text: line[start..end].to_string(),
            start_index: start,
            end_index: end,
        }
    }
}

/// Split `line` into chunks whose visible widths are each `<= max_width`,
/// using the default grapheme segmentation.
///
/// Whitespace at wrap boundaries is preserved on whichever side the
/// algorithm placed it — trailing space stays on the outgoing chunk if
/// the break happens *after* the whitespace, leading space stays on the
/// incoming chunk if the break was forced inside a non-whitespace run.
/// Concatenating every `chunk.text` yields `line` exactly.
///
/// Empty input or a non-positive `max_width` returns a single empty
/// chunk. Input that already fits returns a single chunk covering the
/// whole line.
pub fn word_wrap_line(line: &str, max_width: usize) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk::empty()];
    }
    if visible_width(line) <= max_width {
        return vec![TextChunk::from_slice(line, 0, line.len())];
    }

    let segments: Vec<(usize, &str)> = line.grapheme_indices(true).collect();
    wrap_segments(line, max_width, &segments)
}

/// Like [`word_wrap_line`] but with caller-supplied atomic segments.
///
/// Each entry in `segments` is treated as an indivisible unit; the
/// wrapper will not break between graphemes inside a single segment
/// (except via the oversized-segment recursive path for segments wider
/// than `max_width`).
pub fn word_wrap_line_with_segments(
    line: &str,
    max_width: usize,
    segments: &[TextSegment<'_>],
) -> Vec<TextChunk> {
    if line.is_empty() || max_width == 0 {
        return vec![TextChunk::empty()];
    }
    if visible_width(line) <= max_width {
        return vec![TextChunk::from_slice(line, 0, line.len())];
    }

    let normalized: Vec<(usize, &str)> = segments.iter().map(|s| (s.start_index, s.text)).collect();
    wrap_segments(line, max_width, &normalized)
}

fn wrap_segments(line: &str, max_width: usize, segments: &[(usize, &str)]) -> Vec<TextChunk> {
    let mut chunks: Vec<TextChunk> = Vec::new();
    let mut current_width: usize = 0;
    let mut chunk_start: usize = 0;

    // Position (byte offset) and running width at the last whitespace →
    // non-whitespace transition. This is where a line break is allowed.
    let mut wrap_opp_index: Option<usize> = None;
    let mut wrap_opp_width: usize = 0;

    for i in 0..segments.len() {
        let (char_index, grapheme) = segments[i];
        let g_width = visible_width(grapheme);
        let is_ws = is_whitespace_grapheme(grapheme);

        // Overflow check *before* advancing current_width.
        if current_width + g_width > max_width {
            if let Some(opp) = wrap_opp_index
                && current_width + g_width - wrap_opp_width <= max_width
            {
                // The trailing run since the last wrap opportunity, plus
                // this grapheme, still fits — backtrack to the opportunity.
                chunks.push(TextChunk::from_slice(line, chunk_start, opp));
                chunk_start = opp;
                current_width -= wrap_opp_width;
            } else if chunk_start < char_index {
                // Force-break right here: either no prior opportunity, or
                // backtracking wouldn't have fit this grapheme either
                // (e.g. a wide grapheme about to overflow).
                chunks.push(TextChunk::from_slice(line, chunk_start, char_index));
                chunk_start = char_index;
                current_width = 0;
            }
            wrap_opp_index = None;
        }

        // Oversized single segment (e.g. a paste marker). Split it
        // recursively at grapheme granularity; the last piece becomes
        // the leading edge of the next chunk. The segment remains
        // logically atomic for cursor movement — this split is purely
        // visual.
        if g_width > max_width {
            let sub = word_wrap_line(grapheme, max_width);
            for s in sub.iter().take(sub.len().saturating_sub(1)) {
                chunks.push(TextChunk {
                    text: s.text.clone(),
                    start_index: char_index + s.start_index,
                    end_index: char_index + s.end_index,
                });
            }
            let last = sub
                .last()
                .expect("word_wrap_line returns at least one chunk");
            chunk_start = char_index + last.start_index;
            current_width = visible_width(&last.text);
            wrap_opp_index = None;
            continue;
        }

        current_width += g_width;

        // Record a wrap opportunity at a whitespace → non-whitespace
        // transition. Multiple whitespace graphemes are joined, with the
        // break point landing after the last one before the next word.
        if is_ws
            && let Some((next_idx, next_gr)) = segments.get(i + 1).copied()
            && !is_whitespace_grapheme(next_gr)
        {
            wrap_opp_index = Some(next_idx);
            wrap_opp_width = current_width;
        }
    }

    chunks.push(TextChunk::from_slice(line, chunk_start, line.len()));
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_one_empty_chunk() {
        let chunks = word_wrap_line("", 10);
        assert_eq!(chunks, vec![TextChunk::empty()]);
    }

    #[test]
    fn short_input_returns_one_chunk_spanning_the_whole_line() {
        let chunks = word_wrap_line("hi", 10);
        assert_eq!(
            chunks,
            vec![TextChunk {
                text: "hi".into(),
                start_index: 0,
                end_index: 2,
            }]
        );
    }

    #[test]
    fn reconstructs_the_original_line_exactly() {
        let line = "some 长 text with 几个 graphemes 😀😀";
        let chunks = word_wrap_line(line, 8);
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined, line);
        // Offsets line up with text.
        for c in &chunks {
            assert_eq!(&line[c.start_index..c.end_index], c.text);
        }
    }
}
