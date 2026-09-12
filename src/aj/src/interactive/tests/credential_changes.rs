use super::*;

#[tokio::test]
async fn credential_writes_and_logout_refresh_keep_the_drive_responsive() {
    for (logout, stalled_read) in [(false, false), (true, false), (true, true)] {
        let dir = TempDir::new().unwrap();
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let local_host = world.host().clone();
        let posts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let waiting = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Arc::new(tokio::sync::Notify::new());
        let router = axum::Router::new().fallback({
            let posts = Arc::clone(&posts);
            let waiting = Arc::clone(&waiting);
            let release = Arc::clone(&release);
            move |request: axum::extract::Request| {
                let posts = Arc::clone(&posts);
                let waiting = Arc::clone(&waiting);
                let release = Arc::clone(&release);
                async move {
                    use axum::response::IntoResponse;
                    if request.method() == axum::http::Method::POST {
                        posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if stalled_read {
                            return axum::Json(CredentialOutcome::Applied).into_response();
                        }
                    }
                    waiting.store(true, std::sync::atomic::Ordering::SeqCst);
                    release.notified().await;
                    // A gateway can return this after the host committed a write
                    // but lost its response. It does not prove a refusal.
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(serde_json::json!({
                            "code": "host_unreachable", "message": "Host unavailable"
                        })),
                    )
                        .into_response()
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        world.control = Control::remote(
            crate::remote::RemoteClient::new(&format!("http://{}", listener.local_addr().unwrap()))
                .unwrap(),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let request = if logout {
            AuthPickerAction::LogoutBare {
                provider_id: "anthropic".into(),
            }
        } else {
            AuthPickerAction::ApplyAccount(AccountAction::SetDefault {
                provider_id: "anthropic".into(),
                account_label: "work".into(),
            })
        };
        *shell.borrow().auth_request.borrow_mut() = Some(
            AuthPickerTarget {
                session: world.session().into(),
                host: "opening-host".into(),
            }
            .request(request),
        );
        let observed = Rc::clone(&shell);
        let chat = Rc::clone(&world.chat);
        let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
            writer.write_all(b"x").unwrap();
            assert!(
                poll_for(|| waiting
                    .load(std::sync::atomic::Ordering::SeqCst)
                    .then_some(()))
                .await
                .is_some()
            );
            writer.write_all(b"while-saving").unwrap();
            let typed = settled(Duration::from_secs(2), || {
                (observed.borrow().view().editor.borrow().text() == "xwhile-saving").then_some(())
            })
            .await
            .is_some();
            release.notify_one();
            assert!(typed, "typing must not wait for credential I/O");
            assert!(
                poll_for(|| notices_of(&chat.borrow())
                    .iter()
                    .any(|notice| {
                        notice.contains("opening-host")
                            && if stalled_read {
                                notice.contains("Logged out")
                                    && notice.contains("Could not read credentials")
                            } else {
                                notice.contains("Could not confirm the change")
                                    && notice.contains("Reopen auth status")
                            }
                    })
                    .then_some(()))
                .await
                .is_some()
            );
            assert_eq!(
                posts.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "no automatic retry"
            );
            assert!(
                !notices_of(&chat.borrow())
                    .iter()
                    .any(|notice| notice.contains("refused"))
            );
        })
        .await;
        exit.unwrap();
        local_host.shutdown().await;
        server.abort();
        let _ = server.await;
    }
}
