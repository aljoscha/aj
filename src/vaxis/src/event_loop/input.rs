//! The shared byte-pump core: source- and sink-agnostic input decoding.
//!
//! Both the threaded [`Loop`](crate::event_loop::Loop) and the async front-end
//! drive an [`InputCore`]. It owns a read buffer and the partial-sequence
//! resync, runs the input [`Parser`](crate::parser::Parser), folds capability
//! responses into the shared [`Shared`] detection state, and hands ordinary
//! events (converted to the caller's user type) to a sink. It performs no I/O
//! itself, so it is fully unit-testable by feeding byte fragments.

use std::sync::Arc;

use crate::event::Event;
use crate::event_loop::FromEvent;
use crate::gwidth;
use crate::key::{Key, Modifiers};
use crate::parser::{ParseError, Parser};
use crate::vaxis::Shared;

/// The byte-pump: read buffer plus the parser, parameterized by neither source
/// nor sink so a single implementation serves the threaded and async readers.
pub(crate) struct InputCore {
    parser: Parser,
    shared: Arc<Shared>,
    /// Bytes received but not yet consumed by the parser. After
    /// [`feed`](InputCore::feed) this holds at most one trailing incomplete
    /// sequence.
    buf: Vec<u8>,
}

impl InputCore {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self {
            parser: Parser::new(),
            shared,
            // Upstream uses a 1 KiB read buffer; reserve the same so the common
            // case never reallocates.
            buf: Vec::with_capacity(1024),
        }
    }

    /// Appends `bytes` to the read buffer and dispatches every complete event to
    /// `sink`, retaining any trailing incomplete sequence for the next call.
    ///
    /// Returns the first [`ParseError`] encountered. On error the caller should
    /// stop driving this core: the offending bytes are still buffered, so a
    /// retry would re-hit the same error.
    pub(crate) fn feed<E, S>(&mut self, bytes: &[u8], sink: &mut S) -> Result<(), ParseError>
    where
        E: FromEvent,
        S: FnMut(E),
    {
        self.buf.extend_from_slice(bytes);
        self.process(sink)
    }

    /// Number of buffered bytes (the incomplete tail awaiting more input).
    /// Tests assert the resync consumes the right counts through this.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.buf.len()
    }

    fn process<E, S>(&mut self, sink: &mut S) -> Result<(), ParseError>
    where
        E: FromEvent,
        S: FnMut(E),
    {
        let mut start = 0;
        while start < self.buf.len() {
            // `self.parser` and `self.buf` are disjoint fields, so the mutable
            // parser borrow and the immutable buffer borrow do not conflict.
            let result = self.parser.parse(&self.buf[start..])?;
            if result.n == 0 {
                // The buffer ends mid-sequence. Stop and keep the tail; the
                // next `feed` appends to it and we retry from the same offset.
                break;
            }
            start += result.n;
            if let Some(event) = result.event {
                self.dispatch(event, sink);
            }
        }
        // Drop the consumed prefix, shifting any incomplete tail to the front.
        // The source and destination ranges overlap, so this is a memmove
        // (`copy_within`), matching upstream's manual overlapping shift.
        if start > 0 {
            let remaining = self.buf.len() - start;
            self.buf.copy_within(start.., 0);
            self.buf.truncate(remaining);
        }
        Ok(())
    }

    /// Mirrors upstream `handleEventGeneric`: capability responses and the F3
    /// probes update the shared detected capabilities (and DA1 wakes the
    /// handshake), mouse reports are translated, an in-band winsize sets the
    /// resize flag, and ordinary events are converted via [`FromEvent`] and
    /// handed to the sink.
    fn dispatch<E, S>(&self, event: Event, sink: &mut S)
    where
        E: FromEvent,
        S: FnMut(E),
    {
        match event {
            // The explicit-width and scaled-text probes encode as an F3 cursor
            // report. They are only probes while a query batch is outstanding;
            // once `queries_done`, an F3 is a real key and falls through below.
            Event::KeyPress(ref key)
                if !self.shared.queries_done() && is_explicit_width_probe(key) =>
            {
                self.shared.update_detected(|caps| {
                    caps.explicit_width = true;
                    caps.unicode = gwidth::Method::Unicode;
                });
            }
            Event::KeyPress(ref key)
                if !self.shared.queries_done() && is_scaled_text_probe(key) =>
            {
                self.shared.update_detected(|caps| caps.scaled_text = true);
            }

            Event::CapKittyKeyboard => {
                self.shared
                    .update_detected(|caps| caps.kitty_keyboard = true);
            }
            Event::CapKittyGraphics => {
                self.shared
                    .update_detected(|caps| caps.kitty_graphics = true);
            }
            Event::CapRgb => self.shared.update_detected(|caps| caps.rgb = true),
            Event::CapSgrPixels => self.shared.update_detected(|caps| caps.sgr_pixels = true),
            Event::CapUnicode => {
                self.shared
                    .update_detected(|caps| caps.unicode = gwidth::Method::Unicode);
            }
            Event::CapColorSchemeUpdates => {
                self.shared
                    .update_detected(|caps| caps.color_scheme_updates = true);
            }
            Event::CapMultiCursor => {
                self.shared.update_detected(|caps| caps.multi_cursor = true);
            }
            Event::CapDa1 => self.shared.notify_da1(),

            Event::Mouse(mouse) => {
                let translated = self.shared.translate_mouse(mouse);
                if let Some(event) = E::from_event(Event::Mouse(translated)) {
                    sink(event);
                }
            }
            Event::Winsize(winsize) => {
                // An in-band winsize report proves the terminal supports DEC
                // 2048; flag it so the out-of-band SIGWINCH path stands down and
                // the mode teardown is emitted on reset.
                self.shared.set_in_band_resize();
                if let Some(event) = E::from_event(Event::Winsize(winsize)) {
                    sink(event);
                }
            }

            other => {
                if let Some(event) = E::from_event(other) {
                    sink(event);
                }
            }
        }
    }
}

/// The explicit-width probe reply: an F3 cursor report with the Shift bit set.
fn is_explicit_width_probe(key: &Key) -> bool {
    key.codepoint == Key::F3 && key.mods.contains(Modifiers::SHIFT)
}

/// The scaled-text probe reply: an F3 cursor report with the Alt bit set.
fn is_scaled_text_probe(key: &Key) -> bool {
    key.codepoint == Key::F3 && key.mods.contains(Modifiers::ALT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vaxis::Capabilities;

    /// A user event type narrower than the internal [`Event`]. The `Foo`
    /// variant has no counterpart in `Event`, proving the user type is
    /// decoupled from the internal superset.
    #[derive(Debug, PartialEq, Eq)]
    enum UserEvent {
        Key(Key),
        Foo(u8),
    }

    impl FromEvent for UserEvent {
        fn from_event(event: Event) -> Option<Self> {
            match event {
                Event::KeyPress(key) => Some(UserEvent::Key(key)),
                _ => None,
            }
        }
    }

    fn core() -> InputCore {
        InputCore::new(Shared::new())
    }

    #[test]
    fn resync_across_two_reads() {
        // "\x1b[" is an incomplete CSI: feeding it yields no event and the two
        // bytes are retained. Appending "A" completes CSI A (cursor up).
        let mut core = core();
        let mut events: Vec<Event> = Vec::new();

        core.feed(b"\x1b[", &mut |event: Event| events.push(event))
            .expect("feed");
        assert!(
            events.is_empty(),
            "partial sequence must not produce an event"
        );
        assert_eq!(core.pending(), 2, "the incomplete CSI is retained");

        core.feed(b"A", &mut |event: Event| events.push(event))
            .expect("feed");
        assert_eq!(
            events,
            vec![Event::KeyPress(Key {
                codepoint: Key::UP,
                ..Default::default()
            })]
        );
        assert_eq!(
            core.pending(),
            0,
            "the completed sequence is fully consumed"
        );
    }

    #[test]
    fn resync_byte_at_a_time() {
        // Drip a multi-byte CSI one byte per feed, starting from the "\x1b["
        // intro. We cannot split the leading ESC from the "[": a lone "\x1b" is
        // the Escape key, not a sequence start (the parser's deliberate
        // disambiguation). The bracketed-paste-start "\x1b[200~" completes only
        // on the final byte.
        let mut core = core();
        let mut events: Vec<Event> = Vec::new();
        let mut sink = |event: Event| events.push(event);

        for byte in [&b"\x1b["[..], b"2", b"0", b"0", b"~"] {
            core.feed(byte, &mut sink).expect("feed");
        }
        assert_eq!(events, vec![Event::PasteStart]);
        assert_eq!(core.pending(), 0);
    }

    #[test]
    fn several_concatenated_sequences_in_one_feed() {
        // A single feed holding three back-to-back sequences must produce all
        // three events: 'a', cursor-up, 'b'.
        let mut core = core();
        let mut events: Vec<Event> = Vec::new();
        let mut sink = |event: Event| events.push(event);

        core.feed(b"a\x1b[Ab", &mut sink).expect("feed");
        assert_eq!(
            events,
            vec![
                Event::KeyPress(Key {
                    codepoint: u32::from('a'),
                    text: Some("a".into()),
                    ..Default::default()
                }),
                Event::KeyPress(Key {
                    codepoint: Key::UP,
                    ..Default::default()
                }),
                Event::KeyPress(Key {
                    codepoint: u32::from('b'),
                    text: Some("b".into()),
                    ..Default::default()
                }),
            ]
        );
        assert_eq!(core.pending(), 0);
    }

    #[test]
    fn concatenated_sequences_with_trailing_partial() {
        // A complete key followed by an incomplete CSI: the key is produced and
        // the partial tail is retained.
        let mut core = core();
        let mut events: Vec<Event> = Vec::new();
        let mut sink = |event: Event| events.push(event);

        core.feed(b"a\x1b[", &mut sink).expect("feed");
        assert_eq!(events.len(), 1);
        assert_eq!(core.pending(), 2, "the trailing incomplete CSI is retained");
    }

    #[test]
    fn from_event_filters_to_the_user_subset() {
        // The custom UserEvent only accepts key presses. Mixed internal events
        // (a key press, a mouse report, a focus change) must produce a single
        // UserEvent::Key.
        let mut core = core();
        let mut events: Vec<UserEvent> = Vec::new();
        let mut sink = |event: UserEvent| events.push(event);

        // 'a' (key), SGR mouse, focus-in.
        core.feed(b"a\x1b[<35;1;1m\x1b[I", &mut sink).expect("feed");
        assert_eq!(
            events,
            vec![UserEvent::Key(Key {
                codepoint: u32::from('a'),
                text: Some("a".into()),
                ..Default::default()
            })]
        );
        // The Foo variant exists but is never produced from terminal input; it
        // is reachable only through `post_event`. Reference it so the decoupled
        // variant is exercised.
        assert_ne!(events[0], UserEvent::Foo(0));
    }

    #[test]
    fn capability_responses_fold_into_shared_and_da1_fires() {
        // Feed two DECRPM capability reports, the explicit-width F3 probe, and
        // the DA1 reply. The detected caps must reflect all three, and the DA1
        // handshake must fire so a waiting query_terminal would wake.
        let shared = Shared::new();
        let mut core = InputCore::new(Arc::clone(&shared));
        let mut events: Vec<Event> = Vec::new();
        let mut sink = |event: Event| events.push(event);

        // While a query batch is outstanding, the reader treats F3 reports as
        // capability probes rather than keys.
        shared.begin_query(Capabilities::default());

        // sgr-pixels DECRPM, unicode DECRPM, explicit-width F3 (shift), DA1.
        core.feed(b"\x1b[?1016;1$y\x1b[?2027;1$y\x1b[1;2R\x1b[?c", &mut sink)
            .expect("feed");

        let detected = shared.detected();
        assert!(detected.sgr_pixels, "sgr-pixels capability folded");
        assert_eq!(
            detected.unicode,
            gwidth::Method::Unicode,
            "unicode capability folded"
        );
        assert!(detected.explicit_width, "explicit-width probe folded");
        assert!(shared.da1_fired(), "DA1 handshake fired");
        assert!(shared.queries_done(), "DA1 marks queries done");
        assert!(
            events.is_empty(),
            "capability and probe responses are not user events"
        );
    }

    #[test]
    fn f3_is_a_real_key_once_queries_done() {
        // After detection finishes, an F3-with-shift report is a genuine key
        // press and is delivered, not swallowed as a probe.
        let shared = Shared::new();
        let mut core = InputCore::new(Arc::clone(&shared));
        let mut events: Vec<Event> = Vec::new();
        let mut sink = |event: Event| events.push(event);

        // queries_done defaults true on a fresh Shared.
        core.feed(b"\x1b[1;2R", &mut sink).expect("feed");
        assert_eq!(
            events,
            vec![Event::KeyPress(Key {
                codepoint: Key::F3,
                mods: Modifiers::SHIFT,
                ..Default::default()
            })]
        );
        assert!(!shared.detected().explicit_width);
    }
}
