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

use aj_agent::message::AgentMessage;
use aj_agent::{Agent, AgentSeed};
use aj_conf::{AgentEnv, Config, ConfigSpeed};
use aj_models::auth::AuthStorage;
use aj_models::provider::Provider;
use aj_models::registry::{ModelInfo, ModelRegistry, validate_thinking_level};
use aj_models::types::{Speed, StreamOptions, ThinkingLevel};
use aj_models::{ThinkingConfig, speed_name, thinking_config_from_name, thinking_config_name};
use aj_session::{
    ConversationLog, ConversationPersistence, ThreadFilter, repair_interrupted_tool_uses,
};
use aj_tools::{BuiltinToolOptions, builtin_tools};
use anyhow::{Context, Result};

use crate::SYSTEM_PROMPT;
use crate::cli::args::Args;
use crate::model::{ModelSelection, ResolvedModel};

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
pub(crate) struct RunConfigSnapshot {
    /// Provider handle the next turn streams against.
    pub(crate) provider: Arc<dyn Provider>,
    /// Registry (or scripted) metadata for `provider`'s model.
    pub(crate) model_info: Arc<ModelInfo>,
    /// Per-call stream options (thinking-display mode, etc.).
    pub(crate) stream_options: StreamOptions,
    /// Default thinking effort for the next turn.
    pub(crate) thinking: Option<ThinkingConfig>,
    /// Inference speed mode baked into `stream_options`' headers.
    /// Tracked explicitly so bundle rebuilds (model swap, resume
    /// restore) preserve it and so it can be recorded in the session
    /// log. `None` means standard.
    pub(crate) speed: Option<Speed>,
    /// `(provider_id, model_id)` the model selector pre-selects.
    /// Tracked explicitly rather than read off `model_info` because
    /// the scripted path's provider id (from `--model-api`) differs
    /// from `model_info.provider`, which is always `"scripted"`.
    pub(crate) model_key: (String, String),
}

/// Dependencies for resume-time settings restoration: the model
/// catalog to resolve recorded `(provider, model_id)` pairs against,
/// and the credential store backing the rebuilt bundle's lazy API-key
/// resolver. `None` on the scripted path (and in tests) disables
/// restoration.
pub(crate) struct RestoreContext {
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
    RunConfigSnapshot {
        provider,
        model_info,
        stream_options,
        thinking: crate::model::default_thinking_from_config(config.thinking),
        speed,
        model_key,
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
pub(crate) fn build_initial_run_config(
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
pub(crate) fn thinking_level_for(level: &ThinkingConfig) -> ThinkingLevel {
    match level {
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
                crate::model::apply_thinking_display(
                    &mut cfg.stream_options,
                    config.thinking_display,
                );
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
                crate::model::apply_thinking_display(
                    &mut cfg.stream_options,
                    config.thinking_display,
                );
            }
            Err(err) => {
                tracing::warn!("could not rebuild bundle for restored speed: {err:#}");
                notices.push(format!(
                    "Couldn't apply the session's recorded speed: {err:#}"
                ));
            }
        }
    }

    // Thinking, validated against the (possibly just-restored) model.
    // Clamping rules are provider-specific, so a rejected level keeps
    // the current one rather than guessing a substitute.
    if let Some(level_str) = settings.thinking.as_deref() {
        let current = thinking_config_name(cfg.thinking.as_ref());
        match thinking_config_from_name(level_str) {
            None => notices.push(format!(
                "Session recorded unknown thinking level {level_str:?}; keeping {current}."
            )),
            Some(level) => {
                let validation = match &level {
                    None => Ok(()),
                    Some(tc) => validate_thinking_level(&cfg.model_info, &thinking_level_for(tc)),
                };
                match validation {
                    Ok(()) => cfg.thinking = level,
                    Err(msg) => notices.push(format!(
                        "Can't restore thinking level {level_str:?} ({msg}); keeping {current}."
                    )),
                }
            }
        }
    }

    notices
}

/// An agent plus the host context the caller needs after construction:
/// the [`AgentEnv`] it was built against (for a startup context
/// notice, the footer, and editor autocomplete) and whether the active
/// tool set gates in the skills listing.
pub(crate) struct BuiltAgent {
    pub(crate) agent: Agent,
    pub(crate) env: AgentEnv,
    /// Whether the active tools include `read_file`. Skills are
    /// progressive disclosure reachable only with that tool, so this
    /// gates the skills listing in the assembled system prompt.
    pub(crate) include_skills: bool,
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
pub(crate) fn build_agent(
    config: &Config,
    provider: Arc<dyn Provider>,
    model_info: Arc<ModelInfo>,
    stream_options: StreamOptions,
    thinking: Option<ThinkingConfig>,
    speed: Option<Speed>,
) -> BuiltAgent {
    let tools = builtin_tools(
        &BuiltinToolOptions {
            image_auto_resize: config.image_auto_resize,
        },
        &config.disabled_tools,
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
pub(crate) enum SessionSource {
    Create,
    Resume { session_id: String },
}

impl SessionSource {
    fn is_resume(&self) -> bool {
        matches!(self, SessionSource::Resume { .. })
    }
}

/// A resolved conversation log plus the agent seed material derived
/// from it.
pub(crate) struct PreparedLog {
    /// The opened log, not yet shared behind an `Arc<Mutex<_>>`. The
    /// caller still mutates it (system-prompt freeze via
    /// [`freeze_and_seed`]) before installing the persistence
    /// listener.
    pub(crate) log: ConversationLog,
    /// The linearized user thread captured after repair, ready to seed
    /// the agent. Empty for a fresh log.
    pub(crate) transcript: Vec<AgentMessage>,
    /// Notices from resume-time settings restoration (what was
    /// restored, or why a recorded value was kept out). Empty unless
    /// resuming with a [`RestoreContext`].
    pub(crate) restore_notices: Vec<String>,
}

/// Resolve the log for `source`, repair any interrupted tool uses, and
/// (on a resume with a [`RestoreContext`]) restore the recorded
/// settings into `run_config`.
///
/// Repair runs before the transcript is captured: we re-linearize from
/// the post-repair head so the seed sees any synthesized `tool_result`
/// the repair walk just wrote. On error nothing is shared or
/// installed.
pub(crate) fn prepare_log(
    persistence: &ConversationPersistence,
    source: &SessionSource,
    config: &Config,
    run_config: &Arc<StdMutex<RunConfigSnapshot>>,
    restore: Option<&RestoreContext>,
) -> Result<PreparedLog> {
    let mut log = match source {
        SessionSource::Create => ConversationLog::create(persistence)
            .context("failed to create a fresh conversation log")?,
        SessionSource::Resume { session_id } => ConversationLog::resume(persistence, session_id)
            .with_context(|| format!("failed to resume session {session_id}"))?,
    };

    let mut restore_notices = Vec::new();
    let transcript = if let Some(head) = log.latest_leaf(ThreadFilter::USER) {
        let conversation = log.linearize(&head, ThreadFilter::USER);
        repair_interrupted_tool_uses(&mut log, &conversation)?;
        let head = log
            .latest_leaf(ThreadFilter::USER)
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
pub(crate) fn freeze_and_seed(
    log: &mut ConversationLog,
    agent: &mut Agent,
    transcript: Vec<AgentMessage>,
    env: &AgentEnv,
    include_skills: bool,
    model_key: &(String, String),
    thinking: Option<&ThinkingConfig>,
    speed: Option<Speed>,
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
}
