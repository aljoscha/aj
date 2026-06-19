//! Smoke tests for [`aj_tui::terminal::ProcessTerminal`] — dimensions
//! and the OSC 9;4 progress indicator.
//!
//! Under `cargo test`, the test binary's stdout is a pipe to the
//! cargo parent, so `crossterm::terminal::size()` returns `Err` and
//! the env-var fallback is the path under test.

use aj_tui_testkit as support;

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use aj_tui::terminal::{ProcessTerminal, Terminal, WRITE_LOG_ENV};
use support::with_env;

fn count_substring(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut idx = 0;
    while let Some(found) = haystack[idx..].find(needle) {
        count += 1;
        idx += found + needle.len();
    }
    count
}

fn read_log(path: &PathBuf) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
#[serial_test::serial]
fn columns_and_rows_fall_back_to_env_vars_when_size_probe_fails() {
    let _guard = with_env(&[("COLUMNS", Some("123")), ("LINES", Some("45"))]);

    let terminal = ProcessTerminal::new();

    assert_eq!(terminal.columns(), 123, "should read COLUMNS env var");
    assert_eq!(terminal.rows(), 45, "should read LINES env var");
}

// ---------------------------------------------------------------------------
// OSC 9;4 progress indicator
// ---------------------------------------------------------------------------
//
// The ConEmu / Windows Terminal progress protocol uses state code `3`
// for indeterminate ("pulsing") progress and state code `0` to clear.
// State `1;<value>` is "normal progress at <value>%", a different
// visual that we deliberately do *not* emit for the indeterminate
// case.
//
// These tests serialize on `serial_test::serial` because they mutate
// the process-wide `WRITE_LOG_ENV` variable to observe the
// keepalive thread's stdout writes through the disk log.

const ACTIVE_SEQ: &str = "\x1b]9;4;3\x07";
const CLEAR_SEQ: &str = "\x1b]9;4;0;\x07";

#[test]
#[serial_test::serial]
fn set_progress_true_emits_the_indeterminate_sequence_immediately() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("progress.log");
    let _guard = with_env(&[(WRITE_LOG_ENV, Some(log_path.to_str().unwrap()))]);

    let mut terminal = ProcessTerminal::new();
    terminal.set_progress(true);

    // The on-call emit lands synchronously through `Terminal::write`,
    // so the log carries the active sequence before the keepalive
    // thread has had a chance to fire a second time.
    let log = read_log(&log_path);
    assert!(
        log.contains(ACTIVE_SEQ),
        "expected indeterminate sequence in log, got {:?}",
        log
    );

    // Tear down the keepalive thread cleanly so the test doesn't leak
    // a worker.
    terminal.set_progress(false);
}

#[test]
#[serial_test::serial]
fn set_progress_false_emits_the_clear_sequence_and_stops_re_emitting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("progress.log");
    let _guard = with_env(&[(WRITE_LOG_ENV, Some(log_path.to_str().unwrap()))]);

    let mut terminal = ProcessTerminal::new();
    terminal.set_progress(true);
    terminal.set_progress(false);

    // Wait through more than one keepalive interval. If the worker
    // is still alive (regression: keepalive not cancelled), it will
    // append another active sequence; the test fails.
    thread::sleep(Duration::from_millis(1300));

    let log = read_log(&log_path);
    assert!(
        log.contains(CLEAR_SEQ),
        "clear sequence missing from log {:?}",
        log
    );
    // Exactly one active sequence (the synchronous on-call emit);
    // no follow-on emissions after the cancel.
    assert_eq!(
        count_substring(&log, ACTIVE_SEQ),
        1,
        "keepalive should not re-emit after set_progress(false); log = {:?}",
        log,
    );
}

#[test]
#[serial_test::serial]
fn set_progress_true_keeps_re_emitting_the_indeterminate_sequence() {
    // Holds the indicator on long enough to observe the keepalive
    // thread firing more than once. The interval is 1000ms; we wait
    // for ~2.3s so we comfortably catch at least two re-emissions
    // beyond the initial synchronous write.
    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("progress.log");
    let _guard = with_env(&[(WRITE_LOG_ENV, Some(log_path.to_str().unwrap()))]);

    let mut terminal = ProcessTerminal::new();
    terminal.set_progress(true);
    thread::sleep(Duration::from_millis(2300));
    terminal.set_progress(false);

    let log = read_log(&log_path);
    let emissions = count_substring(&log, ACTIVE_SEQ);
    assert!(
        emissions >= 3,
        "expected at least 3 indeterminate emissions \
         (initial + two keepalives over 2.3s), got {} in log {:?}",
        emissions,
        log,
    );
}

#[test]
#[serial_test::serial]
fn drop_stops_the_keepalive_thread_and_emits_clear() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("progress.log");
    let _guard = with_env(&[(WRITE_LOG_ENV, Some(log_path.to_str().unwrap()))]);

    {
        let mut terminal = ProcessTerminal::new();
        terminal.set_progress(true);
        // Drop scope: terminal goes away here, which must stop the
        // worker and emit the clear sequence as part of `stop()`.
    }

    // Allow ample time for any leaked worker to fire; if `Drop` did
    // its job, no further active emissions land.
    thread::sleep(Duration::from_millis(1300));

    let log = read_log(&log_path);
    assert!(
        log.contains(CLEAR_SEQ),
        "clear sequence missing from log after drop {:?}",
        log,
    );
    assert_eq!(
        count_substring(&log, ACTIVE_SEQ),
        1,
        "keepalive should not survive Drop; log = {:?}",
        log,
    );
}

#[test]
#[serial_test::serial]
fn redundant_set_progress_true_does_not_spawn_a_second_keepalive() {
    // Calling `set_progress(true)` twice in a row must re-emit the
    // active sequence both times (so a transient terminal that
    // missed the first one has another chance) but must keep using
    // the existing keepalive thread. The behavioral signal is the
    // emission count after the cancel: any additional thread would
    // produce duplicate keepalive writes during the wait window.
    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("progress.log");
    let _guard = with_env(&[(WRITE_LOG_ENV, Some(log_path.to_str().unwrap()))]);

    let mut terminal = ProcessTerminal::new();
    terminal.set_progress(true);
    terminal.set_progress(true);
    thread::sleep(Duration::from_millis(1300));
    terminal.set_progress(false);

    let log = read_log(&log_path);
    let emissions = count_substring(&log, ACTIVE_SEQ);
    // 2 synchronous emits + at most 2 keepalive ticks in 1.3s = 4.
    // A leaked second keepalive would push this to 6+. Strict upper
    // bound at 4 to catch the regression.
    assert!(
        emissions <= 4,
        "redundant set_progress(true) must not double the keepalive; \
         emissions = {}, log = {:?}",
        emissions,
        log,
    );
}
