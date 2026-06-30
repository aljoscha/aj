//! The internal `Event` superset produced by the parser and the event loop.

use crate::Winsize;
use crate::cell::{Report, Scheme};
use crate::key::Key;
use crate::mouse::Mouse;

/// The events Vaxis emits internally.
///
/// NOTE: This is the internal superset. The user-facing vxfw `Event` is a
/// different type, and the threaded `Loop` filters this superset down to a
/// caller-chosen subset. Both come in later phases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    KeyPress(Key),
    KeyRelease(Key),
    Mouse(Mouse),
    MouseLeave,
    FocusIn,
    FocusOut,
    /// Bracketed-paste start.
    PasteStart,
    /// Bracketed-paste end.
    PasteEnd,
    /// OSC 52 paste payload. Owned heap data, since clipboard contents are of
    /// arbitrary length and outlive the parse.
    Paste(String),
    /// OSC 4, 10, 11, or 12 color response.
    ColorReport(Report),
    ColorScheme(Scheme),
    Winsize(Winsize),

    // Delivered as discovered terminal capabilities.
    CapKittyKeyboard,
    CapKittyGraphics,
    CapRgb,
    CapSgrPixels,
    CapUnicode,
    CapDa1,
    CapColorSchemeUpdates,
    CapMultiCursor,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_variants() {
        let key = Key {
            codepoint: u32::from('x'),
            ..Default::default()
        };
        let ev = Event::KeyPress(key.clone());
        assert_eq!(ev, Event::KeyPress(key));

        assert_eq!(Event::CapDa1, Event::CapDa1);
        assert_ne!(Event::FocusIn, Event::FocusOut);
    }

    #[test]
    fn paste_round_trips() {
        let ev = Event::Paste("hello".to_string());
        match ev {
            Event::Paste(s) => assert_eq!(s, "hello"),
            other => panic!("expected a paste event, got {other:?}"),
        }
    }
}
