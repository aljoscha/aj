//! Port of upstream `examples/image.zig`: transmits two images to the terminal
//! via the kitty graphics protocol and cycles between them each frame, centered
//! and scaled to `contain`, with `j`/`k` panning the clip region.
//!
//! The two PNGs live next to this file and are resolved relative to the crate
//! manifest so the example runs from any working directory. `zig.png` is
//! decoded and transmitted raw (RGBA); `vaxis.png` is handed to `load_image`,
//! which decodes from the path and transmits as PNG. Both transmission paths
//! are exercised, matching upstream.

use std::error::Error;
use std::time::Duration;

use vaxis::Winsize;
use vaxis::event::Event as VxEvent;
use vaxis::event_loop::{FromEvent, Loop};
use vaxis::image::{ClipRegion, DrawOptions, Image, Scale, Source, TransmitFormat};
use vaxis::key::{Key, Modifiers};
use vaxis::tty::{PosixTty, Tty};
use vaxis::vaxis::{Options, Vaxis};
use vaxis::widgets::alignment;

const ZIG_PNG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/zig.png");
const VAXIS_PNG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/vaxis.png");

enum Event {
    KeyPress(Key),
    Winsize(Winsize),
}

impl FromEvent for Event {
    fn from_event(event: VxEvent) -> Option<Self> {
        match event {
            VxEvent::KeyPress(key) => Some(Event::KeyPress(key)),
            VxEvent::Winsize(ws) => Some(Event::Winsize(ws)),
            _ => None,
        }
    }
}

/// Decodes and transmits both PNGs. Split out so the caller can tear the
/// terminal down cleanly when kitty graphics is unavailable (e.g. under tmux,
/// where transmission fails with `NoGraphicsCapability`).
fn transmit_images(vx: &mut Vaxis, tty: &mut PosixTty) -> Result<[Image; 2], Box<dyn Error>> {
    let zig = image::open(ZIG_PNG)?;
    Ok([
        vx.transmit_image(&mut tty.writer(), &zig, TransmitFormat::Rgba)?,
        vx.load_image(&mut tty.writer(), Source::Path(VAXIS_PNG.to_string()))?,
    ])
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut tty = PosixTty::new()?;
    let mut vx = Vaxis::new(Options::default());
    let mut input_loop = Loop::<Event>::init(&tty, &vx)?;
    input_loop.install_resize_handler(&tty)?;
    input_loop.start();

    vx.enter_alt_screen(&mut tty.writer())?;
    vx.query_terminal(&mut tty.writer(), Duration::from_secs(1))?;

    let imgs = match transmit_images(&mut vx, &mut tty) {
        Ok(imgs) => imgs,
        Err(err) => {
            // Restore the terminal and report rather than spinning a frame loop
            // that would fail to draw on every iteration.
            input_loop.signal_stop();
            let _ = vx.device_status_report(&mut tty.writer());
            input_loop.stop();
            vx.reset_state(&mut tty.writer())?;
            eprintln!("cannot transmit images: {err}");
            return Ok(());
        }
    };

    let mut n: usize = 0;
    let mut clip_y: u16 = 0;

    'main: loop {
        match input_loop.next_event() {
            Event::KeyPress(key) => {
                if key.matches(u32::from('c'), Modifiers::CTRL) {
                    break 'main;
                } else if key.matches(u32::from('l'), Modifiers::CTRL) {
                    vx.queue_refresh();
                } else if key.matches(u32::from('j'), Modifiers::empty()) {
                    clip_y += 1;
                } else if key.matches(u32::from('k'), Modifiers::empty()) {
                    clip_y = clip_y.saturating_sub(1);
                }
            }
            Event::Winsize(ws) => vx.resize(&mut tty.writer(), ws)?,
        }

        n = (n + 1) % imgs.len();
        let win = vx.window();
        win.clear();

        let img = imgs[n];
        let dims = img.cell_size(&win)?;
        let center = alignment::center(win, dims.cols, dims.rows);
        img.draw(
            &center,
            DrawOptions {
                scale: Scale::Contain,
                clip_region: Some(ClipRegion {
                    y: Some(clip_y),
                    ..ClipRegion::default()
                }),
                ..DrawOptions::default()
            },
        )?;

        vx.render(&mut tty.writer())?;
    }

    vx.free_image(&mut tty.writer(), imgs[0].id());
    vx.free_image(&mut tty.writer(), imgs[1].id());

    input_loop.signal_stop();
    let _ = vx.device_status_report(&mut tty.writer());
    input_loop.stop();
    vx.reset_state(&mut tty.writer())?;
    Ok(())
}
