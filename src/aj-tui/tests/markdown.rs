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

use aj_tui::component::Component;
use aj_tui::components::markdown::{Markdown, PreStyle};
use aj_tui::style;
use aj_tui::tui::Tui;

use support::themes::default_markdown_theme;
use support::{VirtualTerminal, plain_lines, plain_lines_trim_end, strip_ansi, wait_for_render};

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
    let mut m = Markdown::new(text);
    m.set_padding_x(padding_x);
    m.set_padding_y(padding_y);
    m.set_theme(default_markdown_theme());
    m
}

/// Build a `Markdown` with hyperlinks (OSC 8) enabled.
fn md_with_hyperlinks(text: &str) -> Markdown {
    let mut m = md(text);
    m.set_hyperlinks(true);
    m
}

/// Build a `Markdown` with an outer italic + gray pre-style, mimicking
/// how a "thinking trace" paragraph is rendered in practice.
fn md_with_gray_italic(text: &str) -> Markdown {
    let mut m = md(text);
    m.set_pre_style(Some(PreStyle {
        color: Some(Box::new(|s| style::gray(s))),
        italic: true,
    }));
    m
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

    tui.root.add_child(Box::new(m));
    tui.root
        .add_child(Box::new(support::StaticLines::new(["INPUT"])));
    wait_for_render(&mut tui);

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

    // `should not be optional` is the text after the inline-code span.
    // The chunk immediately preceding it must re-apply the heading
    // styling (bold + cyan) so the rest of the heading isn't rendered
    // unstyled.
    let after = joined
        .find("should not be optional")
        .expect("should contain text after the inline code");
    let start = after.saturating_sub(40);
    let preceding = &joined[start..after];

    assert!(
        preceding.contains("\x1b[1m"),
        "should re-apply bold before text after code: {:?}",
        preceding,
    );
    assert!(
        preceding.contains("\x1b[36m"),
        "should re-apply cyan before text after code: {:?}",
        preceding,
    );
}

#[test]
fn preserves_heading_styling_after_inline_code_h1() {
    let mut m = md("# Title with `code` inside");
    let lines = m.render(80);
    let joined = lines.join("\n");

    let after = joined
        .find("inside")
        .expect("should contain text after the inline code");
    let start = after.saturating_sub(40);
    let preceding = &joined[start..after];

    // H1 uses heading + underline + bold.
    assert!(
        preceding.contains("\x1b[1m"),
        "should re-apply bold for h1: {:?}",
        preceding,
    );
    assert!(
        preceding.contains("\x1b[36m"),
        "should re-apply cyan for h1: {:?}",
        preceding,
    );
    assert!(
        preceding.contains("\x1b[4m"),
        "should re-apply underline for h1: {:?}",
        preceding,
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

    tui.root.add_child(Box::new(m));
    wait_for_render(&mut tui);

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

    let after = joined
        .find("and more")
        .expect("should contain text after the bold span");
    let start = after.saturating_sub(40);
    let preceding = &joined[start..after];

    assert!(
        preceding.contains("\x1b[1m"),
        "should re-apply bold for h2: {:?}",
        preceding,
    );
    assert!(
        preceding.contains("\x1b[36m"),
        "should re-apply cyan for h2: {:?}",
        preceding,
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

#[test]
fn shows_url_in_parentheses_when_hyperlinks_are_not_supported() {
    let mut m = md("[click here](https://example.com)");
    let lines = m.render(80);
    let plain = plain_lines(&lines).join(" ");

    assert!(plain.contains("click here"), "should contain link text");
    assert!(
        plain.contains("(https://example.com)"),
        "should show URL in parentheses",
    );
}

#[test]
fn shows_mailto_url_in_parentheses_when_hyperlinks_are_not_supported() {
    let mut m = md("[Email me](mailto:test@example.com)");
    let lines = m.render(80);
    let plain = plain_lines(&lines).join(" ");

    assert!(plain.contains("Email me"), "should contain link text");
    assert!(
        plain.contains("(mailto:test@example.com)"),
        "should show mailto URL in parentheses",
    );
}

#[test]
fn emits_osc_8_hyperlink_sequence_when_terminal_supports_hyperlinks() {
    let mut m = md_with_hyperlinks("[click here](https://example.com)");
    let lines = m.render(80);
    let joined = lines.join("");

    assert!(
        joined.contains("\x1b]8;;https://example.com\x1b\\"),
        "should contain OSC 8 open sequence",
    );
    assert!(
        joined.contains("\x1b]8;;\x1b\\"),
        "should contain OSC 8 close sequence",
    );
}

#[test]
fn uses_osc_8_for_mailto_links_when_terminal_supports_hyperlinks() {
    let mut m = md_with_hyperlinks("[Email me](mailto:test@example.com)");
    let lines = m.render(80);
    let joined = lines.join("");

    assert!(
        joined.contains("\x1b]8;;mailto:test@example.com\x1b\\"),
        "should contain OSC 8 open with mailto URL",
    );
    assert!(joined.contains("\x1b]8;;\x1b\\"), "should have OSC 8 close");
}

#[test]
fn uses_osc_8_for_bare_urls_when_terminal_supports_hyperlinks() {
    let mut m = md_with_hyperlinks("Visit https://example.com for more");
    let lines = m.render(80);
    let joined = lines.join("");

    assert!(
        joined.contains("\x1b]8;;https://example.com\x1b\\"),
        "should contain OSC 8 hyperlink",
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
