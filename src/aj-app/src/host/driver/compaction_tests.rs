use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::goal::GoalAction;
use aj_agent::queue::PendingKind;
use aj_agent::tool::TaskStatus;
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::streaming::AssistantMessageEventStream;
use aj_models::types::{
    AssistantContent, Context, SimpleStreamOptions, StopReason, StreamOptions, ToolCall,
    UserContent,
};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};

use crate::host::{Command, HostSetup, SessionHost};
use crate::session_setup::RunConfigDefaults;
use crate::settings::ConfigLayers;
use crate::test_support::{finalized_text_message, scripted_run_config_with_window};

struct RecordingProvider {
    inner: Arc<dyn Provider>,
    requests: Arc<Mutex<Vec<Context>>>,
}

impl Provider for RecordingProvider {
    fn stream(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &StreamOptions,
    ) -> AssistantMessageEventStream {
        self.requests.lock().unwrap().push(context.clone());
        self.inner.stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.requests.lock().unwrap().push(context.clone());
        self.inner.stream_simple(model, context, options)
    }
}

// A panic can leave host teardown writing after the test future is dropped.
// Both guards outlive those tasks, under the scratch runner's owned root.
fn task_lifetime_dir() -> &'static TempDir {
    static ROOT: OnceLock<TempDir> = OnceLock::new();
    let root = ROOT.get_or_init(|| TempDir::with_prefix("aj-task-lifetime-").unwrap());
    Box::leak(Box::new(TempDir::new_in(root.path()).unwrap()))
}

fn test_host(
    dir: &TempDir,
    config: aj_conf::Config,
    run: crate::session_setup::RunConfigSnapshot,
) -> SessionHost {
    SessionHost::new(HostSetup {
        config: Arc::new(Mutex::new(config.clone())),
        layers: Arc::new(Mutex::new(ConfigLayers {
            user: config,
            project: Default::default(),
            project_path: None,
            writes: Default::default(),
        })),
        catalog: Arc::new(Vec::new()),
        defaults: RunConfigDefaults::fixed(run),
        restore: None,
        persistence: aj_session::ConversationPersistence::new(dir.path().join("sessions")),
        auth: aj_models::auth::AuthStorage::new(dir.path().join("auth.json")),
        working_directory: dir.path().to_path_buf(),
        name: None,
        idle_grace: None,
        live_capacity: None,
        list_coalesce: None,
    })
    .unwrap()
}

async fn prompt(host: &SessionHost, id: &str, text: &str) {
    host.command(
        id,
        Command::Prompt {
            agent: AgentId::Main,
            content: vec![UserContent::text(text)],
        },
    )
    .await
    .unwrap();
}

async fn fence(host: &SessionHost, id: &str) {
    // Read-only commands catch up the real driver's events and completed joins.
    host.command(id, Command::Goal(GoalAction::Get.into()))
        .await
        .unwrap();
}

async fn idle(host: &SessionHost, id: &str, session: &super::LiveSession) {
    loop {
        fence(host, id).await;
        if !session.status().working {
            return;
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn compaction_settings_edits_apply_at_the_next_tool_batch_boundary() {
    let scenario = async {
        for (key, before, after, compacts) in [
            ("auto_compact_during_turn", "false", "true", true),
            ("auto_compact_during_turn", "true", "false", false),
            ("auto_compact", "false", "true", true),
            ("auto_compact", "true", "false", false),
            ("compact_threshold", "0.95", "0.85", true),
            ("compact_threshold", "0.85", "0.95", false),
            ("compact_keep_recent", "100000", "1000", true),
            ("compact_keep_recent", "1000", "100000", false),
        ] {
            let dir = task_lifetime_dir();
            let evidence = dir.path().join("evidence.txt");
            std::fs::write(&evidence, "EVIDENCE").unwrap();
            let mut batch = finalized_text_message("");
            batch.stop_reason = StopReason::ToolUse;
            batch.usage.input = 900;
            batch.content = vec![AssistantContent::ToolCall(ToolCall {
                id: "read-evidence".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": evidence}),
            })];
            let mut replies = vec![
                finalized_text_message(&"OLD_WORK_TO_SUMMARIZE ".repeat(3000)),
                batch,
            ];
            if compacts {
                replies.push(finalized_text_message("CHECKPOINT_SUMMARY"));
            }
            replies.push(finalized_text_message("finished"));
            let run = scripted_run_config_with_window(replies, 1000);
            let requests = Arc::new(Mutex::new(Vec::new()));
            {
                let mut run = run.lock().unwrap();
                run.main.provider = Arc::new(RecordingProvider {
                    inner: Arc::clone(&run.main.provider),
                    requests: Arc::clone(&requests),
                });
            }
            let mut config = aj_conf::Config {
                auto_compact: true,
                auto_compact_during_turn: true,
                compact_keep_recent: 1000,
                ..Default::default()
            };
            aj_conf::Config::option(key)
                .unwrap()
                .apply_str(before, &mut config)
                .unwrap();
            let host = test_host(dir, config, run.lock().unwrap().clone());
            host.inner.shared.layers.lock().unwrap().project_path =
                Some(dir.path().join("config.toml"));
            let id = host.create().await.unwrap();
            let session = Arc::clone(&host.inner.sessions.lock().await.get(&id).unwrap().session);
            prompt(&host, &id, "warm history").await;
            idle(&host, &id, &session).await;
            assert_eq!(requests.lock().unwrap().len(), 1);

            // Hold the completed real tool before the compaction check, so the
            // edit is acknowledged while the same run is still active.
            let (completed, tool_completed) = oneshot::channel();
            let (release, proceed) = oneshot::channel();
            let barrier = Mutex::new(Some((completed, proceed)));
            let _listener = session
                .core
                .agent
                .lock()
                .await
                .subscribe(Arc::new(move |event| {
                    let barrier = if let AgentEvent::ToolExecutionEnd { is_error, .. } = event {
                        assert!(!is_error, "the real read_file must succeed");
                        barrier.lock().unwrap().take()
                    } else {
                        None
                    };
                    Box::pin(async move {
                        if let Some((completed, proceed)) = barrier {
                            completed.send(()).unwrap();
                            proceed.await.unwrap();
                        }
                        Ok(())
                    })
                }));
            prompt(&host, &id, "read the evidence").await;
            tool_completed.await.unwrap();
            assert!(session.status().working);
            assert_eq!(requests.lock().unwrap().len(), 2);
            assert_eq!(host.config(&id).await.unwrap().effective[key], before);
            host.edit_config(
                &id,
                aj_wire::ConfigEdit {
                    key: key.into(),
                    value: Some(after.into()),
                    persist: aj_wire::PersistAction::ProjectSet,
                },
            )
            .await
            .unwrap();
            assert_eq!(host.config(&id).await.unwrap().effective[key], after);
            release.send(()).unwrap();
            idle(&host, &id, &session).await;

            {
                let requests = requests.lock().unwrap();
                assert_eq!(
                    requests.len(),
                    if compacts { 4 } else { 3 },
                    "{key}: {before} -> {after}"
                );
                if compacts {
                    assert_eq!(
                        requests[2].system_prompt.as_deref(),
                        Some(aj_session::compaction::SUMMARIZATION_SYSTEM_PROMPT)
                    );
                }
                let continuation =
                    serde_json::to_string(&requests.last().unwrap().messages).unwrap();
                assert_eq!(
                    continuation.contains("CHECKPOINT_SUMMARY"),
                    compacts,
                    "{key}"
                );
                assert_eq!(
                    continuation.contains("OLD_WORK_TO_SUMMARIZE"),
                    !compacts,
                    "{key}"
                );
                assert!(continuation.contains("EVIDENCE"), "{key}");
            }
            host.shutdown().await;
        }
    };
    tokio::time::timeout(Duration::from_secs(20), scenario)
        .await
        .expect("live compaction settings regression timed out");
}

#[tokio::test]
async fn completed_join_folds_buffered_failure_before_it_can_wake_queued_work() {
    use crate::host::live::{LiveSession, SessionStatus, settings_of};
    use crate::session::{SessionCore, SessionEntry, SessionSpec};
    use aj_agent::events::CompactionReason;
    use aj_session::{AppendHandoff, TaggedEvent};

    let dir = task_lifetime_dir();
    let config = aj_conf::Config::default();
    let run = scripted_run_config_with_window(vec![finalized_text_message("unwanted wake")], 1000)
        .lock()
        .unwrap()
        .clone();
    let host = test_host(dir, config.clone(), run.clone());
    let (core, _) = SessionCore::build(
        &config,
        run,
        &host.inner.persistence,
        &SessionSpec::Create {
            entry: SessionEntry::Startup,
            session_env: None,
        },
        None,
    )
    .unwrap();
    let (settings, oracle_settings) = settings_of(&core.run_config);
    let status = SessionStatus {
        epoch: "join-order-test".into(),
        last_seq: core.log.lock().await.last_seq(),
        working: true,
        settings,
        oracle_settings,
        goal: None,
        finished_subs: Default::default(),
        driven_subs: Default::default(),
        interrupted_subs: Default::default(),
        last_activity: chrono::Utc::now(),
        tag: None,
        archived: false,
        last_work: std::time::Instant::now(),
    };
    let (request_tx, requests) = mpsc::unbounded_channel();
    let session = Arc::new(LiveSession::new(
        core,
        AppendHandoff::default(),
        status,
        request_tx,
    ));
    let (events_tx, events) = mpsc::unbounded_channel();
    let (_failure_tx, failure) = oneshot::channel();
    let mut driver = super::Driver::new(
        Arc::clone(&session),
        Arc::clone(&host.inner.shared),
        events,
        requests,
        failure,
        0,
    );
    driver.lifecycle.mark_running(AgentId::Main);
    session
        .core
        .message_queues
        .append_follow_up(AgentId::Main, "PENDING");
    assert!(!session.is_draining());
    assert!(!driver.compaction_failed);

    // Model the select race deterministically: the final event is available,
    // but the completed join is handled before the event arm gets another poll.
    events_tx
        .send(TaggedEvent {
            event: AgentEvent::CompactionEnd {
                agent_id: AgentId::Main,
                reason: CompactionReason::Threshold,
                tokens_before: 900,
                tokens_after: 900,
                summary: None,
                error: Some("summary failed".into()),
                usage: None,
            },
            entry: None,
            branch_settings: None,
        })
        .unwrap();
    driver.on_join(crate::turn::Joined {
        agent: AgentId::Main,
        outcome: Ok(Err(aj_agent::TurnError::Recoverable(
            "compaction failed".into(),
        ))),
    });
    assert!(
        !session.status().working,
        "join must not restart queued work"
    );
    assert_eq!(
        session.core.message_queues.snapshot(AgentId::Main).text,
        "PENDING"
    );
    host.shutdown().await;
}

#[tokio::test]
async fn failed_mid_turn_compaction_holds_queued_input_and_late_task_until_user_prompt() {
    let scenario = async {
        let dir = task_lifetime_dir();
        let gate = dir.path().join("task-gate");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&gate)
                .status()
                .unwrap()
                .success()
        );
        let mut batch = finalized_text_message("");
        batch.stop_reason = StopReason::ToolUse;
        batch.usage.input = 900;
        batch.content = vec![AssistantContent::ToolCall(ToolCall {
            id: "background-work".into(),
            name: "bash".into(),
            arguments: serde_json::json!({
                "command": format!("read -r token < '{}'; printf 'LATE_TASK_RESULT\\n'", gate.display()),
                "description": "wait for test-controlled completion",
                "run_in_background": true
            }),
        })];
        let run = scripted_run_config_with_window(
            vec![
                finalized_text_message(&"OLD_WORK_TO_SUMMARIZE ".repeat(3000)),
                batch,
                finalized_text_message("   "),
                finalized_text_message("explicit restart answered"),
            ],
            1000,
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        {
            let mut run = run.lock().unwrap();
            run.main.provider = Arc::new(RecordingProvider {
                inner: Arc::clone(&run.main.provider),
                requests: Arc::clone(&requests),
            });
        }
        let config = aj_conf::Config {
            auto_compact: true,
            auto_compact_during_turn: true,
            compact_keep_recent: 1000,
            spill_dir: Some(dir.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let host = test_host(dir, config, run.lock().unwrap().clone());
        let id = host.create().await.unwrap();
        let session = Arc::clone(&host.inner.sessions.lock().await.get(&id).unwrap().session);
        prompt(&host, &id, "warm history").await;
        idle(&host, &id, &session).await;
        assert_eq!(requests.lock().unwrap().len(), 1);

        let (summarizing, summary_started) = oneshot::channel();
        let (release_summary, proceed) = oneshot::channel();
        let barrier = Mutex::new(Some((summarizing, proceed)));
        let (events, mut observed) = mpsc::unbounded_channel();
        let _listener = session
            .core
            .agent
            .lock()
            .await
            .subscribe(Arc::new(move |event| {
                let barrier = if matches!(
                    event,
                    AgentEvent::CompactionStart {
                        agent_id: AgentId::Main,
                        ..
                    }
                ) {
                    barrier.lock().unwrap().take()
                } else {
                    None
                };
                if matches!(
                    event,
                    AgentEvent::CompactionEnd { .. } | AgentEvent::TaskEnd { .. }
                ) {
                    events.send(event.clone()).unwrap();
                }
                Box::pin(async move {
                    if let Some((started, proceed)) = barrier {
                        started.send(()).unwrap();
                        proceed.await.unwrap();
                    }
                    Ok(())
                })
            }));
        prompt(&host, &id, "start background work").await;
        summary_started.await.unwrap();
        fence(&host, &id).await;
        assert!(
            session.status().working,
            "mid-turn compaction must remain working"
        );
        let tasks = session.core.task_registry.snapshot();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::Running);
        assert!(!session.core.task_registry.has_notices(AgentId::Main));
        prompt(&host, &id, "QUEUED_FOLLOW_UP").await;
        assert_eq!(
            session.core.message_queues.snapshot(AgentId::Main).kind,
            Some(PendingKind::FollowUp)
        );
        release_summary.send(()).unwrap();
        let AgentEvent::CompactionEnd {
            error: Some(error), ..
        } = observed.recv().await.unwrap()
        else {
            panic!("expected failed summary before background completion")
        };
        assert!(error.contains("empty summary"), "{error}");
        idle(&host, &id, &session).await;
        assert_eq!(
            requests.lock().unwrap().len(),
            3,
            "join must not restart inference"
        );
        assert_eq!(
            session.core.message_queues.snapshot(AgentId::Main).text,
            "QUEUED_FOLLOW_UP"
        );
        {
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests[2].system_prompt.as_deref(),
                Some(aj_session::compaction::SUMMARIZATION_SYSTEM_PROMPT)
            );
            assert!(
                serde_json::to_string(&requests[2].messages)
                    .unwrap()
                    .contains("OLD_WORK_TO_SUMMARIZE")
            );
        }

        // FIFO completion is released only after the failed turn has joined.
        // This exercises a real background Bash driver's late TaskEnd, not an injected event.
        assert!(
            tokio::process::Command::new("bash")
                .arg("-c")
                .arg(format!("printf 'finish\\n' > '{}'", gate.display()))
                .kill_on_drop(true)
                .status()
                .await
                .unwrap()
                .success()
        );
        assert!(matches!(
            observed.recv().await.unwrap(),
            AgentEvent::TaskEnd {
                agent_id: AgentId::Main,
                status: TaskStatus::Exited(Some(0)),
                ..
            }
        ));
        fence(&host, &id).await;
        assert!(!session.status().working, "late TaskEnd must not wake Main");
        assert_eq!(requests.lock().unwrap().len(), 3);
        assert!(session.core.task_registry.has_notices(AgentId::Main));
        assert_eq!(
            session.core.message_queues.snapshot(AgentId::Main).text,
            "QUEUED_FOLLOW_UP"
        );

        prompt(&host, &id, "EXPLICIT_RESTART").await;
        idle(&host, &id, &session).await;
        assert!(!session.core.message_queues.has_pending(AgentId::Main));
        assert!(!session.core.task_registry.has_notices(AgentId::Main));
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 4);
            let resumed = serde_json::to_string(&requests[3].messages).unwrap();
            for text in [
                "EXPLICIT_RESTART",
                "QUEUED_FOLLOW_UP",
                "LATE_TASK_RESULT",
                "<task-notification>",
            ] {
                assert!(
                    resumed.contains(text),
                    "pending input missing from resumed inference: {text}"
                );
            }
        }
        host.shutdown().await;
    };
    tokio::time::timeout(Duration::from_secs(20), scenario)
        .await
        .expect("composed compaction regression timed out");
}
