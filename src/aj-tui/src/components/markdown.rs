//! Markdown rendering component.
//!
//! Renders markdown text to styled terminal lines using ANSI escape codes.
//! Supports headings, paragraphs, code blocks (with syntax highlighting via syntect),
//! lists (ordered and unordered, nested), links, blockquotes, horizontal rules,
//! bold, italic, strikethrough, and inline code.

use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::as_24_bit_terminal_escaped;

use crate::ansi::{visible_width, wrap_text_with_ansi};
use crate::component::Component;
use crate::style;

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// Styling functions for markdown rendering.
pub struct MarkdownTheme {
    pub heading: Box<dyn Fn(&str) -> String>,
    pub bold: Box<dyn Fn(&str) -> String>,
    pub italic: Box<dyn Fn(&str) -> String>,
    pub strikethrough: Box<dyn Fn(&str) -> String>,
    pub code: Box<dyn Fn(&str) -> String>,
    pub code_block: Box<dyn Fn(&str) -> String>,
    pub code_block_border: Box<dyn Fn(&str) -> String>,
    pub link: Box<dyn Fn(&str) -> String>,
    pub link_url: Box<dyn Fn(&str) -> String>,
    pub list_bullet: Box<dyn Fn(&str) -> String>,
    pub quote_border: Box<dyn Fn(&str) -> String>,
    pub quote: Box<dyn Fn(&str) -> String>,
    pub hr: Box<dyn Fn(&str) -> String>,
    pub underline: Box<dyn Fn(&str) -> String>,
    /// When `true`, `[text](url)` and autolinked URLs/emails are
    /// emitted with OSC 8 hyperlink sequences instead of `text (url)`
    /// in parentheses.
    ///
    /// Typically seeded from
    /// [`crate::capabilities::get_capabilities`] so the host
    /// terminal's detected support drives the render path; tests that
    /// want to exercise both shapes can flip this directly on the
    /// theme without touching the global cache.
    pub hyperlinks: bool,
    /// Optional override for syntax highlighting. Receives the raw code
    /// block contents and the optional language tag and returns one styled
    /// line per input line. When `None`, the built-in syntect-based
    /// highlighter is used.
    pub highlight_code: Option<Box<dyn Fn(&str, Option<&str>) -> Vec<String>>>,
    /// Prefix applied to each rendered code block line. Defaults to two
    /// spaces when `None`.
    pub code_block_indent: Option<String>,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        Self {
            heading: Box::new(|s| style::bold(s)),
            bold: Box::new(|s| style::bold(s)),
            italic: Box::new(|s| style::italic(s)),
            strikethrough: Box::new(|s| style::strikethrough(s)),
            code: Box::new(|s| style::yellow(s)),
            code_block: Box::new(|s| s.to_string()),
            code_block_border: Box::new(|s| style::dim(s)),
            link: Box::new(|s| style::cyan(s)),
            link_url: Box::new(|s| style::dim(s)),
            list_bullet: Box::new(|s| style::cyan(s)),
            quote_border: Box::new(|s| style::dim(s)),
            quote: Box::new(|s| style::italic(s)),
            hr: Box::new(|s| style::dim(s)),
            underline: Box::new(|s| style::underline(s)),
            hyperlinks: crate::capabilities::get_capabilities().hyperlinks,
            highlight_code: None,
            code_block_indent: None,
        }
    }
}

/// Outer styling applied to rendered paragraph and heading lines,
/// independently of the theme. Used to tint a whole block of "thinking"
/// prose in, say, dim italic gray while leaving the theme's inline
/// styling (bold, inline-code, links) intact.
///
/// Does NOT apply to blockquotes (which use `theme.quote`), code blocks
/// (which carry their own highlighting), or horizontal rules. Lists
/// currently don't receive pre-styling either; when a test port needs
/// it, extend the render path.
pub struct PreStyle {
    /// Optional color wrapper. Called with the line's already-styled
    /// text and returns the same text with a surrounding SGR color.
    pub color: Option<Box<dyn Fn(&str) -> String>>,
    /// When `true`, the line is wrapped in `\x1b[3m ... \x1b[23m`.
    pub italic: bool,
}

impl PreStyle {
    /// Apply the pre-style to `s`, returning a styled copy. If nothing
    /// is configured this is a no-op clone.
    pub(crate) fn apply(&self, s: &str) -> String {
        let mut out = if self.italic {
            crate::style::italic(s)
        } else {
            s.to_string()
        };
        if let Some(color) = &self.color {
            out = color(&out);
        }
        out
    }
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
    /// A blockquote, stored as one pre-parsed inline run per source
    /// line. Per-line rather than nested-blocks so that every source line
    /// renders as its own row (preserving the author's visible line
    /// breaks) with the quote border on each.
    Blockquote(Vec<Vec<Inline>>),
    /// A GitHub-flavored-markdown style table: one header row, one
    /// alignment spec per column, zero or more data rows. Each cell is
    /// pre-parsed inline content.
    Table {
        headers: Vec<Vec<Inline>>,
        alignments: Vec<Alignment>,
        rows: Vec<Vec<Vec<Inline>>>,
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
    Link(String, String), // (text, url)
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
            let inline_lines: Vec<Vec<Inline>> =
                quote_lines.iter().map(|l| parse_inline(l)).collect();
            blocks.push(Block::Blockquote(inline_lines));
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
            let text = para_lines.join(" ");
            blocks.push(Block::Paragraph(parse_inline(&text)));
        }
    }

    blocks
}

fn parse_heading(line: &str) -> Option<Block> {
    let trimmed = line.trim();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|&c| c == '#').count() as u8;
    if level > 6 {
        return None;
    }
    let rest = trimmed[level as usize..].trim();
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
fn parse_inline(text: &str) -> Vec<Inline> {
    let mut result: Vec<Inline> = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Bold (**text** or __text__).
        if i + 1 < chars.len()
            && ((chars[i] == '*' && chars[i + 1] == '*')
                || (chars[i] == '_' && chars[i + 1] == '_'))
        {
            let marker = chars[i];
            if let Some(end) = find_closing_marker(&chars, i + 2, &[marker, marker]) {
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

        // Italic (*text* or _text_).
        if (chars[i] == '*' || chars[i] == '_')
            && (i + 1 < chars.len() && !chars[i + 1].is_whitespace())
        {
            let marker = chars[i];
            if let Some(end) = find_closing_marker(&chars, i + 1, &[marker]) {
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
                        result.push(Inline::Link(link_text, link_url));
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
            result.push(Inline::Link(url.clone(), url));
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
            result.push(Inline::Link(email.clone(), format!("mailto:{}", email)));
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

// ---------------------------------------------------------------------------
// Markdown component
// ---------------------------------------------------------------------------

/// A component that renders markdown text to styled terminal lines.
pub struct Markdown {
    text: String,
    padding_x: usize,
    padding_y: usize,
    theme: MarkdownTheme,
    /// Outer styling applied to paragraph and heading lines. See
    /// [`PreStyle`].
    pre_style: Option<PreStyle>,
    // Syntax highlighting resources (loaded lazily).
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
    // Cache.
    cached_text: Option<String>,
    cached_width: Option<usize>,
    cached_lines: Option<Vec<String>>,
}

impl Markdown {
    /// Create a new Markdown component with the given text.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            padding_x: 1,
            padding_y: 0,
            theme: MarkdownTheme::default(),
            pre_style: None,
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
            cached_text: None,
            cached_width: None,
            cached_lines: None,
        }
    }

    /// Set the text content.
    pub fn set_text(&mut self, text: &str) {
        if self.text != text {
            self.text = text.to_string();
            self.invalidate_cache();
        }
    }

    /// Set the theme.
    pub fn set_theme(&mut self, theme: MarkdownTheme) {
        self.theme = theme;
        self.invalidate_cache();
    }

    /// Set horizontal padding.
    pub fn set_padding_x(&mut self, padding: usize) {
        self.padding_x = padding;
        self.invalidate_cache();
    }

    /// Set vertical padding.
    pub fn set_padding_y(&mut self, padding: usize) {
        self.padding_y = padding;
        self.invalidate_cache();
    }

    /// Enable or disable OSC 8 hyperlink emission on this component's
    /// theme. Equivalent to mutating `theme.hyperlinks` directly;
    /// preserved as a convenience for callers that build a theme once
    /// and only need to toggle this one flag.
    ///
    /// The default comes from
    /// [`crate::capabilities::get_capabilities`], which detects the
    /// host terminal from environment variables.
    pub fn set_hyperlinks(&mut self, enabled: bool) {
        self.theme.hyperlinks = enabled;
        self.invalidate_cache();
    }

    /// Apply outer styling to rendered paragraph and heading lines.
    /// Pass `None` to clear. See [`PreStyle`].
    pub fn set_pre_style(&mut self, pre_style: Option<PreStyle>) {
        self.pre_style = pre_style;
        self.invalidate_cache();
    }

    fn invalidate_cache(&mut self) {
        self.cached_text = None;
        self.cached_width = None;
        self.cached_lines = None;
    }

    /// Wrap `s` with the configured pre-style (italic then color). If no
    /// pre-style is set, returns the input unchanged.
    fn apply_pre_style(&self, s: &str) -> String {
        match &self.pre_style {
            Some(pre) => pre.apply(s),
            None => s.to_string(),
        }
    }

    /// Render a block to lines.
    fn render_block(&self, block: &Block, content_width: usize) -> Vec<String> {
        match block {
            Block::Heading(level, inlines) => {
                let text = self.render_inlines(inlines);
                let styled = if *level == 1 {
                    (self.theme.heading)(&(self.theme.underline)(&text))
                } else if *level == 2 {
                    (self.theme.heading)(&text)
                } else {
                    let prefix = "#".repeat(*level as usize);
                    (self.theme.heading)(&format!("{} {}", prefix, text))
                };
                let styled = self.apply_pre_style(&styled);
                let mut lines = wrap_text_with_ansi(&styled, content_width);
                lines.push(String::new());
                lines
            }
            Block::Paragraph(inlines) => {
                let text = self.render_inlines(inlines);
                let text = self.apply_pre_style(&text);
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
            Block::UnorderedList(items) => self.render_list(items, false, 0, content_width),
            Block::OrderedList(items) => self.render_list(items, true, 0, content_width),
            Block::Blockquote(quote_lines) => {
                let border = (self.theme.quote_border)("│ ");
                let border_width = visible_width(&border);
                let inner_width = content_width.saturating_sub(border_width);

                let mut lines = Vec::new();
                for inlines in quote_lines {
                    let rendered = self.render_inlines(inlines);
                    let styled = (self.theme.quote)(&rendered);
                    // Wrap long quoted lines; every wrapped segment
                    // (including continuations) gets its own border.
                    let wrapped = if visible_width(&styled) == 0 {
                        // Preserve an explicit blank quoted line.
                        vec![String::new()]
                    } else {
                        wrap_text_with_ansi(&styled, inner_width)
                    };
                    for wl in wrapped {
                        lines.push(format!("{}{}", border, wl));
                    }
                }
                // Remove trailing border-only lines that slipped in
                // from empty inline runs at the end.
                while lines
                    .last()
                    .map(|l| visible_width(l) <= border_width)
                    .unwrap_or(false)
                {
                    lines.pop();
                }
                lines.push(String::new());
                lines
            }
            Block::HorizontalRule => {
                let rule = "─".repeat(content_width.min(80));
                vec![(self.theme.hr)(&rule), String::new()]
            }
            Block::Table {
                headers,
                alignments,
                rows,
            } => self.render_table(headers, alignments, rows, content_width),
        }
    }

    /// Render a table to styled lines. Picks column widths that
    /// accommodate the longest unbreakable token in each column, scales
    /// to fit `content_width` when the natural width exceeds it, and
    /// wraps cell content via `wrap_text_with_ansi`.
    fn render_table(
        &self,
        headers: &[Vec<Inline>],
        alignments: &[Alignment],
        rows: &[Vec<Vec<Inline>>],
        content_width: usize,
    ) -> Vec<String> {
        let n_cols = alignments.len();
        if n_cols == 0 {
            return vec![String::new()];
        }

        // Pre-render every cell once so visible-width calculations
        // (and the subsequent wrapping) share the same text.
        let header_text: Vec<String> = headers.iter().map(|c| self.render_inlines(c)).collect();
        let row_text: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().map(|c| self.render_inlines(c)).collect())
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

        // Each column claims 3 chars of chrome (border + two spaces of
        // padding); one more for the final right border.
        let chrome = 3 * n_cols + 1;
        let available = content_width.saturating_sub(chrome);
        let widths = distribute_column_widths(&natural, &minimum, available);

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
            let text = self.render_inlines(&item.content);
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
            for sub in &item.sub_blocks {
                let sub_lines = match sub {
                    Block::UnorderedList(sub_items) => {
                        self.render_list(sub_items, false, depth + 1, content_width)
                    }
                    Block::OrderedList(sub_items) => {
                        self.render_list(sub_items, true, depth + 1, content_width)
                    }
                    other => self.render_block(other, content_width),
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

    /// Render inline elements to a styled string.
    fn render_inlines(&self, inlines: &[Inline]) -> String {
        let mut result = String::new();
        for inline in inlines {
            match inline {
                Inline::Text(t) => result.push_str(t),
                Inline::Bold(inner) => {
                    let text = self.render_inlines(inner);
                    result.push_str(&(self.theme.bold)(&text));
                }
                Inline::Italic(inner) => {
                    let text = self.render_inlines(inner);
                    result.push_str(&(self.theme.italic)(&text));
                }
                Inline::Strikethrough(inner) => {
                    let text = self.render_inlines(inner);
                    result.push_str(&(self.theme.strikethrough)(&text));
                }
                Inline::Code(code) => {
                    result.push_str(&(self.theme.code)(code));
                }
                Inline::Link(text, url) => {
                    let styled_text = (self.theme.link)(&(self.theme.underline)(text));
                    if self.theme.hyperlinks {
                        // OSC 8: open (`ESC ] 8 ; ; <url> ESC \`),
                        // styled visible text, close (`ESC ] 8 ; ; ESC \`).
                        result.push_str(&format!("\x1b]8;;{}\x1b\\", url));
                        result.push_str(&styled_text);
                        result.push_str("\x1b]8;;\x1b\\");
                    } else if text == url || url.strip_prefix("mailto:") == Some(text) {
                        result.push_str(&styled_text);
                    } else {
                        result.push_str(&styled_text);
                        result.push_str(&(self.theme.link_url)(&format!(" ({})", url)));
                    }
                }
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
        if self.text.is_empty() {
            return Vec::new();
        }

        // Check cache.
        if let (Some(ct), Some(cw), Some(cl)) =
            (&self.cached_text, self.cached_width, &self.cached_lines)
        {
            if ct == &self.text && cw == width {
                return cl.clone();
            }
        }

        let content_width = width.saturating_sub(self.padding_x * 2);
        if content_width == 0 {
            return Vec::new();
        }

        let padding = " ".repeat(self.padding_x);
        let blocks = parse_markdown(&self.text);

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

        // Remove trailing blank lines from content.
        while result.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
            result.pop();
        }

        // Bottom padding.
        for _ in 0..self.padding_y {
            result.push(String::new());
        }

        result
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

    #[test]
    fn test_markdown_heading() {
        let mut md = Markdown::new("# Hello World");
        let lines = md.render(80);
        assert!(!lines.is_empty());
        // Should contain styled text.
        assert!(lines.iter().any(|l| l.contains("Hello World")));
    }

    #[test]
    fn test_markdown_paragraph() {
        let mut md = Markdown::new("This is a paragraph.");
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("This is a paragraph.")));
    }

    #[test]
    fn test_markdown_code_block() {
        let mut md = Markdown::new("```rust\nfn main() {}\n```");
        let lines = md.render(80);
        // Code block should contain the code content somewhere.
        let all = lines.join("\n");
        assert!(all.contains("fn") && all.contains("main"));
    }

    #[test]
    fn test_markdown_list() {
        let mut md = Markdown::new("- item 1\n- item 2\n- item 3");
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("item 1")));
        assert!(lines.iter().any(|l| l.contains("item 3")));
    }

    #[test]
    fn test_markdown_bold_italic() {
        let mut md = Markdown::new("**bold** and *italic*");
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("bold")));
        assert!(lines.iter().any(|l| l.contains("italic")));
    }

    #[test]
    fn test_markdown_link() {
        let mut md = Markdown::new("[Example](https://example.com)");
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("Example")));
        assert!(lines.iter().any(|l| l.contains("https://example.com")));
    }

    #[test]
    fn test_markdown_blockquote() {
        let mut md = Markdown::new("> This is a quote");
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("This is a quote")));
    }

    #[test]
    fn test_markdown_inline_code() {
        let mut md = Markdown::new("Use `code` here");
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("code")));
    }

    #[test]
    fn test_markdown_empty() {
        let mut md = Markdown::new("");
        assert!(md.render(80).is_empty());
    }

    #[test]
    fn test_markdown_hr() {
        let mut md = Markdown::new("---");
        let lines = md.render(80);
        assert!(lines.iter().any(|l| l.contains("─")));
    }

    #[test]
    fn test_inline_parsing() {
        let inlines = parse_inline("hello **bold** world");
        assert_eq!(inlines.len(), 3);
        matches!(&inlines[1], Inline::Bold(_));
    }
}
