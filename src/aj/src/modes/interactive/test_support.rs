//! Shared helpers for the session-lifecycle unit tests in
//! `session.rs` and `interactive.rs`.
//!
//! Lives as a `#[cfg(test)]` child of `modes::interactive` so it can
//! construct [`RunConfigSnapshot`] (whose fields are private to that
//! module) while staying out of release builds.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aj_conf::Config;
use aj_models::registry::ModelInfo;
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::types::{
    AssistantContent, AssistantMessage, StopReason, StreamOptions, TextContent,
};
use aj_session::ConversationPersistence;
use aj_tui::terminal::Terminal;
use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::config::theme::{Theme, ThemeHandle};
use crate::modes::interactive::RunConfigSnapshot;
use crate::modes::interactive::render_settings::RenderSettings;
use crate::modes::interactive::session::{SessionEntry, SessionSpec, SessionWorld};

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

/// [`ModelInfo`] consistent with the identity [`ScriptedProvider`]
/// stamps on every emitted partial, so the agent sees a coherent
/// provider identity in tests.
pub(crate) fn scripted_model_info() -> ModelInfo {
    ModelInfo {
        id: "scripted".to_string(),
        name: "scripted".to_string(),
        api: "scripted".to_string(),
        provider: "scripted".to_string(),
        base_url: "scripted://internal".to_string(),
        reasoning: false,
        supports_adaptive_thinking: false,
        input: vec![aj_models::registry::InputModality::Text],
        cost: aj_models::registry::ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
    }
}

/// Finalized text-only assistant reply for scripting one-turn
/// conversations (no tool calls, `EndTurn`).
pub(crate) fn finalized_text_message(text: &str) -> AssistantMessage {
    AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        api: "scripted".to_string(),
        provider: "scripted".to_string(),
        model: "scripted".to_string(),
        response_id: Some("test-msg".to_string()),
        usage: Default::default(),
        stop_reason: StopReason::Stop,
        error: None,
        timestamp: 0,
    }
}

/// Run-config snapshot over a [`ScriptedProvider`] replaying
/// `messages`. `ExhaustedBehavior::Panic` makes any unscripted
/// extra inference fail loudly.
pub(crate) fn scripted_run_config(
    messages: Vec<AssistantMessage>,
) -> Arc<StdMutex<RunConfigSnapshot>> {
    Arc::new(StdMutex::new(RunConfigSnapshot {
        provider: Arc::new(
            ScriptedProvider::from_messages(messages, 0, Duration::ZERO)
                .on_exhausted(ExhaustedBehavior::Panic),
        ),
        model_info: Arc::new(scripted_model_info()),
        stream_options: StreamOptions::default(),
        thinking: None,
        model_key: ("scripted".to_string(), "scripted".to_string()),
    }))
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
    )
}

/// Drive one prompt turn against the world's agent so the
/// persistence listener writes real entries into the log.
pub(crate) async fn drive_turn(world: &SessionWorld, prompt: &str) {
    world
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
    world.session_id.clone()
}
