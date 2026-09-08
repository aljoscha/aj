use std::future::{Future, poll_fn};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_agent::message::AgentMessage;
use aj_agent::tool::ToolDetails;
use aj_app::outbound::{Offered, OutboundFrame, Receiver, Sender, channel};
use aj_models::streaming::AssistantMessageEvent;
use aj_models::types::{AssistantMessage, Message};
use aj_wire::{DecodedFrame, DurableEvent, Frame};
use serde::Serialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn queue<T: OutboundFrame>(capacity: usize) -> (Sender<T>, Receiver<T>, CancellationToken) {
    let cancelled = CancellationToken::new();
    let (tx, rx) = channel(NonZeroUsize::new(capacity).unwrap(), cancelled.clone());
    (tx, rx, cancelled)
}

fn decoded(frame: Frame) -> DecodedFrame {
    DecodedFrame::try_from(frame).unwrap()
}

fn value(frame: impl Serialize) -> Value {
    serde_json::to_value(frame).unwrap()
}

fn drain<T: OutboundFrame + Serialize>(rx: &mut Receiver<T>) -> Vec<Value> {
    std::iter::from_fn(|| rx.try_recv()).map(value).collect()
}

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("outbound operation did not wake")
}

fn poll_once<F: Future>(future: F) -> Poll<F::Output> {
    std::pin::pin!(future).poll(&mut Context::from_waker(Waker::noop()))
}

// Signal the caller only after registering this task's real waker. No fresh
// poll follows that signal until the operation wakes the task itself.
async fn parked<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut future = std::pin::pin!(future);
        let mut ready = Some(ready);
        poll_fn(|cx| {
            let polled = future.as_mut().poll(cx);
            if let Some(ready) = ready.take() {
                assert!(polled.is_pending());
                ready.send(()).unwrap();
            }
            polled
        })
        .await
    });
    bounded(waiting).await.unwrap();
    task
}

fn event(event: AgentEvent) -> Frame {
    Frame::Event {
        session: "left:s-1".into(),
        epoch: "epoch-1".into(),
        durability: None,
        event: event.into(),
    }
}

fn reliable(text: &str) -> Frame {
    event(AgentEvent::Warning {
        agent_id: AgentId::Main,
        text: text.into(),
    })
}

fn durable(seq: u64) -> Frame {
    let mut frame = event(AgentEvent::Notice {
        agent_id: AgentId::Main,
        text: format!("entry {seq}"),
    });
    if let Frame::Event { durability, .. } = &mut frame {
        *durability = Some(DurableEvent {
            seq,
            entry_id: format!("entry-{seq}"),
        });
    }
    frame
}

fn state(last_seq: u64) -> Frame {
    Frame::State {
        session: "left:s-1".into(),
        epoch: "epoch-1".into(),
        working: true,
        settings: AgentSettings {
            provider: "scripted".into(),
            model_id: "scripted".into(),
            thinking: "off".into(),
            thinking_display: "default".into(),
            speed: "standard".into(),
            verbosity: "default".into(),
        },
        credential_warning: None,
        last_seq,
    }
}

fn snapshots() -> Vec<Frame> {
    let mut frames = vec![state(1)];
    for agent_id in [AgentId::Main, AgentId::Sub(1)] {
        let partial = AssistantMessage::empty();
        frames.push(event(AgentEvent::MessageUpdate {
            agent_id,
            message: AgentMessage::wire(Message::Assistant(partial.clone())),
            event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "tick".into(),
                partial,
            },
        }));
    }
    for task_id in [7, 8] {
        frames.push(event(AgentEvent::TaskOutput {
            agent_id: AgentId::Main,
            task_id,
            call_id: "call-a".into(),
            partial: ToolDetails::Text {
                summary: "running".into(),
                body: String::new(),
            },
        }));
    }
    for call_id in ["call-a", "call-b"] {
        frames.push(event(AgentEvent::ToolExecutionUpdate {
            agent_id: AgentId::Main,
            call_id: call_id.into(),
            tool: "bash".into(),
            args: json!({}),
            partial: ToolDetails::Text {
                summary: "running".into(),
                body: String::new(),
            },
            content: Arc::from(Vec::new()),
        }));
    }
    let other_session = frames
        .iter()
        .cloned()
        .map(|mut frame| {
            match &mut frame {
                Frame::Event { session, .. } | Frame::State { session, .. } => {
                    *session = "right:s-1".into();
                }
                _ => unreachable!("session-scoped snapshots"),
            }
            frame
        })
        .collect::<Vec<_>>();
    frames.extend(other_session);
    frames.push(Frame::List {
        sessions: vec![],
        hosts: vec![],
    });
    frames.push(Frame::Vms { vms: vec![] });
    frames
}

fn key_contract<T: OutboundFrame + Serialize>(convert: impl Fn(Frame) -> T) {
    let frames = snapshots();
    for replacement in &frames {
        let (tx, mut rx, cancelled) = queue(frames.len() + 1);
        for frame in &frames {
            assert_eq!(tx.offer(convert(frame.clone())), Offered::Queued);
        }
        assert_eq!(tx.offer(convert(durable(1))), Offered::Queued);
        assert_eq!(tx.offer(convert(replacement.clone())), Offered::Queued);
        let mut expected: Vec<_> = frames.iter().map(value).collect();
        expected.retain(|frame| frame != &value(replacement));
        expected.extend([value(durable(1)), value(replacement)]);
        assert_eq!(drain(&mut rx), expected);
        assert!(!cancelled.is_cancelled());
    }
}

#[test]
fn every_snapshot_key_is_independent_and_replaced_at_the_tail_at_full_capacity() {
    key_contract(|frame| frame);
    key_contract(decoded);
}

fn overflow_contract<T: OutboundFrame + Serialize + Clone>(lossy: Vec<T>, reliable: Vec<T>) {
    for frame in lossy {
        let (tx, mut rx, cancelled) = queue(1);
        assert_eq!(tx.offer(reliable[0].clone()), Offered::Queued);
        assert_eq!(tx.offer(frame), Offered::Dropped);
        assert!(!cancelled.is_cancelled());
        assert_eq!(drain(&mut rx), vec![value(&reliable[0])]);
    }
    for frame in reliable {
        let (tx, mut rx, cancelled) = queue(2);
        assert_eq!(tx.offer(frame.clone()), Offered::Queued);
        assert_eq!(tx.offer(frame.clone()), Offered::Queued);
        assert_eq!(drain(&mut rx), vec![value(&frame), value(&frame)]);
        assert_eq!(tx.offer(frame.clone()), Offered::Queued);
        assert_eq!(tx.offer(frame.clone()), Offered::Queued);
        assert_eq!(tx.offer(frame.clone()), Offered::Evicted);
        assert!(cancelled.is_cancelled());
        assert!(rx.try_recv().is_none());
        assert!(matches!(poll_once(rx.recv()), Poll::Ready(None)));
        assert_eq!(tx.offer(frame), Offered::Evicted);
    }
}

#[test]
fn snapshots_drop_but_durable_reliable_and_unknown_frames_evict() {
    let reliable = vec![
        durable(1),
        reliable("warning"),
        Frame::Heartbeat,
        Frame::CaughtUp {
            session: "s".into(),
            epoch: "e".into(),
            last_seq: 1,
        },
        Frame::Reset {
            session: "s".into(),
        },
        Frame::Error {
            session: "s".into(),
            epoch: None,
            code: "unknown_session".into(),
            message: "no session here".into(),
            lock_generation: None,
        },
        serde_json::from_value(json!({
            "kind": "event", "session": "s", "epoch": "e",
            "event": {"type": "future_event", "payload": [1, 2]}
        }))
        .unwrap(),
    ];
    overflow_contract(snapshots(), reliable.clone());
    let mut decoded_reliable: Vec<_> = reliable.into_iter().map(decoded).collect();
    decoded_reliable.push(serde_json::from_str(r#"{"kind":"future_frame","payload":42}"#).unwrap());
    overflow_contract(
        snapshots().into_iter().map(decoded).collect(),
        decoded_reliable,
    );
}

#[tokio::test]
async fn decoded_payloads_survive_replacement_pacing_and_retention_verbatim() {
    let old = r#"{"kind":"list","sessions":[],"hosts":[],"extra":{"n":1.2300e+40}}"#;
    let latest = r#"{"kind":"list","sessions":[],"hosts":[],"extra":{"n":9.8700e+40}}"#;
    let unknown = r#"{"kind":"future_frame","payload":{ "n": 1.2300e+40 }}"#;
    let (tx, mut rx, _) = queue::<DecodedFrame>(2);
    assert!(bounded(tx.send_paced(serde_json::from_str(old).unwrap())).await);
    assert_eq!(
        tx.offer(serde_json::from_str(old).unwrap()),
        Offered::Queued
    );
    assert_eq!(
        tx.offer(serde_json::from_str(unknown).unwrap()),
        Offered::Queued
    );
    assert_eq!(
        tx.offer(serde_json::from_str(latest).unwrap()),
        Offered::Queued
    );
    tx.retain(|_| true);
    for expected in [old, unknown, latest] {
        let frame = rx.try_recv().unwrap();
        let raw = match &frame {
            DecodedFrame::Known(frame) => frame.raw_json().unwrap().get(),
            DecodedFrame::Unknown { raw, .. } => raw.get(),
        };
        assert_eq!(raw, expected);
        assert_eq!(serde_json::to_string(&frame).unwrap(), expected);
    }
    assert!(rx.try_recv().is_none());
}

#[tokio::test]
async fn paced_snapshots_never_coalesce_in_either_direction_or_spend_live_capacity() {
    let (tx, mut rx, _) = queue::<Frame>(2);
    assert_eq!(tx.offer(state(1)), Offered::Queued);
    assert!(bounded(tx.send_paced(state(2))).await);
    assert_eq!(drain(&mut rx), [state(1), state(2)].map(value));

    let (tx, mut rx, cancelled) = queue::<Frame>(3);
    assert_eq!(tx.offer(state(1)), Offered::Queued);
    assert!(bounded(tx.send_paced(state(2))).await);
    assert!(bounded(tx.send_paced(state(3))).await);
    assert_eq!(tx.offer(state(4)), Offered::Queued);
    assert_eq!(tx.offer(reliable("one")), Offered::Queued);
    assert_eq!(tx.offer(reliable("two")), Offered::Queued);
    assert_eq!(
        tx.offer(Frame::List {
            sessions: vec![],
            hosts: vec![]
        }),
        Offered::Dropped
    );
    assert_eq!(
        drain(&mut rx),
        [
            state(2),
            state(3),
            state(4),
            reliable("one"),
            reliable("two")
        ]
        .map(value)
    );
    assert!(!cancelled.is_cancelled());
}

#[tokio::test]
async fn pacing_waits_on_the_whole_queue_and_both_receive_paths_release_room() {
    for paced_prefix in [false, true] {
        for nonblocking in [false, true] {
            let (tx, mut rx, cancelled) = queue::<Frame>(1);
            if paced_prefix {
                assert!(bounded(tx.send_paced(state(1))).await);
            } else {
                assert_eq!(tx.offer(state(1)), Offered::Queued);
            }
            let producer = tx.clone();
            let waiting = parked(async move { producer.send_paced(state(2)).await }).await;
            assert!(!cancelled.is_cancelled());
            let received = if nonblocking {
                rx.try_recv()
            } else {
                bounded(rx.recv()).await
            };
            assert_eq!(value(received.unwrap()), value(state(1)));
            assert!(bounded(waiting).await.unwrap());
            assert_eq!(drain(&mut rx), vec![value(state(2))]);
        }
    }
}

#[tokio::test]
async fn retain_releases_only_removed_live_capacity_and_wakes_paced_producers() {
    let (tx, mut rx, _) = queue::<Frame>(2);
    assert!(bounded(tx.send_paced(state(1))).await);
    assert_eq!(tx.offer(durable(1)), Offered::Queued);
    assert_eq!(tx.offer(durable(2)), Offered::Queued);
    // Removing a paced frame must not grant an extra live slot.
    tx.retain(|frame| !matches!(frame, Frame::State { .. }));
    assert_eq!(tx.offer(state(2)), Offered::Dropped);
    let producer = tx.clone();
    let waiting = parked(async move { producer.send_paced(state(3)).await }).await;
    tx.retain(|frame| frame.durable_seq() != Some(1));
    assert!(bounded(waiting).await.unwrap());
    assert_eq!(tx.offer(durable(3)), Offered::Queued);
    assert_eq!(tx.offer(state(4)), Offered::Dropped);
    assert_eq!(
        drain(&mut rx),
        [durable(2), state(3), durable(3)].map(value)
    );
    assert_eq!(tx.offer(durable(4)), Offered::Queued);
    assert_eq!(tx.offer(durable(5)), Offered::Queued);
    assert_eq!(tx.offer(durable(6)), Offered::Evicted);
}

#[tokio::test]
async fn external_cancellation_releases_pacing_but_leaves_queue_lifecycle_to_owner() {
    let (tx, mut rx, cancelled) = queue::<Frame>(1);
    assert_eq!(tx.offer(durable(1)), Offered::Queued);
    let producer = tx.clone();
    let waiting = parked(async move { producer.send_paced(durable(2)).await }).await;
    cancelled.cancel();
    assert!(!bounded(waiting).await.unwrap());
    assert_eq!(drain(&mut rx), vec![value(durable(1))]);
    let reading = parked(async move { rx.recv().await }).await;
    assert_eq!(tx.offer(durable(3)), Offered::Queued);
    assert_eq!(
        value(bounded(reading).await.unwrap().unwrap()),
        value(durable(3))
    );
}

#[tokio::test]
async fn waiting_receivers_wake_on_live_paced_close_and_evict() {
    for action in ["live", "paced", "close", "evict"] {
        let (tx, mut rx, cancelled) = queue::<Frame>(1);
        let reading = parked(async move { rx.recv().await }).await;
        match action {
            "live" => assert_eq!(tx.offer(state(1)), Offered::Queued),
            "paced" => assert!(bounded(tx.send_paced(state(1))).await),
            "close" => tx.close(),
            "evict" => tx.evict(),
            _ => unreachable!(),
        }
        let expected = matches!(action, "live" | "paced").then(|| value(state(1)));
        assert_eq!(bounded(reading).await.unwrap().map(value), expected);
        assert_eq!(cancelled.is_cancelled(), action == "evict");
    }
}

#[tokio::test]
async fn close_drains_in_order_while_eviction_clears_and_both_stop_paced_waiters() {
    for evict in [false, true] {
        let (tx, mut rx, cancelled) = queue::<Frame>(1);
        assert!(bounded(tx.send_paced(state(1))).await);
        assert_eq!(tx.offer(durable(1)), Offered::Queued);
        let producer = tx.clone();
        let waiting = parked(async move { producer.send_paced(durable(2)).await }).await;
        if evict {
            tx.evict();
        } else {
            tx.close();
        }
        assert!(!bounded(waiting).await.unwrap());
        assert_eq!(tx.offer(durable(3)), Offered::Evicted);
        assert!(!bounded(tx.send_paced(durable(4))).await);
        if !evict {
            assert_eq!(value(bounded(rx.recv()).await.unwrap()), value(state(1)));
            assert_eq!(value(rx.try_recv().unwrap()), value(durable(1)));
        }
        assert!(bounded(rx.recv()).await.is_none());
        assert!(rx.try_recv().is_none());
        assert_eq!(cancelled.is_cancelled(), evict);
    }
}

#[tokio::test]
async fn dropping_handles_does_not_implicitly_close_or_cancel() {
    let (tx, mut rx, cancelled) = queue::<Frame>(1);
    assert_eq!(tx.offer(state(1)), Offered::Queued);
    drop(tx);
    assert_eq!(drain(&mut rx), vec![value(state(1))]);
    assert!(poll_once(rx.recv()).is_pending());
    assert!(!cancelled.is_cancelled());

    let (tx, rx, cancelled) = queue::<Frame>(1);
    drop(rx);
    assert!(bounded(tx.send_paced(state(1))).await);
    assert_eq!(tx.clone().offer(durable(1)), Offered::Queued);
    assert!(!cancelled.is_cancelled());
}
