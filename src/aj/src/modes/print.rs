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
//! Print mode opens (or for `continue`, resumes) a
//! [`ConversationLog`](aj_session::ConversationLog) the same way
//! interactive mode does, so a `aj --print "do X"` invocation leaves a
//! resumable session on disk.
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
use aj_agent::{Agent, TaskRegistry, TurnError};
use aj_conf::{Config, ConfigSpeed, Severity};
use aj_models::auth::AuthStorage;
use aj_models::types::Speed;
use aj_session::{ConversationPersistence, ThreadFilter, persistence_listener, replay};
use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

use crate::cli::args::{Args, Command, PrintFormat};
use crate::session_setup::{
    BuiltAgent, PreparedLog, SessionSource, build_agent, build_initial_run_config, freeze_and_seed,
    prepare_log,
};

/// Drive a single print-mode run from `args`.
///
/// The flow mirrors interactive mode's session setup through the
/// shared [`crate::session_setup`] primitives (resolve the run config
/// with CLI > env > config precedence, open + repair the
/// [`ConversationLog`](aj_session::ConversationLog), build the agent,
/// freeze + seed) but skips the
/// readline loop: a single [`Agent::prompt`] runs to completion, then
/// the function either prints the final assistant text (text mode) or
/// relies on the JSONL listener to have streamed every live event
/// already (JSON mode).
///
/// When invoked under the `continue` subcommand, the run resumes the
/// requested session (or "latest for this project") rather than
/// creating a fresh log. On resume the persisted system prompt is
/// reused, any dangling tool_use ids are repaired, the agent's
/// transcript is seeded from the linearized user thread, and in JSON
/// mode the historical events from [`replay`] are drained through the
/// JSON sink before the new turn begins so the consumer sees the full
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

    // Resolve the positionals (messages + `@file` attachments) into the
    // launch turn content. Print mode is one-shot with no editor to fall
    // back on, so an empty result is a hard error rather than a quiet
    // no-op.
    let content = {
        let cwd = std::env::current_dir().unwrap_or_default();
        let input = crate::cli::initial_input(&args, &cwd)?;
        if input.is_empty() {
            bail!("aj --print requires a prompt argument");
        }
        input.into_content()
    };

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

    // Speed selection follows the same precedence as the model:
    // CLI flag > config.toml > default. `--speed` is parsed here; the
    // model bundle itself is resolved in `build_initial_run_config`
    // below.
    let speed = match args.speed.as_deref() {
        Some(s) => Some(s.parse::<ConfigSpeed>().map_err(anyhow::Error::msg)?),
        None => config.speed,
    }
    .map(|s| match s {
        ConfigSpeed::Standard => Speed::Standard,
        ConfigSpeed::Fast => Speed::Fast,
    });

    // Resolve the credential store up front: the registry path of
    // `build_initial_run_config` installs the lazy API-key resolver
    // against it, and the `--api-key` override below targets it.
    let auth = AuthStorage::at_default_path().context("failed to open ~/.aj/auth.json")?;

    // Resolve the initial run config (provider / model / thinking /
    // speed, merged CLI > env > config) plus the resume-time
    // `RestoreContext`; scripted mode skips restoration. Print has no
    // loop, so the snapshot is built, optionally overwritten by the
    // resumed log's recorded settings (inside `prepare_log`), and read
    // once to build the agent.
    let (run_config, restore_context) = build_initial_run_config(&args, &config, &auth, speed)?;
    let run_config = Arc::new(std::sync::Mutex::new(run_config));

    // Apply a `--api-key` runtime override to the resolved provider.
    // Skipped for the scripted fake provider, which needs no creds.
    // Key resolution is lazy, so it's read on the next inference
    // regardless of when the override lands.
    if args.scripted.is_none()
        && let Some(key) = args.api_key.clone()
    {
        let provider_id = {
            let cfg = run_config.lock().expect("run config mutex poisoned");
            cfg.model_key.0.clone()
        };
        auth.set_runtime_api_key(&provider_id, key).await;
    }

    // Resolve which session to open. `continue` with neither an
    // explicit id nor a latest session on disk is a hard error here:
    // print mode is one-shot and has no readline to fall back on.
    let sessions_dir = Config::get_sessions_dir_path()?;
    let conversation_persistence = ConversationPersistence::new(sessions_dir);
    let source = match &resume_request {
        Some(Some(id)) => SessionSource::Resume {
            session_id: id.clone(),
        },
        Some(None) => match conversation_persistence.get_latest_session_id()? {
            Some(latest) => SessionSource::Resume { session_id: latest },
            None => bail!(
                "no conversation sessions to resume; invoke `aj --print \"...\"` \
                 without `continue` to start a fresh session"
            ),
        },
        None => SessionSource::Create,
    };

    // Resolve + repair the log and, on a resume, restore its recorded
    // settings into the run config before the agent is built off it.
    // The log stays unwrapped until after the system-prompt freeze
    // below; it moves behind an `Arc<TokioMutex<_>>` once the
    // persistence listener takes a stake in it.
    let PreparedLog {
        mut log,
        transcript,
        restore_notices,
    } = prepare_log(
        &conversation_persistence,
        &source,
        &config,
        &run_config,
        restore_context.as_ref(),
    )?;
    for notice in &restore_notices {
        eprintln!("aj: {notice}");
    }

    // JSON mode: replay the persisted history through the JSON sink
    // **before** subscribing any listeners, so the consumer sees the
    // full historical trace in emit order without double-firing the
    // persistence listener (events already on disk would otherwise be
    // re-written). Text mode skips the historical events: callers
    // piping the binary's stdout into another process want a clean
    // final answer, not the prior conversation re-stamped.
    if matches!(source, SessionSource::Resume { .. })
        && matches!(args.format, PrintFormat::Json)
        && log.latest_leaf(ThreadFilter::USER).is_some()
    {
        let json = json_event_listener();
        for event in replay(&log) {
            json(&event).await.map_err(|e| {
                anyhow::Error::msg(e)
                    .context("failed to write replayed event to stdout during print-mode resume")
            })?;
        }
    }

    // Build the agent from the (post-restore) run config: a fresh
    // provider/model/thinking/speed bundle plus the disabled-tools
    // filter and a freshly-read `AgentEnv`. Surface any
    // skill-discovery diagnostics to stderr.
    let (provider, model_info, stream_options, thinking, agent_speed, model_key) = {
        let cfg = run_config.lock().expect("run config mutex poisoned");
        (
            Arc::clone(&cfg.provider),
            Arc::clone(&cfg.model_info),
            cfg.stream_options.clone(),
            cfg.thinking.clone(),
            cfg.speed,
            cfg.model_key.clone(),
        )
    };
    let BuiltAgent {
        mut agent,
        env,
        include_skills,
    } = build_agent(
        &config,
        provider,
        model_info,
        stream_options,
        thinking.clone(),
        agent_speed,
    );
    for d in &env.skill_diagnostics {
        eprintln!("aj: warning: {d}");
    }

    // Inject a task registry so background tasks started during the
    // run can be killed at exit instead of orphaned. Print mode has
    // no wake-trigger loop: a notice queued after the final turn is
    // never drained, and whatever still runs when the prompt returns
    // is killed below — the bash tool description tells the model to
    // wait with a blocking `task_output` before finishing here.
    let task_registry = TaskRegistry::default();
    agent.set_task_registry(task_registry.clone());

    // Freeze the system prompt (fresh log) or reuse the persisted one
    // (cache-warm resume), then seed the agent's transcript, prompt,
    // and sub-agent counter floor. Mirrors the interactive path so a
    // session looks identical on disk whether bootstrapped through
    // `--print` or the TUI.
    freeze_and_seed(
        &mut log,
        &mut agent,
        transcript,
        &env,
        include_skills,
        &model_key,
        thinking.as_ref(),
        agent_speed,
    )?;

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
    // Drive the turn inline (print mode is one-shot — no spawn, no
    // responsiveness constraint) and use the result for the exit status.
    // The policy enables only reactive overflow recovery: the threshold
    // and queued-work paths don't apply to a single headless turn.
    // Recovery runs before the background-task teardown below so the
    // retried turn can still use tools.
    let policy = crate::turn::TurnPolicy {
        recover_overflow: config.auto_compact,
        auto_threshold: None,
        keep_recent: config.compact_keep_recent,
    };
    let prompt_result = crate::turn::drive_turn(
        &mut agent,
        &log,
        &policy,
        crate::turn::TurnStart::Content(content),
        |_| {},
        turn_cancel,
    )
    .await;
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
            return Err(anyhow::Error::msg(err).context("agent run failed (recoverable)"));
        }
        Err(TurnError::Fatal(err)) => {
            return Err(anyhow::Error::msg(err).context("agent run failed (fatal)"));
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

/// Build a [`Listener`] that writes each event as one JSONL line on
/// stdout. The listener is synchronous (`listener_from_sync`), and the
/// bus awaits it inline, so events appear in stdout in the same order
/// the agent emits them.
///
/// Every `AgentEvent` the agent emits is serializable (see
/// `aj-agent/src/events.rs`), so serialization is not expected to fail.
/// We still log-and-continue rather than abort the run on a write or
/// serialize error: a downstream consumer that hangs up (broken pipe),
/// or some future non-serializable payload, should surface as a stderr
/// warning and a visible gap, not kill the whole prompt.
fn json_event_listener() -> Listener {
    listener_from_sync(move |event: &AgentEvent| {
        // `ToolExecutionUpdate` is a high-frequency (~10/s) transient
        // progress snapshot for live rendering; it is never persisted
        // and adds only noise to a structured event stream meant for
        // programmatic consumption. The terminal `ToolExecutionEnd`
        // carries the authoritative result.
        if matches!(event, AgentEvent::ToolExecutionUpdate { .. }) {
            return;
        }
        match serde_json::to_string(event) {
            Ok(line) => {
                if let Err(e) = writeln!(io::stdout(), "{line}") {
                    eprintln!("aj: failed to write event to stdout: {e}");
                }
            }
            Err(e) => {
                // A non-serializable payload would land here. Surface
                // enough detail to debug but don't kill the run.
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

    /// Flatten the text content of the launch turn for assertions.
    fn turn_text(content: &[aj_models::types::UserContent]) -> String {
        content
            .iter()
            .filter_map(|c| match c {
                aj_models::types::UserContent::Text(t) => Some(t.text.clone()),
                aj_models::types::UserContent::Image(_) => None,
            })
            .collect()
    }

    /// Resolve a CLI invocation into its launch content (paths resolved
    /// against `/`, which has no `@file` args in these tests).
    fn content(args: &[&str]) -> Vec<aj_models::types::UserContent> {
        crate::cli::initial_input(&parse(args), std::path::Path::new("/"))
            .expect("resolve")
            .into_content()
    }

    #[test]
    fn bare_messages_are_joined() {
        assert_eq!(
            turn_text(&content(&["--print", "hello", "world"])),
            "hello world"
        );
    }

    #[test]
    fn empty_input_when_no_prompt_supplied() {
        let input =
            crate::cli::initial_input(&parse(&["--print"]), std::path::Path::new("/")).unwrap();
        assert!(input.is_empty());
    }

    #[test]
    fn pulls_from_continue_subcommand_prompt() {
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

        let content = content(&["--print", "continue", "session-abc", "hello", "world"]);
        assert_eq!(turn_text(&content), "hello world");
    }

    #[test]
    fn empty_input_when_continue_has_no_prompt() {
        // `continue` with only a session id and no trailing prompt
        // positionals: print mode still requires a prompt, so the
        // bail in `run` fires off this empty result.
        let args = parse(&["--print", "continue", "session-abc"]);
        let input = crate::cli::initial_input(&args, std::path::Path::new("/")).unwrap();
        assert!(input.is_empty());
    }

    #[test]
    fn treats_lone_continue_positional_as_session_id() {
        // `aj --print continue hello` is ambiguous between
        // "resume session `hello`" and "resume latest, run prompt
        // `hello`". Clap's greedy positional consumption picks the
        // first interpretation (single `Option<String>` slot fills
        // first), so there is no prompt and the "requires a prompt"
        // bail in `run` fires. Users who want "latest + prompt"
        // supply the session id explicitly.
        let args = parse(&["--print", "continue", "hello"]);
        match &args.command {
            Some(Command::Continue { session_id, prompt }) => {
                assert_eq!(session_id.as_deref(), Some("hello"));
                assert!(prompt.is_empty());
            }
            other => panic!("expected Continue command, got {other:?}"),
        }
        let input = crate::cli::initial_input(&args, std::path::Path::new("/")).unwrap();
        assert!(input.is_empty());
    }
}
