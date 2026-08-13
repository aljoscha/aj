//! `aj connect <url>`: the client half of connect mode (spec 9.1).
//!
//! Everything here runs before a terminal is taken over, so an unreachable
//! host or a protocol mismatch is a plain CLI error rather than a notice
//! nobody sees. What it produces is the [`Control`] the shell drives, the
//! session it opens with, and the host facts the chrome needs.
//!
//! Session selection is the spec's: an explicit id, else `--new` creates,
//! else the host's most recently modified session, else create one. A create
//! carries the settings this client's user actually stated, because
//! per-session settings follow whoever creates the session (spec section 8),
//! and the host `--host` named when the peer serves more than one.

use std::path::PathBuf;

use aj_app::cli::args::Args;
use aj_conf::{Config, ConfigLayer};
use aj_models::{speed_name, thinking_config_name, verbosity_name};
use aj_wire::{DirectoryHost, Hello, ModelSelection, SessionSettings};
use anyhow::{Context, Result, anyhow};

use crate::control::{Control, ControlError};
use crate::host_picker::resolve_host;
use crate::remote::RemoteClient;

/// Which session `aj connect` opens with.
pub(crate) struct ConnectTarget<'a> {
    pub(crate) url: &'a str,
    /// The session named on the command line, if any.
    pub(crate) session_id: Option<&'a str>,
    /// Whether `--new` asked for a fresh session.
    pub(crate) new: bool,
    /// The host `--host` named, for the session this run creates.
    pub(crate) host: Option<&'a str>,
}

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

/// Dial `target`, settle version skew, and resolve the session to open.
///
/// The launch prompt is not submitted here: the caller submits it through the
/// ordinary prompt path once the shell is attached, exactly as a local run
/// does, so a created and a resumed session behave the same way.
pub(crate) async fn connect(
    args: &Args,
    config: &Config,
    stated: &Stated,
    target: ConnectTarget<'_>,
) -> Result<Connected> {
    let client = RemoteClient::new(target.url)
        .with_context(|| format!("could not use {:?} as a control-port URL", target.url))?;
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
    let host = match target.host {
        Some(named) => resolve_named_host(&control, &hello, named).await?,
        None => None,
    };
    let (session, created) = resolve_session(&control, &target, host, settings, tag).await?;
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
async fn resolve_session(
    control: &Control,
    target: &ConnectTarget<'_>,
    host: Option<String>,
    settings: Option<SessionSettings>,
    tag: Option<String>,
) -> Result<(String, bool)> {
    if let Some(id) = target.session_id {
        return Ok((id.to_string(), false));
    }
    if target.new {
        return Ok((create(control, host, settings, tag).await?, true));
    }
    let list = control
        .sessions()
        .await
        .context("could not read the host's session list")?;
    // Most recently modified, with the id as the tie-break: ids are minted as
    // timestamps, so the higher one is the younger session.
    let latest = list
        .sessions
        .iter()
        .max_by_key(|summary| (summary.last_activity, summary.id.clone()))
        .map(|summary| summary.id.clone());
    match latest {
        Some(session) => Ok((session, false)),
        // A fresh `aj serve` holds nothing, and connect mode would otherwise
        // have nothing to attach at all.
        None => Ok((create(control, host, settings, tag).await?, true)),
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
    let settings = SessionSettings {
        model,
        // The effective config carries the value, the layers carry whether
        // anyone asked for it, so both are consulted per axis.
        thinking: stated.has("thinking").then(|| {
            let level = aj_app::model::default_thinking_from_config(config.thinking);
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
    use clap::Parser;

    use super::*;

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
        assert_eq!(settings.thinking.as_deref(), Some("high"));
    }
}
