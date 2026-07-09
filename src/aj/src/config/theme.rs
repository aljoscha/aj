//! Theme rendering for the aj-tui interactive mode.
//!
//! The backend-neutral palette (the [`Theme`] struct, the semantic
//! [`ThemeColor`] / [`ThemeBg`] tokens, the JSON loader, the file
//! watcher, and the shared [`ThemeHandle`]) lives in
//! [`aj_app::theme`] and is re-exported here so existing
//! `crate::config::theme::...` call sites keep resolving.
//!
//! What lives here is aj's rendering half: the ANSI SGR encoding of a
//! [`ThemeRgb`] (with 256-color downsampling for limited terminals),
//! the [`ThemeExt`] / [`ThemeHandleExt`] extension traits that wrap
//! text in those escapes, and the builders that turn a palette into
//! the `aj-tui` component theme structs ([`MarkdownTheme`],
//! [`EditorTheme`], [`SelectListTheme`], [`ChatTheme`], …).
//!
//! ## ANSI encoding
//!
//! A frontend that paints into an ANSI terminal bakes each
//! [`ThemeRgb`] into an SGR prefix at render time. Truecolor
//! terminals get the 24-bit triple directly. On a 256-color terminal
//! [`fg_ansi`] / [`bg_ansi`] downsample the hex value to the closest
//! entry in the xterm 256-color palette (see [`rgb_to_256`]). A
//! [`ThemeRgb::Default`] token encodes as the terminal's own default
//! (`\x1b[39m` / `\x1b[49m`), and a [`ThemeRgb::Ansi256`] index passes
//! straight through.

use std::sync::Arc;

use aj_models::ThinkingConfig;
use aj_tui::components::editor::EditorTheme;
use aj_tui::components::markdown::{MarkdownTheme, SyntaxStyles};
use aj_tui::components::select_list::SelectListTheme;
use aj_tui::style;

// The palette and its loader / watcher / handle are frontend-agnostic
// and live in `aj_app::theme`. Re-exported so `aj` code keeps saying
// `crate::config::theme::{...}` and so the extension traits and
// builders below refer to the types by bare name.
pub use aj_app::theme::{
    ColorMode, Theme, ThemeBg, ThemeColor, ThemeError, ThemeHandle, ThemeRgb, ThemeWatcherGuard,
    rgb_to_256, watch_user_theme,
};

// ============================================================================
// ANSI encoding — ThemeRgb -> SGR escape
// ============================================================================

// The RGB -> 256-color downsampling (`rgb_to_256`) lives in
// `aj_app::theme` and is re-exported above, since both frontends
// downsample the same palette and the mapping must agree.

/// Encode a foreground [`ThemeRgb`] as an SGR prefix. `Default`
/// yields the terminal's default foreground. An explicit palette
/// index passes through. A hex triple is emitted verbatim on
/// truecolor terminals and downsampled to the 256-color palette
/// otherwise.
fn fg_ansi(color: ThemeRgb, mode: ColorMode) -> String {
    match (color, mode) {
        (ThemeRgb::Default, _) => "\x1b[39m".to_string(),
        (ThemeRgb::Ansi256(i), _) => format!("\x1b[38;5;{i}m"),
        (ThemeRgb::Rgb(r, g, b), ColorMode::Truecolor) => {
            format!("\x1b[38;2;{r};{g};{b}m")
        }
        (ThemeRgb::Rgb(r, g, b), ColorMode::Color256) => {
            let idx = rgb_to_256(r, g, b);
            format!("\x1b[38;5;{idx}m")
        }
    }
}

/// Encode a background [`ThemeRgb`] as an SGR prefix. Same rules as
/// [`fg_ansi`] but targeting the background SGR channel (`48`) and a
/// `Default` background reset (`\x1b[49m`).
fn bg_ansi(color: ThemeRgb, mode: ColorMode) -> String {
    match (color, mode) {
        (ThemeRgb::Default, _) => "\x1b[49m".to_string(),
        (ThemeRgb::Ansi256(i), _) => format!("\x1b[48;5;{i}m"),
        (ThemeRgb::Rgb(r, g, b), ColorMode::Truecolor) => {
            format!("\x1b[48;2;{r};{g};{b}m")
        }
        (ThemeRgb::Rgb(r, g, b), ColorMode::Color256) => {
            let idx = rgb_to_256(r, g, b);
            format!("\x1b[48;5;{idx}m")
        }
    }
}

/// Re-emit a background SGR `prefix` after every escape in `text`
/// that clears the background: `\x1b[0m` (full reset) and `\x1b[49m`
/// (bg reset). Without this, embedded resets would drop the row tint
/// for the rest of the line.
fn reassert_bg(prefix: &str, text: &str) -> String {
    if prefix.is_empty() {
        return text.to_string();
    }
    text.replace("\x1b[0m", &format!("\x1b[0m{prefix}"))
        .replace("\x1b[49m", &format!("\x1b[49m{prefix}"))
}

// ============================================================================
// Extension traits: ANSI painting over the shared palette
// ============================================================================

/// aj-tui ANSI painting over a shared [`Theme`].
///
/// The palette stores structured [`ThemeRgb`] colors; these methods
/// bake them into SGR escapes so aj-tui components can paste the
/// result into a `Line`. Implemented for [`aj_app::theme::Theme`]
/// rather than defined on it because the ANSI contract is
/// backend-specific.
pub trait ThemeExt {
    /// Wrap `text` in the SGR escape for the given foreground token
    /// plus the matching reset. The reset is `\x1b[39m` (default
    /// foreground) so a nested style isn't disturbed.
    fn fg(&self, token: ThemeColor, text: &str) -> String;

    /// Wrap `text` in the SGR escape for the given background token
    /// plus the matching reset.
    ///
    /// The background is re-asserted after any embedded escape that
    /// clears it, so `text` may carry full SGR resets (e.g. syntect-
    /// highlighted code lines end in `\x1b[0m`) without punching
    /// holes in a tinted row.
    fn bg(&self, token: ThemeBg, text: &str) -> String;

    /// Build a closure that applies the given foreground token to
    /// arbitrary text. Captures the resolved color and color mode so
    /// the returned closure is `'static`.
    fn fg_closure(&self, token: ThemeColor) -> Arc<dyn Fn(&str) -> String>;

    /// Build a closure that applies the given background token to
    /// arbitrary text. Same embedded-reset handling as [`Self::bg`].
    fn bg_closure(&self, token: ThemeBg) -> Arc<dyn Fn(&str) -> String>;
}

impl ThemeExt for Theme {
    fn fg(&self, token: ThemeColor, text: &str) -> String {
        format!(
            "{}{text}\x1b[39m",
            fg_ansi(self.fg_color(token), self.color_mode())
        )
    }

    fn bg(&self, token: ThemeBg, text: &str) -> String {
        let prefix = bg_ansi(self.bg_color(token), self.color_mode());
        format!("{prefix}{}\x1b[49m", reassert_bg(&prefix, text))
    }

    fn fg_closure(&self, token: ThemeColor) -> Arc<dyn Fn(&str) -> String> {
        // Both are `Copy`, so the closure owns them and outlives the
        // borrow of `self`.
        let color = self.fg_color(token);
        let mode = self.color_mode();
        Arc::new(move |s: &str| format!("{}{s}\x1b[39m", fg_ansi(color, mode)))
    }

    fn bg_closure(&self, token: ThemeBg) -> Arc<dyn Fn(&str) -> String> {
        let color = self.bg_color(token);
        let mode = self.color_mode();
        Arc::new(move |s: &str| {
            let prefix = bg_ansi(color, mode);
            format!("{prefix}{}\x1b[49m", reassert_bg(&prefix, s))
        })
    }
}

/// Hot-rebinding ANSI painting closures over a shared [`ThemeHandle`].
///
/// Unlike [`ThemeExt::fg_closure`], these resolve through the shared
/// handle on each call, so an in-place [`ThemeHandle::replace`]
/// reskins them without reconstructing the widget that holds them.
pub trait ThemeHandleExt {
    /// Build a foreground-painting closure that re-reads the live
    /// palette on each call.
    fn fg_closure(&self, token: ThemeColor) -> Arc<dyn Fn(&str) -> String>;

    /// Build a background-painting closure with the same hot-rebind
    /// semantics as [`Self::fg_closure`].
    fn bg_closure(&self, token: ThemeBg) -> Arc<dyn Fn(&str) -> String>;
}

impl ThemeHandleExt for ThemeHandle {
    fn fg_closure(&self, token: ThemeColor) -> Arc<dyn Fn(&str) -> String> {
        let handle = self.clone();
        // The `aj-tui` theme structs hold `Arc<dyn Fn(&str) -> String>`
        // without `Send + Sync` bounds (the TUI thread is the only
        // consumer). The closure itself is `Send + Sync` (it captures
        // a `ThemeHandle`, which is), but the trait object's bounds are
        // what clippy checks, so silence it here.
        #[allow(clippy::arc_with_non_send_sync)]
        let closure: Arc<dyn Fn(&str) -> String> =
            Arc::new(move |s: &str| handle.read().fg(token, s));
        closure
    }

    fn bg_closure(&self, token: ThemeBg) -> Arc<dyn Fn(&str) -> String> {
        let handle = self.clone();
        #[allow(clippy::arc_with_non_send_sync)]
        let closure: Arc<dyn Fn(&str) -> String> =
            Arc::new(move |s: &str| handle.read().bg(token, s));
        closure
    }
}

// ============================================================================
// aj-tui theme builders
// ============================================================================

/// Build the [`SelectListTheme`] used by the autocomplete pop-up
/// and the selector overlays. Routes the five rendering closures
/// through the matching semantic tokens.
pub fn select_list_theme(theme: &ThemeHandle) -> SelectListTheme {
    let accent = theme.fg_closure(ThemeColor::Accent);
    let accent_for_bold = Arc::clone(&accent);
    // The `aj-tui` theme structs hold `Arc<dyn Fn(&str) -> String>`
    // without `Send + Sync` bounds, so this composed closure is
    // intentionally non-thread-shareable. We still use `Arc` to
    // match the surrounding API; `Rc` would force a divergent
    // shape.
    #[allow(clippy::arc_with_non_send_sync)]
    let bold_accent: Arc<dyn Fn(&str) -> String> =
        Arc::new(move |s: &str| style::bold(&accent_for_bold(s)));
    SelectListTheme {
        selected_prefix: accent,
        selected_text: bold_accent,
        description: theme.fg_closure(ThemeColor::Muted),
        scroll_info: theme.fg_closure(ThemeColor::Muted),
        no_match: theme.fg_closure(ThemeColor::Muted),
        prefix: theme.fg_closure(ThemeColor::Dim),
        shortcut: theme.fg_closure(ThemeColor::Accent),
    }
}

/// Build the [`aj_tui::components::settings_list::SettingsListTheme`]
/// used by the `/settings` overlay. Mirrors the select-list palette:
/// accent for the selected row (bold label, plain accent value),
/// muted descriptions and hints.
pub fn settings_list_theme(
    theme: &ThemeHandle,
) -> aj_tui::components::settings_list::SettingsListTheme {
    let accent = theme.fg_closure(ThemeColor::Accent);
    let accent_for_label = Arc::clone(&accent);
    let accent_for_marker = Arc::clone(&accent);
    let muted = theme.fg_closure(ThemeColor::Muted);
    // The `aj-tui` theme structs hold `Arc<dyn Fn(&str) -> String>`
    // without `Send + Sync` bounds; match the surrounding API by
    // using `Arc` for the composed closures as well.
    #[allow(clippy::arc_with_non_send_sync)]
    let label: Arc<dyn Fn(&str, bool) -> String> = Arc::new(move |s: &str, selected: bool| {
        if selected {
            style::bold(&accent_for_label(s))
        } else {
            s.to_string()
        }
    });
    #[allow(clippy::arc_with_non_send_sync)]
    let value: Arc<dyn Fn(&str, bool) -> String> =
        Arc::new(
            move |s: &str, selected: bool| {
                if selected { accent(s) } else { muted(s) }
            },
        );
    aj_tui::components::settings_list::SettingsListTheme {
        label,
        value,
        description: theme.fg_closure(ThemeColor::Muted),
        hint: theme.fg_closure(ThemeColor::Dim),
        // The override marker (project window) reuses the accent so a
        // set row's glyph matches the selected-row accent palette.
        marker: accent_for_marker,
        // Two columns wide so unselected rows' two-space gutter
        // lines up; matches the component's own row layout.
        cursor: "→ ".to_string(),
    }
}

/// Build the overlay-window chrome theme used by the command palette
/// and the model / thinking / session selectors. The border picks up
/// [`ThemeColor::BorderMuted`] (light grey, matches the editor's
/// resting border tint); the title uses [`ThemeColor::Accent`] in
/// bold to mirror the screenshot palette.
pub fn overlay_window_theme(
    theme: &ThemeHandle,
) -> aj_tui::components::overlay_window::OverlayWindowTheme {
    let accent = theme.fg_closure(ThemeColor::Accent);
    // The `aj-tui` theme structs hold `Arc<dyn Fn(&str) -> String>`
    // without `Send + Sync` bounds; match the surrounding API by
    // using `Arc` for the composed closure as well.
    #[allow(clippy::arc_with_non_send_sync)]
    let title: Arc<dyn Fn(&str) -> String> = Arc::new(move |s: &str| style::bold(&accent(s)));
    aj_tui::components::overlay_window::OverlayWindowTheme {
        border: theme.fg_closure(ThemeColor::BorderMuted),
        title,
        subtitle: theme.fg_closure(ThemeColor::Dim),
    }
}

/// Build the [`EditorTheme`] for the bottom-of-layout prompt
/// editor. The border picks up [`ThemeColor::BorderMuted`] by
/// default; the host can override per-frame via
/// [`aj_tui::editor_component::EditorComponent::set_border_color`]
/// to surface thinking-level / bash-mode tints.
pub fn editor_theme(theme: &ThemeHandle) -> EditorTheme {
    EditorTheme {
        border_color: theme.fg_closure(ThemeColor::BorderMuted),
        select_list: select_list_theme(theme),
    }
}

/// Bundle of styling primitives shared by every chat-scrollback
/// component (user messages, assistant messages, tool executions,
/// notices, …). Carries the [`MarkdownTheme`] used for rich-text
/// rendering plus the precomputed background-paint closures that
/// individual components need to tint their bubbles.
///
/// Built once per session via [`chat_theme`] and threaded through
/// the [`crate::modes::interactive::event_pump::EventPump`]. Cheap
/// to [`Clone`] — every field is either a [`Clone`] struct of
/// `Arc<dyn Fn>` closures or an `Arc` itself.
#[derive(Clone)]
pub struct ChatTheme {
    /// Foreground / styling theme passed to every [`aj_tui::components::markdown::Markdown`]
    /// widget the chat renders.
    pub markdown: MarkdownTheme,
    /// Foreground colour applied to thinking-channel content.
    /// Drives both the expanded mode (the [`aj_tui::components::markdown::Markdown`]
    /// widget's [`aj_tui::components::markdown::DefaultTextStyle::color`])
    /// and the collapsed-mode `Thinking…` placeholder line. Sharing
    /// one closure keeps the two render paths visually consistent
    /// and makes a theme reload reskin both at once.
    pub thinking_text: Arc<dyn Fn(&str) -> String>,
    /// Background-paint closure for the user-message bubble. Wraps
    /// each rendered row through [`ThemeExt::bg`] with the
    /// [`ThemeBg::UserMessageBg`] palette token so the bubble's
    /// inset rectangle reads as a single tinted block.
    pub user_message_bg: Arc<dyn Fn(&str) -> String>,
    /// Tool-execution bubble tint while the call is in-flight.
    /// Drives the rectangle the [`super::super::modes::interactive::components::tool_execution::ToolExecutionComponent`]
    /// paints between the `ToolExecutionStart` and `ToolExecutionEnd`
    /// events. Picks up the neutral [`ThemeBg::ToolPendingBg`]
    /// palette token.
    pub tool_pending_bg: Arc<dyn Fn(&str) -> String>,
    /// Tool-execution bubble tint applied once the call finishes
    /// without flagging an error (`ToolExecutionEnd { is_error: false }`).
    /// Picks up the success-leaning [`ThemeBg::ToolSuccessBg`]
    /// token (a faintly green-tinted background in the bundled
    /// themes).
    pub tool_success_bg: Arc<dyn Fn(&str) -> String>,
    /// Tool-execution bubble tint applied once the call finishes
    /// with `is_error: true`. Picks up the
    /// [`ThemeBg::ToolErrorBg`] token (a faintly red-tinted
    /// background in the bundled themes) so the eye finds failed
    /// calls without having to read the per-row colouring.
    pub tool_error_bg: Arc<dyn Fn(&str) -> String>,
}

/// Build the [`ChatTheme`] bundle the chat-scrollback components
/// share. New per-bubble background tokens land here so the
/// downstream wiring (event pump → component constructor) only
/// needs to consume `ChatTheme` rather than collecting individual
/// closures.
pub fn chat_theme(theme: &ThemeHandle, syntax_highlight: bool) -> ChatTheme {
    ChatTheme {
        markdown: markdown_theme(theme, syntax_highlight),
        thinking_text: theme.fg_closure(ThemeColor::ThinkingText),
        user_message_bg: theme.bg_closure(ThemeBg::UserMessageBg),
        tool_pending_bg: theme.bg_closure(ThemeBg::ToolPendingBg),
        tool_success_bg: theme.bg_closure(ThemeBg::ToolSuccessBg),
        tool_error_bg: theme.bg_closure(ThemeBg::ToolErrorBg),
    }
}

/// Build the [`MarkdownTheme`] used by the assistant-message and
/// user-message renderers. `syntax_highlight` carries the
/// `config.toml` `syntax_highlighting` option.
///
/// `code_block` stays identity: when highlighting is on the bundled
/// syntect highlighter colors per token inside the block (wrapping it
/// would interfere with its SGR resets), and when it is off the
/// identity closure is exactly the plain, uncolored rendering we want.
pub fn markdown_theme(theme: &ThemeHandle, syntax_highlight: bool) -> MarkdownTheme {
    MarkdownTheme {
        heading: theme.fg_closure(ThemeColor::MdHeading),
        bold: Arc::new(style::bold),
        italic: Arc::new(style::italic),
        strikethrough: Arc::new(style::strikethrough),
        code: theme.fg_closure(ThemeColor::MdCode),
        // Identity for the code-block body; the syntect-backed
        // highlighter (when `highlight_code` is `None`) takes care
        // of per-token colouring inside the block, so we don't want
        // an outer wrapper that would interfere with its SGR resets.
        code_block: Arc::new(|s| s.to_string()),
        code_block_border: theme.fg_closure(ThemeColor::MdCodeBlockBorder),
        link: theme.fg_closure(ThemeColor::MdLink),
        link_url: theme.fg_closure(ThemeColor::MdLinkUrl),
        list_bullet: theme.fg_closure(ThemeColor::MdListBullet),
        quote_border: theme.fg_closure(ThemeColor::MdQuoteBorder),
        quote: theme.fg_closure(ThemeColor::MdQuote),
        hr: theme.fg_closure(ThemeColor::MdHr),
        underline: Arc::new(style::underline),
        highlight_code: None,
        code_block_indent: None,
        syntax_highlight,
        // Map syntect's token categories onto the palette's `Syntax*`
        // tokens. Each closure re-reads the live palette, so code-block
        // colors follow a theme reload and honor the active color mode
        // just like the rest of the markdown styling.
        syntax: SyntaxStyles {
            comment: theme.fg_closure(ThemeColor::SyntaxComment),
            keyword: theme.fg_closure(ThemeColor::SyntaxKeyword),
            function: theme.fg_closure(ThemeColor::SyntaxFunction),
            variable: theme.fg_closure(ThemeColor::SyntaxVariable),
            string: theme.fg_closure(ThemeColor::SyntaxString),
            number: theme.fg_closure(ThemeColor::SyntaxNumber),
            type_name: theme.fg_closure(ThemeColor::SyntaxType),
            operator: theme.fg_closure(ThemeColor::SyntaxOperator),
            punctuation: theme.fg_closure(ThemeColor::SyntaxPunctuation),
        },
    }
}

// ============================================================================
// Editor border tints — thinking level / bash mode
// ============================================================================

/// Map a thinking level onto its dedicated [`ThemeColor`] token.
///
/// The mapping escalates visually with the model's reasoning
/// budget: `None` → muted `Off`; `Low` → soft blue; … →
/// `XHigh` / `Max` / `Ultra` → strong magenta. The JSON theme schema
/// tops out at `ThinkingXhigh`, so the highest levels share that tint —
/// they all represent "the strongest reasoning the active model
/// supports" and the visual cue is the same intent.
fn thinking_color_token(level: Option<&ThinkingConfig>) -> ThemeColor {
    match level {
        None => ThemeColor::ThinkingOff,
        Some(ThinkingConfig::Minimal) => ThemeColor::ThinkingMinimal,
        Some(ThinkingConfig::Low) => ThemeColor::ThinkingLow,
        Some(ThinkingConfig::Medium) => ThemeColor::ThinkingMedium,
        Some(ThinkingConfig::High) => ThemeColor::ThinkingHigh,
        Some(ThinkingConfig::XHigh) | Some(ThinkingConfig::Max) | Some(ThinkingConfig::Ultra) => {
            ThemeColor::ThinkingXhigh
        }
    }
}

/// Build the editor-border closure for a given thinking level.
///
/// The returned closure resolves through the shared [`ThemeHandle`]
/// on each call so a runtime theme reload reskins it without
/// rebuilding the editor. The host hands the closure to
/// [`aj_tui::editor_component::EditorComponent::set_border_color`]
/// whenever the agent's default thinking level changes; the next
/// render picks up the new tint automatically.
pub fn editor_border_color_for_thinking(
    theme: &ThemeHandle,
    level: Option<&ThinkingConfig>,
) -> Arc<dyn Fn(&str) -> String> {
    theme.fg_closure(thinking_color_token(level))
}

/// Build the editor-border closure for bash quick-command mode.
///
/// Reserved for a future bash-mode toggle on the editor; preserved
/// here alongside [`editor_border_color_for_thinking`] so the
/// mode → token mapping lives in one file. Renders against the
/// dedicated `bashMode` palette token (a vivid green in the
/// bundled themes) so a "press `!` to drop into shell" mode is
/// instantly visually distinct from thinking-level tints.
pub fn editor_border_color_for_bash_mode(theme: &ThemeHandle) -> Arc<dyn Fn(&str) -> String> {
    theme.fg_closure(ThemeColor::BashMode)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn bg_is_reasserted_after_embedded_resets() {
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);
        // Extract the raw bg prefix by painting an empty string.
        let prefix = theme
            .bg(ThemeBg::UserMessageBg, "")
            .strip_suffix("\x1b[49m")
            .expect("bg paint ends in bg reset")
            .to_string();
        assert!(!prefix.is_empty(), "test needs a token with a concrete bg");

        // A full SGR reset mid-row (the shape syntect-highlighted code
        // lines carry) must not drop the tint for the rest of the row.
        let painted = theme.bg(ThemeBg::UserMessageBg, "code\x1b[0m padding");
        assert!(
            painted.contains(&format!("\x1b[0m{prefix}")),
            "bg prefix re-asserted after full reset: {painted:?}",
        );

        // Same for an explicit bg reset embedded in the content.
        let painted = theme.bg(ThemeBg::UserMessageBg, "span\x1b[49m tail");
        assert!(
            painted.contains(&format!("\x1b[49m{prefix}")),
            "bg prefix re-asserted after bg reset: {painted:?}",
        );
    }

    #[test]
    fn dark_palette_loads() {
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);
        assert_eq!(theme.name(), "dark");
        // Spot check a couple of tokens to make sure the resolver
        // walked the var ref through to a concrete RGB.
        let accent = theme.fg(ThemeColor::Accent, "X");
        assert!(accent.contains("\x1b[38;2;156;220;254m"));
        // `text` is `""` which means "terminal default" — that
        // encodes as the foreground-reset escape, not a color set.
        let text = theme.fg(ThemeColor::Text, "X");
        assert!(text.starts_with("\x1b[39m"));
    }

    #[test]
    fn light_palette_loads() {
        let theme = Theme::bundled_light_with_mode(ColorMode::Truecolor);
        assert_eq!(theme.name(), "light");
        // `accent` resolves to `lightBlue` which is `#5277a3` —
        // verifies var refs walk through to a concrete RGB.
        let accent = theme.fg(ThemeColor::Accent, "X");
        assert!(accent.contains("\x1b[38;2;82;119;163m"));
    }

    #[test]
    fn ansi256_falls_back_to_palette_index_in_limited_terminal() {
        // 256-color mode downsamples hex values to palette
        // indexes. `#9cdcfe` is a light blue — it should land
        // somewhere in the 6x6x6 cube, not in the grayscale ramp.
        let theme = Theme::bundled_dark_with_mode(ColorMode::Color256);
        let accent = theme.fg(ThemeColor::Accent, "X");
        assert!(
            accent.contains("\x1b[38;5;"),
            "expected 256-color escape, got {accent:?}"
        );
        // Must not be a 24-bit triple in this mode.
        assert!(!accent.contains("\x1b[38;2;"));
    }

    #[test]
    fn integer_color_value_is_treated_as_palette_index() {
        let json = r#"{
            "name": "indexed",
            "vars": {},
            "colors": {
                "accent": 196,
                "border": "", "borderAccent": "", "borderMuted": "",
                "success": "", "error": "", "warning": "", "muted": "",
                "dim": "", "text": "", "thinkingText": "",
                "selectedBg": "", "userMessageBg": "", "userMessageText": "",
                "customMessageBg": "", "customMessageText": "",
                "customMessageLabel": "", "toolPendingBg": "",
                "toolSuccessBg": "", "toolErrorBg": "", "toolTitle": "",
                "toolOutput": "",
                "mdHeading": "", "mdLink": "", "mdLinkUrl": "", "mdCode": "",
                "mdCodeBlock": "", "mdCodeBlockBorder": "", "mdQuote": "",
                "mdQuoteBorder": "", "mdHr": "", "mdListBullet": "",
                "toolDiffAdded": "", "toolDiffRemoved": "", "toolDiffContext": "",
                "syntaxComment": "", "syntaxKeyword": "", "syntaxFunction": "",
                "syntaxVariable": "", "syntaxString": "", "syntaxNumber": "",
                "syntaxType": "", "syntaxOperator": "", "syntaxPunctuation": "",
                "thinkingOff": "", "thinkingMinimal": "", "thinkingLow": "",
                "thinkingMedium": "", "thinkingHigh": "", "thinkingXhigh": "",
                "bashMode": ""
            }
        }"#;
        let theme = Theme::from_json_with_mode("indexed", json, ColorMode::Truecolor)
            .expect("indexed theme must parse");
        let accent = theme.fg(ThemeColor::Accent, "X");
        assert!(accent.contains("\x1b[38;5;196m"));
    }

    #[test]
    fn builders_produce_themed_closures() {
        let handle = ThemeHandle::new(Theme::bundled_dark());
        let ml_theme = markdown_theme(&handle, true);
        // Headings use the default text color (`mdHeading` is empty
        // in the bundled palettes), so the closure emits the default
        // foreground escape rather than a specific color — they're
        // distinguished by bold/underline instead.
        let painted = (ml_theme.heading)("hi");
        assert!(
            painted.contains("\x1b[39m"),
            "expected default foreground escape, got {painted:?}"
        );
        // The inline-code closure carries the `mdCode` color
        // (`#9cdcfe` light blue in dark), either as a 24-bit triple
        // or a 256-color index depending on the detected color mode.
        let painted = (ml_theme.code)("hi");
        let has_truecolor = painted.contains("\x1b[38;2;156;220;254m");
        let has_256 = painted.contains("\x1b[38;5;");
        assert!(
            has_truecolor || has_256,
            "expected inline-code color escape, got {painted:?}"
        );
        // Bold/italic/etc. don't go through the theme — they
        // emit pure SGR style codes via aj_tui::style.
        let painted = (ml_theme.bold)("hi");
        assert!(painted.contains("\x1b[1m"));
    }

    #[test]
    fn markdown_theme_wires_palette_syntax_tokens() {
        // The syntax closures must carry the palette's `Syntax*`
        // colors so code blocks track the active theme. Pin the color
        // mode so the assertion is deterministic regardless of the
        // test environment's terminal.
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);
        let handle = ThemeHandle::new(theme);
        let ml = markdown_theme(&handle, true);

        // dark.json: syntaxKeyword = #569CD6 → rgb(86,156,214),
        // syntaxString = #CE9178 → rgb(206,145,120).
        assert!(
            (ml.syntax.keyword)("kw").contains("\x1b[38;2;86;156;214m"),
            "keyword closure should carry the syntaxKeyword color"
        );
        assert!(
            (ml.syntax.string)("s").contains("\x1b[38;2;206;145;120m"),
            "string closure should carry the syntaxString color"
        );
    }

    #[test]
    fn rgb_to_256_keeps_saturated_colors_in_cube() {
        // A saturated cyan should land in the cube, not the
        // grayscale ramp.
        let idx = rgb_to_256(0x00, 0xd7, 0xff);
        assert!((16..232).contains(&idx), "expected cube index, got {idx}");
    }

    #[test]
    fn rgb_to_256_uses_grayscale_for_neutral_colors() {
        // A neutral gray should land in the grayscale ramp.
        let idx = rgb_to_256(128, 128, 128);
        assert!(
            (232..=255).contains(&idx),
            "expected grayscale index, got {idx}"
        );
    }

    // ------------------------------------------------------------
    // Editor border tints — thinking level / bash mode
    // ------------------------------------------------------------

    /// Each thinking level (and "off") must route to its dedicated
    /// `ThemeColor` token. Locks the mapping so a future re-order
    /// of `ThinkingConfig` variants or a renamed theme token
    /// surfaces here rather than as a silently-wrong border tint.
    #[test]
    fn thinking_color_token_maps_each_level_to_its_token() {
        assert_eq!(thinking_color_token(None), ThemeColor::ThinkingOff);
        assert_eq!(
            thinking_color_token(Some(&ThinkingConfig::Minimal)),
            ThemeColor::ThinkingMinimal
        );
        assert_eq!(
            thinking_color_token(Some(&ThinkingConfig::Low)),
            ThemeColor::ThinkingLow
        );
        assert_eq!(
            thinking_color_token(Some(&ThinkingConfig::Medium)),
            ThemeColor::ThinkingMedium
        );
        assert_eq!(
            thinking_color_token(Some(&ThinkingConfig::High)),
            ThemeColor::ThinkingHigh
        );
        // `XHigh`, `Max`, and `Ultra` all top out at the highest tint
        // the theme schema exposes (`ThinkingXhigh`) — they represent
        // the same "strongest reasoning available" intent.
        assert_eq!(
            thinking_color_token(Some(&ThinkingConfig::XHigh)),
            ThemeColor::ThinkingXhigh
        );
        assert_eq!(
            thinking_color_token(Some(&ThinkingConfig::Max)),
            ThemeColor::ThinkingXhigh
        );
        assert_eq!(
            thinking_color_token(Some(&ThinkingConfig::Ultra)),
            ThemeColor::ThinkingXhigh
        );
    }

    /// The thinking-border closure paints with the resolved palette
    /// token for the requested level. `medium` resolves to dark's
    /// `#81a2be`, so the painted string must carry that escape.
    #[test]
    fn editor_border_color_for_thinking_paints_with_level_token() {
        let handle = ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor));
        let paint = editor_border_color_for_thinking(&handle, Some(&ThinkingConfig::Medium));
        let painted = paint("─");
        assert!(
            painted.contains("\x1b[38;2;129;162;190m"),
            "expected medium thinking tint, got {painted:?}"
        );
    }

    /// `None` (i.e. "no thinking") routes to the `ThinkingOff`
    /// token. Locks the muted-tint default so a future regression
    /// that mis-routes an unset thinking level surfaces here.
    #[test]
    fn editor_border_color_for_thinking_off_paints_with_off_token() {
        let handle = ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor));
        let paint = editor_border_color_for_thinking(&handle, None);
        let painted = paint("─");
        // Dark's `thinkingOff` resolves to `darkGray` → `#505050`.
        assert!(
            painted.contains("\x1b[38;2;80;80;80m"),
            "expected off-thinking dark-gray tint, got {painted:?}"
        );
    }

    /// The hot-reload invariant carries through to the editor
    /// border: a closure built before a `theme.replace()` must
    /// paint with the new palette afterwards. This is what makes
    /// the user-themes fs-watcher cover the editor border without
    /// any additional plumbing.
    #[test]
    fn editor_border_color_picks_up_theme_replace() {
        let handle = ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor));
        let paint = editor_border_color_for_thinking(&handle, Some(&ThinkingConfig::High));
        let before = paint("─");
        // Dark's `thinkingHigh` resolves to `#b294bb`.
        assert!(
            before.contains("\x1b[38;2;178;148;187m"),
            "expected dark `high` tint before swap, got {before:?}"
        );

        handle.replace(Theme::bundled_light_with_mode(ColorMode::Truecolor));
        let after = paint("─");
        // The escape must differ — the closure resolves through
        // the shared handle, so the swap is visible immediately.
        assert_ne!(
            before, after,
            "border closure must repaint after theme swap"
        );
    }

    /// Bash mode routes to its dedicated palette token regardless
    /// of thinking level — verifies the helper is wired into the
    /// `BashMode` token, not folded into the thinking mapping.
    #[test]
    fn editor_border_color_for_bash_mode_paints_with_bash_token() {
        let handle = ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor));
        let paint = editor_border_color_for_bash_mode(&handle);
        let painted = paint("─");
        // Dark's `bashMode` resolves through `green` → `#b5bd68`.
        assert!(
            painted.contains("\x1b[38;2;181;189;104m"),
            "expected bash-mode green tint, got {painted:?}"
        );
    }

    // ------------------------------------------------------------
    // ThemeHandle: hot-swap semantics
    // ------------------------------------------------------------

    #[test]
    fn theme_handle_closure_reflects_replaced_palette() {
        // The cornerstone hot-reload invariant: a closure obtained
        // before `replace` must paint with the new theme's escape
        // after `replace`.
        let handle = ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor));
        let paint = handle.fg_closure(ThemeColor::Accent);

        let before = paint("X");
        // Dark's accent resolves through `lightBlue` → `#9cdcfe`.
        assert!(
            before.contains("\x1b[38;2;156;220;254m"),
            "expected dark accent escape before swap, got {before:?}"
        );

        // Swap to the light palette in-place.
        handle.replace(Theme::bundled_light_with_mode(ColorMode::Truecolor));
        let after = paint("X");
        // Light's accent resolves through `lightBlue` → `#5277a3`.
        assert!(
            after.contains("\x1b[38;2;82;119;163m"),
            "expected light accent escape after swap, got {after:?}"
        );
        // The dark prefix must be gone — otherwise we'd just be
        // concatenating both.
        assert!(
            !after.contains("\x1b[38;2;156;220;254m"),
            "stale dark escape leaked into post-swap output: {after:?}"
        );
    }
}
