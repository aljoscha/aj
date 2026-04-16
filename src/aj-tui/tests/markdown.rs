//! Integration tests for the `Markdown` component.
//!
//! Most tests drive `Markdown::render(width)` directly and assert on the
//! returned lines (plain-text via [`strip_ansi`] when structure matters,
//! joined-raw when styling matters). A handful go end-to-end through a
//! `Tui` + `VirtualTerminal` to catch style-leakage regressions that
//! only show up after the compositor has run.
//!
//! Tests that depend on features we haven't implemented yet (tables,
//! pre-styled inheritance, OSC 8 hyperlinks) are `#[ignore]`d with a
//! reason string; they'll switch on as the features land.

mod support;

use std::sync::Arc;

use aj_tui::component::Component;
use aj_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme};
use aj_tui::style;
use aj_tui::tui::Tui;

use support::themes::default_markdown_theme;
use support::{VirtualTerminal, plain_lines, plain_lines_trim_end, render_now, strip_ansi};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `Markdown` with `padding_x = 0` (so the component's rendered
/// rows line up flush with column 0) and the framework's default
/// markdown theme.
fn md(text: &str) -> Markdown {
    md_with_padding(text, 0, 0)
}

/// Build a `Markdown` with explicit paddings.
fn md_with_padding(text: &str, padding_x: usize, padding_y: usize) -> Markdown {
    Markdown::new(text, padding_x, padding_y, default_markdown_theme(), None)
}

/// Build a `Markdown` with an outer italic + gray default text style,
/// mimicking how a "thinking trace" paragraph is rendered in practice.
fn md_with_gray_italic(text: &str) -> Markdown {
    Markdown::new(
        text,
        0,
        0,
        default_markdown_theme(),
        Some(DefaultTextStyle {
            color: Some(Arc::new(style::gray)),
            italic: true,
            ..Default::default()
        }),
    )
}

// ---------------------------------------------------------------------------
// Nested lists
// ---------------------------------------------------------------------------

#[test]
fn renders_a_simple_nested_list() {
    let mut m = md("- Item 1\n  - Nested 1.1\n  - Nested 1.2\n- Item 2");
    let lines = m.render(80);
    assert!(!lines.is_empty());

    let plain = plain_lines(&lines);
    assert!(plain.iter().any(|l| l.contains("- Item 1")));
    assert!(plain.iter().any(|l| l.contains("  - Nested 1.1")));
    assert!(plain.iter().any(|l| l.contains("  - Nested 1.2")));
    assert!(plain.iter().any(|l| l.contains("- Item 2")));
}

#[test]
fn renders_a_deeply_nested_list() {
    let mut m = md("- Level 1\n  - Level 2\n    - Level 3\n      - Level 4");
    let lines = m.render(80);
    let plain = plain_lines(&lines);

    assert!(plain.iter().any(|l| l.contains("- Level 1")));
    assert!(plain.iter().any(|l| l.contains("  - Level 2")));
    assert!(plain.iter().any(|l| l.contains("    - Level 3")));
    assert!(plain.iter().any(|l| l.contains("      - Level 4")));
}

#[test]
fn renders_an_ordered_nested_list() {
    let mut m = md("1. First\n   1. Nested first\n   2. Nested second\n2. Second");
    let lines = m.render(80);
    let plain = plain_lines(&lines);

    assert!(plain.iter().any(|l| l.contains("1. First")));
    assert!(plain.iter().any(|l| l.contains("  1. Nested first")));
    assert!(plain.iter().any(|l| l.contains("  2. Nested second")));
    assert!(plain.iter().any(|l| l.contains("2. Second")));
}

#[test]
fn renders_mixed_ordered_and_unordered_nested_lists() {
    let mut m = md(
        "1. Ordered item\n   - Unordered nested\n   - Another nested\n\
         2. Second ordered\n   - More nested",
    );
    let lines = m.render(80);
    let plain = plain_lines(&lines);

    assert!(plain.iter().any(|l| l.contains("1. Ordered item")));
    assert!(plain.iter().any(|l| l.contains("  - Unordered nested")));
    assert!(plain.iter().any(|l| l.contains("2. Second ordered")));
}

#[test]
fn preserves_original_numbering_when_code_blocks_split_an_ordered_list() {
    // When code blocks aren't indented under a list item, many markdown
    // parsers see each `N.` as its own list and reset numbering. We want
    // the original numbers preserved verbatim.
    let mut m = md("1. First item\n\n```typescript\n// code block\n```\n\n\
         2. Second item\n\n```typescript\n// another code block\n```\n\n\
         3. Third item");
    let lines = m.render(80);
    let plain: Vec<String> = plain_lines(&lines)
        .into_iter()
        .map(|l| l.trim().to_string())
        .collect();

    // Find every line that starts with a number and period.
    let numbered: Vec<&String> = plain
        .iter()
        .filter(|l| {
            let mut it = l.chars();
            let first = it.next();
            let starts_with_digit = matches!(first, Some(c) if c.is_ascii_digit());
            starts_with_digit && l.contains('.')
        })
        .collect();

    assert_eq!(
        numbered.len(),
        3,
        "expected 3 numbered items, got: {:?}",
        numbered,
    );
    assert!(numbered[0].starts_with("1."), "got: {}", numbered[0]);
    assert!(numbered[1].starts_with("2."), "got: {}", numbered[1]);
    assert!(numbered[2].starts_with("3."), "got: {}", numbered[2]);
}

// ---------------------------------------------------------------------------
// Tab normalization (F27)
//
// pi-tui replaces every `\t` with three spaces before lexing. The
// normalization is observable in two places: tab-indented code-block
// bodies (the literal `\t` would otherwise survive through the parser
// and into the rendered cell) and tab-indented list continuation /
// nested markers (since `indent_of` counts only space bytes, a tab
// indent doesn't open a nested list).
// ---------------------------------------------------------------------------

#[test]
fn tab_in_fenced_code_block_body_is_normalized_to_three_spaces() {
    // A code block with a tab-indented body line. The expectation: the
    // tab byte does not survive into the rendered output; it's replaced
    // by three spaces before parsing, so the visible indent is " " * 3.
    let mut m = md("```\n\thello\n```");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    // No raw tab byte anywhere in the rendered output.
    for line in &plain {
        assert!(
            !line.contains('\t'),
            "tab byte survived into rendered output: {:?}",
            line,
        );
    }

    // The code-block body row contains "   hello" (three-space indent
    // from the normalized tab). Code-block lines also carry the
    // theme's `code_block_indent` prefix (defaults to two spaces);
    // we just assert "   hello" appears as a substring so we don't
    // accidentally couple to the prefix.
    assert!(
        plain.iter().any(|l| l.contains("   hello")),
        "expected three-space-indented body line, got: {:#?}",
        plain,
    );
}

#[test]
fn multiple_tabs_in_code_block_each_expand_to_three_spaces() {
    // Two leading tabs become 6 spaces (three per tab, independently),
    // not four (and not collapsed to a single indent unit).
    let mut m = md("```\n\t\thi\n```");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    assert!(
        plain.iter().any(|l| l.contains("      hi")),
        "expected six-space indent for two tabs, got: {:#?}",
        plain,
    );
}

#[test]
fn tab_indented_list_continuation_is_recognized_as_nested() {
    // A bullet-list whose nested marker is indented with a single tab
    // (3 columns after normalization, deeper than the base zero-column
    // indent). Without normalization, `indent_of` reads the tab as
    // zero indent and the marker would be parsed as a new top-level
    // bullet rather than a nested one.
    let mut m = md("- Top\n\t- Nested");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    // The top-level item appears flush.
    assert!(
        plain.iter().any(|l| l.contains("- Top")),
        "expected top-level bullet, got: {:#?}",
        plain,
    );
    // The nested item appears with leading indentation (the renderer
    // prefixes nested list items with two spaces per level, so the
    // visible result is `  - Nested`). We assert on the rendered shape
    // rather than on whitespace counts so the test isn't sensitive to
    // future indent-prefix changes.
    assert!(
        plain.iter().any(|l| l.starts_with("  - Nested")),
        "expected nested bullet under top-level, got: {:#?}",
        plain,
    );
}

// ---------------------------------------------------------------------------
// Tables
//
// Our Markdown parser does not yet understand `| col | col |` tables — see
// `Block::` enum in `components/markdown.rs`, which has no `Table` variant.
// Each test here asserts on table-specific structure (`│`, `─`, `┼`, row
// dividers, alignment, width wrapping). They'll switch on when tables land.
// ---------------------------------------------------------------------------

#[test]
fn renders_a_simple_table() {
    let mut m = md("| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |");
    let lines = m.render(80);
    let plain = plain_lines(&lines);

    assert!(plain.iter().any(|l| l.contains("Name")));
    assert!(plain.iter().any(|l| l.contains("Age")));
    assert!(plain.iter().any(|l| l.contains("Alice")));
    assert!(plain.iter().any(|l| l.contains("Bob")));
    assert!(plain.iter().any(|l| l.contains('│')));
    assert!(plain.iter().any(|l| l.contains('─')));
}

#[test]
fn renders_row_dividers_between_data_rows() {
    let mut m = md("| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |");
    let lines = m.render(80);
    let plain = plain_lines(&lines);
    let divider_count = plain.iter().filter(|l| l.contains('┼')).count();

    assert_eq!(divider_count, 2, "expected header + row divider");
}

#[test]
fn keeps_column_width_at_least_the_longest_word() {
    let longest = "superlongword";
    let mut m = md(&format!(
        "| Column One | Column Two |\n| --- | --- |\n| {} short | otherword |\n| small | tiny |",
        longest,
    ));
    let lines = m.render(32);
    let plain = plain_lines(&lines);
    let data = plain
        .iter()
        .find(|l| l.contains(longest))
        .expect("expected a data row containing the longest word");

    let segments: Vec<&str> = data.split('│').collect();
    let first_segment = segments
        .get(1)
        .expect("expected at least one inter-border segment");
    let first_column_width = first_segment.len().saturating_sub(2);

    assert!(
        first_column_width >= longest.len(),
        "expected first column width >= {}, got {}",
        longest.len(),
        first_column_width,
    );
}

#[test]
fn renders_tables_with_alignment() {
    let mut m = md("| Left | Center | Right |\n| :--- | :---: | ---: |\n\
         | A | B | C |\n| Long text | Middle | End |");
    let lines = m.render(80);
    let plain = plain_lines(&lines);

    assert!(plain.iter().any(|l| l.contains("Left")));
    assert!(plain.iter().any(|l| l.contains("Center")));
    assert!(plain.iter().any(|l| l.contains("Right")));
    assert!(plain.iter().any(|l| l.contains("Long text")));
}

#[test]
fn handles_tables_with_varying_column_widths() {
    let mut m = md("| Short | Very long column header |\n| --- | --- |\n\
         | A | This is a much longer cell content |\n| B | Short |");
    let lines = m.render(80);
    assert!(!lines.is_empty());

    let plain = plain_lines(&lines);
    assert!(plain.iter().any(|l| l.contains("Very long column header")));
    assert!(
        plain
            .iter()
            .any(|l| l.contains("This is a much longer cell content"))
    );
}

#[test]
fn wraps_table_cells_when_the_table_exceeds_available_width() {
    let mut m = md("| Command | Description | Example |\n| --- | --- | --- |\n\
         | npm install | Install all dependencies | npm install |\n\
         | npm run build | Build the project | npm run build |");
    let lines = m.render(50);
    let plain = plain_lines_trim_end(&lines);

    for line in &plain {
        assert!(
            aj_tui::ansi::visible_width(line) <= 50,
            "line exceeds width 50: {:?} (width {})",
            line,
            aj_tui::ansi::visible_width(line),
        );
    }

    let all = plain.join(" ");
    assert!(all.contains("Command"));
    assert!(all.contains("Description"));
    assert!(all.contains("npm install"));
    assert!(all.contains("Install"));
}

#[test]
fn wraps_long_cell_content_to_multiple_lines() {
    let mut m = md("| Header |\n| --- |\n| This is a very long cell content that should wrap |");
    let lines = m.render(25);
    let plain = plain_lines_trim_end(&lines);

    let data_rows: Vec<&String> = plain
        .iter()
        .filter(|l| l.starts_with('│') && !l.contains('─'))
        .collect();
    assert!(
        data_rows.len() > 2,
        "expected wrapped rows, got {} rows",
        data_rows.len(),
    );

    let all = plain.join(" ");
    assert!(all.contains("very long"));
    assert!(all.contains("cell content"));
    assert!(all.contains("should wrap"));
}

#[test]
fn wraps_long_unbroken_tokens_inside_table_cells() {
    let url = "https://example.com/this/is/a/very/long/url/that/should/wrap";
    let mut m = md(&format!("| Value |\n| --- |\n| prefix {} |", url));
    let width = 30;
    let lines = m.render(width);
    let plain = plain_lines_trim_end(&lines);

    for line in &plain {
        assert!(
            aj_tui::ansi::visible_width(line) <= width,
            "line exceeds width {}: {:?}",
            width,
            line,
        );
    }

    let table_lines: Vec<&String> = plain.iter().filter(|l| l.starts_with('│')).collect();
    assert!(!table_lines.is_empty(), "expected table rows to render");
    for line in &table_lines {
        let border_count = line.matches('│').count();
        assert_eq!(
            border_count, 2,
            "expected 2 borders, got {}: {:?}",
            border_count, line
        );
    }

    // Strip box-drawing + whitespace so we can assert the URL is preserved
    // even if it got split across wrapped lines.
    let joined: String = plain
        .iter()
        .flat_map(|l| l.chars())
        .filter(|c| !"│├┤─ \t".contains(*c))
        .collect();
    assert!(joined.contains("prefix"));
    assert!(joined.contains(url));
}

#[test]
fn wraps_styled_inline_code_inside_table_cells_without_breaking_borders() {
    let mut m = md("| Code |\n| --- |\n| `averyveryveryverylongidentifier` |");
    let width = 20;
    let lines = m.render(width);
    let joined = lines.join("\n");
    assert!(
        joined.contains("\x1b[33m"),
        "inline code should be styled (yellow)",
    );

    let plain = plain_lines_trim_end(&lines);
    for line in &plain {
        assert!(
            aj_tui::ansi::visible_width(line) <= width,
            "line exceeds width {}: {:?}",
            width,
            line,
        );
    }

    let table_lines: Vec<&String> = plain.iter().filter(|l| l.starts_with('│')).collect();
    for line in &table_lines {
        let border_count = line.matches('│').count();
        assert_eq!(
            border_count, 2,
            "expected 2 borders, got {}: {:?}",
            border_count, line
        );
    }
}

#[test]
fn handles_extremely_narrow_width_gracefully() {
    let mut m = md("| A | B | C |\n| --- | --- | --- |\n| 1 | 2 | 3 |");
    let lines = m.render(15);
    let plain = plain_lines_trim_end(&lines);

    assert!(!lines.is_empty(), "should produce output");
    for line in &plain {
        assert!(
            aj_tui::ansi::visible_width(line) <= 15,
            "line exceeds width 15: {:?}",
            line,
        );
    }
}

#[test]
fn renders_table_correctly_when_it_fits_naturally() {
    let mut m = md("| A | B |\n| --- | --- |\n| 1 | 2 |");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    let header = plain
        .iter()
        .find(|l| l.contains("A") && l.contains("B"))
        .expect("should have a header row");
    assert!(header.contains('│'), "header should have borders");

    assert!(
        plain.iter().any(|l| l.contains('├') && l.contains('┼')),
        "should have a separator row",
    );

    assert!(
        plain.iter().any(|l| l.contains('1') && l.contains('2')),
        "should have a data row",
    );
}

#[test]
fn respects_padding_x_when_calculating_table_width() {
    let mut m = md_with_padding(
        "| Column One | Column Two |\n| --- | --- |\n| Data 1 | Data 2 |",
        2,
        0,
    );
    let lines = m.render(40);
    let plain = plain_lines_trim_end(&lines);

    for line in &plain {
        assert!(
            aj_tui::ansi::visible_width(line) <= 40,
            "line exceeds width 40: {:?}",
            line,
        );
    }

    let table_row = plain
        .iter()
        .find(|l| l.contains('│'))
        .expect("expected a table row");
    assert!(
        table_row.starts_with("  "),
        "table should have left padding, got: {:?}",
        table_row,
    );
}

#[test]
fn does_not_add_a_trailing_blank_line_when_table_is_last() {
    let mut m = md("| Name |\n| --- |\n| Alice |");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    assert_ne!(
        plain.last().map(String::as_str),
        Some(""),
        "expected table to end without a blank line: {:?}",
        plain,
    );
}

// ---------------------------------------------------------------------------
// Combined features
// ---------------------------------------------------------------------------

#[test]
fn renders_lists_and_tables_together() {
    let mut m = md("# Test Document\n\n- Item 1\n  - Nested item\n- Item 2\n\n\
         | Col1 | Col2 |\n| --- | --- |\n| A | B |");
    let lines = m.render(80);
    let plain = plain_lines(&lines);

    assert!(plain.iter().any(|l| l.contains("Test Document")));
    assert!(plain.iter().any(|l| l.contains("- Item 1")));
    assert!(plain.iter().any(|l| l.contains("  - Nested item")));
    assert!(plain.iter().any(|l| l.contains("Col1")));
    assert!(plain.iter().any(|l| l.contains('│')));
}

// ---------------------------------------------------------------------------
// Pre-styled text (thinking traces)
// ---------------------------------------------------------------------------

#[test]
fn preserves_gray_italic_styling_after_inline_code() {
    // This replicates how thinking content is rendered in practice.
    // The pre-style should wrap the entire paragraph so that text after
    // an inline-code span retains gray + italic styling.
    let mut m = md_with_gray_italic("This is thinking with `inline code` and more text after");
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(joined.contains("inline code"));
    // Should have gray (90) and italic (3) codes from the pre-style.
    assert!(joined.contains("\x1b[90m"), "expected gray color code");
    assert!(joined.contains("\x1b[3m"), "expected italic code");
    assert!(joined.contains("\x1b[33m"), "expected inline code (yellow)");
}

#[test]
fn preserves_gray_italic_styling_after_bold_text() {
    let mut m = md_with_gray_italic("This is thinking with **bold text** and more after");
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(joined.contains("bold text"));
    assert!(joined.contains("\x1b[90m"), "expected gray color code");
    assert!(joined.contains("\x1b[3m"), "expected italic code");
    assert!(joined.contains("\x1b[1m"), "expected bold code");
}

#[test]
fn pre_styled_text_does_not_leak_italic_into_following_lines_in_tui() {
    // Guards: when thinking content is rendered above an input row, the
    // italic styling from the pre-style must not bleed into subsequent
    // lines on the terminal grid.
    let mut m = md_with_gray_italic("This is thinking with `inline code`");
    let terminal = VirtualTerminal::new(80, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    // We render the markdown and then a sentinel line below it, the way
    // a real chat layout would.
    let markdown_lines = m.render(80);
    let markdown_line_count = markdown_lines.len();
    assert!(markdown_line_count > 0);

    tui.add_child(Box::new(m));
    tui.add_child(Box::new(support::StaticLines::new(["INPUT"])));
    render_now(&mut tui);

    // The sentinel line below the markdown output must not have italic.
    let input_row: u16 = markdown_line_count
        .try_into()
        .expect("row fits in u16 for this test");
    let cell = terminal.cell(input_row, 0).expect("input row should exist");
    assert!(!cell.italic, "italic style leaked into following line");
}

// ---------------------------------------------------------------------------
// Spacing after code blocks
// ---------------------------------------------------------------------------

#[test]
fn exactly_one_blank_line_between_code_block_and_following_paragraph() {
    let mut m = md("hello world\n\n```js\nconst hello = \"world\";\n```\n\n\
         again, hello world");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    let closing = plain
        .iter()
        .position(|l| l == "```")
        .expect("should have closing backticks");

    let after = &plain[closing + 1..];
    let empty_count = after
        .iter()
        .position(|l| !l.is_empty())
        .expect("should have content after the blank line(s)");

    assert_eq!(
        empty_count,
        1,
        "expected 1 blank line after code block, got {}. lines after backticks: {:?}",
        empty_count,
        &after[..after.len().min(5)],
    );
}

#[test]
fn normalizes_paragraph_and_code_block_spacing_to_one_blank_line() {
    let cases = [
        "hello this is text\n```\ncode block\n```\nmore text",
        "hello this is text\n\n```\ncode block\n```\n\nmore text",
    ];
    let expected = vec![
        "hello this is text",
        "",
        "```",
        "  code block",
        "```",
        "",
        "more text",
    ];

    for text in cases {
        let mut m = md(text);
        let lines = m.render(80);
        let plain = plain_lines_trim_end(&lines);

        assert_eq!(
            plain, expected,
            "unexpected spacing for markdown: {:?}",
            text,
        );
    }
}

#[test]
fn does_not_add_a_trailing_blank_line_when_code_block_is_last() {
    for text in [
        "```js\nconst hello = 'world';\n```",
        "hello world\n\n```js\nconst hello = 'world';\n```",
    ] {
        let mut m = md(text);
        let lines = m.render(80);
        let plain = plain_lines_trim_end(&lines);

        assert_ne!(
            plain.last().map(String::as_str),
            Some(""),
            "expected code block to end without a blank line: {:?}",
            plain,
        );
    }
}

// ---------------------------------------------------------------------------
// Horizontal-rule width basis
// ---------------------------------------------------------------------------
//
// pi-tui's `renderToken(width, ...)` is called with `contentWidth =
// max(1, width - paddingX * 2)` (markdown.ts:147), so the local `width`
// parameter of `renderToken` (and the `─.repeat(min(width, 80))` it
// emits at line 420) is the *content* width, not the outer render
// width. Our Rust port matches: `render_block` is called with
// `content_width = width - 2 * padding_x`, and the `Block::HorizontalRule`
// arm emits `─.repeat(content_width.min(80))`. These tests pin down
// that pi-aligned shape so a future refactor that confuses the two
// width axes fails loudly.

#[test]
fn horizontal_rule_caps_at_eighty_visible_columns_when_content_width_is_wider() {
    // padding_x = 0 → content_width = 100; rule must be capped at 80.
    let mut m = md_with_padding("---", 0, 0);
    let lines = m.render(100);
    let plain = plain_lines_trim_end(&lines);

    let rule = plain
        .iter()
        .find(|l| l.contains('─'))
        .expect("expected an hr line");
    let dashes = rule.chars().filter(|c| *c == '─').count();
    assert_eq!(
        dashes, 80,
        "expected hr capped at 80 dashes, got {dashes}; rule: {rule:?}",
    );
}

#[test]
fn horizontal_rule_uses_content_width_when_narrower_than_eighty() {
    // padding_x = 4, width = 50 → content_width = 42, hr emits 42 dashes
    // (pi-tui: contentWidth = max(1, 50 - 8) = 42, then min(42, 80) = 42).
    let mut m = md_with_padding("---", 4, 0);
    let lines = m.render(50);
    let plain = plain_lines_trim_end(&lines);

    let rule = plain
        .iter()
        .find(|l| l.contains('─'))
        .expect("expected an hr line");
    let dashes = rule.chars().filter(|c| *c == '─').count();
    assert_eq!(
        dashes, 42,
        "expected hr to be content-width=42 dashes, got {dashes}; rule: {rule:?}",
    );
}

#[test]
fn horizontal_rule_subtracts_padding_x_pair_from_render_width_for_cap() {
    // padding_x = 4, width = 100 → content_width = 92, capped at 80.
    let mut m = md_with_padding("---", 4, 0);
    let lines = m.render(100);
    let plain = plain_lines_trim_end(&lines);

    let rule = plain
        .iter()
        .find(|l| l.contains('─'))
        .expect("expected an hr line");
    let dashes = rule.chars().filter(|c| *c == '─').count();
    assert_eq!(
        dashes, 80,
        "expected hr capped at 80 dashes (content_width=92 → min(92, 80)), got {dashes}; rule: {rule:?}",
    );
}

// ---------------------------------------------------------------------------
// Spacing after dividers
// ---------------------------------------------------------------------------

#[test]
fn exactly_one_blank_line_between_divider_and_following_paragraph() {
    let mut m = md("hello world\n\n---\n\nagain, hello world");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    let divider = plain
        .iter()
        .position(|l| l.contains('─'))
        .expect("should have a divider");

    let after = &plain[divider + 1..];
    let empty_count = after
        .iter()
        .position(|l| !l.is_empty())
        .expect("should have content after the divider's blank lines");

    assert_eq!(
        empty_count,
        1,
        "expected 1 blank line after divider, got {}. lines after divider: {:?}",
        empty_count,
        &after[..after.len().min(5)],
    );
}

#[test]
fn does_not_add_a_trailing_blank_line_when_divider_is_last() {
    let mut m = md("---");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    assert_ne!(
        plain.last().map(String::as_str),
        Some(""),
        "expected divider to end without a blank line: {:?}",
        plain,
    );
}

// ---------------------------------------------------------------------------
// Spacing after headings
// ---------------------------------------------------------------------------

#[test]
fn exactly_one_blank_line_between_heading_and_following_paragraph() {
    let mut m = md("# Hello\n\nThis is a paragraph");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    let heading = plain
        .iter()
        .position(|l| l.contains("Hello"))
        .expect("should have the heading");

    let after = &plain[heading + 1..];
    let empty_count = after
        .iter()
        .position(|l| !l.is_empty())
        .expect("should have content after the heading");

    assert_eq!(
        empty_count,
        1,
        "expected 1 blank line after heading, got {}. lines after heading: {:?}",
        empty_count,
        &after[..after.len().min(5)],
    );
}

#[test]
fn does_not_add_a_trailing_blank_line_when_heading_is_last() {
    let mut m = md("# Hello");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    assert_ne!(
        plain.last().map(String::as_str),
        Some(""),
        "expected heading to end without a blank line: {:?}",
        plain,
    );
}

// ---------------------------------------------------------------------------
// Spacing after blockquotes
// ---------------------------------------------------------------------------

#[test]
fn exactly_one_blank_line_between_blockquote_and_following_paragraph() {
    let mut m = md("hello world\n\n> This is a quote\n\nagain, hello world");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    let quote = plain
        .iter()
        .position(|l| l.contains("This is a quote"))
        .expect("should have the blockquote");

    let after = &plain[quote + 1..];
    let empty_count = after
        .iter()
        .position(|l| !l.is_empty())
        .expect("should have content after the blockquote");

    assert_eq!(
        empty_count,
        1,
        "expected 1 blank line after blockquote, got {}. lines after quote: {:?}",
        empty_count,
        &after[..after.len().min(5)],
    );
}

#[test]
fn does_not_add_a_trailing_blank_line_when_blockquote_is_last() {
    let mut m = md("> This is a quote");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    assert_ne!(
        plain.last().map(String::as_str),
        Some(""),
        "expected blockquote to end without a blank line: {:?}",
        plain,
    );
}

// ---------------------------------------------------------------------------
// Blockquotes with multiline content
// ---------------------------------------------------------------------------

#[test]
fn lazy_continuation_blockquote_applies_consistent_styling() {
    // Markdown lazy continuation — second line without a `>` is still
    // part of the quote.
    let mut m = md("> Foo\nbar");
    let lines = m.render(80);

    let plain = plain_lines(&lines);
    let quoted: Vec<&String> = plain.iter().filter(|l| l.starts_with("│ ")).collect();
    assert_eq!(quoted.len(), 2, "expected 2 quoted lines, got: {:?}", plain,);

    let foo_line = lines
        .iter()
        .find(|l| l.contains("Foo"))
        .expect("expected a line containing Foo");
    let bar_line = lines
        .iter()
        .find(|l| l.contains("bar"))
        .expect("expected a line containing bar");

    // Both should have italic (from `theme.quote`).
    assert!(
        foo_line.contains("\x1b[3m"),
        "Foo line should have italic: {:?}",
        foo_line,
    );
    assert!(
        bar_line.contains("\x1b[3m"),
        "bar line should have italic: {:?}",
        bar_line,
    );
}

#[test]
fn explicit_multiline_blockquote_applies_consistent_styling() {
    let mut m = md("> Foo\n> bar");
    let lines = m.render(80);

    let plain = plain_lines(&lines);
    let quoted: Vec<&String> = plain.iter().filter(|l| l.starts_with("│ ")).collect();
    assert_eq!(quoted.len(), 2, "expected 2 quoted lines, got: {:?}", plain,);

    let foo_line = lines
        .iter()
        .find(|l| l.contains("Foo"))
        .expect("expected a line containing Foo");
    let bar_line = lines
        .iter()
        .find(|l| l.contains("bar"))
        .expect("expected a line containing bar");

    assert!(foo_line.contains("\x1b[3m"), "Foo should have italic");
    assert!(bar_line.contains("\x1b[3m"), "bar should have italic");
}

#[test]
fn renders_list_content_inside_blockquotes() {
    let mut m = md("> 1. bla bla\n> - nested bullet");
    let lines = m.render(80);
    let plain = plain_lines(&lines);
    let quoted: Vec<&String> = plain.iter().filter(|l| l.starts_with("│ ")).collect();

    assert!(
        quoted.iter().any(|l| l.contains("1. bla bla")),
        "missing ordered list item: {:?}",
        quoted,
    );
    assert!(
        quoted.iter().any(|l| l.contains("- nested bullet")),
        "missing unordered list item: {:?}",
        quoted,
    );
}

#[test]
fn wraps_long_blockquote_lines_and_adds_border_to_each_wrapped_line() {
    let long =
        "This is a very long blockquote line that should wrap to multiple lines when rendered";
    let mut m = md(&format!("> {}", long));
    let lines = m.render(30);
    let plain = plain_lines_trim_end(&lines);

    let content: Vec<&String> = plain.iter().filter(|l| !l.is_empty()).collect();
    assert!(
        content.len() > 1,
        "expected multiple wrapped lines, got: {:?}",
        content,
    );

    for line in &content {
        assert!(
            line.starts_with("│ "),
            "wrapped line should have quote border: {:?}",
            line,
        );
    }

    let all: String = content
        .iter()
        .map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(all.contains("very long"));
    assert!(all.contains("blockquote"));
    assert!(all.contains("multiple"));
}

#[test]
fn indents_wrapped_blockquote_lines_with_styling() {
    let mut m = md("> This is styled text that is long enough to wrap");
    let lines = m.render(25);
    let plain = plain_lines_trim_end(&lines);
    let content: Vec<&String> = plain.iter().filter(|l| !l.is_empty()).collect();

    for line in &content {
        assert!(
            line.starts_with("│ "),
            "line should have quote border: {:?}",
            line,
        );
    }

    let joined = lines.join("\n");
    assert!(joined.contains("\x1b[3m"), "should have italic");
}

#[test]
fn renders_inline_formatting_inside_blockquotes_and_reapplies_quote_styling() {
    let mut m = md("> Quote with **bold** and `code`");
    let lines = m.render(80);
    let plain = plain_lines(&lines);

    assert!(
        plain.iter().any(|l| l.starts_with("│ ")),
        "should have quote border",
    );

    let all = plain.join(" ");
    assert!(all.contains("Quote with"));
    assert!(all.contains("bold"));
    assert!(all.contains("code"));

    let joined = lines.join("\n");
    assert!(joined.contains("\x1b[1m"), "should have bold styling");
    assert!(
        joined.contains("\x1b[33m"),
        "should have yellow inline code"
    );
    assert!(joined.contains("\x1b[3m"), "should have italic from quote");
}

// ---------------------------------------------------------------------------
// Block-level recursion inside blockquotes (F12)
// ---------------------------------------------------------------------------

#[test]
fn renders_fenced_code_block_inside_blockquote() {
    // F12 regression: a fenced code block inside a `>` quote should
    // recurse through `parse_markdown` and render as a code block
    // (with its border rows and content row), rather than as three
    // literal "```" / "let x = 1;" / "```" inline-text rows.
    let mut m = md("> ```\n> let x = 1;\n> ```");
    let lines = m.render(80);
    let plain = plain_lines(&lines);

    let quoted: Vec<&String> = plain.iter().filter(|l| l.starts_with("│ ")).collect();

    // The two ``` border rows survive the recursion and ride inside
    // the quote border (one quoted row each for the open and close
    // fence).
    let fence_rows: Vec<&&String> = quoted.iter().filter(|l| l.contains("```")).collect();
    assert_eq!(
        fence_rows.len(),
        2,
        "expected open and close ``` rows inside the quote, got: {:?}",
        quoted,
    );

    // The code body is preserved verbatim and stays inside the quote.
    assert!(
        quoted.iter().any(|l| l.contains("let x = 1;")),
        "expected code body inside the quote, got: {:?}",
        quoted,
    );

    // The quote's italic styling is still present — recursion didn't
    // strip it.
    let joined = lines.join("\n");
    assert!(
        joined.contains("\x1b[3m"),
        "should still have italic from quote",
    );
}

#[test]
fn fenced_code_block_inside_blockquote_does_not_collapse_surrounding_paragraphs() {
    // A fenced code block in the middle of a quote shouldn't disturb
    // the surrounding paragraph rows: each non-fenced source line
    // still renders on its own bordered row.
    let mut m = md("> first prose\n> ```\n> let x = 1;\n> ```\n> second prose");
    let lines = m.render(80);
    let plain = plain_lines(&lines);

    let quoted: Vec<&String> = plain.iter().filter(|l| l.starts_with("│ ")).collect();

    assert!(
        quoted.iter().any(|l| l.contains("first prose")),
        "expected 'first prose' row: {:?}",
        quoted,
    );
    assert!(
        quoted.iter().any(|l| l.contains("second prose")),
        "expected 'second prose' row: {:?}",
        quoted,
    );
    assert!(
        quoted.iter().any(|l| l.contains("let x = 1;")),
        "expected code body inside the quote: {:?}",
        quoted,
    );
}

// ---------------------------------------------------------------------------
// Heading with inline code
// ---------------------------------------------------------------------------

#[test]
fn preserves_heading_styling_after_inline_code_h3() {
    let mut m = md("### Why `sourceInfo` should not be optional");
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(
        joined.contains("\x1b[33m"),
        "expected yellow for inline code"
    );

    // The chunk strictly between the inline code's content and the
    // trailing prose must contain a fresh open of the heading style.
    // Without re-application, the inline code's `\x1b[39m` would strip
    // the heading color and the rest of the heading would render
    // without it.
    let code_end = joined
        .find("sourceInfo")
        .map(|p| p + "sourceInfo".len())
        .expect("rendered output should contain the inline-code content");
    let trailing = joined
        .find("should not be optional")
        .expect("should contain text after the inline code");
    let between = &joined[code_end..trailing];

    assert!(
        between.contains("\x1b[1m"),
        "should re-open bold after inline code, got: {:?}",
        between,
    );
    assert!(
        between.contains("\x1b[36m"),
        "should re-open cyan after inline code, got: {:?}",
        between,
    );
}

#[test]
fn preserves_heading_styling_after_inline_code_h1() {
    let mut m = md("# Title with `code` inside");
    let lines = m.render(80);
    let joined = lines.join("\n");

    let code_end = joined
        .find("code")
        .map(|p| p + "code".len())
        .expect("rendered output should contain the inline-code content");
    let trailing = joined
        .find("inside")
        .expect("should contain text after the inline code");
    let between = &joined[code_end..trailing];

    // H1 uses heading + underline + bold; all three need to be reopened.
    assert!(
        between.contains("\x1b[1m"),
        "should re-open bold for h1, got: {:?}",
        between,
    );
    assert!(
        between.contains("\x1b[36m"),
        "should re-open cyan for h1, got: {:?}",
        between,
    );
    assert!(
        between.contains("\x1b[4m"),
        "should re-open underline for h1, got: {:?}",
        between,
    );
}

#[test]
fn preserves_heading_color_on_trailing_text_after_inline_code() {
    // F9 from PORTING.md: `# foo \`bar\` baz` keeps the heading color
    // on `baz`. The inline code's `\x1b[39m` resets the foreground to
    // default; without re-applying the heading style around the
    // trailing text, `baz` ends up bold-only with the cyan stripped.
    let mut m = md("# foo `bar` baz");
    let lines = m.render(80);
    let joined = lines.join("\n");

    let baz_pos = joined
        .find(" baz")
        .expect("rendered output should contain ` baz`");
    // Walk back to the most recent cyan-open before the ` baz` text.
    // It must lie strictly after the inline code's `bar` content,
    // proving the heading style was re-emitted between the two.
    let cyan_open_before_baz = joined[..baz_pos]
        .rfind("\x1b[36m")
        .expect("expected at least one cyan open before ` baz`");
    let bar_end = joined
        .find("bar")
        .map(|p| p + "bar".len())
        .expect("rendered output should contain `bar`");
    assert!(
        cyan_open_before_baz > bar_end,
        "cyan open before ` baz` must be re-emitted after the inline \
         code's content (bar_end = {}, cyan_open_before_baz = {}, \
         joined = {:?})",
        bar_end,
        cyan_open_before_baz,
        joined,
    );
}

#[test]
fn does_not_leak_h1_underline_into_padding_when_inline_code_is_the_last_token() {
    let mut m = md("# Important distinction from `open()`");
    let terminal = VirtualTerminal::new(80, 4);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    // Compute the content width before moving the markdown into the TUI.
    let rendered = m.render(80);
    let first_line = rendered
        .first()
        .expect("should have rendered the heading line");
    let content_width = aj_tui::ansi::visible_width(&strip_ansi(first_line));
    assert!(content_width > 0, "should have visible heading content");

    tui.add_child(Box::new(m));
    render_now(&mut tui);

    for col in content_width..80 {
        let col_u16: u16 = col.try_into().expect("col fits in u16");
        let cell = terminal.cell(0, col_u16).expect("cell should exist");
        assert!(
            !cell.underline,
            "expected no underline in padding at col {}, got {:?}",
            col, cell,
        );
    }
}

#[test]
fn preserves_heading_styling_after_bold_text() {
    let mut m = md("## Heading with **bold** and more");
    let lines = m.render(80);
    let joined = lines.join("\n");

    let bold_end = joined
        .find("bold")
        .map(|p| p + "bold".len())
        .expect("rendered output should contain the inner bold text");
    let trailing = joined
        .find("and more")
        .expect("should contain text after the bold span");
    let between = &joined[bold_end..trailing];

    assert!(
        between.contains("\x1b[1m"),
        "should re-open bold for h2 after a bold span, got: {:?}",
        between,
    );
    assert!(
        between.contains("\x1b[36m"),
        "should re-open cyan for h2 after a bold span, got: {:?}",
        between,
    );
}

// ---------------------------------------------------------------------------
// F28: Headings always apply theme.bold (independent of theme.heading)
// ---------------------------------------------------------------------------
//
// pi-tui's heading branch wraps with `theme.bold` separately from
// `theme.heading`:
//   H1:  theme.heading(theme.bold(theme.underline(text)))
//   H2+: theme.heading(theme.bold(text))
// so a custom theme whose `heading` provides only a color (no bold)
// still produces bold heading text. The default theme this test crate
// uses (`default_markdown_theme`) bakes bold into `heading` itself, so
// these tests build a theme where `heading` is *only* a color and
// assert that bold (`\x1b[1m`) still appears on the rendered heading.

/// Build a markdown theme whose `heading` is only `style::cyan` (no
/// bold) and `bold` is `style::bold`. Used by the F28 tests to confirm
/// the heading branch wraps with `theme.bold` separately from
/// `theme.heading`.
fn theme_with_color_only_heading() -> MarkdownTheme {
    MarkdownTheme {
        heading: Arc::new(style::cyan),
        bold: Arc::new(style::bold),
        ..default_markdown_theme()
    }
}

#[test]
fn h1_renders_as_bold_even_when_theme_heading_is_only_a_color() {
    let mut m = Markdown::new("# Heading", 0, 0, theme_with_color_only_heading(), None);
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(
        joined.contains("\x1b[1m"),
        "H1 should apply theme.bold even when theme.heading only colors, got: {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b[36m"),
        "H1 should still apply theme.heading (cyan), got: {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b[4m"),
        "H1 should still apply theme.underline, got: {:?}",
        joined,
    );
}

#[test]
fn h2_renders_as_bold_even_when_theme_heading_is_only_a_color() {
    let mut m = Markdown::new("## Heading", 0, 0, theme_with_color_only_heading(), None);
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(
        joined.contains("\x1b[1m"),
        "H2 should apply theme.bold even when theme.heading only colors, got: {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b[36m"),
        "H2 should still apply theme.heading (cyan), got: {:?}",
        joined,
    );
    // H2 should NOT carry the underline (only H1 does).
    assert!(
        !joined.contains("\x1b[4m"),
        "H2 should not apply theme.underline (H1 only), got: {:?}",
        joined,
    );
}

#[test]
fn h3_renders_as_bold_even_when_theme_heading_is_only_a_color() {
    let mut m = Markdown::new("### Heading", 0, 0, theme_with_color_only_heading(), None);
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(
        joined.contains("\x1b[1m"),
        "H3 should apply theme.bold even when theme.heading only colors, got: {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b[36m"),
        "H3 should still apply theme.heading (cyan), got: {:?}",
        joined,
    );
    // H3+ should also style the `### ` prefix bold + colored.
    let plain = strip_ansi(&joined);
    assert!(
        plain.contains("### Heading"),
        "H3 should render the prefix verbatim, got plain: {:?}",
        plain,
    );
}

#[test]
fn h1_inline_code_still_reopens_bold_underline_and_color_after_reset() {
    // F28 + F9: with `theme.heading` providing only color and
    // `theme.bold` separate, the post-inline-code reapplication must
    // re-emit all three (bold, underline, color) on the trailing text.
    let mut m = Markdown::new(
        "# Title with `code` inside",
        0,
        0,
        theme_with_color_only_heading(),
        None,
    );
    let lines = m.render(80);
    let joined = lines.join("\n");

    let code_end = joined
        .find("code")
        .map(|p| p + "code".len())
        .expect("rendered output should contain the inline-code content");
    let trailing = joined
        .find("inside")
        .expect("should contain text after the inline code");
    let between = &joined[code_end..trailing];

    assert!(
        between.contains("\x1b[1m"),
        "should re-open bold after inline code, got: {:?}",
        between,
    );
    assert!(
        between.contains("\x1b[4m"),
        "should re-open underline after inline code, got: {:?}",
        between,
    );
    assert!(
        between.contains("\x1b[36m"),
        "should re-open cyan after inline code, got: {:?}",
        between,
    );
}

// ---------------------------------------------------------------------------
// Strikethrough syntax
// ---------------------------------------------------------------------------

#[test]
fn renders_double_tilde_as_strikethrough() {
    let mut m = md("Use ~~strikethrough~~ here");
    let lines = m.render(80);
    let joined = lines.join("\n");
    let plain = plain_lines(&lines).join(" ");

    assert!(
        joined.contains("\x1b[9m"),
        "should apply strikethrough styling"
    );
    assert!(plain.contains("strikethrough"));
    assert!(
        !plain.contains("~~strikethrough~~"),
        "should not render delimiters as text",
    );
}

#[test]
fn keeps_single_tilde_as_plain_text() {
    let mut m = md("Use ~strikethrough~ literally");
    let lines = m.render(80);
    let joined = lines.join("\n");
    let plain = plain_lines(&lines).join(" ");

    assert!(
        plain.contains("~strikethrough~"),
        "single-tilde delimiters should remain visible",
    );
    assert!(
        !joined.contains("\x1b[9m"),
        "single-tilde text should not use strikethrough styling",
    );
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

#[test]
fn does_not_duplicate_url_for_autolinked_emails() {
    let mut m = md("Contact user@example.com for help");
    let lines = m.render(80);
    let plain = plain_lines(&lines).join(" ");

    assert!(plain.contains("user@example.com"));
    assert!(
        !plain.contains("mailto:"),
        "should not show mailto: prefix for autolinked emails",
    );
}

#[test]
fn does_not_duplicate_url_for_bare_urls() {
    let mut m = md("Visit https://example.com for more");
    let lines = m.render(80);
    let plain = plain_lines(&lines).join(" ");

    let url_count = plain.matches("https://example.com").count();
    assert_eq!(url_count, 1, "URL should appear exactly once");
}

// Cap-state-dependent link rendering tests (parens fallback when
// `get_capabilities().hyperlinks == false`, OSC 8 emission when
// `true`) live in `tests/capabilities.rs` because they need the
// process-wide cap cache mutated under `#[serial_test::serial]`.
// Mirrors pi-tui's split: pi puts these in `markdown.test.ts`'s
// `describe("Links")` block with an `afterEach(resetCapabilitiesCache)`;
// our crate has a dedicated cap-cache test binary instead. The
// `does_not_duplicate_*` cases above stay here because both render
// branches return the URL exactly once (the autolink and mailto-strip
// shortcuts are independent of `hyperlinks`).

// ---------------------------------------------------------------------------
// Nested inlines in link text (PORTING.md F48)
// ---------------------------------------------------------------------------

#[test]
fn link_with_bold_text_renders_bold_inside_link_styling() {
    // F48: `Inline::Link` carries `Vec<Inline>` for the visible text, so
    // `[**important**](url)` parses as `Link([Bold([Text("important")])])`
    // rather than the pre-F48 `Link("**important**", ...)` shape that
    // captured the asterisks as literal characters. The render pass
    // should produce both the bold open (`\x1b[1m`) and the link's blue
    // (`\x1b[34m`) + underline (`\x1b[4m`) opens — with the visible
    // content reading as "important", not "**important**".
    let mut m = md("[**important**](https://example.com)");
    let lines = m.render(80);
    let plain = plain_lines(&lines).join(" ");
    let joined = lines.join("");

    assert!(
        plain.contains("important"),
        "rendered link should display the bold word: got {:?}",
        plain,
    );
    assert!(
        !plain.contains("**important**"),
        "asterisks must be consumed as bold delimiters, not literal text: got {:?}",
        plain,
    );
    assert!(
        joined.contains("\x1b[1m"),
        "bold inside link text should emit the bold open: got {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b[34m"),
        "link should still emit the blue (link) open: got {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b[4m"),
        "link should still emit the underline open: got {:?}",
        joined,
    );
}

#[test]
fn link_inside_heading_inherits_heading_style_via_context() {
    // F48: pi-tui's link renderer recurses with the same
    // `resolvedStyleContext` it received, so a link inside a heading
    // gets heading-styled link text. Our `render_inline_tokens`'s
    // `Link` arm now does the same — so a `# See [docs](url)` sees the
    // heading wrap (cyan + bold via `theme_with_color_only_heading`)
    // applied to "docs" alongside the link's own underline + blue.
    //
    // Without ctx recursion, the link's inner string would be just
    // "docs" (no heading codes), and the heading style would only
    // appear *outside* the link's own `\x1b[34m\x1b[4m` wrap. We pin
    // down the recursion explicitly: between the link blue open and
    // the literal "docs" there must be a heading-style open
    // (`\x1b[36m` cyan or `\x1b[1m` bold).
    let mut m = Markdown::new(
        "# See [docs](https://example.com)",
        0,
        0,
        theme_with_color_only_heading(),
        None,
    );
    let lines = m.render(80);
    let joined = lines.join("\n");
    let plain = plain_lines(&lines).join(" ");

    assert!(
        plain.contains("docs"),
        "heading should still display the link text: got {:?}",
        plain,
    );

    // Locate the link's blue open and the literal "docs" content.
    let blue_open = joined.find("\x1b[34m").expect("link should emit blue open");
    let docs_pos = joined[blue_open..]
        .find("docs")
        .map(|p| blue_open + p)
        .expect("docs text should appear after the link blue open");

    // Between the blue open and "docs", the heading wrap must reopen.
    // Pi's threaded context produces an `apply_text` call on the
    // "docs" text run that emits `\x1b[1m\x1b[36m` before "docs" and
    // `\x1b[39m\x1b[22m` after it. Without ctx recursion the link's
    // inner content is plain "docs" with no heading codes between the
    // blue open and the text.
    let between = &joined[blue_open..docs_pos];
    assert!(
        between.contains("\x1b[36m") || between.contains("\x1b[1m"),
        "heading style must thread through the link's inner text \
         (expected `\\x1b[36m` or `\\x1b[1m` between the link open at \
         byte {} and `docs` at byte {}); got between bytes: {:?}",
        blue_open,
        docs_pos,
        between,
    );
}

#[test]
fn autolinked_url_renders_with_simple_text_unchanged() {
    // F48 regression guard #1: bare URL and email autolinks now build
    // `Link(vec![Text(url)], url)` shapes. The new parser must not
    // accidentally consume `*` characters from a URL's path/query.
    let mut m = md("Visit https://example.com/path*with*stars for more");
    let lines = m.render(80);
    let plain = plain_lines(&lines).join(" ");

    let url_count = plain.matches("https://example.com/path*with*stars").count();
    assert_eq!(
        url_count, 1,
        "autolinked URL with literal stars should appear exactly once \
         (autolink plain text == url, no parens fallback); got: {:?}",
        plain,
    );
    // And the literal stars survive into the rendered output.
    assert!(
        plain.contains("path*with*stars"),
        "stars in autolinked URL must not be parsed as italic: got {:?}",
        plain,
    );
}

#[test]
fn autolinked_url_inside_heading_keeps_no_parens_fallback() {
    // F48 regression guard #2: with an outer style threaded through
    // the link arm (heading wrap, pre-style, etc.), the rendered inner
    // string carries ANSI codes around the URL — so a comparison
    // between `rendered` and `url` would never match, and the
    // no-parens fallback would silently break, duplicating every
    // autolinked URL inside a heading. The plain-text projection
    // sidesteps this: we compare the unstyled content to the URL.
    //
    // `# https://example.com` autolinks the URL inside a heading; the
    // rendered link content is wrapped with the heading's bold+cyan,
    // but the plain text is still just the URL itself. The autolink
    // fallback should fire (no `(url)` parens after the link).
    let mut m = Markdown::new(
        "# https://example.com",
        0,
        0,
        theme_with_color_only_heading(),
        None,
    );
    let lines = m.render(80);
    let plain = plain_lines(&lines).join(" ");

    let url_count = plain.matches("https://example.com").count();
    assert_eq!(
        url_count, 1,
        "autolinked URL inside a heading should still fire the no-parens \
         fallback (plain text == url, even when rendered text is wrapped \
         in heading ANSI codes); got: {:?}",
        plain,
    );
}

// ---------------------------------------------------------------------------
// HTML-like tags in text
// ---------------------------------------------------------------------------

#[test]
fn renders_html_like_tags_in_text_as_content_rather_than_hiding_them() {
    // When a model emits something like <thinking>content</thinking> in
    // regular text, a strict HTML-passthrough renderer would hide it.
    // We want the content (or the tags themselves) visible.
    let mut m = md("This is text with <thinking>hidden content</thinking> that should be visible");
    let lines = m.render(80);
    let plain = plain_lines(&lines).join(" ");

    assert!(
        plain.contains("hidden content") || plain.contains("<thinking>"),
        "expected tags or their content to be visible; got: {:?}",
        plain,
    );
}

#[test]
fn renders_html_tags_inside_code_blocks() {
    let mut m = md("```html\n<div>Some HTML</div>\n```");
    let lines = m.render(80);
    let plain = plain_lines(&lines).join("\n");

    assert!(
        plain.contains("<div>") && plain.contains("</div>"),
        "HTML inside code blocks should be visible; got: {:?}",
        plain,
    );
}

#[test]
fn html_tags_in_paragraph_text_pass_through_verbatim() {
    // PORTING.md F11: deliberate decision to pass HTML tags through as
    // plain text rather than emit a separate `html` token. A model that
    // wraps content in `<thinking>...</thinking>` should have those
    // exact bytes visible in the rendered output.
    let mut m = md("before <thinking>middle</thinking> after");
    let lines = m.render(80);
    let plain = plain_lines(&lines).join("\n");

    assert!(
        plain.contains("<thinking>") && plain.contains("</thinking>"),
        "literal HTML tags should survive verbatim; got: {:?}",
        plain,
    );
    assert!(
        plain.contains("middle"),
        "tag content should also be visible; got: {:?}",
        plain,
    );
}

// ---------------------------------------------------------------------------
// Hard line breaks (PORTING.md F11)
// ---------------------------------------------------------------------------
//
// CommonMark recognizes two hard-line-break markers at the end of a
// paragraph line: two or more trailing spaces, or a single trailing
// backslash. Either inserts an actual line break in the rendered
// output (rather than the soft-break default of joining with a space).

#[test]
fn paragraph_with_two_trailing_spaces_inserts_hard_line_break() {
    // Note: source string has a literal "  " before the `\n`; that's
    // the CommonMark hard-break marker.
    let mut m = md("first line  \nsecond line");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    let first = plain
        .iter()
        .position(|l| l.contains("first line"))
        .expect("`first line` should be present");
    let second = plain
        .iter()
        .position(|l| l.contains("second line"))
        .expect("`second line` should be present");

    assert!(
        second > first,
        "second line should be on a row after first; got first at {} and second at {}",
        first,
        second,
    );
    assert!(
        !plain[first].contains("second line"),
        "first and second should be on different rows; got combined: {:?}",
        plain[first],
    );
    // The trailing `  ` marker should be stripped from the output —
    // no row should have visible trailing spaces from it.
    assert!(
        !plain[first].ends_with("  "),
        "trailing-space marker should be stripped; got: {:?}",
        plain[first],
    );
}

#[test]
fn paragraph_with_trailing_backslash_inserts_hard_line_break() {
    let mut m = md("first line\\\nsecond line");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    let first = plain
        .iter()
        .position(|l| l.contains("first line"))
        .expect("`first line` should be present");
    let second = plain
        .iter()
        .position(|l| l.contains("second line"))
        .expect("`second line` should be present");

    assert!(second > first, "second line should be on a row after first",);
    // Trailing backslash should be stripped.
    assert!(
        !plain[first].contains('\\'),
        "trailing backslash should be stripped from rendered output; got: {:?}",
        plain[first],
    );
}

#[test]
fn paragraph_with_single_trailing_space_still_breaks_to_a_new_visible_row() {
    // Pi-aligned behavior (matches the upstream marked + wrap_text_with_ansi
    // pipeline): every newline in paragraph source is preserved as a
    // visible line break in the rendered output. A single trailing
    // space carries no special meaning here — the line break before
    // "second line" comes from the literal `\n` in the source.
    let mut m = md("first line \nsecond line");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    let first = plain
        .iter()
        .position(|l| l.contains("first line"))
        .expect("expected `first line` row");
    let second = plain
        .iter()
        .position(|l| l.contains("second line"))
        .expect("expected `second line` row");
    assert!(
        second > first,
        "the two source lines should land on separate rows; got: {:?}",
        plain,
    );
}

#[test]
fn paragraph_with_three_or_more_trailing_spaces_still_inserts_hard_line_break() {
    // The CommonMark rule is "two or more". `   ` (three) and `    `
    // (four) should both trigger.
    for spaces in ["   ", "    ", "      "] {
        let source = format!("first{}\nsecond", spaces);
        let mut m = md(&source);
        let lines = m.render(80);
        let plain = plain_lines_trim_end(&lines);

        let first = plain.iter().position(|l| l.contains("first")).unwrap();
        let second = plain.iter().position(|l| l.contains("second")).unwrap();
        assert!(
            second > first,
            "{} trailing spaces should still insert hard break; got: {:?}",
            spaces.len(),
            plain,
        );
    }
}

#[test]
fn paragraph_without_break_marker_renders_each_source_line_on_its_own_row() {
    // Pi-aligned: soft breaks are preserved as visible line breaks. A
    // multi-line paragraph without any explicit hard-break marker
    // still renders one source line per row — it's the literal `\n`
    // in the source that drives the row split, not the marker.
    let mut m = md("first line\nsecond line\nthird line");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    let first = plain
        .iter()
        .position(|l| l.contains("first line"))
        .expect("expected `first line` row");
    let second = plain
        .iter()
        .position(|l| l.contains("second line"))
        .expect("expected `second line` row");
    let third = plain
        .iter()
        .position(|l| l.contains("third line"))
        .expect("expected `third line` row");
    assert!(
        first < second && second < third,
        "each source line should land on its own row in order; got: {:?}",
        plain,
    );

    // None of the rows should carry the joined-with-space form that
    // strict CommonMark would produce.
    let any_joined = plain
        .iter()
        .any(|l| l.contains("first line second line") || l.contains("second line third line"));
    assert!(
        !any_joined,
        "no row should carry two source lines joined by a space; got: {:?}",
        plain,
    );
}

#[test]
fn multi_line_paragraph_with_inline_styling_preserves_each_row_with_styling() {
    // Pi-aligned regression: a paragraph that mixes inline styling
    // (bold, code) with a soft break should render with each source
    // line on its own row AND the inline styling still applied. The
    // upstream pi/marked path emits the paragraph as a single text
    // run with `\n` baked in, then `wrap_text_with_ansi` splits on
    // `\n` while carrying the active ANSI state across the split. We
    // do the same.
    let mut m = md("a **bold** word\nand `code` too");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    let first = plain
        .iter()
        .position(|l| l.contains("a") && l.contains("bold"))
        .expect("expected first row");
    let second = plain
        .iter()
        .position(|l| l.contains("and") && l.contains("code"))
        .expect("expected second row");
    assert!(
        second > first,
        "soft-break in mid-paragraph should split rows; got: {:?}",
        plain,
    );

    // Bold open (\x1b[1m) for `bold` and yellow code (\x1b[33m) for
    // `code` should both still appear in the rendered output.
    let joined = lines.join("\n");
    assert!(
        joined.contains("\x1b[1m"),
        "expected bold styling to survive soft-break split: {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b[33m"),
        "expected yellow inline-code styling to survive soft-break split: {:?}",
        joined,
    );
}

#[test]
fn paragraph_with_break_at_last_line_strips_marker_without_emitting_blank_row() {
    // A trailing-break marker on the *final* line of a paragraph has
    // nothing to break to. The marker should still be stripped from
    // the output, but no extra blank row should appear inside the
    // paragraph.
    let mut m = md("only line  ");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    let row = plain
        .iter()
        .find(|l| l.contains("only line"))
        .expect("paragraph content should render");
    assert!(
        !row.ends_with("  "),
        "trailing marker should be stripped on the last line too; got: {:?}",
        row,
    );
}

// ---------------------------------------------------------------------------
// Italic/bold word-boundary rule (PORTING.md F10)
// ---------------------------------------------------------------------------
//
// The opening `*` / `_` (single or double) of an emphasis run requires
// a non-word-char on its left, and the closing requires a non-word-char
// on its right. So `5*4*3` and `5**4**3` parse as plain text instead of
// emphasizing the numbers in the middle. Standard well-formed emphasis
// (`*foo*`, `**foo**`, `_foo_`, `__foo__` with whitespace or
// punctuation around it) is unaffected.

#[test]
fn intraword_asterisks_do_not_italicize_when_both_sides_are_word_chars() {
    let mut m = md("5*4*3");
    let lines = m.render(80);
    let joined = lines.join("\n");
    let plain = plain_lines(&lines).join("\n");

    assert!(
        plain.contains("5*4*3"),
        "literal asterisks should survive the rendered text; got: {:?}",
        plain,
    );
    assert!(
        !joined.contains("\x1b[3m"),
        "intraword asterisks must not open italic; got: {:?}",
        joined,
    );
}

#[test]
fn intraword_double_asterisks_do_not_bold_when_both_sides_are_word_chars() {
    let mut m = md("5**4**3");
    let lines = m.render(80);
    let joined = lines.join("\n");
    let plain = plain_lines(&lines).join("\n");

    assert!(
        plain.contains("5**4**3"),
        "literal asterisks should survive the rendered text; got: {:?}",
        plain,
    );
    assert!(
        !joined.contains("\x1b[1m"),
        "intraword `**` must not open bold; got: {:?}",
        joined,
    );
}

#[test]
fn intraword_underscores_do_not_italicize() {
    let mut m = md("foo_bar_baz");
    let lines = m.render(80);
    let joined = lines.join("\n");
    let plain = plain_lines(&lines).join("\n");

    assert!(
        plain.contains("foo_bar_baz"),
        "literal underscores should survive; got: {:?}",
        plain,
    );
    assert!(
        !joined.contains("\x1b[3m"),
        "intraword `_` must not open italic; got: {:?}",
        joined,
    );
}

#[test]
fn intraword_double_underscores_do_not_bold() {
    let mut m = md("foo__bar__baz");
    let lines = m.render(80);
    let joined = lines.join("\n");
    let plain = plain_lines(&lines).join("\n");

    assert!(
        plain.contains("foo__bar__baz"),
        "literal `__` should survive; got: {:?}",
        plain,
    );
    assert!(
        !joined.contains("\x1b[1m"),
        "intraword `__` must not open bold; got: {:?}",
        joined,
    );
}

#[test]
fn well_formed_italic_at_a_word_boundary_still_renders() {
    // Sanity check that the boundary tightening doesn't regress the
    // happy path — a paragraph with `text *foo* bar` keeps italics on
    // `foo`.
    let mut m = md("text *foo* bar");
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(
        joined.contains("\x1b[3m"),
        "well-formed `*foo*` should italicize; got: {:?}",
        joined,
    );
}

#[test]
fn well_formed_bold_at_a_word_boundary_still_renders() {
    let mut m = md("text **foo** bar");
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(
        joined.contains("\x1b[1m"),
        "well-formed `**foo**` should bold; got: {:?}",
        joined,
    );
}

#[test]
fn italic_at_start_of_text_still_opens() {
    // No preceding char counts as a non-word boundary, so `*foo*` at
    // the very beginning of a paragraph still italicizes.
    let mut m = md("*foo* bar");
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(
        joined.contains("\x1b[3m"),
        "leading `*foo*` should italicize; got: {:?}",
        joined,
    );
}

#[test]
fn italic_at_end_of_text_still_closes() {
    // No following char counts as a non-word boundary, so `*foo*` at
    // the very end of a paragraph still italicizes.
    let mut m = md("bar *foo*");
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(
        joined.contains("\x1b[3m"),
        "trailing `*foo*` should italicize; got: {:?}",
        joined,
    );
}

// ---------------------------------------------------------------------------
// Trailing-blank-line trim policy (PORTING.md F30)
// ---------------------------------------------------------------------------
//
// Source markdown that ends with two or more blank lines after a
// non-heading block must render with zero trailing blank rows. This
// is a deliberate (narrow) divergence from pi-tui: pi maps marked's
// trailing `space` token to exactly one `""` row, so pi would emit
// N+1 rows in that case and we emit N. For our agent's chat surface,
// trailing typing artifacts in an LLM-emitted message should not
// leak as dead space at the bottom of the rendered cell. The trim
// runs at the very end of `Markdown::render`, after every block has
// emitted its own per-block trailing spacer; see the policy comment
// above the trim loop in `src/components/markdown.rs` for the
// structural reason and the exact divergence-vs-no-divergence cases.
//
// Two cases that look like divergences but aren't (also locked in
// here so a future drift wouldn't introduce a divergence):
//
//   - Single trailing `\n` after a paragraph: marked produces no
//     trailing `space` token, so pi and we both emit zero trailing
//     blank rows.
//   - Heading followed by any number of trailing `\n`s: marked
//     absorbs the newlines into the heading token's `raw` field, so
//     pi and we both emit zero trailing blank rows.
//
// `padding_y` is applied *after* the trim, so a Markdown with
// explicit vertical padding still renders its requested top/bottom
// blank rows.

#[test]
fn paragraph_with_one_trailing_newline_renders_without_a_trailing_blank_row() {
    // Not a divergence from pi (marked produces no trailing `space`
    // token for `"hello\n"`, so pi also emits no trailing blank).
    // Locked in so a future change to the trim or the parser
    // doesn't accidentally introduce one here.
    let mut m = md("hello\n");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    assert_eq!(
        plain,
        vec!["hello".to_string()],
        "single trailing newline must not produce a trailing blank row: {:?}",
        plain,
    );
}

#[test]
fn paragraph_with_multiple_trailing_blank_lines_renders_without_trailing_blank_rows() {
    // Real divergence from pi: marked emits a trailing `space` token
    // for `"hello\n\n\n\n"`, pi maps it to one `""` row (so pi would
    // output `["hello", ""]`), and our trim removes it (so we output
    // `["hello"]`). One-row divergence by design — see F30 in
    // PORTING.md and the policy comment in `markdown.rs`.
    let mut m = md("hello\n\n\n\n");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    assert_eq!(
        plain,
        vec!["hello".to_string()],
        "trailing blank source lines must not produce trailing blank rows: {:?}",
        plain,
    );
}

#[test]
fn document_ending_with_heading_then_blank_lines_emits_no_trailing_blank_row() {
    // Not a divergence from pi (marked absorbs trailing `\n`s into
    // the heading token's `raw` field, so no trailing `space` token
    // is emitted; pi and we both produce a single heading row).
    // Locked in here so the heading-absorption behavior keeps
    // working — and the cross-block trim path still produces the
    // expected single-row output even if a future change makes the
    // heading branch emit a trailing `""` of its own. (H1's rendered
    // text drops the literal `#` prefix per the heading-style policy;
    // the assertion only cares that the row count is one.)
    let mut m = md("# title\n\n\n");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    assert_eq!(
        plain.len(),
        1,
        "heading + trailing blanks must collapse to a single heading row: {:?}",
        plain,
    );
    assert!(
        plain[0].contains("title"),
        "the surviving row must be the heading content: {:?}",
        plain,
    );
}

#[test]
fn padding_y_is_applied_after_trailing_blank_trim() {
    // The trim runs before bottom padding, so a Markdown with
    // `padding_y = 2` and a trailing-blank-rich input still emits
    // exactly two top blank rows + content + two bottom blank rows
    // — no extra rows from the unconditional per-block spacer or
    // from a residual source-trailing-blank row. Pi with the same
    // input would output six rows (one extra from the source-
    // trailing `space` token); our trim collapses it to five.
    let mut m = md_with_padding("hello\n\n\n", 0, 2);
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    assert_eq!(
        plain,
        vec![
            "".to_string(),
            "".to_string(),
            "hello".to_string(),
            "".to_string(),
            "".to_string(),
        ],
        "trim must run before bottom padding so the row count is exactly 2 + 1 + 2: {:?}",
        plain,
    );
}

#[test]
fn two_paragraphs_separated_by_blank_lines_keep_exactly_one_blank_between() {
    // Not a divergence from pi: spacing between blocks comes from
    // the `space` token between them, mapped to one `""` row by
    // both renderers. The source has 4 blank lines between the
    // paragraphs and 1 trailing newline, but the visible output
    // is still `["alpha", "", "beta"]`. Locked in here as the
    // companion to the trailing-trim cases — the trim only fires
    // at the *end* of the document, never between blocks.
    let mut m = md("alpha\n\n\n\nbeta\n");
    let lines = m.render(80);
    let plain = plain_lines_trim_end(&lines);

    assert_eq!(
        plain,
        vec!["alpha".to_string(), "".to_string(), "beta".to_string()],
        "two paragraphs separated by source blanks must still render with exactly one blank row between them: {:?}",
        plain,
    );
}

// ---------------------------------------------------------------------------
// Empty / whitespace-only input (PORTING.md F40)
// ---------------------------------------------------------------------------

/// pi-tui's `Markdown.render` short-circuits on
/// `!this.text || this.text.trim() === ""`, returning an empty result
/// without running the parser. We mirror that here: any input whose
/// trimmed form is empty (all spaces, tabs, or newlines, plus the
/// truly-empty case) renders zero rows, no parse work happens, and the
/// cache is populated so the second call for the same input is a cheap
/// clone.
#[test]
fn whitespace_only_text_is_treated_as_empty() {
    for whitespace in ["", "   ", "\n\n", "\t\t", " \n\t ", "    \t\n\n  "] {
        let mut m = md(whitespace);
        assert_eq!(
            m.render(80),
            Vec::<String>::new(),
            "whitespace-only input {:?} should render zero rows",
            whitespace,
        );
    }
}

/// A Markdown with non-default `padding_y` still emits zero rows when
/// the source is whitespace-only — pi's early-return runs before
/// padding is applied, and so does ours. Locks in that the F40 guard
/// short-circuits before the top-padding loop.
#[test]
fn whitespace_only_text_with_padding_y_still_renders_zero_rows() {
    let mut m = md_with_padding("   \n\t\n", 0, 3);
    assert_eq!(
        m.render(80),
        Vec::<String>::new(),
        "whitespace-only input with padding_y must still render zero rows",
    );
}

/// Repeated renders of the same whitespace-only input return the same
/// empty vec consistently. Locks in that the F40 cache-write on the
/// early-return branch doesn't trip on a second/third call (e.g. by
/// hitting the cache check, finding stale state from the first
/// trim-empty short-circuit, and emitting unexpected rows). Mirrors
/// the cache-hit pattern in `tests/text.rs`'s
/// `repeat_render_with_same_args_returns_cached_result`.
#[test]
fn repeat_whitespace_only_render_returns_cached_empty_result() {
    let mut m = md("   \n\t\n");
    let first = m.render(80);
    let second = m.render(80);
    let third = m.render(80);

    assert_eq!(first, Vec::<String>::new());
    assert_eq!(first, second);
    assert_eq!(second, third);
}

// ---------------------------------------------------------------------------
// Degenerate render width (PORTING.md F42)
// ---------------------------------------------------------------------------

/// pi-tui's `Markdown.render` clamps `contentWidth` to
/// `Math.max(1, width - this.paddingX * 2)`, so a render width of `0`
/// (or `width < 2 * padding_x`) still produces visible output — every
/// inline content row collapses to a one-cell-wide column with each
/// grapheme on its own row, instead of returning an empty vec.
///
/// `# hello` at `width = 0` therefore renders as one row per visible
/// grapheme of `hello`, plus whatever trailing structure the heading
/// branch emits (the `wrap_text_with_ansi` post-processing keeps each
/// row to `<= 1` visible cell). Locks in the F42 fix; without the
/// `.max(1)` clamp the early `return Vec::new()` we used to have on
/// `content_width == 0` swallowed the entire render.
#[test]
fn render_at_zero_width_clamps_content_width_to_one_cell() {
    let mut m = md("# hello");
    let lines = m.render(0);
    let plain = plain_lines(&lines);

    assert!(
        !plain.is_empty(),
        "render(0) must not collapse to an empty vec; got {:?}",
        plain,
    );

    // Every emitted row must fit inside the one-cell content column.
    // (We don't assert exact row count or content because the heading
    // branch emits trailing-blank rows that are visible-width 0; the
    // important invariant is "no row exceeds the clamped width".)
    for line in &lines {
        let vis = aj_tui::ansi::visible_width(line);
        assert!(
            vis <= 1,
            "row {:?} exceeded the one-cell content width: visible_width = {}",
            line,
            vis,
        );
    }

    // The heading body letters survive across the per-grapheme break.
    let joined: String = plain.iter().flat_map(|s| s.chars()).collect();
    assert!(
        joined.contains('h')
            && joined.contains('e')
            && joined.contains('l')
            && joined.contains('o'),
        "heading body letters must appear somewhere in the rendered rows; got {:?}",
        plain,
    );
}

/// `padding_x = 8, width = 10` is the other degenerate shape:
/// `width < 2 * padding_x` would historically `saturating_sub` to `0`
/// and short-circuit. With the F42 clamp `content_width` is `1` and
/// the heading body still renders one grapheme per row, padded on the
/// left by `padding_x` spaces. Visible-width-only check (the leading
/// padding doesn't count toward the one-cell content column).
#[test]
fn render_with_padding_x_exceeding_half_width_clamps_to_one_cell() {
    let mut m = md_with_padding("# hello", 8, 0);
    let lines = m.render(10);
    let plain = plain_lines(&lines);

    assert!(
        !plain.is_empty(),
        "padding_x = 8 / width = 10 must not collapse to an empty vec; got {:?}",
        plain,
    );

    // The leading `padding_x` spaces are part of every row, but the
    // *content* portion (after the padding) must stay <= 1 visible
    // cell. Total visible row width <= padding_x + content_width = 9.
    for line in &lines {
        let vis = aj_tui::ansi::visible_width(line);
        assert!(
            vis <= 9,
            "row {:?} exceeded padding_x + 1: visible_width = {}",
            line,
            vis,
        );
    }

    let joined: String = plain.iter().flat_map(|s| s.chars()).collect();
    assert!(
        joined.contains('h') && joined.contains('e') && joined.contains('o'),
        "heading body letters must survive the per-grapheme break; got {:?}",
        plain,
    );
}

/// Locks in the cache-hit behavior on the degenerate-width path:
/// once `content_width` reaches `1`, the regular cache write at the
/// tail of `render` fires (F43), so a second call with the same
/// degenerate inputs returns the cached vec verbatim.
#[test]
fn repeat_degenerate_width_render_returns_cached_result() {
    let mut m = md("# hello");
    let first = m.render(0);
    let second = m.render(0);
    let third = m.render(0);

    assert!(
        !first.is_empty(),
        "first render at width 0 must not be empty"
    );
    assert_eq!(
        first, second,
        "second render at width 0 must match the first (cache hit)",
    );
    assert_eq!(
        second, third,
        "third render at width 0 must match the second (cache hit)",
    );
}

// ---------------------------------------------------------------------------
// Blockquote inner-width clamp + table fallback (PORTING.md F44)
// ---------------------------------------------------------------------------
//
// F42 made `content_width = 1` reachable for `Markdown::render`. F44
// brings the recursive paths in line with pi at degenerate widths:
//
// * Blockquote arm: clamp `inner_width` to `>= 1` (pi-tui
//   `markdown.ts:382`: `Math.max(1, width - 2)`). Without the clamp the
//   inner paragraph wrap at `width = 0` prepends a leading empty line,
//   which the blockquote loop turns into a bare `│ ` row before the
//   content rows.
// * Table arm: when `available_for_cells < n_cols` (i.e. the table
//   chrome alone exceeds `content_width`), fall back to wrapping the
//   raw markdown source through `wrap_text_with_ansi` (pi-tui
//   `markdown.ts:696-703`). Without the fallback the table renders
//   with chrome wider than its content cells, producing visually
//   broken output.

/// Blockquote at outer `content_width = 0` recurses with
/// `inner_width = 1` (was `0` before F44), so the inner paragraph
/// wraps `hello` to one cell per row, giving exactly `5` content
/// rows. The pre-F44 path fed `wrap_text_with_ansi` width 0, which
/// prepends an empty line ahead of the per-grapheme break — that
/// empty line becomes a leading bare-border row in the blockquote
/// output, so the pre-F44 row count was `6` and the first non-blank
/// row was a bare `│ ` rather than `│ h`.
#[test]
fn blockquote_at_zero_width_recurses_with_one_cell_inner_width() {
    let mut m = md("> hello");
    let lines = m.render(0);
    let plain = plain_lines(&lines);

    assert!(
        !plain.is_empty(),
        "blockquote at width 0 must not collapse to an empty vec; got {:?}",
        plain,
    );

    // Every emitted row is either empty (trailing blank) or a
    // border-prefixed row whose content portion is at most one cell.
    // `│ ` is two columns wide, so total row visible width is `≤ 3`.
    for line in &lines {
        let vis = aj_tui::ansi::visible_width(line);
        assert!(
            vis <= 3,
            "blockquote row {:?} exceeded border + 1 cell: visible_width = {}",
            line,
            vis,
        );
    }

    // The first (non-trailing-blank) row must be a content row, not a
    // bare-border row. Without the F44 clamp the inner wrap produces
    // a leading empty line, which the blockquote loop turns into a
    // bare `│ ` row before the `h`-row.
    let first_nonempty = plain
        .iter()
        .find(|l| !l.trim().is_empty())
        .expect("blockquote must produce at least one content row");
    assert!(
        first_nonempty.contains('h'),
        "first content row must hold the body letter `h`, not be a bare border; got {:?} from full output {:?}",
        first_nonempty,
        plain,
    );

    // Five body letters (`hello`) → exactly five content rows after
    // the outer `Markdown::render` trim collapses the trailing blank.
    // Without the F44 clamp the inner wrap's leading empty line adds
    // a bare-border row up front, pushing the count to six.
    assert_eq!(
        lines.len(),
        5,
        "blockquote at width 0 must render exactly 5 content rows; got {} rows: {:?}",
        lines.len(),
        plain,
    );
}

/// Cache-hit smoke on the degenerate-width blockquote path: a second
/// render with the same shape returns the cached vec verbatim.
#[test]
fn repeat_degenerate_width_blockquote_render_returns_cached_result() {
    let mut m = md("> hello");
    let first = m.render(0);
    let second = m.render(0);
    let third = m.render(0);

    assert!(
        !first.is_empty(),
        "first render of blockquote at width 0 must not be empty",
    );
    assert_eq!(
        first, second,
        "second render at width 0 must match the first (cache hit)",
    );
    assert_eq!(
        second, third,
        "third render at width 0 must match the second (cache hit)",
    );
}

/// Single-column table at outer `content_width = 0`: chrome (4 chars
/// of `│ _ _ │`) exceeds the content width, so the renderer falls
/// back to wrapping the raw markdown source via `wrap_text_with_ansi`.
/// The output contains the literal source bytes (`|`, `-`, `h`, `x`)
/// per-grapheme on their own rows, and never produces table-chrome
/// glyphs (`┌`, `─`, `├`, etc.) that would be wider than the
/// content area.
#[test]
fn single_column_table_at_zero_width_falls_back_to_raw_markdown() {
    let mut m = md("| h |\n|---|\n| x |");
    let lines = m.render(0);
    let plain = plain_lines(&lines);

    assert!(
        !plain.is_empty(),
        "table at width 0 must not collapse to an empty vec; got {:?}",
        plain,
    );

    // Every emitted row is at most one cell wide — the fallback
    // wraps `raw` at `content_width = 0`, so each row holds a single
    // grapheme of the source.
    for line in &lines {
        let vis = aj_tui::ansi::visible_width(line);
        assert!(
            vis <= 1,
            "row {:?} exceeded one-cell content width: visible_width = {}",
            line,
            vis,
        );
    }

    // Raw markdown bytes (`|`, `h`, `x`) must appear; table-chrome
    // glyphs must not, because the fallback bypasses `render_table_row`.
    // Our port emits `│` sides and `├─...─┤` separators (no top/bottom
    // borders) for rendered tables, so checking for either of those
    // catches the "table rendered anyway" regression.
    let joined: String = plain.iter().flat_map(|s| s.chars()).collect();
    for ch in ['|', 'h', 'x', '-'] {
        assert!(
            joined.contains(ch),
            "raw markdown char `{}` must appear in fallback rows; got {:?}",
            ch,
            plain,
        );
    }
    for ch in ['│', '├', '┤', '┼'] {
        assert!(
            !joined.contains(ch),
            "table-chrome glyph `{}` must not appear when falling back; got {:?}",
            ch,
            plain,
        );
    }
}

/// Two-column table at outer `content_width = 0`: same fallback shape
/// as the single-column case. All four cell letters (a, b, x, y) plus
/// pipes appear in the fallback output, and no table chrome.
#[test]
fn two_column_table_at_zero_width_falls_back_to_raw_markdown() {
    let mut m = md("| a | b |\n|---|---|\n| x | y |");
    let lines = m.render(0);
    let plain = plain_lines(&lines);

    assert!(
        !plain.is_empty(),
        "two-column table at width 0 must not collapse to an empty vec; got {:?}",
        plain,
    );

    for line in &lines {
        let vis = aj_tui::ansi::visible_width(line);
        assert!(
            vis <= 1,
            "row {:?} exceeded one-cell content width: visible_width = {}",
            line,
            vis,
        );
    }

    let joined: String = plain.iter().flat_map(|s| s.chars()).collect();
    for ch in ['a', 'b', 'x', 'y', '|'] {
        assert!(
            joined.contains(ch),
            "raw markdown char `{}` must appear in fallback rows; got {:?}",
            ch,
            plain,
        );
    }
    for ch in ['│', '├', '┤', '┼'] {
        assert!(
            !joined.contains(ch),
            "table-chrome glyph `{}` must not appear when falling back; got {:?}",
            ch,
            plain,
        );
    }
}

/// Boundary case: at exactly `content_width = chrome` (no room for
/// even one cell column past the borders), the renderer still falls
/// back. For a single-column table `chrome = 4`, so width 4 hits the
/// fallback (`available_for_cells = 0 < n_cols = 1`); width 5 fits
/// one cell and renders the full table (with `│` side borders and
/// `├─...─┤` separator rows — our port doesn't emit `┌`/`└` top
/// or bottom borders).
#[test]
fn single_column_table_falls_back_when_width_equals_chrome() {
    let mut m = md("| h |\n|---|\n| x |");
    let plain_at_chrome = plain_lines(&m.render(4));
    let mut m2 = md("| h |\n|---|\n| x |");
    let plain_above_chrome = plain_lines(&m2.render(5));

    let chrome_joined: String = plain_at_chrome.iter().flat_map(|s| s.chars()).collect();
    assert!(
        chrome_joined.contains('|'),
        "width = chrome (4) must fall back to raw markdown (contains ASCII `|`); got {:?}",
        plain_at_chrome,
    );
    assert!(
        !chrome_joined.contains('│') && !chrome_joined.contains('├'),
        "width = chrome (4) must not emit table-chrome glyphs (`│` or `├`); got {:?}",
        plain_at_chrome,
    );

    let above_joined: String = plain_above_chrome.iter().flat_map(|s| s.chars()).collect();
    assert!(
        above_joined.contains('│') && above_joined.contains('├'),
        "width = chrome + 1 (5) must render the table (contains `│` and `├`); got {:?}",
        plain_above_chrome,
    );
}

/// Cache-hit smoke on the degenerate-width table fallback path.
#[test]
fn repeat_degenerate_width_table_render_returns_cached_result() {
    let mut m = md("| a | b |\n|---|---|\n| x | y |");
    let first = m.render(0);
    let second = m.render(0);
    let third = m.render(0);

    assert!(
        !first.is_empty(),
        "first render of table at width 0 must not be empty",
    );
    assert_eq!(
        first, second,
        "second render at width 0 must match the first (cache hit)",
    );
    assert_eq!(
        second, third,
        "third render at width 0 must match the second (cache hit)",
    );
}

// ---------------------------------------------------------------------------
// Inline-style context (PORTING.md F41)
// ---------------------------------------------------------------------------
//
// Pi-tui's `InlineStyleContext` machinery (markdown.ts:73-76) wraps
// each text run with the outer style and re-emits the opens-only prefix
// after each non-text inline so the outer style restores on trailing
// text. We mirror that for headings (heading wrap context) and
// paragraphs (pre-style context). Tables, list items, and blockquote
// internals stay on the identity context per the documented scope.
//
// The tests below assert the *position* of style re-emits (not just
// their presence somewhere in the rendered output), which is what the
// pi-style machinery delivers and the older outer-wrap shape didn't.

#[test]
fn pre_styled_paragraph_reopens_gray_italic_after_inline_code() {
    // F41 main fix on the paragraph branch: with `default_text_style` threaded
    // through the inline context, gray + italic must re-open between
    // an inline code's `\x1b[39m` close and the trailing text on the
    // same paragraph. Before F41 the pre-style wrapped the whole
    // paragraph string once, so the inline code's foreground reset
    // stripped the gray on " more text after" and the trailing close
    // only fired at the very end of the wrapped paragraph.
    let mut m = md_with_gray_italic("This is thinking with `inline code` and more text after");
    let lines = m.render(80);
    let joined = lines.join("\n");

    let inline_code_end = joined
        .find("inline code")
        .map(|p| p + "inline code".len())
        .expect("rendered output should contain 'inline code'");
    let trailing_pos = joined
        .find(" and more text after")
        .expect("rendered output should contain the trailing text");

    // Walk back from the trailing text to the most recent gray-open
    // (`\x1b[90m`) and italic-open (`\x1b[3m`). Both must lie strictly
    // after the inline code's content end, proving the pre-style
    // re-opened between the inline reset and the trailing run.
    let last_gray_open = joined[..trailing_pos]
        .rfind("\x1b[90m")
        .expect("expected at least one gray open before trailing text");
    let last_italic_open = joined[..trailing_pos]
        .rfind("\x1b[3m")
        .expect("expected at least one italic open before trailing text");

    assert!(
        last_gray_open > inline_code_end,
        "gray open must be re-emitted after inline code's content \
         (inline_code_end = {}, last_gray_open = {}, joined = {:?})",
        inline_code_end,
        last_gray_open,
        joined,
    );
    assert!(
        last_italic_open > inline_code_end,
        "italic open must be re-emitted after inline code's content \
         (inline_code_end = {}, last_italic_open = {}, joined = {:?})",
        inline_code_end,
        last_italic_open,
        joined,
    );
}

#[test]
fn pre_styled_paragraph_reopens_gray_italic_after_bold_span() {
    // F41 same shape for `**bold**` inlines: bold's `\x1b[22m` close
    // affects the bold/dim attribute set, but in pi's machinery the
    // pre-style prefix is appended after the bold span anyway so
    // trailing text gets a fresh gray + italic open.
    let mut m = md_with_gray_italic("This is thinking with **bold text** and more after");
    let lines = m.render(80);
    let joined = lines.join("\n");

    let bold_end = joined
        .find("bold text")
        .map(|p| p + "bold text".len())
        .expect("rendered output should contain 'bold text'");
    let trailing_pos = joined
        .find(" and more after")
        .expect("rendered output should contain the trailing text");

    let last_gray_open = joined[..trailing_pos]
        .rfind("\x1b[90m")
        .expect("expected at least one gray open before trailing text");
    // For italic we explicitly look for `\x1b[3m` (4 bytes); the
    // inline's `\x1b[3m` is the same opening sequence for italic so
    // we just want one strictly after the bold content.
    let last_italic_open = joined[..trailing_pos]
        .rfind("\x1b[3m")
        .expect("expected at least one italic open before trailing text");

    assert!(
        last_gray_open > bold_end,
        "gray open must be re-emitted after bold span's content \
         (bold_end = {}, last_gray_open = {}, joined = {:?})",
        bold_end,
        last_gray_open,
        joined,
    );
    assert!(
        last_italic_open > bold_end,
        "italic open must be re-emitted after bold span's content \
         (bold_end = {}, last_italic_open = {}, joined = {:?})",
        bold_end,
        last_italic_open,
        joined,
    );
}

#[test]
fn pre_styled_paragraph_with_no_inlines_still_wraps_text_in_gray_italic() {
    // Sanity: pure-text paragraph with `default_text_style` should still produce
    // gray + italic. Before and after F41 this works; the test guards
    // against a regression where the new context machinery accidentally
    // dropped the wrap on the last text run (e.g. an over-eager trim).
    let mut m = md_with_gray_italic("This is plain thinking text");
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(
        joined.contains("\x1b[90m"),
        "expected gray open in plain pre-styled paragraph: {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b[3m"),
        "expected italic open in plain pre-styled paragraph: {:?}",
        joined,
    );
}

#[test]
fn pre_styled_paragraph_ending_with_inline_code_trims_dangling_prefix() {
    // F41 trim invariant: when the last inline is a non-text token
    // (e.g. inline code), the trailing `style_prefix` we emit after
    // it has no following text to re-open the style for. Pi trims
    // that dangling prefix (markdown.ts:536-538) so the line doesn't
    // end with an opens-only sequence that downstream wrap_text could
    // misinterpret. Mirror that: the rendered output must not end
    // with the pre-style prefix.
    let mut m = md_with_gray_italic("This is thinking with `inline code`");
    let lines = m.render(80);
    let joined = lines.join("\n");

    // The default-style prefix is `\x1b[3m\x1b[90m` (italic-open then
    // gray-open: F47 reordered the apply path to color-innermost /
    // italic-outer to match pi's `applyDefaultStyle`, and H4 moved
    // that path to `Markdown::apply_default_style` while keeping the
    // ordering). After F41's trim, the rendered output should not end
    // with that exact sequence.
    let pre_prefix = "\x1b[3m\x1b[90m";
    assert!(
        !joined.ends_with(pre_prefix),
        "rendered output should not end with a dangling pre-style prefix; got {:?}",
        joined,
    );
    // The inline code's own content and reset must still be present.
    assert!(
        joined.contains("inline code"),
        "expected inline code content: {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b[33m"),
        "expected inline code styling (yellow open): {:?}",
        joined,
    );
}

#[test]
fn h1_inline_code_keeps_its_own_styling_only_not_double_wrapped_with_heading() {
    // F41 visible change on the heading branch: the inline code body
    // is no longer wrapped with the heading style as well as the
    // code style. Previously each top-level inline got its own outer
    // heading-wrap (so `theme.heading(theme.bold(theme.underline(theme.code("bar"))))`
    // produced bold+underlined+colored-AND-yellow `bar`). After F41
    // pi's machinery emits just `theme.code("bar")` for the inline
    // and re-opens the heading style *after* the inline ends.
    //
    // Probe: immediately after the inline code's closing `\x1b[39m`,
    // the next bytes should be the heading style_prefix
    // (`\x1b[36m\x1b[1m\x1b[4m` for H1 with the color-only-heading
    // theme), NOT the heading wrap closes (`\x1b[24m\x1b[22m\x1b[39m`)
    // that the older per-segment wrap would have emitted.
    let mut m = Markdown::new(
        "# foo `bar` baz",
        0,
        0,
        theme_with_color_only_heading(),
        None,
    );
    let lines = m.render(80);
    let joined = lines.join("\n");

    // Find the inline code's content close: `bar\x1b[39m` is the
    // theme.code's closing. Walk past it and inspect what follows.
    let close_marker = "bar\x1b[39m";
    let close_end = joined
        .find(close_marker)
        .map(|p| p + close_marker.len())
        .expect("rendered output should contain the inline-code close");
    let after = &joined[close_end..];

    // After F41, we expect the heading style_prefix re-emit
    // (`\x1b[36m` + `\x1b[1m` + `\x1b[4m` for H1) to immediately
    // follow the code's `\x1b[39m`. The older outer-wrap form would
    // have emitted `\x1b[24m` (underline-off) here instead.
    assert!(
        !after.starts_with("\x1b[24m"),
        "post-inline-code byte should not be the heading wrap's \
         underline-off close — that means the inline body got the \
         old per-segment heading wrap. got after-bytes: {:?}",
        &after[..after.len().min(40)],
    );
    assert!(
        after.starts_with("\x1b[36m"),
        "post-inline-code byte should immediately re-open the heading \
         color (style_prefix re-emit). got after-bytes: {:?}",
        &after[..after.len().min(40)],
    );
}

#[test]
fn h1_inline_code_at_end_of_heading_trims_dangling_heading_prefix() {
    // F41 trim parallel for the heading branch: when the last inline
    // is a non-text token, the heading style_prefix appended after it
    // gets trimmed so the heading line doesn't end with a stray
    // opens-only sequence. Mirrors pi's trim (markdown.ts:536-538).
    let mut m = Markdown::new(
        "# Title with `code`",
        0,
        0,
        theme_with_color_only_heading(),
        None,
    );
    let lines = m.render(80);
    let joined = lines.join("\n");

    // The H1 style_prefix with the color-only-heading theme is
    // `\x1b[36m\x1b[1m\x1b[4m` (cyan + bold + underline). After the
    // trim, the rendered heading should not end with that exact
    // sequence (it would if the trim never fired).
    let h1_prefix = "\x1b[36m\x1b[1m\x1b[4m";
    assert!(
        !joined.ends_with(h1_prefix),
        "heading rendered output should not end with a dangling \
         heading style_prefix; got {:?}",
        joined,
    );
}

// ---------------------------------------------------------------------------
// apply_default_style field-ordering parity with pi (PORTING.md F47, H4)
// ---------------------------------------------------------------------------
//
// Pi-tui's `applyDefaultStyle` (markdown.ts:210-237) applies the
// foreground color first (innermost) and then the text decorations
// (bold, italic, strikethrough, underline) on top, in that order.
// Our [`DefaultTextStyle`] mirrors five of those six fields
// (everything except `bgColor` — see PORTING.md H4 for the deferral
// rationale); F47 pinned the color-innermost / italic-outer shape on
// the apply path, and H4 moved that apply path onto `Markdown` itself
// (`Markdown::apply_default_style`) so the text-decoration calls
// route through `self.theme.bold` / `self.theme.italic` / etc., the
// same way pi's `this.theme.bold` / `this.theme.italic` works.
// The rendered byte structure (and the `style_prefix` extracted by
// `get_style_prefix` for the F41 inline-context machinery) lines up
// with pi.
//
// Visible output is unchanged — italic and color are both active
// for the inner text either way — but byte-for-byte parity matters
// for the inline-context style_prefix re-emit and for any future
// parity-vs-pi test that checks an exact byte boundary.

#[test]
fn apply_default_style_open_order_matches_pi_default_text_style() {
    // Pi's order: `color(text)` first, then `italic(...)` wraps it.
    // For our 2-field DefaultTextStyle that means the bytes open as
    // `\x1b[3m` (italic) THEN `\x1b[90m` (gray) — italic is outer,
    // so its open comes first in the byte stream. Before F47 the
    // order was `\x1b[90m` then `\x1b[3m`.
    let mut m = md_with_gray_italic("plain thinking text");
    let lines = m.render(80);
    let joined = lines.join("\n");

    let italic_open = joined
        .find("\x1b[3m")
        .expect("expected italic open in rendered output");
    let gray_open = joined
        .find("\x1b[90m")
        .expect("expected gray open in rendered output");

    assert!(
        italic_open < gray_open,
        "italic open must precede gray open in the byte stream after F47 \
         (italic_open = {}, gray_open = {}, joined = {:?})",
        italic_open,
        gray_open,
        joined,
    );
}

//
// Pi-tui's heading branch builds a heading-specific `InlineStyleContext`
// but does NOT thread `defaultTextStyle` (its analog of our
// `DefaultTextStyle`) through the heading body or wrap the rendered
// heading line. F45 removed our previous outer `apply_default_style(&styled)`
// wrap on the heading branch so we match pi's narrower scope: the
// default text style applies to paragraphs only.
//
// The tests below assert that a `Markdown` configured with both a
// pre-style and a heading renders the heading without any of the
// pre-style's escape codes. Paragraphs in the same document must still
// receive the pre-style (regression guard so the F45 fix doesn't
// accidentally drop pre-style application altogether).

#[test]
fn pre_styled_text_does_not_wrap_headings() {
    // F45 main fix: a heading rendered with `default_text_style` set must NOT
    // contain the pre-style's gray (`\x1b[90m`) or italic (`\x1b[3m`)
    // codes. Only the heading theme's codes should appear on the
    // heading line. Mirrors pi-tui's `applyDefaultStyle` scope, which
    // covers paragraphs/lists/tables but not headings.
    let mut m = md_with_gray_italic("# My Heading");
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(
        !joined.contains("\x1b[90m"),
        "heading rendered with default_text_style should not contain the \
         pre-style gray open (\\x1b[90m); got {:?}",
        joined,
    );
    assert!(
        !joined.contains("\x1b[3m"),
        "heading rendered with default_text_style should not contain the \
         pre-style italic open (\\x1b[3m); got {:?}",
        joined,
    );
    // Sanity: the heading body text must still be present.
    assert!(
        joined.contains("My Heading"),
        "heading body text must survive: {:?}",
        joined,
    );
}

#[test]
fn pre_styled_text_wraps_paragraphs_but_not_headings_in_same_document() {
    // F45 regression guard: in a mixed-block document, the paragraph
    // still receives pre-style (gray + italic) while the heading does
    // not. Without this test the F45 fix could silently degrade into
    // "pre-style is broken everywhere" and only the F45-specific test
    // above would still pass.
    let mut m = md_with_gray_italic("# Heading line\n\nParagraph line");
    let lines = m.render(80);

    let plain = plain_lines(&lines);
    let heading_idx = plain
        .iter()
        .position(|l| l.contains("Heading line"))
        .expect("expected a rendered row containing the heading text");
    let paragraph_idx = plain
        .iter()
        .position(|l| l.contains("Paragraph line"))
        .expect("expected a rendered row containing the paragraph text");

    let heading_line = &lines[heading_idx];
    let paragraph_line = &lines[paragraph_idx];

    // Heading: no pre-style codes.
    assert!(
        !heading_line.contains("\x1b[90m"),
        "heading line should not contain pre-style gray; got {:?}",
        heading_line,
    );
    assert!(
        !heading_line.contains("\x1b[3m"),
        "heading line should not contain pre-style italic; got {:?}",
        heading_line,
    );

    // Paragraph: pre-style codes still present.
    assert!(
        paragraph_line.contains("\x1b[90m"),
        "paragraph line should still contain pre-style gray; got {:?}",
        paragraph_line,
    );
    assert!(
        paragraph_line.contains("\x1b[3m"),
        "paragraph line should still contain pre-style italic; got {:?}",
        paragraph_line,
    );
}

// ---------------------------------------------------------------------------
// Blockquote inline-context machinery + applyQuoteStyle (PORTING.md F46)
// ---------------------------------------------------------------------------
//
// F46 ports pi-tui's `applyQuoteStyle` + `quoteInlineStyleContext`
// machinery so blockquote rendering matches pi byte-for-byte across
// three previously-tracked deltas:
//
// 1. **Italic underwrap.** `quote_apply(t) = theme.quote(theme.italic(t))`
//    so a custom non-italic `theme.quote` (e.g. cyan-only) still ships
//    italic-styled content. Tested by
//    `custom_non_italic_quote_theme_still_underwraps_with_italic`.
//
// 2. **`\x1b[0m` re-open splice.** Every full SGR reset inside a quoted
//    line is suffixed with `quote_prefix` so the quote/italic state
//    reopens for the line's tail. Tested by
//    `code_block_in_blockquote_reopens_quote_styling_after_syntect_reset`.
//
// 3. **Inline-context surfacing.** Paragraph inlines inside a quote
//    re-emit `quote_prefix` after each non-text inline (codespan, bold,
//    etc.) so the quote styling reopens for trailing text on the same
//    line, mirroring pi's `quoteInlineStyleContext`. Tested by
//    `paragraph_inline_inside_blockquote_reopens_quote_styling_after_inline_code`.

/// Build a markdown theme that mirrors `default_markdown_theme()` but
/// replaces `quote` with a non-italic cyan wrap so the F46 italic
/// underwrap is observable in the rendered output.
fn theme_with_cyan_quote() -> MarkdownTheme {
    MarkdownTheme {
        quote: Arc::new(style::cyan),
        ..default_markdown_theme()
    }
}

#[test]
fn custom_non_italic_quote_theme_still_underwraps_with_italic() {
    // F46 delta 1: pi's `quoteStyle` is
    // `(t) => theme.quote(theme.italic(t))` — `theme.italic` always
    // wraps under `theme.quote`. With a cyan-only `theme.quote` and a
    // plain `> hello` quote, both `\x1b[3m` (italic from the underwrap)
    // AND `\x1b[36m` (cyan from the quote) must appear in the rendered
    // output. The previous F12 path emitted only the cyan wrap.
    let theme = theme_with_cyan_quote();
    let mut m = Markdown::new("> hello", 0, 0, theme, None);
    let lines = m.render(80);
    let joined = lines.join("\n");

    assert!(
        joined.contains("\x1b[3m"),
        "blockquote with cyan-only theme.quote should still emit italic \
         from the F46 underwrap; got: {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b[36m"),
        "blockquote with cyan theme.quote should emit cyan; got: {:?}",
        joined,
    );

    // Sanity: the quoted body text survives the wrap.
    let plain = plain_lines(&lines);
    assert!(
        plain.iter().any(|l| l.contains("hello")),
        "expected the quote body text in the rendered output; got: {:?}",
        plain,
    );
}

#[test]
fn code_block_in_blockquote_reopens_quote_styling_after_syntect_reset() {
    // F46 delta 2: pi's `applyQuoteStyle` replaces every `\x1b[0m`
    // (full SGR reset, emitted by syntect's `as_24_bit_terminal_escaped`)
    // with `\x1b[0m{quote_prefix}` before wrapping with `quote_apply`.
    // The visible effect: a fenced code block inside a `>` quote keeps
    // the quote/italic styling open through the trailing cells of every
    // highlighted code row, instead of falling back to plain after the
    // syntect-emitted `\x1b[0m`.
    //
    // For our default theme (`theme.quote = style::italic`) the
    // quote_prefix is `\x1b[3m\x1b[3m` (the quote's italic plus the
    // underwrap's italic — both fire). Assert that the highlighted code
    // body line contains the splice byte sequence
    // `\x1b[0m\x1b[3m\x1b[3m`. Without F46 the line ended in `\x1b[0m`
    // followed by the quote-italic close (`\x1b[23m`) with no italic
    // re-open before the close.
    let mut m = md("> ```rust\n> let x = 1;\n> ```");
    let lines = m.render(80);

    let body_line = lines
        .iter()
        .find(|l| strip_ansi(l).contains("let x = 1"))
        .expect("expected a rendered row containing the code body");

    assert!(
        body_line.contains("\x1b[0m\x1b[3m\x1b[3m"),
        "code-block-in-blockquote line should splice the quote prefix \
         (\\x1b[3m\\x1b[3m) after every \\x1b[0m so the quote/italic \
         state reopens; got: {:?}",
        body_line,
    );
}

#[test]
fn paragraph_inline_inside_blockquote_reopens_quote_styling_after_inline_code() {
    // F46 delta 3: pi's `quoteInlineStyleContext` threads
    // `style_prefix = quote_prefix` through paragraph-inside-blockquote
    // inline rendering, so each non-text inline (codespan, bold, etc.)
    // is followed by a re-emit of the quote opens. With a cyan-only
    // `theme.quote` so the re-emit is observable as a `\x1b[36m` open
    // byte (default-theme's `\x1b[3m` would coincide with the
    // surrounding italic wrap and wash out the assertion):
    //
    //   `> See `code` for more`
    //   → identity-applied "See "
    //     + theme.code("code") = "\x1b[33mcode\x1b[39m"
    //     + quote_prefix re-emit "\x1b[36m\x1b[3m"
    //     + identity-applied " for more"
    //   → wrapped with apply_quote_style which prepends
    //     "\x1b[36m\x1b[3m" and appends "\x1b[23m\x1b[39m"
    //
    // After the inline code's `\x1b[39m` foreground reset, the
    // quote_prefix re-emit puts the cyan back on for " for more".
    // Without F46 the cyan was only at the outermost wrap and the
    // `\x1b[39m` from the inline code stripped it from the trailing
    // text on the same line.
    let theme = theme_with_cyan_quote();
    let mut m = Markdown::new("> See `code` for more", 0, 0, theme, None);
    let lines = m.render(80);

    let body_line = lines
        .iter()
        .find(|l| strip_ansi(l).contains("for more"))
        .expect("expected a rendered row containing the trailing text");

    // Locate the inline code's foreground close (`\x1b[39m`) and
    // assert that a fresh cyan open (`\x1b[36m`) follows it before the
    // line ends. Walking from the close avoids matching the outer wrap's
    // own cyan open at the line head.
    let close_idx = body_line
        .find("\x1b[39m")
        .expect("inline code should emit its own \\x1b[39m foreground reset");
    let after_close = &body_line[close_idx + "\x1b[39m".len()..];
    assert!(
        after_close.contains("\x1b[36m"),
        "inline-code reset should be followed by a quote-prefix cyan \
         re-open before the trailing text; got after-reset: {:?}",
        after_close,
    );
}

// ---------------------------------------------------------------------------
// List/table context forwarding inside blockquote (PORTING.md F46-followup)
// ---------------------------------------------------------------------------
//
// Pi-tui's `renderToken` blockquote arm threads `quoteInlineStyleContext`
// not just to direct paragraph/heading sub-blocks but also through
// `renderList` (markdown.ts:357) and `renderTable` (markdown.ts:365).
// Closes the F46 "list/table context-free" narrowing so a list or table
// inside a `>` quote re-emits the quote prefix after each non-text
// inline (codespan, bold, etc.) at the per-item / per-cell level.

#[test]
fn list_inside_blockquote_reopens_quote_styling_after_inline_code() {
    // With cyan-only theme.quote so the re-emit shows up as a fresh
    // \x1b[36m open after the inline code's \x1b[39m foreground reset.
    let theme = theme_with_cyan_quote();
    let mut m = Markdown::new(
        "> - item with `code` and trailing text\n> - second item",
        0,
        0,
        theme,
        None,
    );
    let lines = m.render(80);

    let item_line = lines
        .iter()
        .find(|l| strip_ansi(l).contains("trailing text"))
        .expect("expected a rendered row containing the first list item");

    // Locate the inline code's foreground close and walk forward.
    let close_idx = item_line
        .find("\x1b[39m")
        .expect("inline code should emit its own \\x1b[39m foreground reset");
    let after_close = &item_line[close_idx + "\x1b[39m".len()..];
    assert!(
        after_close.contains("\x1b[36m"),
        "list-item-inside-blockquote inline-code reset should be \
         followed by a quote-prefix cyan re-open before the trailing \
         text; got after-reset: {:?}",
        after_close,
    );
}

#[test]
fn table_inside_blockquote_reopens_quote_styling_after_inline_code_in_cell() {
    // Cell content has text after the inline code so the trailing
    // style_prefix trim doesn't strip the re-emit. With the cyan-only
    // theme.quote, the re-emit is observable as a fresh \x1b[36m open
    // after the inline code's \x1b[39m foreground reset.
    let theme = theme_with_cyan_quote();
    let mut m = Markdown::new(
        "> | A | B |\n> |---|---|\n> | `c` end | x |",
        0,
        0,
        theme,
        None,
    );
    let lines = m.render(80);

    let cell_line = lines
        .iter()
        .find(|l| strip_ansi(l).contains("end"))
        .expect("expected a rendered table data row containing the cell text");

    let close_idx = cell_line
        .find("\x1b[39m")
        .expect("inline code should emit its own \\x1b[39m foreground reset");
    let after_close = &cell_line[close_idx + "\x1b[39m".len()..];
    assert!(
        after_close.contains("\x1b[36m"),
        "table-cell-inside-blockquote inline-code reset should be \
         followed by a quote-prefix cyan re-open before the trailing \
         text; got after-reset: {:?}",
        after_close,
    );
}

// ---------------------------------------------------------------------------
// Blockquote pipeline order: trim / apply / wrap (PORTING.md F46-followup)
// ---------------------------------------------------------------------------
//
// Pi-tui's blockquote arm (markdown.ts:392-411) collects rendered
// sub-block lines, pops trailing blanks from the collected vec,
// then for each remaining line applies `applyQuoteStyle`, wraps with
// `wrapTextWithAnsi`, and prepends the border. Three observable
// consequences vs the previous F46 path:
//
// 1. Mid-quote blank rows go through `apply_quote_style("")` instead
//    of being rendered as bare borders. The visible result is the
//    same width (the wrap-empty closes don't add visible cells), but
//    the byte structure now matches pi.
// 2. The trailing-blank trim runs on the collected `quote_lines` vec
//    (a `String::is_empty` check) instead of on the bordered output
//    (a `visible_width <= border_width` check). Functionally
//    equivalent for our outputs but mirrors pi's order.
// 3. Sub-block lines that exceed `inner_width` (e.g. a wide code-block
//    row at a narrow render width) wrap with ANSI state propagation
//    instead of overflowing. Code-block rows aren't internally wrapped
//    by `render_block_in_context`, so the new outer wrap step picks
//    them up.

#[test]
fn mid_quote_blank_row_gets_empty_content_quote_wrap_not_bare_border() {
    // F46-followup #4: pi calls `applyQuoteStyle` on every line,
    // including blanks. A `> first\n>\n> second` source produces a
    // mid-quote blank row that pi renders as
    // `border + applyQuoteStyle("")` = `border + quote_apply("")`,
    // which under the default theme is
    // `\x1b[2m│ \x1b[22m\x1b[3m\x1b[3m\x1b[23m\x1b[23m`. The previous
    // F46 path emitted just the bare border `\x1b[2m│ \x1b[22m`.
    let mut m = md("> first\n>\n> second");
    let lines = m.render(80);

    // Three rows: first content, blank middle, second content.
    let plain = plain_lines(&lines);
    let bordered: Vec<&String> = plain.iter().filter(|l| l.starts_with("│ ")).collect();
    assert_eq!(
        bordered.len(),
        3,
        "expected 3 quoted rows, got: {:?}",
        plain,
    );

    // The middle row is the blank one — the row whose content is just
    // the border + empty-content quote wrap.
    let middle = lines
        .iter()
        .find(|l| {
            // Find a row whose plain-text content (after stripping
            // ANSI) is exactly the border `│ ` with no body.
            strip_ansi(l).trim_end() == "│"
        })
        .expect("expected a mid-quote blank row");

    // The middle row should carry the empty-content quote_apply wrap
    // (`\x1b[3m\x1b[3m\x1b[23m\x1b[23m` with the default theme)
    // immediately after the border. Without the F46-followup change
    // the row is just the bare border with no wrap escapes after it.
    assert!(
        middle.contains("\x1b[3m\x1b[3m"),
        "mid-quote blank row should carry the empty-content quote wrap; \
         got: {:?}",
        middle,
    );
    assert!(
        middle.contains("\x1b[23m\x1b[23m"),
        "mid-quote blank row should carry the empty-content quote close; \
         got: {:?}",
        middle,
    );
}

#[test]
fn long_code_block_line_inside_blockquote_wraps_with_quote_state_propagation() {
    // F46-followup #1: pi wraps after applyQuoteStyle so a code-block
    // line wider than `inner_width` breaks into multiple rows that
    // each carry the quote prefix (italic). Without the new wrap step
    // the long row would overflow past the border. The wrap is
    // exercised here with a fenced code line longer than `inner_width
    // = render_width - 2` (the border width).
    //
    // Use a deliberately-long line inside a code block at width 24
    // (inner_width = 22). The 26-char body must wrap into >= 2 rows
    // where every row sits inside the quote border.
    let mut m = md("> ```\n> abcdefghijklmnopqrstuvwxyz123\n> ```");
    let lines = m.render(24);

    // Every quoted row (those starting with the border) should fit
    // within the rendered width. A row that overflows past 24 cells
    // would mean wrap didn't happen.
    let bordered: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("\x1b[2m│ "))
        .collect();
    for row in &bordered {
        let plain = strip_ansi(row);
        let w = aj_tui::ansi::visible_width(&plain);
        assert!(
            w <= 24,
            "every bordered row must fit within render width 24; \
             got width {} on row {:?}",
            w,
            row,
        );
    }

    // The code body got long enough that it must have wrapped into at
    // least two body rows (excluding the ``` border rows). Body rows
    // are bordered rows whose plain content doesn't start with `│ ```
    // (they're indented code).
    let body_rows: Vec<&&String> = bordered
        .iter()
        .filter(|l| {
            let p = strip_ansi(l);
            !p.contains("```")
        })
        .collect();
    assert!(
        body_rows.len() >= 2,
        "expected at least 2 wrapped code body rows; got: {:?}",
        body_rows,
    );
}

#[test]
fn trailing_blank_inside_blockquote_is_trimmed_before_apply_quote_style() {
    // F46-followup #3: pi pops trailing empty strings from the
    // collected `renderedQuoteLines` vec BEFORE applying the per-line
    // quote style. So a sub-block's terminal `String::new()` spacer
    // doesn't become a wrapped-empty row at the tail of the quote.
    //
    // Sanity: a single-paragraph quote followed by a sibling paragraph
    // should produce exactly one quoted row + one blank separator row
    // + the sibling. No trailing wrapped-empty quote row.
    let mut m = md("> first\n\nsecond");
    let lines = m.render(80);
    let plain = plain_lines(&lines);

    // Find the index of the quoted row and the sibling.
    let quote_idx = plain
        .iter()
        .position(|l| l.starts_with("│ ") && l.contains("first"))
        .expect("expected a quoted `first` row");
    let sibling_idx = plain
        .iter()
        .position(|l| l.contains("second") && !l.starts_with("│ "))
        .expect("expected a `second` row outside the quote");

    // Exactly one row between them — the inter-block blank — and that
    // blank is OUTSIDE the quote (no border).
    assert_eq!(
        sibling_idx - quote_idx,
        2,
        "expected exactly one separator row between quote and sibling; \
         got plain lines: {:?}",
        plain,
    );
    let separator = &plain[quote_idx + 1];
    assert!(
        separator.is_empty() || !separator.starts_with("│ "),
        "separator row between quote and sibling should not carry the \
         quote border; got: {:?}",
        separator,
    );
}
