//! Store commits are durable thread state, not transcript or model context.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use aj_agent::bus::EventBus;
use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_agent::message::AgentMessage;
use aj_app::chat::{ChatState, reduce};
use aj_app::session::AgentLifecycle;
use aj_models::types::{Message, UserMessage};
use aj_session::{
    AppendHandoff, ConversationLog, ConversationPersistence, PersistenceFence, ThreadFilter,
    persistence_listener, persisting_forwarder, project_suffix,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

fn native_run(source: &str) -> aj_app::session_setup::RunConfigSnapshot {
    use aj_app::test_support::{finalized_text_message, scripted_run_config};
    use aj_models::types::{AssistantContent, StopReason, ToolCall};

    let mut call = finalized_text_message("");
    call.stop_reason = StopReason::ToolUse;
    call.content = vec![AssistantContent::ToolCall(ToolCall {
        is_raw: true,
        id: "native-exec".into(),
        name: "exec".into(),
        arguments: json!(source),
    })];
    let run = scripted_run_config(vec![call, finalized_text_message("done")]);
    let mut run = run.lock().unwrap().clone();
    let model = Arc::make_mut(&mut run.main.model_info);
    model.id = "gpt-6-astra".into();
    model.api = "openai-responses".into();
    run.main.model_key = (model.provider.clone(), model.id.clone());
    run
}

async fn prompt(agent: &mut aj_agent::Agent) {
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        agent.prompt("exercise the store".into(), CancellationToken::new()),
    )
    .await
    .expect("native execution finishes")
    .unwrap();
}

#[tokio::test]
async fn native_error_completion_restores_store_through_session_core_after_compaction() {
    use aj_app::session::{SessionCore, SessionEntry, SessionSpec};

    let dir = tempfile::TempDir::new().unwrap();
    let persistence = ConversationPersistence::new(dir.path().join("sessions"));
    let config = aj_conf::Config {
        code_mode: true,
        ..Default::default()
    };
    let (core, _) = SessionCore::build(
        &config,
        native_run(r#"store("saved", {count: 42}); throw new Error("original-script-error");"#),
        &persistence,
        &SessionSpec::Create {
            entry: SessionEntry::Startup,
            session_env: None,
        },
        None,
    )
    .unwrap();
    let (mut agent, log, subscription) = core.into_test_agent();
    prompt(&mut agent).await;
    let error = agent
        .messages()
        .iter()
        .find_map(|message| match message.as_stored_wire() {
            Some(Message::ToolResult(result)) if result.tool_name == "exec" => Some(result),
            _ => None,
        })
        .expect("native exec result");
    assert!(error.is_error);
    assert!(
        serde_json::to_string(error)
            .unwrap()
            .contains("original-script-error")
    );
    let mut guard = log.lock().await;
    let session_id = guard.session_id().to_owned();
    let tail = guard.head().unwrap().clone();
    assert_eq!(
        guard.linearize(&tail, ThreadFilter::USER).code_mode_store(),
        writes(json!({"saved": {"count": 42}}))
    );
    let checkpoint = guard
        .append_compaction(
            ThreadFilter::USER,
            "compacted context".into(),
            tail,
            vec![],
            100,
            None,
            Default::default(),
        )
        .unwrap();
    let conversation = guard.linearize(&checkpoint.id, ThreadFilter::USER);
    assert!(conversation.entries().len() > conversation.agent_messages().len());
    assert!(
        !serde_json::to_string(&conversation.agent_messages())
            .unwrap()
            .contains("original-script-error"),
        "the checkpoint must actually remove the storing script from model context"
    );
    agent.reseed_transcript(conversation.agent_messages());
    drop(guard);
    agent.reset_code_mode(Default::default()).await.unwrap();
    drop(agent);
    drop(subscription);
    drop(log);

    let (core, _) = SessionCore::build(
        &config,
        native_run(r#"text("restored-count=" + load("saved").count);"#),
        &persistence,
        &SessionSpec::Resume {
            entry: SessionEntry::Startup,
            session_id,
        },
        None,
    )
    .unwrap();
    let (mut agent, _log, _subscription) = core.into_test_agent();
    prompt(&mut agent).await;
    let result = agent
        .messages()
        .iter()
        .rev()
        .find_map(|message| match message.as_stored_wire() {
            Some(Message::ToolResult(result)) if result.tool_name == "exec" => Some(result),
            _ => None,
        })
        .unwrap();
    assert!(!result.is_error);
    assert!(
        serde_json::to_string(result)
            .unwrap()
            .contains("restored-count=42")
    );
    agent.reset_code_mode(Default::default()).await.unwrap();
}

#[tokio::test]
async fn native_cancelled_cell_leaves_no_store_commit_on_reopen() {
    use aj_agent::tool::{TaskKind, TaskStatus};
    use aj_app::session::{SessionCore, SessionEntry, SessionSpec};

    let dir = tempfile::TempDir::new().unwrap();
    let persistence = ConversationPersistence::new(dir.path().join("sessions"));
    let config = aj_conf::Config {
        code_mode: true,
        ..Default::default()
    };
    let (core, _) = SessionCore::build(
        &config,
        native_run(r#"store("cancelled", 42); yield_control(); await new Promise(() => {});"#),
        &persistence,
        &SessionSpec::Create {
            entry: SessionEntry::Startup,
            session_env: None,
        },
        None,
    )
    .unwrap();
    let tasks = core.task_registry.clone();
    let (mut agent, log, subscription) = core.into_test_agent();
    prompt(&mut agent).await;
    let running = tasks.snapshot();
    assert_eq!(running.len(), 1, "the cell yielded after staging a write");
    assert!(matches!(running[0].kind, TaskKind::CodeMode { .. }));
    assert_eq!(running[0].status, TaskStatus::Running);
    tasks.shutdown();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tasks.wait_for_quiescence(),
    )
    .await
    .expect("cancelled cell quiesces");
    agent.reset_code_mode(Default::default()).await.unwrap();
    let session_id = log.lock().await.session_id().to_owned();
    drop(agent);
    drop(subscription);
    drop(log);
    let resumed = ConversationLog::resume(&persistence, &session_id).unwrap();
    assert!(
        resumed
            .linearize(resumed.head().unwrap(), ThreadFilter::USER)
            .code_mode_store()
            .is_empty()
    );
    assert!(
        !project_suffix(&resumed.snapshot(), None, &BTreeSet::new())
            .any(|tagged| matches!(tagged.event, AgentEvent::CodeModeStore { .. }))
    );
}

#[tokio::test]
async fn completed_uncollected_cell_flushes_its_store_before_session_shutdown() {
    use aj_agent::tool::{TaskKind, TaskStatus};
    use aj_app::session::{SessionCore, SessionEntry, SessionSpec};

    let dir = tempfile::TempDir::new().unwrap();
    let persistence = ConversationPersistence::new(dir.path().join("sessions"));
    let (core, _) = SessionCore::build(
        &aj_conf::Config {
            code_mode: true,
            ..Default::default()
        },
        native_run(r#"store("uncollected", 42); yield_control();"#),
        &persistence,
        &SessionSpec::Create {
            entry: SessionEntry::Startup,
            session_env: None,
        },
        None,
    )
    .unwrap();
    let tasks = core.task_registry.clone();
    let (mut agent, log, _persistence) = core.into_test_agent();
    let committed = CancellationToken::new();
    let signal = committed.clone();
    let _observer = agent.subscribe(Arc::new(move |event| {
        if matches!(event, AgentEvent::CodeModeStore { .. }) {
            signal.cancel();
        }
        Box::pin(async { Ok(()) })
    }));
    prompt(&mut agent).await;
    tokio::time::timeout(std::time::Duration::from_secs(20), committed.cancelled())
        .await
        .expect("cell commits without wait");
    let open = tasks.snapshot();
    assert_eq!(open.len(), 1);
    assert!(matches!(open[0].kind, TaskKind::CodeMode { .. }));
    assert_eq!(
        open[0].status,
        TaskStatus::Running,
        "result is still uncollected"
    );
    let guard = log.lock().await;
    let reopened = ConversationLog::resume(&persistence, guard.session_id()).unwrap();
    assert_eq!(
        reopened
            .linearize(reopened.head().unwrap(), ThreadFilter::USER)
            .code_mode_store(),
        writes(json!({"uncollected": 42}))
    );
    drop(guard);
    tasks.shutdown();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tasks.wait_for_quiescence(),
    )
    .await
    .expect("shutdown drains the uncollected cell");
    agent.reset_code_mode(Default::default()).await.unwrap();
}

fn writes(value: Value) -> BTreeMap<String, Value> {
    serde_json::from_value(value).unwrap()
}

fn commit(agent_id: AgentId, value: Value) -> AgentEvent {
    AgentEvent::CodeModeStore {
        agent_id,
        writes: writes(value),
    }
}

fn settings() -> AgentSettings {
    AgentSettings {
        context_window: 0,
        provider: "scripted".into(),
        model_id: "scripted".into(),
        thinking: "off".into(),
        thinking_display: "default".into(),
        speed: "standard".into(),
        verbosity: "default".into(),
    }
}

#[tokio::test]
async fn code_mode_store_flushes_without_messages_and_replays_without_chat_or_usage() {
    let dir = tempfile::TempDir::new().unwrap();
    let persistence = ConversationPersistence::new(dir.path().join("sessions"));
    let log = Arc::new(Mutex::new(ConversationLog::create(&persistence).unwrap()));
    let bus = EventBus::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _subscription = bus.subscribe(persisting_forwarder(
        Arc::clone(&log),
        AppendHandoff::default(),
        tx,
        PersistenceFence::default(),
    ));
    let event = commit(
        AgentId::Main,
        json!({"secret": {"nested": [1, null, true]}}),
    );
    bus.emit(event.clone()).await.unwrap();
    let forwarded = rx.try_recv().unwrap();
    let identity = forwarded.entry.unwrap();
    let guard = log.lock().await;
    // No explicit flush or following tool result can make this test pass.
    let resumed = ConversationLog::resume(&persistence, guard.session_id()).unwrap();
    let conversation = resumed.linearize(resumed.head().unwrap(), ThreadFilter::USER);
    assert!(conversation.agent_messages().is_empty());
    assert!(conversation.messages().is_empty());
    assert_eq!(
        conversation.code_mode_store(),
        writes(json!({"secret": {"nested": [1, null, true]}}))
    );
    let replayed: Vec<_> = project_suffix(&resumed.snapshot(), None, &BTreeSet::new()).collect();
    assert_eq!(
        replayed.len(),
        1,
        "state must not synthesize chat or usage events"
    );
    assert_eq!(replayed[0].entry, Some(identity));
    assert_eq!(
        serde_json::to_value(&replayed[0].event).unwrap(),
        serde_json::to_value(&event).unwrap()
    );
    let mut chat = ChatState::new(settings());
    let mut lifecycle = AgentLifecycle::default();
    assert!(!reduce(&mut chat, &mut lifecycle, event, None).0);
    assert!(chat.transcript(AgentId::Main).unwrap().entries().is_empty());
}

#[tokio::test]
async fn code_mode_store_follows_branch_and_thread_ancestry_across_compaction() {
    let dir = tempfile::TempDir::new().unwrap();
    let persistence = ConversationPersistence::new(dir.path().join("sessions"));
    let log = Arc::new(Mutex::new(ConversationLog::create(&persistence).unwrap()));
    let bus = EventBus::new();
    let _subscription = bus.subscribe(persistence_listener(Arc::clone(&log)));
    bus.emit(commit(AgentId::Main, json!({"common": 1, "value": "base"})))
        .await
        .unwrap();
    let fork = log.lock().await.head().unwrap().clone();
    bus.emit(commit(AgentId::Sub(1), json!({"orphan": true})))
        .await
        .expect_err("unspawned child cannot write");
    bus.emit(AgentEvent::SubAgentStart {
        parent: AgentId::Main,
        child: AgentId::Sub(1),
        task: "child".into(),
        tool_name: "agent".into(),
        background: false,
        settings: settings(),
    })
    .await
    .unwrap();
    bus.emit(commit(AgentId::Sub(1), json!({"value": "child"})))
        .await
        .unwrap();
    assert_eq!(log.lock().await.head(), Some(&fork));
    bus.emit(commit(
        AgentId::Main,
        json!({"value": "abandoned", "sibling_only": true}),
    ))
    .await
    .unwrap();
    let sibling = log.lock().await.head().unwrap().clone();
    log.lock().await.set_head(fork.clone()).unwrap();
    bus.emit(AgentEvent::MessageEnd {
        agent_id: AgentId::Main,
        message: AgentMessage::wire(Message::User(UserMessage::text("retained prompt"))),
    })
    .await
    .unwrap();
    let mut guard = log.lock().await;
    let tail = guard.head().unwrap().clone();
    guard
        .append_compaction(
            ThreadFilter::USER,
            "summary".into(),
            tail,
            vec![],
            100,
            None,
            Default::default(),
        )
        .unwrap();
    drop(guard);
    bus.emit(commit(AgentId::Main, json!({"value": null})))
        .await
        .unwrap();
    let guard = log.lock().await;
    let resumed = ConversationLog::resume(&persistence, guard.session_id()).unwrap();
    let current = resumed.linearize(resumed.head().unwrap(), ThreadFilter::USER);
    assert_eq!(
        current.agent_messages().len(),
        2,
        "summary plus retained prompt"
    );
    assert_eq!(
        current.code_mode_store(),
        writes(json!({"common": 1, "value": null}))
    );
    assert_eq!(
        resumed
            .linearize(&sibling, ThreadFilter::USER)
            .code_mode_store(),
        writes(json!({"common": 1, "value": "abandoned", "sibling_only": true}))
    );
    assert_eq!(
        resumed
            .linearize(&fork, ThreadFilter::USER)
            .code_mode_store(),
        writes(json!({"common": 1, "value": "base"}))
    );
    let child = ThreadFilter::subagent(1);
    assert_eq!(
        resumed
            .linearize(&resumed.latest_leaf(child).unwrap(), child)
            .code_mode_store(),
        writes(json!({"value": "child"}))
    );
}
