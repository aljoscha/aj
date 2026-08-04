//! Headless and embedded operation of the control port (spec section 4).
//!
//! `aj serve` composes the same session host the interactive shell does and
//! serves it with no terminal of its own. An interactive run given
//! `--listen` serves the very host it is rendering, so the local shell and
//! every remote client attach as peers rather than to two hosts over one
//! session store, which the store's advisory locks would refuse anyway.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use aj_app::cli::args::Args;
use aj_app::host::SessionHost;
use aj_app::session_setup::{ComposedHost, compose_host};
use aj_app::settings::ConfigLayers;
use aj_conf::Config;
use aj_models::auth::AuthStorage;
use aj_session::ConversationPersistence;
use anyhow::{Context, Result, bail};

use crate::remote::{IdentityGate, IdentityMode, RemoteServer, TailscaleWhois};

/// Resolve `--listen`'s address, reporting the value the user actually
/// wrote rather than a resolver error alone.
///
/// A hostname is accepted and resolved because the tailnet address a host
/// binds is usually reached by name, but exactly one address is bound: an
/// ambiguous name is the operator's to disambiguate, since which of two
/// interfaces carries the control port is a security decision.
fn resolve_listen(listen: &str) -> Result<SocketAddr> {
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
fn build_gate(args: &Args) -> Result<IdentityGate> {
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
/// Teardown order matters and is the reverse of startup: the server stops
/// accepting and lets its streams close, then the host cancels turns through
/// the graceful path, quiesces background tasks, flushes logs, and releases
/// the session locks.
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

    let ComposedHost { host, .. } = compose_host(&args, layers, &auth, &persistence)?;
    let server = match start_server(&args, &host).await {
        Ok(server) => server.expect("serve defaults its listen address above"),
        Err(err) => {
            // The host is already holding session locks, so it has to be
            // wound down even though nothing was served.
            host.shutdown().await;
            return Err(err);
        }
    };

    println!("aj serving {} on {}", host.hello().host_id, server.url());
    wait_for_shutdown().await;

    server.shutdown().await;
    host.shutdown().await;
    Ok(())
}

/// Resolve when the process is asked to stop: Ctrl+C, or SIGTERM from a
/// service manager (the reference unit runs `aj serve` under systemd).
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(err) => {
                tracing::warn!("could not listen for SIGTERM: {err}");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_gate_mode_comes_from_auth() {
        let args =
            |argv: &[&str]| <Args as clap::Parser>::try_parse_from(argv).expect("args parse");
        assert!(build_gate(&args(&["aj"])).is_ok(), "local is the default");
        assert!(build_gate(&args(&["aj", "--auth", "open"])).is_ok());
        assert!(
            build_gate(&args(&["aj", "--auth", "sudo"])).is_err(),
            "an unknown mode is refused rather than silently downgraded",
        );
    }
}
