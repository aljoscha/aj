//! The empty-state splash: an animated `aj` wordmark, a command-palette hint,
//! and two bordered boxes below it, a Context box and a Notices box (Spec E-9).
//!
//! Shown in the chat slot before the conversation has any user or assistant
//! entry. The animation is tick-driven off the frame clock the async driver
//! already runs, so it costs a periodic redraw and no extra thread. Both the
//! drift and the color pulse derive from elapsed wall-clock time (an
//! [`Instant`], like the loader's spinner), so the motion speed is independent
//! of tick-delivery jitter.

use std::cell::RefCell;
use std::f64::consts::TAU;
use std::rc::{Rc, Weak};
use std::time::Instant;

use aj_app::chat::{ChatState, EntryKind, NoticeLevel};
use aj_app::keybindings::{ACTION_PALETTE_OPEN, default_action_shortcut};
use aj_app::notices::ContextLine;
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
/// Horizontal gap between the two side-by-side boxes.
const BOX_GAP: u16 = 2;
/// Minimum readable inner width for a box. Below this the boxes hide rather
/// than cramming content into an unreadable sliver.
const MIN_BOX_INNER: u16 = 20;
/// Cap on a box's inner width, so the pair stays a centered group rather than
/// stretching across a very wide terminal.
const MAX_BOX_INNER: u16 = 48;
/// Minimum box height, borders included. Below this the boxes hide.
const MIN_BOX_HEIGHT: u16 = 5;
/// Blank rows kept below the boxes so they do not touch the slot's bottom edge.
const BOX_BOTTOM_MARGIN: u16 = 1;
/// Top margin above the logo when the boxes are shown. With the boxes hidden
/// the logo+hint block is vertically centered instead.
const TOP_MARGIN: u16 = 1;
/// Wrapped lines the wheel moves a box per notch. A small step keeps the
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
    /// The structured `Context:` listing shown in the left box, set per session
    /// via [`Splash::set_context`]. Empty for a resumed session, which shows no
    /// splash, and for the default state before the first session is wired up.
    context: Vec<ContextLine>,
    /// Top wrapped-line offset of the left Context box, advanced by the mouse
    /// wheel while the cursor is over that box. Clamped to the box's content
    /// every draw. Reset to 0 in [`Splash::set_context`].
    context_scroll: usize,
    /// Top wrapped-line offset of the right Notices box, with the same wheel
    /// behavior as `context_scroll`.
    notices_scroll: usize,
    /// Splash-local rect and scroll bound of the drawn Context box, recorded
    /// each [`draw`](Widget::draw) so [`handle_event`](Widget::handle_event)
    /// can hit-test the wheel against it. `None` when the box is hidden or
    /// absent.
    context_hit: Option<BoxHit>,
    /// Same as `context_hit`, for the Notices box.
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
                context: Vec::new(),
                context_scroll: 0,
                notices_scroll: 0,
                context_hit: None,
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

    /// Replace the structured context shown in the left box.
    ///
    /// Called after the world is built and again on each session switch: the
    /// splash persists across sessions while the context is per session, so the
    /// host pushes the new session's context in rather than the splash reading
    /// it from a shared cell.
    pub(crate) fn set_context(&mut self, context: Vec<ContextLine>) {
        self.context = context;
        // A new session's context and notices start scrolled to the top.
        self.context_scroll = 0;
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
        let key = default_action_shortcut(ACTION_PALETTE_OPEN).unwrap_or_default();
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

    /// The left "Context" box spans, or `None` when there is no context (a
    /// resumed session, which shows no splash anyway).
    ///
    /// Context lives here, in the prominent splash box, rather than folded into
    /// scrollback. A disabled skill row is struck to match aj, with the
    /// strikethrough on the row content only, never on the `  - ` bullet.
    fn context_spans(&self) -> Option<Vec<TextSpan>> {
        if self.context.is_empty() {
            return None;
        }
        let struck = Style {
            strikethrough: true,
            ..self.styles.dim
        };
        let mut spans = Vec::new();
        for line in &self.context {
            if !line.bullet.is_empty() {
                spans.push(TextSpan {
                    text: line.bullet.clone(),
                    style: self.styles.dim,
                    ..TextSpan::default()
                });
            }
            // The header (empty bullet) reads as a label, struck rows are
            // disabled skills, every other row is a dim listing entry.
            let style = if line.struck {
                struck
            } else if line.bullet.is_empty() {
                self.styles.text
            } else {
                self.styles.dim
            };
            spans.push(TextSpan {
                text: format!("{}\n", line.text),
                style,
                ..TextSpan::default()
            });
        }
        Some(spans)
    }

    /// The right "Notices" box spans, or `None` when the active view has no
    /// leading `Notice` entries. Only the leading run counts: once a user or
    /// assistant entry lands the splash is gone, so in practice this is every
    /// notice.
    fn notice_spans(&self) -> Option<Vec<TextSpan>> {
        let chat = self.chat.borrow();
        let transcript = chat.transcript(chat.active_view())?;
        let mut spans = Vec::new();
        for entry in transcript.entries() {
            let EntryKind::Notice(notice) = &entry.kind else {
                break;
            };
            let style = match notice.level {
                NoticeLevel::Info => self.styles.dim,
                NoticeLevel::Warning => self.styles.warning,
                NoticeLevel::Error => self.styles.error,
            };
            spans.push(TextSpan {
                text: format!("{}\n", notice.text),
                style,
                ..TextSpan::default()
            });
        }
        if spans.is_empty() { None } else { Some(spans) }
    }

    /// Geometry for the box row, or `None` when the slot cannot fit readable
    /// boxes. `count` is 1 or 2 (a 0 count short-circuits before calling).
    ///
    /// The hide thresholds are deliberate: two boxes need room for two readable
    /// inner widths plus the gap, and every box needs a minimum height below
    /// the hint. Below either, only the logo and hint show.
    fn plan_boxes(&self, slot_w: u16, slot_h: u16, box_top: u16, count: u16) -> Option<BoxRow> {
        let box_h = slot_h
            .checked_sub(box_top)?
            .checked_sub(BOX_BOTTOM_MARGIN)?;
        if box_h < MIN_BOX_HEIGHT {
            return None;
        }
        let inner_w = if count >= 2 {
            let each = slot_w.checked_sub(2 * BOX_CHROME + BOX_GAP)? / 2;
            if each < MIN_BOX_INNER {
                return None;
            }
            each.min(MAX_BOX_INNER)
        } else {
            let usable = slot_w.checked_sub(BOX_CHROME)?;
            if usable < MIN_BOX_INNER {
                return None;
            }
            usable.min(MAX_BOX_INNER)
        };
        let box_outer = inner_w + BOX_CHROME;
        let group_w = if count >= 2 {
            2 * box_outer + BOX_GAP
        } else {
            box_outer
        };
        Some(BoxRow {
            box_top,
            box_h,
            inner_w,
            box_outer,
            left: center_offset(slot_w, group_w),
        })
    }

    /// A bordered box `inner_w` + [`BOX_CHROME`] wide and `box_h` tall, with
    /// `spans` soft-wrapped to the inner width and windowed at `offset`.
    ///
    /// Content wraps rather than truncating. The returned tuple carries the
    /// offset clamped to the box's current content and the box's `max_scroll`,
    /// for the wheel handler to clamp against. When the wrapped content is
    /// taller than the inner height we draw a scrollbar thumb on the right inner
    /// edge whose position and size reflect the offset and total. The splash
    /// takes no focus and no keys, so the thumb is only an indicator, never a
    /// draggable scrollbar.
    fn render_box(
        &self,
        ctx: &DrawContext,
        spans: Vec<TextSpan>,
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

        // Wrap to the inner width with the height unbounded, so the wrapped
        // extent is the full content and we window it ourselves at `offset`.
        let inner_h = box_h.saturating_sub(2);
        let content_ctx = ctx.with_constraints(
            Size::default(),
            MaxSize {
                width: Some(inner_w),
                height: None,
            },
        );
        let content = RichText::new(spans).draw(&content_ctx);
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

    /// Draw one box at splash-local `col` / `row.box_top`, window it at the
    /// box's stored offset, clamp that offset to the box's current content, and
    /// record the box rect for wheel hit-testing.
    fn place_box(
        &mut self,
        ctx: &DrawContext,
        surface: &mut Surface,
        spans: Vec<TextSpan>,
        kind: BoxKind,
        col: u16,
        row: &BoxRow,
    ) {
        let offset = match kind {
            BoxKind::Context => self.context_scroll,
            BoxKind::Notices => self.notices_scroll,
        };
        let (box_surface, offset, max_scroll) =
            self.render_box(ctx, spans, row.inner_w, row.box_h, offset);
        let hit = BoxHit {
            col,
            row: row.box_top,
            width: row.box_outer,
            height: row.box_h,
            max_scroll,
        };
        match kind {
            BoxKind::Context => {
                self.context_scroll = offset;
                self.context_hit = Some(hit);
            }
            BoxKind::Notices => {
                self.notices_scroll = offset;
                self.notices_hit = Some(hit);
            }
        }
        surface.children.push(SubSurface {
            origin: RelativePoint {
                col: i32::from(col),
                row: i32::from(row.box_top),
            },
            surface: box_surface,
            z_index: 0,
        });
    }

    /// Scroll the box under the cursor by [`WHEEL_STEP`] wrapped lines, clamped
    /// to the box's content, and consume the event. A wheel over neither box
    /// (or already at a box's clamp) falls through unconsumed. Never requests
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
        for kind in [BoxKind::Context, BoxKind::Notices] {
            let hit = match kind {
                BoxKind::Context => self.context_hit,
                BoxKind::Notices => self.notices_hit,
            };
            let Some(hit) = hit else { continue };
            if !hit.contains(col, row) {
                continue;
            }
            let offset = match kind {
                BoxKind::Context => self.context_scroll,
                BoxKind::Notices => self.notices_scroll,
            };
            let next = if up {
                offset.saturating_sub(WHEEL_STEP)
            } else {
                offset.saturating_add(WHEEL_STEP).min(hit.max_scroll)
            };
            if next != offset {
                match kind {
                    BoxKind::Context => self.context_scroll = next,
                    BoxKind::Notices => self.notices_scroll = next,
                }
                ctx.consume_and_redraw();
            }
            return;
        }
    }

    /// Paint the light rounded frame of a box in the muted style.
    fn paint_box_frame(&self, surface: &mut Surface, box_w: u16, box_h: u16) {
        let border = self.styles.dim;
        // `plan_boxes` guarantees box_w >= MIN_BOX_INNER + BOX_CHROME and
        // box_h >= MIN_BOX_HEIGHT, so the corners never collide. The
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

        // Present boxes, left to right: context then notices. A box with no
        // content is dropped here, so an empty box leaves no frame.
        let context = self.context_spans();
        let notices = self.notice_spans();
        let count = u16::from(context.is_some()) + u16::from(notices.is_some());

        let slot_h = ctx.max.height.unwrap_or(region_h + 2);
        // With boxes the logo+hint sit near the top and the boxes fill the
        // space below the hint; with none, the logo+hint block is centered.
        let box_top = TOP_MARGIN + region_h + 3;
        let plan = if count == 0 {
            None
        } else {
            self.plan_boxes(slot_w, slot_h, box_top, count)
        };
        let top = if plan.is_some() {
            TOP_MARGIN
        } else {
            center_offset(slot_h, region_h + 2)
        };

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

        // The two boxes side by side, centered as a group and tall enough to
        // fill the space below the hint. Each present box records its
        // splash-local rect so the wheel can hit-test the cursor against it
        // (see `handle_wheel`); a hidden or absent box records `None`.
        self.context_hit = None;
        self.notices_hit = None;
        if let Some(row) = plan {
            let mut col = row.left;
            if let Some(spans) = context {
                self.place_box(ctx, &mut surface, spans, BoxKind::Context, col, &row);
                col += row.box_outer + BOX_GAP;
            }
            if let Some(spans) = notices {
                self.place_box(ctx, &mut surface, spans, BoxKind::Notices, col, &row);
            }
        }

        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            // The host posts a wake at startup. The Shell forwards it here so
            // the tick chain starts (widgets can only tick from a handler).
            Event::App(user) if user.name == SPLASH_WAKE_EVENT => self.arm_tick(ctx),
            // The wheel scrolls whichever box the cursor is over. vxfw hands us
            // a splash-local position (the splash is a hit target via
            // `wants_events`), which we hit-test against the box rects `draw`
            // recorded. Focus is untouched, matching how the transcript scrolls
            // on the wheel while the editor keeps focus.
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

/// Geometry for the side-by-side box row: where the leftmost box starts, the
/// shared inner and outer widths, and the shared height. Computed by
/// [`Splash::plan_boxes`].
struct BoxRow {
    box_top: u16,
    box_h: u16,
    inner_w: u16,
    box_outer: u16,
    left: u16,
}

/// Which of the two boxes a scroll offset and hit rect belong to.
#[derive(Clone, Copy)]
enum BoxKind {
    Context,
    Notices,
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

    /// Leading notices render inside a bordered box.
    #[test]
    fn renders_notices_in_a_bordered_box() {
        let chat = chat();
        append_notice(&chat, "startup warning");
        let splash = splash(chat);
        let surface = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(60, Some(24)));
        let joined = crate::test_support::rows(&surface).join("\n");
        assert!(joined.contains('╭') && joined.contains('╯'), "box frame");
        assert!(joined.contains("startup warning"), "notice text");
    }

    /// A ContextLine for the disabled-skill row aj strikes.
    fn ctx_line(bullet: &str, text: &str, struck: bool) -> ContextLine {
        ContextLine {
            bullet: bullet.to_string(),
            text: text.to_string(),
            struck,
        }
    }

    /// The context box renders the listing and strikes the disabled skill's
    /// row content, never the bullet.
    #[test]
    fn context_box_strikes_disabled_skill_rows() {
        let splash = splash(chat());
        splash.borrow_mut().set_context(vec![
            ctx_line("", "Context:", false),
            ctx_line("  - ", "alpha (skill: alpha)", false),
            ctx_line("  - ", "beta (skill: beta, disabled)", true),
        ]);
        let surface = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(80, Some(30)));
        let joined = crate::test_support::rows(&surface).join("\n");
        assert!(joined.contains("Context:"), "context header: {joined}");
        assert!(
            joined.contains("beta (skill: beta, disabled)"),
            "disabled skill row: {joined}"
        );
        // The struck cells are exactly the disabled row's content: the word
        // "beta" appears there but the `-` of any bullet never does.
        let struck: String = crate::test_support::flatten(&surface)
            .iter()
            .flatten()
            .filter(|c| c.style.strikethrough)
            .map(|c| c.char.grapheme())
            .collect();
        assert!(
            struck.contains("beta"),
            "the disabled row content is struck: {struck:?}"
        );
        assert!(
            !struck.contains('-'),
            "the bullet is never struck: {struck:?}"
        );
    }

    /// With both context and notices, the splash draws two boxes side by side.
    #[test]
    fn renders_two_boxes_side_by_side() {
        let chat = chat();
        append_notice(&chat, "startup warning");
        let splash = splash(chat);
        splash.borrow_mut().set_context(vec![
            ctx_line("", "Context:", false),
            ctx_line("  - ", "builtin (system prompt)", false),
        ]);
        let surface = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(90, Some(30)));
        let rows = crate::test_support::rows(&surface);
        let joined = rows.join("\n");
        assert!(joined.contains("Context:"), "context box: {rows:?}");
        assert!(joined.contains("startup warning"), "notices box: {rows:?}");
        assert!(
            rows.iter().any(|r| r.matches('╭').count() == 2),
            "two top-left corners on the box row: {rows:?}"
        );
    }

    /// A slot too narrow for two readable boxes, or too short to fit a box
    /// under the hint, hides the boxes and keeps only the logo and hint.
    #[test]
    fn hides_boxes_when_slot_is_too_small() {
        let chat = chat();
        append_notice(&chat, "startup warning");
        let splash = splash(chat);
        splash
            .borrow_mut()
            .set_context(vec![ctx_line("", "Context:", false)]);

        let narrow = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(40, Some(30)));
        let narrow = crate::test_support::rows(&narrow).join("\n");
        assert!(narrow.contains("for commands"), "hint remains: {narrow}");
        assert!(!narrow.contains('╭'), "boxes hidden when narrow: {narrow}");

        let short = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(80, Some(14)));
        let short = crate::test_support::rows(&short).join("\n");
        assert!(short.contains("for commands"), "hint remains: {short}");
        assert!(!short.contains('╭'), "boxes hidden when short: {short}");
    }

    /// Content taller than the box draws the overflow thumb on the right inner
    /// edge, a non-interactive indicator (the splash takes no focus or keys).
    #[test]
    fn overflow_draws_a_scrollbar_thumb() {
        let splash = splash(chat());
        let lines: Vec<ContextLine> = (0..40)
            .map(|i| ctx_line("  - ", &format!("row {i}"), false))
            .collect();
        splash.borrow_mut().set_context(lines);
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

    /// A new session starts scrolled to the top: `set_context` resets both box
    /// offsets, so a prior session's scroll does not carry over.
    #[test]
    fn set_context_resets_both_scroll_offsets() {
        let splash = splash(chat());
        splash.borrow_mut().context_scroll = 5;
        splash.borrow_mut().notices_scroll = 7;
        splash
            .borrow_mut()
            .set_context(vec![ctx_line("", "Context:", false)]);
        assert_eq!(splash.borrow().context_scroll, 0, "context offset reset");
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

    /// A splash whose Notices box overflows a 30-row slot, drawn once so its
    /// box rect is recorded. Returns the splash and the recorded box hit.
    fn notices_splash() -> (Rc<RefCell<Splash>>, BoxHit) {
        let chat = chat();
        for i in 0..40 {
            append_notice(&chat, &format!("notice {i}"));
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

    /// A wheel that lands over neither box is ignored: nothing scrolls and the
    /// event is not consumed, so it falls through to whatever is behind.
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

    /// A splash with both boxes overflowing a 30-row slot, drawn once so both
    /// box rects are recorded. Returns the splash and the context and notices
    /// box hits, in that order.
    fn two_box_splash() -> (Rc<RefCell<Splash>>, BoxHit, BoxHit) {
        let chat = chat();
        for i in 0..40 {
            append_notice(&chat, &format!("notice {i}"));
        }
        let splash = splash(chat);
        let mut lines = vec![ctx_line("", "Context:", false)];
        lines.extend((0..40).map(|i| ctx_line("  - ", &format!("row {i}"), false)));
        splash.borrow_mut().set_context(lines);
        let _ = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(90, Some(30)));
        let context_hit = splash.borrow().context_hit.expect("context box drawn");
        let notices_hit = splash.borrow().notices_hit.expect("notices box drawn");
        (splash, context_hit, notices_hit)
    }

    /// The wheel scrolls only the box under the cursor: a notch over the context
    /// box moves `context_scroll` and leaves `notices_scroll` alone, and vice
    /// versa. Fails if an arm reads or writes the other box's offset.
    #[test]
    fn wheel_routes_to_the_box_under_the_cursor() {
        let (splash, context_hit, notices_hit) = two_box_splash();
        assert!(
            context_hit.max_scroll > 0 && notices_hit.max_scroll > 0,
            "both boxes overflow"
        );

        let mut ctx = EventContext::new();
        splash.borrow_mut().handle_event(
            &mut ctx,
            &wheel(
                context_hit.col + 2,
                context_hit.row + 1,
                mouse::Button::WheelDown,
            ),
        );
        assert_eq!(
            splash.borrow().context_scroll,
            WHEEL_STEP,
            "the context wheel moved the context box"
        );
        assert_eq!(
            splash.borrow().notices_scroll,
            0,
            "the context wheel left the notices box alone"
        );

        let mut ctx = EventContext::new();
        splash.borrow_mut().handle_event(
            &mut ctx,
            &wheel(
                notices_hit.col + 2,
                notices_hit.row + 1,
                mouse::Button::WheelDown,
            ),
        );
        assert_eq!(
            splash.borrow().notices_scroll,
            WHEEL_STEP,
            "the notices wheel moved the notices box"
        );
        assert_eq!(
            splash.borrow().context_scroll,
            WHEEL_STEP,
            "the notices wheel left the context box unchanged"
        );
    }

    /// A long disabled-skill row wrapped across lines keeps the strikethrough
    /// on the wrapped continuation, not just the first line.
    #[test]
    fn struck_context_row_keeps_strikethrough_on_wrapped_continuation() {
        let splash = splash(chat());
        let long = "disabled skill row that is long enough to wrap ".repeat(3);
        splash.borrow_mut().set_context(vec![
            ctx_line("", "Context:", false),
            ctx_line("  - ", &long, true),
        ]);
        let surface = splash
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(80, Some(30)));
        let struck_rows: Vec<usize> = crate::test_support::flatten(&surface)
            .iter()
            .enumerate()
            .filter(|(_, row)| row.iter().any(|c| c.style.strikethrough))
            .map(|(i, _)| i)
            .collect();
        assert!(
            struck_rows.len() >= 2,
            "strikethrough survives the wrap onto the continuation: {struck_rows:?}"
        );
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
