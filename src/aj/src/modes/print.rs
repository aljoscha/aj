//! Non-interactive print mode.
//!
//! Per `docs/aj-next-plan.md` §4.2 the same `aj` binary can run
//! without a TUI: it subscribes to the agent's event bus and writes
//! plain text (or JSONL with `--format json`) to stdout, exiting when
//! `Agent::prompt` returns. Same code path lets callers script the
//! agent or embed it in a parent process.
//!
//! Two output formats:
//!
//! - **Text** ([`PrintFormat::Text`]). Best-effort human-readable
//!   output. Once the prompt completes, the last assistant message's
//!   visible text content is printed to stdout. Streaming partials
//!   are intentionally suppressed — a caller piping `aj --print`
//!   into another process wants a clean final answer, not interleaved
//!   thinking/streaming chatter. Aborted/error stop reasons surface
//!   via the agent's [`crate::TurnError`] return path and exit
//!   non-zero.
//! - **JSON** ([`PrintFormat::Json`]). One [`AgentEvent`] per JSONL
//!   line, in the order they fire on the bus. Same shape the locked
//!   `agent_event_serializes_with_internally_tagged_snake_case_shape`
//!   test in `aj-agent::events` pins for the protocol, so consumers
//!   can rely on stable discriminator keys (`"type"`, `"kind"`) and
//!   `snake_case` variant names. Persistence runs alongside the JSONL
//!   writer; both observe the same event sequence.
//!
//! Print mode opens (or for `continue`, resumes) a [`ConversationLog`]
//! the same way interactive mode does, so a `aj --print "do X"`
//! invocation leaves a resumable session on disk.
//!
//! With `aj continue --print "Q"` (optionally specifying a
//! session id), the resume flow does the same disk handshake as the
//! interactive resume: open the session, reuse the persisted system
//! prompt, repair any interrupted tool calls, then seed the agent's
//! in-memory transcript from the linearized user thread. In JSON
//! output mode the persisted history is streamed through the JSONL
//! sink in [`aj_session::replay`] order *before* the new prompt
//! runs, so consumers see the full event trace (historical and
//! live) in emit order; in text mode the historical events are
//! suppressed and only the new assistant turn's visible text is
//! printed, matching the one-shot text contract.
//!
//! Print mode always requires a positional `prompt` argument — it's
//! fundamentally one-shot and there's no readline to fall back on.
//! In particular, `aj continue --print` *without* a prompt is
//! an error: callers who just want to recover an interrupted tool
//! batch should resume interactively (`aj continue`) and let
//! the readline loop drive the recovery turn.

use std::io::{self, Write};
use std::sync::Arc;

use aj_agent::bus::{Listener, listener_from_sync};
use aj_agent::events::AgentEvent;
use aj_agent::{Agent, AgentSeed, TaskRegistry, TurnError};
use aj_conf::{AgentEnv, Config, ConfigSpeed, Severity};
use aj_models::registry::ModelRegistry;
use aj_models::types::Speed;
use aj_session::{
    ConversationLog, ConversationPersistence, ThreadFilter, persistence_listener,
    repair_interrupted_tool_uses, replay,
};
use aj_tools::{BuiltinToolOptions, get_builtin_tools};
use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

use crate::SYSTEM_PROMPT;
use crate::cli::args::{Args, Command, PrintFormat};
use crate::cli::file_args;
use crate::model::ResolvedModel;

/// Drive a single print-mode run from `args`.
///
/// The flow mirrors the interactive mode's session setup (load
/// config, resolve model args with CLI > env > config precedence,
/// build the agent + tool list, open the [`ConversationLog`]) but
/// skips the readline loop: a single [`Agent::prompt`] runs to
/// completion, then the function either prints the final assistant
/// text (text mode) or relies on the JSONL listener to have
/// streamed every live event already (JSON mode).
///
/// When invoked under the `continue` subcommand, the run resumes
/// the requested session (or "latest for this project") rather than
/// creating a fresh log. On resume the persisted system prompt is
/// reused, any dangling tool_use ids are repaired via
/// [`repair_interrupted_tool_uses`], the agent's transcript is
/// seeded from the linearized user thread, and in JSON mode the
/// historical events from [`replay`] are drained through the JSON
/// sink before the new turn begins so the consumer sees the full
/// event trace in emit order.
pub async fn run(args: Args) -> Result<()> {
    // Validate dispatch shape early so the user sees a clear error
    // instead of a confusing failure later. `Continue` resolves to
    // either a specific session id or "latest for this project";
    // `None` (the default) means "create a fresh session".
    //
    // `list-sessions` and `update-models` are dispatched in `main.rs`
    // before any session setup; reaching them here would mean
    // the dispatcher routed incorrectly.
    let resume_request: Option<Option<String>> = match &args.command {
        None => None,
        Some(Command::Continue { session_id, .. }) => Some(session_id.clone()),
        Some(Command::ListSessions) | Some(Command::UpdateModels) => {
            bail!("aj --print does not accept this subcommand");
        }
    };

    let prompt_text = collect_prompt_text(&args)?;

    // Load config.toml first (lowest priority). Missing or invalid
    // config falls back to defaults so a one-shot `aj --print`
    // works in a freshly-cloned checkout without any setup; any
    // diagnostics (parse errors, unknown keys) are surfaced to
    // stderr so the user knows their file wasn't applied as-is.
    let (config, config_diagnostics) = Config::load();
    for d in &config_diagnostics {
        let label = match d.severity() {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        eprintln!("aj: {label}: {d}");
    }

    // Resolve model args with the same precedence the legacy binary
    // uses: CLI flags > env vars > config.toml > defaults. The CLI
    // struct already pulled env vars via clap's `env = ...` attr, so
    // by the time we get here `args.model_*` is the post-env value.
    let speed = match args.speed.as_deref() {
        Some(s) => Some(s.parse::<ConfigSpeed>().map_err(anyhow::Error::msg)?),
        None => config.speed,
    }
    .map(|s| match s {
        ConfigSpeed::Standard => Speed::Standard,
        ConfigSpeed::Fast => Speed::Fast,
    });

    // Build the tool list. Disabled tools are filtered up-front so
    // the agent never advertises them to the model; this matches the
    // legacy binary's behaviour and keeps the print/interactive
    // surfaces uniform.
    let mut tools = get_builtin_tools(&BuiltinToolOptions {
        image_auto_resize: config.image_auto_resize,
    });
    if !config.disabled_tools.is_empty() {
        tools.retain(|tool| !config.disabled_tools.contains(&tool.name));
        tracing::info!(disabled = ?config.disabled_tools, "filtered disabled tools");
    }

    let env = AgentEnv::new(SYSTEM_PROMPT, &config.disabled_skills);
    for d in &env.skill_diagnostics {
        eprintln!("aj: warning: {d}");
    }

    // Resolve the [`ConversationLog`] for this run before the model:
    // a resumed session's recorded settings take precedence over the
    // CLI/config model selection, so the log must be read first. The
    // log stays unwrapped until after we mutate it (system-prompt
    // freeze, repair walk); it moves behind an `Arc<TokioMutex<_>>`
    // once the persistence listener takes a stake in it.
    let sessions_dir = Config::get_sessions_dir_path()?;
    let conversation_persistence = ConversationPersistence::new(sessions_dir);
    let mut log = match &resume_request {
        Some(Some(id)) => ConversationLog::resume(&conversation_persistence, id)
            .with_context(|| format!("failed to resume session {id}"))?,
        Some(None) => match conversation_persistence.get_latest_session_id()? {
            Some(latest) => ConversationLog::resume(&conversation_persistence, &latest)
                .with_context(|| format!("failed to resume latest session {latest}"))?,
            None => bail!(
                "no conversation sessions to resume; invoke `aj --print \"...\"` \
                 without `continue` to start a fresh session"
            ),
        },
        None => ConversationLog::create(&conversation_persistence)?,
    };

    // Resume-time history replay & repair:
    //
    // - Walk the user thread, synthesize tool_results for any
    //   dangling `tool_use` ids the previous run left behind, and
    //   re-linearize so the seed sees the post-repair view.
    // - Capture the linearized user thread as the agent's seed
    //   transcript so the next `prompt(...)` call sees the same
    //   transcript the model saw on the previous run, and its
    //   recorded settings for the restore step below.
    // - In JSON mode, replay the same disk events through the JSON
    //   sink **before** subscribing any listeners to the bus, so
    //   the consumer sees the full historical trace in emit order
    //   without double-firing the persistence listener (events
    //   that are already on disk would otherwise be re-written).
    //   Text mode skips the historical events: callers piping the
    //   binary's stdout into another process want a clean final
    //   answer, not the prior conversation re-stamped.
    let is_resuming = resume_request.is_some();
    let mut transcript = Vec::new();
    let mut session_settings: Option<aj_session::SessionSettings> = None;
    if is_resuming && let Some(head) = log.latest_leaf(ThreadFilter::USER) {
        let conversation = log.linearize(&head, ThreadFilter::USER);
        repair_interrupted_tool_uses(&mut log, &conversation)?;

        // Re-linearize after repair to capture any synthesized
        // tool_result message the walker just wrote.
        let head = log
            .latest_leaf(ThreadFilter::USER)
            .expect("post-repair head exists when pre-repair head did");
        let conversation = log.linearize(&head, ThreadFilter::USER);
        session_settings = Some(conversation.settings());
        transcript = conversation.agent_messages();

        if matches!(args.format, PrintFormat::Json) {
            let json = json_event_listener();
            for event in replay(&log) {
                json(&event)
                    .await
                    .context("failed to write replayed event to stdout during print-mode resume")?;
            }
        }
    }

    // Build the agent in one of two ways depending on the
    // `--scripted` flag. The scripted path keeps the legacy
    // `Arc<dyn Model>` surface (step 6.8 of
    // `docs/aj-next-progress.md` will port it onto
    // `ScriptedProvider`) and never restores session settings; the
    // real-model path goes through the registry so the binary owns
    // provider dispatch, API key resolution, and speed-driven beta
    // headers, and on a resume applies the session's recorded
    // settings with precedence over the CLI/config selection.
    let mut agent = if let Some(name) = &args.scripted {
        let crate::scripted::ResolvedScriptedModel {
            provider,
            model_info,
        } = crate::scripted::resolve_or_explain(name)?;
        let mut stream_options = aj_models::types::StreamOptions::default();
        crate::model::apply_thinking_display(&mut stream_options, config.thinking_display);
        let mut agent = Agent::with_provider(
            env,
            tools,
            config.disabled_tools.clone(),
            provider,
            model_info,
            stream_options,
            config.thinking,
        );
        agent.set_speed(speed);
        agent
    } else {
        let registry = ModelRegistry::load();
        // Credential store backing the lazy API-key resolver. Print
        // mode has no interactive `/login`, but it still benefits from
        // the full resolution chain (runtime override → env → stored
        // key → stored OAuth) so a credential obtained via the
        // interactive TUI works here too.
        let auth = aj_models::auth::AuthStorage::at_default_path()
            .context("failed to open ~/.aj/auth.json")?;
        let provider_id = args
            .model_api
            .as_deref()
            .or(config.model_api.as_deref())
            .unwrap_or(crate::model::DEFAULT_PROVIDER_ID)
            .to_string();
        if let Some(key) = args.api_key.clone() {
            auth.set_runtime_api_key(&provider_id, key).await;
        }

        // Session-recorded speed wins over the CLI/config value;
        // unknown recorded strings keep the current value with a
        // stderr notice, like the config diagnostics above.
        let mut speed = speed;
        if let Some(s) = session_settings
            .as_ref()
            .and_then(|settings| settings.speed.as_deref())
        {
            match s.parse::<ConfigSpeed>() {
                Ok(ConfigSpeed::Standard) => speed = Some(Speed::Standard),
                Ok(ConfigSpeed::Fast) => speed = Some(Speed::Fast),
                Err(_) => eprintln!("aj: session recorded unknown speed {s:?}; ignoring"),
            }
        }

        // Session-recorded model wins over the CLI/config selection;
        // a catalog miss (or provider-dispatch failure) falls back
        // to the CLI/config resolution with a stderr notice.
        let recorded_model = session_settings
            .as_ref()
            .and_then(|settings| settings.model.clone());
        let restored = recorded_model.as_ref().and_then(|(prov, id)| {
            match registry
                .get(prov, id)
                .cloned()
                .context("not in the model catalog")
                .and_then(|info| crate::model::from_model_info(&auth, info, speed))
            {
                Ok(resolved) => Some(resolved),
                Err(err) => {
                    eprintln!(
                        "aj: session used {prov}/{id}, which is not available \
                         ({err:#}); falling back to the configured model"
                    );
                    None
                }
            }
        });
        let ResolvedModel {
            provider,
            model_info,
            stream_options,
        } = match restored {
            Some(resolved) => resolved,
            None => crate::model::resolve(
                &registry,
                &auth,
                args.model_api.as_deref().or(config.model_api.as_deref()),
                args.model_name.as_deref().or(config.model_name.as_deref()),
                args.model_url.as_deref().or(config.model_url.as_deref()),
                speed,
            )
            .context("failed to resolve model from registry")?,
        };
        let mut stream_options = stream_options;
        crate::model::apply_thinking_display(&mut stream_options, config.thinking_display);
        let mut agent = Agent::with_provider(
            env,
            tools,
            config.disabled_tools.clone(),
            provider,
            model_info,
            stream_options,
            config.thinking,
        );
        agent.set_speed(speed);

        // Session-recorded thinking level wins over `config.thinking`
        // when it parses and the (possibly restored) model accepts
        // it; otherwise keep the config default with a stderr notice.
        if let Some(level_str) = session_settings
            .as_ref()
            .and_then(|settings| settings.thinking.as_deref())
        {
            match aj_models::thinking_config_from_name(level_str) {
                None => {
                    eprintln!("aj: session recorded unknown thinking level {level_str:?}; ignoring")
                }
                Some(level) => {
                    let validation = match &level {
                        None => Ok(()),
                        Some(tc) => aj_models::registry::validate_thinking_level(
                            &agent.model_info(),
                            &thinking_level_for(tc),
                        ),
                    };
                    match validation {
                        Ok(()) => agent.set_default_thinking(level),
                        Err(msg) => eprintln!(
                            "aj: can't restore session thinking level {level_str:?}: {msg}"
                        ),
                    }
                }
            }
        }
        agent
    };
    agent.set_block_images(config.image_block);

    // Inject a task registry so background tasks started during the
    // run can be killed at exit instead of orphaned. Print mode has
    // no wake-trigger loop: a notice queued after the final turn is
    // never drained, and whatever still runs when the prompt returns
    // is killed below — the bash tool description tells the model to
    // wait with a blocking `task_output` before finishing here.
    let task_registry = TaskRegistry::default();
    agent.set_task_registry(task_registry.clone());

    // Resolve the system prompt: reuse a persisted one on resume
    // (cache-warm — the model has the same bytes from the previous
    // run), or assemble fresh from the env and freeze it as the
    // log's root entry on a brand-new session, followed by the
    // initial settings record so a later resume restores them.
    // Mirrors the interactive path exactly so a session looks
    // identical on disk whether it was bootstrapped through
    // `--print` or the TUI.
    let system_prompt = if let Some(persisted) = log.system_prompt() {
        persisted.to_string()
    } else {
        let assembled = agent.assemble_system_prompt();
        if log.is_empty() {
            log.set_system_prompt(assembled.clone())?;
            let model_info = agent.model_info();
            log.append_model_change(ThreadFilter::USER, &model_info.provider, &model_info.id)?;
            log.append_thinking_change(
                ThreadFilter::USER,
                aj_models::thinking_config_name(agent.default_thinking().as_ref()),
            )?;
            log.append_speed_change(ThreadFilter::USER, aj_models::speed_name(speed))?;
        }
        assembled
    };

    // One-shot session seed: the resumed transcript (empty on a
    // fresh log), the frozen system prompt, and the sub-agent
    // counter floor so freshly-minted ids in this run don't collide
    // with sub-agent subtrees already persisted in the log (a fresh
    // log has no subtrees and seeds the counter's initial 0).
    agent.seed_session(AgentSeed {
        transcript,
        assembled_system_prompt: Some(system_prompt),
        sub_agent_counter: log.max_agent_id().unwrap_or(0),
    });

    let log = Arc::new(TokioMutex::new(log));

    // Register the JSONL listener BEFORE the persistence listener so
    // that when persistence errors out (which the listener surfaces
    // as a fatal `Err`), the user has already seen every event up
    // to (but not including) the failure on stdout. Persistence
    // errors get printed by our outer error handler.
    //
    // On a resume the replayed historical events were already
    // drained through the JSONL sink above (and never reached the
    // bus), so the listener here only ever observes live events
    // produced by this run's `prompt(...)` call.
    //
    // For text mode we still register a listener — but it only
    // forwards a synchronous beat per event so the bus is not idle
    // (debug ergonomics; otherwise `cargo run -p aj -- --print
    // ...` would look frozen between events). The actual rendering
    // happens after `prompt` returns when we walk
    // `agent.messages()`. The listener is therefore essentially a
    // no-op in text mode but keeps the structure symmetrical.
    let _stream_handle = match args.format {
        PrintFormat::Json => Some(agent.subscribe(json_event_listener())),
        PrintFormat::Text => None,
    };

    let _persistence_handle = agent.subscribe(persistence_listener(Arc::clone(&log)));

    // Drive a single prompt and observe the result. Errors fall into
    // three buckets:
    //
    // - `Recoverable`: model errored mid-turn or returned a
    //   user-facing failure. The transcript and disk state remain
    //   internally consistent (the agent already synthesized any
    //   compensating tool_result entries before returning), but the
    //   run produced no useful output for the caller. Surface the
    //   error to stderr and exit non-zero.
    // - `Aborted`: the user (or a parent process) sent SIGINT and we
    //   tripped the agent's cancel token. Same outward behaviour as
    //   `Recoverable` — internally-consistent state, non-zero exit.
    // - `Fatal`: a listener errored or the disk write failed. Same
    //   outward behaviour but with a fatal-flavoured error context
    //   so callers can tell them apart in scripts.
    //
    // We honour SIGINT via [`tokio::signal::ctrl_c`] so a Ctrl+C at
    // the shell aborts the in-flight turn instead of killing the
    // process. The handler fires once: a second SIGINT exits the
    // process via tokio's default signal behaviour (we don't re-arm
    // the handler), which gives the user an "abort harder" escape
    // if the first cancel didn't unstick whatever was running.
    let turn_cancel = CancellationToken::new();
    let cancel_for_signal = turn_cancel.clone();
    let ctrl_c_handler = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel_for_signal.cancel();
    });
    let prompt_result = agent.prompt(prompt_text, turn_cancel).await;
    // Stop listening for SIGINT before we return so a stray Ctrl+C
    // during shutdown doesn't trigger a phantom cancel.
    ctrl_c_handler.abort();

    // Kill the background-task tree and reap the process groups
    // before observing the prompt result, so the early error returns
    // below can't orphan tasks.
    crate::modes::shutdown_background_tasks(&task_registry).await;

    match prompt_result {
        Ok(()) => {}
        Err(TurnError::Aborted) => {
            return Err(anyhow!("agent run cancelled (sigint)"));
        }
        Err(TurnError::Recoverable(err)) => {
            return Err(err.context("agent run failed (recoverable)"));
        }
        Err(TurnError::Fatal(err)) => {
            return Err(err.context("agent run failed (fatal)"));
        }
    }

    // Text mode: print the final assistant message's visible text.
    // JSON mode already streamed every event; nothing else to do.
    if matches!(args.format, PrintFormat::Text) {
        print_final_assistant_text(&agent)?;
    }

    // Make sure stdout is flushed before exit so callers piping into
    // another process don't lose buffered bytes.
    let _ = io::stdout().flush();
    Ok(())
}

/// Project a [`aj_models::ThinkingConfig`] onto the wire-level
/// [`aj_models::types::ThinkingLevel`] for validation against a
/// model's effort vocabulary. One-to-one, mirroring the projection
/// the agent applies before each inference.
fn thinking_level_for(level: &aj_models::ThinkingConfig) -> aj_models::types::ThinkingLevel {
    use aj_models::ThinkingConfig;
    use aj_models::types::ThinkingLevel;
    match level {
        ThinkingConfig::Low => ThinkingLevel::Low,
        ThinkingConfig::Medium => ThinkingLevel::Medium,
        ThinkingConfig::High => ThinkingLevel::High,
        ThinkingConfig::XHigh => ThinkingLevel::XHigh,
        ThinkingConfig::Max => ThinkingLevel::Max,
    }
}

/// Collect the free-form prompt arguments into a single string, then
/// run them through `@file` expansion (today a passthrough — see
/// [`crate::cli::file_args::expand`]).
///
/// Prompt text can come from two places depending on the dispatch
/// shape: the top-level positional `args.prompt` (for the
/// no-subcommand path: `aj --print "hello"`), or the
/// `Continue.prompt` positional that lives after the session id
/// (for the resume path: `aj --print continue ID "hello"`).
/// Clap's greedy positional consumption keeps these disjoint —
/// once the parser sees the `continue` subcommand it routes
/// further positionals into `Continue`, so at most one of the
/// two slots is ever populated for a single invocation. We pick
/// whichever is non-empty and join with spaces; both empty is
/// an error.
///
/// Print mode is fundamentally one-shot — there's no readline to
/// fall back on — so a missing prompt is a hard error rather than
/// a quiet no-op.
fn collect_prompt_text(args: &Args) -> Result<String> {
    let prompt_parts: &[String] = match &args.command {
        Some(Command::Continue { prompt, .. }) if !prompt.is_empty() => prompt,
        _ => &args.prompt,
    };
    if prompt_parts.is_empty() {
        bail!("aj --print requires a prompt argument");
    }
    let joined = prompt_parts.join(" ");
    file_args::expand(joined).context("failed to expand @file references in prompt")
}

/// Build a [`Listener`] that writes each event as one JSONL line on
/// stdout. The listener is synchronous (`listener_from_sync`); the
/// bus awaits it inline so events appear in stdout in the same order
/// the agent emits them.
///
/// A serialization or write failure prints a one-line warning to
/// stderr and skips the offending event. We deliberately do **not**
/// fail the run on a stdout write error: today's deferred
/// `MessageStart`/`MessageUpdate`/`MessageEnd` variants are
/// `#[serde(skip)]` (see `aj-agent/src/events.rs`), so a future event
/// emit those would otherwise abort the whole prompt with a confusing
/// "variant cannot be serialized" message. Logging and continuing
/// keeps the run alive while making the gap visible.
fn json_event_listener() -> Listener {
    listener_from_sync(move |event: &AgentEvent| {
        match serde_json::to_string(event) {
            Ok(line) => {
                if let Err(e) = writeln!(io::stdout(), "{line}") {
                    eprintln!("aj: failed to write event to stdout: {e}");
                }
            }
            Err(e) => {
                // The skipped variants land here. Surface enough
                // detail to debug but don't kill the run.
                eprintln!("aj: failed to serialize event: {e}");
            }
        }
    })
}

/// Walk back through the agent's transcript to find the most recent
/// assistant message, then print every visible text block on its own
/// line. Callers piping the output into another process get the clean
/// final answer with no streaming chatter, no tool-result preambles,
/// and no thinking blocks — same contract as a single round-trip
/// through `Agent::prompt`.
fn print_final_assistant_text(agent: &Agent) -> Result<()> {
    let messages = agent.messages();
    let last_assistant = messages.iter().rev().find_map(|m| match m.as_wire() {
        Some(aj_models::types::Message::Assistant(a)) => Some(a),
        _ => None,
    });

    let Some(message) = last_assistant else {
        return Err(anyhow!(
            "agent produced no assistant message; nothing to print"
        ));
    };

    let mut stdout = io::stdout().lock();
    for block in &message.content {
        if let aj_models::types::AssistantContent::Text(t) = block {
            writeln!(stdout, "{}", t.text).context("failed to write assistant text to stdout")?;
        }
    }
    stdout.flush().ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Parse a CLI string into [`Args`] the same way `main.rs` does
    /// at startup. Convenient for tests that exercise the dispatch
    /// / prompt-collection logic without spinning up the binary.
    fn parse(args: &[&str]) -> Args {
        // `clap::Parser::parse_from` includes a binary-name arg at
        // position 0; we slot in `"aj"` so help text matches
        // the real surface.
        let mut argv = vec!["aj"];
        argv.extend_from_slice(args);
        Args::parse_from(argv)
    }

    #[test]
    fn collect_prompt_text_uses_top_level_prompt_when_no_subcommand() {
        let args = parse(&["--print", "hello", "world"]);
        let text = collect_prompt_text(&args).expect("prompt present");
        assert_eq!(text, "hello world");
    }

    #[test]
    fn collect_prompt_text_errors_when_no_prompt_supplied() {
        let args = parse(&["--print"]);
        let err = collect_prompt_text(&args).expect_err("empty prompt should error");
        assert!(
            err.to_string().contains("requires a prompt argument"),
            "unexpected error: {err}",
        );
    }

    #[test]
    fn collect_prompt_text_pulls_from_continue_subcommand_prompt() {
        // Top-level `args.prompt` is empty here because clap routed
        // the positionals after `continue` into the subcommand's
        // own slots: the first into `session_id`, the rest into
        // `prompt`.
        let args = parse(&["--print", "continue", "session-abc", "hello", "world"]);
        match &args.command {
            Some(Command::Continue { session_id, prompt }) => {
                assert_eq!(session_id.as_deref(), Some("session-abc"));
                assert_eq!(prompt, &vec!["hello".to_string(), "world".to_string()]);
            }
            other => panic!("expected Continue command, got {other:?}"),
        }
        assert!(args.prompt.is_empty(), "top-level prompt should be empty");

        let text = collect_prompt_text(&args).expect("continue prompt present");
        assert_eq!(text, "hello world");
    }

    #[test]
    fn collect_prompt_text_errors_when_continue_has_no_prompt() {
        // `continue` with only a session id and no trailing prompt
        // positionals: print mode still requires a prompt, so this
        // is an error rather than a silent no-op.
        let args = parse(&["--print", "continue", "session-abc"]);
        let err = collect_prompt_text(&args).expect_err("empty continue prompt should error");
        assert!(
            err.to_string().contains("requires a prompt argument"),
            "unexpected error: {err}",
        );
    }

    #[test]
    fn collect_prompt_text_treats_lone_continue_positional_as_session_id() {
        // `aj --print continue hello` is ambiguous between
        // "resume session `hello`" and "resume latest, run prompt
        // `hello`". Clap's greedy positional consumption picks the
        // first interpretation (single `Option<String>` slot fills
        // first), and `collect_prompt_text` falls back to the
        // top-level `args.prompt` (empty) so the "requires a prompt"
        // error fires. Users who want "latest + prompt" supply
        // the session id explicitly.
        let args = parse(&["--print", "continue", "hello"]);
        match &args.command {
            Some(Command::Continue { session_id, prompt }) => {
                assert_eq!(session_id.as_deref(), Some("hello"));
                assert!(prompt.is_empty());
            }
            other => panic!("expected Continue command, got {other:?}"),
        }
        let err = collect_prompt_text(&args).expect_err("no prompt should error");
        assert!(
            err.to_string().contains("requires a prompt argument"),
            "unexpected error: {err}",
        );
    }
}
