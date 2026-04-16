//! Smoke tests for the non-terminal pieces of the test-support framework.
//!
//! The virtual-terminal surface has its own regression file
//! (`virtual_terminal_helpers.rs`) and the full engine is exercised by the
//! other integration tests; this file guards the smaller helpers — env
//! guards, fixtures, and theme factories — so they don't silently drift
//! out of sync with the types they wrap.

mod support;

use std::env;

use aj_tui::keys::InputEvent;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use support::fixtures::InputRecorder;
use support::themes::{
    default_editor_theme, default_markdown_theme, default_select_list_theme, identity_editor_theme,
    identity_markdown_theme, identity_select_list_theme,
};
use support::{
    MutableLines, StaticLines, plain_lines, plain_lines_trim_end, strip_ansi, visible_index_of,
    with_env,
};

// ---------------------------------------------------------------------------
// env::with_env
// ---------------------------------------------------------------------------

const ENV_KEY: &str = "AJ_TUI_SUPPORT_SMOKE_VAR";

#[test]
#[serial_test::serial]
fn with_env_sets_value_for_the_lifetime_of_the_guard() {
    // SAFETY: serialized against other env-mutating tests.
    unsafe { env::remove_var(ENV_KEY) };
    assert!(env::var(ENV_KEY).is_err(), "precondition");

    {
        let _guard = with_env(&[(ENV_KEY, Some("hello"))]);
        assert_eq!(env::var(ENV_KEY).as_deref(), Ok("hello"));
    }

    assert!(
        env::var(ENV_KEY).is_err(),
        "variable should be removed once the guard drops",
    );
}

#[test]
#[serial_test::serial]
fn with_env_restores_the_previous_value_when_overriding() {
    // SAFETY: serialized against other env-mutating tests.
    unsafe { env::set_var(ENV_KEY, "original") };

    {
        let _guard = with_env(&[(ENV_KEY, Some("override"))]);
        assert_eq!(env::var(ENV_KEY).as_deref(), Ok("override"));
    }

    assert_eq!(
        env::var(ENV_KEY).as_deref(),
        Ok("original"),
        "previous value should be restored",
    );

    // Cleanup.
    // SAFETY: still inside the serialized section.
    unsafe { env::remove_var(ENV_KEY) };
}

#[test]
#[serial_test::serial]
fn with_env_can_remove_a_previously_set_variable() {
    // SAFETY: serialized against other env-mutating tests.
    unsafe { env::set_var(ENV_KEY, "present") };

    {
        let _guard = with_env(&[(ENV_KEY, None)]);
        assert!(env::var(ENV_KEY).is_err());
    }

    assert_eq!(
        env::var(ENV_KEY).as_deref(),
        Ok("present"),
        "previous value should be restored after a remove-and-restore cycle",
    );

    // Cleanup.
    // SAFETY: still inside the serialized section.
    unsafe { env::remove_var(ENV_KEY) };
}

// ---------------------------------------------------------------------------
// fixtures::InputRecorder
// ---------------------------------------------------------------------------

#[test]
fn input_recorder_captures_events_through_a_shared_log() {
    use aj_tui::component::Component;

    let (mut recorder, events) = InputRecorder::new();
    let sample = InputEvent::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));

    let handled = recorder.handle_input(&sample);

    assert!(handled, "InputRecorder should always mark input as handled");
    let log = events.borrow();
    assert_eq!(log.len(), 1);
    assert!(matches!(
        log[0],
        InputEvent::Key(KeyEvent {
            code: KeyCode::Char('k'),
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// support::send_keys
// ---------------------------------------------------------------------------

#[test]
fn send_keys_dispatches_every_event_in_order_to_the_focused_component() {
    use aj_tui::keys::Key;
    use aj_tui::tui::Tui;

    use support::VirtualTerminal;
    use support::send_keys;

    let (recorder, events) = InputRecorder::new();
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal));
    tui.add_child(Box::new(recorder));
    tui.set_focus(Some(0));

    send_keys(&mut tui, [Key::char('h'), Key::char('i'), Key::enter()]);

    let log = events.borrow();
    assert_eq!(log.len(), 3);
    assert!(matches!(
        log[0],
        InputEvent::Key(KeyEvent {
            code: KeyCode::Char('h'),
            ..
        })
    ));
    assert!(matches!(
        log[1],
        InputEvent::Key(KeyEvent {
            code: KeyCode::Char('i'),
            ..
        })
    ));
    assert!(matches!(
        log[2],
        InputEvent::Key(KeyEvent {
            code: KeyCode::Enter,
            ..
        })
    ));
}

#[test]
fn send_keys_accepts_an_empty_iterator_as_a_noop() {
    use aj_tui::tui::Tui;

    use support::VirtualTerminal;
    use support::send_keys;

    let (recorder, events) = InputRecorder::new();
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal));
    tui.add_child(Box::new(recorder));
    tui.set_focus(Some(0));

    send_keys(&mut tui, std::iter::empty::<InputEvent>());

    assert!(events.borrow().is_empty());
}

// ---------------------------------------------------------------------------
// fixtures::StaticLines
// ---------------------------------------------------------------------------

#[test]
fn static_lines_renders_stored_lines_verbatim_at_any_width() {
    use aj_tui::component::Component;

    let mut component = StaticLines::new(["one", "two", "three"]);

    // Width is passed in but ignored; StaticLines never wraps.
    assert_eq!(component.render(5), vec!["one", "two", "three"]);
    assert_eq!(component.render(80), vec!["one", "two", "three"]);
}

#[test]
fn static_lines_accepts_owned_and_borrowed_strings() {
    use aj_tui::component::Component;

    // IntoIterator<Item: Into<String>> means both `&str` and `String` work.
    let mut component = StaticLines::new(vec!["borrowed".to_string(), "also borrowed".to_string()]);
    assert_eq!(component.render(10), vec!["borrowed", "also borrowed"]);
}

#[test]
fn static_lines_with_no_input_renders_empty() {
    use aj_tui::component::Component;

    let mut component = StaticLines::new(Vec::<&str>::new());
    assert!(component.render(10).is_empty());
}

// ---------------------------------------------------------------------------
// fixtures::StaticOverlay
// ---------------------------------------------------------------------------

#[test]
fn static_overlay_records_the_width_it_was_rendered_at() {
    use aj_tui::component::Component;

    use support::StaticOverlay;

    let (mut overlay, recorded) = StaticOverlay::new(["hello"]);

    assert!(
        recorded.borrow().is_none(),
        "width should start as None until a render happens",
    );

    let out = overlay.render(42);
    assert_eq!(out, vec!["hello"]);
    assert_eq!(*recorded.borrow(), Some(42));

    // A second render overwrites with the new width.
    let _ = overlay.render(10);
    assert_eq!(*recorded.borrow(), Some(10));
}

#[test]
fn static_overlay_accepts_owned_and_borrowed_strings() {
    use aj_tui::component::Component;

    use support::StaticOverlay;

    let (mut overlay, _) =
        StaticOverlay::new(vec!["borrowed".to_string(), "also borrowed".to_string()]);
    assert_eq!(overlay.render(10), vec!["borrowed", "also borrowed"]);
}

// ---------------------------------------------------------------------------
// fixtures::MutableLines
// ---------------------------------------------------------------------------

#[test]
fn mutable_lines_starts_empty() {
    use aj_tui::component::Component;

    let mut lines = MutableLines::new();
    assert!(lines.is_empty());
    assert_eq!(lines.len(), 0);
    assert!(lines.render(80).is_empty());
}

#[test]
fn mutable_lines_with_lines_seeds_initial_content() {
    use aj_tui::component::Component;

    let mut lines = MutableLines::with_lines(["one", "two"]);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines.render(80), vec!["one", "two"]);
}

#[test]
fn mutable_lines_set_replaces_content_for_next_render() {
    use aj_tui::component::Component;

    let mut lines = MutableLines::new();
    lines.set(["first"]);
    assert_eq!(lines.render(80), vec!["first"]);

    lines.set(["second", "third"]);
    assert_eq!(lines.render(80), vec!["second", "third"]);
}

#[test]
fn mutable_lines_append_and_push_extend_content() {
    use aj_tui::component::Component;

    let mut lines = MutableLines::with_lines(["one"]);
    lines.append(["two", "three"]);
    lines.push("four");
    assert_eq!(lines.render(80), vec!["one", "two", "three", "four"]);
}

#[test]
fn mutable_lines_clear_empties_the_buffer() {
    let lines = MutableLines::with_lines(["kept"]);
    assert_eq!(lines.len(), 1);
    lines.clear();
    assert!(lines.is_empty());
}

#[test]
fn mutable_lines_clone_shares_the_same_buffer() {
    use aj_tui::component::Component;

    // Core invariant that makes this fixture useful for streaming tests:
    // the test keeps a handle, installs a clone in the `Tui`, and
    // subsequent mutations on either handle are visible on both.
    let mut original = MutableLines::with_lines(["initial"]);
    let mirror = original.clone();

    mirror.append(["added via mirror"]);
    assert_eq!(original.len(), 2);
    assert_eq!(
        original.render(80),
        vec!["initial", "added via mirror"],
        "mutations through a clone must be visible on the original handle",
    );

    original.clear();
    assert!(mirror.is_empty(), "clone must see the cleared state");
}

#[test]
fn mutable_lines_snapshot_returns_a_detached_copy() {
    let lines = MutableLines::with_lines(["a", "b"]);
    let snap = lines.snapshot();
    lines.push("c");
    assert_eq!(snap, vec!["a", "b"]);
    assert_eq!(lines.len(), 3);
}

// ---------------------------------------------------------------------------
// themes::*
// ---------------------------------------------------------------------------

#[test]
fn default_select_list_theme_closures_wrap_input_in_escape_sequences() {
    let theme = default_select_list_theme();

    for styled in [
        (theme.selected_prefix)("prefix"),
        (theme.selected_text)("text"),
        (theme.description)("desc"),
        (theme.scroll_info)("info"),
        (theme.no_match)("no match"),
    ] {
        assert!(
            styled.contains("\x1b["),
            "expected ANSI styling in {:?}",
            styled,
        );
    }
}

#[test]
fn default_markdown_theme_exposes_every_field() {
    let theme = default_markdown_theme();

    // Exercise every closure once so a field rename surfaces here.
    let _ = (theme.heading)("h");
    let _ = (theme.link)("l");
    let _ = (theme.link_url)("u");
    let _ = (theme.code)("c");
    let _ = (theme.code_block)("c");
    let _ = (theme.code_block_border)("b");
    let _ = (theme.quote)("q");
    let _ = (theme.quote_border)("b");
    let _ = (theme.hr)("-");
    let _ = (theme.list_bullet)("*");
    let _ = (theme.bold)("b");
    let _ = (theme.italic)("i");
    let _ = (theme.strikethrough)("s");
    let _ = (theme.underline)("u");
    assert!(theme.highlight_code.is_none());
    assert!(theme.code_block_indent.is_none());
}

#[test]
fn default_editor_theme_bundles_a_select_list_theme() {
    let theme = default_editor_theme();

    // border_color is the editor's own closure.
    assert!((theme.border_color)("x").contains("\x1b["));
    // And it pulls in a working select-list theme.
    assert!((theme.select_list.selected_text)("x").contains("\x1b["));
}

// ---------------------------------------------------------------------------
// themes::identity_*
// ---------------------------------------------------------------------------

#[test]
fn identity_select_list_theme_passes_text_through_verbatim() {
    let theme = identity_select_list_theme();

    assert_eq!((theme.selected_prefix)("prefix"), "prefix");
    assert_eq!((theme.selected_text)("text"), "text");
    assert_eq!((theme.description)("desc"), "desc");
    assert_eq!((theme.scroll_info)("info"), "info");
    assert_eq!((theme.no_match)("no match"), "no match");
}

#[test]
fn identity_markdown_theme_passes_text_through_verbatim() {
    let theme = identity_markdown_theme();

    for (label, styled) in [
        ("heading", (theme.heading)("h")),
        ("link", (theme.link)("l")),
        ("link_url", (theme.link_url)("u")),
        ("code", (theme.code)("c")),
        ("code_block", (theme.code_block)("cb")),
        ("code_block_border", (theme.code_block_border)("b")),
        ("quote", (theme.quote)("q")),
        ("quote_border", (theme.quote_border)("b")),
        ("hr", (theme.hr)("-")),
        ("list_bullet", (theme.list_bullet)("*")),
        ("bold", (theme.bold)("b")),
        ("italic", (theme.italic)("i")),
        ("strikethrough", (theme.strikethrough)("s")),
        ("underline", (theme.underline)("u")),
    ] {
        assert!(
            !styled.contains("\x1b["),
            "identity markdown theme field {label} should not add ANSI codes, got {styled:?}",
        );
    }
    assert!(theme.highlight_code.is_none());
    assert!(theme.code_block_indent.is_none());
}

#[test]
fn identity_editor_theme_bundles_an_identity_select_list_theme() {
    let theme = identity_editor_theme();

    assert_eq!((theme.border_color)("x"), "x");
    assert_eq!((theme.select_list.selected_text)("x"), "x");
}

// ---------------------------------------------------------------------------
// ansi helpers
// ---------------------------------------------------------------------------

#[test]
fn strip_ansi_removes_sgr_sequences_and_preserves_utf8() {
    let styled = "\x1b[1;31mhello\x1b[0m \u{4E2D}";
    assert_eq!(strip_ansi(styled), "hello \u{4E2D}");
}

#[test]
fn strip_ansi_removes_non_sgr_csi_sequences() {
    // CSI sequences with non-`m` final bytes (cursor move, erase).
    assert_eq!(strip_ansi("\x1b[2J\x1b[Hdone"), "done");
    assert_eq!(strip_ansi("a\x1b[3Bb"), "ab");
}

#[test]
fn strip_ansi_removes_osc_sequences_with_bel_or_st_terminator() {
    // OSC 0 terminated by BEL (how `set_title` is emitted).
    assert_eq!(strip_ansi("\x1b]0;title\x07after"), "after");
    // OSC terminated by ST (ESC \).
    assert_eq!(
        strip_ansi("\x1b]8;;http://x\x1b\\label\x1b]8;;\x1b\\"),
        "label"
    );
}

#[test]
fn strip_ansi_removes_apc_pm_and_sos_sequences() {
    // APC (ESC `_`) is used by the crate's cursor marker (`\x1b_tui:c\x07`).
    assert_eq!(strip_ansi("before\x1b_tui:c\x07after"), "beforeafter");
    // PM (ESC `^`) and SOS (ESC `X`) share the BEL / ST termination.
    assert_eq!(strip_ansi("x\x1b^private\x07y"), "xy");
    assert_eq!(strip_ansi("x\x1bXsos\x1b\\y"), "xy");
}

#[test]
fn plain_lines_helpers_apply_strip_ansi_to_each_row() {
    let rows = vec![
        "\x1b[1mhello\x1b[0m".to_string(),
        "world   ".to_string(),
        "\x1b[31mred\x1b[0m   ".to_string(),
    ];
    assert_eq!(plain_lines(&rows), vec!["hello", "world   ", "red   "]);
    assert_eq!(plain_lines_trim_end(&rows), vec!["hello", "world", "red"],);
}

#[test]
fn visible_index_of_returns_column_past_ansi_prefix() {
    let line = "\x1b[1m>\x1b[0m  hello";
    // Visible prefix before `hello` is ">  " (3 columns).
    assert_eq!(visible_index_of(line, "hello"), 3);
}

#[test]
#[should_panic(expected = "expected")]
fn visible_index_of_panics_when_needle_is_missing() {
    visible_index_of("foo", "bar");
}
