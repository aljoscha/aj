use super::*;
use aj_app::chat::SubAgentStatus;
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::streaming::AssistantMessageEventStream;
use aj_models::types::{Context, Message, SimpleStreamOptions, StreamOptions};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Inference {
    context: Context,
    stream: AssistantMessageEventStream,
    cancel: CancellationToken,
}

impl Inference {
    fn answer(&self, message: AssistantMessage) {
        assert!(
            !self.cancel.is_cancelled(),
            "answering a cancelled inference"
        );
        for step in aj_models::scripted::script_from_message(message, 0, Duration::ZERO).steps {
            self.stream.push(step.event);
        }
        self.stream.end();
    }
}

// Every inference stays open until the test answers it or the host cancels it.
// Receiving a request proves cancellation is aimed at an actual running turn.
struct HeldProvider {
    requests: mpsc::UnboundedSender<Inference>,
    panic_next: Arc<std::sync::atomic::AtomicBool>,
}

impl Provider for HeldProvider {
    fn stream(&self, _: &ModelInfo, _: &Context, _: &StreamOptions) -> AssistantMessageEventStream {
        panic!("the agent supplies simple stream options");
    }

    fn stream_simple(
        &self,
        _: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        assert!(
            !self
                .panic_next
                .swap(false, std::sync::atomic::Ordering::SeqCst),
            "injected provider panic"
        );
        let stream = AssistantMessageEventStream::new();
        assert!(
            self.requests
                .send(Inference {
                    context: context.clone(),
                    stream: stream.clone(),
                    cancel: options.base.cancel.clone().expect("turn cancellation"),
                })
                .is_ok()
        );
        stream
    }
}

fn held() -> (Harness, mpsc::UnboundedReceiver<Inference>) {
    let (h, requests, _) = held_with_panic();
    (h, requests)
}

fn held_with_panic() -> (
    Harness,
    mpsc::UnboundedReceiver<Inference>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let panic_next = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut run = snapshot(scripted(Vec::new(), 0, Duration::ZERO));
    run.main.provider = Arc::new(HeldProvider {
        requests: sender,
        panic_next: Arc::clone(&panic_next),
    });
    (
        Harness::with_run_config(run, Vec::new(), None, None),
        receiver,
        panic_next,
    )
}

async fn next(receiver: &mut mpsc::UnboundedReceiver<Inference>) -> Inference {
    let inference = bounded("provider request", receiver.recv()).await.unwrap();
    assert!(!inference.cancel.is_cancelled());
    inference
}

async fn command(h: &Harness, session: &str, command: Command) {
    bounded("host command", h.host.command(session, command))
        .await
        .unwrap();
}

async fn resume(h: &Harness, session: &str, child: usize, text: &str) {
    command(
        h,
        session,
        Command::Prompt {
            agent: AgentId::Sub(child),
            content: vec![UserContent::text(text)],
        },
    )
    .await;
}

fn apply(client: &mut Client, frames: &[Frame]) {
    for frame in frames {
        let _ = client.client.apply(&mut client.chat, frame.clone());
    }
}

async fn interrupt(h: &Harness, session: &str, client: &mut Client, child: usize) -> Vec<Frame> {
    command(
        h,
        session,
        Command::Cancel {
            agent: AgentId::Sub(child),
        },
    )
    .await;
    let frames = frames_until(&mut client.stream, "child interrupted", |frame| {
        matches!(frame,
            Frame::Event { event, .. } if matches!(event.known(),
                Some(AgentEvent::AgentEnd { agent_id, assignment_pending: true, .. }) if *agent_id == AgentId::Sub(child))
        )
    })
    .await;
    apply(client, &frames);
    assert_assigned_idle(client, child);
    frames
}

fn assert_assigned_idle(client: &Client, child: usize) {
    assert_eq!(
        sub_box(&client.canonical(), child),
        (SubAgentStatus::Running, false)
    );
    assert!(!client.client.lifecycle().is_running(AgentId::Sub(child)));
    assert!(client.client.working(), "parent must remain pending");
}

fn result_count(frames: &[Frame]) -> usize {
    events(frames).iter().filter(|event| matches!(event,
        AgentEvent::ToolExecutionEnd { agent_id: AgentId::Main, call_id, .. } if call_id == "original"
    )).count()
}

fn parent_result(inference: &Inference) -> &aj_models::types::ToolResultMessage {
    let results: Vec<_> = inference
        .context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "original" => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(
        results.len(),
        1,
        "the original call must be fulfilled exactly once"
    );
    results[0]
}

async fn delegate(
    h: &Harness,
    session: &str,
    requests: &mut mpsc::UnboundedReceiver<Inference>,
) -> Inference {
    h.prompt(session, "delegate the investigation").await;
    next(requests).await.answer(calling(
        "delegating",
        "original",
        "agent",
        json!({"task":"investigate original evidence"}),
    ));
    next(requests).await
}

#[tokio::test]
async fn prompt_at_interrupted_continuation_end_runs_after_teardown() {
    let (h, mut requests) = held();
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    let initial = delegate(&h, &session, &mut requests).await;
    let mut frames = interrupt(&h, &session, &mut client, 1).await;
    assert!(initial.cancel.is_cancelled());
    let handles = h.host.local_handles(&session).await.unwrap();
    let child = handles.registry.get(1).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let _subscription = bounded("child lock", child.lock()).await.subscribe({
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        Arc::new(move |event| {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            Box::pin(async move {
                if matches!(
                    event,
                    AgentEvent::AgentEnd {
                        agent_id: AgentId::Sub(1),
                        assignment_pending: true,
                        ..
                    }
                ) {
                    entered.notify_one();
                    release.notified().await;
                }
                Ok(())
            })
        })
    });
    resume(&h, &session, 1, "first continuation").await;
    let continuation = next(&mut requests).await;
    frames.extend(interrupt(&h, &session, &mut client, 1).await);
    bounded("held turn-end delivery", entered.notified()).await;
    assert!(continuation.cancel.is_cancelled());
    // The client sees an idle turn while its task is still winding down.
    // The accepted prompt must run after that task joins, not need another prompt.
    resume(&h, &session, 1, "resume during teardown").await;
    release.notify_one();
    let resumed = next(&mut requests).await;
    assert!(
        serde_json::to_string(&resumed.context)
            .unwrap()
            .contains("resume during teardown")
    );
    resumed.answer(finalized_text_message("verified report"));
    let parent = next(&mut requests).await;
    assert!(!parent_result(&parent).is_error);
    parent.answer(finalized_text_message("assignment completed"));
    frames.extend(client.pump_until_idle().await);
    assert_eq!(result_count(&frames), 1);
    assert_no_dangling(&client.chat);
    h.host.shutdown().await;
}

#[tokio::test]
async fn resumed_provider_panic_fails_original_call_without_cancelling_parent() {
    let (h, mut requests, panic_next) = held_with_panic();
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    let first = delegate(&h, &session, &mut requests).await;
    let mut frames = interrupt(&h, &session, &mut client, 1).await;
    assert!(first.cancel.is_cancelled());
    panic_next.store(true, std::sync::atomic::Ordering::SeqCst);
    resume(&h, &session, 1, "resume and encounter provider panic").await;
    let parent = next(&mut requests).await;
    assert!(!panic_next.load(std::sync::atomic::Ordering::SeqCst));
    assert!(parent_result(&parent).is_error);
    parent.answer(finalized_text_message("continuing after child failure"));
    frames.extend(client.pump_until_idle().await);
    assert_eq!(result_count(&frames), 1);
    assert!(!notice(&frames, CANCELLED));
    assert!(
        assistant_rows(&client.chat, AgentId::Main)
            .iter()
            .any(|text| text == "continuing after child failure")
    );
    assert_no_dangling(&client.chat);
    h.host.shutdown().await;
}

#[tokio::test]
async fn queued_follow_up_survives_successful_initial_assignment_completion() {
    let (h, mut requests) = held();
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    let child = delegate(&h, &session, &mut requests).await;
    resume(&h, &session, 1, "follow up after the original report").await;
    assert!(!child.cancel.is_cancelled());
    child.answer(finalized_text_message("original report"));

    // The parent and the queued child turn can reach inference in either order.
    let a = next(&mut requests).await;
    let b = next(&mut requests).await;
    let is_parent = |request: &Inference| {
        request.context.messages.iter().any(|message|
        matches!(message, Message::ToolResult(result) if result.tool_call_id == "original"))
    };
    assert_ne!(is_parent(&a), is_parent(&b));
    let (parent, follow_up) = if is_parent(&a) { (a, b) } else { (b, a) };
    assert!(!parent_result(&parent).is_error);
    assert!(
        serde_json::to_string(&parent_result(&parent).content)
            .unwrap()
            .contains("original report")
    );
    let context = serde_json::to_string(&follow_up.context).unwrap();
    assert!(context.contains("follow up after the original report"));
    assert!(context.contains("original report"));
    parent.answer(finalized_text_message("parent finished"));
    let mut frames = client.pump_until_idle().await;
    assert!(
        !follow_up.cancel.is_cancelled(),
        "completing the parent must not cancel queued child work"
    );
    follow_up.answer(finalized_text_message("follow-up report"));
    let completion = frames_until(&mut client.stream, "follow-up report", |frame| {
        matches!(frame, Frame::Event { event, .. } if matches!(event.known(),
            Some(AgentEvent::AgentEnd { agent_id: AgentId::Sub(1), .. })))
    })
    .await;
    apply(&mut client, &completion);
    frames.extend(completion);
    assert!(
        assistant_rows(&client.chat, AgentId::Sub(1))
            .iter()
            .any(|text| text == "follow-up report")
    );
    assert_eq!(result_count(&frames), 1);
    assert!(!notice(&frames, CANCELLED));
    h.host.shutdown().await;
}

#[tokio::test]
async fn queued_follow_up_wakes_interrupted_initial_and_continued_assignments() {
    for continued in [false, true] {
        let (h, mut requests) = held();
        let session = h.create().await;
        let mut client = Client::attach(&h.host, &session).await;
        let initial = delegate(&h, &session, &mut requests).await;
        let mut frames = Vec::new();
        let running = if continued {
            frames.extend(interrupt(&h, &session, &mut client, 1).await);
            assert!(initial.cancel.is_cancelled());
            resume(&h, &session, 1, "continue investigating").await;
            next(&mut requests).await
        } else {
            initial
        };
        resume(&h, &session, 1, "queued follow-up evidence").await;
        assert!(!running.cancel.is_cancelled());
        frames.extend(interrupt(&h, &session, &mut client, 1).await);
        assert!(running.cancel.is_cancelled());
        assert_eq!(result_count(&frames), 0);
        let awakened = next(&mut requests).await;
        assert!(
            serde_json::to_string(&awakened.context)
                .unwrap()
                .contains("queued follow-up evidence")
        );
        awakened.answer(finalized_text_message("follow-up evidence verified"));
        let parent = next(&mut requests).await;
        assert!(!parent_result(&parent).is_error);
        assert!(
            serde_json::to_string(&parent_result(&parent).content)
                .unwrap()
                .contains("follow-up evidence verified")
        );
        parent.answer(finalized_text_message("completed after ordinary wake"));
        frames.extend(client.pump_until_idle().await);
        assert_eq!(result_count(&frames), 1);
        assert_no_dangling(&client.chat);
        h.host.shutdown().await;
    }
}

#[tokio::test]
async fn background_task_notice_wakes_idle_assignment_and_fulfills_original_call() {
    let (h, mut requests) = held();
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    let child = delegate(&h, &session, &mut requests).await;
    let gate = h._dir.path().join("evidence-gate");
    child.answer(calling("wait for evidence", "wait", "bash", json!({
        "command": format!("mkfifo {gate:?}; read -r evidence < {gate:?}; printf '%s' \"$evidence\""),
        "description": "collect evidence", "run_in_background": true
    })));
    let running = next(&mut requests).await;
    let handles = h.host.local_handles(&session).await.unwrap();
    let tasks = handles.task_registry.snapshot();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].owner, AgentId::Sub(1));
    assert_eq!(tasks[0].status, TaskStatus::Running);
    let mut frames = interrupt(&h, &session, &mut client, 1).await;
    assert!(running.cancel.is_cancelled());
    bounded("evidence FIFO ready", async {
        while !gate.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    bounded(
        "release evidence",
        tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new()
                .write(true)
                .open(gate)
                .unwrap()
                .write_all(b"verified background evidence\n")
                .unwrap();
        }),
    )
    .await
    .unwrap();
    let awakened = next(&mut requests).await;
    let context = serde_json::to_string(&awakened.context).unwrap();
    assert!(context.contains("verified background evidence"));
    awakened.answer(finalized_text_message("background evidence report"));
    let parent = next(&mut requests).await;
    assert!(!parent_result(&parent).is_error);
    assert!(
        serde_json::to_string(&parent_result(&parent).content)
            .unwrap()
            .contains("background evidence report")
    );
    parent.answer(finalized_text_message("completed after task notice"));
    frames.extend(client.pump_until_idle().await);
    assert_eq!(result_count(&frames), 1);
    assert_no_dangling(&client.chat);
    h.host.shutdown().await;
}

#[tokio::test]
async fn interrupting_then_killing_one_foreground_sibling_leaves_the_other_live() {
    let (h, mut requests) = held();
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    h.prompt(&session, "investigate two independent clues")
        .await;
    let mut calls = calling(
        "two investigators",
        "original",
        "agent",
        json!({"task":"first clue"}),
    );
    calls.content.push(AssistantContent::ToolCall(ToolCall {
        is_raw: false,
        id: "sibling".into(),
        name: "agent".into(),
        arguments: json!({"task":"second clue"}),
    }));
    next(&mut requests).await.answer(calls);
    let a = next(&mut requests).await;
    let b = next(&mut requests).await;
    let first_clue = |request: &Inference| {
        serde_json::to_string(&request.context)
            .unwrap()
            .contains("first clue")
    };
    assert_ne!(first_clue(&a), first_clue(&b));
    let (first, sibling) = if first_clue(&a) { (a, b) } else { (b, a) };
    let mut frames = interrupt(&h, &session, &mut client, 1).await;
    assert!(first.cancel.is_cancelled());
    assert!(!sibling.cancel.is_cancelled());
    command(
        &h,
        &session,
        Command::KillAgent {
            agent: AgentId::Sub(1),
        },
    )
    .await;
    assert!(!sibling.cancel.is_cancelled());
    sibling.answer(finalized_text_message("second clue verified"));
    let parent = next(&mut requests).await;
    assert!(
        serde_json::to_string(&parent_result(&parent).content)
            .unwrap()
            .to_lowercase()
            .contains("cancel")
    );
    let results: Vec<_> = parent
        .context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == "sibling" => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error);
    assert!(
        serde_json::to_string(&results[0].content)
            .unwrap()
            .contains("second clue verified")
    );
    parent.answer(finalized_text_message("both outcomes received"));
    frames.extend(client.pump_until_idle().await);
    assert_eq!(result_count(&frames), 1);
    assert!(!notice(&frames, CANCELLED));
    assert!(errors(&frames).is_empty());
    h.host.shutdown().await;
}

#[tokio::test]
async fn repeated_interruptions_reattach_and_resume_fulfill_the_original_call_once() {
    let (h, mut requests) = held();
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    let first = delegate(&h, &session, &mut requests).await;
    let mut frames = interrupt(&h, &session, &mut client, 1).await;
    assert!(first.cancel.is_cancelled());
    assert_eq!(result_count(&frames), 0);
    assert!(
        requests.try_recv().is_err(),
        "interrupt must not wake parent inference"
    );

    let cursor = client.client.cursor().unwrap().clone();
    client.reattach(&h.host, cursor).await;
    assert_assigned_idle(&client, 1);
    let mut fresh = Client::attach(&h.host, &session).await;
    assert_assigned_idle(&fresh, 1);

    resume(&h, &session, 1, "check the second clue").await;
    let second = next(&mut requests).await;
    let context = serde_json::to_string(&second.context).unwrap();
    assert!(context.contains("investigate original evidence"));
    assert!(context.contains("check the second clue"));
    frames.extend(interrupt(&h, &session, &mut client, 1).await);
    assert!(second.cancel.is_cancelled());
    assert_eq!(result_count(&frames), 0);
    assert!(requests.try_recv().is_err());

    resume(&h, &session, 1, "finish with both clues").await;
    let final_child = next(&mut requests).await;
    let context = serde_json::to_string(&final_child.context).unwrap();
    for text in [
        "investigate original evidence",
        "check the second clue",
        "finish with both clues",
    ] {
        assert!(context.contains(text), "lost conversation context: {text}");
    }
    final_child.answer(finalized_text_message("both clues verified"));
    let parent = next(&mut requests).await;
    let result = parent_result(&parent);
    assert!(!result.is_error);
    assert!(
        serde_json::to_string(&result.content)
            .unwrap()
            .contains("both clues verified")
    );
    parent.answer(finalized_text_message("investigation complete"));
    frames.extend(client.pump_until_idle().await);
    fresh.pump_until_idle().await;
    assert_eq!(result_count(&frames), 1);
    assert_eq!(
        sub_report(&client.canonical(), 1).as_deref(),
        Some("both clues verified")
    );
    assert_eq!(
        sub_box(&client.canonical(), 1),
        (SubAgentStatus::Done, true)
    );
    assert_eq!(
        sub_report(&fresh.canonical(), 1),
        sub_report(&client.canonical(), 1)
    );
    assert_no_dangling(&client.chat);
    assert!(errors(&frames).is_empty());
    assert!(requests.try_recv().is_err());
    h.host.shutdown().await;
}

#[tokio::test]
async fn kill_paused_or_resumed_child_releases_parent_with_one_cancellation_result() {
    for running in [false, true] {
        let (h, mut requests) = held();
        let session = h.create().await;
        let mut client = Client::attach(&h.host, &session).await;
        let first = delegate(&h, &session, &mut requests).await;
        let mut frames = interrupt(&h, &session, &mut client, 1).await;
        assert!(first.cancel.is_cancelled());
        assert_eq!(result_count(&frames), 0);
        let continuation = if running {
            resume(&h, &session, 1, "continue investigating").await;
            Some(next(&mut requests).await)
        } else {
            None
        };
        command(
            &h,
            &session,
            Command::KillAgent {
                agent: AgentId::Sub(1),
            },
        )
        .await;
        let parent = next(&mut requests).await;
        if let Some(continuation) = continuation {
            assert!(continuation.cancel.is_cancelled());
        }
        let result = parent_result(&parent);
        assert!(
            serde_json::to_string(&result.content)
                .unwrap()
                .to_lowercase()
                .contains("cancel")
        );
        parent.answer(finalized_text_message("acknowledged cancellation"));
        frames.extend(client.pump_until_idle().await);
        assert_eq!(result_count(&frames), 1);
        assert!(
            assistant_rows(&client.chat, AgentId::Main)
                .iter()
                .any(|text| text == "acknowledged cancellation")
        );
        assert_no_dangling(&client.chat);
        assert!(errors(&frames).is_empty());
        assert!(requests.try_recv().is_err());
        h.host.shutdown().await;
    }
}

#[tokio::test]
async fn parent_cancel_stops_paused_and_resumed_assignments_but_preserves_detached_work() {
    for resumed in [false, true] {
        let (h, mut requests) = held();
        let session = h.create().await;
        let mut client = Client::attach(&h.host, &session).await;
        h.prompt(&session, "start independent work then delegate")
            .await;
        next(&mut requests).await.answer(calling(
            "background shell",
            "shell",
            "bash",
            json!({
                "command":"mkfifo interruption-gate; read -r value < interruption-gate",
                "description":"wait on an unopened FIFO", "run_in_background":true
            }),
        ));
        next(&mut requests).await.answer(calling(
            "background agent",
            "background",
            "agent",
            json!({
                "task":"independent background investigation", "run_in_background":true
            }),
        ));
        // Detached child and parent can request inference in either order.
        let a = next(&mut requests).await;
        let b = next(&mut requests).await;
        let is_background = |request: &Inference| {
            request.context.messages.iter().any(|message| {
                matches!(message,
            Message::User(user) if user.content.iter().any(|content| matches!(content,
                UserContent::Text(text) if text.text == "independent background investigation")))
            })
        };
        assert_ne!(is_background(&a), is_background(&b));
        let (background, parent) = if is_background(&a) { (a, b) } else { (b, a) };
        parent.answer(calling(
            "foreground agent",
            "original",
            "agent",
            json!({"task":"investigate original evidence"}),
        ));
        let foreground = next(&mut requests).await;
        let mut frames = interrupt(&h, &session, &mut client, 2).await;
        assert!(foreground.cancel.is_cancelled());
        let continuation = if resumed {
            resume(&h, &session, 2, "continue foreground investigation").await;
            Some(next(&mut requests).await)
        } else {
            None
        };
        let handles = h.host.local_handles(&session).await.unwrap();
        let tasks = handles.task_registry.snapshot();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|task| task.status == TaskStatus::Running));
        assert!(
            tasks
                .iter()
                .any(|task| matches!(task.kind, TaskKind::Bash { .. }))
        );
        assert!(
            tasks
                .iter()
                .any(|task| matches!(task.kind, TaskKind::Agent { agent_id: 1, .. }))
        );
        assert!(!background.cancel.is_cancelled());

        command(
            &h,
            &session,
            Command::Cancel {
                agent: AgentId::Main,
            },
        )
        .await;
        frames.extend(client.pump_until_idle().await);
        if let Some(continuation) = continuation {
            assert!(
                continuation.cancel.is_cancelled(),
                "parent cancellation must reach resumed work"
            );
        }
        assert!(
            !background.cancel.is_cancelled(),
            "detached inference survives parent cancellation"
        );
        for task in tasks {
            assert_eq!(
                handles.task_registry.status(task.id),
                Some(TaskStatus::Running)
            );
        }
        assert!(!client.client.lifecycle().is_running(AgentId::Sub(2)));
        assert_ne!(sub_box(&client.canonical(), 2).0, SubAgentStatus::Running);
        assert!(notice(&frames, CANCELLED));
        assert!(errors(&frames).is_empty());
        assert!(
            requests.try_recv().is_err(),
            "cancelled foreground assignment must not resume parent"
        );
        h.host.shutdown().await;
    }
}
