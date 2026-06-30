//! Keyboard input: `Key`, `Modifiers`, `KittyFlags`, the codepoint constants,
//! and the `matches` family.

use phf::phf_map;

bitflags::bitflags! {
    /// Modifier keys for a key match event.
    ///
    /// The bit layout is load-bearing. The input parser builds a `Modifiers`
    /// by reinterpreting `(kitty_mask - 1)` as these bits via
    /// [`Modifiers::from_bits_retain`], so the order shift, alt, ctrl, super,
    /// hyper, meta, caps_lock, num_lock from bit 0 upward must not change.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const ALT = 1 << 1;
        const CTRL = 1 << 2;
        const SUPER = 1 << 3;
        const HYPER = 1 << 4;
        const META = 1 << 5;
        const CAPS_LOCK = 1 << 6;
        const NUM_LOCK = 1 << 7;
    }
}

impl Modifiers {
    /// True when `self` and `other` carry the exact same bits. Mirrors
    /// upstream's `Modifiers.eql`. Equivalent to the derived `==`.
    pub fn eql(self, other: Modifiers) -> bool {
        self.bits() == other.bits()
    }
}

bitflags::bitflags! {
    /// Flags for the Kitty keyboard protocol.
    ///
    /// Upstream backs this with a `u5`. Rust has no `u5`, so we store the five
    /// flags in the low bits of a `u8` and expose the raw value through
    /// [`KittyFlags::bits`] for the `CSI u` push encoder.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct KittyFlags: u8 {
        const DISAMBIGUATE = 1 << 0;
        const REPORT_EVENTS = 1 << 1;
        const REPORT_ALTERNATE_KEYS = 1 << 2;
        const REPORT_ALL_AS_CTL_SEQS = 1 << 3;
        const REPORT_TEXT = 1 << 4;
    }
}

impl Default for KittyFlags {
    /// The default flag set vaxis pushes: everything but `report_events`.
    fn default() -> Self {
        Self::DISAMBIGUATE
            | Self::REPORT_ALTERNATE_KEYS
            | Self::REPORT_ALL_AS_CTL_SEQS
            | Self::REPORT_TEXT
    }
}

/// A keyboard key event.
///
/// NOTE: Codepoints are plain `u32`, not `char`. The [`Key::MULTICODEPOINT`]
/// sentinel is `1_114_113` (max Unicode + 1), which exceeds `char::MAX`, so a
/// `char` cannot represent every value this type must carry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Key {
    /// The Unicode codepoint of the key event.
    pub codepoint: u32,

    /// The text generated from the key event.
    ///
    /// Owned (per the D1 grapheme-ownership decision) so a `Key` can cross
    /// thread and channel boundaries without borrowing the parser's scratch.
    pub text: Option<compact_str::CompactString>,

    /// The shifted codepoint of this key event. Present only if the Shift
    /// modifier was used to generate the event.
    pub shifted_codepoint: Option<u32>,

    /// The key that would have been pressed on a standard keyboard layout.
    /// Useful for shortcut matching.
    pub base_layout_codepoint: Option<u32>,

    pub mods: Modifiers,
}

impl Key {
    // A few special keys encoded as their actual ASCII value.
    pub const TAB: u32 = 0x09;
    pub const ENTER: u32 = 0x0D;
    pub const ESCAPE: u32 = 0x1B;
    pub const SPACE: u32 = 0x20;
    pub const BACKSPACE: u32 = 0x7F;

    /// A key that generated text but cannot be expressed as a single
    /// codepoint. The value is the maximum Unicode codepoint + 1.
    pub const MULTICODEPOINT: u32 = 1_114_112 + 1;

    // Kitty encodes these keys directly in the private use area. We reuse those
    // mappings.
    pub const INSERT: u32 = 57348;
    pub const DELETE: u32 = 57349;
    pub const LEFT: u32 = 57350;
    pub const RIGHT: u32 = 57351;
    pub const UP: u32 = 57352;
    pub const DOWN: u32 = 57353;
    pub const PAGE_UP: u32 = 57354;
    pub const PAGE_DOWN: u32 = 57355;
    pub const HOME: u32 = 57356;
    pub const END: u32 = 57357;
    pub const CAPS_LOCK: u32 = 57358;
    pub const SCROLL_LOCK: u32 = 57359;
    pub const NUM_LOCK: u32 = 57360;
    pub const PRINT_SCREEN: u32 = 57361;
    pub const PAUSE: u32 = 57362;
    pub const MENU: u32 = 57363;
    pub const F1: u32 = 57364;
    pub const F2: u32 = 57365;
    pub const F3: u32 = 57366;
    pub const F4: u32 = 57367;
    pub const F5: u32 = 57368;
    pub const F6: u32 = 57369;
    pub const F7: u32 = 57370;
    pub const F8: u32 = 57371;
    pub const F9: u32 = 57372;
    pub const F10: u32 = 57373;
    pub const F11: u32 = 57374;
    pub const F12: u32 = 57375;
    pub const F13: u32 = 57376;
    pub const F14: u32 = 57377;
    pub const F15: u32 = 57378;
    pub const F16: u32 = 57379;
    pub const F17: u32 = 57380;
    pub const F18: u32 = 57381;
    pub const F19: u32 = 57382;
    pub const F20: u32 = 57383;
    pub const F21: u32 = 57384;
    pub const F22: u32 = 57385;
    pub const F23: u32 = 57386;
    pub const F24: u32 = 57387;
    pub const F25: u32 = 57388;
    pub const F26: u32 = 57389;
    pub const F27: u32 = 57390;
    pub const F28: u32 = 57391;
    pub const F29: u32 = 57392;
    pub const F30: u32 = 57393;
    pub const F31: u32 = 57394;
    pub const F32: u32 = 57395;
    pub const F33: u32 = 57396;
    pub const F34: u32 = 57397;
    pub const F35: u32 = 57398;
    pub const KP_0: u32 = 57399;
    pub const KP_1: u32 = 57400;
    pub const KP_2: u32 = 57401;
    pub const KP_3: u32 = 57402;
    pub const KP_4: u32 = 57403;
    pub const KP_5: u32 = 57404;
    pub const KP_6: u32 = 57405;
    pub const KP_7: u32 = 57406;
    pub const KP_8: u32 = 57407;
    pub const KP_9: u32 = 57408;
    pub const KP_DECIMAL: u32 = 57409;
    pub const KP_DIVIDE: u32 = 57410;
    pub const KP_MULTIPLY: u32 = 57411;
    pub const KP_SUBTRACT: u32 = 57412;
    pub const KP_ADD: u32 = 57413;
    pub const KP_ENTER: u32 = 57414;
    pub const KP_EQUAL: u32 = 57415;
    pub const KP_SEPARATOR: u32 = 57416;
    pub const KP_LEFT: u32 = 57417;
    pub const KP_RIGHT: u32 = 57418;
    pub const KP_UP: u32 = 57419;
    pub const KP_DOWN: u32 = 57420;
    pub const KP_PAGE_UP: u32 = 57421;
    pub const KP_PAGE_DOWN: u32 = 57422;
    pub const KP_HOME: u32 = 57423;
    pub const KP_END: u32 = 57424;
    pub const KP_INSERT: u32 = 57425;
    pub const KP_DELETE: u32 = 57426;
    pub const KP_BEGIN: u32 = 57427;
    pub const MEDIA_PLAY: u32 = 57428;
    pub const MEDIA_PAUSE: u32 = 57429;
    pub const MEDIA_PLAY_PAUSE: u32 = 57430;
    pub const MEDIA_REVERSE: u32 = 57431;
    pub const MEDIA_STOP: u32 = 57432;
    pub const MEDIA_FAST_FORWARD: u32 = 57433;
    pub const MEDIA_REWIND: u32 = 57434;
    pub const MEDIA_TRACK_NEXT: u32 = 57435;
    pub const MEDIA_TRACK_PREVIOUS: u32 = 57436;
    pub const MEDIA_RECORD: u32 = 57437;
    pub const LOWER_VOLUME: u32 = 57438;
    pub const RAISE_VOLUME: u32 = 57439;
    pub const MUTE_VOLUME: u32 = 57440;
    pub const LEFT_SHIFT: u32 = 57441;
    pub const LEFT_CONTROL: u32 = 57442;
    pub const LEFT_ALT: u32 = 57443;
    pub const LEFT_SUPER: u32 = 57444;
    pub const LEFT_HYPER: u32 = 57445;
    pub const LEFT_META: u32 = 57446;
    pub const RIGHT_SHIFT: u32 = 57447;
    pub const RIGHT_CONTROL: u32 = 57448;
    pub const RIGHT_ALT: u32 = 57449;
    pub const RIGHT_SUPER: u32 = 57450;
    pub const RIGHT_HYPER: u32 = 57451;
    pub const RIGHT_META: u32 = 57452;
    pub const ISO_LEVEL_3_SHIFT: u32 = 57453;
    pub const ISO_LEVEL_5_SHIFT: u32 = 57454;

    /// Loose key matching. Returns true when any of three rules holds, after
    /// removing caps_lock and num_lock:
    ///
    /// 1. The codepoint and modifiers match exactly.
    /// 2. The UTF-8 encoding of `cp` matches `text` (also removing shift).
    /// 3. There is a shifted codepoint that matches after removing shift.
    pub fn matches(&self, cp: u32, mods: Modifiers) -> bool {
        self.match_exact(cp, mods)
            || self.match_text(cp, mods)
            || self.match_shifted_codepoint(cp, mods)
    }

    /// True if [`Key::matches`] holds for any of the provided codepoints.
    pub fn matches_any(&self, cps: &[u32], mods: Modifiers) -> bool {
        cps.iter().any(|&cp| self.matches(cp, mods))
    }

    /// Matches base layout codes, useful for shortcut matching when an
    /// alternate keyboard layout is in use.
    pub fn match_shortcut(&self, cp: u32, mods: Modifiers) -> bool {
        let Some(base) = self.base_layout_codepoint else {
            return false;
        };
        cp == base && self.mods.eql(mods)
    }

    /// Matches keys whose shifted form is not just the upper-case version. For
    /// example, shift + semicolon produces a colon, so the key matches against
    /// shift + semicolon or just colon.
    pub fn match_shifted_codepoint(&self, cp: u32, mods: Modifiers) -> bool {
        let Some(shifted) = self.shifted_codepoint else {
            return false;
        };
        if !self.mods.contains(Modifiers::SHIFT) {
            return false;
        }
        let self_mods = self.mods - (Modifiers::SHIFT | Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK);
        // NOTE: upstream prepares a caps/num-cleared copy of `mods` here but
        // then compares against the raw `mods`, so the clearing has no effect.
        // We mirror that effective behavior: a target carrying caps_lock or
        // num_lock will fail to match. This is an upstream quirk, reproduced
        // deliberately rather than "fixed".
        cp == shifted && self_mods.eql(mods)
    }

    /// Matches when the UTF-8 encoding of `cp` plus the relevant modifiers
    /// equals `text`. Consumes shift and caps_lock (and num_lock) when matching.
    pub fn match_text(&self, cp: u32, mods: Modifiers) -> bool {
        let text = match &self.text {
            Some(t) if !t.is_empty() => t,
            _ => return false,
        };

        let self_mods = self.mods - (Modifiers::NUM_LOCK | Modifiers::SHIFT | Modifiers::CAPS_LOCK);

        // Uppercase ASCII codepoints when shift or caps lock is active. Full
        // Unicode case folding is intentionally not handled, matching upstream.
        let cp = if cp < 128
            && (mods.contains(Modifiers::SHIFT) || mods.contains(Modifiers::CAPS_LOCK))
        {
            let byte = u8::try_from(cp).expect("cp < 128 fits in u8");
            u32::from(byte.to_ascii_uppercase())
        } else {
            cp
        };

        let arg_mods = mods - (Modifiers::NUM_LOCK | Modifiers::SHIFT | Modifiers::CAPS_LOCK);

        // The MULTICODEPOINT sentinel and surrogates are not real chars, so
        // they encode to nothing and cannot match any text.
        let Some(ch) = char::from_u32(cp) else {
            return false;
        };
        let mut buf = [0u8; 4];
        let encoded = ch.encode_utf8(&mut buf);
        text.as_str() == encoded && self_mods.eql(arg_mods)
    }

    /// The key must match the codepoint and modifiers exactly. caps_lock and
    /// num_lock are removed before comparing.
    pub fn match_exact(&self, cp: u32, mods: Modifiers) -> bool {
        let self_mods = self.mods - (Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK);
        let tgt_mods = mods - (Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK);
        self.codepoint == cp && self_mods.eql(tgt_mods)
    }

    /// True if the key is a single modifier (for example left_shift).
    pub fn is_modifier(&self) -> bool {
        let cp = self.codepoint;
        cp == Self::LEFT_SHIFT
            || cp == Self::LEFT_ALT
            || cp == Self::LEFT_SUPER
            || cp == Self::LEFT_HYPER
            || cp == Self::LEFT_CONTROL
            || cp == Self::LEFT_META
            || cp == Self::RIGHT_SHIFT
            || cp == Self::RIGHT_ALT
            || cp == Self::RIGHT_SUPER
            || cp == Self::RIGHT_HYPER
            || cp == Self::RIGHT_CONTROL
            || cp == Self::RIGHT_META
    }
}

/// Maps a key name to its codepoint, mirroring upstream's `name_map`. Covers
/// the common punctuation names plus every special-key constant.
static NAME_MAP: phf::Map<&'static str, u32> = phf_map! {
    // common names
    "plus" => 43,       // '+'
    "minus" => 45,      // '-'
    "colon" => 58,      // ':'
    "semicolon" => 59,  // ';'
    "comma" => 44,      // ','

    // special keys
    "tab" => Key::TAB,
    "enter" => Key::ENTER,
    "escape" => Key::ESCAPE,
    "space" => Key::SPACE,
    "backspace" => Key::BACKSPACE,
    "insert" => Key::INSERT,
    "delete" => Key::DELETE,
    "left" => Key::LEFT,
    "right" => Key::RIGHT,
    "up" => Key::UP,
    "down" => Key::DOWN,
    "page_up" => Key::PAGE_UP,
    "page_down" => Key::PAGE_DOWN,
    "home" => Key::HOME,
    "end" => Key::END,
    "caps_lock" => Key::CAPS_LOCK,
    "scroll_lock" => Key::SCROLL_LOCK,
    "num_lock" => Key::NUM_LOCK,
    "print_screen" => Key::PRINT_SCREEN,
    "pause" => Key::PAUSE,
    "menu" => Key::MENU,
    "f1" => Key::F1,
    "f2" => Key::F2,
    "f3" => Key::F3,
    "f4" => Key::F4,
    "f5" => Key::F5,
    "f6" => Key::F6,
    "f7" => Key::F7,
    "f8" => Key::F8,
    "f9" => Key::F9,
    "f10" => Key::F10,
    "f11" => Key::F11,
    "f12" => Key::F12,
    "f13" => Key::F13,
    "f14" => Key::F14,
    "f15" => Key::F15,
    "f16" => Key::F16,
    "f17" => Key::F17,
    "f18" => Key::F18,
    "f19" => Key::F19,
    "f20" => Key::F20,
    "f21" => Key::F21,
    "f22" => Key::F22,
    "f23" => Key::F23,
    "f24" => Key::F24,
    "f25" => Key::F25,
    "f26" => Key::F26,
    "f27" => Key::F27,
    "f28" => Key::F28,
    "f29" => Key::F29,
    "f30" => Key::F30,
    "f31" => Key::F31,
    "f32" => Key::F32,
    "f33" => Key::F33,
    "f34" => Key::F34,
    "f35" => Key::F35,
    "kp_0" => Key::KP_0,
    "kp_1" => Key::KP_1,
    "kp_2" => Key::KP_2,
    "kp_3" => Key::KP_3,
    "kp_4" => Key::KP_4,
    "kp_5" => Key::KP_5,
    "kp_6" => Key::KP_6,
    "kp_7" => Key::KP_7,
    "kp_8" => Key::KP_8,
    "kp_9" => Key::KP_9,
    "kp_decimal" => Key::KP_DECIMAL,
    "kp_divide" => Key::KP_DIVIDE,
    "kp_multiply" => Key::KP_MULTIPLY,
    "kp_subtract" => Key::KP_SUBTRACT,
    "kp_add" => Key::KP_ADD,
    "kp_enter" => Key::KP_ENTER,
    "kp_equal" => Key::KP_EQUAL,
    "kp_separator" => Key::KP_SEPARATOR,
    "kp_left" => Key::KP_LEFT,
    "kp_right" => Key::KP_RIGHT,
    "kp_up" => Key::KP_UP,
    "kp_down" => Key::KP_DOWN,
    "kp_page_up" => Key::KP_PAGE_UP,
    "kp_page_down" => Key::KP_PAGE_DOWN,
    "kp_home" => Key::KP_HOME,
    "kp_end" => Key::KP_END,
    "kp_insert" => Key::KP_INSERT,
    "kp_delete" => Key::KP_DELETE,
    "kp_begin" => Key::KP_BEGIN,
    "media_play" => Key::MEDIA_PLAY,
    "media_pause" => Key::MEDIA_PAUSE,
    "media_play_pause" => Key::MEDIA_PLAY_PAUSE,
    "media_reverse" => Key::MEDIA_REVERSE,
    "media_stop" => Key::MEDIA_STOP,
    "media_fast_forward" => Key::MEDIA_FAST_FORWARD,
    "media_rewind" => Key::MEDIA_REWIND,
    "media_track_next" => Key::MEDIA_TRACK_NEXT,
    "media_track_previous" => Key::MEDIA_TRACK_PREVIOUS,
    "media_record" => Key::MEDIA_RECORD,
    "lower_volume" => Key::LOWER_VOLUME,
    "raise_volume" => Key::RAISE_VOLUME,
    "mute_volume" => Key::MUTE_VOLUME,
    "left_shift" => Key::LEFT_SHIFT,
    "left_control" => Key::LEFT_CONTROL,
    "left_alt" => Key::LEFT_ALT,
    "left_super" => Key::LEFT_SUPER,
    "left_hyper" => Key::LEFT_HYPER,
    "left_meta" => Key::LEFT_META,
    "right_shift" => Key::RIGHT_SHIFT,
    "right_control" => Key::RIGHT_CONTROL,
    "right_alt" => Key::RIGHT_ALT,
    "right_super" => Key::RIGHT_SUPER,
    "right_hyper" => Key::RIGHT_HYPER,
    "right_meta" => Key::RIGHT_META,
    "iso_level_3_shift" => Key::ISO_LEVEL_3_SHIFT,
    "iso_level_5_shift" => Key::ISO_LEVEL_5_SHIFT,
};

/// Looks up a key name, returning its codepoint. Mirrors `name_map.get`.
pub fn name_map(name: &str) -> Option<u32> {
    NAME_MAP.get(name).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_a() {
        let key = Key {
            codepoint: u32::from('a'),
            mods: Modifiers::NUM_LOCK,
            text: Some("a".into()),
            ..Default::default()
        };
        assert!(key.matches(u32::from('a'), Modifiers::empty()));
        assert!(!key.matches(u32::from('a'), Modifiers::SHIFT));
    }

    #[test]
    fn matches_shift_a() {
        let key = Key {
            codepoint: u32::from('a'),
            shifted_codepoint: Some(u32::from('A')),
            mods: Modifiers::SHIFT,
            text: Some("A".into()),
            ..Default::default()
        };
        assert!(key.matches(u32::from('a'), Modifiers::SHIFT));
        assert!(!key.matches(u32::from('a'), Modifiers::empty()));
        assert!(key.matches(u32::from('A'), Modifiers::empty()));
        assert!(!key.matches(u32::from('A'), Modifiers::CTRL));
    }

    #[test]
    fn matches_shift_tab() {
        let key = Key {
            codepoint: Key::TAB,
            mods: Modifiers::SHIFT | Modifiers::NUM_LOCK,
            ..Default::default()
        };
        assert!(key.matches(Key::TAB, Modifiers::SHIFT));
        assert!(!key.matches(Key::TAB, Modifiers::empty()));
    }

    #[test]
    fn matches_shift_semicolon() {
        let key = Key {
            codepoint: u32::from(';'),
            shifted_codepoint: Some(u32::from(':')),
            mods: Modifiers::SHIFT,
            text: Some(":".into()),
            ..Default::default()
        };
        assert!(key.matches(u32::from(';'), Modifiers::SHIFT));
        assert!(key.matches(u32::from(':'), Modifiers::empty()));

        let colon = Key {
            codepoint: u32::from(':'),
            mods: Modifiers::empty(),
            ..Default::default()
        };
        assert!(colon.matches(u32::from(':'), Modifiers::empty()));
    }

    #[test]
    fn name_map_lookup() {
        assert_eq!(name_map("insert"), Some(Key::INSERT));
    }
}
