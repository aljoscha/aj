//! The `Vaxis` runtime: owns both screen buffers and the capability set, runs
//! capability detection, and diff-renders to a writer.
//!
//! # The writer seam
//!
//! Every method that produces wire output takes a `&mut impl std::io::Write`
//! rather than a TTY. The runtime never holds the fd. This is the single most
//! important structural property of the port: the renderer is exercised
//! against an in-memory buffer in tests, and the application (or the phase-7
//! loop) is responsible for handing the runtime the TTY's buffered writer and
//! flushing it.
//!
//! # Teardown: no `Drop` reset
//!
//! Upstream's `deinit` takes the TTY writer and runs the ordered `resetState`
//! teardown. A Rust `Drop` has no writer to hand it, so we deliberately do not
//! reset the terminal in `Drop`. Instead [`Vaxis::reset_state`] takes the
//! writer explicitly and the application or loop calls it on shutdown, mirroring
//! upstream's explicit `deinit(alloc, tty)`. The owned buffers (`screen`,
//! `screen_last`) free themselves through their own `Drop`.
//!
//! # Why `screen` is a `RefCell`
//!
//! [`Window`] is a cheap value type that borrows the screen and writes through
//! it (see the window module's borrow model). Several child windows over one
//! screen must all be able to write, so the screen is wrapped in a `RefCell`
//! and [`Vaxis::window`] hands out `Window<'_>` views that share it. The
//! renderer itself never mutates the front screen, so it borrows the `RefCell`
//! immutably for the duration of a render and mutates the back buffer
//! (`screen_last`) and `state` through the disjoint fields of `self`.

use std::cell::RefCell;
use std::env;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use base64::Engine as _;
use image::DynamicImage;
use image::codecs::png::PngEncoder;

use crate::Winsize;
use crate::cell::{Color, CursorShape, Hyperlink, Kind, Scale, Style, Underline};
use crate::ctlseqs;
use crate::error::Error;
use crate::gwidth;
use crate::image::{Image, Source, TransmitFormat, TransmitMedium};
use crate::internal_screen::InternalScreen;
use crate::key::KittyFlags;
use crate::mouse::{Mouse, Shape as MouseShape};
use crate::screen::{Cursor, Screen};
use crate::window::Window;

/// The set of terminal features Vaxis may use. Detected by capability queries
/// (and overridden by environment variables) before rendering. All default
/// off, with `unicode` defaulting to `wcwidth`, matching upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub kitty_keyboard: bool,
    pub kitty_graphics: bool,
    pub rgb: bool,
    pub unicode: gwidth::Method,
    pub sgr_pixels: bool,
    pub color_scheme_updates: bool,
    pub explicit_width: bool,
    pub scaled_text: bool,
    pub multi_cursor: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            kitty_keyboard: false,
            kitty_graphics: false,
            rgb: false,
            unicode: gwidth::Method::Wcwidth,
            sgr_pixels: false,
            color_scheme_updates: false,
            explicit_width: false,
            scaled_text: false,
            multi_cursor: false,
        }
    }
}

/// Runtime options supplied to [`Vaxis::new`].
#[derive(Debug, Clone)]
pub struct Options {
    /// The kitty keyboard-protocol flag set pushed when the feature is enabled.
    pub kitty_keyboard_flags: KittyFlags,

    /// Whether the application may read the system clipboard (OSC 52 query).
    ///
    /// NOTE: Upstream gates clipboard reads behind an optional allocator
    /// (`system_clipboard_allocator`), because the response payload must be
    /// heap-allocated to hand back to the caller. We own our buffers, so the
    /// allocator collapses to a plain capability flag. When false,
    /// [`Vaxis::request_system_clipboard`] refuses, mirroring upstream's
    /// "no allocator, no request" behavior.
    pub system_clipboard_enabled: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            kitty_keyboard_flags: KittyFlags::default(),
            system_clipboard_enabled: false,
        }
    }
}

/// The SGR wire dialect. `Standard` uses ITU-T colon subparameters, `Legacy`
/// uses semicolons. Selected by capability detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sgr {
    Standard,
    Legacy,
}

/// Mutable terminal-mode bookkeeping. Each feature the runtime turns on flips
/// the matching bit so [`Vaxis::reset_state`] can turn exactly those features
/// back off, in order, on teardown.
#[derive(Debug, Default)]
struct State {
    alt_screen: bool,
    kitty_keyboard: bool,
    bracketed_paste: bool,
    mouse: bool,
    pixel_mouse: bool,
    color_scheme_updates: bool,
    in_band_resize: bool,
    changed_default_fg: bool,
    changed_default_bg: bool,
    changed_cursor_color: bool,
    /// The cursor position as last emitted to the terminal. The renderer uses
    /// it to position relatively on the primary screen.
    cursor: Cursor,
    cursor_secondary: Vec<Cursor>,
    prev_cursor_secondary: Vec<Cursor>,
}

/// The cursor position tracked while walking cells during a render.
#[derive(Debug, Clone, Copy, Default)]
struct CursorPos {
    row: u16,
    col: u16,
}

/// The terminal UI runtime: two cell buffers, the capability set, capability
/// detection, and the diff renderer.
pub struct Vaxis {
    /// The front buffer the application draws into through [`Vaxis::window`].
    /// `RefCell` so several `Window` views can share and write it.
    pub screen: RefCell<Screen>,

    /// The last frame we drew, kept to diff against on the next render.
    pub screen_last: InternalScreen,

    pub caps: Capabilities,
    pub opts: Options,

    /// When true the next render redraws the entire screen.
    pub refresh: bool,

    pub next_img_id: u32,

    /// Enable workarounds for terminal escape-sequence bugs. Currently only the
    /// conpty underline-color form.
    pub enable_workarounds: bool,

    sgr: Sgr,

    /// DA1 capability handshake. [`Vaxis::query_terminal`] blocks on the condvar
    /// until the loop calls [`Vaxis::notify_queries_done`] (woken when the
    /// `cap_da1` response is parsed), or the timeout elapses.
    ///
    /// NOTE: One-shot per runtime, like upstream's futex: the flag is set when
    /// DA1 arrives and never cleared. A second `query_terminal` returns at once.
    da1_received: Mutex<bool>,
    da1_condvar: Condvar,

    /// False while a query batch is outstanding. The input layer reads this to
    /// disambiguate the explicit cursor-position reports (which collide with F3
    /// key encoding) from a real F3 keypress. Atomic because the reader thread
    /// reads it while the main thread writes it.
    queries_done: AtomicBool,

    state: State,
}

impl Vaxis {
    /// Creates a runtime with empty (0x0) buffers. The application must
    /// [`Vaxis::resize`] before rendering anything.
    ///
    /// NOTE: Upstream's `init` also takes an allocator and an environment map.
    /// We own our buffers (no allocator) and read environment variables
    /// directly via [`std::env`] during capability detection, so both
    /// parameters collapse away.
    pub fn new(opts: Options) -> Self {
        Self {
            screen: RefCell::new(Screen::default()),
            screen_last: InternalScreen::new(0, 0),
            caps: Capabilities::default(),
            opts,
            refresh: false,
            next_img_id: 1,
            enable_workarounds: true,
            sgr: Sgr::Standard,
            da1_received: Mutex::new(false),
            da1_condvar: Condvar::new(),
            queries_done: AtomicBool::new(true),
            state: State::default(),
        }
    }

    /// Returns a [`Window`] over the entire screen.
    ///
    /// The returned window borrows `self`, so it must be dropped before any
    /// `&mut self` call (such as [`Vaxis::render`]).
    pub fn window(&self) -> Window<'_> {
        let (width, height) = {
            let screen = self.screen.borrow();
            (screen.width, screen.height)
        };
        Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width,
            height,
            screen: &self.screen,
        }
    }

    /// Marks the next render as a full refresh.
    pub fn queue_refresh(&mut self) {
        self.refresh = true;
    }

    /// Reallocates both buffers for `winsize`, sends the cursor home, and emits
    /// a hardware clear-below-cursor. Every cell is redrawn on the next render.
    pub fn resize<W: Write>(&mut self, w: &mut W, winsize: Winsize) -> Result<(), Error> {
        {
            let mut screen = self.screen.borrow_mut();
            *screen = Screen::new(winsize);
            screen.width_method = self.caps.unicode;
        }
        // Only the front screen is sized to the new geometry; reinitializing the
        // back buffer to defaults forces every cell to redraw next render.
        self.screen_last = InternalScreen::new(winsize.cols, winsize.rows);

        if self.state.alt_screen {
            w.write_all(ctlseqs::HOME.as_bytes())?;
        } else {
            for _ in 0..self.state.cursor.row {
                w.write_all(ctlseqs::RI.as_bytes())?;
            }
            w.write_all(b"\r")?;
        }
        self.state.cursor.row = 0;
        self.state.cursor.col = 0;
        w.write_all(ctlseqs::SGR_RESET.as_bytes())?;
        w.write_all(ctlseqs::ERASE_BELOW_CURSOR.as_bytes())?;
        w.flush()?;
        Ok(())
    }

    /// Resets enabled features, returns the cursor home, and clears below it.
    ///
    /// The teardown order is load-bearing and mirrors upstream exactly: show
    /// the cursor, reset SGR, reset the cursor shape if changed, pop the kitty
    /// keyboard stack, disable mouse and bracketed paste, do the alt-screen vs
    /// primary-screen cursor-home dance, then undo any color-scheme, in-band
    /// resize, and default-color changes. Each step is gated on its `state` bit
    /// so we only undo what we turned on. Showing the cursor and resetting SGR
    /// are best-effort (errors ignored), the rest propagate.
    pub fn reset_state<W: Write>(&mut self, w: &mut W) -> Result<(), Error> {
        let _ = w.write_all(ctlseqs::SHOW_CURSOR.as_bytes());
        let _ = w.write_all(ctlseqs::SGR_RESET.as_bytes());
        if self.screen.borrow().cursor_shape != CursorShape::Default {
            // `.default` resets to the configured shape on most terminals (a
            // blinking block on the rest).
            let _ = ctlseqs::cursor_shape(w, CursorShape::Default);
        }
        if self.state.kitty_keyboard {
            w.write_all(ctlseqs::CSI_U_POP.as_bytes())?;
            self.state.kitty_keyboard = false;
        }
        if self.state.mouse {
            self.set_mouse_mode(w, false)?;
        }
        if self.state.bracketed_paste {
            self.set_bracketed_paste(w, false)?;
        }
        if self.state.alt_screen {
            w.write_all(ctlseqs::HOME.as_bytes())?;
            w.write_all(ctlseqs::ERASE_BELOW_CURSOR.as_bytes())?;
            self.exit_alt_screen(w)?;
        } else {
            w.write_all(b"\r")?;
            for _ in 0..self.state.cursor.row {
                w.write_all(ctlseqs::RI.as_bytes())?;
            }
            w.write_all(ctlseqs::ERASE_BELOW_CURSOR.as_bytes())?;
        }
        if self.state.color_scheme_updates {
            w.write_all(ctlseqs::COLOR_SCHEME_RESET.as_bytes())?;
            self.state.color_scheme_updates = false;
        }
        if self.state.in_band_resize {
            w.write_all(ctlseqs::IN_BAND_RESIZE_RESET.as_bytes())?;
            self.state.in_band_resize = false;
        }
        if self.state.changed_default_fg {
            w.write_all(ctlseqs::OSC10_RESET.as_bytes())?;
            self.state.changed_default_fg = false;
        }
        if self.state.changed_default_bg {
            w.write_all(ctlseqs::OSC11_RESET.as_bytes())?;
            self.state.changed_default_bg = false;
        }
        if self.state.changed_cursor_color {
            w.write_all(ctlseqs::OSC12_RESET.as_bytes())?;
            self.state.changed_cursor_color = false;
        }
        w.flush()?;
        Ok(())
    }

    /// Draws the screen to the terminal, emitting only what changed since the
    /// last render.
    ///
    /// The early return when nothing started is observable: a clean render of an
    /// unchanged screen emits zero bytes.
    pub fn render<W: Write>(&mut self, w: &mut W) -> Result<(), Error> {
        let mut sync_active = false;
        let result = self.render_inner(w, &mut sync_active);
        // On a mid-render error we still leave synchronized output if we opened
        // it, so the terminal does not stay frozen.
        if result.is_err() && sync_active {
            let _ = w.write_all(ctlseqs::SYNC_RESET.as_bytes());
        }
        self.refresh = false;
        result
    }

    fn render_inner<W: Write>(&mut self, w: &mut W, sync_active: &mut bool) -> Result<(), Error> {
        let screen = self.screen.borrow();
        debug_assert_eq!(
            screen.buf.len(),
            usize::from(screen.width) * usize::from(screen.height),
        );
        debug_assert_eq!(screen.buf.len(), self.screen_last.buf.len());

        let mut started = false;

        let cursor_vis_changed = screen.cursor_vis != self.screen_last.cursor_vis;
        let cursor_shape_changed = screen.cursor_shape != self.screen_last.cursor_shape;
        let mouse_shape_changed = screen.mouse_shape != self.screen_last.mouse_shape;
        let cursor_pos_changed = screen.cursor_vis
            && (screen.cursor.row != self.state.cursor.row
                || screen.cursor.col != self.state.cursor.col);
        // NOTE: This faithfully reproduces upstream's `std.meta.eql` on slices,
        // which compares pointer and length, not elements. The variable name
        // "changed" is therefore misleading: it is true when the two slices are
        // the *same* slice. We own `Vec`s rather than share slice headers, so
        // the pointers coincide only when both are empty (independent empty
        // `Vec`s share the dangling alignment pointer). The multi_cursor path is
        // gated on a queried capability we never enable here, so this only
        // matters once that lands. See the report's fidelity note.
        let cursor_secondary_changed =
            screen.cursor_vis && same_slice(&screen.cursor_secondary, &self.state.cursor_secondary);
        let needs_render = self.refresh
            || cursor_vis_changed
            || cursor_shape_changed
            || mouse_shape_changed
            || cursor_pos_changed
            || cursor_secondary_changed;

        let mut reposition = false;
        let mut row: u16 = 0;
        let mut col: u16 = 0;
        // The style and link last emitted to the terminal, diffed against each
        // cell to decide what SGR/OSC8 to write.
        let mut last_style = Style::default();
        let mut link = Hyperlink::default();
        let mut cursor_pos = CursorPos::default();

        // `skip` is per-render scaled-text coverage and is cleared each frame.
        // `skipped` (wide-grapheme trailing coverage) persists across frames.
        for last_cell in &mut self.screen_last.buf {
            last_cell.skip = false;
        }

        if needs_render {
            start_render(
                w,
                self.state.alt_screen,
                self.state.cursor.row,
                self.caps.kitty_graphics,
                &mut cursor_pos,
                &mut reposition,
                &mut started,
                sync_active,
            )?;
        }

        let buf_len = screen.buf.len();
        let mut i = 0usize;
        while i < buf_len {
            let cell = screen.buf[i].clone();
            let cw: u16 = if cell.char.width != 0 {
                u16::from(cell.char.width)
            } else {
                gwidth::gwidth(cell.char.grapheme(), self.caps.unicode).max(1)
            };
            debug_assert!(cw > 0);

            if col >= screen.width {
                row += 1;
                col = 0;
                // Rely on terminal autowrap into the next row when the previous
                // cell wrapped, otherwise force a reposition.
                if !cell.wrapped {
                    reposition = true;
                }
            }

            let skip = {
                let last = &self.screen_last.buf[i];
                (!self.refresh && last.eql(&cell) && !last.skipped && cell.image.is_none())
                    || last.skip
            };
            if skip {
                reposition = true;
                // Close any open OSC 8 hyperlink before we move on.
                if !link.uri.is_empty() {
                    w.write_all(ctlseqs::OSC8_CLEAR.as_bytes())?;
                }
                advance_cell(&mut self.screen_last, &mut i, &mut col, cw);
                continue;
            }

            if !started {
                start_render(
                    w,
                    self.state.alt_screen,
                    self.state.cursor.row,
                    self.caps.kitty_graphics,
                    &mut cursor_pos,
                    &mut reposition,
                    &mut started,
                    sync_active,
                )?;
            }
            self.screen_last.buf[i].skipped = false;
            self.screen_last.write_cell(col, row, &cell);

            // Scaled cells cover a rows*cols block. Mark the covered back-buffer
            // cells `skip` so we do not redraw underneath the scaled glyph.
            if self.caps.scaled_text && cell.scale.scale > 1 {
                debug_assert!(cell.char.width > 0);
                let cols = usize::from(cell.scale.scale) * usize::from(cell.char.width);
                let rows = usize::from(cell.scale.scale);
                for skipped_row in 0..rows {
                    for skipped_col in 0..cols {
                        if skipped_row == 0 && skipped_col == 0 {
                            continue;
                        }
                        let skipped_i = (usize::from(row) + skipped_row)
                            * usize::from(self.screen_last.width)
                            + (usize::from(col) + skipped_col);
                        self.screen_last.buf[skipped_i].skip = true;
                    }
                }
            }

            if reposition {
                reposition = false;
                link = Hyperlink::default();
                if self.state.alt_screen {
                    ctlseqs::cup(w, row + 1, col + 1)?;
                } else if cursor_pos.row == row {
                    let n = col - cursor_pos.col;
                    if n > 0 {
                        ctlseqs::cuf(w, n)?;
                    }
                } else {
                    let n = row - cursor_pos.row;
                    for _ in 0..n {
                        w.write_all(b"\n")?;
                    }
                    w.write_all(b"\r")?;
                    if col > 0 {
                        ctlseqs::cuf(w, col)?;
                    }
                }
            }

            if let Some(img) = &cell.image {
                emit_image_placement(w, img)?;
            }

            self.emit_style_diff(w, &last_style, &cell.style)?;

            if link.uri != cell.link.uri {
                let params = if cell.link.uri.is_empty() {
                    ""
                } else {
                    cell.link.params.as_str()
                };
                ctlseqs::osc8(w, params, &cell.link.uri)?;
            }

            if self.caps.scaled_text && !cell.scale.eql(&Scale::default()) {
                let scale = cell.scale;
                match scale.denominator {
                    0 => unreachable!("scale denominator cannot be 0"),
                    1 => ctlseqs::scaled_text(w, scale.scale, cw, cell.char.grapheme())?,
                    _ => ctlseqs::scaled_text_with_fractions(
                        w,
                        scale.scale,
                        cw,
                        scale.numerator,
                        scale.denominator,
                        scale.vertical_alignment,
                        cell.char.grapheme(),
                    )?,
                }
                cursor_pos.col = col + (cw * u16::from(scale.scale));
                cursor_pos.row = row;
                last_style = cell.style;
                link = cell.link.clone();
                advance_cell(&mut self.screen_last, &mut i, &mut col, cw);
                continue;
            }

            if self.caps.explicit_width && cw > 1 {
                ctlseqs::explicit_width(w, cw, cell.char.grapheme())?;
            } else {
                w.write_all(cell.char.grapheme().as_bytes())?;
            }
            cursor_pos.col = col + cw;
            cursor_pos.row = row;

            last_style = cell.style;
            link = cell.link.clone();
            advance_cell(&mut self.screen_last, &mut i, &mut col, cw);
        }

        if !started {
            return Ok(());
        }

        if screen.cursor_vis {
            if self.state.alt_screen {
                ctlseqs::cup(w, screen.cursor.row + 1, screen.cursor.col + 1)?;
            } else {
                w.write_all(b"\r")?;
                if screen.cursor.row >= cursor_pos.row {
                    for _ in 0..(screen.cursor.row - cursor_pos.row) {
                        w.write_all(b"\n")?;
                    }
                } else {
                    for _ in 0..(cursor_pos.row - screen.cursor.row) {
                        w.write_all(ctlseqs::RI.as_bytes())?;
                    }
                }
                if screen.cursor.col > 0 {
                    ctlseqs::cuf(w, screen.cursor.col)?;
                }
            }
            self.state.cursor.row = screen.cursor.row;
            self.state.cursor.col = screen.cursor.col;
            w.write_all(ctlseqs::SHOW_CURSOR.as_bytes())?;
        } else {
            self.state.cursor.row = cursor_pos.row;
            self.state.cursor.col = cursor_pos.col;
        }

        if screen.cursor_vis && self.caps.multi_cursor {
            w.write_all(ctlseqs::RESET_SECONDARY_CURSORS.as_bytes())?;
            for cur in &screen.cursor_secondary {
                ctlseqs::show_secondary_cursor(w, cur.row + 1, cur.col + 1)?;
            }
            if cursor_secondary_changed {
                self.state.prev_cursor_secondary = std::mem::take(&mut self.state.cursor_secondary);
                self.state.cursor_secondary = screen.cursor_secondary.clone();
            }
        }

        self.screen_last.cursor_vis = screen.cursor_vis;
        if screen.mouse_shape != self.screen_last.mouse_shape {
            ctlseqs::osc22_mouse_shape(w, screen.mouse_shape.to_wire())?;
            self.screen_last.mouse_shape = screen.mouse_shape;
        }
        if screen.cursor_shape != self.screen_last.cursor_shape {
            ctlseqs::cursor_shape(w, screen.cursor_shape)?;
            self.screen_last.cursor_shape = screen.cursor_shape;
        }

        w.write_all(ctlseqs::SYNC_RESET.as_bytes())?;
        w.flush()?;
        Ok(())
    }

    /// Emits the per-cell SGR delta between the last-emitted style and `next`.
    ///
    /// Walks fg/bg/ul colors (with the standard colon vs legacy semicolon forms
    /// and the conpty underline workaround), the underline style, and the seven
    /// attributes. Bold and dim share a reset (`SGR 22`), so flipping one off
    /// re-emits the other if it is still set.
    fn emit_style_diff<W: Write>(
        &self,
        w: &mut W,
        last: &Style,
        next: &Style,
    ) -> Result<(), Error> {
        if !last.fg.eql(&next.fg) {
            match next.fg {
                Color::Default => w.write_all(ctlseqs::FG_RESET.as_bytes())?,
                Color::Index(idx) => match idx {
                    0..=7 => ctlseqs::fg_base(w, idx)?,
                    8..=15 => ctlseqs::fg_bright(w, idx - 8)?,
                    _ => match self.sgr {
                        Sgr::Standard => ctlseqs::fg_indexed(w, idx)?,
                        Sgr::Legacy => ctlseqs::fg_indexed_legacy(w, idx)?,
                    },
                },
                Color::Rgb([r, g, b]) => match self.sgr {
                    Sgr::Standard => ctlseqs::fg_rgb(w, r, g, b)?,
                    Sgr::Legacy => ctlseqs::fg_rgb_legacy(w, r, g, b)?,
                },
            }
        }
        if !last.bg.eql(&next.bg) {
            match next.bg {
                Color::Default => w.write_all(ctlseqs::BG_RESET.as_bytes())?,
                Color::Index(idx) => match idx {
                    0..=7 => ctlseqs::bg_base(w, idx)?,
                    8..=15 => ctlseqs::bg_bright(w, idx - 8)?,
                    _ => match self.sgr {
                        Sgr::Standard => ctlseqs::bg_indexed(w, idx)?,
                        Sgr::Legacy => ctlseqs::bg_indexed_legacy(w, idx)?,
                    },
                },
                Color::Rgb([r, g, b]) => match self.sgr {
                    Sgr::Standard => ctlseqs::bg_rgb(w, r, g, b)?,
                    Sgr::Legacy => ctlseqs::bg_rgb_legacy(w, r, g, b)?,
                },
            }
        }
        if !last.ul.eql(&next.ul) {
            match next.ul {
                Color::Default => w.write_all(ctlseqs::UL_RESET.as_bytes())?,
                Color::Index(idx) => match self.sgr {
                    Sgr::Standard => ctlseqs::ul_indexed(w, idx)?,
                    Sgr::Legacy => ctlseqs::ul_indexed_legacy(w, idx)?,
                },
                Color::Rgb([r, g, b]) => {
                    if self.enable_workarounds {
                        ctlseqs::ul_rgb_conpty(w, r, g, b)?;
                    } else {
                        match self.sgr {
                            Sgr::Standard => ctlseqs::ul_rgb(w, r, g, b)?,
                            Sgr::Legacy => ctlseqs::ul_rgb_legacy(w, r, g, b)?,
                        }
                    }
                }
            }
        }
        if last.ul_style != next.ul_style {
            let seq = match next.ul_style {
                Underline::Off => ctlseqs::UL_OFF,
                Underline::Single => ctlseqs::UL_SINGLE,
                Underline::Double => ctlseqs::UL_DOUBLE,
                Underline::Curly => ctlseqs::UL_CURLY,
                Underline::Dotted => ctlseqs::UL_DOTTED,
                Underline::Dashed => ctlseqs::UL_DASHED,
            };
            w.write_all(seq.as_bytes())?;
        }
        if last.bold != next.bold {
            let seq = if next.bold {
                ctlseqs::BOLD_SET
            } else {
                ctlseqs::BOLD_DIM_RESET
            };
            w.write_all(seq.as_bytes())?;
            // SGR 22 clears both bold and dim, so re-emit dim if still set.
            if next.dim {
                w.write_all(ctlseqs::DIM_SET.as_bytes())?;
            }
        }
        if last.dim != next.dim {
            let seq = if next.dim {
                ctlseqs::DIM_SET
            } else {
                ctlseqs::BOLD_DIM_RESET
            };
            w.write_all(seq.as_bytes())?;
            if next.bold {
                w.write_all(ctlseqs::BOLD_SET.as_bytes())?;
            }
        }
        if last.italic != next.italic {
            let seq = if next.italic {
                ctlseqs::ITALIC_SET
            } else {
                ctlseqs::ITALIC_RESET
            };
            w.write_all(seq.as_bytes())?;
        }
        if last.blink != next.blink {
            let seq = if next.blink {
                ctlseqs::BLINK_SET
            } else {
                ctlseqs::BLINK_RESET
            };
            w.write_all(seq.as_bytes())?;
        }
        if last.reverse != next.reverse {
            let seq = if next.reverse {
                ctlseqs::REVERSE_SET
            } else {
                ctlseqs::REVERSE_RESET
            };
            w.write_all(seq.as_bytes())?;
        }
        if last.invisible != next.invisible {
            let seq = if next.invisible {
                ctlseqs::INVISIBLE_SET
            } else {
                ctlseqs::INVISIBLE_RESET
            };
            w.write_all(seq.as_bytes())?;
        }
        if last.strikethrough != next.strikethrough {
            let seq = if next.strikethrough {
                ctlseqs::STRIKETHROUGH_SET
            } else {
                ctlseqs::STRIKETHROUGH_RESET
            };
            w.write_all(seq.as_bytes())?;
        }
        Ok(())
    }

    /// Prints the screen contents to the writer without storing render state,
    /// leaving the cursor on the line after the last printed line. Useful for
    /// dumping styled output to stdout. Errors if in the alt screen. The cursor
    /// is hidden and mouse shapes are not emitted.
    pub fn pretty_print<W: Write>(&mut self, w: &mut W) -> Result<(), Error> {
        if self.state.alt_screen {
            return Err(Error::NotInPrimaryScreen);
        }
        w.write_all(ctlseqs::HIDE_CURSOR.as_bytes())?;
        w.write_all(ctlseqs::SYNC_SET.as_bytes())?;
        w.write_all(ctlseqs::SGR_RESET.as_bytes())?;
        let result = self.pretty_print_body(w);
        // Best-effort tail (upstream `defer`s, reverse order: SGR then sync).
        let _ = w.write_all(ctlseqs::SGR_RESET.as_bytes());
        let _ = w.write_all(ctlseqs::SYNC_RESET.as_bytes());
        result
    }

    fn pretty_print_body<W: Write>(&mut self, w: &mut W) -> Result<(), Error> {
        let screen = self.screen.borrow();
        let buf_len = screen.buf.len();
        let mut reposition = false;
        let mut row: u16 = 0;
        let mut col: u16 = 0;
        let mut last_style = Style::default();
        let mut link = Hyperlink::default();
        let mut cursor_pos = CursorPos::default();

        let mut i = 0usize;
        while i < buf_len {
            let cell = screen.buf[i].clone();
            let cw: u16 = if cell.char.width != 0 {
                u16::from(cell.char.width)
            } else {
                gwidth::gwidth(cell.char.grapheme(), self.caps.unicode).max(1)
            };

            if col >= screen.width {
                row += 1;
                col = 0;
                if !cell.wrapped {
                    reposition = true;
                }
            }
            if cell.default {
                reposition = true;
                advance_cell(&mut self.screen_last, &mut i, &mut col, cw);
                continue;
            }

            if reposition {
                reposition = false;
                link = Hyperlink::default();
                if cursor_pos.row == row {
                    let n = col - cursor_pos.col;
                    if n > 0 {
                        ctlseqs::cuf(w, n)?;
                    }
                } else {
                    let n = row - cursor_pos.row;
                    for _ in 0..n {
                        w.write_all(b"\n")?;
                    }
                    w.write_all(b"\r")?;
                    if col > 0 {
                        ctlseqs::cuf(w, col)?;
                    }
                }
            }

            if let Some(img) = &cell.image {
                emit_image_placement(w, img)?;
            }

            self.emit_style_diff(w, &last_style, &cell.style)?;

            if link.uri != cell.link.uri {
                let params = if cell.link.uri.is_empty() {
                    ""
                } else {
                    cell.link.params.as_str()
                };
                ctlseqs::osc8(w, params, &cell.link.uri)?;
            }

            w.write_all(cell.char.grapheme().as_bytes())?;
            cursor_pos.col = col + cw;
            cursor_pos.row = row;

            last_style = cell.style;
            link = cell.link.clone();
            advance_cell(&mut self.screen_last, &mut i, &mut col, cw);
        }
        w.write_all(b"\r\n")?;
        w.flush()?;
        Ok(())
    }

    // --- Capability detection ----------------------------------------------

    /// Sends the capability-query batch and blocks until the loop wakes the DA1
    /// handshake (via [`Vaxis::notify_queries_done`]) or `timeout` elapses, then
    /// enables detected features. For a custom main loop, use
    /// [`Vaxis::query_terminal_send`] and [`Vaxis::enable_detected_features`].
    pub fn query_terminal<W: Write>(&mut self, w: &mut W, timeout: Duration) -> Result<(), Error> {
        self.query_terminal_send(w)?;
        {
            let received = self.da1_received.lock().expect("da1 mutex poisoned");
            let _unused = self
                .da1_condvar
                .wait_timeout_while(received, timeout, |r| !*r)
                .expect("da1 mutex poisoned");
        }
        self.queries_done.store(true, Ordering::Relaxed);
        self.enable_detected_features(w)
    }

    /// Writes the capability-query batch in upstream order and flushes.
    ///
    /// The two cursor-position requests bracket the explicit-width and
    /// scaled-text probes: the terminal's reply reports the column the cursor
    /// landed on, which tells us whether those OSC 66 forms moved the cursor.
    pub fn query_terminal_send<W: Write>(&mut self, w: &mut W) -> Result<(), Error> {
        // Exclusive access (&mut self) means no reader thread can observe this
        // write, so the non-atomic set is sound and uses the &mut.
        *self.queries_done.get_mut() = false;
        for seq in [
            ctlseqs::DECRQM_SGR_PIXELS,
            ctlseqs::DECRQM_UNICODE,
            ctlseqs::DECRQM_COLOR_SCHEME,
            ctlseqs::IN_BAND_RESIZE_SET,
            ctlseqs::HOME,
            ctlseqs::EXPLICIT_WIDTH_QUERY,
            ctlseqs::CURSOR_POSITION_REQUEST,
            ctlseqs::HOME,
            ctlseqs::SCALED_TEXT_QUERY,
            ctlseqs::MULTI_CURSOR_QUERY,
            ctlseqs::CURSOR_POSITION_REQUEST,
            ctlseqs::XTVERSION,
            ctlseqs::CSI_U_QUERY,
            ctlseqs::KITTY_GRAPHICS_QUERY,
            ctlseqs::PRIMARY_DEVICE_ATTRS,
        ] {
            w.write_all(seq.as_bytes())?;
        }
        w.flush()?;
        Ok(())
    }

    /// Applies environment overrides, then enables the features that capability
    /// detection turned on (kitty keyboard, unicode mode 2027).
    ///
    /// NOTE: Upstream reads a captured environment map; we read [`std::env`]
    /// directly. We do not reproduce upstream's Windows path (unconditional
    /// legacy SGR), since the port is Linux-first.
    pub fn enable_detected_features<W: Write>(&mut self, w: &mut W) -> Result<(), Error> {
        if env::var_os("TERMUX_VERSION").is_some() {
            self.sgr = Sgr::Legacy;
        }
        if env::var_os("VHS_RECORD").is_some() {
            self.caps.unicode = gwidth::Method::Wcwidth;
            self.caps.kitty_keyboard = false;
            self.sgr = Sgr::Legacy;
        }
        if env::var("TERM_PROGRAM").is_ok_and(|p| p == "vscode") {
            self.sgr = Sgr::Legacy;
        }
        if env::var_os("VAXIS_FORCE_LEGACY_SGR").is_some() {
            self.sgr = Sgr::Legacy;
        }
        if env::var_os("VAXIS_FORCE_WCWIDTH").is_some() {
            self.caps.unicode = gwidth::Method::Wcwidth;
        }
        if env::var_os("VAXIS_FORCE_UNICODE").is_some() {
            self.caps.unicode = gwidth::Method::Unicode;
        }

        if self.caps.kitty_keyboard {
            self.enable_kitty_keyboard(w, self.opts.kitty_keyboard_flags)?;
        }
        // Only enable mode 2027 if we do not have explicit width.
        if self.caps.unicode == gwidth::Method::Unicode && !self.caps.explicit_width {
            w.write_all(ctlseqs::UNICODE_SET.as_bytes())?;
        }
        w.flush()?;
        Ok(())
    }

    /// Wakes the DA1 handshake. The loop calls this when the DA1 response is
    /// parsed, unblocking [`Vaxis::query_terminal`].
    ///
    /// NOTE: Takes `&self` so the loop can call it through a shared reference.
    /// Cross-thread waking in phase 7 shares the handshake separately, since
    /// `Vaxis` is `!Sync` (the screen `RefCell`).
    pub fn notify_queries_done(&self) {
        let mut received = self.da1_received.lock().expect("da1 mutex poisoned");
        *received = true;
        self.da1_condvar.notify_all();
    }

    // --- Per-feature emit methods ------------------------------------------

    /// Enters the alternate screen.
    pub fn enter_alt_screen<W: Write>(&mut self, w: &mut W) -> Result<(), Error> {
        w.write_all(ctlseqs::SMCUP.as_bytes())?;
        w.flush()?;
        self.state.alt_screen = true;
        Ok(())
    }

    /// Exits the alternate screen.
    pub fn exit_alt_screen<W: Write>(&mut self, w: &mut W) -> Result<(), Error> {
        w.write_all(ctlseqs::RMCUP.as_bytes())?;
        w.flush()?;
        self.state.alt_screen = false;
        Ok(())
    }

    /// Pushes the kitty keyboard-protocol flags onto the terminal's stack.
    pub fn enable_kitty_keyboard<W: Write>(
        &mut self,
        w: &mut W,
        flags: KittyFlags,
    ) -> Result<(), Error> {
        ctlseqs::csi_u_push(w, flags.bits())?;
        w.flush()?;
        self.state.kitty_keyboard = true;
        Ok(())
    }

    /// Sends a desktop notification, with an optional title.
    pub fn notify<W: Write>(
        &self,
        w: &mut W,
        title: Option<&str>,
        body: &str,
    ) -> Result<(), Error> {
        match title {
            Some(t) => ctlseqs::osc777_notify(w, t, body)?,
            None => ctlseqs::osc9_notify(w, body)?,
        }
        w.flush()?;
        Ok(())
    }

    /// Sets the window title.
    pub fn set_title<W: Write>(&self, w: &mut W, title: &str) -> Result<(), Error> {
        ctlseqs::osc2_set_title(w, title)?;
        w.flush()?;
        Ok(())
    }

    /// Turns bracketed paste on or off. While on, paste start/end events bracket
    /// the pasted keystrokes.
    pub fn set_bracketed_paste<W: Write>(&mut self, w: &mut W, enable: bool) -> Result<(), Error> {
        let seq = if enable {
            ctlseqs::BP_SET
        } else {
            ctlseqs::BP_RESET
        };
        w.write_all(seq.as_bytes())?;
        w.flush()?;
        self.state.bracketed_paste = enable;
        Ok(())
    }

    /// Sets the mouse shape on the screen. Emitted on the next render.
    pub fn set_mouse_shape(&self, shape: MouseShape) {
        self.screen.borrow_mut().mouse_shape = shape;
    }

    /// Turns mouse reporting on or off. When on and the terminal reports pixel
    /// coordinates (sgr_pixels), the pixel mode is used.
    pub fn set_mouse_mode<W: Write>(&mut self, w: &mut W, enable: bool) -> Result<(), Error> {
        if enable {
            self.state.mouse = true;
            if self.caps.sgr_pixels {
                self.state.pixel_mouse = true;
                w.write_all(ctlseqs::MOUSE_SET_PIXELS.as_bytes())?;
            } else {
                w.write_all(ctlseqs::MOUSE_SET.as_bytes())?;
            }
        } else {
            w.write_all(ctlseqs::MOUSE_RESET.as_bytes())?;
        }
        w.flush()?;
        Ok(())
    }

    /// Translates pixel mouse coordinates into a cell position plus sub-cell
    /// offset. A no-op unless pixel mouse mode is active.
    pub fn translate_mouse(&self, mouse: Mouse) -> Mouse {
        let screen = self.screen.borrow();
        if screen.width == 0 || screen.height == 0 {
            return mouse;
        }
        let mut result = mouse;
        if self.state.pixel_mouse {
            debug_assert_eq!(mouse.xoffset, 0);
            debug_assert_eq!(mouse.yoffset, 0);
            let xpos = mouse.col;
            let ypos = mouse.row;
            let xextra = screen.width_pix % screen.width;
            let yextra = screen.height_pix % screen.height;
            let xcell = i16::try_from((screen.width_pix - xextra) / screen.width).unwrap_or(0);
            let ycell = i16::try_from((screen.height_pix - yextra) / screen.height).unwrap_or(0);
            if xcell == 0 || ycell == 0 {
                return mouse;
            }
            result.col = xpos.div_euclid(xcell);
            result.row = ypos.div_euclid(ycell);
            result.xoffset = u16::try_from(xpos.rem_euclid(xcell)).unwrap_or(0);
            result.yoffset = u16::try_from(ypos.rem_euclid(ycell)).unwrap_or(0);
        }
        result
    }

    /// Copies `text` to the system clipboard via OSC 52.
    pub fn copy_to_system_clipboard<W: Write>(&self, w: &mut W, text: &str) -> Result<(), Error> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(text);
        ctlseqs::osc52_clipboard_copy(w, &b64)?;
        w.flush()?;
        Ok(())
    }

    /// Requests the system clipboard contents via OSC 52. Errors when clipboard
    /// access is not enabled in [`Options`].
    pub fn request_system_clipboard<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        if !self.opts.system_clipboard_enabled {
            return Err(Error::ClipboardDisabled);
        }
        w.write_all(ctlseqs::OSC52_CLIPBOARD_REQUEST.as_bytes())?;
        w.flush()?;
        Ok(())
    }

    /// Sets the terminal's default foreground color.
    pub fn set_terminal_foreground_color<W: Write>(
        &mut self,
        w: &mut W,
        rgb: [u8; 3],
    ) -> Result<(), Error> {
        ctlseqs::osc10_set(w, rgb[0], rgb[1], rgb[2])?;
        w.flush()?;
        self.state.changed_default_fg = true;
        Ok(())
    }

    /// Sets the terminal's default background color.
    pub fn set_terminal_background_color<W: Write>(
        &mut self,
        w: &mut W,
        rgb: [u8; 3],
    ) -> Result<(), Error> {
        ctlseqs::osc11_set(w, rgb[0], rgb[1], rgb[2])?;
        w.flush()?;
        self.state.changed_default_bg = true;
        Ok(())
    }

    /// Sets the terminal cursor color.
    pub fn set_terminal_cursor_color<W: Write>(
        &mut self,
        w: &mut W,
        rgb: [u8; 3],
    ) -> Result<(), Error> {
        ctlseqs::osc12_set(w, rgb[0], rgb[1], rgb[2])?;
        w.flush()?;
        self.state.changed_cursor_color = true;
        Ok(())
    }

    /// Sets the secondary-cursor color. Only emits when multi-cursor support is
    /// detected.
    pub fn set_terminal_cursor_secondary_color<W: Write>(
        &mut self,
        w: &mut W,
        rgb: [u8; 3],
    ) -> Result<(), Error> {
        if self.caps.multi_cursor {
            ctlseqs::secondary_cursors_rgb(w, rgb[0], rgb[1], rgb[2])?;
            w.flush()?;
            self.state.changed_cursor_color = true;
        }
        Ok(())
    }

    /// Clears all secondary cursors.
    ///
    /// NOTE: Upstream juggles slice ownership to free the right buffers. With
    /// owned `Vec`s this is just clearing the front-screen set and the stale
    /// previous set.
    pub fn reset_all_terminal_secondary_cursors(&mut self) {
        self.state.prev_cursor_secondary.clear();
        self.screen.borrow_mut().cursor_secondary.clear();
    }

    /// Adds a secondary cursor at the 0-indexed `(y, x)`.
    pub fn add_terminal_secondary_cursor(&mut self, y: u16, x: u16) {
        self.state.prev_cursor_secondary.clear();
        self.screen
            .borrow_mut()
            .cursor_secondary
            .push(Cursor { row: y, col: x });
    }

    /// Requests a color report for `kind`. Not all terminals respond.
    pub fn query_color<W: Write>(&self, w: &mut W, kind: Kind) -> Result<(), Error> {
        match kind {
            Kind::Fg => w.write_all(ctlseqs::OSC10_QUERY.as_bytes())?,
            Kind::Bg => w.write_all(ctlseqs::OSC11_QUERY.as_bytes())?,
            Kind::Cursor => w.write_all(ctlseqs::OSC12_QUERY.as_bytes())?,
            Kind::Index(idx) => ctlseqs::osc4_query(w, idx)?,
        }
        w.flush()?;
        Ok(())
    }

    /// Subscribes to color-scheme update events. The terminal reports the
    /// current scheme immediately. Support is `caps.color_scheme_updates`.
    pub fn subscribe_to_color_scheme_updates<W: Write>(&mut self, w: &mut W) -> Result<(), Error> {
        w.write_all(ctlseqs::COLOR_SCHEME_REQUEST.as_bytes())?;
        w.write_all(ctlseqs::COLOR_SCHEME_SET.as_bytes())?;
        w.flush()?;
        self.state.color_scheme_updates = true;
        Ok(())
    }

    /// Sends a device status report request.
    pub fn device_status_report<W: Write>(&self, w: &mut W) -> Result<(), Error> {
        w.write_all(ctlseqs::DEVICE_STATUS_REPORT.as_bytes())?;
        w.flush()?;
        Ok(())
    }

    /// Reports the terminal's working directory via OSC 7. `path` must be
    /// absolute.
    pub fn set_terminal_working_directory<W: Write>(
        &self,
        w: &mut W,
        path: &str,
    ) -> Result<(), Error> {
        if path.is_empty() || !path.starts_with('/') {
            return Err(Error::NotAbsolutePath(path.to_string()));
        }
        let hostname = env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
        let host = percent_encode(&hostname, |b| is_uri_unreserved(b) || is_uri_sub_delim(b));
        let encoded_path = percent_encode(path, |b| {
            is_uri_unreserved(b) || is_uri_sub_delim(b) || matches!(b, b'/' | b':' | b'@')
        });
        ctlseqs::osc7(w, format_args!("file://{host}{encoded_path}"))?;
        w.flush()?;
        Ok(())
    }

    // --- Image transmission (kitty graphics) -------------------------------

    /// Transmits an image whose bytes the terminal reads from the local
    /// filesystem (or temp/shared-memory medium), base64-encoding `payload`.
    ///
    /// `payload` is the raw bytes for the chosen medium (typically a path).
    /// Errors with [`Error::PathTooLong`] when the encoded payload would not
    /// fit a single 4096-byte chunk, and with [`Error::NoGraphicsCapability`]
    /// when the terminal lacks kitty graphics. The image id is allocated even
    /// on the `PathTooLong` path, matching upstream.
    pub fn transmit_local_image_path<W: Write>(
        &mut self,
        w: &mut W,
        payload: &[u8],
        width: u16,
        height: u16,
        medium: TransmitMedium,
        format: TransmitFormat,
    ) -> Result<Image, Error> {
        if !self.caps.kitty_graphics {
            return Err(Error::NoGraphicsCapability);
        }
        // The id is consumed even when we bail with PathTooLong below: upstream
        // increments it in a `defer` placed after the capability check.
        let id = self.next_img_id;
        self.next_img_id += 1;

        let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
        if encoded.len() >= 4096 {
            return Err(Error::PathTooLong);
        }

        let medium_char = match medium {
            TransmitMedium::File => 'f',
            TransmitMedium::TempFile => 't',
            TransmitMedium::SharedMem => 's',
        };
        match format {
            TransmitFormat::Rgb => {
                write!(w, "\x1b_Gf=24,s={width},v={height},i={id},t={medium_char};")?;
            }
            TransmitFormat::Rgba => {
                write!(w, "\x1b_Gf=32,s={width},v={height},i={id},t={medium_char};")?;
            }
            TransmitFormat::Png => {
                write!(w, "\x1b_Gf=100,i={id},t={medium_char};")?;
            }
        }
        w.write_all(encoded.as_bytes())?;
        w.write_all(b"\x1b\\")?;
        w.flush()?;
        Ok(Image::new(id, width, height))
    }

    /// Transmits an already-base64-encoded image, chunking the payload at 4096
    /// bytes.
    ///
    /// A payload under 4096 bytes is sent as one APC. Larger payloads are split
    /// into 4096-byte chunks with the kitty continuation flag set (`m=1`) on
    /// every chunk but the last (`m=0`). Errors with
    /// [`Error::NoGraphicsCapability`] when the terminal lacks kitty graphics.
    pub fn transmit_pre_encoded_image<W: Write>(
        &mut self,
        w: &mut W,
        bytes: &[u8],
        width: u16,
        height: u16,
        format: TransmitFormat,
    ) -> Result<Image, Error> {
        if !self.caps.kitty_graphics {
            return Err(Error::NoGraphicsCapability);
        }
        let id = self.next_img_id;
        self.next_img_id += 1;

        let fmt: u8 = match format {
            TransmitFormat::Rgb => 24,
            TransmitFormat::Rgba => 32,
            TransmitFormat::Png => 100,
        };

        if bytes.len() < 4096 {
            write!(w, "\x1b_Gf={fmt},s={width},v={height},i={id};")?;
            w.write_all(bytes)?;
            w.write_all(b"\x1b\\")?;
        } else {
            write!(w, "\x1b_Gf={fmt},s={width},v={height},i={id},m=1;")?;
            w.write_all(&bytes[0..4096])?;
            w.write_all(b"\x1b\\")?;
            let mut n = 4096;
            while n < bytes.len() {
                let end = (n + 4096).min(bytes.len());
                let m = if end == bytes.len() { 0 } else { 1 };
                write!(w, "\x1b_Gm={m};")?;
                w.write_all(&bytes[n..end])?;
                w.write_all(b"\x1b\\")?;
                n += 4096;
            }
        }
        w.flush()?;
        Ok(Image::new(id, width, height))
    }

    /// Encodes a decoded image in `format` and transmits it.
    ///
    /// PNG goes through [`PngEncoder`], RGB through `to_rgb8`, RGBA through
    /// `to_rgba8`. The encoded bytes are base64-ed and handed to
    /// [`Vaxis::transmit_pre_encoded_image`]. Errors with
    /// [`Error::NoGraphicsCapability`] when the terminal lacks kitty graphics
    /// and [`Error::Image`] on an encode failure.
    pub fn transmit_image<W: Write>(
        &mut self,
        w: &mut W,
        img: &DynamicImage,
        format: TransmitFormat,
    ) -> Result<Image, Error> {
        if !self.caps.kitty_graphics {
            return Err(Error::NoGraphicsCapability);
        }
        let width = u16::try_from(img.width()).unwrap_or(u16::MAX);
        let height = u16::try_from(img.height()).unwrap_or(u16::MAX);

        let raw = match format {
            TransmitFormat::Png => {
                let mut png = Vec::new();
                img.write_with_encoder(PngEncoder::new(&mut png))?;
                png
            }
            TransmitFormat::Rgb => img.to_rgb8().into_raw(),
            TransmitFormat::Rgba => img.to_rgba8().into_raw(),
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        self.transmit_pre_encoded_image(w, encoded.as_bytes(), width, height, format)
    }

    /// Decodes an image from a path or from memory and transmits it as PNG.
    ///
    /// Errors with [`Error::NoGraphicsCapability`] when the terminal lacks
    /// kitty graphics and [`Error::Image`] on a decode failure.
    pub fn load_image<W: Write>(&mut self, w: &mut W, src: Source) -> Result<Image, Error> {
        if !self.caps.kitty_graphics {
            return Err(Error::NoGraphicsCapability);
        }
        let img = match src {
            Source::Path(path) => image::open(path)?,
            Source::Mem(bytes) => image::load_from_memory(&bytes)?,
        };
        self.transmit_image(w, &img, TransmitFormat::Png)
    }

    /// Deletes an image from the terminal's graphics memory by id.
    ///
    /// Best-effort: write and flush errors are swallowed, mirroring upstream
    /// (which logs and returns). Takes `&self` since it touches no runtime
    /// state.
    pub fn free_image<W: Write>(&self, w: &mut W, id: u32) {
        if write!(w, "\x1b_Ga=d,d=I,i={id};\x1b\\").is_err() {
            return;
        }
        let _ = w.flush();
    }
}

/// Begins a render: opens synchronized output, hides the cursor, sends the
/// cursor home (absolute on the alt screen, relative on the primary), resets
/// SGR, and clears images if kitty graphics are supported. Idempotent within a
/// render via the `started` flag.
#[allow(clippy::too_many_arguments)]
fn start_render<W: Write>(
    w: &mut W,
    alt_screen: bool,
    cursor_row: u16,
    kitty_graphics: bool,
    cursor_pos: &mut CursorPos,
    reposition: &mut bool,
    started: &mut bool,
    sync_active: &mut bool,
) -> io::Result<()> {
    if *started {
        return Ok(());
    }
    *started = true;
    *sync_active = true;
    w.write_all(ctlseqs::SYNC_SET.as_bytes())?;
    w.write_all(ctlseqs::HIDE_CURSOR.as_bytes())?;
    if alt_screen {
        w.write_all(ctlseqs::HOME.as_bytes())?;
    } else {
        w.write_all(b"\r")?;
        for _ in 0..cursor_row {
            w.write_all(ctlseqs::RI.as_bytes())?;
        }
    }
    w.write_all(ctlseqs::SGR_RESET.as_bytes())?;
    *cursor_pos = CursorPos::default();
    *reposition = true;
    if kitty_graphics {
        w.write_all(ctlseqs::KITTY_GRAPHICS_CLEAR.as_bytes())?;
    }
    Ok(())
}

/// Advances the render cursor past a `cw`-wide cell: marks the back-buffer cells
/// the wide glyph covers as `skipped`, then steps `col` and `i` by the width.
fn advance_cell(screen_last: &mut InternalScreen, i: &mut usize, col: &mut u16, cw: u16) {
    let w = usize::from(cw);
    let mut j = *i + 1;
    while j < *i + w {
        if j >= screen_last.buf.len() {
            break;
        }
        screen_last.buf[j].skipped = true;
        j += 1;
    }
    *col += cw;
    *i += w;
}

/// Emits a kitty image placement command for a cell carrying an [`image`]
/// placement: the `a=p` preamble, then the optional pixel offset (`X`/`Y`), clip
/// region (`x`/`y`/`w`/`h`), size (`r`/`c`), z-index (`z`), and the closing.
///
/// NOTE: The option fragments are written inline rather than through `ctlseqs`,
/// mirroring upstream, which does not lift them into its constant table.
///
/// [`image`]: crate::image
fn emit_image_placement<W: Write>(w: &mut W, img: &crate::image::Placement) -> io::Result<()> {
    ctlseqs::kitty_graphics_preamble(w, img.img_id)?;
    if let Some(offset) = img.options.pixel_offset {
        write!(w, ",X={},Y={}", offset.x, offset.y)?;
    }
    if let Some(clip) = img.options.clip_region {
        if let Some(x) = clip.x {
            write!(w, ",x={x}")?;
        }
        if let Some(y) = clip.y {
            write!(w, ",y={y}")?;
        }
        if let Some(width) = clip.width {
            write!(w, ",w={width}")?;
        }
        if let Some(height) = clip.height {
            write!(w, ",h={height}")?;
        }
    }
    if let Some(size) = img.options.size {
        if let Some(rows) = size.rows {
            write!(w, ",r={rows}")?;
        }
        if let Some(cols) = size.cols {
            write!(w, ",c={cols}")?;
        }
    }
    if let Some(z) = img.options.z_index {
        write!(w, ",z={z}")?;
    }
    w.write_all(ctlseqs::KITTY_GRAPHICS_CLOSING.as_bytes())
}

/// Slice identity, reproducing `std.meta.eql` on Zig slices: equal length and
/// the same backing pointer (not element-wise equality). See the
/// `cursor_secondary_changed` note in the renderer.
fn same_slice(a: &[Cursor], b: &[Cursor]) -> bool {
    a.len() == b.len() && std::ptr::eq(a.as_ptr(), b.as_ptr())
}

/// True for RFC 3986 unreserved bytes: ALPHA / DIGIT / `-` / `.` / `_` / `~`.
fn is_uri_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// True for RFC 3986 sub-delimiter bytes.
fn is_uri_sub_delim(b: u8) -> bool {
    matches!(
        b,
        b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'='
    )
}

/// Percent-encodes `s`, leaving bytes for which `allowed` returns true intact.
fn percent_encode(s: &str, allowed: impl Fn(u8) -> bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if allowed(b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(b >> 4)]));
            out.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Cell, Character};

    fn winsize() -> Winsize {
        Winsize {
            rows: 40,
            cols: 80,
            x_pixel: 0,
            y_pixel: 0,
        }
    }

    /// The ported upstream test: a fresh render with no changes emits nothing.
    #[test]
    fn render_no_output_when_no_changes() {
        let mut vx = Vaxis::new(Options::default());
        let mut out: Vec<u8> = Vec::new();
        vx.render(&mut out).expect("render");
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn render_emits_styled_cell() {
        let mut vx = Vaxis::new(Options::default());
        let mut out: Vec<u8> = Vec::new();
        vx.resize(&mut out, winsize()).expect("resize");
        out.clear();

        {
            let win = vx.window();
            win.write_cell(
                0,
                0,
                Cell {
                    char: Character::new("A", 1),
                    style: Style {
                        fg: Color::Index(1),
                        ..Style::default()
                    },
                    ..Cell::default()
                },
            );
        }
        vx.render(&mut out).expect("render");

        let mut expected: Vec<u8> = Vec::new();
        expected.extend_from_slice(ctlseqs::SYNC_SET.as_bytes());
        expected.extend_from_slice(ctlseqs::HIDE_CURSOR.as_bytes());
        expected.extend_from_slice(b"\r");
        expected.extend_from_slice(ctlseqs::SGR_RESET.as_bytes());
        expected.extend_from_slice(b"\x1b[31m");
        expected.extend_from_slice(b"A");
        expected.extend_from_slice(ctlseqs::SYNC_RESET.as_bytes());
        assert_eq!(out, expected);
    }

    #[test]
    fn render_skips_unchanged_second_pass() {
        let mut vx = Vaxis::new(Options::default());
        let mut out: Vec<u8> = Vec::new();
        vx.resize(&mut out, winsize()).expect("resize");

        {
            let win = vx.window();
            win.write_cell(
                0,
                0,
                Cell {
                    char: Character::new("A", 1),
                    ..Cell::default()
                },
            );
        }
        out.clear();
        vx.render(&mut out).expect("first render");
        assert!(!out.is_empty(), "first render should emit the cell");

        // Nothing changed between renders, so the diff emits zero bytes.
        out.clear();
        vx.render(&mut out).expect("second render");
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn reset_state_emits_teardown_in_order() {
        let mut vx = Vaxis::new(Options::default());
        let mut out: Vec<u8> = Vec::new();
        vx.enter_alt_screen(&mut out).expect("alt");
        vx.enable_kitty_keyboard(&mut out, KittyFlags::default())
            .expect("kitty");
        vx.set_mouse_mode(&mut out, true).expect("mouse");
        out.clear();

        vx.reset_state(&mut out).expect("reset");

        let mut expected: Vec<u8> = Vec::new();
        expected.extend_from_slice(ctlseqs::SHOW_CURSOR.as_bytes());
        expected.extend_from_slice(ctlseqs::SGR_RESET.as_bytes());
        expected.extend_from_slice(ctlseqs::CSI_U_POP.as_bytes());
        expected.extend_from_slice(ctlseqs::MOUSE_RESET.as_bytes());
        expected.extend_from_slice(ctlseqs::HOME.as_bytes());
        expected.extend_from_slice(ctlseqs::ERASE_BELOW_CURSOR.as_bytes());
        expected.extend_from_slice(ctlseqs::RMCUP.as_bytes());
        assert_eq!(out, expected);

        // The teardown cleared the bits it acted on.
        assert!(!vx.state.kitty_keyboard);
        assert!(!vx.state.alt_screen);
    }

    #[test]
    fn query_terminal_send_emits_batch() {
        let mut vx = Vaxis::new(Options::default());
        let mut out: Vec<u8> = Vec::new();
        vx.query_terminal_send(&mut out).expect("query send");

        let mut expected: Vec<u8> = Vec::new();
        for seq in [
            ctlseqs::DECRQM_SGR_PIXELS,
            ctlseqs::DECRQM_UNICODE,
            ctlseqs::DECRQM_COLOR_SCHEME,
            ctlseqs::IN_BAND_RESIZE_SET,
            ctlseqs::HOME,
            ctlseqs::EXPLICIT_WIDTH_QUERY,
            ctlseqs::CURSOR_POSITION_REQUEST,
            ctlseqs::HOME,
            ctlseqs::SCALED_TEXT_QUERY,
            ctlseqs::MULTI_CURSOR_QUERY,
            ctlseqs::CURSOR_POSITION_REQUEST,
            ctlseqs::XTVERSION,
            ctlseqs::CSI_U_QUERY,
            ctlseqs::KITTY_GRAPHICS_QUERY,
            ctlseqs::PRIMARY_DEVICE_ATTRS,
        ] {
            expected.extend_from_slice(seq.as_bytes());
        }
        assert_eq!(out, expected);
        // Sending a batch marks queries as outstanding.
        assert!(!vx.queries_done.load(Ordering::Relaxed));
    }

    #[test]
    fn alt_screen_and_mouse_mode_flip_state() {
        let mut vx = Vaxis::new(Options::default());
        let mut out: Vec<u8> = Vec::new();

        vx.enter_alt_screen(&mut out).expect("enter");
        assert_eq!(out, ctlseqs::SMCUP.as_bytes());
        assert!(vx.state.alt_screen);

        out.clear();
        vx.exit_alt_screen(&mut out).expect("exit");
        assert_eq!(out, ctlseqs::RMCUP.as_bytes());
        assert!(!vx.state.alt_screen);

        out.clear();
        vx.set_mouse_mode(&mut out, true).expect("mouse on");
        assert_eq!(out, ctlseqs::MOUSE_SET.as_bytes());
        assert!(vx.state.mouse);
    }

    #[test]
    fn query_terminal_returns_when_handshake_pre_armed() {
        let mut vx = Vaxis::new(Options::default());
        // Pre-arm the handshake so the wait returns immediately.
        vx.notify_queries_done();
        let mut out: Vec<u8> = Vec::new();
        vx.query_terminal(&mut out, Duration::from_secs(5))
            .expect("query");
        assert!(vx.queries_done.load(Ordering::Relaxed));
        // The batch was still sent.
        assert!(out.starts_with(ctlseqs::DECRQM_SGR_PIXELS.as_bytes()));
    }

    #[test]
    fn render_emits_image_placement() {
        use crate::image::{DrawOptions, Placement};

        let mut vx = Vaxis::new(Options::default());
        let mut out: Vec<u8> = Vec::new();
        vx.resize(&mut out, winsize()).expect("resize");
        out.clear();

        {
            let win = vx.window();
            win.write_cell(
                0,
                0,
                Cell {
                    char: Character::new(" ", 1),
                    image: Some(Placement {
                        img_id: 7,
                        options: DrawOptions {
                            z_index: Some(-1),
                            ..DrawOptions::default()
                        },
                    }),
                    ..Cell::default()
                },
            );
        }
        vx.render(&mut out).expect("render");

        // The placement preamble, the z-index fragment, and the closing all land
        // in the output stream.
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\x1b_Ga=p,i=7"), "preamble missing: {text:?}");
        assert!(text.contains(",z=-1"), "z-index missing: {text:?}");
        assert!(text.contains(",C=1\x1b\\"), "closing missing: {text:?}");
    }

    /// Enables kitty graphics and returns a fresh runtime ready to transmit.
    fn vx_with_graphics() -> Vaxis {
        let mut vx = Vaxis::new(Options::default());
        vx.caps.kitty_graphics = true;
        vx
    }

    /// A 2x2 RGBA image wrapped as a `DynamicImage`.
    fn tiny_rgba() -> DynamicImage {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        DynamicImage::ImageRgba8(img)
    }

    #[test]
    fn transmit_pre_encoded_short_payload_single_apc() {
        let mut vx = vx_with_graphics();
        let mut out: Vec<u8> = Vec::new();
        let payload = vec![b'A'; 100];

        let img = vx
            .transmit_pre_encoded_image(&mut out, &payload, 4, 2, TransmitFormat::Png)
            .expect("transmit");
        assert_eq!((img.id(), img.width(), img.height()), (1, 4, 2));

        let mut expected: Vec<u8> = Vec::new();
        expected.extend_from_slice(b"\x1b_Gf=100,s=4,v=2,i=1;");
        expected.extend_from_slice(&payload);
        expected.extend_from_slice(b"\x1b\\");
        assert_eq!(out, expected);
    }

    #[test]
    fn transmit_pre_encoded_large_payload_chunks() {
        let mut vx = vx_with_graphics();
        let mut out: Vec<u8> = Vec::new();
        // 9000 bytes spans three chunks: 4096 (m=1), 4096 (m=1), 808 (m=0).
        let payload = vec![b'A'; 9000];

        vx.transmit_pre_encoded_image(&mut out, &payload, 1, 1, TransmitFormat::Rgba)
            .expect("transmit");

        let mut expected: Vec<u8> = Vec::new();
        expected.extend_from_slice(b"\x1b_Gf=32,s=1,v=1,i=1,m=1;");
        expected.extend_from_slice(&payload[0..4096]);
        expected.extend_from_slice(b"\x1b\\");
        expected.extend_from_slice(b"\x1b_Gm=1;");
        expected.extend_from_slice(&payload[4096..8192]);
        expected.extend_from_slice(b"\x1b\\");
        expected.extend_from_slice(b"\x1b_Gm=0;");
        expected.extend_from_slice(&payload[8192..9000]);
        expected.extend_from_slice(b"\x1b\\");
        assert_eq!(out, expected);
    }

    #[test]
    fn transmit_image_rgba_and_png_emit_fresh_ids() {
        let mut vx = vx_with_graphics();
        let img = tiny_rgba();

        let mut out: Vec<u8> = Vec::new();
        let rgba = vx
            .transmit_image(&mut out, &img, TransmitFormat::Rgba)
            .expect("rgba");
        assert_eq!((rgba.id(), rgba.width(), rgba.height()), (1, 2, 2));
        assert!(out.starts_with(b"\x1b_Gf=32,s=2,v=2,i=1;"), "rgba header");
        assert!(out.ends_with(b"\x1b\\"));

        let mut out: Vec<u8> = Vec::new();
        let png = vx
            .transmit_image(&mut out, &img, TransmitFormat::Png)
            .expect("png");
        assert_eq!(png.id(), 2);
        assert!(out.starts_with(b"\x1b_Gf=100,s=2,v=2,i=2;"), "png header");
        assert!(!out.is_empty());
    }

    #[test]
    fn transmit_local_image_path_emits_header_and_payload() {
        let mut vx = vx_with_graphics();
        let mut out: Vec<u8> = Vec::new();
        let payload = vec![1u8, 2, 3, 4, 5];

        let img = vx
            .transmit_local_image_path(
                &mut out,
                &payload,
                4,
                2,
                TransmitMedium::File,
                TransmitFormat::Rgb,
            )
            .expect("transmit");
        assert_eq!((img.id(), img.width(), img.height()), (1, 4, 2));

        let encoded = base64::engine::general_purpose::STANDARD.encode(&payload);
        let mut expected: Vec<u8> = Vec::new();
        expected.extend_from_slice(b"\x1b_Gf=24,s=4,v=2,i=1,t=f;");
        expected.extend_from_slice(encoded.as_bytes());
        expected.extend_from_slice(b"\x1b\\");
        assert_eq!(out, expected);
    }

    #[test]
    fn transmit_local_image_path_too_long_consumes_id() {
        let mut vx = vx_with_graphics();
        let mut out: Vec<u8> = Vec::new();
        // 3072 raw bytes base64-encode to exactly 4096 chars, hitting the cap.
        let payload = vec![0u8; 3072];

        let err = vx
            .transmit_local_image_path(
                &mut out,
                &payload,
                1,
                1,
                TransmitMedium::File,
                TransmitFormat::Png,
            )
            .expect_err("should be too long");
        assert!(matches!(err, Error::PathTooLong));
        // Nothing was written, but the id was still consumed.
        assert!(out.is_empty());
        assert_eq!(vx.next_img_id, 2);
    }

    #[test]
    fn load_image_decodes_png_from_memory() {
        let mut vx = vx_with_graphics();

        // Encode a tiny PNG with the image crate, then decode + transmit it.
        let mut png = Vec::new();
        tiny_rgba()
            .write_with_encoder(PngEncoder::new(&mut png))
            .expect("encode png");

        let mut out: Vec<u8> = Vec::new();
        let img = vx
            .load_image(&mut out, Source::Mem(png))
            .expect("load image");
        assert_eq!((img.id(), img.width(), img.height()), (1, 2, 2));
        // load_image transmits as PNG.
        assert!(out.starts_with(b"\x1b_Gf=100,s=2,v=2,i=1;"), "png header");
        assert!(!out.is_empty());
    }

    #[test]
    fn free_image_emits_delete_by_id() {
        let vx = vx_with_graphics();
        let mut out: Vec<u8> = Vec::new();
        vx.free_image(&mut out, 7);
        assert_eq!(out, b"\x1b_Ga=d,d=I,i=7;\x1b\\");
    }

    #[test]
    fn transmits_gated_on_kitty_graphics() {
        // Default caps have kitty_graphics off: every transmit refuses and
        // emits nothing.
        let mut vx = Vaxis::new(Options::default());
        let img = tiny_rgba();

        let mut out: Vec<u8> = Vec::new();
        assert!(matches!(
            vx.transmit_pre_encoded_image(&mut out, b"x", 1, 1, TransmitFormat::Png),
            Err(Error::NoGraphicsCapability)
        ));
        assert!(out.is_empty());

        assert!(matches!(
            vx.transmit_image(&mut out, &img, TransmitFormat::Rgba),
            Err(Error::NoGraphicsCapability)
        ));
        assert!(out.is_empty());

        assert!(matches!(
            vx.transmit_local_image_path(
                &mut out,
                b"x",
                1,
                1,
                TransmitMedium::File,
                TransmitFormat::Png
            ),
            Err(Error::NoGraphicsCapability)
        ));
        assert!(out.is_empty());

        assert!(matches!(
            vx.load_image(&mut out, Source::Mem(vec![0u8])),
            Err(Error::NoGraphicsCapability)
        ));
        assert!(out.is_empty());

        // The id counter is untouched when every transmit refuses.
        assert_eq!(vx.next_img_id, 1);
    }
}
