//! Headless and embedded operation of the control port.
//!
//! The listen-address resolution, the identity gate's construction and the
//! shutdown signal are shared with `aj gateway`: a gateway binds a control port
//! under the same rules, because it is the same remote code execution behind
//! it.
//!
//! `aj serve` composes the same session host the interactive shell does and
//! serves it with no terminal of its own. An interactive run given
//! `--listen` serves the very host it is rendering, so the local shell and
//! every remote client attach as peers rather than to two hosts over one
//! session store, which the store's advisory locks would refuse anyway.

use std::future::Future;
#[cfg(unix)]
use std::future::pending;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use aj_app::cli::args::Args;
use aj_app::host::SessionHost;
use aj_app::session_setup::{ComposedHost, compose_host};
use aj_app::settings::ConfigLayers;
use aj_conf::Config;
use aj_models::auth::AuthStorage;
use aj_session::ConversationPersistence;
use aj_wire::Hello;
use anyhow::{Context, Result, bail};

use crate::remote::{IdentityGate, IdentityMode, RemoteServer, TailscaleWhois};

/// One process-wide grace for orderly teardown before abandoning unfinished work.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Resolve `--listen`'s address, reporting the value the user actually
/// wrote rather than a resolver error alone.
///
/// A hostname is accepted and resolved because the tailnet address a host
/// binds is usually reached by name, but exactly one address is bound: an
/// ambiguous name is the operator's to disambiguate, since which of two
/// interfaces carries the control port is a security decision.
pub(crate) fn resolve_listen(listen: &str) -> Result<SocketAddr> {
    let mut resolved = listen
        .to_socket_addrs()
        .with_context(|| format!("could not resolve the listen address {listen:?}"))?;
    let first = resolved
        .next()
        .with_context(|| format!("the listen address {listen:?} resolved to nothing"))?;
    if resolved.next().is_some() {
        bail!(
            "the listen address {listen:?} resolves to several addresses; \
             name the one to bind explicitly"
        );
    }
    Ok(first)
}

/// Build the identity gate `args` asks for.
///
/// A `tailscale` gate resolves peers through the local tailscale daemon, so
/// constructing it fails when that daemon is unreachable: refusing to start
/// is the honest outcome for a mode whose whole purpose is to reject
/// unidentified peers.
pub(crate) fn build_gate(args: &Args) -> Result<IdentityGate> {
    let mode: IdentityMode = args
        .auth
        .parse()
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("--auth accepts local, tailscale, or open")?;
    match mode {
        IdentityMode::Local => Ok(IdentityGate::local()),
        IdentityMode::Open => Ok(IdentityGate::open()),
        IdentityMode::Tailscale => {
            let resolver = TailscaleWhois::new()
                .context("--auth tailscale needs a reachable tailscale daemon")?;
            Ok(IdentityGate::tailscale(
                args.allow.iter().cloned(),
                Arc::new(resolver),
            ))
        }
    }
}

/// Start the control port for `host`, if `args` asked for one.
///
/// The identity gate's bind check runs here, so an address it will not serve
/// unauthenticated stops the process before a terminal is taken over rather
/// than after.
pub(crate) async fn start_server(args: &Args, host: &SessionHost) -> Result<Option<RemoteServer>> {
    let Some(listen) = args.listen.as_deref() else {
        return Ok(None);
    };
    let addr = resolve_listen(listen)?;
    let gate = build_gate(args)?;
    let server = RemoteServer::bind(host.clone(), addr, gate)
        .await
        .with_context(|| format!("could not serve the control port on {addr}"))?;
    Ok(Some(server))
}

/// `aj serve`: hold this working directory's sessions and serve them until
/// the process is asked to stop.
///
/// Teardown stops accepting first, then the host cancels turns through the
/// graceful path and closes attachments, then the server joins those streams.
pub(crate) async fn run(mut args: Args) -> Result<()> {
    // A bare `aj serve` is expected to be reachable by `aj connect` on the
    // same machine, so the control port is the point of the mode rather than
    // an addition to it: it defaults on, at the same loopback address a bare
    // `--listen` binds.
    if args.listen.is_none() {
        args.listen = Some(aj_app::cli::args::DEFAULT_LISTEN_ADDRESS.to_string());
    }

    let (user_config, user_diagnostics) = Config::load();
    let (project_layer, project_diagnostics) = Config::load_project();
    for diagnostic in user_diagnostics.iter().chain(project_diagnostics.iter()) {
        // Headless mode has no transcript to fold these into, and a
        // misconfigured host is worth saying out loud before it serves.
        eprintln!("aj: {diagnostic}");
    }
    let layers = ConfigLayers {
        user: user_config,
        project: project_layer,
        project_path: Config::project_config_file_path(),
    };
    let auth = AuthStorage::at_default_path().context("failed to open ~/.aj/auth.json")?;
    let sessions_dir = Config::get_sessions_dir_path()?;
    let persistence = ConversationPersistence::new(sessions_dir);

    let ComposedHost { host, .. } = compose_host(&args, layers, &auth, &persistence, None)?;
    let server = match start_server(&args, &host).await {
        Ok(server) => server.expect("serve defaults its listen address above"),
        Err(err) => {
            // The host is already holding session locks, so it has to be
            // wound down even though nothing was served.
            finish_shutdown(host.shutdown()).await;
            return Err(err);
        }
    };

    let signals = ShutdownSignals::new();
    println!("{}", banner(&host.hello(), &server.url()));
    signals.shutdown(shutdown_server(server, &host)).await;
    Ok(())
}

/// Stop a control port, wind down the host it serves, then join its streams.
///
/// Shared by headless and interactive local mode so attached clients cannot
/// make either frontend spend the server grace before host fanout closes.
pub(crate) async fn shutdown_server(server: RemoteServer, host: &SessionHost) {
    server.stop_accepting();
    host.shutdown().await;
    server.shutdown().await;
}

/// Finish process teardown, exiting nonzero if it stalls or another stop arrives.
///
/// Library owners retain their resources until cleanup completes. Only the
/// exiting frontend may abandon that work, and process exit must not be
/// mistaken for successful library teardown.
pub(crate) async fn finish_shutdown<F: Future>(teardown: F) -> F::Output {
    ShutdownSignals::new().finish(teardown).await
}

/// What a `serve` run prints once it is up.
///
/// Leads with the name, which is what the operator who started this process
/// recognizes, and keeps the id, which is what a gateway enrolls and what
/// names the session store. A host that reports no name has only the id to be
/// known by.
fn banner(hello: &Hello, url: &str) -> String {
    match &hello.name {
        Some(name) => format!("aj serving {name} ({}) on {url}", hello.host_id),
        None => format!("aj serving {} on {url}", hello.host_id),
    }
}

/// Persistent stop-signal streams shared by serving modes.
pub(crate) struct ShutdownSignals {
    #[cfg(unix)]
    interrupt: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
    #[cfg(windows)]
    interrupt: Option<tokio::signal::windows::CtrlC>,
}

impl ShutdownSignals {
    /// Install listeners for every stop signal this platform supports.
    pub(crate) fn new() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let interrupt = signal(SignalKind::interrupt())
                .inspect_err(|err| tracing::warn!("could not listen for SIGINT: {err}"))
                .ok();
            let terminate = signal(SignalKind::terminate())
                .inspect_err(|err| tracing::warn!("could not listen for SIGTERM: {err}"))
                .ok();
            Self {
                interrupt,
                terminate,
            }
        }
        #[cfg(windows)]
        {
            let interrupt = tokio::signal::windows::ctrl_c()
                .inspect_err(|err| tracing::warn!("could not listen for Ctrl+C: {err}"))
                .ok();
            Self { interrupt }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self {}
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        loop {
            if self.interrupt.is_none() && self.terminate.is_none() {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
            enum Kind {
                Interrupt,
                Terminate,
            }
            let (kind, signal) = tokio::select! {
                signal = recv_unix(&mut self.interrupt) => (Kind::Interrupt, signal),
                signal = recv_unix(&mut self.terminate) => (Kind::Terminate, signal),
            };
            if signal.is_some() {
                return;
            }
            match kind {
                Kind::Interrupt => self.interrupt = None,
                Kind::Terminate => self.terminate = None,
            }
        }
        #[cfg(windows)]
        {
            match &mut self.interrupt {
                Some(interrupt) => {
                    let _ = interrupt.recv().await;
                }
                None => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }

    /// Await a stop signal, then give teardown one grace or exit on another stop.
    pub(crate) async fn shutdown<F>(mut self, teardown: F)
    where
        F: Future<Output = ()>,
    {
        self.recv().await;
        self.finish(teardown).await;
    }

    async fn finish<F: Future>(&mut self, teardown: F) -> F::Output {
        tokio::select! {
            biased;
            _ = self.recv() => {
                eprintln!("aj: received a second shutdown signal; exiting immediately");
                std::process::exit(1);
            }
            result = teardown => result,
            () = tokio::time::sleep(SHUTDOWN_GRACE) => {
                eprintln!("aj: shutdown grace expired; exiting with unfinished cleanup");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(unix)]
async fn recv_unix(signal: &mut Option<tokio::signal::unix::Signal>) -> Option<()> {
    match signal {
        Some(signal) => signal.recv().await,
        None => pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHUTDOWN_TEST_CHILD: &str = "AJ_SHUTDOWN_TEST_CHILD";

    async fn shutdown_test_child(test: &str) -> std::process::Output {
        let mut child = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", test, "--nocapture"])
            .env(SHUTDOWN_TEST_CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn shutdown test child");

        // Only the child pauses time. A missing shutdown deadline must fail the
        // parent on wall time, with the stuck child killed and reaped.
        match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
            Ok(status) => {
                status.expect("wait for shutdown test child");
            }
            Err(_) => {
                child
                    .kill()
                    .await
                    .expect("kill and reap shutdown test child");
                panic!("{test} exceeded the wall-clock deadline");
            }
        }
        child.wait_with_output().await.expect("child output")
    }

    #[tokio::test]
    async fn pending_cleanup_forces_process_exit() {
        if std::env::var_os(SHUTDOWN_TEST_CHILD).is_some() {
            tokio::time::pause();
            #[cfg(unix)]
            {
                let signals = ShutdownSignals::new();
                tokio::spawn(async {
                    // Service uptime does not consume the shutdown grace.
                    tokio::time::sleep(SHUTDOWN_GRACE * 2).await;
                    nix::sys::signal::kill(
                        nix::unistd::getpid(),
                        nix::sys::signal::Signal::SIGTERM,
                    )
                    .expect("send the first stop signal to this test child");
                });
                signals
                    .shutdown(async {
                        println!("first signal started cleanup");
                        std::future::pending::<()>().await;
                    })
                    .await;
            }
            #[cfg(not(unix))]
            finish_shutdown(std::future::pending::<()>()).await;
            panic!("pending cleanup must not return gracefully");
        }

        let output = shutdown_test_child("serve::tests::pending_cleanup_forces_process_exit").await;
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        #[cfg(unix)]
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("first signal started cleanup"),
            "the first signal, not service uptime, starts the grace: {output:?}",
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).lines().any(|line| {
                line == "aj: shutdown grace expired; exiting with unfinished cleanup"
            }),
            "{output:?}",
        );
    }

    #[tokio::test]
    async fn completed_cleanup_returns_without_spending_grace() {
        if std::env::var_os(SHUTDOWN_TEST_CHILD).is_some() {
            tokio::time::pause();
            let start = tokio::time::Instant::now();
            let result = finish_shutdown(async {
                tokio::task::yield_now().await;
                "cleanup result"
            })
            .await;
            assert_eq!(result, "cleanup result");
            assert_eq!(start.elapsed(), Duration::ZERO);
            println!("cleanup returned without spending grace");
            return;
        }

        let output =
            shutdown_test_child("serve::tests::completed_cleanup_returns_without_spending_grace")
                .await;
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("cleanup returned without spending grace"),
            "{output:?}",
        );
    }

    #[test]
    fn completed_program_does_not_wait_for_blocking_background_work() {
        if std::env::var_os(SHUTDOWN_TEST_CHILD).is_some() {
            crate::run_to_exit(async {
                let (started, running) = tokio::sync::oneshot::channel();
                drop(tokio::task::spawn_blocking(move || {
                    let _ = started.send(());
                    loop {
                        std::thread::park();
                    }
                }));
                running
                    .await
                    .expect("blocking work is running before application exit");
                Ok(())
            })
            .expect("application finished");
            println!("application exited without waiting for background work");
            return;
        }

        crate::run_to_exit(async {
            let output = shutdown_test_child(
                "serve::tests::completed_program_does_not_wait_for_blocking_background_work",
            )
            .await;
            assert!(output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains("application exited without waiting for background work"),
                "{output:?}",
            );
            Ok(())
        })
        .expect("test process completed");
    }

    #[test]
    fn a_listen_address_resolves_to_exactly_one_socket() {
        assert_eq!(
            resolve_listen("127.0.0.1:6161").expect("a literal address"),
            "127.0.0.1:6161".parse::<SocketAddr>().expect("parses"),
        );
        assert!(
            resolve_listen("127.0.0.1").is_err(),
            "an address with no port is refused",
        );
        assert!(resolve_listen("no-such-host.invalid:6161").is_err());
    }

    /// The line a host prints when it comes up names it the way its operator
    /// does, and still carries the id a gateway is told to enroll.
    #[test]
    fn the_banner_leads_with_the_name_and_keeps_the_id() {
        let mut hello = Hello {
            protocol: 1,
            capabilities: Vec::new(),
            app_version: "0.1.0".to_string(),
            host_id: "c6b6667d8f73e75d".to_string(),
            working_directory: None,
            name: Some("~/work/umber/aj".to_string()),
        };
        assert_eq!(
            banner(&hello, "http://127.0.0.1:6161"),
            "aj serving ~/work/umber/aj (c6b6667d8f73e75d) on http://127.0.0.1:6161",
        );

        hello.name = None;
        assert_eq!(
            banner(&hello, "http://127.0.0.1:6161"),
            "aj serving c6b6667d8f73e75d on http://127.0.0.1:6161",
            "a nameless host is announced by the id alone, not by an empty pair of brackets",
        );
    }

    #[test]
    fn the_gate_mode_comes_from_auth() {
        let args = |argv: &[&str]| Args::try_parse_from(argv).expect("args parse");
        assert!(build_gate(&args(&["aj"])).is_ok(), "local is the default");
        assert!(build_gate(&args(&["aj", "--auth", "open"])).is_ok());
        assert!(
            build_gate(&args(&["aj", "--auth", "sudo"])).is_err(),
            "an unknown mode is refused rather than silently downgraded",
        );
    }
}
