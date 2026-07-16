//! Shared helpers for the session-lifecycle unit tests in
//! `session.rs` and `interactive.rs`.
//!
//! The TUI-agnostic scripted-provider and run-config builders live in
//! `aj_app::test_support` and are re-exported here so existing call
//! sites keep resolving. The SessionWorld / `Terminal`-bound helpers
//! (which touch aj-tui types) stay in this crate.

use std::sync::{Arc, Mutex as StdMutex};

use aj_conf::Config;
use aj_session::ConversationPersistence;
use aj_tui::terminal::Terminal;
use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::config::theme::{Theme, ThemeHandle};
use crate::modes::interactive::render_settings::RenderSettings;
use crate::modes::interactive::session::{SessionEntry, SessionSpec, SessionWorld};
use crate::session_setup::RunConfigSnapshot;

pub(crate) use aj_app::test_support::{
    finalized_text_message, scripted_model_info, scripted_run_config,
};

/// Headless [`Terminal`]: fixed 100×24, writes discarded.
/// Component output is read via `Component::render`, not the
/// terminal's write buffer, so a no-op sink is sufficient.
/// Deliberately duplicates the integration-test stub — unit
/// tests cannot import from `tests/`.
pub(crate) struct StubTerminal;

impl Terminal for StubTerminal {
    fn write(&mut self, _: &str) {}
    fn columns(&self) -> u16 {
        100
    }
    fn rows(&self) -> u16 {
        24
    }
    fn move_by(&mut self, _: i32) {}
    fn hide_cursor(&mut self) {}
    fn show_cursor(&mut self) {}
    fn clear_line(&mut self) {}
    fn clear_from_cursor(&mut self) {}
    fn clear_screen(&mut self) {}
    fn set_title(&mut self, _: &str) {}
    fn flush(&mut self) {}
}

/// [`SessionWorld::build`] with a default config, bundled theme,
/// and fixed render settings. The agent's env is read from the
/// host (cwd, git, context files); tests therefore never assert
/// on prompt *text*, only on persisted-vs-held equality.
pub(crate) fn build_test_world(
    persistence: &ConversationPersistence,
    run_config: &Arc<StdMutex<RunConfigSnapshot>>,
    spec: &SessionSpec,
) -> Result<SessionWorld> {
    SessionWorld::build(
        &Config::default(),
        run_config,
        &RenderSettings::new(false, false, true),
        &ThemeHandle::new(Theme::bundled_dark()),
        persistence,
        spec,
        None,
        Arc::new(Vec::new()),
    )
}

/// Drive one prompt turn against the world's agent so the
/// persistence listener writes real entries into the log.
pub(crate) async fn drive_turn(world: &SessionWorld, prompt: &str) {
    world
        .core
        .agent
        .lock()
        .await
        .prompt(prompt.to_string(), CancellationToken::new())
        .await
        .expect("scripted turn completes");
}

pub(crate) fn create_spec() -> SessionSpec {
    SessionSpec::Create {
        entry: SessionEntry::Startup,
    }
}

pub(crate) fn resume_spec(session_id: &str) -> SessionSpec {
    SessionSpec::Resume {
        session_id: session_id.to_string(),
        entry: SessionEntry::Switch,
        head: None,
    }
}

/// Build a `Create` world on `persistence`, drive one scripted
/// text turn, and return the session id. The world is dropped so
/// a later resume reads everything from disk.
pub(crate) async fn one_turn_session(
    persistence: &ConversationPersistence,
    prompt: &str,
    reply: &str,
) -> String {
    let run_config = scripted_run_config(vec![finalized_text_message(reply)]);
    let world = build_test_world(persistence, &run_config, &create_spec()).expect("create world");
    drive_turn(&world, prompt).await;
    world.core.session_id.clone()
}
