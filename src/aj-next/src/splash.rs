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
    /// `spans` soft-wrapped to the inner width.
    ///
    /// Content wraps rather than truncating. When the wrapped content is taller
    /// than the inner height we draw a scrollbar thumb on the right inner edge
    /// as an overflow indicator. The splash takes no focus and no keys, so it
    /// is only an indicator, never a draggable scrollbar.
    fn render_box(
        &self,
        ctx: &DrawContext,
        spans: Vec<TextSpan>,
        inner_w: u16,
        box_h: u16,
    ) -> Surface {
        let box_w = inner_w + BOX_CHROME;
        let mut surface = Surface::with_size(Size {
            width: box_w,
            height: box_h,
        });
        self.paint_box_frame(&mut surface, box_w, box_h);

        // Wrap to the inner width with the height unbounded, so the drawn
        // height is the full wrapped extent and overflow past the box shows.
        let inner_h = box_h - 2;
        let content_ctx = ctx.with_constraints(
            Size::default(),
            MaxSize {
                width: Some(inner_w),
                height: None,
            },
        );
        let content = RichText::new(spans).draw(&content_ctx);
        let total_rows = content.size.height;
        let visible = inner_h.min(total_rows);
        let content_w = content.size.width.min(inner_w);
        for row in 0..visible {
            for col in 0..content_w {
                surface.write_cell(2 + col, 1 + row, content.read_cell(col, row));
            }
        }
        if total_rows > inner_h {
            let thumb_col = box_w - 2;
            for row in 0..thumb_height(inner_h, total_rows) {
                surface.write_cell(
                    thumb_col,
                    1 + row,
                    glyph_cell("\u{2590}", self.styles.scrollbar_thumb),
                );
            }
        }
        surface
    }

    /// Paint the light rounded frame of a box in the muted style.
    fn paint_box_frame(&self, surface: &mut Surface, box_w: u16, box_h: u16) {
        let border = self.styles.dim;
        // box_w >= CHROME (4) and box_h >= 2, so the corners never collide.
        let last_col = box_w - 1;
        let last_row = box_h - 1;
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
        // fill the space below the hint.
        if let Some(row) = plan {
            let mut col = row.left;
            for spans in [context, notices].into_iter().flatten() {
                let box_surface = self.render_box(ctx, spans, row.inner_w, row.box_h);
                surface.children.push(SubSurface {
                    origin: RelativePoint {
                        col: i32::from(col),
                        row: i32::from(row.box_top),
                    },
                    surface: box_surface,
                    z_index: 0,
                });
                col += row.box_outer + BOX_GAP;
            }
        }

        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            // The host posts a wake at startup. The Shell forwards it here so
            // the tick chain starts (widgets can only tick from a handler).
            Event::App(user) if user.name == SPLASH_WAKE_EVENT => self.arm_tick(ctx),
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

/// Height of the overflow thumb: the visible fraction of the wrapped content,
/// at least one row and at most the inner height.
fn thumb_height(inner_h: u16, total_rows: u16) -> u16 {
    let inner = usize::from(inner_h);
    let total = usize::from(total_rows).max(1);
    let h = (inner * inner / total).max(1);
    u16::try_from(h).unwrap_or(inner_h).min(inner_h)
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
