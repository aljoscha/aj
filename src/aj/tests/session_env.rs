#![cfg(unix)]

use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use aj_session::{ConversationEntryKind, ConversationLog, ConversationPersistence};
use tempfile::TempDir;

fn run_aj(home: &TempDir, cwd: &TempDir, args: &[&str]) -> Output {
    let log_path = home.path().join("must-not-exist-before-preflight.log");
    let child = Command::new(env!("CARGO_BIN_EXE_aj"))
        .args(args)
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env_remove("AJ_THINKING")
        .env_remove("AJ_SPEED")
        .env_remove("AJ_LISTEN")
        .env_remove("AJ_NAME")
        .env("AJ_LOG_FILE", log_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run fresh aj binary");
    output_before_deadline(child, Duration::from_secs(5))
}

fn output_before_deadline(mut child: Child, timeout: Duration) -> Output {
    let mut stdout = child.stdout.take().expect("fresh process stdout was piped");
    let mut stderr = child.stderr.take().expect("fresh process stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .read_to_end(&mut bytes)
            .expect("read fresh process stdout");
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .read_to_end(&mut bytes)
            .expect("read fresh process stderr");
        bytes
    });

    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().expect("poll fresh process") {
            break (status, false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            break (child.wait().expect("reap timed-out fresh process"), true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = Output {
        status,
        stdout: stdout_reader.join().expect("stdout reader panicked"),
        stderr: stderr_reader.join().expect("stderr reader panicked"),
    };
    if timed_out {
        panic!(
            "fresh process did not exit within {} seconds: stdout={:?} stderr={:?}",
            timeout.as_secs(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    output
}

#[test]
fn deadline_helper_drains_output_while_the_child_runs() {
    let child = Command::new("/bin/sh")
        .args(["-c", "printf '%131072s' ''"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run large-output fixture");

    let output = output_before_deadline(child, Duration::from_secs(5));
    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 131_072);
    assert!(output.stderr.is_empty());
}

#[test]
fn malformed_global_env_is_refused_before_utility_dispatch_side_effects() {
    let home = TempDir::new().expect("isolated home");
    let cwd = TempDir::new().expect("non-repository working directory");
    let output = run_aj(
        &home,
        &cwd,
        &["--env", "MISSING", "list-sessions", "--env", "OK=value"],
    );

    assert!(!output.status.success(), "malformed --env was accepted");
    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
    assert!(
        stderr.contains("--env: environment argument \"MISSING\" must contain '='"),
        "unexpected refusal: {stderr:?}"
    );
    assert!(
        !home.path().join(".aj").exists(),
        "utility dispatch created config or session state before validating --env"
    );
    assert!(
        !home
            .path()
            .join("must-not-exist-before-preflight.log")
            .exists(),
        "utility dispatch opened AJ_LOG_FILE before validating --env"
    );
}

#[test]
fn main_preflight_refuses_serve_and_gateway_env_before_any_state() {
    for mode in ["serve", "gateway"] {
        let home = TempDir::new().expect("isolated home");
        let cwd = TempDir::new().expect("non-repository working directory");
        let output = run_aj(
            &home,
            &cwd,
            &[
                mode,
                "--listen=127.0.0.1:0",
                "--env",
                "BEADS_ACTOR=session-actor",
            ],
        );

        assert!(!output.status.success(), "{mode} accepted create-only env");
        let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
        assert!(
            stderr.contains("--env is stated per session create"),
            "unexpected {mode} refusal: {stderr:?}"
        );
        assert!(
            !home.path().join(".aj").exists(),
            "{mode} created config state before refusing launch env"
        );
        assert!(
            !home
                .path()
                .join("must-not-exist-before-preflight.log")
                .exists(),
            "{mode} opened AJ_LOG_FILE before refusing launch env"
        );
    }
}

#[test]
fn print_continue_reports_that_env_is_create_only_without_backfilling_the_log() {
    let home = TempDir::new().expect("isolated home");
    let cwd = TempDir::new().expect("non-repository working directory");
    let first = run_aj(
        &home,
        &cwd,
        &["--print", "--scripted", "streaming-text", "legacy"],
    );
    assert!(
        first.status.success(),
        "initial print failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let sessions = home.path().join(".aj/sessions/default");
    let mut logs = std::fs::read_dir(&sessions)
        .expect("sessions directory")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")).then_some(path)
        })
        .collect::<Vec<_>>();
    assert_eq!(logs.len(), 1, "initial print minted one session: {logs:?}");
    let path = logs.pop().expect("one log");
    let session_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("session id")
        .to_string();

    let resumed = run_aj(
        &home,
        &cwd,
        &[
            "--print",
            "--scripted",
            "streaming-text",
            "--env",
            "AJ_SESSION_ENV_TEST_IDENTITY=other",
            "continue",
            &session_id,
            "resume",
        ],
    );
    assert!(
        resumed.status.success(),
        "continued print failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let stderr = String::from_utf8(resumed.stderr).expect("stderr utf8");
    assert!(
        stderr.contains("--env applies only to sessions this run creates")
            && stderr.contains("keeps the environment recorded in its own log"),
        "create-only notice missing from stderr: {stderr:?}"
    );

    let persistence = ConversationPersistence::new(sessions);
    let log = ConversationLog::resume(&persistence, &session_id).expect("resume result log");
    assert_eq!(log.session_env(), None);
    assert_eq!(
        log.entries_in_order()
            .iter()
            .filter(|entry| matches!(entry.entry, ConversationEntryKind::EnvChange { .. }))
            .count(),
        0,
        "print continue backfilled a creation record"
    );
}
