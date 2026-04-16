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
//!
//! Mirrors pi-tui's `packages/tui/test/test-themes.ts`. The tui crate is
//! palette-agnostic by design (no upstream `Default` themes), so every
//! integration test threads a theme through these helpers.

#![allow(dead_code)]

use std::sync::Arc;

use aj_tui::components::editor::EditorTheme;
use aj_tui::components::markdown::MarkdownTheme;
use aj_tui::components::select_list::SelectListTheme;
use aj_tui::components::settings_list::SettingsListTheme;
use aj_tui::style;

/// A select-list theme matching the default set of styling functions used in
/// framework tests: blue prefix, bold selected text, dim for the rest.
pub fn default_select_list_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: Arc::new(style::blue),
        selected_text: Arc::new(style::bold),
        description: Arc::new(style::dim),
        scroll_info: Arc::new(style::dim),
        no_match: Arc::new(style::dim),
    }
}

/// A markdown theme used by framework tests. Pairs bold+cyan headings with
/// yellow inline code and dim code-block borders.
///
/// OSC 8 hyperlink emission is gated on the process-wide capabilities
/// cache (`aj_tui::capabilities::get_capabilities().hyperlinks`),
/// matching pi-tui's `markdown.ts:492` shape. Tests that need a
/// specific cap state should call
/// [`aj_tui::capabilities::set_capabilities`] under
/// `#[serial_test::serial]`.
pub fn default_markdown_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: Arc::new(|s| style::bold(&style::cyan(s))),
        link: Arc::new(style::blue),
        link_url: Arc::new(style::dim),
        code: Arc::new(style::yellow),
        code_block: Arc::new(style::green),
        code_block_border: Arc::new(style::dim),
        quote: Arc::new(style::italic),
        quote_border: Arc::new(style::dim),
        hr: Arc::new(style::dim),
        list_bullet: Arc::new(style::cyan),
        bold: Arc::new(style::bold),
        italic: Arc::new(style::italic),
        strikethrough: Arc::new(style::strikethrough),
        underline: Arc::new(style::underline),
        highlight_code: None,
        code_block_indent: None,
    }
}

/// An editor theme paired with the default select-list fixture for its
/// embedded autocomplete lists.
pub fn default_editor_theme() -> EditorTheme {
    EditorTheme {
        border_color: Arc::new(style::dim),
        select_list: default_select_list_theme(),
    }
}

/// A select-list theme whose closures all return their input verbatim. Use
/// for structural tests that shouldn't have to strip ANSI from the output.
pub fn identity_select_list_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: Arc::new(|s| s.to_string()),
        selected_text: Arc::new(|s| s.to_string()),
        description: Arc::new(|s| s.to_string()),
        scroll_info: Arc::new(|s| s.to_string()),
        no_match: Arc::new(|s| s.to_string()),
    }
}

/// A markdown theme whose closures all return their input verbatim. Use for
/// structural tests that shouldn't have to strip ANSI from the output.
pub fn identity_markdown_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: Arc::new(|s| s.to_string()),
        link: Arc::new(|s| s.to_string()),
        link_url: Arc::new(|s| s.to_string()),
        code: Arc::new(|s| s.to_string()),
        code_block: Arc::new(|s| s.to_string()),
        code_block_border: Arc::new(|s| s.to_string()),
        quote: Arc::new(|s| s.to_string()),
        quote_border: Arc::new(|s| s.to_string()),
        hr: Arc::new(|s| s.to_string()),
        list_bullet: Arc::new(|s| s.to_string()),
        bold: Arc::new(|s| s.to_string()),
        italic: Arc::new(|s| s.to_string()),
        strikethrough: Arc::new(|s| s.to_string()),
        underline: Arc::new(|s| s.to_string()),
        highlight_code: None,
        code_block_indent: None,
    }
}

/// An editor theme whose closures all return their input verbatim. Use for
/// structural tests that shouldn't have to strip ANSI from the output.
pub fn identity_editor_theme() -> EditorTheme {
    EditorTheme {
        border_color: Arc::new(|s| s.to_string()),
        select_list: identity_select_list_theme(),
    }
}

/// A settings-list theme used by framework tests. Bold-on-selected for
/// labels, dim for everything else, and a `"→ "` cursor.
pub fn default_settings_list_theme() -> SettingsListTheme {
    SettingsListTheme {
        label: Arc::new(|s, selected| {
            if selected {
                style::bold(s)
            } else {
                s.to_string()
            }
        }),
        value: Arc::new(|s, _selected| style::dim(s)),
        description: Arc::new(style::dim),
        hint: Arc::new(style::dim),
        cursor: "→ ".to_string(),
    }
}

/// A settings-list theme whose closures all return their input verbatim.
/// Use for structural tests that shouldn't have to strip ANSI from the
/// output.
pub fn identity_settings_list_theme() -> SettingsListTheme {
    SettingsListTheme {
        label: Arc::new(|s, _selected| s.to_string()),
        value: Arc::new(|s, _selected| s.to_string()),
        description: Arc::new(|s| s.to_string()),
        hint: Arc::new(|s| s.to_string()),
        cursor: "→ ".to_string(),
    }
}
