use super::*;
use aj_agent::goal::{Goal, GoalAction, GoalRequest, GoalStatus};
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::streaming::AssistantMessageEventStream;
use aj_models::types::{Context, SimpleStreamOptions, StreamOptions, Usage};
use serde_json::json;

struct Recorded {
    script: Arc<ScriptedProvider>,
    contexts: StdMutex<Vec<Context>>,
}

type HeldInference = (
    AssistantMessageEventStream,
    tokio_util::sync::CancellationToken,
);

struct HoldFirst {
    provider: Arc<Recorded>,
    ready: StdMutex<Option<tokio::sync::oneshot::Sender<HeldInference>>>,
    task: Option<&'static str>,
}

impl Provider for HoldFirst {
    fn stream(&self, _: &ModelInfo, _: &Context, _: &StreamOptions) -> AssistantMessageEventStream {
        panic!("the agent supplies simple stream options");
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let matches_task = self.task.is_none_or(|task| context.messages.iter().any(|message| {
            matches!(message, aj_models::types::Message::User(user)
                if user.content.iter().any(|content| matches!(content, UserContent::Text(text) if text.text == task)))
        }));
        let ready = matches_task
            .then(|| self.ready.lock().unwrap().take())
            .flatten();
        if let Some(ready) = ready {
            self.provider.contexts.lock().unwrap().push(context.clone());
            let stream = AssistantMessageEventStream::new();
            assert!(
                ready
                    .send((
                        stream.clone(),
                        options.base.cancel.clone().expect("turn cancellation")
                    ))
                    .is_ok()
            );
            stream
        } else {
            self.provider.stream_simple(model, context, options)
        }
    }
}

async fn hold_first(
    h: &Harness,
    session: &str,
    provider: Arc<Recorded>,
) -> tokio::sync::oneshot::Receiver<HeldInference> {
    let (ready, receiver) = tokio::sync::oneshot::channel();
    h.host
        .local_handles(session)
        .await
        .unwrap()
        .run_config
        .lock()
        .unwrap()
        .main
        .provider = Arc::new(HoldFirst {
        provider,
        ready: StdMutex::new(Some(ready)),
        task: None,
    });
    receiver
}

fn release(stream: AssistantMessageEventStream, message: AssistantMessage) {
    for step in aj_models::scripted::script_from_message(message, 0, Duration::ZERO).steps {
        stream.push(step.event);
    }
    stream.end();
}

impl Provider for Recorded {
    fn stream(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &StreamOptions,
    ) -> AssistantMessageEventStream {
        self.contexts.lock().unwrap().push(context.clone());
        self.script.stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.contexts.lock().unwrap().push(context.clone());
        self.script.stream_simple(model, context, options)
    }
}

fn recorded(messages: Vec<AssistantMessage>) -> (Harness, Arc<Recorded>) {
    let script = scripted(messages, 0, Duration::ZERO);
    let provider = Arc::new(Recorded {
        script: Arc::clone(&script),
        contexts: StdMutex::new(Vec::new()),
    });
    let mut run = snapshot(script);
    run.main.provider = Arc::<Recorded>::clone(&provider);
    (
        Harness::with_run_config(run, Vec::new(), None, None),
        provider,
    )
}

fn used(mut message: AssistantMessage, input: u64) -> AssistantMessage {
    message.usage = Usage {
        input,
        ..Default::default()
    };
    message
}

async fn action(host: &SessionHost, session: &str, action: impl Into<GoalRequest>) -> Option<Goal> {
    match bounded(
        "goal command",
        host.command(session, Command::Goal(action.into())),
    )
    .await
    .expect("goal operation")
    {
        CommandOutcome::Goal(goal) => goal,
        other => panic!("unexpected goal outcome: {other:?}"),
    }
}

async fn goal(host: &SessionHost, session: &str) -> Goal {
    action(host, session, GoalAction::Get)
        .await
        .expect("goal exists")
}

async fn wait_goal(h: &Harness, session: &str, status: GoalStatus) -> Goal {
    bounded("goal to stop and its turn to finish", async {
        loop {
            let working = h
                .host
                .sessions()
                .await
                .unwrap()
                .sessions
                .iter()
                .find(|row| row.id == session)
                .unwrap()
                .working;
            if !working {
                // Read the subtotal after observing idle, not before a final
                // inference can finish between these two host requests.
                let goal = goal(&h.host, session).await;
                if goal.status == status {
                    return goal;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
}

fn create() -> GoalAction {
    GoalAction::Create {
        objective: "Verify the entire requested change".into(),
        token_budget: None,
    }
}

#[tokio::test]
async fn goal_create_resume_and_replace_adopt_busy_work_without_charging_earlier_inference() {
    for operation in ["create", "resume", "replace"] {
        let (h, provider) = recorded(vec![
            used(
                calling(
                    "verified",
                    "complete",
                    "update_goal",
                    json!({"status":"complete"}),
                ),
                7,
            ),
            used(finalized_text_message("usage report"), 3),
        ]);
        let session = h.create().await;
        let ready = hold_first(&h, &session, Arc::clone(&provider)).await;
        let mut events = h.host.attach(&[attach_request(&session)]).await.unwrap();
        frames_until(&mut events, "attach", |frame| {
            matches!(frame, Frame::CaughtUp { .. })
        })
        .await;
        let original = if operation == "create" {
            h.prompt(&session, "ordinary work").await;
            None
        } else {
            action(&h.host, &session, create()).await
        };
        let (stream, cancel) = bounded("held inference", ready).await.unwrap();
        // The first request is already in flight before pursuit changes. Its
        // eventual usage must not migrate to a replacement or resumed goal.
        frames_until(&mut events, "inference started", |frame| matches!(frame,
            Frame::Event { event, .. } if matches!(event.known(),
                Some(AgentEvent::MessageStart { message, .. })
                    if matches!(message.as_stored_wire(), Some(aj_models::types::Message::Assistant(_))))
        )).await;
        let adopted = match operation {
            "create" => action(&h.host, &session, create()).await.unwrap(),
            "resume" => {
                action(&h.host, &session, GoalAction::Pause).await;
                action(&h.host, &session, GoalAction::Resume).await.unwrap()
            }
            _ => action(
                &h.host,
                &session,
                GoalRequest::for_goal(
                    &original.as_ref().unwrap().id,
                    GoalAction::Replace {
                        objective: "replacement work".into(),
                        token_budget: None,
                    },
                ),
            )
            .await
            .unwrap(),
        };
        assert_eq!(adopted.status, GoalStatus::Active);
        assert!(!cancel.is_cancelled());
        if let Some(original) = original {
            assert_eq!(adopted.id == original.id, operation == "resume");
        }
        release(
            stream,
            used(
                calling(
                    "old inference",
                    "stale",
                    "update_goal",
                    json!({"status":"complete"}),
                ),
                50,
            ),
        );
        let completed = wait_goal(&h, &session, GoalStatus::Complete).await;
        assert_eq!(completed.id, adopted.id);
        assert_eq!(
            completed.tokens_used, 7,
            "{operation}: only newly admitted work counts"
        );
        let contexts = provider.contexts.lock().unwrap().clone();
        assert_eq!(contexts.len(), 3);
        assert!(latest_goal_context(&contexts[1]).contains(&adopted.objective));
        assert!(
            serde_json::to_string(&contexts[1])
                .unwrap()
                .contains("user changed the goal")
        );
        let frames = drained(&mut events);
        assert!(
            !super::events(&frames).iter().any(|event| matches!(
                event,
                AgentEvent::AgentStart {
                    agent_id: AgentId::Main
                }
            )),
            "adoption stays inside the original turn"
        );
        h.host.shutdown().await;
    }
}

#[tokio::test]
async fn goal_replacement_does_not_adopt_a_descendant_requested_by_the_old_inference() {
    let (h, provider) = recorded(vec![
        used(finalized_text_message("old descendant finished"), 13),
        used(finalized_text_message("new goal work"), 7),
    ]);
    let session = h.create().await;
    let held = hold_first(&h, &session, Arc::clone(&provider)).await;
    let original = action(&h.host, &session, create()).await.unwrap();
    let (stream, _) = bounded("old inference", held).await.unwrap();
    let replacement = action(
        &h.host,
        &session,
        GoalRequest::for_goal(
            &original.id,
            GoalAction::Replace {
                objective: "new objective".into(),
                token_budget: Some(1),
            },
        ),
    )
    .await
    .unwrap();
    release(
        stream,
        used(
            calling(
                "old work",
                "child",
                "agent",
                json!({"task":"finish old work"}),
            ),
            50,
        ),
    );
    let finished = wait_goal(&h, &session, GoalStatus::BudgetLimited).await;
    assert_eq!(finished.id, replacement.id);
    assert_eq!(
        finished.tokens_used, 7,
        "the old inference and its delegated work stay outside the replacement"
    );
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 3, "old Main, old descendant, then new Main");
    assert!(latest_goal_context(&contexts[2]).contains("new objective"));
    h.host.shutdown().await;
}

#[tokio::test]
async fn goal_model_stops_return_a_final_subtotal_before_reporting() {
    for (status, expected) in [
        ("paused", GoalStatus::Paused),
        ("blocked", GoalStatus::Blocked),
        ("complete", GoalStatus::Complete),
    ] {
        let (h, provider) = recorded(vec![
            used(
                calling("stopping", "stop", "update_goal", json!({"status":status})),
                7,
            ),
            used(finalized_text_message("report the subtotal"), 11),
        ]);
        let session = h.create().await;
        action(&h.host, &session, create()).await;
        let stopped = wait_goal(&h, &session, expected).await;
        assert_eq!(stopped.tokens_used, 7);
        let contexts = provider.contexts.lock().unwrap().clone();
        assert_eq!(contexts.len(), 2, "no continuation after explicit stop");
        let reported = contexts[1]
            .messages
            .iter()
            .find_map(|message| match message {
                aj_models::types::Message::ToolResult(result) if result.tool_call_id == "stop" => {
                    assert!(!result.is_error);
                    result.content.iter().find_map(|content| match content {
                        UserContent::Text(text) => {
                            Some(serde_json::from_str::<serde_json::Value>(&text.text).unwrap())
                        }
                        _ => None,
                    })
                }
                _ => None,
            })
            .expect("goal tool snapshot");
        assert_eq!(reported["goal"]["tokens_used"], stopped.tokens_used);
        let handles = h.host.local_handles(&session).await.unwrap();
        let log = handles.log.lock().await;
        assert_eq!(log.goal_at(log.head().unwrap()), Some(stopped));
        drop(log);
        h.host.shutdown().await;
    }
}

#[tokio::test]
async fn goal_budget_changes_preserve_usage_and_steer_a_running_turn_at_the_new_limit() {
    let (h, provider) = recorded(vec![
        used(finalized_text_message("first part"), 10),
        used(finalized_text_message("wrap up at the revised budget"), 3),
    ]);
    let session = h.create().await;
    action(
        &h.host,
        &session,
        GoalAction::Create {
            objective: "finish the work".into(),
            token_budget: Some(5),
        },
    )
    .await;
    let initial = wait_goal(&h, &session, GoalStatus::BudgetLimited).await;
    assert_eq!(initial.tokens_used, 10);
    let held = hold_first(&h, &session, Arc::clone(&provider)).await;
    let increased = action(
        &h.host,
        &session,
        GoalRequest::for_goal(
            &initial.id,
            GoalAction::SetBudget {
                token_budget: Some(100),
            },
        ),
    )
    .await
    .unwrap();
    assert_eq!(increased.status, GoalStatus::BudgetLimited);
    assert_eq!(increased.tokens_used, 10);
    assert_eq!(provider.contexts.lock().unwrap().len(), 1);
    action(&h.host, &session, GoalAction::Resume).await;
    let (stream, cancel) = bounded("resumed inference", held).await.unwrap();
    let lowered = action(
        &h.host,
        &session,
        GoalRequest::for_goal(
            &initial.id,
            GoalAction::SetBudget {
                token_budget: Some(10),
            },
        ),
    )
    .await
    .unwrap();
    assert_eq!(lowered.status, GoalStatus::BudgetLimited);
    assert!(!cancel.is_cancelled());
    release(
        stream,
        used(calling("check usage", "usage", "get_goal", json!({})), 7),
    );
    let finished = wait_goal(&h, &session, GoalStatus::BudgetLimited).await;
    assert_eq!(finished.id, initial.id);
    assert_eq!(
        finished.tokens_used, 20,
        "the soft limit still accounts its wrap-up"
    );
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 3);
    assert!(latest_goal_context(&contexts[2]).contains("Do not start new substantive goal work"));
    h.host.shutdown().await;
}

#[tokio::test]
async fn goal_stop_excludes_late_background_descendants_and_their_wake() {
    let (h, provider) = recorded(vec![
        used(
            calling(
                "delegate",
                "child",
                "agent",
                json!({"task":"held goal descendant", "run_in_background":true}),
            ),
            2,
        ),
        used(
            calling("stop", "pause", "update_goal", json!({"status":"paused"})),
            3,
        ),
        used(finalized_text_message("paused"), 5),
        used(finalized_text_message("received late child result"), 11),
    ]);
    let session = h.create().await;
    let (ready, held) = tokio::sync::oneshot::channel();
    h.host
        .local_handles(&session)
        .await
        .unwrap()
        .run_config
        .lock()
        .unwrap()
        .main
        .provider = Arc::new(HoldFirst {
        provider: Arc::clone(&provider),
        ready: StdMutex::new(Some(ready)),
        task: Some("held goal descendant"),
    });
    let mut events = h.host.attach(&[attach_request(&session)]).await.unwrap();
    frames_until(&mut events, "attach", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    action(&h.host, &session, create()).await;
    let (stream, cancel) = bounded("descendant inference", held).await.unwrap();
    let stopped = wait_goal(&h, &session, GoalStatus::Paused).await;
    assert_eq!(stopped.tokens_used, 5);
    assert!(
        !cancel.is_cancelled(),
        "pause does not interrupt descendants"
    );
    let _ = drained(&mut events);
    release(
        stream,
        used(finalized_text_message("descendant finished"), 13),
    );
    frames_until(&mut events, "late result delivered", |frame| matches!(frame,
        Frame::Event { event, .. } if matches!(event.known(), Some(AgentEvent::MessageEnd { agent_id: AgentId::Main, message })
            if serde_json::to_string(message).unwrap().contains("received late child result"))
    )).await;
    until_idle(&mut events).await;
    assert_eq!(
        goal(&h.host, &session).await,
        stopped,
        "neither descendant nor wake changes the final subtotal"
    );
    assert_eq!(provider.contexts.lock().unwrap().len(), 5);
    h.host.shutdown().await;
}

#[tokio::test]
async fn goal_checkpoint_write_failure_uses_session_teardown_without_duplicate_errors() {
    for active in [false, true] {
        let (h, provider) = recorded(vec![finalized_text_message("baseline")]);
        let session = h.create().await;
        let mut client = Client::attach(&h.host, &session).await;
        let held = if active {
            let ready = hold_first(&h, &session, Arc::clone(&provider)).await;
            action(&h.host, &session, create()).await;
            Some(bounded("held inference", ready).await.unwrap())
        } else {
            h.prompt(&session, "materialize baseline").await;
            assert_eq!(assistant_text(&client.pump_until_idle().await), "baseline");
            None
        };
        assert_eq!(provider.contexts.lock().unwrap().len(), 1);
        let path = h
            .persistence
            .sessions_dir()
            .join(format!("{session}.jsonl"));
        let fault = AppendFaultFixture::new(AppendFault::ShortWrite(23));
        let baseline = {
            let handles = h.host.local_handles(&session).await.unwrap();
            let mut log = handles.log.lock().await;
            // With inference held (or idle) and prior writes flushed, the next
            // write is the goal checkpoint, not an assistant-message append.
            log.flush_pending().unwrap();
            let baseline = std::fs::read(&path).unwrap();
            assert!(baseline.ends_with(b"\n"));
            fault.install(&mut log).unwrap();
            baseline
        };
        let command = if active {
            GoalAction::Edit {
                objective: "revised objective".into(),
            }
        } else {
            create()
        };
        assert!(
            bounded(
                "failed goal command",
                h.host.command(&session, Command::Goal(command.into()))
            )
            .await
            .is_err()
        );
        let mut frames = frames_until(
            &mut client.stream,
            "session storage failure",
            |frame| matches!(frame, Frame::Error { code, .. } if code == "persistence_failed"),
        )
        .await;
        bounded("failed session releases its lock", async {
            while SessionLock::is_held(&h.persistence, &session).unwrap() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        frames.extend(drained(&mut client.stream));
        assert_eq!(frames.iter().filter(|frame| matches!(frame, Frame::Error { code, .. } if code == "persistence_failed")).count(), 1);
        assert!(
            errors(&frames).is_empty(),
            "duplicate goal error: {:?}",
            errors(&frames)
        );
        assert_eq!(
            provider.contexts.lock().unwrap().len(),
            1,
            "failure must not start more inference"
        );
        if let Some((_, cancel)) = &held {
            assert!(
                cancel.is_cancelled(),
                "session teardown must cancel active inference"
            );
        }
        let torn = std::fs::read(&path).unwrap();
        assert_eq!(torn.len(), baseline.len() + 23);
        assert!(torn.starts_with(&baseline));
        assert_eq!(
            fault.writes(),
            2,
            "no writes after the partial write and failed continuation"
        );
        h.host.shutdown().await;
    }
}

#[tokio::test]
async fn goal_checkpoint_rejection_without_a_write_fuse_stops_pursuit_and_reports_error() {
    let (h, provider) = recorded(Vec::new());
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    let handles = h.host.local_handles(&session).await.unwrap();
    let persistence = ConversationPersistence::new(h._dir.path().join("invalid-goal-parent"));
    let mut replacement = ConversationLog::create(&persistence).unwrap();
    replacement
        .append(
            None,
            aj_session::ThreadKind::Meta,
            None,
            aj_session::ConversationEntryKind::ThinkingChange {
                level: "off".into(),
            },
        )
        .unwrap();
    assert!(matches!(
        replacement.append_goal_change(None),
        Err(aj_session::ConversationError::InvalidAppend(_))
    ));
    assert!(replacement.write_failure().is_none());
    // Retain the real log and its failure sender while the invalid-parent log
    // exercises a checkpoint rejection that cannot signal the write fuse.
    let original = std::mem::replace(&mut *handles.log.lock().await, replacement);
    assert!(
        bounded(
            "rejected goal",
            h.host.command(&session, Command::Goal(create().into()))
        )
        .await
        .is_err()
    );
    let frames = frames_until(&mut client.stream, "goal error", |frame| {
        matches!(frame, Frame::Event { event, .. } if matches!(event.known(), Some(AgentEvent::Error { .. })))
    }).await;
    assert_eq!(errors(&frames).len(), 1);
    assert!(
        !frames.iter().any(
            |frame| matches!(frame, Frame::Error { code, .. } if code == "persistence_failed")
        )
    );
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Blocked);
    assert!(provider.contexts.lock().unwrap().is_empty());
    {
        let mut log = handles.log.lock().await;
        assert!(log.write_failure().is_none());
        *log = original;
    }
    h.host.shutdown().await;
}

#[tokio::test]
async fn goal_pause_and_clear_leave_the_current_inference_running() {
    for clear in [false, true] {
        let (h, provider) = recorded(Vec::new());
        let session = h.create().await;
        let held = hold_first(&h, &session, Arc::clone(&provider)).await;
        let mut events = h.host.attach(&[attach_request(&session)]).await.unwrap();
        frames_until(&mut events, "attach", |frame| {
            matches!(frame, Frame::CaughtUp { .. })
        })
        .await;
        action(&h.host, &session, create()).await;
        let (stream, cancel) = bounded("held inference", held).await.unwrap();
        action(
            &h.host,
            &session,
            if clear {
                GoalAction::Clear
            } else {
                GoalAction::Pause
            },
        )
        .await;
        assert!(
            !cancel.is_cancelled(),
            "a goal command must not interrupt the turn"
        );
        assert!(
            h.host
                .sessions()
                .await
                .unwrap()
                .sessions
                .iter()
                .any(|row| row.id == session && row.working)
        );
        release(
            stream,
            used(finalized_text_message("the original turn completed"), 9),
        );
        let frames = until_idle(&mut events).await;
        assert!(assistant_text(&frames).contains("the original turn completed"));
        let result = action(&h.host, &session, GoalAction::Get).await;
        if clear {
            assert!(result.is_none());
        } else {
            let result = result.unwrap();
            assert_eq!(result.status, GoalStatus::Paused);
            assert_eq!(
                result.tokens_used, 0,
                "pausing stops goal accounting without cancelling inference"
            );
        }
        assert_eq!(provider.contexts.lock().unwrap().len(), 1);
        h.host.shutdown().await;
    }
}

#[tokio::test]
async fn goal_edit_steers_the_running_turn_and_rejects_completion_of_the_old_objective() {
    let (h, provider) = recorded(vec![
        used(
            calling(
                "new objective verified",
                "new-complete",
                "update_goal",
                json!({"status":"complete"}),
            ),
            5,
        ),
        used(finalized_text_message("revised goal done"), 2),
    ]);
    let session = h.create().await;
    let held = hold_first(&h, &session, Arc::clone(&provider)).await;
    let mut events = h.host.attach(&[attach_request(&session)]).await.unwrap();
    frames_until(&mut events, "attach", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    let original = action(&h.host, &session, create()).await.unwrap();
    let (stream, cancel) = bounded("held inference", held).await.unwrap();
    let edited = action(
        &h.host,
        &session,
        GoalAction::Edit {
            objective: "REVISED objective".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(edited.id, original.id);
    assert_eq!(edited.status, GoalStatus::Active);
    assert_eq!(edited.tokens_used, original.tokens_used);
    assert!(!cancel.is_cancelled());
    h.prompt(&session, "discard this queued user input").await;
    let handles = h.host.local_handles(&session).await.unwrap();
    assert_eq!(handles.queues.pending_counts(), (0, 1));
    h.host
        .command(&session, Command::Queue(QueueOp::Clear))
        .await
        .unwrap();
    assert_eq!(handles.queues.pending_counts(), (0, 0));
    release(
        stream,
        used(
            calling(
                "old objective done",
                "stale-complete",
                "update_goal",
                json!({"status":"complete"}),
            ),
            3,
        ),
    );
    let finished = wait_goal(&h, &session, GoalStatus::Complete).await;
    assert_eq!(finished.objective, "REVISED objective");
    assert_eq!(finished.tokens_used, 8);
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 3);
    assert!(latest_goal_context(&contexts[1]).contains("REVISED objective"));
    assert!(
        serde_json::to_string(&contexts[1])
            .unwrap()
            .contains("user changed the goal")
    );
    assert!(contexts[2].messages.iter().any(|message| matches!(message,
        aj_models::types::Message::ToolResult(result) if result.tool_call_id == "new-complete" && !result.is_error)));
    let frames = drained(&mut events);
    assert_eq!(
        super::events(&frames)
            .iter()
            .filter(|event| matches!(
                event,
                AgentEvent::AgentStart {
                    agent_id: AgentId::Main
                }
            ))
            .count(),
        1,
        "editing takes effect inside the same running turn"
    );
    h.host.shutdown().await;
}

#[tokio::test]
async fn goal_edit_preserves_stopped_states_and_reactivates_completed_work_with_budget_left() {
    for (initial, spent, expected) in [
        (GoalStatus::Paused, 12, GoalStatus::Paused),
        (GoalStatus::Blocked, 12, GoalStatus::Blocked),
        (GoalStatus::UsageLimited, 12, GoalStatus::UsageLimited),
        (GoalStatus::BudgetLimited, 100, GoalStatus::BudgetLimited),
        (GoalStatus::Complete, 100, GoalStatus::BudgetLimited),
        (GoalStatus::Complete, 12, GoalStatus::Active),
    ] {
        let (h, provider) = recorded(vec![
            used(
                calling(
                    "edited work done",
                    "done",
                    "update_goal",
                    json!({"status":"complete"}),
                ),
                5,
            ),
            used(finalized_text_message("complete"), 2),
        ]);
        let session = h.create().await;
        let original = Goal {
            id: "existing".into(),
            objective: "old".into(),
            status: initial,
            token_budget: Some(100),
            tokens_used: spent,
            time_used_seconds: 7,
        };
        let handles = h.host.local_handles(&session).await.unwrap();
        let seed = handles
            .log
            .lock()
            .await
            .append_goal_change(Some(original.clone()))
            .unwrap();
        h.host
            .command(
                &session,
                Command::Head {
                    target: HeadTarget::Entry(seed.id),
                    changes: Default::default(),
                },
            )
            .await
            .unwrap();
        let edited = action(
            &h.host,
            &session,
            GoalAction::Edit {
                objective: "new".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(edited.status, expected);
        assert_eq!(edited.id, original.id);
        assert_eq!(edited.token_budget, original.token_budget);
        assert_eq!(edited.tokens_used, spent);
        assert_eq!(edited.time_used_seconds, 7);
        if expected == GoalStatus::Active {
            assert_eq!(
                wait_goal(&h, &session, GoalStatus::Complete)
                    .await
                    .tokens_used,
                spent + 5
            );
        }
        h.host.shutdown().await;
        assert_eq!(
            provider.contexts.lock().unwrap().len(),
            if expected == GoalStatus::Active { 2 } else { 0 }
        );
    }
}

#[tokio::test]
async fn goal_pursuit_requires_the_models_completion_tool() {
    let (h, provider) = recorded(Vec::new());
    let session = h.create().await;
    h.config
        .lock()
        .unwrap()
        .disabled_tools
        .push("update_goal".into());
    let error = h
        .host
        .command(&session, Command::Goal(create().into()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("update_goal"));
    assert!(action(&h.host, &session, GoalAction::Get).await.is_none());
    assert!(provider.contexts.lock().unwrap().is_empty());
    h.host.shutdown().await;
}

#[tokio::test]
async fn goal_failed_attempts_count_once() {
    let mut failed = used(finalized_text_message("partial response"), 5);
    failed.stop_reason = StopReason::Error;
    failed.error = Some(AssistantError::new(
        ErrorCategory::Transient,
        "retryable stream failure",
    ));
    let (h, provider) = recorded(vec![
        failed,
        used(finalized_text_message("successful retry"), 7),
    ]);
    let session = h.create().await;
    action(
        &h.host,
        &session,
        GoalAction::Create {
            objective: "finish despite a retry".into(),
            token_budget: Some(10),
        },
    )
    .await;
    let result = wait_goal(&h, &session, GoalStatus::BudgetLimited).await;
    assert_eq!(result.tokens_used, 12);
    assert_eq!(provider.contexts.lock().unwrap().len(), 2);
    h.host.shutdown().await;
}

fn latest_goal_context(context: &Context) -> String {
    context
        .messages
        .iter()
        .rev()
        .filter_map(|message| {
            let aj_models::types::Message::User(user) = message else {
                return None;
            };
            user.content
                .iter()
                .filter_map(|content| match content {
                    UserContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .find(|text| {
                    text.contains("Goal state") || text.contains("There is no current goal")
                })
        })
        .next()
        .expect("authoritative goal context")
        .to_string()
}

#[tokio::test]
async fn goal_clear_supersedes_historical_instructions_live_and_after_reopen() {
    let (h, provider) = recorded(vec![
        used(
            calling(
                "blocked",
                "block",
                "update_goal",
                json!({"status":"blocked"}),
            ),
            1,
        ),
        used(finalized_text_message("waiting for user"), 1),
        used(finalized_text_message("answering an ordinary question"), 1),
    ]);
    let session = h.create().await;
    action(&h.host, &session, create()).await;
    wait_goal(&h, &session, GoalStatus::Blocked).await;
    action(&h.host, &session, GoalAction::Clear).await;
    let mut stream = h.host.attach(&[attach_request(&session)]).await.unwrap();
    frames_until(&mut stream, "attach", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    h.prompt(&session, "What happened?").await;
    until_idle(&mut stream).await;
    assert!(
        latest_goal_context(provider.contexts.lock().unwrap().last().unwrap())
            .starts_with("There is no current goal")
    );
    drop(stream);
    h.host.shutdown().await;

    let revived = h.revive(Vec::new());
    let handles = revived.host.local_handles(&session).await.unwrap();
    let recorder = Arc::new(Recorded {
        script: scripted(
            vec![finalized_text_message("ordinary answer after reopen")],
            0,
            Duration::ZERO,
        ),
        contexts: StdMutex::new(Vec::new()),
    });
    handles.run_config.lock().unwrap().main.provider = Arc::<Recorded>::clone(&recorder);
    let mut stream = revived
        .host
        .attach(&[attach_request(&session)])
        .await
        .unwrap();
    frames_until(&mut stream, "reopen attach", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    revived.prompt(&session, "Another question").await;
    until_idle(&mut stream).await;
    assert!(
        latest_goal_context(recorder.contexts.lock().unwrap().last().unwrap())
            .starts_with("There is no current goal")
    );
    assert!(
        action(&revived.host, &session, GoalAction::Get)
            .await
            .is_none()
    );
    revived.host.shutdown().await;
}

#[tokio::test]
async fn goal_idle_continuation_notice_is_visible_live_and_after_reopen() {
    let (h, provider) = recorded(vec![
        calling(
            "setting goal",
            "create",
            "create_goal",
            json!({"objective":"Verify the entire requested change"}),
        ),
        finalized_text_message("first part done"),
        calling(
            "verified everything",
            "done",
            "update_goal",
            json!({"status":"complete"}),
        ),
        finalized_text_message("all done"),
        finalized_text_message("ordinary answer"),
    ]);
    let session = h.create().await;
    let mut live = Client::attach(&h.host, &session).await;
    let cursor = live.client.cursor().unwrap().clone();
    h.prompt(&session, "Set a goal to verify the entire requested change")
        .await;
    wait_goal(&h, &session, GoalStatus::Complete).await;
    let frames = frames_until(&mut live.stream, "completed goal", |frame| {
        matches!(frame, Frame::State { working: false, goal: Some(goal), .. }
            if goal.status == GoalStatus::Complete)
    })
    .await;
    for frame in frames {
        let _ = live.client.apply(&mut live.chat, frame);
    }
    h.prompt(&session, "Explain the result without resuming")
        .await;
    live.pump_until_idle().await;

    let assert_notice = |client: &Client| {
        use aj_app::test_support::CanonicalEntry;
        let state = client.canonical();
        assert_eq!(
            all_notices(&state),
            vec![(AgentId::Main, "Continuing goal".into())],
            "only idle pursuit is announced, not ordinary goal-state updates"
        );
        let rows = &state.agent(AgentId::Main).unwrap().entries;
        let position = |text: &str| {
            rows.iter()
                .position(|row| match row {
                    CanonicalEntry::Notice { text: notice, .. } => notice == text,
                    CanonicalEntry::Assistant { message, .. } => message.to_string().contains(text),
                    _ => false,
                })
                .unwrap()
        };
        assert!(position("first part done") < position("Continuing goal"));
        assert!(position("Continuing goal") < position("verified everything"));
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, CanonicalEntry::User { .. }))
                .count(),
            2,
            "the continuation is not editable user input"
        );
    };
    assert_notice(&live);
    assert!(provider.contexts.lock().unwrap().iter().all(|context| {
        !serde_json::to_string(context)
            .unwrap()
            .contains("Continuing goal")
    }));
    live.reattach(&h.host, cursor).await;
    assert_notice(&live);
    h.host.shutdown().await;
    let revived = h.revive(Vec::new());
    let replay = Client::attach(&revived.host, &session).await;
    assert_notice(&replay);
    revived.host.shutdown().await;
}

#[tokio::test]
async fn goal_tools_continue_across_turns_and_account_only_the_goal_work() {
    let (h, provider) = recorded(vec![
        used(
            calling(
                "setting goal",
                "create",
                "create_goal",
                json!({"objective":"Verify the entire requested change"}),
            ),
            100,
        ),
        used(finalized_text_message("first part done"), 11),
        used(
            calling(
                "verified everything",
                "done",
                "update_goal",
                json!({"status":"complete"}),
            ),
            7,
        ),
        used(finalized_text_message("all done"), 3),
    ]);
    let session = h.create().await;
    h.prompt(&session, "Set a goal to verify the entire requested change")
        .await;
    let result = wait_goal(&h, &session, GoalStatus::Complete).await;
    assert_eq!(
        result.tokens_used, 18,
        "creation inference and post-completion reporting are outside the goal"
    );
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 4);
    let continuation = serde_json::to_string(&contexts[2]).unwrap();
    assert!(continuation.contains("Continue working toward the active goal"));
    assert!(continuation.contains("Verify the entire requested change"));
    let handles = h.host.local_handles(&session).await.unwrap();
    let log = handles.log.lock().await;
    assert_eq!(log.goal_at(log.head().unwrap()), Some(result));
    drop(log);
    h.host.shutdown().await;
}

#[tokio::test]
async fn goal_budget_includes_descendants_excludes_cache_reads_and_steers_wrapup() {
    let mut spawn = used(
        calling(
            "delegate",
            "child",
            "agent",
            json!({"task":"verify one requirement"}),
        ),
        2,
    );
    spawn.usage.cache_read = 100;
    spawn.usage.cache_write = 1;
    spawn.usage.output = 2;
    let (h, provider) = recorded(vec![
        spawn,
        used(finalized_text_message("requirement verified"), 8),
        used(
            finalized_text_message("budget reached, here is remaining work"),
            4,
        ),
    ]);
    let session = h.create().await;
    action(
        &h.host,
        &session,
        GoalAction::Create {
            objective: "Verify everything".into(),
            token_budget: Some(10),
        },
    )
    .await;
    let result = wait_goal(&h, &session, GoalStatus::BudgetLimited).await;
    assert_eq!(
        result.tokens_used, 17,
        "5 main + 8 child + 4 wrap-up, not the 100 cached reads"
    );
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 3);
    assert!(contexts[1].tools.iter().all(|tool| !matches!(
        tool.name.as_str(),
        "get_goal" | "create_goal" | "update_goal"
    )));
    assert!(
        serde_json::to_string(&contexts[2])
            .unwrap()
            .contains("Do not start new substantive goal work")
    );
    h.host.shutdown().await;
}

#[tokio::test]
async fn goal_execution_failures_stop_pursuit_and_successful_tools_reset_the_audit() {
    for (recovery_turn, status_only_turn, blocking_turn) in
        [(None, None, 3), (Some(2), None, 5), (None, Some(2), 4)]
    {
        let mut responses = Vec::new();
        for turn in 1..=blocking_turn {
            if status_only_turn != Some(turn) {
                responses.push(calling(
                    "try to execute",
                    &format!("failed-{turn}"),
                    "bash",
                    // A NUL argument deterministically prevents process spawn.
                    json!({"command":"\u{0}", "description":"execution failure fixture"}),
                ));
            }
            if recovery_turn == Some(turn) {
                responses.push(calling("recovered", "recovery", "get_goal", json!({})));
            }
            responses.push(finalized_text_message("No execution progress yet"));
        }
        // Stop a broken implementation without waiting for a deadline. It must
        // block on its own, before the model supplies this explicit pause.
        responses.push(calling(
            "stop",
            "sentinel",
            "update_goal",
            json!({"status":"paused"}),
        ));
        responses.push(finalized_text_message("stopped"));
        let h = Harness::new(responses);
        let session = h.create().await;
        let mut events = h.host.attach(&[attach_request(&session)]).await.unwrap();
        action(&h.host, &session, create()).await;
        let frames = frames_until(&mut events, "execution audit stops pursuit", |frame| {
            matches!(frame, Frame::State { goal: Some(goal), working: false, .. }
                if goal.status != GoalStatus::Active)
        })
        .await;
        assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Blocked);
        let failures = frames
            .iter()
            .filter(|frame| {
                matches!(frame,
                    Frame::Event { event, .. } if matches!(event.known(),
                        Some(AgentEvent::ToolExecutionEnd {
                            tool, is_error: true,
                            result: aj_agent::tool::ToolDetails::Bash { exit_code: None, .. }, ..
                        }) if tool == "bash"
                    )
                )
            })
            .count();
        assert_eq!(
            failures,
            blocking_turn - usize::from(status_only_turn.is_some())
        );
        h.host.shutdown().await;
    }
}

#[tokio::test]
async fn goal_nonzero_command_exits_are_not_execution_failures() {
    let mut responses = Vec::new();
    for turn in 0..4 {
        responses.push(calling(
            "check a failing command",
            &format!("exit-{turn}"),
            "bash",
            json!({"command":"exit 1", "description":"ordinary nonzero exit fixture"}),
        ));
        responses.push(finalized_text_message("Continue investigating"));
    }
    responses.push(calling(
        "done",
        "complete",
        "update_goal",
        json!({"status":"complete"}),
    ));
    responses.push(finalized_text_message("finished"));
    let h = Harness::new(responses);
    let session = h.create().await;
    let mut events = h.host.attach(&[attach_request(&session)]).await.unwrap();
    action(&h.host, &session, create()).await;
    let frames = frames_until(&mut events, "goal concludes", |frame| {
        matches!(frame, Frame::State { goal: Some(goal), working: false, .. }
            if goal.status != GoalStatus::Active)
    })
    .await;
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Complete);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(frame,
                Frame::Event { event, .. } if matches!(event.known(),
                    Some(AgentEvent::ToolExecutionEnd {
                        tool, is_error: false,
                        result: aj_agent::tool::ToolDetails::Bash { exit_code: Some(1), .. }, ..
                    }) if tool == "bash"
                )
            ))
            .count(),
        4,
        "the real bash tool must report ordinary exits, not tool errors"
    );
    h.host.shutdown().await;
}

#[tokio::test]
async fn goal_empty_continuations_and_terminal_errors_stop_instead_of_spinning() {
    let (h, provider) = recorded(vec![used(finalized_text_message(""), 1); 3]);
    let session = h.create().await;
    action(&h.host, &session, create()).await;
    assert_eq!(
        wait_goal(&h, &session, GoalStatus::Blocked)
            .await
            .tokens_used,
        3
    );
    assert_eq!(provider.contexts.lock().unwrap().len(), 3);
    h.host.shutdown().await;

    let mut error = finalized_text_message("cannot proceed");
    error.stop_reason = StopReason::Error;
    error.error = Some(AssistantError::new(
        ErrorCategory::InvalidRequest,
        "invalid request",
    ));
    let (h, provider) = recorded(vec![error]);
    let session = h.create().await;
    action(&h.host, &session, create()).await;
    wait_goal(&h, &session, GoalStatus::Blocked).await;
    assert_eq!(provider.contexts.lock().unwrap().len(), 1);
    h.host.shutdown().await;
}

#[tokio::test]
async fn goal_main_cancel_pauses_but_attaching_to_live_work_does_not() {
    let slow = scripted(
        vec![finalized_text_message("a long running response")],
        1,
        Duration::from_secs(1),
    );
    let h = Harness::with_provider(slow);
    let session = h.create().await;
    let mut stream = h.host.attach(&[attach_request(&session)]).await.unwrap();
    frames_until(&mut stream, "initial attach", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    action(&h.host, &session, create()).await;
    frames_until(&mut stream, "goal inference", |frame| matches!(frame, Frame::Event { event, .. } if matches!(event.known(), Some(AgentEvent::TurnStart { .. })))).await;
    let mut second = h.host.attach(&[attach_request(&session)]).await.unwrap();
    let attached = frames_until(&mut second, "live reattach", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    assert!(attached.iter().any(|frame| matches!(frame, Frame::State { goal: Some(goal), .. } if goal.status == GoalStatus::Active)));
    h.host
        .command(
            &session,
            Command::Cancel {
                agent: AgentId::Main,
            },
        )
        .await
        .unwrap();
    wait_goal(&h, &session, GoalStatus::Paused).await;
    h.host.shutdown().await;
}

#[tokio::test]
async fn goal_reopen_and_branch_selection_pause_without_implicitly_resuming_on_a_prompt() {
    let h = Harness::new(Vec::new());
    let session = h.create().await;
    let handles = h.host.local_handles(&session).await.unwrap();
    let seed = Goal {
        id: "saved-goal".into(),
        objective: "Saved objective".into(),
        status: GoalStatus::Active,
        token_budget: Some(1000),
        tokens_used: 12,
        time_used_seconds: 3,
    };
    let branch_point = {
        let mut log = handles.log.lock().await;
        let entry = log.append_goal_change(Some(seed.clone())).unwrap();
        log.flush_pending().unwrap();
        assert_eq!(log.goal_at(&entry.id).unwrap().status, GoalStatus::Active);
        entry.id
    };
    drop(handles);
    h.host.shutdown().await;
    let revived = h.revive(vec![
        used(
            finalized_text_message("here is a status explanation, not goal pursuit"),
            50,
        ),
        used(
            calling(
                "verified saved objective",
                "complete",
                "update_goal",
                json!({"status":"complete"}),
            ),
            5,
        ),
        used(finalized_text_message("finished"), 2),
    ]);
    let mut stream = revived
        .host
        .attach(&[attach_request(&session)])
        .await
        .unwrap();
    let block = frames_until(&mut stream, "reopen", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    assert!(block.iter().any(|frame| matches!(frame, Frame::State { working: false, goal: Some(goal), .. } if goal.status == GoalStatus::Paused)));
    revived
        .prompt(&session, "Explain where we left off, don't resume")
        .await;
    until_idle(&mut stream).await;
    assert_eq!(goal(&revived.host, &session).await.tokens_used, 12);
    assert_eq!(
        goal(&revived.host, &session).await.status,
        GoalStatus::Paused
    );
    action(&revived.host, &session, GoalAction::Resume).await;
    let completed = wait_goal(&revived, &session, GoalStatus::Complete).await;
    assert_eq!(completed.tokens_used, 17);
    revived
        .host
        .command(
            &session,
            Command::Head {
                target: HeadTarget::Entry(branch_point),
                changes: Default::default(),
            },
        )
        .await
        .unwrap();
    let restored = goal(&revived.host, &session).await;
    assert_eq!(restored.status, GoalStatus::Paused);
    assert_eq!(restored.tokens_used, 12);
    assert_eq!(restored.id, seed.id);
    revived.host.shutdown().await;
}
