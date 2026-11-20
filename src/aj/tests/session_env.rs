#![cfg(unix)]

use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use aj_session::{LogEntryKind, ProjectSessions, SessionLog};
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
fn cli_migrates_jsonl_sessions_into_the_global_database() {
    use aj_agent::message::AgentMessage;
    use aj_models::types::{Message, UserMessage};
    use aj_session::ThreadKind;

    let home = TempDir::new().unwrap();
    let cwd = TempDir::new_in(home.path()).unwrap();
    std::fs::create_dir(cwd.path().join(".git")).unwrap();
    let database = home.path().join(".aj/sessions.sqlite3");
    let legacy = |project: &str| {
        let store = ProjectSessions::for_project(
            database.clone(),
            home.path().join(".aj/sessions").join(project),
            project.to_owned(),
        );
        let message = AgentMessage::wire(Message::User(UserMessage::text(project)));
        let (id, _) = aj_session::jsonl_session(&store, |log| {
            log.append(
                None,
                ThreadKind::Main,
                None,
                LogEntryKind::Message { message },
            )
            .unwrap();
        });
        (store, id)
    };
    let (here, id) = legacy(cwd.path().file_name().unwrap().to_str().unwrap());
    let (other, other_id) = legacy("other");
    std::fs::write(other.sessions_dir().join("broken.jsonl"), "not json\n").unwrap();
    let run = |args: &[&str]| {
        let output = run_aj(&home, &cwd, args);
        let stdout = String::from_utf8(output.stdout).unwrap();
        (output.status.success(), stdout)
    };

    let (ok, dry) = run(&["migrate-session", &id, "--dry-run"]);
    assert!(ok && dry.contains("would import into SQLite"), "{dry}");
    assert!(!database.exists());

    // One damaged source fails the command without blocking the others.
    let (ok, all) = run(&["migrate-session", "--all"]);
    assert!(!ok, "{all}");
    assert!(
        all.contains("2 imported") && all.contains("1 failed"),
        "{all}"
    );
    let (ok, again) = run(&["migrate-session", &id]);
    assert!(
        ok && again.contains("already imported into SQLite"),
        "{again}"
    );

    assert!(here.in_database(&id).unwrap());
    assert!(other.in_database(&other_id).unwrap());
    let (ok, listed) = run(&["list-sessions"]);
    assert!(ok && listed.lines().count() == 1, "{listed}");

    // Exactly one of an ID or --all selects what to migrate.
    for args in [&["migrate-session"][..], &["migrate-session", &id, "--all"]] {
        assert_eq!(run_aj(&home, &cwd, args).status.code(), Some(2));
    }
}

#[test]
fn session_env_is_refused_before_utility_dispatch_side_effects() {
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
        stderr.contains("unexpected argument '--env'"),
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
fn parser_refuses_serve_and_gateway_env_before_any_state() {
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
            stderr.contains("unexpected argument '--env'"),
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
#[cfg_attr(
    not(feature = "slow-tests"),
    ignore = "slow: paced print-mode subprocesses"
)]
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
    let persistence = ProjectSessions::for_project(
        home.path().join(".aj/sessions.sqlite3"),
        sessions,
        "default".into(),
    );
    let mut logs = persistence.list_sessions().expect("session records");
    assert_eq!(logs.len(), 1, "initial print minted one session: {logs:?}");
    let session_id = logs.pop().expect("one log").session_id;

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

    let log = SessionLog::resume(&persistence, &session_id).expect("resume result log");
    assert_eq!(log.session_env(), None);
    assert_eq!(
        log.entries_in_order()
            .iter()
            .filter(|entry| matches!(entry.entry, LogEntryKind::EnvChange { .. }))
            .count(),
        0,
        "print continue backfilled a creation entry"
    );
}
