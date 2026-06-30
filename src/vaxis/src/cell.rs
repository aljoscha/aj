//! The cell, style, and color value model: `Cell`, `Segment`, `Character`,
//! `Style`, `Color`, `Scale`, and friends.

use std::num::ParseIntError;

use thiserror::Error;

use crate::image::Placement;

/// Storage for a cell's grapheme cluster bytes.
///
/// Per the D1 grapheme-ownership decision (Option A) cells own their grapheme
/// inline through a small-string type, so any cluster that fits inline (in
/// practice all of them) costs no heap allocation. This alias localizes the
/// storage choice: switching to interning (Option B) means changing this type
/// and `Character`'s accessor, not every widget that reads a cell.
pub type Grapheme = compact_str::CompactString;

/// A single display cell: a grapheme plus the style, link, image, and scale
/// that decorate it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cell {
    pub char: Character,
    pub style: Style,
    pub link: Hyperlink,
    pub image: Option<Placement>,
    pub default: bool,
    /// True if this is the last cell printed in a row before a wrap. Vaxis uses
    /// it to decide whether to rely on the terminal's autowrap feature, which
    /// helps with primary-screen resizes.
    pub wrapped: bool,
    pub scale: Scale,
}

/// A contiguous run of text with a constant style. Used as print input.
///
/// NOTE: Upstream `Segment.text` is a borrowed `[]const u8`. We own a `String`
/// so the type carries no lifetime. Segments are short-lived print inputs, the
/// copy is negligible against laying out and rendering the text, and an owned
/// field keeps `Segment` trivially `Send + 'static`. A later print API that
/// wants to skip the copy can take `&str` arguments directly rather than
/// threading a lifetime through this type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub style: Style,
    pub link: Hyperlink,
}

/// A grapheme cluster together with its measured display width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Character {
    grapheme: Grapheme,
    /// Width should only be set when the application is sure the terminal will
    /// measure the same width, which `gwidth` guarantees. A width of 0 tells
    /// vaxis to measure the glyph at render time.
    pub width: u8,
}

impl Character {
    /// Builds a character from a grapheme cluster and its display width.
    pub fn new(grapheme: impl Into<Grapheme>, width: u8) -> Self {
        Self {
            grapheme: grapheme.into(),
            width,
        }
    }

    /// Returns the grapheme cluster's bytes.
    pub fn grapheme(&self) -> &str {
        self.grapheme.as_str()
    }

    /// Replaces the grapheme cluster, leaving the width untouched.
    pub fn set_grapheme(&mut self, grapheme: impl Into<Grapheme>) {
        self.grapheme = grapheme.into();
    }
}

impl Default for Character {
    fn default() -> Self {
        Self {
            grapheme: Grapheme::const_new(" "),
            width: 1,
        }
    }
}

/// The shape of the terminal cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Default,
    BlockBlink,
    Block,
    UnderlineBlink,
    Underline,
    BeamBlink,
    Beam,
}

/// An OSC 8 hyperlink attached to a cell.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hyperlink {
    pub uri: String,
    /// ie "id=app-1234"
    pub params: String,
}

/// Vertical alignment of a scaled glyph within its cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerticalAlignment {
    #[default]
    Top,
    Bottom,
    Center,
}

/// Glyph scaling for a cell.
///
/// NOTE: Upstream packs this into a `u13` (`u3` scale, `u4` numerator, `u4`
/// denominator, `u2` alignment) because the bit layout is what goes on the
/// wire. That packing is a wire-encoding concern reconstructed at encode time
/// in a later phase. Here we keep a plain value struct, so `scale`, `numerator`
/// and `denominator` are `u8` even though only the low 3 or 4 bits are valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scale {
    /// Whole-cell scale factor. The spec allows up to 15, vaxis limits it to 7.
    pub scale: u8,
    /// Fractional scale numerator. The spec allows up to 15, vaxis limits to 7.
    pub numerator: u8,
    /// Fractional scale denominator. The spec allows up to 15, vaxis limits
    /// to 7.
    pub denominator: u8,
    pub vertical_alignment: VerticalAlignment,
}

impl Default for Scale {
    fn default() -> Self {
        Self {
            scale: 1,
            numerator: 1,
            denominator: 1,
            vertical_alignment: VerticalAlignment::Top,
        }
    }
}

impl Scale {
    /// Equality used by the renderer's diff. Equivalent to the derived `==`,
    /// kept as a named method to mirror upstream's `Scale.eql`.
    pub fn eql(&self, other: &Scale) -> bool {
        self == other
    }
}

/// The underline style of a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Underline {
    #[default]
    Off,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// The full SGR style of a cell: colors plus the seven attribute bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub ul: Color,
    pub ul_style: Underline,

    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub blink: bool,
    pub reverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
}

impl Style {
    /// Equality used by the renderer's diff: the seven SGR attribute bits, the
    /// fg/bg/ul colors (via [`Color::eql`]), and the underline style.
    ///
    /// Equivalent to the derived `==`. We spell it out to mirror upstream's
    /// `Style.eql` and to document exactly what the diff compares.
    pub fn eql(&self, other: &Style) -> bool {
        self.bold == other.bold
            && self.dim == other.dim
            && self.italic == other.italic
            && self.blink == other.blink
            && self.reverse == other.reverse
            && self.invisible == other.invisible
            && self.strikethrough == other.strikethrough
            && self.fg.eql(&other.fg)
            && self.bg.eql(&other.bg)
            && self.ul.eql(&other.ul)
            && self.ul_style == other.ul_style
    }
}

/// A terminal color: the default, a 256-color palette index, or true color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Index(u8),
    Rgb([u8; 3]),
}

/// What a [`Color`] applies to when querying or reporting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Fg,
    Bg,
    Cursor,
    Index(u8),
}

/// A color reported back by the terminal in response to a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub kind: Kind,
    pub value: [u8; 3],
}

/// The terminal's reported color scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Dark,
    Light,
}

/// Error from parsing an XParseColor-style RGB specification.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ColorSpecError {
    #[error("invalid color spec, expected the form rgb:rrrr/gggg/bbbb")]
    InvalidSpec,
    #[error("invalid hex channel in color spec: {0}")]
    ParseInt(#[from] ParseIntError),
}

impl Color {
    /// Equality used by the renderer's diff. Equivalent to the derived `==`,
    /// kept as a named method to mirror upstream's `Color.eql`.
    pub fn eql(&self, other: &Color) -> bool {
        match (self, other) {
            (Color::Default, Color::Default) => true,
            (Color::Index(a), Color::Index(b)) => a == b,
            (Color::Rgb(a), Color::Rgb(b)) => a == b,
            _ => false,
        }
    }

    /// Builds an RGB color from the low 24 bits of `val`, laid out `0xRRGGBB`.
    pub fn rgb_from_uint(val: u32) -> Color {
        // Mask to 24 bits, then read the three low bytes big-endian: a 24-bit
        // value is `0x00RRGGBB`, so its big-endian bytes are `[0x00, R, G, B]`.
        let [_, r, g, b] = (val & 0x00ff_ffff).to_be_bytes();
        Color::Rgb([r, g, b])
    }

    /// Parses an XParseColor-style RGB specification into an RGB color. The
    /// spec has the form `rgb:rrrr/gggg/bbbb`.
    ///
    /// NOTE: We take the *low* two hex digits of each four-digit channel
    /// (`channel[2..]`), faithfully reproducing upstream. As upstream notes,
    /// the high two digits are generally equal to the low two for these specs,
    /// so the distinction does not matter in practice.
    pub fn rgb_from_spec(spec: &str) -> Result<Color, ColorSpecError> {
        let (prefix, spec_str) = spec.split_once(':').ok_or(ColorSpecError::InvalidSpec)?;
        if prefix != "rgb" {
            return Err(ColorSpecError::InvalidSpec);
        }

        let mut channels = spec_str.split('/');
        let r = parse_channel(channels.next())?;
        let g = parse_channel(channels.next())?;
        let b = parse_channel(channels.next())?;

        Ok(Color::Rgb([r, g, b]))
    }
}

/// Parses one `rrrr` channel of an RGB spec into a byte from its low two hex
/// digits. Returns [`ColorSpecError::InvalidSpec`] when the channel is missing
/// or not four bytes long.
fn parse_channel(raw: Option<&str>) -> Result<u8, ColorSpecError> {
    let raw = raw.ok_or(ColorSpecError::InvalidSpec)?;
    if raw.len() != 4 {
        return Err(ColorSpecError::InvalidSpec);
    }
    let low = raw.get(2..).ok_or(ColorSpecError::InvalidSpec)?;
    Ok(u8::from_str_radix(low, 16)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_from_spec() {
        let spec = "rgb:aaaa/bbbb/cccc";
        let actual = Color::rgb_from_spec(spec).unwrap();
        match actual {
            Color::Rgb(rgb) => {
                assert_eq!(rgb[0], 0xAA);
                assert_eq!(rgb[1], 0xBB);
                assert_eq!(rgb[2], 0xCC);
            }
            _ => panic!("expected an rgb color"),
        }
    }

    #[test]
    fn rgb_from_spec_rejects_bad_prefix() {
        assert_eq!(
            Color::rgb_from_spec("rgX:aaaa/bbbb/cccc"),
            Err(ColorSpecError::InvalidSpec)
        );
    }

    #[test]
    fn rgb_from_spec_rejects_wrong_channel_length() {
        assert_eq!(
            Color::rgb_from_spec("rgb:aa/bbbb/cccc"),
            Err(ColorSpecError::InvalidSpec)
        );
    }

    #[test]
    fn rgb_from_spec_rejects_non_hex() {
        assert!(matches!(
            Color::rgb_from_spec("rgb:zzzz/bbbb/cccc"),
            Err(ColorSpecError::ParseInt(_))
        ));
    }

    #[test]
    fn rgb_from_uint_extracts_channels() {
        assert_eq!(
            Color::rgb_from_uint(0x00aabbcc),
            Color::Rgb([0xaa, 0xbb, 0xcc])
        );
        assert_eq!(
            Color::rgb_from_uint(0x00112233),
            Color::Rgb([0x11, 0x22, 0x33])
        );
    }

    #[test]
    fn rgb_from_uint_masks_to_24_bits() {
        // Bits above the low 24 are dropped.
        assert_eq!(
            Color::rgb_from_uint(0xff_aabbcc),
            Color::Rgb([0xaa, 0xbb, 0xcc])
        );
    }

    #[test]
    fn color_equality() {
        assert!(Color::Default.eql(&Color::Default));
        assert!(Color::Index(1).eql(&Color::Index(1)));
        assert!(!Color::Index(1).eql(&Color::Index(2)));
        assert!(!Color::Default.eql(&Color::Index(0)));
        assert!(Color::Rgb([1, 2, 3]).eql(&Color::Rgb([1, 2, 3])));
        assert!(!Color::Rgb([1, 2, 3]).eql(&Color::Rgb([1, 2, 4])));
        assert!(!Color::Index(0).eql(&Color::Rgb([0, 0, 0])));
    }

    #[test]
    fn style_equality() {
        let base = Style::default();
        assert!(base.eql(&Style::default()));

        let bold = Style {
            bold: true,
            ..Style::default()
        };
        assert!(!base.eql(&bold));

        let fg = Style {
            fg: Color::Index(4),
            ..Style::default()
        };
        assert!(!base.eql(&fg));

        let ul = Style {
            ul_style: Underline::Curly,
            ..Style::default()
        };
        assert!(!base.eql(&ul));
    }

    #[test]
    fn scale_equality_and_default() {
        let default = Scale::default();
        assert_eq!(default.scale, 1);
        assert_eq!(default.numerator, 1);
        assert_eq!(default.denominator, 1);
        assert_eq!(default.vertical_alignment, VerticalAlignment::Top);
        assert!(default.eql(&Scale::default()));
        assert!(!default.eql(&Scale {
            scale: 2,
            ..Scale::default()
        }));
    }

    #[test]
    fn character_default_is_space() {
        let c = Character::default();
        assert_eq!(c.grapheme(), " ");
        assert_eq!(c.width, 1);
    }

    #[test]
    fn cell_default_matches_upstream() {
        let cell = Cell::default();
        assert_eq!(cell.char.grapheme(), " ");
        assert_eq!(cell.char.width, 1);
        assert_eq!(cell.style, Style::default());
        assert_eq!(cell.link, Hyperlink::default());
        assert!(cell.image.is_none());
        assert!(!cell.default);
        assert!(!cell.wrapped);
        assert_eq!(cell.scale, Scale::default());
    }
}
