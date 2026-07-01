//! TUI-agnostic test builders shared with downstream crates' tests.
//!
//! Gated behind the `test-support` feature (not `#[cfg(test)]`) so
//! consuming crates can build the same scripted-provider and run-config
//! fixtures in their own tests. A crate's `cfg(test)` items are not
//! visible across crate boundaries, which is why this is a feature.
//!
//! Frontend-bound helpers (a `Terminal` stub, the interactive
//! `SessionWorld` builder) stay in the consuming binary.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aj_agent::Agent;
use aj_agent::bus::SubscriptionHandle;
use aj_conf::Config;
use aj_models::registry::ModelInfo;
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::types::{
    AssistantContent, AssistantMessage, StopReason, StreamOptions, TextContent,
};
use aj_session::{ConversationLog, ConversationPersistence, persistence_listener};
use tokio::sync::Mutex as TokioMutex;

use crate::session_setup::{
    BuiltAgent, PreparedLog, RunConfigSnapshot, SessionSource, build_agent, freeze_and_seed,
    prepare_log,
};

/// [`ModelInfo`] consistent with the identity [`ScriptedProvider`]
/// stamps on every emitted partial, so the agent sees a coherent
/// provider identity in tests.
pub fn scripted_model_info() -> ModelInfo {
    ModelInfo {
        id: "scripted".to_string(),
        name: "scripted".to_string(),
        api: "scripted".to_string(),
        provider: "scripted".to_string(),
        base_url: "scripted://internal".to_string(),
        reasoning: false,
        supports_adaptive_thinking: false,
        supports_verbosity: false,
        input: vec![aj_models::registry::InputModality::Text],
        cost: aj_models::registry::ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
    }
}

/// Finalized text-only assistant reply for scripting one-turn
/// conversations (no tool calls, `EndTurn`).
pub fn finalized_text_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        api: "scripted".to_string(),
        provider: "scripted".to_string(),
        model: "scripted".to_string(),
        response_id: Some("test-msg".to_string()),
        usage: Default::default(),
        stop_reason: StopReason::Stop,
        error: None,
        timestamp: 0,
    }
}

/// [`finalized_text_message`] carrying a non-zero input-token `usage`,
/// so occupancy-driven triggers see a real context size.
pub fn finalized_text_message_with_usage(text: &str, input_tokens: u64) -> AssistantMessage {
    let mut m = finalized_text_message(text);
    m.usage.input = input_tokens;
    m
}

/// Run-config snapshot over a [`ScriptedProvider`] replaying
/// `messages`. `ExhaustedBehavior::Panic` makes any unscripted
/// extra inference fail loudly.
pub fn scripted_run_config(messages: Vec<AssistantMessage>) -> Arc<StdMutex<RunConfigSnapshot>> {
    Arc::new(StdMutex::new(RunConfigSnapshot {
        provider: Arc::new(
            ScriptedProvider::from_messages(messages, 0, Duration::ZERO)
                .on_exhausted(ExhaustedBehavior::Panic),
        ),
        model_info: Arc::new(scripted_model_info()),
        stream_options: StreamOptions::default(),
        thinking: None,
        speed: None,
        model_key: ("scripted".to_string(), "scripted".to_string()),
        session_id: None,
    }))
}

/// Like [`scripted_run_config`] but with a non-zero `context_window`,
/// for tests that exercise occupancy-driven triggers (threshold
/// compaction, silent overflow).
pub fn scripted_run_config_with_window(
    messages: Vec<AssistantMessage>,
    context_window: u64,
) -> Arc<StdMutex<RunConfigSnapshot>> {
    let mut model_info = scripted_model_info();
    model_info.context_window = context_window;
    Arc::new(StdMutex::new(RunConfigSnapshot {
        provider: Arc::new(
            ScriptedProvider::from_messages(messages, 0, Duration::ZERO)
                .on_exhausted(ExhaustedBehavior::Panic),
        ),
        model_info: Arc::new(model_info),
        stream_options: StreamOptions::default(),
        thinking: None,
        speed: None,
        model_key: ("scripted".to_string(), "scripted".to_string()),
        session_id: None,
    }))
}

/// Build a headless agent over a fresh (`Create`) session, running the
/// frontend-agnostic half of the interactive session setup:
/// [`prepare_log`] -> [`build_agent`] -> [`freeze_and_seed`], then
/// subscribing the persistence listener so driven turns write real
/// entries into the returned log. This is the shared core of the
/// interactive `SessionWorld::build`, minus the event pump, sub-agent
/// registry, and TUI theme.
///
/// The returned [`SubscriptionHandle`] must be kept alive for the life
/// of the agent: dropping it detaches the persistence listener, so a
/// caller that needs driven turns persisted (compaction reads the log)
/// binds it rather than discarding it.
pub fn build_test_agent(
    persistence: &ConversationPersistence,
    run_config: &Arc<StdMutex<RunConfigSnapshot>>,
) -> (Agent, Arc<TokioMutex<ConversationLog>>, SubscriptionHandle) {
    let config = Config::default();

    let PreparedLog {
        mut log,
        transcript,
        restore_notices: _,
    } = prepare_log(
        persistence,
        &SessionSource::Create,
        &config,
        run_config,
        None,
    )
    .expect("prepare log");

    let (provider, model_info, stream_options, thinking, speed, verbosity, model_key) = {
        let cfg = run_config.lock().expect("run config mutex poisoned");
        (
            Arc::clone(&cfg.provider),
            Arc::clone(&cfg.model_info),
            cfg.stream_options.clone(),
            cfg.thinking.clone(),
            cfg.speed,
            cfg.stream_options.verbosity,
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
        speed,
    );

    freeze_and_seed(
        &mut log,
        &mut agent,
        transcript,
        &env,
        include_skills,
        &model_key,
        thinking.as_ref(),
        speed,
        verbosity,
    )
    .expect("freeze and seed");

    let log = Arc::new(TokioMutex::new(log));
    let persistence_handle = agent.subscribe(persistence_listener(Arc::clone(&log)));
    (agent, log, persistence_handle)
}
