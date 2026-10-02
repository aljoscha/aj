use std::sync::{Arc, Mutex};

use aj_agent::bus::listener_from_sync;
use aj_agent::events::{AgentEvent, AgentId, CompactionPhase};
use aj_agent::queue::MessageQueues;
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::streaming::AssistantMessageEventStream;
use aj_models::types::{
    AssistantContent, AssistantError, AssistantMessage, Context, ErrorCategory, Message,
    SimpleStreamOptions, StopReason, StreamOptions, ToolCall,
};
use aj_session::{AppendHandoff, ConversationEntryKind, ConversationPersistence, ThreadFilter};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::{TurnPolicy, TurnStart, drive_turn, turn_policy};
use crate::session::{SessionCore, SessionEntry, SessionSpec};
use crate::session_setup::RunConfigSnapshot;
use crate::test_support::{
    build_test_agent, finalized_text_message, scripted_run_config_with_window,
};

const OLD_WORK: &str = "OLD_WORK_TO_SUMMARIZE ";
const SUMMARY: &str = "CHECKPOINT_SUMMARY";

type RecordedRequests = Arc<Mutex<Vec<Context>>>;

struct RecordingProvider {
    inner: Arc<dyn Provider>,
    requests: RecordedRequests,
}

impl Provider for RecordingProvider {
    fn stream(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &StreamOptions,
    ) -> AssistantMessageEventStream {
        self.requests.lock().unwrap().push(context.clone());
        self.inner.stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.requests.lock().unwrap().push(context.clone());
        self.inner.stream_simple(model, context, options)
    }
}

fn recording_config(
    replies: Vec<AssistantMessage>,
) -> (Arc<Mutex<RunConfigSnapshot>>, RecordedRequests) {
    let config = scripted_run_config_with_window(Vec::new(), 10_000);
    let requests = Arc::new(Mutex::new(Vec::new()));
    {
        let mut config = config.lock().unwrap();
        config.main.provider = Arc::new(RecordingProvider {
            // ScriptedProvider observes cancellation at delayed steps.
            inner: Arc::new(
                ScriptedProvider::from_messages(replies, 0, std::time::Duration::from_millis(1))
                    .on_exhausted(ExhaustedBehavior::Panic),
            ),
            requests: Arc::clone(&requests),
        });
    }
    (config, requests)
}

fn policy() -> TurnPolicy {
    TurnPolicy {
        recover_overflow: true,
        auto_threshold: None,
        during_turn_threshold: Some(0.8),
        // The short current user and complete batch fit, but the old work does not.
        keep_recent: 2000,
    }
}

fn read_batch(dir: &TempDir, input: u64, contents: &str) -> AssistantMessage {
    let mut message = finalized_text_message("");
    message.stop_reason = StopReason::ToolUse;
    message.usage.input = input;
    message.content = (0..2)
        .map(|i| {
            let path = dir.path().join(format!("evidence-{i}.txt"));
            std::fs::write(&path, contents).unwrap();
            AssistantContent::ToolCall(ToolCall {
                id: format!("read-{i}"),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": path}),
            })
        })
        .collect();
    message
}

fn json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap()
}

fn assert_summary_request(context: &Context) {
    assert_eq!(
        context.system_prompt.as_deref(),
        Some(aj_session::compaction::SUMMARIZATION_SYSTEM_PROMPT)
    );
    assert!(context.tools.is_empty());
    assert!(json(&context.messages).contains(OLD_WORK.trim()));
}

fn assert_batch(context: &Context) {
    let calls: Vec<_> = context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(message) => Some(&message.content),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call.id.as_str()),
            _ => None,
        })
        .collect();
    let results: Vec<_> = context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => {
                assert!(!result.is_error, "real read_file must succeed");
                Some(result.tool_call_id.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(calls, ["read-0", "read-1"]);
    assert_eq!(results, calls);
}

async fn assert_replay(
    agent: &aj_agent::Agent,
    log: &Arc<tokio::sync::Mutex<aj_session::ConversationLog>>,
    store: &ConversationPersistence,
    config: &Arc<Mutex<RunConfigSnapshot>>,
    checkpoints: usize,
) {
    let guard = log.lock().await;
    let conversation = guard.linearize(guard.head().unwrap(), ThreadFilter::USER);
    assert_eq!(
        conversation
            .entries()
            .iter()
            .filter(|entry| matches!(entry.entry, ConversationEntryKind::Compaction { .. }))
            .count(),
        checkpoints
    );
    let mut persisted = agent.messages().to_vec();
    // Failed inference is retained separately by the live runtime, but its
    // terminal MessageEnd is still durable and restored on resume.
    if let Some(last) = agent.last_assistant()
        && last.stop_reason == StopReason::Error
    {
        persisted.push(Message::Assistant(last.clone()).into());
    }
    assert_eq!(json(&conversation.agent_messages()), json(&persisted));
    let session_id = guard.session_id().to_string();
    drop(guard);
    let (core, _) = SessionCore::build(
        &aj_conf::Config::default(),
        config.lock().unwrap().clone(),
        store,
        &SessionSpec::Resume {
            session_id,
            entry: SessionEntry::Startup,
        },
        None,
    )
    .unwrap();
    let (replayed, _log, _persistence) = core.into_test_agent();
    assert_eq!(json(&replayed.messages()), json(&persisted));
}

#[tokio::test]
async fn batch_compacts_before_continuing_with_steering_and_replays_exactly() {
    let dir = TempDir::new().unwrap();
    let (config, requests) = recording_config(vec![
        finalized_text_message(&OLD_WORK.repeat(3000)),
        read_batch(&dir, 9000, "evidence"),
        finalized_text_message(SUMMARY),
        finalized_text_message("finished"),
    ]);
    let store = ConversationPersistence::new(dir.path().join("sessions"));
    let (mut agent, log, _persistence) = build_test_agent(&store, &config);
    agent
        .prompt("old request".into(), CancellationToken::new())
        .await
        .unwrap();
    let queues = MessageQueues::default();
    agent.set_message_queues(queues.clone());
    let events = Arc::new(Mutex::new(Vec::new()));
    let _listener = agent.subscribe(listener_from_sync({
        let events = Arc::clone(&events);
        let queues = queues.clone();
        move |event| {
            if matches!(
                event,
                AgentEvent::CompactionProgress {
                    phase: CompactionPhase::Summarizing,
                    ..
                }
            ) {
                queues.append_steering(AgentId::Main, "STEERING_DURING_SUMMARY");
            }
            // Queue follow-up after steering is taken, since the public queue API
            // intentionally coalesces simultaneous pending input into one slot.
            if let AgentEvent::MessageEnd { message, .. } = event
                && json(message).contains("STEERING_DURING_SUMMARY")
            {
                queues.append_follow_up(AgentId::Main, "FOLLOW_UP_LATER");
            }
            events.lock().unwrap().push(event.clone());
        }
    }));
    drive_turn(
        &mut agent,
        &log,
        &AppendHandoff::default(),
        policy,
        TurnStart::Prompt("read both files".into()),
        |_| {},
        CancellationToken::new(),
    )
    .await
    .unwrap();

    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_summary_request(&requests[2]);
        let continuation = &requests[3];
        assert_batch(continuation);
        let text = json(&continuation.messages);
        assert!(text.contains(SUMMARY));
        assert!(!text.contains(OLD_WORK.trim()));
        assert!(text.contains("STEERING_DURING_SUMMARY"));
        assert!(!text.contains("FOLLOW_UP_LATER"));
        assert!(!json(&requests[2]).contains("STEERING_DURING_SUMMARY"));
        assert_eq!(queues.drain_follow_up(AgentId::Main), ["FOLLOW_UP_LATER"]);
    }

    {
        let events = events.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::AgentStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::AgentEnd { .. }))
                .count(),
            1
        );
        let start = events
            .iter()
            .position(|e| matches!(e, AgentEvent::CompactionStart { .. }))
            .unwrap();
        assert_eq!(
            events[..start]
                .iter()
                .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
                .count(),
            2
        );
        let end = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    AgentEvent::CompactionEnd {
                        summary: Some(_),
                        ..
                    }
                )
            })
            .unwrap();
        let steering = events.iter().position(|e| matches!(e, AgentEvent::MessageEnd { message, .. } if json(message).contains("STEERING_DURING_SUMMARY"))).unwrap();
        assert!(end < steering);
    }
    assert_replay(&agent, &log, &store, &config, 1).await;
}

#[tokio::test]
async fn opt_out_and_low_reported_usage_skip_even_with_large_tool_results() {
    for threshold in [None, Some(0.8)] {
        let dir = TempDir::new().unwrap();
        let input = if threshold.is_none() { 9000 } else { 100 };
        let (config, requests) = recording_config(vec![
            finalized_text_message(&OLD_WORK.repeat(3000)),
            read_batch(&dir, input, &"large evidence line\n".repeat(1800)),
            finalized_text_message("finished"),
        ]);
        let store = ConversationPersistence::new(dir.path().join("sessions"));
        let (mut agent, log, _persistence) = build_test_agent(&store, &config);
        agent
            .prompt("old request".into(), CancellationToken::new())
            .await
            .unwrap();
        let policy = TurnPolicy {
            during_turn_threshold: threshold,
            ..policy()
        };
        drive_turn(
            &mut agent,
            &log,
            &AppendHandoff::default(),
            move || policy,
            TurnStart::Prompt("read".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await
        .unwrap();
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert_batch(&requests[2]);
            assert!(json(&requests[2].messages).contains(OLD_WORK.trim()));
            let result_tokens: u64 = requests[2]
                .messages
                .iter()
                .filter(|m| matches!(m, Message::ToolResult(_)))
                .map(aj_session::compaction::estimate_message_tokens)
                .sum();
            assert!(
                result_tokens > 8000,
                "fixture must exceed the occupancy threshold if eagerly estimated"
            );
        }
        assert_replay(&agent, &log, &store, &config, 0).await;
    }
}

#[tokio::test]
async fn failed_summary_stops_mid_turn_and_overflow_recovery_without_changing_history() {
    for (overflow, silent_overflow) in [(false, false), (false, true), (true, false)] {
        let dir = TempDir::new().unwrap();
        let next = if overflow {
            let mut message = finalized_text_message("");
            message.stop_reason = StopReason::Error;
            message.error = Some(AssistantError::new(
                ErrorCategory::ContextOverflow,
                "prompt too long",
            ));
            message
        } else if silent_overflow {
            // Tool calls drive continuation even when a provider reports Stop.
            // Retained high usage must not misclassify a compaction failure as
            // a conversational overflow and retry it.
            let mut message = read_batch(&dir, 11_000, "evidence");
            message.stop_reason = StopReason::Stop;
            message
        } else {
            read_batch(&dir, 9000, "evidence")
        };
        let (config, requests) = recording_config(vec![
            finalized_text_message(&OLD_WORK.repeat(3000)),
            next,
            finalized_text_message("   "),
        ]);
        let store = ConversationPersistence::new(dir.path().join("sessions"));
        let (mut agent, log, _persistence) = build_test_agent(&store, &config);
        agent
            .prompt("old request".into(), CancellationToken::new())
            .await
            .unwrap();
        let result = drive_turn(
            &mut agent,
            &log,
            &AppendHandoff::default(),
            policy,
            TurnStart::Prompt("read".into()),
            |_| {},
            CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(result, Err(aj_agent::TurnError::Recoverable(_))),
            "{result:?}"
        );
        assert!(result.unwrap_err().to_string().contains("empty summary"));
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3, "failed summary must not retry inference");
            assert_summary_request(&requests[2]);
        }
        assert!(json(&agent.messages()).contains(&OLD_WORK.repeat(3000)));
        assert_replay(&agent, &log, &store, &config, 0).await;
    }
}

#[tokio::test]
async fn cancellation_before_commit_stops_without_checkpoint_or_continuation() {
    for cancel_phase in [CompactionPhase::Summarizing, CompactionPhase::Saving] {
        let dir = TempDir::new().unwrap();
        let (config, requests) = recording_config(vec![
            finalized_text_message(&OLD_WORK.repeat(3000)),
            read_batch(&dir, 9000, "evidence"),
            finalized_text_message("summary must be canceled"),
        ]);
        let store = ConversationPersistence::new(dir.path().join("sessions"));
        let (mut agent, log, _persistence) = build_test_agent(&store, &config);
        agent
            .prompt("old request".into(), CancellationToken::new())
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let reached_boundary = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _listener = agent.subscribe(listener_from_sync({
            let cancel = cancel.clone();
            let reached_boundary = Arc::clone(&reached_boundary);
            move |event| {
                if matches!(
                    event,
                    AgentEvent::CompactionProgress {
                        phase,
                        ..
                    } if *phase == cancel_phase
                ) {
                    reached_boundary.store(true, std::sync::atomic::Ordering::SeqCst);
                    cancel.cancel();
                }
            }
        }));
        let result = drive_turn(
            &mut agent,
            &log,
            &AppendHandoff::default(),
            policy,
            TurnStart::Prompt("read".into()),
            |_| {},
            cancel,
        )
        .await;
        assert!(
            matches!(result, Err(aj_agent::TurnError::Aborted)),
            "{result:?}"
        );
        assert!(
            reached_boundary.load(std::sync::atomic::Ordering::SeqCst),
            "a real compaction plan must reach the cancellation boundary"
        );
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert_summary_request(&requests[2]);
        }
        assert!(json(&agent.messages()).contains(&OLD_WORK.repeat(3000)));
        assert_replay(&agent, &log, &store, &config, 0).await;
    }
}

#[tokio::test]
async fn nothing_to_compact_continues_once_instead_of_looping() {
    let dir = TempDir::new().unwrap();
    let (config, requests) = recording_config(vec![
        read_batch(&dir, 9000, "evidence"),
        finalized_text_message("finished"),
    ]);
    let store = ConversationPersistence::new(dir.path().join("sessions"));
    let (mut agent, log, _persistence) = build_test_agent(&store, &config);
    let events = Arc::new(Mutex::new(Vec::new()));
    let _listener = agent.subscribe(listener_from_sync({
        let events = Arc::clone(&events);
        move |event| events.lock().unwrap().push(event.clone())
    }));
    drive_turn(
        &mut agent,
        &log,
        &AppendHandoff::default(),
        policy,
        TurnStart::Prompt("read".into()),
        |_| {},
        CancellationToken::new(),
    )
    .await
    .unwrap();
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_batch(&requests[1]);
        let events = events.lock().unwrap();
        assert!(!events.iter().any(|e| matches!(
            e,
            AgentEvent::CompactionStart { .. } | AgentEvent::CompactionEnd { .. }
        )));
    }
    assert_replay(&agent, &log, &store, &config, 0).await;
}

#[test]
fn during_turn_policy_requires_main_auto_compact_and_explicit_opt_in() {
    assert!(!aj_conf::Config::default().auto_compact_during_turn);
    for target in [AgentId::Main, AgentId::Sub(1)] {
        for auto_compact in [false, true] {
            for opt_in in [false, true] {
                let config = Arc::new(Mutex::new(aj_conf::Config {
                    auto_compact,
                    auto_compact_during_turn: opt_in,
                    compact_threshold: 0.75,
                    ..Default::default()
                }));
                assert_eq!(
                    turn_policy(target, &config).during_turn_threshold,
                    (target == AgentId::Main && auto_compact && opt_in).then_some(0.75)
                );
            }
        }
    }
}
