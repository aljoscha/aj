//! Tests for the `VirtualTerminal` test-support helpers themselves.
//!
//! These verify the inspection and state-manipulation methods that tests
//! rely on — things like `clear_viewport` and `reset` — behave the way
//! their docs describe. If these regress, every other integration test
//! that depends on them becomes unreliable.

mod support;

use aj_tui::keys::InputEvent;
use aj_tui::terminal::Terminal;
use futures::StreamExt;

use vt100_ctt::Color;

use support::VirtualTerminal;

#[test]
fn clear_viewport_wipes_contents_without_logging_a_write() {
    let mut terminal = VirtualTerminal::new(20, 5);
    terminal.write("hello");

    assert_eq!(terminal.viewport()[0], "hello");
    let writes_before = terminal.writes_joined();

    terminal.clear_viewport();

    // Cursor went home and contents are blank.
    assert_eq!(terminal.cursor(), (0, 0));
    assert_eq!(terminal.viewport()[0], "");
    // The captured write log is untouched (the clear was out-of-band).
    assert_eq!(terminal.writes_joined(), writes_before);
}

#[tokio::test]
async fn reset_returns_terminal_to_pristine_state() {
    let mut terminal = VirtualTerminal::new(20, 5);
    terminal.write("some text");
    terminal.hide_cursor();
    terminal.set_title("title");
    terminal.start().expect("start should succeed");
    terminal.stop();
    // `resize` is the only remaining way to enqueue an input event without
    // going through a real backend; use it to seed channel state so we can
    // verify `reset` drains it.
    terminal.resize(20, 5);

    terminal.reset();

    assert_eq!(terminal.viewport()[0], "");
    assert_eq!(terminal.cursor(), (0, 0));
    assert!(terminal.is_cursor_visible());
    assert_eq!(terminal.title(), "");
    assert_eq!(terminal.writes_joined(), "");
    // After reset, taking the stream and trying to recv should find no
    // events — the resize above was drained.
    let mut stream = terminal
        .take_input_stream()
        .expect("take_input_stream should return Some on first call");
    let got = tokio::time::timeout(std::time::Duration::from_millis(5), stream.next()).await;
    assert!(
        got.is_err(),
        "input stream should be empty after reset, got {got:?}",
    );
    assert_eq!(terminal.start_count(), 0);
    assert_eq!(terminal.stop_count(), 0);
    // Dimensions are preserved.
    assert_eq!(terminal.columns(), 20);
    assert_eq!(terminal.rows(), 5);
}

#[tokio::test]
async fn reset_preserves_dimensions_after_a_resize() {
    let mut terminal = VirtualTerminal::new(20, 5);
    terminal.resize(40, 10);
    // Drain the resize event by taking the stream and pulling one item.
    let mut stream = terminal
        .take_input_stream()
        .expect("take_input_stream should return Some");
    let _ = tokio::time::timeout(std::time::Duration::from_millis(5), stream.next()).await;

    terminal.write("content");
    terminal.reset();

    assert_eq!(terminal.columns(), 40);
    assert_eq!(terminal.rows(), 10);
    assert_eq!(terminal.viewport().len(), 10);
}

#[test]
fn scroll_buffer_returns_viewport_when_no_scrollback_has_accumulated() {
    let mut terminal = VirtualTerminal::new(20, 3);
    // Three lines exactly fill the viewport — no scrollback.
    terminal.write("a\r\nb\r\nc");

    let buffer = terminal.scroll_buffer();
    // No scrollback rows, so the buffer is just the viewport.
    assert_eq!(buffer, vec!["a", "b", "c"]);
}

#[test]
fn scroll_buffer_includes_lines_that_scrolled_off_screen() {
    let mut terminal = VirtualTerminal::new(20, 3);
    // Six lines into a 3-row viewport: the first three scroll into
    // scrollback, the last three remain visible.
    terminal.write("one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");

    let buffer = terminal.scroll_buffer();
    assert_eq!(buffer, vec!["one", "two", "three", "four", "five", "six"],);
    // The current viewport only shows the trailing rows.
    assert_eq!(terminal.viewport(), vec!["four", "five", "six"]);
}

#[test]
fn scroll_buffer_preserves_the_current_scrollback_offset() {
    let mut terminal = VirtualTerminal::new(20, 3);
    terminal.write("one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix");

    // Offset is 0 by default; reading the buffer should leave it at 0.
    assert_eq!(terminal.scroll_buffer().len(), 6);
    assert_eq!(terminal.viewport(), vec!["four", "five", "six"]);
}

#[test]
fn viewport_trimmed_drops_trailing_empties_but_keeps_interior_blanks() {
    let mut terminal = VirtualTerminal::new(20, 6);
    terminal.write("a\r\n\r\nb");

    assert_eq!(terminal.viewport(), vec!["a", "", "b", "", "", ""]);
    // Trailing blanks gone; interior blank between "a" and "b" preserved.
    assert_eq!(terminal.viewport_trimmed(), vec!["a", "", "b"]);
}

#[test]
fn viewport_trimmed_returns_empty_vec_when_nothing_was_written() {
    let terminal = VirtualTerminal::new(10, 4);
    assert_eq!(terminal.viewport_trimmed(), Vec::<String>::new());
}

#[test]
fn virtual_terminal_start_is_a_noop_and_idempotent() {
    let mut terminal = VirtualTerminal::new(10, 3);
    // Should succeed and leave nothing observable behind on the write log
    // or parser. The start counter is the one observable side-effect; it's
    // covered by its own tests below.
    terminal.start().expect("start should succeed");
    terminal.start().expect("start should be idempotent");
    assert_eq!(terminal.writes_joined(), "");
    assert_eq!(terminal.viewport()[0], "");
}

// ---------------------------------------------------------------------------
// Start / stop lifecycle counters
// ---------------------------------------------------------------------------

#[test]
fn start_and_stop_counters_default_to_zero() {
    let terminal = VirtualTerminal::new(10, 3);
    assert_eq!(terminal.start_count(), 0);
    assert_eq!(terminal.stop_count(), 0);
}

#[test]
fn start_count_increments_on_each_start_call() {
    let mut terminal = VirtualTerminal::new(10, 3);

    terminal.start().expect("start should succeed");
    assert_eq!(terminal.start_count(), 1);

    terminal.start().expect("start should be idempotent");
    assert_eq!(terminal.start_count(), 2);
}

#[test]
fn stop_count_increments_on_each_stop_call() {
    let mut terminal = VirtualTerminal::new(10, 3);

    terminal.stop();
    assert_eq!(terminal.stop_count(), 1);

    terminal.stop();
    assert_eq!(terminal.stop_count(), 2);
}

#[test]
fn start_and_stop_calls_do_not_write_bytes_or_touch_viewport() {
    let mut terminal = VirtualTerminal::new(10, 3);

    terminal.start().expect("start should succeed");
    terminal.stop();
    terminal.start().expect("restart should succeed");

    assert_eq!(
        terminal.writes_joined(),
        "",
        "start/stop must not emit to the writes log",
    );
    assert_eq!(terminal.viewport()[0], "");
}

#[test]
fn start_and_stop_counters_are_shared_across_clones() {
    // Consistent with the progress / Kitty-protocol flags: lifecycle state
    // lives in the shared `Rc<RefCell<_>>`, so incrementing on any handle
    // must be visible on every other handle.
    let mut terminal = VirtualTerminal::new(10, 3);
    let mut mirror = terminal.clone();

    terminal.start().expect("start should succeed");
    assert_eq!(mirror.start_count(), 1);

    mirror.stop();
    assert_eq!(terminal.stop_count(), 1);
}

#[test]
fn reset_clears_start_and_stop_counters() {
    let mut terminal = VirtualTerminal::new(10, 3);
    terminal.start().expect("start should succeed");
    terminal.stop();
    assert_eq!(terminal.start_count(), 1);
    assert_eq!(terminal.stop_count(), 1);

    terminal.reset();

    assert_eq!(terminal.start_count(), 0);
    assert_eq!(terminal.stop_count(), 0);
}

#[test]
fn writes_after_reset_draw_into_the_fresh_buffer() {
    // Regression guard for the subtle invariant that `reset()` replaces the
    // underlying parser — not just the SGR state. If `reset` forgot to
    // swap the parser, leftover styling (or stale content) from before the
    // reset could silently bleed into the next render.
    let mut terminal = VirtualTerminal::new(10, 3);

    // Seed styled content that, if it leaked, would show up either as
    // visible text on row 0 or as inherited styling on the next write.
    terminal.write("\x1b[1;31mOLD\x1b[0m\r\nOLD2");
    assert_eq!(terminal.viewport()[0], "OLD");
    assert_eq!(terminal.viewport()[1], "OLD2");

    terminal.reset();
    terminal.write("NEW");

    assert_eq!(terminal.viewport()[0], "NEW");
    assert_eq!(terminal.viewport()[1], "");
    let cell = terminal
        .cell(0, 0)
        .expect("fresh buffer should have a cell at (0,0)");
    assert_eq!(cell.contents, "N");
    assert!(!cell.bold, "bold from pre-reset content must not leak");
    assert!(matches!(cell.fg, Color::Default));
}

// ---------------------------------------------------------------------------
// Input queuing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn take_input_stream_surfaces_resize_events_in_order() {
    let mut terminal = VirtualTerminal::new(20, 5);
    terminal.resize(30, 6);
    terminal.resize(40, 7);

    let mut stream = terminal
        .take_input_stream()
        .expect("take_input_stream should return Some");

    let first = stream.next().await.expect("first resize");
    assert!(matches!(first, InputEvent::Resize(30, 6)));

    let second = stream.next().await.expect("second resize");
    assert!(matches!(second, InputEvent::Resize(40, 7)));

    // Nothing else is queued.
    let maybe = tokio::time::timeout(std::time::Duration::from_millis(5), stream.next()).await;
    assert!(maybe.is_err(), "stream should be empty now");
}

#[tokio::test]
async fn take_input_stream_returns_none_on_second_call() {
    let mut terminal = VirtualTerminal::new(20, 5);
    assert!(terminal.take_input_stream().is_some());
    assert!(terminal.take_input_stream().is_none());
}

// ---------------------------------------------------------------------------
// Viewport / cursor / title read-backs
// ---------------------------------------------------------------------------

#[test]
fn viewport_text_joins_rows_with_newlines_and_preserves_height() {
    let mut terminal = VirtualTerminal::new(10, 4);
    terminal.write("hi\r\nworld");

    // Four rows total, last two blank, joined by '\n'.
    assert_eq!(terminal.viewport_text(), "hi\nworld\n\n");
}

#[test]
fn cursor_tracks_parser_cursor_position() {
    let mut terminal = VirtualTerminal::new(20, 5);
    assert_eq!(terminal.cursor(), (0, 0));

    terminal.write("hello");
    assert_eq!(terminal.cursor(), (0, 5));

    terminal.write("\r\n");
    assert_eq!(terminal.cursor(), (1, 0));
}

#[test]
fn hide_and_show_cursor_toggle_is_cursor_visible() {
    let mut terminal = VirtualTerminal::new(10, 3);
    assert!(terminal.is_cursor_visible(), "cursor visible by default");

    terminal.hide_cursor();
    assert!(!terminal.is_cursor_visible());

    terminal.show_cursor();
    assert!(terminal.is_cursor_visible());
}

#[test]
fn set_title_is_readable_back_and_also_emits_an_osc_sequence() {
    let mut terminal = VirtualTerminal::new(10, 3);

    terminal.set_title("hello");

    assert_eq!(terminal.title(), "hello");
    // The OSC 0 sequence was captured in the writes log.
    assert!(terminal.writes_joined().contains("\x1b]0;hello\x07"));
}

// ---------------------------------------------------------------------------
// Progress indicator (OSC 9;4)
// ---------------------------------------------------------------------------

#[test]
fn progress_defaults_to_inactive() {
    let terminal = VirtualTerminal::new(10, 3);
    assert!(
        !terminal.is_progress_active(),
        "progress flag should start cleared",
    );
    assert_eq!(
        terminal.writes_joined(),
        "",
        "no escape should have been emitted without a call to set_progress",
    );
}

#[test]
fn set_progress_true_flips_flag_and_emits_the_indeterminate_osc_sequence() {
    let mut terminal = VirtualTerminal::new(10, 3);

    terminal.set_progress(true);

    assert!(terminal.is_progress_active());
    assert!(
        terminal.writes_joined().contains("\x1b]9;4;1;0\x07"),
        "expected OSC 9;4;1;0 in writes, got {:?}",
        terminal.writes_joined(),
    );
}

#[test]
fn set_progress_false_clears_flag_and_emits_the_clear_osc_sequence() {
    let mut terminal = VirtualTerminal::new(10, 3);
    terminal.set_progress(true);
    terminal.clear_writes();

    terminal.set_progress(false);

    assert!(!terminal.is_progress_active());
    assert!(
        terminal.writes_joined().contains("\x1b]9;4;0;\x07"),
        "expected OSC 9;4;0; in writes, got {:?}",
        terminal.writes_joined(),
    );
}

#[test]
fn progress_flag_is_shared_across_clones() {
    // Mirrors the invariant the Kitty-protocol flag relies on: toggling
    // progress on any clone must be visible everywhere the `Rc<RefCell>`
    // state is reachable.
    let mut terminal = VirtualTerminal::new(10, 3);
    let mirror = terminal.clone();

    terminal.set_progress(true);
    assert!(mirror.is_progress_active());

    terminal.set_progress(false);
    assert!(!mirror.is_progress_active());
}

#[test]
fn reset_clears_the_progress_flag() {
    let mut terminal = VirtualTerminal::new(10, 3);
    terminal.set_progress(true);
    assert!(terminal.is_progress_active());

    terminal.reset();

    assert!(!terminal.is_progress_active());
}

#[test]
fn terminal_trait_default_set_progress_is_a_noop() {
    // The trait default for `set_progress` is a no-op so a hand-rolled
    // minimal `Terminal` impl compiles without writing a body, and does
    // not emit anything to its underlying sink. `ProcessTerminal` and
    // `VirtualTerminal` each override.
    struct CountingTerminal {
        writes: usize,
    }
    impl Terminal for CountingTerminal {
        fn write(&mut self, _data: &str) {
            self.writes += 1;
        }
        fn columns(&self) -> u16 {
            0
        }
        fn rows(&self) -> u16 {
            0
        }
        fn move_by(&mut self, _lines: i32) {}
        fn hide_cursor(&mut self) {}
        fn show_cursor(&mut self) {}
        fn clear_line(&mut self) {}
        fn clear_from_cursor(&mut self) {}
        fn clear_screen(&mut self) {}
        fn set_title(&mut self, _title: &str) {}
        fn flush(&mut self) {}
    }

    let mut terminal = CountingTerminal { writes: 0 };
    terminal.set_progress(true);
    terminal.set_progress(false);
    assert_eq!(
        terminal.writes, 0,
        "default set_progress must not call write()",
    );
}

// ---------------------------------------------------------------------------
// Writes log
// ---------------------------------------------------------------------------

#[test]
fn writes_log_captures_every_call_in_order() {
    let mut terminal = VirtualTerminal::new(10, 3);

    terminal.write("one");
    terminal.write("two");
    terminal.hide_cursor();

    let writes = terminal.writes();
    assert_eq!(writes.len(), 3);
    assert_eq!(writes[0], "one");
    assert_eq!(writes[1], "two");
    assert_eq!(writes[2], "\x1b[?25l");
    drop(writes);

    assert_eq!(terminal.writes_joined(), "onetwo\x1b[?25l");
}

#[test]
fn clear_writes_empties_the_log_without_touching_parser_state() {
    let mut terminal = VirtualTerminal::new(10, 3);
    terminal.write("keep on screen");

    terminal.clear_writes();

    assert_eq!(terminal.writes_joined(), "");
    // Parser still shows what was written.
    assert_eq!(terminal.viewport()[0], "keep on screen");
}

#[test]
fn flush_is_a_noop_and_does_not_re_append_to_the_writes_log() {
    let mut terminal = VirtualTerminal::new(10, 3);
    terminal.write("hi");

    let before = terminal.writes_joined();
    terminal.flush();
    terminal.flush();
    let after = terminal.writes_joined();

    assert_eq!(before, after, "flush should not append to the writes log");
    assert_eq!(after, "hi");
}

// ---------------------------------------------------------------------------
// Per-cell style (CellInfo)
// ---------------------------------------------------------------------------

#[test]
fn cell_reports_plain_cell_attributes() {
    let mut terminal = VirtualTerminal::new(10, 3);
    terminal.write("a");

    let cell = terminal.cell(0, 0).expect("cell (0,0) should exist");
    assert_eq!(cell.contents, "a");
    assert!(!cell.bold);
    assert!(!cell.italic);
    assert!(!cell.underline);
    assert!(!cell.inverse);
    assert!(!cell.is_wide);
    assert!(!cell.is_wide_continuation);
    assert_eq!(cell.fg, Color::Default);
    assert_eq!(cell.bg, Color::Default);
}

#[test]
fn cell_reports_bold_italic_and_underline_attributes() {
    let mut terminal = VirtualTerminal::new(10, 3);
    // SGR 1=bold, 3=italic, 4=underline.
    terminal.write("\x1b[1;3;4mX\x1b[0m");

    let cell = terminal.cell(0, 0).expect("cell (0,0) should exist");
    assert_eq!(cell.contents, "X");
    assert!(cell.bold);
    assert!(cell.italic);
    assert!(cell.underline);
}

#[test]
fn cell_reports_inverse_and_colors() {
    let mut terminal = VirtualTerminal::new(10, 3);
    // SGR 7 = inverse; 31 = red fg; 42 = green bg.
    terminal.write("\x1b[7;31;42mZ\x1b[0m");

    let cell = terminal.cell(0, 0).expect("cell (0,0) should exist");
    assert!(cell.inverse);
    assert!(
        matches!(cell.fg, Color::Idx(1)),
        "expected red fg, got {:?}",
        cell.fg,
    );
    assert!(
        matches!(cell.bg, Color::Idx(2)),
        "expected green bg, got {:?}",
        cell.bg,
    );
}

#[test]
fn cell_reports_wide_characters_and_their_continuation_cells() {
    let mut terminal = VirtualTerminal::new(10, 3);
    // CJK ideograph is width-2.
    terminal.write("中");

    let lead = terminal.cell(0, 0).expect("lead cell should exist");
    assert_eq!(lead.contents, "中");
    assert!(lead.is_wide, "CJK ideograph should be marked wide");
    assert!(!lead.is_wide_continuation);

    let cont = terminal.cell(0, 1).expect("continuation cell should exist");
    assert!(
        cont.is_wide_continuation,
        "cell after a wide char should be a continuation",
    );
}

#[test]
fn cell_returns_none_for_out_of_bounds_coordinates() {
    let terminal = VirtualTerminal::new(10, 3);
    assert!(terminal.cell(0, 10).is_none());
    assert!(terminal.cell(3, 0).is_none());
}

#[test]
fn cells_in_row_snapshots_every_column_left_to_right() {
    let mut terminal = VirtualTerminal::new(5, 2);
    // Two styled chars, then padding. Exercises all three common shapes:
    // a styled lead cell, a plain cell, and empty trailing cells.
    terminal.write("\x1b[1mA\x1b[0mb");

    let row = terminal.cells_in_row(0).expect("row 0 should be in bounds");
    assert_eq!(row.len(), 5, "one cell per column");
    assert_eq!(row[0].contents, "A");
    assert!(row[0].bold);
    assert_eq!(row[1].contents, "b");
    assert!(!row[1].bold);
    // Trailing cells are empty placeholders.
    for cell in &row[2..] {
        assert_eq!(cell.contents, "");
        assert!(!cell.bold);
        assert!(matches!(cell.fg, Color::Default));
    }
}

#[test]
fn cells_in_row_includes_wide_character_continuation_cells() {
    let mut terminal = VirtualTerminal::new(4, 1);
    terminal.write("中X");

    let row = terminal.cells_in_row(0).expect("row 0 should be in bounds");
    assert_eq!(row.len(), 4);
    assert_eq!(row[0].contents, "中");
    assert!(row[0].is_wide);
    assert!(row[1].is_wide_continuation);
    assert_eq!(row[2].contents, "X");
    assert!(!row[2].is_wide);
    assert!(!row[2].is_wide_continuation);
}

#[test]
fn cells_in_row_returns_none_for_out_of_bounds_rows() {
    let terminal = VirtualTerminal::new(10, 3);
    assert!(terminal.cells_in_row(3).is_none());
    assert!(terminal.cells_in_row(99).is_none());
}

// ---------------------------------------------------------------------------
// Resize
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resize_updates_dimensions_and_queues_a_resize_event() {
    let mut terminal = VirtualTerminal::new(20, 5);

    terminal.resize(40, 10);

    assert_eq!(terminal.columns(), 40);
    assert_eq!(terminal.rows(), 10);
    // Viewport vector now has 10 rows.
    assert_eq!(terminal.viewport().len(), 10);

    // A Resize event was queued for the Tui to observe.
    let mut stream = terminal
        .take_input_stream()
        .expect("take_input_stream should return Some");
    let event = stream.next().await.expect("resize should queue an event");
    assert!(matches!(event, InputEvent::Resize(40, 10)));
}

// ---------------------------------------------------------------------------
// Kitty protocol flag
// ---------------------------------------------------------------------------

#[test]
fn kitty_protocol_active_defaults_to_true_and_is_settable() {
    let terminal = VirtualTerminal::new(10, 3);
    assert!(
        terminal.kitty_protocol_active(),
        "Kitty protocol should default to active for tests",
    );

    terminal.set_kitty_protocol_active(false);
    assert!(!terminal.kitty_protocol_active());

    terminal.set_kitty_protocol_active(true);
    assert!(terminal.kitty_protocol_active());
}

#[test]
fn kitty_protocol_active_flag_is_shared_across_clones() {
    // The flag lives in the shared `Rc<RefCell<_>>` state, so toggling it on
    // one handle must be observable on any clone. This is the same plumbing
    // guarantee the other shared fields rely on — guarding it here prevents
    // a subtle regression where the flag could accidentally grow per-handle
    // storage if the State struct were split up.
    let terminal = VirtualTerminal::new(10, 3);
    let mirror = terminal.clone();

    terminal.set_kitty_protocol_active(false);
    assert!(!mirror.kitty_protocol_active());

    mirror.set_kitty_protocol_active(true);
    assert!(terminal.kitty_protocol_active());
}

#[test]
fn terminal_trait_default_kitty_protocol_active_is_false() {
    // The trait's default implementation returns `false`, so a hand-rolled
    // minimal `Terminal` impl that doesn't participate in the negotiation
    // (i.e., never pushed the Kitty enhancement flags) reports as inactive.
    // `ProcessTerminal` and `VirtualTerminal` each override, but the
    // default must stay conservative so consumers can't accidentally
    // assume presence.
    struct NoopTerminal;
    impl Terminal for NoopTerminal {
        fn write(&mut self, _data: &str) {}
        fn columns(&self) -> u16 {
            0
        }
        fn rows(&self) -> u16 {
            0
        }
        fn move_by(&mut self, _lines: i32) {}
        fn hide_cursor(&mut self) {}
        fn show_cursor(&mut self) {}
        fn clear_line(&mut self) {}
        fn clear_from_cursor(&mut self) {}
        fn clear_screen(&mut self) {}
        fn set_title(&mut self, _title: &str) {}
        fn flush(&mut self) {}
    }

    let terminal = NoopTerminal;
    assert!(!terminal.kitty_protocol_active());
}

// ---------------------------------------------------------------------------
// Clone sharing
// ---------------------------------------------------------------------------

#[test]
fn cloned_handles_share_a_single_underlying_terminal() {
    let mut terminal = VirtualTerminal::new(10, 3);
    let mirror = terminal.clone();

    terminal.write("shared");
    terminal.set_title("t");
    terminal.hide_cursor();

    // Every read-back surface on the clone reflects the writes the
    // original handle made.
    assert_eq!(mirror.viewport()[0], "shared");
    assert_eq!(mirror.title(), "t");
    assert!(!mirror.is_cursor_visible());
    assert_eq!(mirror.writes_joined(), terminal.writes_joined());
}

// ---------------------------------------------------------------------------
// Row trimming contract
// ---------------------------------------------------------------------------

#[test]
fn viewport_strips_trailing_whitespace_from_each_row() {
    // VirtualTerminal::viewport()'s contract is "one trimmed string per
    // row" — if we ever swap the underlying parser or the parser's
    // own trimming behavior changes, every test that compares
    // `viewport()[r]` against an exact string would need to add
    // `.trim_end()` calls. Pin the contract here so a parser bump
    // can't silently regress it.
    let mut terminal = VirtualTerminal::new(20, 3);
    // Row 0: text plus four explicit trailing spaces. The viewport
    // should report just "hello" with no spaces.
    terminal.write("hello    ");
    let row = &terminal.viewport()[0];
    assert!(
        !row.ends_with(' '),
        "row 0 should have trailing spaces stripped; got {row:?}",
    );
    assert_eq!(row, "hello");
}

#[test]
fn cells_in_row_uses_the_parsers_authoritative_width() {
    // After construction the cached `state.columns` and the parser's
    // grid width should agree. `cells_in_row` should return exactly
    // that many CellInfos — not more, not fewer. This is the
    // structural property G1 protects: if the cached width were ever
    // larger than the parser's, cells_in_row would silently emit
    // `CellInfo::default()` filler for the OOB columns.
    let terminal = VirtualTerminal::new(20, 3);
    let row = terminal.cells_in_row(0).expect("row 0 should be in range");
    assert_eq!(
        row.len(),
        20,
        "cells_in_row should return exactly the parser-grid width; \
         got {} entries",
        row.len(),
    );
}

// ---------------------------------------------------------------------------
// LoggingVirtualTerminal alias
// ---------------------------------------------------------------------------

#[test]
fn logging_virtual_terminal_is_a_naming_alias_over_virtual_terminal() {
    use support::LoggingVirtualTerminal;

    // Constructing through the alias name and using it via the
    // `Terminal` trait should be indistinguishable from constructing
    // a `VirtualTerminal` directly. The alias exists to flag intent
    // ("this test is about emitted bytes"), not to introduce a new
    // type with different semantics.
    let mut terminal: LoggingVirtualTerminal = LoggingVirtualTerminal::new(10, 2);
    terminal.write("alpha");
    terminal.set_title("hi");
    terminal.hide_cursor();

    // The same writes-log surface that's on `VirtualTerminal` is
    // available through the alias name.
    let log = terminal.writes_joined();
    assert!(
        log.contains("alpha") && log.contains("\x1b]0;hi\x07") && log.contains("\x1b[?25l"),
        "expected the alias to share the writes log; got {log:?}",
    );
}
