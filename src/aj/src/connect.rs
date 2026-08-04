//! `aj connect <url>`: the client half of connect mode (spec 9.1).
//!
//! Everything here runs before a terminal is taken over, so an unreachable
//! host or a protocol mismatch is a plain CLI error rather than a notice
//! nobody sees. What it produces is the [`Control`] the shell drives, the
//! session it opens with, and the host facts the chrome needs.
//!
//! Session selection is the spec's: an explicit id, else `--new` creates,
//! else the host's most recently modified session, else create one. A create
//! carries this client's own resolved inference settings, because per-session
//! settings follow whoever creates the session (spec section 8).

use std::path::PathBuf;

use aj_app::cli::args::Args;
use aj_conf::Config;
use aj_models::{speed_name, thinking_config_name, verbosity_name};
use aj_wire::{Hello, ModelSelection, SessionSettings};
use anyhow::{Context, Result};

use crate::control::Control;
use crate::remote::RemoteClient;

/// Which session `aj connect` opens with.
pub(crate) struct ConnectTarget<'a> {
    pub(crate) url: &'a str,
    /// The session named on the command line, if any.
    pub(crate) session_id: Option<&'a str>,
    /// Whether `--new` asked for a fresh session.
    pub(crate) new: bool,
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
    let settings = creator_settings(args, config);
    let (session, created) = resolve_session(&control, &target, settings).await?;
    Ok(Connected {
        control,
        session,
        working_directory,
        created,
    })
}

/// Resolve the session to attach per spec 9.1, creating one when that is what
/// the rule says.
async fn resolve_session(
    control: &Control,
    target: &ConnectTarget<'_>,
    settings: SessionSettings,
) -> Result<(String, bool)> {
    if let Some(id) = target.session_id {
        return Ok((id.to_string(), false));
    }
    if target.new {
        return Ok((create(control, settings).await?, true));
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
        None => Ok((create(control, settings).await?, true)),
    }
}

async fn create(control: &Control, settings: SessionSettings) -> Result<String> {
    control
        .create(Some(settings), None)
        .await
        .context("could not create a session on the host")
}

/// This client's resolved inference settings, for a session it creates.
///
/// Resolved from the same inputs a local run's run config comes from (CLI
/// flags over env over config), because a connect-mode client's own defaults
/// are what its sessions should run with (spec section 8). Every axis it can
/// name is sent, since an axis the creator omits falls back to the host's
/// config rather than to this client's.
///
/// The model is the one axis that can go unnamed: with no model pinned there
/// is no `(api, name)` pair to send, and asking for "that provider's default"
/// is not something the wire can express, so the host's own default model
/// applies.
fn creator_settings(args: &Args, config: &Config) -> SessionSettings {
    let selection = aj_app::model::ModelSelection::merge(args, config);
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
    let thinking = aj_app::model::default_thinking_from_config(config.thinking);
    let verbosity = config
        .verbosity
        .map(aj_app::model::config_verbosity_to_unified);
    SessionSettings {
        model,
        thinking: Some(thinking_config_name(thinking.as_ref()).to_string()),
        thinking_display: Some(
            aj_app::session_setup::thinking_display_name(config.thinking_display).to_string(),
        ),
        speed: Some(speed_name(speed).to_string()),
        verbosity: Some(verbosity_name(verbosity).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn args(argv: &[&str]) -> Args {
        Args::try_parse_from(argv).expect("args parse")
    }

    /// With nothing pinned, every axis a client can name is still sent (the
    /// host's config only applies to what the creator omits), and the model is
    /// left out because there is no `(api, name)` pair to name.
    #[test]
    fn creator_settings_send_the_resolved_defaults() {
        let config = Config::default();
        let settings = creator_settings(&args(&["aj"]), &config);
        assert_eq!(settings.model, None);
        // The thinking default is the config's own, not a hard-coded "off":
        // this client's resolved value is what the created session runs at.
        let thinking = aj_app::model::default_thinking_from_config(config.thinking);
        assert_eq!(
            settings.thinking.as_deref(),
            Some(thinking_config_name(thinking.as_ref())),
        );
        assert_eq!(
            settings.thinking_display.as_deref(),
            Some(aj_app::session_setup::thinking_display_name(
                config.thinking_display
            )),
        );
        assert_eq!(settings.speed.as_deref(), Some("standard"));
        assert_eq!(settings.verbosity.as_deref(), Some("default"));
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
        );
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
