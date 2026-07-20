//! Frontend-agnostic settings mutation and persistence.
//!
//! This module owns the rules for what a settings change does to the
//! running session's config and to disk: staging into the loop-side
//! [`RunConfigSnapshot`] and per-sub overrides, recording the change on
//! the session log, and writing (or clearing) the value in a config
//! layer. The write mechanics themselves (format-preserving `toml_edit`
//! read-modify-write, comment and key-order preservation, the
//! cross-process `ConfigLock`) live in `aj-conf`. The functions here are
//! thin wrappers over [`Config::persist_changed`] and
//! [`ConfigLayer::persist`].
//!
//! The confirm cores return the data a frontend needs to reconcile its
//! own view (the new footer settings, whether the change applied, a row
//! correction) without this module ever touching a rendering backend.
//! Overlay construction and the pump/view reconcile stay in each
//! frontend.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aj_agent::events::{AgentId, AgentSettings};
use aj_conf::{Config, ConfigLayer, ConfigSpeed, ConfigThinkingLevel, ConfigVerbosity};
use aj_models::auth::AuthStorage;
use aj_models::registry::{ModelInfo, validate_thinking_level};
use aj_models::types::Speed;
use aj_models::{ThinkingConfig, speed_name, verbosity_name};
use aj_session::ThreadFilter;

use crate::commands::thinking_level_name;
use crate::model::{
    ResolvedModel, apply_thinking_display, apply_verbosity, config_verbosity_to_unified,
    from_model_info,
};
use crate::session::SessionCore;
use crate::session_setup::{RunConfigSnapshot, thinking_level_for};

/// The two config-file layers a frontend can edit.
///
/// The effective [`Config`] a running session reads is held separately
/// so the many readers stay unchanged. Whenever a layer changes,
/// [`Self::effective`] recomputes it. The user layer is the base. The
/// project layer overlays it (see [`ConfigLayer`]).
pub struct ConfigLayers {
    /// `~/.aj/config.toml` (defaults plus the user's overrides).
    pub user: Config,
    /// `<git-root>/.aj/config.toml` overlay; empty outside a project.
    pub project: ConfigLayer,
    /// Where the project layer persists, or `None` when the process is
    /// not inside a git repository (project editing is unavailable).
    pub project_path: Option<PathBuf>,
}

impl ConfigLayers {
    /// The effective config: the project layer overlaid on the user
    /// layer. A frontend sets its live config to this value.
    pub fn effective(&self) -> Config {
        self.project.overlay_onto(&self.user)
    }
}

/// Which configuration layer a settings edit persists to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigTarget {
    /// The user's `~/.aj/config.toml`.
    User,
    /// The current project's `<git-root>/.aj/config.toml`.
    Project,
}

/// How a confirmed setting change persists, beyond the effect it
/// always has on the running session.
///
/// The `/thinking` and `/model` overlays are session-scoped
/// ([`PersistAction::None`]); the settings windows persist to a config
/// layer as the new default for future sessions. A project clear
/// removes the key so the value falls back to the user (or built-in)
/// default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistAction {
    /// Session only: leave every config file untouched.
    None,
    /// Write the value to the user `config.toml`.
    User,
    /// Write the value to the project `config.toml` as an override.
    ProjectSet,
    /// Remove the key(s) from the project `config.toml`.
    ProjectClear,
}

impl PersistAction {
    /// The persist action for a value change (not a clear) made in a
    /// settings window targeting `target`.
    pub fn set_for(target: ConfigTarget) -> Self {
        match target {
            ConfigTarget::User => PersistAction::User,
            ConfigTarget::Project => PersistAction::ProjectSet,
        }
    }
}

/// Map the agent's live default thinking back onto its persisted
/// `config.toml` representation. The forward map
/// [`crate::model::default_thinking_from_config`] collapses
/// [`ConfigThinkingLevel::Off`] to `None`; this is its exact inverse,
/// so a popup choice round-trips through `config.toml` unchanged.
pub fn config_thinking_level(thinking: Option<&aj_models::ThinkingConfig>) -> ConfigThinkingLevel {
    use aj_models::ThinkingConfig;
    match thinking {
        None => ConfigThinkingLevel::Off,
        Some(ThinkingConfig::Minimal) => ConfigThinkingLevel::Minimal,
        Some(ThinkingConfig::Low) => ConfigThinkingLevel::Low,
        Some(ThinkingConfig::Medium) => ConfigThinkingLevel::Medium,
        Some(ThinkingConfig::High) => ConfigThinkingLevel::High,
        Some(ThinkingConfig::XHigh) => ConfigThinkingLevel::XHigh,
        Some(ThinkingConfig::Max) => ConfigThinkingLevel::Max,
    }
}

/// Apply `mutate` to the user config layer, refresh the effective
/// config the running session reads, and persist the user
/// `~/.aj/config.toml`.
///
/// Selector outcomes change live agent/TUI state for the running
/// session; this mirrors the change into the user layer so it survives
/// a restart. The in-memory mutation and the effective-config refresh
/// happen first, under the layers lock, which is dropped before the
/// file write so persistence never holds the lock across I/O. The
/// write goes through [`Config::persist_changed`] (a comment-preserving
/// per-key merge under a cross-process lock). A save failure returns a
/// user-facing notice rather than an error.
pub fn persist_user(
    layers: &Arc<Mutex<ConfigLayers>>,
    effective: &Arc<Mutex<Config>>,
    mutate: impl FnOnce(&mut Config),
) -> Option<String> {
    let (baseline, updated) = {
        let mut l = layers.lock().expect("config layers mutex poisoned");
        let baseline = l.user.clone();
        mutate(&mut l.user);
        let updated = l.user.clone();
        *effective.lock().expect("config mutex poisoned") = l.effective();
        (baseline, updated)
    };
    match updated.persist_changed(&baseline) {
        Ok(()) => None,
        Err(err) => Some(format!("(couldn't save to config.toml: {err})")),
    }
}

/// Set or clear keys in the project config layer, refresh the effective
/// config, and persist the project `<git-root>/.aj/config.toml`.
///
/// Each entry is `(option_name, Some(value) to set | None to remove)`.
/// A set stores an explicit override (presence-tracked), so even a
/// value equal to the built-in default is written and shadows the user
/// layer. Mirrors [`persist_user`]'s discipline: the in-memory edit and
/// effective-config refresh run under the layers lock, the file write
/// off it. Reports the first set error (or a save failure) as a notice;
/// returns a notice too when there is no project (not in a git repo).
pub fn persist_project(
    layers: &Arc<Mutex<ConfigLayers>>,
    effective: &Arc<Mutex<Config>>,
    entries: &[(&str, Option<&str>)],
) -> Option<String> {
    let (baseline, updated, path, set_error) = {
        let mut l = layers.lock().expect("config layers mutex poisoned");
        let Some(path) = l.project_path.clone() else {
            return Some("(no project config: not inside a git repository)".to_string());
        };
        let baseline = l.project.clone();
        let mut set_error = None;
        for (key, value) in entries {
            match value {
                Some(v) => {
                    if let Err(e) = l.project.set_str(key, v) {
                        set_error = Some(format!("(couldn't set {key}: {e})"));
                        break;
                    }
                }
                None => l.project.clear(key),
            }
        }
        let updated = l.project.clone();
        *effective.lock().expect("config mutex poisoned") = l.effective();
        (baseline, updated, path, set_error)
    };
    if let Some(err) = set_error {
        return Some(err);
    }
    match updated.persist(&baseline, &path) {
        Ok(()) => None,
        Err(err) => Some(format!("(couldn't save to project config.toml: {err})")),
    }
}

/// Persist a single-key settings change to the layer named by
/// `persist`, the common case for the settings-window arms.
///
/// `value` is the canonical string to write (`None` means "unset this
/// key"). For the user layer the change is applied via `user_mutate`
/// (which keeps the existing default-dropping semantics: a value equal
/// to the default removes the key). For the project layer the value is
/// stored verbatim as an override, or removed when `value` is `None`
/// (an explicit "unset"/"default" choice) or `persist` is a clear.
pub fn persist_setting(
    layers: &Arc<Mutex<ConfigLayers>>,
    effective: &Arc<Mutex<Config>>,
    persist: PersistAction,
    key: &str,
    value: Option<&str>,
    user_mutate: impl FnOnce(&mut Config),
) -> Option<String> {
    match persist {
        PersistAction::None => None,
        PersistAction::User => persist_user(layers, effective, user_mutate),
        PersistAction::ProjectSet => persist_project(layers, effective, &[(key, value)]),
        PersistAction::ProjectClear => persist_project(layers, effective, &[(key, None)]),
    }
}

/// New footer identity a frontend should surface after a main-agent
/// settings change, plus its context-window denominator. The frontend
/// notes it so the footer's model line and context gauge reflect the
/// change immediately rather than waiting for the next turn.
pub struct FooterUpdate {
    pub settings: AgentSettings,
    pub context_window: u64,
}

/// Result of a main-agent thinking or model confirm.
///
/// `footer` is `Some` when the change applied and the frontend should
/// refresh the Main footer entry. It is `None` when the change did not
/// apply (a provider rebuild failure) and the footer is left as-is.
pub struct MainConfirm {
    pub footer: Option<FooterUpdate>,
    pub notice: String,
}

/// Result of a main-agent speed confirm.
///
/// Speed rebuilds the provider bundle, which can fail (e.g. a provider
/// not in the registry). On failure nothing is staged and the frontend
/// reverts the settings row to `previous`.
pub enum SpeedConfirm {
    /// The rebuild succeeded: the change is staged and logged. Carries
    /// the new footer identity and the confirmation notice.
    Applied {
        footer: FooterUpdate,
        notice: String,
    },
    /// The rebuild failed: nothing staged. The frontend should revert
    /// the speed row to `previous` and show `notice`.
    Failed { previous: String, notice: String },
}

/// Result of a sub-agent thinking or model confirm.
///
/// `applied` is true only when the change was staged into the sub's
/// override map (the target was promptable and any validation passed),
/// which is the signal for the frontend to refresh the target's footer
/// entry. On the not-promptable and validation-rejected paths it is
/// false and nothing was staged.
pub struct SubConfirm {
    pub notice: String,
    pub applied: bool,
}

/// Apply a confirmed thinking pick to the main agent: stage it into the
/// run config, record it on the session log's user thread, and persist
/// it per `persist`. Returns the new footer identity and the notice.
/// The frontend applies the border tint and footer note.
pub async fn confirm_thinking_for_main(
    level: Option<ThinkingConfig>,
    persist: PersistAction,
    run_config: &Arc<Mutex<RunConfigSnapshot>>,
    config: &Arc<Mutex<Config>>,
    layers: &Arc<Mutex<ConfigLayers>>,
    core: &SessionCore,
) -> MainConfirm {
    // Stage the new thinking effort into the loop-side snapshot; the
    // next turn applies it. Never locks the agent, so it's safe while
    // a turn is running (the in-flight turn keeps its effort; the
    // change takes effect next turn). Read the rest of the settings
    // identity back for the footer entry.
    let (settings, context_window) = {
        let mut cfg = run_config.lock().expect("run config mutex poisoned");
        cfg.thinking = level.clone();
        (
            AgentSettings {
                provider: cfg.model_key.0.clone(),
                model_id: cfg.model_key.1.clone(),
                thinking: thinking_level_name(&level).to_string(),
                speed: speed_name(cfg.speed).to_string(),
                verbosity: verbosity_name(cfg.stream_options.verbosity).to_string(),
            },
            cfg.model_info.context_window,
        )
    };
    let name = thinking_level_name(&level);
    // Record the change on the session log's user thread so a later
    // resume restores this level.
    let log_note = {
        let mut log = core.log.lock().await;
        log.append_thinking_change(ThreadFilter::USER, name)
            .err()
            .map(|err| format!("(couldn't record in session log: {err})"))
    };
    // Persist as the new default only when the change should outlive
    // this session (the settings windows). The `/thinking` overlay
    // command is session-scoped: it relies on the session-log record
    // above to survive a resume and leaves the default untouched.
    let save_note = persist_setting(
        layers,
        config,
        persist,
        "thinking",
        Some(thinking_level_name(&level)),
        |c| c.thinking = Some(config_thinking_level(level.as_ref())),
    );
    let mut notice = format!("Thinking effort set to {name}.");
    for note in [save_note, log_note].into_iter().flatten() {
        notice.push(' ');
        notice.push_str(&note);
    }
    MainConfirm {
        footer: Some(FooterUpdate {
            settings,
            context_window,
        }),
        notice,
    }
}

/// Apply a confirmed thinking pick to sub-agent `n`: validate against
/// the target's model, stage into the sub-override map (applied at the
/// sub's next turn start), and record on the sub's log thread.
/// Deliberately does not touch `config.toml` or the run config. Those
/// record the session default, which is main's concern.
///
/// `tracked_model` is the model the frontend currently shows for the
/// target, resolved to a catalog entry. It is the validation fallback
/// used when no bundle override is staged for the agent. Validation is
/// lenient: with no model to check against it is skipped, matching
/// scripted mode.
pub async fn confirm_thinking_for_sub(
    level: Option<ThinkingConfig>,
    n: usize,
    tracked_model: Option<Arc<ModelInfo>>,
    core: &SessionCore,
) -> SubConfirm {
    let target = AgentId::Sub(n);
    if core.resolve_agent(target).is_none() {
        return SubConfirm {
            notice: "This agent can't be prompted.".to_string(),
            applied: false,
        };
    }
    let name = thinking_level_name(&level);
    // Validate the chosen level (including off) against the target's
    // model: the staged bundle override's info if present, else the model
    // the frontend tracks, else skip (no model in scope, e.g. scripted).
    let wire = level
        .as_ref()
        .map(thinking_level_for)
        .unwrap_or(aj_models::types::ThinkingLevel::Off);
    let target_info: Option<Arc<ModelInfo>> = {
        let overrides = core
            .sub_overrides
            .lock()
            .expect("sub overrides mutex poisoned");
        overrides
            .get(&n)
            .and_then(|o| o.bundle.as_ref())
            .map(|(_, info, _, _)| Arc::clone(info))
    }
    .or(tracked_model);
    if let Some(info) = target_info
        && let Err(msg) = validate_thinking_level(&info, &wire)
    {
        return SubConfirm {
            notice: format!("Can't set thinking level {name:?} for agent {n}: {msg}"),
            applied: false,
        };
    }
    // Stage the standing choice; the sub's next turn applies it.
    core.sub_overrides
        .lock()
        .expect("sub overrides mutex poisoned")
        .entry(n)
        .or_default()
        .thinking = Some(level.clone());
    // Record the change on the sub-agent's log thread so a resumed
    // transcript reflects it.
    let log_note = {
        let mut log = core.log.lock().await;
        log.append_thinking_change(ThreadFilter::subagent(n), name)
            .err()
            .map(|err| format!("(couldn't record in session log: {err})"))
    };
    let mut notice = format!("Thinking effort set to {name} for agent {n}.");
    if let Some(note) = log_note {
        notice.push(' ');
        notice.push_str(&note);
    }
    SubConfirm {
        notice,
        applied: true,
    }
}

/// Apply a confirmed model pick to the main agent: rebuild the bundle,
/// stage it into the run config, record it on the session log's user
/// thread, and (per `persist`) write or clear the choice in a config
/// layer as the default for new sessions. Returns the new footer
/// identity (or `None` on a rebuild failure) and the notice.
pub async fn confirm_model_for_main(
    info: ModelInfo,
    persist: PersistAction,
    auth: &AuthStorage,
    run_config: &Arc<Mutex<RunConfigSnapshot>>,
    config: &Arc<Mutex<Config>>,
    layers: &Arc<Mutex<ConfigLayers>>,
    core: &SessionCore,
) -> MainConfirm {
    // Construct a fresh provider handle from the picked catalog entry,
    // carrying the active speed over so e.g. `--speed fast` survives a
    // model pick (degrading silently on providers that ignore it).
    let speed = {
        let cfg = run_config.lock().expect("run config mutex poisoned");
        cfg.speed
    };
    match from_model_info(auth, info.clone(), speed) {
        Ok(ResolvedModel {
            provider,
            model_info,
            mut stream_options,
        }) => {
            // Re-apply the configured thinking-display mode and
            // verbosity: the rebuilt baseline options would otherwise
            // silently drop them on every model swap.
            let (display, verbosity) = {
                let cfg = config.lock().expect("config mutex poisoned");
                (cfg.thinking_display, cfg.verbosity)
            };
            apply_thinking_display(&mut stream_options, display);
            apply_verbosity(&mut stream_options, verbosity);
            // Stage the swap into the loop-side snapshot (provider +
            // model + options + the pre-select key); the next turn
            // applies it. Never locks the agent, so it's safe mid-turn —
            // the in-flight turn keeps its model and the swap takes
            // effect next turn. Thinking effort is preserved; read it
            // back for the footer entry.
            let (current_thinking, current_verbosity) = {
                let mut cfg = run_config.lock().expect("run config mutex poisoned");
                cfg.provider = provider;
                cfg.model_info = model_info;
                cfg.stream_options = stream_options;
                cfg.model_key = (info.provider.clone(), info.id.clone());
                (cfg.thinking.clone(), cfg.stream_options.verbosity)
            };
            // Record the new settings identity so the footer's model
            // line and context-window denominator reflect the swap
            // immediately rather than waiting for the next turn.
            let settings = AgentSettings {
                provider: info.provider.clone(),
                model_id: info.id.clone(),
                thinking: thinking_level_name(&current_thinking).to_string(),
                speed: speed_name(speed).to_string(),
                verbosity: verbosity_name(current_verbosity).to_string(),
            };
            let context_window = info.context_window;
            // Record the change on the session log's user thread so a
            // later resume restores this model.
            let log_note = {
                let mut log = core.log.lock().await;
                log.append_model_change(ThreadFilter::USER, &info.provider, &info.id)
                    .err()
                    .map(|err| format!("(couldn't record in session log: {err})"))
            };
            // Persist the model choice (provider + id) as the new
            // default only when the change should outlive this session
            // (the settings windows). The `/model` overlay command is
            // session-scoped: it relies on the session-log record above
            // to survive a resume and leaves the default untouched.
            // `model_url` is intentionally left untouched: it's a
            // user-supplied endpoint override, not part of "which
            // model", and pinning the catalog's base URL into it would
            // freeze out future `models.json` updates.
            let save_note = match persist {
                PersistAction::None => None,
                PersistAction::User => persist_user(layers, config, |c| {
                    c.model_api = Some(info.provider.clone());
                    c.model_name = Some(info.id.clone());
                }),
                PersistAction::ProjectSet => persist_project(
                    layers,
                    config,
                    &[
                        ("model_api", Some(info.provider.as_str())),
                        ("model_name", Some(info.id.as_str())),
                    ],
                ),
                PersistAction::ProjectClear => {
                    persist_project(layers, config, &[("model_api", None), ("model_name", None)])
                }
            };
            let mut notice = format!(
                "Model set to {} ({}/{}).",
                info.name, info.provider, info.id
            );
            for note in [save_note, log_note].into_iter().flatten() {
                notice.push(' ');
                notice.push_str(&note);
            }
            MainConfirm {
                footer: Some(FooterUpdate {
                    settings,
                    context_window,
                }),
                notice,
            }
        }
        Err(err) => MainConfirm {
            footer: None,
            notice: format!("Failed to switch to {}: {err}", info.name),
        },
    }
}

/// Apply a confirmed model pick to sub-agent `n`: rebuild the bundle at
/// `effective_speed` and stage it into the sub-override map (applied at
/// the sub's next turn start), then record on the sub's log thread.
/// Deliberately does not touch `config.toml` or the run config.
///
/// `effective_speed` is the speed the frontend resolved for the target
/// (its staged override if any, else its tracked speed), so the rebuilt
/// bundle re-stamps the same speed-derived headers.
pub async fn confirm_model_for_sub(
    info: &ModelInfo,
    n: usize,
    auth: &AuthStorage,
    effective_speed: Option<Speed>,
    core: &SessionCore,
) -> SubConfirm {
    let target = AgentId::Sub(n);
    if core.resolve_agent(target).is_none() {
        return SubConfirm {
            notice: "This agent can't be prompted.".to_string(),
            applied: false,
        };
    }
    match from_model_info(auth, info.clone(), effective_speed) {
        Ok(ResolvedModel {
            provider,
            model_info,
            stream_options,
        }) => {
            // Stage the standing bundle choice; the sub's next turn
            // applies it.
            //
            // NOTE(aljoscha): the rebuilt bundle's `stream_options`
            // come from `from_model_info` (defaults), so a sub's
            // `thinking_display` and `verbosity` revert to the server
            // default on a model swap. Unlike the main path
            // (`confirm_model_for_main`), we don't re-apply the config
            // values here. The two settings behave identically, and
            // sub-agent display tuning isn't exposed, so we accept the
            // gap rather than thread config through the sub path.
            core.sub_overrides
                .lock()
                .expect("sub overrides mutex poisoned")
                .entry(n)
                .or_default()
                .bundle = Some((
                provider,
                model_info,
                stream_options,
                (info.provider.clone(), info.id.clone()),
            ));
            // Record the change on the sub-agent's log thread so a
            // resumed transcript reflects it.
            let log_note = {
                let mut log = core.log.lock().await;
                log.append_model_change(ThreadFilter::subagent(n), &info.provider, &info.id)
                    .err()
                    .map(|err| format!("(couldn't record in session log: {err})"))
            };
            let mut notice = format!(
                "Model set to {} ({}/{}) for agent {n}.",
                info.name, info.provider, info.id
            );
            if let Some(note) = log_note {
                notice.push(' ');
                notice.push_str(&note);
            }
            SubConfirm {
                notice,
                applied: true,
            }
        }
        Err(err) => SubConfirm {
            notice: format!("Failed to switch to {}: {err}", info.name),
            applied: false,
        },
    }
}

/// Apply a confirmed output-verbosity pick to the main agent: stage it
/// onto the run config's stream options, persist per `persist`, and
/// record it on the session log's user thread. Verbosity is a plain
/// stream-option field (no headers, no bundle rebuild), so unlike
/// [`confirm_speed_for_main`] this neither rebuilds the provider nor
/// touches the footer. Providers gate the field on per-model support,
/// so on a model that ignores verbosity this records the preference
/// without changing what's sent. Returns the user-facing notice.
pub async fn confirm_verbosity_for_main(
    verbosity: Option<ConfigVerbosity>,
    persist: PersistAction,
    run_config: &Arc<Mutex<RunConfigSnapshot>>,
    config: &Arc<Mutex<Config>>,
    layers: &Arc<Mutex<ConfigLayers>>,
    core: &SessionCore,
) -> String {
    let unified = verbosity.map(config_verbosity_to_unified);
    let name = verbosity_name(unified);
    {
        let mut cfg = run_config.lock().expect("run config mutex poisoned");
        cfg.stream_options.verbosity = unified;
    }
    // Record on the user thread so a later resume restores this value.
    let log_note = {
        let mut log = core.log.lock().await;
        log.append_verbosity_change(ThreadFilter::USER, name)
            .err()
            .map(|err| format!("(couldn't record in session log: {err})"))
    };
    // The verbosity name (`low`/`medium`/`high`) is the canonical
    // config value; `None` means "unset" and removes the key.
    let verbosity_str = verbosity.map(|v| v.to_string());
    let save_note = persist_setting(
        layers,
        config,
        persist,
        "verbosity",
        verbosity_str.as_deref(),
        |c| c.verbosity = verbosity,
    );
    let mut notice = format!("Output verbosity set to {name}. Takes effect next turn.");
    for note in [save_note, log_note].into_iter().flatten() {
        notice.push(' ');
        notice.push_str(&note);
    }
    notice
}

/// Apply a speed change to the main agent: rebuild the provider bundle
/// at the current model so the speed-derived headers are re-stamped,
/// stage it into the run config, persist per `persist`, and record on
/// the session log's user thread. On a rebuild failure (e.g. scripted
/// mode, whose provider isn't in the registry) nothing is staged and
/// the caller reverts the settings row via [`SpeedConfirm::Failed`].
pub async fn confirm_speed_for_main(
    speed: Option<Speed>,
    persist: PersistAction,
    auth: &AuthStorage,
    run_config: &Arc<Mutex<RunConfigSnapshot>>,
    config: &Arc<Mutex<Config>>,
    layers: &Arc<Mutex<ConfigLayers>>,
    core: &SessionCore,
) -> SpeedConfirm {
    let name = speed_name(speed);
    let (model_info, prev_speed) = {
        let cfg = run_config.lock().expect("run config mutex poisoned");
        ((*cfg.model_info).clone(), cfg.speed)
    };
    match from_model_info(auth, model_info, speed) {
        Ok(ResolvedModel {
            provider,
            model_info,
            mut stream_options,
        }) => {
            // The rebuilt baseline options would otherwise drop the
            // configured thinking-display mode and verbosity.
            let (display, verbosity) = {
                let cfg = config.lock().expect("config mutex poisoned");
                (cfg.thinking_display, cfg.verbosity)
            };
            apply_thinking_display(&mut stream_options, display);
            apply_verbosity(&mut stream_options, verbosity);
            // Stage into the loop-side snapshot; the next turn applies
            // it. Never locks the agent, so it's safe mid-turn.
            let (settings, context_window) = {
                let mut cfg = run_config.lock().expect("run config mutex poisoned");
                cfg.provider = provider;
                cfg.model_info = model_info;
                cfg.stream_options = stream_options;
                cfg.speed = speed;
                (
                    AgentSettings {
                        provider: cfg.model_key.0.clone(),
                        model_id: cfg.model_key.1.clone(),
                        thinking: thinking_level_name(&cfg.thinking).to_string(),
                        speed: name.to_string(),
                        verbosity: verbosity_name(cfg.stream_options.verbosity).to_string(),
                    },
                    cfg.model_info.context_window,
                )
            };
            // Record the change on the session log's user thread so a
            // later resume restores this speed.
            let log_note = {
                let mut log = core.log.lock().await;
                log.append_speed_change(ThreadFilter::USER, name)
                    .err()
                    .map(|err| format!("(couldn't record in session log: {err})"))
            };
            // "standard" persists to the user layer as key removal:
            // it's the default, and `speed_from_name` maps it to `None`
            // on the wire. The project layer stores it explicitly so it
            // can override a user `fast`.
            let save_note = persist_setting(layers, config, persist, "speed", Some(name), |c| {
                c.speed = match speed {
                    None | Some(Speed::Standard) => None,
                    Some(Speed::Fast) => Some(ConfigSpeed::Fast),
                };
            });
            let mut notice = format!("Speed set to {name}. Takes effect next turn.");
            for note in [save_note, log_note].into_iter().flatten() {
                notice.push(' ');
                notice.push_str(&note);
            }
            SpeedConfirm::Applied {
                footer: FooterUpdate {
                    settings,
                    context_window,
                },
                notice,
            }
        }
        Err(err) => SpeedConfirm::Failed {
            previous: speed_name(prev_speed).to_string(),
            notice: format!("Failed to set speed {name}: {err}"),
        },
    }
}

/// Fully composed settings-window description for `option`: the schema
/// one-liner plus the settings-window note the interactive windows show
/// below the highlighted row.
///
/// This is the single source of the description copy both frontends
/// render. It is frontend-neutral: an addendum that only applies to one
/// frontend (e.g. the aj-classic note that `show_frame_stats` only
/// affects the aj-next TUI) is appended by that frontend, not here.
pub fn option_description(option: &aj_conf::ConfigOption) -> String {
    match option.name {
        // The model row folds `model_api` + `model_name`, so its text names
        // both keys rather than describing `model_api` alone.
        "model_api" => "Model the main agent uses, applied from the next turn. \
             Persisted as model_api + model_name."
            .to_string(),
        "model_url" => describe(
            option,
            "Takes effect on restart. Submit an empty value to unset.",
        ),
        "thinking_display" => describe(
            option,
            "\"default\" keeps the provider's stock behavior. Takes effect next turn.",
        ),
        "speed" => describe(option, "Takes effect next turn."),
        "verbosity" => describe(
            option,
            "\"default\" leaves the server default. Takes effect next turn.",
        ),
        "disabled_tools" | "disabled_skills" => describe(
            option,
            "Toggles apply when the picker closes; takes effect for new sessions.",
        ),
        "image_auto_resize" | "image_block" | "syntax_highlighting" => {
            describe(option, "Takes effect for new sessions.")
        }
        "compact_threshold" => describe(option, "A fraction between 0.0 and 1.0."),
        "compact_keep_recent" => describe(option, "A positive number of tokens."),
        // Plain schema string: thinking, theme, show_thinking_block,
        // show_token_usage, show_image_in_terminal, auto_compact, bash_rtk,
        // show_frame_stats, and model_name (folded into the model row, never
        // shown alone).
        _ => option.description.to_string(),
    }
}

/// Schema description plus a settings-window note.
fn describe(option: &aj_conf::ConfigOption, note: &str) -> String {
    format!("{} {}", option.description, note)
}

#[cfg(test)]
mod tests {
    use aj_conf::Config;

    use crate::settings::option_description;

    fn option(name: &str) -> &'static aj_conf::ConfigOption {
        Config::OPTIONS
            .iter()
            .find(|o| o.name == name)
            .expect("option exists")
    }

    #[test]
    fn model_api_uses_the_custom_folded_text() {
        assert_eq!(
            option_description(option("model_api")),
            "Model the main agent uses, applied from the next turn. \
             Persisted as model_api + model_name."
        );
    }

    #[test]
    fn noted_option_appends_the_settings_note() {
        let speed = option("speed");
        assert_eq!(
            option_description(speed),
            format!("{} Takes effect next turn.", speed.description)
        );
    }

    #[test]
    fn plain_option_returns_the_schema_string() {
        let thinking = option("thinking");
        assert_eq!(option_description(thinking), thinking.description);
    }

    #[test]
    fn show_frame_stats_is_frontend_neutral() {
        let opt = option("show_frame_stats");
        // No aj-classic addendum here: the frontend appends it.
        assert_eq!(option_description(opt), opt.description);
        assert!(!option_description(opt).contains("aj-next"));
    }

    #[test]
    fn every_noted_option_appends_its_exact_note() {
        // Golden table of the settings-specific note each option appends to its
        // schema description. Guards against a note being dropped or altered,
        // which would silently thin the settings-window help text.
        let noted: &[(&str, &str)] = &[
            (
                "model_url",
                "Takes effect on restart. Submit an empty value to unset.",
            ),
            (
                "thinking_display",
                "\"default\" keeps the provider's stock behavior. Takes effect next turn.",
            ),
            ("speed", "Takes effect next turn."),
            (
                "verbosity",
                "\"default\" leaves the server default. Takes effect next turn.",
            ),
            (
                "disabled_tools",
                "Toggles apply when the picker closes; takes effect for new sessions.",
            ),
            (
                "disabled_skills",
                "Toggles apply when the picker closes; takes effect for new sessions.",
            ),
            ("image_auto_resize", "Takes effect for new sessions."),
            ("image_block", "Takes effect for new sessions."),
            ("syntax_highlighting", "Takes effect for new sessions."),
            ("compact_threshold", "A fraction between 0.0 and 1.0."),
            ("compact_keep_recent", "A positive number of tokens."),
        ];
        for (name, note) in noted {
            let opt = option(name);
            assert_eq!(
                option_description(opt),
                format!("{} {}", opt.description, note),
                "option {name} note drifted"
            );
        }
    }
}
