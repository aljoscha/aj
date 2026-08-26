#![cfg(unix)]

use std::collections::HashSet;
use std::net::SocketAddr;
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

    fn address(&self) -> SocketAddr {
        self.url
            .strip_prefix("http://")
            .expect("HTTP server URL")
            .parse()
            .expect("server socket address")
    }
}

impl Drop for ScratchServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

async fn assert_second_signal_exits(server: &mut ScratchServer) {
    let stream = reqwest::get(format!("{}/v1/events", server.url))
        .await
        .expect("open a stream that holds server teardown in its grace");
    assert!(
        stream.status().is_success(),
        "the stream must be open or the first signal may finish teardown"
    );

    server.signal(Signal::SIGINT);
    wait_until_listener_closes(server.address()).await;
    let began = Instant::now();
    server.signal(Signal::SIGTERM);
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
    drop(stream);
}

async fn wait_until_listener_closes(address: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(address).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the first signal never stopped the scratch listener");
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
    // Queue both stop signals while the owned child is suspended. This makes
    // the escalation independent of how quickly an empty gateway tears down.
    gateway.signal(Signal::SIGSTOP);
    gateway.signal(Signal::SIGINT);
    gateway.signal(Signal::SIGTERM);
    let began = Instant::now();
    gateway.signal(Signal::SIGCONT);
    let (status, stderr) = gateway.wait(ESCALATION_DEADLINE).await;

    assert!(
        !status.success(),
        "an escalated gateway shutdown has nonzero status"
    );
    assert!(
        began.elapsed() < ESCALATION_DEADLINE,
        "the queued second signal exits now rather than waiting for teardown"
    );
    assert_eq!(
        stderr.lines().collect::<Vec<_>>(),
        vec!["aj: received a second shutdown signal; exiting immediately"],
        "gateway escalation writes one diagnostic line"
    );
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
