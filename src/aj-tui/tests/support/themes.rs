//! Shared theme fixtures for tests.
//!
//! Each factory here returns an instance of a real theme type exported by
//! `aj_tui`. They come in two flavors:
//!
//! - `default_*`: applies the canonical styling used in framework tests,
//!   built on top of [`aj_tui::style`]. Use when a test wants to assert on
//!   styling (bold, color, dim, etc.).
//! - `identity_*`: returns closures that pass strings through verbatim.
//!   Use when the test cares about structure/layout, not styling — you get
//!   the real theme type shape without needing to strip ANSI codes from
//!   the output.

#![allow(dead_code)]

use aj_tui::components::editor::EditorTheme;
use aj_tui::components::markdown::MarkdownTheme;
use aj_tui::components::select_list::SelectListTheme;
use aj_tui::style;

/// A select-list theme matching the default set of styling functions used in
/// framework tests: blue prefix, bold selected text, dim for the rest.
pub fn default_select_list_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: Box::new(|s| style::blue(s)),
        selected_text: Box::new(|s| style::bold(s)),
        description: Box::new(|s| style::dim(s)),
        scroll_info: Box::new(|s| style::dim(s)),
        no_match: Box::new(|s| style::dim(s)),
    }
}

/// A markdown theme used by framework tests. Pairs bold+cyan headings with
/// yellow inline code and dim code-block borders.
///
/// `hyperlinks` is explicitly set to `false` so tests that don't care
/// about OSC 8 don't have to strip the sequence from their
/// assertions. Tests that exercise the hyperlink path should flip the
/// flag directly on the returned theme.
pub fn default_markdown_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: Box::new(|s| style::bold(&style::cyan(s))),
        link: Box::new(|s| style::blue(s)),
        link_url: Box::new(|s| style::dim(s)),
        code: Box::new(|s| style::yellow(s)),
        code_block: Box::new(|s| style::green(s)),
        code_block_border: Box::new(|s| style::dim(s)),
        quote: Box::new(|s| style::italic(s)),
        quote_border: Box::new(|s| style::dim(s)),
        hr: Box::new(|s| style::dim(s)),
        list_bullet: Box::new(|s| style::cyan(s)),
        bold: Box::new(|s| style::bold(s)),
        italic: Box::new(|s| style::italic(s)),
        strikethrough: Box::new(|s| style::strikethrough(s)),
        underline: Box::new(|s| style::underline(s)),
        hyperlinks: false,
        highlight_code: None,
        code_block_indent: None,
    }
}

/// An editor theme paired with the default select-list fixture for its
/// embedded autocomplete lists.
pub fn default_editor_theme() -> EditorTheme {
    EditorTheme {
        border_color: Box::new(|s| style::dim(s)),
        select_list: default_select_list_theme(),
    }
}

/// A select-list theme whose closures all return their input verbatim. Use
/// for structural tests that shouldn't have to strip ANSI from the output.
pub fn identity_select_list_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: Box::new(|s| s.to_string()),
        selected_text: Box::new(|s| s.to_string()),
        description: Box::new(|s| s.to_string()),
        scroll_info: Box::new(|s| s.to_string()),
        no_match: Box::new(|s| s.to_string()),
    }
}

/// A markdown theme whose closures all return their input verbatim. Use for
/// structural tests that shouldn't have to strip ANSI from the output.
pub fn identity_markdown_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: Box::new(|s| s.to_string()),
        link: Box::new(|s| s.to_string()),
        link_url: Box::new(|s| s.to_string()),
        code: Box::new(|s| s.to_string()),
        code_block: Box::new(|s| s.to_string()),
        code_block_border: Box::new(|s| s.to_string()),
        quote: Box::new(|s| s.to_string()),
        quote_border: Box::new(|s| s.to_string()),
        hr: Box::new(|s| s.to_string()),
        list_bullet: Box::new(|s| s.to_string()),
        bold: Box::new(|s| s.to_string()),
        italic: Box::new(|s| s.to_string()),
        strikethrough: Box::new(|s| s.to_string()),
        underline: Box::new(|s| s.to_string()),
        hyperlinks: false,
        highlight_code: None,
        code_block_indent: None,
    }
}

/// An editor theme whose closures all return their input verbatim. Use for
/// structural tests that shouldn't have to strip ANSI from the output.
pub fn identity_editor_theme() -> EditorTheme {
    EditorTheme {
        border_color: Box::new(|s| s.to_string()),
        select_list: identity_select_list_theme(),
    }
}
