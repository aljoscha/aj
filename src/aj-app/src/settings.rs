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
use aj_conf::{
    Config, ConfigLayer, ConfigSpeed, ConfigThinkingDisplay, ConfigThinkingLevel, ConfigVerbosity,
};
use aj_models::auth::AuthStorage;
use aj_models::registry::{ModelInfo, validate_thinking_level};
use aj_models::types::Speed;
use aj_models::{ThinkingConfig, speed_name, verbosity_name};
use aj_session::{EntryRef, ThreadFilter};

use crate::commands::thinking_level_name;
use crate::model::{
    ResolvedModel, apply_thinking_display, config_verbosity_to_unified, from_model_info,
};
use crate::session::SessionCore;
use crate::session_setup::{
    ModelConfig, RunConfigSnapshot, thinking_display_name, thinking_level_for,
};

/// Presentation belongs to the connecting frontend, not the session host.
/// All other schema options are host-owned defaults.
pub fn is_presentation(key: &str) -> bool {
    matches!(
        key,
        "theme"
            | "show_thinking_block"
            | "show_token_usage"
            | "compact_transcript"
            | "show_frame_stats"
            | "sidebar_cols"
            | "show_image_in_terminal"
            | "syntax_highlighting"
            | "keybindings"
    )
}

/// Editable schema values, returned intact to the trusted peer.
/// AuthStorage records and arbitrary config-file keys are not settings.
pub fn host_values(config: &Config) -> std::collections::BTreeMap<String, String> {
    let mut values = schema_values(config);
    values.retain(|key, _| !is_presentation(key));
    values
}

/// Every schema option in the editor's input vocabulary. Unset optionals keep
/// the schema's explicit sentinel, distinct from an empty string.
pub fn schema_values(config: &Config) -> std::collections::BTreeMap<String, String> {
    Config::OPTIONS
        .iter()
        .map(|option| {
            let value = if matches!(option.kind, aj_conf::ValueKind::StringList) {
                option
                    .to_toml(config)
                    .and_then(|item| {
                        item.as_array().map(|array| {
                            array
                                .iter()
                                .filter_map(|item| item.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                    })
                    .unwrap_or_default()
            } else {
                option.display(config)
            };
            (option.name.to_string(), value)
        })
        .collect()
}

/// The synthetic settings row folding `model_api` + `model_name` into one
/// picker-backed entry. Its value is a `provider/id` string.
pub const MODEL_SETTING_ID: &str = "model";

/// The "leave unset" value for options whose absence has its own meaning
/// (`thinking_display`, `verbosity`). Writing it removes the key.
pub const UNSET_VALUE: &str = "default";

/// Whether `key` is an inference axis: a value the running session applies
/// as well as one the config files hold. Everything else in the schema is
/// config only.
pub fn is_axis(key: &str) -> bool {
    matches!(
        key,
        MODEL_SETTING_ID
            | "thinking"
            | "thinking_display"
            | "speed"
            | "verbosity"
            | "oracle_model"
            | "oracle_thinking"
            | "oracle_speed"
            | "oracle_verbosity"
    )
}

/// The settings axis an editor row names, with the model resolved against
/// `models`.
pub fn setting_axis(
    models: &[ModelInfo],
    key: &str,
    value: &str,
) -> Result<crate::host::SettingsAxis, String> {
    use crate::host::SettingsAxis;
    if !is_axis(key) {
        return Err(format!("Unknown settings axis {key:?}."));
    }
    let (target, key) = match key.strip_prefix("oracle_") {
        Some(key) => (ModelTarget::Oracle, key),
        None => (ModelTarget::Main, key),
    };
    let axis = match key {
        MODEL_SETTING_ID => value
            .split_once('/')
            .and_then(|(provider, model)| {
                models
                    .iter()
                    .find(|info| info.provider == provider && info.id == model)
            })
            .cloned()
            .map(SettingsAxis::Model)
            .ok_or_else(|| format!("Unknown model {value}.")),
        "thinking" => aj_models::thinking_config_from_name(value)
            .map(SettingsAxis::Thinking)
            .ok_or_else(|| format!("Unknown thinking level {value:?}.")),
        "speed" => aj_models::speed_from_name(value)
            .map(SettingsAxis::Speed)
            .ok_or_else(|| format!("Unknown speed {value:?}.")),
        "thinking_display" if value == UNSET_VALUE => Ok(SettingsAxis::ThinkingDisplay(None)),
        "thinking_display" => value
            .parse::<ConfigThinkingDisplay>()
            .map(|value| SettingsAxis::ThinkingDisplay(Some(value))),
        "verbosity" if value == UNSET_VALUE => Ok(SettingsAxis::Verbosity(None)),
        "verbosity" => value
            .parse::<ConfigVerbosity>()
            .map(|value| SettingsAxis::Verbosity(Some(value))),
        _ => Err(format!("Unknown settings axis {key:?}.")),
    }?;
    Ok(target.axis(axis))
}

/// Write one config value into the layer the edit names, after validating
/// the whole edit. Axis keys take the editor's vocabulary and land the same
/// way a persisted settings command lands them, without touching any
/// session. Presentation keys belong to the frontend and are refused.
pub fn edit_config(
    layers: &Arc<Mutex<ConfigLayers>>,
    effective: &Arc<Mutex<Config>>,
    models: &[ModelInfo],
    edit: aj_wire::ConfigEdit,
) -> Result<(), crate::host::HostError> {
    use crate::host::HostError;
    let option = Config::option(&edit.key).filter(|_| !is_presentation(&edit.key));
    if option.is_none() && !is_axis(&edit.key) {
        return Err(HostError::Invalid(format!(
            "Unknown setting {:?}.",
            edit.key
        )));
    }
    match edit.persist {
        PersistAction::None => {
            return Err(HostError::Invalid(
                "A config edit names the layer to write.".into(),
            ));
        }
        PersistAction::ProjectClear if edit.value.is_some() => {
            return Err(HostError::Invalid(
                "A project clear must not carry a value.".into(),
            ));
        }
        PersistAction::User | PersistAction::ProjectSet if edit.value.is_none() => {
            return Err(HostError::Invalid(
                "A setting write requires a value.".into(),
            ));
        }
        _ => (),
    }
    if matches!(
        edit.persist,
        PersistAction::ProjectSet | PersistAction::ProjectClear
    ) && layers
        .lock()
        .expect("config layers mutex poisoned")
        .project_path
        .is_none()
    {
        return Err(HostError::Invalid(
            "Project settings need a git repository on the host.".into(),
        ));
    }
    let note = if is_axis(&edit.key) {
        match &edit.value {
            Some(value) => {
                let axis = setting_axis(models, &edit.key, value).map_err(HostError::Invalid)?;
                persist_axis(layers, effective, edit.persist, &axis)
            }
            None if edit.key == MODEL_SETTING_ID || edit.key == "oracle_model" => persist_project(
                layers,
                effective,
                &[
                    (
                        if edit.key == MODEL_SETTING_ID {
                            "model_api"
                        } else {
                            "oracle_model_api"
                        },
                        None,
                    ),
                    (
                        if edit.key == MODEL_SETTING_ID {
                            "model_name"
                        } else {
                            "oracle_model_name"
                        },
                        None,
                    ),
                ],
            ),
            None => persist_project(layers, effective, &[(&edit.key, None)]),
        }
    } else {
        let option = option.expect("checked above");
        if let Some(value) = &edit.value {
            option
                .apply_str(value, &mut Config::default())
                .map_err(|err| HostError::Invalid(err.to_string()))?;
        }
        let value = edit.value.as_deref().filter(|value| {
            !matches!(edit.key.as_str(), "model_url" | "oracle_model_url") || !value.is_empty()
        });
        persist_setting(
            layers,
            effective,
            edit.persist,
            &edit.key,
            value,
            |config| {
                if edit.key == "model_url" {
                    config.model_url = value.map(String::from);
                } else if edit.key == "oracle_model_url" {
                    config.oracle_model_url = value.map(String::from);
                } else if let Some(value) = value {
                    option
                        .apply_str(value, config)
                        .expect("validated schema value");
                }
            },
        )
    };
    match note {
        Some(note) => Err(HostError::Internal(note.into())),
        None => Ok(()),
    }
}

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
    /// Serializes complete edits without holding the snapshot lock during I/O.
    pub writes: Arc<Mutex<()>>,
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

pub use aj_wire::PersistAction;

impl ConfigTarget {
    /// Persist a value change to this config layer.
    pub fn persist_action(self) -> PersistAction {
        match self {
            Self::User => PersistAction::User,
            Self::Project => PersistAction::ProjectSet,
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

/// Write a user-layer edit to `~/.aj/config.toml`, then publish it in memory.
///
/// Disk goes first, so memory only ever reflects what was saved: a failed
/// save leaves the layers and the effective config unchanged, and retrying
/// the same value writes again instead of diffing against a layer that
/// already has it. Edits sharing `layers` serialize from the baseline read
/// through publication. Snapshot readers do not wait for the write lock or
/// disk I/O. Callers on an async runtime must offload this blocking operation.
pub fn persist_user(
    layers: &Arc<Mutex<ConfigLayers>>,
    effective: &Arc<Mutex<Config>>,
    mutate: impl Fn(&mut Config),
) -> Option<String> {
    update_user(layers, effective, |baseline| {
        let mut updated = baseline.clone();
        mutate(&mut updated);
        updated.persist_changed(baseline)?;
        Ok(updated)
    })
}

/// Serialize an edit and publish its in-memory changes only after saving.
fn update_user(
    layers: &Arc<Mutex<ConfigLayers>>,
    effective: &Arc<Mutex<Config>>,
    write: impl FnOnce(&Config) -> Result<Config, aj_conf::ConfigError>,
) -> Option<String> {
    let writes = Arc::clone(&layers.lock().expect("config layers mutex poisoned").writes);
    let _write = writes.lock().expect("config write mutex poisoned");
    let baseline = layers
        .lock()
        .expect("config layers mutex poisoned")
        .user
        .clone();
    let updated = match write(&baseline) {
        Ok(updated) => updated,
        Err(err) => return Some(format!("(couldn't save to config.toml: {err})")),
    };
    let mut layers = layers.lock().expect("config layers mutex poisoned");
    layers.user = updated;
    *effective.lock().expect("config mutex poisoned") = layers.effective();
    None
}

/// Set or clear project-layer overrides in `<git-root>/.aj/config.toml`,
/// then publish them in memory, with the same discipline as [`persist_user`].
pub fn persist_project(
    layers: &Arc<Mutex<ConfigLayers>>,
    effective: &Arc<Mutex<Config>>,
    entries: &[(&str, Option<&str>)],
) -> Option<String> {
    let writes = Arc::clone(&layers.lock().expect("config layers mutex poisoned").writes);
    let _write = writes.lock().expect("config write mutex poisoned");
    fn apply(layer: &mut ConfigLayer, entries: &[(&str, Option<&str>)]) -> Option<String> {
        for (key, value) in entries {
            match value {
                Some(value) => {
                    if let Err(err) = layer.set_str(key, value) {
                        return Some(format!("(couldn't set {key}: {err})"));
                    }
                }
                None => layer.clear(key),
            }
        }
        None
    }
    let (baseline, path) = {
        let layers = layers.lock().expect("config layers mutex poisoned");
        let Some(path) = layers.project_path.clone() else {
            return Some("(no project config: not inside a git repository)".to_string());
        };
        (layers.project.clone(), path)
    };
    let mut updated = baseline.clone();
    if let Some(note) = apply(&mut updated, entries) {
        return Some(note);
    }
    if let Err(err) = updated.persist(&baseline, &path) {
        return Some(format!("(couldn't save to project config.toml: {err})"));
    }
    let mut layers = layers.lock().expect("config layers mutex poisoned");
    layers.project = updated;
    *effective.lock().expect("config mutex poisoned") = layers.effective();
    None
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
    user_mutate: impl Fn(&mut Config),
) -> Option<String> {
    match persist {
        PersistAction::None => None,
        PersistAction::User => persist_user(layers, effective, user_mutate),
        PersistAction::ProjectSet => persist_project(layers, effective, &[(key, value)]),
        PersistAction::ProjectClear => persist_project(layers, effective, &[(key, None)]),
    }
}

/// The independently configured model whose settings a session edit changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTarget {
    Main,
    Oracle,
}

impl ModelTarget {
    pub fn model(self, run: &RunConfigSnapshot) -> &ModelConfig {
        match self {
            Self::Main => &run.main,
            Self::Oracle => &run.oracle,
        }
    }
    pub fn model_mut(self, run: &mut RunConfigSnapshot) -> &mut ModelConfig {
        match self {
            Self::Main => &mut run.main,
            Self::Oracle => &mut run.oracle,
        }
    }
    fn key(self, key: &'static str) -> &'static str {
        match (self, key) {
            (Self::Main, _) => key,
            (Self::Oracle, "model_api") => "oracle_model_api",
            (Self::Oracle, "model_name") => "oracle_model_name",
            (Self::Oracle, "thinking") => "oracle_thinking",
            (Self::Oracle, "speed") => "oracle_speed",
            (Self::Oracle, "verbosity") => "oracle_verbosity",
            _ => unreachable!("unsupported model setting"),
        }
    }
    /// Target an ordinary model, effort, speed, or verbosity edit. Oracle has
    /// no separate thinking-display axis, so callers must not pass one here.
    pub fn axis(self, axis: crate::host::SettingsAxis) -> crate::host::SettingsAxis {
        use crate::host::SettingsAxis::*;
        if self == Self::Main {
            return axis;
        }
        match axis {
            Model(v) => OracleModel(v),
            Thinking(v) => OracleThinking(v),
            Speed(v) => OracleSpeed(v),
            Verbosity(v) => OracleVerbosity(v),
            _ => unreachable!("unsupported Oracle axis"),
        }
    }
    fn notice(self, notice: String) -> String {
        match self {
            Self::Main => notice,
            Self::Oracle => format!("Oracle: {notice}"),
        }
    }
}

impl crate::host::SettingsAxis {
    pub fn model_target(&self) -> ModelTarget {
        use crate::host::SettingsAxis::*;
        match self {
            OracleModel(_) | OracleThinking(_) | OracleSpeed(_) | OracleVerbosity(_) => {
                ModelTarget::Oracle
            }
            _ => ModelTarget::Main,
        }
    }
}

/// Save an axis choice as a config default, refreshing `config` from the layers.
///
/// Does not change runtime settings or append to the session log, so a frontend
/// can save defaults while keeping a branch choice pending. Returns a save
/// failure notice, or `None` on success (including [`PersistAction::None`]).
pub fn persist_axis(
    layers: &Arc<Mutex<ConfigLayers>>,
    config: &Arc<Mutex<Config>>,
    persist: PersistAction,
    axis: &crate::host::SettingsAxis,
) -> Option<String> {
    use crate::host::SettingsAxis;

    let target = axis.model_target();
    match axis {
        SettingsAxis::Thinking(level) | SettingsAxis::OracleThinking(level) => persist_setting(
            layers,
            config,
            persist,
            target.key("thinking"),
            Some(thinking_level_name(level)),
            |c| {
                let value = Some(config_thinking_level(level.as_ref()));
                if target == ModelTarget::Main {
                    c.thinking = value;
                } else {
                    c.oracle_thinking = value;
                }
            },
        ),
        SettingsAxis::Model(info) | SettingsAxis::OracleModel(info) => {
            // `model_url` is a user-supplied endpoint override, not part of
            // the model choice. Saving the catalog URL would freeze out
            // updates to models.json.
            match persist {
                PersistAction::None => None,
                PersistAction::User => update_user(layers, config, |baseline| {
                    // Saving a model pins the whole pair, even when it equals
                    // the built-in selection. Other config keys remain untouched.
                    let mut selected = ConfigLayer::default();
                    selected
                        .set_str(target.key("model_api"), &info.provider)
                        .expect("model provider is a string");
                    selected
                        .set_str(target.key("model_name"), &info.id)
                        .expect("model name is a string");
                    selected.persist(&ConfigLayer::default(), &Config::config_file_path()?)?;
                    Ok(selected.overlay_onto(baseline))
                }),
                PersistAction::ProjectSet => persist_project(
                    layers,
                    config,
                    &[
                        (target.key("model_api"), Some(info.provider.as_str())),
                        (target.key("model_name"), Some(info.id.as_str())),
                    ],
                ),
                PersistAction::ProjectClear => persist_project(
                    layers,
                    config,
                    &[
                        (target.key("model_api"), None),
                        (target.key("model_name"), None),
                    ],
                ),
            }
        }
        SettingsAxis::Speed(speed) | SettingsAxis::OracleSpeed(speed) => {
            // Standard removes the user key but is an explicit project
            // override, so a project can override a user default of fast.
            persist_setting(
                layers,
                config,
                persist,
                target.key("speed"),
                Some(speed_name(*speed)),
                |c| {
                    let value = match speed {
                        None | Some(Speed::Standard) => None,
                        Some(Speed::Fast) => Some(ConfigSpeed::Fast),
                    };
                    if target == ModelTarget::Main {
                        c.speed = value;
                    } else {
                        c.oracle_speed = value;
                    }
                },
            )
        }
        SettingsAxis::Verbosity(verbosity) | SettingsAxis::OracleVerbosity(verbosity) => {
            // The default choice removes the key in either layer.
            let value = verbosity.map(|value| value.to_string());
            persist_setting(
                layers,
                config,
                persist,
                target.key("verbosity"),
                value.as_deref(),
                |c| {
                    if target == ModelTarget::Main {
                        c.verbosity = *verbosity;
                    } else {
                        c.oracle_verbosity = *verbosity;
                    }
                },
            )
        }
        SettingsAxis::ThinkingDisplay(display) => {
            let value = display.map(|value| value.to_string());
            persist_setting(
                layers,
                config,
                persist,
                "thinking_display",
                value.as_deref(),
                |c| c.thinking_display = *display,
            )
        }
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

/// Result of a main or Oracle thinking or model confirm.
///
/// `footer` is `Some` when the change applied and the frontend should
/// refresh the Main footer entry. It is `None` when the change did not
/// apply (a provider rebuild failure) and the footer is left as-is.
pub struct ModelConfirm {
    pub footer: Option<FooterUpdate>,
    /// The confirmation line on its own, which is also what the log
    /// entry's projection renders.
    pub notice: String,
    /// Problems that do not belong on the durable settings notice: a
    /// failed config write, a failed log record. A frontend joins them
    /// onto the confirmation with [`Confirmation::message`]; a host
    /// publishes them separately, because a backfill regenerates the
    /// projected notice and nothing else.
    pub notes: Vec<String>,
    /// The settings entry the change appended, absent when the append
    /// failed. A host tags the projected notice with it.
    pub entry: Option<EntryRef>,
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
        notes: Vec<String>,
        entry: Option<EntryRef>,
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
    pub notes: Vec<String>,
    /// The settings entry the change appended on the sub-agent's thread.
    pub entry: Option<EntryRef>,
    pub applied: bool,
}

/// Result of the main-agent verbosity confirm, which neither rebuilds the
/// provider bundle nor moves the footer.
pub struct VerbosityConfirm {
    pub notice: String,
    pub notes: Vec<String>,
    pub entry: Option<EntryRef>,
}

/// A settings confirm's user-facing text: the confirmation plus whatever
/// went wrong beside it.
pub trait Confirmation {
    fn notice(&self) -> &str;
    fn notes(&self) -> &[String];

    /// The confirmation with its notes appended, space-separated. What a
    /// frontend folding one line shows.
    fn message(&self) -> String {
        let mut out = self.notice().to_string();
        for note in self.notes() {
            out.push(' ');
            out.push_str(note);
        }
        out
    }
}

macro_rules! confirmation {
    ($ty:ty) => {
        impl Confirmation for $ty {
            fn notice(&self) -> &str {
                &self.notice
            }

            fn notes(&self) -> &[String] {
                &self.notes
            }
        }
    };
}

confirmation!(ModelConfirm);
confirmation!(SubConfirm);
confirmation!(VerbosityConfirm);

/// What a settings confirm amounts to for a caller that does not render a
/// footer: whether the change applied, the entry it appended, and the text
/// beside it.
///
/// One shape for every axis, because the axis-specific confirms each say
/// "this did not apply" differently (a `None` footer, an `applied` flag, a
/// `Failed` variant). A caller that destructured two of three fields would
/// silently treat a refused change as an accepted one, which is what this
/// exists to prevent.
pub struct ConfirmOutcome {
    /// False when nothing was staged: the target has no live handle, the
    /// provider bundle could not be rebuilt, validation rejected the
    /// value. `notice` says which.
    pub applied: bool,
    /// The confirmation line, which is also what the entry's projection
    /// renders, or the refusal when `applied` is false.
    pub notice: String,
    pub notes: Vec<String>,
    /// The settings entry the change appended, absent when the change
    /// applied but the append failed (a note carries the reason) and when
    /// it did not apply at all.
    pub entry: Option<EntryRef>,
}

impl From<ModelConfirm> for ConfirmOutcome {
    fn from(confirm: ModelConfirm) -> Self {
        Self {
            applied: confirm.footer.is_some(),
            notice: confirm.notice,
            notes: confirm.notes,
            entry: confirm.entry,
        }
    }
}

impl From<SubConfirm> for ConfirmOutcome {
    fn from(confirm: SubConfirm) -> Self {
        Self {
            applied: confirm.applied,
            notice: confirm.notice,
            notes: confirm.notes,
            entry: confirm.entry,
        }
    }
}

impl From<VerbosityConfirm> for ConfirmOutcome {
    fn from(confirm: VerbosityConfirm) -> Self {
        Self {
            // Verbosity stages a plain field: nothing to rebuild, nothing
            // to validate, so it cannot be refused.
            applied: true,
            notice: confirm.notice,
            notes: confirm.notes,
            entry: confirm.entry,
        }
    }
}

impl From<SpeedConfirm> for ConfirmOutcome {
    fn from(confirm: SpeedConfirm) -> Self {
        match confirm {
            SpeedConfirm::Applied {
                notice,
                notes,
                entry,
                ..
            } => Self {
                applied: true,
                notice,
                notes,
                entry,
            },
            SpeedConfirm::Failed { notice, .. } => Self {
                applied: false,
                notice,
                notes: Vec::new(),
                entry: None,
            },
        }
    }
}

/// Apply a confirmed thinking pick to the selected model: stage it into the
/// run config, record it on the session log's user thread, and persist
/// it per `persist`. Returns the new footer identity and the notice.
/// The frontend applies the border tint and footer note.
pub async fn confirm_thinking(
    target: ModelTarget,
    level: Option<ThinkingConfig>,
    persist: PersistAction,
    run_config: &Arc<Mutex<RunConfigSnapshot>>,
    config: &Arc<Mutex<Config>>,
    layers: &Arc<Mutex<ConfigLayers>>,
    core: &SessionCore,
) -> ModelConfirm {
    // Stage the new thinking effort into the loop-side snapshot; the
    // next turn applies it. Never locks the agent, so it's safe while
    // a turn is running (the in-flight turn keeps its effort; the
    // change takes effect next turn). Read the rest of the settings
    // identity back for the footer entry.
    let (settings, context_window) = {
        let mut run = run_config.lock().expect("run config mutex poisoned");
        let cfg = target.model_mut(&mut run);
        cfg.thinking = level.clone();
        (cfg.settings(), cfg.model_info.context_window)
    };
    let name = thinking_level_name(&level);
    // Record the change on the session log's user thread so a later
    // resume restores this level.
    let (entry, log_note) = {
        let mut log = core.log.lock().await;
        record(match target {
            ModelTarget::Main => log.append_thinking_change(ThreadFilter::USER, name),
            ModelTarget::Oracle => log.append_oracle_thinking_change(name),
        })
    };
    // Persist as the new default only when the change should outlive
    // this session (the settings windows). The `/thinking` overlay
    // command is session-scoped: it relies on the session-log record
    // above to survive a resume and leaves the default untouched.
    let save_note = persist_axis(
        layers,
        config,
        persist,
        &target.axis(crate::host::SettingsAxis::Thinking(level)),
    );
    ModelConfirm {
        footer: Some(FooterUpdate {
            settings,
            context_window,
        }),
        notice: target.notice(format!("Thinking effort set to {name}.")),
        notes: [save_note, log_note].into_iter().flatten().collect(),
        entry,
    }
}

/// Split an append result into the entry it produced and the note to show
/// when it failed. A failed record does not undo the change, which is live
/// for this session. A persistence I/O failure has also fused the log, and the
/// owning driver ends the session over that rather than over this note.
fn record(
    appended: Result<EntryRef, aj_session::ConversationError>,
) -> (Option<EntryRef>, Option<String>) {
    match appended {
        Ok(entry) => (Some(entry), None),
        Err(err) => (
            None,
            Some(format!("(couldn't record in session log: {err})")),
        ),
    }
}

/// Apply a confirmed thinking pick to sub-agent `n`: validate against
/// the target's model, stage into the sub-override map (applied at the
/// sub's next turn start), and record on the sub's log thread.
/// Deliberately does not touch `config.toml` or the run config. Those
/// record the session default, which is main's concern.
///
/// `tracked_model` is the target child's recorded model, resolved to a
/// catalog entry. It is the validation fallback
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
            notes: Vec::new(),
            entry: None,
            applied: false,
        };
    }
    let name = thinking_level_name(&level);
    // Validate the chosen level (including off) against the target's
    // model: the staged bundle override's info if present, else its recorded
    // model, else skip (no model in scope, e.g. scripted).
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
            notes: Vec::new(),
            entry: None,
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
    let (entry, log_note) = {
        let mut log = core.log.lock().await;
        record(log.append_thinking_change(ThreadFilter::subagent(n), name))
    };
    SubConfirm {
        notice: format!("Thinking effort set to {name} for agent {n}."),
        notes: log_note.into_iter().collect(),
        entry,
        applied: true,
    }
}

/// Apply a confirmed model pick to the selected model: rebuild the bundle,
/// stage it into the run config, record it on the session log's user
/// thread, and (per `persist`) write or clear the choice in a config
/// layer as the default for new sessions. Returns the new footer
/// identity (or `None` on a rebuild failure) and the notice.
pub async fn confirm_model(
    target: ModelTarget,
    info: ModelInfo,
    persist: PersistAction,
    auth: &AuthStorage,
    run_config: &Arc<Mutex<RunConfigSnapshot>>,
    config: &Arc<Mutex<Config>>,
    layers: &Arc<Mutex<ConfigLayers>>,
    core: &SessionCore,
) -> ModelConfirm {
    let previous = {
        let run = run_config.lock().expect("run config mutex poisoned");
        target.model(&run).clone()
    };
    let replacement = from_model_info(auth, info.clone(), previous.speed).map(|bundle| {
        let mut options = bundle.stream_options;
        apply_thinking_display(&mut options, previous.thinking_display);
        options.verbosity = previous.stream_options.verbosity;
        ModelConfig {
            provider: bundle.provider,
            model_info: bundle.model_info,
            stream_options: options,
            model_key: (info.provider.clone(), info.id.clone()),
            ..previous
        }
    });
    match replacement {
        Ok(mut model) => {
            // Never lock the agent here. A running turn retains its bundle,
            // while the next main turn takes these staged choices.
            let settings = {
                let mut run = run_config.lock().expect("run config mutex poisoned");
                run.accounts
                    .install(&mut model.stream_options, auth, &model.model_info.provider);
                let settings = model.settings();
                *target.model_mut(&mut run) = model;
                settings
            };
            // Record the new settings identity so the footer's model
            // line and context-window denominator reflect the swap
            // immediately rather than waiting for the next turn.
            let context_window = info.context_window;
            // Record the change on the session log's user thread so a
            // later resume restores this model.
            let (entry, log_note) = {
                let mut log = core.log.lock().await;
                record(match target {
                    ModelTarget::Main => log.append_model_change(
                        ThreadFilter::USER,
                        &info.provider,
                        &info.id,
                        info.context_window,
                    ),
                    ModelTarget::Oracle => log.append_oracle_model_change(&info.provider, &info.id),
                })
            };
            // Persist the model choice (provider + id) as the new
            // default only when the change should outlive this session
            // (the settings windows). The `/model` overlay command is
            // session-scoped: it relies on the session-log record above
            // to survive a resume and leaves the default untouched.
            let save_note = persist_axis(
                layers,
                config,
                persist,
                &target.axis(crate::host::SettingsAxis::Model(info.clone())),
            );
            ModelConfirm {
                footer: Some(FooterUpdate {
                    settings,
                    context_window,
                }),
                notice: target.notice(format!(
                    "Model set to {} ({}/{}).",
                    info.name, info.provider, info.id
                )),
                notes: [save_note, log_note].into_iter().flatten().collect(),
                entry,
            }
        }
        Err(err) => ModelConfirm {
            footer: None,
            notice: target.notice(format!("Failed to switch to {}: {err}", info.name)),
            notes: Vec::new(),
            entry: None,
        },
    }
}

/// Apply a confirmed model pick to sub-agent `n`: rebuild the bundle at
/// the child's speed and stage it into the sub-override map (applied at
/// the sub's next turn start), then record on the sub's log thread.
/// Deliberately does not touch `config.toml` or the run config.
///
/// `recorded_speed` is the target child's recorded baseline. A staged speed
/// or model bundle takes precedence, keeping request options in step with
/// the child's own identity rather than the main agent's settings.
pub async fn confirm_model_for_sub(
    info: &ModelInfo,
    n: usize,
    auth: &AuthStorage,
    recorded_speed: Option<Speed>,
    core: &SessionCore,
) -> SubConfirm {
    let target = AgentId::Sub(n);
    if core.resolve_agent(target).is_none() {
        return SubConfirm {
            notice: "This agent can't be prompted.".to_string(),
            notes: Vec::new(),
            entry: None,
            applied: false,
        };
    }
    let effective_speed = core
        .sub_overrides
        .lock()
        .expect("sub overrides mutex poisoned")
        .get(&n)
        .and_then(|overrides| {
            overrides.speed.or_else(|| {
                overrides
                    .bundle
                    .as_ref()
                    .map(|(_, _, options, _)| options.speed)
            })
        })
        .unwrap_or(recorded_speed);
    match from_model_info(auth, info.clone(), effective_speed) {
        Ok(ResolvedModel {
            provider,
            model_info,
            mut stream_options,
        }) => {
            core.run_config
                .lock()
                .expect("run config mutex poisoned")
                .accounts
                .install(&mut stream_options, auth, &info.provider);
            // Stage the standing bundle choice; the sub's next turn
            // applies it.
            //
            // NOTE(aljoscha): the rebuilt bundle's `stream_options`
            // come from `from_model_info` (defaults), so a sub's
            // `thinking_display` and `verbosity` revert to the server
            // default on a model swap. Unlike the main path
            // (`confirm_model`), we don't re-apply the config
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
            let (entry, log_note) = {
                let mut log = core.log.lock().await;
                record(log.append_model_change(
                    ThreadFilter::subagent(n),
                    &info.provider,
                    &info.id,
                    info.context_window,
                ))
            };
            SubConfirm {
                notice: format!(
                    "Model set to {} ({}/{}) for agent {n}.",
                    info.name, info.provider, info.id
                ),
                notes: log_note.into_iter().collect(),
                entry,
                applied: true,
            }
        }
        Err(err) => SubConfirm {
            notice: format!("Failed to switch to {}: {err}", info.name),
            notes: Vec::new(),
            entry: None,
            applied: false,
        },
    }
}

/// Apply a confirmed output-verbosity pick to the selected model: stage it
/// onto the run config's stream options, persist per `persist`, and
/// record it on the session log's user thread. Verbosity is a plain
/// stream-option field (no headers, no bundle rebuild), so unlike
/// [`confirm_speed`] this neither rebuilds the provider nor
/// touches the footer. Providers gate the field on per-model support,
/// so on a model that ignores verbosity this records the preference
/// without changing what's sent. Returns the user-facing notice.
pub async fn confirm_verbosity(
    target: ModelTarget,
    verbosity: Option<ConfigVerbosity>,
    persist: PersistAction,
    run_config: &Arc<Mutex<RunConfigSnapshot>>,
    config: &Arc<Mutex<Config>>,
    layers: &Arc<Mutex<ConfigLayers>>,
    core: &SessionCore,
) -> VerbosityConfirm {
    let unified = verbosity.map(config_verbosity_to_unified);
    let name = verbosity_name(unified);
    {
        let mut run = run_config.lock().expect("run config mutex poisoned");
        let cfg = target.model_mut(&mut run);
        cfg.stream_options.verbosity = unified;
    }
    // Record on the user thread so a later resume restores this value.
    let (entry, log_note) = {
        let mut log = core.log.lock().await;
        record(match target {
            ModelTarget::Main => log.append_verbosity_change(ThreadFilter::USER, name),
            ModelTarget::Oracle => log.append_oracle_verbosity_change(name),
        })
    };
    let save_note = persist_axis(
        layers,
        config,
        persist,
        &target.axis(crate::host::SettingsAxis::Verbosity(verbosity)),
    );
    VerbosityConfirm {
        notice: target.notice(format!(
            "Output verbosity set to {name}. Takes effect next turn."
        )),
        notes: [save_note, log_note].into_iter().flatten().collect(),
        entry,
    }
}

/// Applies the main session's live-only reasoning-display choice.
///
/// The choice updates provider stream options and may become a config default,
/// but it is never written to the session log. A resumed session therefore
/// starts from the creator's current config unless a client changes it again.
pub fn confirm_thinking_display_for_main(
    display: Option<ConfigThinkingDisplay>,
    persist: PersistAction,
    run_config: &Arc<Mutex<RunConfigSnapshot>>,
    config: &Arc<Mutex<Config>>,
    layers: &Arc<Mutex<ConfigLayers>>,
) -> ConfirmOutcome {
    {
        let mut cfg = run_config.lock().expect("run config mutex poisoned");
        cfg.main.thinking_display = display;
        apply_thinking_display(&mut cfg.main.stream_options, display);
    }
    let name = thinking_display_name(display);
    let save_note = persist_axis(
        layers,
        config,
        persist,
        &crate::host::SettingsAxis::ThinkingDisplay(display),
    );
    ConfirmOutcome {
        applied: true,
        notice: format!("Thinking display set to {name}. Takes effect next turn."),
        notes: save_note.into_iter().collect(),
        entry: None,
    }
}

/// Apply a speed change to the selected model: rebuild the provider bundle
/// at the current model so the speed-derived headers are re-stamped,
/// stage it into the run config, persist per `persist`, and record on
/// the session log's user thread. On a rebuild failure (e.g. scripted
/// mode, whose provider isn't in the registry) nothing is staged and
/// the caller reverts the settings row via [`SpeedConfirm::Failed`].
pub async fn confirm_speed(
    target: ModelTarget,
    speed: Option<Speed>,
    persist: PersistAction,
    auth: &AuthStorage,
    run_config: &Arc<Mutex<RunConfigSnapshot>>,
    config: &Arc<Mutex<Config>>,
    layers: &Arc<Mutex<ConfigLayers>>,
    core: &SessionCore,
) -> SpeedConfirm {
    let name = speed_name(speed);
    let (model_info, prev_speed, display, verbosity) = {
        let run = run_config.lock().expect("run config mutex poisoned");
        let cfg = target.model(&run);
        (
            (*cfg.model_info).clone(),
            cfg.speed,
            cfg.thinking_display,
            cfg.stream_options.verbosity,
        )
    };
    match from_model_info(auth, model_info, speed) {
        Ok(ResolvedModel {
            provider,
            model_info,
            mut stream_options,
        }) => {
            // The rebuilt baseline would otherwise drop this session's
            // creator-selected display and verbosity choices.
            apply_thinking_display(&mut stream_options, display);
            stream_options.verbosity = verbosity;
            // Stage into the loop-side snapshot; the next turn applies
            // it. Never locks the agent, so it's safe mid-turn.
            let (settings, context_window) = {
                let mut run = run_config.lock().expect("run config mutex poisoned");
                let accounts = run.accounts.clone();
                let cfg = target.model_mut(&mut run);
                cfg.provider = provider;
                cfg.model_info = model_info;
                cfg.stream_options = stream_options;
                cfg.speed = speed;
                accounts.install(&mut cfg.stream_options, auth, &cfg.model_info.provider);
                (cfg.settings(), cfg.model_info.context_window)
            };
            // Record the change on the session log's user thread so a
            // later resume restores this speed.
            let (entry, log_note) = {
                let mut log = core.log.lock().await;
                record(match target {
                    ModelTarget::Main => log.append_speed_change(ThreadFilter::USER, name),
                    ModelTarget::Oracle => log.append_oracle_speed_change(name),
                })
            };
            let save_note = persist_axis(
                layers,
                config,
                persist,
                &target.axis(crate::host::SettingsAxis::Speed(speed)),
            );
            SpeedConfirm::Applied {
                footer: FooterUpdate {
                    settings,
                    context_window,
                },
                notice: target.notice(format!("Speed set to {name}. Takes effect next turn.")),
                notes: [save_note, log_note].into_iter().flatten().collect(),
                entry,
            }
        }
        Err(err) => SpeedConfirm::Failed {
            previous: speed_name(prev_speed).to_string(),
            notice: target.notice(format!("Failed to set speed {name}: {err}")),
        },
    }
}

/// Fully composed settings-window description for `option`: the schema
/// one-liner plus the settings-window note the interactive windows show
/// below the highlighted row.
///
/// This is the single source of the description copy the frontend
/// renders. It is frontend-neutral: an addendum specific to the frontend
/// (e.g. a note that `show_frame_stats` only affects the frame-stats
/// overlay) is appended by that frontend, not here.
pub fn option_description(option: &aj_conf::ConfigOption) -> String {
    match option.name {
        // The model row folds `model_api` + `model_name`, so its text names
        // both keys rather than describing `model_api` alone.
        "oracle_model_api" => "Model Oracle uses, applied from the next turn. Persisted as oracle_model_api + oracle_model_name.".to_string(),
        "model_api" => "Model the main agent uses, applied from the next turn. \
             Persisted as model_api + model_name."
            .to_string(),
        "model_url" | "oracle_model_url" => describe(
            option,
            "Takes effect on restart. Submit an empty value to unset.",
        ),
        "thinking_display" => describe(
            option,
            "\"default\" keeps the provider's stock behavior. Takes effect next turn.",
        ),
        "speed" | "oracle_speed" | "oracle_thinking" => describe(option, "Takes effect next turn."),
        "verbosity" | "oracle_verbosity" => describe(
            option,
            "\"default\" leaves the server default. Takes effect next turn.",
        ),
        // The tool catalog is rebuilt at the start of every turn from the
        // effective config, so everything feeding it lands next turn.
        "disabled_tools" => describe(
            option,
            "Toggles apply when the picker closes. Takes effect next turn.",
        ),
        "image_auto_resize" | "bash_rtk" => describe(option, "Takes effect next turn."),
        "disabled_skills" => describe(
            option,
            "Toggles apply when the picker closes. Takes effect for new sessions.",
        ),
        "image_block" => describe(option, "Takes effect for new sessions."),
        "compact_threshold" => describe(option, "A fraction between 0.0 and 1.0."),
        "compact_keep_recent" => describe(option, "A positive number of tokens."),
        // Plain schema string: thinking, theme, show_thinking_block,
        // show_token_usage, show_image_in_terminal, compact_transcript,
        // auto_compact, syntax_highlighting, show_frame_stats, and
        // model_name (folded into the model row, never shown alone).
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
        for name in ["thinking", "syntax_highlighting", "compact_transcript"] {
            let opt = option(name);
            assert_eq!(option_description(opt), opt.description, "option {name}");
        }
    }

    #[test]
    fn show_frame_stats_is_frontend_neutral() {
        let opt = option("show_frame_stats");
        // No frontend addendum here: the shared description is exactly the
        // option's own, and the frontend appends any frontend-specific note.
        assert_eq!(option_description(opt), opt.description);
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
                "Toggles apply when the picker closes. Takes effect next turn.",
            ),
            (
                "disabled_skills",
                "Toggles apply when the picker closes. Takes effect for new sessions.",
            ),
            ("image_auto_resize", "Takes effect next turn."),
            ("bash_rtk", "Takes effect next turn."),
            ("image_block", "Takes effect for new sessions."),
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
