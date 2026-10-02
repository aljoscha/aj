use super::*;
use aj_agent::goal::{Goal, GoalAction, GoalStatus};
use aj_models::provider::Provider;
use aj_models::streaming::AssistantMessageEventStream;
use aj_models::types::{
    AssistantContent, Context, SimpleStreamOptions, StopReason, StreamOptions, ToolCall,
};

type HeldInference = (AssistantMessageEventStream, CancellationToken);

struct HoldFirst {
    ready: StdMutex<Option<tokio::sync::oneshot::Sender<HeldInference>>>,
    next: Arc<aj_models::scripted::ScriptedProvider>,
}

impl Provider for HoldFirst {
    fn stream(&self, _: &ModelInfo, _: &Context, _: &StreamOptions) -> AssistantMessageEventStream {
        panic!("agent uses simple options")
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        if let Some(ready) = self.ready.lock().unwrap().take() {
            let stream = AssistantMessageEventStream::new();
            assert!(
                ready
                    .send((stream.clone(), options.base.cancel.clone().unwrap()))
                    .is_ok()
            );
            stream
        } else {
            self.next.stream_simple(model, context, options)
        }
    }
}

fn choose(writer: &mut PipeWriter, query: &str) {
    writer.write_all(b"\x01\x0b").unwrap();
    writer.write_all(query.as_bytes()).unwrap();
    writer.write_all(b"\r").unwrap();
}

fn clear_filter(writer: &mut PipeWriter) {
    writer.write_all(b"\x01\x0b").unwrap();
}

async fn page(shell: &Rc<RefCell<Shell>>, needle: &str) -> String {
    let result = poll_for(|| {
        if !shell.borrow().overlays.borrow().is_open() {
            return None;
        }
        let rows = top_overlay_rows(shell).join("\n");
        rows.contains(needle).then_some(rows)
    })
    .await;
    result.unwrap_or_else(|| {
        panic!(
            "missing {needle:?} in goal overlay: {:?}",
            top_overlay_rows(shell)
        )
    })
}

async fn depth(shell: &Rc<RefCell<Shell>>, expected: usize) {
    assert!(
        poll_for(|| (shell.borrow().overlays.borrow().depth() == expected).then_some(()))
            .await
            .is_some(),
        "expected overlay depth {expected}"
    );
}

async fn wait_for_save(shell: &Rc<RefCell<Shell>>) {
    assert!(
        poll_for(|| (!top_overlay_rows(shell).join("\n").contains("Saving…")).then_some(()))
            .await
            .is_some()
    );
}

async fn current(control: &Control, session: &str) -> Option<Goal> {
    let CommandOutcome::Goal(goal) = control
        .command(session, Command::Goal(GoalAction::Get.into()))
        .await
        .unwrap()
    else {
        panic!("goal response")
    };
    goal
}

#[tokio::test]
async fn goal_palette_manages_goals_through_real_keys_locally_and_remotely() {
    for connected in [false, true] {
        let host_dir = TempDir::new().unwrap();
        let client_dir = TempDir::new().unwrap();
        let (ready, held) = tokio::sync::oneshot::channel();
        let mut finish = aj_app::test_support::finalized_text_message("verified");
        finish.content.push(AssistantContent::ToolCall(ToolCall {
            id: "complete-goal".into(),
            name: "update_goal".into(),
            arguments: serde_json::json!({"status":"complete"}),
        }));
        finish.stop_reason = StopReason::ToolUse;
        let provider = Arc::new(HoldFirst {
            ready: StdMutex::new(Some(ready)),
            next: crate::remote::tests::scripted(
                vec![finish, aj_app::test_support::finalized_text_message("done")],
                0,
                Duration::ZERO,
            ),
        });
        let (mut world, shell, config, remote) = if connected {
            let handles = crate::remote::tests::HostHandles::new(&host_dir);
            let config = Arc::clone(&handles.config);
            let host = crate::remote::tests::scripted_host(&host_dir, provider, handles, None);
            let server = crate::remote::RemoteServer::bind(
                host.clone(),
                "127.0.0.1:0".parse().unwrap(),
                crate::remote::IdentityGate::local(),
            )
            .await
            .unwrap();
            let remote = RemoteHost { host, server };
            let session = remote.host.create().await.unwrap();
            let (world, shell) = connect_world_and_shell(&client_dir, &remote, &[&session]).await;
            (world, shell, config, Some(remote))
        } else {
            let (world, shell) = world_and_shell(&host_dir, "streaming-text").await;
            world.handles().run_config.lock().unwrap().main.provider = provider;
            let config = Arc::clone(&world.config);
            (world, shell, config, None)
        };
        let session = world.session().to_string();
        let control = world.control.clone();
        let chat = Rc::clone(&world.chat);
        let status = Rc::clone(&world.status);
        let observed = Rc::clone(&shell);
        shell
            .borrow()
            .view()
            .editor
            .borrow_mut()
            .set_text("conversation draft");

        let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
            writer.write_all(b"\x0fgoal\r").unwrap();
            depth(&observed, 2).await;
            let initial = page(&observed, "Start goal").await;
            assert!(!initial.contains("Refresh") && !initial.to_lowercase().contains("host"));

            choose(&mut writer, "objective");
            depth(&observed, 3).await;
            let objective = "first line\nsecond line";
            // Keep the full multiline payload on the real input path as one paste.
            writer.write_all(b"\x1b[200~").unwrap();
            writer.write_all(objective.as_bytes()).unwrap();
            writer.write_all(b"\x1b[201~\r").unwrap();
            depth(&observed, 2).await;
            page(&observed, "first line").await;

            choose(&mut writer, "token budget");
            depth(&observed, 3).await;
            writer.write_all(b"100\r").unwrap();
            depth(&observed, 2).await;
            page(&observed, "100").await;
            assert!(
                current(&control, &session).await.is_none(),
                "editing fields does not start pursuit"
            );

            choose(&mut writer, "start goal");
            let (stream, cancel) = tokio::time::timeout(SETTLE_DEADLINE, held)
                .await
                .unwrap()
                .unwrap();
            assert!(
                poll_for(|| chat
                    .borrow()
                    .goal
                    .as_ref()
                    .filter(|goal| goal.status == GoalStatus::Active)
                    .cloned())
                .await
                .is_some()
            );
            let management = page(&observed, "Status").await;
            assert!(management.contains("first line"));
            let created = current(&control, &session).await.unwrap();
            assert_eq!(created.objective, objective);
            assert_eq!(created.token_budget, Some(100));
            wait_for_save(&observed).await;

            choose(&mut writer, "objective");
            depth(&observed, 3).await;
            page(&observed, "second line").await;
            writer.write_all(b" revised").unwrap();
            page(&observed, "second line revised").await;
            control
                .command(
                    &session,
                    Command::Goal(
                        GoalAction::Edit {
                            objective: "a concurrent objective change".into(),
                        }
                        .into(),
                    ),
                )
                .await
                .unwrap();
            assert!(
                poll_for(|| chat
                    .borrow()
                    .goal
                    .as_ref()
                    .filter(|goal| goal.objective == "a concurrent objective change")
                    .map(|_| ()))
                .await
                .is_some()
            );
            assert!(
                top_overlay_rows(&observed)
                    .join("\n")
                    .contains("second line revised"),
                "live goal updates must not replace the editor's draft"
            );
            config
                .lock()
                .unwrap()
                .disabled_tools
                .push("update_goal".into());
            writer.write_all(b"\r").unwrap();
            depth(&observed, 2).await;
            page(&observed, "Enable update_goal").await;
            assert!(
                top_overlay_rows(&observed)
                    .iter()
                    .any(|row| row.contains("> objective")),
                "a refused action keeps its filter"
            );
            choose(&mut writer, "objective");
            depth(&observed, 3).await;
            page(&observed, "second line revised").await;
            config
                .lock()
                .unwrap()
                .disabled_tools
                .retain(|tool| tool != "update_goal");
            writer.write_all(b"\r").unwrap();
            depth(&observed, 2).await;
            page(&observed, "Status").await;
            wait_for_save(&observed).await;
            assert_eq!(
                current(&control, &session).await.unwrap().objective,
                format!("{objective} revised")
            );

            choose(&mut writer, "pause");
            page(&observed, "Resume").await;
            wait_for_save(&observed).await;
            assert!(!cancel.is_cancelled());
            for step in aj_models::scripted::script_from_message(
                aj_app::test_support::finalized_text_message("current turn finished"),
                0,
                Duration::ZERO,
            )
            .steps
            {
                stream.push(step.event);
            }
            stream.end();
            assert!(
                poll_for(|| (!status.borrow().running).then_some(()))
                    .await
                    .is_some()
            );
            choose(&mut writer, "resume");
            assert!(
                poll_for(|| chat
                    .borrow()
                    .goal
                    .as_ref()
                    .filter(|goal| goal.status == GoalStatus::Complete)
                    .cloned())
                .await
                .is_some()
            );
            wait_for_save(&observed).await;
            let completed = page(&observed, "complete").await;
            assert!(
                !completed.contains("Refresh"),
                "completion arrives without a refresh action"
            );
            assert!(!completed.contains("View objective"));
            choose(&mut writer, "objective");
            depth(&observed, 3).await;
            page(&observed, "second line revised").await;
            writer.write_all(b"\x1b").unwrap();
            depth(&observed, 2).await;
            assert_eq!(
                current(&control, &session).await.unwrap().status,
                GoalStatus::Complete,
                "inspecting the objective and cancelling must not resume completed work"
            );
            choose(&mut writer, "clear");
            page(&observed, "Start goal").await;
            assert!(
                poll_for(|| chat.borrow().goal.is_none().then_some(()))
                    .await
                    .is_some()
            );
            assert!(current(&control, &session).await.is_none());
            writer.write_all(b"\x1b").unwrap();
            depth(&observed, 1).await;
            writer.write_all(b"\x1b").unwrap();
            depth(&observed, 0).await;
            assert_eq!(
                observed.borrow().view().editor.borrow().text(),
                "conversation draft"
            );
            writer.write_all(b"!").unwrap();
            assert!(
                poll_for(|| (observed.borrow().view().editor.borrow().text()
                    == "conversation draft!")
                    .then_some(()))
                .await
                .is_some()
            );
        })
        .await;
        exit.unwrap();
        assert!(
            user_rows(&world).is_empty(),
            "goal UI must not submit command text as user prompts"
        );
        if let Some(remote) = remote {
            remote.shutdown().await;
        } else {
            shut_down(&world).await;
        }
    }
}

#[tokio::test]
async fn goal_palette_shows_unsupported_host_and_remains_cancellable() {
    let dir = TempDir::new().unwrap();
    let remote = RemoteHost::start(&dir, "streaming-text").await;
    let (mut world, shell) = connect_world_and_shell(&dir, &remote, &[]).await;
    world.control = Control::remote(
        crate::remote::RemoteClient::new(&format!("{}/nowhere", remote.url())).unwrap(),
    );
    let observed = Rc::clone(&shell);
    let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
        writer.write_all(b"\x0fgoal\r").unwrap();
        depth(&observed, 2).await;
        page(&observed, "Start goal").await;
        choose(&mut writer, "objective");
        depth(&observed, 3).await;
        writer.write_all(b"keep this draft\r").unwrap();
        depth(&observed, 2).await;
        choose(&mut writer, "start goal");
        page(&observed, "Goals are not supported by this version of AJ.").await;
        choose(&mut writer, "objective");
        depth(&observed, 3).await;
        page(&observed, "keep this draft").await;
        writer.write_all(b"\x1b").unwrap();
        depth(&observed, 2).await;
        writer.write_all(b"\x1b").unwrap();
        depth(&observed, 1).await;
    })
    .await;
    exit.unwrap();
    remote.shutdown().await;
}

struct HoldAll(tokio::sync::mpsc::UnboundedSender<HeldInference>);

impl Provider for HoldAll {
    fn stream(&self, _: &ModelInfo, _: &Context, _: &StreamOptions) -> AssistantMessageEventStream {
        panic!("agent uses simple options")
    }

    fn stream_simple(
        &self,
        _: &ModelInfo,
        _: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        self.0
            .send((stream.clone(), options.base.cancel.clone().unwrap()))
            .unwrap();
        stream
    }
}

fn finish_inference(stream: AssistantMessageEventStream) {
    for step in aj_models::scripted::script_from_message(
        aj_app::test_support::finalized_text_message_with_usage("progress", 40),
        0,
        Duration::ZERO,
    )
    .steps
    {
        stream.push(step.event);
    }
    stream.end();
}

async fn goal_matching(chat: &Rc<RefCell<ChatState>>, predicate: impl Fn(&Goal) -> bool) -> Goal {
    poll_for(|| {
        chat.borrow()
            .goal
            .as_ref()
            .filter(|goal| predicate(goal))
            .cloned()
    })
    .await
    .expect("goal state arrives on the stream")
}

#[tokio::test]
async fn goal_palette_budget_replacement_and_completed_edit_through_both_adapters() {
    for connected in [false, true] {
        let host_dir = TempDir::new().unwrap();
        let client_dir = TempDir::new().unwrap();
        let (ready, mut held) = tokio::sync::mpsc::unbounded_channel();
        let handles = crate::remote::tests::HostHandles::new(&host_dir);
        let config = Arc::clone(&handles.config);
        let host =
            crate::remote::tests::scripted_host(&host_dir, Arc::new(HoldAll(ready)), handles, None);
        let server = crate::remote::RemoteServer::bind(
            host.clone(),
            "127.0.0.1:0".parse().unwrap(),
            crate::remote::IdentityGate::local(),
        )
        .await
        .unwrap();
        let remote = RemoteHost { host, server };
        let session = remote.host.create().await.unwrap();
        let (mut world, shell) = connect_world_and_shell(&client_dir, &remote, &[&session]).await;
        if !connected {
            world.control = Control::local(remote.host.clone());
        }
        let control = world.control.clone();
        let chat = Rc::clone(&world.chat);
        let observed = Rc::clone(&shell);
        let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
            writer.write_all(b"\x0fgoal\r").unwrap();
            depth(&observed, 2).await;
            choose(&mut writer, "objective");
            depth(&observed, 3).await;
            writer.write_all(b"original objective\r").unwrap();
            depth(&observed, 2).await;
            choose(&mut writer, "start goal");
            let (stream, _) = held.recv().await.unwrap();
            finish_inference(stream);
            let (_stream, cancel) = held.recv().await.unwrap();
            let original = goal_matching(&chat, |goal| goal.tokens_used > 0).await;
            assert_eq!(original.status, GoalStatus::Active);
            wait_for_save(&observed).await;

            choose(&mut writer, "token budget");
            depth(&observed, 3).await;
            writer
                .write_all(original.tokens_used.to_string().as_bytes())
                .unwrap();
            writer.write_all(b"\r").unwrap();
            depth(&observed, 2).await;
            let exhausted =
                goal_matching(&chat, |goal| goal.status == GoalStatus::BudgetLimited).await;
            assert_eq!(exhausted.id, original.id);
            assert_eq!(exhausted.tokens_used, original.tokens_used);
            wait_for_save(&observed).await;
            clear_filter(&mut writer);
            let rows = page(&observed, "Token budget").await;
            assert!(
                !rows.contains("Resume"),
                "exhausted goals cannot resume: {rows}"
            );
            assert!(
                !cancel.is_cancelled(),
                "budget edits do not cancel current work"
            );

            choose(&mut writer, "token budget");
            depth(&observed, 3).await;
            clear_filter(&mut writer);
            writer
                .write_all((original.tokens_used + 100).to_string().as_bytes())
                .unwrap();
            writer.write_all(b"\r").unwrap();
            depth(&observed, 2).await;
            goal_matching(&chat, |goal| {
                goal.token_budget == Some(original.tokens_used + 100)
            })
            .await;
            wait_for_save(&observed).await;
            clear_filter(&mut writer);
            page(&observed, "Resume").await;
            assert_eq!(
                current(&control, &session).await.unwrap().status,
                GoalStatus::BudgetLimited
            );
            choose(&mut writer, "token budget");
            depth(&observed, 3).await;
            clear_filter(&mut writer);
            writer.write_all(b"\r").unwrap();
            depth(&observed, 2).await;
            let unlimited = goal_matching(&chat, |goal| goal.token_budget.is_none()).await;
            assert_eq!(unlimited.status, GoalStatus::BudgetLimited);
            assert_eq!(unlimited.tokens_used, original.tokens_used);
            assert_eq!(unlimited.id, original.id);
            wait_for_save(&observed).await;
            choose(&mut writer, "resume");
            goal_matching(&chat, |goal| goal.status == GoalStatus::Active).await;
            wait_for_save(&observed).await;

            choose(&mut writer, "new goal");
            depth(&observed, 3).await;
            let rows = page(&observed, "Replace goal").await;
            assert!(
                !rows.contains("original objective"),
                "replacement has an independent draft"
            );
            choose(&mut writer, "objective");
            depth(&observed, 4).await;
            writer.write_all(b"replacement draft\r").unwrap();
            depth(&observed, 3).await;
            choose(&mut writer, "replace goal");
            depth(&observed, 4).await;
            page(&observed, "Replace unfinished goal").await;
            writer.write_all(b"\r").unwrap(); // Safe default: keep editing.
            depth(&observed, 3).await;
            assert_eq!(current(&control, &session).await.unwrap().id, original.id);
            clear_filter(&mut writer);
            page(&observed, "replacement draft").await;
            choose(&mut writer, "replace goal");
            depth(&observed, 4).await;
            config
                .lock()
                .unwrap()
                .disabled_tools
                .push("update_goal".into());
            choose(&mut writer, "replace unfinished");
            depth(&observed, 3).await;
            page(&observed, "Goal pursuit requires").await;
            assert_eq!(current(&control, &session).await.unwrap().id, original.id);
            clear_filter(&mut writer);
            page(&observed, "replacement draft").await;
            config
                .lock()
                .unwrap()
                .disabled_tools
                .retain(|tool| tool != "update_goal");
            choose(&mut writer, "replace goal");
            depth(&observed, 4).await;
            choose(&mut writer, "replace unfinished");
            depth(&observed, 3).await;
            let replacement = goal_matching(&chat, |goal| goal.id != original.id).await;
            assert_eq!(replacement.objective, "replacement draft");
            assert_eq!(replacement.tokens_used, 0);
            assert_eq!(replacement.time_used_seconds, 0);
            wait_for_save(&observed).await;
            writer.write_all(b"\x1b").unwrap();
            depth(&observed, 2).await;

            control
                .command(&session, Command::Goal(GoalAction::Complete.into()))
                .await
                .unwrap();
            goal_matching(&chat, |goal| goal.status == GoalStatus::Complete).await;
            choose(&mut writer, "objective");
            depth(&observed, 3).await;
            page(&observed, "Save and continue").await;
            writer.write_all(b"\r").unwrap(); // Unchanged complete objective reactivates.
            depth(&observed, 2).await;
            let continued = goal_matching(&chat, |goal| goal.status == GoalStatus::Active).await;
            assert_eq!(continued.id, replacement.id);
            assert_eq!(continued.objective, replacement.objective);
            wait_for_save(&observed).await;
            control
                .command(&session, Command::Goal(GoalAction::Complete.into()))
                .await
                .unwrap();
            goal_matching(&chat, |goal| goal.status == GoalStatus::Complete).await;
            choose(&mut writer, "new goal");
            depth(&observed, 3).await;
            choose(&mut writer, "objective");
            depth(&observed, 4).await;
            writer.write_all(b"completed replacement\r").unwrap();
            depth(&observed, 3).await;
            choose(&mut writer, "replace goal");
            goal_matching(&chat, |goal| goal.objective == "completed replacement").await;
            assert_eq!(
                observed.borrow().overlays.borrow().depth(),
                3,
                "completed replacement needs no extra confirmation"
            );
            wait_for_save(&observed).await;
            writer.write_all(b"\x1b").unwrap();
            depth(&observed, 2).await;
            choose(&mut writer, "objective");
            depth(&observed, 3).await;
            writer.write_all(b" with unsent changes").unwrap();
            let old = current(&control, &session).await.unwrap();
            control
                .command(
                    &session,
                    Command::Goal(aj_agent::goal::GoalRequest::for_goal(
                        old.id,
                        GoalAction::Replace {
                            objective: "concurrent replacement".into(),
                            token_budget: None,
                        },
                    )),
                )
                .await
                .unwrap();
            goal_matching(&chat, |goal| goal.objective == "concurrent replacement").await;
            page(&observed, "with unsent changes").await;
            writer.write_all(b"\r").unwrap();
            depth(&observed, 2).await;
            page(&observed, "goal changed").await;
            assert_eq!(
                current(&control, &session).await.unwrap().objective,
                "concurrent replacement"
            );
            choose(&mut writer, "unsent objective");
            depth(&observed, 3).await;
            page(&observed, "with unsent changes").await;
            writer.write_all(b"\x1b").unwrap();
            depth(&observed, 2).await;
            choose(&mut writer, "token budget");
            depth(&observed, 3).await;
            writer.write_all(b"71").unwrap();
            let old = current(&control, &session).await.unwrap();
            control
                .command(
                    &session,
                    Command::Goal(aj_agent::goal::GoalRequest::for_goal(
                        old.id,
                        GoalAction::Replace {
                            objective: "another replacement".into(),
                            token_budget: None,
                        },
                    )),
                )
                .await
                .unwrap();
            goal_matching(&chat, |goal| goal.objective == "another replacement").await;
            writer.write_all(b"\r").unwrap();
            depth(&observed, 2).await;
            wait_for_save(&observed).await;
            assert_eq!(
                current(&control, &session).await.unwrap().token_budget,
                None
            );
            choose(&mut writer, "unsent budget");
            depth(&observed, 3).await;
            page(&observed, "71").await;
            writer.write_all(b"\x1b").unwrap();
            depth(&observed, 2).await;
            control
                .command(&session, Command::Goal(GoalAction::Pause.into()))
                .await
                .unwrap();
        })
        .await;
        exit.unwrap();
        remote.shutdown().await;
    }
}

#[tokio::test]
async fn goal_palette_reports_delayed_refusal_after_all_goal_windows_close() {
    use axum::{Json, Router, routing::post};
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let signal = Arc::clone(&started);
    let gate = Arc::clone(&release);
    let router = Router::new().route(
        "/v1/sessions/{session}/goal",
        post(move || {
            let signal = Arc::clone(&signal);
            let gate = Arc::clone(&gate);
            async move {
                signal.notify_one();
                gate.notified().await;
                (
                    reqwest::StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "code": "conflict", "message": "delayed goal refusal",
                    })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let serving = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = TempDir::new().unwrap();
    let remote = RemoteHost::start(&dir, "streaming-text").await;
    let (mut world, shell) = connect_world_and_shell(&dir, &remote, &[]).await;
    world.control = Control::remote(crate::remote::RemoteClient::new(&url).unwrap());
    let observed = Rc::clone(&shell);
    let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
        writer.write_all(b"\x0fgoal\r").unwrap();
        depth(&observed, 2).await;
        choose(&mut writer, "objective");
        depth(&observed, 3).await;
        writer.write_all(b"requested objective\r").unwrap();
        depth(&observed, 2).await;
        choose(&mut writer, "start goal");
        tokio::time::timeout(SETTLE_DEADLINE, started.notified())
            .await
            .unwrap();
        writer.write_all(b"\x1b").unwrap();
        depth(&observed, 1).await;
        writer.write_all(b"\x1b").unwrap();
        depth(&observed, 0).await;
        writer.write_all(b"still typing").unwrap();
        assert!(
            poll_for(
                || (observed.borrow().view().editor.borrow().text() == "still typing")
                    .then_some(())
            )
            .await
            .is_some()
        );
        assert!(
            !toast_lines(&observed)
                .iter()
                .any(|line| line.contains("delayed goal refusal"))
        );
        release.notify_one();
        assert!(
            poll_for(|| toast_lines(&observed)
                .iter()
                .any(|line| line.contains("delayed goal refusal"))
                .then_some(()))
            .await
            .is_some()
        );
        assert_eq!(observed.borrow().overlays.borrow().depth(), 0);
        assert_eq!(
            observed.borrow().view().editor.borrow().text(),
            "still typing"
        );
    })
    .await;
    exit.unwrap();
    remote.shutdown().await;
    serving.abort();
    let _ = serving.await;
}

#[tokio::test]
async fn goal_palette_prompts_on_entry_not_live_updates_or_reconnect() {
    for connected in [false, true] {
        let host_dir = TempDir::new().unwrap();
        let client_dir = TempDir::new().unwrap();
        let (ready, mut held) = tokio::sync::mpsc::unbounded_channel();
        let host = crate::remote::tests::scripted_host(
            &host_dir,
            Arc::new(HoldAll(ready)),
            crate::remote::tests::HostHandles::new(&host_dir),
            None,
        );
        let server = crate::remote::RemoteServer::bind(
            host.clone(),
            "127.0.0.1:0".parse().unwrap(),
            crate::remote::IdentityGate::local(),
        )
        .await
        .unwrap();
        let remote = RemoteHost { host, server };
        let saved = remote.host.create().await.unwrap();
        let control = if connected {
            Control::remote(crate::remote::RemoteClient::new(&remote.url()).unwrap())
        } else {
            Control::local(remote.host.clone())
        };
        control
            .command(
                &saved,
                Command::Goal(
                    GoalAction::Create {
                        objective: "saved objective".into(),
                        token_budget: None,
                    }
                    .into(),
                ),
            )
            .await
            .unwrap();
        let (stream, _) = held.recv().await.unwrap();
        control
            .command(&saved, Command::Goal(GoalAction::Pause.into()))
            .await
            .unwrap();
        finish_inference(stream);
        crate::remote::tests::bounded("saved goal turn to finish", async {
            while control
                .sessions()
                .await
                .unwrap()
                .sessions
                .iter()
                .any(|row| row.id == saved && row.working)
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        let (mut world, shell) = connect_world_and_shell(&client_dir, &remote, &[&saved]).await;
        world.control = control.clone();
        let observed = Rc::clone(&shell);
        let chat = Rc::clone(&world.chat);
        let session = saved.clone();
        let actions = control.clone();
        let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
            depth(&observed, 1).await;
            page(&observed, "Leave stopped").await;
            page(&observed, "Resume").await;
            writer.write_all(b"\r").unwrap();
            depth(&observed, 0).await;
            assert_eq!(
                current(&actions, &session).await.unwrap().status,
                GoalStatus::Paused
            );
            actions
                .command(&session, Command::Goal(GoalAction::Block.into()))
                .await
                .unwrap();
            goal_matching(&chat, |goal| goal.status == GoalStatus::Blocked).await;
            writer.write_all(b"draft").unwrap();
            assert!(
                poll_for(
                    || (observed.borrow().view().editor.borrow().text() == "draft").then_some(())
                )
                .await
                .is_some()
            );
            assert_eq!(
                observed.borrow().overlays.borrow().depth(),
                0,
                "live stopped states never open a prompt"
            );
        })
        .await;
        exit.unwrap();
        assert!(
            held.try_recv().is_err(),
            "entry and leave stopped never run inference"
        );

        world.stream_mut().cut();
        let observed = Rc::clone(&shell);
        let status = Rc::clone(&world.status);
        let chat = Rc::clone(&world.chat);
        let notice = reattached_notice(&world.control);
        assert!(!notices_of(&chat.borrow()).iter().any(|text| text == notice));
        let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
            assert!(
                poll_for(|| notices_of(&chat.borrow())
                    .iter()
                    .any(|text| text == notice)
                    .then_some(()))
                .await
                .is_some()
            );
            assert_eq!(status.borrow().connection, Connection::Connected);
            writer.write_all(b" after reconnect").unwrap();
            assert!(
                poll_for(|| (observed.borrow().view().editor.borrow().text()
                    == "draft after reconnect")
                    .then_some(()))
                .await
                .is_some()
            );
            assert_eq!(observed.borrow().overlays.borrow().depth(), 0);
        })
        .await;
        exit.unwrap();

        let other = remote.host.create().await.unwrap();
        let (mut app, _writer, _root) = app_over(&shell).await;
        assert!(matches!(
            apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Resume(other)).await,
            Focus::Moved
        ));
        settle_pending_transition(&mut app, &shell, &mut world).await;
        assert!(matches!(
            apply_focus_request(
                &mut app,
                &shell,
                &mut world,
                FocusRequest::Resume(saved.clone())
            )
            .await,
            Focus::Moved
        ));
        settle_pending_transition(&mut app, &shell, &mut world).await;
        let observed = Rc::clone(&shell);
        let chat = Rc::clone(&world.chat);
        let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
            depth(&observed, 1).await;
            page(&observed, "Leave stopped").await;
            choose(&mut writer, "resume");
            depth(&observed, 0).await;
            goal_matching(&chat, |goal| goal.status == GoalStatus::Active).await;
            let (stream, _) = held.recv().await.unwrap();
            control
                .command(&saved, Command::Goal(GoalAction::Pause.into()))
                .await
                .unwrap();
            finish_inference(stream);
            goal_matching(&chat, |goal| goal.status == GoalStatus::Paused).await;
            writer.write_all(b"!").unwrap();
            assert!(
                poll_for(|| observed
                    .borrow()
                    .view()
                    .editor
                    .borrow()
                    .text()
                    .ends_with('!')
                    .then_some(()))
                .await
                .is_some()
            );
            assert_eq!(
                observed.borrow().overlays.borrow().depth(),
                0,
                "a live pause is not an entry"
            );
        })
        .await;
        exit.unwrap();
        remote.shutdown().await;
    }
}
