use super::*;

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
