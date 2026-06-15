//! Markdown rendering component.
//!
//! Renders markdown text to styled terminal lines using ANSI escape codes.
//! Supports headings, paragraphs, code blocks (with syntax highlighting via syntect),
//! lists (ordered and unordered, nested), links, blockquotes, horizontal rules,
//! bold, italic, strikethrough, and inline code.

use std::sync::{Arc, OnceLock};

use syntect::easy::ScopeRegionIterator;
use syntect::highlighting::ScopeSelectors;
use syntect::parsing::{MatchPower, ParseState, Scope, ScopeStack, SyntaxSet};

use crate::ansi::{
    apply_background_to_line, extract_ansi_code, visible_width, wrap_text_with_ansi,
};
use crate::capabilities::get_capabilities;
use crate::component::Component;

/// Tabs in the source markdown are normalized to this many spaces before
/// parsing. Three spaces (rather than four) matches the `Text` component's
/// `TAB_AS_SPACES` constant; the choice is a UX call rather than a
/// CommonMark requirement.
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
/// We deliberately do not provide a `Default` impl: the tui crate stays
/// palette-agnostic, and the agent layer builds a theme from its central
/// palette and passes it to [`Markdown::new`]. Tests build themes via
/// `tests/support/themes.rs`.
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
    /// Whether to syntax-highlight fenced code blocks. When `false`,
    /// code-block bodies render as plain text via `code_block`.
    /// Ignored when `highlight_code` is set: an explicit override
    /// always wins.
    pub syntax_highlight: bool,
    /// Per-category styling for syntax-highlighted code. Used by the
    /// built-in highlighter (when `highlight_code` is `None` and
    /// `syntax_highlight` is `true`) to color each token according to
    /// the scope syntect assigns it. See [`SyntaxStyles`].
    pub syntax: SyntaxStyles,
}

/// Per-category styling closures for syntax-highlighted code.
///
/// The built-in highlighter tokenizes code with syntect, maps each
/// token's scope onto one of these categories (see
/// [`SyntaxCategory`]), and applies the matching closure. Tokens that
/// match no category render in the terminal's default foreground. The
/// closures use `Arc` so a theme stays cheaply cloneable.
#[derive(Clone)]
pub struct SyntaxStyles {
    pub comment: Arc<dyn Fn(&str) -> String>,
    pub keyword: Arc<dyn Fn(&str) -> String>,
    pub function: Arc<dyn Fn(&str) -> String>,
    pub variable: Arc<dyn Fn(&str) -> String>,
    pub string: Arc<dyn Fn(&str) -> String>,
    pub number: Arc<dyn Fn(&str) -> String>,
    pub type_name: Arc<dyn Fn(&str) -> String>,
    pub operator: Arc<dyn Fn(&str) -> String>,
    pub punctuation: Arc<dyn Fn(&str) -> String>,
}

impl SyntaxStyles {
    fn style(&self, category: SyntaxCategory) -> &dyn Fn(&str) -> String {
        match category {
            SyntaxCategory::Comment => &*self.comment,
            SyntaxCategory::Keyword => &*self.keyword,
            SyntaxCategory::Function => &*self.function,
            SyntaxCategory::Variable => &*self.variable,
            SyntaxCategory::String => &*self.string,
            SyntaxCategory::Number => &*self.number,
            SyntaxCategory::Type => &*self.type_name,
            SyntaxCategory::Operator => &*self.operator,
            SyntaxCategory::Punctuation => &*self.punctuation,
        }
    }
}

/// Syntax-highlighting categories the built-in highlighter recognizes.
/// Each maps to one styling closure in [`SyntaxStyles`].
#[derive(Clone, Copy)]
enum SyntaxCategory {
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

/// The syntect syntax definitions, loaded once and shared across all
/// `Markdown` instances.
///
/// syntect compiles a grammar's regexes lazily on first use and caches
/// them inside the `SyntaxSet` (via `OnceCell`). That first compile is
/// expensive (~15ms for a heavy grammar like Rust), so loading a fresh
/// set per component would pay it for every code block. Sharing one
/// set means the compile happens once per process and every later
/// block reuses the cached regexes. The set is read-only to callers
/// (`parse_line` takes `&SyntaxSet`), and its caches are `Sync`, so a
/// process-global is sound.
fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// TextMate-style scope selectors per category, parsed once.
///
/// We classify a token by picking the category whose selector matches
/// its scope stack with the highest [`MatchPower`] — the same
/// most-specific-wins rule syntect themes use. `keyword` and
/// `keyword.operator` overlap on purpose: an operator token matches
/// both, and the more specific `keyword.operator` selector wins, so
/// operators get their own color rather than the keyword color.
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

/// Pick the best-matching category for a scope stack, or `None` when
/// no category's selector matches (the token then renders in the
/// default foreground).
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

/// Outer styling applied to rendered paragraph lines, independently of
/// the theme's inline styling. Used to tint a whole block of
/// "thinking" prose in, say, dim italic gray while leaving the theme's
/// inline styling (inline code, links, code-block highlighting) intact.
///
/// Use [`Default::default`] + struct-update syntax to set just the
/// fields you care about:
///
/// ```ignore
/// DefaultTextStyle {
///     color: Some(Arc::new(style::gray)),
///     italic: true,
///     ..Default::default()
/// }
/// ```
///
/// **Field set.** Six fields: `color`, `bg_color`, `bold`, `italic`,
/// `strikethrough`, `underline`. `color` and the four text-decoration
/// flags are wired through [`Markdown::apply_default_style`]
/// (per-text-run styling, applied inside the inline-style context).
/// `bg_color` is wired through [`Markdown::render`]'s row-emission
/// stage: each post-wrap row is built as
/// `left_margin + line + right_margin`, padded to the full render
/// width, and routed through the `bg_fn` so the background reaches
/// every cell in the row (including the top/bottom padding rows).
/// Production callers that want per-message backgrounds typically
/// wrap [`Markdown`] in a [`crate::components::TextBox`] with a
/// `bg_fn`, but `bg_color` is available for callers that want
/// per-paragraph backgrounds without an outer box.
///
/// **Apply order.** [`Markdown::apply_default_style`] applies the
/// per-text-run fields in the order
/// color → bold → italic → strikethrough → underline, with the
/// text-decoration calls routing through `theme.bold` /
/// `theme.italic` / etc. so a custom theme can override how each
/// decoration is styled. Color is innermost so the open codes nest as
/// `underline strikethrough italic bold color {text} ...closes`.
/// `bg_color` is applied independently at row-emission time and does
/// not nest with the per-run wrappers.
///
/// **Rendering scope.** Applies to paragraphs only. Does NOT apply to
/// headings, blockquotes (which use `theme.quote`), code blocks (which
/// carry their own highlighting), or horizontal rules. Lists and table
/// cells currently don't receive default-text-styling either; when a
/// caller needs it, extend the render path.
#[derive(Clone, Default)]
pub struct DefaultTextStyle {
    /// Optional color wrapper. Called with the line's already-styled
    /// text and returns the same text with a surrounding SGR color.
    pub color: Option<Arc<dyn Fn(&str) -> String>>,
    /// Optional background-color wrapper applied at the row-emission
    /// stage of [`Markdown::render`]. Each rendered row (including the
    /// top and bottom padding rows from `padding_y`) is padded out to
    /// the full render width and then routed through this closure, so
    /// the background reaches every cell of the component's bounding
    /// box rather than just the content text. The closure is *not*
    /// applied per inline run — per-run color goes through
    /// [`Self::color`] above; this field only controls the row-level
    /// background.
    pub bg_color: Option<Arc<dyn Fn(&str) -> String>>,
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
// Inline-style context
// ---------------------------------------------------------------------------

/// Outer style applied to a sequence of inline tokens, with proper
/// restoration after each non-text inline.
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
/// content reopens the quote styling after the reset.
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

/// Returns true when `line` has no visible non-whitespace content —
/// every visible character is whitespace, or there are no visible
/// characters at all.
///
/// Used by the trailing-blank trim in [`Markdown::render`]. The trim
/// ran on `String::is_empty` before the row-emission rewrite that
/// pads every row to the full render width and optionally wraps it in
/// a `bg_fn`; both transforms turn what used to be a `""` row into
/// either `" ".repeat(width)` or `bg_fn(" ".repeat(width))`. This
/// helper steps over ANSI escape sequences (CSI / OSC / APC, via
/// [`extract_ansi_code`]) so an SGR-tinted row of spaces still counts
/// as blank for the trim.
fn is_blank_row(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\x1b' {
            if let Some(code) = extract_ansi_code(s, i) {
                i += code.byte_len;
                continue;
            }
        }
        // Not (the start of) an ANSI escape we recognize — read the
        // next visible char and check it's whitespace. UTF-8-safe via
        // `chars().next()`, which always reads a complete code point.
        let rest = &s[i..];
        let Some(c) = rest.chars().next() else {
            break;
        };
        if !c.is_whitespace() {
            return false;
        }
        i += c.len_utf8();
    }
    true
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
    /// stable table.
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

/// A list item under construction in [`parse_list`]: its inline source
/// text accumulated across the marker line and any soft-wrapped
/// continuation lines, plus nested sub-blocks. The text is parsed into
/// inlines once, after the item is complete, so markup that lands on (or
/// straddles) a continuation line still renders as markup.
struct PendingItem {
    text: String,
    sub_blocks: Vec<Block>,
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
    /// The first field is the parsed inline tokens that make up the
    /// link's *visible* text — so a `[**bold**](url)` keeps the bold
    /// emphasis nested under the link. For autolinks (bare URLs,
    /// emails) the inner is `vec![Inline::Text(url)]` so the visible
    /// text is the URL itself. The second field is the URL target.
    Link(Vec<Inline>, String),
}

/// Maximum nesting depth the parser descends before degrading to
/// literal text.
///
/// Caps two independent recursion families: block nesting (blockquotes
/// and lists share one counter, since a list can sit inside a quote)
/// and inline nesting (emphasis and links). Past the limit the parser
/// stops building structure and emits the remainder as plain text.
///
/// This is the only guard against unbounded recursion on the untrusted
/// model output this component renders. A Rust stack overflow is an
/// uncatchable process abort — no panic hook, no terminal restore — so
/// the cap, not graceful unwinding, is what keeps a pathologically
/// nested message from taking the TUI down. Because [`Block`] and
/// [`Inline`] are only ever built by the parser, capping it bounds the
/// AST depth and therefore also bounds the render-time recursion that
/// walks it; the render methods need no separate guard.
///
/// 64 is far above realistic content (a handful of levels) while
/// keeping the worst-case render stack to a few hundred frames.
const MAX_NESTING_DEPTH: usize = 64;

/// Length of the backtick run opening `trimmed`, if it qualifies as a
/// code fence (three or more backticks).
fn fence_len(trimmed: &str) -> Option<usize> {
    let n = trimmed.chars().take_while(|&c| c == '`').count();
    (n >= 3).then_some(n)
}

/// Parse markdown text into blocks.
///
/// `depth` tracks block-nesting (incremented per blockquote level,
/// shared with [`parse_list`]); top-level callers pass 0. At
/// [`MAX_NESTING_DEPTH`] the text is emitted as one literal paragraph
/// rather than recursing further.
fn parse_markdown(text: &str, depth: usize) -> Vec<Block> {
    if depth >= MAX_NESTING_DEPTH {
        return vec![Block::Paragraph(parse_inline(text, 0))];
    }
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

        // Fenced code block. CommonMark fence semantics: the opener
        // is a run of three or more backticks, optionally followed by
        // an info string; the closer is a run of at least as many
        // backticks with nothing else on the line. Tracking the fence
        // length is what makes nesting work: a ``` block quoted
        // inside a ```` fence stays inside it, and an opener-looking
        // line with an info string never closes a block.
        if let Some(open_len) = fence_len(trimmed) {
            let lang = trimmed[open_len..].trim().to_string();
            let lang = if lang.is_empty() { None } else { Some(lang) };
            let mut code_lines: Vec<&str> = Vec::new();
            i += 1;
            while i < lines.len() {
                let t = lines[i].trim();
                if fence_len(t).is_some_and(|n| n >= open_len) && t.chars().all(|c| c == '`') {
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
            let sub_blocks = parse_markdown(&inner, depth + 1);
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
            let (list, new_i) = parse_list(&lines, i, false, depth);
            blocks.push(list);
            i = new_i;
            continue;
        }

        // Ordered list.
        if line_number(line).is_some() {
            let (list, new_i) = parse_list(&lines, i, true, depth);
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
            blocks.push(Block::Paragraph(parse_inline(&text, 0)));
        }
    }

    blocks
}

/// Join paragraph source lines with a literal `\n` between them.
///
/// We deliberately diverge from strict CommonMark "soft break renders
/// as space" semantics: every newline in the source is preserved as a
/// newline in the rendered output. Downstream `wrap_text_with_ansi`
/// splits on `\n` to produce visible rows. The motivation is UX: a
/// CLI user typing a multi-line message expects each typed line to
/// render on its own row — the "concatenate-with-space" CommonMark
/// default isn't a good fit for an agent's chat surface.
///
/// CommonMark also recognizes two explicit hard-line-break markers at
/// the end of a paragraph line:
///
/// 1. Two or more trailing spaces.
/// 2. A single trailing backslash.
///
/// Even though every soft break already inserts a `\n`, we still
/// strip these markers when present so they don't render literally as
/// trailing whitespace or a stray `\\` at end-of-row. The user's
/// intent ("force a line break here") is honored either way; we just
/// keep the marker bytes from leaking into the visible output.
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
    Some(Block::Heading(level, parse_inline(rest, 0)))
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
///
/// `depth` is the shared block-nesting counter (see [`parse_markdown`]);
/// a nested sub-list opens only while `depth + 1 < MAX_NESTING_DEPTH`,
/// otherwise the deeper line folds into the current item as
/// continuation text.
fn parse_list(lines: &[&str], start: usize, ordered: bool, depth: usize) -> (Block, usize) {
    // Indentation of the first list marker defines the base level. A
    // later line at or beyond this indent that is NOT itself a list
    // marker is treated as a continuation of the last item's content.
    // A list marker at strictly greater indent opens a nested list.
    let base_indent = indent_of(lines[start]);

    let mut pending: Vec<PendingItem> = Vec::new();
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
                    pending.push(PendingItem {
                        text: line[base_indent + marker_len..].to_string(),
                        sub_blocks: Vec::new(),
                        number: None,
                    });
                    i += 1;
                }
                (true, _, Some(_)) => {
                    let (_ind, marker_len, n) = is_number.unwrap();
                    pending.push(PendingItem {
                        text: line[base_indent + marker_len..].to_string(),
                        sub_blocks: Vec::new(),
                        number: Some(n),
                    });
                    i += 1;
                }
                _ => break,
            }
        } else {
            // Deeper indent than our base.
            //
            // A nested list opens only while we have block-nesting
            // budget; at the [`MAX_NESTING_DEPTH`] cap the deeper line
            // is folded into the current item as continuation text so
            // pathologically indented input can't recurse without
            // bound.
            if (is_bullet.is_some() || is_number.is_some()) && depth + 1 < MAX_NESTING_DEPTH {
                // Nested list. Parse it recursively and attach to the
                // most recent item's sub_blocks.
                let nested_ordered = is_number.is_some();
                let (nested, new_i) = parse_list(lines, i, nested_ordered, depth + 1);
                if let Some(last) = pending.last_mut() {
                    last.sub_blocks.push(nested);
                }
                i = new_i;
            } else if let Some(last) = pending.last_mut() {
                // Continuation text under the last item (also the
                // landing spot for a nested marker we refused to
                // descend into at the depth cap). Join onto the item's
                // text with a single space; the whole item is parsed as
                // one inline run below, so a code span / emphasis / link
                // on the continuation line — or one straddling the wrap —
                // renders as markup rather than literal characters.
                last.text.push(' ');
                last.text.push_str(trimmed);
                i += 1;
            } else {
                break;
            }
        }
    }

    // Parse each item's accumulated text once, now that any soft-wrapped
    // continuation lines have been folded in.
    let items: Vec<ListItem> = pending
        .into_iter()
        .map(|p| ListItem {
            content: parse_inline(&p.text, 0),
            sub_blocks: p.sub_blocks,
            number: p.number,
        })
        .collect();

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

    let headers: Vec<Vec<Inline>> = header_cells.iter().map(|c| parse_inline(c, 0)).collect();
    let mut rows: Vec<Vec<Vec<Inline>>> = Vec::new();
    let mut i = start + 2;

    while i < lines.len() {
        let Some(cells) = split_table_row(lines[i]) else {
            break;
        };
        let row: Vec<Vec<Inline>> = cells
            .iter()
            .map(|c| parse_inline(c, 0))
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
            // cell per column.
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
///
/// `depth` tracks inline-nesting (emphasis/link recursion), independent
/// of block nesting; block-level callers pass 0. At [`MAX_NESTING_DEPTH`]
/// the text is returned as a single literal run rather than recursing.
fn parse_inline(text: &str, depth: usize) -> Vec<Inline> {
    if depth >= MAX_NESTING_DEPTH {
        return vec![Inline::Text(text.to_string())];
    }
    let mut result: Vec<Inline> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Bold (**text** or __text__). Word-boundary rule: the opening
        // `**`/`__` must not be preceded by a word character, and the
        // closing `**`/`__` must not be followed by one. Prevents
        // `5**4**3` from bolding the `4`, and (paired with the italic
        // arm below) keeps `_` from opening intraword.
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
                result.push(Inline::Bold(parse_inline(&inner, depth + 1)));
                i = end + 2;
                continue;
            }
        }

        // Strikethrough (~~text~~). Strict double-tilde delimiters
        // with non-whitespace boundaries: the character immediately
        // after the opening `~~` and the character immediately before
        // the closing `~~` must both be non-whitespace and non-tilde,
        // and the character immediately after the closing `~~` (if
        // any) must not be a tilde. Prevents loose tilde usage —
        // `~~ foo ~~`, `~~foo~~~`, `~~~~~~~~` — from accidentally
        // activating strikethrough.
        if i + 1 < chars.len()
            && chars[i] == '~'
            && chars[i + 1] == '~'
            && let Some(&after_open) = chars.get(i + 2)
            && !after_open.is_whitespace()
            && after_open != '~'
            && let Some(end) = find_strict_strikethrough_close(&chars, i + 2)
        {
            if !current.is_empty() {
                result.push(Inline::Text(std::mem::take(&mut current)));
            }
            let inner: String = chars[i + 2..end].iter().collect();
            result.push(Inline::Strikethrough(parse_inline(&inner, depth + 1)));
            i = end + 2;
            continue;
        }

        // Italic (*text* or _text_). Word-boundary rule: the opening
        // `*`/`_` must not be preceded by a word character, and the
        // closing must not be followed by one. Prevents `5*4*3` from
        // italicizing the `4`, and `foo_bar_baz` from being parsed as
        // emphasis. The `chars[i - 1] != chars[i]` guard rejects the
        // tail of a longer delimiter run (e.g. the second `*` in a
        // rejected `**` opener) so `5**4**3` doesn't fall through to
        // italic on `4`.
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
                    result.push(Inline::Italic(parse_inline(&inner, depth + 1)));
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
                        // emphasis.
                        result.push(Inline::Link(parse_inline(&link_text, depth + 1), link_url));
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

/// Find the strict closing `~~` for a strikethrough span starting at
/// `start` (the index after the opening `~~`).
///
/// Enforces two boundary rules:
///
/// - The character immediately before the closing `~~` must be
///   non-whitespace and non-tilde. Without this `~~foo ~~` would
///   strikethrough `foo `, which doesn't match GFM-style strict
///   strikethrough.
/// - The character immediately after the closing `~~` (if any) must
///   not be another tilde. Without this `~~foo~~~` would strikethrough
///   `foo` and leave a stray `~`; the strict reading rejects the
///   match outright when the run of closing tildes exceeds two.
///
/// Returns `None` for an empty content (the caller already enforces
/// non-whitespace, non-tilde at `start`, but this is the structural
/// reason the loop starts at `start + 1`).
fn find_strict_strikethrough_close(chars: &[char], start: usize) -> Option<usize> {
    if start + 2 > chars.len() {
        return None;
    }
    for i in (start + 1)..=chars.len() - 2 {
        if chars[i] == '~' && chars[i + 1] == '~' {
            let prev = chars[i - 1];
            if prev.is_whitespace() || prev == '~' {
                continue;
            }
            if let Some(&next) = chars.get(i + 2)
                && next == '~'
            {
                continue;
            }
            return Some(i);
        }
    }
    None
}

/// Like a plain "find marker" scan, but skips marker occurrences that
/// are followed by a word character. Used by the bold/italic
/// word-boundary rule: a closing `*`/`_` (single or double) must sit
/// at a non-word boundary on its right, otherwise it isn't a valid
/// emphasis close.
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
/// instead.
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
/// The render-affecting state — padding, theme, and default text
/// style — is fixed at construction time. Only the text content is
/// mutable post-construction (via [`Markdown::set_text`]). Callers
/// that need to swap theme, padding, or default text style should
/// build a fresh `Markdown`.
///
/// OSC 8 hyperlink emission is gated on
/// [`crate::capabilities::get_capabilities`], read inline at the
/// link-render site rather than carried on the theme.
pub struct Markdown {
    text: String,
    padding_x: usize,
    padding_y: usize,
    theme: MarkdownTheme,
    /// Outer styling applied to paragraph lines. See
    /// [`DefaultTextStyle`].
    default_text_style: Option<DefaultTextStyle>,
    // Cache.
    cached_text: Option<String>,
    cached_width: Option<usize>,
    cached_lines: Option<Vec<String>>,
}

impl Markdown {
    /// Create a new Markdown component.
    ///
    /// Padding axes and the optional default text style are required
    /// at construction. The theme is taken by value so the tui crate
    /// stays palette-agnostic; the agent layer is responsible for
    /// assembling a [`MarkdownTheme`] from its central palette.
    /// Callers that don't want a default text style pass `None`.
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
            cached_text: None,
            cached_width: None,
            cached_lines: None,
        }
    }

    /// Set the text content.
    ///
    /// The only mutator on `Markdown` post-construction. Padding,
    /// theme, and the default text style are immutable per instance —
    /// callers that need to swap them should build a fresh component.
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
    /// set, returns the input unchanged. Color is applied first
    /// (innermost), then text decorations in
    /// `bold → italic → strikethrough → underline` order on top.
    /// Each text-decoration call routes through the configured
    /// [`MarkdownTheme`] (`theme.bold`, `theme.italic`, etc.) so a
    /// custom theme can override the SGR shape of each decoration.
    ///
    /// `bg_color` is intentionally not handled here — it's wired
    /// through the row-emission stage of [`Self::render`] instead;
    /// see [`DefaultTextStyle::bg_color`].
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
    /// the paragraph branch builds its own default-style context) and
    /// for list-item sub-blocks (which stay context-free).
    fn render_block(&self, block: &Block, content_width: usize) -> Vec<String> {
        self.render_block_in_context(block, content_width, None)
    }

    /// Render a block to lines under an optional outer inline-style
    /// context.
    ///
    /// The `ctx` parameter is forwarded to the paragraph branch (so a
    /// paragraph inside a blockquote consumes the quote's inline
    /// context instead of the document-level default style). Heading
    /// and every other block branch ignores `ctx` and either builds
    /// its own context (heading) or doesn't need one (code blocks,
    /// lists, tables, hr).
    fn render_block_in_context(
        &self,
        block: &Block,
        content_width: usize,
        ctx: Option<&InlineStyleContext>,
    ) -> Vec<String> {
        match block {
            Block::Heading(level, inlines) => {
                // Build a heading-specific `InlineStyleContext`:
                // `apply_text` wraps each text run with the heading
                // style (heading + bold + optional H1 underline);
                // `style_prefix` is the opens of that wrap, re-emitted
                // after every non-text inline so the heading color /
                // bold / underline reopens on whatever follows.
                // Without this, an inline-code span's own `\x1b[39m`
                // inside `# foo \`bar\` baz` would strip the heading
                // color from `baz`.
                //
                // The heading branch uses *only* the heading wrap on
                // text runs and does NOT thread the document's
                // `default_text_style` through the heading body or
                // wrap the rendered heading line. Paragraphs are the
                // only block that gets the default text style applied;
                // see [`DefaultTextStyle`].
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
                    // get the same treatment.
                    let prefix_text = format!("{} ", "#".repeat(usize::from(*level)));
                    format!("{}{}", heading_apply_ref(&prefix_text), body)
                } else {
                    body
                };
                // No outer `apply_default_style` wrap here.
                //
                // The heading branch is width-agnostic — we emit the
                // styled heading as a single line (or one line per
                // embedded `\n` once the outer wrap pass splits on
                // them) and let the outer wrap pass in
                // [`Component::render`] break long headings to
                // `content_width`.
                vec![styled, String::new()]
            }
            Block::Paragraph(inlines) => {
                // Thread `default_text_style` through the inline walk
                // via an `InlineStyleContext` instead of wrapping the
                // whole paragraph string once at the end. An outer
                // wrap (`apply_default_style(&text)` after
                // `render_inlines`) loses the default style's color
                // on any text following an inline reset: an inline
                // code's `\x1b[39m` resets the foreground, and the
                // outermost color closer at the end of the paragraph
                // doesn't re-open it for the trailing text. Threading
                // through the context puts the default-style opens
                // back after every non-text inline so trailing text
                // re-opens gray + italic + whatever else.
                //
                // Scope: only paragraphs get the default text style
                // threaded. Lists and table cells stay on identity-
                // context `render_inlines`, matching the
                // [`DefaultTextStyle`] rustdoc contract. Headings use
                // their own heading context above and intentionally
                // do not get a default-style wrap.
                //
                // When `ctx` is supplied (we're inside a blockquote),
                // use it directly and skip the default-style build.
                // The supplied `ctx` is the quote inline context —
                // identity `apply_text` plus a `style_prefix` of the
                // quote wrapper's opens, re-emitted after each
                // non-text inline so the quote styling reopens for
                // whatever follows. The default text style is
                // intentionally not applied inside blockquotes.
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
                // The paragraph branch is width-agnostic. Emit the
                // styled paragraph as a single string; the outer wrap
                // pass in [`Component::render`] (which calls
                // `wrap_text_with_ansi(line, content_width)` over
                // every emitted line, splitting on `\n` along the way)
                // is responsible for the visual wrap.
                vec![text, String::new()]
            }
            Block::CodeBlock(lang, code) => {
                let mut lines = Vec::new();
                let border_open = if let Some(l) = lang {
                    format!("```{}", l)
                } else {
                    "```".to_string()
                };
                lines.push((self.theme.code_block_border)(&border_open));

                // Pick the highlighter: an explicit override hook
                // wins, otherwise the built-in syntect highlighter
                // when enabled, otherwise plain text styled by
                // `code_block`.
                let highlighted = match &self.theme.highlight_code {
                    Some(hook) => hook(code, lang.as_deref()),
                    None if self.theme.syntax_highlight => {
                        self.highlight_code(code, lang.as_deref())
                    }
                    None => code.lines().map(|l| (self.theme.code_block)(l)).collect(),
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
                // Clamp to at least 1 so a degenerate outer
                // `content_width` (e.g. `content_width = 1`) recurses
                // with a usable per-grapheme width. For non-degenerate
                // widths (`content_width >= border_width + 1`, i.e.
                // >= 3 with the default `│ ` border) the `.max(1)` is
                // a no-op so the common path is unchanged.
                let inner_width = content_width.saturating_sub(border_width).max(1);

                // Quote-style machinery:
                //
                // - `quote_apply` wraps each line with
                //   `theme.quote(theme.italic(line))`. The explicit
                //   `theme.italic` underwrap means a custom non-italic
                //   `theme.quote` (e.g. just a color) still ships
                //   italic.
                //
                // - `quote_prefix` is the *opens* of `quote_apply`,
                //   extracted via [`get_style_prefix`]. For a theme
                //   where `theme.quote = style::italic` that's
                //   `\x1b[3m\x1b[3m` (the quote's italic plus the
                //   underwrap's italic — both fire). For a custom
                //   `theme.quote = style::cyan` it's
                //   `\x1b[36m\x1b[3m`.
                //
                // - `quote_inline_ctx` is identity `apply_text` (the
                //   default text style does not apply inside
                //   blockquotes) plus `style_prefix = quote_prefix`.
                //   Threaded through every sub-block so paragraph
                //   inlines re-emit `quote_prefix` after each
                //   non-text inline; the outer `apply_quote_style`
                //   wrap then re-opens the quote-italic block.
                //
                // - `apply_quote_style(line)` replaces every
                //   `\x1b[0m` (full SGR reset) with
                //   `\x1b[0m{quote_prefix}` so downstream content
                //   (notably syntect-highlighted code which
                //   terminates each line with `\x1b[0m`) reopens the
                //   quote/italic style after the reset, then wraps
                //   with `quote_apply`. Skipped when `quote_prefix`
                //   is empty (defensive — the sentinel trick in
                //   `get_style_prefix` returns "" only when the
                //   wrapper swallows the sentinel, which neither
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

                // Phased pipeline.
                //
                // Phase 1: collect rendered sub-block lines.
                let mut quote_lines: Vec<String> = Vec::new();
                for block in sub_blocks {
                    let block_lines =
                        self.render_block_in_context(block, inner_width, Some(&quote_inline_ctx));
                    quote_lines.extend(block_lines);
                }

                // Phase 2: pop trailing blanks. Avoids rendering an
                // extra empty quote line before the outer blockquote
                // spacing. Mid-quote blank rows from sub-block
                // separators stay and get the wrap; only the tail
                // trailing blanks are dropped here.
                while quote_lines.last().is_some_and(String::is_empty) {
                    quote_lines.pop();
                }

                // Phase 3: apply quote style → wrap → prepend
                // border. Every remaining row, including mid-quote
                // blanks, goes through `apply_quote_style` so a blank
                // row gets the empty-content wrap (`quote_apply("")`)
                // instead of a bare border. The wrap step after
                // `apply_quote_style` is a no-op for rows that already
                // fit (the paragraph's internal wrap to `inner_width`
                // covers the common case), but a sub-block whose
                // lines exceed `inner_width` (e.g. a wide code-block
                // row at a narrow render width) now wraps correctly
                // with ANSI state propagation, instead of overflowing
                // past the border.
                let mut lines = Vec::new();
                for ql in &quote_lines {
                    let styled = apply_quote_style(ql, quote_apply_ref, &quote_prefix);
                    let wrapped = wrap_text_with_ansi(&styled, inner_width);
                    for wl in wrapped {
                        lines.push(format!("{}{}", border, wl));
                    }
                }
                // One blank row after the whole quote. The outer
                // trailing-blank trim drops it if this is the
                // document's last block.
                lines.push(String::new());
                lines
            }
            Block::HorizontalRule => {
                // Render `─.repeat(min(content_width, 80))`. The 80-cell
                // cap keeps a horizontal rule readable on very wide
                // terminals.
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
    /// `wrap_text_with_ansi` instead — "too narrow to render a stable
    /// table".
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

        // Fallback branch: when the available width can't fit
        // even one cell per column, rendering a table would produce
        // visually broken output (a border row wider than the
        // content, or zero-width cells). Fall back to
        // `wrap_text_with_ansi(raw, content_width)` plus the trailing-
        // blank spacer the rest of `render_block`'s arms emit (the
        // outer `Markdown::render` trim will collapse it if this is
        // the last block).
        //
        // Our parser always populates `raw` for `Block::Table`, and
        // our trailing-blank emission shape (always emit, outer trim
        // drops it when not needed) handles the document-tail case.
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
        // Forward `ctx` to inline rendering so a table inside a
        // blockquote re-emits the quote prefix after every non-text
        // inline. When `ctx` is `None` (top-level table) we use the
        // identity context via `render_inlines`.
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

        // Per-column natural width (max visible cell width) and minimum
        // width (longest unbreakable token, capped at
        // `MAX_UNBROKEN_TOKEN_WIDTH`). The cap keeps one very long token
        // from pinning a column to its full width and starving the
        // others; tokens past the cap are hard-broken by
        // `wrap_text_with_ansi`. `natural` is left uncapped so short
        // content still gets its preferred width.
        let mut natural = vec![0_usize; n_cols];
        let mut minimum = vec![1_usize; n_cols];
        for col in 0..n_cols {
            if let Some(text) = header_text.get(col) {
                natural[col] = natural[col].max(visible_width(text));
                minimum[col] =
                    minimum[col].max(longest_token_width(text).min(MAX_UNBROKEN_TOKEN_WIDTH));
            }
            for row in &row_text {
                if let Some(text) = row.get(col) {
                    natural[col] = natural[col].max(visible_width(text));
                    minimum[col] =
                        minimum[col].max(longest_token_width(text).min(MAX_UNBROKEN_TOKEN_WIDTH));
                }
            }
        }

        // Past the fallback gate, `available_for_cells >= n_cols >= 1`,
        // so `distribute_column_widths` always sees a usable budget.
        let widths = distribute_column_widths(&natural, &minimum, available_for_cells);

        let mut lines: Vec<String> = Vec::new();
        let separator = make_separator_row(&widths);

        lines.push(make_top_border_row(&widths));
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
        lines.push(make_bottom_border_row(&widths));
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
            // Forward `ctx` to inline rendering so a list inside a
            // blockquote re-emits the quote prefix after every
            // non-text inline (codespan, bold, etc.). When `ctx` is
            // `None` (top-level list, or list inside a non-context
            // sub-block), we use the identity context via
            // `render_inlines`.
            let text = match ctx {
                Some(ctx) => self.render_inline_tokens(&item.content, ctx),
                None => self.render_inlines(&item.content),
            };
            // The list branch is width-agnostic — emit the
            // bullet + text as a single line and let the outer wrap
            // pass in [`Component::render`]
            // (`wrap_text_with_ansi(line, content_width)`) break long
            // items. Continuation lines therefore land flush-left at
            // column 0 of `content_width`. Sub-block lines (nested
            // lists, code blocks, etc.) are extended verbatim — they
            // already carry their own indentation.
            lines.push(format!("{}{}{}", indent, styled_bullet, text));

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
    /// [`InlineStyleContext`].
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
    /// The capability is read inline at render time via
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

    /// Render a sequence of inline tokens under an outer style
    /// context.
    ///
    /// For each text run, `ctx.apply_text` wraps the text in the
    /// outer style (the heading wrap, the default-style wrap, etc.).
    /// For each non-text inline (bold, italic, strikethrough, code,
    /// link), we render the inline with its own styling and then
    /// append `ctx.style_prefix` so the outer style reopens for
    /// whatever follows. The `style_prefix` is just the *opens* of
    /// the outer wrap, extracted via [`get_style_prefix`]; an
    /// inline's own `\x1b[39m` (foreground reset) or `\x1b[22m`
    /// (bold off) would otherwise strip the matching outer state from
    /// following text.
    ///
    /// Trailing dangling `style_prefix` (no text after the last
    /// non-text inline) is trimmed at the end so we don't leave a
    /// stray opens-only sequence at the line boundary.
    ///
    /// Text runs are split by `\n` and `apply_text` is applied per
    /// segment so each line carries its own opens and closes. This
    /// keeps downstream `wrap_text_with_ansi` from inheriting an
    /// unbalanced open across a hard line break.
    ///
    /// Nested inlines (e.g. `**bold *italic***`) recurse with the
    /// same `ctx`. The nested result has the outer-applied per-text
    /// styling baked in, then the per-variant theme wrap (bold,
    /// italic, etc.) wraps that nested result.
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
                    // (heading wrap, default-style wrap) reaches the
                    // link's visible text. The plain-text version
                    // drives the autolink-vs-fallback decision in
                    // [`Markdown::render_link`].
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

    /// Highlight `code`, tokenizing with syntect and coloring each
    /// token through the theme's [`SyntaxStyles`].
    ///
    /// We use syntect only to assign scopes; the colors come from the
    /// palette closures so code blocks track the active theme (and its
    /// color mode) like the rest of the markdown. One `ParseState` /
    /// `ScopeStack` spans the whole block so multi-line constructs
    /// (block comments, multi-line strings) carry their scope across
    /// lines. Each emitted line ends in a full SGR reset (`\x1b[0m`),
    /// matching what downstream blockquote / background re-assertion
    /// keys on.
    fn highlight_code(&self, code: &str, lang: Option<&str>) -> Vec<String> {
        let syntax_set = syntax_set();
        let syntax = lang
            .and_then(|l| syntax_set.find_syntax_by_token(l))
            .unwrap_or_else(|| syntax_set.find_syntax_plain_text());

        let styles = &self.theme.syntax;
        let selectors = category_selectors();

        let mut parse_state = ParseState::new(syntax);
        let mut stack = ScopeStack::new();
        let mut lines = Vec::new();

        for line in code.lines() {
            let Ok(ops) = parse_state.parse_line(line, syntax_set) else {
                // Tokenizing failed: fall back to the plain code-block
                // styler so the line still renders sensibly.
                lines.push((self.theme.code_block)(line));
                continue;
            };

            let mut out = String::new();
            let mut errored = false;
            for (text, op) in ScopeRegionIterator::new(&ops, line) {
                // The op precedes its text region, so apply it before
                // classifying (the leading region carries a no-op).
                if stack.apply(op).is_err() {
                    errored = true;
                    break;
                }
                if text.is_empty() {
                    continue;
                }
                match classify_scope(stack.as_slice(), selectors) {
                    Some(category) => out.push_str(&styles.style(category)(text)),
                    None => out.push_str(text),
                }
            }

            if errored {
                lines.push((self.theme.code_block)(line));
            } else {
                out.push_str("\x1b[0m");
                lines.push(out);
            }
        }
        lines
    }
}

impl Component for Markdown {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // The cache check runs *before* the empty-text guard so that
        // a repeat render of a whitespace-only input (whose first call
        // wrote the empty-vec cache below) returns the cached result
        // without re-running `text.trim()`.
        if let (Some(ct), Some(cw), Some(cl)) =
            (&self.cached_text, self.cached_width, &self.cached_lines)
        {
            if ct == &self.text && cw == width {
                return cl.clone();
            }
        }

        // Empty / whitespace-only text is treated as "nothing to
        // draw" and returns an empty vec without parsing. We populate
        // the cache here so the cache check above hits on the next
        // call with the same input.
        if self.text.trim().is_empty() {
            self.cached_text = Some(self.text.clone());
            self.cached_width = Some(width);
            self.cached_lines = Some(Vec::new());
            return Vec::new();
        }

        // Clamp `content_width` to `1` on degenerate render widths
        // (`width = 0`, or `width < 2 * padding_x`). The downstream
        // paths (`render_block`, `render_list`, table rendering, hr,
        // paragraph wrap) all accept `width = 1` gracefully —
        // `wrap_text_with_ansi` breaks long words one grapheme per
        // row, hr emits a one-cell `─`, and bullets / quote borders
        // reduce the inner width via their own saturating
        // subtractions.
        let content_width = width.saturating_sub(self.padding_x * 2).max(1);

        let left_margin = " ".repeat(self.padding_x);
        let right_margin = " ".repeat(self.padding_x);
        // `bg_fn`: enabled when the optional
        // `default_text_style.bg_color` is set. When `Some`, every
        // emitted row (content + top/bottom padding) is padded to the
        // full render width and routed through this closure so the
        // background reaches every cell of the bounding box.
        let bg_fn: Option<&Arc<dyn Fn(&str) -> String>> = self
            .default_text_style
            .as_ref()
            .and_then(|d| d.bg_color.as_ref());
        // Normalize tabs to three spaces before parsing. The cache
        // key (`cached_text`) holds the *original* text so an
        // unchanged input still hits the cache; normalization is
        // idempotent and deterministic, so a hit returns the same
        // result we'd produce by re-normalizing.
        let normalized = self.text.replace('\t', TAB_AS_SPACES);
        let blocks = parse_markdown(&normalized, 0);

        // Phase 1: collect block-rendered lines (no horizontal
        // padding, no outer wrap yet). The per-block `render_block`
        // arms are width-agnostic (paragraph/heading/list emit one
        // line per source line; blockquote and table wrap internally
        // at their own narrower widths), so the lines coming out of
        // this loop can exceed `content_width` and need an outer wrap
        // pass.
        let mut content_lines: Vec<String> = Vec::new();
        for block in &blocks {
            content_lines.extend(self.render_block(block, content_width));
        }

        // Phase 2: outer wrap pass. Block renderers are width-
        // agnostic; the wrap to `content_width` happens here so list
        // bullets, paragraphs, and headings all word-break identically
        // with continuation lines flush-left at column 0. Block
        // renderers that wrap internally at a NARROWER inner width
        // (blockquotes use `inner_width = content_width -
        // border_width`, table cells use `widths[col]`) already
        // produce lines `<= content_width`, so the outer wrap is a
        // no-op on those.
        let mut wrapped: Vec<String> = Vec::with_capacity(content_lines.len());
        for line in content_lines.drain(..) {
            wrapped.extend(wrap_text_with_ansi(&line, content_width));
        }

        // Phase 3: row emission. Each wrapped content row is
        // bracketed with `left_margin` / `right_margin` and then
        // either:
        //   * routed through `bg_fn` (which
        //     `apply_background_to_line` pads to the full render
        //     width before tinting), or
        //   * padded out to `width` with plain spaces so the row
        //     spans the entire component bounding box even without a
        //     background.
        let mut rendered_rows: Vec<String> = Vec::with_capacity(wrapped.len());
        for line in wrapped {
            let line_with_margins = format!("{}{}{}", left_margin, line, right_margin);
            let row = if let Some(bg) = bg_fn {
                apply_background_to_line(&line_with_margins, width, bg.as_ref())
            } else {
                let visible_len = visible_width(&line_with_margins);
                let padding_needed = width.saturating_sub(visible_len);
                format!("{}{}", line_with_margins, " ".repeat(padding_needed))
            };
            rendered_rows.push(row);
        }

        // Trim trailing blank rows from the rendered content before
        // applying top/bottom padding.
        //
        // Each [`Markdown::render_block`] arm unconditionally appends
        // a single `String::new()` so that one blank row separates
        // this block from whatever follows in the source. Our parser
        // collapses blank source lines into nothing (see
        // [`parse_markdown`]), so we have no "next-token type"
        // available to gate the trailing emission; unconditional emit
        // + post-trim is the structurally cleaner shape for this AST.
        //
        // The deliberate behavior for the chat surface this crate
        // ships into: a document ending in any block emits zero
        // trailing blank rows, regardless of how many `String::new()`
        // spacers the emit path layered on. Trailing typing artifacts
        // in an LLM-emitted message read as dead space at the bottom
        // of the rendered cell, not intentional structure. The
        // cross-block invariant is covered by the
        // `does_not_add_a_trailing_blank_line_when_*_is_last` family
        // in `tests/markdown.rs` for headings, paragraphs, code
        // blocks, blockquotes, tables, and horizontal rules.
        //
        // The predicate is ANSI-stripping-aware ([`is_blank_row`]) so
        // the per-row right-pad and the `bg_fn` wrap layered on top
        // (a row of background-tinted spaces) both count as blank.
        while rendered_rows
            .last()
            .map(|l| is_blank_row(l))
            .unwrap_or(false)
        {
            rendered_rows.pop();
        }

        // Top / bottom padding rows (`padding_y`). The bg-fn
        // branch routes through `apply_background_to_line` so the
        // background reaches every padding cell; the no-bg branch
        // emits a row of plain spaces. Both branches produce a row
        // of exactly `width` cells; the trailing-blank trim above is
        // ANSI-aware so a future render that re-runs trim over these
        // rows would still treat them as blank.
        let empty_line = " ".repeat(width);
        let padding_row: String = if let Some(bg) = bg_fn {
            apply_background_to_line(&empty_line, width, bg.as_ref())
        } else {
            empty_line
        };

        let mut result: Vec<String> = Vec::with_capacity(rendered_rows.len() + 2 * self.padding_y);
        for _ in 0..self.padding_y {
            result.push(padding_row.clone());
        }
        result.extend(rendered_rows);
        for _ in 0..self.padding_y {
            result.push(padding_row.clone());
        }

        // Cache the pre-fallback result so a subsequent render of
        // the same `(text, width)` hits the cache check at the top of
        // this method. The cache stores `result` *before* the `[""]`
        // fallback below: a cache-hit returns whatever was computed
        // verbatim (potentially the empty vec), only the first-call
        // return path runs through the fallback.
        self.cached_text = Some(self.text.clone());
        self.cached_width = Some(width);
        self.cached_lines = Some(result.clone());

        // `[""]` fallback: a non-empty input that produces zero
        // rendered rows still emits a single blank row so the
        // component occupies one cell. Defensive — every
        // `parse_markdown(non-whitespace)` returns at least one
        // block, and every `render_block` arm emits at least one
        // line plus a separator, so this branch is unreachable for
        // real inputs.
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
/// ANSI-aware). Callers cap this at `MAX_UNBROKEN_TOKEN_WIDTH` when
/// deriving a column's minimum width; up to that cap a column stays wide
/// enough to hold its longest token without mid-token wrapping.
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

/// Upper bound on a column's minimum width. A column floor is the
/// longest unbreakable token it contains, clamped to this many columns
/// so a single overlong token (URL, hash, identifier) can't pin the
/// column to its full width and starve its neighbours. Tokens past the
/// cap are hard-broken by `wrap_text_with_ansi`.
const MAX_UNBROKEN_TOKEN_WIDTH: usize = 30;

/// Distribute `available` columns of cell width across `natural.len()`
/// columns, given each column's `natural` (preferred) width and
/// `minimum` (floor) width.
///
/// The allocation proceeds in two stages:
///
/// 1. **Effective minimums.** Start from the per-column `minimum`. If
///    their sum already exceeds `available`, the table is narrower than
///    its floors: collapse every column to width 1 and hand the
///    remaining budget out proportional to each column's
///    `minimum - 1` weight (leftover distributed one column at a time).
/// 2. **Widths.** If every column's `natural` width fits
///    (`sum(natural) <= available`), give each column
///    `max(natural, effective_min)`. Otherwise floor each column at its
///    effective minimum and distribute the leftover budget proportional
///    to each column's growth potential (`natural - effective_min`),
///    then hand out any rounding remainder one column at a time to
///    columns still below their natural width.
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

    // Round-off: hand out the remaining budget one column at a time to
    // any column still below its natural width.
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

/// Build a horizontal rule row spanning the table, using the given
/// corner/junction glyphs: `left` at the start, `mid` between columns,
/// `right` at the end. The fill character is always `─`, sized to each
/// column width plus the two cells of padding so it matches the
/// `│ … │` data rows.
fn make_border_row(widths: &[usize], left: char, mid: char, right: char) -> String {
    let mut out = String::new();
    out.push(left);
    for (idx, w) in widths.iter().enumerate() {
        if idx > 0 {
            out.push(mid);
        }
        // Two chars of padding become `─`s to match the border width.
        for _ in 0..(w + 2) {
            out.push('─');
        }
    }
    out.push(right);
    out
}

/// The `┌─...┬─...┐` rule that frames the top of the table.
fn make_top_border_row(widths: &[usize]) -> String {
    make_border_row(widths, '┌', '┬', '┐')
}

/// The `├─...┼─...┤` row used as the header separator and between data
/// rows.
fn make_separator_row(widths: &[usize]) -> String {
    make_border_row(widths, '├', '┼', '┤')
}

/// The `└─...┴─...┘` rule that frames the bottom of the table.
fn make_bottom_border_row(widths: &[usize]) -> String {
    make_border_row(widths, '└', '┴', '┘')
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
    /// input through verbatim. Matches
    /// `tests/support/themes.rs::identity_markdown_theme` so unit
    /// tests and integration tests share the same convention.
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
            syntax_highlight: true,
            syntax: SyntaxStyles {
                comment: Arc::new(|s| s.to_string()),
                keyword: Arc::new(|s| s.to_string()),
                function: Arc::new(|s| s.to_string()),
                variable: Arc::new(|s| s.to_string()),
                string: Arc::new(|s| s.to_string()),
                number: Arc::new(|s| s.to_string()),
                type_name: Arc::new(|s| s.to_string()),
                operator: Arc::new(|s| s.to_string()),
                punctuation: Arc::new(|s| s.to_string()),
            },
        }
    }

    /// `Markdown::render` populates the cache fields at the tail of
    /// a non-empty render. With the cache write in place, the cache
    /// check at the top of `render` actually fires on a subsequent
    /// call with the same `(text, width)`. Uses private-field access
    /// via the in-module `mod tests` block — the integration-test
    /// suite can only observe behavior, not the cache state directly.
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

    /// A second render with the same `(text, width)` returns the
    /// cached lines verbatim (the cache-check at the top of `render`
    /// fires before any parse work). Observable via the returned
    /// value being equal to the first call; the underlying parse-skip
    /// is locked in by the cache-state assertion in the companion
    /// test above.
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

    /// Changing the text via `set_text` invalidates the cache so
    /// the next render reflects the new input rather than returning
    /// a stale clone of the previous result.
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

    /// A width change with the same text also invalidates the
    /// cache match — the cache-check requires both `text` and
    /// `width` to match, so a `(text, w1)` cached result doesn't
    /// satisfy a `(text, w2)` query. The new render writes a fresh
    /// cache entry for the new width.
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

    /// When every column's natural width fits the budget, each column
    /// is allocated its natural width (raised to its floor where the
    /// floor is larger).
    #[test]
    fn distribute_returns_natural_widths_when_everything_fits() {
        let widths = distribute_column_widths(&[10, 5, 8], &[3, 3, 3], 100);
        assert_eq!(widths, vec![10, 5, 8]);
    }

    /// The allocation never exceeds the available budget, even when the
    /// natural widths overflow it.
    #[test]
    fn distribute_never_exceeds_budget_when_shrinking() {
        let widths = distribute_column_widths(&[40, 40, 40], &[5, 5, 5], 30);
        assert!(
            widths.iter().sum::<usize>() <= 30,
            "allocation {widths:?} overflowed the budget",
        );
        assert!(widths.iter().all(|&w| w >= 1));
    }

    /// When even the per-column floors don't fit, columns collapse and
    /// the budget is shared out by floor weight — every column still
    /// gets at least one column and the total stays within budget.
    #[test]
    fn distribute_collapses_when_minimums_exceed_budget() {
        let widths = distribute_column_widths(&[30, 30], &[30, 30], 10);
        assert_eq!(widths.len(), 2);
        assert!(widths.iter().all(|&w| w >= 1));
        assert!(widths.iter().sum::<usize>() <= 10);
    }

    /// A column whose floor is capped (its longest token exceeds the
    /// cap) does not pin the column to the token width: the neighbour
    /// column still receives a meaningful share of the budget, and the
    /// allocation stays within budget.
    #[test]
    fn distribute_capped_floor_leaves_room_for_neighbours() {
        // Column 0: a 60-wide token capped to `MAX_UNBROKEN_TOKEN_WIDTH`.
        // Column 1: natural 40, floor 8.
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

    /// Deepest block-nesting level in an AST. Recurses over the same
    /// shape the renderer walks, so it's itself bounded by the parser
    /// cap under test.
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

    /// Pathologically nested block input must not recurse without
    /// bound: a long run of `> ` (blockquotes) or increasing-indent
    /// list markers nests one AST level per token, which would overflow
    /// the stack at parse or render time without [`MAX_NESTING_DEPTH`].
    /// The parser caps the descent and folds the remainder into literal
    /// text, so the resulting AST is shallow regardless of input depth.
    #[test]
    fn parser_caps_block_nesting_on_adversarial_input() {
        // ~100k nested blockquotes collapse to a capped quote stack
        // wrapping a single literal paragraph.
        let quotes = parse_markdown(&"> ".repeat(100_000), 0);
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
        let list = parse_markdown(&nested_list, 0);
        assert!(
            block_depth(&list) <= MAX_NESTING_DEPTH + 1,
            "list nesting not capped: depth {}",
            block_depth(&list),
        );
    }
}
