//! Cross-implementation markdown rendering parity harness.
//!
//! Two markdown renderers ship in this workspace and must lay text out
//! identically:
//!
//! - `aj_tui::components::markdown::Markdown` (the reference), an ANSI
//!   renderer used by the `aj` frontend.
//! - `aj_app::markdown::render_markdown` (under test), a backend-agnostic
//!   renderer that emits role-tagged styled spans, consumed by aj-next.
//!
//! This harness renders one broad corpus through both and proves parity
//! mechanically. Level 1 (comprehensive) asserts the *visible text* of
//! every wrapped row matches row-for-row across many widths, so any
//! wrap, spacing, indent, or table-geometry divergence fails the test.
//! Level 2 (focused) asserts the two agree on which runs are tagged with
//! which *role* (heading / emphasis / code / link / list marker), the one
//! thing visible-text parity can't see.
//!
//! Alignment. The comparison is apples-to-apples only after reconciling
//! the two renderers' incidental differences:
//!
//! - Padding. `Markdown` is built with `padding_x = padding_y = 0`, so its
//!   content width equals the render width and it emits no padding rows,
//!   matching `render_markdown`'s width parameter exactly.
//! - Code-block indent. The identity theme leaves `code_block_indent` at
//!   `None`, which `Markdown` folds in as two spaces, the same
//!   `CODE_BLOCK_INDENT` the shared renderer prepends.
//! - Syntax highlighting. Both renderers run the same syntect classifier.
//!   The identity theme's `SyntaxStyles` closures pass code text through
//!   verbatim, so `Markdown`'s highlighted lines carry the same visible
//!   characters the shared renderer's code spans concatenate to. The one
//!   artifact is the trailing `\x1b[0m` reset `Markdown` appends per
//!   highlighted line, stripped below with every other escape.
//! - Trailing whitespace. `Markdown` pads every row out to the full width
//!   with spaces. The shared renderer trims trailing whitespace off
//!   wrapped rows. Both sides are compared after `trim_end`, a normalization
//!   that only drops invisible trailing spaces and so never hides a
//!   content or internal-spacing divergence.
//! - Hyperlinks. `Markdown::render_link` gates OSC-8 on the process-wide
//!   terminal capabilities. We force `hyperlinks = false` via
//!   [`set_capabilities`] and set `RenderOpts.hyperlinks = false` to match,
//!   so both render a non-autolink link as `text (url)` and an autolink as
//!   the bare URL. This keeps the visible text deterministic and identical
//!   regardless of the host terminal the test runs on.

use std::sync::Arc;

use aj_app::markdown::{
    Emphasis, MarkdownRow, RenderOpts, SpanKind, SyntaxCategory, render_markdown,
};
use aj_tui::capabilities::{TerminalCapabilities, set_capabilities};
use aj_tui::component::Component;
use aj_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme, SyntaxStyles};
use aj_tui_testkit::plain_lines_trim_end;
use aj_tui_testkit::themes::identity_markdown_theme;

// ---------------------------------------------------------------------------
// Widths and corpus
// ---------------------------------------------------------------------------

/// Widths every corpus document is rendered at. Spans degenerate widths
/// (1, 2, 3) that exercise the per-grapheme break paths, the narrow-table
/// prose fallback, and the blockquote inner-width clamp, up through wide
/// widths that exceed the 80-column horizontal-rule cap.
const WIDTHS: &[usize] = &[1, 2, 3, 5, 10, 20, 40, 80, 100];

/// `(name, markdown)` documents fed through both renderers. The name is
/// only used to label a mismatch in the failure report.
const CORPUS: &[(&str, &str)] = &[
    // --- Headings ---------------------------------------------------------
    (
        "headings_h1_to_h6",
        "# Heading one\n## Heading two\n### Heading three\n#### Heading four\n##### Heading five\n###### Heading six",
    ),
    (
        "heading_with_inline",
        "## A heading with **bold** and `code` and a [link](https://example.com)",
    ),
    // --- Paragraphs and inline styling -----------------------------------
    (
        "paragraph_plain",
        "This is a simple paragraph of prose that should wrap across several lines when the render width is small enough to force a break.",
    ),
    (
        "inline_styles",
        "Text with **bold**, *italic*, ~~strike~~, `code`, and a [link](https://example.com/page) inline.",
    ),
    (
        "inline_nested_emphasis",
        "***bold italic*** and **bold with _italic_ inside** and a `code span` too.",
    ),
    (
        "autolink_and_email",
        "Visit https://example.com now, or email me@example.org for the details.",
    ),
    (
        "hard_and_soft_breaks",
        "line one with a hard break  \nline two after the hard break\nline three after a soft break",
    ),
    // --- Code blocks ------------------------------------------------------
    (
        "code_no_lang",
        "```\nplain code line one\nplain code line two\n```",
    ),
    (
        "code_rust",
        "```rust\nfn main() {\n    let x = 42;\n    println!(\"{}\", x);\n}\n```",
    ),
    (
        "code_unknown_lang",
        "```zonk\nsome := code(with, tokens)\nother line\n```",
    ),
    (
        "code_long_line",
        "```\nthis_is_a_really_long_single_code_token_that_must_wrap_at_smaller_widths_for_sure_indeed\n```",
    ),
    (
        "code_internal_blank_line",
        "```\nfirst line\n\nthird line\n```",
    ),
    // --- Lists ------------------------------------------------------------
    ("list_unordered", "- one\n- two\n- three"),
    ("list_ordered", "1. first\n2. second\n3. third"),
    ("list_ordered_custom_start", "5. five\n6. six\n7. seven"),
    (
        "list_nested_depths",
        "- a\n  - a1\n    - a1a\n    - a1b\n  - a2\n- b",
    ),
    (
        "list_ordered_split_by_code_fence",
        "1. one\n2. two\n\n```\nintervening code\n```\n\n3. three\n4. four",
    ),
    (
        "list_long_wrapping_item",
        "- This is a list item with a lot of content that will need to wrap onto multiple continuation lines at narrow widths to verify flush-left continuation.",
    ),
    (
        "list_item_with_sub_blocks",
        "- item with sub-blocks\n\n  a nested paragraph inside the item\n\n  - nested bullet one\n  - nested bullet two\n\n  ```\n  code in item\n  ```",
    ),
    // --- Blockquotes ------------------------------------------------------
    ("quote_single_line", "> a single quoted line"),
    (
        "quote_multi_line",
        "> quoted line one\n> quoted line two\n> quoted line three",
    ),
    (
        "quote_nested_blocks",
        "> ## quoted heading\n>\n> quoted paragraph with **bold** text\n>\n> - quoted item one\n> - quoted item two\n>\n> ```\n> code in quote\n> ```",
    ),
    (
        "quote_with_table",
        "> | A | B |\n> | --- | --- |\n> | 1 | 2 |",
    ),
    (
        "quote_nested_quotes",
        "> outer quote text\n>\n> > inner quote text\n> >\n> > deeper inner line",
    ),
    // --- Tables -----------------------------------------------------------
    (
        "table_two_col",
        "| Name | Value |\n| --- | --- |\n| alpha | 1 |\n| beta | 2 |",
    ),
    (
        "table_three_col_alignments",
        "| Left | Center | Right |\n| :--- | :---: | ---: |\n| a | b | c |\n| dd | ee | ff |",
    ),
    (
        "table_wrapping_cell",
        "| Item | Description |\n| --- | --- |\n| one | this description cell has enough words to wrap within its column at moderate widths |\n| two | short |",
    ),
    (
        "table_long_token",
        "| Key | Value |\n| --- | --- |\n| hash | abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ |\n| ok | fine |",
    ),
    (
        "table_inline_styled_cells",
        "| Name | Note |\n| --- | --- |\n| **bold** name | uses `code` and a [link](https://example.com) |\n| plain | short |",
    ),
    // --- Horizontal rules -------------------------------------------------
    (
        "hr_between_paragraphs",
        "above the rule\n\n---\n\nbelow the rule",
    ),
    // --- Mixed / inter-block spacing --------------------------------------
    (
        "mixed_document",
        "# Title\n\nIntro paragraph with a [link](https://example.com) and `code`.\n\n## Section\n\n- bullet one\n- bullet two\n\n```rust\nlet y = 1;\n```\n\n> a closing quote\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n\n---\n\nFinal paragraph.",
    ),
    // --- Edge cases -------------------------------------------------------
    ("empty", ""),
    ("whitespace_only", "   \n \n\t\n   "),
    (
        "html_like_tags",
        "Some <thinking> tag and <div>block content</div> shown literally.",
    ),
    (
        "ends_in_heading",
        "intro paragraph\n\n## a trailing heading",
    ),
    ("ends_in_paragraph", "## a heading\n\na trailing paragraph"),
    ("ends_in_code", "intro\n\n```\ntrailing code\n```"),
    (
        "ends_in_list",
        "intro\n\n- trailing item one\n- trailing item two",
    ),
    ("ends_in_quote", "intro\n\n> a trailing quote"),
    (
        "ends_in_table",
        "intro\n\n| A | B |\n| --- | --- |\n| 1 | 2 |",
    ),
    ("ends_in_hr", "intro\n\n---"),
];

// ---------------------------------------------------------------------------
// Deterministic capability override
// ---------------------------------------------------------------------------

/// Force OSC-8 hyperlinks off in the process-wide capability cache so
/// `Markdown::render_link` takes its no-escape path (visible `text (url)`)
/// regardless of the terminal the test runs under. Paired with
/// `RenderOpts.hyperlinks = false` on the shared side.
fn force_no_hyperlinks() {
    set_capabilities(TerminalCapabilities {
        hyperlinks: false,
        true_color: false,
        images: None,
    });
}

// ---------------------------------------------------------------------------
// Level 1: visible-text parity
// ---------------------------------------------------------------------------

/// The reference renderer's rows as plain visible text. Identity theme, no
/// padding, ANSI stripped, trailing whitespace trimmed.
fn aj_visible_rows(text: &str, width: usize) -> Vec<String> {
    let mut md = Markdown::new(text, 0, 0, identity_markdown_theme(), None);
    plain_lines_trim_end(&md.render(width))
}

/// A shared-renderer row's visible text: the concatenation of its spans'
/// text, trailing whitespace trimmed to match `aj_visible_rows`.
fn shared_row_text(row: &MarkdownRow) -> String {
    let s: String = row.iter().map(|span| span.text.as_str()).collect();
    s.trim_end().to_string()
}

/// The shared renderer's rows as plain visible text.
fn shared_visible_rows(text: &str, width: usize) -> Vec<String> {
    let opts = RenderOpts {
        hyperlinks: false,
        default_emphasis: Emphasis::default(),
        syntax_highlight: true,
    };
    render_markdown(text, width, &opts)
        .iter()
        .map(shared_row_text)
        .collect()
}

/// Render a row-by-row diff of two row lists for a failure message.
fn diff_rows(reference: &[String], under_test: &[String]) -> String {
    let mut out = String::new();
    let n = reference.len().max(under_test.len());
    for i in 0..n {
        let r = reference.get(i).map(String::as_str);
        let u = under_test.get(i).map(String::as_str);
        let marker = if r == u { " " } else { "!" };
        out.push_str(&format!("  {marker} [{i}] aj={r:?} shared={u:?}\n"));
    }
    out
}

/// The non-blank rows of `rows`, in order. A blank row is one whose visible
/// text is empty or whitespace-only.
fn non_blank_rows(rows: &[String]) -> Vec<&str> {
    rows.iter()
        .map(String::as_str)
        .filter(|r| !r.trim().is_empty())
        .collect()
}

/// Outcome of comparing a reference row list against the one under test.
#[derive(PartialEq, Eq)]
enum RowMatch {
    /// Row-for-row identical.
    Exact,
    /// Identical once blank rows are ignored, but the blank-row counts
    /// differ. See the width-1 handling in [`level1_visible_text_parity`].
    ModuloBlankRows,
    /// A real content divergence.
    Different,
}

fn compare_rows(reference: &[String], under_test: &[String]) -> RowMatch {
    if reference == under_test {
        RowMatch::Exact
    } else if non_blank_rows(reference) == non_blank_rows(under_test) {
        RowMatch::ModuloBlankRows
    } else {
        RowMatch::Different
    }
}

#[test]
fn level1_visible_text_parity() {
    force_no_hyperlinks();

    let mut failures = String::new();
    for (name, markdown) in CORPUS {
        for &width in WIDTHS {
            let reference = aj_visible_rows(markdown, width);
            let under_test = shared_visible_rows(markdown, width);
            match compare_rows(&reference, &under_test) {
                RowMatch::Exact => {}
                // Documented degenerate-width deviation, accepted only at
                // width 1. A whitespace-only code-block line renders to two
                // blank rows in the reference and one in the shared
                // renderer at width 1. The reference's code lines carry a
                // trailing `\x1b[0m` syntect reset, and at width 1 its wrap
                // reads the "two-space indent + reset" run as a non-blank
                // token (the ESC is not whitespace) and breaks it across two
                // rows, so an empty code line survives as two blank rows
                // rather than collapsing to one. The shared renderer emits
                // the indent as a single whitespace span that trims to one
                // blank row, which is the more correct result. The visible
                // content is identical (all blank), so only the blank-row
                // count differs, and only at this degenerate width. Any
                // structural spacing bug would also show at width >= 2,
                // where the comparison stays fully strict, and non-blank
                // content stays strict here too.
                RowMatch::ModuloBlankRows if width == 1 => {}
                _ => {
                    failures.push_str(&format!(
                        "\n=== {name} @ width {width} ===\n{}",
                        diff_rows(&reference, &under_test)
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "visible-text parity divergences:\n{failures}"
    );
}

// ---------------------------------------------------------------------------
// Level 2: role / emphasis parity (focused)
// ---------------------------------------------------------------------------
//
// Level 1 proves the two renderers place the same characters on the same
// rows. It cannot see which runs each renderer tags as a heading, as
// emphasis, as inline code, as a link, or as a list marker, because the
// identity theme erases every role to plain text. Level 2 fills that gap on
// a focused corpus.
//
// The reference is driven with a SENTINEL theme whose element closures wrap
// their input in unique markers (`heading` -> `⟦H⟧..⟦/H⟧`, `bold` ->
// `⟦B⟧..⟦/B⟧`, and so on). Its rendered rows are then parsed back into a
// sequence of (role, emphasis, text) runs. The shared renderer's spans map
// onto the same run vocabulary directly from each span's `kind` and
// `emphasis`. We compare the run sequences row-for-row.
//
// Scope. The generic role comparison covers headings, bold/italic/strike,
// inline code, links (text + trailing url), and list markers. Two known
// intentional deviations are handled as explicit, commented exceptions
// below rather than papered over: inline code emphasis composition inside a
// heading, and blockquote recoloring. Code syntax-category tagging gets its
// own focused check. Table styling roles are verified only at the
// visible-text level (Level 1).

/// Semantic role of a run, the shared vocabulary both renderers map onto.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Text,
    Heading,
    InlineCode,
    CodeBlock,
    CodeBlockBorder,
    ListMarker,
    QuoteBorder,
    Quote,
    LinkText,
    LinkUrl,
    Hr,
    TableBorder,
    TableCell,
}

/// Text-decoration bits, mirroring both renderers' emphasis model.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
struct Emph {
    bold: bool,
    italic: bool,
    strike: bool,
    underline: bool,
}

/// One styled run: a role, its emphasis, and the visible text.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Run {
    role: Role,
    emph: Emph,
    text: String,
}

/// A sentinel-marked theme: every element closure wraps its text in a
/// unique `⟦TAG⟧..⟦/TAG⟧` marker so the reference's rendered rows can be
/// parsed back into (role, emphasis) runs. Syntax categories use a shared
/// close tag (`⟦SYN:kw⟧..⟦/SYN⟧`). Alignment knobs match the identity theme
/// used in Level 1 (`code_block_indent = None` folds in two spaces,
/// `syntax_highlight = true` exercises the syntect classifier).
fn sentinel_theme() -> MarkdownTheme {
    fn wrap(tag: &'static str) -> Arc<dyn Fn(&str) -> String> {
        Arc::new(move |s| format!("\u{27E6}{tag}\u{27E7}{s}\u{27E6}/{tag}\u{27E7}"))
    }
    fn syn(cat: &'static str) -> Arc<dyn Fn(&str) -> String> {
        Arc::new(move |s| format!("\u{27E6}SYN:{cat}\u{27E7}{s}\u{27E6}/SYN\u{27E7}"))
    }
    MarkdownTheme {
        heading: wrap("H"),
        bold: wrap("B"),
        italic: wrap("I"),
        strikethrough: wrap("S"),
        code: wrap("C"),
        code_block: wrap("CBK"),
        code_block_border: wrap("CBB"),
        link: wrap("L"),
        link_url: wrap("LU"),
        list_bullet: wrap("M"),
        quote_border: wrap("QB"),
        quote: wrap("Q"),
        hr: wrap("HR"),
        underline: wrap("U"),
        highlight_code: None,
        code_block_indent: None,
        syntax_highlight: true,
        syntax: SyntaxStyles {
            comment: syn("comment"),
            keyword: syn("keyword"),
            function: syn("function"),
            variable: syn("variable"),
            string: syn("string"),
            number: syn("number"),
            type_name: syn("type"),
            operator: syn("operator"),
            punctuation: syn("punctuation"),
        },
    }
}

/// Resolve a marker stack to a run's role and emphasis. The innermost role
/// marker wins. Emphasis markers accumulate. `SYN:*` markers carry a code
/// role plus the category (returned separately by the caller when needed).
fn role_emph_from_stack(stack: &[String]) -> (Role, Emph) {
    let mut role = Role::Text;
    let mut emph = Emph::default();
    for tag in stack {
        match tag.as_str() {
            "H" => role = Role::Heading,
            "C" => role = Role::InlineCode,
            "CBK" => role = Role::CodeBlock,
            "CBB" => role = Role::CodeBlockBorder,
            "L" => role = Role::LinkText,
            "LU" => role = Role::LinkUrl,
            "M" => role = Role::ListMarker,
            "Q" => role = Role::Quote,
            "QB" => role = Role::QuoteBorder,
            "HR" => role = Role::Hr,
            "B" => emph.bold = true,
            "I" => emph.italic = true,
            "S" => emph.strike = true,
            "U" => emph.underline = true,
            t if t.starts_with("SYN:") => role = Role::CodeBlock,
            _ => {}
        }
    }
    (role, emph)
}

/// Merge adjacent runs that share a role and emphasis, concatenating their
/// text. Normalizes away the span-splitting differences between the two
/// renderers (the reference emits one marker pair per inline, the shared
/// renderer coalesces same-style spans during wrap) so only genuine role
/// boundaries survive.
fn merge_runs(runs: Vec<Run>) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::with_capacity(runs.len());
    for run in runs {
        if let Some(last) = out.last_mut()
            && last.role == run.role
            && last.emph == run.emph
        {
            last.text.push_str(&run.text);
            continue;
        }
        out.push(run);
    }
    out
}

/// Parse a sentinel-marked reference row (ANSI already stripped, trailing
/// whitespace trimmed) into merged (role, emphasis, text) runs.
fn parse_sentinel_runs(row: &str) -> Vec<Run> {
    let mut stack: Vec<String> = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    let mut buf = String::new();

    let flush = |buf: &mut String, stack: &[String], runs: &mut Vec<Run>| {
        if buf.is_empty() {
            return;
        }
        let (role, emph) = role_emph_from_stack(stack);
        runs.push(Run {
            role,
            emph,
            text: std::mem::take(buf),
        });
    };

    let mut rest = row;
    while let Some(open) = rest.find('\u{27E6}') {
        buf.push_str(&rest[..open]);
        let after = &rest[open + '\u{27E6}'.len_utf8()..];
        let close = after
            .find('\u{27E7}')
            .expect("sentinel marker missing closing bracket");
        let tag = &after[..close];
        flush(&mut buf, &stack, &mut runs);
        if let Some(t) = tag.strip_prefix('/') {
            if let Some(pos) = stack.iter().rposition(|x| x == t) {
                stack.remove(pos);
            }
        } else {
            stack.push(tag.to_string());
        }
        rest = &after[close + '\u{27E7}'.len_utf8()..];
    }
    buf.push_str(rest);
    flush(&mut buf, &stack, &mut runs);
    merge_runs(runs)
}

/// Map a shared-renderer span's kind onto the run role vocabulary.
fn role_of_kind(kind: SpanKind) -> Role {
    match kind {
        SpanKind::Text => Role::Text,
        SpanKind::Heading(_) => Role::Heading,
        SpanKind::InlineCode => Role::InlineCode,
        SpanKind::CodeBlock => Role::CodeBlock,
        SpanKind::CodeBlockBorder => Role::CodeBlockBorder,
        SpanKind::ListMarker => Role::ListMarker,
        SpanKind::QuoteBorder => Role::QuoteBorder,
        SpanKind::Quote => Role::Quote,
        SpanKind::LinkText => Role::LinkText,
        SpanKind::LinkUrl => Role::LinkUrl,
        SpanKind::Hr => Role::Hr,
        SpanKind::TableBorder => Role::TableBorder,
        SpanKind::TableCell => Role::TableCell,
    }
}

/// Map a shared-renderer row's spans onto merged (role, emphasis, text)
/// runs, the same shape [`parse_sentinel_runs`] produces for the reference.
fn shared_runs(row: &MarkdownRow) -> Vec<Run> {
    let runs = row
        .iter()
        .map(|sp| Run {
            role: role_of_kind(sp.kind),
            emph: Emph {
                bold: sp.emphasis.bold,
                italic: sp.emphasis.italic,
                strike: sp.emphasis.strikethrough,
                underline: sp.emphasis.underline,
            },
            text: sp.text.clone(),
        })
        .collect();
    merge_runs(runs)
}

/// The reference's sentinel rows for `text` at `width`, one parsed run list
/// per row.
fn aj_run_rows(text: &str, width: usize) -> Vec<Vec<Run>> {
    let mut md = Markdown::new(text, 0, 0, sentinel_theme(), None);
    plain_lines_trim_end(&md.render(width))
        .iter()
        .map(|row| parse_sentinel_runs(row))
        .collect()
}

/// The shared renderer's rows for `text` at `width`, one run list per row.
fn shared_run_rows(text: &str, width: usize) -> Vec<Vec<Run>> {
    let opts = RenderOpts {
        hyperlinks: false,
        default_emphasis: Emphasis::default(),
        syntax_highlight: true,
    };
    render_markdown(text, width, &opts)
        .iter()
        .map(shared_runs)
        .collect()
}

/// Focused corpus for the generic role comparison. Every case is chosen so
/// the reference's sentinel output nests cleanly (no re-emitted style
/// prefixes): plain-text headings and paragraph/list inline styling, where
/// the reference's inline-style context carries an empty style prefix.
const LEVEL2_CORPUS: &[(&str, &str)] = &[
    ("h1", "# Alpha heading"),
    ("h2", "## Beta heading"),
    ("h3", "### Gamma heading"),
    ("h4", "#### Delta heading"),
    ("para_bold", "text with **bold** word"),
    ("para_italic", "text with *italic* word"),
    ("para_strike", "text with ~~strike~~ word"),
    ("para_code", "text with `code` word"),
    ("para_link", "see [the docs](https://example.com/docs) here"),
    ("para_autolink", "go https://example.com now"),
    ("para_email", "mail me@example.org please"),
    ("nested_emphasis", "**bold with _italic_ inside** tail"),
    ("list_unordered", "- one\n- two"),
    ("list_ordered", "1. first\n2. second"),
];

#[test]
fn level2_role_parity() {
    force_no_hyperlinks();

    // Rendered at a wide width so the sentinel markers, which count as
    // visible width to the reference's wrap, never push a short case onto a
    // second row. Wrapping parity is Level 1's job. Here every corpus line
    // stays a single row on both sides.
    let width = 200;
    let mut failures = String::new();
    for (name, markdown) in LEVEL2_CORPUS {
        let reference = aj_run_rows(markdown, width);
        let under_test = shared_run_rows(markdown, width);
        if reference != under_test {
            failures.push_str(&format!(
                "\n=== {name} ===\n  aj     = {reference:?}\n  shared = {under_test:?}\n"
            ));
        }
    }

    assert!(failures.is_empty(), "role parity divergences:\n{failures}");
}

/// First run with the given role across all rows, if any.
fn first_run_with_role(rows: &[Vec<Run>], role: Role) -> Option<Run> {
    rows.iter().flatten().find(|r| r.role == role).cloned()
}

#[test]
fn level2_inline_code_emphasis_composition_deviation() {
    force_no_hyperlinks();

    // Documented deviation (see `render_inlines` in the shared renderer):
    // the shared span model composes bold / italic / underline onto
    // InlineCode and Link spans uniformly, because every span carries its
    // full style explicitly. The reference ANSI renderer applies the outer
    // heading and default-emphasis styling only to text runs, re-emitting
    // the style prefix after each non-text inline, so its inline code stays
    // free of the outer emphasis. We assert the known difference rather than
    // hide it, so a regression that silently aligned (or further diverged)
    // the two would be caught.

    // Inline code inside a heading. The shared renderer composes the
    // heading's bold onto the code span. The reference does not.
    let heading = "## Heading `code` tail";
    let aj_code = first_run_with_role(&aj_run_rows(heading, 200), Role::InlineCode)
        .expect("reference heading contains an inline-code run");
    let shared_code = first_run_with_role(&shared_run_rows(heading, 200), Role::InlineCode)
        .expect("shared heading contains an inline-code run");
    assert!(
        !aj_code.emph.bold,
        "reference leaves heading inline code un-bolded, got {aj_code:?}"
    );
    assert!(
        shared_code.emph.bold,
        "shared composes heading bold onto inline code, got {shared_code:?}"
    );

    // Inline code inside a default-emphasis (italic) paragraph. Same shape:
    // the shared renderer composes the italic onto the code span, and the
    // reference does not.
    let para = "prose with `code` inside";
    let mut md = Markdown::new(
        para,
        0,
        0,
        sentinel_theme(),
        Some(DefaultTextStyle {
            italic: true,
            ..Default::default()
        }),
    );
    let aj_rows: Vec<Vec<Run>> = plain_lines_trim_end(&md.render(200))
        .iter()
        .map(|r| parse_sentinel_runs(r))
        .collect();
    let aj_para_code = first_run_with_role(&aj_rows, Role::InlineCode)
        .expect("reference paragraph contains an inline-code run");
    let opts = RenderOpts {
        hyperlinks: false,
        default_emphasis: Emphasis {
            italic: true,
            ..Default::default()
        },
        syntax_highlight: true,
    };
    let shared_rows: Vec<Vec<Run>> = render_markdown(para, 200, &opts)
        .iter()
        .map(shared_runs)
        .collect();
    let shared_para_code = first_run_with_role(&shared_rows, Role::InlineCode)
        .expect("shared paragraph contains an inline-code run");
    assert!(
        !aj_para_code.emph.italic,
        "reference leaves default-emphasis inline code upright, got {aj_para_code:?}"
    );
    assert!(
        shared_para_code.emph.italic,
        "shared composes default emphasis onto inline code, got {shared_para_code:?}"
    );
}

#[test]
fn level2_blockquote_role_deviation() {
    force_no_hyperlinks();

    // Documented deviation (see `render_blockquote` in the shared renderer):
    // the shared model paints the QuoteBorder and any non-text spans (inline
    // code, table borders/cells) in base style and retags only Text spans to
    // Quote + italic. The reference wraps the whole quoted line in the quote
    // (italic) style, so its inline code picks up the quote's italic. Quote
    // role parity is therefore only proven at the visible-text level
    // (Level 1). Here we assert the known styling difference and confirm the
    // shared model's border/text roles.
    let quote = "> quoted `code` text";

    let aj_code = first_run_with_role(&aj_run_rows(quote, 200), Role::InlineCode)
        .expect("reference quote contains an inline-code run");
    let shared_code = first_run_with_role(&shared_run_rows(quote, 200), Role::InlineCode)
        .expect("shared quote contains an inline-code run");
    assert!(
        aj_code.emph.italic,
        "reference recolors the whole quoted line, so italic reaches code, got {aj_code:?}"
    );
    assert!(
        !shared_code.emph.italic,
        "shared leaves inline code in base style inside a quote, got {shared_code:?}"
    );

    // The shared model still borders the row (QuoteBorder) and italicizes the
    // quoted prose (Quote + italic), which is the parity we do keep.
    let flat: Vec<Run> = shared_run_rows(quote, 200).into_iter().flatten().collect();
    assert!(
        flat.iter().any(|r| r.role == Role::QuoteBorder),
        "shared quote has a border span, got {flat:?}"
    );
    assert!(
        flat.iter().any(|r| r.role == Role::Quote && r.emph.italic),
        "shared quote text is Quote + italic, got {flat:?}"
    );
}

// ---------------------------------------------------------------------------
// Level 2: syntax-category parity
// ---------------------------------------------------------------------------
//
// Both renderers run the same syntect classifier over code blocks. This
// proves they assign the same category to the same tokens by comparing the
// per-body-line (category, text) run sequences. The reference's sentinel
// syntax closures tag each classified token with `⟦SYN:<cat>⟧`. The shared
// renderer carries the category on each code span's `syntax` field.

/// Map a `SYN:<cat>` marker payload onto the shared renderer's category.
fn syntax_category(name: &str) -> SyntaxCategory {
    match name {
        "comment" => SyntaxCategory::Comment,
        "keyword" => SyntaxCategory::Keyword,
        "function" => SyntaxCategory::Function,
        "variable" => SyntaxCategory::Variable,
        "string" => SyntaxCategory::String,
        "number" => SyntaxCategory::Number,
        "type" => SyntaxCategory::Type,
        "operator" => SyntaxCategory::Operator,
        "punctuation" => SyntaxCategory::Punctuation,
        other => panic!("unknown syntax category marker: {other}"),
    }
}

/// A shared-renderer code row as merged (category, text) runs.
fn shared_syntax_runs(row: &MarkdownRow) -> Vec<(Option<SyntaxCategory>, String)> {
    let mut out: Vec<(Option<SyntaxCategory>, String)> = Vec::new();
    for sp in row {
        if let Some(last) = out.last_mut()
            && last.0 == sp.syntax
        {
            last.1.push_str(&sp.text);
        } else {
            out.push((sp.syntax, sp.text.clone()));
        }
    }
    out
}

/// A reference code row (ANSI stripped, trimmed) as merged (category, text)
/// runs. Body rows carry only `SYN:*` markers around classified tokens and
/// bare text for the indent and unclassified tokens. A `CBK`/other marker
/// (tokenizer-error fallback) leaves the category `None`, matching the
/// shared renderer's error fallback.
fn parse_syntax_runs(row: &str) -> Vec<(Option<SyntaxCategory>, String)> {
    let mut current: Option<SyntaxCategory> = None;
    let mut out: Vec<(Option<SyntaxCategory>, String)> = Vec::new();
    let mut buf = String::new();

    let flush = |cat, buf: &mut String, out: &mut Vec<(Option<SyntaxCategory>, String)>| {
        if buf.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut()
            && last.0 == cat
        {
            last.1.push_str(buf);
            buf.clear();
        } else {
            out.push((cat, std::mem::take(buf)));
        }
    };

    let mut rest = row;
    while let Some(open) = rest.find('\u{27E6}') {
        buf.push_str(&rest[..open]);
        let after = &rest[open + '\u{27E6}'.len_utf8()..];
        let close = after
            .find('\u{27E7}')
            .expect("sentinel marker missing closing bracket");
        let tag = &after[..close];
        flush(current, &mut buf, &mut out);
        if let Some(cat) = tag.strip_prefix("SYN:") {
            current = Some(syntax_category(cat));
        } else if tag == "/SYN" {
            current = None;
        }
        // Any other marker (e.g. the CBK error fallback) leaves `current`
        // at `None`, matching the shared renderer's uncategorized fallback.
        rest = &after[close + '\u{27E7}'.len_utf8()..];
    }
    buf.push_str(rest);
    flush(current, &mut buf, &mut out);
    out
}

#[test]
fn level2_code_syntax_category_parity() {
    force_no_hyperlinks();

    let doc = "```rust\nfn add(a: i32) -> i32 { a + 1 }\nlet s = \"hi\";\n// a note\n```";
    // Wide enough that the sentinel `⟦SYN:..⟧` markers, which count as
    // visible width to the reference's wrap, never split a code line. That
    // keeps the reference's row count equal to the shared renderer's for the
    // index-aligned comparison below.
    let width = 1000;

    let shared_rows = render_markdown(
        doc,
        width,
        &RenderOpts {
            hyperlinks: false,
            default_emphasis: Emphasis::default(),
            syntax_highlight: true,
        },
    );
    let mut md = Markdown::new(doc, 0, 0, sentinel_theme(), None);
    let aj_rows = plain_lines_trim_end(&md.render(width));

    assert_eq!(
        aj_rows.len(),
        shared_rows.len(),
        "row counts must match for index-aligned comparison"
    );

    let mut compared = 0;
    for (aj_row, shared_row) in aj_rows.iter().zip(shared_rows.iter()) {
        // Code body rows are the ones whose spans are all CodeBlock. Fence
        // rows are CodeBlockBorder and the trailing spacer is empty.
        let is_body =
            !shared_row.is_empty() && shared_row.iter().all(|sp| sp.kind == SpanKind::CodeBlock);
        if !is_body {
            continue;
        }
        assert_eq!(
            parse_syntax_runs(aj_row),
            shared_syntax_runs(shared_row),
            "syntax-category run mismatch on code body row {aj_row:?}"
        );
        compared += 1;
    }
    assert!(
        compared >= 3,
        "expected to compare the three code body lines, compared {compared}"
    );
}

#[test]
fn level2_code_no_syntax_highlight_parity() {
    force_no_hyperlinks();

    // Same corpus as the highlighting-on case, but with highlighting off on
    // both sides. Every code body row should collapse to a single
    // uncategorized run, and the visible text (indent included) must agree.
    let doc = "```rust\nfn add(a: i32) -> i32 { a + 1 }\nlet s = \"hi\";\n// a note\n```";
    let width = 1000;

    let shared_rows = render_markdown(
        doc,
        width,
        &RenderOpts {
            hyperlinks: false,
            default_emphasis: Emphasis::default(),
            syntax_highlight: false,
        },
    );
    // The reference's code-block closure still wraps each line (in `CBK`
    // markers), which `parse_syntax_runs` treats as uncategorized, matching
    // the shared renderer's plain spans.
    let mut theme = sentinel_theme();
    theme.syntax_highlight = false;
    let mut md = Markdown::new(doc, 0, 0, theme, None);
    let aj_rows = plain_lines_trim_end(&md.render(width));

    assert_eq!(
        aj_rows.len(),
        shared_rows.len(),
        "row counts must match for index-aligned comparison"
    );

    let mut compared = 0;
    for (aj_row, shared_row) in aj_rows.iter().zip(shared_rows.iter()) {
        let is_body =
            !shared_row.is_empty() && shared_row.iter().all(|sp| sp.kind == SpanKind::CodeBlock);
        if !is_body {
            continue;
        }
        let shared = shared_syntax_runs(shared_row);
        assert!(
            shared.iter().all(|(cat, _)| cat.is_none()),
            "highlighting off leaves the shared body uncategorized: {shared:?}"
        );
        assert_eq!(
            parse_syntax_runs(aj_row),
            shared,
            "code body row mismatch with highlighting off {aj_row:?}"
        );
        compared += 1;
    }
    assert!(
        compared >= 3,
        "expected to compare the three code body lines, compared {compared}"
    );
}
