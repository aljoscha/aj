//! [`MarkdownView`]: a width-aware transcript widget that renders markdown
//! through the shared [`aj_app::markdown`] renderer.
//!
//! Why a bespoke widget rather than [`RichText`](vaxis::vxfw::RichText). The
//! shared renderer pre-wraps markdown to a specific width, so the paint step
//! needs that width up front. `RichText` instead soft-wraps its spans at draw
//! time, which would re-wrap already-wrapped rows. So this widget calls
//! [`render_markdown`] at `ctx.max.width` inside its own `draw` and paints one
//! [`MarkdownRow`](aj_app::markdown::MarkdownRow) per surface row with wrapping
//! disabled.
//!
//! One view can carry several [`MarkdownSegment`]s, each with its own
//! `base_style`. That lets a single assistant entry mix plain text (the normal
//! text color) with a thinking block (its own color plus italic) while code,
//! headings, and links keep their palette roles. The `base_style` applies only
//! to `Text` spans. Every other [`SpanKind`] resolves through the theme-driven
//! [`MarkdownStyles`] mapper.

use aj_app::markdown::{RenderOpts, SpanKind, StyledSpan, SyntaxCategory, render_markdown};
use aj_app::theme::{Theme, ThemeColor};
use vaxis::cell::{Cell, Character, Color, Hyperlink, Style, Underline};
use vaxis::vxfw::{DrawContext, MaxSize, RichText, Size, Surface, TextSpan, Widget};

use crate::transcript::vaxis_color;

/// One block of source rendered by a [`MarkdownView`], with the styling that
/// governs its plain text.
pub(crate) struct MarkdownSegment {
    /// The markdown source for this block.
    pub(crate) text: String,
    /// Render knobs (hyperlink capability, default paragraph emphasis).
    pub(crate) opts: RenderOpts,
    /// Style applied to this segment's [`SpanKind::Text`] spans (assistant
    /// prose uses the normal text style, a thinking block its own
    /// color-plus-italic). Non-text roles ignore it and take their palette
    /// color from [`MarkdownStyles`].
    pub(crate) base_style: Style,
}

/// Resolves a markdown [`SpanKind`] (and any [`SyntaxCategory`]) to a
/// foreground vaxis [`Style`]. Built from the theme and rebuilt on a runtime
/// swap alongside [`TranscriptStyles`](crate::transcript::TranscriptStyles),
/// so a hot-reload re-tints markdown.
#[derive(Clone, Copy)]
pub(crate) struct MarkdownStyles {
    heading: Style,
    inline_code: Style,
    code_block: Style,
    code_block_border: Style,
    list_marker: Style,
    quote_border: Style,
    quote: Style,
    link_text: Style,
    link_url: Style,
    hr: Style,
    /// Foreground per syntax category, indexed by [`syntax_index`]. Consumed
    /// when a code-block span carries a category, which the shared renderer
    /// emits only while syntax highlighting is enabled (see
    /// [`RenderOpts::syntax_highlight`]).
    syntax: [Color; 9],
}

/// Stable slot for a [`SyntaxCategory`] in [`MarkdownStyles::syntax`]. Kept in
/// lockstep with the token order in [`MarkdownStyles::from_theme`].
fn syntax_index(cat: SyntaxCategory) -> usize {
    match cat {
        SyntaxCategory::Comment => 0,
        SyntaxCategory::Keyword => 1,
        SyntaxCategory::Function => 2,
        SyntaxCategory::Variable => 3,
        SyntaxCategory::String => 4,
        SyntaxCategory::Number => 5,
        SyntaxCategory::Type => 6,
        SyntaxCategory::Operator => 7,
        SyntaxCategory::Punctuation => 8,
    }
}

impl MarkdownStyles {
    pub(crate) fn from_theme(theme: &Theme) -> MarkdownStyles {
        let mode = theme.color_mode();
        let fg = |token: ThemeColor| Style {
            fg: vaxis_color(theme.fg_color(token), mode),
            ..Style::default()
        };
        let color = |token: ThemeColor| vaxis_color(theme.fg_color(token), mode);
        MarkdownStyles {
            heading: fg(ThemeColor::MdHeading),
            inline_code: fg(ThemeColor::MdCode),
            code_block: fg(ThemeColor::MdCodeBlock),
            code_block_border: fg(ThemeColor::MdCodeBlockBorder),
            list_marker: fg(ThemeColor::MdListBullet),
            quote_border: fg(ThemeColor::MdQuoteBorder),
            quote: fg(ThemeColor::MdQuote),
            link_text: fg(ThemeColor::MdLink),
            link_url: fg(ThemeColor::MdLinkUrl),
            hr: fg(ThemeColor::MdHr),
            // Order matches `syntax_index`.
            syntax: [
                color(ThemeColor::SyntaxComment),
                color(ThemeColor::SyntaxKeyword),
                color(ThemeColor::SyntaxFunction),
                color(ThemeColor::SyntaxVariable),
                color(ThemeColor::SyntaxString),
                color(ThemeColor::SyntaxNumber),
                color(ThemeColor::SyntaxType),
                color(ThemeColor::SyntaxOperator),
                color(ThemeColor::SyntaxPunctuation),
            ],
        }
    }

    /// Resolve `span`'s vaxis style. Plain text (and table cells, which have no
    /// dedicated token) start from the segment's `base`, every other role from
    /// its palette color. A syntax category overrides the foreground, then the
    /// span's emphasis bits are OR-ed on top so nested `**_x_**` composes.
    fn resolve(&self, span: &StyledSpan, base: Style) -> Style {
        let mut style = match span.kind {
            SpanKind::Text | SpanKind::TableBorder | SpanKind::TableCell => base,
            SpanKind::Heading(_) => self.heading,
            SpanKind::InlineCode => self.inline_code,
            SpanKind::CodeBlock => self.code_block,
            SpanKind::CodeBlockBorder => self.code_block_border,
            SpanKind::ListMarker => self.list_marker,
            SpanKind::QuoteBorder => self.quote_border,
            SpanKind::Quote => self.quote,
            SpanKind::LinkText => self.link_text,
            SpanKind::LinkUrl => self.link_url,
            SpanKind::Hr => self.hr,
        };
        if let Some(cat) = span.syntax {
            style.fg = self.syntax[syntax_index(cat)];
        }
        let e = span.emphasis;
        style.bold |= e.bold;
        style.italic |= e.italic;
        style.strikethrough |= e.strikethrough;
        if e.underline {
            style.ul_style = Underline::Single;
        }
        style
    }
}

/// Laid-out markdown rows for a specific width. The parse-and-wrap is the
/// expensive part, so we memoize it and reuse the result while the width holds.
struct RowCache {
    width: u16,
    /// One entry per surface row, in paint order. Blank rows (segment
    /// separators) are empty span vectors.
    rows: Vec<Vec<TextSpan>>,
}

/// A markdown transcript entry: optional plain leading rows (a compaction
/// header) above one or more markdown [`MarkdownSegment`]s.
pub(crate) struct MarkdownView {
    segments: Vec<MarkdownSegment>,
    /// Plain chrome painted above the markdown (the compaction header). Empty
    /// for a normal assistant entry. Rendered through the wrap engine each
    /// draw rather than the markdown parser, so a leading inset space and any
    /// markdown-significant characters in the header survive verbatim.
    leading: Vec<TextSpan>,
    styles: MarkdownStyles,
    /// Memoized rows keyed on width. The segment texts are fixed for a view's
    /// lifetime, so width alone identifies the cache. Streaming rebuilds the
    /// view with new text (a fresh, empty cache), which is the "text changed"
    /// invalidation. Virtualization bounds live views to the visible entries.
    cache: Option<RowCache>,
}

impl MarkdownView {
    pub(crate) fn new(
        segments: Vec<MarkdownSegment>,
        leading: Vec<TextSpan>,
        styles: MarkdownStyles,
    ) -> MarkdownView {
        MarkdownView {
            segments,
            leading,
            styles,
            cache: None,
        }
    }

    /// Ensure [`RowCache`] holds the laid-out rows for `width`, re-rendering
    /// only when the width changed.
    fn ensure_rows(&mut self, width: u16) {
        if self.cache.as_ref().is_some_and(|c| c.width == width) {
            return;
        }
        let rows = segment_rows(&self.segments, &self.styles, width);
        self.cache = Some(RowCache { width, rows });
    }

    /// Lay the leading header out through the wrap engine at `width`. `None`
    /// when there is no leading chrome.
    fn draw_leading(&self, ctx: &DrawContext, width: u16) -> Option<Surface> {
        if self.leading.is_empty() {
            return None;
        }
        let inner = ctx.with_constraints(
            Size {
                width: 0,
                height: 0,
            },
            MaxSize {
                width: Some(width),
                height: None,
            },
        );
        let mut rich = RichText::new(self.leading.clone());
        rich.softwrap = true;
        Some(rich.draw(&inner))
    }
}

impl Widget for MarkdownView {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let width = ctx.max.width.unwrap_or(ctx.min.width);
        if width == 0 {
            return Surface::with_size(Size {
                width: 0,
                height: 0,
            });
        }

        let leading = self.draw_leading(ctx, width);
        let leading_h = leading.as_ref().map_or(0, |s| s.size.height);

        self.ensure_rows(width);
        let rows = &self
            .cache
            .as_ref()
            .expect("ensure_rows populated the cache")
            .rows;
        let md_h = u16::try_from(rows.len()).unwrap_or(u16::MAX);

        // One blank row separates a leading header from the markdown below it,
        // then a trailing blank spacer keeps consecutive entries from
        // colliding (the transcript convention, see `entry_spans`).
        let gap = u16::from(leading_h > 0 && md_h > 0);
        let total = leading_h + gap + md_h + 1;
        let mut surface = Surface::with_size(Size {
            width,
            height: total,
        });

        if let Some(ls) = &leading {
            blit(&mut surface, ls);
        }
        let md_start = leading_h + gap;
        for (i, row) in rows.iter().enumerate() {
            let r = md_start + u16::try_from(i).unwrap_or(0);
            paint_row(&mut surface, r, row, ctx);
        }
        surface
    }
}

/// Lay `segments` out to pre-wrapped vaxis span rows at `width`, one row per
/// visual line, with a single blank row between consecutive non-empty
/// segments. The shared renderer pre-wraps to `width`, so callers paint these
/// rows with wrapping disabled.
fn segment_rows(
    segments: &[MarkdownSegment],
    styles: &MarkdownStyles,
    width: u16,
) -> Vec<Vec<TextSpan>> {
    let mut rows: Vec<Vec<TextSpan>> = Vec::new();
    for seg in segments {
        let seg_rows = render_markdown(&seg.text, usize::from(width), &seg.opts);
        if seg_rows.is_empty() {
            continue;
        }
        // One blank row between consecutive segments, matching the block
        // separation the plain-text renderer used before.
        if !rows.is_empty() {
            rows.push(Vec::new());
        }
        for md_row in &seg_rows {
            rows.push(to_vaxis_row(md_row, seg, styles));
        }
    }
    rows
}

/// Render `segments` to a surface at `width`, one markdown row per surface
/// row. Unlike [`MarkdownView`] this adds no leading chrome and no trailing
/// spacer, so a caller can composite the rows into its own layout (the
/// sub-agent box paints them under its box tint). Rows are pre-wrapped by the
/// shared renderer, so painting disables further wrapping.
///
/// Expects `width >= 1`. A zero width yields a zero-width surface (the shared
/// renderer clamps its wrap width to at least one, but the painted cells then
/// clip away), which is not a useful render.
pub(crate) fn draw_markdown_segments(
    ctx: &DrawContext,
    segments: &[MarkdownSegment],
    styles: &MarkdownStyles,
    width: u16,
) -> Surface {
    let rows = segment_rows(segments, styles, width);
    let height = u16::try_from(rows.len()).unwrap_or(u16::MAX);
    let mut surface = Surface::with_size(Size { width, height });
    for (i, row) in rows.iter().enumerate() {
        paint_row(&mut surface, u16::try_from(i).unwrap_or(0), row, ctx);
    }
    surface
}

/// Convert one shared [`MarkdownRow`](aj_app::markdown::MarkdownRow) into a
/// vaxis span row, resolving each span's style against `seg`'s base and mapper.
///
/// The OSC-8 target rides along only when the segment allows hyperlinks. With
/// hyperlinks off the renderer already appended a visible ` (url)` span, so
/// emitting the escape too would be redundant.
fn to_vaxis_row(
    row: &[StyledSpan],
    seg: &MarkdownSegment,
    styles: &MarkdownStyles,
) -> Vec<TextSpan> {
    row.iter()
        .map(|span| {
            let style = styles.resolve(span, seg.base_style);
            let link = match (seg.opts.hyperlinks, &span.link) {
                (true, Some(uri)) => Hyperlink {
                    uri: uri.clone(),
                    params: String::new(),
                },
                _ => Hyperlink::default(),
            };
            TextSpan {
                text: span.text.clone(),
                style,
                link,
            }
        })
        .collect()
}

/// Paint one pre-wrapped span row into `surface` at `row`, left to right. The
/// row already fits `width`, so there is no wrapping or clipping here.
fn paint_row(surface: &mut Surface, row: u16, spans: &[TextSpan], ctx: &DrawContext) {
    let mut col: u16 = 0;
    for span in spans {
        for item in ctx.grapheme_iterator(&span.text) {
            let grapheme = item.bytes(&span.text);
            let width = u8::try_from(ctx.string_width(grapheme)).unwrap_or(1);
            surface.write_cell(
                col,
                row,
                Cell {
                    char: Character::new(grapheme, width),
                    style: span.style,
                    link: span.link.clone(),
                    ..Cell::default()
                },
            );
            col = col.saturating_add(u16::from(width));
        }
    }
}

/// Copy `src`'s cells into `dst` at the top-left, clipping anything past
/// `dst`'s bounds.
fn blit(dst: &mut Surface, src: &Surface) {
    for r in 0..src.size.height {
        for c in 0..src.size.width {
            dst.write_cell(c, r, src.read_cell(c, r));
        }
    }
}

#[cfg(test)]
mod tests {
    use aj_app::markdown::Emphasis;
    use aj_app::theme::{ColorMode, Theme};

    use super::*;

    fn theme() -> Theme {
        Theme::bundled_dark_with_mode(ColorMode::Truecolor)
    }

    fn styles() -> MarkdownStyles {
        MarkdownStyles::from_theme(&theme())
    }

    fn fg(token: ThemeColor) -> Color {
        let t = theme();
        vaxis_color(t.fg_color(token), t.color_mode())
    }

    fn text_style() -> Style {
        Style {
            fg: fg(ThemeColor::Text),
            ..Style::default()
        }
    }

    fn thinking_style() -> Style {
        Style {
            italic: true,
            fg: fg(ThemeColor::ThinkingText),
            ..Style::default()
        }
    }

    fn opts(hyperlinks: bool, emphasis: Emphasis) -> RenderOpts {
        RenderOpts {
            hyperlinks,
            default_emphasis: emphasis,
            syntax_highlight: false,
        }
    }

    fn draw(view: &mut MarkdownView, width: u16) -> Surface {
        view.draw(&crate::test_support::draw_ctx(width, None))
    }

    fn rows(surface: &Surface) -> Vec<String> {
        crate::test_support::rows(surface)
    }

    /// First composited cell whose grapheme equals `needle`.
    fn cell_with(surface: &Surface, needle: &str) -> Cell {
        crate::test_support::flatten(surface)
            .into_iter()
            .flatten()
            .find(|c| c.char.grapheme() == needle)
            .unwrap_or_else(|| panic!("no cell rendering {needle:?}"))
    }

    /// A heading, inline code, and link each resolve to their palette role,
    /// and the link cell carries the OSC-8 target.
    #[test]
    fn markdown_roles_map_to_theme_styles() {
        let seg = MarkdownSegment {
            text: "# H\n\nx `k` y [L](https://e.com).".to_string(),
            opts: opts(true, Emphasis::default()),
            base_style: text_style(),
        };
        let mut view = MarkdownView::new(vec![seg], Vec::new(), styles());
        let surface = draw(&mut view, 40);

        let heading = cell_with(&surface, "H");
        assert_eq!(heading.style.fg, fg(ThemeColor::MdHeading), "heading color");
        assert!(heading.style.bold, "heading is bold");

        let code = cell_with(&surface, "k");
        assert_eq!(code.style.fg, fg(ThemeColor::MdCode), "inline code color");

        let link = cell_with(&surface, "L");
        assert_eq!(link.style.fg, fg(ThemeColor::MdLink), "link text color");
        assert_eq!(link.style.ul_style, Underline::Single, "link underlined");
        assert_eq!(link.link.uri, "https://e.com", "OSC-8 target set");

        // Plain body text keeps the segment's base style.
        let body = cell_with(&surface, "x");
        assert_eq!(body.style.fg, fg(ThemeColor::Text), "body text color");
    }

    /// With `syntax_highlight` on, a recognized-language code block colors its
    /// tokens by category (a `let` keyword takes the keyword syntax color).
    /// With it off, the same token stays the plain code-block color.
    #[test]
    fn syntax_highlight_toggles_code_token_color() {
        let src = "```rust\nlet y = 2;\n```".to_string();

        let mut on = MarkdownView::new(
            vec![MarkdownSegment {
                text: src.clone(),
                opts: RenderOpts {
                    hyperlinks: false,
                    default_emphasis: Emphasis::default(),
                    syntax_highlight: true,
                },
                base_style: text_style(),
            }],
            Vec::new(),
            styles(),
        );
        let on_surface = draw(&mut on, 40);
        assert_eq!(
            cell_with(&on_surface, "l").style.fg,
            fg(ThemeColor::SyntaxKeyword),
            "`let` keyword takes the keyword syntax color when highlighting is on",
        );

        let mut off = MarkdownView::new(
            vec![MarkdownSegment {
                text: src,
                opts: RenderOpts {
                    hyperlinks: false,
                    default_emphasis: Emphasis::default(),
                    syntax_highlight: false,
                },
                base_style: text_style(),
            }],
            Vec::new(),
            styles(),
        );
        let off_surface = draw(&mut off, 40);
        assert_eq!(
            cell_with(&off_surface, "l").style.fg,
            fg(ThemeColor::MdCodeBlock),
            "same token stays the plain code-block color when highlighting is off",
        );
    }

    /// With hyperlinks off the visible ` (url)` fallback appears and no OSC-8
    /// escape rides on the link text.
    #[test]
    fn hyperlinks_off_shows_url_and_sets_no_escape() {
        let seg = MarkdownSegment {
            text: "[L](https://e.com)".to_string(),
            opts: opts(false, Emphasis::default()),
            base_style: text_style(),
        };
        let mut view = MarkdownView::new(vec![seg], Vec::new(), styles());
        let surface = draw(&mut view, 40);
        let joined = rows(&surface).join("\n");
        assert!(
            joined.contains("(https://e.com)"),
            "url fallback: {joined:?}"
        );
        assert!(cell_with(&surface, "L").link.uri.is_empty(), "no OSC-8");
    }

    /// A thinking segment paints its plain text in the thinking color and
    /// italic, driven by the base style and the italic default emphasis.
    #[test]
    fn thinking_segment_is_colored_and_italic() {
        let seg = MarkdownSegment {
            text: "pondered".to_string(),
            opts: opts(
                true,
                Emphasis {
                    italic: true,
                    ..Emphasis::default()
                },
            ),
            base_style: thinking_style(),
        };
        let mut view = MarkdownView::new(vec![seg], Vec::new(), styles());
        let surface = draw(&mut view, 40);
        let cell = cell_with(&surface, "p");
        assert_eq!(
            cell.style.fg,
            fg(ThemeColor::ThinkingText),
            "thinking color"
        );
        assert!(cell.style.italic, "thinking text is italic");
    }

    /// Two segments stack with one blank row between them and a trailing blank
    /// spacer below the last.
    #[test]
    fn segments_stack_with_separator_and_trailing_spacer() {
        let seg = |text: &str| MarkdownSegment {
            text: text.to_string(),
            opts: opts(true, Emphasis::default()),
            base_style: text_style(),
        };
        let mut view = MarkdownView::new(vec![seg("AAA"), seg("BBB")], Vec::new(), styles());
        let surface = draw(&mut view, 40);
        assert_eq!(rows(&surface), vec!["AAA", "", "BBB", ""]);
    }

    /// A leading header renders above the markdown, separated by one blank
    /// row, and the whole entry ends in the spacer.
    #[test]
    fn leading_header_sits_above_the_markdown() {
        let header = TextSpan {
            text: " Header line".to_string(),
            style: text_style(),
            ..TextSpan::default()
        };
        let seg = MarkdownSegment {
            text: "body".to_string(),
            opts: opts(true, Emphasis::default()),
            base_style: text_style(),
        };
        let mut view = MarkdownView::new(vec![seg], vec![header], styles());
        let surface = draw(&mut view, 40);
        assert_eq!(rows(&surface), vec![" Header line", "", "body", ""]);
    }

    /// A zero width yields an empty surface rather than panicking.
    #[test]
    fn zero_width_draws_empty() {
        let seg = MarkdownSegment {
            text: "anything".to_string(),
            opts: opts(true, Emphasis::default()),
            base_style: text_style(),
        };
        let mut view = MarkdownView::new(vec![seg], Vec::new(), styles());
        let surface = view.draw(&crate::test_support::draw_ctx(0, None));
        assert_eq!(surface.size, Size::default());
    }

    /// Re-drawing at the same width reuses the cache. A new width re-renders.
    #[test]
    fn row_cache_keys_on_width() {
        let seg = MarkdownSegment {
            text: "one two three four five".to_string(),
            opts: opts(true, Emphasis::default()),
            base_style: text_style(),
        };
        let mut view = MarkdownView::new(vec![seg], Vec::new(), styles());
        let _ = draw(&mut view, 40);
        assert_eq!(view.cache.as_ref().map(|c| c.width), Some(40));
        let _ = draw(&mut view, 10);
        assert_eq!(view.cache.as_ref().map(|c| c.width), Some(10));
    }
}
