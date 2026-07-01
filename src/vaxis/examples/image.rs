//! Port of upstream `examples/image.zig`: transmits two images to the terminal
//! via the kitty graphics protocol and cycles between them each frame, centered
//! and scaled to `contain`, with `j`/`k` panning the clip region.
//!
//! Upstream decodes `examples/vaxis.png` from disk. To keep the example free of
//! an external asset we generate two small gradients in memory: one transmitted
//! raw (RGBA), one round-tripped through PNG bytes via `load_image`, so both
//! transmission paths are exercised.

use std::error::Error;
use std::io::Cursor;
use std::time::Duration;

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use vaxis::Winsize;
use vaxis::event::Event as VxEvent;
use vaxis::event_loop::{FromEvent, Loop};
use vaxis::image::{ClipRegion, DrawOptions, Scale, Source, TransmitFormat};
use vaxis::key::{Key, Modifiers};
use vaxis::tty::{PosixTty, Tty};
use vaxis::vaxis::{Options, Vaxis};
use vaxis::widgets::alignment;

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

/// A small RGBA gradient, so the example needs no image file on disk.
fn gradient(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgba8(RgbaImage::from_fn(width, height, |x, y| {
        let r = u8::try_from((x * 255) / width.max(1)).unwrap_or(255);
        let g = u8::try_from((y * 255) / height.max(1)).unwrap_or(255);
        let b = u8::try_from(((x + y) * 255) / (width + height).max(1)).unwrap_or(255);
        Rgba([r, g, b, 255])
    }))
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut tty = PosixTty::new()?;
    let mut vx = Vaxis::new(Options::default());
    let mut input_loop = Loop::<Event>::init(&tty, &vx)?;
    input_loop.install_resize_handler(&tty)?;
    input_loop.start();

    vx.enter_alt_screen(&mut tty.writer())?;
    vx.query_terminal(&mut tty.writer(), Duration::from_secs(1))?;

    let img1 = gradient(64, 64);
    let img2 = gradient(48, 80);
    let mut img2_png = Vec::new();
    img2.write_to(&mut Cursor::new(&mut img2_png), ImageFormat::Png)?;

    let imgs = [
        vx.transmit_image(&mut tty.writer(), &img1, TransmitFormat::Rgba)?,
        vx.load_image(&mut tty.writer(), Source::Mem(img2_png))?,
    ];

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
