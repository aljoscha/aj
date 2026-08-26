//! Cancelled-turn facts through the real agent and persistence listener.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::TurnError;
use aj_agent::bus::listener_from_sync;
use aj_agent::events::AgentEvent;
use aj_app::session_setup::RunConfigSnapshot;
use aj_app::test_support::{build_test_agent, scripted_model_info};
use aj_models::provider::Provider;
use aj_models::registry::{ModelCost, ModelInfo};
use aj_models::scripted::{ExhaustedBehavior, ProviderScript, ScriptedProvider};
use aj_models::streaming::{AssistantMessageEvent, AssistantMessageEventStream, DoneReason};
use aj_models::types::{
    AssistantContent, AssistantMessage, Context, ErrorCategory, Message, SimpleStreamOptions,
    StopReason, StreamOptions, TextContent, Usage, UsageCost,
};
use aj_session::{ConversationLog, ConversationPersistence, ThreadFilter};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn priced_model() -> ModelInfo {
    ModelInfo {
        cost: ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            tiers: Vec::new(),
        },
        ..scripted_model_info()
    }
}

fn run_config(scripts: Vec<ProviderScript>, model: ModelInfo) -> Arc<Mutex<RunConfigSnapshot>> {
    let provider: Arc<dyn Provider> =
        Arc::new(ScriptedProvider::new(scripts).on_exhausted(ExhaustedBehavior::Panic));
    provider_run_config(provider, model)
}

fn provider_run_config(
    provider: Arc<dyn Provider>,
    model: ModelInfo,
) -> Arc<Mutex<RunConfigSnapshot>> {
    Arc::new(Mutex::new(RunConfigSnapshot {
        provider,
        model_info: Arc::new(model),
        stream_options: StreamOptions::default(),
        thinking: None,
        thinking_display: None,
        speed: None,
        model_key: ("scripted".to_string(), "scripted".to_string()),
        session_id: None,
    }))
}

struct ImmediateProvider {
    events: Vec<AssistantMessageEvent>,
}

impl Provider for ImmediateProvider {
    fn stream(
        &self,
        _model: &ModelInfo,
        _context: &Context,
        _options: &StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        for event in &self.events {
            stream.push(event.clone());
        }
        stream
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.stream(model, context, &options.base)
    }
}

fn partial(text: &str) -> AssistantMessage {
    let mut partial = AssistantMessage::empty();
    partial.api = "scripted".to_string();
    partial.provider = "scripted".to_string();
    partial.model = "scripted".to_string();
    if !text.is_empty() {
        partial.content = vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })];
    }
    partial
}

fn persisted_assistants(
    persistence: &ConversationPersistence,
    session_id: &str,
) -> Vec<AssistantMessage> {
    let log = ConversationLog::resume(persistence, session_id).expect("resume persisted session");
    let head = log
        .latest_leaf(ThreadFilter::USER)
        .expect("persisted user-thread head");
    log.linearize(&head, ThreadFilter::USER)
        .messages()
        .into_iter()
        .filter_map(|message| match message {
            Message::Assistant(message) => Some(message),
            _ => None,
        })
        .collect()
}

fn assert_usage(usage: &Usage, counts: (u64, u64, u64, u64), expected_cost: (f64, f64, f64, f64)) {
    let (input, output, cache_read, cache_write) = counts;
    assert_eq!(
        (
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_write,
        ),
        counts,
    );
    assert_eq!(
        usage.total_tokens,
        input + output + cache_read + cache_write
    );
    assert!((usage.cost.input - expected_cost.0).abs() < 1e-12);
    assert!((usage.cost.output - expected_cost.1).abs() < 1e-12);
    assert!((usage.cost.cache_read - expected_cost.2).abs() < 1e-12);
    assert!((usage.cost.cache_write - expected_cost.3).abs() < 1e-12);
    assert!(
        (usage.cost.total
            - (expected_cost.0 + expected_cost.1 + expected_cost.2 + expected_cost.3))
            .abs()
            < 1e-12
    );
}

/// A terminal already queued when cancellation fires contributes its exact
/// usage and account to the one aborted terminal persisted for the turn.
#[tokio::test]
async fn cancellation_drains_a_queued_priced_terminal_before_persisting() {
    let start = partial("");
    let mut text_start = partial("");
    text_start.content = vec![AssistantContent::Text(TextContent {
        text: String::new(),
        text_signature: None,
    })];
    let first_delta = partial("x");
    let second_delta = partial("xy");
    let mut done = second_delta.clone();
    done.account = Some("work".to_string());
    done.stop_reason = StopReason::Stop;
    done.usage = Usage {
        input: 1_000,
        output: 200,
        cache_read: 2_000,
        cache_write: 500,
        ..Usage::default()
    };
    let script = ProviderScript::from_events(vec![
        AssistantMessageEvent::Start { partial: start },
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: text_start,
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "x".to_string(),
            partial: first_delta,
        },
        AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "y".to_string(),
            partial: second_delta,
        },
        AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: done,
        },
    ]);

    let root = TempDir::new().expect("tempdir");
    let persistence = ConversationPersistence::new(root.path().join("sessions"));
    let (mut agent, log, persistence_handle) =
        build_test_agent(&persistence, &run_config(vec![script], priced_model()));
    let session_id = log.lock().await.session_id().to_string();

    let cancel = CancellationToken::new();
    let cancel_from_listener = cancel.clone();
    let fired = Arc::new(AtomicBool::new(false));
    let fired_from_listener = Arc::clone(&fired);
    let updates = Arc::new(Mutex::new(Vec::new()));
    let updates_from_listener = Arc::clone(&updates);
    let event_handle = agent.subscribe(listener_from_sync(move |event| {
        let AgentEvent::MessageUpdate { event, .. } = event else {
            return;
        };
        let label = match event {
            AssistantMessageEvent::Start { .. } => "start".to_string(),
            AssistantMessageEvent::TextStart { .. } => "text_start".to_string(),
            AssistantMessageEvent::TextDelta { delta, .. } => format!("delta:{delta}"),
            AssistantMessageEvent::Done { .. } => "done".to_string(),
            AssistantMessageEvent::Error { reason, .. } => format!("error:{reason:?}"),
            other => format!("other:{other:?}"),
        };
        updates_from_listener.lock().unwrap().push(label);
        if matches!(event, AssistantMessageEvent::TextDelta { delta, .. } if delta == "x")
            && !fired_from_listener.swap(true, Ordering::SeqCst)
        {
            cancel_from_listener.cancel();
        }
    }));

    let prompt_result = agent.prompt("cancel me".to_string(), cancel).await;
    drop(agent);
    drop(event_handle);
    drop(persistence_handle);
    drop(log);
    let persisted = persisted_assistants(&persistence, &session_id);
    assert_eq!(persisted.len(), 1, "persisted assistant count");
    let persisted = &persisted[0];
    assert_eq!(persisted.stop_reason, StopReason::Aborted);
    assert_eq!(persisted.account.as_deref(), Some("work"));
    assert_usage(
        &persisted.usage,
        (1_000, 200, 2_000, 500),
        (0.003, 0.003, 0.000_6, 0.001_875),
    );
    let [AssistantContent::Text(text)] = persisted.content.as_slice() else {
        panic!(
            "expected exactly one persisted text block, got {:?}",
            persisted.content
        );
    };
    assert_eq!(text.text, "xy");
    let error = prompt_result.expect_err("the listener cancels the turn");
    assert!(matches!(error, TurnError::Aborted), "got {error:?}");
    assert!(
        fired.load(Ordering::SeqCst),
        "the fixture fired cancellation"
    );
    assert_eq!(
        *updates.lock().unwrap(),
        ["start", "text_start", "delta:x", "delta:y", "error:Aborted",],
        "the ready nonterminal was forwarded once and Done was folded, not forwarded",
    );
}

/// The agent preserves a provider-specific terminal price instead of
/// recalculating it from its tier-blind model rates.
#[tokio::test]
async fn cancellation_keeps_the_queued_providers_exact_price() {
    let start = partial("");
    let mut text_start = partial("");
    text_start.content = vec![AssistantContent::Text(TextContent {
        text: String::new(),
        text_signature: None,
    })];
    let delta = partial("x");
    let mut done = delta.clone();
    done.stop_reason = StopReason::Stop;
    done.usage = Usage {
        input: 1_000_000,
        output: 1_000_000,
        total_tokens: 2_000_000,
        cost: UsageCost {
            input: 1.5,
            output: 7.5,
            total: 9.0,
            ..UsageCost::default()
        },
        ..Usage::default()
    };
    let provider: Arc<dyn Provider> = Arc::new(ImmediateProvider {
        events: vec![
            AssistantMessageEvent::Start { partial: start },
            AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: text_start,
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "x".to_string(),
                partial: delta,
            },
            AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: done,
            },
        ],
    });

    let root = TempDir::new().expect("tempdir");
    let persistence = ConversationPersistence::new(root.path().join("sessions"));
    let (mut agent, log, persistence_handle) =
        build_test_agent(&persistence, &provider_run_config(provider, priced_model()));
    let session_id = log.lock().await.session_id().to_string();
    let cancel = CancellationToken::new();
    let cancel_from_listener = cancel.clone();
    let fired = Arc::new(AtomicBool::new(false));
    let fired_from_listener = Arc::clone(&fired);
    let event_handle = agent.subscribe(listener_from_sync(move |event| {
        if matches!(
            event,
            AgentEvent::MessageUpdate {
                event: AssistantMessageEvent::TextDelta { .. },
                ..
            }
        ) && !fired_from_listener.swap(true, Ordering::SeqCst)
        {
            cancel_from_listener.cancel();
        }
    }));

    let prompt_result = agent.prompt("cancel me".to_string(), cancel).await;
    drop(agent);
    drop(event_handle);
    drop(persistence_handle);
    drop(log);
    let persisted = persisted_assistants(&persistence, &session_id);
    assert_eq!(persisted.len(), 1, "persisted assistant count");
    let persisted = &persisted[0];
    assert_eq!(persisted.stop_reason, StopReason::Aborted);
    assert_eq!(persisted.usage.total_tokens, 2_000_000);
    assert_eq!(persisted.usage.cost.input, 1.5);
    assert_eq!(persisted.usage.cost.output, 7.5);
    assert_eq!(persisted.usage.cost.total, 9.0);
    let error = prompt_result.expect_err("the listener cancels the turn");
    assert!(matches!(error, TurnError::Aborted), "got {error:?}");
    assert!(
        fired.load(Ordering::SeqCst),
        "the fixture fired cancellation"
    );
}

/// Usage disclosed on an early partial stays priced when cancellation finds
/// no queued terminal and persistence records the synthesized abort.
#[tokio::test(start_paused = true)]
async fn cancellation_persists_an_anthropic_shaped_priced_partial() {
    let mut start = partial("");
    start.usage.input = 1_000;
    start.usage.cache_read = 2_000;
    start.usage.cache_write = 500;
    let mut done = start.clone();
    done.stop_reason = StopReason::Stop;
    let script = ProviderScript::new()
        .push_immediate(AssistantMessageEvent::Start { partial: start })
        .push(
            Duration::from_secs(60),
            AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: done,
            },
        );

    let root = TempDir::new().expect("tempdir");
    let persistence = ConversationPersistence::new(root.path().join("sessions"));
    let (mut agent, log, persistence_handle) =
        build_test_agent(&persistence, &run_config(vec![script], priced_model()));
    let session_id = log.lock().await.session_id().to_string();
    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    let fire_handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        fire.cancel();
    });

    let prompt_result = agent.prompt("cancel me".to_string(), cancel).await;
    drop(agent);
    drop(persistence_handle);
    drop(log);
    let persisted = persisted_assistants(&persistence, &session_id);
    assert_eq!(persisted.len(), 1, "persisted assistant count");
    let persisted = &persisted[0];
    assert_eq!(persisted.stop_reason, StopReason::Aborted);
    assert_usage(
        &persisted.usage,
        (1_000, 0, 2_000, 500),
        (0.003, 0.0, 0.000_6, 0.001_875),
    );
    assert!(
        persisted.usage.cost.total > 0.0,
        "the fixture must persist disclosed spend"
    );
    assert!(matches!(
        persisted.error.as_ref().map(|error| error.category),
        Some(ErrorCategory::Aborted)
    ));
    let error = prompt_result.expect_err("the timer cancels the turn");
    assert!(matches!(error, TurnError::Aborted), "got {error:?}");
    fire_handle.await.expect("cancel task");
}
