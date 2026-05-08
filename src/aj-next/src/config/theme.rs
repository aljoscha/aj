//! Theme — palette + style closure builders for the interactive
//! mode's components.
//!
//! Each `*_theme()` function returns a freshly-constructed theme
//! struct from `aj-tui` populated with closures from
//! [`aj_tui::style`]. The interactive mode hands these themes to
//! the [`Markdown`](aj_tui::components::markdown::Markdown),
//! [`Editor`](aj_tui::components::editor::Editor), and
//! [`SelectList`](aj_tui::components::select_list::SelectListTheme)
//! components on construction.
//!
//! `aj-tui` is intentionally palette-agnostic (no `Default` impl
//! for any of the theme types) so the host has to assemble these
//! itself. Centralising the assembly in this module keeps every
//! component renderer using the same colours; the
//! "Selectors and theming" Phase 1 step replaces the inline closures
//! with a configurable palette layer.

use std::sync::Arc;

use aj_tui::components::editor::EditorTheme;
use aj_tui::components::markdown::MarkdownTheme;
use aj_tui::components::select_list::SelectListTheme;
use aj_tui::style;

/// Theme used by the autocomplete pop-up and any future selector
/// overlays. Cyan highlights for the focused entry, dim
/// descriptions and scroll markers.
pub fn select_list_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: Arc::new(style::cyan),
        selected_text: Arc::new(|s| style::bold(&style::cyan(s))),
        description: Arc::new(style::dim),
        scroll_info: Arc::new(style::dim),
        no_match: Arc::new(style::dim),
    }
}

/// Theme for the prompt editor at the bottom of the layout.
///
/// The border picks up the dim style so the editor doesn't compete
/// with the chat scrollback for visual weight; the autocomplete
/// pop-up shares the [`select_list_theme`].
pub fn editor_theme() -> EditorTheme {
    EditorTheme {
        border_color: Arc::new(style::dim),
        select_list: select_list_theme(),
    }
}

/// Theme used for rendering assistant messages and replayed user
/// input through [`aj_tui::components::markdown::Markdown`]. Code
/// blocks render with the bundled syntect highlighter (we leave
/// `highlight_code = None`); inline code is yellow, links are cyan
/// with a dim trailing URL, and headings stay bold-only so a
/// terminal that lacks colour support still has hierarchy.
pub fn markdown_theme() -> MarkdownTheme {
    MarkdownTheme {
        heading: Arc::new(style::bold),
        bold: Arc::new(style::bold),
        italic: Arc::new(style::italic),
        strikethrough: Arc::new(style::strikethrough),
        code: Arc::new(style::yellow),
        // Identity for the code-block body; the syntect-backed
        // highlighter (when `highlight_code` is `None`) takes care
        // of per-token colouring inside the block, so we don't want
        // an outer wrapper that would interfere with its SGR resets.
        code_block: Arc::new(|s| s.to_string()),
        code_block_border: Arc::new(style::dim),
        link: Arc::new(style::cyan),
        link_url: Arc::new(style::dim),
        list_bullet: Arc::new(style::cyan),
        quote_border: Arc::new(style::dim),
        quote: Arc::new(style::italic),
        hr: Arc::new(style::dim),
        underline: Arc::new(style::underline),
        highlight_code: None,
        code_block_indent: None,
    }
}
