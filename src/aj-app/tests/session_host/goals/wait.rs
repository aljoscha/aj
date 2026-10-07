use super::*;

fn waiting() -> AssistantMessage {
    calling("waiting for input", "wait", "wait", json!({}))
}

fn complete() -> AssistantMessage {
    calling(
        "verified",
        "done",
        "update_goal",
        json!({"status":"complete"}),
    )
}

async fn yielded(client: &mut Client) -> Vec<Frame> {
    let mut frames = frames_until(&mut client.stream, "Main yields", |frame| {
        matches!(frame, Frame::Event { event, .. } if matches!(event.known(),
            Some(AgentEvent::AgentEnd { agent_id: AgentId::Main, waiting: true, .. })))
    })
    .await;
    // AgentEnd precedes the turn join. Only the following idle state fences
    // the host's decision whether to launch a synthetic continuation.
    frames.extend(until_idle(&mut client.stream).await);
    for frame in &frames {
        let _ = client.client.apply(&mut client.chat, frame.clone());
    }
    frames
}

async fn completed(client: &mut Client) {
    frames_until(
        &mut client.stream,
        "completed goal and joined Main",
        |frame| {
            matches!(frame, Frame::State { working: false, goal: Some(goal), .. }
            if goal.status == GoalStatus::Complete)
        },
    )
    .await;
}

fn live_task(registry: &aj_agent::TaskRegistry) -> aj_agent::tool::TaskId {
    let (task, _) = registry.register_unowned_for_test(
        AgentId::Main,
        "server".into(),
        TaskKind::Bash {
            command: "server".into(),
        },
        "server".into(),
        Arc::new(FixedTaskOutput),
    );
    assert_eq!(registry.status(task), Some(TaskStatus::Running));
    task
}

#[tokio::test]
async fn standalone_wait_holds_goal_pursuit_until_input_or_resume() {
    let (h, provider) = recorded(vec![
        waiting(),
        finalized_text_message("user input received"),
        calling("waiting again", "wait-again", "wait", json!({})),
        complete(),
        finalized_text_message("finished"),
    ]);
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    action(&h.host, &session, create()).await;
    yielded(&mut client).await;

    // Read-only commands after the joined-idle frame must not release wait.
    let active = goal(&h.host, &session).await;
    assert_eq!(active.status, GoalStatus::Active);
    assert!(
        !h.host
            .sessions()
            .await
            .unwrap()
            .sessions
            .iter()
            .find(|row| row.id == session)
            .unwrap()
            .working
    );
    h.host.session_info(&session).await.unwrap();
    assert_eq!(goal(&h.host, &session).await.status, active.status);
    assert_eq!(provider.contexts.lock().unwrap().len(), 1);
    let replay = Client::attach(&h.host, &session).await;
    let snapshot = replay.canonical();
    assert!(snapshot.agent(AgentId::Main).unwrap().entries.iter().any(|entry| matches!(entry,
        aj_app::test_support::CanonicalEntry::Tool { call_id, status: aj_app::chat::ToolStatus::Done { is_error: false }, .. }
            if call_id == "wait")));
    drop(replay);

    h.prompt(&session, "new user input").await;
    yielded(&mut client).await;
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Active);
    {
        let contexts = provider.contexts.lock().unwrap();
        assert_eq!(
            contexts.len(),
            3,
            "input releases wait, then ordinary pursuit resumes"
        );
        assert!(
            serde_json::to_string(&contexts[1])
                .unwrap()
                .contains("new user input")
        );
        assert!(
            serde_json::to_string(&contexts[2])
                .unwrap()
                .contains("Continue working toward the active goal")
        );
    }
    action(&h.host, &session, GoalAction::Resume).await;
    completed(&mut client).await;
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Complete);
    assert_eq!(provider.contexts.lock().unwrap().len(), 5);
    h.host.shutdown().await;
}

#[cfg_attr(
    not(feature = "slow-tests"),
    ignore = "slow: elapsed goal time across idle wait"
)]
#[tokio::test]
async fn waiting_counts_toward_goal_time_without_running_work() {
    let (h, provider) = recorded(vec![waiting()]);
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    action(&h.host, &session, create()).await;
    yielded(&mut client).await;
    let before = goal(&h.host, &session).await;
    assert_eq!(before.status, GoalStatus::Active);
    assert_eq!(running_tasks(&h.host, &session).await, 0);

    let now = std::time::Instant::now();
    assert_eq!(
        client
            .chat
            .goal_runtime_seconds(now + Duration::from_secs(10))
            - client.chat.goal_runtime_seconds(now),
        10,
        "the idle client's elapsed-time display keeps advancing"
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let after = goal(&h.host, &session).await;
    assert_eq!(after.status, GoalStatus::Active);
    assert!(after.time_used_seconds > before.time_used_seconds);
    assert_eq!(provider.contexts.lock().unwrap().len(), 1);
    h.host.shutdown().await;
}

#[tokio::test]
async fn mixed_wait_returns_all_results_and_does_not_affect_later_steps() {
    for (tool, arguments, is_error) in [
        ("get_goal", json!({}), false),
        ("bash", json!({"run_in_background": true}), true),
    ] {
        let mut batch = waiting();
        batch.content.push(AssistantContent::ToolCall(ToolCall {
            is_raw: false,
            id: "sibling".into(),
            name: tool.into(),
            arguments,
        }));
        let (h, provider) = recorded(vec![
            batch,
            calling("inspect work", "next", "todo_read", json!({})),
            calling("wait now", "standalone-wait", "wait", json!({})),
        ]);
        let session = h.create().await;
        let mut client = Client::attach(&h.host, &session).await;
        action(&h.host, &session, create()).await;
        let frames = yielded(&mut client).await;
        let ends = super::super::events(&frames)
            .into_iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::AgentEnd {
                        agent_id: AgentId::Main,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(ends, 1, "mixed wait must continue within the same run");
        let contexts = provider.contexts.lock().unwrap().clone();
        assert_eq!(contexts.len(), 3, "only the standalone wait yields");
        let results: Vec<_> = contexts[1]
            .messages
            .iter()
            .filter_map(|message| match message {
                aj_models::types::Message::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_call_id, "wait");
        assert!(!results[0].is_error);
        assert_eq!(results[1].tool_call_id, "sibling");
        assert_eq!(
            results[1].is_error, is_error,
            "exercise both success and failure"
        );
        assert!(contexts[2].messages.iter().any(|message| matches!(message,
            aj_models::types::Message::ToolResult(result) if result.tool_call_id == "next" && !result.is_error)));
        assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Active);
        h.host.shutdown().await;
    }
}

#[tokio::test]
async fn goal_edit_during_the_yielding_step_still_resumes_pursuit() {
    let (h, provider) = recorded(vec![complete(), finalized_text_message("finished")]);
    let session = h.create().await;
    let held = hold_first(&h, &session, Arc::clone(&provider)).await;
    let mut client = Client::attach(&h.host, &session).await;
    action(&h.host, &session, create()).await;
    let (stream, _) = bounded("held Main inference", held).await.unwrap();
    action(
        &h.host,
        &session,
        GoalAction::Edit {
            objective: "edited objective".into(),
        },
    )
    .await;
    release(stream, waiting());
    completed(&mut client).await;
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 3);
    assert!(
        serde_json::to_string(&contexts[1])
            .unwrap()
            .contains("edited objective")
    );
    h.host.shutdown().await;
}

#[tokio::test]
async fn wait_after_sampling_an_edited_goal_is_honored() {
    let (h, provider) = recorded(vec![waiting()]);
    let session = h.create().await;
    let held = hold_first(&h, &session, Arc::clone(&provider)).await;
    let mut client = Client::attach(&h.host, &session).await;
    action(&h.host, &session, create()).await;
    let (stream, _) = bounded("held Main inference", held).await.unwrap();
    action(
        &h.host,
        &session,
        GoalAction::Edit {
            objective: "edited objective".into(),
        },
    )
    .await;
    release(
        stream,
        calling("check current work", "todos", "todo_read", json!({})),
    );
    yielded(&mut client).await;
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Active);
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 2);
    assert!(
        serde_json::to_string(&contexts[1])
            .unwrap()
            .contains("edited objective")
    );
    h.host.shutdown().await;
}

#[tokio::test]
async fn mixed_wait_does_not_hold_a_goal_created_in_the_same_batch() {
    let mut batch = calling(
        "create the requested goal",
        "create",
        "create_goal",
        json!({
            "objective": "wait for the requested input"
        }),
    );
    batch.content.push(AssistantContent::ToolCall(ToolCall {
        is_raw: false,
        id: "wait".into(),
        name: "wait".into(),
        arguments: json!({}),
    }));
    let (h, provider) = recorded(vec![
        batch,
        finalized_text_message("goal created"),
        calling("wait now", "standalone-wait", "wait", json!({})),
    ]);
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    h.prompt(&session, "Create a goal and wait for my next message")
        .await;
    yielded(&mut client).await;
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Active);
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 3);
    assert!(
        serde_json::to_string(&contexts[2])
            .unwrap()
            .contains("Continue working toward the active goal")
    );
    h.host.shutdown().await;
}

#[tokio::test]
async fn user_input_queued_before_yield_is_not_lost() {
    let (h, provider) = recorded(vec![complete(), finalized_text_message("finished")]);
    let session = h.create().await;
    let held = hold_first(&h, &session, Arc::clone(&provider)).await;
    let mut client = Client::attach(&h.host, &session).await;
    action(&h.host, &session, create()).await;
    let (stream, _) = bounded("held Main inference", held).await.unwrap();
    h.prompt(&session, "queued user input").await;
    let handles = h.host.local_handles(&session).await.unwrap();
    assert!(handles.queues.has_pending(AgentId::Main));
    release(stream, waiting());
    completed(&mut client).await;
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Complete);
    assert!(!handles.queues.has_pending(AgentId::Main));
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 3);
    assert!(
        serde_json::to_string(&contexts[1])
            .unwrap()
            .contains("queued user input")
    );
    h.host.shutdown().await;
}

#[tokio::test]
async fn completion_already_queued_before_yield_is_not_lost() {
    let (h, provider) = recorded(vec![complete(), finalized_text_message("finished")]);
    let session = h.create().await;
    let held = hold_first(&h, &session, Arc::clone(&provider)).await;
    let handles = h.host.local_handles(&session).await.unwrap();
    let task = live_task(&handles.task_registry);
    let mut client = Client::attach(&h.host, &session).await;
    action(&h.host, &session, create()).await;
    let (stream, _) = bounded("held Main inference", held).await.unwrap();
    // A real registry notice is queued while Main is still busy. Its own
    // AgentEnd must wake the owner even though no later task event arrives.
    handles.task_registry.finish(
        task,
        TaskStatus::Exited(Some(0)),
        aj_agent::tool::TaskNotice {
            owner: AgentId::Main,
            task_id: task,
            kind: TaskKind::Bash {
                command: "server".into(),
            },
            label: "server".into(),
            status: TaskStatus::Exited(Some(0)),
            body: "server completed before yield".into(),
        },
    );
    assert!(handles.task_registry.has_notices(AgentId::Main));
    release(stream, waiting());
    completed(&mut client).await;
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Complete);
    assert!(!handles.task_registry.has_notices(AgentId::Main));
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 3);
    assert!(
        serde_json::to_string(&contexts[1])
            .unwrap()
            .contains("server completed before yield")
    );
    h.host.shutdown().await;
}

#[tokio::test]
async fn late_background_agent_completion_wakes_yielded_main() {
    let (h, provider) = recorded(vec![
        calling(
            "delegate",
            "child",
            "agent",
            json!({"task":"held child", "run_in_background":true}),
        ),
        waiting(),
        complete(),
        finalized_text_message("finished"),
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
        task: Some("held child"),
    });
    let mut client = Client::attach(&h.host, &session).await;
    action(&h.host, &session, create()).await;
    let (stream, cancel) = bounded("background child inference", held).await.unwrap();
    yielded(&mut client).await;
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Active);
    assert_eq!(provider.contexts.lock().unwrap().len(), 3);
    assert_eq!(running_tasks(&h.host, &session).await, 1);
    assert!(!cancel.is_cancelled());
    release(
        stream,
        finalized_text_message("child completion reaches Main"),
    );
    completed(&mut client).await;
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Complete);
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 5);
    assert!(
        serde_json::to_string(&contexts[3])
            .unwrap()
            .contains("child completion reaches Main")
    );
    assert_eq!(running_tasks(&h.host, &session).await, 0);
    h.host.shutdown().await;
}

#[tokio::test]
async fn a_live_task_without_wait_does_not_suppress_goal_continuation() {
    let (h, provider) = recorded(vec![
        finalized_text_message("keep working while the server runs"),
        complete(),
        finalized_text_message("finished"),
    ]);
    let session = h.create().await;
    let handles = h.host.local_handles(&session).await.unwrap();
    let task = live_task(&handles.task_registry);
    let mut client = Client::attach(&h.host, &session).await;
    action(&h.host, &session, create()).await;
    completed(&mut client).await;
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Complete);
    assert_eq!(
        handles.task_registry.status(task),
        Some(TaskStatus::Running)
    );
    let contexts = provider.contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 3);
    assert!(
        serde_json::to_string(&contexts[1])
            .unwrap()
            .contains("Continue working toward the active goal")
    );
    handles
        .task_registry
        .set_status(task, TaskStatus::Exited(Some(0)));
    h.host.shutdown().await;
}
