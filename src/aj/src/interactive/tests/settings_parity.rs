use super::*;
use std::sync::Mutex;

fn skill(root: &Path, name: &str) {
    let directory = root.join(".agents/skills").join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: use {name} for testing\n---\nInstructions\n"),
    )
    .unwrap();
}

#[tokio::test]
async fn settings_parity_local_and_http_adapters() {
    let Some(home) = isolated_test_home() else {
        return;
    };
    skill(&home, "adapter-skill");
    let dir = TempDir::new().unwrap();
    let handles = crate::remote::tests::HostHandles::new(&dir);
    let project_path = dir.path().join(".aj/config.toml");
    {
        let mut layers = handles.layers.lock().unwrap();
        layers.user.model_url = Some("https://user:private@example.org/v1?token=credential".into());
        layers.project_path = Some(project_path.clone());
        *handles.config.lock().unwrap() = layers.effective();
    }
    let host = crate::remote::tests::scripted_host(
        &dir,
        crate::remote::tests::scripted(Vec::new(), 0, Duration::ZERO),
        handles.clone(),
        None,
    );
    let session = host.create().await.unwrap();
    let live = host.local_handles(&session).await.unwrap();
    assert!(
        live.env
            .skills
            .iter()
            .any(|skill| skill.name == "adapter-skill" && skill.enabled)
    );
    let server = crate::remote::RemoteServer::bind(
        host.clone(),
        "127.0.0.1:0".parse().unwrap(),
        crate::remote::IdentityGate::local(),
    )
    .await
    .unwrap();
    let controls = [
        Control::local(host.clone()),
        Control::remote(crate::remote::RemoteClient::new(&server.url()).unwrap()),
    ];
    for control in controls {
        let values = control.config(&session).await.unwrap();
        assert_eq!(
            values.user["model_url"],
            "https://user:private@example.org/v1?token=credential"
        );
        assert_eq!(values.effective["model_url"], values.user["model_url"]);
        for endpoint in [
            "https://peer:password@example.org/v2?token=editable#fragment",
            "https://user:private@example.org/v1?token=credential",
        ] {
            control
                .edit_config(
                    &session,
                    aj_wire::ConfigEdit {
                        key: "model_url".into(),
                        value: Some(endpoint.into()),
                        persist: PersistAction::User,
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                control.config(&session).await.unwrap().user["model_url"],
                endpoint
            );
            assert_eq!(Config::load().0.model_url.as_deref(), Some(endpoint));
        }
        for endpoint in ["https://oracle.example.org/v1", ""] {
            control
                .edit_config(
                    &session,
                    aj_wire::ConfigEdit {
                        key: "oracle_model_url".into(),
                        value: Some(endpoint.into()),
                        persist: PersistAction::User,
                    },
                )
                .await
                .unwrap();
            let expected = (!endpoint.is_empty()).then_some(endpoint);
            let saved = Config::load().0;
            assert_eq!(saved.oracle_model_url.as_deref(), expected);
            assert_eq!(
                saved.model_url.as_deref(),
                Some(values.user["model_url"].as_str())
            );
            let view = control.config(&session).await.unwrap();
            assert_eq!(view.user["oracle_model_url"], expected.unwrap_or("<unset>"));
            assert_eq!(
                view.effective["oracle_model_url"],
                view.user["oracle_model_url"]
            );
            assert_eq!(view.user["model_url"], values.user["model_url"]);
        }
        assert!(!values.user.contains_key("theme"));
        assert_eq!(
            values.user.len(),
            Config::OPTIONS
                .iter()
                .filter(|option| !aj_app::settings::is_presentation(option.name))
                .count()
        );

        for persist in [
            PersistAction::User,
            PersistAction::ProjectSet,
            PersistAction::ProjectClear,
        ] {
            control
                .edit_config(
                    &session,
                    aj_wire::ConfigEdit {
                        key: "auto_compact".into(),
                        value: (persist != PersistAction::ProjectClear).then(|| "false".into()),
                        persist,
                    },
                )
                .await
                .unwrap();
            let view = control.config(&session).await.unwrap();
            assert_eq!(view.effective["auto_compact"], "false");
            assert_eq!(
                view.project_keys.contains(&"auto_compact".into()),
                persist == PersistAction::ProjectSet
            );
            assert!(!Config::load().0.auto_compact);
        }
        assert!(
            !std::fs::read_to_string(&project_path)
                .unwrap()
                .contains("auto_compact")
        );
        let before = serde_json::to_value(control.config(&session).await.unwrap()).unwrap();
        for (key, value, persist) in [
            ("unknown_setting", Some("true"), PersistAction::User),
            ("auto_compact", Some("invalid"), PersistAction::User),
            ("compact_threshold", Some("2"), PersistAction::User),
            ("theme", Some("dark"), PersistAction::User),
            ("auto_compact", Some("true"), PersistAction::ProjectClear),
        ] {
            assert!(
                control
                    .edit_config(
                        &session,
                        aj_wire::ConfigEdit {
                            key: key.into(),
                            value: value.map(String::from),
                            persist
                        }
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                serde_json::to_value(control.config(&session).await.unwrap()).unwrap(),
                before
            );
        }

        control
            .toggle_skill(
                &session,
                aj_wire::SkillToggle {
                    name: "adapter-skill".into(),
                    disable: true,
                },
            )
            .await
            .unwrap();
        assert!(
            !control
                .skills(&session)
                .await
                .unwrap()
                .into_iter()
                .find(|skill| skill.name == "adapter-skill")
                .unwrap()
                .enabled
        );
        assert_eq!(
            control.config(&session).await.unwrap().user["disabled_skills"],
            "adapter-skill"
        );
        assert!(
            live.env
                .skills
                .iter()
                .find(|skill| skill.name == "adapter-skill")
                .unwrap()
                .enabled,
            "running prompt is frozen"
        );
        let next = host.create().await.unwrap();
        assert!(
            !host
                .local_handles(&next)
                .await
                .unwrap()
                .env
                .skills
                .iter()
                .find(|skill| skill.name == "adapter-skill")
                .unwrap()
                .enabled
        );
        control
            .toggle_skill(
                &session,
                aj_wire::SkillToggle {
                    name: "adapter-skill".into(),
                    disable: false,
                },
            )
            .await
            .unwrap();
        assert!(Config::load().0.disabled_skills.is_empty());

        for (persist, level) in [
            (PersistAction::User, ThinkingConfig::High),
            (PersistAction::ProjectSet, ThinkingConfig::Low),
            (PersistAction::ProjectClear, ThinkingConfig::High),
        ] {
            control
                .command(
                    &session,
                    Command::Settings(SettingsChange {
                        agent: AgentId::Main,
                        persist,
                        axis: SettingsAxis::Thinking(Some(level.clone())),
                    }),
                )
                .await
                .unwrap();
            assert_eq!(live.run_config.lock().unwrap().main.thinking, Some(level));
            assert_eq!(
                control
                    .config(&session)
                    .await
                    .unwrap()
                    .project_keys
                    .contains(&"thinking".into()),
                persist == PersistAction::ProjectSet
            );
        }
        // Axis keys write through the config edit too, in the editor's
        // vocabulary, without touching the running session.
        for (key, value, expected) in [
            ("thinking", "low", "low"),
            ("model", "openai/gpt-catalog", "openai"),
            ("verbosity", aj_app::settings::UNSET_VALUE, "<unset>"),
        ] {
            control
                .edit_config(
                    &session,
                    aj_wire::ConfigEdit {
                        key: key.into(),
                        value: Some(value.into()),
                        persist: PersistAction::User,
                    },
                )
                .await
                .unwrap();
            let shown = if key == "model" { "model_api" } else { key };
            assert_eq!(
                control.config(&session).await.unwrap().user[shown],
                expected
            );
        }
        assert_eq!(
            live.run_config.lock().unwrap().main.thinking,
            Some(ThinkingConfig::High),
            "a config edit cannot change the running session"
        );
        assert_eq!(
            Config::load().0.thinking,
            Some(aj_conf::ConfigThinkingLevel::Low)
        );
        assert_eq!(Config::load().0.model_name.as_deref(), Some("gpt-catalog"));
        assert!(
            control
                .edit_config(
                    &session,
                    aj_wire::ConfigEdit {
                        key: "model".into(),
                        value: Some("nowhere/none".into()),
                        persist: PersistAction::User,
                    },
                )
                .await
                .is_err()
        );
    }
    let before = std::fs::read(home.join(".aj/config.toml")).unwrap();
    for (route, body) in [
        (
            "config",
            serde_json::json!({"key":"auto_compact","value":"true","persist":"user","surprise":true}),
        ),
        (
            "skills",
            serde_json::json!({"name":"adapter-skill","disable":true,"surprise":true}),
        ),
        (
            "settings",
            serde_json::json!({"thinking":"high","persist":"user","surprise":true}),
        ),
    ] {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/sessions/{session}/{route}", server.url()))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read(home.join(".aj/config.toml")).unwrap(), before);
    }
    host.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn settings_parity_stalled_saves_allow_navigation_and_close_without_replaying() {
    for (command, key, endpoint, branch) in [
        ("settings", "auto_compact", "/config", false),
        ("settings", "thinking", "/settings", false),
        ("skills", "save-skill", "/skills", false),
        ("settings", "thinking", "/config", true),
    ] {
        let dir = TempDir::new().unwrap();
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let local_host = world.host().clone();
        let defaults = world.control.config(world.session()).await.unwrap();
        if branch {
            arm_branch(&shell.borrow().view().branch_anchor, "m1".into());
            assert!(branch_settings(&shell).unwrap().thinking.is_none());
        }
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let router = axum::Router::new().fallback({
            let writes = Arc::clone(&writes);
            let release = Arc::clone(&release);
            move |request: axum::extract::Request| {
                let writes = Arc::clone(&writes);
                let release = Arc::clone(&release);
                let defaults = defaults.clone();
                async move {
                    use axum::response::IntoResponse;
                    if request.method() == axum::http::Method::GET {
                        return if request.uri().path().ends_with("/skills") {
                            axum::Json(vec![aj_wire::SkillInfo {
                                name: "save-skill".into(), description: "A host skill".into(),
                                path: "/host/skills/save-skill/SKILL.md".into(),
                                enabled: true, disable_model_invocation: false,
                            }]).into_response()
                        } else {
                            axum::Json(defaults).into_response()
                        };
                    }
                    assert!(request.uri().path().ends_with(endpoint), "wrong save endpoint: {}", request.uri());
                    writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    release.notified().await;
                    (axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(serde_json::json!({"code":"host_unreachable","message":"Host unavailable"})))
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
        let observed = Rc::clone(&shell);
        let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
            writer.write_all(format!("\x0f{command}\r").as_bytes()).unwrap();
            assert!(poll_for(|| {
                (observed.borrow().overlays.borrow().depth() == 2
                    && if command == "settings" {
                        observed.borrow().settings_ui.borrow().as_ref().and_then(|ui| ui.value_of(key)).is_some()
                    } else { top_overlay_rows(&observed).join("\n").contains(key) }).then_some(())
            }).await.is_some());
            writer.write_all(format!("{key}\r").as_bytes()).unwrap();
            // Thinking uses a submenu. Confirm its selected level as well.
            if key == "thinking" {
                assert!(
                    poll_for(|| (observed.borrow().overlays.borrow().depth() == 3).then_some(()))
                        .await
                        .is_some()
                );
                writer.write_all(b"\r").unwrap();
            }
            assert!(
                poll_for(|| (writes.load(std::sync::atomic::Ordering::SeqCst) == 1).then_some(()))
                    .await
                    .is_some()
            );
            if branch {
                assert!(branch_settings(&observed).unwrap().thinking.is_some(),
                    "the branch choice must not wait for saving its default");
            }
            writer.write_all(b"\rtyped-while-saving").unwrap();
            let typed = settled(Duration::from_secs(2), || {
                top_overlay_rows(&observed)
                    .join("\n")
                    .contains("typed-while-saving")
                    .then_some(())
            })
            .await
            .is_some();
            let page = top_overlay_rows(&observed).join("\n");
            writer.write_all(b"\x1b").unwrap();
            let closed = settled(Duration::from_secs(2), || {
                (observed.borrow().overlays.borrow().depth() == 1).then_some(())
            })
            .await
            .is_some();
            // Release even on a regression, so teardown cannot wait for the request timeout.
            release.notify_one();
            assert!(
                typed && closed,
                "typing and close must not wait for a save: {key}, typed={typed}, closed={closed}, page={page}"
            );
            assert!(
                poll_for(|| toast_lines(&observed)
                    .iter()
                    .any(|s| s.contains("Host unavailable"))
                    .then_some(()))
                .await
                .is_some()
            );
            assert_eq!(
                writes.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "one explicit save, no replay"
            );
        })
        .await;
        exit.unwrap();
        local_host.shutdown().await;
        server.abort();
        let _ = server.await;
    }
}

// Each HTTP host has its own process HOME and cwd. Sharing those with the
// client would make a mistaken client-disk write satisfy host persistence tests.
struct SettingsProcess {
    root: TempDir,
    child: std::process::Child,
    url: String,
    session: String,
}

impl SettingsProcess {
    async fn start() -> Self {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("home/.aj")).unwrap();
        std::fs::create_dir_all(root.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(root.path().join("project/.aj")).unwrap();
        std::fs::write(
            root.path().join("home/.aj/config.toml"),
            "thinking = \"off\"\nauto_compact = false\ntheme = \"dark\"\nsidebar_cols = 31\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("project/.aj/config.toml"),
            "compact_keep_recent = 12345\n",
        )
        .unwrap();
        skill(&root.path().join("project"), "host-only-skill");
        let log = std::fs::File::create(root.path().join("child.log")).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "interactive::tests::settings_parity::settings_parity_host_process",
                "--nocapture",
            ])
            .env("AJ_SETTINGS_HOST_PROCESS", root.path())
            .env("HOME", root.path().join("home"))
            .current_dir(root.path().join("project"))
            .stdin(std::process::Stdio::piped())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut process = Self {
            root,
            child,
            url: String::new(),
            session: String::new(),
        };
        let ready = process.root.path().join("ready.json");
        crate::remote::tests::bounded("settings host readiness", async {
            loop {
                if let Ok(bytes) = std::fs::read(&ready)
                    && let Ok((url, session)) = serde_json::from_slice::<(String, String)>(&bytes)
                {
                    process.url = url;
                    process.session = session;
                    break;
                }
                if let Some(status) = process.child.try_wait().unwrap() {
                    panic!(
                        "settings child exited {status}: {}",
                        std::fs::read_to_string(process.root.path().join("child.log")).unwrap()
                    );
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        process
    }

    fn user_file(&self) -> String {
        std::fs::read_to_string(self.root.path().join("home/.aj/config.toml")).unwrap()
    }
}

impl Drop for SettingsProcess {
    fn drop(&mut self) {
        self.child.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn settings_parity_host_process() {
    let Some(root) = std::env::var_os("AJ_SETTINGS_HOST_PROCESS").map(PathBuf::from) else {
        return;
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let args = Args::parse_from(["aj", "--scripted", "streaming-text"]);
        let user = Config::load().0;
        let layers = ConfigLayers {
            writes: Default::default(),
            user,
            project: Config::load_project().0,
            project_path: Config::project_config_file_path(),
        };
        let auth = AuthStorage::new(root.join("home/.aj/auth.json"));
        let persistence = ConversationPersistence::new(root.join("sessions"));
        let ComposedHost { host, .. } =
            compose_host(&args, layers, &auth, &persistence, None).unwrap();
        let session = host.create().await.unwrap();
        let server = crate::remote::RemoteServer::bind(
            host.clone(),
            "127.0.0.1:0".parse().unwrap(),
            crate::remote::IdentityGate::local(),
        )
        .await
        .unwrap();
        std::fs::write(
            root.join("ready.json"),
            serde_json::to_vec(&(server.url(), session)).unwrap(),
        )
        .unwrap();
        tokio::task::spawn_blocking(|| {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::stdin(), &mut bytes).unwrap();
        })
        .await
        .unwrap();
        host.shutdown().await;
        server.shutdown().await;
    });
}

#[tokio::test]
async fn settings_parity_gateway_ui_uses_opening_host_and_client_presentation() {
    let Some(home) = isolated_test_home() else {
        return;
    };
    std::fs::create_dir_all(home.join(".aj")).unwrap();
    std::fs::write(
        home.join(".aj/config.toml"),
        "theme = \"light\"\nsidebar_cols = 47\n",
    )
    .unwrap();
    skill(&home, "client-only-skill");
    let left = SettingsProcess::start().await;
    let right = SettingsProcess::start().await;
    let state = TempDir::new().unwrap();
    let gateway = crate::gateway::Gateway::new(crate::gateway::GatewaySetup {
        state_dir: state.path().into(),
        static_hosts: Vec::new(),
        tuning: crate::gateway::Tuning::default(),
    })
    .unwrap();
    gateway.enroll(&left.url).await.unwrap();
    gateway.enroll(&right.url).await.unwrap();
    let server = crate::gateway::GatewayServer::bind(
        gateway.clone(),
        "127.0.0.1:0".parse().unwrap(),
        crate::remote::IdentityGate::local(),
    )
    .await
    .unwrap();
    let gateway = RemoteGateway {
        gateway,
        server,
        _state: state,
    };
    gateway.until_sessions(2).await;
    let client_dir = TempDir::new().unwrap();
    let left_id = crate::remote::RemoteClient::new(&left.url)
        .unwrap()
        .hello()
        .await
        .unwrap()
        .host_id;
    let left_session = format!("{left_id}:{}", left.session);
    let (mut world, shell) =
        connect_world_and_shell_at(&client_dir, &gateway.url(), &[&left_session]).await;
    world.config_layers.lock().unwrap().user = Config::load().0;
    let client_before = std::fs::read(home.join(".aj/config.toml")).unwrap();
    assert!(matches!(
        apply_command(&mut world, &shell, CommandAction::OpenSettings).await,
        ActionEffect::OpenedOverlay
    ));
    let value = |id| {
        shell
            .borrow()
            .settings_ui
            .borrow()
            .as_ref()
            .unwrap()
            .value_of(id)
            .unwrap()
    };
    assert_eq!(value("auto_compact"), "false", "host user defaults");
    assert_eq!(value("theme"), "light", "client presentation");
    assert_eq!(value("sidebar_cols"), "47");
    let (mut app, mut writer, root) = app_over(&shell).await;
    focus_overlay(&mut app, &root);
    let keys = b"auto_compact\r";
    writer.write_all(keys).unwrap();
    for _ in keys {
        let event = app.next_input().await.unwrap();
        app.handle_input(event);
    }
    let mut queued = shell.borrow().take_activity();
    assert_eq!(queued.len(), 1, "the real settings widget queued its edit");

    assert!(matches!(
        apply_command(&mut world, &shell, CommandAction::OpenProjectSettings).await,
        ActionEffect::OpenedOverlay
    ));
    assert_eq!(
        value("compact_keep_recent"),
        "12345",
        "host project override"
    );
    let owner = SettingsOwner::capture(&world, &shell, Arc::clone(&world.catalog));
    let mut watch = inert_theme_watch();
    apply_owned_setting_change(
        &world,
        &shell,
        &mut watch,
        &owner,
        PersistAction::ProjectClear,
        "compact_keep_recent",
        "20000",
    )
    .await;
    assert!(
        !std::fs::read_to_string(left.root.path().join("project/.aj/config.toml"))
            .unwrap()
            .contains("compact_keep_recent")
    );

    let before_thinking = world.client().settings().unwrap().thinking.clone();
    arm_branch(
        &shell.borrow().view().branch_anchor,
        "pending-message".into(),
    );
    let branch_owner = SettingsOwner::capture(&world, &shell, Arc::clone(&world.catalog));
    apply_owned_setting_change(
        &world,
        &shell,
        &mut watch,
        &branch_owner,
        PersistAction::User,
        "thinking",
        "high",
    )
    .await;
    assert_eq!(
        shell
            .borrow()
            .view()
            .branch_anchor
            .borrow()
            .as_ref()
            .unwrap()
            .changes
            .settings
            .thinking
            .as_deref(),
        Some("high")
    );
    assert!(left.user_file().contains("thinking = \"high\""));
    let probe = connect_world_at(&client_dir, &gateway.url(), &[&left_session]).await;
    assert_eq!(
        probe.client().settings().unwrap().thinking,
        before_thinking,
        "a new attachment observes the unchanged running session"
    );
    drop(probe);
    shell.borrow().view().branch_anchor.borrow_mut().take();

    let fetch = open_host_skills(&world, &shell);
    let list = Rc::clone(&fetch.list);
    fill_host_skills(fetch).await;
    assert_eq!(
        list.borrow().value_of("host-only-skill").as_deref(),
        Some("enabled")
    );
    assert!(list.borrow().value_of("client-only-skill").is_none());
    focus_overlay(&mut app, &root);
    let keys = b"host-only-skill\r";
    writer.write_all(keys).unwrap();
    for _ in keys {
        let event = app.next_input().await.unwrap();
        app.handle_input(event);
    }
    queued.extend(shell.borrow().take_activity());
    assert_eq!(
        queued.len(),
        2,
        "both widgets captured an opening operation"
    );

    let right_before = right.user_file();
    // Switch the actual focused session before applying the captured operation.
    let right_session = world
        .directory
        .rows()
        .iter()
        .find(|row| row.id != left_session)
        .unwrap()
        .id
        .clone();
    focus_session(
        &mut app,
        &shell,
        &mut world,
        right_session.clone(),
        PendingTransition::Switch {
            session: right_session.clone(),
            tag: None,
        },
    )
    .await;
    assert_eq!(world.session(), right_session);
    apply_selector_activity(&mut world, &shell, &mut watch, queued).await;
    assert!(
        left.user_file()
            .contains("disabled_skills = [\"host-only-skill\"]")
    );
    assert!(!left.user_file().contains("auto_compact = false"));
    assert_eq!(
        right.user_file(),
        right_before,
        "focus cannot retarget host writes"
    );
    assert_eq!(
        std::fs::read(home.join(".aj/config.toml")).unwrap(),
        client_before,
        "host writes never touch client disk"
    );
    let notice = apply_owned_setting_change(
        &world,
        &shell,
        &mut watch,
        &owner,
        PersistAction::ProjectSet,
        "sidebar_cols",
        "52",
    )
    .await
    .unwrap();
    assert!(notice.contains("on this client"), "{notice}");
    apply_owned_setting_change(
        &world,
        &shell,
        &mut watch,
        &owner,
        PersistAction::User,
        "sidebar_cols",
        "52",
    )
    .await;
    assert_eq!(Config::load().0.sidebar_cols, 52);
    assert!(left.user_file().contains("sidebar_cols = 31"));
    assert_eq!(right.user_file(), right_before);
    let client_project = client_dir.path().join(".aj/config.toml");
    world.config_layers.lock().unwrap().project_path = Some(client_project.clone());
    for (persist, value) in [
        (PersistAction::ProjectSet, "44"),
        (PersistAction::ProjectClear, "52"),
    ] {
        apply_owned_setting_change(
            &world,
            &shell,
            &mut watch,
            &owner,
            persist,
            "sidebar_cols",
            value,
        )
        .await;
        assert_eq!(world.config.lock().unwrap().sidebar_cols.to_string(), value);
    }
    assert!(
        !std::fs::read_to_string(client_project)
            .unwrap()
            .contains("sidebar_cols")
    );
    assert_eq!(right.user_file(), right_before);
    assert!(left.user_file().contains("sidebar_cols = 31"));
    drop(world);
    gateway.shutdown().await;
}

#[tokio::test]
async fn settings_parity_unsupported_host_never_falls_back_to_client_disk() {
    let Some(home) = isolated_test_home() else {
        return;
    };
    std::fs::create_dir_all(home.join(".aj")).unwrap();
    std::fs::write(home.join(".aj/config.toml"), "auto_compact = false\n").unwrap();
    let dir = TempDir::new().unwrap();
    let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&requests);
    let router = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let observed = Arc::clone(&observed);
        async move {
            observed
                .lock()
                .unwrap()
                .push((request.method().clone(), request.uri().path().to_string()));
            (
                axum::http::StatusCode::NOT_FOUND,
                axum::Json(
                    serde_json::json!({"code":"unknown_endpoint","message":"not supported"}),
                ),
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    world.control = Control::remote(crate::remote::RemoteClient::new(&url).unwrap());
    let before = std::fs::read(home.join(".aj/config.toml")).unwrap();
    let before_values = aj_app::settings::schema_values(&world.config_layers.lock().unwrap().user);
    for command in [
        CommandAction::OpenSettings,
        CommandAction::OpenProjectSettings,
    ] {
        assert!(matches!(
            apply_command(&mut world, &shell, command).await,
            ActionEffect::OpenedOverlay
        ));
        assert!(
            top_overlay_rows(&shell)
                .join("\n")
                .contains("does not serve the settings editor")
        );
        let rows = top_overlay_rows(&shell).join("\n");
        assert!(
            rows.contains("theme") && !rows.contains("auto_compact"),
            "{rows}"
        );
        assert!(
            !rows.contains("theme *"),
            "client rows are never host-marked: {rows}"
        );
    }
    let fetch = open_host_skills(&world, &shell);
    let owner = fetch.owner.clone();
    fill_host_skills(fetch).await;
    assert!(
        top_overlay_rows(&shell)
            .join("\n")
            .contains("does not serve the skills editor")
    );
    let notice = apply_owned_setting_change(
        &world,
        &shell,
        &mut inert_theme_watch(),
        &owner,
        PersistAction::User,
        "disabled_tools",
        "bash",
    )
    .await
    .unwrap();
    assert!(notice.contains("does not serve"));
    assert!(
        apply_skill_toggle(&owner, "some-skill", true)
            .await
            .contains("does not serve")
    );
    assert_eq!(std::fs::read(home.join(".aj/config.toml")).unwrap(), before);
    assert_eq!(
        aj_app::settings::schema_values(&world.config_layers.lock().unwrap().user),
        before_values
    );
    assert_eq!(
        requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _)| *method == axum::http::Method::POST)
            .count(),
        2,
        "only the explicitly requested writes were attempted"
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn settings_parity_stalled_open_keeps_drive_responsive_and_ignores_late_reply() {
    let dir = TempDir::new().unwrap();
    let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
    let defaults = world.control.config(world.session()).await.unwrap();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let replied = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let router = axum::Router::new().route(
        "/v1/sessions/{id}/config",
        axum::routing::get({
            let requests = Arc::clone(&requests);
            let release = Arc::clone(&release);
            let replied = Arc::clone(&replied);
            move || {
                requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let release = Arc::clone(&release);
                let replied = Arc::clone(&replied);
                let mut defaults = defaults.clone();
                async move {
                    release.notified().await;
                    defaults.user.insert("auto_compact".into(), "false".into());
                    replied.store(true, std::sync::atomic::Ordering::SeqCst);
                    axum::Json(defaults)
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    world.control = Control::remote(crate::remote::RemoteClient::new(&url).unwrap());
    let observed = Rc::clone(&shell);
    let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
        writer.write_all(b"\x0fsettings\r").unwrap();
        assert!(
            poll_for(|| (requests.load(std::sync::atomic::Ordering::SeqCst) == 1).then_some(()))
                .await
                .is_some()
        );
        assert!(
            top_overlay_rows(&observed)
                .join("\n")
                .contains("Loading host settings")
        );
        writer.write_all(b"typed-while-loading").unwrap();
        let typed = settled(Duration::from_secs(2), || {
            top_overlay_rows(&observed)
                .join("\n")
                .contains("typed-while-loading")
                .then_some(())
        })
        .await
        .is_some();
        writer.write_all(b"\x1b").unwrap();
        let closed = settled(Duration::from_secs(2), || {
            observed
                .borrow()
                .settings_ui
                .borrow()
                .is_none()
                .then_some(())
        })
        .await
        .is_some();
        // Always release on failure, so the regression fails rather than waiting
        // for the transport timeout to let the test exit.
        if !typed || !closed {
            release.notify_one();
        }
        assert!(typed && closed, "typing and close must precede the reply");
        // Escape reveals the retained palette. Close that before opening a
        // fresh palette, otherwise Ctrl+O toggles the retained one off.
        if observed.borrow().overlays.borrow().depth() > 0 {
            writer.write_all(b"\x1b").unwrap();
            assert!(
                poll_for(|| (observed.borrow().overlays.borrow().depth() == 0).then_some(()))
                    .await
                    .is_some()
            );
        }
        writer.write_all(b"\x0fhelp\r").unwrap();
        assert!(
            poll_for(|| (observed.borrow().overlays.borrow().depth() == 2).then_some(()))
                .await
                .is_some()
        );
        let help_rows = top_overlay_rows(&observed);
        assert!(observed.borrow().settings_ui.borrow().is_none());
        release.notify_one();
        assert!(
            poll_for(|| replied
                .load(std::sync::atomic::Ordering::SeqCst)
                .then_some(()))
            .await
            .is_some()
        );
        // Keep driving while the delivered reply and its redraw are processed.
        // Neither the unrelated help window nor the closed editor may change.
        assert!(
            settled(Duration::from_secs(1), || {
                (observed.borrow().settings_ui.borrow().is_some()
                    || top_overlay_rows(&observed) != help_rows)
                    .then_some(())
            })
            .await
            .is_none()
        );
        writer.write_all(b"\x1b").unwrap();
        assert!(
            poll_for(|| (observed.borrow().overlays.borrow().depth() == 1).then_some(()))
                .await
                .is_some()
        );
    })
    .await;
    exit.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn settings_parity_failed_saves_leave_defaults_coherent_and_retry_writes_disk() {
    let Some(home) = isolated_test_home() else {
        return;
    };
    skill(&home, "save-skill");
    let dir = TempDir::new().unwrap();
    let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
    let host = world.host().clone();
    let layers = Arc::clone(&world.config_layers);
    let effective = Arc::clone(&world.config);
    let project = dir.path().join("project/config.toml");
    layers.lock().unwrap().project_path = Some(project.clone());
    let server = crate::remote::RemoteServer::bind(
        host.clone(),
        "127.0.0.1:0".parse().unwrap(),
        crate::remote::IdentityGate::local(),
    )
    .await
    .unwrap();
    let user = home.join(".aj/config.toml");
    let mut watch = inert_theme_watch();
    for remote in [false, true] {
        world.control = if remote {
            Control::remote(crate::remote::RemoteClient::new(&server.url()).unwrap())
        } else {
            Control::local(host.clone())
        };
        for (target, path, command) in [
            (PersistAction::User, &user, CommandAction::OpenSettings),
            (
                PersistAction::ProjectSet,
                &project,
                CommandAction::OpenProjectSettings,
            ),
        ] {
            apply_command(&mut world, &shell, command).await;
            let ui = Rc::clone(&shell.borrow().settings_ui);
            let list = Rc::clone(&ui.borrow().as_ref().unwrap().list);
            let original = list.borrow().value_of("auto_compact").unwrap();
            let value = if original == "true" { "false" } else { "true" };
            let before = world.control.config(world.session()).await.unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let backup = std::fs::read(path).ok();
            if backup.is_some() {
                std::fs::remove_file(path).unwrap();
            }
            std::fs::create_dir(path).unwrap();
            let (mut app, mut writer, root) = app_over(&shell).await;
            focus_overlay(&mut app, &root);
            let keys = b"auto_compact\r";
            writer.write_all(keys).unwrap();
            for _ in keys {
                let event = app.next_input().await.unwrap();
                app.handle_input(event);
            }
            assert_eq!(
                list.borrow().value_of("auto_compact").as_deref(),
                Some(value),
                "the widget made an optimistic edit"
            );
            let activity = shell.borrow().take_activity();
            assert_eq!(activity.len(), 1);
            let SelectorActivity::SettingChange { owner, .. } = &activity[0] else {
                panic!("settings edit");
            };
            let owner = owner.clone();
            apply_selector_activity(&mut world, &shell, &mut watch, activity).await;
            assert!(
                toast_lines(&shell)
                    .iter()
                    .any(|note| note.contains("couldn't save"))
            );
            assert_eq!(list.borrow().value_of("auto_compact").unwrap(), value);
            assert!(
                top_overlay_rows(&shell)
                    .join("\n")
                    .contains("Reopen this window")
            );
            let after = world.control.config(world.session()).await.unwrap();
            assert_eq!(after.user, before.user);
            assert_eq!(after.effective, before.effective);
            assert_eq!(
                aj_app::settings::host_values(&effective.lock().unwrap()),
                before.effective
            );
            let presentation_before = list.borrow().value_of("show_token_usage").unwrap();
            let show = presentation_before != "true";
            let note = apply_owned_setting_change(
                &world,
                &shell,
                &mut watch,
                &owner,
                target,
                "show_token_usage",
                if show { "true" } else { "false" },
            )
            .await
            .unwrap();
            assert!(note.contains("couldn't save"), "{note}");
            assert_eq!(
                world.chat.borrow().show_token_usage,
                show,
                "the live presentation effect is independent of defaults"
            );
            assert_eq!(
                list.borrow().value_of("show_token_usage").unwrap(),
                presentation_before
            );
            assert_eq!(
                effective.lock().unwrap().show_token_usage.to_string(),
                presentation_before
            );
            // Axis application is independent of persistence: the live session
            // takes the edit, the failed save is reported, and the row keeps
            // the chosen value until the editor is reopened.
            let thinking_before = list.borrow().value_of("thinking").unwrap();
            let thinking = if thinking_before == "high" {
                "low"
            } else {
                "high"
            };
            list.borrow().set_value("thinking", thinking);
            apply_owned_setting_change(
                &world, &shell, &mut watch, &owner, target, "thinking", thinking,
            )
            .await;
            assert_eq!(list.borrow().value_of("thinking").unwrap(), thinking);
            assert!(
                top_overlay_rows(&shell)
                    .join("\n")
                    .contains("Reopen this window"),
                "a session-applied change must not confirm a failed default save"
            );
            assert_eq!(
                host.local_handles(world.session())
                    .await
                    .unwrap()
                    .run_config
                    .lock()
                    .unwrap()
                    .main
                    .thinking
                    .as_ref()
                    .map(|thinking| aj_models::thinking_config_name(Some(thinking))),
                Some(thinking)
            );
            let model_before = list.borrow().value_of(MODEL_SETTING_ID).unwrap();
            let live_before = host
                .local_handles(world.session())
                .await
                .unwrap()
                .run_config
                .lock()
                .unwrap()
                .main
                .model_key
                .clone();
            let model = owner
                .models
                .iter()
                .find(|model| {
                    format!("{}/{}", model.provider, model.id) != model_before
                        && (model.provider.clone(), model.id.clone()) != live_before
                        && aj_models::provider::provider_for(&model.api).is_some()
                        && aj_models::registry::validate_thinking_level(
                            model,
                            &aj_models::types::ThinkingLevel::High,
                        )
                        .is_ok()
                        && aj_models::registry::validate_thinking_level(
                            model,
                            &aj_models::types::ThinkingLevel::Low,
                        )
                        .is_ok()
                })
                .expect("a different servable model supporting both test thinking choices")
                .clone();
            let model_value = format!("{}/{}", model.provider, model.id);
            list.borrow().set_value(MODEL_SETTING_ID, &model_value);
            apply_owned_setting_change(
                &world,
                &shell,
                &mut watch,
                &owner,
                target,
                MODEL_SETTING_ID,
                &model_value,
            )
            .await;
            assert_eq!(
                list.borrow().value_of(MODEL_SETTING_ID).unwrap(),
                model_value
            );
            assert_eq!(
                host.local_handles(world.session())
                    .await
                    .unwrap()
                    .run_config
                    .lock()
                    .unwrap()
                    .main
                    .model_key,
                (model.provider.clone(), model.id.clone())
            );
            let defaults = world.control.config(world.session()).await.unwrap();
            assert_eq!(defaults.user, before.user);
            assert_eq!(defaults.effective, before.effective);
            assert_eq!(
                aj_app::settings::host_values(&effective.lock().unwrap()),
                before.effective
            );
            if target == PersistAction::User {
                let fetch = open_host_skills(&world, &shell);
                let skill_owner = fetch.owner.clone();
                let skill_list = Rc::clone(&fetch.list);
                fill_host_skills(fetch).await;
                assert_eq!(
                    skill_list.borrow().value_of("save-skill").as_deref(),
                    Some("enabled")
                );
                skill_list.borrow().set_value("save-skill", "disabled");
                assert!(
                    apply_skill_toggle(&skill_owner, "save-skill", true)
                        .await
                        .contains("couldn't save")
                );
                assert_eq!(
                    skill_list.borrow().value_of("save-skill").as_deref(),
                    Some("disabled")
                );
                let next = host.create().await.unwrap();
                assert!(
                    host.local_handles(&next)
                        .await
                        .unwrap()
                        .env
                        .skills
                        .iter()
                        .any(|skill| skill.name == "save-skill" && skill.enabled)
                );
                assert!(layers.lock().unwrap().user.disabled_skills.is_empty());
            }
            std::fs::remove_dir(path).unwrap();
            if let Some(bytes) = backup {
                std::fs::write(path, bytes).unwrap();
            }
            // Reopening reads the unchanged defaults. Retry the rejected values
            // after repairing the filesystem.
            apply_command(&mut world, &shell, command).await;
            let reopened = Rc::clone(&shell.borrow().settings_ui.borrow().as_ref().unwrap().list);
            assert_eq!(
                reopened.borrow().value_of("auto_compact").unwrap(),
                original
            );
            apply_owned_setting_change(
                &world,
                &shell,
                &mut watch,
                &owner,
                target,
                "auto_compact",
                value,
            )
            .await;
            apply_owned_setting_change(
                &world, &shell, &mut watch, &owner, target, "thinking", thinking,
            )
            .await;
            apply_owned_setting_change(
                &world,
                &shell,
                &mut watch,
                &owner,
                target,
                MODEL_SETTING_ID,
                &model_value,
            )
            .await;
            let disk = std::fs::read_to_string(path).unwrap();
            let parsed: toml::Value = toml::from_str(&disk).unwrap();
            assert_eq!(
                parsed
                    .get("auto_compact")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or_else(|| Config::default().auto_compact)
                    .to_string(),
                value
            );
            assert_eq!(parsed["thinking"].as_str(), Some(thinking));
            assert_eq!(parsed["model_api"].as_str(), Some(model.provider.as_str()));
            assert_eq!(parsed["model_name"].as_str(), Some(model.id.as_str()));
            assert_eq!(
                list.borrow().value_of(MODEL_SETTING_ID).as_deref(),
                Some(model_value.as_str())
            );
            let after = world.control.config(world.session()).await.unwrap();
            assert_eq!(
                if target == PersistAction::User {
                    &after.user
                } else {
                    &after.effective
                }["auto_compact"],
                value
            );
            assert_eq!(
                aj_app::settings::host_values(&effective.lock().unwrap()),
                after.effective
            );
            if target == PersistAction::User {
                assert!(
                    apply_skill_toggle(&owner, "save-skill", true)
                        .await
                        .contains("Takes effect")
                );
                assert!(
                    std::fs::read_to_string(path)
                        .unwrap()
                        .contains("save-skill")
                );
                let next = host.create().await.unwrap();
                assert!(
                    host.local_handles(&next)
                        .await
                        .unwrap()
                        .env
                        .skills
                        .iter()
                        .any(|skill| skill.name == "save-skill" && !skill.enabled)
                );
                apply_skill_toggle(&owner, "save-skill", false).await;
            }
        }
    }
    host.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn settings_parity_client_project_without_host_project_is_editable() {
    let Some(_home) = isolated_test_home() else {
        return;
    };
    let dir = TempDir::new().unwrap();
    let remote = RemoteHost::start(&dir, "streaming-text").await;
    let (mut world, shell) = connect_world_and_shell(&dir, &remote, &["--new"]).await;
    assert!(
        !world
            .control
            .config(world.session())
            .await
            .unwrap()
            .has_project
    );
    let project = dir.path().join(".aj/config.toml");
    world.config_layers.lock().unwrap().project_path = Some(project.clone());
    world.config_layers.lock().unwrap().user.sidebar_cols = 49;
    apply_command(&mut world, &shell, CommandAction::OpenProjectSettings).await;
    let list = Rc::clone(&shell.borrow().settings_ui.borrow().as_ref().unwrap().list);
    assert_eq!(
        list.borrow().value_of("sidebar_cols").as_deref(),
        Some("49")
    );
    let mut owner = SettingsOwner::capture(&world, &shell, Arc::clone(&world.catalog));
    owner.bind_rows(&list);
    let mut watch = inert_theme_watch();
    for (persist, value) in [
        (PersistAction::ProjectSet, "53"),
        (PersistAction::ProjectClear, "49"),
    ] {
        // The widget shows the chosen value before the edit is applied.
        list.borrow().set_value("sidebar_cols", value);
        apply_owned_setting_change(
            &world,
            &shell,
            &mut watch,
            &owner,
            persist,
            "sidebar_cols",
            value,
        )
        .await;
        assert_eq!(
            list.borrow().value_of("sidebar_cols").as_deref(),
            Some(value)
        );
        assert_eq!(world.config.lock().unwrap().sidebar_cols.to_string(), value);
        let disk = std::fs::read_to_string(&project).unwrap();
        assert_eq!(
            disk.contains("sidebar_cols"),
            persist == PersistAction::ProjectSet
        );
    }
    let note = apply_owned_setting_change(
        &world,
        &shell,
        &mut watch,
        &owner,
        PersistAction::ProjectSet,
        "auto_compact",
        "false",
    )
    .await
    .unwrap();
    assert!(note.contains("repository on the host"), "{note}");
    assert!(
        !std::fs::read_to_string(project)
            .unwrap()
            .contains("auto_compact")
    );
    remote.shutdown().await;
}

/// With a branch draft open, saving an axis from the settings editor writes
/// the host's config at once while the session effect waits on the draft,
/// locally and over a connection alike.
#[tokio::test]
async fn settings_parity_branch_draft_saves_config_without_touching_the_session() {
    let Some(_home) = isolated_test_home() else {
        return;
    };
    let dir = TempDir::new().unwrap();
    let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
    let host = world.host().clone();
    let server = crate::remote::RemoteServer::bind(
        host.clone(),
        "127.0.0.1:0".parse().unwrap(),
        crate::remote::IdentityGate::local(),
    )
    .await
    .unwrap();
    let mut watch = inert_theme_watch();
    for (remote, level) in [(false, "low"), (true, "high")] {
        world.control = if remote {
            Control::remote(crate::remote::RemoteClient::new(&server.url()).unwrap())
        } else {
            Control::local(host.clone())
        };
        let live_before = host
            .local_handles(world.session())
            .await
            .unwrap()
            .run_config
            .lock()
            .unwrap()
            .main
            .thinking
            .clone();
        arm_branch(&shell.borrow().view().branch_anchor, "m1".to_string());
        apply_command(&mut world, &shell, CommandAction::OpenSettings).await;
        let list = Rc::clone(&shell.borrow().settings_ui.borrow().as_ref().unwrap().list);
        let mut owner = SettingsOwner::capture(&world, &shell, Arc::clone(&world.catalog));
        owner.bind_rows(&list);
        list.borrow().set_value("thinking", level);
        assert!(
            apply_owned_setting_change(
                &world,
                &shell,
                &mut watch,
                &owner,
                PersistAction::User,
                "thinking",
                level,
            )
            .await
            .is_none()
        );
        let painted = painted_rows(&shell, 120, 40).join("\n");
        assert!(painted.contains("Default saved"), "{painted}");
        assert_eq!(
            Config::load().0.thinking.map(|level| level.to_string()),
            Some(level.to_string()),
            "the host config took the default"
        );
        assert_eq!(
            host.local_handles(world.session())
                .await
                .unwrap()
                .run_config
                .lock()
                .unwrap()
                .main
                .thinking,
            live_before,
            "the running session waits for the branch"
        );
        assert_eq!(
            shell
                .borrow()
                .view()
                .branch_anchor
                .borrow()
                .as_ref()
                .unwrap()
                .settings()
                .thinking
                .as_deref(),
            Some(level),
            "the draft carries the choice"
        );
        // A refused default save does not discard the independent branch choice
        // or require reopening the editor before correcting that choice.
        assert!(world.config_layers.lock().unwrap().project_path.is_none());
        shell.borrow().overlays.borrow_mut().close_all();
        apply_command(&mut world, &shell, CommandAction::OpenProjectSettings).await;
        let list = Rc::clone(&shell.borrow().settings_ui.borrow().as_ref().unwrap().list);
        let mut owner = SettingsOwner::capture(&world, &shell, Arc::clone(&world.catalog));
        owner.bind_rows(&list);
        let live = host.local_handles(world.session()).await.unwrap();
        let (main_before, oracle_before) = {
            let run = live.run_config.lock().unwrap();
            (run.main.settings(), run.oracle.settings())
        };
        let defaults_before = Config::load().0;
        for key in ["thinking", "oracle_thinking"] {
            let before = list.borrow().value_of(key);
            for choice in ["minimal", "max"] {
                list.borrow().set_value(key, choice);
                apply_selector_activity(
                    &mut world,
                    &shell,
                    &mut watch,
                    vec![SelectorActivity::SettingChange {
                        owner: owner.clone(),
                        target: ConfigTarget::Project,
                        id: key.into(),
                        value: choice.into(),
                    }],
                )
                .await;
                assert_eq!(list.borrow().value_of(key), before);
                let draft = branch_settings(&shell).unwrap();
                let staged = if key == "thinking" {
                    draft.thinking
                } else {
                    draft.oracle_thinking
                };
                assert_eq!(staged.as_deref(), Some(choice));
                let page = top_overlay_rows(&shell).join("\n");
                assert!(!page.contains("Reopen this window"), "{page}");
                let run = live.run_config.lock().unwrap();
                assert_eq!(run.main.settings(), main_before);
                assert_eq!(run.oracle.settings(), oracle_before);
            }
        }
        let defaults = Config::load().0;
        assert_eq!(defaults.thinking, defaults_before.thinking);
        assert_eq!(defaults.oracle_thinking, defaults_before.oracle_thinking);
        *shell.borrow().view().branch_anchor.borrow_mut() = None;
        shell.borrow().overlays.borrow_mut().close_all();
    }
    host.shutdown().await;
    server.shutdown().await;
}

#[tokio::test]
async fn settings_parity_client_edits_do_not_wait_for_host_rows() {
    let Some(_home) = isolated_test_home() else {
        return;
    };
    let dir = TempDir::new().unwrap();
    let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
    let before = world.config.lock().unwrap().show_token_usage;
    let mut defaults = world.control.config(world.session()).await.unwrap();
    defaults.user.insert("auto_compact".into(), "false".into());
    defaults.user.insert("oracle_thinking".into(), "low".into());
    let release = Arc::new(tokio::sync::Notify::new());
    let router = axum::Router::new().route(
        "/v1/sessions/{id}/config",
        axum::routing::get({
            let release = Arc::clone(&release);
            move || {
                let release = Arc::clone(&release);
                let defaults = defaults.clone();
                async move {
                    release.notified().await;
                    axum::Json(defaults)
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    world.control = Control::remote(
        crate::remote::RemoteClient::new(&format!("http://{}", listener.local_addr().unwrap()))
            .unwrap(),
    );
    world.direct_host_label = Some("settings-host".into());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let observed = Rc::clone(&shell);
    let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
        writer.write_all(b"\x0fsettings\r").unwrap();
        assert!(
            poll_for(|| {
                if observed.borrow().settings_ui.borrow().is_none() {
                    return None;
                }
                let page = top_overlay_rows(&observed).join("\n");
                (page.contains("Loading host settings") && page.contains("show_token_usage"))
                    .then_some(())
            })
            .await
            .is_some()
        );
        assert!(
            observed
                .borrow()
                .settings_ui
                .borrow()
                .as_ref()
                .unwrap()
                .value_of("oracle_thinking")
                .is_none(),
            "Oracle choices must come from the host, not the client defaults"
        );
        writer.write_all(b"show_token_usage\r").unwrap();
        let edited = poll_for(|| (Config::load().0.show_token_usage != before).then_some(())).await;
        // Release even on a regression so teardown does not await a network timeout.
        release.notify_one();
        assert!(edited.is_some(), "client save waited for the host read");
        assert!(
            poll_for(|| observed
                .borrow()
                .settings_ui
                .borrow()
                .as_ref()
                .and_then(
                    |ui| (ui.value_of("auto_compact").as_deref() == Some("false")).then_some(())
                ))
            .await
            .is_some()
        );
        let page = top_overlay_rows(&observed).join("\n");
        assert!(
            page.contains("show_token_usage"),
            "host fill lost the filter: {page}"
        );
        assert!(!page.contains("show_token_usage *"), "{page}");
        assert!(
            page.contains("* Host-side settings on settings-host"),
            "{page}"
        );
        assert_eq!(
            observed
                .borrow()
                .settings_ui
                .borrow()
                .as_ref()
                .unwrap()
                .value_of("show_token_usage"),
            Some((!before).to_string())
        );
        writer.write_all(b"\x15auto_compact").unwrap();
        assert!(
            poll_for(|| top_overlay_rows(&observed)
                .join("\n")
                .contains("auto_compact *")
                .then_some(()))
            .await
            .is_some()
        );
        assert_eq!(
            observed
                .borrow()
                .settings_ui
                .borrow()
                .as_ref()
                .unwrap()
                .value_of("oracle_thinking")
                .as_deref(),
            Some("low")
        );
        writer.write_all(b"\x15oracle_thinking").unwrap();
        assert!(
            poll_for(|| top_overlay_rows(&observed)
                .join("\n")
                .contains("oracle_thinking *")
                .then_some(()))
            .await
            .is_some()
        );
        writer.write_all(b"\x1b").unwrap();
    })
    .await;
    exit.unwrap();
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn settings_parity_rejected_values_can_be_corrected_without_reopening() {
    let Some(_home) = isolated_test_home() else {
        return;
    };
    let dir = TempDir::new().unwrap();
    let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
    let host = world.host().clone();
    let server = crate::remote::RemoteServer::bind(
        host.clone(),
        "127.0.0.1:0".parse().unwrap(),
        crate::remote::IdentityGate::local(),
    )
    .await
    .unwrap();
    for (control, corrected) in [
        (Control::local(host.clone()), 0.6),
        (
            Control::remote(crate::remote::RemoteClient::new(&server.url()).unwrap()),
            0.7,
        ),
    ] {
        world.control = control;
        shell.borrow().toasts.borrow_mut().clear();
        let previous = Config::load().0.compact_threshold.to_string();
        let run_config = Arc::clone(&world.local.as_ref().unwrap().run_config);
        let observed = Rc::clone(&shell);
        let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
            writer.write_all(b"\x0fsettings\r").unwrap();
            assert!(
                poll_for(|| observed
                    .borrow()
                    .settings_ui
                    .borrow()
                    .as_ref()
                    .and_then(|ui| ui.value_of("compact_threshold")))
                .await
                .is_some()
            );
            let speed_before = observed
                .borrow()
                .settings_ui
                .borrow()
                .as_ref()
                .unwrap()
                .value_of("speed");
            writer.write_all(b"speed\r").unwrap();
            assert!(
                poll_for(|| toast_lines(&observed)
                    .iter()
                    .any(|line| line.contains("Failed to set speed"))
                    .then_some(()))
                .await
                .is_some()
            );
            assert_eq!(
                observed
                    .borrow()
                    .settings_ui
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .value_of("speed"),
                speed_before
            );
            assert_eq!(
                aj_models::speed_name(run_config.lock().unwrap().main.speed),
                "standard"
            );
            writer.write_all(b"\x15compact_threshold\r").unwrap();
            assert!(
                poll_for(|| (observed.borrow().overlays.borrow().depth() == 3).then_some(()))
                    .await
                    .is_some(),
                "submenu did not open, page: {:?}",
                top_overlay_rows(&observed)
            );
            writer.write_all(b"\x152\r").unwrap();
            assert!(
                poll_for(|| toast_lines(&observed)
                    .iter()
                    .any(|line| line.contains("compact_threshold"))
                    .then_some(()))
                .await
                .is_some()
            );
            assert_eq!(
                observed
                    .borrow()
                    .settings_ui
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .value_of("compact_threshold"),
                Some(previous)
            );
            let page = top_overlay_rows(&observed).join("\n");
            assert!(
                !page.contains("Reopen this window"),
                "a refused value must remain editable: {page}"
            );
            writer.write_all(b"\r").unwrap();
            assert!(
                poll_for(|| (observed.borrow().overlays.borrow().depth() == 3).then_some(()))
                    .await
                    .is_some(),
                "submenu did not open, page: {:?}",
                top_overlay_rows(&observed)
            );
            writer
                .write_all(format!("\x15{corrected}\r").as_bytes())
                .unwrap();
            assert!(
                poll_for(|| (Config::load().0.compact_threshold == corrected).then_some(()))
                    .await
                    .is_some()
            );
            writer.write_all(b"\x1b").unwrap();
            assert!(
                poll_for(|| (observed.borrow().overlays.borrow().depth() == 1).then_some(()))
                    .await
                    .is_some()
            );
            writer.write_all(b"\x1b").unwrap();
            assert!(
                poll_for(|| (observed.borrow().overlays.borrow().depth() == 0).then_some(()))
                    .await
                    .is_some()
            );
        })
        .await;
        exit.unwrap();
    }
    server.shutdown().await;
    host.shutdown().await;
}
