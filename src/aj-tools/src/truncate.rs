//! Tool-output truncation primitives.
//!
//! Every long tool output gets clipped against two simultaneous budgets
//! — a line count and a byte count — and whichever fires first wins.
//! The resulting [`TruncationResult`] carries enough information for
//! both the wire footer the model sees and the structured payload the
//! UI renders.
//!
//! - [`truncate_head`] keeps the first N lines / bytes, suitable for
//!   file reads where the beginning carries more signal.
//! - [`truncate_tail`] keeps the last N lines / bytes, suitable for
//!   shell commands where errors and final results live at the end.
//! - Single-line overflows are surfaced through
//!   [`TruncationResult::first_line_exceeds_limit`] (head) and
//!   [`TruncationResult::last_line_partial`] (tail) so callers can
//!   emit an actionable escape message instead of leaking a partial
//!   line.

/// Default line cap for `read_file`.
pub const READ_MAX_LINES: usize = 2_000;
/// Default byte cap for `read_file`.
pub const READ_MAX_BYTES: usize = 50 * 1024;

/// Default line cap for `bash`.
pub const BASH_MAX_LINES: usize = 2_000;
/// Default byte cap for `bash`.
pub const BASH_MAX_BYTES: usize = 50 * 1024;

/// Short local alias for the canonical truncation-cause enum that
/// lives in `aj-agent` (where it's part of the persisted `ToolDetails`
/// schema). Callers inside `aj-tools` use this name; consumers outside
/// are expected to import [`aj_agent::tool::TruncationCause`] directly.
pub use aj_agent::tool::TruncationCause as TruncatedBy;

/// Outcome of a single truncation pass.
///
/// `total_*` describe the source the caller handed in; `output_*`
/// describe the kept content (`content`). When `truncated` is `false`
/// both pairs are equal and the rest of the flags are at their default
/// "nothing special happened" values.
#[derive(Clone, Debug)]
pub struct TruncationResult {
    /// Kept content (possibly empty in the `first_line_exceeds_limit`
    /// or zero-line-cap cases).
    pub content: String,
    /// Whether any source content was dropped.
    pub truncated: bool,
    /// Which budget triggered the truncation. `None` iff `!truncated`.
    /// When both budgets are reached at once (the kept content hits the
    /// line cap while its byte total is still within the byte cap),
    /// `Lines` wins.
    pub truncated_by: Option<TruncatedBy>,
    /// Line count of the source content. Split on `\n` and drop a
    /// single trailing empty element introduced by a terminating
    /// newline (so `"a\n"` counts as one line, not two).
    pub total_lines: usize,
    /// Byte length of the source content.
    pub total_bytes: usize,
    /// Line count of the kept content (same rule as `total_lines`).
    pub output_lines: usize,
    /// Byte length of the kept content.
    pub output_bytes: usize,
    /// Tail-only: the kept content begins with a partial line because
    /// the source's trailing line was larger than the byte budget.
    pub last_line_partial: bool,
    /// Head-only: the source's first line alone exceeded the byte
    /// budget. When set, `content` is empty and the caller is expected
    /// to surface an actionable escape (e.g. point the model at a
    /// `sed`/`head -c` fallback).
    pub first_line_exceeds_limit: bool,
    /// The line cap that was applied. Echoed back for callers that
    /// build messages mentioning the limit.
    pub max_lines: usize,
    /// The byte cap that was applied.
    pub max_bytes: usize,
}

/// Human-readable byte size.
///
/// - `< 1KiB`: `"<N>B"` (integer)
/// - `< 1MiB`: `"<X.Y>KB"` (1 decimal)
/// - `≥ 1MiB`: `"<X.Y>MB"` (1 decimal)
///
/// Sizes large enough to lose precision in the `usize -> f64` cast
/// (>2^53 bytes) are well past anything any tool would render.
#[allow(clippy::as_conversions)]
pub fn format_size(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * 1024;
    if bytes < KIB {
        format!("{bytes}B")
    } else if bytes < MIB {
        format!("{:.1}KB", bytes as f64 / KIB as f64)
    } else {
        format!("{:.1}MB", bytes as f64 / MIB as f64)
    }
}

/// Split a string into lines for counting: any string that ends in a
/// newline has the final empty element stripped, so `"a\n"` counts as
/// one line, not two. The empty string yields zero lines.
fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Truncate to the first N lines / M bytes.
///
/// Used by `read_file` and any future head-oriented tool. Never emits a
/// partial line; if the very first source line is larger than
/// `max_bytes` the result has empty `content` and
/// `first_line_exceeds_limit = true` so the caller can route to an
/// escape message instead of trying to display nonsense.
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    // First-line overflow: refuse to emit a partial line; signal the
    // caller to use a bash escape instead.
    if let Some(first) = lines.first() {
        if first.len() > max_bytes {
            return TruncationResult {
                content: String::new(),
                truncated: true,
                truncated_by: Some(TruncatedBy::Bytes),
                total_lines,
                total_bytes,
                output_lines: 0,
                output_bytes: 0,
                last_line_partial: false,
                first_line_exceeds_limit: true,
                max_lines,
                max_bytes,
            };
        }
    }

    // Walk forward, accumulating lines until adding the next line
    // would breach either budget. `running_bytes` is the byte length
    // of the joined content so far, including the separating `\n`
    // between kept lines.
    let mut kept: Vec<&str> = Vec::new();
    let mut running_bytes: usize = 0;
    let mut truncated_by: TruncatedBy = TruncatedBy::Lines;
    for line in &lines {
        if kept.len() >= max_lines {
            truncated_by = TruncatedBy::Lines;
            break;
        }
        let extra = line.len() + if kept.is_empty() { 0 } else { 1 };
        if running_bytes + extra > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        kept.push(line);
        running_bytes += extra;
    }

    let output = kept.join("\n");
    let output_bytes = output.len();
    let output_lines = kept.len();

    TruncationResult {
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines,
        output_bytes,
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Truncate to the last N lines / M bytes.
///
/// Used by `bash`. May return a partial first kept line (signalled
/// through `last_line_partial`) when the source's trailing line is
/// larger than `max_bytes`, so the model still sees the tail bytes
/// that usually carry the error message.
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let total_bytes = content.len();
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return TruncationResult {
            content: content.to_string(),
            truncated: false,
            truncated_by: None,
            total_lines,
            total_bytes,
            output_lines: total_lines,
            output_bytes: total_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines,
            max_bytes,
        };
    }

    // Walk backwards, prepending lines until adding the next line
    // would breach either budget.
    let mut kept: Vec<&str> = Vec::new();
    let mut running_bytes: usize = 0;
    let mut truncated_by: TruncatedBy = TruncatedBy::Lines;
    let mut last_line_partial = false;
    // Owned fallback for the partial-trailing-line case so the
    // returned `content` can outlive the borrow into `content`.
    let mut partial_line: Option<String> = None;
    for line in lines.iter().rev() {
        if kept.len() >= max_lines {
            truncated_by = TruncatedBy::Lines;
            break;
        }
        let extra = line.len() + if kept.is_empty() { 0 } else { 1 };
        if running_bytes + extra > max_bytes {
            truncated_by = TruncatedBy::Bytes;
            // Edge case: we haven't kept any line yet and this one
            // line alone is bigger than the budget. Keep the trailing
            // `max_bytes` of it so the model still sees the end of the
            // overflowing line; flag the partial.
            if kept.is_empty() {
                let partial = take_last_bytes_utf8(line, max_bytes);
                running_bytes = partial.len();
                partial_line = Some(partial);
                last_line_partial = true;
            }
            break;
        }
        kept.push(line);
        running_bytes += extra;
    }

    // Tie-break contract: when both budgets are hit at once (line cap
    // reached, byte total still within the byte cap), `Lines` wins. The
    // backward walk checks the line cap before the byte cap, so this
    // makes the documented tie-break explicit rather than leaving it to
    // loop ordering.
    if kept.len() >= max_lines && running_bytes <= max_bytes && partial_line.is_none() {
        truncated_by = TruncatedBy::Lines;
    }

    let output: String = if let Some(partial) = partial_line {
        partial
    } else {
        // We walked back-to-front; reverse to restore source order.
        kept.reverse();
        kept.join("\n")
    };
    let output_bytes = output.len();
    // A partial-leading-line counts as a single output line.
    let output_lines = if last_line_partial { 1 } else { kept.len() };

    TruncationResult {
        content: output,
        truncated: true,
        truncated_by: Some(truncated_by),
        total_lines,
        total_bytes,
        output_lines,
        output_bytes,
        last_line_partial,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    }
}

/// Return the trailing `max_bytes` of `s` as an owned `String`,
/// snapping forward to the next UTF-8 code-point boundary so we never
/// split a multi-byte character.
fn take_last_bytes_utf8(s: &str, max_bytes: usize) -> String {
    let bytes = s.as_bytes();
    if bytes.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = bytes.len() - max_bytes;
    // UTF-8 continuation bytes match `0b10xxxxxx`. Advance until we
    // land on a leading byte (or run out, which is impossible for
    // valid UTF-8 here but harmless).
    while start < bytes.len() && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    // `start` now sits on a code-point boundary, so this slice never
    // splits a character (and never panics).
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_thresholds() {
        assert_eq!(format_size(0), "0B");
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(1536), "1.5KB");
        assert_eq!(format_size(50 * 1024), "50.0KB");
        assert_eq!(format_size(2 * 1024 * 1024), "2.0MB");
    }

    #[test]
    fn split_for_counting_drops_one_trailing_newline() {
        assert_eq!(split_lines_for_counting(""), Vec::<&str>::new());
        assert_eq!(split_lines_for_counting("a"), vec!["a"]);
        assert_eq!(split_lines_for_counting("a\n"), vec!["a"]);
        assert_eq!(split_lines_for_counting("a\nb"), vec!["a", "b"]);
        assert_eq!(split_lines_for_counting("a\nb\n"), vec!["a", "b"]);
        // Two trailing newlines: only one is consumed.
        assert_eq!(split_lines_for_counting("a\n\n"), vec!["a", ""]);
    }

    #[test]
    fn truncate_head_passes_through_when_under_caps() {
        let r = truncate_head("a\nb\nc", 10, 1024);
        assert!(!r.truncated);
        assert_eq!(r.content, "a\nb\nc");
        assert_eq!(r.total_lines, 3);
        assert_eq!(r.output_lines, 3);
    }

    #[test]
    fn truncate_head_hits_line_cap_first() {
        let src = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_head(&src, 3, 10_000);
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(r.output_lines, 3);
        assert_eq!(r.total_lines, 10);
        assert_eq!(r.content, "line 1\nline 2\nline 3");
    }

    #[test]
    fn truncate_head_hits_byte_cap_first() {
        // Three 100-byte lines (99 'x' + the implicit join newline).
        let line: String = "x".repeat(99);
        let src = format!("{line}\n{line}\n{line}");
        // Cap at 150 bytes: first line (99) + '\n' (1) + second line
        // (99) = 199, over budget. So we keep only the first line.
        let r = truncate_head(&src, 10, 150);
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(r.output_lines, 1);
        assert_eq!(r.content, line);
    }

    #[test]
    fn truncate_head_flags_first_line_exceeds_limit() {
        let line: String = "z".repeat(200);
        let r = truncate_head(&line, 10, 50);
        assert!(r.truncated);
        assert!(r.first_line_exceeds_limit);
        assert_eq!(r.output_lines, 0);
        assert_eq!(r.content, "");
    }

    #[test]
    fn truncate_tail_passes_through_when_under_caps() {
        let r = truncate_tail("a\nb\nc", 10, 1024);
        assert!(!r.truncated);
        assert_eq!(r.content, "a\nb\nc");
    }

    #[test]
    fn truncate_tail_hits_line_cap_first() {
        let src = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_tail(&src, 3, 10_000);
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(r.output_lines, 3);
        assert_eq!(r.total_lines, 10);
        assert_eq!(r.content, "line 8\nline 9\nline 10");
    }

    #[test]
    fn truncate_tail_partial_last_line_when_source_line_is_huge() {
        // One 200-byte line; cap at 50 bytes. Expect a 50-byte partial.
        let line: String = "y".repeat(200);
        let r = truncate_tail(&line, 10, 50);
        assert!(r.truncated);
        assert!(r.last_line_partial);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(r.output_lines, 1);
        assert_eq!(r.output_bytes, 50);
        assert_eq!(r.content.len(), 50);
        assert!(r.content.ends_with(&"y".repeat(50)));
    }

    #[test]
    fn truncate_tail_keeps_trailing_lines_under_byte_cap() {
        let lines = (1..=5)
            .map(|i| format!("{:04}", i)) // 4 bytes/line
            .collect::<Vec<_>>()
            .join("\n");
        // Total: 5*4 + 4 newlines = 24 bytes. Cap at 9 → keep last 2
        // lines (4 + 1 + 4 = 9).
        let r = truncate_tail(&lines, 10, 9);
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
        assert_eq!(r.output_lines, 2);
        assert_eq!(r.content, "0004\n0005");
    }

    #[test]
    fn truncate_tail_lines_win_when_both_budgets_hit_at_once() {
        // Five 4-byte lines. Keeping the last two yields "0004\n0005" =
        // 9 bytes, exactly the byte cap, while the line cap (2) is also
        // reached. The simultaneous tie must resolve to `Lines`.
        let lines = (1..=5)
            .map(|i| format!("{i:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_tail(&lines, 2, 9);
        assert!(r.truncated);
        assert_eq!(r.truncated_by, Some(TruncatedBy::Lines));
        assert_eq!(r.output_lines, 2);
        assert_eq!(r.output_bytes, 9);
        assert_eq!(r.content, "0004\n0005");
    }

    #[test]
    fn take_last_bytes_utf8_respects_char_boundary() {
        // 'é' is 0xC3 0xA9 in UTF-8 (2 bytes). The string is 8 bytes
        // ("aaaaéé"); take last 3 — splitting in the middle of an 'é'
        // would be invalid, so we should snap forward to a boundary.
        let s = "aaaaéé";
        let kept = take_last_bytes_utf8(s, 3);
        assert!(kept.chars().all(|c| c == 'a' || c == 'é'));
        assert!(kept.is_ascii() || kept.contains('é'));
        // And the result must be valid UTF-8 by construction (Rust
        // would have panicked otherwise on `s[start..]`).
        let _ = kept.as_bytes();
    }
}
