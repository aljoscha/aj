//! The empty-state splash: an animated `aj` wordmark, a command-palette hint,
//! and, when there are startup warnings, one bordered notices box below them
//! (Spec E-9).
//!
//! Shown in the chat slot before the conversation has any user or assistant
//! entry. The animation is tick-driven off the frame clock the async driver
//! already runs, so it costs a periodic redraw and no extra thread. Both the
//! drift and the color pulse derive from elapsed wall-clock time (an
//! [`Instant`], like the loader's spinner), so the motion speed is independent
//! of tick-delivery jitter.
//!
//! The box surfaces the transcript's leading warning-level `Notice` entries.
//! It sizes to its content and caps at a small height, staying short by
//! default and scrolling with the mouse wheel when the warnings overflow. The
//! logo and hint are centered vertically in the slot with the box hanging
//! below them, so the wordmark sits near the middle. On a short terminal the
//! block is pulled up from center only as far as the box needs to fit below it.

use std::cell::RefCell;
use std::f64::consts::TAU;
use std::rc::{Rc, Weak};
use std::time::Instant;

use aj_app::chat::{ChatState, EntryKind, NoticeLevel};
use aj_app::keybindings::{ACTION_PALETTE_OPEN, action_shortcut};
use aj_app::theme::{ColorMode, ThemeRgb};
use vaxis::cell::{Cell, Character, Color, Style};
use vaxis::mouse;
use vaxis::vxfw::{
    DrawContext, Event, EventContext, MaxSize, RelativePoint, RichText, Size, SubSurface, Surface,
    TextSpan, Widget,
};

use crate::transcript::{TranscriptStyles, vaxis_color};

/// Frame cadence, matching the loader's 80ms tick.
pub(crate) const FRAME_INTERVAL_MS: u32 = 80;

/// Name of the host-posted [`vaxis::vxfw::UserEvent`] that kicks the splash
/// animation chain. Widgets can only schedule ticks from an event handler, so
/// the host posts this once at startup and the Shell forwards it here.
pub(crate) const SPLASH_WAKE_EVENT: &str = "aj-next.splash.wake";

/// Decorative lavender-to-purple gradient for the logo, light to dark.
///
/// Per Spec E-9 this is a widget-local gradient, deliberately NOT a theme
/// token: it is ornament, not semantic UI color, so it is fixed rather than
/// palette-resolved. It renders through [`vaxis_color`] so it downsamples on
/// non-truecolor terminals like every other color.
const LAVENDER_RAMP: [(u8, u8, u8); 7] = [
    (0xE8, 0xE2, 0xFF),
    (0xC9, 0xBC, 0xFF),
    (0xB3, 0x9D, 0xFF),
    (0x9D, 0x7D, 0xF5),
    (0x8A, 0x63, 0xE8),
    (0x7B, 0x4F, 0xD8),
    (0x6A, 0x3F, 0xC0),
];

/// The `a` glyph of the wordmark, `A_WIDTH` columns wide and [`LOGO_HEIGHT`]
/// rows tall. A space is a transparent cell. Any other glyph is painted.
const LOGO_A: [&str; 6] = [
    "      ", //
    " ████ ", //
    "    █ ", //
    " █████", //
    "█   █ ", //
    " ████ ", //
];

/// The `j` glyph of the wordmark (dot, stem, left-hooking descender).
const LOGO_J: [&str; 6] = [
    "   █ ", //
    "     ", //
    "   █ ", //
    "   █ ", //
    "█  █ ", //
    " ██  ", //
];

const LOGO_HEIGHT: u16 = 6;
const A_WIDTH: u16 = 6;
const J_WIDTH: u16 = 5;

/// Maximum drift of the logo from its home position, in cells. The logo lives
/// inside a region padded by this much on every side, so it drifts without
/// ever clipping the slot edges or nudging the hint and notices below it.
const DRIFT_X: u16 = 3;
const DRIFT_Y: u16 = 1;
/// Drift periods, in seconds. Coprime-ish periods keep the horizontal and
/// vertical motion from resynchronizing into a flat diagonal.
const DRIFT_X_PERIOD: f64 = 9.0;
const DRIFT_Y_PERIOD: f64 = 13.0;

/// Period of the color pulse, in seconds. Each glyph's ramp position advances
/// with this clock, phase-shifted per column so the brightness travels across
/// the word rather than blinking in unison.
const PULSE_PERIOD: f64 = 3.0;
/// Per-row phase offset, tilting the traveling color wave slightly diagonal.
const ROW_PHASE: f64 = 0.35;

/// Baseline inter-letter gap, in cells.
const GAP_MIN: u16 = 2;
/// Extra gap the spacing oscillation adds on top of [`GAP_MIN`]. A subtle
/// size-like "breathing" the terminal can express where it cannot scale glyphs.
const GAP_SWING: u16 = 2;
/// Period of the spacing oscillation, in seconds.
const GAP_PERIOD: f64 = 5.0;

/// Box chrome: 2 border columns plus 1 padding column on each side.
const BOX_CHROME: u16 = 4;
/// Minimum readable inner width for the box. Below this it hides rather than
/// cramming content into an unreadable sliver.
const MIN_BOX_INNER: u16 = 20;
/// Cap on the box's inner width, so it stays a centered block rather than
/// stretching across a very wide terminal.
const MAX_BOX_INNER: u16 = 72;
/// Minimum available height below the hint to show a box at all. The rendered
/// box is content-sized within that.
const MIN_BOX_HEIGHT: u16 = 5;
/// Cap on the visible content rows in the box. The box sizes to its content
/// up to this many rows and scrolls beyond it, keeping the splash short by
/// default. Tunable: a larger cap trades a taller box for less scrolling.
const MAX_NOTICE_ROWS: u16 = 16;
/// Blank rows kept below the box so it does not touch the slot's bottom edge.
const BOX_BOTTOM_MARGIN: u16 = 1;
/// Vertical margin bounding where the centered logo+hint block sits: it caps
/// the space `plan_box` leaves for the box and, held equal to
/// `BOX_BOTTOM_MARGIN`, makes a full-height box pull the block up to exactly
/// this margin (see the centering note in `draw`).
const TOP_MARGIN: u16 = 1;
/// Wrapped lines the wheel moves the box per notch. A small step keeps the
/// scroll legible on a short box.
const WHEEL_STEP: usize = 3;

/// The empty-state splash widget. Non-interactive: it takes no focus and
/// consumes no keys, only self-targeted ticks and the startup wake.
pub(crate) struct Splash {
    /// Weak self-reference so tick commands can target this widget, captured at
    /// construction with [`Rc::new_cyclic`] like the loader.
    me: Weak<RefCell<Splash>>,
    chat: Rc<RefCell<ChatState>>,
    styles: Rc<TranscriptStyles>,
    /// Top wrapped-line offset of the notices box, advanced by the mouse wheel
    /// while the cursor is over the box. Clamped to the box's content every
    /// draw. Reset to 0 by [`Splash::reset_scroll`] on a session switch.
    notices_scroll: usize,
    /// Splash-local rect and scroll bound of the drawn notices box, recorded
    /// each [`draw`](Widget::draw) so [`handle_event`](Widget::handle_event)
    /// can hit-test the wheel against it. `None` when the box is hidden or
    /// absent.
    notices_hit: Option<BoxHit>,
    /// The lavender ramp resolved to vaxis colors for the theme's color mode,
    /// so a non-truecolor terminal gets palette indices. Rebuilt on a swap.
    ramp: Vec<Color>,
    /// Wall-clock origin of the animation. Drift and pulse derive from elapsed
    /// time, so the motion speed does not depend on tick-delivery jitter.
    started: Instant,
    /// Whether a tick targeting this widget is in flight, guarding against
    /// stacking multiple tick chains when a wake and a pending tick interleave.
    tick_armed: bool,
    /// Set by [`draw`](Widget::draw) whenever the splash actually renders, and
    /// cleared when a tick consumes it. A tick that finds it clear means the
    /// transcript has replaced the splash, so the animation pump stops there.
    drawn: bool,
}

impl Splash {
    pub(crate) fn new(
        chat: Rc<RefCell<ChatState>>,
        styles: Rc<TranscriptStyles>,
        mode: ColorMode,
    ) -> Rc<RefCell<Splash>> {
        Rc::new_cyclic(|me| {
            RefCell::new(Splash {
                me: Weak::clone(me),
                chat,
                styles,
                notices_scroll: 0,
                notices_hit: None,
                ramp: build_ramp(mode),
                started: Instant::now(),
                tick_armed: false,
                drawn: false,
            })
        })
    }

    /// Replace the palette styles and recompute the ramp for the (possibly
    /// changed) color mode, for a runtime theme swap.
    pub(crate) fn set_styles(&mut self, styles: Rc<TranscriptStyles>, mode: ColorMode) {
        self.styles = styles;
        self.ramp = build_ramp(mode);
    }

    /// Return the notices box to the top, for a session switch.
    pub(crate) fn reset_scroll(&mut self) {
        self.notices_scroll = 0;
    }

    /// Schedule the next animation tick if none is pending, and latch a redraw
    /// so the new frame paints. The visibility guard lives in the tick handler,
    /// not here, so the startup wake can kick the chain before the first draw.
    fn arm_tick(&mut self, ctx: &mut EventContext) {
        ctx.redraw = true;
        if self.tick_armed {
            return;
        }
        self.tick_armed = true;
        ctx.tick(
            FRAME_INTERVAL_MS,
            self.me.upgrade().expect("splash self-reference is live"),
        );
    }

    /// The `{key} for commands` hint spans: the palette key bold in the
    /// keybinding-hint color, the rest muted. The key resolves through the
    /// keybinding data so it is never a literal (mirrors the copy hint).
    fn hint_spans(&self) -> Vec<TextSpan> {
        let key = action_shortcut(ACTION_PALETTE_OPEN).unwrap_or_default();
        vec![
            TextSpan {
                text: key,
                style: self.styles.keybinding_hint,
                ..TextSpan::default()
            },
            TextSpan {
                text: " for commands".to_string(),
                style: self.styles.dim,
                ..TextSpan::default()
            },
        ]
    }

    /// Draw the wordmark into a `logo_w` x [`LOGO_HEIGHT`] surface, painting
    /// each glyph cell in its animated ramp color.
    fn draw_logo(&self, gap: u16, logo_w: u16, cycle: f64) -> Surface {
        let mut surface = Surface::with_size(Size {
            width: logo_w,
            height: LOGO_HEIGHT,
        });
        for row in 0..LOGO_HEIGHT {
            self.paint_glyph(&mut surface, &LOGO_A, 0, row, logo_w, cycle);
            self.paint_glyph(&mut surface, &LOGO_J, A_WIDTH + gap, row, logo_w, cycle);
        }
        surface
    }

    /// Paint one glyph's `row` at column offset `col0`. Glyph cells are all
    /// single width, so the character index is the display column.
    fn paint_glyph(
        &self,
        surface: &mut Surface,
        glyph: &[&str; 6],
        col0: u16,
        row: u16,
        logo_w: u16,
        cycle: f64,
    ) {
        for (i, ch) in glyph[usize::from(row)].chars().enumerate() {
            if ch == ' ' {
                continue;
            }
            let Ok(offset) = u16::try_from(i) else {
                break;
            };
            let col = col0 + offset;
            surface.write_cell(
                col,
                row,
                Cell {
                    char: Character::new(ch.to_string(), 1),
                    style: Style {
                        fg: self.cell_color(col, row, logo_w, cycle),
                        ..Style::default()
                    },
                    ..Cell::default()
                },
            );
        }
    }

    /// The ramp color for a logo cell: a traveling brightness wave, phase-
    /// shifted per column (one full ramp across the word) and tilted per row.
    fn cell_color(&self, col: u16, row: u16, logo_w: u16, cycle: f64) -> Color {
        let n = self.ramp.len();
        if n == 0 {
            return Color::Default;
        }
        let col_off = f64::from(col) / f64::from(logo_w.max(1)) * TAU;
        let row_off = f64::from(row) * ROW_PHASE;
        // 0.5 - 0.5*cos maps the angle onto [0, 1] with a smooth turn at both
        // ends, so the ramp eases in and out rather than snapping at the loop.
        let wave = 0.5 - 0.5 * (cycle + col_off + row_off).cos();
        self.ramp[ramp_index(wave, n)]
    }

    /// The notices box spans: the leading run of Warning/Error `Notice`
    /// entries, or `None` when there are none (which hides the box).
    ///
    /// NOTE: We skip Info-level notices here. Context and any other
    /// informational notice are scrollback content, so the box surfaces only
    /// attention-worthy warnings. Only the leading run counts: once a user or
    /// assistant entry lands the splash is gone. An Info notice inside the run
    /// is skipped without ending the run, so a leading context notice followed
    /// by warnings still shows the warnings.
    fn notice_spans(&self) -> Option<Vec<TextSpan>> {
        let chat = self.chat.borrow();
        let transcript = chat.transcript(chat.active_view())?;
        let mut spans = Vec::new();
        for entry in transcript.entries() {
            let EntryKind::Notice(notice) = &entry.kind else {
                break;
            };
            let style = match notice.level {
                NoticeLevel::Info => continue,
                NoticeLevel::Warning => self.styles.warning,
                NoticeLevel::Error => self.styles.error,
            };
            // NOTE: We push the notice text verbatim, so any raw SGR
            // strikethrough markers would render literally here. Only the Info
            // context notice carries them and Info is skipped above, so the box
            // never sees a marker. The transcript path parses them instead.
            spans.push(TextSpan {
                text: format!("{}\n", notice.text),
                style,
                ..TextSpan::default()
            });
        }
        if spans.is_empty() { None } else { Some(spans) }
    }

    /// Content-independent geometry for the notices box: its inner and outer
    /// widths, its horizontal offset, and the vertical space available for the
    /// box below the `header_h`-tall logo-and-hint header. `None` when the slot
    /// is too narrow or too short for a readable box, in which case only the
    /// logo and hint show.
    fn plan_box(&self, slot_w: u16, slot_h: u16, header_h: u16) -> Option<BoxPlan> {
        // The box is bounded by the slot minus the header, the top margin, and
        // the bottom margin. This is the tallest the box may grow before it
        // would overflow the slot below a centered block, matching the max box
        // height the old top-anchored layout allowed.
        let available_h = slot_h
            .saturating_sub(header_h)
            .saturating_sub(TOP_MARGIN + BOX_BOTTOM_MARGIN);
        if available_h < MIN_BOX_HEIGHT {
            return None;
        }
        let usable = slot_w.checked_sub(BOX_CHROME)?;
        if usable < MIN_BOX_INNER {
            return None;
        }
        let inner_w = usable.min(MAX_BOX_INNER);
        let box_outer = inner_w + BOX_CHROME;
        Some(BoxPlan {
            inner_w,
            box_outer,
            left: center_offset(slot_w, box_outer),
            available_h,
        })
    }

    /// Soft-wrap `spans` to `inner_w` with the height unbounded, so the wrapped
    /// surface holds the full content. The caller both sizes the box from its
    /// height and windows it at the scroll offset, so the wrap happens once.
    fn wrap_content(&self, ctx: &DrawContext, spans: Vec<TextSpan>, inner_w: u16) -> Surface {
        let content_ctx = ctx.with_constraints(
            Size::default(),
            MaxSize {
                width: Some(inner_w),
                height: None,
            },
        );
        RichText::new(spans).draw(&content_ctx)
    }

    /// A bordered box `inner_w` + [`BOX_CHROME`] wide and `box_h` tall, framing
    /// the pre-wrapped `content` windowed at `offset`.
    ///
    /// The returned tuple carries the offset clamped to the content and the
    /// box's `max_scroll`, for the wheel handler to clamp against. When the
    /// content is taller than the inner height we draw a scrollbar thumb on the
    /// right inner edge whose position and size reflect the offset and total.
    /// The splash takes no focus and no keys, so the thumb is only an
    /// indicator, never a draggable scrollbar.
    fn render_box(
        &self,
        content: &Surface,
        inner_w: u16,
        box_h: u16,
        offset: usize,
    ) -> (Surface, usize, usize) {
        let box_w = inner_w + BOX_CHROME;
        let mut surface = Surface::with_size(Size {
            width: box_w,
            height: box_h,
        });
        self.paint_box_frame(&mut surface, box_w, box_h);

        let inner_h = box_h.saturating_sub(2);
        let total_rows = content.size.height;
        // Clamp the offset to the box's current content every draw, so a resize
        // or a content change can never leave it scrolled past the end.
        let max_scroll = usize::from(total_rows).saturating_sub(usize::from(inner_h));
        let offset = offset.min(max_scroll);
        let first = u16::try_from(offset).unwrap_or(u16::MAX);
        let content_w = content.size.width.min(inner_w);
        let visible = inner_h.min(total_rows.saturating_sub(first));
        for row in 0..visible {
            for col in 0..content_w {
                surface.write_cell(2 + col, 1 + row, content.read_cell(col, first + row));
            }
        }
        if total_rows > inner_h {
            let thumb_col = box_w.saturating_sub(2);
            let (thumb_top, thumb_h) = thumb_span(inner_h, total_rows, offset, max_scroll);
            for row in 0..thumb_h {
                surface.write_cell(
                    thumb_col,
                    1 + thumb_top + row,
                    glyph_cell("\u{2590}", self.styles.scrollbar_thumb),
                );
            }
        }
        (surface, offset, max_scroll)
    }

    /// Scroll the notices box by [`WHEEL_STEP`] wrapped lines when the cursor
    /// is over it, clamped to its content, and consume the event. A wheel off
    /// the box (or already at a clamp) falls through unconsumed. Never requests
    /// focus: the editor keeps focus so the user can still type their first
    /// message, the wheel only moves the box under the cursor.
    fn handle_wheel(&mut self, ctx: &mut EventContext, m: &mouse::Mouse) {
        let up = match m.button {
            mouse::Button::WheelUp => true,
            mouse::Button::WheelDown => false,
            _ => return,
        };
        let (Ok(col), Ok(row)) = (u16::try_from(m.col), u16::try_from(m.row)) else {
            return;
        };
        let Some(hit) = self.notices_hit else { return };
        if !hit.contains(col, row) {
            return;
        }
        let offset = self.notices_scroll;
        let next = if up {
            offset.saturating_sub(WHEEL_STEP)
        } else {
            offset.saturating_add(WHEEL_STEP).min(hit.max_scroll)
        };
        if next != offset {
            self.notices_scroll = next;
            ctx.consume_and_redraw();
        }
    }

    /// Paint the light rounded frame of a box in the muted style.
    fn paint_box_frame(&self, surface: &mut Surface, box_w: u16, box_h: u16) {
        let border = self.styles.dim;
        // `plan_box` guarantees box_w >= MIN_BOX_INNER + BOX_CHROME, and the
        // content sizing keeps box_h >= 3, so the corners never collide. The
        // `saturating_sub`s only defend a future caller that skips that plan.
        let last_col = box_w.saturating_sub(1);
        let last_row = box_h.saturating_sub(1);
        surface.write_cell(0, 0, glyph_cell("╭", border));
        surface.write_cell(last_col, 0, glyph_cell("╮", border));
        surface.write_cell(0, last_row, glyph_cell("╰", border));
        surface.write_cell(last_col, last_row, glyph_cell("╯", border));
        for col in 1..last_col {
            surface.write_cell(col, 0, glyph_cell("─", border));
            surface.write_cell(col, last_row, glyph_cell("─", border));
        }
        for row in 1..last_row {
            surface.write_cell(0, row, glyph_cell("│", border));
            surface.write_cell(last_col, row, glyph_cell("│", border));
        }
    }
}

impl Widget for Splash {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        self.drawn = true;
        let slot_w = ctx.max.width.unwrap_or(ctx.min.width);
        let t = self.started.elapsed().as_secs_f64();

        // Logo geometry, with the breathing inter-letter gap folded in.
        let gap = GAP_MIN + osc_u16(t, GAP_PERIOD, GAP_SWING);
        let logo_w = A_WIDTH + gap + J_WIDTH;
        // The logo lives in a region padded by the drift amplitude, so drifting
        // never resizes the block or clips the slot.
        let region_w = logo_w + 2 * DRIFT_X;
        let region_h = LOGO_HEIGHT + 2 * DRIFT_Y;

        // The notices box, shown only when there are warning-level notices. A
        // run with no warnings yields `None` and leaves only the logo and hint.
        let notices = self.notice_spans();

        let slot_h = ctx.max.height.unwrap_or(region_h + 2);
        // The padded logo region, the gap rows, and the hint form the header
        // above the box. The logo+hint block is centered and the box hangs
        // below it, so we need the box height before we can place the block.
        let header_h = region_h + 3;
        let plan = notices
            .as_ref()
            .and_then(|_| self.plan_box(slot_w, slot_h, header_h));

        // Size the box up front, since where the block anchors depends on the
        // box height (`max_top` below). We wrap the notice content here and
        // carry the wrapped surface to the box placement below, so we never
        // wrap twice. `box_h` is stable per session (the notices are static and
        // `region_h` is const), so the anchor adds no vertical jitter across
        // frames.
        let boxed = match (notices, &plan) {
            (Some(spans), Some(plan)) => {
                let content = self.wrap_content(ctx, spans, plan.inner_w);
                let total_rows = content.size.height;
                // As tall as the content plus the two border rows, capped at
                // MAX_NOTICE_ROWS and at the space the plan leaves for the box.
                let box_h = total_rows
                    .min(MAX_NOTICE_ROWS)
                    .saturating_add(2)
                    .min(plan.available_h);
                Some((content, box_h))
            }
            _ => None,
        };

        // Center the logo+hint block on its own height. This is where the
        // block sits when the box fits below it.
        let center_top = center_offset(slot_h, region_h + 2);
        // Center the logo+hint, but pull the block up from center only as far
        // as the box needs to fit below it. When the box fills the space this
        // collapses to the old top-anchored layout: box_h == available_h makes
        // max_top == TOP_MARGIN, so top pins there.
        let top = match &boxed {
            Some((_, box_h)) => {
                let max_top = slot_h.saturating_sub(BOX_BOTTOM_MARGIN + box_h + header_h);
                center_top.min(max_top).max(TOP_MARGIN)
            }
            None => center_top,
        };
        let box_top = top + header_h;

        let mut surface = Surface::with_size(Size {
            width: slot_w,
            height: slot_h,
        });

        // The logo, centered horizontally in its region and drifting within it.
        let region_left = center_offset(slot_w, region_w);
        let cycle = TAU * t / PULSE_PERIOD;
        let logo = self.draw_logo(gap, logo_w, cycle);
        let logo_col = i32::from(region_left + DRIFT_X) + drift(t, DRIFT_X_PERIOD, DRIFT_X);
        let logo_row = i32::from(top + DRIFT_Y) + drift(t, DRIFT_Y_PERIOD, DRIFT_Y);
        surface.children.push(SubSurface {
            origin: RelativePoint {
                col: logo_col,
                row: logo_row,
            },
            surface: logo,
            z_index: 0,
        });

        // The hint line, centered by its own laid-out width.
        let hint_ctx = ctx.with_constraints(
            Size::default(),
            MaxSize {
                width: Some(slot_w),
                height: None,
            },
        );
        let hint = RichText::new(self.hint_spans()).draw(&hint_ctx);
        let hint_row = top + region_h + 1;
        surface.children.push(SubSurface {
            origin: RelativePoint {
                col: i32::from(center_offset(slot_w, hint.size.width)),
                row: i32::from(hint_row),
            },
            surface: hint,
            z_index: 0,
        });

        // The notices box just below the hint, sized above. It records its
        // splash-local rect so the wheel can hit-test the cursor against it
        // (see `handle_wheel`); a hidden or absent box records `None`.
        self.notices_hit = None;
        if let (Some((content, box_h)), Some(plan)) = (boxed, plan) {
            let (box_surface, offset, max_scroll) =
                self.render_box(&content, plan.inner_w, box_h, self.notices_scroll);
            self.notices_scroll = offset;
            self.notices_hit = Some(BoxHit {
                col: plan.left,
                row: box_top,
                width: plan.box_outer,
                height: box_h,
                max_scroll,
            });
            surface.children.push(SubSurface {
                origin: RelativePoint {
                    col: i32::from(plan.left),
                    row: i32::from(box_top),
                },
                surface: box_surface,
                z_index: 0,
            });
        }

        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            // The host posts a wake at startup. The Shell forwards it here so
            // the tick chain starts (widgets can only tick from a handler).
            Event::App(user) if user.name == SPLASH_WAKE_EVENT => self.arm_tick(ctx),
            // The wheel scrolls the notices box when the cursor is over it.
            // vxfw hands us a splash-local position (the splash is a hit target
            // via `wants_events`), which we hit-test against the box rect
            // `draw` recorded. Focus is untouched, matching how the transcript
            // scrolls on the wheel while the editor keeps focus.
            Event::Mouse(m) => self.handle_wheel(ctx, m),
            Event::Tick => {
                self.tick_armed = false;
                ctx.redraw = true;
                // Re-arm only while the splash is still the drawn child. `drawn`
                // is set by `draw` and cleared here: a tick that finds it clear
                // means the transcript replaced the splash, so the pump ends.
                if self.drawn {
                    self.drawn = false;
                    self.arm_tick(ctx);
                }
            }
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// A single-cell glyph with a foreground style and no background.
fn glyph_cell(grapheme: &str, style: Style) -> Cell {
    Cell {
        char: Character::new(grapheme, 1),
        style,
        ..Cell::default()
    }
}

/// Content-independent geometry for the notices box: its inner and outer
/// widths, its horizontal offset, and the vertical space available for the box
/// within the centered group. Computed by [`Splash::plan_box`]. The box's
/// height is derived from its content, capped at `available_h`.
struct BoxPlan {
    inner_w: u16,
    box_outer: u16,
    left: u16,
    available_h: u16,
}

/// A drawn box's splash-local rectangle plus its scroll bound, recorded each
/// draw so [`Splash::handle_wheel`] can hit-test the cursor and clamp the
/// resulting offset without re-wrapping the content.
#[derive(Clone, Copy)]
struct BoxHit {
    col: u16,
    row: u16,
    width: u16,
    height: u16,
    /// Largest in-range offset, `total_wrapped.saturating_sub(inner_height)`.
    max_scroll: usize,
}

impl BoxHit {
    /// Whether splash-local `(col, row)` falls inside the box rectangle.
    fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.col
            && col < self.col.saturating_add(self.width)
            && row >= self.row
            && row < self.row.saturating_add(self.height)
    }
}

/// Height of the overflow thumb: the visible fraction of the wrapped content,
/// at least one row and at most the inner height.
fn thumb_height(inner_h: u16, total_rows: u16) -> u16 {
    let inner = usize::from(inner_h);
    let total = usize::from(total_rows).max(1);
    let h = (inner * inner / total).max(1);
    u16::try_from(h).unwrap_or(inner_h).min(inner_h)
}

/// Top position and height of the overflow thumb within the inner height: the
/// height is the visible fraction (at least one row), and the top reflects how
/// far `offset` has scrolled into `max_scroll`, so the thumb reads as a real
/// position indicator rather than a fixed overflow mark.
fn thumb_span(inner_h: u16, total_rows: u16, offset: usize, max_scroll: usize) -> (u16, u16) {
    let thumb_h = thumb_height(inner_h, total_rows);
    let travel = inner_h.saturating_sub(thumb_h);
    if max_scroll == 0 || travel == 0 {
        return (0, thumb_h);
    }
    // Round to the nearest track row so offset 0 pins the top and
    // offset == max_scroll pins the bottom exactly.
    let offset = offset.min(max_scroll);
    let top = (offset * usize::from(travel) + max_scroll / 2) / max_scroll;
    (u16::try_from(top).unwrap_or(travel).min(travel), thumb_h)
}

/// Resolve the lavender ramp to vaxis colors for `mode`, downsampling `Rgb`
/// triples on a non-truecolor terminal exactly as the transcript does.
fn build_ramp(mode: ColorMode) -> Vec<Color> {
    LAVENDER_RAMP
        .iter()
        .map(|&(r, g, b)| vaxis_color(ThemeRgb::Rgb(r, g, b), mode))
        .collect()
}

/// Half the slack between an outer and inner size, saturating so an undersized
/// slot pins to zero rather than underflowing.
fn center_offset(outer: u16, inner: u16) -> u16 {
    outer.saturating_sub(inner) / 2
}

/// A bounded sinusoidal drift in cells, in `[-amp, amp]`.
fn drift(t: f64, period: f64, amp: u16) -> i32 {
    round_i32(f64::from(amp) * (TAU * t / period).sin())
}

/// A bounded non-negative oscillation in cells, in `[0, swing]`.
fn osc_u16(t: f64, period: f64, swing: u16) -> u16 {
    round_u16(f64::from(swing) * (0.5 - 0.5 * (TAU * t / period).cos()))
}

/// Map a `[0, 1]` wave onto a valid ramp index.
#[allow(clippy::as_conversions)]
fn ramp_index(wave: f64, n: usize) -> usize {
    // Decorative animation math. `wave` is clamped to `[0, 1]` by its cosine
    // form and `n` is a handful of shades, so scaling then rounding to a usize
    // index loses no information. The `.min` defends the top edge.
    let scaled = wave * f64::from(u16::try_from(n.saturating_sub(1)).unwrap_or(u16::MAX));
    (scaled.round().max(0.0) as usize).min(n - 1)
}

/// Round a small, signed cell offset to `i32`.
#[allow(clippy::as_conversions)]
fn round_i32(v: f64) -> i32 {
    // `v` is a few cells at most, so rounding then truncating never overflows.
    v.round() as i32
}

/// Round a small, non-negative cell count to `u16`, clamping the cast.
#[allow(clippy::as_conversions)]
fn round_u16(v: f64) -> u16 {
    v.round().clamp(0.0, f64::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
    use aj_app::session::AgentLifecycle;
    use aj_app::theme::Theme;
    use vaxis::vxfw::Command;

    use super::*;

    fn chat() -> Rc<RefCell<ChatState>> {
        Rc::new(RefCell::new(ChatState::new(
            AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            0,
            Arc::new(Vec::new()),
        )))
    }

    fn styles() -> Rc<TranscriptStyles> {
        Rc::new(TranscriptStyles::from_theme(
            &Theme::bundled_dark_with_mode(ColorMode::Truecolor),
            crate::terminal::TerminalCaps::default(),
        ))
    }

    fn append_notice(chat: &Rc<RefCell<ChatState>>, text: &str) {
        let mut life = AgentLifecycle::default();
        let _ = aj_app::chat::reduce(
            &mut chat.borrow_mut(),
            &mut life,
            AgentEvent::Notice {
                agent_id: AgentId::Main,
                text: text.to_string(),
            },
        );
    }

    fn append_warning(chat: &Rc<RefCell<ChatState>>, text: &str) {
        let mut life = AgentLifecycle::default();
        let _ = aj_app::chat::reduce(
            &mut chat.borrow_mut(),
            &mut life,
            AgentEvent::Warning {
                agent_id: AgentId::Main,
                text: text.to_string(),
            },
        );
    }

    fn append_error(chat: &Rc<RefCell<ChatState>>, text: &str) {
        let mut life = AgentLifecycle::default();
        let _ = aj_app::chat::reduce(
            &mut chat.borrow_mut(),
            &mut life,
            AgentEvent::Error {
                agent_id: AgentId::Main,
                text: text.to_string(),
            },
        );
    }

    fn splash(chat: Rc<RefCell<ChatState>>) -> Rc<RefCell<Splash>> {
        Splash::new(chat, styles(), ColorMode::Truecolor)
    }

    /// The wordmark art rows must match the declared widths, or the column
    /// arithmetic in `paint_glyph` and the region sizing drift out of sync.
    #[test]
    fn logo_art_matches_declared_dimensions() {
        assert_eq!(LOGO_A.len(), usize::from(LOGO_HEIGHT));
        assert_eq!(LOGO_J.len(), usize::from(LOGO_HEIGHT));
        for row in LOGO_A {
            assert_eq!(row.chars().count(), usize::from(A_WIDTH), "a row: {row:?}");
        }
        for row in LOGO_J {
            assert_eq!(row.chars().count(), usize::from(J_WIDTH), "j row: {row:?}");
        }
    }

    /// With no notices the splash renders the logo and the hint but no box.
    #[test]
    fn renders_hint_without_a_box_when_no_notices() {
        let splash = splash(chat());
        let surface = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(60, Some(24)));
        let rows = crate::test_support::rows(&surface);
        let joined = rows.join("\n");
        assert!(joined.contains("for commands"), "hint missing: {rows:?}");
        assert!(
            !joined.contains('╭'),
            "no notices should mean no box: {rows:?}"
        );
    }

    /// Leading warnings render inside a bordered box.
    #[test]
    fn renders_notices_in_a_bordered_box() {
        let chat = chat();
        append_warning(&chat, "startup warning");
        let splash = splash(chat);
        let surface = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(60, Some(24)));
        let joined = crate::test_support::rows(&surface).join("\n");
        assert!(joined.contains('╭') && joined.contains('╯'), "box frame");
        assert!(joined.contains("startup warning"), "notice text");
    }

    /// The box surfaces warning-level notices only: a leading Info notice
    /// carrying the context listing stays out of the box while a Warning shows.
    #[test]
    fn notice_box_shows_warnings_not_context() {
        let chat = chat();
        append_notice(&chat, "Context:\n  - builtin (system prompt)");
        append_warning(&chat, "sandbox warning");
        let splash = splash(chat);
        let surface = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(60, Some(24)));
        let joined = crate::test_support::rows(&surface).join("\n");
        assert!(
            joined.contains('╭'),
            "the box renders for the warning: {joined}"
        );
        assert!(
            joined.contains("sandbox warning"),
            "the warning shows in the box: {joined}"
        );
        assert!(
            !joined.contains("Context:"),
            "the info context is filtered out of the box: {joined}"
        );
    }

    /// An Error-level notice reaches the box like a Warning does: `notice_spans`
    /// styles the Error arm rather than skipping it, so the error text shows.
    #[test]
    fn notice_box_shows_error_level_notices() {
        let chat = chat();
        append_error(&chat, "config load failed");
        let splash = splash(chat);
        let surface = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(60, Some(24)));
        let joined = crate::test_support::rows(&surface).join("\n");
        assert!(
            joined.contains('╭'),
            "the box renders for the error: {joined}"
        );
        assert!(
            joined.contains("config load failed"),
            "the error notice shows in the box: {joined}"
        );
    }

    /// On a tall slot the box sizes to its content: with two short warnings its
    /// frame spans only a handful of rows and does not reach the slot bottom,
    /// rather than filling the space below the hint.
    #[test]
    fn notice_box_is_content_sized() {
        let chat = chat();
        append_warning(&chat, "first warning");
        append_warning(&chat, "second warning");
        let splash = splash(chat);
        let rows = crate::test_support::rows(
            &splash
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(60, Some(40))),
        );
        let top = rows.iter().position(|r| r.contains('╭')).expect("box top");
        let bottom = rows
            .iter()
            .rposition(|r| r.contains('╰'))
            .expect("box bottom");
        // Two one-line warnings wrap to two rows, so the frame is content + 2
        // border rows tall, well under the tall slot. A box that filled
        // `available_h` would span far more.
        assert!(
            bottom - top <= 5,
            "the box is content-sized, not full-height: top={top} bottom={bottom}"
        );
        assert!(
            bottom < rows.len() - 2,
            "the box does not reach the slot bottom: bottom={bottom} of {}",
            rows.len()
        );
    }

    /// Many warnings on a tall slot cap the box height rather than growing it:
    /// the frame pins to `MAX_NOTICE_ROWS` content rows plus the two border
    /// rows, well under the slot's available height, and the overflow scrolls.
    #[test]
    fn notice_box_caps_at_max_notice_rows() {
        let chat = chat();
        for i in 0..20 {
            append_warning(&chat, &format!("warning {i}"));
        }
        let splash = splash(chat);
        let rows = crate::test_support::rows(
            &splash
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(60, Some(40))),
        );
        let top = rows.iter().position(|r| r.contains('╭')).expect("box top");
        let bottom = rows
            .iter()
            .rposition(|r| r.contains('╰'))
            .expect("box bottom");
        // Twenty one-line warnings wrap to twenty rows, far past the cap, so the
        // frame height pins to MAX_NOTICE_ROWS content rows plus two borders.
        // Neutering the cap (e.g. capping at MAX_NOTICE_ROWS * 100) lets the box
        // grow to its content or `available_h`, making this span larger.
        assert_eq!(
            bottom - top + 1,
            usize::from(MAX_NOTICE_ROWS) + 2,
            "the box height caps at MAX_NOTICE_ROWS + borders: top={top} bottom={bottom}"
        );
    }

    /// On a tall slot where the box fits below a centered block, the logo+hint
    /// is centered on its own height (not the whole group), so the hint lands
    /// at the centered offset well below the top-anchored row. The box hangs
    /// below the hint. A regression to `top = TOP_MARGIN` would put the hint at
    /// row `TOP_MARGIN + region_h + 1` instead of the centered row, failing the
    /// exact-position assert.
    #[test]
    fn short_box_centers_the_logo_and_hint_on_a_tall_slot() {
        let chat = chat();
        append_warning(&chat, "first warning");
        append_warning(&chat, "second warning");
        let splash = splash(chat);
        let slot_h: u16 = 40;
        let rows = crate::test_support::rows(
            &splash
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(60, Some(slot_h))),
        );
        let hint_row = rows
            .iter()
            .position(|r| r.contains("for commands"))
            .expect("hint row");
        let box_top = rows.iter().position(|r| r.contains('╭')).expect("box top");
        let region_h = LOGO_HEIGHT + 2 * DRIFT_Y;
        // The box fits below the centered block, so the block sits at exactly
        // the no-box centered offset and the hint lands at its bottom row.
        let top = center_offset(slot_h, region_h + 2);
        assert_eq!(
            hint_row,
            usize::from(top + region_h + 1),
            "the logo+hint is centered, so the hint sits at the centered offset, \
             not the top-anchored row"
        );
        assert!(
            box_top > hint_row,
            "the box hangs below the hint: box_top={box_top} hint_row={hint_row}"
        );
    }

    /// When the box fills the available height the block is pulled all the way
    /// up to `TOP_MARGIN`: `box_h == available_h` makes `max_top == TOP_MARGIN`,
    /// so `top` pins there. Ten warnings on an 18-row slot make `box_h` fill
    /// `available_h`, and the hint then lands at its top-anchored row.
    #[test]
    fn full_box_top_anchors_the_group() {
        let chat = chat();
        for i in 0..10 {
            append_warning(&chat, &format!("warning {i}"));
        }
        let splash = splash(chat);
        let rows = crate::test_support::rows(
            &splash
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(60, Some(18))),
        );
        let hint_row = rows
            .iter()
            .position(|r| r.contains("for commands"))
            .expect("hint row");
        let box_top = rows.iter().position(|r| r.contains('╭')).expect("box top");
        let region_h = LOGO_HEIGHT + 2 * DRIFT_Y;
        assert_eq!(
            hint_row,
            usize::from(TOP_MARGIN + region_h + 1),
            "a full box top-anchors the group, so the hint lands at TOP_MARGIN + region_h + 1"
        );
        assert_eq!(
            box_top,
            usize::from(TOP_MARGIN + region_h + 3),
            "the box sits directly below the header at the top-anchored offset"
        );
    }

    /// A slot too narrow for a readable box, or too short to fit a box under
    /// the hint, hides the box and keeps only the logo and hint.
    #[test]
    fn hides_box_when_slot_is_too_small() {
        let chat = chat();
        append_warning(&chat, "startup warning");
        let splash = splash(chat);

        let narrow = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(20, Some(30)));
        let narrow = crate::test_support::rows(&narrow).join("\n");
        assert!(narrow.contains("for commands"), "hint remains: {narrow}");
        assert!(!narrow.contains('╭'), "box hidden when narrow: {narrow}");

        let short = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(80, Some(14)));
        let short = crate::test_support::rows(&short).join("\n");
        assert!(short.contains("for commands"), "hint remains: {short}");
        assert!(!short.contains('╭'), "box hidden when short: {short}");
    }

    /// Content taller than the box draws the overflow thumb on the right inner
    /// edge, a non-interactive indicator (the splash takes no focus or keys).
    #[test]
    fn overflow_draws_a_scrollbar_thumb() {
        let chat = chat();
        for i in 0..40 {
            append_warning(&chat, &format!("warning {i}"));
        }
        let splash = splash(chat);
        let surface = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(80, Some(24)));
        let joined = crate::test_support::rows(&surface).join("\n");
        assert!(
            joined.contains('\u{2590}'),
            "overflow thumb on the right inner edge: {joined}"
        );
    }

    /// The thumb top tracks the offset: pinned at the top for offset 0, at the
    /// bottom of its travel for `max_scroll`, and proportional in between, so
    /// the thumb is a real position indicator rather than a static mark.
    #[test]
    fn thumb_top_tracks_the_offset() {
        let inner_h: u16 = 10;
        let total_rows: u16 = 40;
        let max_scroll = usize::from(total_rows) - usize::from(inner_h);
        let thumb_h = thumb_span(inner_h, total_rows, 0, max_scroll).1;
        let travel = inner_h - thumb_h;
        assert!(travel > 0, "the thumb has room to move: {travel}");

        assert_eq!(
            thumb_span(inner_h, total_rows, 0, max_scroll).0,
            0,
            "offset 0 pins the thumb at the top"
        );
        assert_eq!(
            thumb_span(inner_h, total_rows, max_scroll, max_scroll).0,
            travel,
            "offset == max_scroll pins the thumb at the bottom of its travel"
        );
        assert_eq!(
            thumb_span(inner_h, total_rows, max_scroll / 2, max_scroll).0,
            travel / 2,
            "a middle offset maps to the middle of the travel"
        );
    }

    /// A session switch starts the box at the top: `reset_scroll` zeroes the
    /// offset, so a prior session's scroll does not carry over.
    #[test]
    fn reset_scroll_zeroes_the_offset() {
        let splash = splash(chat());
        splash.borrow_mut().notices_scroll = 7;
        splash.borrow_mut().reset_scroll();
        assert_eq!(splash.borrow().notices_scroll, 0, "notices offset reset");
    }

    /// A synthetic wheel `Event::Mouse` at splash-local `(col, row)`.
    fn wheel(col: u16, row: u16, button: mouse::Button) -> Event {
        Event::Mouse(mouse::Mouse {
            col: i16::try_from(col).expect("col fits i16"),
            row: i16::try_from(row).expect("row fits i16"),
            xoffset: 0,
            yoffset: 0,
            button,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Press,
        })
    }

    /// A splash whose notices box overflows a 30-row slot, drawn once so its
    /// box rect is recorded. Returns the splash and the recorded box hit.
    fn notices_splash() -> (Rc<RefCell<Splash>>, BoxHit) {
        let chat = chat();
        for i in 0..40 {
            append_warning(&chat, &format!("notice {i}"));
        }
        let splash = splash(chat);
        let _ = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(80, Some(30)));
        let hit = splash.borrow().notices_hit.expect("notices box drawn");
        (splash, hit)
    }

    /// A wheel over the box scrolls the box's content window, not just the
    /// thumb, and focus is never requested (the editor keeps focus so the user
    /// can still type).
    #[test]
    fn wheel_scrolls_the_box_under_the_cursor_without_stealing_focus() {
        let (splash, hit) = notices_splash();
        assert!(hit.max_scroll > 0, "the notices overflow the box");
        // First content row inside the top border, in splash-local coordinates.
        let content_row = usize::from(hit.row) + 1;
        let before = crate::test_support::rows(
            &splash
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(80, Some(30))),
        );
        assert!(
            before[content_row].contains("notice 0"),
            "the window starts at the first notice: {:?}",
            before[content_row]
        );

        let mut ctx = EventContext::new();
        splash.borrow_mut().handle_event(
            &mut ctx,
            &wheel(hit.col + 2, hit.row + 1, mouse::Button::WheelDown),
        );
        assert!(ctx.consume_event, "a scroll consumes the wheel");
        assert!(
            !ctx.cmds
                .iter()
                .any(|c| matches!(c, Command::RequestFocus(_))),
            "wheel scroll must not steal focus"
        );
        assert_eq!(
            splash.borrow().notices_scroll,
            WHEEL_STEP,
            "advanced one step"
        );

        let after = crate::test_support::rows(
            &splash
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(80, Some(30))),
        );
        // The content window advanced by the offset: the first notice scrolled
        // off the top and a later one now sits there. Checking the content row
        // (not the whole surface) fails if `render_box` windows from line 0 and
        // only the thumb glyph moves.
        assert!(
            !after[content_row].contains("notice 0")
                && after[content_row].contains(&format!("notice {WHEEL_STEP}")),
            "the content window shifted by the offset: {:?}",
            after[content_row]
        );
    }

    /// The wheel clamps at both ends: it can't scroll above the top, and it
    /// pins to `max_scroll` at the bottom rather than running past the end.
    #[test]
    fn wheel_clamps_at_the_top_and_bottom() {
        let (splash, hit) = notices_splash();
        let inside = wheel(hit.col + 2, hit.row + 1, mouse::Button::WheelDown);
        let up = wheel(hit.col + 2, hit.row + 1, mouse::Button::WheelUp);

        // At the top, a wheel-up cannot go negative and does not consume.
        let mut ctx = EventContext::new();
        splash.borrow_mut().handle_event(&mut ctx, &up);
        assert_eq!(splash.borrow().notices_scroll, 0, "clamped at the top");
        assert!(!ctx.consume_event, "no scroll at the top falls through");

        // Drive the wheel past the end; the offset pins to max_scroll.
        for _ in 0..20 {
            let mut ctx = EventContext::new();
            splash.borrow_mut().handle_event(&mut ctx, &inside);
        }
        assert_eq!(
            splash.borrow().notices_scroll,
            hit.max_scroll,
            "clamped at the bottom"
        );
        let mut ctx = EventContext::new();
        splash.borrow_mut().handle_event(&mut ctx, &inside);
        assert!(!ctx.consume_event, "no scroll at the bottom falls through");
    }

    /// A stored offset left past the content (e.g. by a resize or content
    /// shrink) is clamped to the last page on the next draw, independent of the
    /// handler-side clamp in `wheel_clamps_at_the_top_and_bottom`.
    #[test]
    fn draw_clamps_a_stored_offset_past_the_end() {
        let (splash, hit) = notices_splash();
        assert!(hit.max_scroll > 0, "the notices overflow the box");
        // Shove the stored offset well past the content, then draw.
        splash.borrow_mut().notices_scroll = hit.max_scroll + 1000;
        let rows = crate::test_support::rows(
            &splash
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(80, Some(30))),
        );
        // The draw clamps the offset back into range, so it pins to the last
        // page and the final notice is visible. Without the draw-side clamp the
        // offset stays out of range and the window is empty.
        assert_eq!(
            splash.borrow().notices_scroll,
            hit.max_scroll,
            "the stored offset is clamped to the last page every draw"
        );
        assert!(
            rows.iter().any(|r| r.contains("notice 39")),
            "the clamped last page shows the final notice: {rows:?}"
        );
    }

    /// A wheel that lands off the box is ignored: nothing scrolls and the event
    /// is not consumed, so it falls through to whatever is behind.
    #[test]
    fn wheel_outside_the_boxes_is_ignored() {
        let (splash, _hit) = notices_splash();
        let mut ctx = EventContext::new();
        // Row 0 is the logo/hint band, well above the box top.
        splash
            .borrow_mut()
            .handle_event(&mut ctx, &wheel(0, 0, mouse::Button::WheelDown));
        assert_eq!(splash.borrow().notices_scroll, 0, "nothing scrolled");
        assert!(!ctx.consume_event, "an off-box wheel falls through");
    }

    /// The wake kicks exactly one tick chain, each tick re-arms while the
    /// splash keeps drawing, and the chain dies once it stops being drawn.
    #[test]
    fn wake_and_ticks_drive_the_pump_until_hidden() {
        let splash = splash(chat());
        let wake = Event::App(vaxis::vxfw::UserEvent {
            name: SPLASH_WAKE_EVENT.to_string(),
            data: None,
        });

        let mut ctx = EventContext::new();
        splash.borrow_mut().handle_event(&mut ctx, &wake);
        assert!(ctx.redraw);
        assert_eq!(ctx.cmds.len(), 1);
        assert!(matches!(ctx.cmds[0], Command::Tick(_)));

        // A second wake while a tick is pending must not stack a chain.
        let mut ctx = EventContext::new();
        splash.borrow_mut().handle_event(&mut ctx, &wake);
        assert!(ctx.cmds.is_empty(), "no duplicate tick chain");

        // A draw marks the splash visible, so the pending tick re-arms.
        splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(60, Some(24)));
        let mut ctx = EventContext::new();
        splash.borrow_mut().handle_event(&mut ctx, &Event::Tick);
        assert!(ctx.redraw);
        assert_eq!(ctx.cmds.len(), 1, "tick re-arms while visible");

        // No draw since the last tick means the transcript replaced us, so the
        // next tick repaints once but does not re-arm.
        let mut ctx = EventContext::new();
        splash.borrow_mut().handle_event(&mut ctx, &Event::Tick);
        assert!(ctx.redraw, "final repaint");
        assert!(ctx.cmds.is_empty(), "chain ends once hidden");
    }

    /// Drift and the color cycle advance with elapsed wall-clock time, so
    /// backdating the origin moves the logo deterministically.
    #[test]
    fn animation_advances_with_elapsed_time() {
        let splash = splash(chat());
        let first = crate::test_support::flatten(
            &splash
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(60, Some(24))),
        );
        splash.borrow_mut().started = Instant::now() - Duration::from_millis(2200);
        let second = crate::test_support::flatten(
            &splash
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(60, Some(24))),
        );
        assert_ne!(
            first, second,
            "the composited frame must change as the animation advances"
        );
    }
}
