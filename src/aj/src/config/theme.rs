//! Theme — JSON-loaded palette + builders for the interactive
//! mode's component themes.
//!
//! ## Shape
//!
//! A [`Theme`] is a resolved palette: every semantic token (one of
//! the [`ThemeColor`] / [`ThemeBg`] variants) maps to a precomputed
//! ANSI SGR escape, ready to wrap text. The palette is loaded from
//! a JSON file that uses two layers:
//!
//! 1. **`vars`** — named hex / 256-color values for reuse.
//! 2. **`colors`** — semantic tokens; each value is either an
//!    explicit color (hex, 256-color index, or empty for terminal
//!    default) or a reference to a `vars` key.
//!
//! Var references resolve transitively so a theme can layer
//! "purpose" (`accent`, `borderMuted`) on top of "value"
//! (`#8abeb7`, `#666666`) without duplication.
//!
//! ## Built-in themes
//!
//! Two palettes ship bundled in the binary: `dark` (the default)
//! and `light`. The JSON for each lives next to this file and is
//! embedded via [`include_str!`] so the binary always loads cleanly
//! offline.
//!
//! ## User themes
//!
//! Drop a `<name>.json` into `~/.aj/themes/` and set
//! `theme = "<name>"` in `config.toml`. The loader looks at the
//! user-themes directory first, falls back to the built-in catalog
//! on miss, and falls back further to the bundled `dark` theme if
//! the named theme fails to parse so the binary always comes up
//! with a working palette.
//!
//! ## Color modes
//!
//! Modern terminals support 24-bit truecolor; some older ones cap
//! out at the 256-color palette (Apple Terminal, GNU screen without
//! `COLORTERM=truecolor`, dumb terminals). [`Theme`] detects this
//! at construction time from `$COLORTERM` / `$TERM` /
//! `$TERM_PROGRAM` and downsamples hex values to the 256-color
//! cube on terminals that can't show them.
//!
//! ## Builders
//!
//! [`markdown_theme`], [`editor_theme`], and [`select_list_theme`]
//! consume a [`Theme`] and return populated [`MarkdownTheme`] /
//! [`EditorTheme`] / [`SelectListTheme`] structs from `aj-tui`,
//! ready to hand to the matching components.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use aj_models::ThinkingConfig;
use aj_tui::components::editor::EditorTheme;
use aj_tui::components::markdown::MarkdownTheme;
use aj_tui::components::select_list::SelectListTheme;
use aj_tui::style;
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Bundled "dark" palette. The default; loads when `theme` is unset
/// in `config.toml` or when an explicitly-named theme fails to
/// parse.
const DARK_THEME_JSON: &str = include_str!("theme/dark.json");

/// Bundled "light" palette. Companion to `dark`; picked via
/// `theme = "light"` in `config.toml`.
const LIGHT_THEME_JSON: &str = include_str!("theme/light.json");

// ============================================================================
// Semantic tokens
// ============================================================================

/// Foreground color tokens. Every variant is a *purpose* (e.g.
/// `Accent`, `Success`, `MdHeading`) the renderers consult — the
/// concrete value comes from whatever palette is loaded.
///
/// The full set matches the schema shipped by the JSON loader so
/// future renderer additions land here without re-jiggling the
/// catalog; only a subset is consumed by the builders today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeColor {
    /// Primary accent: selectors, focused row, prompt cursor.
    Accent,
    /// Default border tint.
    Border,
    /// Highlighted border tint.
    BorderAccent,
    /// Subtle border tint — what the editor uses by default.
    BorderMuted,
    /// Success state.
    Success,
    /// Error state.
    Error,
    /// Warning state.
    Warning,
    /// Secondary text.
    Muted,
    /// Tertiary, even-more-subtle text.
    Dim,
    /// Default text. Renders as the terminal's foreground color.
    Text,
    /// Thinking block body text.
    ThinkingText,

    /// User-message body text.
    UserMessageText,
    /// Custom-message body text.
    CustomMessageText,
    /// Custom-message type label.
    CustomMessageLabel,
    /// Tool-call header title.
    ToolTitle,
    /// Tool-call body output.
    ToolOutput,

    /// Markdown heading.
    MdHeading,
    /// Markdown link text.
    MdLink,
    /// Markdown link URL trailer.
    MdLinkUrl,
    /// Markdown inline code.
    MdCode,
    /// Markdown code block body (typically left identity so the
    /// syntect highlighter's per-token colors win).
    MdCodeBlock,
    /// Markdown code block fence/border lines.
    MdCodeBlockBorder,
    /// Markdown blockquote text.
    MdQuote,
    /// Markdown blockquote prefix bar.
    MdQuoteBorder,
    /// Markdown horizontal rule.
    MdHr,
    /// Markdown list bullets / numbers.
    MdListBullet,

    /// Diff added lines (`+`).
    ToolDiffAdded,
    /// Diff removed lines (`-`).
    ToolDiffRemoved,
    /// Diff context lines.
    ToolDiffContext,

    /// Syntax: comments.
    SyntaxComment,
    /// Syntax: keywords.
    SyntaxKeyword,
    /// Syntax: function names.
    SyntaxFunction,
    /// Syntax: variable names.
    SyntaxVariable,
    /// Syntax: string literals.
    SyntaxString,
    /// Syntax: number literals.
    SyntaxNumber,
    /// Syntax: type names.
    SyntaxType,
    /// Syntax: operators.
    SyntaxOperator,
    /// Syntax: punctuation.
    SyntaxPunctuation,

    /// Editor border in `Off` thinking mode.
    ThinkingOff,
    /// Editor border in `Minimal` thinking mode.
    ThinkingMinimal,
    /// Editor border in `Low` thinking mode.
    ThinkingLow,
    /// Editor border in `Medium` thinking mode.
    ThinkingMedium,
    /// Editor border in `High` thinking mode.
    ThinkingHigh,
    /// Editor border in `XHigh` thinking mode.
    ThinkingXhigh,

    /// Editor border in bash quick-command mode.
    BashMode,
}

impl ThemeColor {
    /// JSON-token name used for this semantic color in palette files.
    pub fn json_key(self) -> &'static str {
        match self {
            ThemeColor::Accent => "accent",
            ThemeColor::Border => "border",
            ThemeColor::BorderAccent => "borderAccent",
            ThemeColor::BorderMuted => "borderMuted",
            ThemeColor::Success => "success",
            ThemeColor::Error => "error",
            ThemeColor::Warning => "warning",
            ThemeColor::Muted => "muted",
            ThemeColor::Dim => "dim",
            ThemeColor::Text => "text",
            ThemeColor::ThinkingText => "thinkingText",
            ThemeColor::UserMessageText => "userMessageText",
            ThemeColor::CustomMessageText => "customMessageText",
            ThemeColor::CustomMessageLabel => "customMessageLabel",
            ThemeColor::ToolTitle => "toolTitle",
            ThemeColor::ToolOutput => "toolOutput",
            ThemeColor::MdHeading => "mdHeading",
            ThemeColor::MdLink => "mdLink",
            ThemeColor::MdLinkUrl => "mdLinkUrl",
            ThemeColor::MdCode => "mdCode",
            ThemeColor::MdCodeBlock => "mdCodeBlock",
            ThemeColor::MdCodeBlockBorder => "mdCodeBlockBorder",
            ThemeColor::MdQuote => "mdQuote",
            ThemeColor::MdQuoteBorder => "mdQuoteBorder",
            ThemeColor::MdHr => "mdHr",
            ThemeColor::MdListBullet => "mdListBullet",
            ThemeColor::ToolDiffAdded => "toolDiffAdded",
            ThemeColor::ToolDiffRemoved => "toolDiffRemoved",
            ThemeColor::ToolDiffContext => "toolDiffContext",
            ThemeColor::SyntaxComment => "syntaxComment",
            ThemeColor::SyntaxKeyword => "syntaxKeyword",
            ThemeColor::SyntaxFunction => "syntaxFunction",
            ThemeColor::SyntaxVariable => "syntaxVariable",
            ThemeColor::SyntaxString => "syntaxString",
            ThemeColor::SyntaxNumber => "syntaxNumber",
            ThemeColor::SyntaxType => "syntaxType",
            ThemeColor::SyntaxOperator => "syntaxOperator",
            ThemeColor::SyntaxPunctuation => "syntaxPunctuation",
            ThemeColor::ThinkingOff => "thinkingOff",
            ThemeColor::ThinkingMinimal => "thinkingMinimal",
            ThemeColor::ThinkingLow => "thinkingLow",
            ThemeColor::ThinkingMedium => "thinkingMedium",
            ThemeColor::ThinkingHigh => "thinkingHigh",
            ThemeColor::ThinkingXhigh => "thinkingXhigh",
            ThemeColor::BashMode => "bashMode",
        }
    }

    /// Full closed enumeration. Used by [`Theme::from_json`] to
    /// iterate the schema keys when populating the resolved map.
    fn all() -> &'static [ThemeColor] {
        &[
            ThemeColor::Accent,
            ThemeColor::Border,
            ThemeColor::BorderAccent,
            ThemeColor::BorderMuted,
            ThemeColor::Success,
            ThemeColor::Error,
            ThemeColor::Warning,
            ThemeColor::Muted,
            ThemeColor::Dim,
            ThemeColor::Text,
            ThemeColor::ThinkingText,
            ThemeColor::UserMessageText,
            ThemeColor::CustomMessageText,
            ThemeColor::CustomMessageLabel,
            ThemeColor::ToolTitle,
            ThemeColor::ToolOutput,
            ThemeColor::MdHeading,
            ThemeColor::MdLink,
            ThemeColor::MdLinkUrl,
            ThemeColor::MdCode,
            ThemeColor::MdCodeBlock,
            ThemeColor::MdCodeBlockBorder,
            ThemeColor::MdQuote,
            ThemeColor::MdQuoteBorder,
            ThemeColor::MdHr,
            ThemeColor::MdListBullet,
            ThemeColor::ToolDiffAdded,
            ThemeColor::ToolDiffRemoved,
            ThemeColor::ToolDiffContext,
            ThemeColor::SyntaxComment,
            ThemeColor::SyntaxKeyword,
            ThemeColor::SyntaxFunction,
            ThemeColor::SyntaxVariable,
            ThemeColor::SyntaxString,
            ThemeColor::SyntaxNumber,
            ThemeColor::SyntaxType,
            ThemeColor::SyntaxOperator,
            ThemeColor::SyntaxPunctuation,
            ThemeColor::ThinkingOff,
            ThemeColor::ThinkingMinimal,
            ThemeColor::ThinkingLow,
            ThemeColor::ThinkingMedium,
            ThemeColor::ThinkingHigh,
            ThemeColor::ThinkingXhigh,
            ThemeColor::BashMode,
        ]
    }
}

/// Background color tokens. Smaller set than [`ThemeColor`] —
/// backgrounds are concentrated in user / custom / tool message
/// boxes today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeBg {
    /// Selected-row background in select-list overlays.
    SelectedBg,
    /// User message body background.
    UserMessageBg,
    /// Custom message body background.
    CustomMessageBg,
    /// Tool-call box background while running.
    ToolPendingBg,
    /// Tool-call box background after a successful run.
    ToolSuccessBg,
    /// Tool-call box background after a failed run.
    ToolErrorBg,
}

impl ThemeBg {
    /// JSON-token name used for this semantic background in palette
    /// files.
    pub fn json_key(self) -> &'static str {
        match self {
            ThemeBg::SelectedBg => "selectedBg",
            ThemeBg::UserMessageBg => "userMessageBg",
            ThemeBg::CustomMessageBg => "customMessageBg",
            ThemeBg::ToolPendingBg => "toolPendingBg",
            ThemeBg::ToolSuccessBg => "toolSuccessBg",
            ThemeBg::ToolErrorBg => "toolErrorBg",
        }
    }

    /// Full closed enumeration. Used by [`Theme::from_json`] to
    /// iterate the schema keys when populating the resolved map.
    fn all() -> &'static [ThemeBg] {
        &[
            ThemeBg::SelectedBg,
            ThemeBg::UserMessageBg,
            ThemeBg::CustomMessageBg,
            ThemeBg::ToolPendingBg,
            ThemeBg::ToolSuccessBg,
            ThemeBg::ToolErrorBg,
        ]
    }
}

// ============================================================================
// JSON schema
// ============================================================================

/// A color value as it appears on disk — before var refs are
/// resolved. Strings carry hex (`#rrggbb`), a var name, or `""`
/// (terminal default); integers are 256-color palette indexes.
///
/// `#[serde(untagged)]` so the field accepts either shape without
/// a tag.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum ColorValueJson {
    Str(String),
    Index(u8),
}

/// Resolved color — what gets handed to the ANSI encoder. Once var
/// refs are walked every value is one of these three shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedColor {
    /// 24-bit RGB triple from a hex literal.
    Rgb(u8, u8, u8),
    /// 256-color palette index.
    Ansi256(u8),
    /// Terminal default (matches the `""` JSON value).
    Default,
}

/// The schema parsed from a palette JSON file.
#[derive(Debug, Clone, Deserialize)]
struct ThemeJson {
    name: String,
    #[serde(default)]
    vars: HashMap<String, ColorValueJson>,
    colors: HashMap<String, ColorValueJson>,
}

// ============================================================================
// Color mode detection
// ============================================================================

/// Whether the terminal can render 24-bit RGB directly. Detected
/// once at theme construction; truecolor is the default, falling
/// back to 256-color when the environment hints at a limited
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Truecolor,
    Color256,
}

impl ColorMode {
    /// Best-effort detection from the process environment, used to
    /// pick the downsample boundary between truecolor and 256-color
    /// output.
    pub fn detect() -> Self {
        if let Ok(ct) = env::var("COLORTERM")
            && (ct == "truecolor" || ct == "24bit")
        {
            return ColorMode::Truecolor;
        }
        // Windows Terminal supports truecolor.
        if env::var("WT_SESSION").is_ok() {
            return ColorMode::Truecolor;
        }
        let term = env::var("TERM").unwrap_or_default();
        // Truly limited terminals.
        if term.is_empty() || term == "dumb" || term == "linux" {
            return ColorMode::Color256;
        }
        // Apple Terminal doesn't support truecolor.
        if env::var("TERM_PROGRAM").as_deref() == Ok("Apple_Terminal") {
            return ColorMode::Color256;
        }
        // GNU screen without explicit opt-in.
        if term == "screen" || term.starts_with("screen-") || term.starts_with("screen.") {
            return ColorMode::Color256;
        }
        ColorMode::Truecolor
    }
}

// ============================================================================
// Color-space helpers
// ============================================================================

fn hex_to_rgb(hex: &str) -> Result<(u8, u8, u8), ThemeError> {
    let cleaned = hex.strip_prefix('#').unwrap_or(hex);
    if cleaned.len() != 6 {
        return Err(ThemeError::InvalidColor(hex.to_string()));
    }
    let r = u8::from_str_radix(&cleaned[0..2], 16)
        .map_err(|_| ThemeError::InvalidColor(hex.to_string()))?;
    let g = u8::from_str_radix(&cleaned[2..4], 16)
        .map_err(|_| ThemeError::InvalidColor(hex.to_string()))?;
    let b = u8::from_str_radix(&cleaned[4..6], 16)
        .map_err(|_| ThemeError::InvalidColor(hex.to_string()))?;
    Ok((r, g, b))
}

/// The xterm 6x6x6 color cube channel values.
const CUBE_VALUES: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The xterm 24-step grayscale ramp values (palette indices
/// 232..=255). Computed as `8 + i * 10` per the ramp definition.
fn gray_values() -> [u8; 24] {
    let mut out = [0u8; 24];
    let mut i: u8 = 0;
    while i < 24 {
        out[usize::from(i)] = 8 + i * 10;
        i += 1;
    }
    out
}

fn closest_cube_index(value: u8) -> usize {
    let mut min_dist = u32::MAX;
    let mut min_idx = 0usize;
    for (i, &v) in CUBE_VALUES.iter().enumerate() {
        let dist = u32::from(value).abs_diff(u32::from(v));
        if dist < min_dist {
            min_dist = dist;
            min_idx = i;
        }
    }
    min_idx
}

fn closest_gray_index(value: u8) -> usize {
    let grays = gray_values();
    let mut min_dist = u32::MAX;
    let mut min_idx = 0usize;
    for (i, &v) in grays.iter().enumerate() {
        let dist = u32::from(value).abs_diff(u32::from(v));
        if dist < min_dist {
            min_dist = dist;
            min_idx = i;
        }
    }
    min_idx
}

fn color_distance(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    // Weighted squared distance with BT.601 luma coefficients
    // (0.299/0.587/0.114), scaled by 1000 to stay in integer math.
    // We only care about the order of distances, not their
    // absolute values, so the scaling factor cancels out.
    //
    // Worst-case input is `dr = 255` → `dr*dr = 65_025`, weighted
    // by 587 yields ~38M. Summing three weighted terms stays well
    // under u32::MAX.
    let dr = u32::from(r1).abs_diff(u32::from(r2));
    let dg = u32::from(g1).abs_diff(u32::from(g2));
    let db = u32::from(b1).abs_diff(u32::from(b2));
    dr * dr * 299 + dg * dg * 587 + db * db * 114
}

/// Map a 24-bit RGB triple onto the closest entry in the xterm
/// 256-color palette. The 6x6x6 cube wins for any non-neutral
/// hue; the 24-step grayscale ramp wins only when the color is
/// nearly desaturated (max-min channel spread < 10) *and* the
/// grayscale entry is empirically closer than the cube pick.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let ri = closest_cube_index(r);
    let gi = closest_cube_index(g);
    let bi = closest_cube_index(b);
    let cube_r = CUBE_VALUES[ri];
    let cube_g = CUBE_VALUES[gi];
    let cube_b = CUBE_VALUES[bi];
    let cube_index =
        u8::try_from(16 + 36 * ri + 6 * gi + bi).expect("6x6x6 cube indices stay within u8 range");
    let cube_dist = color_distance(r, g, b, cube_r, cube_g, cube_b);

    // Compute luma in integer arithmetic: BT.601 coefficients are
    // 0.299/0.587/0.114; scaling by 1000 keeps three decimals of
    // precision without touching floats. The max possible sum is
    // 255*1000, which fits in u32 with room to spare.
    let luma_scaled = 299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b);
    // Round-to-nearest by adding half the divisor before dividing.
    let gray_u32 = (luma_scaled + 500) / 1000;
    // `gray_u32` is at most ((255*1000)+500)/1000 = 255, so this
    // try_from never fails; `.unwrap_or(255)` is defensive only.
    let gray_clamped = u8::try_from(gray_u32).unwrap_or(255);
    let grays = gray_values();
    let gi_ramp = closest_gray_index(gray_clamped);
    let gray_value = grays[gi_ramp];
    let gray_index = u8::try_from(232 + gi_ramp).expect("grayscale indices fit in u8");
    let gray_dist = color_distance(r, g, b, gray_value, gray_value, gray_value);

    let max_c = r.max(g).max(b);
    let min_c = r.min(g).min(b);
    let spread = max_c.saturating_sub(min_c);

    // Only prefer grayscale when the color is nearly neutral, so
    // a deliberate cyan / red tint isn't flattened to gray on
    // limited terminals.
    if spread < 10 && gray_dist < cube_dist {
        gray_index
    } else {
        cube_index
    }
}

fn fg_ansi(color: &ResolvedColor, mode: ColorMode) -> String {
    match (color, mode) {
        (ResolvedColor::Default, _) => "\x1b[39m".to_string(),
        (ResolvedColor::Ansi256(i), _) => format!("\x1b[38;5;{i}m"),
        (ResolvedColor::Rgb(r, g, b), ColorMode::Truecolor) => {
            format!("\x1b[38;2;{r};{g};{b}m")
        }
        (ResolvedColor::Rgb(r, g, b), ColorMode::Color256) => {
            let idx = rgb_to_256(*r, *g, *b);
            format!("\x1b[38;5;{idx}m")
        }
    }
}

fn bg_ansi(color: &ResolvedColor, mode: ColorMode) -> String {
    match (color, mode) {
        (ResolvedColor::Default, _) => "\x1b[49m".to_string(),
        (ResolvedColor::Ansi256(i), _) => format!("\x1b[48;5;{i}m"),
        (ResolvedColor::Rgb(r, g, b), ColorMode::Truecolor) => {
            format!("\x1b[48;2;{r};{g};{b}m")
        }
        (ResolvedColor::Rgb(r, g, b), ColorMode::Color256) => {
            let idx = rgb_to_256(*r, *g, *b);
            format!("\x1b[48;5;{idx}m")
        }
    }
}

// ============================================================================
// Var resolution
// ============================================================================

/// Walk var-references through `vars` until a concrete color falls
/// out. Detects cycles via the `visited` set so a malformed theme
/// produces a clear error rather than an infinite loop.
fn resolve_var(
    value: &ColorValueJson,
    vars: &HashMap<String, ColorValueJson>,
    visited: &mut Vec<String>,
) -> Result<ResolvedColor, ThemeError> {
    match value {
        ColorValueJson::Index(i) => Ok(ResolvedColor::Ansi256(*i)),
        ColorValueJson::Str(s) => {
            if s.is_empty() {
                return Ok(ResolvedColor::Default);
            }
            if let Some(stripped) = s.strip_prefix('#') {
                let (r, g, b) = hex_to_rgb(stripped)?;
                return Ok(ResolvedColor::Rgb(r, g, b));
            }
            // Bare string: treat as a var reference.
            if visited.iter().any(|seen| seen == s) {
                return Err(ThemeError::VarCycle(s.clone()));
            }
            let target = vars
                .get(s)
                .ok_or_else(|| ThemeError::UnknownVar(s.clone()))?;
            visited.push(s.clone());
            let resolved = resolve_var(target, vars, visited);
            visited.pop();
            resolved
        }
    }
}

// ============================================================================
// Theme
// ============================================================================

/// Errors that can occur loading / parsing a theme.
#[derive(Debug, Error)]
pub enum ThemeError {
    /// The theme file referenced an unknown built-in name and no
    /// matching `~/.aj/themes/<name>.json` exists.
    #[error("theme not found: {0}")]
    NotFound(String),
    /// The on-disk JSON failed to parse.
    #[error("failed to parse theme '{0}': {1}")]
    ParseError(String, serde_json::Error),
    /// The JSON parsed but a color value is malformed (e.g.
    /// short hex string).
    #[error("invalid color value: {0}")]
    InvalidColor(String),
    /// A var-reference points to a name that doesn't exist in the
    /// `vars` map.
    #[error("unknown var reference: {0}")]
    UnknownVar(String),
    /// A chain of var-references loops back to itself.
    #[error("circular var reference: {0}")]
    VarCycle(String),
    /// A required token in the `colors` map is missing.
    #[error("theme is missing required color token: {0}")]
    MissingColor(String),
    /// Reading the file from disk failed.
    #[error("failed to read theme file '{0}': {1}")]
    ReadError(PathBuf, std::io::Error),
}

/// A loaded, var-resolved palette. Each [`ThemeColor`] / [`ThemeBg`]
/// token maps to a precomputed ANSI SGR escape.
#[derive(Debug, Clone)]
pub struct Theme {
    name: String,
    mode: ColorMode,
    fg: HashMap<ThemeColor, String>,
    bg: HashMap<ThemeBg, String>,
}

impl Theme {
    /// The name carried in the JSON `name` field. Useful for
    /// "selected theme" indicators in any future settings UI.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The color mode this theme's ANSI escapes are encoded for.
    pub fn color_mode(&self) -> ColorMode {
        self.mode
    }

    /// Wrap `text` in the SGR escape for the given foreground
    /// token plus the matching reset. The reset is `\x1b[39m`
    /// (default foreground) so a nested style isn't disturbed.
    pub fn fg(&self, token: ThemeColor, text: &str) -> String {
        let prefix = self.fg.get(&token).map(|s| s.as_str()).unwrap_or("");
        format!("{prefix}{text}\x1b[39m")
    }

    /// Wrap `text` in the SGR escape for the given background
    /// token plus the matching reset.
    pub fn bg(&self, token: ThemeBg, text: &str) -> String {
        let prefix = self.bg.get(&token).map(|s| s.as_str()).unwrap_or("");
        format!("{prefix}{text}\x1b[49m")
    }

    /// Build a closure that applies the given foreground token to
    /// arbitrary text. Used by the builder functions below to
    /// produce the `Arc<dyn Fn(&str) -> String>` closures the
    /// `aj-tui` theme structs require.
    pub fn fg_closure(&self, token: ThemeColor) -> Arc<dyn Fn(&str) -> String> {
        let prefix = self.fg.get(&token).cloned().unwrap_or_default();
        Arc::new(move |s: &str| format!("{prefix}{s}\x1b[39m"))
    }

    /// Build a closure that applies the given background token to
    /// arbitrary text.
    pub fn bg_closure(&self, token: ThemeBg) -> Arc<dyn Fn(&str) -> String> {
        let prefix = self.bg.get(&token).cloned().unwrap_or_default();
        Arc::new(move |s: &str| format!("{prefix}{s}\x1b[49m"))
    }

    /// Parse a JSON theme document. The default [`ColorMode`] is
    /// detected from the environment; pass a specific mode via
    /// [`Theme::from_json_with_mode`] from tests so output stays
    /// deterministic.
    pub fn from_json(label: &str, json: &str) -> Result<Self, ThemeError> {
        Self::from_json_with_mode(label, json, ColorMode::detect())
    }

    /// Parse a JSON theme document with an explicit color mode.
    pub fn from_json_with_mode(
        label: &str,
        json: &str,
        mode: ColorMode,
    ) -> Result<Self, ThemeError> {
        let parsed: ThemeJson =
            serde_json::from_str(json).map_err(|e| ThemeError::ParseError(label.into(), e))?;

        let mut fg = HashMap::with_capacity(ThemeColor::all().len());
        for token in ThemeColor::all() {
            let key = token.json_key();
            let raw = parsed
                .colors
                .get(key)
                .ok_or_else(|| ThemeError::MissingColor(key.to_string()))?;
            let mut visited = Vec::new();
            let resolved = resolve_var(raw, &parsed.vars, &mut visited)?;
            fg.insert(*token, fg_ansi(&resolved, mode));
        }

        let mut bg = HashMap::with_capacity(ThemeBg::all().len());
        for token in ThemeBg::all() {
            let key = token.json_key();
            let raw = parsed
                .colors
                .get(key)
                .ok_or_else(|| ThemeError::MissingColor(key.to_string()))?;
            let mut visited = Vec::new();
            let resolved = resolve_var(raw, &parsed.vars, &mut visited)?;
            bg.insert(*token, bg_ansi(&resolved, mode));
        }

        Ok(Theme {
            name: parsed.name,
            mode,
            fg,
            bg,
        })
    }

    /// Load the theme `name`. The lookup order is:
    /// 1. The bundled catalog (`dark`, `light`).
    /// 2. `~/.aj/themes/<name>.json` if the file exists.
    ///
    /// User themes can override built-ins by living in the user
    /// themes dir under the same name. On parse error this falls
    /// back to the bundled `dark` palette so the binary always
    /// comes up with a working theme.
    pub fn load(name: &str) -> Theme {
        match Self::load_strict(name) {
            Ok(theme) => theme,
            Err(err) => {
                tracing::warn!("failed to load theme '{name}': {err}; falling back to dark");
                Self::bundled_dark()
            }
        }
    }

    /// Like [`Theme::load`] but returns the error instead of
    /// falling back. Tests use this so a malformed bundle fails
    /// noisily; production code uses [`Theme::load`] for
    /// resilience.
    pub fn load_strict(name: &str) -> Result<Theme, ThemeError> {
        // User dir wins so a user can override the bundled themes
        // by name. We still need to fail open: a missing user
        // file falls through to the bundled catalog.
        if let Some(path) = user_theme_path(name)
            && path.exists()
        {
            let content =
                fs::read_to_string(&path).map_err(|e| ThemeError::ReadError(path.clone(), e))?;
            return Self::from_json(&path.display().to_string(), &content);
        }
        match name {
            "dark" => Self::from_json("dark (bundled)", DARK_THEME_JSON),
            "light" => Self::from_json("light (bundled)", LIGHT_THEME_JSON),
            other => Err(ThemeError::NotFound(other.to_string())),
        }
    }

    /// The bundled `dark` palette. Used as the safe fallback when
    /// a named theme fails to load. Panics in tests if the bundled
    /// JSON itself is invalid (which it won't be — it's a fixture
    /// covered by [`tests::dark_palette_loads`]).
    pub fn bundled_dark() -> Theme {
        Self::from_json("dark (bundled)", DARK_THEME_JSON)
            .expect("bundled dark.json must parse cleanly")
    }

    /// The bundled `light` palette. Companion to [`Theme::bundled_dark`].
    pub fn bundled_light() -> Theme {
        Self::from_json("light (bundled)", LIGHT_THEME_JSON)
            .expect("bundled light.json must parse cleanly")
    }

    /// List the theme names known to the loader: every built-in
    /// plus every `*.json` file in the user themes directory. Used
    /// by future `/theme` selector autocomplete; the loader doesn't
    /// otherwise care about discovery.
    pub fn available() -> Vec<String> {
        let mut names: Vec<String> = vec!["dark".to_string(), "light".to_string()];
        if let Some(dir) = user_themes_dir()
            && let Ok(read) = fs::read_dir(&dir)
        {
            for entry in read.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("json")
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                    && !names.iter().any(|n| n == stem)
                {
                    names.push(stem.to_string());
                }
            }
        }
        names.sort();
        names
    }
}

/// Return the path to a user-defined theme file. Returns `None`
/// when `$HOME` is unset (the loader silently skips the user dir
/// in that case).
fn user_theme_path(name: &str) -> Option<PathBuf> {
    user_themes_dir().map(|dir| dir.join(format!("{name}.json")))
}

/// Return `~/.aj/themes/`. Doesn't create the directory; callers
/// only ever read from it. `None` when `$HOME` is unset.
fn user_themes_dir() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".aj").join("themes"))
}

// ============================================================================
// ThemeHandle: shared, hot-swappable theme reference
// ============================================================================

/// Shared, hot-swappable handle to a [`Theme`]. The interactive
/// mode threads a single [`ThemeHandle`] through every theme
/// builder so a runtime palette swap (fs-watcher fires; a future
/// `/theme` selector picks a new theme) is visible to every
/// component without rebuilding any of them.
///
/// The closures returned by [`Self::fg_closure`] /
/// [`Self::bg_closure`] dereference through the inner [`RwLock`]
/// on each call, so an in-place [`Self::replace`] is enough to
/// re-skin everything that's still alive. Callers should follow
/// up with `tui.invalidate()` + `tui.request_render()` to push
/// the change to screen.
#[derive(Clone)]
pub struct ThemeHandle {
    inner: Arc<RwLock<Theme>>,
}

impl ThemeHandle {
    /// Wrap a freshly-loaded [`Theme`] in a sharable handle.
    pub fn new(theme: Theme) -> Self {
        Self {
            inner: Arc::new(RwLock::new(theme)),
        }
    }

    /// Replace the inner theme. The next render call from any
    /// component painted through the closures handed out by
    /// [`Self::fg_closure`] / [`Self::bg_closure`] picks up the
    /// new palette automatically; the host is responsible for
    /// invalidating cached render output (`tui.invalidate()`) and
    /// requesting a fresh frame (`tui.request_render()`).
    pub fn replace(&self, theme: Theme) {
        match self.inner.write() {
            Ok(mut guard) => *guard = theme,
            Err(poisoned) => {
                // Reader thread panicked while holding a read
                // lock — vanishingly unlikely (we never panic
                // inside [`Theme::fg`] / [`Theme::bg`]) but if it
                // happened we'd rather keep the binary alive with
                // the fresh palette than propagate the panic.
                let mut guard = poisoned.into_inner();
                *guard = theme;
            }
        }
    }

    /// Snapshot the active theme's name (e.g. `"dark"`,
    /// `"light"`, or the user-supplied `"<name>"`). Cheap clone of
    /// a `String`.
    pub fn name(&self) -> String {
        self.read().name().to_string()
    }

    /// Snapshot the active theme's [`ColorMode`].
    pub fn color_mode(&self) -> ColorMode {
        self.read().color_mode()
    }

    /// Build a foreground-painting closure. The closure resolves
    /// through the shared lock on each call so a [`Self::replace`]
    /// reskins it without reconstructing the widget that holds it.
    pub fn fg_closure(&self, token: ThemeColor) -> Arc<dyn Fn(&str) -> String> {
        let handle = Arc::clone(&self.inner);
        // The `aj-tui` theme structs hold `Arc<dyn Fn(&str) -> String>`
        // without `Send + Sync` bounds (the TUI thread is the only
        // consumer). `Arc<RwLock<Theme>>` is `Send + Sync`, but the
        // closure itself only needs to satisfy the trait object's
        // bounds, so this is fine.
        #[allow(clippy::arc_with_non_send_sync)]
        let closure: Arc<dyn Fn(&str) -> String> =
            Arc::new(move |s: &str| handle.read().expect("theme rwlock poisoned").fg(token, s));
        closure
    }

    /// Build a background-painting closure. Same hot-rebind
    /// semantics as [`Self::fg_closure`].
    pub fn bg_closure(&self, token: ThemeBg) -> Arc<dyn Fn(&str) -> String> {
        let handle = Arc::clone(&self.inner);
        #[allow(clippy::arc_with_non_send_sync)]
        let closure: Arc<dyn Fn(&str) -> String> =
            Arc::new(move |s: &str| handle.read().expect("theme rwlock poisoned").bg(token, s));
        closure
    }

    /// Read access to the underlying [`Theme`]. Used by callers
    /// that need to read a token directly without going through a
    /// closure (currently nothing in `aj` does this — exposed
    /// for completeness and tests).
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Theme> {
        self.inner.read().expect("theme rwlock poisoned")
    }
}

// ============================================================================
// Theme file watcher
// ============================================================================

/// Debounce window for fs notifications. Editors writing through
/// tempfile+rename produce a burst of events; coalescing them
/// avoids re-parsing the same file 3-4 times in a row.
const WATCHER_DEBOUNCE: Duration = Duration::from_millis(100);

/// Guard returned by [`watch_user_theme`]. Dropping it tears down
/// the notify watcher and the spawned debouncer task.
pub struct ThemeWatcherGuard {
    // The notify watcher stops as soon as it's dropped. We hold
    // it inside the guard so the caller can decide its lifetime.
    _watcher: notify::RecommendedWatcher,
}

/// Start watching `~/.aj/themes/<name>.json` for changes.
///
/// Returns `None` when:
/// - `name` refers to a bundled theme (`dark` / `light`) — built-
///   ins live inside the binary and can't be edited at runtime.
/// - `$HOME` is unset (no user themes directory).
/// - The user themes directory doesn't exist (nothing to watch).
/// - The notify backend fails to initialise (rare; usually a
///   resource-exhaustion scenario, e.g. `inotify` instances cap).
///
/// On a successful return, a debounced re-parse task emits a fresh
/// [`Theme`] through the returned receiver every time the file
/// settles after a write burst. Parse errors are swallowed
/// silently so a mid-save invalid-JSON snapshot doesn't blow away
/// the running palette.
///
/// The watcher targets the **directory** rather than the file
/// itself: editors that write through tempfile+rename invalidate
/// a file-level watch on the first event. Watching the directory
/// survives those rebinds; we filter the event paths to the
/// matching filename so unrelated files in the same directory are
/// ignored.
pub fn watch_user_theme(name: &str) -> Option<(ThemeWatcherGuard, UnboundedReceiver<Theme>)> {
    let dir = user_themes_dir()?;
    watch_user_theme_in_dir(&dir, name)
}

/// Same as [`watch_user_theme`] but with an explicit themes
/// directory. Exposed for the unit tests so they can drive the
/// watcher against a tempdir without touching `$HOME`.
pub(crate) fn watch_user_theme_in_dir(
    dir: &Path,
    name: &str,
) -> Option<(ThemeWatcherGuard, UnboundedReceiver<Theme>)> {
    // Skip bundled names — there's no on-disk file to watch (the
    // user can still place a `dark.json` in the user dir to
    // override the bundled palette, in which case we'd watch
    // that). Honour the override by checking for the file
    // existence rather than the name alone.
    let path = dir.join(format!("{name}.json"));
    if !path.exists() {
        return None;
    }
    let filename = path.file_name()?.to_owned();

    // Channel feeding from notify's callback to our debouncer
    // task. We don't care about the event payload; one zero-sized
    // ping is enough to schedule a reload.
    let (notify_tx, mut notify_rx) = unbounded_channel::<()>();
    // The notify callback may fire on a non-tokio thread (e.g.
    // the inotify reader thread); we filter event paths there to
    // keep the ping channel low-volume.
    let filename_for_cb = filename.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(event) => event,
            Err(err) => {
                tracing::debug!("theme watcher reported error: {err}");
                return;
            }
        };
        // Only react to data-changing events. Access-time
        // updates and metadata-only flips don't change the
        // file contents.
        if !matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        ) {
            return;
        }
        // Filter to events that touch our specific theme
        // file. Some platforms emit events without paths;
        // when that happens we still ping so the debouncer
        // can decide based on the file's current state.
        let mentions_us = event.paths.is_empty()
            || event
                .paths
                .iter()
                .any(|p| p.file_name() == Some(filename_for_cb.as_os_str()));
        if !mentions_us {
            return;
        }
        let _ = notify_tx.send(());
    })
    .ok()?;
    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .map_err(|e| {
            tracing::warn!("theme watcher failed to start: {e}");
            e
        })
        .ok()?;

    let (theme_tx, theme_rx) = unbounded_channel::<Theme>();
    let path_for_task = path.clone();
    let mode = ColorMode::detect();
    tokio::spawn(async move {
        loop {
            // Block until the first event arrives.
            if notify_rx.recv().await.is_none() {
                // Watcher dropped; the caller is winding down.
                break;
            }
            // Coalesce: sleep the debounce window, then drain
            // anything that arrived during it. This collapses
            // editor write bursts into one re-parse.
            tokio::time::sleep(WATCHER_DEBOUNCE).await;
            while notify_rx.try_recv().is_ok() {}

            // Quiet period elapsed; attempt the reload. We
            // tolerate transient ENOENT (editors momentarily
            // unlink before atomic-rename) and parse failures
            // (the user is mid-edit) by skipping this iteration
            // and waiting for the next event.
            let content = match fs::read_to_string(&path_for_task) {
                Ok(s) => s,
                Err(err) => {
                    tracing::debug!(
                        "theme watcher: read failed for {}: {err}",
                        path_for_task.display()
                    );
                    continue;
                }
            };
            let label = path_for_task.display().to_string();
            match Theme::from_json_with_mode(&label, &content, mode) {
                Ok(theme) => {
                    if theme_tx.send(theme).is_err() {
                        // Receiver dropped; the interactive mode
                        // has shut down.
                        break;
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        "theme watcher: parse failed for {}: {err}",
                        path_for_task.display()
                    );
                }
            }
        }
    });

    Some((ThemeWatcherGuard { _watcher: watcher }, theme_rx))
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
    /// each rendered row through [`Theme::bg`] with the
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
pub fn chat_theme(theme: &ThemeHandle) -> ChatTheme {
    ChatTheme {
        markdown: markdown_theme(theme),
        thinking_text: theme.fg_closure(ThemeColor::ThinkingText),
        user_message_bg: theme.bg_closure(ThemeBg::UserMessageBg),
        tool_pending_bg: theme.bg_closure(ThemeBg::ToolPendingBg),
        tool_success_bg: theme.bg_closure(ThemeBg::ToolSuccessBg),
        tool_error_bg: theme.bg_closure(ThemeBg::ToolErrorBg),
    }
}

/// Build the [`MarkdownTheme`] used by the assistant-message and
/// user-message renderers. Code-block bodies stay identity because
/// the bundled syntect highlighter colors per token inside the
/// block — wrapping it would interfere with its SGR resets.
pub fn markdown_theme(theme: &ThemeHandle) -> MarkdownTheme {
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
    }
}

// ============================================================================
// Editor border tints — thinking level / bash mode
// ============================================================================

/// Map a thinking level onto its dedicated [`ThemeColor`] token.
///
/// The mapping escalates visually with the model's reasoning
/// budget: `None` → muted `Off`; `Low` → soft blue; … →
/// `XHigh` / `Max` → strong magenta. `Max` is the highest value
/// the model layer exposes; the JSON theme schema tops out at
/// `ThinkingXhigh`, so the two highest levels share that tint —
/// both represent "the strongest reasoning the active model
/// supports" and the visual cue is the same intent.
fn thinking_color_token(level: Option<&ThinkingConfig>) -> ThemeColor {
    match level {
        None => ThemeColor::ThinkingOff,
        Some(ThinkingConfig::Low) => ThemeColor::ThinkingLow,
        Some(ThinkingConfig::Medium) => ThemeColor::ThinkingMedium,
        Some(ThinkingConfig::High) => ThemeColor::ThinkingHigh,
        Some(ThinkingConfig::XHigh) | Some(ThinkingConfig::Max) => ThemeColor::ThinkingXhigh,
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
    fn dark_palette_loads() {
        let theme = Theme::from_json_with_mode("dark", DARK_THEME_JSON, ColorMode::Truecolor)
            .expect("dark.json must parse");
        assert_eq!(theme.name(), "dark");
        // Spot check a couple of tokens to make sure the resolver
        // walked the var ref through to a concrete RGB.
        let accent = theme.fg(ThemeColor::Accent, "X");
        assert!(accent.contains("\x1b[38;2;138;190;183m"));
        // `text` is `""` which means "terminal default" — that
        // encodes as the foreground-reset escape, not a color set.
        let text = theme.fg(ThemeColor::Text, "X");
        assert!(text.starts_with("\x1b[39m"));
    }

    #[test]
    fn light_palette_loads() {
        let theme = Theme::from_json_with_mode("light", LIGHT_THEME_JSON, ColorMode::Truecolor)
            .expect("light.json must parse");
        assert_eq!(theme.name(), "light");
        // `accent` resolves to `teal` which is `#5a8080` —
        // verifies var refs walk through to a concrete RGB.
        let accent = theme.fg(ThemeColor::Accent, "X");
        assert!(accent.contains("\x1b[38;2;90;128;128m"));
    }

    #[test]
    fn ansi256_falls_back_to_palette_index_in_limited_terminal() {
        // 256-color mode downsamples hex values to palette
        // indexes. `#8abeb7` is teal-ish — it should land somewhere
        // in the 6x6x6 cube, not in the grayscale ramp.
        let theme = Theme::from_json_with_mode("dark", DARK_THEME_JSON, ColorMode::Color256)
            .expect("dark.json must parse");
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
    fn var_cycle_is_detected() {
        let json = r#"{
            "name": "cyclic",
            "vars": { "a": "b", "b": "a" },
            "colors": { "accent": "a",
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
                "bashMode": "" }
        }"#;
        let err = Theme::from_json_with_mode("cyclic", json, ColorMode::Truecolor)
            .expect_err("cyclic var refs must fail to load");
        assert!(
            matches!(err, ThemeError::VarCycle(_)),
            "expected VarCycle, got {err:?}"
        );
    }

    #[test]
    fn missing_required_color_is_reported() {
        // Schema requires `accent` — drop it and observe the error.
        let json = r#"{ "name": "broken", "vars": {}, "colors": {} }"#;
        let err = Theme::from_json_with_mode("broken", json, ColorMode::Truecolor)
            .expect_err("missing colors must fail to load");
        assert!(matches!(err, ThemeError::MissingColor(_)));
    }

    #[test]
    fn unknown_var_is_reported() {
        let json = r#"{
            "name": "broken",
            "vars": {},
            "colors": { "accent": "nope",
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
                "bashMode": "" }
        }"#;
        let err = Theme::from_json_with_mode("broken", json, ColorMode::Truecolor)
            .expect_err("unknown var ref must fail to load");
        assert!(matches!(err, ThemeError::UnknownVar(_)), "got {err:?}");
    }

    #[test]
    fn load_bundled_names() {
        // `Theme::load` consults the user dir first; absent any
        // file there it falls back to the bundled name. We test
        // the success path: both bundled names yield themes.
        let dark = Theme::load("dark");
        assert_eq!(dark.name(), "dark");
        let light = Theme::load("light");
        assert_eq!(light.name(), "light");
    }

    #[test]
    fn load_unknown_name_falls_back_to_dark() {
        // The user-friendly `load` swallows errors and uses dark
        // so the binary never fails to come up due to a typo.
        let theme = Theme::load("definitely-not-a-real-theme-name");
        assert_eq!(theme.name(), "dark");
    }

    #[test]
    fn builders_produce_themed_closures() {
        let handle = ThemeHandle::new(Theme::bundled_dark());
        let ml_theme = markdown_theme(&handle);
        // The heading closure should wrap text in the heading
        // foreground escape — `#f0c674` resolved via the dark
        // palette. Either as a 24-bit triple or a 256-color
        // index depending on the detected color mode of the
        // bundled theme.
        let painted = (ml_theme.heading)("hi");
        let has_truecolor = painted.contains("\x1b[38;2;240;198;116m");
        let has_256 = painted.contains("\x1b[38;5;");
        assert!(
            has_truecolor || has_256,
            "expected heading color escape, got {painted:?}"
        );
        // Bold/italic/etc. don't go through the theme — they
        // emit pure SGR style codes via aj_tui::style.
        let painted = (ml_theme.bold)("hi");
        assert!(painted.contains("\x1b[1m"));
    }

    #[test]
    fn available_lists_bundled_names() {
        let names = Theme::available();
        assert!(names.contains(&"dark".to_string()));
        assert!(names.contains(&"light".to_string()));
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
        // `XHigh` and `Max` both top out at the highest tint the
        // theme schema exposes (`ThinkingXhigh`) — they represent
        // the same "strongest reasoning available" intent.
        assert_eq!(
            thinking_color_token(Some(&ThinkingConfig::XHigh)),
            ThemeColor::ThinkingXhigh
        );
        assert_eq!(
            thinking_color_token(Some(&ThinkingConfig::Max)),
            ThemeColor::ThinkingXhigh
        );
    }

    /// The thinking-border closure paints with the resolved palette
    /// token for the requested level. `medium` resolves to dark's
    /// `#81a2be`, so the painted string must carry that escape.
    #[test]
    fn editor_border_color_for_thinking_paints_with_level_token() {
        let handle = ThemeHandle::new(
            Theme::from_json_with_mode("dark", DARK_THEME_JSON, ColorMode::Truecolor)
                .expect("dark.json must parse"),
        );
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
        let handle = ThemeHandle::new(
            Theme::from_json_with_mode("dark", DARK_THEME_JSON, ColorMode::Truecolor)
                .expect("dark.json must parse"),
        );
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
        let handle = ThemeHandle::new(
            Theme::from_json_with_mode("dark", DARK_THEME_JSON, ColorMode::Truecolor)
                .expect("dark.json must parse"),
        );
        let paint = editor_border_color_for_thinking(&handle, Some(&ThinkingConfig::High));
        let before = paint("─");
        // Dark's `thinkingHigh` resolves to `#b294bb`.
        assert!(
            before.contains("\x1b[38;2;178;148;187m"),
            "expected dark `high` tint before swap, got {before:?}"
        );

        handle.replace(
            Theme::from_json_with_mode("light", LIGHT_THEME_JSON, ColorMode::Truecolor)
                .expect("light.json must parse"),
        );
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
        let handle = ThemeHandle::new(
            Theme::from_json_with_mode("dark", DARK_THEME_JSON, ColorMode::Truecolor)
                .expect("dark.json must parse"),
        );
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
        let handle = ThemeHandle::new(
            Theme::from_json_with_mode("dark", DARK_THEME_JSON, ColorMode::Truecolor)
                .expect("dark.json must parse"),
        );
        let paint = handle.fg_closure(ThemeColor::Accent);

        let before = paint("X");
        // Dark's accent is `#8abeb7` — truecolor encoding.
        assert!(
            before.contains("\x1b[38;2;138;190;183m"),
            "expected dark accent escape before swap, got {before:?}"
        );

        // Swap to the light palette in-place.
        handle.replace(
            Theme::from_json_with_mode("light", LIGHT_THEME_JSON, ColorMode::Truecolor)
                .expect("light.json must parse"),
        );
        let after = paint("X");
        // Light's accent resolves through `teal` → `#5a8080`.
        assert!(
            after.contains("\x1b[38;2;90;128;128m"),
            "expected light accent escape after swap, got {after:?}"
        );
        // The dark prefix must be gone — otherwise we'd just be
        // concatenating both.
        assert!(
            !after.contains("\x1b[38;2;138;190;183m"),
            "stale dark escape leaked into post-swap output: {after:?}"
        );
    }

    #[test]
    fn theme_handle_name_tracks_replacement() {
        let handle = ThemeHandle::new(Theme::bundled_dark());
        assert_eq!(handle.name(), "dark");
        handle.replace(Theme::bundled_light());
        assert_eq!(handle.name(), "light");
    }

    // ------------------------------------------------------------
    // Theme file watcher
    // ------------------------------------------------------------

    /// Build a minimal JSON theme document with a specified
    /// `accent` color. Useful for watcher tests that need to
    /// produce a parseable file with a distinguishing field
    /// without spelling out the full 51-key schema each time.
    fn minimal_theme_json(name: &str, accent_hex: &str) -> String {
        format!(
            r#"{{
                "name": "{name}",
                "vars": {{}},
                "colors": {{
                    "accent": "{accent_hex}",
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
                }}
            }}"#
        )
    }

    #[tokio::test]
    async fn watch_user_theme_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir created");
        // Even though "nothing" sounds like a bundled name (it
        // isn't), the watcher only fires when the file actually
        // exists on disk — bundled names need a user override to
        // be watchable.
        let result = watch_user_theme_in_dir(dir.path(), "nothing-here");
        assert!(
            result.is_none(),
            "watcher must not start when the theme file is absent"
        );
    }

    #[tokio::test]
    async fn watch_user_theme_delivers_reparsed_theme_on_file_edit() {
        // End-to-end test: write a theme file, start the watcher,
        // modify the file, assert that a freshly-parsed `Theme`
        // arrives on the channel within a reasonable timeout.
        // Filesystem watch delivery is asynchronous so we give the
        // pipeline a generous 5-second budget; in practice it
        // resolves in <500ms on every backend we've tried.
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("custom.json");
        std::fs::write(&path, minimal_theme_json("custom", "#000000")).expect("write seed");

        let (_guard, mut rx) = watch_user_theme_in_dir(dir.path(), "custom")
            .expect("watcher should start when the file exists");

        // Trigger a modification. We use `fs::write` (which
        // truncates and writes) rather than tempfile+rename to
        // avoid relying on platform-specific atomic-rename
        // semantics in the test; the production watcher handles
        // both shapes because it watches the directory.
        std::fs::write(&path, minimal_theme_json("custom", "#ff0000")).expect("rewrite");

        let new_theme = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("watcher delivered a theme within 5s")
            .expect("channel was open");
        assert_eq!(new_theme.name(), "custom");
        // The new accent should be `#ff0000`; encoded as truecolor
        // when the host detects it, otherwise as a 256-color
        // approximation. Accept either since the test runs in
        // arbitrary terminals.
        let painted = new_theme.fg(ThemeColor::Accent, "X");
        let has_truecolor = painted.contains("\x1b[38;2;255;0;0m");
        let has_256 = painted.contains("\x1b[38;5;");
        assert!(
            has_truecolor || has_256,
            "expected red accent escape post-edit, got {painted:?}"
        );
    }

    #[tokio::test]
    async fn watch_user_theme_ignores_unrelated_files_in_same_dir() {
        // The watcher targets the directory, so unrelated files in
        // it (themes the user is editing for a *different*
        // session, log files, etc.) shouldn't trigger reloads.
        let dir = tempfile::tempdir().expect("tempdir created");
        let our_path = dir.path().join("ours.json");
        let other_path = dir.path().join("other.json");
        std::fs::write(&our_path, minimal_theme_json("ours", "#112233")).expect("write ours");

        let (_guard, mut rx) = watch_user_theme_in_dir(dir.path(), "ours")
            .expect("watcher should start when the file exists");

        // Write a *different* file. The watcher's filename filter
        // should drop this event.
        std::fs::write(&other_path, minimal_theme_json("other", "#445566")).expect("write other");

        // Wait through the debounce window plus a generous margin
        // for fs-event delivery, then assert no theme arrived. A
        // false positive here (a stray event) would surface as a
        // received message — which is exactly what we want to
        // catch.
        let result = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(
            result.is_err(),
            "watcher fired for an unrelated filename: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn watch_user_theme_swallows_parse_errors_silently() {
        // A mid-edit save can momentarily produce invalid JSON.
        // The watcher should *not* propagate the error; it should
        // wait for the next write that produces a parseable
        // document.
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("partial.json");
        std::fs::write(&path, minimal_theme_json("partial", "#000000")).expect("write seed");

        let (_guard, mut rx) = watch_user_theme_in_dir(dir.path(), "partial")
            .expect("watcher should start when the file exists");

        // First write: invalid JSON. The watcher should re-read,
        // fail to parse, and discard silently.
        std::fs::write(&path, "{ this is not valid json").expect("write invalid");
        // Give the debouncer time to fire and drop the bad event.
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Channel must still be empty.
        assert!(
            rx.try_recv().is_err(),
            "watcher must not surface a parse error as a Theme"
        );

        // Second write: valid again. Now we expect a delivery.
        std::fs::write(&path, minimal_theme_json("partial", "#abcdef")).expect("write valid");
        let recovered = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("watcher recovered within 5s")
            .expect("channel was open");
        assert_eq!(recovered.name(), "partial");
    }
}
