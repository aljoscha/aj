use std::collections::BTreeSet;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_wire::{
    CancelRequest, CompactRequest, CreateSessionRequest, Cursor, DecodedAgentEvent, DecodedFrame,
    ErrorResponse, Frame, HeadRequest, Hello, ModelSelection, PromptInput, PromptRequest,
    QueueCounts, QueueOperation, QueueOutcome, QueueRequest, QueueState, SessionCreated,
    SessionList, SessionSettings, SessionSummary, SessionTree, SettingsRequest, SteerRequest,
    TaskDetails, TaskTable, VmList,
};
use serde_json::{Value, json};

const EVENT_TYPES: &[&str] = &[
    "agent_start",
    "agent_end",
    "turn_start",
    "turn_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    "sub_agent_start",
    "sub_agent_end",
    "task_start",
    "task_output",
    "task_end",
    "notice",
    "warning",
    "error",
    "stream_retry",
    "usage_update",
    "compaction_start",
    "compaction_progress",
    "compaction_end",
    "queue_update",
];

const FRAME_KINDS: &[&str] = &[
    "event",
    "state",
    "caught_up",
    "list",
    "reset",
    "heartbeat",
    "vms",
];

fn fixture(name: &str) -> Value {
    serde_json::from_str(match name {
        "agent_events" => include_str!("fixtures/agent-events.json"),
        "frames" => include_str!("fixtures/frames.json"),
        "models" => include_str!("fixtures/models.json"),
        _ => panic!("unknown fixture {name}"),
    })
    .expect("fixture is valid JSON")
}

#[test]
fn command_request_shapes_are_pinned() {
    let prompt: PromptRequest = serde_json::from_value(json!({
        "text": "hello",
        "future_field": true
    }))
    .unwrap();
    assert_eq!(prompt.agent, None);
    assert!(matches!(prompt.input, PromptInput::Text { ref text } if text == "hello"));

    let blocks: PromptRequest = serde_json::from_value(json!({
        "agent": {"sub": 2},
        "content": [{"type":"text","text":"structured"}]
    }))
    .unwrap();
    assert_eq!(blocks.agent, Some(AgentId::Sub(2)));
    assert!(matches!(blocks.input, PromptInput::Content { ref content } if content.len() == 1));

    assert_eq!(
        serde_json::to_value(SteerRequest {
            text: "now".into(),
            agent: None,
        })
        .unwrap(),
        json!({"text":"now"})
    );
    assert_eq!(
        serde_json::to_value(CancelRequest { agent: None }).unwrap(),
        json!({})
    );
    assert_eq!(
        serde_json::to_value(QueueRequest {
            op: QueueOperation::Remove,
            agent: Some(AgentId::Main),
        })
        .unwrap(),
        json!({"op":"remove","agent":"main"})
    );
    assert_eq!(
        serde_json::to_value(QueueOutcome {
            text: Some("bring me back".into()),
        })
        .unwrap(),
        json!({"text":"bring me back"})
    );
    assert_eq!(
        serde_json::to_value(CompactRequest {
            instructions: Some("keep protocol notes".into()),
        })
        .unwrap(),
        json!({"instructions":"keep protocol notes"})
    );
    assert_eq!(
        serde_json::to_value(HeadRequest::entry("entry-7")).unwrap(),
        json!({"entry":"entry-7"})
    );
    // The branch-from-a-message shape, which the host resolves to the
    // entry's parent (spec 6.6).
    assert_eq!(
        serde_json::to_value(HeadRequest::before("entry-7")).unwrap(),
        json!({"before":"entry-7"})
    );
    // Both shapes decode, and an unknown field is ignored as spec 6.10 asks.
    assert_eq!(
        serde_json::from_value::<HeadRequest>(json!({"before":"entry-7","junk":1})).unwrap(),
        HeadRequest::before("entry-7"),
    );
}

fn selected_model() -> ModelSelection {
    ModelSelection {
        api: "openai".into(),
        url: Some("https://models.example/v1".into()),
        name: "gpt-remote".into(),
    }
}

fn session_settings() -> SessionSettings {
    SessionSettings {
        model: Some(selected_model()),
        thinking: Some("high".into()),
        thinking_display: Some("detailed".into()),
        speed: Some("standard".into()),
        verbosity: Some("medium".into()),
    }
}

#[test]
fn settings_use_the_cli_selection_triple_and_create_round_trips() {
    let request = SettingsRequest {
        agent: None,
        change: session_settings(),
    };
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({
            "model": {
                "api":"openai",
                "url":"https://models.example/v1",
                "name":"gpt-remote"
            },
            "thinking":"high",
            "thinking_display":"detailed",
            "speed":"standard",
            "verbosity":"medium"
        })
    );

    let create = CreateSessionRequest {
        settings: Some(session_settings()),
        prompt: Some(PromptInput::Text {
            text: "start here".into(),
        }),
    };
    let encoded = serde_json::to_value(&create).unwrap();
    assert_eq!(encoded["prompt"], json!({"text":"start here"}));
    assert_eq!(
        serde_json::from_value::<CreateSessionRequest>(encoded)
            .unwrap()
            .settings,
        create.settings,
    );
    assert_eq!(
        serde_json::to_value(SessionCreated {
            id: "session-1".into()
        })
        .unwrap(),
        json!({"id":"session-1"})
    );
}

#[test]
fn state_and_task_detail_models_pin_the_new_phase_two_fields() {
    let settings = AgentSettings {
        provider: "openai".into(),
        model_id: "gpt-remote".into(),
        thinking: "high".into(),
        thinking_display: "detailed".into(),
        speed: "standard".into(),
        verbosity: "medium".into(),
    };
    let frame = Frame::State {
        session: "session-1".into(),
        epoch: "epoch-1".into(),
        working: false,
        settings,
        last_seq: 4,
    };
    assert_eq!(
        serde_json::to_value(frame).unwrap()["settings"]["thinking_display"],
        "detailed"
    );

    let detail: TaskDetails = serde_json::from_value(json!({
        "id": 3,
        "status": "running",
        "stdout_tail": "out",
        "stderr_tail": "err",
        "stdout_total_bytes": 20,
        "stderr_total_bytes": 4,
        "report": null,
        "future_field": "ignored"
    }))
    .unwrap();
    assert_eq!(detail.id, 3);
    assert_eq!(detail.stdout_total_bytes + detail.stderr_total_bytes, 24);
}

#[test]
fn agent_settings_without_thinking_display_remain_readable() {
    let settings: AgentSettings = serde_json::from_value(json!({
        "provider": "scripted",
        "model_id": "scripted-model",
        "thinking": "off",
        "speed": "standard",
        "verbosity": "default"
    }))
    .unwrap();
    assert!(settings.thinking_display.is_empty());
    assert!(
        serde_json::to_value(settings)
            .unwrap()
            .get("thinking_display")
            .is_none(),
        "legacy snapshots re-serialize without fabricating a value",
    );
}

#[test]
fn every_agent_event_has_a_pinned_round_trip_fixture() {
    let fixtures = fixture("agent_events")
        .as_array()
        .expect("agent event fixture is an array")
        .clone();
    let actual_types = fixtures
        .iter()
        .map(|value| {
            value["type"]
                .as_str()
                .expect("event type is a string")
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    let expected_types = EVENT_TYPES
        .iter()
        .map(|value| (*value).to_string())
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_types, expected_types);
    assert_eq!(fixtures.len(), EVENT_TYPES.len());

    for expected in fixtures {
        let encoded = serde_json::to_string(&expected).unwrap();
        let event: AgentEvent = serde_json::from_str(&encoded)
            .unwrap_or_else(|error| panic!("failed to decode {expected}: {error}"));
        assert_eq!(agent_event_type(&event), expected["type"]);
        let actual = serde_json::to_value(event).expect("event re-serializes");
        assert_eq!(actual, expected);

        let decoded: DecodedAgentEvent = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(decoded, DecodedAgentEvent::Known(_)));
        assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
    }
}

#[test]
fn known_agent_event_decode_ignores_extra_fields() {
    let known: AgentEvent = serde_json::from_value(json!({
        "type": "notice",
        "agent_id": "main",
        "text": "hello",
        "added_later": {"anything": true}
    }))
    .expect("known event with an extra field decodes");
    assert!(matches!(known, AgentEvent::Notice { .. }));
}

#[test]
fn unknown_agent_event_is_retained_by_the_wire_wrapper() {
    let expected = json!({
        "type": "future_event",
        "agent_id": "main",
        "future_payload": [1, 2, 3]
    });
    assert!(serde_json::from_value::<AgentEvent>(expected.clone()).is_err());

    let decoded: DecodedAgentEvent =
        serde_json::from_value(expected.clone()).expect("unknown event type decodes");
    let DecodedAgentEvent::Unknown { event_type, raw } = &decoded else {
        panic!("expected unknown event wrapper");
    };
    assert_eq!(event_type, "future_event");
    assert_eq!(serde_json::from_str::<Value>(raw.get()).unwrap(), expected);
    assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
}

#[test]
fn malformed_known_agent_event_is_not_downgraded_to_unknown() {
    let malformed = json!({
        "type": "notice",
        "agent_id": "main"
    });
    assert!(serde_json::from_value::<DecodedAgentEvent>(malformed).is_err());

    for missing_or_invalid_tag in [json!({"text": "hi"}), json!({"type": 1})] {
        assert!(serde_json::from_value::<DecodedAgentEvent>(missing_or_invalid_tag).is_err());
    }
}

#[test]
fn decoded_agent_event_rejects_duplicate_known_fields() {
    for duplicate in [
        r#"{"type":"notice","agent_id":"main","text":"first","text":"last"}"#,
        r#"{"type":"notice","type":"notice","agent_id":"main","text":"hello"}"#,
    ] {
        assert!(serde_json::from_str::<AgentEvent>(duplicate).is_err());
        assert!(serde_json::from_str::<DecodedAgentEvent>(duplicate).is_err());
    }
}

#[test]
fn decoded_agent_event_forwards_known_additions_exactly() {
    assert_decoded_round_trip::<DecodedAgentEvent>(
        r#"{"type":"notice","agent_id":"main","text":"hello","added_later":{"n":18446744073709551616}}"#,
    );
}

#[test]
fn decoded_agent_event_forwards_unknown_numbers_exactly() {
    assert_decoded_round_trip::<DecodedAgentEvent>(
        r#"{"type":"future_event","integer":18446744073709551616}"#,
    );
    assert_decoded_round_trip::<DecodedAgentEvent>(r#"{"type":"future_event","float":1e400}"#);
}

#[test]
fn locally_constructed_known_values_serialize_from_typed_data() {
    let event_json = json!({
        "type": "notice",
        "agent_id": "main",
        "text": "hello"
    });
    let event = serde_json::from_value::<AgentEvent>(event_json.clone()).unwrap();
    let decoded_event = DecodedAgentEvent::from(event);
    let DecodedAgentEvent::Known(event) = &decoded_event else {
        panic!("expected known event");
    };
    assert!(event.raw_json().is_none());
    assert_eq!(serde_json::to_value(decoded_event).unwrap(), event_json);

    let decoded_frame = DecodedFrame::try_from(Frame::Heartbeat).unwrap();
    let DecodedFrame::Known(frame) = &decoded_frame else {
        panic!("expected known frame");
    };
    assert!(frame.raw_json().is_none());
    assert_eq!(
        serde_json::to_value(decoded_frame).unwrap(),
        json!({"kind": "heartbeat"})
    );
}

#[test]
fn every_frame_kind_has_a_pinned_round_trip_fixture() {
    let fixtures = fixture("frames")
        .as_array()
        .expect("frame fixture is an array")
        .clone();
    let mut actual_kinds = BTreeSet::new();

    for expected in fixtures {
        let frame: DecodedFrame = serde_json::from_value(expected.clone())
            .unwrap_or_else(|error| panic!("failed to decode {expected}: {error}"));
        let DecodedFrame::Known(known) = &frame else {
            panic!("expected known frame");
        };
        actual_kinds.insert(frame_kind(known.value()));
        let actual = serde_json::to_value(frame).expect("frame re-serializes");
        assert_eq!(actual, expected);
    }

    assert_eq!(
        actual_kinds,
        FRAME_KINDS.iter().copied().collect::<BTreeSet<_>>()
    );
}

#[test]
fn event_frame_decode_backfills_the_log_entry_id() {
    let value = fixture("frames")[0].clone();
    let frame: DecodedFrame = serde_json::from_value(value).expect("event frame decodes");
    let DecodedFrame::Known(frame) = frame else {
        panic!("expected known frame");
    };
    let Frame::Event {
        durability: Some(durability),
        event,
        ..
    } = frame.value()
    else {
        panic!("expected durable MessageEnd event frame");
    };
    let DecodedAgentEvent::Known(event) = event else {
        panic!("expected known event");
    };
    let AgentEvent::MessageEnd { message, .. } = event.value() else {
        panic!("expected MessageEnd event");
    };
    assert_eq!(message.id(), durability.entry_id);
}

#[test]
fn frame_decode_is_forward_compatible() {
    let state: DecodedFrame = serde_json::from_value(json!({
        "kind": "state",
        "session": "session-1",
        "epoch": "epoch-1",
        "working": false,
        "settings": {
            "provider": "scripted",
            "model_id": "scripted-model",
            "thinking": "off",
            "speed": "standard",
            "verbosity": "default"
        },
        "last_seq": 7,
        "added_later": true
    }))
    .expect("known frame with an extra field decodes");
    let DecodedFrame::Known(state) = state else {
        panic!("expected known state frame");
    };
    assert!(matches!(state.value(), Frame::State { .. }));

    let expected = json!({
        "kind": "future_frame",
        "session": "session-1",
        "payload": {"anything": true}
    });
    assert!(serde_json::from_value::<Frame>(expected.clone()).is_err());
    let unknown: DecodedFrame =
        serde_json::from_value(expected.clone()).expect("unknown frame kind decodes");
    let DecodedFrame::Unknown { kind, raw } = &unknown else {
        panic!("expected unknown frame wrapper");
    };
    assert_eq!(kind, "future_frame");
    assert_eq!(serde_json::from_str::<Value>(raw.get()).unwrap(), expected);
    assert_eq!(serde_json::to_value(unknown).unwrap(), expected);
}

#[test]
fn decoded_frame_rejects_duplicate_known_fields() {
    for duplicate in [
        r#"{"kind":"reset","session":"first","session":"last"}"#,
        r#"{"kind":"heartbeat","kind":"heartbeat"}"#,
    ] {
        assert!(serde_json::from_str::<Frame>(duplicate).is_err());
        assert!(serde_json::from_str::<DecodedFrame>(duplicate).is_err());
    }
}

#[test]
fn decoded_frame_forwards_known_additions_exactly() {
    assert_decoded_round_trip::<DecodedFrame>(
        r#"{"kind":"state","session":"session-1","epoch":"epoch-1","working":false,"settings":{"provider":"scripted","model_id":"scripted-model","thinking":"off","speed":"standard","verbosity":"default","future_setting":true},"last_seq":7,"added_later":{"n":18446744073709551616}}"#,
    );
    assert_decoded_round_trip::<DecodedFrame>(
        r#"{"kind":"event","session":"session-1","epoch":"epoch-1","event":{"type":"notice","agent_id":"main","text":"hello","future_event_field":true},"future_frame_field":true}"#,
    );
}

#[test]
fn decoded_frame_forwards_unknown_numbers_exactly() {
    assert_decoded_round_trip::<DecodedFrame>(
        r#"{"kind":"future_frame","integer":18446744073709551616}"#,
    );
    assert_decoded_round_trip::<DecodedFrame>(r#"{"kind":"future_frame","float":1e400}"#);
}

#[test]
fn decoded_frame_rewrites_known_session_without_losing_additions() {
    let input = r#"{"kind":"state","session":"old","epoch":"e","working":false,"settings":{"provider":"p","model_id":"m","thinking":"off","speed":"standard","verbosity":"default","future_setting":{"huge":1e400}},"last_seq":7,"future_number":18446744073709551616}"#;
    let expected = r#"{"kind":"state","session":"gateway:old","epoch":"e","working":false,"settings":{"provider":"p","model_id":"m","thinking":"off","speed":"standard","verbosity":"default","future_setting":{"huge":1e400}},"last_seq":7,"future_number":18446744073709551616}"#;
    let mut frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert!(frame.rewrite_session("gateway:old").unwrap());

    assert_eq!(serde_json::to_string(&frame).unwrap(), expected);
}

#[test]
fn decoded_frame_rewrites_unknown_session_without_parsing_payloads() {
    let input = r#"{"kind":"future_frame","session":"old","future_number":1e400}"#;
    let expected = r#"{"kind":"future_frame","session":"gateway:old","future_number":1e400}"#;
    let mut frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert!(frame.rewrite_session("gateway:old").unwrap());

    assert_eq!(serde_json::to_string(&frame).unwrap(), expected);
}

#[test]
fn durable_event_metadata_is_all_or_nothing() {
    let missing_entry_id = json!({
        "kind": "event",
        "session": "session-1",
        "epoch": "epoch-1",
        "seq": 1,
        "event": {"type": "notice", "agent_id": "main", "text": "hi"}
    });
    assert!(serde_json::from_value::<Frame>(missing_entry_id).is_err());

    let missing_seq = json!({
        "kind": "event",
        "session": "session-1",
        "epoch": "epoch-1",
        "entry_id": "entry-1",
        "event": {"type": "notice", "agent_id": "main", "text": "hi"}
    });
    assert!(serde_json::from_value::<Frame>(missing_seq).is_err());

    let empty_entry_id = json!({
        "kind": "event",
        "session": "session-1",
        "epoch": "epoch-1",
        "seq": 1,
        "entry_id": "",
        "event": {"type": "notice", "agent_id": "main", "text": "hi"}
    });
    assert!(serde_json::from_value::<Frame>(empty_entry_id).is_err());
}

#[test]
fn message_end_requires_non_null_durable_metadata() {
    let event = r#"{"type":"message_end","agent_id":"main","message":{"role":"user","content":[{"type":"text","text":"hello"}],"timestamp":10}}"#;
    let absent =
        format!(r#"{{"kind":"event","session":"session-1","epoch":"epoch-1","event":{event}}}"#);
    let null = format!(
        r#"{{"kind":"event","session":"session-1","epoch":"epoch-1","seq":null,"entry_id":null,"event":{event}}}"#
    );

    assert!(serde_json::from_str::<DecodedFrame>(&absent).is_err());
    assert!(serde_json::from_str::<DecodedFrame>(&null).is_err());
}

#[test]
fn locally_constructed_message_end_requires_and_backfills_durability() {
    let event: AgentEvent = serde_json::from_str(
        r#"{"type":"message_end","agent_id":"main","message":{"role":"user","content":[{"type":"text","text":"hello"}],"timestamp":10}}"#,
    )
    .unwrap();
    let frame = Frame::Event {
        session: "session-1".into(),
        epoch: "epoch-1".into(),
        durability: None,
        event: DecodedAgentEvent::from(event),
    };
    assert!(serde_json::to_string(&frame).is_err());
    assert!(DecodedFrame::try_from(frame).is_err());

    let event: AgentEvent = serde_json::from_str(
        r#"{"type":"message_end","agent_id":"main","message":{"role":"user","content":[{"type":"text","text":"hello"}],"timestamp":10}}"#,
    )
    .unwrap();
    let frame = Frame::Event {
        session: "session-1".into(),
        epoch: "epoch-1".into(),
        durability: Some(aj_wire::DurableEvent {
            seq: 1,
            entry_id: "entry-1".into(),
        }),
        event: DecodedAgentEvent::from(event),
    };
    assert!(serde_json::to_string(&frame).is_err());

    let decoded = DecodedFrame::try_from(frame).expect("local frame is prepared");
    let DecodedFrame::Known(frame) = &decoded else {
        panic!("expected known frame");
    };
    let Frame::Event { event, .. } = frame.value() else {
        panic!("expected event frame");
    };
    let DecodedAgentEvent::Known(event) = event else {
        panic!("expected known event");
    };
    let AgentEvent::MessageEnd { message, .. } = event.value() else {
        panic!("expected message end");
    };
    assert_eq!(message.id(), "entry-1");
    assert!(serde_json::to_string(&decoded).is_ok());
}

#[test]
fn explicit_null_event_metadata_is_rejected() {
    let frame = r#"{"kind":"event","session":"session-1","epoch":"epoch-1","seq":null,"entry_id":null,"event":{"type":"notice","agent_id":"main","text":"hello"}}"#;
    assert!(serde_json::from_str::<DecodedFrame>(frame).is_err());
}

#[test]
fn durable_unknown_event_keeps_its_envelope() {
    let expected = json!({
        "kind": "event",
        "session": "session-1",
        "epoch": "epoch-1",
        "seq": 9,
        "entry_id": "entry-9",
        "event": {
            "type": "future_event",
            "payload": "retained"
        }
    });

    let decoded: DecodedFrame = serde_json::from_value(expected.clone()).unwrap();
    let DecodedFrame::Known(frame) = &decoded else {
        panic!("expected known event frame");
    };
    let Frame::Event {
        durability: Some(durability),
        event,
        ..
    } = frame.value()
    else {
        panic!("expected durable event with unknown nested event");
    };
    let DecodedAgentEvent::Unknown { event_type, raw } = event else {
        panic!("expected unknown nested event");
    };
    assert_eq!(durability.seq, 9);
    assert_eq!(durability.entry_id, "entry-9");
    assert_eq!(event_type, "future_event");
    assert_eq!(
        serde_json::from_str::<Value>(raw.get()).unwrap(),
        expected["event"]
    );
    assert_eq!(serde_json::to_value(decoded).unwrap(), expected);
}

#[test]
fn malformed_known_frame_is_not_downgraded_to_unknown() {
    let malformed = json!({
        "kind": "state",
        "session": "session-1",
        "epoch": "epoch-1",
        "working": false,
        "last_seq": 0
    });
    assert!(serde_json::from_value::<DecodedFrame>(malformed).is_err());

    for missing_or_invalid_tag in [json!({"payload": true}), json!({"kind": 1})] {
        assert!(serde_json::from_value::<DecodedFrame>(missing_or_invalid_tag).is_err());
    }
}

#[test]
fn non_event_wire_models_have_pinned_round_trip_fixtures() {
    let fixtures = fixture("models");

    assert_round_trip::<Hello>(&fixtures["hello"]);
    assert_round_trip::<SessionList>(&fixtures["session_list"]);
    assert_round_trip::<TaskTable>(&fixtures["tasks"]);
    assert_round_trip::<QueueState>(&fixtures["queue"]);
    assert_round_trip::<SessionTree>(&fixtures["tree"]);
    assert_round_trip::<VmList>(&fixtures["vms"]);
    assert_round_trip::<ErrorResponse>(&fixtures["error"]);
}

/// A directory row carries `last_seq` only when the session is live (spec
/// 6.8). A cold row omits the key entirely rather than reporting a zero a
/// client would read as "nothing here", and its activity stamp, which is the
/// signal it does carry, is required on every row.
#[test]
fn a_directory_row_carries_a_position_only_while_live() {
    let list: SessionList = serde_json::from_value(fixture("models")["session_list"].clone())
        .expect("the pinned session list decodes");
    let [live, cold] = &list.sessions[..] else {
        panic!("the fixture pins one live row and one cold one");
    };
    assert_eq!((live.live, live.last_seq), (true, Some(7)));
    assert_eq!((cold.live, cold.last_seq), (false, None));

    let encoded = serde_json::to_value(&list).expect("the list re-serializes");
    assert!(
        encoded["sessions"][1].get("last_seq").is_none(),
        "a cold row omits the key: {}",
        encoded["sessions"][1],
    );

    let mut stampless = fixture("models")["session_list"]["sessions"][1].clone();
    stampless
        .as_object_mut()
        .expect("a row is an object")
        .remove("last_activity");
    assert!(
        serde_json::from_value::<SessionSummary>(stampless).is_err(),
        "the activity stamp is required on every row",
    );
}

/// A row's tag and host are display metadata a row may simply not have: an
/// untagged session, and a plain host's rows, which are all its own (spec
/// 6.8). Absent is a key that is not there rather than an empty string, in
/// both directions, so a client can tell "no label" from "a label that is
/// blank" and an older peer's row still decodes.
#[test]
fn a_rows_tag_and_host_are_absent_rather_than_empty() {
    let list: SessionList = serde_json::from_value(fixture("models")["session_list"].clone())
        .expect("the pinned session list decodes");
    let [gateway, plain] = &list.sessions[..] else {
        panic!("the fixture pins one row with both fields and one with neither");
    };
    assert_eq!(gateway.tag.as_deref(), Some("fix-auth"));
    assert_eq!(gateway.host.as_deref(), Some("workstation"));
    assert_eq!((plain.tag.as_deref(), plain.host.as_deref()), (None, None));

    let row = SessionSummary {
        id: "session-2".to_string(),
        live: false,
        working: false,
        queued: QueueCounts::default(),
        tasks: 0,
        last_seq: None,
        last_activity: gateway.last_activity,
        tag: None,
        host: None,
        unreachable: false,
    };
    let encoded = serde_json::to_value(&row).expect("the row serializes");
    assert!(
        encoded.get("tag").is_none() && encoded.get("host").is_none(),
        "an untagged row on a plain host emits neither key: {encoded}",
    );
    assert_eq!(
        serde_json::from_value::<SessionSummary>(encoded).expect("it decodes again"),
        row,
        "and a row lacking both keys reads back as carrying neither",
    );

    let labelled = SessionSummary {
        tag: Some("fix-auth".to_string()),
        host: Some("workstation".to_string()),
        ..row
    };
    let encoded = serde_json::to_value(&labelled).expect("the row serializes");
    assert_eq!(encoded["tag"], json!("fix-auth"));
    assert_eq!(encoded["host"], json!("workstation"));
    assert_eq!(
        serde_json::from_value::<SessionSummary>(encoded).expect("it decodes again"),
        labelled,
    );
}

/// A cursor's `<epoch>:<seq>` encoding round-trips, and the shapes that
/// are not one are refused rather than guessed at.
#[test]
fn a_cursor_round_trips_through_its_wire_encoding() {
    let cursor = Cursor {
        epoch: "0f1e2d3c".to_string(),
        seq: 42,
    };
    assert_eq!(cursor.to_string(), "0f1e2d3c:42");
    assert_eq!("0f1e2d3c:42".parse::<Cursor>(), Ok(cursor));

    // The epoch is opaque to whoever echoes it back, so a colon inside it
    // survives the round trip.
    let colons = Cursor {
        epoch: "a:b".to_string(),
        seq: 7,
    };
    assert_eq!(colons.to_string().parse::<Cursor>(), Ok(colons));

    for malformed in ["", "epoch", "epoch:", ":7", "epoch:-1", "epoch:x"] {
        assert!(
            malformed.parse::<Cursor>().is_err(),
            "{malformed:?} is not a cursor",
        );
    }
}

fn assert_round_trip<T>(expected: &Value)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let decoded: T = serde_json::from_value(expected.clone())
        .unwrap_or_else(|error| panic!("failed to decode {expected}: {error}"));
    let actual = serde_json::to_value(decoded).expect("model re-serializes");
    assert_eq!(actual, *expected);
}

fn assert_decoded_round_trip<T>(expected: &str)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let decoded: T = serde_json::from_str(expected)
        .unwrap_or_else(|error| panic!("failed to decode {expected}: {error}"));
    let actual = serde_json::to_string(&decoded).expect("value re-serializes");
    assert_eq!(actual, expected);
}

fn agent_event_type(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart { .. } => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart { .. } => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageUpdate { .. } => "message_update",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
        AgentEvent::SubAgentStart { .. } => "sub_agent_start",
        AgentEvent::SubAgentEnd { .. } => "sub_agent_end",
        AgentEvent::TaskStart { .. } => "task_start",
        AgentEvent::TaskOutput { .. } => "task_output",
        AgentEvent::TaskEnd { .. } => "task_end",
        AgentEvent::Notice { .. } => "notice",
        AgentEvent::Warning { .. } => "warning",
        AgentEvent::Error { .. } => "error",
        AgentEvent::StreamRetry { .. } => "stream_retry",
        AgentEvent::UsageUpdate { .. } => "usage_update",
        AgentEvent::CompactionStart { .. } => "compaction_start",
        AgentEvent::CompactionProgress { .. } => "compaction_progress",
        AgentEvent::CompactionEnd { .. } => "compaction_end",
        AgentEvent::QueueUpdate { .. } => "queue_update",
    }
}

fn frame_kind(frame: &Frame) -> &'static str {
    match frame {
        Frame::Event { .. } => "event",
        Frame::State { .. } => "state",
        Frame::CaughtUp { .. } => "caught_up",
        Frame::List { .. } => "list",
        Frame::Reset { .. } => "reset",
        Frame::Heartbeat => "heartbeat",
        Frame::Vms { .. } => "vms",
    }
}
