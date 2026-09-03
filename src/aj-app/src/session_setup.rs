//! Mode-agnostic session setup shared by the print and interactive
//! frontends.
//!
//! Both modes assemble the same things at startup: the run-config
//! snapshot (provider/model/thinking/speed merged from CLI > env >
//! config), the conversation log (created or resumed, with interrupted
//! tool uses repaired), resume-time settings restoration, the agent
//! itself, and the system-prompt freeze + transcript seed. This module
//! owns those steps so the two frontends differ only in what they wrap
//! around them: the interactive `SessionWorld` adds the sub-agent
//! registry, bus subscriptions, and event pump. Print mode adds the
//! JSONL / persistence listeners and the one-shot turn.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use aj_agent::events::AgentSettings;
use aj_agent::message::AgentMessage;
use aj_agent::{Agent, AgentSeed};
use aj_conf::{AgentEnv, Config, ConfigSpeed, ConfigThinkingDisplay};
use aj_models::auth::AuthStorage;
use aj_models::provider::Provider;
use aj_models::registry::{ModelInfo, ModelRegistry};
use aj_models::types::{Speed, StreamOptions, ThinkingLevel, Verbosity};
use aj_models::{
    ThinkingConfig, speed_name, thinking_config_from_name, thinking_config_name,
    verbosity_from_name, verbosity_name,
};
use aj_session::{
    ConversationError, ConversationLog, ConversationPersistence, TailRepair, ThreadFilter,
    repair_interrupted_tool_uses,
};
use aj_tools::{BuiltinToolOptions, builtin_tools_for_model};
use anyhow::{Context, Result};

use crate::SYSTEM_PROMPT;
use crate::cli::args::Args;
use crate::host::{HostSetup, SessionHost};
use crate::model::{ModelSelection, ResolvedModel};
use crate::settings::ConfigLayers;

/// Loop-side snapshot of the agent's run configuration.
///
/// The interactive loop spawns each turn into a task that holds the
/// agent `TokioMutex` for the turn's entire duration, so the loop
/// itself must never `agent.lock().await`. That would suspend the
/// whole `select!` (including its Ctrl+C arm) until the turn ends.
///
/// This snapshot is therefore the loop-side source of truth for "what
/// the next turn runs against". The model and thinking selectors
/// mutate it without touching the agent. The footer renders the active
/// model and effort from it. The submit handler copies it into the
/// agent just before each turn starts (while holding the turn's own
/// lock, which is uncontended because no turn is in flight yet). A
/// model or thinking change made mid-turn is thus accepted (and shown
/// in the footer) immediately, but only takes effect on the *next*
/// turn. The in-flight turn keeps the config it captured when it
/// started.
///
/// Print mode has no loop, so it builds one of these, optionally
/// overwrites it with the resumed log's recorded settings, and reads
/// it once to build its agent.
///
/// What lives here is what the user stages for the *next* turn. Values
/// that are simply read from the effective config, like the tool
/// catalog's inputs, are deliberately not copied in. The interactive
/// turn reads those from the effective config itself, so a settings
/// change cannot leave a stale copy behind here.
///
/// One of these belongs to exactly one session. A process serving
/// several live sessions clones its process-wide default per session
/// (see [`SessionCore::build`]), because the fields below are mutated
/// per session: the log stamps `session_id` on it, and a resume
/// overwrites the whole model bundle from the log's record. A shared
/// snapshot would cross-contaminate prompt-cache keys and model
/// selection between sessions.
///
/// [`SessionCore::build`]: crate::session::SessionCore::build
#[derive(Clone)]
pub struct RunConfigSnapshot {
    /// Provider handle the next turn streams against.
    pub provider: Arc<dyn Provider>,
    /// Registry (or scripted) metadata for `provider`'s model.
    pub model_info: Arc<ModelInfo>,
    /// Per-call stream options (thinking-display mode, etc.).
    pub stream_options: StreamOptions,
    /// Default thinking effort for the next turn.
    pub thinking: Option<ThinkingConfig>,
    /// Canonical reasoning-display choice for the next turn.
    ///
    /// We track the config value rather than reconstructing it from
    /// provider-specific stream options because that mapping is lossy.
    pub thinking_display: Option<ConfigThinkingDisplay>,
    /// Inference speed mode baked into `stream_options`' headers.
    /// Tracked explicitly so bundle rebuilds (model swap, resume
    /// restore) preserve it and so it can be recorded in the session
    /// log. `None` means standard.
    pub speed: Option<Speed>,
    /// `(provider_id, model_id)` the model selector pre-selects.
    /// Tracked explicitly rather than read off `model_info` because
    /// the scripted path's provider id (from `--model-api`) differs
    /// from `model_info.provider`, which is always `"scripted"`.
    pub model_key: (String, String),
    /// Stable per-session prompt-cache key (the conversation/session
    /// id). Providers that key prompt caching on it (OpenAI Responses,
    /// Codex) reuse the cached prefix across this session's turns when
    /// it's set. Stamped onto `stream_options.session_id` before each
    /// turn, but held here separately because a model swap rebuilds
    /// `stream_options` from registry defaults, which would otherwise
    /// drop it. `None` until the log is opened in [`prepare_log`].
    pub session_id: Option<String>,
}

impl RunConfigSnapshot {
    /// The settings identity the next turn runs against.
    ///
    /// The run config is what a turn is stamped from, so this is the
    /// authority for "the active model", not the agent, whose own copy lags
    /// by a turn.
    pub fn settings(&self) -> AgentSettings {
        AgentSettings {
            provider: self.model_key.0.clone(),
            model_id: self.model_key.1.clone(),
            thinking: thinking_config_name(self.thinking.as_ref()).to_string(),
            thinking_display: thinking_display_name(self.thinking_display).to_string(),
            speed: speed_name(self.speed).to_string(),
            verbosity: verbosity_name(self.stream_options.verbosity).to_string(),
        }
    }
}

/// Returns the canonical name used in state-frame settings.
pub fn thinking_display_name(display: Option<ConfigThinkingDisplay>) -> &'static str {
    match display {
        None => "default",
        Some(ConfigThinkingDisplay::Summarized) => "summarized",
        Some(ConfigThinkingDisplay::Detailed) => "detailed",
        Some(ConfigThinkingDisplay::Omitted) => "omitted",
    }
}

/// Parses the canonical state-frame thinking-display vocabulary.
pub fn thinking_display_from_name(name: &str) -> Option<Option<ConfigThinkingDisplay>> {
    match name {
        "default" => Some(None),
        "summarized" => Some(Some(ConfigThinkingDisplay::Summarized)),
        "detailed" => Some(Some(ConfigThinkingDisplay::Detailed)),
        "omitted" => Some(Some(ConfigThinkingDisplay::Omitted)),
        _ => None,
    }
}

/// Dependencies for resume-time settings restoration: the model
/// catalog to resolve recorded `(provider, model_id)` pairs against,
/// and the credential store backing the rebuilt bundle's lazy API-key
/// resolver. `None` on the scripted path (and in tests) disables
/// restoration.
pub struct RestoreContext {
    pub registry: Arc<ModelRegistry>,
    pub auth: AuthStorage,
}

/// Construct the loop-side run-config snapshot from a resolved
/// provider bundle, fanning the configured thinking-display onto the
/// stream options.
fn build_run_config(
    config: &Config,
    provider: Arc<dyn Provider>,
    model_info: Arc<ModelInfo>,
    mut stream_options: StreamOptions,
    model_key: (String, String),
    thinking: Option<ThinkingConfig>,
    speed: Option<Speed>,
) -> RunConfigSnapshot {
    crate::model::apply_thinking_display(&mut stream_options, config.thinking_display);
    crate::model::apply_verbosity(&mut stream_options, config.verbosity);
    RunConfigSnapshot {
        provider,
        model_info,
        stream_options,
        thinking,
        thinking_display: config.thinking_display,
        speed,
        model_key,
        // Filled in by `prepare_log` once the log (and thus the session
        // id) exists; the initial resolve runs before then.
        session_id: None,
    }
}

/// Resolve the process's initial run config from CLI args, config, and
/// the credential store, plus the resume-time [`RestoreContext`] the
/// registry path needs (`None` on the scripted path).
///
/// The scripted path keeps the `--scripted` fake provider and never
/// restores session settings. The registry path goes through the model
/// registry so the binary owns provider dispatch, API-key resolution,
/// and speed-driven headers. Both apply the CLI > env > config
/// model-selection precedence through [`ModelSelection`].
pub fn build_initial_run_config(
    args: &Args,
    config: &Config,
    auth: &AuthStorage,
    thinking: Option<ThinkingConfig>,
    speed: Option<Speed>,
) -> Result<(RunConfigSnapshot, Option<RestoreContext>)> {
    let selection = ModelSelection::merge(args, config);
    if let Some(name) = &args.scripted {
        let crate::scripted::ResolvedScriptedModel {
            provider,
            model_info,
        } = crate::scripted::resolve_or_explain(name)?;
        let model_key = (selection.provider_id().to_string(), model_info.id.clone());
        let run_config = build_run_config(
            config,
            provider,
            model_info,
            StreamOptions::default(),
            model_key,
            thinking,
            speed,
        );
        Ok((run_config, None))
    } else {
        let registry = ModelRegistry::load();
        let ResolvedModel {
            provider,
            model_info,
            stream_options,
        } = crate::model::resolve(&registry, auth, &selection, speed)
            .context("failed to resolve model from registry")?;
        let model_key = (model_info.provider.clone(), model_info.id.clone());
        let run_config = build_run_config(
            config,
            provider,
            model_info,
            stream_options,
            model_key,
            thinking,
            speed,
        );
        let restore = RestoreContext {
            registry: Arc::new(registry),
            auth: auth.clone(),
        };
        Ok((run_config, Some(restore)))
    }
}

/// Project a [`ThinkingConfig`] onto the wire-level [`ThinkingLevel`]
/// for validation against a model's effort vocabulary. One-to-one,
/// mirroring the projection the agent applies before each inference.
pub fn thinking_level_for(level: &ThinkingConfig) -> ThinkingLevel {
    match level {
        ThinkingConfig::Minimal => ThinkingLevel::Minimal,
        ThinkingConfig::Low => ThinkingLevel::Low,
        ThinkingConfig::Medium => ThinkingLevel::Medium,
        ThinkingConfig::High => ThinkingLevel::High,
        ThinkingConfig::XHigh => ThinkingLevel::XHigh,
        ThinkingConfig::Max => ThinkingLevel::Max,
    }
}

/// Write a resumed session's recorded settings back into the shared
/// run config, per the resume precedence: the log's record wins over
/// the current defaults. An axis the log doesn't record keeps the
/// current value. Returns the user-facing notices describing what was
/// restored or why a recorded value was kept out.
///
/// Speed is restored first because the model bundle rebuilds below
/// stamp speed-derived headers. Auth is deliberately not checked: key
/// resolution is lazy (see `crate::model`), so an uncredentialed
/// restored provider surfaces at the next turn, where the user can
/// `/login`.
pub(crate) fn restore_session_settings(
    config: &Config,
    run_config: &Arc<StdMutex<RunConfigSnapshot>>,
    settings: &aj_session::SessionSettings,
    restore: &RestoreContext,
) -> Vec<String> {
    let mut notices = Vec::new();
    let mut cfg = run_config.lock().expect("run config mutex poisoned");

    // Speed. `None` and `Some(Standard)` are equivalent on the wire,
    // so changes are tracked by canonical name.
    let prior_speed_name = speed_name(cfg.speed);
    if let Some(s) = settings.speed.as_deref() {
        match s.parse::<ConfigSpeed>() {
            Ok(ConfigSpeed::Standard) => cfg.speed = Some(Speed::Standard),
            Ok(ConfigSpeed::Fast) => cfg.speed = Some(Speed::Fast),
            Err(_) => notices.push(format!(
                "Session recorded unknown speed {s:?}; keeping {prior_speed_name}."
            )),
        }
    }

    // Model. Skipped when the record matches the active bundle, except
    // that a restored speed still needs the bundle's headers rebuilt to
    // match.
    let model_changed = settings.model.as_ref().is_some_and(|(prov, id)| {
        (prov.as_str(), id.as_str()) != (&*cfg.model_key.0, &*cfg.model_key.1)
    });
    if model_changed {
        let (prov, id) = settings.model.as_ref().expect("checked above");
        let resolved = restore
            .registry
            .get(prov, id)
            .cloned()
            .context("not in the model catalog")
            .and_then(|info| crate::model::from_model_info(&restore.auth, info, cfg.speed));
        match resolved {
            Ok(resolved) => {
                let name = resolved.model_info.name.clone();
                cfg.provider = resolved.provider;
                cfg.model_info = resolved.model_info;
                cfg.stream_options = resolved.stream_options;
                let display = cfg.thinking_display;
                crate::model::apply_thinking_display(&mut cfg.stream_options, display);
                crate::model::apply_verbosity(&mut cfg.stream_options, config.verbosity);
                cfg.model_key = (prov.clone(), id.clone());
                notices.push(format!("Restored model {name} ({prov}/{id}) from session."));
            }
            Err(err) => {
                tracing::warn!("could not restore session model {prov}/{id}: {err:#}");
                notices.push(format!(
                    "Session used {prov}/{id}, which is not available; continuing with {}/{}.",
                    cfg.model_key.0, cfg.model_key.1
                ));
            }
        }
    } else if speed_name(cfg.speed) != prior_speed_name {
        // Same model, different speed: rebuild the bundle so the
        // stream options carry the restored speed's headers.
        match crate::model::from_model_info(&restore.auth, (*cfg.model_info).clone(), cfg.speed) {
            Ok(resolved) => {
                cfg.provider = resolved.provider;
                cfg.model_info = resolved.model_info;
                cfg.stream_options = resolved.stream_options;
                let display = cfg.thinking_display;
                crate::model::apply_thinking_display(&mut cfg.stream_options, display);
                crate::model::apply_verbosity(&mut cfg.stream_options, config.verbosity);
            }
            Err(err) => {
                tracing::warn!("could not rebuild bundle for restored speed: {err:#}");
                notices.push(format!(
                    "Couldn't apply the session's recorded speed: {err:#}"
                ));
            }
        }
    }

    // Verbosity, re-applied onto the (possibly just-rebuilt) stream
    // options. A recorded value wins over the config default; an
    // unknown string keeps the current value rather than guessing.
    if let Some(verbosity_str) = settings.verbosity.as_deref() {
        let current = verbosity_name(cfg.stream_options.verbosity);
        match verbosity_from_name(verbosity_str) {
            Some(v) => cfg.stream_options.verbosity = v,
            None => notices.push(format!(
                "Session recorded unknown verbosity {verbosity_str:?}; keeping {current}."
            )),
        }
    }

    // Thinking: apply the recorded level verbatim. It was recorded
    // alongside its model, so it is normally valid for the restored
    // model. A mismatch (e.g. the recorded model is gone and a different
    // one was picked) surfaces at the next turn's validation rather than
    // being silently substituted here.
    if let Some(level_str) = settings.thinking.as_deref() {
        let current = thinking_config_name(cfg.thinking.as_ref());
        match thinking_config_from_name(level_str) {
            None => notices.push(format!(
                "Session recorded unknown thinking level {level_str:?}; keeping {current}."
            )),
            Some(level) => cfg.thinking = level,
        }
    }

    notices
}

/// The builtin-tool construction options `config` selects.
pub(crate) fn builtin_tool_options(config: &Config) -> BuiltinToolOptions {
    BuiltinToolOptions {
        image_auto_resize: config.image_auto_resize,
        bash_rtk: config.bash_rtk,
        spill_dir: config.spill_dir.as_ref().map(PathBuf::from),
    }
}

/// An agent plus the host context the caller needs after construction:
/// the [`AgentEnv`] it was built against (for a startup context
/// notice, the footer, and editor autocomplete) and whether the active
/// tool set gates in the skills listing.
pub struct BuiltAgent {
    pub agent: Agent,
    pub env: AgentEnv,
    /// Whether the active tools include `read_file`. Skills are
    /// progressive disclosure reachable only with that tool, so this
    /// gates the skills listing in the assembled system prompt.
    pub include_skills: bool,
}

/// Construct a fresh, not-yet-shared [`Agent`] from the persisted
/// config and a resolved provider bundle.
///
/// `thinking`/`speed` come from the caller's run-config snapshot
/// rather than from `config`, so a runtime `/thinking` change carries
/// into agents built for later sessions. The [`AgentEnv`] is read
/// fresh, so a new session picks up edits to AGENTS.md files, a system
/// prompt override, and the current date. Skill-discovery diagnostics
/// ride on the returned `env`. The caller decides how to surface them.
pub fn build_agent(
    config: &Config,
    provider: Arc<dyn Provider>,
    model_info: Arc<ModelInfo>,
    stream_options: StreamOptions,
    thinking: Option<ThinkingConfig>,
    speed: Option<Speed>,
) -> BuiltAgent {
    let tools = builtin_tools_for_model(
        &builtin_tool_options(config),
        &config.disabled_tools,
        model_info.family.as_deref(),
    );
    let include_skills = tools.iter().any(|tool| tool.name == "read_file");
    let env = AgentEnv::new(SYSTEM_PROMPT, &config.disabled_skills);
    let mut agent = Agent::with_provider(
        env.working_directory.clone(),
        tools,
        config.disabled_tools.clone(),
        provider,
        model_info,
        stream_options,
        // Set below from the resolved snapshot value rather than
        // passed through here, so it stays in lockstep with `speed`.
        None,
    );
    agent.set_block_images(config.image_block);
    agent.set_default_thinking(thinking);
    agent.set_speed(speed);
    BuiltAgent {
        agent,
        env,
        include_skills,
    }
}

/// Whether a session is freshly created or resumed from disk. The
/// mode-agnostic counterpart to the interactive `SessionSpec`, which
/// additionally carries the header-notice wording.
pub enum SessionSource {
    Create {
        session_env: Option<BTreeMap<String, String>>,
    },
    Resume {
        session_id: String,
    },
}

impl SessionSource {
    fn is_resume(&self) -> bool {
        matches!(self, SessionSource::Resume { .. })
    }

    pub fn creation_env(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            SessionSource::Create { session_env } => session_env.as_ref(),
            SessionSource::Resume { .. } => None,
        }
    }
}

/// A resolved conversation log plus the agent seed material derived
/// from it.
pub struct PreparedLog {
    /// The opened log, not yet shared behind an `Arc<Mutex<_>>`. The
    /// caller still mutates it (system-prompt freeze via
    /// [`freeze_and_seed`]) before installing the persistence
    /// listener.
    pub log: ConversationLog,
    /// The linearized user thread captured after repair, ready to seed
    /// the agent. Empty for a fresh log.
    pub transcript: Vec<AgentMessage>,
    /// Notices from resume-time settings restoration (what was
    /// restored, or why a recorded value was kept out). Empty unless
    /// resuming with a [`RestoreContext`].
    pub restore_notices: Vec<String>,
    /// The one ephemeral recovery notice, present exactly when this open's
    /// resume repaired the log's interrupted final write. It belongs to this
    /// open: a later open finds clean bytes and produces none.
    pub recovery_notice: Option<String>,
    /// Immutable session environment from the explicit create or resumed log.
    /// `None` differs from a recorded empty map.
    pub session_env: Option<BTreeMap<String, String>>,
}

/// The user-facing text for one performed tail repair, as the failed-write
/// recovery design words it for the reopen flow.
fn recovery_notice_text(repair: &TailRepair) -> String {
    match repair {
        TailRepair::RemovedIncompleteRecord { .. } => "Recovered this session after an \
             interrupted write. AJ removed an incomplete final record. Check the last visible \
             action and retry it if needed."
            .to_string(),
        TailRepair::CompletedFraming => "Recovered this session after an interrupted write. AJ \
             completed the final record's framing. Check the last visible action and retry it \
             if needed."
            .to_string(),
    }
}

/// Resolve the log for `source`, repair any interrupted tool uses, and
/// (on a resume with a [`RestoreContext`]) restore the recorded
/// settings into `run_config`.
///
/// Repair runs before the transcript is captured: we re-linearize from
/// the post-repair head so the seed sees any synthesized `tool_result`
/// the repair walk just wrote. On error nothing is shared or
/// installed.
pub fn prepare_log(
    persistence: &ConversationPersistence,
    source: &SessionSource,
    config: &Config,
    run_config: &Arc<StdMutex<RunConfigSnapshot>>,
    restore: Option<&RestoreContext>,
) -> Result<PreparedLog> {
    let mut log = match source {
        SessionSource::Create { .. } => ConversationLog::create(persistence)
            .context("failed to create a fresh conversation log")?,
        SessionSource::Resume { session_id, .. } => {
            ConversationLog::resume(persistence, session_id).map_err(|err| match &err {
                // The refusal copy is the user-facing report the recovery
                // design fixes; a generic resume context on top would bury
                // it behind surfaces that show only an error's top line.
                ConversationError::UnsafeTail { .. } => anyhow::Error::new(err),
                _ => anyhow::Error::new(err)
                    .context(format!("failed to resume session {session_id}")),
            })?
        }
    };
    let recovery_notice = log.take_tail_repair().as_ref().map(recovery_notice_text);
    let session_env = match source {
        SessionSource::Create { session_env } => session_env.clone(),
        SessionSource::Resume { .. } => log.session_env().cloned(),
    };

    let mut restore_notices = Vec::new();
    let transcript = if let Some(head) = log.head().cloned() {
        let conversation = log.linearize(&head, ThreadFilter::USER);
        repair_interrupted_tool_uses(&mut log, &conversation)?;
        let head = log
            .head()
            .cloned()
            .expect("post-repair head exists when pre-repair head did");
        let conversation = log.linearize(&head, ThreadFilter::USER);
        if source.is_resume()
            && let Some(restore) = restore
        {
            restore_notices =
                restore_session_settings(config, run_config, &conversation.settings(), restore);
        }
        conversation.agent_messages()
    } else {
        Vec::new()
    };

    // Stamp the session's stable prompt-cache key onto the run config
    // now that the log exists. We do it after the restore block above:
    // a resume rebuilds `stream_options` from registry defaults (which
    // carry no cache key), so stamping earlier would be undone here.
    // `session_id` is the durable source the per-turn apply re-stamps
    // from after a mid-session model swap; the direct `stream_options`
    // stamp covers print mode and the initial agent build, which read
    // these options without going through the per-turn apply.
    {
        let mut cfg = run_config.lock().expect("run config mutex poisoned");
        let session_id = log.session_id().to_string();
        cfg.stream_options.session_id = Some(session_id.clone());
        cfg.session_id = Some(session_id);
    }

    Ok(PreparedLog {
        log,
        transcript,
        restore_notices,
        recovery_notice,
        session_env,
    })
}

/// Resolve the session's system prompt and seed the agent.
///
/// A brand-new log gets the freshly-assembled prompt frozen as its
/// root entry, followed by the initial settings record so a later
/// resume can restore the model/thinking/speed even if the defaults
/// change in between. A resume reuses the persisted prompt verbatim
/// (cache-warm) and leaves the log untouched. Either way the agent is
/// seeded with the transcript, the prompt, and the sub-agent counter
/// floor (so freshly minted sub-agent ids don't collide with subtrees
/// already on disk).
pub fn freeze_and_seed(
    log: &mut ConversationLog,
    agent: &mut Agent,
    transcript: Vec<AgentMessage>,
    env: &AgentEnv,
    include_skills: bool,
    creation_env: Option<&BTreeMap<String, String>>,
    model_key: &(String, String),
    thinking: Option<&ThinkingConfig>,
    speed: Option<Speed>,
    verbosity: Option<Verbosity>,
) -> Result<()> {
    let system_prompt = if let Some(persisted) = log.system_prompt() {
        persisted.to_string()
    } else {
        let assembled = crate::system_prompt::assemble_system_prompt(env, include_skills);
        if log.is_empty() {
            log.set_system_prompt(assembled.clone())?;
            if let Some(session_env) = creation_env {
                log.append_env_change(session_env.clone())?;
            }
            log.append_model_change(ThreadFilter::USER, &model_key.0, &model_key.1)?;
            log.append_thinking_change(ThreadFilter::USER, thinking_config_name(thinking))?;
            log.append_speed_change(ThreadFilter::USER, speed_name(speed))?;
            log.append_verbosity_change(ThreadFilter::USER, verbosity_name(verbosity))?;
        }
        assembled
    };

    agent.seed_session(AgentSeed {
        transcript,
        assembled_system_prompt: Some(system_prompt),
        sub_agent_counter: log.max_agent_id().unwrap_or(0),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use aj_conf::ConfigLayer;
    use tempfile::TempDir;

    use super::*;

    fn empty_auth(dir: &TempDir) -> AuthStorage {
        AuthStorage::new(dir.path().join("auth.json"))
    }

    /// The stock configuration: nothing stated anywhere, so a composed host
    /// takes its defaults and the test measures only what it sets.
    fn layers() -> ConfigLayers {
        ConfigLayers {
            user: Config::default(),
            project: ConfigLayer::default(),
            project_path: None,
        }
    }

    /// The spill directory reaches tool construction as a path, and stays
    /// unset when the config leaves it unset so bash falls back to the ambient
    /// temp directory.
    #[test]
    fn the_spill_directory_reaches_tool_construction() {
        assert_eq!(
            builtin_tool_options(&Config::default()).spill_dir,
            None,
            "unset means the ambient temp directory, not a path of our choosing",
        );

        let config = Config {
            spill_dir: Some("/var/tmp/aj-spill".to_string()),
            ..Config::default()
        };
        assert_eq!(
            builtin_tool_options(&config).spill_dir,
            Some(PathBuf::from("/var/tmp/aj-spill")),
        );
    }

    /// `--name` reaches the host a run composes, which is the only path it
    /// travels: nothing else in a run carries it, and a host that never got it
    /// answers with a derived name that reads plausibly enough to hide the
    /// break until someone reads the sidebar.
    ///
    /// A name the wire would not carry stops the composition rather than
    /// being dropped on the way, so the operator hears about it instead of
    /// wondering why their host is labelled by its directory.
    #[tokio::test]
    async fn the_name_flag_reaches_the_composed_host() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let compose =
            |args: &Args| compose_host(args, layers(), &empty_auth(&dir), &persistence, None);

        let illegal =
            Args::parse_from(["aj", "--scripted", "streaming-text", "--name", "two\nlines"]);
        let err = compose(&illegal).err().expect("an illegal name is refused");
        assert!(err.to_string().contains("--name"), "got {err}");

        let named = Args::parse_from([
            "aj",
            "--scripted",
            "streaming-text",
            "--name",
            "the-fleet-host",
        ]);
        let composed = compose(&named).expect("compose a host");
        assert_eq!(
            composed.host.hello().name.as_deref(),
            Some("the-fleet-host"),
        );
        composed.host.shutdown().await;
    }

    /// The scripted path applies the CLI > config provider-id
    /// precedence to `model_key.0` even though it does no registry
    /// lookup, and never produces a `RestoreContext`.
    #[test]
    fn scripted_run_config_uses_merged_provider_id() {
        let dir = TempDir::new().expect("tempdir");
        let args = Args::parse_from([
            "aj",
            "--scripted",
            "streaming-text",
            "--model-api",
            "openai",
        ]);
        let (run_config, restore) =
            build_initial_run_config(&args, &Config::default(), &empty_auth(&dir), None, None)
                .expect("scripted run config");
        assert!(restore.is_none(), "scripted path never restores settings");
        assert_eq!(run_config.model_key.0, "openai");
        assert_eq!(run_config.model_key.1, "scripted/streaming-text");
    }

    #[test]
    fn scripted_run_config_defaults_provider_id_when_unset() {
        let dir = TempDir::new().expect("tempdir");
        let args = Args::parse_from(["aj", "--scripted", "streaming-text"]);
        let (run_config, _restore) =
            build_initial_run_config(&args, &Config::default(), &empty_auth(&dir), None, None)
                .expect("scripted run config");
        assert_eq!(run_config.model_key.0, crate::model::DEFAULT_PROVIDER_ID);
    }

    /// `prepare_log` stamps the opened log's id onto the run config as
    /// the session's prompt-cache key. The initial resolve runs before
    /// a log exists, so the field starts empty and is filled here; the
    /// stamp lands on both the durable `session_id` and the
    /// `stream_options` the agent build reads directly.
    #[test]
    fn prepare_log_stamps_session_id_onto_run_config() {
        let dir = TempDir::new().expect("tempdir");
        let args = Args::parse_from(["aj", "--scripted", "streaming-text"]);
        let config = Config::default();
        let (run_config, _restore) =
            build_initial_run_config(&args, &config, &empty_auth(&dir), None, None)
                .expect("scripted run config");
        assert!(run_config.session_id.is_none());
        assert!(run_config.stream_options.session_id.is_none());

        let run_config = Arc::new(StdMutex::new(run_config));
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let prepared = prepare_log(
            &persistence,
            &SessionSource::Create { session_env: None },
            &config,
            &run_config,
            None,
        )
        .expect("prepare log");

        let session_id = prepared.log.session_id().to_string();
        let cfg = run_config.lock().expect("run config mutex poisoned");
        assert_eq!(cfg.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            cfg.stream_options.session_id.as_deref(),
            Some(session_id.as_str())
        );
    }

    /// The CLI value outranks config and `off` means no reasoning request,
    /// rather than a synthetic thinking configuration.
    #[test]
    fn thinking_resolution_prefers_the_cli_and_collapses_off_to_none() {
        let config = Config {
            thinking: Some(aj_conf::ConfigThinkingLevel::High),
            ..Config::default()
        };
        let overridden = Args::parse_from(["aj", "--thinking", "off"]);
        assert_eq!(
            resolve_thinking(&overridden, &config).expect("resolve thinking"),
            None,
        );

        let inherited = Args::parse_from(["aj"]);
        assert!(
            inherited.thinking.is_none(),
            "the config-fallback fixture inherited AJ_THINKING and measures nothing",
        );
        assert_eq!(
            resolve_thinking(&inherited, &config).expect("resolve thinking"),
            Some(ThinkingConfig::High),
        );
    }
}

/// A composed session host plus the shared handles a frontend's settings
/// surfaces mutate alongside it.
///
/// One composition path serves every mode: the interactive shell, the
/// server it embeds for `--listen`, and headless `aj serve`. They differ in
/// what they wrap around the host, never in how the host is built, which is
/// what keeps a local shell and a remote client peers over one host rather
/// than two differently-configured ones.
pub struct ComposedHost {
    pub host: SessionHost,
    /// The effective config every session reads, shared so a settings
    /// change is visible to the whole process.
    pub config: Arc<StdMutex<Config>>,
    /// The user and project layers behind `config`, which the settings
    /// windows edit and persist.
    pub layers: Arc<StdMutex<ConfigLayers>>,
    pub catalog: Arc<Vec<ModelInfo>>,
}

/// Resolve the thinking effort this run starts at: the CLI flag (including its
/// environment backing), else the effective config. `off` becomes `None`.
pub fn resolve_thinking(args: &Args, config: &Config) -> Result<Option<ThinkingConfig>> {
    Ok(crate::model::default_thinking_from_config(
        args.thinking.or(config.thinking),
    ))
}

/// Resolve the inference speed this run starts at: the CLI flag, else the
/// effective config.
pub fn resolve_speed(args: &Args, config: &Config) -> Result<Option<Speed>> {
    let configured = match args.speed.as_deref() {
        Some(name) => Some(name.parse::<ConfigSpeed>().map_err(anyhow::Error::msg)?),
        None => config.speed,
    };
    Ok(configured.map(|speed| match speed {
        ConfigSpeed::Standard => Speed::Standard,
        ConfigSpeed::Fast => Speed::Fast,
    }))
}

/// Compose the session host for `layers`' working directory.
///
/// `idle_grace` is how long the host holds an idle, unattached session before
/// releasing it. `None` takes the host's own default, which is what a real run
/// wants, and a test that cannot wait one out passes its own.
pub fn compose_host(
    args: &Args,
    layers: ConfigLayers,
    auth: &AuthStorage,
    persistence: &ConversationPersistence,
    idle_grace: Option<std::time::Duration>,
) -> Result<ComposedHost> {
    let config = layers.effective();
    let thinking = resolve_thinking(args, &config)?;
    let speed = resolve_speed(args, &config)?;
    let name = args
        .host_name()
        .map_err(|err| anyhow::anyhow!("--name: {err}"))?;
    let (run_config, restore) = build_initial_run_config(args, &config, auth, thinking, speed)?;
    let catalog = crate::commands::load_model_catalog();
    let config = Arc::new(StdMutex::new(config));
    let layers = Arc::new(StdMutex::new(layers));
    let host = SessionHost::new(HostSetup {
        config: Arc::clone(&config),
        layers: Arc::clone(&layers),
        catalog: Arc::clone(&catalog),
        run_config,
        restore,
        persistence: persistence.clone(),
        auth: auth.clone(),
        working_directory: std::env::current_dir().unwrap_or_default(),
        name,
        idle_grace,
        live_capacity: None,
    })?;
    Ok(ComposedHost {
        host,
        config,
        layers,
        catalog,
    })
}
