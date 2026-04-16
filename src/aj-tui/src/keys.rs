//! Key parsing and input event representation.
//!
//! Provides types for representing keyboard input events and an `InputEvent`
//! enum that components receive via `handle_input`.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// An input event delivered to components.
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// A keyboard event (key press with optional modifiers).
    Key(KeyEvent),
    /// Pasted text (from bracketed paste mode).
    Paste(String),
    /// Terminal was resized to (columns, rows).
    Resize(u16, u16),
}

impl InputEvent {
    /// Returns true if this event matches the given key code with no modifiers.
    pub fn is_key(&self, code: KeyCode) -> bool {
        matches!(self, InputEvent::Key(k) if k.code == code && k.modifiers == KeyModifiers::NONE)
    }

    /// Returns true if this event matches the given key code with Ctrl held.
    pub fn is_ctrl(&self, c: char) -> bool {
        matches!(self, InputEvent::Key(k)
            if k.code == KeyCode::Char(c)
            && k.modifiers.contains(KeyModifiers::CONTROL))
    }

    /// Returns true if this event matches the given key code with Alt held.
    pub fn is_alt(&self, c: char) -> bool {
        matches!(self, InputEvent::Key(k)
            if k.code == KeyCode::Char(c)
            && k.modifiers.contains(KeyModifiers::ALT))
    }

    /// Returns true if this event matches the given key code with Shift held.
    pub fn is_shift_key(&self, code: KeyCode) -> bool {
        matches!(self, InputEvent::Key(k)
            if k.code == code
            && k.modifiers.contains(KeyModifiers::SHIFT))
    }

    /// Returns true if this is a printable character with no modifiers
    /// (or only Shift, which produces uppercase/symbols).
    pub fn as_char(&self) -> Option<char> {
        match self {
            InputEvent::Key(k) => {
                let mods_without_shift = k.modifiers - KeyModifiers::SHIFT;
                if mods_without_shift.is_empty() {
                    if let KeyCode::Char(c) = k.code {
                        return Some(c);
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Returns true if this event is a key-release event (kitty protocol).
    ///
    /// `Tui::handle_input` uses this to gate dispatch: releases are
    /// filtered out for components that don't set
    /// [`Component::wants_key_release`][crate::component::Component::wants_key_release].
    pub fn is_key_release(&self) -> bool {
        matches!(self, InputEvent::Key(k) if k.kind == KeyEventKind::Release)
    }

    /// Returns true if this event is a key-repeat event (kitty protocol).
    ///
    /// Repeats are delivered alongside presses; this helper exists so
    /// components that care (e.g. rate-limiters on hold-to-repeat
    /// actions) can opt out without looking at the underlying
    /// `KeyEventKind` themselves.
    pub fn is_key_repeat(&self) -> bool {
        matches!(self, InputEvent::Key(k) if k.kind == KeyEventKind::Repeat)
    }
}

/// Convert a crossterm `Event` into an `InputEvent`.
///
/// Byte sequences that crossterm doesn't recognize as a typed event —
/// most notably terminal cell-size replies (`\x1b[6;<h>;<w>t`, sent
/// in response to a `\x1b[16t` query) — never produce an [`Event`]
/// at all. `Parser::advance` clears its accumulator on `Err` from
/// the CSI parse, so they're consumed at the byte layer.
///
/// This is the structural counterpart to pi-tui's explicit
/// `consumeCellSizeResponse` step in `TUI.handleInput`. Pi
/// pattern-matches against raw bytes at the typed-input layer; we
/// can't, because bytes are already parsed by the time they'd reach
/// here. We rely on crossterm's parser doing the equivalent drop one
/// layer up. We don't currently send `\x1b[16t` (no image support),
/// so the only source of these replies is environmental (e.g. tmux
/// passthrough), and we have no `setCellDimensions` side-effect to
/// preserve from pi. See PORTING.md E5 for the byte-level trace and
/// the partial-reply edge note (where both pi and crossterm degrade
/// identically).
impl TryFrom<Event> for InputEvent {
    type Error = ();

    fn try_from(event: Event) -> Result<Self, ()> {
        match event {
            // Preserve Press, Repeat, and Release. Press and Repeat flow
            // through as regular key events; Release events are filtered
            // by `Tui::handle_input` unless the receiving component
            // opts in via `Component::wants_key_release`.
            Event::Key(key) => Ok(InputEvent::Key(key)),
            Event::Paste(text) => Ok(InputEvent::Paste(text)),
            Event::Resize(cols, rows) => Ok(InputEvent::Resize(cols, rows)),
            _ => Err(()),
        }
    }
}

/// Returns `true` when `event` is a "byte-form newline" — an
/// Enter-equivalent encoding that mirrors pi-tui's hand-rolled
/// byte-level fallbacks against `\n`, `\x1b\r`, and (in principle)
/// `\x1b[13;2~`.
///
/// Used by components (the [`Editor`]'s newline branch is the
/// canonical caller) so terminals that deliver Shift/Alt+Enter via
/// a byte sequence the keybindings registry doesn't recognize still
/// trigger newline insertion. Fires *regardless* of `disable_submit`
/// to match pi-tui's structure: the byte fallbacks are part of the
/// newline branch and bypass the submit branch's `disableSubmit`
/// gate.
///
/// **What this recognizes (precisely; modifier set must match
/// exactly to avoid shadowing user-customized bindings):**
///
/// - [`KeyCode::Char('\n')`] with no modifiers — a raw LF byte that
///   crossterm hands off as a `Char` (test harnesses, piped input,
///   terminals that haven't entered raw mode).
/// - [`KeyCode::Char('j')`] with exactly `CONTROL` — crossterm's
///   parsing of a raw LF byte (0x0A) under raw mode. Historically
///   `Ctrl+J == LF`, and pi-tui's `data === "\n"` fallback fires on
///   the same byte. Yes, this means a user pressing Ctrl+J (alone)
///   inserts a newline; that is parity with pi-tui's design and
///   pi-tui ships no other default binding for Ctrl+J. Combined
///   modifiers like `Ctrl+Shift+J` do *not* match — those are real
///   distinct keystrokes.
/// - [`KeyCode::Enter`] with exactly `ALT` — the parsed form of
///   `\x1b\r`. Pi-tui catches this as `data === "\x1b\r"`.
///
/// **Plain `KeyCode::Enter` and `KeyCode::Enter + Ctrl` /
/// `+ Shift` are intentionally not recognized.** Plain Enter is
/// the user's submit key. Shift+Enter is the registry-bound
/// `tui.input.newLine` default — `kb.matches` catches it before
/// any byte-form fallback. Ctrl+Enter has no pi-tui byte fallback,
/// and a user might rebind `tui.input.submit` to it, so the helper
/// must not shadow that.
///
/// Crossterm 0.28 does not parse the `\x1b[13;2~` xterm Shift+Enter
/// encoding — those bytes are dropped before they can reach a
/// component. That gap can't be closed at this layer; revisit when
/// crossterm grows the surface.
pub fn is_newline_event(event: &InputEvent) -> bool {
    let InputEvent::Key(key) = event else {
        return false;
    };
    match key.code {
        KeyCode::Char('\n') if key.modifiers == KeyModifiers::NONE => true,
        KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => true,
        KeyCode::Enter if key.modifiers == KeyModifiers::ALT => true,
        _ => false,
    }
}

/// Convenience constructors for common key events (useful in tests and keybindings).
pub struct Key;

impl Key {
    pub fn char(c: char) -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    pub fn ctrl(c: char) -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    pub fn alt(c: char) -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT))
    }

    pub fn enter() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    pub fn shift_enter() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))
    }

    pub fn escape() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
    }

    pub fn tab() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
    }

    pub fn backtab() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
    }

    pub fn backspace() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
    }

    pub fn alt_backspace() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT))
    }

    pub fn delete() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE))
    }

    pub fn up() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
    }

    pub fn down() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
    }

    pub fn left() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
    }

    pub fn right() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
    }

    pub fn ctrl_left() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL))
    }

    pub fn ctrl_right() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL))
    }

    pub fn alt_left() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT))
    }

    pub fn alt_right() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT))
    }

    pub fn home() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE))
    }

    pub fn end() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
    }

    pub fn page_up() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
    }

    pub fn page_down() -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
    }

    pub fn f(n: u8) -> InputEvent {
        InputEvent::Key(KeyEvent::new(KeyCode::F(n), KeyModifiers::NONE))
    }
}

// ---------------------------------------------------------------------------
// Key ID matching
// ---------------------------------------------------------------------------

/// Check whether `event` matches the string keybinding descriptor `key_id`.
///
/// The descriptor grammar is `modifier+...+modifier+key`, lowercase, with
/// modifiers drawn from `ctrl`, `alt`, `shift`, and `super`, and with the
/// key being a single character, a named key (`enter`, `escape`, `tab`,
/// `backspace`, `delete`, `insert`, `up`, `down`, `left`, `right`,
/// `home`, `end`, `pageUp` / `pageup`, `pageDown` / `pagedown`, `space`),
/// or a function key like `f1` through `f24`.
///
/// Unknown descriptors return `false` rather than panicking.
pub fn key_id_matches(event: &InputEvent, key_id: &str) -> bool {
    let Some(parsed) = parse_key_id(key_id) else {
        return false;
    };
    let InputEvent::Key(key) = event else {
        return false;
    };
    if key.code != parsed.code {
        return false;
    }
    // For printable chars, Shift is folded into the character case by the
    // terminal; ignore it when the descriptor did not explicitly mention
    // shift. For non-character codes like `enter` or `tab`, Shift is an
    // observable modifier that must match exactly.
    let effective_mods = if matches!(parsed.code, KeyCode::Char(_)) && !parsed.shift_explicit {
        key.modifiers - KeyModifiers::SHIFT
    } else {
        key.modifiers
    };
    effective_mods == parsed.modifiers
}

/// Produce a display-side descriptor string for a [`KeyEvent`] that
/// round-trips through [`key_id_matches`].
///
/// The mirror of [`key_id_matches`]: given a typed [`InputEvent`],
/// return the canonical string form (`"ctrl+c"`, `"shift+enter"`,
/// `"f5"`, `"a"`, …). Useful for:
///
/// - Displaying key bindings in help text or settings UI.
/// - Debug logging that wants to show what key the user pressed
///   using the same syntax [`key_id_matches`] accepts.
///
/// Returns `None` when the event isn't representable as a descriptor
/// (non-[`InputEvent::Key`] variants, unrepresentable key codes like
/// [`KeyCode::Media`], function keys outside `F1..=F24`, or a
/// `Char` code whose character is neither a printable ASCII nor a
/// named key).
///
/// Modifier ordering is fixed to `shift+ctrl+alt+super+<key>` for
/// display consistency; parser-side ([`key_id_matches`]) accepts
/// modifiers in any order.
///
/// For printable characters, Shift is folded into the character
/// itself by most terminals (`Char('A')` with no modifier), so the
/// descriptor does not emit a standalone `shift+` prefix for a
/// letter. Kitty-protocol terminals that report `Char('a')` with
/// `SHIFT` set also produce `"a"` here — the two encodings compare
/// equal under [`key_id_matches`] without `shift_explicit`.
pub fn format_key_descriptor(event: &InputEvent) -> Option<String> {
    let InputEvent::Key(key) = event else {
        return None;
    };

    let key_name = match key.code {
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "escape".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pageUp".to_string(),
        KeyCode::PageDown => "pageDown".to_string(),
        KeyCode::F(n) if (1..=24).contains(&n) => format!("f{n}"),
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(c) if c.is_ascii_graphic() => {
            // Fold shifted letters into the lowercase form so the
            // emitted descriptor round-trips: `Char('A')` prints as
            // `"a"` rather than a bare `"A"` (which `parse_key_id`
            // would coerce to `"a"` via `to_ascii_lowercase`
            // anyway, but the display side should match the canonical
            // lowercase convention).
            c.to_ascii_lowercase().to_string()
        }
        _ => return None,
    };

    // Modifier suppression rules:
    // - For a printable character (`KeyCode::Char` other than space),
    //   drop the SHIFT bit: terminals fold Shift into the character's
    //   case or symbol, and the descriptor convention is `"a"` not
    //   `"shift+a"` (cf. `key_id_matches`' `shift_explicit` handling).
    // - Space (`"space"`) is a named key, so shift is meaningful if
    //   present and we keep it.
    // - All named keys (enter, tab, arrows, function keys, …)
    //   preserve shift.
    let mut mods = key.modifiers;
    if let KeyCode::Char(c) = key.code {
        if c != ' ' {
            mods -= KeyModifiers::SHIFT;
        }
    }

    let mut parts = Vec::with_capacity(5);
    if mods.contains(KeyModifiers::SHIFT) {
        parts.push("shift");
    }
    if mods.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl");
    }
    if mods.contains(KeyModifiers::ALT) {
        parts.push("alt");
    }
    if mods.contains(KeyModifiers::SUPER) {
        parts.push("super");
    }

    if parts.is_empty() {
        Some(key_name)
    } else {
        Some(format!("{}+{}", parts.join("+"), key_name))
    }
}

struct ParsedKeyId {
    code: KeyCode,
    modifiers: KeyModifiers,
    /// Whether `shift` appeared explicitly in the descriptor. For
    /// character keys, shift is otherwise ignored when comparing.
    shift_explicit: bool,
}

fn parse_key_id(input: &str) -> Option<ParsedKeyId> {
    if input.is_empty() {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    let mut shift_explicit = false;

    // Split on `+`, but allow a trailing literal `+` key (e.g. `ctrl++`).
    // We do this by splitting once per modifier prefix. The tail is the
    // key name.
    let lower = input.to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('+').collect();
    if parts.iter().any(|s| s.is_empty()) {
        // An empty segment can only come from a literal `+` key; rebuild.
        // e.g. `ctrl++` -> ["ctrl", "", ""]; key is "+", the rest are
        // modifiers.
        parts.retain(|s| !s.is_empty());
        for modifier in &parts {
            match *modifier {
                "ctrl" => modifiers |= KeyModifiers::CONTROL,
                "alt" => modifiers |= KeyModifiers::ALT,
                "shift" => {
                    modifiers |= KeyModifiers::SHIFT;
                    shift_explicit = true;
                }
                "super" => modifiers |= KeyModifiers::SUPER,
                _ => return None,
            }
        }
        return Some(ParsedKeyId {
            code: KeyCode::Char('+'),
            modifiers,
            shift_explicit,
        });
    }

    let key = parts.pop()?;
    for modifier in &parts {
        match *modifier {
            "ctrl" => modifiers |= KeyModifiers::CONTROL,
            "alt" => modifiers |= KeyModifiers::ALT,
            "shift" => {
                modifiers |= KeyModifiers::SHIFT;
                shift_explicit = true;
            }
            "super" => modifiers |= KeyModifiers::SUPER,
            _ => return None,
        }
    }

    let code = match key {
        "enter" | "return" | "ret" => KeyCode::Enter,
        "escape" | "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        k if k.starts_with('f') && k[1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u8 = k[1..].parse().ok()?;
            if n == 0 {
                return None;
            }
            KeyCode::F(n)
        }
        k => {
            let mut chars = k.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(ch)
        }
    };

    Some(ParsedKeyId {
        code,
        modifiers,
        shift_explicit,
    })
}

#[cfg(test)]
mod key_id_tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> InputEvent {
        InputEvent::Key(KeyEvent::new(code, mods))
    }

    #[test]
    fn plain_character_matches() {
        assert!(key_id_matches(
            &key(KeyCode::Char('a'), KeyModifiers::NONE),
            "a"
        ));
        assert!(!key_id_matches(
            &key(KeyCode::Char('b'), KeyModifiers::NONE),
            "a"
        ));
    }

    #[test]
    fn ctrl_modifier_matches() {
        assert!(key_id_matches(
            &key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            "ctrl+c"
        ));
        assert!(!key_id_matches(
            &key(KeyCode::Char('c'), KeyModifiers::NONE),
            "ctrl+c"
        ));
    }

    #[test]
    fn alt_modifier_matches() {
        assert!(key_id_matches(
            &key(KeyCode::Left, KeyModifiers::ALT),
            "alt+left"
        ));
    }

    #[test]
    fn named_keys_match() {
        assert!(key_id_matches(
            &key(KeyCode::Enter, KeyModifiers::NONE),
            "enter"
        ));
        assert!(key_id_matches(
            &key(KeyCode::PageUp, KeyModifiers::NONE),
            "pageUp"
        ));
        assert!(key_id_matches(
            &key(KeyCode::Enter, KeyModifiers::SHIFT),
            "shift+enter"
        ));
        assert!(!key_id_matches(
            &key(KeyCode::Enter, KeyModifiers::NONE),
            "shift+enter"
        ));
    }

    #[test]
    fn function_keys_match() {
        assert!(key_id_matches(
            &key(KeyCode::F(5), KeyModifiers::NONE),
            "f5"
        ));
        assert!(!key_id_matches(
            &key(KeyCode::F(4), KeyModifiers::NONE),
            "f5"
        ));
    }

    #[test]
    fn shift_is_ignored_for_printable_chars_unless_explicit() {
        // Terminals fold shift into the character: "A" arrives as
        // Char('A') with modifiers NONE on most terminals, or with
        // modifiers SHIFT on Kitty. The descriptor `"a"` should match
        // both shapes; `"shift+a"` should match only the explicit one.
        assert!(key_id_matches(
            &key(KeyCode::Char('a'), KeyModifiers::SHIFT),
            "a"
        ));
        assert!(key_id_matches(
            &key(KeyCode::Char('a'), KeyModifiers::SHIFT),
            "shift+a"
        ));
    }

    #[test]
    fn unknown_descriptors_return_false() {
        assert!(!key_id_matches(
            &key(KeyCode::Char('a'), KeyModifiers::NONE),
            "bogus+a"
        ));
        assert!(!key_id_matches(
            &key(KeyCode::Char('a'), KeyModifiers::NONE),
            ""
        ));
    }

    #[test]
    fn literal_plus_key() {
        assert!(key_id_matches(
            &key(KeyCode::Char('+'), KeyModifiers::NONE),
            "+"
        ));
        assert!(key_id_matches(
            &key(KeyCode::Char('+'), KeyModifiers::CONTROL),
            "ctrl++"
        ));
    }

    // -- format_key_descriptor --

    #[test]
    fn format_descriptor_plain_character() {
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Char('a'), KeyModifiers::NONE)).as_deref(),
            Some("a"),
        );
    }

    #[test]
    fn format_descriptor_uppercase_char_folds_to_lowercase() {
        // `Char('A')` reaches us from most terminals with no modifier
        // (shift folded into the case). Descriptor is canonical `"a"`.
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Char('A'), KeyModifiers::NONE)).as_deref(),
            Some("a"),
        );
    }

    #[test]
    fn format_descriptor_char_drops_shift_modifier() {
        // Kitty-protocol terminals report shifted letters as
        // `Char('a')` with `SHIFT` set. Descriptor should suppress
        // the redundant shift prefix.
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Char('a'), KeyModifiers::SHIFT)).as_deref(),
            Some("a"),
        );
    }

    #[test]
    fn format_descriptor_ctrl_modifier() {
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)).as_deref(),
            Some("ctrl+c"),
        );
    }

    #[test]
    fn format_descriptor_alt_arrow() {
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Left, KeyModifiers::ALT)).as_deref(),
            Some("alt+left"),
        );
    }

    #[test]
    fn format_descriptor_named_keys() {
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Enter, KeyModifiers::NONE)).as_deref(),
            Some("enter"),
        );
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Enter, KeyModifiers::SHIFT)).as_deref(),
            Some("shift+enter"),
        );
        assert_eq!(
            format_key_descriptor(&key(KeyCode::PageUp, KeyModifiers::NONE)).as_deref(),
            Some("pageUp"),
        );
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Esc, KeyModifiers::NONE)).as_deref(),
            Some("escape"),
        );
    }

    #[test]
    fn format_descriptor_space_preserves_shift() {
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Char(' '), KeyModifiers::NONE)).as_deref(),
            Some("space"),
        );
        // `shift+space` is a real binding on Kitty terminals.
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Char(' '), KeyModifiers::SHIFT)).as_deref(),
            Some("shift+space"),
        );
    }

    #[test]
    fn format_descriptor_function_key() {
        assert_eq!(
            format_key_descriptor(&key(KeyCode::F(5), KeyModifiers::NONE)).as_deref(),
            Some("f5"),
        );
        assert_eq!(
            format_key_descriptor(&key(KeyCode::F(12), KeyModifiers::CONTROL)).as_deref(),
            Some("ctrl+f12"),
        );
    }

    #[test]
    fn format_descriptor_modifier_order_is_shift_ctrl_alt_super() {
        // All four modifiers on an arrow key; output must be exactly
        // `shift+ctrl+alt+super+up`.
        let mods =
            KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER;
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Up, mods)).as_deref(),
            Some("shift+ctrl+alt+super+up"),
        );
    }

    #[test]
    fn format_descriptor_literal_plus_key() {
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Char('+'), KeyModifiers::NONE)).as_deref(),
            Some("+"),
        );
        assert_eq!(
            format_key_descriptor(&key(KeyCode::Char('+'), KeyModifiers::CONTROL)).as_deref(),
            Some("ctrl++"),
        );
    }

    #[test]
    fn format_descriptor_returns_none_for_non_key_events() {
        assert!(format_key_descriptor(&InputEvent::Resize(80, 24)).is_none());
        assert!(format_key_descriptor(&InputEvent::Paste("x".to_string())).is_none());
    }

    #[test]
    fn format_descriptor_returns_none_for_unrepresentable_codes() {
        // F25 and above are outside the descriptor grammar.
        assert!(format_key_descriptor(&key(KeyCode::F(25), KeyModifiers::NONE)).is_none());
        // Non-ASCII-graphic chars (e.g. CJK) are not representable in
        // the descriptor string — `parse_key_id` wouldn't accept them
        // either.
        assert!(format_key_descriptor(&key(KeyCode::Char('中'), KeyModifiers::NONE)).is_none());
    }

    #[test]
    fn format_descriptor_round_trips_through_key_id_matches() {
        // Every descriptor produced by `format_key_descriptor` must
        // compare equal to its source event under `key_id_matches`.
        // This is the explicit round-trip contract.
        let cases = [
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            key(KeyCode::Char(' '), KeyModifiers::SHIFT),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::SHIFT),
            key(KeyCode::Left, KeyModifiers::ALT),
            key(KeyCode::F(5), KeyModifiers::NONE),
            key(KeyCode::F(12), KeyModifiers::CONTROL),
            key(KeyCode::Char('+'), KeyModifiers::CONTROL),
            key(
                KeyCode::Up,
                KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        ];
        for event in cases {
            let descriptor =
                format_key_descriptor(&event).unwrap_or_else(|| panic!("descriptor for {event:?}"));
            assert!(
                key_id_matches(&event, &descriptor),
                "descriptor {descriptor:?} must match event {event:?}",
            );
        }
    }

    #[test]
    fn is_newline_event_recognizes_pi_tui_byte_form_fallbacks() {
        // `KeyCode::Char('\n')` with no modifiers — crossterm's
        // parsed form of a raw LF byte from non-raw-mode input.
        assert!(is_newline_event(&key(
            KeyCode::Char('\n'),
            KeyModifiers::NONE
        )));

        // Exactly `Ctrl+J` — crossterm's parsed form of raw LF in
        // raw mode. Combined modifiers (Ctrl+Shift+J etc.) do NOT
        // match.
        assert!(is_newline_event(&key(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL
        )));

        // Exactly `Alt+Enter` — the parsed form of `\x1b\r`.
        assert!(is_newline_event(&key(KeyCode::Enter, KeyModifiers::ALT)));
    }

    #[test]
    fn is_newline_event_excludes_keys_that_are_or_might_be_user_bindings() {
        // Plain Enter is `tui.input.submit` by default.
        assert!(!is_newline_event(&key(KeyCode::Enter, KeyModifiers::NONE)));

        // Shift+Enter is `tui.input.newLine` by default — the registry
        // catches it via `kb.matches`. The fallback doesn't need to
        // (and shouldn't, in case the user rebinds it).
        assert!(!is_newline_event(&key(KeyCode::Enter, KeyModifiers::SHIFT)));

        // Ctrl+Enter has no pi-tui byte fallback. Some users rebind
        // `tui.input.submit` to it; the fallback must not shadow that.
        assert!(!is_newline_event(&key(
            KeyCode::Enter,
            KeyModifiers::CONTROL
        )));

        // Combined modifiers — none have pi-tui fallbacks.
        assert!(!is_newline_event(&key(
            KeyCode::Enter,
            KeyModifiers::SHIFT | KeyModifiers::ALT
        )));
        assert!(!is_newline_event(&key(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        )));
        assert!(!is_newline_event(&key(
            KeyCode::Char('\n'),
            KeyModifiers::SHIFT
        )));
    }

    #[test]
    fn is_newline_event_rejects_unrelated_keys() {
        // Plain `j` (no modifier) — must not match. Only
        // exactly Ctrl+J is treated as raw LF.
        assert!(!is_newline_event(&key(
            KeyCode::Char('j'),
            KeyModifiers::NONE
        )));
        assert!(!is_newline_event(&key(
            KeyCode::Char('a'),
            KeyModifiers::NONE
        )));
        assert!(!is_newline_event(&key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_newline_event(&key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!is_newline_event(&key(KeyCode::Tab, KeyModifiers::NONE)));

        // Non-key events: Paste / Resize must always return false.
        assert!(!is_newline_event(&InputEvent::Paste("\n".to_string())));
        assert!(!is_newline_event(&InputEvent::Resize(80, 24)));
    }
}
