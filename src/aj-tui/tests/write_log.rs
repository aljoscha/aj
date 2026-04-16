//! Tests for the `AJ_TUI_WRITE_LOG` environment variable wired up on
//! [`aj_tui::terminal::ProcessTerminal`].
//!
//! Every test serializes on [`serial_test::serial`] because they mutate
//! the process-wide environment, and most of them also exercise
//! `ProcessTerminal::write` which writes to stdout. Test harness
//! captures stdout for each test, so the test output stays clean, but
//! two tests running in parallel would still race on the env var and
//! on the captured log path.

mod support;

use std::fs;
use std::path::PathBuf;

use aj_tui::terminal::{ProcessTerminal, Terminal, WRITE_LOG_ENV};

use support::with_env;

/// Build a path under the per-test temp directory. Kept small so
/// each test reads as: create a tempdir, place the log inside it,
/// assert on the contents, drop the tempdir.
fn tmp_path(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    (dir, path)
}

// ---------------------------------------------------------------------------
// Unset env var
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn unset_env_leaves_the_write_log_path_empty() {
    let _guard = with_env(&[(WRITE_LOG_ENV, None)]);
    let terminal = ProcessTerminal::new();
    assert!(
        terminal.write_log_path().is_none(),
        "no log path should be set when the env var is missing",
    );
}

#[test]
#[serial_test::serial]
fn empty_string_env_is_treated_as_unset() {
    let _guard = with_env(&[(WRITE_LOG_ENV, Some(""))]);
    let terminal = ProcessTerminal::new();
    assert!(
        terminal.write_log_path().is_none(),
        "empty env value should behave the same as unset",
    );
}

// ---------------------------------------------------------------------------
// File-path mode
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn file_path_env_appends_every_write_to_the_named_file() {
    let (_dir, log_path) = tmp_path("writes.log");
    let _guard = with_env(&[(WRITE_LOG_ENV, Some(log_path.to_str().unwrap()))]);

    let mut terminal = ProcessTerminal::new();
    assert_eq!(terminal.write_log_path(), Some(log_path.as_path()));

    terminal.write("first ");
    terminal.write("second ");
    terminal.write("\x1b[1mbold\x1b[0m");

    let contents = fs::read_to_string(&log_path).expect("log written");
    assert_eq!(contents, "first second \x1b[1mbold\x1b[0m");
}

#[test]
#[serial_test::serial]
fn file_path_env_appends_rather_than_truncates_across_instances() {
    let (_dir, log_path) = tmp_path("append.log");
    // Prime the file with existing content so we can observe append vs
    // truncate.
    fs::write(&log_path, "PRE-EXISTING\n").unwrap();

    let _guard = with_env(&[(WRITE_LOG_ENV, Some(log_path.to_str().unwrap()))]);
    let mut terminal = ProcessTerminal::new();
    terminal.write("new-write");

    let contents = fs::read_to_string(&log_path).expect("log written");
    assert_eq!(
        contents, "PRE-EXISTING\nnew-write",
        "existing content must be preserved; new writes append",
    );
}

#[test]
#[serial_test::serial]
fn nonexistent_file_path_is_created_on_first_write() {
    // The resolver treats a value that doesn't exist yet as a file
    // path. Appending to a nonexistent file should create it on the
    // first write.
    let (_dir, log_path) = tmp_path("fresh.log");
    assert!(!log_path.exists(), "precondition: file does not exist yet");

    let _guard = with_env(&[(WRITE_LOG_ENV, Some(log_path.to_str().unwrap()))]);
    let mut terminal = ProcessTerminal::new();
    terminal.write("hello");

    let contents = fs::read_to_string(&log_path).expect("log created");
    assert_eq!(contents, "hello");
}

// ---------------------------------------------------------------------------
// Directory mode
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn directory_env_writes_into_a_generated_per_process_file() {
    // Point the env var at an existing directory; the resolver must
    // produce a file under it named aj-tui-<timestamp>-<pid>.log.
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = with_env(&[(WRITE_LOG_ENV, Some(dir.path().to_str().unwrap()))]);

    let mut terminal = ProcessTerminal::new();
    let log_path = terminal
        .write_log_path()
        .expect("directory mode should produce a log path")
        .to_path_buf();

    // The file must live inside the directory…
    assert!(
        log_path.starts_with(dir.path()),
        "log file must live under the env-supplied directory; got {:?}",
        log_path,
    );
    // …and have a name that starts with "aj-tui-" so it's obvious at
    // a glance.
    let filename = log_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    assert!(
        filename.starts_with("aj-tui-") && filename.ends_with(".log"),
        "unexpected generated filename {:?}",
        filename,
    );
    assert!(
        filename.contains(&format!("-{}.log", std::process::id())),
        "filename should embed the current pid; got {:?}",
        filename,
    );

    terminal.write("via-directory");
    let contents = fs::read_to_string(&log_path).expect("generated log written");
    assert_eq!(contents, "via-directory");
}

#[test]
#[serial_test::serial]
fn two_process_terminals_sharing_a_path_both_append() {
    let (_dir, log_path) = tmp_path("shared.log");
    let _guard = with_env(&[(WRITE_LOG_ENV, Some(log_path.to_str().unwrap()))]);

    let mut first = ProcessTerminal::new();
    let mut second = ProcessTerminal::new();

    first.write("A");
    second.write("B");
    first.write("C");

    let contents = fs::read_to_string(&log_path).expect("log written");
    assert_eq!(contents, "ABC");
}
