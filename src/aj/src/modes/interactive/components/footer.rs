//! Footer — persistent bottom status line.
//!
//! Renders a single dim row underneath the editor with the model
//! name, working directory, and a context-window occupancy
//! indicator. State is mutated through the typed setters below
//! so the event pump can refresh individual fields as `TurnUsage`
//! / `Notice` events arrive.
//!
//! Theming (the color thresholds on the context-occupancy
//! percentage) lives entirely inside this component so callers
//! pass raw data and don't have to know which palette the footer
//! is rendering against.
//!
//! See `docs/aj-next-plan.md` §4 — `components/footer.rs`.

use std::any::Any;

use aj_tui::ansi::truncate_to_width;
use aj_tui::component::Component;
use aj_tui::keys::InputEvent;
use aj_tui::style;

/// Snapshot describing how full the active model's context window
/// is. The footer renders this as `tokens/window (percent%)`,
/// coloring the percentage by occupancy.
///
/// `tokens.None` means "not yet known" — typically a fresh thread
/// before the first assistant turn — and renders as `?`. A
/// `context_window` of `0` suppresses the indicator entirely so
/// the footer stays silent for models with no published window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
}

/// Thin wrapper around a single dim text row.
pub struct Footer {
    /// Provider model name, e.g. `claude-sonnet-4`.
    model: Option<String>,
    /// Working directory, displayed as the host-relative path.
    cwd: Option<String>,
    /// Snapshot of context-window occupancy. The renderer turns
    /// this into a `12.3k/200k (6.1%)` style string and applies
    /// the threshold-based color to the percentage substring.
    context_usage: Option<ContextUsage>,
}

impl Footer {
    /// Build an empty footer. The component renders nothing until
    /// at least one of the setters has been called.
    pub fn new() -> Self {
        Self {
            model: None,
            cwd: None,
            context_usage: None,
        }
    }

    /// Replace the model-name field. `None` removes it.
    pub fn set_model(&mut self, model: Option<String>) {
        self.model = model;
    }

    /// Replace the working-directory field. `None` removes it.
    pub fn set_cwd(&mut self, cwd: Option<String>) {
        self.cwd = cwd;
    }

    /// Replace the context-window occupancy snapshot. `None`
    /// removes the indicator from the rendered row entirely.
    pub fn set_context_usage(&mut self, usage: Option<ContextUsage>) {
        self.context_usage = usage;
    }
}

impl Default for Footer {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Footer {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(m) = &self.model {
            parts.push(m.clone());
        }
        if let Some(c) = &self.cwd {
            parts.push(c.clone());
        }
        if let Some(u) = self.context_usage.as_ref()
            && let Some(rendered) = render_context_usage(*u)
        {
            parts.push(rendered);
        }
        if parts.is_empty() {
            return Vec::new();
        }
        // One-column left indent matches the header and the chat
        // scrollback's `padding_x = 1`, so the persistent status
        // bars share a left edge with the messages between them.
        //
        // Narrow-terminal handling: a long cwd or model name can
        // easily push the joined row past the terminal width, so
        // we truncate with an ellipsis instead of overflowing.
        // Wrapping would push the editor up a row and shift the
        // hardware cursor away from where the diff engine expects
        // it; a single truncated row keeps the layout stable.
        if width == 0 {
            return vec![String::new()];
        }
        let content = parts.join("  ·  ");
        let truncated = truncate_to_width(&content, width - 1, "…", false);
        vec![format!(" {}", style::dim(&truncated))]
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }
}

impl AsRef<dyn Any> for Footer {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

/// Format `usage` as e.g. `"12.3k/200k (6.1%)"`. The percentage
/// substring is colored by occupancy — yellow above 70%, red
/// above 90%, otherwise uncolored so it inherits the outer dim
/// wrap applied in [`Component::render`]. Returns `None` when
/// `context_window` is 0; the footer has nothing meaningful to
/// say in that case and the indicator drops out of the row.
///
/// The color helpers ([`style::yellow`], [`style::red`]) close
/// their span with `\x1b[39m` (foreground reset only), not the
/// all-reset `\x1b[0m`. That preserves the surrounding
/// `style::dim` from `render`, so the colored substring still
/// reads as dim-yellow / dim-red rather than a vivid pop.
///
/// Token counts large enough to lose precision in the `u64 ->
/// f64` cast (>2^53 tokens) are well past any model's published
/// context window.
#[allow(clippy::as_conversions)]
fn render_context_usage(usage: ContextUsage) -> Option<String> {
    if usage.context_window == 0 {
        return None;
    }
    let window_str = format_tokens(usage.context_window);
    match usage.tokens {
        None => Some(format!("?/{window_str}")),
        Some(tokens) => {
            let tokens_str = format_tokens(tokens);
            let percent = (tokens as f64 / usage.context_window as f64) * 100.0;
            let pct_str = format!("({percent:.1}%)");
            let pct_colored = if percent > 90.0 {
                style::red(&pct_str)
            } else if percent > 70.0 {
                style::yellow(&pct_str)
            } else {
                pct_str
            };
            Some(format!("{tokens_str}/{window_str} {pct_colored}"))
        }
    }
}

/// Compact token-count formatter: `987` → `"987"`, `2_500` →
/// `"2.5k"`, `247_321` → `"247k"`, `2_500_000` → `"2.5M"`,
/// `12_000_000` → `"12M"`. One decimal at the low end of each
/// scale, integer at the high end — keeps the rendered string
/// narrow without losing useful precision.
///
/// Counts large enough to lose precision in the `u64 -> f64`
/// cast (>2^53) are well past anything a context window or a
/// realistic session would reach.
#[allow(clippy::as_conversions)]
fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        format!("{n}")
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n < 1_000_000 {
        // Half-up rounding to the nearest thousand. Integer
        // division truncates, so adding 500 first picks the
        // closer thousand without floating-point.
        format!("{}k", (n + 500) / 1_000)
    } else if n < 10_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{}M", (n + 500_000) / 1_000_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_footer_renders_nothing() {
        let mut f = Footer::new();
        assert!(f.render(80).is_empty());
    }

    #[test]
    fn populated_footer_joins_fields_with_separator() {
        let mut f = Footer::new();
        f.set_model(Some("claude-sonnet-4".into()));
        f.set_cwd(Some("/home/user/proj".into()));
        let lines = f.render(80);
        assert_eq!(lines.len(), 1);
        let visible = lines[0].replace("\x1b[2m", "").replace("\x1b[22m", "");
        assert!(visible.contains("claude-sonnet-4"));
        assert!(visible.contains("/home/user/proj"));
        assert!(visible.contains("·"));
    }

    /// Regression: a long cwd or model name on a narrow terminal
    /// used to overflow because `render` ignored `width`. Mirrors
    /// the equivalent test in `header.rs`.
    #[test]
    fn rendered_lines_never_exceed_width_for_any_width() {
        let mut f = Footer::new();
        f.set_model(Some("claude-sonnet-4-very-long-model-name".into()));
        f.set_cwd(Some("/home/user/some/deeply/nested/project/path".into()));
        f.set_context_usage(Some(ContextUsage {
            tokens: Some(12_345),
            context_window: 200_000,
        }));
        for width in [0usize, 1, 2, 10, 40, 64, 80, 200] {
            let lines = f.render(width);
            for (i, line) in lines.iter().enumerate() {
                let w = aj_tui::ansi::visible_width(line);
                assert!(
                    w <= width,
                    "line {i} exceeds width {width}: visible_width = {w}, line = {line:?}",
                );
            }
        }
    }

    /// Sanity-check the scale-aware token formatter at each band
    /// boundary so a refactor can't silently change displayed
    /// values.
    #[test]
    fn format_tokens_spans_all_bands() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(987), "987");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(2_500), "2.5k");
        assert_eq!(format_tokens(9_999), "10.0k");
        assert_eq!(format_tokens(10_000), "10k");
        assert_eq!(format_tokens(247_321), "247k");
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(2_500_000), "2.5M");
        assert_eq!(format_tokens(12_000_000), "12M");
    }

    #[test]
    fn context_usage_renders_unknown_as_question_mark() {
        let s = render_context_usage(ContextUsage {
            tokens: None,
            context_window: 200_000,
        })
        .expect("non-empty window should render");
        assert_eq!(s, "?/200k");
    }

    #[test]
    fn context_usage_with_zero_window_renders_nothing() {
        assert!(
            render_context_usage(ContextUsage {
                tokens: Some(1_000),
                context_window: 0,
            })
            .is_none(),
            "a 0-token context window suppresses the indicator",
        );
    }

    /// Below the warning threshold the percentage substring is
    /// uncolored (it inherits the outer dim wrap applied in
    /// `render`). Pin that so future palette additions don't
    /// accidentally pre-colour low occupancy values.
    #[test]
    fn context_usage_low_occupancy_is_uncoloured() {
        let s = render_context_usage(ContextUsage {
            tokens: Some(20_000),
            context_window: 200_000,
        })
        .expect("rendered");
        assert_eq!(s, "20k/200k (10.0%)");
    }

    /// Yellow kicks in strictly above 70%. Use a value just past
    /// the boundary so a single-line numeric tweak shows up as a
    /// failure.
    #[test]
    fn context_usage_above_70_percent_is_yellow() {
        // 140_001 / 200_000 = 70.0005% > 70%.
        let s = render_context_usage(ContextUsage {
            tokens: Some(140_001),
            context_window: 200_000,
        })
        .expect("rendered");
        // Yellow span is SGR-33 ... SGR-39.
        assert!(s.contains("\x1b[33m"), "expected yellow open in {s:?}");
        assert!(s.contains("\x1b[39m"), "expected colour close in {s:?}");
        // The numeric part lives inside the colour span.
        assert!(s.contains("(70.0%)") || s.contains("(70.1%)"));
    }

    /// Red kicks in strictly above 90%. Pin the colour with a
    /// value past the boundary.
    #[test]
    fn context_usage_above_90_percent_is_red() {
        // 180_001 / 200_000 = 90.0005% > 90%.
        let s = render_context_usage(ContextUsage {
            tokens: Some(180_001),
            context_window: 200_000,
        })
        .expect("rendered");
        // Red span is SGR-31 ... SGR-39.
        assert!(s.contains("\x1b[31m"), "expected red open in {s:?}");
        assert!(s.contains("\x1b[39m"), "expected colour close in {s:?}");
    }
}
