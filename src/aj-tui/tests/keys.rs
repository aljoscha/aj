//! Tests for `keys.rs`'s convenience surface: the `Key::*` constructors
//! and the `InputEvent::is_*` / `as_char` predicates.
//!
//! The low-level byte-parsing done by crossterm for a real terminal
//! isn't in scope here — we delegate that to crossterm at the
//! `ProcessTerminal` boundary. These tests cover the Rust layer that
//! components actually branch on.

use aj_tui::keys::{InputEvent, Key};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

// ---------------------------------------------------------------------------
// Key::* constructors
// ---------------------------------------------------------------------------

#[test]
fn char_constructor_produces_no_modifier_char_event() {
    let e = Key::char('a');
    assert!(
        matches!(
            e,
            InputEvent::Key(KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: KeyModifiers::NONE,
                ..
            })
        ),
        "got {:?}",
        e,
    );
}

#[test]
fn ctrl_and_alt_constructors_set_the_respective_modifier() {
    assert!(matches!(
        Key::ctrl('c'),
        InputEvent::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    ));
    assert!(matches!(
        Key::alt('b'),
        InputEvent::Key(KeyEvent {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::ALT,
            ..
        })
    ));
}

#[test]
fn enter_and_shift_enter_constructors() {
    assert!(matches!(
        Key::enter(),
        InputEvent::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            ..
        })
    ));
    assert!(matches!(
        Key::shift_enter(),
        InputEvent::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::SHIFT,
            ..
        })
    ));
}

#[test]
fn named_key_constructors_cover_common_cases() {
    let cases: Vec<(InputEvent, KeyCode)> = vec![
        (Key::escape(), KeyCode::Esc),
        (Key::tab(), KeyCode::Tab),
        (Key::backspace(), KeyCode::Backspace),
        (Key::delete(), KeyCode::Delete),
        (Key::up(), KeyCode::Up),
        (Key::down(), KeyCode::Down),
        (Key::left(), KeyCode::Left),
        (Key::right(), KeyCode::Right),
        (Key::home(), KeyCode::Home),
        (Key::end(), KeyCode::End),
        (Key::page_up(), KeyCode::PageUp),
        (Key::page_down(), KeyCode::PageDown),
    ];

    for (event, expected) in cases {
        match event {
            InputEvent::Key(k) => {
                assert_eq!(k.code, expected);
                assert_eq!(k.modifiers, KeyModifiers::NONE);
            }
            other => panic!("expected Key event, got {:?}", other),
        }
    }
}

#[test]
fn backtab_carries_shift() {
    let e = Key::backtab();
    assert!(matches!(
        e,
        InputEvent::Key(KeyEvent {
            code: KeyCode::BackTab,
            modifiers: KeyModifiers::SHIFT,
            ..
        })
    ));
}

#[test]
fn function_key_constructor() {
    for n in 1u8..=12 {
        match Key::f(n) {
            InputEvent::Key(k) => assert_eq!(k.code, KeyCode::F(n)),
            other => panic!("expected Key, got {:?}", other),
        }
    }
}

// ---------------------------------------------------------------------------
// InputEvent predicates
// ---------------------------------------------------------------------------

#[test]
fn is_key_matches_only_the_exact_code_with_no_modifiers() {
    assert!(Key::enter().is_key(KeyCode::Enter));
    assert!(!Key::shift_enter().is_key(KeyCode::Enter));
    assert!(!Key::char('a').is_key(KeyCode::Char('b')));
}

#[test]
fn is_ctrl_matches_only_with_control_modifier() {
    assert!(Key::ctrl('c').is_ctrl('c'));
    assert!(!Key::char('c').is_ctrl('c'));
    assert!(!Key::alt('c').is_ctrl('c'));
}

#[test]
fn is_alt_matches_only_with_alt_modifier() {
    assert!(Key::alt('b').is_alt('b'));
    assert!(!Key::char('b').is_alt('b'));
    assert!(!Key::ctrl('b').is_alt('b'));
}

#[test]
fn is_shift_key_matches_only_with_shift_modifier() {
    assert!(Key::shift_enter().is_shift_key(KeyCode::Enter));
    assert!(!Key::enter().is_shift_key(KeyCode::Enter));
}

#[test]
fn as_char_returns_the_character_when_printable_without_modifiers() {
    assert_eq!(Key::char('x').as_char(), Some('x'));
    assert_eq!(Key::char('ñ').as_char(), Some('ñ'));
    assert_eq!(Key::enter().as_char(), None);
    assert_eq!(Key::ctrl('c').as_char(), None);
    assert_eq!(Key::alt('a').as_char(), None);
}

#[test]
fn as_char_ignores_shift_modifier() {
    // Shift is folded into the character itself by the terminal; a shift
    // modifier alone should still produce a printable char.
    let shifted = InputEvent::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
    assert_eq!(shifted.as_char(), Some('A'));
}

// ---------------------------------------------------------------------------
// Paste and resize events
// ---------------------------------------------------------------------------

#[test]
fn paste_event_flows_through_as_paste() {
    let p = InputEvent::Paste("hello world".to_string());
    assert!(matches!(&p, InputEvent::Paste(s) if s == "hello world"));
    // Paste events are not keys.
    assert_eq!(p.as_char(), None);
    assert!(!p.is_key(KeyCode::Char('h')));
}

#[test]
fn resize_event_flows_through_as_resize() {
    let r = InputEvent::Resize(80, 24);
    match r {
        InputEvent::Resize(cols, rows) => {
            assert_eq!(cols, 80);
            assert_eq!(rows, 24);
        }
        other => panic!("expected Resize, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Crossterm event conversion
// ---------------------------------------------------------------------------

#[test]
fn crossterm_press_events_convert_cleanly() {
    use crossterm::event::{Event, KeyEventKind, KeyEventState};

    let press = Event::Key(KeyEvent {
        code: KeyCode::Char('a'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
    let event: InputEvent = press.try_into().expect("press converts");
    assert_eq!(event.as_char(), Some('a'));
}

#[test]
fn crossterm_release_and_repeat_events_convert_to_typed_events() {
    // Both Release and Repeat kinds flow through the conversion so the
    // Tui can see them and apply the per-component gate. Release events
    // are filtered by `Tui::handle_input` unless the focused component
    // sets `wants_key_release`; Repeat events are always delivered.
    use crossterm::event::{Event, KeyEventKind, KeyEventState};

    let release = Event::Key(KeyEvent {
        code: KeyCode::Char('a'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Release,
        state: KeyEventState::NONE,
    });
    let converted: InputEvent = release.try_into().expect("release converts");
    assert!(converted.is_key_release());
    assert!(!converted.is_key_repeat());

    let repeat = Event::Key(KeyEvent {
        code: KeyCode::Char('a'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Repeat,
        state: KeyEventState::NONE,
    });
    let converted: InputEvent = repeat.try_into().expect("repeat converts");
    assert!(converted.is_key_repeat());
    assert!(!converted.is_key_release());
}

#[test]
fn crossterm_paste_and_resize_convert_cleanly() {
    use crossterm::event::Event;

    let paste = Event::Paste("hi".to_string());
    let event: InputEvent = paste.try_into().expect("paste converts");
    assert!(matches!(event, InputEvent::Paste(ref s) if s == "hi"));

    let resize = Event::Resize(100, 30);
    let event: InputEvent = resize.try_into().expect("resize converts");
    assert!(matches!(event, InputEvent::Resize(100, 30)));
}

// ---------------------------------------------------------------------------
// key_id_matches: descriptor grammar
//
// These tests exercise the keybinding-descriptor language that
// components and application code use via `key_id_matches(&event, "…")`.
// Raw byte-stream parsing (e.g. matching the Kitty CSI-u sequence
// `"\x1b[1089::99;5u"` as `ctrl+c`) is out of scope: crossterm owns
// byte parsing at the `ProcessTerminal` boundary, so we only test the
// descriptor-matching half. Each case crafts the pre-parsed
// `InputEvent` and asserts the descriptor string matches (or doesn't).
// ---------------------------------------------------------------------------

use aj_tui::keys::key_id_matches;

fn key(code: KeyCode, mods: KeyModifiers) -> InputEvent {
    InputEvent::Key(KeyEvent::new(code, mods))
}

// -- Combined modifiers ------------------------------------------------------

#[test]
fn ctrl_shift_letter_descriptor_requires_both_modifiers() {
    let event = key(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(key_id_matches(&event, "ctrl+shift+p"));
    assert!(
        key_id_matches(&event, "shift+ctrl+p"),
        "modifier order is irrelevant"
    );
    // Missing either modifier rejects the match.
    assert!(!key_id_matches(
        &key(KeyCode::Char('p'), KeyModifiers::CONTROL),
        "ctrl+shift+p",
    ));
    assert!(!key_id_matches(
        &key(KeyCode::Char('p'), KeyModifiers::SHIFT),
        "ctrl+shift+p",
    ));
}

#[test]
fn ctrl_alt_letter_descriptor_requires_both_modifiers() {
    let event = key(
        KeyCode::Char('h'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    );
    assert!(key_id_matches(&event, "ctrl+alt+h"));
    assert!(!key_id_matches(
        &key(KeyCode::Char('h'), KeyModifiers::ALT),
        "ctrl+alt+h",
    ));
}

#[test]
fn super_modifier_stands_alone_and_combines() {
    let super_k = key(KeyCode::Char('k'), KeyModifiers::SUPER);
    assert!(key_id_matches(&super_k, "super+k"));

    let ctrl_super_k = key(
        KeyCode::Char('k'),
        KeyModifiers::CONTROL | KeyModifiers::SUPER,
    );
    assert!(key_id_matches(&ctrl_super_k, "ctrl+super+k"));
    assert!(!key_id_matches(&ctrl_super_k, "super+k"));

    let all_mods_k = key(
        KeyCode::Char('k'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::SUPER,
    );
    assert!(key_id_matches(&all_mods_k, "ctrl+shift+super+k"));
    assert!(key_id_matches(&all_mods_k, "shift+ctrl+super+k"));
}

#[test]
fn super_modifier_with_named_key() {
    let super_enter = key(KeyCode::Enter, KeyModifiers::SUPER);
    assert!(key_id_matches(&super_enter, "super+enter"));
}

// -- Digit descriptors -------------------------------------------------------

#[test]
fn bare_digit_descriptor_matches_char_digit() {
    assert!(key_id_matches(
        &key(KeyCode::Char('1'), KeyModifiers::NONE),
        "1",
    ));
    assert!(!key_id_matches(
        &key(KeyCode::Char('2'), KeyModifiers::NONE),
        "1",
    ));
}

#[test]
fn ctrl_digit_descriptor_matches() {
    assert!(key_id_matches(
        &key(KeyCode::Char('1'), KeyModifiers::CONTROL),
        "ctrl+1",
    ));
    assert!(!key_id_matches(
        &key(KeyCode::Char('1'), KeyModifiers::CONTROL),
        "ctrl+2",
    ));
}

#[test]
fn shift_digit_descriptor_matches_explicit_shift() {
    // Digits are Char, so they follow the printable-shift rule: shift in
    // the descriptor must match shift on the event exactly.
    let shift_1 = key(KeyCode::Char('1'), KeyModifiers::SHIFT);
    assert!(key_id_matches(&shift_1, "shift+1"));
    assert!(
        key_id_matches(&shift_1, "1"),
        "printable shift is permissive"
    );
}

// -- Symbol descriptors ------------------------------------------------------

#[test]
fn ctrl_symbol_descriptors_match() {
    for (ch, descriptor) in [
        ('/', "ctrl+/"),
        ('\\', "ctrl+\\"),
        (']', "ctrl+]"),
        ('[', "ctrl+["),
        ('_', "ctrl+_"),
        ('-', "ctrl+-"),
    ] {
        let event = key(KeyCode::Char(ch), KeyModifiers::CONTROL);
        assert!(
            key_id_matches(&event, descriptor),
            "{:?} should match descriptor {:?}",
            event,
            descriptor,
        );
    }
}

#[test]
fn literal_plus_in_descriptor_is_disambiguated() {
    // `ctrl++` is modifiers=["ctrl"], key="+".
    assert!(key_id_matches(
        &key(KeyCode::Char('+'), KeyModifiers::CONTROL),
        "ctrl++",
    ));
    // Bare `+` is a standalone key.
    assert!(key_id_matches(
        &key(KeyCode::Char('+'), KeyModifiers::NONE),
        "+",
    ));
}

// -- Named keys with modifiers ----------------------------------------------

#[test]
fn enter_variants_match_their_descriptors() {
    for (mods, descriptor) in [
        (KeyModifiers::NONE, "enter"),
        (KeyModifiers::SHIFT, "shift+enter"),
        (KeyModifiers::CONTROL, "ctrl+enter"),
        (KeyModifiers::ALT, "alt+enter"),
    ] {
        let event = key(KeyCode::Enter, mods);
        assert!(
            key_id_matches(&event, descriptor),
            "Enter+{:?} should match {:?}",
            mods,
            descriptor,
        );
    }
}

#[test]
fn tab_variants_match_their_descriptors() {
    assert!(key_id_matches(
        &key(KeyCode::Tab, KeyModifiers::NONE),
        "tab",
    ));
    assert!(key_id_matches(
        &key(KeyCode::Tab, KeyModifiers::CONTROL),
        "ctrl+tab",
    ));
    assert!(key_id_matches(
        &key(KeyCode::Tab, KeyModifiers::ALT),
        "alt+tab",
    ));
    // BackTab carries Shift: `shift+tab` vs descriptor `shift+tab` works
    // only if the event uses KeyCode::Tab with Shift explicit, which
    // matches how crossterm emits it under Kitty. The legacy BackTab
    // shape is a separate code.
    assert!(key_id_matches(
        &key(KeyCode::Tab, KeyModifiers::SHIFT),
        "shift+tab",
    ));
}

#[test]
fn backspace_variants_match_their_descriptors() {
    assert!(key_id_matches(
        &key(KeyCode::Backspace, KeyModifiers::NONE),
        "backspace",
    ));
    assert!(key_id_matches(
        &key(KeyCode::Backspace, KeyModifiers::CONTROL),
        "ctrl+backspace",
    ));
    assert!(key_id_matches(
        &key(KeyCode::Backspace, KeyModifiers::ALT),
        "alt+backspace",
    ));
}

#[test]
fn space_variants_match_their_descriptors() {
    // space is aliased to Char(' '), so the printable-shift rule applies
    // just like digits and letters.
    assert!(key_id_matches(
        &key(KeyCode::Char(' '), KeyModifiers::NONE),
        "space",
    ));
    assert!(key_id_matches(
        &key(KeyCode::Char(' '), KeyModifiers::CONTROL),
        "ctrl+space",
    ));
    assert!(key_id_matches(
        &key(KeyCode::Char(' '), KeyModifiers::ALT),
        "alt+space",
    ));
}

#[test]
fn insert_variants_match_their_descriptors() {
    assert!(key_id_matches(
        &key(KeyCode::Insert, KeyModifiers::NONE),
        "insert",
    ));
    assert!(
        key_id_matches(&key(KeyCode::Insert, KeyModifiers::NONE), "ins"),
        "short alias also works",
    );
    assert!(key_id_matches(
        &key(KeyCode::Insert, KeyModifiers::SHIFT),
        "shift+insert",
    ));
    assert!(key_id_matches(
        &key(KeyCode::Insert, KeyModifiers::CONTROL),
        "ctrl+insert",
    ));
}

#[test]
fn arrow_home_end_pageup_pagedown_with_modifiers() {
    let cases: &[(KeyCode, &str)] = &[
        (KeyCode::Up, "up"),
        (KeyCode::Down, "down"),
        (KeyCode::Left, "left"),
        (KeyCode::Right, "right"),
        (KeyCode::Home, "home"),
        (KeyCode::End, "end"),
        (KeyCode::PageUp, "pageup"),
        (KeyCode::PageDown, "pagedown"),
    ];

    for (code, name) in cases {
        // Plain.
        assert!(key_id_matches(&key(*code, KeyModifiers::NONE), name));
        // With each modifier.
        for (mods, prefix) in [
            (KeyModifiers::SHIFT, "shift+"),
            (KeyModifiers::CONTROL, "ctrl+"),
            (KeyModifiers::ALT, "alt+"),
        ] {
            let descriptor = format!("{}{}", prefix, name);
            assert!(
                key_id_matches(&key(*code, mods), &descriptor),
                "{:?}+{:?} should match {:?}",
                mods,
                code,
                descriptor,
            );
        }
    }
}

#[test]
fn page_up_descriptor_accepts_mixed_case() {
    // `pageUp`, `pageup`, `PAGEUP`, `PageUp` should all match the same
    // event because descriptors are lowercased before parsing.
    let event = key(KeyCode::PageUp, KeyModifiers::NONE);
    for descriptor in ["pageup", "pageUp", "PAGEUP", "PageUp"] {
        assert!(
            key_id_matches(&event, descriptor),
            "descriptor {:?} should match",
            descriptor,
        );
    }
}

#[test]
fn delete_descriptor_and_short_alias_match() {
    let event = key(KeyCode::Delete, KeyModifiers::NONE);
    assert!(key_id_matches(&event, "delete"));
    assert!(key_id_matches(&event, "del"));
}

#[test]
fn escape_descriptor_and_short_alias_match() {
    let event = key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(key_id_matches(&event, "escape"));
    assert!(key_id_matches(&event, "esc"));
}

// -- Function keys -----------------------------------------------------------

#[test]
fn function_keys_through_f24_parse() {
    for n in 1u8..=24 {
        let event = key(KeyCode::F(n), KeyModifiers::NONE);
        let descriptor = format!("f{}", n);
        assert!(
            key_id_matches(&event, &descriptor),
            "F{} should match descriptor {:?}",
            n,
            descriptor,
        );
    }
}

#[test]
fn function_key_descriptor_rejects_invalid_numbers() {
    // f0 is rejected by parse_key_id (returns None before matching).
    assert!(!key_id_matches(
        &key(KeyCode::F(1), KeyModifiers::NONE),
        "f0",
    ));
    // Non-digit suffixes fall through to single-char matching.
    assert!(!key_id_matches(
        &key(KeyCode::F(5), KeyModifiers::NONE),
        "fx",
    ));
}

// -- Non-matches / edge cases -----------------------------------------------

#[test]
fn descriptor_with_mismatched_key_rejects() {
    assert!(!key_id_matches(
        &key(KeyCode::Char('a'), KeyModifiers::CONTROL),
        "ctrl+b",
    ));
}

#[test]
fn descriptor_with_extra_modifier_on_event_rejects() {
    // Event has ctrl+shift, descriptor asks for only ctrl — mismatch.
    let event = key(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    );
    assert!(!key_id_matches(&event, "ctrl+a"));
    assert!(!key_id_matches(&event, "alt+a"));
    assert!(key_id_matches(&event, "ctrl+alt+a"));
}

#[test]
fn unknown_modifier_names_reject_the_match() {
    assert!(!key_id_matches(
        &key(KeyCode::Char('a'), KeyModifiers::NONE),
        "hyper+a",
    ));
    assert!(!key_id_matches(
        &key(KeyCode::Char('a'), KeyModifiers::NONE),
        "meta+a",
    ));
}

#[test]
fn empty_descriptor_rejects_everything() {
    assert!(!key_id_matches(
        &key(KeyCode::Char('a'), KeyModifiers::NONE),
        "",
    ));
    assert!(!key_id_matches(
        &key(KeyCode::Enter, KeyModifiers::NONE),
        ""
    ));
}

#[test]
fn non_key_events_never_match_key_descriptors() {
    assert!(!key_id_matches(&InputEvent::Paste("a".into()), "a"));
    assert!(!key_id_matches(&InputEvent::Resize(10, 20), "enter"));
}

// ---------------------------------------------------------------------------
// Additional coverage over the typed-event surface.
//
// Crossterm handles byte-level escape-sequence parsing at the
// ProcessTerminal boundary, so this file doesn't exercise parsing of
// raw sequences. What it does cover: conversion, matcher semantics
// across modifier combinations and key variants, and the interaction
// between kind (Press/Repeat/Release) and the rest of the API.
// ---------------------------------------------------------------------------

#[test]
fn as_char_returns_none_for_named_keys() {
    // Printable-character helpers must not pick up Enter, Escape, arrow
    // keys, or function keys even when modifiers are clear — callers
    // use as_char() to decide whether to insert the character into a
    // buffer, and a non-printable slip-through would corrupt input.
    for code in [
        KeyCode::Enter,
        KeyCode::Esc,
        KeyCode::Tab,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::F(1),
        KeyCode::Home,
        KeyCode::End,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Backspace,
        KeyCode::Delete,
        KeyCode::Insert,
    ] {
        let event = key(code, KeyModifiers::NONE);
        assert!(
            event.as_char().is_none(),
            "as_char should be None for {:?}",
            code,
        );
    }
}

#[test]
fn as_char_returns_none_when_ctrl_or_alt_is_held() {
    // Control / Alt always suppress the "printable character" path,
    // even for Char events. Shift alone still returns the character
    // (so shift+a yields Some('a'); callers can see the modifier on
    // the underlying KeyEvent if they need to distinguish case).
    for mods in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ] {
        let event = key(KeyCode::Char('a'), mods);
        assert!(
            event.as_char().is_none(),
            "as_char should be None for Char('a') with {:?}",
            mods,
        );
    }
}

#[test]
fn is_key_release_and_is_key_repeat_are_false_for_press_events() {
    // Defaults to Press kind; sanity-check that both helpers
    // correctly say "no" when the event isn't their specific kind.
    let press = key(KeyCode::Char('a'), KeyModifiers::NONE);
    assert!(!press.is_key_release());
    assert!(!press.is_key_repeat());
}

#[test]
fn is_key_release_is_false_for_paste_and_resize_events() {
    assert!(!InputEvent::Paste("x".into()).is_key_release());
    assert!(!InputEvent::Resize(10, 20).is_key_release());
    assert!(!InputEvent::Paste("x".into()).is_key_repeat());
    assert!(!InputEvent::Resize(10, 20).is_key_repeat());
}

#[test]
fn modifier_order_in_descriptor_does_not_matter() {
    // The matcher is order-independent; ours follows suit
    // via the bitflag check. Confirm both orderings match.
    let event = key(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(key_id_matches(&event, "ctrl+shift+a"));
    assert!(key_id_matches(&event, "shift+ctrl+a"));

    // Three-way: ctrl+alt+shift+a vs alt+shift+ctrl+a.
    let event3 = key(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
    );
    assert!(key_id_matches(&event3, "ctrl+alt+shift+a"));
    assert!(key_id_matches(&event3, "alt+shift+ctrl+a"));
    assert!(key_id_matches(&event3, "shift+alt+ctrl+a"));
}

#[test]
fn common_named_key_aliases_match() {
    // A few shortened names match alongside the canonical ones.
    assert!(key_id_matches(
        &key(KeyCode::Enter, KeyModifiers::NONE),
        "return"
    ));
    assert!(key_id_matches(
        &key(KeyCode::PageUp, KeyModifiers::NONE),
        "pgup"
    ));
    assert!(key_id_matches(
        &key(KeyCode::PageDown, KeyModifiers::NONE),
        "pgdn"
    ));
    assert!(key_id_matches(
        &key(KeyCode::Esc, KeyModifiers::NONE),
        "esc"
    ));
    assert!(key_id_matches(
        &key(KeyCode::Delete, KeyModifiers::NONE),
        "del"
    ));
}

#[test]
fn space_matches_both_literal_space_and_named_descriptor() {
    // Space can be expressed either as the literal " " character or as
    // "space". Tests exercise both forms so a keybinding config isn't
    // forced into one convention.
    let event = key(KeyCode::Char(' '), KeyModifiers::NONE);
    assert!(key_id_matches(&event, "space"));
    assert!(key_id_matches(&event, " "));
}

#[test]
fn release_events_still_carry_enough_info_to_match_descriptors() {
    // A release event has kind=Release but the same code + modifiers
    // as a press. The key_id matcher doesn't branch on kind, which
    // matches the design intent: components that want to treat
    // presses and releases differently look at is_key_release() and
    // wants_key_release() rather than baking it into the descriptor.
    let release = InputEvent::Key(KeyEvent::new_with_kind(
        KeyCode::Char('a'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    assert!(release.is_key_release());
    assert!(key_id_matches(&release, "a"));
}

#[test]
fn function_key_aliases_are_case_insensitive() {
    // F keys accept both "f5" and "F5" — the matcher does case-folding
    // before matching, so our matcher should too.
    let event = key(KeyCode::F(5), KeyModifiers::NONE);
    assert!(key_id_matches(&event, "f5"));
    assert!(key_id_matches(&event, "F5"));
}

#[test]
fn named_key_descriptors_are_case_insensitive() {
    // Same invariant as function keys, for named keys: "enter" vs
    // "Enter" vs "ENTER" all match.
    let event = key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(key_id_matches(&event, "enter"));
    assert!(key_id_matches(&event, "Enter"));
    assert!(key_id_matches(&event, "ENTER"));
}

#[test]
fn modifier_names_are_case_insensitive() {
    let event = key(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert!(key_id_matches(&event, "Ctrl+Shift+a"));
    assert!(key_id_matches(&event, "CTRL+SHIFT+a"));
}

#[test]
fn crossterm_event_variants_we_do_not_support_are_dropped() {
    // FocusGained / FocusLost / Mouse events don't translate into our
    // InputEvent. `try_from` should return Err rather than silently
    // producing a misleading typed event.
    use crossterm::event::Event;

    assert!(InputEvent::try_from(Event::FocusGained).is_err());
    assert!(InputEvent::try_from(Event::FocusLost).is_err());
}

#[test]
fn paste_event_does_not_match_char_descriptor() {
    // A Paste("a") event should not be matchable as the key "a".
    // Upstream makes the same distinction; matching a paste as a
    // keystroke would confuse keybinding dispatch.
    let paste = InputEvent::Paste("a".into());
    assert!(!key_id_matches(&paste, "a"));
    assert!(!paste.is_key(KeyCode::Char('a')));
    assert!(paste.as_char().is_none());
}
