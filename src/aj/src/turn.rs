//! The turn driver: drive one user-initiated turn and its automatic
//! continuations (overflow recovery, queued-work delivery, threshold
//! compaction) to quiescence.
//!
//! `aj::compaction` owns the compaction *mechanics* (`run_compaction`);
//! this module owns the turn *lifecycle*. Both the interactive TUI and
//! `--print` drive turns through [`drive_turn`], so the post-turn policy
//! lives in exactly one place rather than being duplicated across the
//! two frontends' loops. See `docs/compaction-spec.md` §7.2.

use std::sync::Arc;

use aj_agent::events::CompactionReason;
use aj_agent::{Agent, TurnError, WakeOutcome};
use aj_models::errors::is_context_overflow;
use aj_models::types::UserContent;
use aj_session::ConversationLog;
use aj_session::compaction::should_compact;
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

use crate::compaction::run_compaction;

/// How a turn sequence begins.
pub enum TurnStart {
    /// A typed user prompt. Drives [`Agent::prompt`].
    Prompt(String),
    /// CLI launch content (text + `@file`/image blocks). Drives
    /// [`Agent::prompt_with_content`].
    Content(Vec<UserContent>),
    /// Drain queued notices/messages and run. Drives [`Agent::wake`]; a
    /// no-op (no events) when nothing is pending.
    Wake,
    /// Compact only — no turn. Drives `run_compaction` and returns.
    Compact {
        reason: CompactionReason,
        instructions: Option<String>,
    },
}

/// The automatic continuations [`drive_turn`] applies after each turn.
///
/// Constructed per caller: interactive Main enables all of them, a
/// sub-agent continuation only `wake`, print mode only
/// `recover_overflow`.
pub struct TurnPolicy {
    /// Compact and retry once when a turn fails with a context overflow.
    pub recover_overflow: bool,
    /// `Some(t)`: after a successful turn whose occupancy crossed `t` of
    /// the model's context window, compact (no re-drive). `None`
    /// disables the threshold trigger (print mode, sub-agents).
    pub auto_threshold: Option<f64>,
    /// After a turn, deliver queued notices/messages via [`Agent::wake`]
    /// and continue. Off in print mode (one-shot).
    pub wake: bool,
    /// Recent-tail budget kept verbatim across a compaction.
    pub keep_recent: u64,
}

/// Message appended to the error chain when overflow recovery's retry
/// overflows again. Shared so interactive and print word it identically.
const OVERFLOW_GIVEUP: &str =
    "context overflow recovery failed; reduce context or switch to a larger-context model";

/// Drive one turn and its automatic continuations to quiescence.
///
/// `reconfigure` re-stamps the latest staged run-config onto the agent
/// before each inference (interactive's `apply_turn_config`; a no-op in
/// print mode). Returns the final turn result: `Ok` when the sequence
/// settled cleanly, `Recoverable`/`Aborted` for the caller to surface,
/// `Fatal` to bubble out. Progress (compaction start/end, message
/// events) is emitted on the agent bus as it happens, so a spawned
/// caller's UI updates live mid-sequence.
///
/// The single `cancel` token covers the whole sequence: one fire stops
/// the in-flight inference and every continuation.
pub async fn drive_turn(
    agent: &mut Agent,
    log: &Arc<TokioMutex<ConversationLog>>,
    policy: &TurnPolicy,
    start: TurnStart,
    mut reconfigure: impl FnMut(&mut Agent),
    cancel: CancellationToken,
) -> Result<(), TurnError> {
    reconfigure(agent);
    let mut result = match start {
        // A compact-only start has no turn and no post-turn ladder.
        TurnStart::Compact {
            reason,
            instructions,
        } => {
            let _ = run_compaction(
                agent,
                log,
                reason,
                instructions.as_deref(),
                policy.keep_recent,
                cancel,
            )
            .await;
            return Ok(());
        }
        TurnStart::Prompt(text) => agent.prompt(text, cancel.clone()).await,
        TurnStart::Content(content) => agent.prompt_with_content(content, cancel.clone()).await,
        TurnStart::Wake => agent.wake(cancel.clone()).await.map(|_| ()),
    };

    // One reactive overflow recovery per sequence; a repeat overflow
    // surfaces the wrapped error instead of looping.
    let mut overflow_recovered = false;

    loop {
        // 1. Reactive overflow recovery (compact + retry once). The
        //    failed assistant is classified from the agent's retained
        //    terminal message, no log round-trip.
        if matches!(result, Err(TurnError::Recoverable(_)))
            && policy.recover_overflow
            && last_turn_overflowed(agent)
        {
            if overflow_recovered {
                return result.map_err(wrap_overflow_giveup);
            }
            overflow_recovered = true;
            reconfigure(agent);
            let _ = run_compaction(
                agent,
                log,
                CompactionReason::Overflow,
                None,
                policy.keep_recent,
                cancel.clone(),
            )
            .await;
            // `run_compaction` trims the trailing failed assistant from
            // the reseed, so the transcript ends in a user/tool-result
            // message and `continue_run`'s precondition holds.
            result = agent.continue_run(cancel.clone()).await;
            continue;
        }

        // 2. Any other error (a non-overflow recoverable, or an abort):
        //    hand it back for the caller to surface.
        if result.is_err() {
            return result;
        }

        // 3. Deliver work that queued while the turn ran. `wake`
        //    self-gates: `Empty` means nothing pending (no events
        //    emitted), so we fall through to the threshold check.
        if policy.wake {
            reconfigure(agent);
            match agent.wake(cancel.clone()).await {
                Ok(WakeOutcome::Ran) => {
                    result = Ok(());
                    continue;
                }
                Ok(WakeOutcome::Empty) => {}
                // A wake turn can itself error/overflow; loop so step 1/2
                // handle its result.
                Err(e) => {
                    result = Err(e);
                    continue;
                }
            }
        }

        // 4. Threshold compaction. Terminal for the sequence: the next
        //    turn happens on the next prompt or wake.
        if let Some(threshold) = policy.auto_threshold
            && over_threshold(agent, threshold)
        {
            reconfigure(agent);
            let _ = run_compaction(
                agent,
                log,
                CompactionReason::Threshold,
                None,
                policy.keep_recent,
                cancel.clone(),
            )
            .await;
        }
        return result;
    }
}

/// Whether the most recent inference was a context overflow, read from
/// the agent's retained terminal assistant message (no log round-trip).
fn last_turn_overflowed(agent: &Agent) -> bool {
    let window = agent.model_info().context_window;
    agent
        .last_assistant()
        .is_some_and(|m| is_context_overflow(m, Some(window)))
}

/// Whether the last turn's occupancy crossed `threshold` of the window.
/// Occupancy is the prompt size the provider reported for the most
/// recent response (`input + cache_read + cache_write`) — the same
/// numerator the footer shows.
fn over_threshold(agent: &Agent, threshold: f64) -> bool {
    let window = agent.model_info().context_window;
    let Some(tokens) = agent.last_assistant().map(|m| {
        m.usage
            .input
            .saturating_add(m.usage.cache_read)
            .saturating_add(m.usage.cache_write)
    }) else {
        return false;
    };
    should_compact(tokens, window, threshold)
}

fn wrap_overflow_giveup(err: TurnError) -> TurnError {
    match err {
        TurnError::Recoverable(e) => TurnError::Recoverable(e.context(OVERFLOW_GIVEUP)),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use aj_agent::TurnError;
    use aj_models::types::{AssistantContent, AssistantMessage};
    use aj_session::ConversationPersistence;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::{TurnPolicy, TurnStart, drive_turn};
    use crate::modes::interactive::test_support::{
        build_test_world, create_spec, finalized_text_message, finalized_text_message_with_usage,
        scripted_run_config, scripted_run_config_with_window,
    };

    /// A terminal `Error` carrying a [`ContextOverflow`] category — the
    /// shape the model layer produces when the prompt didn't fit. The
    /// agent classifies it as non-retryable, so a turn that hits it
    /// surfaces `Recoverable` with this message retained as
    /// `last_assistant`.
    ///
    /// [`ContextOverflow`]: aj_models::types::ErrorCategory::ContextOverflow
    fn overflow_error_message() -> AssistantMessage {
        let mut m = finalized_text_message("");
        m.stop_reason = aj_models::types::StopReason::Error;
        m.error = Some(aj_models::types::AssistantError::new(
            aj_models::types::ErrorCategory::ContextOverflow,
            "prompt is too long: 250000 tokens > 200000 maximum",
        ));
        m
    }

    /// Policy that drives reactive overflow recovery and nothing else
    /// (no wake, no threshold compaction).
    fn recover_policy() -> TurnPolicy {
        TurnPolicy {
            recover_overflow: true,
            auto_threshold: None,
            wake: false,
            keep_recent: 20_000,
        }
    }

    /// Concatenated text of the agent's retained terminal message.
    fn last_assistant_text(agent: &aj_agent::Agent) -> String {
        agent
            .last_assistant()
            .expect("terminal message retained")
            .content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// A turn that overflows then succeeds on the recovery retry settles
    /// `Ok`, with the success retained as the terminal message.
    #[tokio::test]
    async fn overflow_recovers_and_retries_succeeds() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![
            overflow_error_message(),
            finalized_text_message("recovered"),
        ]);
        let world = build_test_world(&persistence, &run_config, &create_spec()).expect("world");

        let mut agent = world.agent.lock().await;
        let policy = recover_policy();
        let result = drive_turn(
            &mut agent,
            &world.log,
            &policy,
            TurnStart::Prompt("hi".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        assert!(
            result.is_ok(),
            "recovered turn should settle Ok: {result:?}"
        );
        assert_eq!(
            agent
                .last_assistant()
                .expect("terminal message")
                .stop_reason,
            aj_models::types::StopReason::Stop
        );
        assert!(last_assistant_text(&agent).contains("recovered"));
    }

    /// A second overflow on the recovery retry surfaces the wrapped
    /// give-up error rather than looping on compaction.
    #[tokio::test]
    async fn repeat_overflow_returns_wrapped_giveup() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config =
            scripted_run_config(vec![overflow_error_message(), overflow_error_message()]);
        let world = build_test_world(&persistence, &run_config, &create_spec()).expect("world");

        let mut agent = world.agent.lock().await;
        let policy = recover_policy();
        let result = drive_turn(
            &mut agent,
            &world.log,
            &policy,
            TurnStart::Prompt("hi".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        match result {
            Err(TurnError::Recoverable(e)) => {
                assert!(
                    format!("{e:#}").contains("context overflow recovery failed"),
                    "expected give-up context, got: {e:#}"
                );
            }
            other => panic!("expected wrapped recoverable give-up, got {other:?}"),
        }
    }

    /// With `recover_overflow` disabled, an overflow surfaces raw — no
    /// compaction, no retry, no give-up wrapping.
    #[tokio::test]
    async fn overflow_not_recovered_when_policy_disabled() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![overflow_error_message()]);
        let world = build_test_world(&persistence, &run_config, &create_spec()).expect("world");

        let mut agent = world.agent.lock().await;
        let policy = TurnPolicy {
            recover_overflow: false,
            auto_threshold: None,
            wake: false,
            keep_recent: 20_000,
        };
        let result = drive_turn(
            &mut agent,
            &world.log,
            &policy,
            TurnStart::Prompt("hi".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        match result {
            Err(TurnError::Recoverable(e)) => {
                assert!(
                    !format!("{e:#}").contains("recovery failed"),
                    "raw overflow should not be wrapped as a give-up: {e:#}"
                );
            }
            other => panic!("expected raw recoverable overflow, got {other:?}"),
        }
    }

    /// A clean turn with no continuation triggers returns `Ok` after a
    /// single inference.
    #[tokio::test]
    async fn successful_turn_without_triggers_returns_ok() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("done")]);
        let world = build_test_world(&persistence, &run_config, &create_spec()).expect("world");

        let mut agent = world.agent.lock().await;
        let policy = TurnPolicy {
            recover_overflow: false,
            auto_threshold: None,
            wake: false,
            keep_recent: 20_000,
        };
        let result = drive_turn(
            &mut agent,
            &world.log,
            &policy,
            TurnStart::Prompt("hi".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        assert!(result.is_ok(), "clean turn should settle Ok: {result:?}");
        assert!(last_assistant_text(&agent).contains("done"));
    }

    /// A successful turn whose occupancy crossed the threshold compacts
    /// once (the reseeded transcript carries the summary) and does not
    /// re-drive inference.
    #[tokio::test]
    async fn over_threshold_turn_compacts_once() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        // Window 1000; the threshold turn reports 900 input tokens
        // (> 0.85 * 1000). The threshold turn's large user prompt makes
        // the keep-recent cut land on that user message (a turn start,
        // so no split), leaving the prior turn as the range to summarize.
        let run_config = scripted_run_config_with_window(
            vec![
                finalized_text_message("first answer"),
                finalized_text_message_with_usage("ok", 900),
                finalized_text_message("SUMMARY of earlier work"),
            ],
            1000,
        );
        let world = build_test_world(&persistence, &run_config, &create_spec()).expect("world");
        crate::modes::interactive::test_support::drive_turn(&world, "first question").await;

        let mut agent = world.agent.lock().await;
        let policy = TurnPolicy {
            recover_overflow: false,
            auto_threshold: Some(0.85),
            wake: false,
            keep_recent: 10,
        };
        let result = drive_turn(
            &mut agent,
            &world.log,
            &policy,
            TurnStart::Prompt("X".repeat(2000)),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        assert!(result.is_ok(), "threshold turn settles Ok: {result:?}");
        assert!(
            format!("{:?}", agent.messages()).contains("SUMMARY of earlier work"),
            "reseeded transcript carries the compaction summary: {:?}",
            agent.messages()
        );
    }

    /// A successful turn under the threshold neither compacts nor
    /// re-drives: occupancy 100 against a 1000-token window stays below
    /// the 0.85 bar, and the strict provider would panic on a second
    /// (summary) inference.
    #[tokio::test]
    async fn under_threshold_turn_does_not_compact() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config_with_window(
            vec![finalized_text_message_with_usage("ok", 100)],
            1000,
        );
        let world = build_test_world(&persistence, &run_config, &create_spec()).expect("world");

        let mut agent = world.agent.lock().await;
        let policy = TurnPolicy {
            recover_overflow: false,
            auto_threshold: Some(0.85),
            wake: false,
            keep_recent: 10,
        };
        let result = drive_turn(
            &mut agent,
            &world.log,
            &policy,
            TurnStart::Prompt("hi".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;

        assert!(
            result.is_ok(),
            "under-threshold turn settles Ok: {result:?}"
        );
        assert!(
            !format!("{:?}", agent.messages()).contains("compacted into the following summary"),
            "no compaction summary should be present: {:?}",
            agent.messages()
        );
    }
}
