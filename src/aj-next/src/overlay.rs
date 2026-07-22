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

use aj_app::keybindings::{
    ACTION_OVERLAY_CLOSE_ALL, action_shortcut, fixed_keys, format_keybinding,
};
use aj_app::theme::{Theme, ThemeBg, ThemeColor};
use vaxis::cell::Style;
use vaxis::mouse;
use vaxis::vxfw::{
    DrawContext, Event, EventContext, RelativePoint, SelectStyles, Size, Surface, Widget, WidgetRef,
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
    /// The content-heavy overlays (help, auth status, session info,
    /// usage): centered, ~85% of the terminal width clamped to
    /// [72, 120] columns, with inner rows at ~80% of the terminal
    /// height minus chrome, clamped to [14, 32].
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

    /// Number of overlays currently stacked. Used by tests to assert the
    /// chaining depth (palette plus a child it opened).
    #[cfg(test)]
    pub(crate) fn depth(&self) -> usize {
        self.levels.len()
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

/// An event target for transient surfaces painted above base content.
pub(crate) struct MouseBlocker {
    on_mouse: Box<dyn FnMut()>,
}

impl MouseBlocker {
    pub(crate) fn new(on_mouse: Box<dyn FnMut()>) -> MouseBlocker {
        MouseBlocker { on_mouse }
    }
}

impl Widget for MouseBlocker {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        Surface::with_size(ctx.max.size())
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::Mouse(m) = event else {
            return;
        };
        (self.on_mouse)();
        if !matches!(
            m.button,
            mouse::Button::WheelUp
                | mouse::Button::WheelDown
                | mouse::Button::WheelLeft
                | mouse::Button::WheelRight
        ) {
            ctx.consume_event();
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Frame styles for overlay windows, resolved once from the theme with the
/// same token mapping `aj` uses: a muted border, a bold accent title, and a
/// muted key-hint subtitle. Carries the pick-list row styles too so a list
/// overlay (palette, selectors, settings) draws its selection band from the
/// same palette snapshot.
#[derive(Clone)]
pub(crate) struct OverlayChrome {
    pub(crate) border: Style,
    pub(crate) title: Style,
    pub(crate) subtitle: Style,
    pub(crate) select: SelectStyles,
}

impl OverlayChrome {
    pub(crate) fn from_theme(theme: &Theme) -> OverlayChrome {
        let mode = theme.color_mode();
        let fg = |token: ThemeColor| Style {
            fg: vaxis_color(theme.fg_color(token), mode),
            ..Style::default()
        };
        OverlayChrome {
            border: fg(ThemeColor::BorderMuted),
            title: Style {
                bold: true,
                ..fg(ThemeColor::Accent)
            },
            subtitle: fg(ThemeColor::Muted),
            select: select_styles_from_theme(theme),
        }
    }
}

/// Shared pick-list row styles from the theme, used by every list overlay via
/// [`OverlayChrome::select`]: the E-7 full-width band over `ThemeBg::SelectedBg`
/// with normal text on top, and a muted secondary column (the description).
///
/// The prefix (category) column and the secondary column both use `Muted`, and
/// the shortcut column is the `KeybindingHint` token drawn bold: coloring the
/// shortcut with the hint token is the ratified E-10 aj-next divergence from
/// `aj`. Only the palette sets a prefix or shortcut, so those columns are inert
/// for the other overlays. The label is plain here (the shared default); the
/// palette bolds its own copy on top, so bold labels are palette-only (see
/// `crate::palette::open_palette`).
///
/// The filter marker uses `Muted` too, so the `> ` prompt before the query
/// input reads as subtle chrome rather than competing with the query text.
pub(crate) fn select_styles_from_theme(theme: &Theme) -> SelectStyles {
    let mode = theme.color_mode();
    let fg = |token: ThemeColor| Style {
        fg: vaxis_color(theme.fg_color(token), mode),
        ..Style::default()
    };
    SelectStyles {
        selected_bg: vaxis_color(theme.bg_color(ThemeBg::SelectedBg), mode),
        label: fg(ThemeColor::Text),
        prefix: fg(ThemeColor::Muted),
        shortcut: Style {
            bold: true,
            ..fg(ThemeColor::KeybindingHint)
        },
        secondary: fg(ThemeColor::Muted),
        scrollbar_thumb: fg(ThemeColor::Muted),
        marker: fg(ThemeColor::Muted),
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

/// Tears the whole overlay stack down and moves focus to `fallback`
/// (the editor). A terminal confirm returns to the transcript, not to
/// any parent overlay, so it clears the stack rather than popping one.
pub(crate) fn close_all(
    stack: &Rc<RefCell<OverlayStack>>,
    ctx: &mut EventContext,
    fallback: &WidgetRef,
) {
    stack.borrow_mut().close_all();
    ctx.request_focus(Rc::clone(fallback));
    ctx.redraw = true;
}

// ============================================================================
// Overlay subtitles (key-hint labels)
// ============================================================================
//
// The single source for every overlay's key-hint subtitle, so the palette,
// the read-only content pages, the login dialog, and the selectors read one
// wording rather than each formatting its own literal (Spec F: hint labels
// resolved, never hardcoded).
//
// NOTE: Esc/Enter are FIXED `vxfw` widget conventions here. The overlay
// widgets (`ContentOverlay`, `FilterableSelect`, `LoginDialog`) hardcode Esc
// to dismiss and Enter to confirm/submit, so those chords are not registered
// as rebindable actions in `aj_app`'s vocabulary. We still resolve the
// *labels* through `format_keybinding`, a single formatting source, so a
// display spelling can't drift from a raw "Esc"/"Enter" literal. The close-all
// chord IS a keymap action (see `crate::keymap`), so it resolves through
// `action_shortcut`, and the copy chord is the fixed Ctrl+Y
// convention. Making the in-widget Esc/Enter *handling* rebindable is a
// tracked follow-up.

/// The fixed confirm-action label (`Enter`), resolved through the keybinding
/// data. Enter is a fixed `vxfw` widget convention (see the NOTE above), so
/// only the label resolves here, never the handling. This is the single home
/// for the confirm-label spelling and its canonical `"enter"` chord string.
pub(crate) fn confirm_key_label() -> String {
    format_keybinding("enter")
}

/// The fixed close/cancel-action label (`Esc`), resolved through the
/// keybinding data. Esc is a fixed `vxfw` widget convention (see the NOTE
/// above), so only the label resolves here, never the handling. This is the
/// single home for the close-label spelling and its canonical `"escape"`
/// chord string.
pub(crate) fn close_key_label() -> String {
    format_keybinding("escape")
}

/// Subtitle for the read-only content pages (help, auth status, session
/// info, usage): just how to close.
///
/// Reads `"{cancel} to close"`, or a `"{cancel} back  \u{2022}  {close} close"`
/// split when a distinct close-all chord resolves (the content overlay is a
/// modal, so the close-all chord tears the whole stack down while Esc returns
/// to the parent).
pub(crate) fn subtitle_close() -> String {
    let cancel = close_key_label();
    match action_shortcut(ACTION_OVERLAY_CLOSE_ALL) {
        Some(close_all) if close_all != cancel => {
            format!("{cancel} back  \u{2022}  {close_all} close")
        }
        _ => format!("{cancel} to close"),
    }
}

/// Subtitle for confirmable pick-list overlays (command palette, selectors):
/// how to confirm the highlighted row and how to close.
///
/// Reads `"{confirm} to confirm  \u{2022}  {cancel} to close"`, splitting the
/// close hint into `"{cancel} back  \u{2022}  {close} close"` when a distinct
/// close-all chord resolves. The `close` wording is shared with every
/// confirmable overlay so the visual language stays uniform.
pub(crate) fn subtitle_confirm_close() -> String {
    let confirm = confirm_key_label();
    let cancel = close_key_label();
    match action_shortcut(ACTION_OVERLAY_CLOSE_ALL) {
        Some(close_all) if close_all != cancel => {
            format!("{confirm} to confirm  \u{2022}  {cancel} back  \u{2022}  {close_all} close")
        }
        _ => format!("{confirm} to confirm  \u{2022}  {cancel} to close"),
    }
}

/// Subtitle for the OAuth login dialog: how to copy the URL, submit a pasted
/// code, and cancel. Ctrl+Y is the fixed copy convention, submit/cancel resolve
/// through `format_keybinding`.
pub(crate) fn subtitle_login() -> String {
    let copy = fixed_keys::CTRL_Y;
    let submit = confirm_key_label();
    let cancel = close_key_label();
    format!(
        "{copy} to copy URL  \u{2022}  {submit} to submit pasted code  \u{2022}  {cancel} to cancel"
    )
}

/// Subtitle for the stay-open editing windows (settings, skills): how to act
/// on the highlighted row and how to close. `verb` is the per-window
/// activation word (`"edit"`, `"toggle"`).
///
/// Reads `"{confirm} to {verb}  \u{2022}  {cancel} to close"`.
pub(crate) fn subtitle_edit_close(verb: &str) -> String {
    // NOTE: unlike the dismissable modals above, these windows keep the simple
    // "to close" form even though the close-all chord also tears them down.
    // Surfacing the `{cancel} back  \u{2022}  {close} close` split here is a
    // deferred wording decision, not a label-resolution gap. Space is not an
    // activation alias in `SettingList`, so the hint names only the resolved
    // confirm chord.
    let confirm = confirm_key_label();
    let cancel = close_key_label();
    format!("{confirm} to {verb}  \u{2022}  {cancel} to close")
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

    #[test]
    fn transient_blocker_consumes_buttons_but_leaves_wheel_scrolling_available() {
        let seen = Rc::new(std::cell::Cell::new(0));
        let seen_c = Rc::clone(&seen);
        let mut blocker = MouseBlocker::new(Box::new(move || seen_c.set(seen_c.get() + 1)));
        let event = |button| {
            Event::Mouse(mouse::Mouse {
                col: 0,
                row: 0,
                xoffset: 0,
                yoffset: 0,
                button,
                mods: mouse::Modifiers::empty(),
                kind: mouse::Type::Press,
            })
        };

        let mut ctx = EventContext::new();
        blocker.handle_event(&mut ctx, &event(mouse::Button::Left));
        assert!(
            ctx.consume_event,
            "button input cannot reach obscured content"
        );

        let mut ctx = EventContext::new();
        blocker.handle_event(&mut ctx, &event(mouse::Button::WheelUp));
        assert!(!ctx.consume_event, "wheel input keeps its existing routing");
        assert_eq!(seen.get(), 2, "both events interrupt pending clicks");
    }

    /// The subtitle builders resolve their labels through the keybinding
    /// data, never a raw literal. The expectations are themselves derived
    /// from `format_keybinding`/`action_shortcut`, so a rebind moves
    /// both the rendered label and the assertion together.
    #[test]
    fn subtitle_builders_resolve_labels_from_binding_data() {
        let cancel = format_keybinding("escape");
        let confirm = format_keybinding("enter");
        let close_all = action_shortcut(ACTION_OVERLAY_CLOSE_ALL);

        // Read-only pages: the content overlay's close hint.
        let close = subtitle_close();
        assert!(!close.is_empty());
        assert!(close.contains(&cancel), "{close}");
        match &close_all {
            Some(k) if *k != cancel => {
                assert_eq!(close, format!("{cancel} back  \u{2022}  {k} close"));
                assert!(close.contains(k), "{close}");
            }
            _ => assert_eq!(close, format!("{cancel} to close")),
        }

        // Command palette / selectors: confirm plus close.
        let confirm_close = subtitle_confirm_close();
        assert!(!confirm_close.is_empty());
        assert!(confirm_close.contains(&confirm), "{confirm_close}");
        assert!(confirm_close.contains(&cancel), "{confirm_close}");
        if let Some(k) = &close_all
            && *k != cancel
        {
            assert!(confirm_close.contains(k), "{confirm_close}");
        }

        // Login dialog: the fixed copy label plus the resolved submit/cancel.
        let login = subtitle_login();
        assert!(!login.is_empty());
        assert!(login.contains(fixed_keys::CTRL_Y), "{login}");
        assert!(login.contains(&confirm), "{login}");
        assert!(login.contains(&cancel), "{login}");

        // Stay-open editing windows: the resolved verb plus close labels.
        let edit = subtitle_edit_close("edit");
        assert_eq!(
            edit,
            format!("{confirm} to edit  \u{2022}  {cancel} to close")
        );
    }
}
