#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::process::{ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use aj_wire::{
    DirectoryHost, Frame, Hello, PROTOCOL_VERSION, QueueCounts, SessionList, SessionSummary,
};
use axum::{Json, Router, http::header, routing::get};
use chrono::Utc;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use vaxis::Winsize;
use vaxis::widgets::terminal::{command::Command, pty::Pty};

struct TestHost {
    url: String,
    session_reads: Arc<AtomicUsize>,
    event_reads: Arc<AtomicUsize>,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<Result<(), std::io::Error>>,
}

#[derive(Clone, Copy)]
enum PeerKind {
    Host,
    Gateway,
}

impl TestHost {
    async fn start(kind: PeerKind) -> Self {
        let session_reads = Arc::new(AtomicUsize::new(0));
        let event_reads = Arc::new(AtomicUsize::new(0));
        let reads = Arc::clone(&session_reads);
        let events = Arc::clone(&event_reads);
        let gateway = matches!(kind, PeerKind::Gateway);
        let listed = SessionList {
            sessions: vec![SessionSummary {
                id: if gateway {
                    "host-id:latest-session".to_string()
                } else {
                    "latest-session".to_string()
                },
                live: false,
                working: false,
                queued: QueueCounts::default(),
                tasks: 0,
                last_seq: None,
                last_activity: Utc::now(),
                tag: None,
                host: gateway.then(|| "host-id".to_string()),
                unreachable: false,
                archived: false,
                locked: false,
                lock_generation: None,
            }],
            hosts: gateway
                .then(|| {
                    vec![DirectoryHost {
                        id: Some("host-id".to_string()),
                        address: None,
                        name: Some("workstation".to_string()),
                        working_directory: Some(std::path::PathBuf::from("/workstation")),
                        unreachable: false,
                    }]
                })
                .unwrap_or_default(),
        };
        let hello = Hello {
            protocol: PROTOCOL_VERSION,
            capabilities: Vec::new(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            host_id: if gateway { "gateway" } else { "test-host" }.to_string(),
            working_directory: (!gateway).then(|| std::path::PathBuf::from("/test-host")),
            name: None,
        };
        let app = Router::new()
            .route(
                "/v1/hello",
                get(move || {
                    let hello = hello.clone();
                    async { Json(hello) }
                }),
            )
            .route(
                "/v1/sessions",
                get(move || {
                    reads.fetch_add(1, Ordering::SeqCst);
                    let listed = listed.clone();
                    async { Json(listed) }
                }),
            )
            // This is the in-band refusal a real host serves for an unknown
            // attach. Reaching it means the preflight was skipped and allows
            // the child to proceed toward terminal initialization.
            .route(
                "/v1/events",
                get(move || {
                    events.fetch_add(1, Ordering::SeqCst);
                    async {
                        let frame = Frame::Error {
                            session: "nosuchsession".to_string(),
                            epoch: None,
                            code: "unknown_session".to_string(),
                            message: "unknown session nosuchsession".to_string(),
                            lock_generation: None,
                        };
                        let data = serde_json::to_string(&frame).expect("encode the refusal frame");
                        (
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            format!("data: {data}\n\n"),
                        )
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback test host");
        let address = listener.local_addr().expect("read the bound address");
        let (shutdown, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stopped.await;
                })
                .await
        });
        Self {
            url: format!("http://{address}"),
            session_reads,
            event_reads,
            shutdown,
            task,
        }
    }

    async fn stop(self) -> (usize, usize) {
        self.shutdown.send(()).expect("the test host is running");
        self.task
            .await
            .expect("join the test host")
            .expect("stop the test host");
        (
            self.session_reads.load(Ordering::SeqCst),
            self.event_reads.load(Ordering::SeqCst),
        )
    }
}

struct ChildOutput {
    status: ExitStatus,
    stderr: String,
    terminal: Vec<u8>,
    timed_out: bool,
}

struct DetachedOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

async fn run_connect(
    home: &TempDir,
    url: &str,
    id: &str,
    host: Option<&str>,
    case: usize,
) -> ChildOutput {
    let home = home.path().to_path_buf();
    let url = url.to_string();
    let id = id.to_string();
    let host = host.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let stderr_path = home.join(format!("stderr-{case}"));
        let mut args = vec![
            format!("HOME={}", home.display()),
            "AJ_LOG_FILE=/dev/null".to_string(),
            "RUST_LOG=".to_string(),
            "RUST_BACKTRACE=0".to_string(),
            "TERM=xterm-256color".to_string(),
            format!("AJ_TEST_STDERR={}", stderr_path.display()),
            "/bin/sh".to_string(),
            "-c".to_string(),
            "exec \"$@\" 2>\"$AJ_TEST_STDERR\"".to_string(),
            "sh".to_string(),
            env!("CARGO_BIN_EXE_aj").to_string(),
            "connect".to_string(),
            url,
            id.clone(),
        ];
        if let Some(host) = host {
            args.extend(["--host".to_string(), host]);
        }
        let args = args.into_iter().map(OsString::from).collect();
        let mut command = Command::new(OsString::from("env"), args);
        command.set_working_directory(home);
        let pty = Pty::open().expect("open a controlling pty");
        pty.set_size(Winsize {
            rows: 24,
            cols: 80,
            x_pixel: 0,
            y_pixel: 0,
        })
        .expect("size the controlling pty");
        let mut child = command.spawn(&pty.slave).expect("spawn aj connect");
        drop(pty.slave);
        let reader = thread::spawn(move || read_terminal(pty.master));

        let deadline = Instant::now() + Duration::from_secs(5);
        let (status, timed_out) = loop {
            if let Some(status) = child.try_wait().expect("poll aj connect") {
                break (status, false);
            }
            if Instant::now() >= deadline {
                child.kill().expect("kill a stuck aj connect");
                break (child.wait().expect("reap a stuck aj connect"), true);
            }
            thread::sleep(Duration::from_millis(10));
        };
        let terminal = reader.join().expect("join the pty reader");
        let stderr = std::fs::read_to_string(&stderr_path)
            .unwrap_or_else(|err| panic!("read stderr for {id:?}: {err}"));
        ChildOutput {
            status,
            stderr,
            terminal,
            timed_out,
        }
    })
    .await
    .expect("join the aj connect runner")
}

/// Run connect in a fresh process session with no controlling terminal.
/// Correct preflight needs no terminal, while reaching `PosixTty::new` first
/// fails at its `/dev/tty` acquisition and changes the observed refusal.
async fn run_connect_without_terminal(home: &TempDir, url: &str, id: &str) -> DetachedOutput {
    let home = home.path().to_path_buf();
    let url = url.to_string();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || {
        let stdout_path = home.join("detached-stdout");
        let stderr_path = home.join("detached-stderr");
        let stdout = File::create(&stdout_path).expect("create detached stdout");
        let stderr = File::create(&stderr_path).expect("create detached stderr");
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_aj"));
        command
            .args(["connect", &url, &id])
            .current_dir(&home)
            .env("HOME", &home)
            .env("AJ_LOG_FILE", "/dev/null")
            .env("RUST_LOG", "")
            .env("RUST_BACKTRACE", "0")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        // SAFETY: the post-fork hook calls only setsid, the same
        // async-signal-safe operation used by vaxis's PTY launcher.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid()
                    .map(|_| ())
                    .map_err(std::io::Error::from)
            });
        }
        let mut child = command.spawn().expect("spawn detached aj connect");
        let deadline = Instant::now() + Duration::from_secs(5);
        let (status, timed_out) = loop {
            if let Some(status) = child.try_wait().expect("poll detached aj connect") {
                break (status, false);
            }
            if Instant::now() >= deadline {
                child.kill().expect("kill a stuck detached aj connect");
                break (
                    child.wait().expect("reap a stuck detached aj connect"),
                    true,
                );
            }
            thread::sleep(Duration::from_millis(10));
        };
        DetachedOutput {
            status,
            stdout: std::fs::read_to_string(&stdout_path).expect("read detached stdout"),
            stderr: std::fs::read_to_string(&stderr_path).expect("read detached stderr"),
            timed_out,
        }
    })
    .await
    .expect("join the detached aj connect runner")
}

fn read_terminal(master: OwnedFd) -> Vec<u8> {
    let mut file = File::from(master);
    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => output.extend_from_slice(&buffer[..read]),
            // Linux reports EIO after every slave descriptor closes.
            Err(err) if err.raw_os_error() == Some(5) => break,
            Err(err) => panic!("read the controlling pty: {err}"),
        }
    }
    output
}

fn assert_preterminal_refusal(
    id: &str,
    expected_stderr: &str,
    output: ChildOutput,
    session_reads: usize,
    event_reads: usize,
) {
    assert!(
        !output.timed_out,
        "aj connect for {id:?} took the terminal instead of refusing",
    );
    let code = output
        .status
        .code()
        .unwrap_or_else(|| panic!("aj connect for {id:?} ended without an exit code"));
    assert_ne!(code, 0, "aj connect for {id:?} reported success");
    assert_eq!(
        output.stderr, expected_stderr,
        "the refusal for {id:?} was not a plain stderr sentence",
    );
    assert!(
        !output
            .terminal
            .windows(vaxis::ctlseqs::SMCUP.len())
            .any(|window| window == vaxis::ctlseqs::SMCUP.as_bytes()),
        "aj connect for {id:?} entered the alternate screen before refusing",
    );
    assert_eq!(
        session_reads, 1,
        "the explicit id {id:?} should cost exactly one session-list read",
    );
    assert_eq!(
        event_reads, 0,
        "the explicit id {id:?} reached attach instead of refusing",
    );
}

#[tokio::test]
async fn an_unknown_explicit_session_refuses_before_the_terminal_and_exits_nonzero() {
    let home = TempDir::new().expect("temporary home");
    for (case, (id, expected_stderr)) in [
        (
            "nosuchsession",
            "Error: unknown session \"nosuchsession\"\n",
        ),
        ("", "Error: unknown session \"\"\n"),
        (
            "odd\"\\\n\t\u{1b}",
            "Error: unknown session \"odd\\\"\\\\\\n\\t\\u{1b}\"\n",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let host = TestHost::start(PeerKind::Host).await;
        let output = run_connect(&home, &host.url, id, None, case).await;
        let (session_reads, event_reads) = host.stop().await;

        assert_preterminal_refusal(id, expected_stderr, output, session_reads, event_reads);
    }
}

#[tokio::test]
async fn an_unknown_explicit_session_refuses_without_acquiring_a_terminal() {
    let home = TempDir::new().expect("temporary home");
    let host = TestHost::start(PeerKind::Host).await;

    let output = run_connect_without_terminal(&home, &host.url, "nosuchsession").await;
    let (session_reads, event_reads) = host.stop().await;

    assert!(!output.timed_out, "the detached refusal did not finish");
    assert_ne!(
        output.status.code().expect("a normal exit code"),
        0,
        "the detached refusal reported success",
    );
    assert_eq!(
        output.stderr, "Error: unknown session \"nosuchsession\"\n",
        "terminal acquisition replaced the preflight refusal",
    );
    assert_eq!(output.stdout, "", "the detached refusal wrote to stdout");
    assert_eq!(
        session_reads, 1,
        "the detached preflight refetched sessions"
    );
    assert_eq!(event_reads, 0, "the detached preflight reached attach");
}

#[tokio::test]
async fn a_gateway_host_and_unknown_explicit_session_share_one_directory_read() {
    let home = TempDir::new().expect("temporary home");
    let id = "host-id:nosuchsession";
    let gateway = TestHost::start(PeerKind::Gateway).await;

    let output = run_connect(&home, &gateway.url, id, Some("host-id"), 0).await;
    let (session_reads, event_reads) = gateway.stop().await;

    assert_preterminal_refusal(
        id,
        "Error: unknown session \"host-id:nosuchsession\"\n",
        output,
        session_reads,
        event_reads,
    );
}

#[tokio::test]
async fn a_gateway_still_validates_host_before_accepting_a_listed_session() {
    let home = TempDir::new().expect("temporary home");
    let id = "host-id:latest-session";
    let gateway = TestHost::start(PeerKind::Gateway).await;

    let output = run_connect(&home, &gateway.url, id, Some("missing-host"), 0).await;
    let (session_reads, event_reads) = gateway.stop().await;

    assert!(!output.timed_out, "the host refusal did not finish");
    assert_ne!(
        output.status.code().expect("a normal exit code"),
        0,
        "the invalid host reported success",
    );
    assert_eq!(
        output.stderr,
        "Error: --host: \"missing-host\" names no host here: host-id (workstation)\n",
    );
    assert!(
        !output
            .terminal
            .windows(vaxis::ctlseqs::SMCUP.len())
            .any(|window| window == vaxis::ctlseqs::SMCUP.as_bytes()),
        "the invalid host reached terminal setup",
    );
    assert_eq!(session_reads, 1, "host validation refetched the directory");
    assert_eq!(event_reads, 0, "the invalid host reached attach");
}
