//! Nested audit messages cross persistence, remote replay, and the frontend,
//! but never the model-context boundary.

use std::collections::BTreeSet;
use std::sync::Arc;

use aj_agent::bus::EventBus;
use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_agent::message::{AgentMessage, AgentMessageKind};
use aj_agent::tool::ToolDetails;
use aj_app::chat::{ChatState, EntryKind, ToolStatus, reduce};
use aj_app::session::AgentLifecycle;
use aj_models::types::{
    AssistantContent, AssistantMessage, Message, ToolCall, ToolResultMessage, UserMessage,
};
use aj_session::{
    ConversationLog, ConversationPersistence, ThreadFilter, persistence_listener, project_suffix,
};
use aj_wire::{DecodedFrame, DurableEvent, Frame};
use serde_json::json;
use tokio::sync::Mutex;

#[tokio::test]
async fn tool_activity_persists_replays_and_stays_out_of_model_context() {
    let dir = tempfile::TempDir::new().unwrap();
    let persistence = ConversationPersistence::new(dir.path().join("sessions"));
    let log = Arc::new(Mutex::new(ConversationLog::create(&persistence).unwrap()));
    let bus = EventBus::new();
    let _subscription = bus.subscribe(persistence_listener(Arc::clone(&log)));
    let body = "nested output, not model input\n".repeat(30);
    let details = ToolDetails::Text {
        summary: "nested read".into(),
        body: body.clone(),
    };
    let mut result = ToolResultMessage::text("nested", "read_file", &body, false);
    result.details = Some(serde_json::to_value(&details).unwrap());
    let call = |id: &str| {
        AgentMessage::tool_activity(
            "cell".into(),
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    is_raw: false,
                    id: id.into(),
                    name: "read_file".into(),
                    arguments: json!({"path":"nested.txt"}),
                })],
                ..AssistantMessage::empty()
            }),
        )
    };
    let call_message = call("nested");
    let call_entry = call_message.id().to_string();
    let audit_result =
        AgentMessage::tool_activity("cell".into(), Message::ToolResult(result.clone()));
    let events = vec![
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::User(UserMessage::text("prompt"))),
        },
        AgentEvent::ToolExecutionStart {
            agent_id: AgentId::Main,
            call_id: "nested".into(),
            tool: "read_file".into(),
            args: json!({"path":"nested.txt"}),
        },
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: call_message,
        },
        AgentEvent::ToolExecutionEnd {
            agent_id: AgentId::Main,
            call_id: "nested".into(),
            tool: "read_file".into(),
            result: details,
            content: result.content.into(),
            is_error: false,
        },
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: audit_result,
        },
        AgentEvent::ToolExecutionStart {
            agent_id: AgentId::Main,
            call_id: "unfinished".into(),
            tool: "read_file".into(),
            args: json!({"path":"nested.txt"}),
        },
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: call("unfinished"),
        },
    ];
    let settings = AgentSettings {
        context_window: 0,
        provider: "scripted".into(),
        model_id: "scripted".into(),
        thinking: "off".into(),
        thinking_display: "default".into(),
        speed: "standard".into(),
        verbosity: "default".into(),
    };
    let mut live = ChatState::new(settings.clone());
    let mut lifecycle = AgentLifecycle::default();
    for event in events {
        bus.emit(event.clone()).await.unwrap();
        let _ = reduce(&mut live, &mut lifecycle, event, None);
    }
    let guard = log.lock().await;
    let raw = std::fs::read_to_string(guard.path()).unwrap();
    assert!(
        raw.contains("body_ref"),
        "fixture must exercise detail hydration"
    );
    let mut resumed = ConversationLog::resume(&persistence, guard.session_id()).unwrap();
    drop(guard);
    let head = resumed.latest_leaf(ThreadFilter::USER).unwrap();
    let conversation = resumed.linearize(&head, ThreadFilter::USER);
    assert_eq!(conversation.agent_messages().len(), 4);
    assert_eq!(conversation.messages().len(), 1);
    assert!(matches!(&conversation.messages()[0], Message::User(_)));
    let model_messages =
        aj_agent::projection::transcript_to_messages(&conversation.agent_messages());
    assert_eq!(
        serde_json::to_value(model_messages).unwrap(),
        serde_json::to_value(conversation.messages()).unwrap(),
        "the agent's inference projection of the resume seed excludes audit data"
    );
    for audit in conversation.agent_messages().iter().skip(1) {
        assert!(audit.as_stored_wire().is_none());
        assert!(audit.to_projected_wire().is_none());
    }

    let mut replayed = ChatState::new(settings);
    let mut lifecycle = AgentLifecycle::default();
    let mut audit_count = 0;
    for tagged in project_suffix(&resumed.snapshot(), None, &BTreeSet::new()) {
        assert!(!matches!(tagged.event, AgentEvent::UsageUpdate { .. }));
        let frame = Frame::Event {
            session: "session".into(),
            epoch: "epoch".into(),
            durability: tagged.entry.map(|entry| DurableEvent {
                seq: entry.seq,
                entry_id: entry.id,
                branch_settings: None,
            }),
            event: tagged.event.into(),
        };
        let wire = serde_json::to_string(&frame).unwrap();
        let DecodedFrame::Known(decoded) = serde_json::from_str(&wire).unwrap() else {
            panic!("known frame")
        };
        let Frame::Event {
            event, durability, ..
        } = decoded.value()
        else {
            panic!("event frame")
        };
        let event = event.known().expect("audit is a known event");
        if let AgentEvent::MessageEnd { message, .. } = event
            && let AgentMessageKind::ToolActivity(activity) = &message.kind
        {
            audit_count += 1;
            assert_eq!(activity.cell_id, "cell");
            assert_eq!(message.id(), durability.as_ref().unwrap().entry_id);
            assert!(message.to_projected_wire().is_none());
        }
        let _ = reduce(
            &mut replayed,
            &mut lifecycle,
            event.clone(),
            durability.as_ref(),
        );
    }
    assert_eq!(audit_count, 3);
    let tool_rows = |state: &ChatState| {
        state
            .transcript(AgentId::Main)
            .unwrap()
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Tool(tool) => Some((
                    tool.call_id.clone(),
                    tool.args.clone(),
                    tool.status,
                    serde_json::to_value(&tool.details).unwrap(),
                    serde_json::to_value(&tool.content).unwrap(),
                )),
                EntryKind::User(user) => {
                    assert_eq!(user.joined_text(), "prompt");
                    None
                }
                other => panic!("audit created a non-tool row: {other:?}"),
            })
            .collect::<Vec<_>>()
    };
    let rows = tool_rows(&replayed);
    assert_eq!(rows, tool_rows(&live));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].2, ToolStatus::Done { is_error: false });
    assert_eq!(rows[0].3["body"], body);
    assert_eq!(rows[1].2, ToolStatus::Running);

    resumed
        .append_compaction(
            ThreadFilter::USER,
            "summary".into(),
            call_entry,
            vec![],
            100,
            None,
            Default::default(),
        )
        .unwrap();
    let resumed = ConversationLog::resume(&persistence, resumed.session_id()).unwrap();
    let head = resumed.latest_leaf(ThreadFilter::USER).unwrap();
    let conversation = resumed.linearize(&head, ThreadFilter::USER);
    let messages = aj_agent::projection::transcript_to_messages(&conversation.agent_messages());
    assert_eq!(
        messages.len(),
        1,
        "retained audits stay out after compaction and resume"
    );
    assert!(!serde_json::to_string(&messages).unwrap().contains("nested"));
}
