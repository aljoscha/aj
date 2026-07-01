//! Encoding a [`Key`] back into the byte sequence a child program expects on
//! its input, the inverse of the crate's input [`crate::parser`].
//!
//! Only the legacy (non-Kitty) encoding is implemented. Upstream hits
//! `unreachable` for any non-zero Kitty flag set, and we mirror that limitation
//! by returning [`EncodeError::KittyUnsupported`] (see [`encode`]).

use std::io::Write;

use thiserror::Error;

use crate::key::{Key, KittyFlags};

/// A failure while encoding a key.
#[derive(Debug, Error)]
pub enum EncodeError {
    /// Writing to the output failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Kitty keyboard encoding was requested (a non-zero flag set). Upstream
    /// leaves this path unimplemented, and so do we.
    #[error("kitty keyboard encoding is not implemented")]
    KittyUnsupported,
}

/// Encodes `key` to `writer`.
///
/// A release event (`press == false`) writes nothing. A press with an empty
/// Kitty flag set uses the [`legacy`] encoding. A non-empty flag set returns
/// [`EncodeError::KittyUnsupported`].
pub fn encode(
    writer: &mut impl Write,
    key: &Key,
    press: bool,
    kitty_flags: KittyFlags,
) -> Result<(), EncodeError> {
    if !press {
        return Ok(());
    }
    if !kitty_flags.is_empty() {
        return Err(EncodeError::KittyUnsupported);
    }
    legacy(writer, key)
}

/// The legacy (pre-Kitty) key encoding.
fn legacy(writer: &mut impl Write, key: &Key) -> Result<(), EncodeError> {
    // Text always wins: if the key carries text, write it verbatim.
    if let Some(text) = &key.text {
        writer.write_all(text.as_bytes())?;
        return Ok(());
    }

    const SHIFT: u8 = 0b0000_0001;
    const ALT: u8 = 0b0000_0010;
    const CTRL: u8 = 0b0000_0100;

    let effective_mods = key.mods.bits() & (SHIFT | ALT | CTRL);

    // No mods and an ASCII codepoint: write the byte directly.
    if effective_mods == 0 && key.codepoint <= 0x7f {
        let b = u8::try_from(key.codepoint).expect("codepoint <= 0x7f fits in u8");
        writer.write_all(&[b])?;
        return Ok(());
    }

    // ctrl + lowercase ASCII maps to a C0 control byte.
    if effective_mods == CTRL && (u32::from(b'a')..=u32::from(b'z')).contains(&key.codepoint) {
        let b = u8::try_from(key.codepoint).expect("lowercase ASCII fits in u8");
        writer.write_all(&[b.saturating_sub(0x60)])?;
        return Ok(());
    }

    // alt + printable ASCII: ESC then the character.
    if effective_mods == ALT && key.codepoint >= u32::from(b' ') && key.codepoint < 0x7f {
        let b = u8::try_from(key.codepoint).expect("printable ASCII fits in u8");
        write!(writer, "\x1b{}", char::from(b))?;
        return Ok(());
    }

    // ctrl + alt + lowercase ASCII: ESC then the control number in decimal.
    //
    // NOTE: upstream does not return after this write, falling through to the
    // special-key lookup below. For the a-z range that lookup returns without
    // writing more, so the fall-through is harmless. We reproduce it.
    if effective_mods == (CTRL | ALT)
        && (u32::from(b'a')..=u32::from(b'z')).contains(&key.codepoint)
    {
        write!(writer, "\x1b{}", key.codepoint - 0x60)?;
    }

    let def = match key.codepoint {
        Key::ESCAPE => ESCAPE,
        Key::ENTER | Key::KP_ENTER => ENTER,
        Key::TAB => TAB,
        Key::BACKSPACE => BACKSPACE,
        Key::INSERT | Key::KP_INSERT => INSERT,
        Key::DELETE | Key::KP_DELETE => DELETE,
        Key::LEFT | Key::KP_LEFT => LEFT,
        Key::RIGHT | Key::KP_RIGHT => RIGHT,
        Key::UP | Key::KP_UP => UP,
        Key::DOWN | Key::KP_DOWN => DOWN,
        Key::PAGE_UP | Key::KP_PAGE_UP => PAGE_UP,
        Key::PAGE_DOWN | Key::KP_PAGE_DOWN => PAGE_DOWN,
        Key::HOME | Key::KP_HOME => HOME,
        Key::END | Key::KP_END => END,
        Key::F1 => F1,
        Key::F2 => F2,
        Key::F3 => F3_LEGACY,
        Key::F4 => F4,
        Key::F5 => F5,
        Key::F6 => F6,
        Key::F7 => F7,
        Key::F8 => F8,
        Key::F9 => F9,
        Key::F10 => F10,
        Key::F11 => F11,
        Key::F12 => F12,
        _ => return Ok(()),
    };

    let suffix = char::from(def.suffix);
    if effective_mods == 0 {
        if def.number == 1 {
            // The number-1 keys split: F1-F4 use SS3 (ESC O), the rest CSI.
            match key.codepoint {
                Key::F1 | Key::F2 | Key::F3 | Key::F4 => write!(writer, "\x1bO{suffix}")?,
                _ => write!(writer, "\x1b[{suffix}")?,
            }
        } else {
            write!(writer, "\x1b[{}{suffix}", def.number)?;
        }
    } else {
        write!(writer, "\x1b[{};{}{suffix}", def.number, effective_mods + 1)?;
    }
    Ok(())
}

/// A special key's CSI/SS3 encoding: a numeric parameter and a final byte.
struct Definition {
    number: u32,
    suffix: u8,
}

// NOTE: upstream also defines caps_lock/scroll_lock/num_lock/print_screen/
// pause/menu and f3/f13.. definitions that the legacy switch never reaches. We
// only define the ones the legacy encoding actually emits.
const ESCAPE: Definition = Definition {
    number: 27,
    suffix: b'u',
};
const ENTER: Definition = Definition {
    number: 13,
    suffix: b'u',
};
const TAB: Definition = Definition {
    number: 9,
    suffix: b'u',
};
const BACKSPACE: Definition = Definition {
    number: 127,
    suffix: b'u',
};
const INSERT: Definition = Definition {
    number: 2,
    suffix: b'~',
};
const DELETE: Definition = Definition {
    number: 3,
    suffix: b'~',
};
const LEFT: Definition = Definition {
    number: 1,
    suffix: b'D',
};
const RIGHT: Definition = Definition {
    number: 1,
    suffix: b'C',
};
const UP: Definition = Definition {
    number: 1,
    suffix: b'A',
};
const DOWN: Definition = Definition {
    number: 1,
    suffix: b'B',
};
const PAGE_UP: Definition = Definition {
    number: 5,
    suffix: b'~',
};
const PAGE_DOWN: Definition = Definition {
    number: 6,
    suffix: b'~',
};
const HOME: Definition = Definition {
    number: 1,
    suffix: b'H',
};
const END: Definition = Definition {
    number: 1,
    suffix: b'F',
};
const F1: Definition = Definition {
    number: 1,
    suffix: b'P',
};
const F2: Definition = Definition {
    number: 1,
    suffix: b'Q',
};
/// The legacy F3 form: CSI 1 R. Upstream keeps a separate curly-brace-free
/// `f3` (`CSI 13 ~`) that the legacy path does not use.
const F3_LEGACY: Definition = Definition {
    number: 1,
    suffix: b'R',
};
const F4: Definition = Definition {
    number: 1,
    suffix: b'S',
};
const F5: Definition = Definition {
    number: 15,
    suffix: b'~',
};
const F6: Definition = Definition {
    number: 17,
    suffix: b'~',
};
const F7: Definition = Definition {
    number: 18,
    suffix: b'~',
};
const F8: Definition = Definition {
    number: 19,
    suffix: b'~',
};
const F9: Definition = Definition {
    number: 20,
    suffix: b'~',
};
const F10: Definition = Definition {
    number: 21,
    suffix: b'~',
};
const F11: Definition = Definition {
    number: 23,
    suffix: b'~',
};
const F12: Definition = Definition {
    number: 24,
    suffix: b'~',
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::Modifiers;

    fn enc(key: &Key) -> Vec<u8> {
        let mut out = Vec::new();
        encode(&mut out, key, true, KittyFlags::empty()).unwrap();
        out
    }

    fn key(codepoint: u32, mods: Modifiers) -> Key {
        Key {
            codepoint,
            mods,
            ..Default::default()
        }
    }

    #[test]
    fn bare_ascii() {
        assert_eq!(enc(&key(u32::from('a'), Modifiers::empty())), b"a");
    }

    #[test]
    fn text_wins() {
        let k = Key {
            codepoint: u32::from('a'),
            text: Some("é".into()),
            ..Default::default()
        };
        assert_eq!(enc(&k), "é".as_bytes());
    }

    #[test]
    fn ctrl_a_is_control_byte() {
        assert_eq!(enc(&key(u32::from('a'), Modifiers::CTRL)), b"\x01");
    }

    #[test]
    fn alt_a_is_esc_prefixed() {
        assert_eq!(enc(&key(u32::from('a'), Modifiers::ALT)), b"\x1ba");
    }

    #[test]
    fn ascii_control_keys_emit_their_byte() {
        // Enter, Tab, Backspace, Escape all have ASCII codepoints <= 0x7f, so
        // with no mods they take the bare-ASCII path.
        assert_eq!(enc(&key(Key::ENTER, Modifiers::empty())), b"\r");
        assert_eq!(enc(&key(Key::TAB, Modifiers::empty())), b"\t");
        assert_eq!(enc(&key(Key::BACKSPACE, Modifiers::empty())), b"\x7f");
        assert_eq!(enc(&key(Key::ESCAPE, Modifiers::empty())), b"\x1b");
    }

    #[test]
    fn arrows_unmodified() {
        assert_eq!(enc(&key(Key::LEFT, Modifiers::empty())), b"\x1b[D");
        assert_eq!(enc(&key(Key::RIGHT, Modifiers::empty())), b"\x1b[C");
        assert_eq!(enc(&key(Key::UP, Modifiers::empty())), b"\x1b[A");
        assert_eq!(enc(&key(Key::DOWN, Modifiers::empty())), b"\x1b[B");
    }

    #[test]
    fn arrows_with_shift() {
        // shift = mod bit 0, so the modifier parameter is shift + 1 = 2.
        assert_eq!(enc(&key(Key::LEFT, Modifiers::SHIFT)), b"\x1b[1;2D");
        assert_eq!(enc(&key(Key::UP, Modifiers::CTRL)), b"\x1b[1;5A");
    }

    #[test]
    fn home_end_and_navigation() {
        assert_eq!(enc(&key(Key::HOME, Modifiers::empty())), b"\x1b[H");
        assert_eq!(enc(&key(Key::END, Modifiers::empty())), b"\x1b[F");
        assert_eq!(enc(&key(Key::INSERT, Modifiers::empty())), b"\x1b[2~");
        assert_eq!(enc(&key(Key::DELETE, Modifiers::empty())), b"\x1b[3~");
        assert_eq!(enc(&key(Key::PAGE_UP, Modifiers::empty())), b"\x1b[5~");
        assert_eq!(enc(&key(Key::PAGE_DOWN, Modifiers::empty())), b"\x1b[6~");
    }

    #[test]
    fn function_keys_unmodified() {
        // F1-F4 use SS3 (ESC O), F5+ use CSI with a numeric parameter.
        assert_eq!(enc(&key(Key::F1, Modifiers::empty())), b"\x1bOP");
        assert_eq!(enc(&key(Key::F2, Modifiers::empty())), b"\x1bOQ");
        assert_eq!(enc(&key(Key::F3, Modifiers::empty())), b"\x1bOR");
        assert_eq!(enc(&key(Key::F4, Modifiers::empty())), b"\x1bOS");
        assert_eq!(enc(&key(Key::F5, Modifiers::empty())), b"\x1b[15~");
        assert_eq!(enc(&key(Key::F12, Modifiers::empty())), b"\x1b[24~");
    }

    #[test]
    fn function_keys_with_modifiers() {
        assert_eq!(enc(&key(Key::F1, Modifiers::SHIFT)), b"\x1b[1;2P");
        assert_eq!(enc(&key(Key::F5, Modifiers::SHIFT)), b"\x1b[15;2~");
    }

    #[test]
    fn release_writes_nothing() {
        let mut out = Vec::new();
        encode(
            &mut out,
            &key(u32::from('a'), Modifiers::empty()),
            false,
            KittyFlags::empty(),
        )
        .unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn kitty_flags_are_unsupported() {
        let mut out = Vec::new();
        let result = encode(
            &mut out,
            &key(u32::from('a'), Modifiers::empty()),
            true,
            KittyFlags::DISAMBIGUATE,
        );
        assert!(matches!(result, Err(EncodeError::KittyUnsupported)));
    }
}
