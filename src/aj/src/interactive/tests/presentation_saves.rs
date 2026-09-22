use super::*;

#[tokio::test]
async fn thinking_shortcut_persists_and_settings_reads_the_saved_default() {
    let Some(home) = isolated_test_home() else {
        return;
    };
    let dir = TempDir::new().unwrap();
    let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
    let user_file = home.join(".aj/config.toml");
    assert!(!world.chat.borrow().show_thinking_block);
    assert!(!user_file.exists());
    let observed = Rc::clone(&shell);
    let chat = Rc::clone(&world.chat);
    let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
        for value in [true, false] {
            writer.write_all(b"\x1bt").unwrap();
            assert!(
                poll_for(|| {
                    let (saved, diagnostics) = Config::load();
                    (user_file.exists()
                        && diagnostics.is_empty()
                        && saved.show_thinking_block == value
                        && chat.borrow().show_thinking_block == value)
                        .then_some(())
                })
                .await
                .is_some(),
                "Alt+T must update the display and persist {value}"
            );
            writer.write_all(b"\x0fsettings\r").unwrap();
            assert!(
                poll_for(|| {
                    observed
                        .borrow()
                        .settings_ui
                        .borrow()
                        .as_ref()
                        .and_then(|ui| ui.value_of("show_thinking_block"))
                        .filter(|saved| saved == &value.to_string())
                })
                .await
                .is_some(),
                "the overlay must agree with the shortcut"
            );
            writer.write_all(b"\x1b").unwrap();
            assert!(
                poll_for(|| observed
                    .borrow()
                    .settings_ui
                    .borrow()
                    .is_none()
                    .then_some(()))
                .await
                .is_some()
            );
            writer.write_all(b"\x1b").unwrap();
            assert!(
                poll_for(|| (!observed.borrow().overlays.borrow().is_open()).then_some(()))
                    .await
                    .is_some()
            );
        }
    })
    .await;
    exit.unwrap();
    world.host().shutdown().await;
}

#[tokio::test]
async fn rapid_thinking_shortcuts_keep_input_live_and_finish_saving_on_quit() {
    let Some(home) = isolated_test_home() else {
        return;
    };
    let dir = TempDir::new().unwrap();
    let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
    let user_file = home.join(".aj/config.toml");
    let writes = Arc::clone(&world.config_layers.lock().unwrap().writes);
    let chat = Rc::clone(&world.chat);
    let observed = Rc::clone(&shell);
    assert!(!chat.borrow().show_thinking_block);

    let (exit, (release_tx, holder)) =
        drive_until(&mut world, &shell, move |mut writer| async move {
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let _guard = writes.lock().unwrap();
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(SETTLE_DEADLINE).is_ok()
            });
            ready_rx.recv_timeout(SETTLE_DEADLINE).unwrap();
            for value in [true, false] {
                writer.write_all(b"\x1bt").unwrap();
                assert!(
                    poll_for(|| (chat.borrow().show_thinking_block == value).then_some(()))
                        .await
                        .is_some(),
                    "visibility must change without waiting for disk"
                );
            }
            writer.write_all(b"typed-while-saving").unwrap();
            let responsive = poll_for(|| {
                (observed.borrow().view().editor.borrow().text() == "typed-while-saving")
                    .then_some(())
            })
            .await
            .is_some();
            assert!(responsive);
            (release_tx, holder)
        })
        .await;
    assert!(matches!(exit.unwrap(), SessionExit::Quit));
    assert!(!user_file.exists(), "quit must precede persistence");
    let _ = release_tx.send(());
    assert!(holder.join().unwrap());
    // Teardown must finish writes without polling abandoned overlay reads.
    shell
        .borrow_mut()
        .fills
        .push(Box::pin(std::future::pending()));
    tokio::time::timeout(SETTLE_DEADLINE, finish_presentation_saves(&shell))
        .await
        .expect("presentation writes finish during shutdown");
    let (saved, diagnostics) = Config::load();
    assert!(user_file.exists() && diagnostics.is_empty());
    assert!(!saved.show_thinking_block, "the last keypress must win");
    world.host().shutdown().await;
}

#[tokio::test]
async fn presentation_save_keeps_ui_live_while_config_write_is_blocked() {
    let Some(home) = isolated_test_home() else {
        return;
    };
    let dir = TempDir::new().unwrap();
    let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
    let writes = Arc::clone(&world.config_layers.lock().unwrap().writes);
    let user_file = home.join(".aj/config.toml");
    assert!(!user_file.exists());
    assert!(!shell.borrow().show_frame_stats.get());
    assert!(
        !crate::test_support::rows(&shell.borrow_mut().draw(&full_draw_ctx()))
            .join("\n")
            .contains("frame stats")
    );

    let observed = Rc::clone(&shell);
    let (exit, ()) = drive_until(&mut world, &shell, move |mut writer| async move {
        writer.write_all(b"\x0fsettings\r").unwrap();
        assert!(
            poll_for(|| {
                observed
                    .borrow()
                    .settings_ui
                    .borrow()
                    .as_ref()
                    .and_then(|ui| ui.value_of("show_frame_stats"))
                    .filter(|value| value == "false")
            })
            .await
            .is_some()
        );
        writer.write_all(b"show_frame_stats").unwrap();
        assert!(
            poll_for(|| {
                top_overlay_rows(&observed)
                    .join("\n")
                    .contains("show_frame_stats")
                    .then_some(())
            })
            .await
            .is_some()
        );

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = writes.lock().unwrap();
            ready_tx.send(()).unwrap();
            // drive_until polls input and observations on one task. An inline
            // write blocks both, so only this thread can release that regression.
            // Dropping the sender on assertion failure also releases the lock.
            release_rx.recv_timeout(SETTLE_DEADLINE).is_ok()
        });
        ready_rx.recv_timeout(SETTLE_DEADLINE).unwrap();
        writer.write_all(b"\r").unwrap();
        assert!(
            poll_for(|| observed.borrow().show_frame_stats.get().then_some(()))
                .await
                .is_some(),
            "the settings edit must reach the live presentation effect"
        );

        writer.write_all(b"typed-while-saving").unwrap();
        let typed = settled(Duration::from_secs(2), || {
            top_overlay_rows(&observed)
                .join("\n")
                .contains("typed-while-saving")
                .then_some(())
        })
        .await
        .is_some();
        writer.write_all(b"\x1b").unwrap();
        let closed = settled(Duration::from_secs(2), || {
            (observed.borrow().overlays.borrow().depth() == 1
                && observed.borrow().settings_ui.borrow().is_none())
            .then_some(())
        })
        .await
        .is_some();
        // Close the retained palette too, exposing the live rendering and
        // leaving Ctrl+O free to open a fresh settings editor after the save.
        writer.write_all(b"\x1b").unwrap();
        let rendered = settled(Duration::from_secs(2), || {
            if observed.borrow().overlays.borrow().depth() != 0 {
                return None;
            }
            crate::test_support::rows(&observed.borrow_mut().draw(&full_draw_ctx()))
                .join("\n")
                .contains("frame stats")
                .then_some(())
        })
        .await
        .is_some();
        let unsaved = !user_file.exists();

        // Release before asserting the observations, including failed ones.
        let _ = release_tx.send(());
        let released_explicitly = holder.join().unwrap();
        assert!(
            released_explicitly && typed && closed && rendered && unsaved,
            "UI must respond before persistence: explicit release={released_explicitly}, \
             typed={typed}, closed={closed}, rendered={rendered}, unsaved={unsaved}"
        );
        assert!(
            poll_for(|| {
                toast_lines(&observed)
                    .iter()
                    .any(|line| line.contains("Frame-stats overlay shown."))
                    .then_some(())
            })
            .await
            .is_some()
        );
        let saved: toml::Value =
            toml::from_str(&std::fs::read_to_string(&user_file).unwrap()).unwrap();
        assert_eq!(saved["show_frame_stats"].as_bool(), Some(true));

        writer.write_all(b"\x0fsettings\r").unwrap();
        assert!(
            poll_for(|| {
                observed
                    .borrow()
                    .settings_ui
                    .borrow()
                    .as_ref()
                    .and_then(|ui| ui.value_of("show_frame_stats"))
                    .filter(|value| value == "true")
            })
            .await
            .is_some(),
            "a fresh settings editor must read the saved default"
        );
    })
    .await;
    exit.unwrap();
    world.host().shutdown().await;
}
