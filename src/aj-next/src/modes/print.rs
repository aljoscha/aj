//! Non-interactive print mode.
//!
//! Per `docs/aj-next-plan.md` §4.2 the same `aj-next` binary can run
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
//!   are intentionally suppressed — a caller piping `aj-next --print`
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
//! the same way interactive mode does, so a `aj-next --print "do X"`
//! invocation leaves a resumable thread on disk. A future commit will
//! teach print mode to honour `aj-next continue --print`; today the
//! function bails with a clear error if a `Continue` subcommand is
//! piped in alongside `--print` so users know the corner is unwired.

use std::io::{self, Write};
use std::sync::Arc;

use aj_agent::Agent;
use aj_agent::TurnError;
use aj_agent::bus::{Listener, listener_from_sync};
use aj_agent::events::AgentEvent;
use aj_conf::{AgentEnv, Config, ConfigSpeed};
use aj_models::messages::{ContentBlockParam, Role, Speed};
use aj_models::{ModelArgs, create_model};
use aj_session::{ConversationLog, ConversationPersistence, persistence_listener};
use aj_tools::get_builtin_tools;
use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::Mutex as TokioMutex;

use crate::SYSTEM_PROMPT;
use crate::cli::args::{Args, Command, PrintFormat};
use crate::cli::file_args;

/// Drive a single print-mode run from `args`.
///
/// The flow mirrors the legacy interactive binary's setup (load
/// config, resolve model args with CLI > env > config precedence,
/// build the agent + tool list, open a fresh thread) but skips the
/// readline loop: a single [`Agent::prompt`] runs to completion, then
/// the function either prints the final assistant text or relies on
/// the JSONL listener to have streamed every event already.
pub async fn run(args: Args) -> Result<()> {
    // Validate dispatch shape early so the user sees a clear error
    // instead of a confusing failure later. Print mode against
    // `continue` would need to replay history through the JSON
    // sink and resume from a persisted thread; the wiring lands
    // alongside the interactive resume work in a follow-up commit.
    match &args.command {
        None => {}
        Some(Command::Continue { .. }) => {
            bail!(
                "aj-next --print does not yet support `continue`; \
                 invoke without a subcommand to start a fresh thread"
            );
        }
        // `list-threads` and `models` are dispatched in `main.rs`
        // before any session setup; reaching them here would mean
        // the dispatcher routed incorrectly.
        Some(Command::ListThreads) | Some(Command::Models { .. }) => {
            bail!("aj-next --print does not accept this subcommand");
        }
    }

    let prompt_text = collect_prompt_text(&args)?;

    // Load config.toml first (lowest priority). Missing or invalid
    // config falls back to defaults so a one-shot `aj-next --print`
    // works in a freshly-cloned checkout without any setup.
    let config = Config::load().unwrap_or_else(|e| {
        tracing::warn!("failed to load config.toml: {e}");
        Config::default()
    });

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

    let model_args = ModelArgs {
        api: args
            .model_api
            .clone()
            .or_else(|| config.model_api.clone())
            .unwrap_or_else(|| "anthropic".to_string()),
        url: args.model_url.clone().or_else(|| config.model_url.clone()),
        model_name: args
            .model_name
            .clone()
            .or_else(|| config.model_name.clone()),
        speed,
    };
    let model = create_model(model_args).context("failed to construct model handle")?;

    // Build the tool list. Disabled tools are filtered up-front so
    // the agent never advertises them to the model; this matches the
    // legacy binary's behaviour and keeps the print/interactive
    // surfaces uniform.
    let mut tools = get_builtin_tools();
    if !config.disabled_tools.is_empty() {
        tools.retain(|tool| !config.disabled_tools.contains(&tool.name));
        tracing::info!(disabled = ?config.disabled_tools, "filtered disabled tools");
    }

    let env = AgentEnv::new();
    let mut agent = Agent::new(
        env,
        SYSTEM_PROMPT,
        tools,
        config.disabled_tools.clone(),
        Arc::clone(&model),
        config.thinking,
    );

    // Set up persistence in the same way interactive mode will: an
    // `Arc<TokioMutex<_>>` shared between the binary (for
    // start/end-of-run inspection) and the listener (for inline
    // appends). For now we only need the listener side; the binary
    // doesn't read the log back during a one-shot run.
    let threads_dir = Config::get_threads_dir_path()?;
    let conversation_persistence = ConversationPersistence::new(threads_dir);
    let log = ConversationLog::create(&conversation_persistence)?;
    let log = Arc::new(TokioMutex::new(log));

    // Freeze the system prompt as the log's root entry so future
    // resumes (`aj-next continue <id>`) reuse the same bytes the
    // model saw on this turn. The agent stores its own copy via
    // `set_assembled_system_prompt`; this push happens before any
    // turn runs so the persistence listener can rely on the prompt
    // already being on disk by the time a `MessagePersisted` fires.
    let system_prompt = agent.assemble_system_prompt();
    {
        let mut log = log.lock().await;
        if log.is_empty() {
            log.set_system_prompt(system_prompt.clone())?;
        }
    }
    agent.set_assembled_system_prompt(system_prompt);

    // Register the JSONL listener BEFORE the persistence listener so
    // that when persistence errors out (which the listener surfaces
    // as a fatal `Err`), the user has already seen every event up
    // to (but not including) the failure on stdout. Persistence
    // errors get printed by our outer error handler.
    //
    // For text mode we still register a listener — but it only
    // forwards a synchronous beat per event so the bus is not idle
    // (debug ergonomics; otherwise `cargo run -p aj-next -- --print
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
    // two buckets:
    //
    // - `Recoverable`: model errored mid-turn or returned a
    //   user-facing failure. The transcript and disk state remain
    //   internally consistent (the agent already synthesized any
    //   compensating tool_result entries before returning), but the
    //   run produced no useful output for the caller. Surface the
    //   error to stderr and exit non-zero.
    // - `Fatal`: a listener errored or the disk write failed. Same
    //   outward behaviour but with a fatal-flavoured error context
    //   so callers can tell them apart in scripts.
    let prompt_result = agent.prompt(prompt_text).await;
    match prompt_result {
        Ok(()) => {}
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

/// Collect the free-form prompt arguments into a single string, then
/// run them through `@file` expansion (today a passthrough — see
/// [`crate::cli::file_args::expand`]).
///
/// Returns an error if no prompt text was supplied; print mode is
/// fundamentally one-shot and needs an initial message to do anything.
fn collect_prompt_text(args: &Args) -> Result<String> {
    if args.prompt.is_empty() {
        bail!("aj-next --print requires a prompt argument");
    }
    let joined = args.prompt.join(" ");
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
                    eprintln!("aj-next: failed to write event to stdout: {e}");
                }
            }
            Err(e) => {
                // The skipped variants land here. Surface enough
                // detail to debug but don't kill the run.
                eprintln!("aj-next: failed to serialize event: {e}");
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
    let last_assistant = messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant));

    let Some(message) = last_assistant else {
        // Reaching this branch means the prompt ran to completion
        // without producing an assistant message — typically a
        // protocol error caught after the user message was already
        // appended. Surface a clear error so scripts can tell the
        // run failed even though `Agent::prompt` returned `Ok`.
        return Err(anyhow!(
            "agent produced no assistant message; nothing to print"
        ));
    };

    let mut stdout = io::stdout().lock();
    for block in &message.content {
        if let ContentBlockParam::TextBlock { text, .. } = block {
            writeln!(stdout, "{text}").context("failed to write assistant text to stdout")?;
        }
    }
    stdout.flush().ok();
    Ok(())
}
