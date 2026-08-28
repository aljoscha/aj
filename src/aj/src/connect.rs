//! `aj connect <url>`: the client half of connect mode (spec 9.1).
//!
//! Everything here runs before a terminal is taken over, so an unreachable
//! host or a protocol mismatch is a plain CLI error rather than a notice
//! nobody sees. What it produces is the [`Control`] the shell drives, the
//! session it opens with, and the host facts the chrome needs.
//!
//! Session selection is the spec's, and the command line has already been
//! read into the three states it has (`aj_app::cli::args::ConnectSession`): a
//! session named, one this run creates, or the host's own choice. A create
//! carries the settings this client's user actually stated, because
//! per-session settings follow whoever creates the session (spec section 8),
//! and the host `--host` named when the peer serves more than one.

use std::path::PathBuf;

use aj_app::cli::args::{Args, ConnectLaunch, ConnectSession};
use aj_conf::{Config, ConfigLayer};
use aj_models::{speed_name, thinking_config_name, verbosity_name};
use aj_wire::{DirectoryHost, Hello, ModelSelection, SessionSettings};
use anyhow::{Context, Result, anyhow};

use crate::control::{Control, ControlError};
use crate::host_picker::resolve_host;
use crate::remote::RemoteClient;

/// A connected client: the control surface, the focused session, and what the
/// handshake said about the host.
pub(crate) struct Connected {
    pub(crate) control: Control,
    pub(crate) session: String,
    /// The host's working directory, which is what the header and the footer
    /// show. `None` when the host did not report one, which leaves those
    /// showing the url instead of a directory this machine does not have.
    pub(crate) working_directory: Option<PathBuf>,
    /// Whether the session was created by this connect, which is what decides
    /// if it gets a fresh session's notices.
    pub(crate) created: bool,
}

/// Dial what `launch` asks for, settle version skew, and resolve the session
/// to open.
///
/// Taking the whole launch rather than its parts is what keeps the run that is
/// dialled the run the command line asked for: a caller outside `aj_app` has no
/// way to assemble one that says something else.
///
/// The launch prompt is not submitted here: the caller submits it through the
/// ordinary prompt path once the shell is attached, exactly as a local run
/// does, so a created and a resumed session behave the same way.
pub(crate) async fn connect(
    args: &Args,
    config: &Config,
    stated: &Stated,
    launch: &ConnectLaunch<'_>,
) -> Result<Connected> {
    let client = RemoteClient::new(launch.url())
        .with_context(|| format!("could not use {:?} as a control-port URL", launch.url()))?;
    let hello: Hello = client
        .hello()
        .await
        .with_context(|| format!("could not reach an aj host at {}", client.base()))?;
    let working_directory = hello.working_directory.clone();
    let control = Control::remote(client);
    let settings = creator_settings(args, config, stated);
    // Refused here rather than on the host: a create the host would reject
    // for its label is a round trip that reports after the terminal is gone,
    // and the flag is this client's own input to validate (spec 6.6).
    let tag = args.launch_tag().map_err(|err| anyhow!("--tag: {err}"))?;
    let host = match launch.host() {
        Some(named) => resolve_named_host(&control, &hello, named).await?,
        None => None,
    };
    let (session, created) =
        resolve_session(&control, launch.session(), host, settings, tag).await?;
    Ok(Connected {
        control,
        session,
        working_directory,
        created,
    })
}

/// The full id of the host `--host` named, refused before the terminal is
/// taken over.
///
/// A plain host is its own single candidate, named by the id it introduced
/// itself with, which is also the only value its create route accepts (spec
/// 6.6). It is recognized by the working directory a gateway reports none of,
/// and answered from the handshake alone: a directory read is an enumeration of
/// the host's whole store (spec 6.7), and there is nothing in it this needs.
///
/// A gateway's candidates are the hosts it publishes (spec 7.1). `None` when it
/// publishes none, because then there is nowhere to create at all and the
/// gateway's own refusal says so better than one invented here.
async fn resolve_named_host(
    control: &Control,
    hello: &Hello,
    named: &str,
) -> Result<Option<String>> {
    let hosts = match &hello.working_directory {
        Some(_) => vec![DirectoryHost {
            id: Some(hello.host_id.clone()),
            address: None,
            // The host named itself in this handshake, and this row stands in
            // for what a gateway would have published about it.
            name: hello.name.clone(),
            unreachable: false,
        }],
        None => {
            control
                .sessions()
                .await
                .context("could not read the host's session list")?
                .hosts
        }
    };
    if hosts.is_empty() {
        return Ok(None);
    }
    resolve_host(&hosts, named)
        .map(Some)
        .map_err(|err| anyhow!("--host: {err}"))
}

/// Resolve the session to attach per spec 9.1, creating one when that is what
/// the rule says.
///
/// The default attach passes over archived rows, so a host whose sessions are
/// all archived creates one exactly as an empty host does. An explicit id is
/// answered whatever its bit says: archiving puts a session away, it does not
/// close it, so naming one always works.
async fn resolve_session(
    control: &Control,
    session: ConnectSession<'_>,
    host: Option<String>,
    settings: Option<SessionSettings>,
    tag: Option<String>,
) -> Result<(String, bool)> {
    match session {
        ConnectSession::Named(id) => Ok((id.to_string(), false)),
        ConnectSession::Fresh => Ok((create(control, host, settings, tag).await?, true)),
        ConnectSession::Latest => {
            let list = control
                .sessions()
                .await
                .context("could not read the host's session list")?;
            // Most recently modified, with the id as the tie-break: ids are
            // minted as timestamps, so the higher one is the younger session.
            // An archived session is one its user is done with, and this is
            // the one branch that picks a session nobody named, so those rows
            // are passed over.
            let latest = list
                .sessions
                .iter()
                .filter(|summary| !summary.archived)
                .max_by_key(|summary| (summary.last_activity, summary.id.clone()))
                .map(|summary| summary.id.clone());
            match latest {
                Some(session) => Ok((session, false)),
                // A fresh `aj serve` holds nothing, and a host holding only
                // archived sessions offers nothing either, so connect mode
                // would otherwise have nothing to attach at all.
                None => Ok((create(control, host, settings, tag).await?, true)),
            }
        }
    }
}

/// Create the session connect mode opens with, per spec 9.1.
///
/// A create whose session exists but whose label did not land is not a
/// failed create: connect attaches the session it just made and says what
/// did not stick. Failing here instead would leave a session on the host
/// that nobody asked for and nobody is looking at.
async fn create(
    control: &Control,
    host: Option<String>,
    settings: Option<SessionSettings>,
    tag: Option<String>,
) -> Result<String> {
    match control.create(host, settings, None, tag).await {
        Ok(session) => Ok(session),
        Err(ControlError::PartialCreate { session, message }) => {
            eprintln!("aj: warning: {message}");
            Ok(session)
        }
        // A peer serving several hosts will not guess which one a create is
        // for, and a run with no terminal cannot be asked, so the refusal
        // names the flag that answers it.
        Err(err) if err.ambiguous_host() => {
            Err(err).context("could not create a session: name the host it is for with --host <id>")
        }
        Err(err) => Err(err).context("could not create a session"),
    }
}

/// Which settings a human actually stated, as opposed to what a config
/// resolves to when nobody said anything.
///
/// Provenance is the line spec section 8 draws for what travels with a create,
/// and it is only readable from the config *layers*: the effective [`Config`]
/// a process runs with has the built-in fallbacks baked into the same `Option`
/// fields, so a written `thinking = "xhigh"` and an absent one look identical
/// there.
pub(crate) struct Stated {
    user: ConfigLayer,
    project: ConfigLayer,
}

impl Stated {
    pub(crate) fn new(user: ConfigLayer, project: ConfigLayer) -> Self {
        Self { user, project }
    }

    /// Whether either layer writes `key`. Which layer wins does not matter
    /// here, only that some file says something.
    fn has(&self, key: &str) -> bool {
        self.user.is_set(key) || self.project.is_set(key)
    }
}

/// The settings this client *stated*, for a session it creates, or `None`
/// when it stated nothing.
///
/// Spec section 8 draws the line at provenance rather than at value: an axis
/// travels only when a human named it, through a CLI flag, an environment
/// variable, or an entry written in this client's config. The built-in
/// fallback a config resolves to when nothing is written is not a preference,
/// and sending it would ask the host to honor an opinion nobody holds, which a
/// host serving a narrow-vocabulary model would rightly refuse. What we omit
/// the host defaults itself, against the model it actually runs.
fn creator_settings(args: &Args, config: &Config, stated: &Stated) -> Option<SessionSettings> {
    let selection = aj_app::model::ModelSelection::merge(args, config);
    // A model is stated by naming it, and `merge` already answers that: with
    // none pinned anywhere there is no `(api, name)` pair to send, and "that
    // provider's default" is not something the wire can express.
    let model = selection.name.as_ref().map(|name| ModelSelection {
        api: selection.provider_id().to_string(),
        url: selection.url.clone(),
        name: name.clone(),
    });
    let speed = args
        .speed
        .as_deref()
        .and_then(|name| name.parse::<aj_conf::ConfigSpeed>().ok())
        .or(config.speed)
        .map(|speed| match speed {
            aj_conf::ConfigSpeed::Standard => aj_models::types::Speed::Standard,
            aj_conf::ConfigSpeed::Fast => aj_models::types::Speed::Fast,
        });
    let thinking = args.thinking.or(config.thinking);
    let settings = SessionSettings {
        model,
        // The effective config carries the value, the layers carry whether
        // anyone asked for it, so both are consulted per axis.
        thinking: (args.thinking.is_some() || stated.has("thinking")).then(|| {
            let level = aj_app::model::default_thinking_from_config(thinking);
            thinking_config_name(level.as_ref()).to_string()
        }),
        thinking_display: stated.has("thinking_display").then(|| {
            aj_app::session_setup::thinking_display_name(config.thinking_display).to_string()
        }),
        speed: (args.speed.is_some() || stated.has("speed")).then(|| speed_name(speed).to_string()),
        verbosity: stated.has("verbosity").then(|| {
            let unified = config
                .verbosity
                .map(aj_app::model::config_verbosity_to_unified);
            verbosity_name(unified).to_string()
        }),
    };
    (settings != SessionSettings::default()).then_some(settings)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aj_app::host::{Command, SessionHost};
    use aj_wire::SessionSummary;
    use clap::Parser;
    use tempfile::TempDir;

    use super::*;
    use crate::remote::tests::{HostHandles, addr, bounded, scripted, scripted_host};
    use crate::remote::{IdentityGate, RemoteServer};

    fn args(argv: &[&str]) -> Args {
        Args::try_parse_from(argv).expect("args parse")
    }

    /// A layer that writes `keys`, standing in for a config file that does.
    /// The values only have to parse: statedness is what is under test.
    fn wrote(keys: &[(&str, &str)]) -> ConfigLayer {
        let mut layer = ConfigLayer::default();
        for (key, value) in keys {
            layer
                .set_str(key, value)
                .unwrap_or_else(|err| panic!("fixture sets {key:?}: {err}"));
        }
        layer
    }

    fn nothing_stated() -> Stated {
        Stated::new(ConfigLayer::default(), ConfigLayer::default())
    }

    /// A stock client states nothing, so nothing travels and the host defaults
    /// every axis itself. This is what lets a default install create a session
    /// on a host whose model has a narrower thinking vocabulary than the
    /// built-in fallback names (spec section 8).
    #[test]
    fn creator_settings_are_empty_when_nothing_is_stated() {
        // The fallback is baked into the effective config, which is exactly
        // why value alone cannot decide this.
        let config = Config::default();
        assert!(config.thinking.is_some());
        assert_eq!(
            creator_settings(&args(&["aj"]), &config, &nothing_stated()),
            None
        );
    }

    /// A written config entry is a statement, so it travels even when it names
    /// the same value the built-in fallback would have produced.
    #[test]
    fn creator_settings_carry_written_config_entries() {
        let config = Config {
            thinking: Some(aj_conf::ConfigThinkingLevel::XHigh),
            ..Config::default()
        };
        let stated = Stated::new(wrote(&[("thinking", "xhigh")]), ConfigLayer::default());
        let settings = creator_settings(&args(&["aj"]), &config, &stated).expect("stated");
        assert_eq!(settings.thinking.as_deref(), Some("xhigh"));
        // Untouched axes stay unstated rather than riding along.
        assert_eq!(settings.model, None);
        assert_eq!(settings.speed, None);
        assert_eq!(settings.verbosity, None);
        assert_eq!(settings.thinking_display, None);
    }

    /// The CLI wins over config, and a pinned model travels as the triple the
    /// host resolves against its own catalog (spec 6.6).
    #[test]
    fn creator_settings_carry_the_cli_selection() {
        let mut config = Config::default();
        config.model_api = Some("openai".to_string());
        config.model_name = Some("from-config".to_string());
        config.speed = Some(aj_conf::ConfigSpeed::Standard);
        config.thinking = Some(aj_conf::ConfigThinkingLevel::High);
        let settings = creator_settings(
            &args(&[
                "aj",
                "--model-name",
                "from-cli",
                "--model-url",
                "https://proxy.example/v1",
                "--speed",
                "fast",
                "--thinking",
                "off",
            ]),
            &config,
            &Stated::new(
                ConfigLayer::default(),
                wrote(&[("thinking", "high"), ("speed", "standard")]),
            ),
        )
        .expect("stated");
        assert_eq!(
            settings.model,
            Some(ModelSelection {
                api: "openai".to_string(),
                url: Some("https://proxy.example/v1".to_string()),
                name: "from-cli".to_string(),
            }),
        );
        assert_eq!(settings.speed.as_deref(), Some("fast"));
        assert_eq!(settings.thinking.as_deref(), Some("off"));
    }

    /// Clap reads `AJ_THINKING` in an isolated child process so no environment
    /// mutation can race this test binary's parallel argument parsing. The
    /// child then crosses the connect create boundary, where an environment
    /// value must count as creator-stated rather than as a config fallback.
    #[test]
    fn aj_thinking_environment_is_creator_stated_on_connect() {
        const CHILD_SENTINEL: &str = "AJ_TEST_CONNECT_THINKING_CHILD_SENTINEL";

        if let Some(sentinel) = std::env::var_os(CHILD_SENTINEL) {
            let parsed = args(&["aj", "connect", "http://host:6161", "--new"]);
            assert_eq!(
                parsed.thinking,
                Some(aj_conf::ConfigThinkingLevel::Low),
                "clap did not read AJ_THINKING",
            );
            let settings = creator_settings(&parsed, &Config::default(), &nothing_stated())
                .expect("the environment value is creator-stated");
            assert_eq!(settings.thinking.as_deref(), Some("low"));
            std::fs::write(sentinel, "observed").expect("write the child sentinel");
            return;
        }

        let dir = TempDir::new().expect("sentinel tempdir");
        let sentinel = dir.path().join("observed");
        let output =
            std::process::Command::new(std::env::current_exe().expect("locate this test binary"))
                .args([
                    "--exact",
                    "connect::tests::aj_thinking_environment_is_creator_stated_on_connect",
                    "--nocapture",
                ])
                .env("AJ_THINKING", "low")
                .env(CHILD_SENTINEL, &sentinel)
                .output()
                .expect("run the isolated parser child");

        assert!(
            output.status.success(),
            "isolated parser child failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            std::fs::read_to_string(sentinel).expect("the exact child test ran"),
            "observed",
        );
    }

    /// A real host behind a real loopback control port, with the [`Control`] a
    /// client drives it through.
    ///
    /// Session selection reads what the host publishes, so both the archive
    /// and the read that answers it cross the wire. The store is the guard's,
    /// so nothing outlives the test.
    struct Peer {
        _dir: TempDir,
        host: SessionHost,
        server: RemoteServer,
        control: Control,
    }

    impl Peer {
        async fn start() -> Self {
            let dir = TempDir::new().expect("tempdir");
            let host = scripted_host(
                &dir,
                scripted(Vec::new(), 0, Duration::ZERO),
                HostHandles::new(&dir),
                None,
            );
            let server =
                RemoteServer::bind(host.clone(), addr("127.0.0.1:0"), IdentityGate::local())
                    .await
                    .expect("bind a loopback control port");
            let control = Control::remote(RemoteClient::new(&server.url()).expect("client"));
            Self {
                _dir: dir,
                host,
                server,
                control,
            }
        }

        /// A session on the host, which is the only way a fresh one holds any.
        async fn create(&self) -> String {
            bounded("a create", self.control.create(None, None, None, None))
                .await
                .expect("create a session")
        }

        async fn archive(&self, session: &str) {
            bounded(
                "an archive",
                self.control
                    .command(session, Command::Archive { archived: true }),
            )
            .await
            .expect("archive the session");
        }

        async fn rows(&self) -> Vec<SessionSummary> {
            bounded("the session list", self.control.sessions())
                .await
                .expect("the session list")
                .sessions
        }

        async fn row(&self, session: &str) -> SessionSummary {
            self.rows()
                .await
                .into_iter()
                .find(|summary| summary.id == session)
                .unwrap_or_else(|| panic!("{session} is in the host's directory"))
        }

        /// Dial this peer the way `aj connect <url> [argv...]` does, from argv
        /// through the handshake to the session it opens with.
        async fn dial(&self, argv: &[&str]) -> Connected {
            let url = self.server.url();
            let mut line = vec!["aj", "connect", &url];
            line.extend_from_slice(argv);
            let args = args(&line);
            let launch = args
                .connect_launch()
                .expect("connect args parse as connect");
            bounded(
                "connect to resolve a session",
                connect(&args, &Config::default(), &nothing_stated(), &launch),
            )
            .await
            .expect("connect to the host")
        }

        async fn shutdown(self) {
            self.host.shutdown().await;
            self.server.shutdown().await;
        }
    }

    /// Bare connect takes the newest session its user is not done with, so an
    /// archived row is passed over even when it is the one the rule would
    /// otherwise have landed on.
    #[tokio::test]
    async fn bare_connect_passes_over_an_archived_session() {
        let peer = Peer::start().await;
        let older = peer.create().await;
        let newer = peer.create().await;
        // The premise: unarchived, `newer` is what bare connect picks, so
        // archiving it is what the next dial is answering. Without this the
        // test measures nothing.
        assert_eq!(
            peer.dial(&[]).await.session,
            newer,
            "the fixture's second session is not the one bare connect takes"
        );

        peer.archive(&newer).await;
        assert!(
            peer.row(&newer).await.archived,
            "the archive command did not set the bit the host publishes"
        );

        let connected = peer.dial(&[]).await;
        assert_eq!(
            connected.session, older,
            "bare connect attached the archived session"
        );
        assert!(
            !connected.created,
            "bare connect created a session instead of attaching the unarchived one"
        );
        peer.shutdown().await;
    }

    /// A host whose every session is archived offers nothing to attach, so
    /// bare connect creates, exactly as against a host holding none.
    #[tokio::test]
    async fn bare_connect_creates_when_every_session_is_archived() {
        let peer = Peer::start().await;
        let put_away = peer.create().await;
        peer.archive(&put_away).await;
        let rows = peer.rows().await;
        assert!(
            !rows.is_empty() && rows.iter().all(|summary| summary.archived),
            "the fixture holds {} sessions and not all are archived, so a create here proves nothing",
            rows.len()
        );

        let connected = peer.dial(&[]).await;
        assert_ne!(
            connected.session, put_away,
            "bare connect attached the archived session"
        );
        assert!(
            connected.created,
            "bare connect reported an attach for the session it minted"
        );
        assert!(
            !peer.row(&connected.session).await.archived,
            "the session bare connect created is archived"
        );
        peer.shutdown().await;
    }

    /// Naming a session is asking for that one: the archived bit puts a
    /// session away rather than closing it, so an explicit id is answered
    /// whatever the bit says.
    #[tokio::test]
    async fn an_explicit_id_resolves_an_archived_session() {
        let peer = Peer::start().await;
        let put_away = peer.create().await;
        peer.archive(&put_away).await;
        assert!(
            peer.row(&put_away).await.archived,
            "the archive command did not set the bit the host publishes"
        );

        let connected = peer.dial(&[&put_away]).await;
        assert_eq!(
            connected.session, put_away,
            "an explicit id did not attach the archived session it named"
        );
        assert!(
            !connected.created,
            "an explicit id created a session instead of attaching the one it named"
        );
        peer.shutdown().await;
    }

    /// `--new` with launch input creates. The grammar puts that input in the
    /// session-id slot, and a run that read it as an id would attach nothing
    /// and leave the host without the session it was asked for.
    ///
    /// The prompt's text goes nowhere here: `connect` resolves a session and
    /// the shell submits the turn later, so what it carries is pinned in the
    /// composed world instead.
    #[tokio::test]
    async fn new_with_launch_input_creates_a_session() {
        let peer = Peer::start().await;
        let held = peer.create().await;
        // Every assertion below reads against this one row. Without it a create
        // and an attach leave the same count behind.
        assert_eq!(
            peer.rows().await.len(),
            1,
            "the fixture host does not hold the single session the counts are read against",
        );

        let connected = peer
            .dial(&["--new", "Reply with the single word: ok"])
            .await;

        let rows = peer.rows().await;
        assert_eq!(
            rows.len(),
            2,
            "the host holds {} sessions, so the run created none",
            rows.len(),
        );
        assert!(
            rows.iter().any(|summary| summary.id == connected.session),
            "connect opened {} which the host does not hold",
            connected.session,
        );
        assert_ne!(
            connected.session, held,
            "--new attached the session the host already held",
        );
        assert!(
            connected.created,
            "connect reported a resume for the session it minted",
        );
        peer.shutdown().await;
    }

    /// Launch input that happens to name a session the host holds is still
    /// launch input: under `--new` the id slot carries no id at all, so the
    /// run creates rather than attaching what the prompt spells.
    #[tokio::test]
    async fn new_creates_when_the_launch_input_names_a_session() {
        let peer = Peer::start().await;
        let held = peer.create().await;
        // Every assertion below reads against this one row. Without it a create
        // and an attach leave the same count behind.
        assert_eq!(
            peer.rows().await.len(),
            1,
            "the fixture host does not hold the single session the counts are read against",
        );

        let connected = peer.dial(&["--new", &held]).await;

        assert_ne!(
            connected.session, held,
            "the launch input was resolved as the session to attach",
        );
        assert_eq!(
            peer.rows().await.len(),
            2,
            "the run attached the session its prompt spelled instead of creating one",
        );
        assert!(
            connected.created,
            "connect reported a resume for the session it minted",
        );
        peer.shutdown().await;
    }
}
