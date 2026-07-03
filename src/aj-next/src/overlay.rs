//! The overlay/modal substrate for the alt-screen shell (Spec E §5, §6).
//!
//! `vxfw` has no built-in modal system, so the shell composes one from
//! z-indexed `SubSurface`s and focus: the host owns a stack of open overlays,
//! and while it is non-empty the Shell's draw appends a full-viewport
//! [`Scrim`] plus the top overlay's widget above the base layout. Only the
//! top overlay is drawn, matching `aj`'s "push hides the parent" behavior.
//!
//! Focus contract: every stack mutation must move focus explicitly via
//! `EventContext::request_focus`. The framework's `FocusHandler` falls back
//! to the ROOT when the focused widget vanishes from the tree, so popping an
//! overlay without restoring focus would leave keystrokes dispatching along a
//! root-only path instead of reaching the editor.

use std::cell::RefCell;
use std::rc::Rc;

use aj_app::theme::{Theme, ThemeColor};
use vaxis::cell::Style;
use vaxis::vxfw::{
    DrawContext, Event, EventContext, RelativePoint, Size, Surface, Widget, WidgetRef,
};

use crate::transcript::vaxis_color;

/// Chrome rows an overlay window adds around its inner content, mirrored
/// from the vaxis `OverlayWindow` frame (top/bottom border plus padding).
const OVERLAY_CHROME_ROWS: u16 = vaxis::vxfw::OVERLAY_WINDOW_CHROME_ROWS;

/// Inner-content row budget of a small overlay. Sized so the command palette
/// shows its whole catalog without scrolling. The box's on-screen footprint
/// is this plus the chrome rows.
const SMALL_OVERLAY_INNER_ROWS: u16 = 22;

/// Floor and ceiling for a large overlay's inner-content rows. The floor
/// keeps the box usable on a standard 24-row terminal, the ceiling stops it
/// from swallowing the whole screen on a very tall one.
const LARGE_OVERLAY_MIN_INNER_ROWS: u16 = 14;
const LARGE_OVERLAY_MAX_INNER_ROWS: u16 = 32;

/// How an overlay is sized and anchored, ported from `aj`'s compositor
/// options into a placement the Shell resolves against the live terminal
/// size each frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayPlacement {
    /// The command palette and the compact pickers: centered, ~75% of the
    /// terminal width clamped to [72, 100] columns so the box doesn't
    /// stretch uncomfortably wide on large monitors, at a fixed height of
    /// 22 inner rows plus chrome.
    Small,
    /// The content-heavy overlays (session switcher, prompt history):
    /// centered, ~85% of the terminal width clamped to [72, 120] columns,
    /// with inner rows at ~80% of the terminal height minus chrome, clamped
    /// to [14, 32].
    // No overlay uses this placement yet (the selectors that need it are
    // the next port); the geometry is exercised by tests below.
    #[allow(dead_code)]
    Large,
}

/// Resolves a percentage of a cell dimension, rounded to nearest.
///
/// Terminal dimensions stay far below the f32 precision threshold (2^24),
/// so the lossy casts round-trip exactly in practice.
#[allow(clippy::as_conversions)]
fn percent(reference: u16, p: f32) -> u16 {
    (f32::from(reference) * p / 100.0).round() as u16
}

impl OverlayPlacement {
    /// Computes the overlay's origin and size for a terminal of `term`
    /// cells: clamp the width band, cap the height to the terminal, then
    /// center.
    pub(crate) fn resolve(&self, term: Size) -> (RelativePoint, Size) {
        let (width, height) = match self {
            OverlayPlacement::Small => (
                percent(term.width, 75.0).clamp(72, 100),
                SMALL_OVERLAY_INNER_ROWS + OVERLAY_CHROME_ROWS,
            ),
            OverlayPlacement::Large => {
                let inner = percent(term.height, 80.0)
                    .saturating_sub(OVERLAY_CHROME_ROWS)
                    .clamp(LARGE_OVERLAY_MIN_INNER_ROWS, LARGE_OVERLAY_MAX_INNER_ROWS);
                (
                    percent(term.width, 85.0).clamp(72, 120),
                    inner + OVERLAY_CHROME_ROWS,
                )
            }
        };
        let size = Size {
            width: width.min(term.width).max(1),
            height: height.min(term.height).max(1),
        };
        let origin = RelativePoint {
            row: i32::from(term.height.saturating_sub(size.height) / 2),
            col: i32::from(term.width.saturating_sub(size.width) / 2),
        };
        (origin, size)
    }
}

/// One level of the modal stack.
pub(crate) struct OpenOverlay {
    /// The overlay's root widget (typically an `OverlayWindow`), drawn as a
    /// `SubSurface` above the scrim while this level is on top.
    pub(crate) widget: WidgetRef,
    /// The widget that receives focus while this level is on top (an inner
    /// filter field, say), re-focused when a pushed child is popped.
    pub(crate) focus: WidgetRef,
    pub(crate) placement: OverlayPlacement,
}

/// The modal stack: plain host state the Shell reads at draw time.
///
/// Mutations only move `Vec` entries. The callers own the focus movement
/// (see the module docs) and the redraw request.
#[derive(Default)]
pub(crate) struct OverlayStack {
    levels: Vec<OpenOverlay>,
}

impl OverlayStack {
    pub(crate) fn is_open(&self) -> bool {
        !self.levels.is_empty()
    }

    /// The overlay currently drawn and focused.
    pub(crate) fn top(&self) -> Option<&OpenOverlay> {
        self.levels.last()
    }

    /// Adds a level on top. Parents stay on the stack, so a later
    /// [`back`](Self::back) returns to them.
    pub(crate) fn push(&mut self, overlay: OpenOverlay) {
        self.levels.push(overlay);
    }

    /// Pops the top level, returning the focus target of the uncovered
    /// parent, or `None` when the stack emptied (focus then belongs to the
    /// editor).
    pub(crate) fn back(&mut self) -> Option<WidgetRef> {
        self.levels.pop();
        self.top().map(|o| Rc::clone(&o.focus))
    }

    /// Tears the whole stack down (a terminal confirm or the close-all
    /// chord). Focus then belongs to the editor.
    // The close-all chord arrives with the keymap wiring, but the teardown
    // is part of the stack's contract already.
    #[allow(dead_code)]
    pub(crate) fn close_all(&mut self) {
        self.levels.clear();
    }
}

/// The modal backdrop: a full-viewport transparent layer drawn between the
/// base layout and the top overlay.
///
/// Visually it paints nothing, so the overlay window floats over the fully
/// visible base layout, the same composition `aj` uses (no backdrop).
/// Behaviorally it consumes every mouse event that targets it, blocking
/// clicks and wheel scrolls from reaching the widgets underneath.
///
/// NOTE: mouse blocking happens at-target and in bubbling, not in the
/// capturing phase. Hit-testing collects every widget under the pointer, so
/// base widgets that intersect the point still see the event in their
/// capturing phase before the scrim (the deepest hit) consumes it at-target.
/// A base widget that acts on capture-phase mouse observation (the
/// transcript's follow-tail disengage on wheel-up, say) still reacts. That
/// leakage is a known quirk of composing modality from z-order alone.
pub(crate) struct Scrim;

impl Widget for Scrim {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // An empty buffer paints nothing, so the base layout stays fully
        // visible under the floating overlay, matching how aj composites
        // its overlay windows with no backdrop. The full-size geometry
        // still participates in hit-testing, which is the scrim's job.
        Surface {
            size: ctx.max.size(),
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children: Vec::new(),
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        // Clicks and wheel scrolls stop here instead of bubbling into the
        // base layout. Nothing changed visually, so no redraw.
        if let Event::Mouse(_) = event {
            ctx.consume_event();
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Frame styles for overlay windows, resolved once from the theme with the
/// same token mapping `aj` uses: a muted border, a bold accent title, and a
/// dim key-hint subtitle.
pub(crate) struct OverlayChrome {
    pub(crate) border: Style,
    pub(crate) title: Style,
    pub(crate) subtitle: Style,
}

impl OverlayChrome {
    pub(crate) fn from_theme(theme: &Theme) -> OverlayChrome {
        let fg = |token: ThemeColor| Style {
            fg: vaxis_color(theme.fg_color(token)),
            ..Style::default()
        };
        OverlayChrome {
            border: fg(ThemeColor::BorderMuted),
            title: Style {
                bold: true,
                ..fg(ThemeColor::Accent)
            },
            subtitle: fg(ThemeColor::Dim),
        }
    }
}

/// Pops the top overlay and moves focus to the uncovered parent's target,
/// or to `fallback` (the editor) when the stack emptied. See the module
/// docs for why the explicit focus move is mandatory.
pub(crate) fn close_top(
    stack: &Rc<RefCell<OverlayStack>>,
    ctx: &mut EventContext,
    fallback: &WidgetRef,
) {
    let parent_focus = stack.borrow_mut().back();
    ctx.request_focus(parent_focus.unwrap_or_else(|| Rc::clone(fallback)));
    ctx.redraw = true;
}

/// Placeholder rows for the proof-of-mechanism palette. The real command
/// catalog (and its dispatch) arrives with the selector port. These only
/// exercise the overlay stack end to end.
///
/// Row shape mirrors the real palette: the display label is the stable
/// `{category}  {title}` columns, the filter key is `{category} {title}` so
/// typing a category surfaces its whole group.
pub(crate) fn placeholder_palette_items() -> Vec<vaxis::vxfw::SelectItem> {
    [
        ("model", "Switch model"),
        ("model", "Set thinking level"),
        ("session", "Switch session"),
        ("session", "Compact conversation"),
        ("help", "Show help"),
        ("app", "Quit"),
    ]
    .into_iter()
    .map(|(category, title)| {
        vaxis::vxfw::SelectItem::new(
            format!("{category:<10} {title}"),
            format!("{category} {title}"),
        )
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use vaxis::mouse;

    use super::*;

    fn size(width: u16, height: u16) -> Size {
        Size { width, height }
    }

    #[test]
    fn small_placement_clamps_width_and_pins_height() {
        // 80x24: 75% of 80 is 60, below the 72-column floor. The 26-row box
        // is taller than the terminal, so it caps at 24 and pins to row 0.
        let (origin, sz) = OverlayPlacement::Small.resolve(size(80, 24));
        assert_eq!(sz, size(72, 24));
        assert_eq!(origin, RelativePoint { row: 0, col: 4 });

        // 200x50: 75% of 200 is 150, above the 100-column ceiling.
        let (origin, sz) = OverlayPlacement::Small.resolve(size(200, 50));
        assert_eq!(sz, size(100, 26));
        assert_eq!(origin, RelativePoint { row: 12, col: 50 });

        // 120x40: 75% of 120 is 90, inside the band.
        let (origin, sz) = OverlayPlacement::Small.resolve(size(120, 40));
        assert_eq!(sz, size(90, 26));
        assert_eq!(origin, RelativePoint { row: 7, col: 15 });

        // Narrower than the floor: the terminal wins.
        let (origin, sz) = OverlayPlacement::Small.resolve(size(60, 30));
        assert_eq!(sz, size(60, 26));
        assert_eq!(origin, RelativePoint { row: 2, col: 0 });
    }

    #[test]
    fn large_placement_scales_height_with_the_terminal() {
        // 80x24: 85% of 80 is 68, below the 72 floor. Inner rows:
        // 80% of 24 is 19, minus 4 chrome is 15, inside [14, 32].
        let (origin, sz) = OverlayPlacement::Large.resolve(size(80, 24));
        assert_eq!(sz, size(72, 19));
        assert_eq!(origin, RelativePoint { row: 2, col: 4 });

        // 200x60: width caps at 120. Inner rows 80% of 60 minus 4 is 44,
        // capped at 32, so the box is 36 tall.
        let (origin, sz) = OverlayPlacement::Large.resolve(size(200, 60));
        assert_eq!(sz, size(120, 36));
        assert_eq!(origin, RelativePoint { row: 12, col: 40 });

        // Very short terminal: the 14-row inner floor plus chrome exceeds
        // the 16 rows available, so the box truncates to the terminal.
        let (origin, sz) = OverlayPlacement::Large.resolve(size(100, 16));
        assert_eq!(sz, size(85, 16));
        assert_eq!(origin, RelativePoint { row: 0, col: 7 });
    }

    #[test]
    fn stack_back_returns_the_parent_focus_target() {
        let mut stack = OverlayStack::default();
        assert!(!stack.is_open());
        assert!(stack.back().is_none());

        let level = || {
            let w: WidgetRef = Rc::new(RefCell::new(Scrim));
            OpenOverlay {
                widget: Rc::clone(&w),
                focus: w,
                placement: OverlayPlacement::Small,
            }
        };
        let parent = level();
        let parent_focus = Rc::clone(&parent.focus);
        stack.push(parent);
        stack.push(level());
        assert!(stack.is_open());

        // Popping the child uncovers the parent and names its focus target.
        let focus = stack.back().expect("parent focus target");
        assert!(vaxis::vxfw::widget_eq(&focus, &parent_focus));
        // Popping the last level empties the stack: focus falls to the
        // editor, which the caller owns.
        assert!(stack.back().is_none());
        assert!(!stack.is_open());

        stack.push(level());
        stack.push(level());
        stack.close_all();
        assert!(!stack.is_open());
    }

    #[test]
    fn scrim_consumes_mouse_events_and_ignores_keys() {
        let mut scrim = Scrim;
        let mut ctx = EventContext::new();
        let mouse_event = Event::Mouse(mouse::Mouse {
            col: 3,
            row: 3,
            xoffset: 0,
            yoffset: 0,
            button: mouse::Button::WheelUp,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Press,
        });
        scrim.handle_event(&mut ctx, &mouse_event);
        assert!(ctx.consume_event, "mouse is blocked from bubbling");

        let mut ctx = EventContext::new();
        scrim.handle_event(
            &mut ctx,
            &Event::KeyPress(vaxis::key::Key {
                codepoint: u32::from('x'),
                ..vaxis::key::Key::default()
            }),
        );
        assert!(!ctx.consume_event, "keys route by focus, not by the scrim");
    }
}
