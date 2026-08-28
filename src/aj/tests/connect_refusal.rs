#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::process::ExitStatus;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use aj_wire::{Frame, Hello, PROTOCOL_VERSION, QueueCounts, SessionList, SessionSummary};
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

impl TestHost {
    async fn start() -> Self {
        let session_reads = Arc::new(AtomicUsize::new(0));
        let event_reads = Arc::new(AtomicUsize::new(0));
        let reads = Arc::clone(&session_reads);
        let events = Arc::clone(&event_reads);
        let listed = SessionList {
            sessions: vec![SessionSummary {
                id: "latest-session".to_string(),
                live: false,
                working: false,
                queued: QueueCounts::default(),
                tasks: 0,
                last_seq: None,
                last_activity: Utc::now(),
                tag: None,
                host: None,
                unreachable: false,
                archived: false,
                locked: false,
                lock_generation: None,
            }],
            hosts: Vec::new(),
        };
        let app = Router::new()
            .route(
                "/v1/hello",
                get(|| async {
                    Json(Hello {
                        protocol: PROTOCOL_VERSION,
                        capabilities: Vec::new(),
                        app_version: env!("CARGO_PKG_VERSION").to_string(),
                        host_id: "test-host".to_string(),
                        working_directory: Some(std::path::PathBuf::from("/test-host")),
                        name: None,
                    })
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

async fn run_connect(home: &TempDir, url: &str, id: &str, case: usize) -> ChildOutput {
    let home = home.path().to_path_buf();
    let url = url.to_string();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || {
        let stderr_path = home.join(format!("stderr-{case}"));
        let args = [
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
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
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

#[tokio::test]
async fn an_unknown_explicit_session_refuses_before_the_terminal_and_exits_nonzero() {
    let home = TempDir::new().expect("temporary home");
    for (case, id) in ["nosuchsession", ""].into_iter().enumerate() {
        let host = TestHost::start().await;
        let output = run_connect(&home, &host.url, id, case).await;
        let (session_reads, event_reads) = host.stop().await;

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
            output.stderr,
            format!("Error: unknown session {id:?}\n"),
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
}
