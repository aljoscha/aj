use std::time::Duration;

use super::*;
use crate::remote::tests::{HostHandles, addr, bounded, host_setup, scripted, snapshot};
use crate::remote::{IdentityGate, RemoteServer};
use aj_agent::goal::{Goal, GoalAction, GoalRequest, GoalStatus};
use aj_app::test_support::finalized_text_message_with_usage;

async fn goal(control: &Control, session: &str, action: impl Into<GoalRequest>) -> Option<Goal> {
    match control
        .command(session, Command::Goal(action.into()))
        .await
        .unwrap()
    {
        CommandOutcome::Goal(goal) => goal,
        other => panic!("expected a goal outcome, got {other:?}"),
    }
}

async fn limited(control: &Control, session: &str) -> Goal {
    bounded("goal budget stop", async {
        loop {
            if control
                .sessions()
                .await
                .unwrap()
                .sessions
                .iter()
                .any(|row| row.id == session && !row.working)
            {
                let current = goal(control, session, GoalAction::Get).await.unwrap();
                if current.status == GoalStatus::BudgetLimited {
                    return current;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
}

#[tokio::test]
async fn goal_reads_and_mutations_match_local_and_remote() {
    // Host tasks can outlive a failed assertion, so their directory lives until exit.
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| tempfile::TempDir::with_prefix("aj-task-lifetime-").unwrap());
    let dir = Box::leak(Box::new(tempfile::TempDir::new_in(root.path()).unwrap()));
    let provider = scripted(
        vec![finalized_text_message_with_usage("done", 100); 6],
        0,
        Duration::ZERO,
    );
    let host = SessionHost::new(host_setup(
        dir,
        snapshot(provider),
        HostHandles::new(dir),
        None,
    ))
    .unwrap();
    let server = RemoteServer::bind_with(
        host.clone(),
        addr("127.0.0.1:0"),
        IdentityGate::local(),
        Duration::from_millis(50),
    )
    .await
    .unwrap();
    for control in [
        Control::local(host.clone()),
        Control::remote(RemoteClient::new(&server.url()).unwrap()),
    ] {
        let session = host.create().await.unwrap();
        assert!(goal(&control, &session, GoalAction::Get).await.is_none());
        let created = goal(
            &control,
            &session,
            GoalAction::Create {
                objective: "finish the fixture".into(),
                token_budget: Some(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(created.objective, "finish the fixture");
        assert_eq!(created.token_budget, Some(1));
        let stopped = limited(&control, &session).await;
        assert_eq!(stopped.tokens_used, 100);
        assert!(
            control
                .command(
                    &session,
                    Command::Goal(
                        GoalAction::Create {
                            objective: "replace".into(),
                            token_budget: Some(1)
                        }
                        .into()
                    )
                )
                .await
                .is_err()
        );
        let edited = goal(
            &control,
            &session,
            GoalAction::Edit {
                objective: "edited objective".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(edited.id, created.id);
        assert_eq!(edited.objective, "edited objective");
        assert_eq!(
            goal(&control, &session, GoalAction::Pause)
                .await
                .unwrap()
                .status,
            GoalStatus::BudgetLimited
        );
        assert_eq!(
            goal(&control, &session, GoalAction::Block)
                .await
                .unwrap()
                .status,
            GoalStatus::BudgetLimited
        );
        assert!(
            control
                .command(&session, Command::Goal(GoalAction::Resume.into()))
                .await
                .is_err(),
            "a spent budget must be increased before resuming"
        );
        for budget in [Some(200), None, Some(101)] {
            let adjusted = goal(
                &control,
                &session,
                GoalRequest::for_goal(
                    &created.id,
                    GoalAction::SetBudget {
                        token_budget: budget,
                    },
                ),
            )
            .await
            .unwrap();
            assert_eq!(adjusted.id, created.id);
            assert_eq!(adjusted.token_budget, budget);
            assert_eq!(adjusted.tokens_used, 100);
            assert_eq!(
                adjusted.status,
                GoalStatus::BudgetLimited,
                "saving a budget does not resume pursuit"
            );
        }
        assert!(
            control
                .command(
                    &session,
                    Command::Goal(GoalRequest::for_goal(
                        &created.id,
                        GoalAction::SetBudget {
                            token_budget: Some(0)
                        }
                    ))
                )
                .await
                .is_err()
        );
        assert_eq!(
            goal(&control, &session, GoalAction::Get)
                .await
                .unwrap()
                .token_budget,
            Some(101)
        );
        goal(&control, &session, GoalAction::Resume).await;
        assert_eq!(limited(&control, &session).await.tokens_used, 200);

        let replace = GoalAction::Replace {
            objective: "replacement".into(),
            token_budget: Some(1),
        };
        assert!(
            control
                .command(&session, Command::Goal(replace.clone().into()))
                .await
                .is_err(),
            "replacement requires an explicitly identified goal"
        );
        assert!(
            control
                .command(
                    &session,
                    Command::Goal(GoalRequest::for_goal(
                        &created.id,
                        GoalAction::Replace {
                            objective: " ".into(),
                            token_budget: None
                        }
                    ))
                )
                .await
                .is_err()
        );
        assert_eq!(
            goal(&control, &session, GoalAction::Get).await.unwrap().id,
            created.id,
            "invalid replacement must not clear the current goal"
        );
        let replaced = goal(
            &control,
            &session,
            GoalRequest::for_goal(&created.id, replace),
        )
        .await
        .unwrap();
        assert_ne!(replaced.id, created.id);
        assert_eq!(replaced.tokens_used, 0);
        let settled = limited(&control, &session).await;
        assert_eq!(settled.tokens_used, 100, "replacement resets usage");
        for stale in [
            GoalAction::Edit {
                objective: "stale editor".into(),
            },
            GoalAction::SetBudget { token_budget: None },
            GoalAction::Replace {
                objective: "stale replacement".into(),
                token_budget: None,
            },
            GoalAction::Resume,
            GoalAction::Pause,
            GoalAction::Clear,
        ] {
            let error = control
                .command(
                    &session,
                    Command::Goal(GoalRequest::for_goal(&created.id, stale)),
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("goal changed"), "{error}");
            assert_eq!(
                goal(&control, &session, GoalAction::Get).await,
                Some(settled.clone())
            );
        }
        assert_eq!(
            goal(&control, &session, GoalAction::Complete)
                .await
                .unwrap()
                .status,
            GoalStatus::Complete
        );
        assert!(goal(&control, &session, GoalAction::Clear).await.is_none());
        assert!(goal(&control, &session, GoalAction::Get).await.is_none());
    }
    host.shutdown().await;
    server.shutdown().await;
}
