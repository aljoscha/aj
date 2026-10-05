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
async fn wait_finishes_siblings_and_only_input_or_resume_releases_goal_pursuit() {
    let mut batch = waiting();
    batch.content.push(AssistantContent::ToolCall(ToolCall {
        id: "sibling".into(),
        name: "get_goal".into(),
        arguments: json!({}),
    }));
    let (h, provider) = recorded(vec![
        batch,
        finalized_text_message("user input received"),
        calling("waiting again", "wait-again", "wait", json!({})),
        complete(),
        finalized_text_message("finished"),
    ]);
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    action(&h.host, &session, create()).await;
    let frames = yielded(&mut client).await;
    let results = super::super::events(&frames)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::TurnEnd {
                agent_id: AgentId::Main,
                tool_results,
                ..
            } => Some(tool_results),
            _ => None,
        })
        .expect("the full batch ends before yielding");
    assert_eq!(results.len(), 2);
    for id in ["wait", "sibling"] {
        assert!(
            results
                .iter()
                .any(|result| result.tool_call_id == id && !result.is_error)
        );
    }

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
    assert_eq!(goal(&h.host, &session).await, active);
    assert_eq!(provider.contexts.lock().unwrap().len(), 1);
    let replay = Client::attach(&h.host, &session).await;
    let snapshot = replay.canonical();
    for id in ["wait", "sibling"] {
        assert!(snapshot.agent(AgentId::Main).unwrap().entries.iter().any(|entry| matches!(entry,
            aj_app::test_support::CanonicalEntry::Tool { call_id, status: aj_app::chat::ToolStatus::Done { is_error: false }, .. }
                if call_id == id)));
    }
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
async fn model_can_create_a_goal_and_wait_in_the_same_batch() {
    let mut batch = calling(
        "create the requested goal",
        "create",
        "create_goal",
        json!({
            "objective": "wait for the requested input"
        }),
    );
    batch.content.push(AssistantContent::ToolCall(ToolCall {
        id: "wait".into(),
        name: "wait".into(),
        arguments: json!({}),
    }));
    let (h, provider) = recorded(vec![batch]);
    let session = h.create().await;
    let mut client = Client::attach(&h.host, &session).await;
    h.prompt(&session, "Create a goal and wait for my next message")
        .await;
    yielded(&mut client).await;
    assert_eq!(goal(&h.host, &session).await.status, GoalStatus::Active);
    assert_eq!(provider.contexts.lock().unwrap().len(), 1);
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
