//! Control-sequence constants and typed encoders: the spec for all wire
//! output.
//!
//! Upstream keeps every sequence as a string, some literal and some
//! `std.fmt` templates filled in at the call site. We split that into two
//! shapes. Sequences with no placeholders stay `pub const` byte strings.
//! Parameterized sequences become typed encoder functions that write into a
//! generic [`std::io::Write`], so the parameter types and the colon-versus-
//! semicolon SGR variants are checked at compile time instead of by getting
//! a format string's argument list right by hand.

use std::io::{self, Write};

use crate::cell::{CursorShape, VerticalAlignment};

// Queries
pub const PRIMARY_DEVICE_ATTRS: &str = "\x1b[c";
pub const TERTIARY_DEVICE_ATTRS: &str = "\x1b[=c";
pub const DEVICE_STATUS_REPORT: &str = "\x1b[5n";
pub const XTVERSION: &str = "\x1b[>0q";
pub const DECRQM_FOCUS: &str = "\x1b[?1004$p";
pub const DECRQM_SGR_PIXELS: &str = "\x1b[?1016$p";
pub const DECRQM_SYNC: &str = "\x1b[?2026$p";
pub const DECRQM_UNICODE: &str = "\x1b[?2027$p";
pub const DECRQM_COLOR_SCHEME: &str = "\x1b[?2031$p";
pub const CSI_U_QUERY: &str = "\x1b[?u";
pub const KITTY_GRAPHICS_QUERY: &str = "\x1b_Gi=1,a=q\x1b\\";
pub const SIXEL_GEOMETRY_QUERY: &str = "\x1b[?2;1;0S";
pub const CURSOR_POSITION_REQUEST: &str = "\x1b[6n";
pub const EXPLICIT_WIDTH_QUERY: &str = "\x1b]66;w=1; \x1b\\";
pub const SCALED_TEXT_QUERY: &str = "\x1b]66;s=2; \x1b\\";
pub const MULTI_CURSOR_QUERY: &str = "\x1b[> q";

// Mouse. We try for button motion and any motion. Terminals will enable the
// last one we tried (any motion). This was added because zellij doesn't
// support any motion currently.
// See: https://github.com/zellij-org/zellij/issues/1679
pub const MOUSE_SET: &str = "\x1b[?1002;1003;1004;1006h";
pub const MOUSE_SET_PIXELS: &str = "\x1b[?1002;1003;1004;1016h";
pub const MOUSE_RESET: &str = "\x1b[?1002;1003;1004;1006;1016l";

// In-band window size reports
pub const IN_BAND_RESIZE_SET: &str = "\x1b[?2048h";
pub const IN_BAND_RESIZE_RESET: &str = "\x1b[?2048l";

// Sync
pub const SYNC_SET: &str = "\x1b[?2026h";
pub const SYNC_RESET: &str = "\x1b[?2026l";

// Unicode
pub const UNICODE_SET: &str = "\x1b[?2027h";
pub const UNICODE_RESET: &str = "\x1b[?2027l";

/// Emits the explicit-width OSC for a grapheme: `OSC 66 ; w={width} ; {text} ST`.
pub fn explicit_width<W: Write>(w: &mut W, width: u16, text: &str) -> io::Result<()> {
    write!(w, "\x1b]66;w={width};{text}\x1b\\")
}

// Text sizing

/// Emits a whole-cell scaled-text run: `OSC 66 ; s={scale} : w={width} ; {text} ST`.
pub fn scaled_text<W: Write>(w: &mut W, scale: u8, width: u16, text: &str) -> io::Result<()> {
    write!(w, "\x1b]66;s={scale}:w={width};{text}\x1b\\")
}

/// Emits a fractionally scaled-text run, adding numerator/denominator and the
/// vertical-alignment selector to [`scaled_text`].
pub fn scaled_text_with_fractions<W: Write>(
    w: &mut W,
    scale: u8,
    width: u16,
    numerator: u8,
    denominator: u8,
    vertical_alignment: VerticalAlignment,
    text: &str,
) -> io::Result<()> {
    let v = vertical_alignment_code(vertical_alignment);
    write!(
        w,
        "\x1b]66;s={scale}:w={width}:n={numerator}:d={denominator}:v={v};{text}\x1b\\"
    )
}

// Bracketed paste
pub const BP_SET: &str = "\x1b[?2004h";
pub const BP_RESET: &str = "\x1b[?2004l";

// Color scheme updates
pub const COLOR_SCHEME_REQUEST: &str = "\x1b[?996n";
pub const COLOR_SCHEME_SET: &str = "\x1b[?2031h";
pub const COLOR_SCHEME_RESET: &str = "\x1b[?2031l";

// Key encoding

/// Pushes a kitty keyboard-protocol flag set: `CSI > {flags} u`.
pub fn csi_u_push<W: Write>(w: &mut W, flags: u8) -> io::Result<()> {
    write!(w, "\x1b[>{flags}u")
}

pub const CSI_U_POP: &str = "\x1b[<u";

// Cursor
pub const HOME: &str = "\x1b[H";

/// Absolute cursor position: `CSI {row} ; {col} H`. Both are 1-based.
pub fn cup<W: Write>(w: &mut W, row: u16, col: u16) -> io::Result<()> {
    write!(w, "\x1b[{row};{col}H")
}

pub const HIDE_CURSOR: &str = "\x1b[?25l";
pub const SHOW_CURSOR: &str = "\x1b[?25h";

/// Sets the cursor shape via `DECSCUSR` (`CSI {n} SP q`). `n` is the shape's
/// discriminant: 0 default, 1 blinking block, ... 6 steady bar.
pub fn cursor_shape<W: Write>(w: &mut W, shape: CursorShape) -> io::Result<()> {
    let code: u8 = match shape {
        CursorShape::Default => 0,
        CursorShape::BlockBlink => 1,
        CursorShape::Block => 2,
        CursorShape::UnderlineBlink => 3,
        CursorShape::Underline => 4,
        CursorShape::BeamBlink => 5,
        CursorShape::Beam => 6,
    };
    write!(w, "\x1b[{code} q")
}

pub const RI: &str = "\x1bM";
pub const IND: &str = "\n";

/// Cursor forward by `n` columns: `CSI {n} C`.
pub fn cuf<W: Write>(w: &mut W, n: u16) -> io::Result<()> {
    write!(w, "\x1b[{n}C")
}

/// Cursor back by `n` columns: `CSI {n} D`.
pub fn cub<W: Write>(w: &mut W, n: u16) -> io::Result<()> {
    write!(w, "\x1b[{n}D")
}

// Multi cursor

/// Sets the secondary-cursor color (RGB): `CSI > 40 ; 2 : {r} : {g} : {b} SP q`.
pub fn secondary_cursors_rgb<W: Write>(w: &mut W, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(w, "\x1b[>40;2:{r}:{g}:{b} q")
}

pub const RESET_SECONDARY_CURSORS: &str = "\x1b[>0;4 q";

/// Shows a secondary cursor at a 1-based position: `CSI > 29 ; 2 : {row} : {col} SP q`.
pub fn show_secondary_cursor<W: Write>(w: &mut W, row: u16, col: u16) -> io::Result<()> {
    write!(w, "\x1b[>29;2:{row}:{col} q")
}

// Erase
pub const ERASE_BELOW_CURSOR: &str = "\x1b[J";

// Alt screen
pub const SMCUP: &str = "\x1b[?1049h";
pub const RMCUP: &str = "\x1b[?1049l";

// SGR reset all
pub const SGR_RESET: &str = "\x1b[m";

// Colors
//
// Vaxis emits SGR colors in two wire dialects. The "standard" forms use the
// ITU-T colon subparameter separator (`38:2:r:g:b`), which is what the spec
// prescribes and what disambiguates RGB from the legacy indexed form. The
// `_legacy` forms use semicolons (`38;2;r;g;b`) for terminals that never
// implemented colon subparameters. The choice is made per terminal capability
// at the call site, so both must be available and byte-exact.

/// Foreground from the 8-color base set: `CSI 3{n} m` (`n` is 0..=7).
pub fn fg_base<W: Write>(w: &mut W, n: u8) -> io::Result<()> {
    write!(w, "\x1b[3{n}m")
}

/// Foreground from the bright 8-color set: `CSI 9{n} m` (`n` is 0..=7).
pub fn fg_bright<W: Write>(w: &mut W, n: u8) -> io::Result<()> {
    write!(w, "\x1b[9{n}m")
}

/// Background from the 8-color base set: `CSI 4{n} m` (`n` is 0..=7).
pub fn bg_base<W: Write>(w: &mut W, n: u8) -> io::Result<()> {
    write!(w, "\x1b[4{n}m")
}

/// Background from the bright 8-color set: `CSI 10{n} m` (`n` is 0..=7).
pub fn bg_bright<W: Write>(w: &mut W, n: u8) -> io::Result<()> {
    write!(w, "\x1b[10{n}m")
}

pub const FG_RESET: &str = "\x1b[39m";
pub const BG_RESET: &str = "\x1b[49m";
pub const UL_RESET: &str = "\x1b[59m";

/// 256-color indexed foreground (colon form): `CSI 38 : 5 : {idx} m`.
pub fn fg_indexed<W: Write>(w: &mut W, idx: u8) -> io::Result<()> {
    write!(w, "\x1b[38:5:{idx}m")
}

/// 256-color indexed background (colon form): `CSI 48 : 5 : {idx} m`.
pub fn bg_indexed<W: Write>(w: &mut W, idx: u8) -> io::Result<()> {
    write!(w, "\x1b[48:5:{idx}m")
}

/// 256-color indexed underline (colon form): `CSI 58 : 5 : {idx} m`.
pub fn ul_indexed<W: Write>(w: &mut W, idx: u8) -> io::Result<()> {
    write!(w, "\x1b[58:5:{idx}m")
}

/// RGB foreground (colon form): `CSI 38 : 2 : {r} : {g} : {b} m`.
pub fn fg_rgb<W: Write>(w: &mut W, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(w, "\x1b[38:2:{r}:{g}:{b}m")
}

/// RGB background (colon form): `CSI 48 : 2 : {r} : {g} : {b} m`.
pub fn bg_rgb<W: Write>(w: &mut W, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(w, "\x1b[48:2:{r}:{g}:{b}m")
}

/// RGB underline (colon form): `CSI 58 : 2 : {r} : {g} : {b} m`.
pub fn ul_rgb<W: Write>(w: &mut W, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(w, "\x1b[58:2:{r}:{g}:{b}m")
}

/// 256-color indexed foreground (legacy semicolon form): `CSI 38 ; 5 ; {idx} m`.
pub fn fg_indexed_legacy<W: Write>(w: &mut W, idx: u8) -> io::Result<()> {
    write!(w, "\x1b[38;5;{idx}m")
}

/// 256-color indexed background (legacy semicolon form): `CSI 48 ; 5 ; {idx} m`.
pub fn bg_indexed_legacy<W: Write>(w: &mut W, idx: u8) -> io::Result<()> {
    write!(w, "\x1b[48;5;{idx}m")
}

/// 256-color indexed underline (legacy semicolon form): `CSI 58 ; 5 ; {idx} m`.
pub fn ul_indexed_legacy<W: Write>(w: &mut W, idx: u8) -> io::Result<()> {
    write!(w, "\x1b[58;5;{idx}m")
}

/// RGB foreground (legacy semicolon form): `CSI 38 ; 2 ; {r} ; {g} ; {b} m`.
pub fn fg_rgb_legacy<W: Write>(w: &mut W, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(w, "\x1b[38;2;{r};{g};{b}m")
}

/// RGB background (legacy semicolon form): `CSI 48 ; 2 ; {r} ; {g} ; {b} m`.
pub fn bg_rgb_legacy<W: Write>(w: &mut W, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(w, "\x1b[48;2;{r};{g};{b}m")
}

/// RGB underline (legacy semicolon form): `CSI 58 ; 2 ; {r} ; {g} ; {b} m`.
pub fn ul_rgb_legacy<W: Write>(w: &mut W, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(w, "\x1b[58;2;{r};{g};{b}m")
}

/// RGB underline for conpty: `CSI 58 : 2 : : {r} : {g} : {b} m`.
///
/// NOTE: The empty field after `2` is the colorspace-id subparameter. Conpty
/// requires it present-but-empty, where the regular [`ul_rgb`] form omits it.
pub fn ul_rgb_conpty<W: Write>(w: &mut W, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(w, "\x1b[58:2::{r}:{g}:{b}m")
}

// Underlines
// NOTE: UL_OFF could be `\x1b[4:0m`, but `24` is more widely supported.
pub const UL_OFF: &str = "\x1b[24m";
pub const UL_SINGLE: &str = "\x1b[4m";
pub const UL_DOUBLE: &str = "\x1b[4:2m";
pub const UL_CURLY: &str = "\x1b[4:3m";
pub const UL_DOTTED: &str = "\x1b[4:4m";
pub const UL_DASHED: &str = "\x1b[4:5m";

// Attributes
pub const BOLD_SET: &str = "\x1b[1m";
pub const DIM_SET: &str = "\x1b[2m";
pub const ITALIC_SET: &str = "\x1b[3m";
pub const BLINK_SET: &str = "\x1b[5m";
pub const REVERSE_SET: &str = "\x1b[7m";
pub const INVISIBLE_SET: &str = "\x1b[8m";
pub const STRIKETHROUGH_SET: &str = "\x1b[9m";
pub const BOLD_DIM_RESET: &str = "\x1b[22m";
pub const ITALIC_RESET: &str = "\x1b[23m";
pub const BLINK_RESET: &str = "\x1b[25m";
pub const REVERSE_RESET: &str = "\x1b[27m";
pub const INVISIBLE_RESET: &str = "\x1b[28m";
pub const STRIKETHROUGH_RESET: &str = "\x1b[29m";

// OSC sequences

/// Sets the window title: `OSC 2 ; {title} ST`.
pub fn osc2_set_title<W: Write>(w: &mut W, title: &str) -> io::Result<()> {
    write!(w, "\x1b]2;{title}\x1b\\")
}

/// Reports the working directory: `OSC 7 ; {uri} ST`. The uri is written via
/// its `Display`, matching upstream's `{f}` "format-via-the-type" placeholder.
pub fn osc7<W: Write>(w: &mut W, uri: impl std::fmt::Display) -> io::Result<()> {
    write!(w, "\x1b]7;{uri}\x1b\\")
}

/// Opens a hyperlink: `OSC 8 ; {params} ; {uri} ST`.
pub fn osc8<W: Write>(w: &mut W, params: &str, uri: &str) -> io::Result<()> {
    write!(w, "\x1b]8;{params};{uri}\x1b\\")
}

pub const OSC8_CLEAR: &str = "\x1b]8;;\x1b\\";

/// Posts a desktop notification: `OSC 9 ; {body} ST`.
pub fn osc9_notify<W: Write>(w: &mut W, body: &str) -> io::Result<()> {
    write!(w, "\x1b]9;{body}\x1b\\")
}

/// Posts a desktop notification with a title: `OSC 777 ; notify ; {title} ; {body} ST`.
pub fn osc777_notify<W: Write>(w: &mut W, title: &str, body: &str) -> io::Result<()> {
    write!(w, "\x1b]777;notify;{title};{body}\x1b\\")
}

/// Sets the pointer (mouse) shape: `OSC 22 ; {shape} ST`.
pub fn osc22_mouse_shape<W: Write>(w: &mut W, shape: &str) -> io::Result<()> {
    write!(w, "\x1b]22;{shape}\x1b\\")
}

/// Copies base64 data to the clipboard: `OSC 52 ; c ; {b64} ST`.
pub fn osc52_clipboard_copy<W: Write>(w: &mut W, b64: &str) -> io::Result<()> {
    write!(w, "\x1b]52;c;{b64}\x1b\\")
}

pub const OSC52_CLIPBOARD_REQUEST: &str = "\x1b]52;c;?\x1b\\";

// Kitty graphics
pub const KITTY_GRAPHICS_CLEAR: &str = "\x1b_Ga=d\x1b\\";

/// Opens a kitty graphics placement command: `APC G a=p,i={id}`. The caller
/// appends placement options and closes with [`KITTY_GRAPHICS_CLOSING`].
pub fn kitty_graphics_preamble<W: Write>(w: &mut W, id: u32) -> io::Result<()> {
    write!(w, "\x1b_Ga=p,i={id}")
}

pub const KITTY_GRAPHICS_CLOSING: &str = ",C=1\x1b\\";

// Color control sequences (OSC 4 / 10 / 11 / 12).
//
// The set forms encode each 8-bit channel as the X11 `rgb:RRRR/GGGG/BBBB`
// 16-bit spec by writing the byte twice (`{byte:02x}{byte:02x}`), so 0xab
// becomes `abab`. Hex is lowercase and zero-padded to width 2, matching
// upstream's `{x:0>2}`.

/// Queries palette color `idx`: `OSC 4 ; {idx} ; ? ST`.
pub fn osc4_query<W: Write>(w: &mut W, idx: u8) -> io::Result<()> {
    write!(w, "\x1b]4;{idx};?\x1b\\")
}

/// Resets _all_ palette colors.
pub const OSC4_RESET: &str = "\x1b]104\x1b\\";

/// Queries the default foreground color.
pub const OSC10_QUERY: &str = "\x1b]10;?\x1b\\";

/// Sets the default foreground color: `OSC 10 ; rgb:RRRR/GGGG/BBBB ST`.
pub fn osc10_set<W: Write>(w: &mut W, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(
        w,
        "\x1b]10;rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\"
    )
}

/// Resets the foreground to the terminal default.
pub const OSC10_RESET: &str = "\x1b]110\x1b\\";

/// Queries the default background color.
pub const OSC11_QUERY: &str = "\x1b]11;?\x1b\\";

/// Sets the default background color: `OSC 11 ; rgb:RRRR/GGGG/BBBB ST`.
pub fn osc11_set<W: Write>(w: &mut W, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(
        w,
        "\x1b]11;rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\"
    )
}

/// Resets the background to the terminal default.
pub const OSC11_RESET: &str = "\x1b]111\x1b\\";

/// Queries the cursor color.
pub const OSC12_QUERY: &str = "\x1b]12;?\x1b\\";

/// Sets the cursor color: `OSC 12 ; rgb:RRRR/GGGG/BBBB ST`.
pub fn osc12_set<W: Write>(w: &mut W, r: u8, g: u8, b: u8) -> io::Result<()> {
    write!(
        w,
        "\x1b]12;rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\"
    )
}

/// Resets the cursor to the terminal default.
pub const OSC12_RESET: &str = "\x1b]112\x1b\\";

fn vertical_alignment_code(alignment: VerticalAlignment) -> u8 {
    match alignment {
        VerticalAlignment::Top => 0,
        VerticalAlignment::Bottom => 1,
        VerticalAlignment::Center => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs an encoder into a buffer and returns the bytes as a `String` for
    /// byte-exact comparison against the expected escape sequence.
    fn encode(f: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn cursor_encoders() {
        assert_eq!(encode(|w| cup(w, 1, 1)), "\x1b[1;1H");
        assert_eq!(encode(|w| cup(w, 12, 34)), "\x1b[12;34H");
        assert_eq!(encode(|w| cuf(w, 3)), "\x1b[3C");
        assert_eq!(encode(|w| cub(w, 7)), "\x1b[7D");
    }

    #[test]
    fn cursor_shape_discriminants() {
        assert_eq!(
            encode(|w| cursor_shape(w, CursorShape::Default)),
            "\x1b[0 q"
        );
        assert_eq!(encode(|w| cursor_shape(w, CursorShape::Block)), "\x1b[2 q");
        assert_eq!(encode(|w| cursor_shape(w, CursorShape::Beam)), "\x1b[6 q");
    }

    #[test]
    fn csi_u_push_flags() {
        // 0b11101 == 29: a representative kitty keyboard flag set.
        assert_eq!(encode(|w| csi_u_push(w, 0b11101)), "\x1b[>29u");
    }

    #[test]
    fn rgb_color_variants() {
        // Standard colon form versus legacy semicolon form versus the conpty
        // form with the empty colorspace field.
        assert_eq!(
            encode(|w| fg_rgb(w, 0xAB, 0xCD, 0xEF)),
            "\x1b[38:2:171:205:239m"
        );
        assert_eq!(
            encode(|w| fg_rgb_legacy(w, 0xAB, 0xCD, 0xEF)),
            "\x1b[38;2;171;205;239m"
        );
        assert_eq!(
            encode(|w| ul_rgb_conpty(w, 0xAB, 0xCD, 0xEF)),
            "\x1b[58:2::171:205:239m"
        );
    }

    #[test]
    fn indexed_and_base_colors() {
        assert_eq!(encode(|w| fg_base(w, 1)), "\x1b[31m");
        assert_eq!(encode(|w| fg_bright(w, 1)), "\x1b[91m");
        assert_eq!(encode(|w| bg_base(w, 4)), "\x1b[44m");
        assert_eq!(encode(|w| bg_bright(w, 4)), "\x1b[104m");
        assert_eq!(encode(|w| fg_indexed(w, 200)), "\x1b[38:5:200m");
        assert_eq!(encode(|w| fg_indexed_legacy(w, 200)), "\x1b[38;5;200m");
    }

    #[test]
    fn osc_color_set_hex_padding() {
        // {x:0>2} hex: lowercase, zero-padded to two digits, byte written twice
        // for the X11 16-bit rgb spec.
        assert_eq!(
            encode(|w| osc10_set(w, 0xAB, 0xCD, 0xEF)),
            "\x1b]10;rgb:abab/cdcd/efef\x1b\\"
        );
        assert_eq!(
            encode(|w| osc11_set(w, 0x01, 0x00, 0x0f)),
            "\x1b]11;rgb:0101/0000/0f0f\x1b\\"
        );
        assert_eq!(encode(|w| osc4_query(w, 7)), "\x1b]4;7;?\x1b\\");
    }

    #[test]
    fn osc_text_encoders() {
        assert_eq!(
            encode(|w| osc8(w, "id=1", "http://x")),
            "\x1b]8;id=1;http://x\x1b\\"
        );
        assert_eq!(
            encode(|w| osc7(w, "file://host/path")),
            "\x1b]7;file://host/path\x1b\\"
        );
        assert_eq!(encode(|w| osc2_set_title(w, "hi")), "\x1b]2;hi\x1b\\");
    }

    #[test]
    fn text_sizing_encoders() {
        assert_eq!(encode(|w| explicit_width(w, 2, "x")), "\x1b]66;w=2;x\x1b\\");
        assert_eq!(
            encode(|w| scaled_text(w, 3, 2, "x")),
            "\x1b]66;s=3:w=2;x\x1b\\"
        );
        assert_eq!(
            encode(|w| scaled_text_with_fractions(w, 3, 2, 1, 4, VerticalAlignment::Center, "x")),
            "\x1b]66;s=3:w=2:n=1:d=4:v=2;x\x1b\\"
        );
    }

    #[test]
    fn multi_cursor_encoders() {
        assert_eq!(
            encode(|w| secondary_cursors_rgb(w, 1, 2, 3)),
            "\x1b[>40;2:1:2:3 q"
        );
        assert_eq!(
            encode(|w| show_secondary_cursor(w, 5, 9)),
            "\x1b[>29;2:5:9 q"
        );
    }

    #[test]
    fn kitty_graphics_preamble_id() {
        assert_eq!(encode(|w| kitty_graphics_preamble(w, 7)), "\x1b_Ga=p,i=7");
    }

    #[test]
    fn literal_consts() {
        assert_eq!(PRIMARY_DEVICE_ATTRS, "\x1b[c");
        assert_eq!(SMCUP, "\x1b[?1049h");
        assert_eq!(RMCUP, "\x1b[?1049l");
        assert_eq!(SGR_RESET, "\x1b[m");
        assert_eq!(KITTY_GRAPHICS_QUERY, "\x1b_Gi=1,a=q\x1b\\");
        assert_eq!(OSC8_CLEAR, "\x1b]8;;\x1b\\");
        assert_eq!(RESET_SECONDARY_CURSORS, "\x1b[>0;4 q");
    }
}
