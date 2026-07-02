//! The `Send` event type the input readers produce for the vxfw runtimes.

use crate::Winsize;
use crate::cell;
use crate::event::Event as InternalEvent;
use crate::event_loop::FromEvent;
use crate::key::Key;
use crate::mouse::Mouse;
use crate::vxfw::Event;

/// The `Send` event type carried by the input readers.
///
/// NOTE: This is the reader-produced subset of [`Event`] plus
/// [`LoopEvent::Init`] (which the synchronous `App` posts on start). It exists
/// because the full vxfw [`Event`] holds an `Rc` in its `App` variant and so is
/// not `Send`, while the readers (the threaded loop and the async front-end)
/// require a `Send` event. A runtime converts each drained `LoopEvent` into an
/// [`Event`] for dispatch, so application-posted `Event::App` values never
/// travel through a reader.
pub(crate) enum LoopEvent {
    KeyPress(Key),
    KeyRelease(Key),
    Mouse(Mouse),
    MouseLeave,
    FocusIn,
    FocusOut,
    PasteStart,
    PasteEnd,
    Paste(String),
    ColorReport(cell::Report),
    ColorScheme(cell::Scheme),
    Winsize(Winsize),
    Init,
}

impl LoopEvent {
    /// Converts a loop event into the user-facing dispatch event.
    pub(crate) fn into_event(self) -> Event {
        match self {
            LoopEvent::KeyPress(k) => Event::KeyPress(k),
            LoopEvent::KeyRelease(k) => Event::KeyRelease(k),
            LoopEvent::Mouse(m) => Event::Mouse(m),
            LoopEvent::MouseLeave => Event::MouseLeave,
            LoopEvent::FocusIn => Event::FocusIn,
            LoopEvent::FocusOut => Event::FocusOut,
            LoopEvent::PasteStart => Event::PasteStart,
            LoopEvent::PasteEnd => Event::PasteEnd,
            LoopEvent::Paste(s) => Event::Paste(s),
            LoopEvent::ColorReport(r) => Event::ColorReport(r),
            LoopEvent::ColorScheme(s) => Event::ColorScheme(s),
            LoopEvent::Winsize(ws) => Event::Winsize(ws),
            LoopEvent::Init => Event::Init,
        }
    }
}

impl FromEvent for LoopEvent {
    fn from_event(event: InternalEvent) -> Option<Self> {
        Some(match event {
            InternalEvent::KeyPress(k) => LoopEvent::KeyPress(k),
            InternalEvent::KeyRelease(k) => LoopEvent::KeyRelease(k),
            InternalEvent::Mouse(m) => LoopEvent::Mouse(m),
            InternalEvent::MouseLeave => LoopEvent::MouseLeave,
            InternalEvent::FocusIn => LoopEvent::FocusIn,
            InternalEvent::FocusOut => LoopEvent::FocusOut,
            InternalEvent::PasteStart => LoopEvent::PasteStart,
            InternalEvent::PasteEnd => LoopEvent::PasteEnd,
            InternalEvent::Paste(s) => LoopEvent::Paste(s),
            InternalEvent::ColorReport(r) => LoopEvent::ColorReport(r),
            InternalEvent::ColorScheme(s) => LoopEvent::ColorScheme(s),
            InternalEvent::Winsize(ws) => LoopEvent::Winsize(ws),
            // Capability responses are consumed by the reader, never delivered.
            _ => return None,
        })
    }
}
