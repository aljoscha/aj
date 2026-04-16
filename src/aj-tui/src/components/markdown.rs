//! Markdown rendering component.
//!
//! Renders markdown text to styled terminal lines using ANSI escape codes.
//! Supports headings, paragraphs, code blocks (with syntax highlighting via syntect),
//! lists (ordered and unordered, nested), links, blockquotes, horizontal rules,
//! bold, italic, strikethrough, and inline code.

use std::sync::Arc;

use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::as_24_bit_terminal_escaped;

use crate::ansi::{visible_width, wrap_text_with_ansi};
use crate::capabilities::get_capabilities;
use crate::component::Component;

/// Tabs in the source markdown are normalized to this many spaces before
/// parsing, mirroring pi-tui's `text.replace(/\t/g, "   ")` step at the top
/// of `Markdown::render`. Three spaces (rather than four) matches both
/// pi-tui's chosen visible width and the `Text` component's `TAB_AS_SPACES`
/// constant; the choice is a UX call rather than a CommonMark requirement.
///
/// Visible on tab-indented input: a fenced code block whose body uses
/// hard tabs would otherwise render with a literal `\t` byte (one cell
/// wide on most terminals, but stylistically wrong) instead of the
/// expected indent. List parsing is also affected — `indent_of` counts
/// only space bytes, so a tab-indented continuation line wouldn't be
/// recognized as nested without this normalization.
const TAB_AS_SPACES: &str = "   ";

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// Styling functions for markdown rendering.
///
/// Mirrors pi-tui's `MarkdownTheme` interface
/// (`packages/tui/src/components/markdown.ts`). Pi-tui ships no upstream
/// default theme — the agent layer builds one from its central palette
/// and passes it to [`Markdown::new`]. We deliberately do not provide a
/// `Default` impl: the tui crate stays palette-agnostic, and tests
/// build themes via `tests/support/themes.rs` (mirroring pi's
/// `packages/tui/test/test-themes.ts`).
///
/// The closures use `Arc` rather than `Box` so a single theme can be
/// cheaply cloned (e.g. for snapshot purposes or sharing with a sibling
/// component).
#[derive(Clone)]
pub struct MarkdownTheme {
    pub heading: Arc<dyn Fn(&str) -> String>,
    pub bold: Arc<dyn Fn(&str) -> String>,
    pub italic: Arc<dyn Fn(&str) -> String>,
    pub strikethrough: Arc<dyn Fn(&str) -> String>,
    pub code: Arc<dyn Fn(&str) -> String>,
    pub code_block: Arc<dyn Fn(&str) -> String>,
    pub code_block_border: Arc<dyn Fn(&str) -> String>,
    pub link: Arc<dyn Fn(&str) -> String>,
    pub link_url: Arc<dyn Fn(&str) -> String>,
    pub list_bullet: Arc<dyn Fn(&str) -> String>,
    pub quote_border: Arc<dyn Fn(&str) -> String>,
    pub quote: Arc<dyn Fn(&str) -> String>,
    pub hr: Arc<dyn Fn(&str) -> String>,
    pub underline: Arc<dyn Fn(&str) -> String>,
    /// Optional override for syntax highlighting. Receives the raw code
    /// block contents and the optional language tag and returns one styled
    /// line per input line. When `None`, the built-in syntect-based
    /// highlighter is used.
    pub highlight_code: Option<Arc<dyn Fn(&str, Option<&str>) -> Vec<String>>>,
    /// Prefix applied to each rendered code block line. Defaults to two
    /// spaces when `None`.
    pub code_block_indent: Option<String>,
}

/// Outer styling applied to rendered paragraph lines, independently of
/// the theme's inline styling. Used to tint a whole block of
/// "thinking" prose in, say, dim italic gray while leaving the theme's
/// inline styling (inline code, links, code-block highlighting) intact.
///
/// Pi-name parity (PORTING.md H4). Mirrors pi-tui's `DefaultTextStyle`
/// interface (`packages/tui/src/components/markdown.ts:34-47`). The
/// `Markdown::default_text_style` field, the constructor argument, and
/// the [`apply_default_style`][Markdown::apply_default_style] helper
/// all carry the pi-aligned name so a reader following along from pi
/// finds the same identifiers on both sides.
///
/// Use [`Default::default`] + struct-update syntax to set just the
/// fields you care about, mirroring how a TS consumer writes
/// `{ color: gray, italic: true }` against pi's all-optional
/// interface:
///
/// ```ignore
/// DefaultTextStyle {
///     color: Some(Arc::new(style::gray)),
///     italic: true,
///     ..Default::default()
/// }
/// ```
///
/// **Field-set parity with pi (minus `bgColor`).** Pi's
/// `DefaultTextStyle` exposes six fields: `color`, `bgColor`, `bold`,
/// `italic`, `strikethrough`, `underline`. We mirror five of them —
/// every text-decoration field. `bgColor` is deliberately omitted
/// because pi routes it through the margin/padding-fill stage at the
/// top of [`Markdown::render`] (`markdown.ts:164`: `const bgFn =
/// this.defaultTextStyle?.bgColor;`), not via `applyDefaultStyle`.
/// Adding it here would also require fixing a separate gap that
/// pre-exists this decision: our `Markdown::render` only emits a left
/// padding, while pi emits `leftMargin + line + rightMargin` then pads
/// to `width` and applies `bgFn` over the whole row
/// (`markdown.ts:161-191`). That's a wider component-architecture gap
/// (a separate work item, not a `DefaultTextStyle` field). The
/// observable consumer impact is bounded: pi-coding-agent itself does
/// not use `bgColor` in production — it wraps the [`Markdown`] in a
/// [`crate::components::TextBox`] (pi's `Box`) with a `bg_fn` for
/// per-message backgrounds, an approach that already works in our
/// port.
///
/// **Apply order.** [`Markdown::apply_default_style`] applies these
/// fields in pi's order (color → bold → italic → strikethrough →
/// underline, per `applyDefaultStyle` at `markdown.ts:218-234`), with
/// the text-decoration calls routing through `theme.bold` /
/// `theme.italic` / etc. so a custom theme can override how each
/// decoration is styled. Color is innermost so the open codes nest as
/// `underline strikethrough italic bold color {text} ...closes`.
///
/// **Rendering scope.** Applies to paragraphs only. Does NOT apply to
/// headings (per pi-tui parity, F45), blockquotes (which use
/// `theme.quote`), code blocks (which carry their own highlighting),
/// or horizontal rules. Lists and table cells currently don't receive
/// default-text-styling either; when a test port needs it, extend the
/// render path.
#[derive(Clone, Default)]
pub struct DefaultTextStyle {
    /// Optional color wrapper. Called with the line's already-styled
    /// text and returns the same text with a surrounding SGR color.
    pub color: Option<Arc<dyn Fn(&str) -> String>>,
    /// When `true`, [`Markdown::apply_default_style`] wraps the line
    /// with the configured theme's `bold` styler.
    pub bold: bool,
    /// When `true`, [`Markdown::apply_default_style`] wraps the line
    /// with the configured theme's `italic` styler.
    pub italic: bool,
    /// When `true`, [`Markdown::apply_default_style`] wraps the line
    /// with the configured theme's `strikethrough` styler.
    pub strikethrough: bool,
    /// When `true`, [`Markdown::apply_default_style`] wraps the line
    /// with the configured theme's `underline` styler.
    pub underline: bool,
}

// ---------------------------------------------------------------------------
// Inline-style context (pi-tui parity, F41)
// ---------------------------------------------------------------------------

/// Outer style applied to a sequence of inline tokens, with proper
/// restoration after each non-text inline. Mirrors pi-tui's
/// `InlineStyleContext` (markdown.ts:73-76).
///
/// Two parts:
///
/// - `apply_text` is called on every text run; it wraps the run in the
///   outer style (the heading wrap for headings, the configured
///   pre-style for paragraphs, identity for table cells / list items).
/// - `style_prefix` is just the *opens* of that wrap, extracted via the
///   sentinel trick in [`get_style_prefix`]. It gets appended after
///   each non-text inline (codespan, bold, italic, strikethrough, link)
///   so the outer style re-opens for whatever follows. Without this an
///   inline's own SGR closes (e.g. an inline code's `\x1b[39m`
///   foreground reset) would strip the matching outer state from the
///   trailing text on the same line.
///
/// Trailing dangling `style_prefix` (no text after the last non-text
/// inline) is trimmed in [`Markdown::render_inline_tokens`].
struct InlineStyleContext<'a> {
    apply_text: &'a dyn Fn(&str) -> String,
    style_prefix: &'a str,
}

/// Extract the *opens* of a styling closure as a prefix string.
/// Mirrors pi-tui's `getStylePrefix` (markdown.ts:273-279).
///
/// The closure is called with a NUL byte (U+0000) as input. Whatever
/// the closure emits before the NUL is the opens-only prefix; whatever
/// it emits after the NUL is the closing tail. We slice the output at
/// the NUL position and return the prefix half. Theme wrappers are
/// simple SGR wrappers (`\x1b[Nm{text}\x1b[Mm`) that pass arbitrary
/// content through, so the sentinel survives intact.
///
/// Returns an empty string when the closure swallows the sentinel
/// (not expected for any real wrapper, but defensive).
fn get_style_prefix(wrap: &dyn Fn(&str) -> String) -> String {
    let sentinel = "\u{0}";
    let styled = wrap(sentinel);
    match styled.find(sentinel) {
        Some(idx) => styled[..idx].to_string(),
        None => String::new(),
    }
}

/// Wrap `line` with a quote-style closure, splicing the quote's opens
/// after every full SGR reset (`\x1b[0m`) inside `line` so downstream
/// content reopens the quote styling after the reset. Mirrors pi-tui's
/// `applyQuoteStyle` (markdown.ts:373-379).
///
/// The visible target is fenced code blocks inside `>` quotes:
/// syntect-highlighted code lines terminate with `\x1b[0m`, which would
/// otherwise close the outer quote/italic state for the trailing cells
/// of every highlighted row. Splicing the quote prefix after each reset
/// keeps the styling on through to the line's end.
///
/// `quote_prefix` is allowed to be empty (defensive — the
/// [`get_style_prefix`] sentinel trick returns "" only when the wrapper
/// swallows the sentinel, which neither `theme.quote` nor `theme.italic`
/// does for any real theme): the splice is skipped and the wrap fires
/// directly.
fn apply_quote_style(
    line: &str,
    quote_apply: &dyn Fn(&str) -> String,
    quote_prefix: &str,
) -> String {
    if quote_prefix.is_empty() {
        return quote_apply(line);
    }
    let with_reapplied = line.replace("\x1b[0m", &format!("\x1b[0m{}", quote_prefix));
    quote_apply(&with_reapplied)
}

// ---------------------------------------------------------------------------
// Simple markdown parser
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
    /// body is a full sub-document: the parser strips the `> ` prefix
    /// from each line, recursively parses the result, and stores the
    /// resulting blocks here. The renderer prepends the quote border
    /// to every emitted sub-block line, so a fenced code block, list,
    /// heading, table, or any other block-level element nested inside
    /// a `>` quote renders as its native block. Multi-line plain-text
    /// quotes still render as multiple bordered rows because the
    /// paragraph parser (see [`join_paragraph_lines`]) preserves `\n`
    /// between source lines and downstream `wrap_text_with_ansi`
    /// expands those newlines into visible rows.
    Blockquote(Vec<Block>),
    /// A GitHub-flavored-markdown style table: one header row, one
    /// alignment spec per column, zero or more data rows. Each cell is
    /// pre-parsed inline content. `raw` holds the original markdown
    /// source for the table block (header + separator + body lines
    /// joined with `\n`); it's the fallback content the renderer
    /// wraps when the available width is too narrow to render a
    /// stable table (pi-tui parity, `markdown.ts:696-703` —
    /// `token.raw` falls back through `wrapTextWithAnsi`).
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
    /// Nested blocks (e.g. sub-lists) that belong to this item. Rendered
    /// indented one level below the item's own bullet.
    sub_blocks: Vec<Block>,
    /// For ordered items, the source marker (e.g. `1` in `"1. Foo"`).
    /// Preserved verbatim so that lists split by intervening blocks
    /// don't restart numbering from `1` in the rendered output.
    /// `None` for unordered items.
    number: Option<u32>,
}

/// Inline content within a paragraph or heading.
#[derive(Debug)]
enum Inline {
    Text(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Code(String),
    /// `[text](url)` markdown link, autolinked URL, or autolinked email.
    /// The first field is the parsed inline tokens that make up the
    /// link's *visible* text — so a `[**bold**](url)` keeps the bold
    /// emphasis nested under the link, mirroring pi-tui's
    /// `link.tokens` shape (`markdown.ts:454-476`). For autolinks
    /// (bare URLs, emails) the inner is `vec![Inline::Text(url)]` so
    /// the visible text is the URL itself. The second field is the
    /// URL target.
    Link(Vec<Inline>, String),
}

/// Parse markdown text into blocks.
fn parse_markdown(text: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];

        // Blank source lines are not emitted as blocks — every block's
        // renderer already appends a single trailing blank line, so an
        // explicit blank would double the spacing between blocks.
        if line.trim().is_empty() {
            i += 1;
            continue;
        }

        // Heading.
        if let Some(heading) = parse_heading(line) {
            blocks.push(heading);
            i += 1;
            continue;
        }

        // Horizontal rule.
        let trimmed = line.trim();
        if (trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___"))
            && trimmed
                .chars()
                .all(|c| c == '-' || c == '*' || c == '_' || c == ' ')
            && trimmed.len() >= 3
        {
            blocks.push(Block::HorizontalRule);
            i += 1;
            continue;
        }

        // Fenced code block.
        if trimmed.starts_with("```") {
            let lang = trimmed[3..].trim().to_string();
            let lang = if lang.is_empty() { None } else { Some(lang) };
            let mut code_lines: Vec<&str> = Vec::new();
            i += 1;
            while i < lines.len() {
                if lines[i].trim().starts_with("```") {
                    i += 1;
                    break;
                }
                code_lines.push(lines[i]);
                i += 1;
            }
            blocks.push(Block::CodeBlock(lang, code_lines.join("\n")));
            continue;
        }

        // Blockquote.
        if trimmed.starts_with("> ") || trimmed == ">" {
            let mut quote_lines: Vec<String> = Vec::new();
            while i < lines.len() {
                let l = lines[i].trim();
                if l.starts_with("> ") {
                    quote_lines.push(l[2..].to_string());
                } else if l == ">" {
                    quote_lines.push(String::new());
                } else if !l.is_empty() && !quote_lines.is_empty() {
                    // Lazy continuation: a non-blank line immediately
                    // following a `>` line is still part of the quote.
                    quote_lines.push(l.to_string());
                } else {
                    break;
                }
                i += 1;
            }
            // Recursively parse the stripped quote body so that block-
            // level elements (fenced code blocks, lists, headings,
            // tables, horizontal rules, nested blockquotes) inside a
            // `>` quote render as their native block instead of as
            // literal per-line inline text. Multi-line plain-text
            // quotes (`> Foo\n> bar`) still render as two visible rows
            // because the inner paragraph parser preserves `\n`
            // between source lines (see `join_paragraph_lines`).
            let inner = quote_lines.join("\n");
            let sub_blocks = parse_markdown(&inner);
            blocks.push(Block::Blockquote(sub_blocks));
            continue;
        }

        // Table: GitHub-flavored-markdown tables need a row of cells
        // followed by an alignment separator. We peek at the next line
        // to disambiguate from paragraphs that coincidentally start
        // with `|`.
        if let Some((table, new_i)) = parse_table(&lines, i) {
            blocks.push(table);
            i = new_i;
            continue;
        }

        // Unordered list.
        if line_bullet(line).is_some() {
            let (list, new_i) = parse_list(&lines, i, false);
            blocks.push(list);
            i = new_i;
            continue;
        }

        // Ordered list.
        if line_number(line).is_some() {
            let (list, new_i) = parse_list(&lines, i, true);
            blocks.push(list);
            i = new_i;
            continue;
        }

        // Paragraph: collect lines until a blank line or block start.
        let mut para_lines: Vec<&str> = Vec::new();
        while i < lines.len() {
            let l = lines[i];
            if l.trim().is_empty() {
                break;
            }
            if parse_heading(l).is_some() {
                break;
            }
            if l.trim().starts_with("```") {
                break;
            }
            if l.trim().starts_with("> ") || l.trim() == ">" {
                break;
            }
            let lt = l.trim();
            if line_bullet(l).is_some() || line_number(l).is_some() {
                break;
            }
            // Keep the `lt` binding in scope for the check below; the
            // paragraph collector doesn't use it further.
            let _ = lt;
            para_lines.push(l);
            i += 1;
        }
        if !para_lines.is_empty() {
            let text = join_paragraph_lines(&para_lines);
            blocks.push(Block::Paragraph(parse_inline(&text)));
        }
    }

    blocks
}

/// Join paragraph source lines, choosing either a single space (soft
/// line break — the default) or a literal `\n` (hard line break) at
/// each boundary.
///
/// CommonMark recognizes two hard-line-break markers at the end of a
/// paragraph line:
///
/// 1. Two or more trailing spaces.
/// 2. A single trailing backslash.
///
/// On either marker, the joiner inserts `\n` between this line and the
/// next; the marker itself is stripped so it doesn't render literally.
/// Downstream `wrap_text_with_ansi` splits on `\n` and emits each part
/// as its own visual row, so the hard break renders as an actual line
/// break in the output.
///
/// Without a marker, lines join with a single space — the standard
/// "soft line break renders as a space" behavior. Headings and
/// blockquotes don't go through this helper because each of their
/// source lines already maps to its own rendered row.
/// Join paragraph source lines with literal `\n` between them.
///
/// We deliberately diverge from strict CommonMark "soft break renders as
/// space" semantics: every newline in the source is preserved as a
/// newline in the rendered output. This matches the upstream pi/marked
/// renderer (marked preserves `\n` inside paragraph text tokens; our
/// downstream `wrap_text_with_ansi` splits on `\n` to produce visible
/// rows). The motivation is UX: a CLI user typing a multi-line message
/// expects each typed line to render on its own row — the
/// "concatenate-with-space" CommonMark default isn't a good fit for an
/// agent's chat surface.
///
/// CommonMark also recognizes two explicit hard-line-break markers at
/// the end of a paragraph line:
///
/// 1. Two or more trailing spaces.
/// 2. A single trailing backslash.
///
/// Even though every soft break already inserts a `\n`, we still strip
/// these markers when present so they don't render literally as
/// trailing whitespace or a stray `\\` at end-of-row. The user's intent
/// ("force a line break here") is honored either way; we just keep the
/// marker bytes from leaking into the visible output.
fn join_paragraph_lines(lines: &[&str]) -> String {
    let mut out = String::new();
    let last = lines.len().saturating_sub(1);
    for (idx, line) in lines.iter().enumerate() {
        let (stripped, _hard_break) = split_hard_break(line);
        out.push_str(stripped);
        if idx < last {
            out.push('\n');
        }
    }
    out
}

/// Detect a CommonMark hard-line-break marker at the end of `line` and
/// strip it. The two recognized markers are two or more trailing
/// spaces and a single trailing backslash.
///
/// Returns `(stripped, had_marker)`. `had_marker` is preserved on the
/// return for callers who care about the distinction; current callers
/// ignore it because every paragraph soft break already inserts `\n`
/// (see [`join_paragraph_lines`]).
fn split_hard_break(line: &str) -> (&str, bool) {
    if let Some(stripped) = line.strip_suffix('\\') {
        return (stripped, true);
    }
    let trimmed = line.trim_end_matches(' ');
    let trailing_spaces = line.len() - trimmed.len();
    if trailing_spaces >= 2 {
        return (trimmed, true);
    }
    (line, false)
}

fn parse_heading(line: &str) -> Option<Block> {
    let trimmed = line.trim();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level_count = trimmed.chars().take_while(|&c| c == '#').count();
    if level_count > 6 {
        return None;
    }
    // `level_count` is bounded above by 6, so it always fits in `u8`.
    let level = u8::try_from(level_count).unwrap_or(6);
    let rest = trimmed[level_count..].trim();
    // Trim trailing # marks.
    let rest = rest.trim_end_matches('#').trim();
    Some(Block::Heading(level, parse_inline(rest)))
}

/// Return the `(indent, marker_len)` for a line that starts with an
/// unordered-list marker (`- `, `* `, or `+ `), or `None` otherwise.
fn line_bullet(line: &str) -> Option<(usize, usize)> {
    let indent = indent_of(line);
    let rest = &line[indent..];
    if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
        Some((indent, 2))
    } else {
        None
    }
}

/// Return the `(indent, marker_len, number)` for a line that starts with
/// an ordered-list marker (`N. `), or `None` otherwise.
fn line_number(line: &str) -> Option<(usize, usize, u32)> {
    let indent = indent_of(line);
    let rest = &line[indent..];
    let dot_pos = rest.find(". ")?;
    let num_part = &rest[..dot_pos];
    if num_part.is_empty() || !num_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n = num_part.parse::<u32>().ok()?;
    Some((indent, dot_pos + 2, n))
}

/// Count leading ASCII spaces. Tabs aren't supported in list indentation
/// here — markdown authoring we care about uses spaces.
fn indent_of(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

/// Parse a list starting at `lines[start]`, nesting sub-lists when
/// subsequent lines use deeper indentation.
///
/// `ordered` picks between the unordered and ordered output variant; the
/// first line's marker determines the list style and its column-0
/// indentation sets the base level. More-indented list markers become
/// nested sub-lists attached to the previous item's `sub_blocks`.
fn parse_list(lines: &[&str], start: usize, ordered: bool) -> (Block, usize) {
    // Indentation of the first list marker defines the base level. A
    // later line at or beyond this indent that is NOT itself a list
    // marker is treated as a continuation of the last item's content.
    // A list marker at strictly greater indent opens a nested list.
    let base_indent = indent_of(lines[start]);

    let mut items: Vec<ListItem> = Vec::new();
    let mut i = start;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // Blank line or outdent ends the list.
        if trimmed.is_empty() {
            break;
        }
        let this_indent = indent_of(line);
        if this_indent < base_indent {
            break;
        }

        let is_bullet = line_bullet(line);
        let is_number = line_number(line);

        if this_indent == base_indent {
            // A marker of the wrong style at our own indent level ends
            // this list (the caller will pick up the new one).
            match (ordered, is_bullet, is_number) {
                (false, Some(_), _) => {
                    // Open new unordered item at this level.
                    let (_ind, marker_len) = is_bullet.unwrap();
                    let content_text = &line[base_indent + marker_len..];
                    items.push(ListItem {
                        content: parse_inline(content_text),
                        sub_blocks: Vec::new(),
                        number: None,
                    });
                    i += 1;
                }
                (true, _, Some(_)) => {
                    let (_ind, marker_len, n) = is_number.unwrap();
                    let content_text = &line[base_indent + marker_len..];
                    items.push(ListItem {
                        content: parse_inline(content_text),
                        sub_blocks: Vec::new(),
                        number: Some(n),
                    });
                    i += 1;
                }
                _ => break,
            }
        } else {
            // Deeper indent than our base.
            if is_bullet.is_some() || is_number.is_some() {
                // Nested list. Parse it recursively and attach to the
                // most recent item's sub_blocks.
                let nested_ordered = is_number.is_some();
                let (nested, new_i) = parse_list(lines, i, nested_ordered);
                if let Some(last) = items.last_mut() {
                    last.sub_blocks.push(nested);
                }
                i = new_i;
            } else if let Some(last) = items.last_mut() {
                // Continuation text under the last item.
                last.content.push(Inline::Text(format!(" {}", trimmed)));
                i += 1;
            } else {
                break;
            }
        }
    }

    if ordered {
        (Block::OrderedList(items), i)
    } else {
        (Block::UnorderedList(items), i)
    }
}

/// Attempt to parse a GFM-style table starting at `lines[start]`. Returns
/// `None` if the shape isn't right (missing separator, mismatched column
/// counts, etc.) so the caller can fall through to paragraph parsing.
fn parse_table(lines: &[&str], start: usize) -> Option<(Block, usize)> {
    // Need at least two lines — header + separator.
    if start + 1 >= lines.len() {
        return None;
    }

    let header_cells = split_table_row(lines[start])?;
    let align_cells = split_table_row(lines[start + 1])?;
    let alignments = parse_table_alignments(&align_cells)?;
    if header_cells.len() != alignments.len() {
        return None;
    }

    let headers: Vec<Vec<Inline>> = header_cells.iter().map(|c| parse_inline(c)).collect();
    let mut rows: Vec<Vec<Vec<Inline>>> = Vec::new();
    let mut i = start + 2;

    while i < lines.len() {
        let Some(cells) = split_table_row(lines[i]) else {
            break;
        };
        let row: Vec<Vec<Inline>> = cells
            .iter()
            .map(|c| parse_inline(c))
            .chain(std::iter::repeat_with(Vec::new))
            .take(alignments.len())
            .collect();
        rows.push(row);
        i += 1;
    }

    Some((
        Block::Table {
            headers,
            alignments,
            rows,
            // Capture the raw markdown source for this table block so
            // the renderer can fall back to wrapping the source text
            // when the available width can't accommodate even one
            // cell per column. Mirrors marked's `token.raw`.
            raw: lines[start..i].join("\n"),
        },
        i,
    ))
}

/// If `line` looks like a table row (`| ... | ... |`), split it into
/// trimmed cell strings. Returns `None` if it doesn't have the minimum
/// structure (leading and trailing `|` with at least one interior
/// delimiter).
fn split_table_row(line: &str) -> Option<Vec<String>> {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') || trimmed.len() < 2 {
        return None;
    }
    // Trim the outer pipes and split on interior `|`. Cells are
    // trim()-ed so `| Alice  | 30 |` renders as `["Alice", "30"]`.
    let inner = &trimmed[1..trimmed.len() - 1];
    let cells: Vec<String> = inner.split('|').map(|c| c.trim().to_string()).collect();
    if cells.is_empty() { None } else { Some(cells) }
}

/// Parse the alignment spec row (`| :--- | :---: | ---: |`). Returns
/// `None` if any cell isn't a valid dash-plus-optional-colons pattern.
fn parse_table_alignments(cells: &[String]) -> Option<Vec<Alignment>> {
    cells
        .iter()
        .map(|c| {
            let trimmed = c.trim();
            let (left_colon, rest) = match trimmed.strip_prefix(':') {
                Some(rest) => (true, rest),
                None => (false, trimmed),
            };
            let (right_colon, dashes) = match rest.strip_suffix(':') {
                Some(rest) => (true, rest),
                None => (false, rest),
            };
            if dashes.is_empty() || !dashes.chars().all(|c| c == '-') {
                return None;
            }
            Some(match (left_colon, right_colon) {
                (true, true) => Alignment::Center,
                (false, true) => Alignment::Right,
                _ => Alignment::Left,
            })
        })
        .collect()
}

/// Parse inline markdown elements (bold, italic, code, links, etc.).
///
/// Raw HTML tags are deliberately *not* recognized as a separate token
/// type — `<thinking>`, `<div>`, etc. fall through the parser as plain
/// text and render literally. This is the opposite of a strict
/// CommonMark passthrough (which would emit a raw `html` token and have
/// the renderer drop or copy it verbatim into HTML output) but the
/// right call for a terminal renderer driving model output: a model
/// that emits `<thinking>...</thinking>` should have those bytes
/// visible to the user, not silently swallowed. The companion
/// regression test is
/// `renders_html_like_tags_in_text_as_content_rather_than_hiding_them`
/// in `tests/markdown.rs`.
fn parse_inline(text: &str) -> Vec<Inline> {
    let mut result: Vec<Inline> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Bold (**text** or __text__). Word-boundary rule: the opening
        // `**`/`__` must not be preceded by a word character, and the
        // closing `**`/`__` must not be followed by one. Prevents
        // `5**4**3` from bolding the `4`, and (paired with the italic
        // arm below) keeps `_` from opening intraword. See PORTING.md
        // F10.
        if i + 1 < chars.len()
            && ((chars[i] == '*' && chars[i + 1] == '*')
                || (chars[i] == '_' && chars[i + 1] == '_'))
            && !preceded_by_word_char(&chars, i)
        {
            let marker = chars[i];
            if let Some(end) = find_emphasis_closing(&chars, i + 2, &[marker, marker]) {
                if !current.is_empty() {
                    result.push(Inline::Text(std::mem::take(&mut current)));
                }
                let inner: String = chars[i + 2..end].iter().collect();
                result.push(Inline::Bold(parse_inline(&inner)));
                i = end + 2;
                continue;
            }
        }

        // Strikethrough (~~text~~).
        if i + 1 < chars.len() && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(end) = find_closing_marker(&chars, i + 2, &['~', '~']) {
                if !current.is_empty() {
                    result.push(Inline::Text(std::mem::take(&mut current)));
                }
                let inner: String = chars[i + 2..end].iter().collect();
                result.push(Inline::Strikethrough(parse_inline(&inner)));
                i = end + 2;
                continue;
            }
        }

        // Italic (*text* or _text_). Word-boundary rule: the opening
        // `*`/`_` must not be preceded by a word character, and the
        // closing must not be followed by one. Prevents `5*4*3` from
        // italicizing the `4`, and `foo_bar_baz` from being parsed as
        // emphasis. The `chars[i - 1] != chars[i]` guard rejects the
        // tail of a longer delimiter run (e.g. the second `*` in a
        // rejected `**` opener) so `5**4**3` doesn't fall through to
        // italic on `4`. See PORTING.md F10.
        if (chars[i] == '*' || chars[i] == '_')
            && (i + 1 < chars.len() && !chars[i + 1].is_whitespace())
            && !preceded_by_word_char(&chars, i)
            && (i == 0 || chars[i - 1] != chars[i])
        {
            let marker = chars[i];
            if let Some(end) = find_emphasis_closing(&chars, i + 1, &[marker]) {
                if end > i + 1 {
                    if !current.is_empty() {
                        result.push(Inline::Text(std::mem::take(&mut current)));
                    }
                    let inner: String = chars[i + 1..end].iter().collect();
                    result.push(Inline::Italic(parse_inline(&inner)));
                    i = end + 1;
                    continue;
                }
            }
        }

        // Inline code (`code`).
        if chars[i] == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '`') {
                let end = i + 1 + end;
                if !current.is_empty() {
                    result.push(Inline::Text(std::mem::take(&mut current)));
                }
                let code: String = chars[i + 1..end].iter().collect();
                result.push(Inline::Code(code));
                i = end + 1;
                continue;
            }
        }

        // Link [text](url).
        if chars[i] == '[' {
            if let Some(close_bracket) = chars[i + 1..].iter().position(|&c| c == ']') {
                let close_bracket = i + 1 + close_bracket;
                if close_bracket + 1 < chars.len() && chars[close_bracket + 1] == '(' {
                    if let Some(close_paren) =
                        chars[close_bracket + 2..].iter().position(|&c| c == ')')
                    {
                        let close_paren = close_bracket + 2 + close_paren;
                        if !current.is_empty() {
                            result.push(Inline::Text(std::mem::take(&mut current)));
                        }
                        let link_text: String = chars[i + 1..close_bracket].iter().collect();
                        let link_url: String =
                            chars[close_bracket + 2..close_paren].iter().collect();
                        // Recursively parse the bracket content as
                        // inlines so `[**bold**](url)` keeps its
                        // emphasis. Mirrors pi-tui's `marked` link
                        // tokenization (the `tokens` field on a
                        // link token is the inner inline AST).
                        result.push(Inline::Link(parse_inline(&link_text), link_url));
                        i = close_paren + 1;
                        continue;
                    }
                }
            }
        }

        // Autolink: bare `http://` or `https://` URL.
        if let Some(end) = bare_url_end(&chars, i) {
            if !current.is_empty() {
                result.push(Inline::Text(std::mem::take(&mut current)));
            }
            let url: String = chars[i..end].iter().collect();
            // Autolinks have a single text inline whose content is
            // the URL itself. Wrapping in `vec![Text(...)]` keeps the
            // shape uniform with `[text](url)` parsing — the renderer
            // treats both paths via [`Markdown::render_link`].
            result.push(Inline::Link(vec![Inline::Text(url.clone())], url));
            i = end;
            continue;
        }

        // Autolink: bare email `local@domain.tld`. Only fires when the
        // previous character (or start of text) is a non-word boundary,
        // so `foo@bar` inside an identifier still parses as plain text.
        if chars[i] == '@'
            && i > 0
            && !current.is_empty()
            && let Some((start, end)) = bare_email_span(&chars, i, current.len())
        {
            // `start` and `end` are indexes into `chars`. We have to
            // snip the local part back out of `current` since that
            // buffer already captured it.
            let local_len_chars = i - start;
            let truncate_to = current.len().saturating_sub(local_len_chars);
            current.truncate(truncate_to);
            if !current.is_empty() {
                result.push(Inline::Text(std::mem::take(&mut current)));
            }
            let email: String = chars[start..end].iter().collect();
            result.push(Inline::Link(
                vec![Inline::Text(email.clone())],
                format!("mailto:{}", email),
            ));
            i = end;
            continue;
        }

        current.push(chars[i]);
        i += 1;
    }

    if !current.is_empty() {
        result.push(Inline::Text(current));
    }

    result
}

/// If `chars[start..]` begins with `http://` or `https://`, return the
/// index one past the end of the URL — greedy match until whitespace or
/// a trailing-punctuation character (`,`, `.`, `;`, `:`, `)`, `]`, `}`,
/// `!`, `?`, `"`, `'`). Those are excluded so a URL at the end of a
/// sentence doesn't eat the period.
fn bare_url_end(chars: &[char], start: usize) -> Option<usize> {
    let scheme: &[&[char]] = &[
        &['h', 't', 't', 'p', ':', '/', '/'],
        &['h', 't', 't', 'p', 's', ':', '/', '/'],
    ];
    let prefix_len = scheme
        .iter()
        .find(|s| chars.len() >= start + s.len() && &chars[start..start + s.len()] == **s)
        .map(|s| s.len())?;

    // Only autolink at word boundaries — i.e. the preceding char is
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
    // prose. Leave brackets/braces balanced: if the URL contains a `(`
    // we leave a trailing `)`, otherwise we trim it.
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
    // The local-part chars must actually be in `current` (not split
    // across a previous Inline element) — if they're not, we can't
    // safely back them out.
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
    // The domain's TLD portion must have at least one alphabetic char
    // so something like `a@1.2` doesn't autolink.
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

/// Find closing marker sequence in chars starting from `start`.
fn find_closing_marker(chars: &[char], start: usize, marker: &[char]) -> Option<usize> {
    let mlen = marker.len();
    if start + mlen > chars.len() {
        return None;
    }
    for i in start..=chars.len() - mlen {
        if &chars[i..i + mlen] == marker {
            return Some(i);
        }
    }
    None
}

/// Like [`find_closing_marker`], but skips marker occurrences that are
/// followed by a word character. Used by the bold/italic word-boundary
/// rule: a closing `*`/`_` (single or double) must sit at a non-word
/// boundary on its right, otherwise it isn't a valid emphasis close.
fn find_emphasis_closing(chars: &[char], start: usize, marker: &[char]) -> Option<usize> {
    let mlen = marker.len();
    if start + mlen > chars.len() {
        return None;
    }
    for i in start..=chars.len() - mlen {
        if &chars[i..i + mlen] == marker {
            let after = i + mlen;
            if after >= chars.len() || !is_emphasis_word_char(chars[after]) {
                return Some(i);
            }
        }
    }
    None
}

/// `true` if `chars[i]` is preceded by a word character — used by the
/// bold/italic open-boundary rule. Treats start-of-input as a word
/// boundary.
fn preceded_by_word_char(chars: &[char], i: usize) -> bool {
    i > 0 && is_emphasis_word_char(chars[i - 1])
}

/// Word-char predicate for emphasis boundaries: alphanumeric or `_`.
/// Underscore counts as a word char so that `foo_bar_baz` is treated as
/// a single intraword run rather than three emphasis candidates.
fn is_emphasis_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Recursively extract the plain-text content from a sequence of
/// inlines, dropping every styling layer. Used by
/// [`Markdown::render_link`] to drive the autolink-vs-fallback
/// decision: the styled link text carries ANSI codes that would never
/// match the raw URL, so we compare against the unstyled content
/// instead. Mirrors what pi-tui gets from `marked`'s `text` field on
/// a link token (the concatenated plain text of the inner inlines).
fn inline_plain_text(inlines: &[Inline]) -> String {
    let mut s = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(t) => s.push_str(t),
            Inline::Bold(inner) | Inline::Italic(inner) | Inline::Strikethrough(inner) => {
                s.push_str(&inline_plain_text(inner))
            }
            Inline::Code(c) => s.push_str(c),
            // A link inside a link would render its inner text; the
            // plain-text projection follows the same shape so a
            // pathological `[[autolink](u1)](u2)` falls back via the
            // inner-link's plain text. (Our parser doesn't currently
            // emit nested links, but the recursion costs nothing.)
            Inline::Link(inner, _) => s.push_str(&inline_plain_text(inner)),
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Markdown component
// ---------------------------------------------------------------------------

/// A component that renders markdown text to styled terminal lines.
///
/// The render-affecting state — padding, theme, and pre-style — is
/// fixed at construction time, mirroring pi-tui's `Markdown` shape
/// (`packages/tui/src/components/markdown.ts:79-103`). Only the text
/// content is mutable post-construction (via [`Markdown::set_text`]),
/// matching pi's lone `setText` setter. Callers that need to swap
/// theme, padding, or pre-style should build a fresh `Markdown`.
///
/// Per F49 (PORTING.md), the previous Rust-side setters
/// (`set_padding_x`, `set_padding_y`, `set_pre_style`, `set_theme`)
/// were removed in favor of this required-at-construction shape.
/// (`set_pre_style` was tied to the previous `PreStyle` type — see
/// H4 for the rename to [`DefaultTextStyle`].)
/// `set_hyperlinks` (and the `MarkdownTheme.hyperlinks` field it
/// mutated) were removed in H4 in favor of reading
/// [`crate::capabilities::get_capabilities`] inline at the
/// link-render site, matching pi-tui's `markdown.ts:492`.
pub struct Markdown {
    text: String,
    padding_x: usize,
    padding_y: usize,
    theme: MarkdownTheme,
    /// Outer styling applied to paragraph lines. See
    /// [`DefaultTextStyle`].
    default_text_style: Option<DefaultTextStyle>,
    // Syntax highlighting resources (loaded lazily).
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
    // Cache.
    cached_text: Option<String>,
    cached_width: Option<usize>,
    cached_lines: Option<Vec<String>>,
}

impl Markdown {
    /// Create a new Markdown component.
    ///
    /// Byte-for-byte mirrors pi-tui's
    /// `new Markdown(text, paddingX, paddingY, theme, defaultTextStyle?)`
    /// constructor (`packages/tui/src/components/markdown.ts:91-103`):
    /// padding axes and the optional pre-style (pi's `defaultTextStyle`)
    /// are required at construction. The theme is taken by value so the
    /// tui crate stays palette-agnostic; the agent layer is responsible
    /// for assembling a [`MarkdownTheme`] from its central palette.
    ///
    /// `default_text_style` is `Option<DefaultTextStyle>` rather than
    /// just `DefaultTextStyle` because pi's `defaultTextStyle`
    /// parameter is optional; callers that don't need it pass `None`.
    pub fn new(
        text: &str,
        padding_x: usize,
        padding_y: usize,
        theme: MarkdownTheme,
        default_text_style: Option<DefaultTextStyle>,
    ) -> Self {
        Self {
            text: text.to_string(),
            padding_x,
            padding_y,
            theme,
            default_text_style,
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
            cached_text: None,
            cached_width: None,
            cached_lines: None,
        }
    }

    /// Set the text content.
    ///
    /// The only mutator on `Markdown` post-construction, mirroring
    /// pi-tui's `setText` (`markdown.ts:105-108`). Padding, theme, and
    /// the default text style are immutable per instance — callers
    /// that need to swap them should build a fresh component.
    pub fn set_text(&mut self, text: &str) {
        if self.text != text {
            self.text = text.to_string();
            self.invalidate_cache();
        }
    }

    fn invalidate_cache(&mut self) {
        self.cached_text = None;
        self.cached_width = None;
        self.cached_lines = None;
    }

    /// Wrap `s` with the configured default text style. If none is
    /// set, returns the input unchanged. Mirrors pi-tui's
    /// `applyDefaultStyle` (`markdown.ts:210-237`) byte-for-byte:
    /// color is applied first (innermost), then text decorations in
    /// `bold → italic → strikethrough → underline` order on top.
    /// Each text-decoration call routes through the configured
    /// [`MarkdownTheme`] (`theme.bold`, `theme.italic`, etc.) so a
    /// custom theme can override the SGR shape of each decoration.
    ///
    /// Note that `bgColor` is intentionally not handled here even
    /// though it's part of pi's `DefaultTextStyle` interface — pi
    /// routes `bgColor` through the margin/padding-fill stage at the
    /// top of [`Self::render`] (`markdown.ts:164`), not via
    /// `applyDefaultStyle`. Our [`DefaultTextStyle`] omits the field
    /// entirely; see its rustdoc for the deferral rationale.
    fn apply_default_style(&self, s: &str) -> String {
        let Some(d) = &self.default_text_style else {
            return s.to_string();
        };
        let mut out = s.to_string();
        if let Some(color) = &d.color {
            out = color(&out);
        }
        if d.bold {
            out = (self.theme.bold)(&out);
        }
        if d.italic {
            out = (self.theme.italic)(&out);
        }
        if d.strikethrough {
            out = (self.theme.strikethrough)(&out);
        }
        if d.underline {
            out = (self.theme.underline)(&out);
        }
        out
    }

    /// Render a block to lines.
    ///
    /// Thin wrapper over [`Markdown::render_block_in_context`] with no
    /// outer inline-style context — used for top-level blocks (where
    /// the paragraph branch builds its own pre-style context) and for
    /// list-item sub-blocks (where pi keeps lists context-free in our
    /// narrower port; F46 spec says "other branches stay context-free").
    fn render_block(&self, block: &Block, content_width: usize) -> Vec<String> {
        self.render_block_in_context(block, content_width, None)
    }

    /// Render a block to lines under an optional outer inline-style
    /// context. Mirrors pi-tui's `renderToken(token, width, nextType,
    /// styleContext?)` signature (`markdown.ts:287-292`).
    ///
    /// The `ctx` parameter is forwarded to the paragraph branch (so a
    /// paragraph inside a blockquote consumes the quote's inline
    /// context instead of the document-level pre-style). Heading and
    /// every other block branch ignores `ctx` and either builds its
    /// own context (heading) or doesn't need one (code blocks, lists,
    /// tables, hr). This matches pi's heading branch (`markdown.ts:310-313`,
    /// builds `headingStyleContext` regardless of incoming context) and
    /// scopes the F46 port to the blockquote → paragraph case where
    /// the visible byte-path divergence lives.
    fn render_block_in_context(
        &self,
        block: &Block,
        content_width: usize,
        ctx: Option<&InlineStyleContext>,
    ) -> Vec<String> {
        match block {
            Block::Heading(level, inlines) => {
                // Pi-tui builds a heading-specific `InlineStyleContext`
                // (markdown.ts:300-313): `apply_text` wraps each text
                // run with the heading style (heading + bold +
                // optional H1 underline; the F28 formula);
                // `style_prefix` is the opens of that wrap, re-emitted
                // after every non-text inline so the heading color/
                // bold/underline reopens on whatever follows. Without
                // this, an inline-code span's own `\x1b[39m` inside
                // `# foo \`bar\` baz` would strip the heading color
                // from `baz` (F9 / F41).
                //
                // F45 / pi parity: pi's heading uses *only* the heading
                // wrap on text runs and does NOT thread the document's
                // `defaultTextStyle` (our `default_text_style`) through
                // the heading body or wrap the rendered heading line.
                // We mirror that here — `apply_text` is the heading
                // wrap, and there is no outer `apply_default_style`
                // wrap on the styled heading line (paragraphs are the
                // only block that gets the default text style applied;
                // see [`DefaultTextStyle`]).
                let heading_apply: Box<dyn Fn(&str) -> String + '_> = match *level {
                    1 => Box::new(|t: &str| {
                        (self.theme.heading)(&(self.theme.bold)(&(self.theme.underline)(t)))
                    }),
                    _ => Box::new(|t: &str| (self.theme.heading)(&(self.theme.bold)(t))),
                };
                let heading_apply_ref: &dyn Fn(&str) -> String = &*heading_apply;
                let heading_prefix = get_style_prefix(heading_apply_ref);
                let ctx = InlineStyleContext {
                    apply_text: heading_apply_ref,
                    style_prefix: &heading_prefix,
                };
                let body = self.render_inline_tokens(inlines, &ctx);
                let styled = if *level >= 3 {
                    // H3+ render the `### ` prefix as its own
                    // heading-styled segment so the prefix and body
                    // get the same treatment (F28).
                    let prefix_text = format!("{} ", "#".repeat(usize::from(*level)));
                    format!("{}{}", heading_apply_ref(&prefix_text), body)
                } else {
                    body
                };
                // No outer `apply_default_style` wrap here
                // (F45 pi parity).
                let mut lines = wrap_text_with_ansi(&styled, content_width);
                lines.push(String::new());
                lines
            }
            Block::Paragraph(inlines) => {
                // F41: thread `default_text_style` through the inline
                // walk via an `InlineStyleContext` instead of wrapping
                // the whole paragraph string once at the end. Pi-tui
                // calls this its "default inline style context" and
                // uses it for paragraphs, lists, and tables
                // (markdown.ts:280-285).
                //
                // The previous outer-wrap shape
                // (`apply_default_style(&text)` after `render_inlines`)
                // lost the default style's color on any text following
                // an inline reset: an inline code's `\x1b[39m` reset
                // the foreground, and the outermost color closer at
                // the end of the paragraph didn't re-open it for the
                // trailing text. Threading through the context puts
                // the default-style opens back after every non-text
                // inline so trailing text re-opens gray + italic +
                // whatever else.
                //
                // Scope: only paragraphs get the default text style
                // threaded. Lists and table cells stay on identity-
                // context `render_inlines`, matching the existing
                // [`DefaultTextStyle`] rustdoc contract ("Lists and
                // table cells currently don't receive default-text-
                // styling either"). Headings use their own heading
                // context above and intentionally do not get a
                // default-style wrap (F45 pi parity).
                //
                // F46: when `ctx` is supplied (we're inside a
                // blockquote), use it directly and skip the default-
                // style build. Mirrors pi-tui's
                // `renderInlineTokens(token.tokens, styleContext)` at
                // `markdown.ts:325` plus the blockquote-supplied
                // `quoteInlineStyleContext` whose comment at
                // `markdown.ts:386` reads "Default message style
                // should not apply inside blockquotes". The supplied
                // `ctx` is the quote inline context — identity
                // `apply_text` plus a `style_prefix` of the quote
                // wrapper's opens, re-emitted after each non-text
                // inline so the quote styling reopens for whatever
                // follows.
                let text = if let Some(ctx) = ctx {
                    self.render_inline_tokens(inlines, ctx)
                } else {
                    let default_apply: Box<dyn Fn(&str) -> String + '_> =
                        Box::new(|t: &str| self.apply_default_style(t));
                    let default_apply_ref: &dyn Fn(&str) -> String = &*default_apply;
                    // When `default_text_style` is unset,
                    // `apply_default_style` is the identity, the
                    // sentinel call returns `\u{0}`, and
                    // `get_style_prefix` slices to "". The
                    // `style_prefix` is "" so no re-emit and no trim,
                    // observably the same shape as the old
                    // `render_inlines` no-context path.
                    let default_prefix = if self.default_text_style.is_some() {
                        get_style_prefix(default_apply_ref)
                    } else {
                        String::new()
                    };
                    let para_ctx = InlineStyleContext {
                        apply_text: default_apply_ref,
                        style_prefix: &default_prefix,
                    };
                    self.render_inline_tokens(inlines, &para_ctx)
                };
                let mut lines = wrap_text_with_ansi(&text, content_width);
                lines.push(String::new());
                lines
            }
            Block::CodeBlock(lang, code) => {
                let mut lines = Vec::new();
                let border_open = if let Some(l) = lang {
                    format!("```{}", l)
                } else {
                    "```".to_string()
                };
                lines.push((self.theme.code_block_border)(&border_open));

                // Highlight via the theme override if present, otherwise
                // fall back to the built-in syntect-based highlighter.
                let highlighted = match &self.theme.highlight_code {
                    Some(hook) => hook(code, lang.as_deref()),
                    None => self.highlight_code(code, lang.as_deref()),
                };

                let default_indent = "  ";
                let indent = self
                    .theme
                    .code_block_indent
                    .as_deref()
                    .unwrap_or(default_indent);
                for hl_line in &highlighted {
                    lines.push(format!("{}{}", indent, hl_line));
                }

                lines.push((self.theme.code_block_border)("```"));
                lines.push(String::new());
                lines
            }
            Block::UnorderedList(items) => self.render_list(items, false, 0, content_width, ctx),
            Block::OrderedList(items) => self.render_list(items, true, 0, content_width, ctx),
            Block::Blockquote(sub_blocks) => {
                let border = (self.theme.quote_border)("│ ");
                let border_width = visible_width(&border);
                // F44(A): clamp to at least 1 so a degenerate
                // outer `content_width` (now reachable via F42 at
                // `content_width = 1`) recurses with a usable
                // per-grapheme width, mirroring pi-tui
                // (`markdown.ts:382`: `Math.max(1, width - 2)`).
                // For non-degenerate widths
                // (`content_width >= border_width + 1`, i.e. >= 3
                // with the default `│ ` border) the `.max(1)` is
                // a no-op so the common path is unchanged.
                let inner_width = content_width.saturating_sub(border_width).max(1);

                // F46: pi-tui parity machinery (`markdown.ts:370-417`).
                //
                // - `quote_apply` mirrors pi's `quoteStyle`: wrap each
                //   line with `theme.quote(theme.italic(line))`. The
                //   explicit `theme.italic` underwrap is the visible
                //   delta from the F12 path: a custom non-italic
                //   `theme.quote` (e.g. just a color) now still ships
                //   italic, matching pi for any theme — not just our
                //   default whose `theme.quote = style::italic` made
                //   the underwrap look redundant.
                //
                // - `quote_prefix` is the *opens* of `quote_apply`,
                //   extracted via [`get_style_prefix`]. For our
                //   default theme where `theme.quote = style::italic`
                //   that's `\x1b[3m\x1b[3m` (the quote's italic plus
                //   the underwrap's italic — both fire). For a custom
                //   `theme.quote = style::cyan` it's `\x1b[36m\x1b[3m`.
                //
                // - `quote_inline_ctx` is identity `apply_text`
                //   (no pre-style in here — pi's `markdown.ts:386`
                //   comment: "Default message style should not apply
                //   inside blockquotes") plus `style_prefix =
                //   quote_prefix`. Threaded through every sub-block
                //   so paragraph inlines re-emit `quote_prefix` after
                //   each non-text inline; the outer `apply_quote_style`
                //   wrap then re-opens the quote-italic block.
                //
                // - `apply_quote_style(line)` does pi's
                //   `markdown.ts:373-379`: replace every `\x1b[0m`
                //   (full SGR reset) with `\x1b[0m{quote_prefix}` so
                //   downstream content (notably syntect-highlighted
                //   code which terminates each line with `\x1b[0m`)
                //   reopens the quote/italic style after the reset,
                //   then wrap with `quote_apply`. Skipped when
                //   `quote_prefix` is empty (defensive — the sentinel
                //   trick in `get_style_prefix` returns "" only when
                //   the wrapper swallows the sentinel, which neither
                //   `theme.quote` nor `theme.italic` does).
                let quote_apply: Box<dyn Fn(&str) -> String + '_> =
                    Box::new(|t: &str| (self.theme.quote)(&(self.theme.italic)(t)));
                let quote_apply_ref: &dyn Fn(&str) -> String = &*quote_apply;
                let quote_prefix = get_style_prefix(quote_apply_ref);
                let identity = |t: &str| t.to_string();
                let identity_ref: &dyn Fn(&str) -> String = &identity;
                let quote_inline_ctx = InlineStyleContext {
                    apply_text: identity_ref,
                    style_prefix: &quote_prefix,
                };

                // F46-followup: phased pipeline matching pi's order
                // (markdown.ts:392-411).
                //
                // Phase 1: collect rendered sub-block lines.
                let mut quote_lines: Vec<String> = Vec::new();
                for block in sub_blocks {
                    let block_lines =
                        self.render_block_in_context(block, inner_width, Some(&quote_inline_ctx));
                    quote_lines.extend(block_lines);
                }

                // Phase 2: pop trailing blanks (pi markdown.ts:402-404,
                // "Avoid rendering an extra empty quote line before the
                // outer blockquote spacing"). Mid-quote blank rows from
                // sub-block separators stay and get the wrap; only the
                // tail trailing blanks are dropped here.
                while quote_lines.last().is_some_and(String::is_empty) {
                    quote_lines.pop();
                }

                // Phase 3: apply quote style → wrap → prepend border
                // (pi markdown.ts:406-411). Every remaining row,
                // including mid-quote blanks, goes through
                // `apply_quote_style` so a blank row gets the
                // empty-content wrap (`quote_apply("")`) instead of a
                // bare border. The wrap step after `apply_quote_style`
                // matches pi's `wrapTextWithAnsi(styledLine,
                // quoteContentWidth)`: rows that already fit are a
                // no-op (the paragraph's internal wrap to `inner_width`
                // covers the common case), but a sub-block whose lines
                // exceed `inner_width` (e.g. a wide code-block row at
                // a narrow render width) now wraps correctly with ANSI
                // state propagation, instead of overflowing past the
                // border.
                let mut lines = Vec::new();
                for ql in &quote_lines {
                    let styled = apply_quote_style(ql, quote_apply_ref, &quote_prefix);
                    let wrapped = wrap_text_with_ansi(&styled, inner_width);
                    for wl in wrapped {
                        lines.push(format!("{}{}", border, wl));
                    }
                }
                // One blank row after the whole quote. The outer F30
                // trim drops it if this is the document's last block.
                lines.push(String::new());
                lines
            }
            Block::HorizontalRule => {
                // pi-tui (`markdown.ts:420`) emits
                // `─.repeat(min(width, 80))` where the local `width`
                // parameter of `renderToken` is the *content* width
                // (passed in from `render(width)` as
                // `max(1, width - paddingX * 2)`, see line 147).
                // `content_width.min(80)` matches that shape.
                let rule = "─".repeat(content_width.min(80));
                vec![(self.theme.hr)(&rule), String::new()]
            }
            Block::Table {
                headers,
                alignments,
                rows,
                raw,
            } => self.render_table(headers, alignments, rows, raw, content_width, ctx),
        }
    }

    /// Render a table to styled lines. Picks column widths that
    /// accommodate the longest unbreakable token in each column, scales
    /// to fit `content_width` when the natural width exceeds it, and
    /// wraps cell content via `wrap_text_with_ansi`.
    ///
    /// When `content_width` can't accommodate even one cell per column
    /// (i.e. `available_for_cells < n_cols` after subtracting the
    /// `3 * n_cols + 1` chars of border/padding chrome), falls back
    /// to wrapping `raw` (the original markdown source) through
    /// `wrap_text_with_ansi` instead. Mirrors pi-tui's
    /// `markdown.ts:696-703` "too narrow to render a stable table"
    /// branch.
    fn render_table(
        &self,
        headers: &[Vec<Inline>],
        alignments: &[Alignment],
        rows: &[Vec<Vec<Inline>>],
        raw: &str,
        content_width: usize,
        ctx: Option<&InlineStyleContext>,
    ) -> Vec<String> {
        let n_cols = alignments.len();
        if n_cols == 0 {
            return vec![String::new()];
        }

        // Border overhead: each column claims 3 chars of chrome
        // (left border + two spaces of padding around cell content),
        // plus one for the final right border. Computed up front so
        // the fallback branch below can use it.
        let chrome = 3 * n_cols + 1;
        let available_for_cells = content_width.saturating_sub(chrome);

        // Fallback branch (pi-tui parity, `markdown.ts:696-703`): when
        // the available width can't fit even one cell per column,
        // rendering a table would produce visually broken output (a
        // border row wider than the content, or zero-width cells).
        // Pi falls back to wrapping the raw markdown source instead;
        // we mirror that with `wrap_text_with_ansi(raw, content_width)`
        // plus the trailing-blank spacer the rest of `render_block`'s
        // arms emit (the outer `Markdown::render` trim will collapse
        // it if this is the last block).
        //
        // Pi additionally treats a missing `token.raw` as "emit nothing"
        // and gates the trailing blank on `nextTokenType !== "space"`.
        // Our parser always populates `raw` for `Block::Table`, and
        // our trailing-blank emission shape (always emit, outer trim
        // drops it when not needed) already matches the pi-vs-trim-
        // last F30 divergence — see the `Markdown::render` trim loop.
        if available_for_cells < n_cols {
            let mut lines = if raw.is_empty() {
                Vec::new()
            } else {
                wrap_text_with_ansi(raw, content_width)
            };
            lines.push(String::new());
            return lines;
        }

        // Pre-render every cell once so visible-width calculations
        // (and the subsequent wrapping) share the same text.
        // F46-followup: forward `ctx` to inline rendering so a table
        // inside a blockquote re-emits the quote prefix after every
        // non-text inline. Mirrors pi's `renderTable` (`markdown.ts:683`,
        // calls `renderInlineTokens(..., styleContext)` for header and
        // body cells). When `ctx` is `None` (top-level table) we use
        // the identity context via `render_inlines`.
        let inline_render = |inlines: &[Inline]| -> String {
            match ctx {
                Some(ctx) => self.render_inline_tokens(inlines, ctx),
                None => self.render_inlines(inlines),
            }
        };
        let header_text: Vec<String> = headers.iter().map(|c| inline_render(c)).collect();
        let row_text: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().map(|c| inline_render(c)).collect())
            .collect();

        // Per-column natural width (max visible width) and minimum width
        // (longest unbreakable token).
        let mut natural = vec![0_usize; n_cols];
        let mut minimum = vec![1_usize; n_cols];
        for col in 0..n_cols {
            if let Some(text) = header_text.get(col) {
                natural[col] = natural[col].max(visible_width(text));
                minimum[col] = minimum[col].max(longest_token_width(text));
            }
            for row in &row_text {
                if let Some(text) = row.get(col) {
                    natural[col] = natural[col].max(visible_width(text));
                    minimum[col] = minimum[col].max(longest_token_width(text));
                }
            }
            // Empty columns still need width 1 so the border doesn't
            // collapse.
            natural[col] = natural[col].max(minimum[col]).max(1);
        }

        // Past the fallback gate, `available_for_cells >= n_cols >= 1`,
        // so `distribute_column_widths` always sees a usable budget.
        let widths = distribute_column_widths(&natural, &minimum, available_for_cells);

        let mut lines: Vec<String> = Vec::new();
        let separator = make_separator_row(&widths);

        push_row(
            &mut lines,
            render_table_row(&header_text, &widths, alignments),
        );
        lines.push(separator.clone());
        for (idx, row) in row_text.iter().enumerate() {
            if idx > 0 {
                lines.push(separator.clone());
            }
            push_row(&mut lines, render_table_row(row, &widths, alignments));
        }
        lines.push(String::new());
        lines
    }

    fn render_list(
        &self,
        items: &[ListItem],
        ordered: bool,
        depth: usize,
        content_width: usize,
        ctx: Option<&InlineStyleContext>,
    ) -> Vec<String> {
        let indent = "  ".repeat(depth);
        let mut lines = Vec::new();

        for (idx, item) in items.iter().enumerate() {
            let bullet = if ordered {
                // Preserve the source marker when we captured it;
                // otherwise fall back to positional numbering. This
                // keeps numbering stable across lists that were split
                // by intervening blocks (e.g. code fences between items).
                let n = item
                    .number
                    .unwrap_or_else(|| u32::try_from(idx + 1).unwrap_or(u32::MAX));
                format!("{}. ", n)
            } else {
                "- ".to_string()
            };
            let styled_bullet = (self.theme.list_bullet)(&bullet);
            // F46-followup: forward `ctx` to inline rendering so a list
            // inside a blockquote re-emits the quote prefix after every
            // non-text inline (codespan, bold, etc.). Mirrors pi's
            // `renderListItem` (`markdown.ts:602-620`) which passes
            // `styleContext` down to `renderInlineTokens`. When `ctx`
            // is `None` (top-level list, or list inside a non-context
            // sub-block), we use the identity context via
            // `render_inlines` — same as before.
            let text = match ctx {
                Some(ctx) => self.render_inline_tokens(&item.content, ctx),
                None => self.render_inlines(&item.content),
            };
            let bullet_width = visible_width(&bullet);
            let text_width = content_width.saturating_sub(indent.len() + bullet_width);

            let wrapped = wrap_text_with_ansi(&text, text_width);
            for (i, wl) in wrapped.iter().enumerate() {
                if i == 0 {
                    lines.push(format!("{}{}{}", indent, styled_bullet, wl));
                } else {
                    let continuation = " ".repeat(bullet_width);
                    lines.push(format!("{}{}{}", indent, continuation, wl));
                }
            }

            // Render nested sub-blocks under this item at depth+1.
            // Forward `ctx` so a paragraph (or further-nested list)
            // inside a list-inside-blockquote also re-emits the quote
            // prefix.
            for sub in &item.sub_blocks {
                let sub_lines = match sub {
                    Block::UnorderedList(sub_items) => {
                        self.render_list(sub_items, false, depth + 1, content_width, ctx)
                    }
                    Block::OrderedList(sub_items) => {
                        self.render_list(sub_items, true, depth + 1, content_width, ctx)
                    }
                    other => self.render_block_in_context(other, content_width, ctx),
                };
                // Drop the trailing blank line each sub-block appends;
                // within a list we don't want spacer rows between items.
                let trimmed: Vec<String> = {
                    let mut v = sub_lines;
                    while v.last().is_some_and(String::is_empty) {
                        v.pop();
                    }
                    v
                };
                lines.extend(trimmed);
            }
        }
        lines.push(String::new());
        lines
    }

    /// Render inline elements to a styled string with no outer
    /// styling context — text runs render verbatim, non-text inlines
    /// (bold, italic, strikethrough, code, link) carry only their own
    /// per-variant styling. Used by call sites that don't have an
    /// outer style to thread (table cells, list items, blockquote
    /// internals).
    ///
    /// Implementation note: this is a thin wrapper over
    /// [`Markdown::render_inline_tokens`] with an identity
    /// [`InlineStyleContext`]. Mirrors pi-tui's `renderInlineTokens`
    /// being called with the default (no `defaultTextStyle`) context.
    fn render_inlines(&self, inlines: &[Inline]) -> String {
        let identity = |t: &str| t.to_string();
        let ctx = InlineStyleContext {
            apply_text: &identity,
            style_prefix: "",
        };
        self.render_inline_tokens(inlines, &ctx)
    }

    /// Render a link inline (`[text](url)` or autolinked URL/email),
    /// gated on the terminal's detected OSC 8 hyperlink support.
    ///
    /// Mirrors pi-tui's `markdown.ts:492` shape byte-for-byte: the cap
    /// is read inline at render time via
    /// [`crate::capabilities::get_capabilities`] (the process-wide
    /// capabilities cache, seeded by env-based detection or by an
    /// explicit [`crate::capabilities::set_capabilities`] override).
    ///
    /// `rendered` is the link's visible text after the inner inlines
    /// have been rendered through the active [`InlineStyleContext`]
    /// (so a `[**bold**](url)` arrives with the bold escape codes
    /// already wrapped around `bold`). `plain` is the same content
    /// stripped of every styling layer — needed for the autolink-
    /// vs-fallback decision (`text == url`), since the styled
    /// `rendered` string carries ANSI codes that would never match
    /// the bare URL.
    fn render_link(&self, rendered: &str, plain: &str, url: &str) -> String {
        let styled_text = (self.theme.link)(&(self.theme.underline)(rendered));
        if get_capabilities().hyperlinks {
            // OSC 8: open (`ESC ] 8 ; ; <url> ESC \`),
            // styled visible text, close (`ESC ] 8 ; ; ESC \`).
            format!("\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\", url, styled_text)
        } else if plain == url || url.strip_prefix("mailto:") == Some(plain) {
            styled_text
        } else {
            format!(
                "{}{}",
                styled_text,
                (self.theme.link_url)(&format!(" ({})", url))
            )
        }
    }

    /// Render a sequence of inline tokens under an outer style context.
    /// Mirrors pi-tui's `renderInlineTokens` (markdown.ts:448-541).
    ///
    /// For each text run, `ctx.apply_text` wraps the text in the outer
    /// style (the heading wrap, the pre-style, etc.). For each non-text
    /// inline (bold, italic, strikethrough, code, link), we render the
    /// inline with its own styling and then append `ctx.style_prefix`
    /// so the outer style reopens for whatever follows. The
    /// `style_prefix` is just the *opens* of the outer wrap, extracted
    /// via [`get_style_prefix`]; an inline's own `\x1b[39m` (foreground
    /// reset) or `\x1b[22m` (bold off) would otherwise strip the
    /// matching outer state from following text.
    ///
    /// Trailing dangling `style_prefix` (no text after the last
    /// non-text inline) is trimmed at the end so we don't leave a
    /// stray opens-only sequence at the line boundary — pi does the
    /// same trim (markdown.ts:536-538).
    ///
    /// Text runs are split by `\n` and `apply_text` is applied per
    /// segment so each line carries its own opens and closes; mirrors
    /// pi's `applyTextWithNewlines` (markdown.ts:452-455). This keeps
    /// downstream `wrap_text_with_ansi` from inheriting an unbalanced
    /// open across a hard line break.
    ///
    /// Nested inlines (e.g. `**bold *italic***`) recurse with the same
    /// `ctx`, mirroring pi's `resolvedStyleContext` pass-through. The
    /// nested result has the outer-applied per-text styling baked in,
    /// then the per-variant theme wrap (bold, italic, etc.) wraps that
    /// nested result. Same shape pi produces.
    fn render_inline_tokens(&self, inlines: &[Inline], ctx: &InlineStyleContext) -> String {
        let apply_with_newlines = |t: &str| -> String {
            // Single-segment fast path avoids the split/collect/join
            // for the common case (text without embedded newlines).
            if !t.contains('\n') {
                return (ctx.apply_text)(t);
            }
            t.split('\n')
                .map(|seg| (ctx.apply_text)(seg))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mut result = String::new();
        for inline in inlines {
            match inline {
                Inline::Text(t) => result.push_str(&apply_with_newlines(t)),
                Inline::Bold(inner) => {
                    let inner_str = self.render_inline_tokens(inner, ctx);
                    result.push_str(&(self.theme.bold)(&inner_str));
                    result.push_str(ctx.style_prefix);
                }
                Inline::Italic(inner) => {
                    let inner_str = self.render_inline_tokens(inner, ctx);
                    result.push_str(&(self.theme.italic)(&inner_str));
                    result.push_str(ctx.style_prefix);
                }
                Inline::Strikethrough(inner) => {
                    let inner_str = self.render_inline_tokens(inner, ctx);
                    result.push_str(&(self.theme.strikethrough)(&inner_str));
                    result.push_str(ctx.style_prefix);
                }
                Inline::Code(code) => {
                    result.push_str(&(self.theme.code)(code));
                    result.push_str(ctx.style_prefix);
                }
                Inline::Link(inner, url) => {
                    // Recurse with the same `ctx` so an outer style
                    // (heading wrap, pre-style) reaches the link's
                    // visible text — pi-tui's
                    // `renderInlineTokens(token.tokens,
                    // resolvedStyleContext)` shape. The plain-text
                    // version drives the autolink-vs-fallback
                    // decision in [`Markdown::render_link`].
                    let inner_str = self.render_inline_tokens(inner, ctx);
                    let plain = inline_plain_text(inner);
                    result.push_str(&self.render_link(&inner_str, &plain, url));
                    result.push_str(ctx.style_prefix);
                }
            }
        }

        // Trim trailing dangling `style_prefix`. When `style_prefix` is
        // empty (identity context) the loop short-circuits via the
        // outer `is_empty` check; otherwise it strips any opens-only
        // tail left by a non-text inline at the end of the sequence.
        if !ctx.style_prefix.is_empty() {
            while result.ends_with(ctx.style_prefix) {
                let new_len = result.len() - ctx.style_prefix.len();
                result.truncate(new_len);
            }
        }

        result
    }

    /// Highlight code using syntect.
    fn highlight_code(&self, code: &str, lang: Option<&str>) -> Vec<String> {
        let syntax = lang
            .and_then(|l| self.syntax_set.find_syntax_by_token(l))
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let theme = &self.theme_set.themes["base16-ocean.dark"];
        let mut highlighter = HighlightLines::new(syntax, theme);

        let mut lines = Vec::new();
        for line in code.lines() {
            match highlighter.highlight_line(line, &self.syntax_set) {
                Ok(ranges) => {
                    let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
                    lines.push(format!("{}\x1b[0m", escaped));
                }
                Err(_) => {
                    lines.push((self.theme.code_block)(line));
                }
            }
        }
        lines
    }
}

impl Component for Markdown {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Check cache. Pi-tui parity (`markdown.ts:117-120`): the cache
        // check runs *before* the empty-text guard so that a repeat
        // render of a whitespace-only input (whose first call wrote the
        // empty-vec cache below) returns the cached result without
        // re-running `text.trim()`.
        if let (Some(ct), Some(cw), Some(cl)) =
            (&self.cached_text, self.cached_width, &self.cached_lines)
        {
            if ct == &self.text && cw == width {
                return cl.clone();
            }
        }

        // Empty / whitespace-only text is treated as "nothing to draw"
        // and returns an empty vec without parsing — pi-tui parity with
        // `markdown.ts:126` (`!this.text || this.text.trim() === ""`).
        // We populate the cache here (matching `markdown.ts:127-132`)
        // so the cache check above hits on the next call with the same
        // input. Mirrors the analogous F14 fix on the `Text` component.
        if self.text.trim().is_empty() {
            self.cached_text = Some(self.text.clone());
            self.cached_width = Some(width);
            self.cached_lines = Some(Vec::new());
            return Vec::new();
        }

        // Clamp `content_width` to `1` on degenerate render widths
        // (`width = 0`, or `width < 2 * padding_x`), pi-tui parity
        // (`markdown.ts:123`: `Math.max(1, width - this.paddingX * 2)`).
        // F42. The downstream paths (`render_block`, `render_list`,
        // table rendering, hr, paragraph wrap) all accept `width = 1`
        // gracefully — `wrap_text_with_ansi` breaks long words one
        // grapheme per row, hr emits a one-cell `─`, and bullets /
        // quote borders reduce the inner width via their own
        // saturating subtractions. Compare F14 / F40 on `Text` and
        // F34 (open) on `TextBox`.
        let content_width = width.saturating_sub(self.padding_x * 2).max(1);

        let padding = " ".repeat(self.padding_x);
        // Normalize tabs to three spaces before parsing (pi-tui parity).
        // The cache key (`cached_text`) holds the *original* text so an
        // unchanged input still hits the cache; normalization is
        // idempotent and deterministic, so a hit returns the same result
        // we'd produce by re-normalizing.
        let normalized = self.text.replace('\t', TAB_AS_SPACES);
        let blocks = parse_markdown(&normalized);

        let mut result = Vec::new();

        // Top padding.
        for _ in 0..self.padding_y {
            result.push(String::new());
        }

        for block in &blocks {
            let block_lines = self.render_block(block, content_width);
            for line in block_lines {
                result.push(format!("{}{}", padding, line));
            }
        }

        // Trim trailing blank rows from the rendered content before
        // applying bottom padding (F30, narrow divergence from pi-tui).
        //
        // Structural reason. Each [`Markdown::render_block`] arm
        // unconditionally appends a single `String::new()` so that one
        // blank row separates this block from whatever follows in the
        // source. Pi-tui's renderer is instead next-token-aware —
        // marked's lexer emits explicit `space` tokens between blocks
        // and pi only emits `""` when the next token exists and isn't
        // `space`, so the natural emission already produces the right
        // spacing without trimming. Our parser collapses blank source
        // lines into nothing (see [`parse_markdown`]), so we have no
        // "next-token type" available to gate the trailing emission;
        // unconditional emit + post-trim is the structurally cleaner
        // shape for this AST.
        //
        // Visible difference (verified empirically against pi using
        // marked@15.0.12; see PORTING.md F30 for the trace). The
        // divergence is narrower than "pi preserves trailing blanks":
        //
        //   - Document with 0 or 1 trailing `\n`: marked produces no
        //     trailing `space` token, so pi and our port both emit
        //     zero trailing blank rows. **No divergence.**
        //   - Document ending in a heading followed by any number of
        //     trailing `\n`s: marked absorbs the trailing newlines
        //     into the heading token's `raw` field, so there's still
        //     no trailing `space` token and both renderers emit zero
        //     trailing blank rows. **No divergence.**
        //   - Document ending with 2+ trailing `\n`s after a non-
        //     heading block (paragraph, code block, list, blockquote,
        //     horizontal rule, table): marked emits one trailing
        //     `space` token regardless of how many `\n`s, pi maps it
        //     to exactly one `""` row, and our trim removes it. So
        //     pi outputs N+1 rows and we output N rows — divergence
        //     by exactly one row.
        //
        // We accept the one-row divergence in the third case for the
        // chat surface this crate ships into: trailing typing
        // artifacts in an LLM-emitted message read as dead space at
        // the bottom of the rendered cell, not intentional structure.
        // Mirrors F11's same-direction divergence that chose chat-
        // surface UX over literal CommonMark fidelity.
        //
        // Cross-block invariants the trim preserves (covered by the
        // `does_not_add_a_trailing_blank_line_when_*_is_last` family
        // in `tests/markdown.rs` for headings, paragraphs, code
        // blocks, blockquotes, tables, and horizontal rules):
        // a document ending in any block emits zero trailing blank
        // rows, regardless of how many `String::new()` spacers the
        // emit path layered on.
        while result.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
            result.pop();
        }

        // Bottom padding.
        for _ in 0..self.padding_y {
            result.push(String::new());
        }

        // Cache the pre-fallback result so a subsequent render of the
        // same `(text, width)` hits the cache check at the top of this
        // method. Pi-tui parity (`markdown.ts:196-199`). The cache
        // stores `result` *before* the `[""]` fallback below — same
        // asymmetry F14 documented for `Text`: a cache-hit returns
        // whatever was computed verbatim (potentially the empty vec),
        // only the first-call return path runs through the fallback.
        self.cached_text = Some(self.text.clone());
        self.cached_width = Some(width);
        self.cached_lines = Some(result.clone());

        // `[""]` fallback (pi `markdown.ts:201`): a non-empty input
        // that produces zero rendered rows still emits a single blank
        // row so the component occupies one cell. Defensive in our
        // port — every `parse_markdown(non-whitespace)` returns at
        // least one block, and every `render_block` arm emits at
        // least one line plus a separator, so this branch is
        // unreachable for real inputs; kept for byte-level pi parity.
        if result.is_empty() {
            vec![String::new()]
        } else {
            result
        }
    }

    fn invalidate(&mut self) {
        self.invalidate_cache();
    }
}

// ---------------------------------------------------------------------------
// Table rendering helpers (free functions; not tied to Markdown state)
// ---------------------------------------------------------------------------

/// Length of the longest contiguous non-whitespace run (visible-width,
/// ANSI-aware). Used to pick a column's minimum width so wrapping never
/// slices a token mid-glyph.
fn longest_token_width(text: &str) -> usize {
    // Strip ANSI first so token boundaries are computed on the visible
    // characters. `visible_width` already ignores CSI sequences, so we
    // iterate runs of non-whitespace and measure each.
    let stripped = strip_ansi_for_tokens(text);
    stripped
        .split_whitespace()
        .map(unicode_width::UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
}

/// Minimal ANSI stripper that drops CSI SGR sequences. Only used for
/// measuring token widths; not exported.
fn strip_ansi_for_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            while let Some(&c2) = chars.peek() {
                chars.next();
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Distribute `available` columns across `n` columns.
///
/// Three tiers:
///
/// 1. If every column's natural width fits (`sum(natural) <= available`),
///    return natural widths.
/// 2. If the preferred minimums fit (`sum(minimum) <= available`),
///    allocate each column its minimum and distribute the rest
///    proportionally to each column's slack (`natural - minimum`).
/// 3. Otherwise the preferred minimums don't even fit — `available` is
///    less than `sum(longest unbreakable token per column)`. Allocate
///    proportionally by preferred-minimum weight, with every column
///    getting at least 1; `wrap_text_with_ansi` will hard-break any
///    token that still doesn't fit. This is the "extremely narrow
///    width" path.
fn distribute_column_widths(natural: &[usize], minimum: &[usize], available: usize) -> Vec<usize> {
    let n = natural.len();
    let natural_total: usize = natural.iter().sum();

    if natural_total <= available {
        return natural.to_vec();
    }

    let min_total: usize = minimum.iter().sum();
    if min_total <= available {
        let extra_budget = available - min_total;
        let slack: Vec<usize> = natural
            .iter()
            .zip(minimum.iter())
            .map(|(n, m)| n.saturating_sub(*m))
            .collect();
        let slack_total: usize = slack.iter().sum();

        let mut widths = minimum.to_vec();
        if slack_total > 0 {
            let mut distributed = 0_usize;
            for i in 0..n {
                let add = extra_budget * slack[i] / slack_total;
                widths[i] += add;
                distributed += add;
            }
            let mut leftover = extra_budget.saturating_sub(distributed);
            for i in 0..n {
                if leftover == 0 {
                    break;
                }
                if widths[i] < natural[i] {
                    widths[i] += 1;
                    leftover -= 1;
                }
            }
        }
        return widths;
    }

    // Even the preferred minimums don't fit. Distribute `available`
    // proportionally by minimum weight, clamped to at least 1 per
    // column. Wrapping will hard-break any overlong tokens.
    let mut widths: Vec<usize> = minimum
        .iter()
        .map(|m| (available * *m / min_total).max(1))
        .collect();
    // If the `.max(1)` clamp pushed us over budget, trim from the
    // largest column until we fit (or every column is at 1).
    let mut total: usize = widths.iter().sum();
    while total > available {
        let Some((i, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > 1)
            .max_by_key(|(_, w)| **w)
        else {
            break;
        };
        widths[i] -= 1;
        total -= 1;
    }
    widths
}

/// Render a single data (or header) row to a string. Wraps each cell
/// independently; if wrapping produces multiple visual lines per cell,
/// pads shorter cells with blank lines so borders line up.
fn render_table_row(cells: &[String], widths: &[usize], alignments: &[Alignment]) -> String {
    let n = widths.len();
    let wrapped_per_cell: Vec<Vec<String>> = (0..n)
        .map(|c| {
            let text = cells.get(c).map(String::as_str).unwrap_or("");
            if widths[c] == 0 {
                vec![String::new()]
            } else {
                wrap_text_with_ansi(text, widths[c])
            }
        })
        .collect();

    let max_lines = wrapped_per_cell.iter().map(Vec::len).max().unwrap_or(1);

    let mut out = String::new();
    for line_idx in 0..max_lines {
        if line_idx > 0 {
            out.push('\n');
        }
        out.push('│');
        for col in 0..n {
            let cell_line = wrapped_per_cell[col]
                .get(line_idx)
                .cloned()
                .unwrap_or_default();
            let aligned = pad_cell(&cell_line, widths[col], alignments[col]);
            out.push(' ');
            out.push_str(&aligned);
            out.push(' ');
            out.push('│');
        }
    }
    out
}

/// Pad `content` (may contain ANSI) to `width` visible columns according
/// to `alignment`. When the content is already at or over the target,
/// returns it unchanged — callers are expected to pre-wrap.
fn pad_cell(content: &str, width: usize, alignment: Alignment) -> String {
    let vw = visible_width(content);
    if vw >= width {
        return content.to_string();
    }
    let padding = width - vw;
    match alignment {
        Alignment::Left => format!("{}{}", content, " ".repeat(padding)),
        Alignment::Right => format!("{}{}", " ".repeat(padding), content),
        Alignment::Center => {
            let left = padding / 2;
            let right = padding - left;
            format!("{}{}{}", " ".repeat(left), content, " ".repeat(right))
        }
    }
}

/// The `├─...┼─...┤` row used as the header separator and between data
/// rows.
fn make_separator_row(widths: &[usize]) -> String {
    let mut out = String::new();
    out.push('├');
    for (idx, w) in widths.iter().enumerate() {
        if idx > 0 {
            out.push('┼');
        }
        // Two chars of padding become `─`s to match the border width.
        for _ in 0..(w + 2) {
            out.push('─');
        }
    }
    out.push('┤');
    out
}

/// Split an already-rendered row on embedded `\n` boundaries and push
/// each visual line onto `lines`. `render_table_row` returns a single
/// string with newlines when a cell wrapped to multiple visual lines;
/// the outer `Vec<String>` contract of `render_block` expects one
/// entry per visual line.
fn push_row(lines: &mut Vec<String>, row: String) {
    for l in row.split('\n') {
        lines.push(l.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identity theme for in-module tests — every closure passes its
    /// input through verbatim. Mirrors
    /// `tests/support/themes.rs::identity_markdown_theme` so unit tests
    /// and integration tests share the same convention.
    fn identity_theme() -> MarkdownTheme {
        MarkdownTheme {
            heading: Arc::new(|s| s.to_string()),
            bold: Arc::new(|s| s.to_string()),
            italic: Arc::new(|s| s.to_string()),
            strikethrough: Arc::new(|s| s.to_string()),
            code: Arc::new(|s| s.to_string()),
            code_block: Arc::new(|s| s.to_string()),
            code_block_border: Arc::new(|s| s.to_string()),
            link: Arc::new(|s| s.to_string()),
            link_url: Arc::new(|s| s.to_string()),
            list_bullet: Arc::new(|s| s.to_string()),
            quote_border: Arc::new(|s| s.to_string()),
            quote: Arc::new(|s| s.to_string()),
            hr: Arc::new(|s| s.to_string()),
            underline: Arc::new(|s| s.to_string()),
            highlight_code: None,
            code_block_indent: None,
        }
    }

    #[test]
    fn test_markdown_heading() {
        let mut md = Markdown::new("# Hello World", 1, 0, identity_theme(), None);
        let lines = md.render(80);
        assert!(!lines.is_empty());
        // Should contain styled text.
        assert!(lines.iter().any(|l| l.contains("Hello World")));
    }

    #[test]
    fn test_markdown_paragraph() {
        let mut md = Markdown::new("This is a paragraph.", 1, 0, identity_theme(), None);
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("This is a paragraph.")));
    }

    #[test]
    fn test_markdown_code_block() {
        let mut md = Markdown::new("```rust\nfn main() {}\n```", 1, 0, identity_theme(), None);
        let lines = md.render(80);
        // Code block should contain the code content somewhere.
        let all = lines.join("\n");
        assert!(all.contains("fn") && all.contains("main"));
    }

    #[test]
    fn test_markdown_list() {
        let mut md = Markdown::new("- item 1\n- item 2\n- item 3", 1, 0, identity_theme(), None);
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("item 1")));
        assert!(lines.iter().any(|l| l.contains("item 3")));
    }

    #[test]
    fn test_markdown_bold_italic() {
        let mut md = Markdown::new("**bold** and *italic*", 1, 0, identity_theme(), None);
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("bold")));
        assert!(lines.iter().any(|l| l.contains("italic")));
    }

    #[test]
    fn test_markdown_link() {
        let mut md = Markdown::new(
            "[Example](https://example.com)",
            1,
            0,
            identity_theme(),
            None,
        );
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("Example")));
        assert!(lines.iter().any(|l| l.contains("https://example.com")));
    }

    #[test]
    fn test_markdown_blockquote() {
        let mut md = Markdown::new("> This is a quote", 1, 0, identity_theme(), None);
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("This is a quote")));
    }

    #[test]
    fn test_markdown_inline_code() {
        let mut md = Markdown::new("Use `code` here", 1, 0, identity_theme(), None);
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("code")));
    }

    #[test]
    fn test_markdown_empty() {
        let mut md = Markdown::new("", 1, 0, identity_theme(), None);
        assert!(md.render(80).is_empty());
    }

    #[test]
    fn test_markdown_hr() {
        let mut md = Markdown::new("---", 1, 0, identity_theme(), None);
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("─")));
    }

    #[test]
    fn test_inline_parsing() {
        let inlines = parse_inline("hello **bold** world");
        assert_eq!(inlines.len(), 3);
        matches!(&inlines[1], Inline::Bold(_));
    }

    /// `Markdown::new` (F33 follow-up) takes the theme as a required
    /// argument and applies it directly to render output. We use a
    /// sentinel `code` styler so the marker can't be confused with any
    /// of the test fixture's escape codes.
    #[test]
    fn new_applies_supplied_theme_to_render_output() {
        let theme = MarkdownTheme {
            code: Arc::new(|s| format!("<<{}>>", s)),
            ..identity_theme()
        };
        let mut md = Markdown::new("Use `foo` here", 1, 0, theme, None);
        let lines = md.render(80);
        let blob = lines.join("\n");
        assert!(
            blob.contains("<<foo>>"),
            "new() theme code styler must be applied: got {blob:?}",
        );
    }

    /// F43: `Markdown::render` populates the cache fields at the tail of
    /// a non-empty render. With the cache write in place, the cache
    /// check at the top of `render` actually fires on a subsequent call
    /// with the same `(text, width)`. Pi-tui parity with
    /// `markdown.ts:196-199`. Uses private-field access via the
    /// in-module `mod tests` block — the integration-test suite can
    /// only observe behavior, not the cache state directly.
    #[test]
    fn render_populates_cache_at_tail_for_non_empty_input() {
        let mut md = Markdown::new("# hello", 1, 0, identity_theme(), None);
        // Cache is empty before any render.
        assert!(md.cached_text.is_none());
        assert!(md.cached_width.is_none());
        assert!(md.cached_lines.is_none());

        let first = md.render(80);
        assert!(!first.is_empty(), "non-empty input must render rows");

        // Cache is populated at the tail.
        assert_eq!(md.cached_text.as_deref(), Some("# hello"));
        assert_eq!(md.cached_width, Some(80));
        let cached = md.cached_lines.as_ref().expect("cache must be set");
        assert_eq!(cached, &first);
    }

    /// F43: a second render with the same `(text, width)` returns the
    /// cached lines verbatim (the cache-check at the top of `render`
    /// fires before any parse work). The fix is observable via the
    /// returned value being equal to the first call; the underlying
    /// parse-skip is locked in by the cache-state assertion in the
    /// companion test above.
    #[test]
    fn second_render_with_same_inputs_returns_cached_result() {
        let mut md = Markdown::new(
            "# hello\n\nbody **bold** text",
            1,
            0,
            identity_theme(),
            None,
        );
        let first = md.render(80);
        let second = md.render(80);
        assert_eq!(first, second);
    }

    /// F43: changing the text via `set_text` invalidates the cache so
    /// the next render reflects the new input rather than returning a
    /// stale clone of the previous result. Verifies the existing
    /// `invalidate_cache()` call in `set_text` composes correctly with
    /// the new tail cache write.
    #[test]
    fn set_text_invalidates_cache_so_next_render_reflects_mutation() {
        let mut md = Markdown::new("# hello", 1, 0, identity_theme(), None);
        let first = md.render(80);
        assert!(md.cached_lines.is_some());

        md.set_text("## world");
        // Cache cleared by set_text.
        assert!(md.cached_text.is_none());
        assert!(md.cached_lines.is_none());

        let second = md.render(80);
        assert_ne!(first, second, "render must reflect the new text");
        // And the cache is now repopulated for the new input.
        assert_eq!(md.cached_text.as_deref(), Some("## world"));
    }

    /// F43: a width change with the same text also invalidates the
    /// cache match — the cache-check requires both `text` and `width`
    /// to match, so a `(text, w1)` cached result doesn't satisfy a
    /// `(text, w2)` query. The new render writes a fresh cache entry
    /// for the new width.
    #[test]
    fn render_at_a_different_width_misses_the_cache_and_repopulates() {
        let mut md = Markdown::new(
            "paragraph one with enough words to wrap differently",
            1,
            0,
            identity_theme(),
            None,
        );
        let narrow = md.render(20);
        assert_eq!(md.cached_width, Some(20));

        let wide = md.render(80);
        assert_eq!(md.cached_width, Some(80));
        assert_ne!(
            narrow, wide,
            "different widths must produce different wrapping",
        );
    }
}
