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
    ConversationLog, ConversationPersistence, EntryId, ThreadFilter, repair_interrupted_tool_uses,
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
    speed: Option<Speed>,
) -> RunConfigSnapshot {
    crate::model::apply_thinking_display(&mut stream_options, config.thinking_display);
    crate::model::apply_verbosity(&mut stream_options, config.verbosity);
    RunConfigSnapshot {
        provider,
        model_info,
        stream_options,
        thinking: crate::model::default_thinking_from_config(config.thinking),
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
    Create,
    Resume {
        session_id: String,
        /// Optional user-thread head override, applied after resume and
        /// before repair (see [`prepare_log`]). `None` keeps the log's
        /// default head (`latest_leaf`).
        head: Option<EntryId>,
    },
}

impl SessionSource {
    fn is_resume(&self) -> bool {
        matches!(self, SessionSource::Resume { .. })
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
        SessionSource::Create => ConversationLog::create(persistence)
            .context("failed to create a fresh conversation log")?,
        SessionSource::Resume { session_id, .. } => {
            ConversationLog::resume(persistence, session_id)
                .with_context(|| format!("failed to resume session {session_id}"))?
        }
    };

    // Install a requested head override before repair runs, so repair
    // anchors its synthesized tool results at the branch path's tip, not at
    // the abandoned tail's. The override is apply-or-fail: a stale or
    // invalid id (truncated file, hand-edited log) fails the whole build
    // rather than silently resuming the default head, so a successful
    // return guarantees the requested head is installed. The empty-log case
    // (head `None`) never carries an override. No context wrapper here:
    // `set_head`'s `InvalidHead` message names the offending entry and is
    // fit to surface to the user verbatim, a wrapper would bury it.
    if let SessionSource::Resume { head: Some(h), .. } = source {
        log.set_head(h.clone())?;
    }

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
    use clap::Parser;
    use tempfile::TempDir;

    use super::*;

    fn empty_auth(dir: &TempDir) -> AuthStorage {
        AuthStorage::new(dir.path().join("auth.json"))
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
            build_initial_run_config(&args, &Config::default(), &empty_auth(&dir), None)
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
            build_initial_run_config(&args, &Config::default(), &empty_auth(&dir), None)
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
            build_initial_run_config(&args, &config, &empty_auth(&dir), None)
                .expect("scripted run config");
        assert!(run_config.session_id.is_none());
        assert!(run_config.stream_options.session_id.is_none());

        let run_config = Arc::new(StdMutex::new(run_config));
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let prepared = prepare_log(
            &persistence,
            &SessionSource::Create,
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

    /// Build a two-user-message session on `persistence` and return its id
    /// plus the two message ids, in append order. The user thread is
    /// `system_prompt -> m1 -> m2`.
    fn two_message_session(
        persistence: &ConversationPersistence,
    ) -> (String, aj_session::EntryId, aj_session::EntryId) {
        use aj_agent::message::AgentMessage;
        use aj_models::types::{Message, UserMessage};
        use aj_session::{ConversationEntryKind, ConversationLog, ThreadKind};

        let mut log = ConversationLog::create(persistence).expect("create log");
        let sp = log
            .set_system_prompt("prompt".to_string())
            .expect("system prompt")
            .id;
        let user = |text: &str| ConversationEntryKind::Message {
            message: AgentMessage::wire(Message::User(UserMessage::text(text))),
        };
        let m1 = log
            .append(Some(sp), ThreadKind::User, None, user("one"))
            .expect("first user message")
            .id;
        let m2 = log
            .append(Some(m1.clone()), ThreadKind::User, None, user("two"))
            .expect("second user message")
            .id;
        (log.session_id().to_string(), m1, m2)
    }

    /// A valid head override rebuilds and repairs from the override point:
    /// resuming at the first message linearizes only that message. A
    /// successful return guarantees the override applied (a stale one errors,
    /// see below).
    #[test]
    fn prepare_log_applies_a_valid_head_override() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let (session_id, m1, _m2) = two_message_session(&persistence);

        let config = Config::default();
        let args = Args::parse_from(["aj", "--scripted", "streaming-text"]);
        let (run_config, _restore) =
            build_initial_run_config(&args, &config, &empty_auth(&dir), None).expect("run config");
        let run_config = Arc::new(StdMutex::new(run_config));

        let prepared = prepare_log(
            &persistence,
            &SessionSource::Resume {
                session_id,
                head: Some(m1),
            },
            &config,
            &run_config,
            None,
        )
        .expect("prepare log");

        assert_eq!(
            prepared.transcript.len(),
            1,
            "the override linearizes only the branch path (the first message)"
        );
    }

    /// A stale head override (an id not in the log) fails the build: the
    /// override is apply-or-fail, so the caller's fallback machinery (not a
    /// silent default-head resume) handles it. The error IS the log's
    /// `InvalidHead`, naming the requested head, so the caller's notice
    /// surfaces the reason directly.
    #[test]
    fn prepare_log_errors_on_a_stale_head_override() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let (session_id, _m1, _m2) = two_message_session(&persistence);

        let config = Config::default();
        let args = Args::parse_from(["aj", "--scripted", "streaming-text"]);
        let (run_config, _restore) =
            build_initial_run_config(&args, &config, &empty_auth(&dir), None).expect("run config");
        let run_config = Arc::new(StdMutex::new(run_config));

        // `expect_err` needs `Debug` on the Ok type, which `PreparedLog`
        // doesn't carry, so unpack manually.
        let Err(err) = prepare_log(
            &persistence,
            &SessionSource::Resume {
                session_id,
                head: Some("does-not-exist".to_string()),
            },
            &config,
            &run_config,
            None,
        ) else {
            panic!("a stale head override fails the build");
        };

        let chain = format!("{err:#}");
        assert!(
            chain.contains(
                "invalid conversation head: entry does-not-exist is not in this session's log"
            ),
            "the InvalidHead message names the requested head: {chain}"
        );
    }

    /// Repair runs after the head override is installed, so a branch whose tip
    /// ends in a dangling tool_call is healed on the OVERRIDE path, not on the
    /// abandoned tail. This pins the ordering in `prepare_log`: install the
    /// override, then linearize and repair from it.
    #[test]
    fn prepare_log_repairs_the_override_path_not_the_abandoned_tail() {
        use aj_agent::message::AgentMessage;
        use aj_models::types::{
            AssistantContent, AssistantMessage, Message, ToolCall, UserMessage,
        };
        use aj_session::{ConversationEntryKind, ConversationLog, ThreadKind};
        use serde_json::json;

        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));

        // Two sibling branches off the system prompt, each ending in its own
        // dangling tool_call: the branch we override to (tip `a_branch`) and
        // the abandoned tail (tip `a_tail`, appended last so it is the default
        // `latest_leaf` head).
        let (session_id, a_branch) = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            let sp = log
                .set_system_prompt("prompt".to_string())
                .expect("system prompt")
                .id;
            let user = |text: &str| ConversationEntryKind::Message {
                message: AgentMessage::wire(Message::User(UserMessage::text(text))),
            };
            let tool_call = |id: &str| ConversationEntryKind::Message {
                message: AgentMessage::wire(Message::Assistant(AssistantMessage {
                    content: vec![AssistantContent::ToolCall(ToolCall {
                        id: id.to_string(),
                        name: "ping".to_string(),
                        arguments: json!({}),
                    })],
                    ..AssistantMessage::empty()
                })),
            };
            let m_branch = log
                .append(Some(sp.clone()), ThreadKind::User, None, user("branch"))
                .expect("branch user message")
                .id;
            let a_branch = log
                .append(
                    Some(m_branch),
                    ThreadKind::User,
                    None,
                    tool_call("tu-branch"),
                )
                .expect("branch dangling tool_call")
                .id;
            let m_tail = log
                .append(Some(sp), ThreadKind::User, None, user("tail"))
                .expect("tail user message")
                .id;
            log.append(Some(m_tail), ThreadKind::User, None, tool_call("tu-tail"))
                .expect("tail dangling tool_call");
            (log.session_id().to_string(), a_branch)
        };

        let config = Config::default();
        let args = Args::parse_from(["aj", "--scripted", "streaming-text"]);
        let (run_config, _restore) =
            build_initial_run_config(&args, &config, &empty_auth(&dir), None).expect("run config");
        let run_config = Arc::new(StdMutex::new(run_config));

        let prepared = prepare_log(
            &persistence,
            &SessionSource::Resume {
                session_id,
                head: Some(a_branch.clone()),
            },
            &config,
            &run_config,
            None,
        )
        .expect("prepare log");

        // The seeded transcript is the branch path with the synthesized result
        // at its tip; it never touches the abandoned tail's dangling call.
        match prepared
            .transcript
            .last()
            .expect("a seeded message")
            .as_stored_wire()
        {
            Some(Message::ToolResult(tr)) => {
                assert_eq!(
                    tr.tool_call_id, "tu-branch",
                    "repaired the branch's dangling call"
                );
                assert!(tr.is_error, "the synthesized result is error-flagged");
            }
            other => panic!("expected a synthesized ToolResult at the branch tip, got {other:?}"),
        }
        assert!(
            !prepared.transcript.iter().any(|m| matches!(
                m.as_stored_wire(),
                Some(Message::ToolResult(tr)) if tr.tool_call_id == "tu-tail"
            )),
            "the abandoned tail's dangling call is not repaired onto the branch path"
        );

        // The synthesized result anchors at the branch tip, proving the
        // override was installed before repair ran (otherwise it would chain
        // off the abandoned tail's `a_tail`).
        let synthesized = prepared
            .log
            .entries_in_order()
            .into_iter()
            .find(|e| {
                matches!(
                    &e.entry,
                    ConversationEntryKind::Message { message }
                        if matches!(
                            message.as_stored_wire(),
                            Some(Message::ToolResult(tr)) if tr.tool_call_id == "tu-branch"
                        )
                )
            })
            .expect("the synthesized tool_result is in the log");
        assert_eq!(
            synthesized.parent_id.as_deref(),
            Some(a_branch.as_str()),
            "the synthesized result anchors at the branch tip, not the abandoned tail"
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
pub fn compose_host(
    args: &Args,
    layers: ConfigLayers,
    auth: &AuthStorage,
    persistence: &ConversationPersistence,
) -> Result<ComposedHost> {
    let config = layers.effective();
    let speed = resolve_speed(args, &config)?;
    let (run_config, restore) = build_initial_run_config(args, &config, auth, speed)?;
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
    })?;
    Ok(ComposedHost {
        host,
        config,
        layers,
        catalog,
    })
}
