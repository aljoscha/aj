//! Backend-agnostic markdown renderer.
//!
//! Turns markdown text into a flat list of pre-wrapped visual rows, where
//! each row is a sequence of [`StyledSpan`]s tagged with a semantic
//! [`SpanKind`] and a set of [`Emphasis`] bits. Styling (the concrete
//! colors and SGR attributes) is left to the consuming frontend, which
//! keeps this crate free of any TUI backend while both frontends render
//! the same markdown shape. This mirrors the neutral-output pattern used
//! by [`crate::diff`].
//!
//! Why a flat role-tagged model. A terminal frontend ultimately paints
//! rows of styled cells, so the shared layer does all the layout work
//! (parsing, inline styling, word wrap) and hands the backend rows it only
//! has to color. The span model carries the semantic role rather than a
//! concrete style, so a backend maps `SpanKind::Heading` onto its own
//! palette. There is no cross-row state to reconstruct (unlike an ANSI
//! byte stream), because every span carries its full style explicitly.
//!
//! Scope. This layer renders prose (headings, paragraphs, inline
//! emphasis, inline code, links), fenced/indented code blocks with
//! syntect-driven syntax highlighting, lists (ordered and unordered,
//! nested), blockquotes, top-level horizontal rules, and GFM tables as
//! their native box-drawing column layout (see [`render_table`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::OnceLock;

use pulldown_cmark::{
    Alignment as PdAlignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use syntect::easy::ScopeRegionIterator;
use syntect::highlighting::ScopeSelectors;
use syntect::parsing::{MatchPower, ParseState, Scope, ScopeStack, SyntaxSet};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// Neutral styled-row model
// ---------------------------------------------------------------------------

/// Semantic role of a rendered span. Frontends map each kind onto their
/// own palette (heading color, inline-code background, link color, and so
/// on). The full set is fixed here so backends can match exhaustively.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanKind {
    /// Plain body text.
    Text,
    /// Heading text, carrying the level (1..=6) so a backend can size or
    /// color per level.
    Heading(u8),
    /// An inline `` `code` `` span.
    InlineCode,
    /// A line of a fenced or indented code block.
    CodeBlock,
    /// The ```` ``` ```` fence rows that frame a code block.
    CodeBlockBorder,
    /// A list bullet or ordinal marker (`- `, `1. `).
    ListMarker,
    /// The `│ ` border prefixing a blockquote line.
    QuoteBorder,
    /// Text inside a blockquote.
    Quote,
    /// The visible text of a link.
    LinkText,
    /// The trailing ` (url)` a link appends when hyperlinks are off.
    LinkUrl,
    /// A horizontal rule.
    Hr,
    /// A table's box-drawing border/junction glyphs.
    TableBorder,
    /// The content of a table cell.
    TableCell,
}

/// Syntax-highlighting category assigned to a code-block span. Populated by
/// the syntect-based classifier ([`highlight_code`]) for fenced/indented
/// code, and `None` for tokens that match no category. Frontends map each
/// category onto their own palette color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyntaxCategory {
    Comment,
    Keyword,
    Function,
    Variable,
    String,
    Number,
    Type,
    Operator,
    Punctuation,
}

/// Text-decoration bits carried by a span. Composed by nesting: an inner
/// `**_x_**` accumulates both `bold` and `italic`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Emphasis {
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
}

impl Emphasis {
    fn with_bold(mut self) -> Self {
        self.bold = true;
        self
    }

    fn with_italic(mut self) -> Self {
        self.italic = true;
        self
    }

    fn with_strikethrough(mut self) -> Self {
        self.strikethrough = true;
        self
    }

    fn with_underline(mut self) -> Self {
        self.underline = true;
        self
    }
}

/// One styled run of text within a row. `text` never contains a newline:
/// row boundaries are the only line structure, and wrapping has already
/// split logical lines into rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledSpan {
    /// The visible text. Never contains `'\n'`.
    pub text: String,
    /// Semantic role, chosen by the backend's palette.
    pub kind: SpanKind,
    /// Text-decoration bits to apply on top of the role's base style.
    pub emphasis: Emphasis,
    /// Set only on code-block spans that syntax classification matched to
    /// a category. `None` on every other span, and on code-block tokens
    /// that matched no category (they render in the default foreground).
    pub syntax: Option<SyntaxCategory>,
    /// OSC-8 hyperlink target for link spans. The backend decides whether
    /// to emit the escape based on its own capability. See
    /// [`RenderOpts::hyperlinks`].
    pub link: Option<String>,
}

impl StyledSpan {
    /// A span of `text` with the given `kind`, no emphasis, no syntax
    /// category, and no link target.
    fn plain(text: impl Into<String>, kind: SpanKind) -> Self {
        Self {
            text: text.into(),
            kind,
            emphasis: Emphasis::default(),
            syntax: None,
            link: None,
        }
    }

    /// Same span with `text` replaced. Used when splitting a span across a
    /// row boundary while preserving its styling.
    fn with_text(&self, text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: self.kind,
            emphasis: self.emphasis,
            syntax: self.syntax,
            link: self.link.clone(),
        }
    }

    /// True if this span carries the same styling as `other` (everything
    /// except `text`). Adjacent spans that match are merged during row
    /// finalization.
    fn same_style(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.emphasis == other.emphasis
            && self.syntax == other.syntax
            && self.link == other.link
    }
}

/// One pre-wrapped visual row: the spans that paint a single terminal line.
pub type MarkdownRow = Vec<StyledSpan>;

/// Knobs that affect the produced rows.
#[derive(Clone, Debug, Default)]
pub struct RenderOpts {
    /// Whether the backend can emit OSC-8 hyperlinks. When `true`, a link
    /// renders as visible text only (the backend wraps it in the escape
    /// using [`StyledSpan::link`]). When `false`, a non-autolink link
    /// appends a visible ` (url)` [`SpanKind::LinkUrl`] span, which the
    /// wrap accounts for. Only the width-affecting text decision lives
    /// here. The escape bytes are the backend's job.
    pub hyperlinks: bool,
    /// Emphasis applied to plain paragraph text. Lets a caller render, for
    /// example, "thinking" prose in italic. Not applied to headings, code
    /// blocks, rules, or blockquote contents (a quote's own italic is the
    /// only decoration on quoted prose), matching the paragraph-only scope
    /// of the styling it stands in for.
    pub default_emphasis: Emphasis,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Render markdown `text` into pre-wrapped visual rows at `width` columns.
///
/// The pipeline is: expand tabs, parse into the block AST, render each
/// block into logical lines of styled spans, word-wrap each logical line
/// to `width`, and emit one blank row between blocks. Trailing blank rows
/// are trimmed so a document never ends in dead space. Whitespace-only
/// input renders zero rows.
///
/// `width` is clamped to at least 1 so the wrap always makes progress on a
/// degenerate width.
pub fn render_markdown(text: &str, width: usize, opts: &RenderOpts) -> Vec<MarkdownRow> {
    if text.trim().is_empty() {
        return Vec::new();
    }

    let width = width.max(1);
    let normalized = expand_tabs(text);
    let blocks = parse_markdown(&normalized);

    let mut rows: Vec<MarkdownRow> = Vec::new();
    for block in &blocks {
        for logical_line in render_block(block, width, opts) {
            rows.extend(wrap_spans(&logical_line, width));
        }
        // One blank spacer after every block. Consecutive blocks are then
        // separated by exactly one blank row, and the trailing-trim below
        // drops the spacer after the final block.
        rows.push(Vec::new());
    }

    while rows.last().is_some_and(is_blank_row) {
        rows.pop();
    }

    rows
}

/// True when a row has no visible non-whitespace content. An empty row and
/// a row of whitespace-only spans both count.
fn is_blank_row(row: &MarkdownRow) -> bool {
    row.iter().all(|s| s.text.trim().is_empty())
}

// ---------------------------------------------------------------------------
// Block rendering
// ---------------------------------------------------------------------------

/// Indent prefix folded into every code-block body line. Matches the
/// default the styled renderer uses so a code block sits inset from the
/// surrounding prose.
const CODE_BLOCK_INDENT: &str = "  ";

/// Cap on a top-level horizontal rule's width so it stays readable on very
/// wide terminals.
const HR_MAX_WIDTH: usize = 80;

/// Render a block into logical lines of styled spans (pre-wrap). Each
/// returned line may still contain embedded `'\n'` from soft/hard breaks;
/// [`wrap_spans`] splits those into rows.
fn render_block(block: &Block, width: usize, opts: &RenderOpts) -> Vec<Vec<StyledSpan>> {
    match block {
        Block::Heading(level, inlines) => {
            // Heading text is bold, and H1 is additionally underlined. This
            // base emphasis is the starting point for the inline walk. The
            // default paragraph emphasis intentionally does not apply to
            // headings.
            let base = Emphasis {
                bold: true,
                underline: *level == 1,
                ..Default::default()
            };
            let kind = SpanKind::Heading(*level);
            let mut spans = Vec::new();
            // H3+ get a visible `### ` prefix, styled the same as the
            // heading body. H1/H2 render the styled text with no marker.
            if *level >= 3 {
                let prefix = format!("{} ", "#".repeat(usize::from(*level)));
                spans.push(StyledSpan {
                    text: prefix,
                    kind,
                    emphasis: base,
                    syntax: None,
                    link: None,
                });
            }
            render_inlines(inlines, kind, base, None, opts, &mut spans);
            vec![spans]
        }
        Block::Paragraph(inlines) => {
            let mut spans = Vec::new();
            render_inlines(
                inlines,
                SpanKind::Text,
                opts.default_emphasis,
                None,
                opts,
                &mut spans,
            );
            vec![spans]
        }
        Block::CodeBlock(lang, code) => {
            let mut lines: Vec<Vec<StyledSpan>> = Vec::new();
            let fence_open = match lang {
                Some(l) => format!("```{l}"),
                None => "```".to_string(),
            };
            lines.push(vec![StyledSpan::plain(
                fence_open,
                SpanKind::CodeBlockBorder,
            )]);
            // Classify each source line into (category, text) runs. The
            // indent is folded into a leading no-syntax `CodeBlock` span so
            // a backend that backgrounds code covers the inset too. Long
            // lines are word-wrapped by the shared wrap (via the outer
            // render loop), which preserves each run's syntax across the
            // break because a split span copies the `syntax` field. This
            // matches the styled renderer, which wraps every emitted line
            // uniformly.
            let highlighted = highlight_code(code, lang.as_deref());
            for line_runs in highlighted.iter() {
                let mut spans = vec![StyledSpan::plain(CODE_BLOCK_INDENT, SpanKind::CodeBlock)];
                for (category, text) in line_runs {
                    spans.push(StyledSpan {
                        text: text.clone(),
                        kind: SpanKind::CodeBlock,
                        emphasis: Emphasis::default(),
                        syntax: *category,
                        link: None,
                    });
                }
                lines.push(spans);
            }
            lines.push(vec![StyledSpan::plain("```", SpanKind::CodeBlockBorder)]);
            lines
        }
        Block::HorizontalRule => {
            let rule = "─".repeat(width.min(HR_MAX_WIDTH));
            vec![vec![StyledSpan::plain(rule, SpanKind::Hr)]]
        }
        Block::UnorderedList(items) => render_list(items, false, 0, width, opts),
        Block::OrderedList(items) => render_list(items, true, 0, width, opts),
        Block::Blockquote(sub_blocks) => render_blockquote(sub_blocks, width, opts),
        Block::Table {
            headers,
            alignments,
            rows,
            raw,
        } => render_table(headers, alignments, rows, raw, width, opts),
    }
}

/// Render a list into logical lines of styled spans (pre-wrap).
///
/// One logical line per item: a [`SpanKind::ListMarker`] span carrying the
/// depth indent plus the bullet, followed by the item's inline content.
/// The indent (`"  "` per level) is folded into the marker span because
/// the leading spaces carry no visible color. `depth` drives that indent
/// and grows by one per nesting level.
///
/// We stay width-agnostic and emit each item as a single logical line. The
/// outer wrap in [`render_markdown`] then breaks a long item, and because
/// that wrap carries no hang indent the continuation rows land flush-left
/// at column 0 rather than under the bullet. This is the same
/// continuation-flush-left shape the styled renderer produces by wrapping
/// list lines uniformly.
///
/// Ordered items number from `item.number` when the parser captured a
/// source marker, falling back to the positional index. Keying off the
/// captured marker keeps numbering stable across a list that an
/// intervening block (e.g. a code fence) split into separate lists.
///
/// Nested sub-blocks render at `depth + 1`: nested lists recurse here,
/// every other block goes through [`render_block`]. In this neutral model
/// [`render_block`] never appends a trailing blank spacer (the outer
/// [`render_markdown`] loop owns inter-block spacing), so sub-block lines
/// are extended directly with no spacer to drop, keeping list items tight.
fn render_list(
    items: &[ListItem],
    ordered: bool,
    depth: usize,
    width: usize,
    opts: &RenderOpts,
) -> Vec<Vec<StyledSpan>> {
    let indent = "  ".repeat(depth);
    let mut lines = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let bullet = if ordered {
            let n = item
                .number
                .unwrap_or_else(|| u32::try_from(idx + 1).unwrap_or(u32::MAX));
            format!("{n}. ")
        } else {
            "- ".to_string()
        };
        let mut spans = vec![StyledSpan::plain(
            format!("{indent}{bullet}"),
            SpanKind::ListMarker,
        )];
        // `default_emphasis` is paragraph-level, so list item content does
        // not inherit it (a list inside an italic thinking block keeps its
        // items upright), matching how a blockquote clears it.
        render_inlines(
            &item.content,
            SpanKind::Text,
            Emphasis::default(),
            None,
            opts,
            &mut spans,
        );
        lines.push(spans);
        for sub in &item.sub_blocks {
            let sub_lines = match sub {
                Block::UnorderedList(sub_items) => {
                    render_list(sub_items, false, depth + 1, width, opts)
                }
                Block::OrderedList(sub_items) => {
                    render_list(sub_items, true, depth + 1, width, opts)
                }
                other => render_block(other, width, opts),
            };
            lines.extend(sub_lines);
        }
    }
    lines
}

/// Render a blockquote into logical lines of styled spans (pre-wrap).
///
/// The quote body is a full sub-document: each sub-block renders as its
/// own native block ([`render_block`]) at `inner_width`, so a nested
/// paragraph, list, code block, heading, or even a nested quote keeps its
/// own structure. `inner_width` reserves the two columns the `"│ "` border
/// occupies.
///
/// Neutral-model retag. The styled renderer needs an escape-reopen hack:
/// it splices the quote's opening SGR codes back in after every full reset
/// so downstream content (syntect code lines end in a reset) re-enters the
/// quote style. The neutral model carries each span's full style
/// explicitly, so there is nothing to reopen. We instead prepend a
/// [`SpanKind::QuoteBorder`] span to every produced row and retag that
/// row's [`SpanKind::Text`] spans to [`SpanKind::Quote`] with italic
/// emphasis. Other kinds (heading, inline code, code block, list marker,
/// link) are left untouched, so nested code and headings keep their own
/// role and only the plain text runs pick up the quote color and italic.
///
/// We wrap each sub-block at `inner_width` before bordering because the
/// border must lead every visual row. Sub-blocks are separated by one
/// blank spacer row, matching the one-blank-per-block shape the outer loop
/// gives top-level blocks, and trailing blanks are dropped so the quote
/// does not end in a bare border. A surviving mid-quote blank row still
/// gets the border prefix, rendering as `│ ` rather than an empty line.
fn render_blockquote(
    sub_blocks: &[Block],
    width: usize,
    opts: &RenderOpts,
) -> Vec<Vec<StyledSpan>> {
    // The `"│ "` border is two columns. `.max(1)` keeps a usable inner
    // width on a degenerate outer width so the recursion still makes
    // progress. For any non-degenerate width it is a no-op.
    let inner_width = width.saturating_sub(2).max(1);

    // Clear `default_emphasis` inside the quote. It is a paragraph-level
    // knob (thinking prose renders italic), but the retag below already
    // makes every quoted text run italic, and the quote's italic is the
    // only decoration quoted prose should carry. Clearing it keeps a
    // future non-italic `default_emphasis` from leaking past the quote
    // boundary, matching the identity inline context the styled renderer
    // uses inside quotes. `hyperlinks` still applies.
    let inner_opts = RenderOpts {
        default_emphasis: Emphasis::default(),
        ..opts.clone()
    };

    let mut inner_rows: Vec<MarkdownRow> = Vec::new();
    for sub in sub_blocks {
        for logical_line in render_block(sub, inner_width, &inner_opts) {
            inner_rows.extend(wrap_spans(&logical_line, inner_width));
        }
        inner_rows.push(Vec::new());
    }
    while inner_rows.last().is_some_and(is_blank_row) {
        inner_rows.pop();
    }

    let mut rows: Vec<MarkdownRow> = Vec::with_capacity(inner_rows.len());
    for inner in inner_rows {
        let mut row = vec![StyledSpan::plain("│ ", SpanKind::QuoteBorder)];
        for mut span in inner {
            if span.kind == SpanKind::Text {
                span.kind = SpanKind::Quote;
                span.emphasis.italic = true;
            }
            row.push(span);
        }
        rows.push(row);
    }
    rows
}

// ---------------------------------------------------------------------------
// Table rendering
// ---------------------------------------------------------------------------

/// Upper bound on a column's minimum width. A column floor is the longest
/// unbreakable token it holds, clamped to this many columns so one overlong
/// token (a URL, hash, or identifier) can't pin the column to its full
/// width and starve its neighbours. Tokens past the cap are hard-broken by
/// the shared wrap.
const MAX_UNBROKEN_TOKEN_WIDTH: usize = 30;

/// Render a GFM table into logical rows of styled spans (pre-wrap).
///
/// Layout is box-drawing chrome around per-column cells: a top border, the
/// header row, a separator rule, the body rows (each pair split by a rule),
/// and a bottom border. Border and junction glyphs carry
/// [`SpanKind::TableBorder`] and cell text [`SpanKind::TableCell`], both of
/// which a frontend paints in the base text color, so the box reads as
/// plain chrome rather than styled content.
///
/// The produced rows are already sized to `width`, so the outer wrap pass
/// in [`render_markdown`] is a no-op on them, the same as it is on a
/// blockquote's bordered rows. We therefore emit no trailing blank spacer:
/// the outer loop owns the single inter-block blank, like every other
/// [`render_block`] arm.
fn render_table(
    headers: &[Vec<Inline>],
    alignments: &[Alignment],
    rows: &[Vec<Vec<Inline>>],
    raw: &str,
    width: usize,
    opts: &RenderOpts,
) -> Vec<Vec<StyledSpan>> {
    let n_cols = alignments.len();
    if n_cols == 0 {
        return vec![Vec::new()];
    }

    // Border overhead per column: its left border plus the two padding
    // columns around the cell (3 columns), plus one for the final right
    // border. Whatever is left is the budget the cells share.
    let chrome = 3 * n_cols + 1;
    let available_for_cells = width.saturating_sub(chrome);

    // Narrow fallback: when the budget can't fit even one column per cell, a
    // bordered table would render broken (a border wider than its content,
    // or zero-width cells). Fall back to the raw source as prose so no
    // content is dropped. We return it as one logical line and let the outer
    // wrap break it to width, exactly as a paragraph renders.
    if available_for_cells < n_cols {
        let mut spans = Vec::new();
        render_inlines(
            &[Inline::Text(raw.to_string())],
            SpanKind::Text,
            opts.default_emphasis,
            None,
            opts,
            &mut spans,
        );
        return vec![spans];
    }

    // Pre-render each cell to spans once so width measurement and the later
    // wrap share the same content. Cells are paragraph-level independent
    // (like list items), so they inherit no `default_emphasis` and their
    // plain text runs are tagged `TableCell`.
    let header_cells: Vec<Vec<StyledSpan>> = headers.iter().map(|c| render_cell(c, opts)).collect();
    let body_cells: Vec<Vec<Vec<StyledSpan>>> = rows
        .iter()
        .map(|r| r.iter().map(|c| render_cell(c, opts)).collect())
        .collect();

    // Per-column natural width (max visible cell width, uncapped) and
    // minimum width (longest unbreakable token, capped at
    // `MAX_UNBROKEN_TOKEN_WIDTH`, floored at 1). `natural` stays uncapped so
    // short content still gets its preferred width. The cap on `minimum`
    // keeps an overlong token from pinning the column and starving its
    // neighbours.
    let mut natural = vec![0usize; n_cols];
    let mut minimum = vec![1usize; n_cols];
    for col in 0..n_cols {
        if let Some(cell) = header_cells.get(col) {
            let text = cell_plain_text(cell);
            natural[col] = natural[col].max(display_width(&text));
            minimum[col] =
                minimum[col].max(longest_token_width(&text).min(MAX_UNBROKEN_TOKEN_WIDTH));
        }
        for row in &body_cells {
            if let Some(cell) = row.get(col) {
                let text = cell_plain_text(cell);
                natural[col] = natural[col].max(display_width(&text));
                minimum[col] =
                    minimum[col].max(longest_token_width(&text).min(MAX_UNBROKEN_TOKEN_WIDTH));
            }
        }
    }

    // Past the fallback gate `available_for_cells >= n_cols >= 1`, so the
    // distribution always sees a usable budget.
    let widths = distribute_column_widths(&natural, &minimum, available_for_cells);

    let separator = make_border_row(&widths, '├', '┼', '┤');
    let mut out: Vec<Vec<StyledSpan>> = Vec::new();
    out.push(make_border_row(&widths, '┌', '┬', '┐'));
    out.extend(render_table_row(&header_cells, &widths, alignments));
    out.push(separator.clone());
    for (idx, row) in body_cells.iter().enumerate() {
        // A rule between consecutive body rows matches the header separator,
        // so every row reads as its own boxed cell.
        if idx > 0 {
            out.push(separator.clone());
        }
        out.extend(render_table_row(row, &widths, alignments));
    }
    out.push(make_border_row(&widths, '└', '┴', '┘'));
    out
}

/// Render a table cell's inlines to spans. Plain text runs are tagged
/// [`SpanKind::TableCell`] and inherit no `default_emphasis`.
fn render_cell(inlines: &[Inline], opts: &RenderOpts) -> Vec<StyledSpan> {
    let mut spans = Vec::new();
    render_inlines(
        inlines,
        SpanKind::TableCell,
        Emphasis::default(),
        None,
        opts,
        &mut spans,
    );
    spans
}

/// Visible text of a pre-rendered cell, used only for column-width math.
fn cell_plain_text(cell: &[StyledSpan]) -> String {
    cell.iter().map(|s| s.text.as_str()).collect()
}

/// Width of the longest whitespace-delimited token in `text`. Callers cap
/// this at [`MAX_UNBROKEN_TOKEN_WIDTH`] when deriving a column floor, so up
/// to the cap a column stays wide enough to hold its longest token without
/// mid-token wrapping.
fn longest_token_width(text: &str) -> usize {
    text.split_whitespace()
        .map(display_width)
        .max()
        .unwrap_or(0)
}

/// Distribute `available` columns of cell width across `natural.len()`
/// columns, given each column's `natural` (preferred) width and `minimum`
/// (floor) width.
///
/// The allocation proceeds in two stages:
///
/// 1. Effective minimums. Start from the per-column `minimum`. If their sum
///    already exceeds `available`, the table is narrower than its floors:
///    collapse every column to width 1 and hand the remaining budget out
///    proportional to each column's `minimum - 1` weight (leftover
///    distributed one column at a time).
/// 2. Widths. If every column's `natural` width fits
///    (`sum(natural) <= available`), give each column
///    `max(natural, effective_min)`. Otherwise floor each column at its
///    effective minimum and distribute the leftover budget proportional to
///    each column's growth potential (`natural - effective_min`), then hand
///    out any rounding remainder one column at a time to columns still below
///    their natural width.
fn distribute_column_widths(natural: &[usize], minimum: &[usize], available: usize) -> Vec<usize> {
    let n = natural.len();

    // Stage 1: effective minimums.
    let mut min_widths = minimum.to_vec();
    let min_total: usize = min_widths.iter().sum();
    if min_total > available {
        min_widths = vec![1; n];
        let remaining = available.saturating_sub(n);
        if remaining > 0 {
            let total_weight: usize = minimum.iter().map(|m| m.saturating_sub(1)).sum();
            let mut allocated = 0_usize;
            if total_weight > 0 {
                for i in 0..n {
                    let weight = minimum[i].saturating_sub(1);
                    let add = weight * remaining / total_weight;
                    min_widths[i] += add;
                    allocated += add;
                }
            }
            let mut leftover = remaining - allocated;
            for w in min_widths.iter_mut() {
                if leftover == 0 {
                    break;
                }
                *w += 1;
                leftover -= 1;
            }
        }
    }
    let min_cells_width: usize = min_widths.iter().sum();

    // Stage 2: widths.
    let natural_total: usize = natural.iter().sum();
    if natural_total <= available {
        return (0..n).map(|i| natural[i].max(min_widths[i])).collect();
    }

    let total_grow_potential: usize = (0..n)
        .map(|i| natural[i].saturating_sub(min_widths[i]))
        .sum();
    let extra_width = available.saturating_sub(min_cells_width);
    let mut widths: Vec<usize> = (0..n)
        .map(|i| {
            let delta = natural[i].saturating_sub(min_widths[i]);
            let grow = if total_grow_potential > 0 {
                delta * extra_width / total_grow_potential
            } else {
                0
            };
            min_widths[i] + grow
        })
        .collect();

    // Round-off: hand out the remaining budget one column at a time to any
    // column still below its natural width.
    let allocated: usize = widths.iter().sum();
    let mut remaining = available.saturating_sub(allocated);
    while remaining > 0 {
        let mut grew = false;
        for i in 0..n {
            if remaining == 0 {
                break;
            }
            if widths[i] < natural[i] {
                widths[i] += 1;
                remaining -= 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    widths
}

/// Build a horizontal rule spanning the table with the given corner and
/// junction glyphs: `left` at the start, `mid` between columns, `right` at
/// the end. The fill is `─`, sized to each column width plus its two padding
/// columns so it lines up with the `│ … │` content rows. The whole rule is
/// one [`SpanKind::TableBorder`] span.
fn make_border_row(widths: &[usize], left: char, mid: char, right: char) -> Vec<StyledSpan> {
    let mut border = String::new();
    border.push(left);
    for (idx, w) in widths.iter().enumerate() {
        if idx > 0 {
            border.push(mid);
        }
        // The two padding columns become `─` too, so the rule matches the
        // width of the `│ … │` rows.
        for _ in 0..(w + 2) {
            border.push('─');
        }
    }
    border.push(right);
    vec![StyledSpan::plain(border, SpanKind::TableBorder)]
}

/// Render one header or body row into visual rows.
///
/// Each cell's spans wrap independently to its column width. A cell taller
/// than the others (its content wrapped to more visual lines) makes the row
/// span multiple lines. Shorter cells are blank-padded on the extra lines so
/// the `│` borders stay aligned down the row. The `│` separators carry
/// [`SpanKind::TableBorder`]. The single spaces framing each cell are plain
/// `Text` (invisible either way).
fn render_table_row(
    cells: &[Vec<StyledSpan>],
    widths: &[usize],
    alignments: &[Alignment],
) -> Vec<Vec<StyledSpan>> {
    let n = widths.len();
    let wrapped_per_cell: Vec<Vec<MarkdownRow>> = (0..n)
        .map(|c| {
            let empty = Vec::new();
            let spans = cells.get(c).unwrap_or(&empty);
            // A zero-width column holds a single empty line: the shared wrap
            // makes no progress at width 0, so we short-circuit it.
            if widths[c] == 0 {
                vec![Vec::new()]
            } else {
                wrap_spans(spans, widths[c])
            }
        })
        .collect();

    let max_lines = wrapped_per_cell.iter().map(Vec::len).max().unwrap_or(1);

    let mut out: Vec<Vec<StyledSpan>> = Vec::with_capacity(max_lines);
    for line_idx in 0..max_lines {
        let mut row: MarkdownRow = vec![StyledSpan::plain("│", SpanKind::TableBorder)];
        for col in 0..n {
            let cell_line = wrapped_per_cell[col]
                .get(line_idx)
                .cloned()
                .unwrap_or_default();
            row.push(StyledSpan::plain(" ", SpanKind::Text));
            pad_cell(&mut row, cell_line, widths[col], alignments[col]);
            row.push(StyledSpan::plain(" ", SpanKind::Text));
            row.push(StyledSpan::plain("│", SpanKind::TableBorder));
        }
        out.push(row);
    }
    out
}

/// Pad `cell_line` to `width` visible columns per `alignment`, appending the
/// result to `row`. Padding is plain `Text` spaces, invisible so their kind
/// never shows. A line already at or over `width` gets no padding: the cell
/// was pre-wrapped to the column width, and a wide grapheme can still land
/// one column over at a degenerate width.
fn pad_cell(row: &mut MarkdownRow, cell_line: MarkdownRow, width: usize, alignment: Alignment) {
    let vw: usize = cell_line.iter().map(span_width).sum();
    if vw >= width {
        row.extend(cell_line);
        return;
    }
    let padding = width - vw;
    // Center biases the odd extra column to the right (`left = padding / 2`).
    let (left, right) = match alignment {
        Alignment::Left => (0, padding),
        Alignment::Right => (padding, 0),
        Alignment::Center => (padding / 2, padding - padding / 2),
    };
    if left > 0 {
        row.push(StyledSpan::plain(" ".repeat(left), SpanKind::Text));
    }
    row.extend(cell_line);
    if right > 0 {
        row.push(StyledSpan::plain(" ".repeat(right), SpanKind::Text));
    }
}

/// Render a sequence of inline tokens into `out`, composing emphasis
/// through nesting.
///
/// `base_kind` is the role for plain text runs (`Text`, `Heading`, or
/// `LinkText` inside a link). `emphasis` is the accumulated decoration to
/// apply. `link` is the enclosing link's target, threaded so nested
/// content carries the OSC-8 target.
///
/// Unlike the styled renderer, whose ANSI-state hack applies the outer
/// heading/default styling only to text runs, we compose emphasis
/// uniformly: inline code nested under `**` (or under a heading, or under
/// `default_emphasis`) carries that emphasis too. There is no cross-span
/// state to reopen in this model, so uniform composition is the natural
/// encoding. The observable divergence is limited to inline code inside a
/// heading or a `default_emphasis` paragraph, which we consider an
/// improvement.
fn render_inlines(
    inlines: &[Inline],
    base_kind: SpanKind,
    emphasis: Emphasis,
    link: Option<&str>,
    opts: &RenderOpts,
    out: &mut Vec<StyledSpan>,
) {
    for inline in inlines {
        match inline {
            Inline::Text(t) => out.push(StyledSpan {
                text: t.clone(),
                kind: base_kind,
                emphasis,
                syntax: None,
                link: link.map(str::to_string),
            }),
            Inline::Bold(inner) => {
                render_inlines(inner, base_kind, emphasis.with_bold(), link, opts, out)
            }
            Inline::Italic(inner) => {
                render_inlines(inner, base_kind, emphasis.with_italic(), link, opts, out)
            }
            Inline::Strikethrough(inner) => render_inlines(
                inner,
                base_kind,
                emphasis.with_strikethrough(),
                link,
                opts,
                out,
            ),
            Inline::Code(code) => out.push(StyledSpan {
                text: code.clone(),
                kind: SpanKind::InlineCode,
                emphasis,
                syntax: None,
                link: link.map(str::to_string),
            }),
            Inline::Link(inner, url) => {
                // Link text is underlined and carries the target. Nested
                // content overrides the link target to `url`.
                render_inlines(
                    inner,
                    SpanKind::LinkText,
                    emphasis.with_underline(),
                    Some(url),
                    opts,
                    out,
                );
                // When the backend lacks OSC-8, append the visible target
                // so the URL stays reachable, unless the visible text is
                // already the URL (an autolink), where the parens would be
                // redundant. Mirrors the styled renderer's link fallback.
                let plain = inline_plain_text(inner);
                let is_autolink =
                    plain == *url || url.strip_prefix("mailto:") == Some(plain.as_str());
                if !opts.hyperlinks && !is_autolink {
                    out.push(StyledSpan {
                        text: format!(" ({url})"),
                        kind: SpanKind::LinkUrl,
                        emphasis,
                        syntax: None,
                        link: None,
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Syntax highlighting
// ---------------------------------------------------------------------------

/// The syntect syntax definitions, loaded once and shared across the whole
/// process.
///
/// syntect compiles a grammar's regexes lazily on first use and caches them
/// inside the `SyntaxSet`. That first compile is expensive (tens of ms for
/// a heavy grammar like Rust), so loading a fresh set per code block would
/// pay it every time. A process-global set means the compile happens once
/// and every later block reuses the cached regexes. The set is read-only to
/// callers and its caches are `Sync`, so sharing it is sound.
fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// TextMate-style scope selectors per category, parsed once.
///
/// We classify a token by picking the category whose selector matches its
/// scope stack with the highest [`MatchPower`], the same most-specific-wins
/// rule syntect themes use. `keyword` and `keyword.operator` overlap on
/// purpose: an operator token matches both, and the more specific
/// `keyword.operator` wins, so operators get their own category rather than
/// the keyword one.
fn category_selectors() -> &'static [(SyntaxCategory, ScopeSelectors)] {
    static SELECTORS: OnceLock<Vec<(SyntaxCategory, ScopeSelectors)>> = OnceLock::new();
    SELECTORS.get_or_init(|| {
        let defs: &[(SyntaxCategory, &str)] = &[
            (SyntaxCategory::Comment, "comment"),
            (SyntaxCategory::Keyword, "keyword, storage"),
            (
                SyntaxCategory::Function,
                "entity.name.function, support.function, variable.function",
            ),
            (SyntaxCategory::Variable, "variable"),
            (SyntaxCategory::String, "string"),
            (SyntaxCategory::Number, "constant.numeric"),
            (
                SyntaxCategory::Type,
                "entity.name.type, entity.name.class, support.type, support.class",
            ),
            (SyntaxCategory::Operator, "keyword.operator"),
            (SyntaxCategory::Punctuation, "punctuation"),
        ];
        defs.iter()
            .filter_map(|(cat, sel)| sel.parse::<ScopeSelectors>().ok().map(|s| (*cat, s)))
            .collect()
    })
}

/// Pick the best-matching category for a scope stack, or `None` when no
/// category's selector matches (the token then renders in the default
/// foreground). Highest [`MatchPower`] wins, so a more specific selector
/// beats a broader one on the same token.
fn classify_scope(
    scopes: &[Scope],
    selectors: &[(SyntaxCategory, ScopeSelectors)],
) -> Option<SyntaxCategory> {
    let mut best: Option<(MatchPower, SyntaxCategory)> = None;
    for (cat, sel) in selectors {
        if let Some(power) = sel.does_match(scopes)
            && best.is_none_or(|(bp, _)| power > bp)
        {
            best = Some((power, *cat));
        }
    }
    best.map(|(_, cat)| cat)
}

/// Classify `code` into per-line runs of `(category, text)`.
///
/// One inner `Vec` per source line, in order. Each run is a maximal slice
/// of the line that syntect assigned the same category (or `None` when no
/// category matched). We only use syntect to assign scopes: the concrete
/// colors are the frontend's job via [`SyntaxCategory`].
///
/// A single [`ParseState`] and [`ScopeStack`] span the whole block so
/// multi-line constructs (block comments, multi-line strings) carry their
/// scope from one line to the next. When the language is unknown or absent
/// we fall back to plain-text syntax, which matches no category, so every
/// run comes back `None`. On a tokenizer error for a line we emit that line
/// as a single plain run so it still renders.
///
/// The result is memoized per thread keyed by `(lang, code)`. Syntect parsing
/// is the dominant width-independent render cost, so caching it keeps a
/// re-layout at a new width (a resize, or the transcript-focus gutter shift,
/// both of which re-lay-out every visible entry) from re-running syntect on
/// unchanged code. Callers get a shared handle, so a hit is a refcount bump.
fn highlight_code(code: &str, lang: Option<&str>) -> Highlighted {
    let key = highlight_key(code, lang);
    if let Some(hit) = HIGHLIGHT_CACHE.with(|cache| cache.borrow_mut().get(key)) {
        return hit;
    }

    let syntax_set = syntax_set();
    let syntax = lang
        .and_then(|l| syntax_set.find_syntax_by_token(l))
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());

    let selectors = category_selectors();
    let mut parse_state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut lines: Vec<Vec<(Option<SyntaxCategory>, String)>> = Vec::new();

    for line in code.lines() {
        let Ok(ops) = parse_state.parse_line(line, syntax_set) else {
            lines.push(vec![(None, line.to_string())]);
            continue;
        };

        let mut runs: Vec<(Option<SyntaxCategory>, String)> = Vec::new();
        let mut errored = false;
        for (text, op) in ScopeRegionIterator::new(&ops, line) {
            // The op precedes its text region, so apply it before
            // classifying. The leading region carries a no-op op.
            if stack.apply(op).is_err() {
                errored = true;
                break;
            }
            if text.is_empty() {
                continue;
            }
            runs.push((
                classify_scope(stack.as_slice(), selectors),
                text.to_string(),
            ));
        }

        if errored {
            lines.push(vec![(None, line.to_string())]);
        } else {
            lines.push(runs);
        }
    }

    let highlighted = Rc::new(lines);
    HIGHLIGHT_CACHE.with(|cache| cache.borrow_mut().insert(key, Rc::clone(&highlighted)));
    highlighted
}

/// Highlighted code: one entry per source line, each a list of `(category,
/// text)` runs. Shared out of [`HighlightCache`] so a hit is a refcount bump
/// rather than a deep copy.
type Highlighted = Rc<Vec<Vec<(Option<SyntaxCategory>, String)>>>;

/// Distinct code blocks remembered per thread. Comfortably covers a viewport's
/// worth of code, which is all a resize or gutter shift re-lays-out at once.
const HIGHLIGHT_CACHE_CAPACITY: usize = 128;

thread_local! {
    /// Per-thread memo for [`highlight_code`]. Thread-local rather than a
    /// shared static: rendering runs on a single thread, so the memo needs no
    /// lock and can hold `Rc`. A test thread gets its own, so a warm cache
    /// cannot leak across tests.
    static HIGHLIGHT_CACHE: RefCell<HighlightCache> = RefCell::new(HighlightCache::new());
}

/// A bounded LRU memo keyed by a hash of `(lang, code)`.
///
/// The key is a hash, not the source, so a lookup allocates nothing. A slot
/// holds the highlighted runs of whatever code first populated the key, so a
/// 64-bit collision would render that other block's text in place of this one.
/// That is astronomically unlikely and stays bounded (no crash, no corruption
/// of unrelated state), so we accept it rather than storing the source to
/// verify against.
struct HighlightCache {
    slots: HashMap<u64, HighlightSlot>,
    /// Monotonic counter stamped onto a slot on every access, so eviction can
    /// drop the coldest slot.
    tick: u64,
}

struct HighlightSlot {
    highlighted: Highlighted,
    last_used: u64,
}

impl HighlightCache {
    fn new() -> HighlightCache {
        HighlightCache {
            slots: HashMap::new(),
            tick: 0,
        }
    }

    /// The cached highlight for `key`, marking the slot most-recently-used.
    fn get(&mut self, key: u64) -> Option<Highlighted> {
        self.tick += 1;
        let tick = self.tick;
        let slot = self.slots.get_mut(&key)?;
        slot.last_used = tick;
        Some(Rc::clone(&slot.highlighted))
    }

    /// Store `highlighted` for `key`, evicting the coldest slot past capacity.
    fn insert(&mut self, key: u64, highlighted: Highlighted) {
        let last_used = self.tick;
        self.slots.insert(
            key,
            HighlightSlot {
                highlighted,
                last_used,
            },
        );
        if self.slots.len() > HIGHLIGHT_CACHE_CAPACITY {
            self.evict_coldest();
        }
    }

    /// Remove the least-recently-used slot. O(n) over the map, but only fires
    /// on an insert past capacity, and the map is bounded.
    fn evict_coldest(&mut self) {
        if let Some(key) = self
            .slots
            .iter()
            .min_by_key(|(_, slot)| slot.last_used)
            .map(|(key, _)| *key)
        {
            self.slots.remove(&key);
        }
    }
}

/// Hash `(lang, code)` into the [`HighlightCache`] key.
fn highlight_key(code: &str, lang: Option<&str>) -> u64 {
    let mut hasher = DefaultHasher::new();
    lang.hash(&mut hasher);
    code.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
fn highlight_cache_len() -> usize {
    HIGHLIGHT_CACHE.with(|cache| cache.borrow().slots.len())
}

#[cfg(test)]
fn reset_highlight_cache() {
    HIGHLIGHT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        cache.slots.clear();
        cache.tick = 0;
    });
}

// ---------------------------------------------------------------------------
// Tab expansion
// ---------------------------------------------------------------------------

/// Columns a tab expands to before parsing and wrapping.
const TAB_WIDTH: usize = 3;

/// The spaces a tab normalizes to, kept in sync with [`TAB_WIDTH`].
const TAB_AS_SPACES: &str = "   ";

const _: () = assert!(TAB_AS_SPACES.len() == TAB_WIDTH);

/// Expand tabs to [`TAB_AS_SPACES`] so downstream width math and the
/// rendered glyph count agree. A UX call over CommonMark's four-column
/// tab: a fenced code block with hard tabs otherwise renders with literal
/// `\t` bytes instead of a visible indent. Normalization is idempotent, so
/// re-running it is harmless.
fn expand_tabs(text: &str) -> String {
    text.replace('\t', TAB_AS_SPACES)
}

// ---------------------------------------------------------------------------
// Markdown parser
// ---------------------------------------------------------------------------

/// A parsed markdown block.
#[derive(Debug)]
enum Block {
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    CodeBlock(Option<String>, String),
    UnorderedList(Vec<ListItem>),
    OrderedList(Vec<ListItem>),
    /// A blockquote, stored as a list of nested blocks. The blockquote
    /// body is a full sub-document: the parser strips the `> ` prefix from
    /// each line, recursively parses the result, and stores the resulting
    /// blocks here. Multi-line plain-text quotes render as multiple rows
    /// because soft breaks are preserved as `\n` (see [`parse_markdown`])
    /// and the wrap expands those newlines into visible rows.
    Blockquote(Vec<Block>),
    /// A GitHub-flavored-markdown table: one header row, one alignment spec
    /// per column, zero or more data rows. Each cell is pre-parsed inline
    /// content. `raw` holds the original markdown source for the table
    /// block (header + separator + body lines joined with `\n`), used as the
    /// prose fallback when the render width is too narrow for a bordered
    /// table (see [`render_table`]).
    Table {
        headers: Vec<Vec<Inline>>,
        alignments: Vec<Alignment>,
        rows: Vec<Vec<Vec<Inline>>>,
        raw: String,
    },
    HorizontalRule,
}

/// Column alignment for a markdown table, driven by the separator row's
/// leading/trailing colons (`:---`, `---:`, `:---:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alignment {
    Left,
    Center,
    Right,
}

#[derive(Debug)]
struct ListItem {
    content: Vec<Inline>,
    /// Nested blocks (e.g. sub-lists) that belong to this item.
    sub_blocks: Vec<Block>,
    /// For ordered items, the source marker (e.g. `1` in `"1. Foo"`).
    /// Preserved verbatim so that lists split by intervening blocks don't
    /// restart numbering from `1` in the rendered output. `None` for
    /// unordered items.
    number: Option<u32>,
}

#[derive(Debug)]
enum Inline {
    Text(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Code(String),
    /// `[text](url)` markdown link, autolinked URL, or autolinked email.
    /// The first field is the parsed inline tokens that make up the link's
    /// *visible* text, so a `[**bold**](url)` keeps the bold emphasis
    /// nested under the link. For autolinks the inner is
    /// `vec![Inline::Text(url)]` so the visible text is the URL itself. The
    /// second field is the URL target.
    Link(Vec<Inline>, String),
}

/// Maximum nesting depth the parser descends before degrading to literal
/// text.
///
/// Caps two independent recursion families: block nesting (blockquotes and
/// lists share one counter, since a list can sit inside a quote) and inline
/// nesting (emphasis and links). Past the limit the parser stops building
/// structure and emits the remainder as plain text.
///
/// This is the only guard against unbounded recursion on the untrusted
/// model output this renderer consumes. A Rust stack overflow is an
/// uncatchable process abort, so the cap, not graceful unwinding, is what
/// keeps a pathologically nested message from taking the process down.
/// Because [`Block`] and [`Inline`] are only ever built by the parser,
/// capping it bounds the AST depth and therefore also the render-time
/// recursion that walks it.
///
/// 64 is far above realistic content while keeping the worst-case render
/// stack to a few hundred frames.
const MAX_NESTING_DEPTH: usize = 64;

/// Parse markdown `text` into the render AST ([`Block`] / [`Inline`]).
///
/// We delegate CommonMark + GFM parsing to `pulldown-cmark` and fold its
/// event stream into the small block/inline tree the renderer walks.
///
/// Three behaviors diverge from strict CommonMark on purpose, because the
/// target surface is an agent's chat output rather than an HTML document:
///
/// - Soft and hard line breaks both map to a literal `\n`. A CLI user
///   typing a multi-line message expects each line on its own row, so we
///   never collapse a soft break to a space. The wrap splits on `\n`.
/// - Raw HTML (`<thinking>`, `<div>`, ...) is emitted as literal visible
///   text rather than interpreted or hidden. A model that wraps content in
///   tags should have those exact bytes shown to the user.
/// - Bare URLs and emails are autolinked by a post-pass over text runs (see
///   [`linkify`]); pulldown only autolinks the angle-bracket form.
///
/// [`MAX_NESTING_DEPTH`] bounds the produced AST depth: structure deeper
/// than the cap degrades to literal text.
fn parse_markdown(text: &str) -> Vec<Block> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let mut stack: Vec<Frame> = vec![Frame::new(FrameKind::Document)];
    // Count of structural `Start`s swallowed past `MAX_NESTING_DEPTH`.
    // While non-zero we push no frames. Text content degrades into the
    // deepest surviving frame so nothing is lost, and the `suppress`
    // counter keeps `Start`/`End` balanced so we resume cleanly once
    // nesting unwinds.
    let mut suppress: usize = 0;

    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        match event {
            Event::Start(tag) => match tag {
                // Table head/row aren't frames: they only group cells. We
                // track them as state on the enclosing table frame, and
                // they must not touch `suppress` (neither does their
                // matching End).
                Tag::TableHead | Tag::TableRow => {
                    if suppress == 0
                        && let FrameKind::Table { current_row, .. } =
                            &mut stack.last_mut().unwrap().kind
                    {
                        current_row.clear();
                    }
                }
                _ => {
                    if suppress > 0 {
                        suppress += 1;
                    } else if stack.len() - 1 >= MAX_NESTING_DEPTH {
                        suppress = 1;
                    } else {
                        let kind = start_tag_to_kind(tag, text, range);
                        stack.push(Frame::new(kind));
                    }
                }
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::TableHead => {
                    if suppress == 0
                        && let FrameKind::Table {
                            headers,
                            current_row,
                            ..
                        } = &mut stack.last_mut().unwrap().kind
                    {
                        *headers = std::mem::take(current_row);
                    }
                }
                TagEnd::TableRow => {
                    if suppress == 0
                        && let FrameKind::Table {
                            rows, current_row, ..
                        } = &mut stack.last_mut().unwrap().kind
                    {
                        rows.push(std::mem::take(current_row));
                    }
                }
                TagEnd::TableCell => {
                    if suppress > 0 {
                        suppress -= 1;
                    } else {
                        let mut cell = stack.pop().unwrap();
                        cell.flush_text();
                        if let FrameKind::Table { current_row, .. } =
                            &mut stack.last_mut().unwrap().kind
                        {
                            current_row.push(std::mem::take(&mut cell.inlines));
                        }
                    }
                }
                _ => {
                    if suppress > 0 {
                        suppress -= 1;
                    } else {
                        let frame = stack.pop().unwrap();
                        finish_frame(frame, &mut stack);
                    }
                }
            },
            // Raw HTML (block or inline) passes through as literal text.
            Event::Text(s) | Event::Html(s) | Event::InlineHtml(s) => {
                stack.last_mut().unwrap().push_text(&s);
            }
            Event::Code(s) => {
                let top = stack.last_mut().unwrap();
                if suppress > 0 {
                    top.push_text(&s);
                } else {
                    top.push_inline(Inline::Code(s.to_string()));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                stack.last_mut().unwrap().push_text("\n");
            }
            Event::Rule => {
                if suppress == 0 {
                    stack.last_mut().unwrap().push_block(Block::HorizontalRule);
                }
            }
            // Footnotes, task lists, and math require options we don't
            // enable, so these never appear. Degrade to text defensively
            // rather than risk a panic on a future options change.
            Event::FootnoteReference(s) | Event::InlineMath(s) | Event::DisplayMath(s) => {
                stack.last_mut().unwrap().push_text(&s);
            }
            Event::TaskListMarker(_) => {}
        }
    }

    // pulldown emits balanced events, so only the document frame remains.
    // The loop is a defensive unwind for any frame left open (e.g. by a
    // future change) so we never drop content.
    while stack.len() > 1 {
        let frame = stack.pop().unwrap();
        finish_frame(frame, &mut stack);
    }
    let mut document = stack.pop().unwrap();
    finish_block_container(&mut document)
}

/// A node under construction while folding the pulldown event stream.
///
/// Every frame buffers inline content two ways: `current_text` is the raw
/// text not yet tokenized, and `inlines` holds the tokens produced so far.
/// `flush_text` moves the former into the latter (linkifying as it goes).
/// `blocks` collects child blocks for the block-level containers.
struct Frame {
    kind: FrameKind,
    current_text: String,
    inlines: Vec<Inline>,
    blocks: Vec<Block>,
}

/// What a [`Frame`] represents, plus the per-kind state needed to build its
/// AST node on close.
enum FrameKind {
    Document,
    Paragraph,
    Heading(u8),
    BlockQuote,
    /// Fenced or indented code block, carrying the info-string language tag.
    CodeBlock(Option<String>),
    /// A raw HTML block, rendered as a literal-text paragraph.
    HtmlBlock,
    /// `next_number` is the marker for the next item (seeded from the
    /// list's start number); `items` accumulates finished items.
    List {
        ordered: bool,
        next_number: u32,
        items: Vec<ListItem>,
    },
    Item,
    Emphasis,
    Strong,
    Strikethrough,
    Link(String),
    Image(String),
    /// `current_row` accumulates cells until the row/head closes; `headers`
    /// and `rows` collect finished rows; `raw` is the source slice used for
    /// the narrow-width fallback.
    Table {
        alignments: Vec<Alignment>,
        headers: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
        current_row: Vec<Vec<Inline>>,
        raw: String,
    },
    TableCell,
}

impl Frame {
    fn new(kind: FrameKind) -> Self {
        Self {
            kind,
            current_text: String::new(),
            inlines: Vec::new(),
            blocks: Vec::new(),
        }
    }

    fn push_text(&mut self, s: &str) {
        self.current_text.push_str(s);
    }

    /// Flush buffered text into inline tokens, autolinking bare URLs and
    /// emails along the way. Code-block bodies bypass this (they read
    /// `current_text` directly on close) so their contents stay literal.
    fn flush_text(&mut self) {
        if !self.current_text.is_empty() {
            let text = std::mem::take(&mut self.current_text);
            self.inlines.extend(linkify(&text));
        }
    }

    fn push_inline(&mut self, inline: Inline) {
        self.flush_text();
        self.inlines.push(inline);
    }

    /// Append a child block. Inline content buffered before this block is
    /// first wrapped into a paragraph so source order is preserved: this is
    /// how a tight list item's text or stray inline content ahead of a
    /// nested block becomes its own paragraph.
    fn push_block(&mut self, block: Block) {
        self.flush_text();
        if !self.inlines.is_empty() {
            let inlines = std::mem::take(&mut self.inlines);
            self.blocks.push(Block::Paragraph(inlines));
        }
        self.blocks.push(block);
    }
}

/// Pop a finished frame and attach its product to the parent (now top of
/// `stack`). Inline frames contribute an [`Inline`]; block frames a
/// [`Block`]; list items push onto the enclosing list.
fn finish_frame(mut frame: Frame, stack: &mut Vec<Frame>) {
    match std::mem::replace(&mut frame.kind, FrameKind::Document) {
        // The document frame is unwound by the caller, never here.
        FrameKind::Document => {}
        FrameKind::Paragraph => {
            frame.flush_text();
            let inlines = std::mem::take(&mut frame.inlines);
            // pulldown can emit an empty paragraph (e.g. a blank quote
            // line); drop it rather than render an empty row.
            if !inlines.is_empty() {
                stack
                    .last_mut()
                    .unwrap()
                    .push_block(Block::Paragraph(inlines));
            }
        }
        FrameKind::Heading(level) => {
            frame.flush_text();
            let inlines = std::mem::take(&mut frame.inlines);
            stack
                .last_mut()
                .unwrap()
                .push_block(Block::Heading(level, inlines));
        }
        FrameKind::BlockQuote => {
            let blocks = finish_block_container(&mut frame);
            stack
                .last_mut()
                .unwrap()
                .push_block(Block::Blockquote(blocks));
        }
        FrameKind::CodeBlock(lang) => {
            // Strip the single trailing newline pulldown appends so
            // `code.lines()` in the renderer matches the source lines.
            let body = frame
                .current_text
                .strip_suffix('\n')
                .unwrap_or(&frame.current_text)
                .to_string();
            stack
                .last_mut()
                .unwrap()
                .push_block(Block::CodeBlock(lang, body));
        }
        FrameKind::HtmlBlock => {
            frame.flush_text();
            let inlines = std::mem::take(&mut frame.inlines);
            if !inlines.is_empty() {
                stack
                    .last_mut()
                    .unwrap()
                    .push_block(Block::Paragraph(inlines));
            }
        }
        FrameKind::List { ordered, items, .. } => {
            let block = if ordered {
                Block::OrderedList(items)
            } else {
                Block::UnorderedList(items)
            };
            stack.last_mut().unwrap().push_block(block);
        }
        FrameKind::Item => {
            let item = build_list_item(frame);
            // Number the item from the enclosing list's running counter so
            // a list whose start marker isn't 1 (or that pulldown split off
            // a preceding list) renders with the right first number.
            if let FrameKind::List {
                ordered,
                next_number,
                items,
            } = &mut stack.last_mut().unwrap().kind
            {
                let number = if *ordered {
                    let n = *next_number;
                    *next_number = next_number.saturating_add(1);
                    Some(n)
                } else {
                    None
                };
                items.push(ListItem { number, ..item });
            }
        }
        FrameKind::Emphasis => {
            frame.flush_text();
            let inner = std::mem::take(&mut frame.inlines);
            stack.last_mut().unwrap().push_inline(Inline::Italic(inner));
        }
        FrameKind::Strong => {
            frame.flush_text();
            let inner = std::mem::take(&mut frame.inlines);
            stack.last_mut().unwrap().push_inline(Inline::Bold(inner));
        }
        FrameKind::Strikethrough => {
            frame.flush_text();
            let inner = std::mem::take(&mut frame.inlines);
            stack
                .last_mut()
                .unwrap()
                .push_inline(Inline::Strikethrough(inner));
        }
        FrameKind::Link(url) => {
            frame.flush_text();
            let inner = std::mem::take(&mut frame.inlines);
            stack
                .last_mut()
                .unwrap()
                .push_inline(Inline::Link(inner, url));
        }
        FrameKind::Image(url) => {
            // Terminals can't show images, so we render the alt text as a
            // link to the source: the reference stays reachable, and an
            // image with no alt text degrades to a bare link.
            frame.flush_text();
            let inner = std::mem::take(&mut frame.inlines);
            stack
                .last_mut()
                .unwrap()
                .push_inline(Inline::Link(inner, url));
        }
        FrameKind::Table {
            alignments,
            headers,
            rows,
            raw,
            ..
        } => {
            stack.last_mut().unwrap().push_block(Block::Table {
                headers,
                alignments,
                rows,
                raw,
            });
        }
        FrameKind::TableCell => {
            // Reached only via the defensive unwind. The normal path is the
            // End(TableCell) arm. Degrade leftover cell content to text.
            frame.flush_text();
            let inlines = std::mem::take(&mut frame.inlines);
            if !inlines.is_empty() {
                stack
                    .last_mut()
                    .unwrap()
                    .push_block(Block::Paragraph(inlines));
            }
        }
    }
}

/// Finish a block-level container: flush any trailing inline content into a
/// final paragraph, then return its collected blocks.
fn finish_block_container(frame: &mut Frame) -> Vec<Block> {
    frame.flush_text();
    if !frame.inlines.is_empty() {
        let inlines = std::mem::take(&mut frame.inlines);
        frame.blocks.push(Block::Paragraph(inlines));
    }
    std::mem::take(&mut frame.blocks)
}

/// Split a finished item frame into its bullet content and nested blocks.
///
/// The item's first paragraph (or, for a tight list, its direct inline
/// text) becomes the bullet `content`; everything else becomes `sub_blocks`
/// rendered indented under the bullet. `number` is assigned by the caller.
fn build_list_item(mut frame: Frame) -> ListItem {
    frame.flush_text();
    let mut content: Vec<Inline> = Vec::new();
    let mut sub_blocks: Vec<Block> = Vec::new();
    for block in std::mem::take(&mut frame.blocks) {
        match block {
            Block::Paragraph(inlines) if content.is_empty() => content = inlines,
            other => sub_blocks.push(other),
        }
    }
    // Direct inline text on a tight item never became a block. Adopt it as
    // the content, or as a trailing paragraph if a block already claimed it.
    let leftover = std::mem::take(&mut frame.inlines);
    if !leftover.is_empty() {
        if content.is_empty() {
            content = leftover;
        } else {
            sub_blocks.push(Block::Paragraph(leftover));
        }
    }
    ListItem {
        content,
        sub_blocks,
        number: None,
    }
}

/// Map a pulldown `Start(Tag)` onto the [`FrameKind`] we push for it.
/// `source` and `range` are used only to capture a table's raw source.
fn start_tag_to_kind(tag: Tag, source: &str, range: std::ops::Range<usize>) -> FrameKind {
    match tag {
        Tag::Paragraph => FrameKind::Paragraph,
        Tag::Heading { level, .. } => FrameKind::Heading(heading_level(level)),
        Tag::BlockQuote(_) => FrameKind::BlockQuote,
        Tag::CodeBlock(kind) => FrameKind::CodeBlock(code_block_lang(kind)),
        Tag::HtmlBlock => FrameKind::HtmlBlock,
        Tag::List(start) => FrameKind::List {
            ordered: start.is_some(),
            next_number: u32::try_from(start.unwrap_or(1)).unwrap_or(u32::MAX),
            items: Vec::new(),
        },
        Tag::Item => FrameKind::Item,
        Tag::Emphasis => FrameKind::Emphasis,
        Tag::Strong => FrameKind::Strong,
        Tag::Strikethrough => FrameKind::Strikethrough,
        Tag::Link { dest_url, .. } => FrameKind::Link(dest_url.to_string()),
        Tag::Image { dest_url, .. } => FrameKind::Image(dest_url.to_string()),
        Tag::Table(aligns) => FrameKind::Table {
            alignments: aligns.into_iter().map(convert_alignment).collect(),
            headers: Vec::new(),
            rows: Vec::new(),
            current_row: Vec::new(),
            raw: source
                .get(range)
                .unwrap_or("")
                .trim_end_matches('\n')
                .to_string(),
        },
        Tag::TableCell => FrameKind::TableCell,
        // TableHead/TableRow are intercepted before this call. The
        // remaining variants require options we don't enable, so they never
        // reach here. Treat any straggler as a transparent paragraph so its
        // text still surfaces.
        _ => FrameKind::Paragraph,
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Language tag for a code block: the first whitespace-delimited token of a
/// fenced block's info string, or `None` for an indented block or empty
/// info string.
fn code_block_lang(kind: CodeBlockKind) -> Option<String> {
    match kind {
        CodeBlockKind::Indented => None,
        CodeBlockKind::Fenced(info) => info
            .split_whitespace()
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}

/// Map pulldown's column alignment onto ours. A column with no alignment
/// colon renders left-aligned, matching the default.
fn convert_alignment(alignment: PdAlignment) -> Alignment {
    match alignment {
        PdAlignment::Right => Alignment::Right,
        PdAlignment::Center => Alignment::Center,
        PdAlignment::None | PdAlignment::Left => Alignment::Left,
    }
}

/// Split a plain-text run into text and autolink inlines.
///
/// pulldown only autolinks the angle-bracket form (`<http://...>`), so we
/// recover GFM-style bare URL and email autolinks here. Each match becomes
/// an `Inline::Link` whose visible text is the URL/email itself (so the
/// renderer's `plain == url` check fires the no-parens fallback). A URL
/// containing markdown-active characters (`*`, `_`) can be split by
/// pulldown's inline parsing before it reaches this pass. The common case
/// (no such characters) round-trips intact.
fn linkify(text: &str) -> Vec<Inline> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Inline> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    while i < chars.len() {
        if let Some(end) = bare_url_end(&chars, i) {
            if !buf.is_empty() {
                out.push(Inline::Text(std::mem::take(&mut buf)));
            }
            let url: String = chars[i..end].iter().collect();
            out.push(Inline::Link(vec![Inline::Text(url.clone())], url));
            i = end;
            continue;
        }

        // The local part of a bare email has already been buffered into
        // `buf`; `bare_email_span` checks it's recoverable and reports the
        // span so we can back it out.
        if chars[i] == '@'
            && let Some((start, end)) = bare_email_span(&chars, i, buf.chars().count())
        {
            for _ in 0..(i - start) {
                buf.pop();
            }
            if !buf.is_empty() {
                out.push(Inline::Text(std::mem::take(&mut buf)));
            }
            let email: String = chars[start..end].iter().collect();
            out.push(Inline::Link(
                vec![Inline::Text(email.clone())],
                format!("mailto:{email}"),
            ));
            i = end;
            continue;
        }

        buf.push(chars[i]);
        i += 1;
    }

    if !buf.is_empty() {
        out.push(Inline::Text(buf));
    }
    out
}

/// If `chars[start..]` begins with `http://` or `https://`, return the
/// index one past the end of the URL: greedy match until whitespace or a
/// trailing-punctuation character. Trailing punctuation is excluded so a
/// URL at the end of a sentence doesn't eat the period.
fn bare_url_end(chars: &[char], start: usize) -> Option<usize> {
    let scheme: &[&[char]] = &[
        &['h', 't', 't', 'p', ':', '/', '/'],
        &['h', 't', 't', 'p', 's', ':', '/', '/'],
    ];
    let prefix_len = scheme
        .iter()
        .find(|s| chars.len() >= start + s.len() && &chars[start..start + s.len()] == **s)
        .map(|s| s.len())?;

    // Only autolink at word boundaries, i.e. the preceding char is
    // whitespace or not alphanumeric. Prevents `xhttps://...` from
    // matching.
    if start > 0 {
        let prev = chars[start - 1];
        if prev.is_alphanumeric() {
            return None;
        }
    }

    let mut end = start + prefix_len;
    while end < chars.len() {
        let c = chars[end];
        if c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`') {
            break;
        }
        end += 1;
    }
    // Strip trailing punctuation that's almost never part of a URL in
    // prose. Leave brackets/braces balanced: if the URL contains a `(` we
    // leave a trailing `)`, otherwise we trim it.
    while end > start + prefix_len {
        let c = chars[end - 1];
        let unbalanced_close = match c {
            ')' => !chars[start..end].contains(&'('),
            ']' => !chars[start..end].contains(&'['),
            '}' => !chars[start..end].contains(&'{'),
            _ => false,
        };
        if matches!(c, ',' | '.' | ';' | ':' | '!' | '?') || unbalanced_close {
            end -= 1;
            continue;
        }
        break;
    }

    if end <= start + prefix_len {
        None
    } else {
        Some(end)
    }
}

/// If `chars[at]` is `@` and the surrounding context looks like a bare
/// email, return `(start, end)` where `start` is the index of the first
/// local-part char and `end` is one past the last domain char.
///
/// `local_in_current` is the number of local-part characters the outer
/// parser has already buffered in `current`; used to back them out.
fn bare_email_span(chars: &[char], at: usize, local_in_current: usize) -> Option<(usize, usize)> {
    // Local part: walk backwards over valid chars.
    let mut start = at;
    while start > 0 && is_email_local_char(chars[start - 1]) {
        start -= 1;
    }
    if start == at {
        return None;
    }
    // The local-part chars must actually be in `current` (not split across
    // a previous Inline element); if they're not, we can't safely back them
    // out.
    if at - start > local_in_current {
        return None;
    }
    // The character before the local part must be a word boundary.
    if start > 0 && chars[start - 1].is_alphanumeric() {
        return None;
    }

    // Domain part: at least one label + TLD.
    let mut end = at + 1;
    while end < chars.len() && is_email_domain_char(chars[end]) {
        end += 1;
    }
    // Trim trailing punctuation the same way URL autolink does.
    while end > at + 1 {
        let c = chars[end - 1];
        if matches!(c, ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}') {
            end -= 1;
            continue;
        }
        break;
    }
    let domain: String = chars[at + 1..end].iter().collect();
    if !domain.contains('.') {
        return None;
    }
    // The domain's TLD portion must have at least one alphabetic char so
    // something like `a@1.2` doesn't autolink.
    let tld = domain.rsplit('.').next().unwrap_or("");
    if tld.is_empty() || !tld.chars().any(|c| c.is_alphabetic()) {
        return None;
    }

    Some((start, end))
}

fn is_email_local_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '%')
}

fn is_email_domain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-')
}

/// Recursively extract the plain-text content from a sequence of inlines,
/// dropping every styling layer. Used to drive the autolink-vs-fallback
/// decision in [`render_inlines`]: the visible link text is compared
/// against the raw URL to decide whether to append the ` (url)` suffix.
fn inline_plain_text(inlines: &[Inline]) -> String {
    let mut s = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(t) => s.push_str(t),
            Inline::Bold(inner) | Inline::Italic(inner) | Inline::Strikethrough(inner) => {
                s.push_str(&inline_plain_text(inner))
            }
            Inline::Code(c) => s.push_str(c),
            // A link inside a link would render its inner text. The
            // plain-text projection follows the same shape.
            Inline::Link(inner, _) => s.push_str(&inline_plain_text(inner)),
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Width measurement
// ---------------------------------------------------------------------------

/// Terminal display width of one grapheme cluster.
///
/// Control characters are width 0. Isolated regional-indicator codepoints
/// (`U+1F1E6..=U+1F1FF`) are held at 2 rather than the 1 `unicode-width`
/// reports, because terminals usually render a half-arrived flag as a
/// 2-wide tofu glyph. Holding at 2 keeps width math stable while a flag
/// pair is assembled. Everything else defers to `unicode-width`.
fn grapheme_width(grapheme: &str) -> usize {
    let Some(first) = grapheme.chars().next() else {
        return 0;
    };
    if first.is_control() {
        return 0;
    }
    let cp = u32::from(first);
    if (0x1F1E6..=0x1F1FF).contains(&cp) {
        return 2;
    }
    UnicodeWidthStr::width(grapheme)
}

/// Display width of a plain string: the sum of its grapheme widths. Input
/// is expected to be tab-expanded and free of ANSI escapes (the render
/// pipeline guarantees both), so no stripping is needed.
fn display_width(s: &str) -> usize {
    s.graphemes(true).map(grapheme_width).sum()
}

fn span_width(span: &StyledSpan) -> usize {
    display_width(&span.text)
}

fn row_width(row: &[StyledSpan]) -> usize {
    row.iter().map(span_width).sum()
}

// ---------------------------------------------------------------------------
// Span wrapping
// ---------------------------------------------------------------------------
//
// This reimplements the ANSI word-wrap on neutral spans. Because every span
// carries its full style explicitly, there is no escape-code state to track
// across rows: wrapping is pure unicode-width measurement plus bookkeeping
// of which span each token came from. The token-fill, long-word-break, and
// trailing-trim rules mirror the ANSI wrap so a given input produces the
// same row widths.

/// A wrap token: a maximal run of spaces or a maximal run of non-spaces
/// (a "word"). A word may carry more than one styled piece when spans of
/// differing style abut with no space between them (e.g. inline code
/// touching plain text), which is what lets the wrap keep such a run on one
/// row instead of breaking mid-word.
struct Token {
    spans: Vec<StyledSpan>,
    is_whitespace: bool,
    width: usize,
}

/// Wrap a block's logical lines (which may still hold embedded `'\n'`) into
/// visual rows at `width`.
fn wrap_spans(spans: &[StyledSpan], width: usize) -> Vec<MarkdownRow> {
    let mut rows = Vec::new();
    for line in split_on_newlines(spans) {
        rows.extend(wrap_logical_line(&line, width));
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

/// Split spans on embedded `'\n'` into logical lines. `str::split('\n')`
/// yields one more part than there are newlines, so an empty part around a
/// newline opens a fresh (possibly empty) logical line, matching how the
/// ANSI wrap splits on `'\n'` before wrapping each piece.
fn split_on_newlines(spans: &[StyledSpan]) -> Vec<Vec<StyledSpan>> {
    let mut lines: Vec<Vec<StyledSpan>> = vec![Vec::new()];
    for span in spans {
        for (i, part) in span.text.split('\n').enumerate() {
            if i > 0 {
                lines.push(Vec::new());
            }
            if !part.is_empty() {
                lines.last_mut().unwrap().push(span.with_text(part));
            }
        }
    }
    lines
}

/// Wrap one logical line (no embedded newlines) into rows.
fn wrap_logical_line(line: &[StyledSpan], width: usize) -> Vec<MarkdownRow> {
    if line.is_empty() {
        return vec![Vec::new()];
    }
    // Fits as-is: return verbatim (coalesced) without trimming, matching
    // the ANSI wrap's fast path which keeps trailing whitespace on a line
    // that already fits.
    if row_width(line) <= width {
        return vec![finalize_row(line.to_vec(), false)];
    }

    let tokens = split_into_tokens(line);
    let mut rows: Vec<MarkdownRow> = Vec::new();
    let mut current: MarkdownRow = Vec::new();
    let mut current_width = 0usize;

    for token in &tokens {
        // A non-whitespace token wider than the whole width fits on no row;
        // break it grapheme by grapheme, flushing the in-progress row first.
        if token.width > width && !token.is_whitespace {
            if !current.is_empty() {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            let mut broken = break_long_token(token, width);
            // All broken rows but the last are complete. The last one
            // continues filling with the tokens that follow.
            if let Some(last) = broken.pop() {
                rows.append(&mut broken);
                current_width = row_width(&last);
                current = last;
            }
            continue;
        }

        let total_needed = current_width + token.width;
        if total_needed > width && current_width > 0 {
            rows.push(std::mem::take(&mut current));
            if token.is_whitespace {
                // A wrap boundary drops the whitespace run that would have
                // led the next row.
                current_width = 0;
            } else {
                current = token.spans.clone();
                current_width = token.width;
            }
        } else {
            current.extend(token.spans.iter().cloned());
            current_width += token.width;
        }
    }

    if !current.is_empty() {
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }

    rows.into_iter().map(|r| finalize_row(r, true)).collect()
}

/// Split a logical line into wrap tokens on space/non-space boundaries.
/// Only the ASCII space (`' '`) delimits tokens, matching the ANSI wrap;
/// the input is tab-expanded, so no other horizontal whitespace remains.
fn split_into_tokens(line: &[StyledSpan]) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut current: Vec<StyledSpan> = Vec::new();
    let mut in_whitespace = false;

    for span in line {
        for ch in span.text.chars() {
            let is_space = ch == ' ';
            if is_space != in_whitespace && !current.is_empty() {
                tokens.push(finish_token(std::mem::take(&mut current)));
            }
            in_whitespace = is_space;
            push_grapheme(&mut current, &ch.to_string(), span);
        }
    }
    if !current.is_empty() {
        tokens.push(finish_token(current));
    }
    tokens
}

fn finish_token(spans: Vec<StyledSpan>) -> Token {
    let width = spans.iter().map(span_width).sum();
    // A run is either all spaces or all non-spaces, so an all-blank token is
    // whitespace. `trim` also treats exotic whitespace (e.g. non-breaking
    // space) as blank, matching the ANSI wrap's `trim().is_empty()`.
    let is_whitespace = spans.iter().all(|s| s.text.trim().is_empty());
    Token {
        spans,
        is_whitespace,
        width,
    }
}

/// Break a token wider than `width` grapheme by grapheme, preserving each
/// grapheme's originating style. Mirrors the ANSI long-word break: a
/// grapheme that would overflow the current row starts a new one, even at
/// width 1 where a wide grapheme still occupies its own (over-width) row.
fn break_long_token(token: &Token, width: usize) -> Vec<MarkdownRow> {
    let mut rows: Vec<MarkdownRow> = Vec::new();
    let mut current: MarkdownRow = Vec::new();
    let mut current_width = 0usize;

    for piece in &token.spans {
        for g in piece.text.graphemes(true) {
            let gw = grapheme_width(g);
            if current_width + gw > width {
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            push_grapheme(&mut current, g, piece);
            current_width += gw;
        }
    }

    if !current.is_empty() {
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

/// Append grapheme `g` to `row`, coalescing into the last span when it
/// shares `style`'s styling, otherwise starting a new span from `style`.
fn push_grapheme(row: &mut MarkdownRow, g: &str, style: &StyledSpan) {
    if let Some(last) = row.last_mut()
        && last.same_style(style)
    {
        last.text.push_str(g);
        return;
    }
    row.push(style.with_text(g));
}

/// Coalesce adjacent same-style spans and (optionally) trim trailing
/// whitespace. Coalescing yields the minimal span representation. Trimming
/// drops the trailing whitespace a wrap boundary leaves behind.
fn finalize_row(row: MarkdownRow, trim: bool) -> MarkdownRow {
    let mut row = coalesce(row);
    if trim {
        trim_row_end(&mut row);
    }
    row
}

fn coalesce(row: MarkdownRow) -> MarkdownRow {
    let mut out: MarkdownRow = Vec::with_capacity(row.len());
    for span in row {
        if let Some(last) = out.last_mut()
            && last.same_style(&span)
        {
            last.text.push_str(&span.text);
            continue;
        }
        out.push(span);
    }
    out
}

fn trim_row_end(row: &mut MarkdownRow) {
    while let Some(last) = row.last_mut() {
        let trimmed_len = last.text.trim_end().len();
        if trimmed_len == last.text.len() {
            break;
        }
        if trimmed_len == 0 {
            row.pop();
        } else {
            last.text.truncate(trimmed_len);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Shared helpers
    // -----------------------------------------------------------------

    /// A single plain-`Text` span carrying `s`.
    fn text_span(s: &str) -> StyledSpan {
        StyledSpan::plain(s, SpanKind::Text)
    }

    /// One logical line made of a single plain-text span.
    fn line(s: &str) -> Vec<StyledSpan> {
        vec![text_span(s)]
    }

    /// The concatenated visible text of a row.
    fn row_text(row: &MarkdownRow) -> String {
        row.iter().map(|s| s.text.as_str()).collect()
    }

    fn rows_text(rows: &[MarkdownRow]) -> Vec<String> {
        rows.iter().map(row_text).collect()
    }

    /// Flatten the inline tree into a depth-first list of node references,
    /// so a test can assert a variant appears anywhere in a paragraph.
    fn flatten<'a>(inlines: &'a [Inline], out: &mut Vec<&'a Inline>) {
        for inline in inlines {
            out.push(inline);
            match inline {
                Inline::Bold(x)
                | Inline::Italic(x)
                | Inline::Strikethrough(x)
                | Inline::Link(x, _) => flatten(x, out),
                Inline::Text(_) | Inline::Code(_) => {}
            }
        }
    }

    fn any_italic(inlines: &[Inline]) -> bool {
        let mut all = Vec::new();
        flatten(inlines, &mut all);
        all.iter().any(|i| matches!(i, Inline::Italic(_)))
    }

    fn any_bold(inlines: &[Inline]) -> bool {
        let mut all = Vec::new();
        flatten(inlines, &mut all);
        all.iter().any(|i| matches!(i, Inline::Bold(_)))
    }

    fn any_strikethrough(inlines: &[Inline]) -> bool {
        let mut all = Vec::new();
        flatten(inlines, &mut all);
        all.iter().any(|i| matches!(i, Inline::Strikethrough(_)))
    }

    fn paragraph_inlines(blocks: &[Block]) -> &[Inline] {
        match blocks.first() {
            Some(Block::Paragraph(inlines)) => inlines,
            other => panic!("expected a leading paragraph, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Parse layer
    // -----------------------------------------------------------------

    /// Deepest block-nesting level in an AST. Recurses over the same shape
    /// the renderer walks, so it's itself bounded by the parser cap under
    /// test.
    fn block_depth(blocks: &[Block]) -> usize {
        blocks
            .iter()
            .map(|b| match b {
                Block::Blockquote(inner) => 1 + block_depth(inner),
                Block::UnorderedList(items) | Block::OrderedList(items) => {
                    1 + items
                        .iter()
                        .map(|it| block_depth(&it.sub_blocks))
                        .max()
                        .unwrap_or(0)
                }
                _ => 1,
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn parser_caps_block_nesting_on_adversarial_input() {
        // ~100k nested blockquotes collapse to a capped quote stack
        // wrapping a single literal paragraph.
        let quotes = parse_markdown(&"> ".repeat(100_000));
        assert!(
            block_depth(&quotes) <= MAX_NESTING_DEPTH + 1,
            "blockquote nesting not capped: depth {}",
            block_depth(&quotes),
        );

        // 1000 list levels (each line one space deeper) fold into
        // continuation text past the cap.
        let mut nested_list = String::new();
        for level in 0..1000 {
            nested_list.push_str(&" ".repeat(level));
            nested_list.push_str("- x\n");
        }
        let list = parse_markdown(&nested_list);
        assert!(
            block_depth(&list) <= MAX_NESTING_DEPTH + 1,
            "list nesting not capped: depth {}",
            block_depth(&list),
        );
    }

    #[test]
    fn adversarial_inline_markers_render_without_aborting() {
        // Long runs of emphasis and link openers exercise the inline-parse
        // recursion, which shares the nesting cap with block structure. The
        // cap keeps them bounded, so the full pipeline (parse, render, wrap)
        // finishes without overflowing the stack and without exploding the
        // row count. A bare `[` run is quadratic in the parser, so we keep
        // the run modest.
        for marker in ["*", "_", "~", "["] {
            let rows = render_markdown(&marker.repeat(10_000), 1000, &opts());
            assert!(
                rows.len() < 1_000,
                "`{marker}` run produced {} rows",
                rows.len(),
            );
        }
    }

    #[test]
    fn expand_tabs_replaces_each_tab_with_three_spaces() {
        assert_eq!(expand_tabs("\thello"), "   hello");
        assert_eq!(expand_tabs("\t\thi"), "      hi");
        assert_eq!(expand_tabs("no tabs"), "no tabs");
    }

    #[test]
    fn code_block_body_keeps_normalized_tab_indent() {
        let blocks = parse_markdown(&expand_tabs("```\n\thello\n```"));
        match blocks.first() {
            Some(Block::CodeBlock(lang, body)) => {
                assert_eq!(lang.as_deref(), None);
                assert_eq!(body, "   hello");
            }
            other => panic!("expected a code block, got {other:?}"),
        }
    }

    #[test]
    fn double_tilde_parses_as_strikethrough() {
        let blocks = parse_markdown("Use ~~struck~~ here");
        let inlines = paragraph_inlines(&blocks);
        assert!(any_strikethrough(inlines), "got {inlines:?}");
        assert_eq!(inline_plain_text(inlines), "Use struck here");
    }

    #[test]
    fn loose_double_tilde_stays_literal() {
        // Whitespace-padded tildes fail GFM's flanking rule and render as
        // text rather than opening a strikethrough span.
        let blocks = parse_markdown("Use ~~ foo ~~ here");
        let inlines = paragraph_inlines(&blocks);
        assert!(!any_strikethrough(inlines), "got {inlines:?}");
        assert!(inline_plain_text(inlines).contains("~~ foo ~~"));
    }

    #[test]
    fn markdown_link_parses_with_visible_text_and_target() {
        let blocks = parse_markdown("see [the docs](http://example.com/x) now");
        let inlines = paragraph_inlines(&blocks);
        let mut all = Vec::new();
        flatten(inlines, &mut all);
        let link = all
            .iter()
            .find_map(|i| match i {
                Inline::Link(inner, url) => Some((inner, url)),
                _ => None,
            })
            .expect("a link inline");
        assert_eq!(inline_plain_text(link.0), "the docs");
        assert_eq!(link.1, "http://example.com/x");
    }

    #[test]
    fn bare_url_is_autolinked_with_url_as_visible_text() {
        let blocks = parse_markdown("visit http://example.com/path now");
        let inlines = paragraph_inlines(&blocks);
        let mut all = Vec::new();
        flatten(inlines, &mut all);
        let link = all
            .iter()
            .find_map(|i| match i {
                Inline::Link(inner, url) => Some((inner, url.clone())),
                _ => None,
            })
            .expect("an autolinked url");
        // Visible text equals the target: this is what drives the
        // no-parens autolink fallback at render time.
        assert_eq!(inline_plain_text(link.0), "http://example.com/path");
        assert_eq!(link.1, "http://example.com/path");
    }

    #[test]
    fn bare_email_is_autolinked_as_mailto() {
        let blocks = parse_markdown("mail me at foo@example.com please");
        let inlines = paragraph_inlines(&blocks);
        let mut all = Vec::new();
        flatten(inlines, &mut all);
        let link = all
            .iter()
            .find_map(|i| match i {
                Inline::Link(inner, url) => Some((inner, url.clone())),
                _ => None,
            })
            .expect("an autolinked email");
        assert_eq!(inline_plain_text(link.0), "foo@example.com");
        assert_eq!(link.1, "mailto:foo@example.com");
    }

    #[test]
    fn breaks_are_preserved_as_literal_newlines() {
        // Soft break, two-space hard break, and backslash hard break all
        // map to a literal `\n`, and the break markers are consumed.
        for src in [
            "first line\nsecond line",
            "first line  \nsecond line",
            "first line\\\nsecond line",
        ] {
            let blocks = parse_markdown(src);
            let inlines = paragraph_inlines(&blocks);
            assert_eq!(
                inline_plain_text(inlines),
                "first line\nsecond line",
                "source {src:?}",
            );
        }
    }

    #[test]
    fn intraword_asterisks_emphasize_per_commonmark() {
        // `5*4*3` italicizes `4`; `5**4**3` bolds it.
        assert!(any_italic(paragraph_inlines(&parse_markdown("5*4*3"))));
        assert!(any_bold(paragraph_inlines(&parse_markdown("5**4**3"))));
    }

    #[test]
    fn intraword_underscores_stay_literal() {
        let single = parse_markdown("foo_bar_baz");
        assert!(!any_italic(paragraph_inlines(&single)));
        assert_eq!(inline_plain_text(paragraph_inlines(&single)), "foo_bar_baz");

        let double = parse_markdown("foo__bar__baz");
        assert!(!any_bold(paragraph_inlines(&double)));
        assert_eq!(
            inline_plain_text(paragraph_inlines(&double)),
            "foo__bar__baz",
        );
    }

    #[test]
    fn html_tags_pass_through_as_literal_text() {
        let blocks = parse_markdown("before <thinking>middle</thinking> after");
        let inlines = paragraph_inlines(&blocks);
        let plain = inline_plain_text(inlines);
        assert!(plain.contains("<thinking>"), "got {plain:?}");
        assert!(plain.contains("</thinking>"), "got {plain:?}");
        assert!(plain.contains("middle"), "got {plain:?}");
    }

    #[test]
    fn empty_and_whitespace_input_parse_to_no_blocks() {
        for src in ["", "   ", "\n\n", "\t\t", " \n\t "] {
            assert!(
                parse_markdown(&expand_tabs(src)).is_empty(),
                "source {src:?} should parse to no blocks",
            );
        }
    }

    #[test]
    fn table_parses_into_complete_ast() {
        // The AST is complete now even though native table layout renders
        // later. Confirm headers, alignments, rows, and raw are populated.
        let blocks = parse_markdown("| A | B |\n| :-- | --: |\n| 1 | 2 |");
        match blocks.first() {
            Some(Block::Table {
                headers,
                alignments,
                rows,
                raw,
            }) => {
                assert_eq!(headers.len(), 2);
                assert_eq!(alignments, &[Alignment::Left, Alignment::Right]);
                assert_eq!(rows.len(), 1);
                assert!(raw.contains("| A | B |"));
            }
            other => panic!("expected a table, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    // Span wrap
    // -----------------------------------------------------------------

    #[test]
    fn greedy_fill_packs_words_to_width() {
        let rows = wrap_spans(&line("hello world this is a test"), 10);
        assert_eq!(rows_text(&rows), vec!["hello", "world this", "is a test"]);
        for row in &rows {
            assert!(row_width(row) <= 10, "row {:?} too wide", row_text(row));
        }
    }

    #[test]
    fn long_word_breaks_char_by_char() {
        let rows = wrap_spans(&line("aaaaaaaaaaaaaaaaaaaa"), 5);
        assert_eq!(rows_text(&rows), vec!["aaaaa", "aaaaa", "aaaaa", "aaaaa"]);
    }

    #[test]
    fn trailing_whitespace_is_trimmed_at_wrap_boundary() {
        // A pure-whitespace input wider than the width collapses to an
        // empty (trimmed) row rather than carrying spaces.
        let rows = wrap_spans(&line("  "), 1);
        assert_eq!(rows.len(), 1);
        assert!(row_width(&rows[0]) <= 1);
        assert_eq!(row_text(&rows[0]), "");
    }

    #[test]
    fn embedded_newline_starts_a_new_row() {
        let rows = wrap_spans(&line("first\nsecond"), 80);
        assert_eq!(rows_text(&rows), vec!["first", "second"]);
    }

    #[test]
    fn cjk_width_is_measured_as_two_columns() {
        // Each ideograph is two columns wide, so at width 4 two fit per row.
        let rows = wrap_spans(&line("你好世界"), 4);
        assert_eq!(rows_text(&rows), vec!["你好", "世界"]);
        for row in &rows {
            assert_eq!(row_width(row), 4);
        }
    }

    #[test]
    fn wide_grapheme_occupies_its_own_row_at_degenerate_width() {
        // At width 1 a 2-wide grapheme still can't be split. It lands on a
        // row of its own, matching the ANSI long-word break.
        let rows = wrap_spans(&line("你"), 1);
        assert_eq!(row_text(rows.last().unwrap()), "你");
    }

    #[test]
    fn a_word_spanning_two_styles_is_treated_as_one_token() {
        // "aabb" has no space, so it is a single word even though its two
        // halves carry different styles. At width 3 it breaks at the char
        // level ("aab" / "b"), never at the style boundary. A per-span
        // tokenizer would wrongly split it into "aa" / "bb".
        let spans = vec![
            StyledSpan::plain("aa", SpanKind::Text),
            StyledSpan::plain("bb", SpanKind::InlineCode),
        ];
        let rows = wrap_spans(&spans, 3);
        assert_eq!(rows_text(&rows), vec!["aab", "b"]);
        assert_eq!(rows[0][0].kind, SpanKind::Text);
        assert_eq!(rows[0][1].kind, SpanKind::InlineCode);
        assert_eq!(rows[1][0].kind, SpanKind::InlineCode);
    }

    #[test]
    fn adjacent_same_style_spans_coalesce_in_a_row() {
        let spans = vec![text_span("foo "), text_span("bar")];
        let rows = wrap_spans(&spans, 80);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 1, "same-style spans should merge");
        assert_eq!(rows[0][0].text, "foo bar");
    }

    // -----------------------------------------------------------------
    // Render
    // -----------------------------------------------------------------

    fn opts() -> RenderOpts {
        RenderOpts::default()
    }

    /// A repeated `(lang, code)` returns the very same cached handle (a hit,
    /// not a recompute) and adds no slot, while a different key adds one.
    #[test]
    fn highlight_code_memoizes_by_lang_and_code() {
        reset_highlight_cache();
        let code = "let x = 1;\nfn main() {}\n";

        let first = highlight_code(code, Some("rust"));
        assert_eq!(highlight_cache_len(), 1, "miss populated one slot");

        let second = highlight_code(code, Some("rust"));
        assert!(Rc::ptr_eq(&first, &second), "hit returns the cached handle");
        assert_eq!(highlight_cache_len(), 1, "hit adds no slot");

        // Same source, different language is a distinct key.
        let _python = highlight_code(code, Some("python"));
        assert_eq!(highlight_cache_len(), 2, "distinct key adds a slot");
    }

    /// The memo is bounded: past capacity it evicts, so a long code-heavy
    /// session cannot grow it without limit.
    #[test]
    fn highlight_cache_is_bounded() {
        reset_highlight_cache();
        for i in 0..(HIGHLIGHT_CACHE_CAPACITY + 32) {
            let code = format!("const N{i}: usize = {i};");
            let _ = highlight_code(&code, Some("rust"));
        }
        assert_eq!(
            highlight_cache_len(),
            HIGHLIGHT_CACHE_CAPACITY,
            "cache stays at capacity"
        );
    }

    /// Eviction drops the coldest slot: a key kept warm by a recent lookup
    /// survives an over-capacity insert, while the least-recently-used one is
    /// dropped and recomputed on its next lookup.
    #[test]
    fn highlight_cache_evicts_the_coldest_slot() {
        reset_highlight_cache();
        let cold_src = "const COLD: u8 = 0;";
        let warm_src = "const WARM: u8 = 1;";
        // `cold` is inserted first, so it starts as the least-recently-used.
        let cold = highlight_code(cold_src, Some("rust"));
        let warm = highlight_code(warm_src, Some("rust"));
        for i in 0..(HIGHLIGHT_CACHE_CAPACITY - 2) {
            let code = format!("const N{i}: u8 = 2;");
            let _ = highlight_code(&code, Some("rust"));
        }
        assert_eq!(highlight_cache_len(), HIGHLIGHT_CACHE_CAPACITY);

        // Touch `warm` so `cold` is now the coldest, then insert one more
        // distinct key to force exactly one eviction.
        let _ = highlight_code(warm_src, Some("rust"));
        let _ = highlight_code("const EXTRA: u8 = 3;", Some("rust"));
        assert_eq!(highlight_cache_len(), HIGHLIGHT_CACHE_CAPACITY);

        // `warm` is still the same cached allocation (a hit), `cold` was
        // evicted and comes back as a fresh allocation (a miss).
        assert!(
            Rc::ptr_eq(&warm, &highlight_code(warm_src, Some("rust"))),
            "warm key survived eviction"
        );
        assert!(
            !Rc::ptr_eq(&cold, &highlight_code(cold_src, Some("rust"))),
            "cold key was evicted"
        );
    }

    #[test]
    fn empty_input_renders_no_rows() {
        assert!(render_markdown("", 80, &opts()).is_empty());
        assert!(render_markdown("   \n\t\n", 80, &opts()).is_empty());
    }

    #[test]
    fn h1_renders_bold_underlined_heading_text_without_a_marker() {
        let rows = render_markdown("# Title", 80, &opts());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), 1);
        let span = &rows[0][0];
        assert_eq!(span.text, "Title");
        assert_eq!(span.kind, SpanKind::Heading(1));
        assert!(span.emphasis.bold);
        assert!(span.emphasis.underline);
    }

    #[test]
    fn h3_prefixes_the_heading_with_hashes() {
        let rows = render_markdown("### Sub", 80, &opts());
        assert_eq!(rows.len(), 1);
        // Prefix and body share the heading style, so they coalesce.
        assert_eq!(row_text(&rows[0]), "### Sub");
        assert_eq!(rows[0][0].kind, SpanKind::Heading(3));
        assert!(rows[0][0].emphasis.bold);
        assert!(!rows[0][0].emphasis.underline);
    }

    #[test]
    fn paragraph_inline_emphasis_sets_bits_on_the_span() {
        let rows = render_markdown("a **b** c", 80, &opts());
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row_text(row), "a b c");
        let bold = row
            .iter()
            .find(|s| s.text == "b")
            .expect("a span carrying `b`");
        assert!(bold.emphasis.bold);
        assert_eq!(bold.kind, SpanKind::Text);
    }

    #[test]
    fn nested_emphasis_composes_bits() {
        let rows = render_markdown("**_x_**", 80, &opts());
        let span = &rows[0][0];
        assert_eq!(span.text, "x");
        assert!(span.emphasis.bold);
        assert!(span.emphasis.italic);
    }

    #[test]
    fn inline_code_gets_its_own_kind() {
        let rows = render_markdown("use `x` now", 80, &opts());
        let row = &rows[0];
        let code = row
            .iter()
            .find(|s| s.kind == SpanKind::InlineCode)
            .expect("an inline-code span");
        assert_eq!(code.text, "x");
    }

    #[test]
    fn link_without_hyperlinks_appends_the_visible_url() {
        let rows = render_markdown("[text](http://ex.com)", 80, &opts());
        let row = &rows[0];
        let link_text = &row[0];
        assert_eq!(link_text.text, "text");
        assert_eq!(link_text.kind, SpanKind::LinkText);
        assert!(link_text.emphasis.underline);
        assert_eq!(link_text.link.as_deref(), Some("http://ex.com"));
        let url = &row[1];
        assert_eq!(url.text, " (http://ex.com)");
        assert_eq!(url.kind, SpanKind::LinkUrl);
    }

    #[test]
    fn link_with_hyperlinks_omits_the_visible_url() {
        let render_opts = RenderOpts {
            hyperlinks: true,
            ..Default::default()
        };
        let rows = render_markdown("[text](http://ex.com)", 80, &render_opts);
        let row = &rows[0];
        assert_eq!(row.len(), 1, "no visible url span when hyperlinks are on");
        assert_eq!(row[0].kind, SpanKind::LinkText);
        assert_eq!(row[0].link.as_deref(), Some("http://ex.com"));
    }

    #[test]
    fn autolink_never_appends_a_redundant_url() {
        // Visible text equals the target, so no ` (url)` is added even when
        // hyperlinks are off.
        let rows = render_markdown("http://ex.com/path", 80, &opts());
        let row = &rows[0];
        assert!(row.iter().all(|s| s.kind != SpanKind::LinkUrl));
        assert_eq!(row_text(row), "http://ex.com/path");
        assert_eq!(row[0].kind, SpanKind::LinkText);
    }

    #[test]
    fn code_block_emits_border_and_body_rows() {
        let rows = render_markdown("```rust\nlet x = 1;\n```", 80, &opts());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0].kind, SpanKind::CodeBlockBorder);
        assert_eq!(rows[0][0].text, "```rust");
        // The body row is split into highlighted runs, all `CodeBlock`
        // kind, with the indent folded into a leading no-syntax span. Its
        // concatenated text still reproduces the indented source line.
        assert!(rows[1].iter().all(|s| s.kind == SpanKind::CodeBlock));
        assert_eq!(row_text(&rows[1]), "  let x = 1;");
        assert_eq!(rows[2][0].kind, SpanKind::CodeBlockBorder);
        assert_eq!(rows[2][0].text, "```");
    }

    #[test]
    fn horizontal_rule_fills_and_caps_at_eighty() {
        let wide = render_markdown("---", 100, &opts());
        assert_eq!(wide.len(), 1);
        assert_eq!(wide[0][0].kind, SpanKind::Hr);
        assert_eq!(wide[0][0].text.chars().count(), 80);

        let narrow = render_markdown("---", 10, &opts());
        assert_eq!(narrow[0][0].text.chars().count(), 10);
    }

    #[test]
    fn blocks_are_separated_by_a_single_blank_row() {
        let rows = render_markdown("para one\n\npara two", 80, &opts());
        assert_eq!(rows.len(), 3);
        assert_eq!(row_text(&rows[0]), "para one");
        assert!(is_blank_row(&rows[1]));
        assert_eq!(row_text(&rows[2]), "para two");
    }

    #[test]
    fn no_trailing_blank_row_after_the_final_block() {
        let rows = render_markdown("# Heading", 80, &opts());
        assert!(!rows.last().is_none_or(is_blank_row));
    }

    #[test]
    fn mixed_document_places_one_blank_between_top_level_blocks() {
        // Pins the net blank-row placement across a document that mixes a
        // paragraph, a list, a multi-paragraph blockquote, and a trailing
        // paragraph. The spacer-ownership contract: exactly one blank
        // between adjacent top-level blocks, no blank between list items, a
        // bordered blank between the quote's two paragraphs, and no
        // trailing blank.
        let rows = render_markdown("Para.\n\n- a\n- b\n\n> q1\n>\n> q2\n\nafter", 80, &opts());
        assert_eq!(
            rows_text(&rows),
            vec![
                "Para.", "", "- a", "- b", "", "│ q1", "│ ", "│ q2", "", "after",
            ],
        );
    }

    #[test]
    fn default_emphasis_styles_paragraph_text_but_not_headings() {
        let render_opts = RenderOpts {
            default_emphasis: Emphasis {
                italic: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let para = render_markdown("plain text", 80, &render_opts);
        assert!(para[0][0].emphasis.italic, "paragraph text picks up italic");

        let heading = render_markdown("# Head", 80, &render_opts);
        assert!(
            !heading[0][0].emphasis.italic,
            "headings ignore default emphasis",
        );
        assert!(heading[0][0].emphasis.bold);
    }

    #[test]
    fn degenerate_width_never_exceeds_one_column_and_keeps_content() {
        let rows = render_markdown("hello world", 1, &opts());
        for row in &rows {
            assert!(row_width(row) <= 1, "row {:?} too wide", row_text(row));
        }
        let joined: String = rows.iter().map(row_text).collect();
        assert!(joined.contains('h') && joined.contains('w'));
    }

    // -----------------------------------------------------------------
    // Lists
    // -----------------------------------------------------------------

    /// The `ListMarker` span leading `row`, or `None` when the row does
    /// not open with one.
    fn marker(row: &MarkdownRow) -> Option<&StyledSpan> {
        row.first().filter(|s| s.kind == SpanKind::ListMarker)
    }

    #[test]
    fn nested_unordered_list_indents_two_spaces_per_level() {
        let rows = render_markdown(
            "- Item 1\n  - Nested 1.1\n  - Nested 1.2\n- Item 2",
            80,
            &opts(),
        );
        let texts = rows_text(&rows);
        assert_eq!(
            texts,
            vec!["- Item 1", "  - Nested 1.1", "  - Nested 1.2", "- Item 2",],
        );
        // Each row opens with a `ListMarker` whose text is exactly the
        // indent plus the bullet, and the content carries `Text` kind.
        assert_eq!(marker(&rows[0]).unwrap().text, "- ");
        assert_eq!(marker(&rows[1]).unwrap().text, "  - ");
        assert_eq!(rows[0][1].kind, SpanKind::Text);
        assert_eq!(rows[0][1].text, "Item 1");
    }

    #[test]
    fn deeply_nested_list_grows_indent_by_level() {
        let rows = render_markdown(
            "- Level 1\n  - Level 2\n    - Level 3\n      - Level 4",
            80,
            &opts(),
        );
        assert_eq!(
            rows_text(&rows),
            vec![
                "- Level 1",
                "  - Level 2",
                "    - Level 3",
                "      - Level 4",
            ],
        );
    }

    #[test]
    fn ordered_list_uses_source_numbers_and_dot_markers() {
        let rows = render_markdown("1. First\n2. Second\n3. Third", 80, &opts());
        assert_eq!(rows_text(&rows), vec!["1. First", "2. Second", "3. Third"]);
        assert_eq!(marker(&rows[0]).unwrap().text, "1. ");
        assert_eq!(marker(&rows[2]).unwrap().text, "3. ");
    }

    #[test]
    fn ordered_numbering_preserved_across_code_block_split() {
        // A code fence between items makes many parsers restart numbering
        // at 1. We preserve the captured source markers, so the three
        // items stay 1./2./3.
        let rows = render_markdown(
            "1. First item\n\n```\ncode\n```\n\n2. Second item\n\n```\nmore\n```\n\n3. Third item",
            80,
            &opts(),
        );
        let numbered: Vec<String> = rows_text(&rows)
            .into_iter()
            .filter(|l| marker_line_number(l).is_some())
            .collect();
        assert_eq!(
            numbered,
            vec!["1. First item", "2. Second item", "3. Third item"]
        );
    }

    /// The ordinal a `N. ` list line opens with, if any.
    fn marker_line_number(line: &str) -> Option<u32> {
        let (num, rest) = line.split_once(". ")?;
        num.parse::<u32>().ok().filter(|_| !rest.is_empty())
    }

    #[test]
    fn ordered_nested_list_numbers_independently_per_level() {
        let rows = render_markdown(
            "1. First\n   1. Nested first\n   2. Nested second\n2. Second",
            80,
            &opts(),
        );
        assert_eq!(
            rows_text(&rows),
            vec![
                "1. First",
                "  1. Nested first",
                "  2. Nested second",
                "2. Second",
            ],
        );
    }

    #[test]
    fn mixed_ordered_and_unordered_nesting_renders_each_marker() {
        let rows = render_markdown(
            "1. Ordered item\n   - Unordered nested\n   - Another nested\n\
             2. Second ordered\n   - More nested",
            80,
            &opts(),
        );
        let texts = rows_text(&rows);
        assert!(texts.iter().any(|l| l == "1. Ordered item"));
        assert!(texts.iter().any(|l| l == "  - Unordered nested"));
        assert!(texts.iter().any(|l| l == "2. Second ordered"));
        assert!(texts.iter().any(|l| l == "  - More nested"));
    }

    #[test]
    fn long_list_item_continuation_lands_flush_left() {
        // A long item wraps, and because the shared wrap carries no hang
        // indent the continuation row starts at column 0, not under the
        // bullet.
        let rows = render_markdown("- alpha beta gamma delta", 12, &opts());
        let texts = rows_text(&rows);
        assert_eq!(texts[0], "- alpha beta");
        // Continuation rows have no marker and start flush-left.
        for row in &rows[1..] {
            assert!(
                marker(row).is_none(),
                "continuation must not carry a marker"
            );
        }
        assert!(!texts[1].starts_with(' '), "continuation is flush-left");
        let joined = texts.join(" ");
        assert!(joined.contains("gamma") && joined.contains("delta"));
    }

    #[test]
    fn list_item_paragraph_sub_block_stays_tight() {
        // A loose item with a second paragraph renders both on adjacent
        // rows with no blank spacer between them: the neutral model's
        // sub-blocks carry no trailing spacer.
        let rows = render_markdown("- first para\n\n  second para\n- next item", 80, &opts());
        let texts = rows_text(&rows);
        assert_eq!(texts, vec!["- first para", "second para", "- next item"],);
    }

    #[test]
    fn list_renders_a_single_trailing_blank_before_the_next_block() {
        let rows = render_markdown("- a\n- b\n\nafter", 80, &opts());
        let texts = rows_text(&rows);
        assert_eq!(texts, vec!["- a", "- b", "", "after"]);
    }

    // -----------------------------------------------------------------
    // Blockquotes
    // -----------------------------------------------------------------

    /// True when `row` opens with the `│ ` quote border.
    fn has_border(row: &MarkdownRow) -> bool {
        row.first()
            .is_some_and(|s| s.kind == SpanKind::QuoteBorder && s.text == "│ ")
    }

    #[test]
    fn blockquote_borders_and_retags_text_to_quote_italic() {
        let rows = render_markdown("> hello", 80, &opts());
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(has_border(row));
        let body = &row[1];
        assert_eq!(body.text, "hello");
        assert_eq!(body.kind, SpanKind::Quote);
        assert!(body.emphasis.italic, "quote text is italic");
    }

    #[test]
    fn multiline_blockquote_borders_every_row() {
        // A soft break inside the quote keeps each line on its own
        // bordered row.
        let rows = render_markdown("> Foo\n> bar", 80, &opts());
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(has_border));
        assert_eq!(row_text(&rows[0]), "│ Foo");
        assert_eq!(row_text(&rows[1]), "│ bar");
    }

    #[test]
    fn blockquote_wraps_long_lines_and_borders_each_wrapped_row() {
        let long = "This is a very long blockquote line that should wrap across rows";
        let rows = render_markdown(&format!("> {long}"), 24, &opts());
        assert!(rows.len() > 1, "expected the quote to wrap, got {rows:?}");
        for row in &rows {
            assert!(
                has_border(row),
                "each wrapped row keeps the border: {row:?}"
            );
            assert!(row_width(row) <= 24, "row too wide: {:?}", row_text(row));
        }
    }

    #[test]
    fn blockquote_mid_blank_row_keeps_border() {
        // Two paragraphs inside the quote produce a mid blank row, which
        // still gets the border prefix (`│ `) rather than rendering bare.
        let rows = render_markdown("> first\n>\n> second", 80, &opts());
        assert_eq!(rows.len(), 3);
        assert_eq!(row_text(&rows[0]), "│ first");
        // The middle row is the border alone.
        assert_eq!(rows[1].len(), 1);
        assert!(has_border(&rows[1]));
        assert_eq!(row_text(&rows[1]), "│ ");
        assert_eq!(row_text(&rows[2]), "│ second");
    }

    #[test]
    fn code_block_inside_blockquote_keeps_its_own_kinds() {
        let rows = render_markdown("> ```\n> let x = 1;\n> ```", 80, &opts());
        // Three quoted rows: open fence, body, close fence.
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(has_border));
        // The fence rows keep `CodeBlockBorder`, the body `CodeBlock`,
        // none of them get retagged to `Quote`.
        assert_eq!(rows[0][1].kind, SpanKind::CodeBlockBorder);
        assert_eq!(rows[2][1].kind, SpanKind::CodeBlockBorder);
        assert!(rows[1][1..].iter().all(|s| s.kind == SpanKind::CodeBlock));
        assert_eq!(row_text(&rows[1]), "│   let x = 1;");
    }

    #[test]
    fn list_inside_blockquote_keeps_list_marker_kind() {
        let rows = render_markdown("> 1. bla bla\n> - nested bullet", 80, &opts());
        assert!(rows.iter().all(has_border));
        let ordered = rows
            .iter()
            .find(|r| row_text(r).contains("1. bla bla"))
            .expect("ordered item row");
        // The marker keeps `ListMarker`; only the item text is retagged
        // to `Quote`.
        assert_eq!(ordered[1].kind, SpanKind::ListMarker);
        assert_eq!(ordered[1].text, "1. ");
        assert_eq!(ordered[2].kind, SpanKind::Quote);
        assert!(ordered[2].emphasis.italic);
        assert!(rows.iter().any(|r| row_text(r).contains("- nested bullet")));
    }

    #[test]
    fn heading_inside_blockquote_keeps_heading_kind() {
        let rows = render_markdown("> # Title", 80, &opts());
        assert_eq!(rows.len(), 1);
        assert!(has_border(&rows[0]));
        assert_eq!(rows[0][1].kind, SpanKind::Heading(1));
        assert_eq!(rows[0][1].text, "Title");
    }

    #[test]
    fn nested_blockquote_stacks_borders() {
        let rows = render_markdown("> > deep", 80, &opts());
        assert_eq!(rows.len(), 1);
        // The two stacked borders share styling, so the wrap coalesces
        // them into one `QuoteBorder` span, then the retagged text.
        assert_eq!(rows[0].len(), 2);
        assert_eq!(rows[0][0].kind, SpanKind::QuoteBorder);
        assert_eq!(rows[0][0].text, "│ │ ");
        assert_eq!(row_text(&rows[0]), "│ │ deep");
        let body = &rows[0][1];
        assert_eq!(body.kind, SpanKind::Quote);
        assert_eq!(body.text, "deep");
    }

    #[test]
    fn blockquote_inline_code_keeps_inline_code_kind() {
        let rows = render_markdown("> use `x` here", 80, &opts());
        let row = &rows[0];
        assert!(has_border(row));
        let code = row
            .iter()
            .find(|s| s.kind == SpanKind::InlineCode)
            .expect("inline code keeps its kind inside a quote");
        assert_eq!(code.text, "x");
        // Surrounding prose is retagged to quote+italic.
        assert!(
            row.iter()
                .any(|s| s.kind == SpanKind::Quote && s.emphasis.italic)
        );
    }

    #[test]
    fn blockquote_does_not_inherit_default_emphasis() {
        // `default_emphasis` is a paragraph-only knob. Inside a quote it is
        // cleared, so quoted prose carries only the quote's own italic and
        // not, say, a caller's bold. If it leaked through, the quote text
        // would come back bold as well.
        let render_opts = RenderOpts {
            default_emphasis: Emphasis {
                bold: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let rows = render_markdown("> quoted", 80, &render_opts);
        let body = &rows[0][1];
        assert_eq!(body.kind, SpanKind::Quote);
        assert!(body.emphasis.italic, "quote text stays italic");
        assert!(
            !body.emphasis.bold,
            "default emphasis must not leak into the quote",
        );
    }

    #[test]
    fn list_item_does_not_inherit_default_emphasis() {
        // `default_emphasis` is paragraph-only, so a list rendered inside an
        // italic thinking block keeps its item text upright rather than
        // carrying the caller's emphasis.
        let render_opts = RenderOpts {
            default_emphasis: Emphasis {
                bold: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let rows = render_markdown("- item", 80, &render_opts);
        let content = &rows[0][1];
        assert_eq!(content.kind, SpanKind::Text);
        assert!(
            !content.emphasis.bold,
            "default emphasis must not leak into list item content",
        );
    }

    // -----------------------------------------------------------------
    // Code-block syntax highlighting
    // -----------------------------------------------------------------

    /// The set of syntax categories present on a body row.
    fn categories(rows: &[MarkdownRow]) -> Vec<SyntaxCategory> {
        rows.iter()
            .flat_map(|r| r.iter())
            .filter(|s| s.kind == SpanKind::CodeBlock)
            .filter_map(|s| s.syntax)
            .collect()
    }

    #[test]
    fn rust_code_block_classifies_keyword_and_comment() {
        let rows = render_markdown("```rust\n// note\nlet y = 2;\n```", 80, &opts());
        let cats = categories(&rows);
        assert!(
            cats.contains(&SyntaxCategory::Comment),
            "comment token should classify as Comment: {cats:?}",
        );
        assert!(
            cats.contains(&SyntaxCategory::Keyword),
            "`let` should classify as Keyword: {cats:?}",
        );
        assert!(
            cats.contains(&SyntaxCategory::Number),
            "`2` should classify as Number: {cats:?}",
        );
    }

    #[test]
    fn code_block_syntax_spans_carry_category_on_the_syntax_field() {
        let rows = render_markdown("```rust\nlet y = 2;\n```", 80, &opts());
        let body = &rows[1];
        let keyword = body
            .iter()
            .find(|s| s.syntax == Some(SyntaxCategory::Keyword))
            .expect("a keyword-classified span");
        assert_eq!(keyword.kind, SpanKind::CodeBlock);
        assert_eq!(keyword.text, "let");
    }

    #[test]
    fn unknown_language_leaves_every_run_uncategorized() {
        let rows = render_markdown("```qwerty\nlet y = 2;\n```", 80, &opts());
        // An unknown token falls back to plain-text syntax, so no run gets
        // a category, and the body is preserved verbatim.
        assert!(categories(&rows).is_empty());
        assert_eq!(row_text(&rows[1]), "  let y = 2;");
    }

    #[test]
    fn code_block_without_a_language_is_plain() {
        let rows = render_markdown("```\nlet y = 2;\n```", 80, &opts());
        assert!(categories(&rows).is_empty());
        assert_eq!(row_text(&rows[1]), "  let y = 2;");
    }

    #[test]
    fn block_comment_scope_carries_across_lines() {
        // A single `ParseState`/`ScopeStack` spans the whole block, so a
        // Rust `/* ... */` comment opened on one line keeps its comment
        // scope on the next line's content.
        let rows = render_markdown("```rust\n/* still\ncomment */\n```", 80, &opts());
        let cats = categories(&rows);
        assert!(
            cats.iter()
                .filter(|c| **c == SyntaxCategory::Comment)
                .count()
                >= 2,
            "both comment lines should classify as Comment: {cats:?}",
        );
    }

    #[test]
    fn highlighted_run_preserves_its_category_across_a_wrap() {
        // Wrapping a long code line splits spans, and each split copies
        // the originating run's `syntax`, so the category survives the
        // break rather than resetting to `None`.
        let rows = render_markdown(
            "```rust\nlet reallylongidentifiername = 2;\n```",
            12,
            &opts(),
        );
        // The body wrapped over several rows. A keyword classification is
        // still present somewhere among them.
        assert!(
            categories(&rows).contains(&SyntaxCategory::Keyword),
            "keyword category should survive wrapping: {:?}",
            rows_text(&rows),
        );
    }

    // -----------------------------------------------------------------
    // Tables
    // -----------------------------------------------------------------

    #[test]
    fn distribute_returns_natural_widths_when_everything_fits() {
        // Every column's natural width fits the budget, so each column is
        // allocated its natural width (raised to its floor where larger).
        let widths = distribute_column_widths(&[10, 5, 8], &[3, 3, 3], 100);
        assert_eq!(widths, vec![10, 5, 8]);
    }

    #[test]
    fn distribute_never_exceeds_budget_when_shrinking() {
        // The allocation stays within budget even when the natural widths
        // overflow it, and every column keeps at least one column.
        let widths = distribute_column_widths(&[40, 40, 40], &[5, 5, 5], 30);
        assert!(
            widths.iter().sum::<usize>() <= 30,
            "allocation {widths:?} overflowed the budget",
        );
        assert!(widths.iter().all(|&w| w >= 1));
    }

    #[test]
    fn distribute_collapses_when_minimums_exceed_budget() {
        // When even the per-column floors don't fit, columns collapse and
        // the budget is shared out by floor weight, still one column each
        // and within budget.
        let widths = distribute_column_widths(&[30, 30], &[30, 30], 10);
        assert_eq!(widths.len(), 2);
        assert!(widths.iter().all(|&w| w >= 1));
        assert!(widths.iter().sum::<usize>() <= 10);
    }

    #[test]
    fn distribute_capped_floor_leaves_room_for_neighbours() {
        // Column 0 holds a 60-wide token capped to `MAX_UNBROKEN_TOKEN_WIDTH`
        // so it does not pin itself to the token width and starve column 1.
        let cap = MAX_UNBROKEN_TOKEN_WIDTH;
        let widths = distribute_column_widths(&[60, 40], &[cap, 8], 53);
        assert_eq!(widths.len(), 2);
        assert!(widths.iter().sum::<usize>() <= 53);
        assert!(
            widths[1] >= 8,
            "neighbour column should keep at least its floor; got {widths:?}",
        );
        assert!(
            widths[0] < 60,
            "capped column should be narrower than its overlong token; got {widths:?}",
        );
    }

    #[test]
    fn renders_a_basic_two_column_table() {
        let rows = render_markdown("| A | B |\n| --- | --- |\n| 1 | 2 |", 80, &opts());
        assert_eq!(
            rows_text(&rows),
            vec![
                "┌───┬───┐",
                "│ A │ B │",
                "├───┼───┤",
                "│ 1 │ 2 │",
                "└───┴───┘"
            ],
        );
        // Top, separator, and bottom are single `TableBorder` rows.
        for i in [0, 2, 4] {
            assert_eq!(rows[i].len(), 1, "border row {i} is a single span");
            assert_eq!(rows[i][0].kind, SpanKind::TableBorder);
        }
        // Content rows open with a `│` `TableBorder` and carry `TableCell`
        // text between the frame spaces.
        let header = &rows[1];
        assert_eq!(header[0].kind, SpanKind::TableBorder);
        assert_eq!(header[0].text, "│");
        assert!(
            header
                .iter()
                .any(|s| s.kind == SpanKind::TableCell && s.text == "A"),
        );
        assert!(
            rows[3]
                .iter()
                .any(|s| s.kind == SpanKind::TableCell && s.text == "1"),
        );
    }

    #[test]
    fn renders_table_alignments_left_center_right() {
        let rows = render_markdown(
            "| Left | Center | Right |\n| :--- | :---: | ---: |\n| a | b | c |",
            80,
            &opts(),
        );
        let texts = rows_text(&rows);
        assert_eq!(
            texts.len(),
            5,
            "top, header, separator, one data row, bottom"
        );

        // Splitting a content row on `│` yields the per-column cell text
        // framed by its two padding spaces, which pins the padding placement:
        // left keeps content flush-left, right flush-right, center split with
        // the odd column biased right.
        let header_segments: Vec<&str> = texts[1].split('│').collect();
        assert_eq!(
            header_segments,
            vec!["", " Left ", " Center ", " Right ", ""]
        );

        let data_segments: Vec<&str> = texts[3].split('│').collect();
        assert_eq!(data_segments, vec!["", " a    ", "   b    ", "     c ", ""]);
    }

    #[test]
    fn wraps_a_cell_wider_than_its_column_across_visual_rows() {
        // At width 20 the columns distribute to [12, 1], so the long first
        // cell wraps to two visual rows while the short neighbour renders on
        // the first and is blank-padded (still aligned) on the continuation.
        let rows = render_markdown(
            "| A | B |\n| --- | --- |\n| one two three four | x |",
            20,
            &opts(),
        );
        let texts = rows_text(&rows);
        assert_eq!(
            texts.len(),
            6,
            "top, header, separator, two data rows, bottom"
        );
        // Every row (borders and both visual lines of the wrapped data row)
        // is the same width, so the table is a clean rectangle: the
        // continuation line's blank-padded neighbour keeps the `│` aligned.
        for row in &rows {
            assert_eq!(
                row_width(row),
                row_width(&rows[0]),
                "row not rectangular: {:?}",
                row_text(row),
            );
        }

        let data0 = &texts[3];
        let data1 = &texts[4];
        assert!(
            data0.contains("one two") && !data0.contains("three"),
            "first visual row holds the leading words: {data0:?}",
        );
        assert!(
            data1.contains("three four"),
            "continuation row holds the wrapped words: {data1:?}",
        );

        // The neighbour column holds `x` on the first row and is blank on the
        // continuation.
        let seg0: Vec<&str> = data0.split('│').collect();
        assert_eq!(seg0.get(2).map(|s| s.trim()), Some("x"));
        let seg1: Vec<&str> = data1.split('│').collect();
        assert_eq!(
            seg1.get(2).map(|s| s.trim()),
            Some(""),
            "neighbour column is blank-padded on the wrap continuation: {data1:?}",
        );
    }

    #[test]
    fn narrow_width_falls_back_to_raw_prose() {
        // Three columns need chrome 10, so width 12 leaves only 2 cells of
        // budget (< n_cols): a bordered table can't render, and we fall back
        // to the raw source wrapped as prose. No box-drawing chrome appears.
        let rows = render_markdown(
            "| A | B | C |\n| --- | --- | --- |\n| 1 | 2 | 3 |",
            12,
            &opts(),
        );
        for row in &rows {
            assert!(row_width(row) <= 12, "row too wide: {:?}", row_text(row));
            for span in row {
                assert_ne!(
                    span.kind,
                    SpanKind::TableBorder,
                    "fallback must not draw table borders: {:?}",
                    rows_text(&rows),
                );
                assert_ne!(span.kind, SpanKind::TableCell);
            }
        }
        let joined = rows_text(&rows).join("\n");
        assert!(
            joined.contains('|'),
            "raw ASCII pipes are preserved: {joined:?}"
        );
        assert!(
            !joined.contains('│'),
            "no box-drawing borders in the fallback: {joined:?}",
        );
        assert!(joined.contains('A') && joined.contains('B') && joined.contains('C'));
    }

    #[test]
    fn overlong_token_column_leaves_room_for_its_neighbour() {
        // A 60-char unbreakable token in column 0 is capped to
        // `MAX_UNBROKEN_TOKEN_WIDTH`, so column 1 keeps a usable share and
        // the token hard-breaks across rows rather than widening its column.
        let token = "a".repeat(60);
        let src = format!("| Link | Note |\n| --- | --- |\n| {token} | readable |");
        let rows = render_markdown(&src, 50, &opts());
        for row in &rows {
            assert!(row_width(row) <= 50, "row too wide: {:?}", row_text(row));
        }
        let texts = rows_text(&rows);
        assert!(
            texts.iter().any(|l| l.contains("readable")),
            "neighbour column content survives: {texts:?}",
        );
        assert!(
            !texts.iter().any(|l| l.contains(&token)),
            "overlong token wraps rather than widening its column: {texts:?}",
        );
    }

    #[test]
    fn table_cell_inline_code_keeps_its_kind() {
        let rows = render_markdown("| Code |\n| --- |\n| `x` |", 80, &opts());
        let data = rows
            .iter()
            .find(|r| r.iter().any(|s| s.kind == SpanKind::InlineCode))
            .expect("a data row with inline code");
        let code = data
            .iter()
            .find(|s| s.kind == SpanKind::InlineCode)
            .expect("an inline-code span in the cell");
        assert_eq!(code.text, "x");
        // The cell still frames with table borders.
        assert_eq!(data[0].kind, SpanKind::TableBorder);
    }

    #[test]
    fn table_ends_without_a_trailing_blank_row() {
        let rows = render_markdown("| Name |\n| --- |\n| Alice |", 80, &opts());
        assert!(
            !rows.last().is_none_or(is_blank_row),
            "table must not end in a blank row",
        );
        let last = rows.last().unwrap();
        assert_eq!(last[0].kind, SpanKind::TableBorder);
        assert!(
            row_text(last).starts_with('└'),
            "final row is the bottom border"
        );
    }

    #[test]
    fn table_is_followed_by_a_single_blank_before_the_next_block() {
        // The outer render loop owns the inter-block spacer, so the table
        // contributes no trailing blank of its own: exactly one blank sits
        // between the bottom border and the following paragraph.
        let rows = render_markdown("| A |\n| --- |\n| 1 |\n\nafter", 80, &opts());
        let texts = rows_text(&rows);
        assert_eq!(texts.last().map(String::as_str), Some("after"));
        assert_eq!(
            texts.iter().filter(|l| l.is_empty()).count(),
            1,
            "exactly one blank row: {texts:?}",
        );
        let blank_idx = texts.iter().position(String::is_empty).unwrap();
        assert!(
            texts[blank_idx - 1].starts_with('└'),
            "blank follows the bottom border"
        );
        assert_eq!(texts[blank_idx + 1], "after");
    }

    #[test]
    fn table_inside_a_blockquote_keeps_the_quote_border_on_every_row() {
        // A table nested in a blockquote reaches `render_table` through the
        // blockquote recursion. Every table row (border and content alike)
        // is prefixed with the `│ ` quote border, the box-drawing glyphs
        // keep `TableBorder`, and the cells keep `TableCell`. Only the
        // invisible framing spaces (plain `Text`) are retagged to `Quote`.
        let rows = render_markdown("> | A | B |\n> | --- | --- |\n> | 1 | 2 |", 80, &opts());
        assert_eq!(
            rows_text(&rows),
            vec![
                "│ ┌───┬───┐",
                "│ │ A │ B │",
                "│ ├───┼───┤",
                "│ │ 1 │ 2 │",
                "│ └───┴───┘",
            ],
        );
        for row in &rows {
            assert_eq!(
                row[0].kind,
                SpanKind::QuoteBorder,
                "every row opens with the quote border: {:?}",
                row_text(row),
            );
        }
        // The box chars stay table chrome, not quote text.
        assert!(rows[0].iter().any(|s| s.kind == SpanKind::TableBorder));
        assert!(
            rows[1]
                .iter()
                .any(|s| s.kind == SpanKind::TableCell && s.text == "A"),
        );
        // The table stays a clean rectangle even under the quote border.
        let w = row_width(&rows[0]);
        assert!(
            rows.iter().all(|r| row_width(r) == w),
            "all quoted table rows share one width: {:?}",
            rows_text(&rows),
        );
    }
}
