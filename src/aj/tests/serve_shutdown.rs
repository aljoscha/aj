#![cfg(unix)]

use std::collections::HashSet;
use std::process::Stdio;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

const START_DEADLINE: Duration = Duration::from_secs(20);
const ESCALATION_DEADLINE: Duration = Duration::from_secs(2);
const HEALTHY_DEADLINE: Duration = Duration::from_secs(5);

struct ScratchServer {
    child: Child,
    stderr: Option<tokio::process::ChildStderr>,
    _home: TempDir,
    url: String,
}

impl ScratchServer {
    async fn start(mode: &str, banner_prefix: &str) -> Self {
        let home = TempDir::new().expect("isolated HOME");
        let mut child = Command::new(env!("CARGO_BIN_EXE_aj"))
            .args(["--listen=127.0.0.1:0", "--auth=open", mode])
            .current_dir(home.path())
            .env("HOME", home.path())
            .env_remove("AJ_LOG_FILE")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("start scratch server");
        let stdout = child.stdout.take().expect("server stdout");
        let mut stdout = BufReader::new(stdout);
        let mut banner = String::new();
        tokio::time::timeout(START_DEADLINE, stdout.read_line(&mut banner))
            .await
            .expect("server printed its banner in time")
            .expect("read server banner");
        assert!(
            banner.starts_with(banner_prefix),
            "the scratch process reached the serving state: {banner:?}"
        );
        let url = banner
            .trim()
            .rsplit_once(" on ")
            .map(|(_, url)| url.to_string())
            .expect("banner ends in the listening URL");
        let stderr = child.stderr.take();
        Self {
            child,
            stderr,
            _home: home,
            url,
        }
    }

    fn signal(&self, signal: Signal) {
        let pid = self.child.id().expect("server is still running");
        let pid = i32::try_from(pid).expect("the child PID fits in a platform pid_t");
        kill(Pid::from_raw(pid), signal).expect("signal the scratch server process");
    }

    async fn wait(&mut self, deadline: Duration) -> (std::process::ExitStatus, String) {
        let status = tokio::time::timeout(deadline, self.child.wait())
            .await
            .expect("server exited before the test deadline")
            .expect("wait for server");
        let mut stderr = String::new();
        self.stderr
            .take()
            .expect("server stderr")
            .read_to_string(&mut stderr)
            .await
            .expect("read server stderr");
        (status, stderr)
    }
}

impl Drop for ScratchServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn assert_second_signal_exits(server: &mut ScratchServer) {
    // Queue two distinct stop signals while the child cannot tear down. This
    // pins the persistent listeners without making the test depend on a slow
    // shutdown phase that healthy production code should avoid.
    server.signal(Signal::SIGSTOP);
    server.signal(Signal::SIGINT);
    server.signal(Signal::SIGTERM);
    let began = Instant::now();
    server.signal(Signal::SIGCONT);
    let (status, stderr) = server.wait(ESCALATION_DEADLINE).await;

    assert!(
        !status.success(),
        "an escalated shutdown has nonzero status"
    );
    assert!(
        began.elapsed() < ESCALATION_DEADLINE,
        "the second signal exits now rather than waiting for teardown"
    );
    assert_eq!(
        stderr.lines().collect::<Vec<_>>(),
        vec!["aj: received a second shutdown signal; exiting immediately"],
        "escalation writes one diagnostic line"
    );
}

#[tokio::test]
async fn a_second_shutdown_signal_exits_immediately() {
    let mut serve = ScratchServer::start("serve", "aj serving ").await;
    assert_second_signal_exits(&mut serve).await;
}

#[tokio::test]
async fn a_gateway_inherits_second_signal_escalation() {
    let mut gateway = ScratchServer::start("gateway", "aj gateway ").await;
    let enrollment: Value = reqwest::get(format!("{}/v1/hosts", gateway.url))
        .await
        .expect("read the scratch gateway enrollment")
        .error_for_status()
        .expect("gateway enrollment status")
        .json()
        .await
        .expect("gateway enrollment JSON");
    assert!(
        enrollment["hosts"]
            .as_array()
            .is_some_and(|hosts| hosts.is_empty()),
        "the isolated HOME starts with no ambient enrollment state: {enrollment}"
    );
    assert_second_signal_exits(&mut gateway).await;
}

#[tokio::test]
async fn one_sigterm_closes_an_attached_stream_without_spending_the_server_grace() {
    let mut serve = ScratchServer::start("serve", "aj serving ").await;
    let mut stream = reqwest::get(format!("{}/v1/events", serve.url))
        .await
        .expect("open an event stream");
    assert!(
        stream.status().is_success(),
        "the fixture has a live stream for server shutdown to join"
    );
    let first = tokio::time::timeout(Duration::from_secs(1), stream.chunk())
        .await
        .expect("the event stream begins")
        .expect("read the first event chunk")
        .expect("the open event stream has a first chunk");
    assert!(!first.is_empty(), "the first event chunk carries bytes");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), stream.chunk())
            .await
            .is_err(),
        "the event stream remains open before shutdown"
    );
    let began = Instant::now();

    serve.signal(Signal::SIGTERM);
    let (status, stderr) = serve.wait(ESCALATION_DEADLINE).await;

    assert!(status.success(), "one stop signal stays graceful: {stderr}");
    assert!(
        began.elapsed() < ESCALATION_DEADLINE,
        "host fanout closure ends the stream without spending the five-second server grace"
    );
    assert!(stderr.is_empty(), "healthy shutdown is quiet: {stderr}");
    drop(stream);
}

#[tokio::test]
async fn one_sigterm_still_shuts_a_multi_session_host_down_cleanly() {
    let mut serve = ScratchServer::start("serve", "aj serving ").await;
    let http = reqwest::Client::new();
    for _ in 0..4 {
        let response = http
            .post(format!("{}/v1/sessions", serve.url))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("create a scratch session");
        assert!(response.status().is_success(), "scratch session creation");
    }
    let directory: Value = http
        .get(format!("{}/v1/sessions", serve.url))
        .send()
        .await
        .expect("read scratch sessions")
        .error_for_status()
        .expect("session directory status")
        .json()
        .await
        .expect("session directory JSON");
    let ids: HashSet<&str> = directory["sessions"]
        .as_array()
        .expect("session rows")
        .iter()
        .filter_map(|row| row["id"].as_str())
        .collect();
    assert_eq!(
        ids.len(),
        4,
        "the process holds several distinct sessions before it is stopped"
    );
    let began = Instant::now();

    serve.signal(Signal::SIGTERM);
    let (status, stderr) = serve.wait(HEALTHY_DEADLINE).await;

    assert!(status.success(), "one stop signal stays graceful: {stderr}");
    assert!(
        began.elapsed() < HEALTHY_DEADLINE,
        "idle sessions do not spend their grace ceilings"
    );
    assert!(stderr.is_empty(), "healthy shutdown is quiet: {stderr}");
}
