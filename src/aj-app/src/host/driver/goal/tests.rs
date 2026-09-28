use super::*;
use crate::host::{Command, CommandOutcome, HostSetup, Request, SessionHost};
use crate::session_setup::RunConfigDefaults;
use crate::settings::ConfigLayers;
use crate::test_support::{finalized_text_message, scripted_run_config};
use aj_models::types::{AssistantMessage, StopReason, ToolCall, UserContent};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

fn host(dir: &tempfile::TempDir, responses: Vec<AssistantMessage>) -> SessionHost {
    SessionHost::new(HostSetup {
        config: Arc::new(Mutex::new(aj_conf::Config::default())),
        layers: Arc::new(Mutex::new(ConfigLayers {
            user: aj_conf::Config::default(),
            project: Default::default(),
            project_path: None,
            writes: Default::default(),
        })),
        catalog: Arc::new(Vec::new()),
        defaults: RunConfigDefaults::fixed(scripted_run_config(responses).lock().unwrap().clone()),
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

#[tokio::test]
async fn goal_replacement_after_admission_cannot_adopt_delayed_start_or_descendants() {
    use aj_models::types::Usage;

    let dir = tempfile::TempDir::new().unwrap();
    let mut old = finalized_text_message("delegate old work");
    old.usage = Usage {
        input: 50,
        ..Default::default()
    };
    old.stop_reason = StopReason::ToolUse;
    old.content.push(AssistantContent::ToolCall(ToolCall {
        id: "old-child".into(),
        name: "agent".into(),
        arguments: serde_json::json!({"task":"finish old work"}),
    }));
    let mut child = finalized_text_message("old child finished");
    child.usage = Usage {
        input: 13,
        ..Default::default()
    };
    let mut new = finalized_text_message("new goal work");
    new.usage = Usage {
        input: 7,
        ..Default::default()
    };
    let host = host(&dir, vec![old, child, new]);
    let id = host.create().await.unwrap();
    let session = Arc::clone(&host.inner.sessions.lock().await.get(&id).unwrap().session);
    let tool_session = Arc::downgrade(&session);
    let inference_session = std::sync::Weak::clone(&tool_session);
    let (admitted, ready) = oneshot::channel();
    let (release, proceed) = oneshot::channel();
    let barrier = Arc::new(Mutex::new(Some((admitted, proceed))));
    session.core.agent.lock().await.set_goal_control(
        Arc::new(move |action, cancel, revision| {
            let session = tool_session.upgrade().unwrap();
            Box::pin(async move {
                let (reply, result) = oneshot::channel();
                assert!(session.send(Request::GoalTool {
                    action,
                    cancel,
                    revision,
                    reply
                }));
                result.await.unwrap_or(Err(GoalError::Unsupported))
            })
        }),
        Arc::new(move |cancel| {
            let session = inference_session.upgrade().unwrap();
            let barrier = barrier.lock().unwrap().take();
            Box::pin(async move {
                let (reply, result) = oneshot::channel();
                assert!(session.send(Request::GoalInference { cancel, reply }));
                let revision = result.await.unwrap()?;
                if let Some((admitted, proceed)) = barrier {
                    admitted.send(()).unwrap();
                    proceed.await.unwrap();
                }
                Ok(revision)
            })
        }),
    );
    let CommandOutcome::Goal(Some(original)) = host
        .command(
            &id,
            Command::Goal(
                GoalAction::Create {
                    objective: "original work".into(),
                    token_budget: None,
                }
                .into(),
            ),
        )
        .await
        .unwrap()
    else {
        panic!("created goal")
    };
    tokio::time::timeout(Duration::from_secs(10), ready)
        .await
        .unwrap()
        .unwrap();
    // The inference already has an owner, but neither its MessageStart nor its
    // descendant's spawn can reach the driver until after replacement.
    let CommandOutcome::Goal(Some(replacement)) = host
        .command(
            &id,
            Command::Goal(GoalRequest::for_goal(
                original.id,
                GoalAction::Replace {
                    objective: "replacement work".into(),
                    token_budget: Some(1),
                },
            )),
        )
        .await
        .unwrap()
    else {
        panic!("replaced goal")
    };
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let CommandOutcome::Goal(Some(goal)) = host
                .command(&id, Command::Goal(GoalAction::Get.into()))
                .await
                .unwrap()
            else {
                panic!("goal exists")
            };
            if !session.status().working && goal.status != GoalStatus::Active {
                assert_eq!(goal.id, replacement.id);
                assert_eq!(goal.status, GoalStatus::BudgetLimited);
                assert_eq!(
                    goal.tokens_used, 7,
                    "neither old inference nor its descendant belongs to the replacement"
                );
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        session.core.log.lock().await.sub_agent_ids().len(),
        1,
        "the old response must actually delegate work"
    );
    host.shutdown().await;
}

#[tokio::test]
async fn queued_goal_mutations_cannot_undo_the_users_clear_or_cancel() {
    for clear in [true, false] {
        let dir = tempfile::TempDir::new().unwrap();
        let mut response = finalized_text_message("updating goal");
        response.stop_reason = StopReason::ToolUse;
        response.content.push(AssistantContent::ToolCall(ToolCall {
            id: "goal-tool".into(),
            name: if clear { "create_goal" } else { "update_goal" }.into(),
            arguments: if clear {
                serde_json::json!({"objective":"old request"})
            } else {
                serde_json::json!({"status":"complete"})
            },
        }));
        let mut responses = vec![response];
        if clear {
            responses.push(finalized_text_message("turn finished without cancelling"));
        }
        let host = host(&dir, responses);
        let id = host.create().await.unwrap();
        let session = Arc::clone(&host.inner.sessions.lock().await.get(&id).unwrap().session);
        let weak = Arc::downgrade(&session);
        let inference_session = std::sync::Weak::clone(&weak);
        let submitted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = Arc::clone(&submitted);
        // Exercise the real tool/turn token and driver queue. Admit the
        // user's interruption immediately ahead of that tool's request.
        session.core.agent.lock().await.set_goal_control(
            Arc::new(move |action, cancel, revision| {
                let session = weak.upgrade().unwrap();
                let observed = Arc::clone(&observed);
                Box::pin(async move {
                    let (reply, user_result) = oneshot::channel();
                    assert!(session.send(Request::Command {
                        command: if clear {
                            Command::Goal(GoalAction::Clear.into())
                        } else {
                            Command::Cancel {
                                agent: AgentId::Main,
                            }
                        },
                        reply,
                    }));
                    let (reply, result) = oneshot::channel();
                    assert!(session.send(Request::GoalTool {
                        action,
                        cancel,
                        revision,
                        reply
                    }));
                    observed.store(true, std::sync::atomic::Ordering::Release);
                    let _ = user_result.await;
                    result.await.unwrap_or(Err(GoalError::Unsupported))
                })
            }),
            Arc::new(move |cancel| {
                let session = inference_session.upgrade().unwrap();
                Box::pin(async move {
                    let (reply, result) = oneshot::channel();
                    assert!(session.send(Request::GoalInference { cancel, reply }));
                    result.await.unwrap_or(Err(GoalError::Unsupported))
                })
            }),
        );
        let start = if clear {
            Command::Prompt {
                agent: AgentId::Main,
                content: vec![UserContent::text("set a goal")],
            }
        } else {
            Command::Goal(
                GoalAction::Create {
                    objective: "keep working".into(),
                    token_budget: None,
                }
                .into(),
            )
        };
        host.command(&id, start).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let outcome = host
                    .command(&id, Command::Goal(GoalAction::Get.into()))
                    .await
                    .unwrap();
                if submitted.load(std::sync::atomic::Ordering::Acquire) && !session.status().working
                {
                    match outcome {
                        CommandOutcome::Goal(None) if clear => break,
                        CommandOutcome::Goal(Some(goal))
                            if !clear && goal.status == GoalStatus::Paused =>
                        {
                            break;
                        }
                        other => panic!("cancelled tool mutated goal: {other:?}"),
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        if clear {
            assert_eq!(
                session
                    .core
                    .agent
                    .lock()
                    .await
                    .last_assistant()
                    .unwrap()
                    .stop_reason,
                StopReason::Stop
            );
        }
        host.shutdown().await;
    }
}
