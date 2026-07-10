//! The empty-state splash: an animated `aj` wordmark, a command-palette hint,
//! and a bordered box of the startup notices (Spec E-9).
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

/// The empty-state splash widget. Non-interactive: it takes no focus and
/// consumes no keys, only self-targeted ticks and the startup wake.
pub(crate) struct Splash {
    /// Weak self-reference so tick commands can target this widget, captured at
    /// construction with [`Rc::new_cyclic`] like the loader.
    me: Weak<RefCell<Splash>>,
    chat: Rc<RefCell<ChatState>>,
    styles: Rc<TranscriptStyles>,
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

    /// The bordered notices box, or `None` when the active view has no leading
    /// `Notice` entries. Only the leading run counts: once a user or assistant
    /// entry lands the splash is gone, so in practice this is every notice.
    fn draw_notices_box(&self, ctx: &DrawContext, slot_w: u16) -> Option<Surface> {
        let chat = self.chat.borrow();
        let transcript = chat.transcript(chat.active_view())?;
        let mut lines: Vec<(String, Style)> = Vec::new();
        for entry in transcript.entries() {
            let EntryKind::Notice(notice) = &entry.kind else {
                break;
            };
            let style = match notice.level {
                NoticeLevel::Info => self.styles.dim,
                NoticeLevel::Warning => self.styles.warning,
                NoticeLevel::Error => self.styles.error,
            };
            for line in notice.text.lines() {
                lines.push((line.to_string(), style));
            }
        }
        if lines.is_empty() {
            return None;
        }

        // 2 border columns plus 1 padding column on each side.
        const CHROME: u16 = 4;
        let max_inner = slot_w.saturating_sub(CHROME).max(1);
        let mut inner_w = 0u16;
        for (line, _) in &lines {
            let w = u16::try_from(ctx.string_width(line))
                .unwrap_or(u16::MAX)
                .min(max_inner);
            inner_w = inner_w.max(w);
        }
        let box_w = inner_w + CHROME;
        let box_h = u16::try_from(lines.len()).unwrap_or(u16::MAX) + 2;
        let mut surface = Surface::with_size(Size {
            width: box_w,
            height: box_h,
        });
        self.paint_box_frame(&mut surface, box_w, box_h);

        let content_end = 2 + inner_w;
        for (i, (line, style)) in lines.iter().enumerate() {
            let Ok(offset) = u16::try_from(i) else {
                break;
            };
            let row = 1 + offset;
            let mut col = 2u16;
            for item in ctx.grapheme_iterator(line) {
                if col >= content_end {
                    // The line is wider than the box: mark the clip with an
                    // ellipsis in the last content cell and stop.
                    surface.write_cell(content_end - 1, row, glyph_cell("…", *style));
                    break;
                }
                let grapheme = item.bytes(line);
                let w = u8::try_from(ctx.string_width(grapheme)).unwrap_or(1).max(1);
                surface.write_cell(col, row, glyph_cell(grapheme, *style));
                col = col.saturating_add(u16::from(w));
            }
        }
        Some(surface)
    }

    /// Paint the light rounded frame of the notices box in the muted style.
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

        let notices = self.draw_notices_box(ctx, slot_w);

        // Vertical layout: region, blank, hint, then (if present) blank + box.
        let mut content_h = region_h + 2;
        if let Some(box_surface) = &notices {
            content_h += 1 + box_surface.size.height;
        }
        let slot_h = ctx.max.height.unwrap_or(content_h);

        let mut surface = Surface::with_size(Size {
            width: slot_w,
            height: slot_h,
        });
        let top = center_offset(slot_h, content_h);

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

        if let Some(box_surface) = notices {
            let box_row = top + region_h + 3;
            surface.children.push(SubSurface {
                origin: RelativePoint {
                    col: i32::from(center_offset(slot_w, box_surface.size.width)),
                    row: i32::from(box_row),
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
