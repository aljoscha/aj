use super::*;

async fn host(dir: &TempDir, name: &str) -> RemoteHost {
    let handles = crate::remote::tests::HostHandles::new(dir);
    host_with_handles(dir, name, handles).await
}

async fn host_with_handles(
    dir: &TempDir,
    name: &str,
    handles: crate::remote::tests::HostHandles,
) -> RemoteHost {
    let mut delegate = aj_app::test_support::finalized_text_message("delegate");
    delegate
        .content
        .push(aj_models::types::AssistantContent::ToolCall(
            aj_models::types::ToolCall {
                id: "selector-child".into(),
                name: "agent".into(),
                arguments: serde_json::json!({"task": "inspect the fixture"}),
            },
        ));
    delegate.stop_reason = aj_models::types::StopReason::ToolUse;
    let provider = crate::remote::tests::scripted(
        vec![
            delegate,
            aj_app::test_support::finalized_text_message("child done"),
            aj_app::test_support::finalized_text_message("parent done"),
        ],
        0,
        Duration::ZERO,
    );
    let mut run = crate::remote::tests::snapshot(provider);
    let model = ModelInfo {
        provider: "openai".into(),
        api: "openai-responses".into(),
        id: format!("{name}-first"),
        name: format!("{name} first"),
        base_url: "https://unused.invalid/v1".into(),
        reasoning: true,
        reasoning_options: vec![aj_models::registry::ReasoningOption::Effort {
            values: vec![
                aj_models::types::ThinkingLevel::Low,
                aj_models::types::ThinkingLevel::High,
            ],
        }],
        ..aj_app::test_support::scripted_model_info()
    };
    run.model_key = (model.provider.clone(), model.id.clone());
    run.model_info = Arc::new(model.clone());
    run.thinking = Some(ThinkingConfig::Low);
    let second = ModelInfo {
        id: format!("{name}-second"),
        name: format!("{name} second"),
        ..model.clone()
    };
    let mut setup = crate::remote::tests::host_setup(dir, run, handles, Some(name));
    setup.catalog = Arc::new(vec![model, second]);
    let host = aj_app::host::SessionHost::new(setup).unwrap();
    let server = crate::remote::RemoteServer::bind(
        host.clone(),
        "127.0.0.1:0".parse().unwrap(),
        crate::remote::IdentityGate::local(),
    )
    .await
    .unwrap();
    RemoteHost { host, server }
}

/// Open the way the command does, keeping the fetch instead of parking it, so
/// the test drives the fill itself.
fn open(world: &World, shell: &Rc<RefCell<Shell>>, action: CommandAction) -> SelectorFetch {
    let thinking = match action {
        CommandAction::OpenThinkingSelector => true,
        CommandAction::OpenModelSelector => false,
        other => panic!("not a selector: {other:?}"),
    };
    open_host_selector(world, shell, thinking)
}

async fn focus(world: &mut World, shell: &Rc<RefCell<Shell>>, app: &mut AsyncApp, session: &str) {
    apply_focus_request(app, shell, world, FocusRequest::Resume(session.into())).await;
    settle_pending_transition(app, shell, world).await;
    assert_eq!(world.session(), session);
}

#[tokio::test]
async fn selector_parity_host_catalog_and_confirmations_local_direct_gateway() {
    for mode in ["local", "direct", "gateway"] {
        let left_dir = TempDir::new().unwrap();
        let right_dir = TempDir::new().unwrap();
        let client_dir = TempDir::new().unwrap();
        let left = host(&left_dir, "selector-left").await;
        let right = host(&right_dir, "selector-right").await;
        let session = left.host.create().await.unwrap();
        let other_host = if mode == "gateway" { &right } else { &left };
        let other = other_host.host.create().await.unwrap();
        let opening = left.host.local_handles(&session).await.unwrap();
        let elsewhere = other_host.host.local_handles(&other).await.unwrap();
        let other_before = elsewhere.run_config.lock().unwrap().model_key.clone();
        let gateway = RemoteGateway::over(&[&left, &right]).await;
        gateway.until_sessions(2).await;
        let (url, initial, other) = if mode == "gateway" {
            (
                gateway.url(),
                format!("{}:{session}", left.host.hello().host_id),
                format!("{}:{other}", right.host.hello().host_id),
            )
        } else {
            (left.url(), session.clone(), other)
        };
        let (mut world, shell) = connect_world_and_shell_at(&client_dir, &url, &[&initial]).await;
        if mode == "local" {
            world.control = Control::local(left.host.clone());
        }
        let client_catalog = Arc::clone(&world.catalog);
        assert!(!client_catalog.is_empty());
        assert!(
            client_catalog
                .iter()
                .all(|info| !info.id.starts_with("selector-"))
        );
        let (mut app, mut writer, root) = app_over(&shell).await;
        let mut watch = inert_theme_watch();

        run_prompt(&mut world, "create a child").await;
        assert_eq!(reattach(&mut world, &shell).await.unwrap(), CatchUp::Caught);
        let sub = first_sub(&world.chat.borrow());
        world.chat.borrow_mut().set_active_view(AgentId::Sub(sub));
        let mut sub_edits = Vec::new();
        for (action, query) in [
            (CommandAction::OpenModelSelector, "selector-left-second"),
            (CommandAction::OpenThinkingSelector, "high"),
        ] {
            let fetch = open(&world, &shell, action);
            assert_eq!(fetch.target, AgentId::Sub(sub));
            fill_host_selector(fetch).await;
            focus_overlay(&mut app, &root);
            type_text(&mut app, &mut writer, query).await;
            press(&mut app, &mut writer, b"\r").await;
            sub_edits.extend(shell.borrow().take_activity());
        }
        assert_eq!(sub_edits.len(), 2);
        focus(&mut world, &shell, &mut app, &other).await;
        apply_selector_activity(&mut world, &shell, &mut watch, sub_edits).await;
        {
            let overrides = opening.sub_overrides.lock().unwrap();
            let staged = overrides
                .get(&sub)
                .expect("opening host's sub was targeted");
            assert_eq!(staged.thinking, Some(Some(ThinkingConfig::High)));
            assert_eq!(staged.bundle.as_ref().unwrap().1.id, "selector-left-second");
        }
        assert!(elsewhere.sub_overrides.lock().unwrap().is_empty());
        assert_eq!(
            opening.run_config.lock().unwrap().model_key.1,
            "selector-left-first"
        );
        assert_eq!(
            opening.run_config.lock().unwrap().thinking,
            Some(ThinkingConfig::Low)
        );
        focus(&mut world, &shell, &mut app, &initial).await;
        world.chat.borrow_mut().set_active_view(AgentId::Main);

        for (action, query) in [
            (CommandAction::OpenModelSelector, "selector-left-second"),
            (CommandAction::OpenThinkingSelector, "high"),
        ] {
            focus(&mut world, &shell, &mut app, &initial).await;
            let fetch = open(&world, &shell, action);
            let select = fetch.select.upgrade().unwrap();
            assert_eq!(select.borrow().visible_labels(), ["Loading host models…"]);
            focus_overlay(&mut app, &root);
            press(&mut app, &mut writer, b"\r").await;
            assert!(
                shell.borrow().take_activity().is_empty(),
                "loading is inert"
            );
            fill_host_selector(fetch).await;
            let labels = select.borrow().visible_labels();
            if action == CommandAction::OpenModelSelector {
                assert_eq!(
                    labels,
                    ["selector-left first (current)", "selector-left second"]
                );
            } else {
                assert_eq!(labels, ["low (current)", "high"]);
            }
            type_text(&mut app, &mut writer, query).await;
            press(&mut app, &mut writer, b"\r").await;
            let edits = shell.borrow().take_activity();
            assert_eq!(edits.len(), 1);
            focus(&mut world, &shell, &mut app, &other).await;
            apply_selector_activity(&mut world, &shell, &mut watch, edits).await;
            assert_eq!(
                opening.run_config.lock().unwrap().model_key.1,
                "selector-left-second",
                "{mode}: model confirmation belongs to the opening session"
            );
            assert_eq!(elsewhere.run_config.lock().unwrap().model_key, other_before);
            assert_eq!(
                elsewhere.run_config.lock().unwrap().thinking,
                Some(ThinkingConfig::Low)
            );
        }
        assert_eq!(
            opening.run_config.lock().unwrap().thinking,
            Some(ThinkingConfig::High)
        );
        assert!(
            Arc::ptr_eq(&client_catalog, &world.catalog),
            "no global catalog proxy"
        );
        focus(&mut world, &shell, &mut app, &initial).await;
        for disposition in ["stage", "rearm", "switch"] {
            let anchor = Rc::clone(&shell.borrow().view().branch_anchor);
            arm_branch(&anchor, "opening-branch".into());
            let fetch = open(&world, &shell, CommandAction::OpenModelSelector);
            fill_host_selector(fetch).await;
            focus_overlay(&mut app, &root);
            type_text(&mut app, &mut writer, "selector-left-first").await;
            press(&mut app, &mut writer, b"\r").await;
            let edits = shell.borrow().take_activity();
            assert_eq!(edits.len(), 1);
            if disposition == "rearm" {
                arm_branch(&anchor, "different-branch".into());
            }
            if disposition == "switch" {
                focus(&mut world, &shell, &mut app, &other).await;
            }
            apply_selector_activity(&mut world, &shell, &mut watch, edits).await;
            let staged = anchor
                .borrow()
                .as_ref()
                .and_then(|draft| draft.changes.settings.model.clone());
            if disposition != "stage" {
                assert!(
                    staged.is_none(),
                    "a different draft must not receive this choice"
                );
            } else {
                assert_eq!(staged.unwrap().name, "selector-left-first");
            }
            assert_eq!(
                opening.run_config.lock().unwrap().model_key.1,
                "selector-left-second"
            );
            assert_eq!(elsewhere.run_config.lock().unwrap().model_key, other_before);
            focus(&mut world, &shell, &mut app, &initial).await;
        }
        gateway.shutdown().await;
        left.shutdown().await;
        right.shutdown().await;
    }
}

#[tokio::test]
async fn selector_parity_unsupported_and_disconnected_have_no_fallback() {
    let host_dir = TempDir::new().unwrap();
    let client_dir = TempDir::new().unwrap();
    let remote = host(&host_dir, "selector-errors").await;
    let (mut world, shell) = connect_world_and_shell(&client_dir, &remote, &[]).await;
    let (mut app, mut writer, root) = app_over(&shell).await;
    for (url, expected) in [
        (format!("{}/unsupported", remote.url()), "does not serve"),
        (dead_url().await, "Could not use the host"),
    ] {
        world.control = Control::remote(crate::remote::RemoteClient::new(&url).unwrap());
        for action in [
            CommandAction::OpenModelSelector,
            CommandAction::OpenThinkingSelector,
        ] {
            let fetch = open(&world, &shell, action);
            let select = fetch.select.upgrade().unwrap();
            fill_host_selector(fetch).await;
            assert!(select.borrow().visible_labels()[0].contains(expected));
            focus_overlay(&mut app, &root);
            press(&mut app, &mut writer, b"\r").await;
            assert!(shell.borrow().take_activity().is_empty());
            press(&mut app, &mut writer, b"\x1b").await;
            assert!(!shell.borrow().overlays.borrow().is_open());
        }
    }
    remote.shutdown().await;
}

#[tokio::test]
async fn selector_parity_uncatalogued_runtime_keeps_thinking_edits() {
    for mode in ["local", "direct", "gateway"] {
        let host_dir = TempDir::new().unwrap();
        let client_dir = TempDir::new().unwrap();
        let remote = RemoteHost::start(&host_dir, "streaming-text").await;
        let session = remote.host.create().await.unwrap();
        let gateway = RemoteGateway::over(&[&remote]).await;
        gateway.until_sessions(1).await;
        let (url, selected) = if mode == "gateway" {
            (
                gateway.url(),
                format!("{}:{session}", remote.host.hello().host_id),
            )
        } else {
            (remote.url(), session.clone())
        };
        let (mut world, shell) = connect_world_and_shell_at(&client_dir, &url, &[&selected]).await;
        if mode == "local" {
            world.control = Control::local(remote.host.clone());
        }
        let model = viewed_model(&world, AgentId::Main);
        let catalog = world.control.models(&selected).await.unwrap();
        assert!(
            catalog
                .iter()
                .all(|info| (info.provider.clone(), info.id.clone()) != model)
        );
        let handles = remote.host.local_handles(&session).await.unwrap();
        assert_ne!(
            handles.run_config.lock().unwrap().thinking,
            Some(ThinkingConfig::High)
        );
        let (mut app, mut writer, root) = app_over(&shell).await;
        let fetch = open(&world, &shell, CommandAction::OpenThinkingSelector);
        fill_host_selector(fetch).await;
        focus_overlay(&mut app, &root);
        type_text(&mut app, &mut writer, "high").await;
        press(&mut app, &mut writer, b"\r").await;
        let edits = shell.borrow().take_activity();
        assert_eq!(
            edits.len(),
            1,
            "{mode}: an injected runtime model remains editable"
        );
        apply_selector_activity(&mut world, &shell, &mut inert_theme_watch(), edits).await;
        assert_eq!(
            handles.run_config.lock().unwrap().thinking,
            Some(ThinkingConfig::High)
        );
        gateway.shutdown().await;
        remote.shutdown().await;
    }
}

/// A slow host answer for a selector the user already closed must not land
/// in the selector opened afterwards for another host.
#[tokio::test]
async fn selector_parity_cancel_and_reopen_isolates_delayed_host_fill() {
    let left_dir = TempDir::new().unwrap();
    let right_dir = TempDir::new().unwrap();
    let client_dir = TempDir::new().unwrap();
    let left = host(&left_dir, "selector-delayed").await;
    let right = host(&right_dir, "selector-ready").await;
    let session = left.host.create().await.unwrap();
    let other = right.host.create().await.unwrap();
    let gateway = RemoteGateway::over(&[&left, &right]).await;
    gateway.until_sessions(2).await;
    let initial = format!("{}:{session}", left.host.hello().host_id);
    let other = format!("{}:{other}", right.host.hello().host_id);
    let (mut world, shell) =
        connect_world_and_shell_at(&client_dir, &gateway.url(), &[&initial]).await;
    let (mut app, mut writer, root) = app_over(&shell).await;
    // The first read goes to a peer that accepts the connection and holds it
    // until released.
    let stalled = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stalled_url = format!("http://{}", stalled.local_addr().unwrap());
    let (release, released) = oneshot::channel::<()>();
    let hold = tokio::spawn(async move {
        let (socket, _) = stalled.accept().await.unwrap();
        let _ = released.await;
        drop(socket);
    });
    // Opening is synchronous: nothing here waits on the host.
    let mut fetch = open(&world, &shell, CommandAction::OpenModelSelector);
    fetch.owner.control = Control::remote(crate::remote::RemoteClient::new(&stalled_url).unwrap());
    let closed = std::rc::Weak::clone(&fetch.select);
    let pending = fill_host_selector(fetch);
    tokio::pin!(pending);
    tokio::select! {
        biased;
        _ = &mut pending => panic!("the host read must wait for its answer"),
        _ = tokio::time::sleep(Duration::from_millis(50)) => {},
    }
    focus_overlay(&mut app, &root);
    assert!(
        top_overlay_rows(&shell)
            .join("\n")
            .contains("Loading host models")
    );
    press(&mut app, &mut writer, b"\x1b").await;
    app.render(&root).unwrap();
    assert!(!shell.borrow().overlays.borrow().is_open());
    focus(&mut world, &shell, &mut app, &other).await;
    let fresh = open(&world, &shell, CommandAction::OpenModelSelector);
    let select = fresh.select.upgrade().unwrap();
    fill_host_selector(fresh).await;
    let labels = select.borrow().visible_labels();
    assert_eq!(
        labels,
        ["selector-ready first (current)", "selector-ready second"]
    );
    release.send(()).unwrap();
    hold.await.unwrap();
    pending.await;
    assert!(
        closed.upgrade().is_none(),
        "closed list is not retained by the read"
    );
    assert_eq!(
        select.borrow().visible_labels(),
        labels,
        "late reply cannot fill the new list"
    );
    assert!(shell.borrow().take_activity().is_empty());
    gateway.shutdown().await;
    left.shutdown().await;
    right.shutdown().await;
}

#[tokio::test]
async fn model_selector_does_not_require_the_config_editor_endpoint() {
    use axum::{Json, Router, routing::get};

    let host_dir = TempDir::new().unwrap();
    let client_dir = TempDir::new().unwrap();
    let remote = host(&host_dir, "catalog-only").await;
    let (mut world, shell) = connect_world_and_shell(&client_dir, &remote, &[]).await;
    let catalog = remote.host.models(world.session()).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/v1/sessions/{id}/models",
        get(move || async move { Json(catalog) }),
    );
    let peer = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    world.control = Control::remote(crate::remote::RemoteClient::new(&url).unwrap());
    assert!(world.control.config(world.session()).await.is_err());
    let fetch = open(&world, &shell, CommandAction::OpenModelSelector);
    let select = fetch.select.upgrade().unwrap();
    fill_host_selector(fetch).await;
    assert_eq!(
        select.borrow().visible_labels(),
        ["catalog-only first (current)", "catalog-only second"]
    );
    peer.abort();
    let _ = peer.await;
    remote.shutdown().await;
}
