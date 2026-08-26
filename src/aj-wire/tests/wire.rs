use std::collections::{BTreeMap, BTreeSet};

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_wire::{
    ArchiveRequest, CancelRequest, CompactRequest, CreateSessionRequest, Cursor, DecodedAgentEvent,
    DecodedFrame, DirectoryHost, EnrollHostRequest, ErrorResponse, Frame, HeadRequest, Hello,
    HostList, HostNameError, HostSource, HostSummary, MAX_HOST_NAME_BYTES, MergedDirectory,
    ModelSelection, PromptInput, PromptRequest, QueueCounts, QueueOperation, QueueOutcome,
    QueueRequest, QueueState, RawObject, SessionCreated, SessionList, SessionSettings,
    SessionSummary, SessionTree, SettingsRequest, SteerRequest, TagRequest, TaskDetails, TaskTable,
    VmList, normalize_host_name,
};
use serde_json::value::RawValue;
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
    "error",
    "reset",
    "heartbeat",
    "vms",
];

/// Frames in the shapes that make the session reader's and the rewrite's rules
/// observable: a known and an unknown kind, host-scoped and session-scoped, a
/// payload carrying a `session` of its own, a duplicated key, a key spelled
/// with an escape, and number literals no float survives.
const FORWARDED_FRAMES: &[&str] = &[
    r#"{"kind":"reset","session":"session-1"}"#,
    r#"{"kind":"caught_up","session":"session-1","epoch":"e","last_seq":3,"future":1e400}"#,
    r#"{"kind":"heartbeat"}"#,
    r#"{"kind":"future_frame","session":"session-1"}"#,
    r#"{"kind":"future_frame","host_scoped":true}"#,
    r#"{"kind":"future_frame","session":"session-1","payload":{"session":"nested","huge":1e400}}"#,
    r#"{"kind":"future_frame","payload":{"session":"nested"},"rows":[{"session":"row"}]}"#,
    r#"{"kind":"future_frame","session":"first","session":"last"}"#,
    r#"{"kind":"future_frame","\u0073ession":"session-1"}"#,
    r#"{"kind":"future_frame","session":"a\u0062c","big":18446744073709551616}"#,
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
        tag: None,
        host: None,
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
            id: "session-1".into(),
            incomplete: None,
        })
        .unwrap(),
        json!({"id":"session-1"}),
        "a create that applied everything says only the id",
    );
    // The session exists either way, so the id is the answer and what did
    // not land rides along beside it.
    let partial = SessionCreated {
        id: "session-1".into(),
        incomplete: Some("session session-1 created, tag not applied: nope".into()),
    };
    let encoded = serde_json::to_value(&partial).unwrap();
    assert_eq!(
        encoded,
        json!({
            "id":"session-1",
            "incomplete":"session session-1 created, tag not applied: nope"
        })
    );
    assert_eq!(
        serde_json::from_value::<SessionCreated>(encoded).unwrap(),
        partial,
    );
}

/// A create may name the host it is for (spec 6.6), in the same vocabulary a
/// directory row's `host` field and an enrolled host's id use. The field is
/// optional and additive: a client that names none sends no key at all, which
/// is what leaves the choice of host to the server that answers.
#[test]
fn a_create_names_the_host_it_is_for_or_leaves_the_choice_to_the_server() {
    let targeted = CreateSessionRequest {
        host: Some("workstation".to_string()),
        ..CreateSessionRequest::default()
    };
    assert_eq!(
        serde_json::to_value(&targeted).unwrap(),
        json!({"host": "workstation"}),
    );
    assert_eq!(
        serde_json::to_value(CreateSessionRequest::default()).unwrap(),
        json!({}),
        "a create that names no host emits no key",
    );
    // A null reads as no host named, so a client that spells the absence out
    // is not refused for it.
    for body in [
        json!({"host": "workstation"}),
        json!({"tag": "fix-auth"}),
        json!({"host": null}),
    ] {
        let expected = body.get("host").and_then(Value::as_str);
        assert_eq!(
            serde_json::from_value::<CreateSessionRequest>(body.clone())
                .unwrap_or_else(|err| panic!("{body} decodes: {err}"))
                .host
                .as_deref(),
            expected,
            "{body}",
        );
    }
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

/// Every frame kind is pinned to a fixture on both the paths that write one: the
/// JSON a wire-decoded frame retains and forwards, and the encoder a locally
/// built frame writes from its typed value. That contract is the one
/// `DecodedFrame::session` and `DecodedFrame::rows` read on.
///
/// Only the second reaches `FrameRef`. Re-serializing the decoded frame alone
/// hands back the bytes the fixture was read from, which is identity whatever
/// the encoder does with it.
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

        let local = DecodedFrame::try_from(known.value().clone())
            .unwrap_or_else(|error| panic!("failed to rebuild {expected}: {error}"));
        let DecodedFrame::Known(rebuilt) = &local else {
            panic!("expected known frame");
        };
        assert!(
            rebuilt.raw_json().is_none(),
            "the rebuilt frame retained JSON of its own, so both halves of this \
             test read the same bytes back and neither reaches the encoder: \
             {expected}",
        );
        let written = serde_json::to_value(&local).expect("the rebuilt frame serializes");
        assert_eq!(
            written, expected,
            "this build writes a frame of this kind in a shape the fixture does \
             not pin",
        );
        let forwarded = serde_json::to_value(&frame).expect("frame re-serializes");
        assert_eq!(
            forwarded, expected,
            "a frame decoded from the wire did not forward the bytes it arrived as",
        );
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
    let before: DecodedFrame = serde_json::from_str(input).unwrap();
    let mut after = before.clone();

    assert!(after.rewrite_session("gateway:old").unwrap());

    assert_rewrote_session(&before, &after, "gateway:old");
    assert_eq!(known_session(&after), Some("gateway:old"));
    // The number literals are the one place byte preservation is the property
    // rather than a coincidence: re-parsing would round 2^64 to a float and
    // reject 1e400 outright.
    let json = serde_json::to_string(&after).unwrap();
    assert!(json.contains("1e400"), "{json}");
    assert!(json.contains("18446744073709551616"), "{json}");
}

#[test]
fn decoded_frame_rewrites_unknown_session_without_parsing_payloads() {
    let input = r#"{"kind":"future_frame","session":"old","future_number":1e400}"#;
    let before: DecodedFrame = serde_json::from_str(input).unwrap();
    let mut after = before.clone();

    assert!(after.rewrite_session("gateway:old").unwrap());

    assert_rewrote_session(&before, &after, "gateway:old");
    let DecodedFrame::Unknown { kind, .. } = &after else {
        panic!("a rewritten unknown frame stays unknown");
    };
    assert_eq!(kind, "future_frame");
    assert!(
        serde_json::to_string(&after).unwrap().contains("1e400"),
        "an unparsed payload keeps its number literals",
    );
}

/// Every frame kind either carries a top-level `session` a gateway rewrites,
/// or is host-scoped and forwarded as is (spec 6.10). The partition lives in
/// [`frame_carries_session`], whose match is exhaustive, so a new frame
/// variant does not compile until it says which side it is on.
///
/// The fixtures are fed in pretty-printed so that "left untouched" is a
/// statement with teeth: a frame that went through a re-serialization would
/// come back compacted.
#[test]
fn every_frame_kind_says_whether_its_session_can_be_rewritten() {
    let mut covered = BTreeSet::new();

    for fixture in fixture("frames").as_array().expect("frames are an array") {
        let input = serde_json::to_string_pretty(fixture).unwrap();
        let before: DecodedFrame = serde_json::from_str(&input).unwrap();
        let DecodedFrame::Known(known) = &before else {
            panic!("the pinned fixtures are all known kinds");
        };
        let carries = frame_carries_session(known.value());
        covered.insert(frame_kind(known.value()));
        assert_eq!(
            known.value().session().is_some(),
            carries,
            "Frame::session agrees on the partition: {input}",
        );

        let mut after = before.clone();
        assert_eq!(
            after.rewrite_session("gateway:session-1").unwrap(),
            carries,
            "{input}",
        );
        if carries {
            assert_rewrote_session(&before, &after, "gateway:session-1");
            assert_eq!(known_session(&after), Some("gateway:session-1"));
        } else {
            // A host-scoped frame is not re-serialized at all, so a `list`
            // frame's rows keep the ids a gateway rewrites for itself.
            assert_eq!(serde_json::to_string(&after).unwrap(), input);
        }
    }

    assert_eq!(
        covered,
        FRAME_KINDS.iter().copied().collect::<BTreeSet<_>>(),
        "one fixture per frame kind",
    );
}

/// A frame built in process retains no JSON, so the rewrite goes through its
/// typed value instead. Both paths answer for the same partition, and a
/// host-scoped frame comes back untouched here too.
#[test]
fn a_locally_built_frame_rewrites_through_its_typed_value() {
    let mut covered = BTreeSet::new();

    for frame in local_frames() {
        let before = DecodedFrame::try_from(frame).expect("a local frame is valid");
        let DecodedFrame::Known(known) = &before else {
            panic!("a local frame is known");
        };
        assert!(known.raw_json().is_none(), "a local frame retains no JSON");
        let carries = frame_carries_session(known.value());
        covered.insert(frame_kind(known.value()));
        assert_eq!(known.value().session().is_some(), carries);

        let mut after = before.clone();
        let kind = frame_kind(known.value());
        assert_eq!(
            after.rewrite_session("gateway:old").unwrap(),
            carries,
            "{kind}"
        );
        if carries {
            assert_rewrote_session(&before, &after, "gateway:old");
            assert_eq!(known_session(&after), Some("gateway:old"));
        } else {
            assert_eq!(
                serde_json::to_string(&after).unwrap(),
                serde_json::to_string(&before).unwrap(),
                "{kind}",
            );
        }
    }

    assert_eq!(
        covered,
        FRAME_KINDS.iter().copied().collect::<BTreeSet<_>>(),
        "one locally built frame per frame kind",
    );
}

/// An unknown frame with no top-level `session` is host-scoped and forwarded
/// as is (spec 6.10). A `session` down in a payload is not the frame's
/// session, and finding one there must not turn the frame into a
/// session-scoped one.
#[test]
fn an_unknown_frame_without_a_top_level_session_is_forwarded_as_is() {
    let input = r#"{
  "kind": "future_frame",
  "host_scoped": true,
  "payload": {"session": "nested", "huge": 1e400}
}"#;
    let mut frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert!(!frame.rewrite_session("gateway:whatever").unwrap());

    assert_eq!(
        serde_json::to_string(&frame).unwrap(),
        input,
        "a declined frame is handed on exactly as it arrived",
    );
}

/// The rewrite reaches the top-level `session` and nothing else. This is what
/// makes it safe on a frame the forwarder does not understand: an event
/// payload may plausibly carry a `session` field of its own, and it has to
/// arrive as the host wrote it.
#[test]
fn the_rewrite_reaches_only_the_top_level_session() {
    let event = r#"{"type":"notice","agent_id":"main","text":"hi","session":"payload-session"}"#;
    let input = format!(r#"{{"kind":"event","session":"old","epoch":"e","event":{event}}}"#);
    let before: DecodedFrame = serde_json::from_str(&input).unwrap();
    let mut after = before.clone();

    assert!(after.rewrite_session("gateway:old").unwrap());

    assert_rewrote_session(&before, &after, "gateway:old");
    assert_eq!(top_level_fields(&after)["event"], event);

    let payload = r#"{"nested":{"session":"deep","huge":1e400}}"#;
    let input = format!(r#"{{"kind":"future_frame","session":"old","payload":{payload}}}"#);
    let before: DecodedFrame = serde_json::from_str(&input).unwrap();
    let mut after = before.clone();

    assert!(after.rewrite_session("gateway:old").unwrap());

    assert_rewrote_session(&before, &after, "gateway:old");
    assert_eq!(top_level_fields(&after)["payload"], payload);
}

/// A duplicate `session` is malformed and refused for a known kind, but an
/// unknown frame is forwarded as it arrived, duplicates included, and a reader
/// downstream may take either occurrence. So every occurrence is rewritten.
#[test]
fn every_top_level_session_occurrence_is_rewritten() {
    let input = r#"{"kind":"future_frame","session":"old","session":"older"}"#;
    let mut frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert!(frame.rewrite_session("gateway:old").unwrap());

    // Counted in the text rather than looked up by key, which is the whole
    // point: a map keyed by name would hide the second occurrence.
    let json = serde_json::to_string(&frame).unwrap();
    assert_eq!(
        json.matches(r#""session":"gateway:old""#).count(),
        2,
        "{json}"
    );
    assert!(!json.contains("older"), "{json}");
}

/// A gateway mints `<host_id>:<session_id>` from two opaque halves (spec 6.2),
/// so the replacement goes in as JSON rather than spliced in as text. A
/// control character in an id would otherwise break the line-oriented framing
/// the frame travels in.
#[test]
fn a_session_id_needing_escapes_survives_the_rewrite() {
    let replacement = "h\"o\\st\u{1}\n:sitzplatz\u{2013}1";

    for input in [
        r#"{"kind":"reset","session":"old"}"#,
        r#"{"kind":"future_frame","session":"old"}"#,
    ] {
        let mut frame: DecodedFrame = serde_json::from_str(input).unwrap();

        assert!(frame.rewrite_session(replacement).unwrap());

        let json = serde_json::to_string(&frame).unwrap();
        assert!(!json.contains('\n'), "{json}");
        let value: Value = serde_json::from_str(&json).expect("the rewrite emits valid JSON");
        assert_eq!(value["session"], json!(replacement), "{json}");
        if matches!(frame, DecodedFrame::Known(_)) {
            assert_eq!(known_session(&frame), Some(replacement));
        }
    }
}

/// The key is matched after JSON unescaping, not by looking for `"session"` in
/// the frame's bytes. Pinned because that scan looks like a cheap
/// optimization and is not one.
#[test]
fn the_session_key_is_matched_after_json_unescaping() {
    let input = r#"{"kind":"future_frame","\u0073ession":"old"}"#;
    let mut frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert!(frame.rewrite_session("gateway:old").unwrap());

    assert_eq!(top_level_fields(&frame)["session"], r#""gateway:old""#);
}

/// A gateway rewrites a frame it has already rewritten whenever its own view
/// of the id moves, and it routes on the typed value while forwarding the
/// retained JSON, so the two must not drift apart.
#[test]
fn a_rewritten_frame_can_be_rewritten_again() {
    let input =
        r#"{"kind":"caught_up","session":"old","epoch":"e","last_seq":3,"added_later":true}"#;
    let mut frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert!(frame.rewrite_session("first:old").unwrap());
    assert_eq!(known_session(&frame), Some("first:old"));
    let once = frame.clone();
    assert!(frame.rewrite_session("second:old").unwrap());

    assert_rewrote_session(&once, &frame, "second:old");
    assert_eq!(known_session(&frame), Some("second:old"));
    assert_eq!(top_level_fields(&frame)["added_later"], "true");

    let mut local = DecodedFrame::try_from(Frame::Reset {
        session: "old".to_string(),
    })
    .unwrap();
    assert!(local.rewrite_session("first:old").unwrap());
    assert!(local.rewrite_session("second:old").unwrap());
    assert_eq!(known_session(&local), Some("second:old"));
}

/// Rewriting to the id a frame already carries changes nothing that matters.
/// It may still change bytes, because the id is re-emitted the way serde
/// writes a string rather than the way the host spelled it, and spec 6.10 asks
/// only for structural equality.
#[test]
fn rewriting_to_the_same_id_changes_nothing_that_matters() {
    let input = r#"{"kind":"future_frame","session":"a\u0062c","huge":1e400}"#;
    let before: DecodedFrame = serde_json::from_str(input).unwrap();
    let mut after = before.clone();

    assert!(after.rewrite_session("abc").unwrap());

    assert_rewrote_session(&before, &after, "abc");
    let spelled = top_level_fields(&before).remove("session").unwrap();
    let rewritten = top_level_fields(&after).remove("session").unwrap();
    assert_eq!(
        serde_json::from_str::<String>(&spelled).unwrap(),
        serde_json::from_str::<String>(&rewritten).unwrap(),
        "the id is the one the frame already carried",
    );
    assert_ne!(spelled, rewritten, "byte identity is not the promise");
}

/// Every frame kind either belongs to a session, which the reader names, or is
/// host-scoped (spec 6.10). The reader answers for the partition
/// [`frame_carries_session`] draws, and names the same session the typed
/// [`Frame::session`] does.
#[test]
fn every_frame_kind_says_which_session_it_belongs_to() {
    let mut covered = BTreeSet::new();

    for fixture in fixture("frames").as_array().expect("frames are an array") {
        let input = serde_json::to_string_pretty(fixture).unwrap();
        let frame: DecodedFrame = serde_json::from_str(&input).unwrap();
        let DecodedFrame::Known(known) = &frame else {
            panic!("the pinned fixtures are all known kinds");
        };
        covered.insert(frame_kind(known.value()));

        let session = frame.session().expect("a pinned frame names its session");
        assert_eq!(
            session.is_some(),
            frame_carries_session(known.value()),
            "{input}",
        );
        assert_eq!(session.as_deref(), known.value().session(), "{input}");
    }

    assert_eq!(
        covered,
        FRAME_KINDS.iter().copied().collect::<BTreeSet<_>>(),
        "one fixture per frame kind",
    );
}

/// A frame built in process retains no JSON, so the reader falls back to its
/// typed value, which is the source the rewrite writes into for such a frame.
#[test]
fn a_locally_built_frame_reads_its_session_from_its_typed_value() {
    let mut covered = BTreeSet::new();

    for frame in local_frames() {
        let frame = DecodedFrame::try_from(frame).expect("a local frame is valid");
        let DecodedFrame::Known(known) = &frame else {
            panic!("a local frame is known");
        };
        assert!(known.raw_json().is_none(), "a local frame retains no JSON");
        let kind = frame_kind(known.value());
        covered.insert(kind);

        let session = frame.session().expect("a local frame names its session");
        assert_eq!(
            session.is_some(),
            frame_carries_session(known.value()),
            "{kind}",
        );
        assert_eq!(session.as_deref(), known.value().session(), "{kind}");
    }

    assert_eq!(
        covered,
        FRAME_KINDS.iter().copied().collect::<BTreeSet<_>>(),
        "one locally built frame per frame kind",
    );
}

/// An unknown frame has no typed value, so its id comes off the JSON the
/// gateway forwards. Reading it must leave the payload unparsed: a host a
/// version ahead may put number literals in there that no float survives, and
/// a frame that cannot be read is a frame that cannot be namespaced.
#[test]
fn an_unknown_session_scoped_frame_reads_its_id() {
    let input =
        r#"{"kind":"future_frame","session":"session-1","huge":1e400,"big":18446744073709551616}"#;
    let frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert!(matches!(frame, DecodedFrame::Unknown { .. }));
    assert_eq!(frame.session().unwrap().as_deref(), Some("session-1"));
}

/// An unknown frame with no top-level `session` is host-scoped and forwarded as
/// is (spec 6.10). A `session` down in its payload is not the frame's, and
/// finding one there must not make a session-scoped frame of it: the gateway
/// would namespace a host-scoped frame and route it to a session that does not
/// exist.
#[test]
fn an_unknown_host_scoped_frame_reads_no_session() {
    let input = r#"{"kind":"future_frame","host_scoped":true,"payload":{"session":"nested","huge":1e400},"rows":[{"session":"row"}]}"#;
    let frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert_eq!(frame.session().unwrap(), None);
}

/// The reader reaches the top-level `session` and nothing else, the field the
/// rewrite replaces. An event payload may plausibly carry a `session` of its
/// own, and a gateway that read that one would namespace the frame with an id
/// its own rewrite never touched.
///
/// The nested occurrence comes first in both fixtures, so taking the first
/// `session` anywhere in the document is not enough to pass.
#[test]
fn the_reader_reaches_only_the_top_level_session() {
    let event = r#"{"type":"notice","agent_id":"main","text":"hi","session":"payload-session"}"#;
    let input = format!(r#"{{"kind":"event","epoch":"e","event":{event},"session":"envelope"}}"#);
    let frame: DecodedFrame = serde_json::from_str(&input).unwrap();

    assert_eq!(frame.session().unwrap().as_deref(), Some("envelope"));

    let input =
        r#"{"kind":"future_frame","nested":{"deep":{"session":"deep"}},"session":"envelope"}"#;
    let frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert_eq!(frame.session().unwrap().as_deref(), Some("envelope"));
}

/// The key is matched after JSON unescaping, not by looking for `"session"` in
/// the frame's bytes. Pinned on the read side too, because that scan looks like
/// a cheap optimization and is not one.
#[test]
fn the_session_key_is_read_after_json_unescaping() {
    let input = r#"{"kind":"future_frame","\u0073ession":"session-1"}"#;
    let frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert_eq!(frame.session().unwrap().as_deref(), Some("session-1"));
}

/// A gateway mints `<host_id>:<session_id>` from two opaque halves (spec 6.2),
/// so an id may need JSON escapes. What the rewrite wrote reads back as the id
/// rather than as its spelling, which is what makes the pair usable more than
/// once.
#[test]
fn a_session_id_needing_escapes_reads_back_as_it_was_written() {
    let replacement = "h\"o\\st\u{1}\n:sitzplatz\u{2013}1";

    for input in [
        r#"{"kind":"reset","session":"old"}"#,
        r#"{"kind":"future_frame","session":"old"}"#,
    ] {
        let mut frame: DecodedFrame = serde_json::from_str(input).unwrap();

        assert!(frame.rewrite_session(replacement).unwrap());

        assert_eq!(
            frame.session().unwrap().as_deref(),
            Some(replacement),
            "{input}",
        );
    }

    // A host may spell an id with escapes of its own, which are not part of it.
    let input = r#"{"kind":"future_frame","session":"a\u0062c\n"}"#;
    let frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert_eq!(frame.session().unwrap().as_deref(), Some("abc\n"));
}

/// A duplicate `session` is malformed and refused for a known kind, but an
/// unknown frame is forwarded as it arrived. The reader takes the last
/// occurrence, the one a client that parses the frame into a map reads, so a
/// gateway namespaces by the id its clients see. The rewrite replaces every
/// occurrence, so from then on there is nothing left to choose between.
#[test]
fn a_duplicated_session_reads_the_last_occurrence() {
    let input = r#"{"kind":"future_frame","session":"first","session":"last"}"#;
    let mut frame: DecodedFrame = serde_json::from_str(input).unwrap();

    assert_eq!(frame.session().unwrap().as_deref(), Some("last"));
    assert_eq!(
        serde_json::from_str::<Value>(input).unwrap()["session"],
        json!("last"),
        "the last occurrence is what a map-parsing reader takes",
    );

    assert!(frame.rewrite_session("gateway:last").unwrap());
    assert_eq!(frame.session().unwrap().as_deref(), Some("gateway:last"));
}

/// A top-level `session` no id can be read from is malformed (spec 6.3 mints
/// ids as strings) and there is nothing in it to namespace with, so the reader
/// says so rather than guess. `null` counts as present, the way the rewrite
/// counts it and the way an event frame's `seq` must be omitted rather than
/// nulled. A string token whose escapes do not decode counts as present too: a
/// known kind is refused at decode over it, an unknown kind is forwarded because
/// its payload is never parsed, so only the reader is left to object.
///
/// This is the one class of input where the reader does not answer as the
/// rewrite does: the rewrite replaces the field and reports `true`. `None` would
/// be worse than an error here, it would call the frame host-scoped, and a
/// gateway would forward it carrying the host's own id.
#[test]
fn a_session_that_is_not_a_readable_string_is_an_error_rather_than_a_missing_one() {
    for input in [
        r#"{"kind":"future_frame","session":null}"#,
        r#"{"kind":"future_frame","session":42}"#,
        r#"{"kind":"future_frame","session":["session-1"]}"#,
        r#"{"kind":"future_frame","session":{"id":"session-1"}}"#,
        r#"{"kind":"future_frame","session":"\ud800"}"#,
    ] {
        let mut frame: DecodedFrame = serde_json::from_str(input).unwrap();

        assert!(frame.session().is_err(), "{input}");

        // The rewrite takes presence as enough, so the frame is namespaced and
        // reads back afterwards.
        assert!(
            frame.rewrite_session("gateway:session-1").unwrap(),
            "{input}"
        );
        assert_eq!(
            frame.session().unwrap().as_deref(),
            Some("gateway:session-1"),
            "{input}",
        );
    }

    // The same frame under a known kind does not get this far.
    assert!(
        serde_json::from_str::<DecodedFrame>(r#"{"kind":"reset","session":"\ud800"}"#).is_err(),
    );
}

/// The property that makes the reader and the rewrite safe as a pair for a
/// gateway: across every frame either of them may meet, the reader names an id
/// exactly where the rewrite finds a field to replace, and what the rewrite
/// wrote is what the reader reads back.
#[test]
fn the_session_reader_and_the_rewrite_agree_on_every_frame() {
    for fixture in fixture("frames").as_array().expect("frames are an array") {
        let input = serde_json::to_string_pretty(fixture).unwrap();
        assert_reads_what_the_rewrite_writes(&serde_json::from_str(&input).unwrap());
    }

    for frame in local_frames() {
        let frame = DecodedFrame::try_from(frame).expect("a local frame is valid");
        assert_reads_what_the_rewrite_writes(&frame);
    }

    for input in FORWARDED_FRAMES {
        assert_reads_what_the_rewrite_writes(&serde_json::from_str(input).unwrap());
    }
}

/// A `list` frame's rows come back as their host wrote them (spec 6.10): a
/// gateway re-emits them under its own name, so a field this build has no type
/// for, and a number literal no float survives, have to travel through the read
/// and back out again.
#[test]
fn a_list_frames_rows_are_read_as_their_host_wrote_them() {
    let frame: DecodedFrame = serde_json::from_str(GATEWAY_ROWS).expect("a list frame decodes");

    let rows = frame
        .rows()
        .expect("the rows are readable")
        .expect("a list frame has rows");

    let [live, cold] = &rows[..] else {
        panic!("the fixture carries two rows: {rows:?}");
    };
    assert_eq!(live.get::<String>("id").expect("an id"), Some("s-1".into()));
    assert_eq!(
        serde_json::to_string(live).expect("a row re-serializes"),
        r#"{"id":"s-1","live":true,"working":false,"queued":{"steering":0,"follow_up":0},"tasks":0,"last_seq":7,"last_activity":"2026-08-03T12:00:00Z","unreachable":false,"preview":{"text":"hello","weight":18446744073709551616}}"#,
        "a row is re-emitted as it arrived, `preview` and its literal included",
    );
    assert_eq!(cold.get::<String>("id").expect("an id"), Some("s-0".into()));
}

/// The three fields a gateway owns on a row it re-emits (spec 6.10), edited
/// through the same primitive that rewrites a frame's session: `id` and
/// `unreachable` are replaced where they sit, `host` is added to a plain host's
/// row that has none, and nothing else moves.
#[test]
fn a_row_takes_the_fields_a_gateway_owns_and_keeps_the_rest() {
    let frame: DecodedFrame = serde_json::from_str(GATEWAY_ROWS).expect("a list frame decodes");
    let mut rows = frame.rows().expect("readable").expect("rows");
    let row = &mut rows[0];

    row.set("id", "left:s-1").expect("a string encodes");
    row.set("host", "left").expect("a string encodes");
    row.set("unreachable", &true).expect("a bool encodes");

    assert_eq!(
        serde_json::to_string(row).expect("a row re-serializes"),
        r#"{"id":"left:s-1","live":true,"working":false,"queued":{"steering":0,"follow_up":0},"tasks":0,"last_seq":7,"last_activity":"2026-08-03T12:00:00Z","unreachable":true,"preview":{"text":"hello","weight":18446744073709551616},"host":"left"}"#,
        "`id` and `unreachable` replaced where they sat, `host` appended, and a \
         payload the gateway never parsed",
    );
    assert_eq!(
        serde_json::from_str::<SessionSummary>(
            &serde_json::to_string(row).expect("a row re-serializes")
        )
        .expect("an edited row still decodes")
        .host
        .as_deref(),
        Some("left"),
    );
}

/// A locally built `list` frame retains no JSON, so its rows come from its typed
/// values, which is the fallback the session reader makes for the same reason.
/// Every other kind has no rows at all, unknown kinds included: a gateway
/// forwards those whole rather than merging them.
#[test]
fn only_a_list_frame_has_rows() {
    let local = DecodedFrame::try_from(Frame::List {
        sessions: vec![pinned_row()],
        hosts: Vec::new(),
    })
    .expect("a local list frame is valid");
    let rows = local.rows().expect("readable").expect("a list frame");
    assert_eq!(
        rows[0].get::<String>("id").expect("an id"),
        Some("session-0".to_string()),
        "a frame with no retained JSON answers from its typed rows",
    );
    assert_eq!(
        serde_json::from_str::<SessionSummary>(
            &serde_json::to_string(&rows[0]).expect("a row re-serializes")
        )
        .expect("it decodes"),
        pinned_row(),
    );

    for frame in local_frames() {
        let carries_rows = matches!(frame, Frame::List { .. });
        let frame = DecodedFrame::try_from(frame).expect("a local frame is valid");
        assert_eq!(
            frame.rows().expect("readable").is_some(),
            carries_rows,
            "{frame:?}",
        );
    }
    for input in FORWARDED_FRAMES {
        let frame: DecodedFrame = serde_json::from_str(input).expect("a frame decodes");
        assert!(
            frame.rows().expect("readable").is_none(),
            "no kind but `list` has rows: {input}",
        );
    }
}

/// The primitive under the row edit and the session rewrite: it names one
/// top-level field and touches nothing else, keys keep the order they arrived
/// in, and a value below the top level is never parsed, so a nested `id` is not
/// a field a gateway owns.
#[test]
fn a_raw_object_edits_the_one_field_it_names() {
    let mut object: RawObject = serde_json::from_str(
        r#"{"id":"s-1","nested":{"id":"inner","huge":1e400},"tag":"fix-auth"}"#,
    )
    .expect("an object");

    assert_eq!(
        object.get::<String>("id").expect("a string"),
        Some("s-1".into())
    );
    assert_eq!(object.get::<String>("absent").expect("no key"), None);
    assert!(
        object.get::<String>("nested").is_err(),
        "a field that is not a string says so rather than reading as absent",
    );

    object.set("id", "left:s-1").expect("a string encodes");
    object.set("unreachable", &false).expect("a bool encodes");
    assert_eq!(
        serde_json::to_string(&object).expect("it re-serializes"),
        r#"{"id":"left:s-1","nested":{"id":"inner","huge":1e400},"tag":"fix-auth","unreachable":false}"#,
        "the named field is replaced where it sat, a new one is appended",
    );
}

/// A duplicated key is malformed and a gateway forwards it all the same, so the
/// read takes the occurrence a reader that parses the object into a map would,
/// and the edit leaves no other occurrence behind for anyone to disagree over.
#[test]
fn a_raw_object_reads_the_last_duplicate_and_replaces_every_one() {
    let mut object: RawObject =
        serde_json::from_str(r#"{"id":"first","live":true,"id":"last"}"#).expect("an object");

    assert_eq!(
        object.get::<String>("id").expect("a string"),
        Some("last".to_string()),
    );

    object.set("id", "left:s-1").expect("a string encodes");
    assert_eq!(
        serde_json::to_string(&object).expect("it re-serializes"),
        r#"{"id":"left:s-1","live":true,"id":"left:s-1"}"#,
    );
}

/// A value built in process has no wire JSON to keep, so it is encoded once and
/// edited from there.
#[test]
fn a_raw_object_can_be_encoded_from_a_typed_value() {
    let mut object = RawObject::encode(&pinned_row()).expect("a row is an object");
    object.set("host", "left").expect("a string encodes");

    let row: SessionSummary =
        serde_json::from_str(&serde_json::to_string(&object).expect("it re-serializes"))
            .expect("it decodes");
    assert_eq!(row.host.as_deref(), Some("left"));
    assert!(
        RawObject::encode(&[1, 2, 3]).is_err(),
        "an array is not an object to edit",
    );
}

/// Two rows are the same when they carry the same fields in the same order with
/// the same text, which is what lets a gateway ask whether its merged directory
/// moved without parsing every row back.
#[test]
fn raw_objects_compare_on_the_text_they_would_emit() {
    let row = |raw: &str| serde_json::from_str::<RawObject>(raw).expect("an object");

    assert_eq!(
        row(r#"{"id":"s-1","live":true}"#),
        row(r#"{"id":"s-1","live":true}"#)
    );
    assert_ne!(
        row(r#"{"id":"s-1","live":true}"#),
        row(r#"{"id":"s-1","live":false}"#)
    );
    assert_ne!(
        row(r#"{"id":"s-1","live":true}"#),
        row(r#"{"live":true,"id":"s-1"}"#),
        "key order is what a re-emitted row keeps, so it counts as a difference",
    );
    assert_ne!(row(r#"{"id":"s-1"}"#), row(r#"{"id":"s-1","live":true}"#));
}

/// A gateway's `list` frame names the hosts it has enrolled alongside the rows
/// (spec 7.1). Additive: a plain host's frame carries no such key, and an older
/// peer's frame reads as naming none.
///
/// A host is named by exactly one of `id` and `address`: the id once the gateway
/// has spoken to it, and the address it is enrolled at until then. Which one it
/// is is what tells a client "an empty group labelled by address, no id yet"
/// from "a group whose sessions it can address". A `name` rides on top of
/// either, for the host that reported one.
#[test]
fn a_list_frame_names_the_hosts_a_gateway_enrolled() {
    let frame: DecodedFrame = serde_json::from_value(fixture("frames")[5].clone())
        .expect("the pinned gateway list frame decodes");
    let DecodedFrame::Known(known) = &frame else {
        panic!("a list frame is a known kind");
    };
    let Frame::List { sessions, hosts } = known.value() else {
        panic!("the fixture at that index is the gateway's list frame");
    };
    assert_eq!(sessions[0].host.as_deref(), Some("workstation"));
    let named = vec![
        DirectoryHost {
            id: Some("workstation".to_string()),
            address: None,
            name: Some("~/work/lewitt/aj".to_string()),
            unreachable: false,
        },
        DirectoryHost {
            id: Some("laptop".to_string()),
            address: None,
            name: None,
            unreachable: true,
        },
        DirectoryHost {
            id: None,
            address: Some("http://100.64.0.9:6161".to_string()),
            name: None,
            unreachable: true,
        },
    ];
    assert_eq!(
        hosts, &named,
        "a host with no rows here is still named, which is what a client renders \
         an empty group from, and the one this gateway has never spoken to is \
         labelled by its address with nothing in the id position",
    );

    // The encoder, not the retained JSON: a frame built here has none, so this
    // is what pins the shape a gateway writes.
    let written = serde_json::to_value(
        DecodedFrame::try_from(Frame::List {
            sessions: sessions.clone(),
            hosts: named,
        })
        .expect("a local list frame is valid"),
    )
    .expect("it serializes");
    assert_eq!(
        written["hosts"],
        fixture("frames")[5]["hosts"],
        "an absent id is an absent key and never a null or an address: {written}",
    );

    let plain = serde_json::to_value(
        DecodedFrame::try_from(Frame::List {
            sessions: Vec::new(),
            hosts: Vec::new(),
        })
        .expect("a local list frame is valid"),
    )
    .expect("it serializes");
    assert_eq!(
        plain,
        json!({"kind": "list", "sessions": []}),
        "a plain host names no hosts and writes no key",
    );
    let older: Frame = serde_json::from_value(json!({"kind": "list", "sessions": []}))
        .expect("a frame with no hosts key decodes");
    assert!(matches!(older, Frame::List { hosts, .. } if hosts.is_empty()));
}

/// The directory a gateway composes is one value serving two places: the
/// sessions read and the `list` frames, which is what keeps a client that reads
/// and a client that watches from disagreeing (spec 7.1). Its rows travel as
/// their hosts wrote them, and what comes out the other side is what a typed
/// client decodes.
#[test]
fn a_merged_directory_writes_the_read_and_the_frame_from_one_value() {
    let rows: DecodedFrame = serde_json::from_str(GATEWAY_ROWS).expect("a list frame decodes");
    let mut sessions = rows.rows().expect("readable").expect("rows");
    for (index, row) in sessions.iter_mut().enumerate() {
        row.set("id", &format!("left:s-{index}"))
            .expect("a string encodes");
        row.set("host", "left").expect("a string encodes");
    }
    let directory = MergedDirectory {
        sessions,
        hosts: vec![
            DirectoryHost {
                id: Some("left".to_string()),
                address: None,
                name: None,
                unreachable: false,
            },
            DirectoryHost {
                id: Some("right".to_string()),
                address: None,
                name: None,
                unreachable: true,
            },
        ],
    };

    let read = serde_json::to_string(&directory).expect("the read body serializes");
    let frame = serde_json::to_string(&directory.as_frame()).expect("the frame serializes");

    assert_eq!(
        frame,
        format!(r#"{{"kind":"list",{}}}"#, &read[1..read.len() - 1]),
        "the frame is the read body under a kind: {frame}",
    );
    assert!(
        frame.contains(r#""preview":{"text":"hello","weight":18446744073709551616}"#),
        "a field this build has no type for reaches the client whole: {frame}",
    );

    let decoded: DecodedFrame = serde_json::from_str(&frame).expect("a client decodes it");
    let DecodedFrame::Known(known) = &decoded else {
        panic!("it is a list frame");
    };
    let Frame::List { sessions, hosts } = known.value() else {
        panic!("it is a list frame");
    };
    assert_eq!(
        sessions
            .iter()
            .map(|row| (row.id.as_str(), row.host.as_deref()))
            .collect::<Vec<_>>(),
        vec![("left:s-0", Some("left")), ("left:s-1", Some("left"))],
    );
    assert_eq!(hosts, &directory.hosts);
    assert_eq!(
        serde_json::from_str::<SessionList>(&read).expect("the read decodes"),
        SessionList {
            sessions: sessions.clone(),
            hosts: hosts.clone(),
        },
        "and the sessions read decodes into the same rows and hosts",
    );
}

/// A `list` frame with two rows: one from a host a version ahead, carrying a
/// field this build has no type for and a number literal no float survives.
const GATEWAY_ROWS: &str = r#"{"kind":"list","sessions":[{"id":"s-1","live":true,"working":false,"queued":{"steering":0,"follow_up":0},"tasks":0,"last_seq":7,"last_activity":"2026-08-03T12:00:00Z","unreachable":false,"preview":{"text":"hello","weight":18446744073709551616}},{"id":"s-0","live":false,"working":false,"queued":{"steering":0,"follow_up":0},"tasks":0,"last_activity":"2026-08-03T11:00:00Z","unreachable":false}]}"#;

/// One typed row, for the paths that have no wire JSON to start from.
fn pinned_row() -> SessionSummary {
    serde_json::from_value(fixture("models")["session_list"]["sessions"][1].clone())
        .expect("the pinned cold row decodes")
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
fn message_end_decodes_the_account_and_old_frames_without_it() {
    let decode = |json: &str| {
        let decoded: DecodedFrame = serde_json::from_str(json).expect("message_end decodes");
        let DecodedFrame::Known(frame) = decoded else {
            panic!("expected known frame");
        };
        let Frame::Event { event, .. } = frame.value() else {
            panic!("expected event frame");
        };
        let DecodedAgentEvent::Known(event) = event else {
            panic!("expected known event");
        };
        let AgentEvent::MessageEnd { message, .. } = event.value() else {
            panic!("expected message_end");
        };
        let Some(aj_models::types::Message::Assistant(message)) = message.as_stored_wire() else {
            panic!("expected assistant message");
        };
        message.account.clone()
    };

    let labeled = r#"{"kind":"event","session":"session-1","epoch":"epoch-1","seq":1,"entry_id":"entry-1","event":{"type":"message_end","agent_id":"main","message":{"role":"assistant","content":[],"api":"scripted","provider":"anthropic","model":"claude-test","account":"work","usage":{"input":0,"output":0,"cache_read":0,"cache_write":0,"total_tokens":0,"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0}},"stop_reason":"Stop","timestamp":10}}}"#;
    assert_eq!(decode(labeled).as_deref(), Some("work"));

    // A literal old frame, not a value built by today's serializer.
    // Only this direction proves a pre-account peer still decodes.
    let old = r#"{"kind":"event","session":"session-1","epoch":"epoch-1","seq":1,"entry_id":"entry-1","event":{"type":"message_end","agent_id":"main","message":{"role":"assistant","content":[],"api":"scripted","provider":"anthropic","model":"claude-test","usage":{"input":0,"output":0,"cache_read":0,"cache_write":0,"total_tokens":0,"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0}},"stop_reason":"Stop","timestamp":10}}}"#;
    assert_eq!(decode(old), None);
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
    assert_round_trip::<HostList>(&fixtures["hosts"]);
    assert_round_trip::<VmList>(&fixtures["vms"]);
    assert_round_trip::<ErrorResponse>(&fixtures["error"]);
}

/// The name a host reports for itself is display metadata it may not have: an
/// older host says nothing, and neither does one whose working directory made
/// no legal name. Absent is a key that is not there rather than an empty
/// string, in both directions, so a reader can tell "no name" from "a name
/// that is blank" and fall back to the id for the one case that means it.
#[test]
fn a_hosts_name_is_absent_rather_than_empty() {
    let hello: Hello = serde_json::from_value(fixture("models")["hello"].clone())
        .expect("the pinned hello decodes");
    assert_eq!(hello.name.as_deref(), Some("~/work/project"));
    assert_eq!(
        hello.working_directory.as_deref(),
        Some(std::path::Path::new("/home/dev/work/project")),
        "the pinned name is the one that directory derives",
    );

    let nameless = Hello {
        name: None,
        ..hello.clone()
    };
    let written = serde_json::to_value(&nameless).expect("a nameless hello serializes");
    assert!(
        written.get("name").is_none(),
        "an absent name is an absent key and never a null or an empty string: {written}",
    );
    let older: Hello = serde_json::from_value(written).expect("a hello with no name decodes");
    assert_eq!(older.name, None);
}

/// A field this build has never heard of does not cost the handshake, which
/// is what makes a name additive: it reached older clients as an unknown key
/// before they had a type for it (spec 6.10).
#[test]
fn a_hello_from_a_newer_host_still_decodes_with_its_name() {
    let mut newer = fixture("models")["hello"].clone();
    newer["fleet"] = json!({"region": "eu", "weight": 3});

    let decoded: Hello =
        serde_json::from_value(newer).expect("a hello with unknown fields decodes");
    assert_eq!(
        decoded.name.as_deref(),
        Some("~/work/project"),
        "and the fields this build does know arrive intact",
    );
    assert_eq!(decoded.host_id, "host-1");
}

/// What a legal host name is, for every peer that states, republishes or
/// paints one. Blank names nothing, so a caller has one case for "no name"
/// rather than two.
///
/// Control characters are refused rather than trimmed away: a name is painted
/// into a terminal, and the escape that would move its cursor must not
/// survive the trip as a label.
#[test]
fn a_host_name_is_one_trimmed_line_within_the_cap() {
    assert_eq!(
        normalize_host_name("  ~/work/umber/aj  "),
        Ok(Some("~/work/umber/aj".to_string())),
    );
    for blank in ["", "   ", "\t", " \n "] {
        assert_eq!(
            normalize_host_name(blank),
            Ok(None),
            "{blank:?} names nothing",
        );
    }

    assert_eq!(
        normalize_host_name("two\nlines"),
        Err(HostNameError::Control)
    );
    assert_eq!(
        normalize_host_name("\u{1b}[31mred"),
        Err(HostNameError::Control),
        "an escape sequence is not a label",
    );

    assert!(normalize_host_name(&"a".repeat(MAX_HOST_NAME_BYTES)).is_ok());
    assert_eq!(
        normalize_host_name(&"a".repeat(MAX_HOST_NAME_BYTES + 1)),
        Err(HostNameError::TooLong {
            bytes: MAX_HOST_NAME_BYTES + 1,
        }),
    );
    let padded = format!("  {}  ", "a".repeat(MAX_HOST_NAME_BYTES));
    assert!(padded.len() > MAX_HOST_NAME_BYTES);
    assert!(
        normalize_host_name(&padded).is_ok(),
        "the trim happens first, so padding cannot push a legal name over the cap",
    );
    let wide = "é".repeat(MAX_HOST_NAME_BYTES / 2 + 1);
    assert!(
        wide.chars().count() <= MAX_HOST_NAME_BYTES,
        "fits by characters"
    );
    assert!(
        matches!(
            normalize_host_name(&wide),
            Err(HostNameError::TooLong { .. })
        ),
        "but the cap is bytes, which is what the payload costs",
    );
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
        archived: false,
        locked: false,
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

/// A row says it is archived only when it is, and a row that says nothing is
/// not: absent reads as unarchived, which is what every row an older host
/// wrote says, and what keeps the key off the great majority of rows (spec
/// 6.10).
///
/// The bit is orthogonal to liveness. The fixture's archived row is the live
/// one deliberately: archiving a session that is up and working is allowed and
/// changes nothing but this field.
#[test]
fn an_archived_row_says_so_and_an_unarchived_one_stays_silent() {
    let list: SessionList = serde_json::from_value(fixture("models")["session_list"].clone())
        .expect("the pinned session list decodes");
    let [archived, plain] = &list.sessions[..] else {
        panic!("the fixture pins one archived row and one that is not");
    };
    assert_eq!((archived.archived, archived.live), (true, true));
    assert!(!plain.archived, "a row with no key is not archived");

    let row = SessionSummary {
        archived: false,
        ..pinned_row()
    };
    let encoded = serde_json::to_value(&row).expect("the row serializes");
    assert!(
        encoded.get("archived").is_none(),
        "an unarchived row emits no key: {encoded}",
    );
    assert_eq!(
        serde_json::from_value::<SessionSummary>(encoded).expect("it decodes again"),
        row,
        "and a row lacking the key reads back as unarchived",
    );

    let put_away = SessionSummary {
        archived: true,
        ..row
    };
    let encoded = serde_json::to_value(&put_away).expect("the row serializes");
    assert_eq!(encoded["archived"], json!(true));
    assert_eq!(
        serde_json::from_value::<SessionSummary>(encoded).expect("it decodes again"),
        put_away,
    );
}

/// The archive command's body: one bool, where `false` unarchives, so a
/// client needs no second route to put a session back. A blank body is the
/// same request, which is what the server's `{}` default reads it as.
#[test]
fn an_archive_request_carries_one_bool_and_defaults_to_unarchiving() {
    assert_eq!(
        serde_json::to_value(ArchiveRequest { archived: true }).unwrap(),
        json!({"archived": true}),
    );
    assert_eq!(
        serde_json::from_value::<ArchiveRequest>(json!({})).unwrap(),
        ArchiveRequest::default(),
    );
    assert!(!ArchiveRequest::default().archived);
    assert!(
        serde_json::from_value::<ArchiveRequest>(json!({"archived": false, "extra": 1}))
            .is_ok_and(|request| !request.archived),
        "a field this build has no type for is ignored (spec 6.10)",
    );
}

/// The enrollment request: one address and nothing else, which is all an
/// operator hands a gateway (spec 7.1). An address is required, because a
/// gateway cannot dial what it was not told, and a field a newer client sends
/// alongside it is ignored (spec 6.10).
#[test]
fn an_enrollment_request_carries_one_address() {
    assert_eq!(
        serde_json::to_value(EnrollHostRequest {
            address: "100.64.0.2:6161".to_string(),
        })
        .unwrap(),
        json!({"address": "100.64.0.2:6161"}),
    );
    assert_eq!(
        serde_json::from_value::<EnrollHostRequest>(json!({
            "address": "http://100.64.0.2:6161",
            "added_later": {"resources": 2}
        }))
        .expect("an addition a newer client sends is ignored")
        .address,
        "http://100.64.0.2:6161",
    );
    assert!(
        serde_json::from_value::<EnrollHostRequest>(json!({})).is_err(),
        "a gateway cannot dial an address it was never given",
    );
}

/// An enrolled-host row says only what the gateway knows: a host that has
/// never answered has no id to report, and one whose connection is up has no
/// failure to report (spec 7.1). Both are absent keys rather than empty
/// strings, in both directions, so a client can tell "not known" from "known
/// to be blank" and an older gateway's row still decodes.
#[test]
fn an_enrolled_host_row_reports_an_id_and_an_error_only_when_it_has_one() {
    let hosts: HostList = serde_json::from_value(fixture("models")["hosts"].clone())
        .expect("the pinned host list decodes");
    let [dynamic, configured] = &hosts.hosts[..] else {
        panic!("the fixture pins one dynamic row and one configured one");
    };
    assert_eq!(
        (dynamic.id.as_deref(), dynamic.source, dynamic.connected),
        (Some("workstation"), HostSource::Dynamic, true),
    );
    assert_eq!(dynamic.error, None, "a host that is up reports no failure");
    assert_eq!(
        (
            configured.id.as_deref(),
            configured.source,
            configured.connected,
        ),
        (None, HostSource::Config, false),
        "a configured host that has never answered cannot be given an id",
    );
    assert_eq!(configured.error.as_deref(), Some("connection refused"));

    let encoded = serde_json::to_value(&hosts).expect("the list re-serializes");
    assert!(
        encoded["hosts"][1].get("id").is_none(),
        "an unanswered host omits the key: {}",
        encoded["hosts"][1],
    );
    assert!(
        encoded["hosts"][0].get("error").is_none(),
        "a connected host omits the key: {}",
        encoded["hosts"][0],
    );
    assert_eq!(
        (
            &encoded["hosts"][0]["source"],
            &encoded["hosts"][1]["source"],
        ),
        (&json!("dynamic"), &json!("config")),
        "where an enrollment came from is one snake_case token",
    );

    let row = HostSummary {
        id: None,
        address: "http://100.64.0.9:6161".to_string(),
        source: HostSource::Config,
        connected: false,
        sessions: 0,
        error: None,
    };
    let encoded = serde_json::to_value(&row).expect("the row serializes");
    assert!(
        encoded.get("id").is_none() && encoded.get("error").is_none(),
        "a host nothing is known about yet emits neither key: {encoded}",
    );
    assert_eq!(
        serde_json::from_value::<HostSummary>(encoded).expect("it decodes again"),
        row,
        "and a row lacking both keys reads back as carrying neither",
    );

    let known = HostSummary {
        id: Some("workstation".to_string()),
        error: Some("connection refused".to_string()),
        ..row
    };
    let mut encoded = serde_json::to_value(&known).expect("the row serializes");
    assert_eq!(encoded["id"], json!("workstation"));
    assert_eq!(encoded["error"], json!("connection refused"));
    encoded["added_later"] = json!(true);
    assert_eq!(
        serde_json::from_value::<HostSummary>(encoded)
            .expect("a newer gateway's row decodes (spec 6.10)"),
        known,
    );
}

/// The tag command's body: one string, where blank means clear (spec 6.6), so
/// a client needs no second route to remove a label. A blank body is the same
/// request, which is what the server's `{}` default reads it as.
#[test]
fn a_tag_request_carries_one_string_and_defaults_to_clearing() {
    assert_eq!(
        serde_json::to_value(TagRequest {
            tag: "fix-auth".to_string(),
        })
        .unwrap(),
        json!({"tag": "fix-auth"}),
    );
    assert_eq!(
        serde_json::from_value::<TagRequest>(json!({})).unwrap(),
        TagRequest::default(),
    );
    assert_eq!(TagRequest::default().tag, "");

    // A creator can name one up front, and leaving it out is untagged.
    let create = CreateSessionRequest {
        tag: Some("fix-auth".to_string()),
        ..CreateSessionRequest::default()
    };
    let encoded = serde_json::to_value(&create).unwrap();
    assert_eq!(encoded, json!({"tag": "fix-auth"}));
    assert_eq!(
        serde_json::from_value::<CreateSessionRequest>(encoded)
            .unwrap()
            .tag,
        create.tag,
    );
    assert_eq!(
        serde_json::from_value::<CreateSessionRequest>(json!({}))
            .unwrap()
            .tag,
        None,
    );
}

/// The error frame is the error envelope of spec 6.6 with a session on it
/// (spec 6.3): the same `code` and `message` an error body carries, plus the
/// epoch when the error is about one.
///
/// The epoch is an absent key rather than a null when there is none, in both
/// directions, which is what an attach refusal writes: the session was never
/// resolved, so there is no epoch it could be about. Additive fields ride
/// along, and a client that does not know a `code` renders its `message`
/// (spec 6.10).
#[test]
fn an_error_frame_carries_the_envelope_and_an_optional_epoch() {
    let refusal = Frame::Error {
        session: "session-1".to_string(),
        epoch: None,
        code: "unknown_session".to_string(),
        message: "unknown session session-1".to_string(),
    };
    let encoded = serde_json::to_value(&refusal).expect("the frame serializes");
    assert_eq!(
        encoded,
        json!({
            "kind": "error",
            "session": "session-1",
            "code": "unknown_session",
            "message": "unknown session session-1",
        }),
        "an absent epoch is an absent key and never a null: {encoded}",
    );

    let scoped: Frame = serde_json::from_value(json!({
        "kind": "error",
        "session": "session-1",
        "epoch": "epoch-1",
        "code": "stale_branch",
        "message": "that branch is gone",
        "added_later": {"entry": "entry-7"},
    }))
    .expect("a newer peer's error frame decodes");
    let Frame::Error {
        session,
        epoch,
        code,
        message,
    } = &scoped
    else {
        panic!("expected an error frame, got {scoped:?}");
    };
    assert_eq!(
        (session.as_str(), epoch.as_deref()),
        ("session-1", Some("epoch-1")),
    );
    assert_eq!(
        (code.as_str(), message.as_str()),
        ("stale_branch", "that branch is gone"),
        "an unknown code decodes and carries its own sentence",
    );

    for missing in [
        json!({"kind": "error", "session": "session-1", "message": "no code"}),
        json!({"kind": "error", "session": "session-1", "code": "no_message"}),
        json!({"kind": "error", "code": "no_session", "message": "no session"}),
    ] {
        assert!(
            serde_json::from_value::<Frame>(missing.clone()).is_err(),
            "an error frame names a session, a code and a message: {missing}",
        );
    }
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
        Frame::Error { .. } => "error",
        Frame::Reset { .. } => "reset",
        Frame::Heartbeat => "heartbeat",
        Frame::Vms { .. } => "vms",
    }
}

/// Whether a frame kind carries the top-level `session` a gateway rewrites
/// (spec 6.3), stated here rather than read off the library so that the two
/// can be held against each other.
///
/// The match is exhaustive on purpose. A new frame variant does not compile
/// until it declares which side of the rewrite it is on.
fn frame_carries_session(frame: &Frame) -> bool {
    match frame {
        Frame::Event { .. }
        | Frame::State { .. }
        | Frame::CaughtUp { .. }
        | Frame::Error { .. }
        | Frame::Reset { .. } => true,
        Frame::List { .. } | Frame::Heartbeat | Frame::Vms { .. } => false,
    }
}

/// One frame per variant, built in process, so none of them retains any JSON.
fn local_frames() -> Vec<Frame> {
    let notice: AgentEvent = serde_json::from_value(json!({
        "type": "notice",
        "agent_id": "main",
        "text": "hello"
    }))
    .expect("the notice event decodes");
    vec![
        Frame::Event {
            session: "old".to_string(),
            epoch: "epoch-1".to_string(),
            durability: None,
            event: DecodedAgentEvent::from(notice),
        },
        Frame::State {
            session: "old".to_string(),
            epoch: "epoch-1".to_string(),
            working: true,
            settings: AgentSettings {
                provider: "scripted".to_string(),
                model_id: "scripted-model".to_string(),
                thinking: "off".to_string(),
                thinking_display: "default".to_string(),
                speed: "standard".to_string(),
                verbosity: "default".to_string(),
            },
            last_seq: 7,
        },
        Frame::CaughtUp {
            session: "old".to_string(),
            epoch: "epoch-1".to_string(),
            last_seq: 7,
        },
        Frame::List {
            sessions: Vec::new(),
            hosts: Vec::new(),
        },
        Frame::Error {
            session: "old".to_string(),
            // The shape an attach refusal takes: the session was never
            // resolved, so there is no epoch it could be about.
            epoch: None,
            code: "unknown_session".to_string(),
            message: "unknown session old".to_string(),
        },
        Frame::Reset {
            session: "old".to_string(),
        },
        Frame::Heartbeat,
        Frame::Vms { vms: Vec::new() },
    ]
}

/// The session a known frame's typed value reports, which is what a gateway
/// routes on while the retained JSON is what it forwards.
fn known_session(frame: &DecodedFrame) -> Option<&str> {
    let DecodedFrame::Known(known) = frame else {
        panic!("expected a known frame");
    };
    known.value().session()
}

/// The top-level fields of the JSON a frame forwards, keyed by name, each
/// value kept as the exact text that was emitted.
///
/// This is the comparison spec 6.10 calls for: top-level key order is not
/// significant, and everything below the top level travels verbatim, which is
/// what re-emitting a frame unchanged means for a forwarder that does not
/// understand it. Decoding into [`Value`] would be weaker, it rounds a number
/// literal an unknown payload may carry and refuses `1e400` outright. It also
/// keeps only one of a duplicated key, so a test about duplicates works on the
/// text instead.
fn top_level_fields(frame: &DecodedFrame) -> BTreeMap<String, String> {
    let json = serde_json::to_string(frame).expect("a frame serializes");
    serde_json::from_str::<BTreeMap<String, Box<RawValue>>>(&json)
        .expect("a frame is a JSON object")
        .into_iter()
        .map(|(key, value)| (key, value.get().to_string()))
        .collect()
}

/// Asserts a rewrite moved the top-level `session` to `replacement` and left
/// every other top-level field exactly as it was.
fn assert_rewrote_session(before: &DecodedFrame, after: &DecodedFrame, replacement: &str) {
    let mut expected = top_level_fields(before);
    let previous = expected.insert(
        "session".to_string(),
        serde_json::to_string(replacement).expect("a session id is a JSON string"),
    );
    assert!(previous.is_some(), "the frame carried a top-level session");
    assert_eq!(top_level_fields(after), expected);
}

/// Asserts the reader and the rewrite answer for the same field: an id comes
/// back exactly when the rewrite finds one to replace, and reading the
/// rewritten frame gives the id that went in.
fn assert_reads_what_the_rewrite_writes(frame: &DecodedFrame) {
    let json = serde_json::to_string(frame).expect("a frame serializes");
    let read = frame
        .session()
        .expect("a well-formed frame reads its session");

    let mut rewritten = frame.clone();
    let carries = rewritten
        .rewrite_session("gateway:new")
        .expect("the rewrite runs");
    assert_eq!(read.is_some(), carries, "{json}");

    let expected = carries.then_some("gateway:new");
    assert_eq!(
        rewritten
            .session()
            .expect("a rewritten frame reads its session")
            .as_deref(),
        expected,
        "{json}",
    );
}
