//! Inline image protocol encoders (Kitty graphics + iTerm2 OSC 1337)
//! plus the bookkeeping the differential renderer needs to keep
//! multi-row image placements consistent across frames.
//!
//! ## Multi-row image rows in the renderer
//!
//! An inline image occupies N terminal rows but the differential
//! renderer operates on a `Vec<String>` of logical rows. We
//! represent an image as N strings: one string carries the image
//! escape, the other N-1 are empty. Protocol-specific tricks make
//! the cursor land where the diff engine assumes after the row is
//! painted:
//!
//! - **Kitty graphics**: include `C=1` (suppress cursor advance)
//!   and `r=<rows>` (allocate exactly N rows). The image escape
//!   sits at index 0; indices 1..N-1 are `""`.
//! - **iTerm2 OSC 1337**: terminal advances N rows on its own.
//!   We prepend `\x1b[<N-1>A` on the row carrying the escape so
//!   the post-image cursor lands on the same logical row the diff
//!   engine thinks it's on. Indices 0..N-2 are `""`; index N-1
//!   holds the escape.
//!
//! Image rows have visible width 0 but are long byte-wise. The
//! renderer uses [`is_image_line`] to skip width validation and
//! the per-line SGR/OSC reset for these rows. Kitty placements
//! must be deleted (by image ID) before redraw; the renderer uses
//! [`extract_kitty_image_ids`] to discover which IDs were placed
//! on each previously-rendered row.

use std::sync::atomic::{AtomicU32, Ordering};

/// Per-cell pixel size used when the host terminal does not report
/// one. Conservative default tuned for typical 9x18 monospace
/// fonts; the terminal still scales the image to its own cell
/// pixel size regardless, so an inaccurate default only affects
/// the initial cell-footprint math.
pub const DEFAULT_CELL_PIXEL_SIZE: (u32, u32) = (9, 18);

/// Allocate a fresh Kitty image ID. The protocol allows
/// `0 < id < 2^24`. The counter is process-global so IDs are
/// unique within one aj run; reuse across runs is harmless because
/// placements are owned by the terminal.
pub fn next_kitty_image_id() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(1);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Wrap inside the protocol's 24-bit range, never returning 0.
    let id = id & 0x00FF_FFFF;
    if id == 0 { 1 } else { id }
}

/// Max payload bytes per Kitty graphics chunk (protocol limit).
const KITTY_CHUNK_BYTES: usize = 4096;

/// Build the Kitty graphics escape for an image.
///
/// `base64_data` is the already-base64-encoded PNG bytes; Kitty
/// only accepts PNG inline (callers are responsible for falling
/// back to text when the source mime is JPEG). `cols` / `rows`
/// are the cell footprint to allocate, `image_id` is a fresh ID
/// from [`next_kitty_image_id`].
///
/// The first chunk parameters: `a=T` (transmit & display),
/// `f=100` (PNG), `q=2` (suppress responses), `C=1` (don't
/// advance the cursor), `c=<cols>`, `r=<rows>`, `i=<image_id>`.
/// Payloads larger than [`KITTY_CHUNK_BYTES`] base64 bytes are
/// split into multiple `m=1` chunks followed by a final `m=0`
/// chunk; only the first chunk re-states the parameter list.
pub fn kitty_sequence(base64_data: &str, cols: u32, rows: u32, image_id: u32) -> String {
    let bytes = base64_data.as_bytes();
    let mut out = String::with_capacity(bytes.len() + 64);

    if bytes.len() <= KITTY_CHUNK_BYTES {
        out.push_str(&format!(
            "\x1b_Ga=T,f=100,q=2,C=1,c={cols},r={rows},i={image_id};",
        ));
        out.push_str(base64_data);
        out.push_str("\x1b\\");
        return out;
    }

    let mut offset = 0;
    let mut first = true;
    while offset < bytes.len() {
        let end = (offset + KITTY_CHUNK_BYTES).min(bytes.len());
        let more = end < bytes.len();
        if first {
            out.push_str(&format!(
                "\x1b_Ga=T,f=100,q=2,C=1,c={cols},r={rows},i={image_id},m=1;",
            ));
            first = false;
        } else if more {
            out.push_str("\x1b_Gm=1;");
        } else {
            out.push_str("\x1b_Gm=0;");
        }
        // Safe: base64 bytes are ASCII.
        out.push_str(&base64_data[offset..end]);
        out.push_str("\x1b\\");
        offset = end;
    }
    out
}

/// Build the Kitty delete-by-id escape. Emit before redrawing any
/// row that previously held a placement of this image; Kitty
/// doesn't replace placements by overwriting cells.
pub fn kitty_delete(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id}\x1b\\")
}

/// Build the iTerm2 inline-image escape (OSC 1337).
///
/// iTerm2 advances the cursor `rows` lines after rendering; we
/// prepend `\x1b[<rows-1>A` (cursor up `rows-1`) so the terminal's
/// post-image cursor lands at the start of the last logical row
/// of the placement, matching what the diff engine assumes after
/// writing the N-th `Vec<String>` row. When `rows == 1` no
/// cursor-up is needed.
///
/// iTerm2 accepts PNG and JPEG; no chunking required.
pub fn iterm2_sequence(base64_data: &str, cols: u32, rows: u32) -> String {
    let mut out = String::with_capacity(base64_data.len() + 64);
    if rows > 1 {
        out.push_str(&format!("\x1b[{}A", rows - 1));
    }
    out.push_str(&format!(
        "\x1b]1337;File=inline=1;width={cols};height={rows};preserveAspectRatio=1:",
    ));
    out.push_str(base64_data);
    out.push('\x07');
    out
}

/// True when `line` carries an image protocol escape. The
/// renderer skips width validation and the per-line SGR/OSC reset
/// on these rows — both would corrupt the payload.
pub fn is_image_line(line: &str) -> bool {
    line.contains("\x1b_G") || line.contains("\x1b]1337;File=")
}

/// Extract every Kitty image ID placed in `line`.
///
/// Scans for `\x1b_G…;` openers and pulls the `i=<digits>` value
/// out of the parameter list. Multi-chunk lines re-state the ID
/// only in the first chunk, so a typical placement yields one ID
/// per row. Returns an empty vec when no Kitty placement is
/// present.
pub fn extract_kitty_image_ids(line: &str) -> Vec<u32> {
    let bytes = line.as_bytes();
    let mut ids = Vec::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'_' && bytes[i + 2] == b'G' {
            // Walk the parameter list until `;` (start of payload)
            // or `\x1b\\` (end of escape with no payload).
            let mut j = i + 3;
            let params_end = loop {
                if j >= bytes.len() {
                    break j;
                }
                let b = bytes[j];
                if b == b';' {
                    break j;
                }
                if b == 0x1b {
                    break j;
                }
                j += 1;
            };
            // Parse `i=<digits>` inside `bytes[i+3..params_end]`.
            if let Some(id) = parse_kitty_image_id(&bytes[i + 3..params_end]) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            i = params_end;
        } else {
            i += 1;
        }
    }
    ids
}

/// Find an `i=<digits>` parameter in a Kitty parameter run
/// (`a=T,f=100,...,i=42`). Returns the parsed u32 or `None`.
fn parse_kitty_image_id(params: &[u8]) -> Option<u32> {
    let mut k = 0;
    while k < params.len() {
        // Match `i=` at a parameter boundary (start or after `,`).
        let at_boundary = k == 0 || params[k - 1] == b',';
        if at_boundary && k + 1 < params.len() && params[k] == b'i' && params[k + 1] == b'=' {
            let mut m = k + 2;
            let start = m;
            while m < params.len() && params[m].is_ascii_digit() {
                m += 1;
            }
            if m > start {
                let s = std::str::from_utf8(&params[start..m]).ok()?;
                return s.parse::<u32>().ok();
            }
        }
        k += 1;
    }
    None
}

/// Compute the cell-grid footprint for an image.
///
/// `image_pixels` is the source `(width, height)` in pixels;
/// `cell_pixel` is `(width, height)` per terminal cell. The
/// result is scaled to fit within `max_cells = (max_cols,
/// max_rows)` preserving aspect ratio. Minimum result is `(1,
/// 1)` so a tiny image still occupies at least one cell.
pub fn image_cell_footprint(
    image_pixels: (u32, u32),
    cell_pixel: (u32, u32),
    max_cells: (u32, u32),
) -> (u32, u32) {
    let (iw, ih) = image_pixels;
    let (cw, ch) = cell_pixel;
    let (max_cols, max_rows) = max_cells;
    if iw == 0 || ih == 0 || cw == 0 || ch == 0 || max_cols == 0 || max_rows == 0 {
        return (1, 1);
    }
    // Initial cell-grid footprint at 1:1 pixel scale.
    let base_cols = iw.div_ceil(cw).max(1);
    let base_rows = ih.div_ceil(ch).max(1);
    // Scale down preserving aspect ratio if either axis overflows.
    // Use f32 arithmetic; terminal sizes stay well within f32 precision.
    #[allow(clippy::as_conversions)]
    let scale_cols = if base_cols > max_cols {
        max_cols as f32 / base_cols as f32
    } else {
        1.0
    };
    #[allow(clippy::as_conversions)]
    let scale_rows = if base_rows > max_rows {
        max_rows as f32 / base_rows as f32
    } else {
        1.0
    };
    let scale = scale_cols.min(scale_rows);
    #[allow(clippy::as_conversions)]
    let cols = ((base_cols as f32) * scale).floor().max(1.0) as u32;
    #[allow(clippy::as_conversions)]
    let rows = ((base_rows as f32) * scale).floor().max(1.0) as u32;
    (cols.min(max_cols), rows.min(max_rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_image_ids_are_unique() {
        let a = next_kitty_image_id();
        let b = next_kitty_image_id();
        let c = next_kitty_image_id();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn is_image_line_recognises_both_protocols() {
        assert!(is_image_line("\x1b_Ga=T,f=100;abc\x1b\\"));
        assert!(is_image_line("\x1b]1337;File=inline=1:abc\x07"));
        assert!(!is_image_line("plain text"));
        assert!(!is_image_line("\x1b[31mred\x1b[0m"));
    }

    #[test]
    fn extract_ids_finds_single_chunk_id() {
        let line = "\x1b_Ga=T,f=100,c=4,r=2,i=42;ZGF0YQ==\x1b\\";
        assert_eq!(extract_kitty_image_ids(line), vec![42]);
    }

    #[test]
    fn extract_ids_finds_first_chunk_only_in_multi_chunk() {
        // Only the first chunk re-states the ID; continuation chunks
        // carry only `m=1` / `m=0`. The extractor should return a
        // single ID for the whole row.
        let line = format!(
            "\x1b_Ga=T,f=100,c=4,r=2,i=7,m=1;{}\x1b\\\x1b_Gm=1;{}\x1b\\\x1b_Gm=0;{}\x1b\\",
            "A".repeat(10),
            "B".repeat(10),
            "C".repeat(10),
        );
        assert_eq!(extract_kitty_image_ids(&line), vec![7]);
    }

    #[test]
    fn extract_ids_empty_for_non_image_lines() {
        assert!(extract_kitty_image_ids("hello world").is_empty());
        assert!(extract_kitty_image_ids("\x1b[31mred\x1b[0m").is_empty());
    }

    #[test]
    fn footprint_clamps_to_one_by_one_minimum() {
        assert_eq!(image_cell_footprint((1, 1), (9, 18), (40, 20)), (1, 1));
        assert_eq!(image_cell_footprint((0, 0), (9, 18), (40, 20)), (1, 1));
    }

    #[test]
    fn footprint_respects_max_bounds_and_aspect_ratio() {
        // 1000x500 image with 10x10 cells → base (100, 50).
        // Cap (50, 25) means scale 0.5 → (50, 25).
        let (c, r) = image_cell_footprint((1000, 500), (10, 10), (50, 25));
        assert!(c <= 50 && r <= 25);
        // Aspect ratio ~2:1 preserved.
        assert!(c >= 2 * r - 2 && c <= 2 * r + 2, "got {c}x{r}");
    }

    #[test]
    fn kitty_sequence_single_chunk_for_small_payload() {
        let s = kitty_sequence("abc", 4, 2, 99);
        assert!(s.starts_with("\x1b_Ga=T,f=100,q=2,C=1,c=4,r=2,i=99;abc"));
        assert!(s.ends_with("\x1b\\"));
        assert!(!s.contains("m=1"));
    }

    #[test]
    fn kitty_sequence_chunks_large_payload() {
        // 10 KiB of base64 → must produce m=1 chunks and a final m=0.
        let payload = "A".repeat(10 * 1024);
        let s = kitty_sequence(&payload, 4, 2, 5);
        assert!(s.contains(",m=1;"), "first chunk should advertise m=1");
        assert!(s.contains("\x1b_Gm=1;"), "middle chunks");
        assert!(s.contains("\x1b_Gm=0;"), "final chunk");
        // First chunk's parameter list precedes the first `m=1`.
        let first_chunk = s.find("i=5,m=1;").expect("first chunk header");
        let final_chunk = s.find("\x1b_Gm=0;").expect("final chunk");
        assert!(first_chunk < final_chunk);
    }

    #[test]
    fn iterm2_sequence_prepends_cursor_up_only_when_rows_gt_one() {
        let single = iterm2_sequence("abc", 4, 1);
        assert!(single.starts_with("\x1b]1337;File="));
        assert!(!single.contains("\x1b[0A"));

        let multi = iterm2_sequence("abc", 4, 5);
        assert!(multi.starts_with("\x1b[4A"));
        assert!(multi.contains("\x1b]1337;File=inline=1;width=4;height=5"));
        assert!(multi.ends_with('\x07'));
    }
}
