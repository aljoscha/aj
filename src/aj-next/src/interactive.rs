//! The interactive alt-screen shell, driven by `vxfw::AsyncApp`.
//!
//! The base layout from the alt-screen UX spec: a one-line header, a
//! flex-filling transcript, an editor, and a one-line footer, stacked
//! in a `FlexColumn`. A real agent session backs the shell: prompts
//! submitted from the editor spawn turns through the shared
//! `aj_app::turn` helpers, agent events fold into the [`ChatState`]
//! model, and the [`TranscriptView`] renders it with follow-tail.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use aj_agent::TurnError;
use aj_agent::events::{AgentEvent, AgentId};
use aj_app::chat::{ChatState, reduce};
use aj_app::cli::args::{Args, Command};
use aj_app::commands::load_model_catalog;
use aj_app::session::{SessionCore, SessionEntry, SessionSpec};
use aj_app::session_setup::{RunConfigSnapshot, build_initial_run_config};
use aj_app::shutdown::{format_resume_hint, format_usage_summary};
use aj_app::theme::Theme;
use aj_app::turn::{TurnStart, join_next_or_pending, spawn_turn, spawn_wake_turn, turn_policy};
use aj_conf::{Config, ConfigDiagnostic, ConfigSpeed, Severity};
use aj_models::auth::AuthStorage;
use aj_models::types::Speed;
use aj_session::{ConversationPersistence, ThreadFilter, replay};
use anyhow::{Context, Result, anyhow};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use vaxis::key::Modifiers;
use vaxis::tty::PosixTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    AsyncApp, DrawContext, Event, EventContext, FlexColumn, FlexItem, Options, Surface, Text,
    TextField, Widget, WidgetRef, draw_widget, to_widget_ref,
};

use crate::transcript::TranscriptView;

/// Everything the select loop mutates besides the `AsyncApp`: the
/// session core, the shared chat model, and the turn bookkeeping.
///
/// Kept separate from the [`Shell`] widget so the loop's arm helpers
/// are drivable headlessly in tests, without a terminal.
struct World {
    core: SessionCore,
    /// The chat model, shared with the [`TranscriptView`]. Only the
    /// loop mutates it (via [`reduce`] and the arm helpers). The view
    /// reads it at draw time. Never borrowed across an await.
    chat: Rc<RefCell<ChatState>>,
    config: Arc<StdMutex<Config>>,
    run_config: Arc<StdMutex<RunConfigSnapshot>>,
    /// In-flight turns keyed by the agent running them, plus the
    /// host's clone of each turn's cancel token. The token map's key
    /// set is exactly "agents this host is currently driving".
    turns: JoinSet<(AgentId, Result<(), TurnError>)>,
    turn_cancels: HashMap<AgentId, CancellationToken>,
}

/// Build the session world: run config, session core, and the chat
/// model seeded from the main agent and any resumed history.
///
/// Mirrors `aj`'s interactive assembly (`SessionCore::build` off the
/// shared run-config snapshot), then folds replayed history, config
/// diagnostics, and restore notices into the chat model so the first
/// frame shows them.
async fn build_world(
    args: &Args,
    config: Config,
    diagnostics: &[ConfigDiagnostic],
    auth: &AuthStorage,
    persistence: &ConversationPersistence,
) -> Result<World> {
    let speed = match args.speed.as_deref() {
        Some(s) => Some(s.parse::<ConfigSpeed>().map_err(anyhow::Error::msg)?),
        None => config.speed,
    }
    .map(|s| match s {
        ConfigSpeed::Standard => Speed::Standard,
        ConfigSpeed::Fast => Speed::Fast,
    });

    let (run_config, restore) = build_initial_run_config(args, &config, auth, speed)?;
    let run_config = Arc::new(StdMutex::new(run_config));

    // `aj continue` with neither an explicit id nor a latest session
    // on disk degrades to a fresh session, matching `aj`. The session
    // selector (interactive resume picking) is a later phase.
    let spec = match &args.command {
        Some(Command::Continue {
            session_id: Some(id),
            prompt: _,
        }) => SessionSpec::Resume {
            session_id: id.clone(),
            entry: SessionEntry::Startup,
        },
        Some(Command::Continue {
            session_id: None,
            prompt: _,
        }) => match persistence.get_latest_session_id()? {
            Some(latest) => SessionSpec::Resume {
                session_id: latest,
                entry: SessionEntry::Startup,
            },
            None => {
                eprintln!("No latest conversation to resume; starting a fresh session.");
                SessionSpec::Create {
                    entry: SessionEntry::Startup,
                }
            }
        },
        _ => SessionSpec::Create {
            entry: SessionEntry::Startup,
        },
    };

    let (mut core, seed) =
        SessionCore::build(&config, &run_config, persistence, &spec, restore.as_ref())?;

    let catalog = load_model_catalog();
    let mut chat = ChatState::new(seed.settings, seed.context_window, catalog);
    chat.hide_thinking_block = config.hide_thinking_block;
    chat.show_image_in_terminal = config.image_show_in_terminal;

    // Replay a resumed session's history through the same reducer the
    // live events go through. Replay never hits the bus, so nothing is
    // double-persisted. A fresh log replays nothing.
    {
        let log = Arc::clone(&core.log);
        let log = log.lock().await;
        for event in replay(&log) {
            let _ = reduce(&mut chat, &mut core.lifecycle, event);
        }
    }

    // Startup notices, after replay so resumed history stays on top.
    for d in diagnostics {
        let text = d.to_string();
        let event = match d.severity() {
            Severity::Warning => AgentEvent::Warning {
                agent_id: AgentId::Main,
                text,
            },
            Severity::Error => AgentEvent::Error {
                agent_id: AgentId::Main,
                text,
            },
        };
        let _ = reduce(&mut chat, &mut core.lifecycle, event);
    }
    for notice in std::mem::take(&mut core.restore_notices) {
        let _ = reduce(&mut chat, &mut core.lifecycle, notice_event(&notice));
    }
    // Apply a `--api-key` runtime override to the resolved provider, then
    // nudge toward logging in when no credential is configured. Both are
    // skipped for the scripted fake provider, which needs no credentials.
    if args.scripted.is_none() {
        let provider_id = {
            let cfg = run_config.lock().expect("run config mutex poisoned");
            cfg.model_key.0.clone()
        };
        if let Some(key) = args.api_key.clone() {
            auth.set_runtime_api_key(&provider_id, key).await;
        }
        let warning = match auth.has_auth(&provider_id).await {
            Ok(true) => None,
            Ok(false) => Some(format!(
                "Heads up: {}",
                aj_app::model::missing_key_message(&provider_id)
            )),
            Err(err) => Some(format!(
                "Couldn't check credentials for {provider_id:?}: {err}"
            )),
        };
        if let Some(text) = warning {
            let event = AgentEvent::Warning {
                agent_id: AgentId::Main,
                text,
            };
            let _ = reduce(&mut chat, &mut core.lifecycle, event);
        }
    }
    // Launch positionals (`aj-next <msg>` / `continue <id> <msg>`) are
    // not auto-submitted yet. Say so instead of silently dropping them.
    let has_launch_input = !args.prompt.is_empty()
        || matches!(&args.command, Some(Command::Continue { prompt, .. }) if !prompt.is_empty());
    if has_launch_input {
        let _ = reduce(
            &mut chat,
            &mut core.lifecycle,
            notice_event("Launch prompt input is not wired up yet. Type it below."),
        );
    }

    Ok(World {
        core,
        chat: Rc::new(RefCell::new(chat)),
        config: Arc::new(StdMutex::new(config)),
        run_config,
        turns: JoinSet::new(),
        turn_cancels: HashMap::new(),
    })
}

/// Wrap a host-side notice in the [`AgentEvent::Notice`] shape so it
/// folds through the same reducer arm as bus notices.
fn notice_event(text: &str) -> AgentEvent {
    AgentEvent::Notice {
        agent_id: AgentId::Main,
        text: text.to_string(),
    }
}

/// Fold `text` into the chat model as a Main-agent notice row.
fn fold_notice(world: &mut World, text: &str) {
    let _ = reduce(
        &mut world.chat.borrow_mut(),
        &mut world.core.lifecycle,
        notice_event(text),
    );
}

/// Fold `first` plus everything else already buffered on the event
/// channel into the chat model. Returns whether anything changed
/// renderable state, so the caller requests one redraw per batch, not
/// one per streaming chunk.
fn drain_events(world: &mut World, first: AgentEvent) -> bool {
    let mut chat = world.chat.borrow_mut();
    let mut redraw = reduce(&mut chat, &mut world.core.lifecycle, first).0;
    while let Ok(event) = world.core.event_rx.try_recv() {
        redraw |= reduce(&mut chat, &mut world.core.lifecycle, event).0;
    }
    redraw
}

/// Outcome of an editor submit.
enum Submit {
    /// A turn was spawned (or a notice explains why not).
    Handled,
    /// The viewed agent is busy. The caller restores the text into
    /// the editor.
    Busy(String),
}

/// Handle an editor submit: spawn a prompt turn on the viewed agent if
/// it is idle.
///
/// While a turn runs, `aj` queues the submit as a follow-up message.
/// aj-next has no pending-message UI yet, so the simplest faithful
/// behavior is to refuse and let the caller put the text back in the
/// editor. Queueing arrives with the pending box in a later phase.
fn handle_submit(world: &mut World, text: String) -> Submit {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return Submit::Handled;
    }
    let target = world.chat.borrow().active_view();
    if world.turn_cancels.contains_key(&target) || world.core.is_running(target) {
        return Submit::Busy(trimmed);
    }
    let policy = turn_policy(target, &world.config);
    // The user's message row arrives back over the bus as
    // `MessageEnd { User }`, so nothing is inserted into the model here.
    let spawned = spawn_turn(
        &world.core,
        &world.run_config,
        target,
        TurnStart::Prompt(trimmed),
        policy,
        &mut world.turns,
        &mut world.turn_cancels,
    );
    if !spawned {
        fold_notice(world, "This agent can't be prompted.");
    }
    Submit::Handled
}

/// Handle one completed turn from the join set. Returns `Err` only for
/// fatal outcomes (turn task panic, `TurnError::Fatal`), which end the
/// session.
fn handle_turn_join(
    world: &mut World,
    joined: Result<(AgentId, Result<(), TurnError>), tokio::task::JoinError>,
) -> Result<()> {
    let (id, result) = joined.map_err(|join_err| anyhow!("agent task panicked: {join_err}"))?;
    world.turn_cancels.remove(&id);
    world.core.mark_idle(id);
    // Main-turn completion bounds every nested initial spawn it
    // started. Drain any sub still marked running that this host is
    // not independently driving, so a leaked sub-agent can't pin the
    // running set forever.
    if id == AgentId::Main {
        for sub in world.core.running_agents() {
            if matches!(sub, AgentId::Sub(_)) && !world.turn_cancels.contains_key(&sub) {
                world.core.mark_idle(sub);
            }
        }
    }
    // Post-turn wake: deliver queued task notices the moment the agent
    // goes idle. `Agent::wake` is a no-op when nothing is pending.
    // NOTE: `aj` additionally wakes on live `TaskStart`/`TaskEnd`
    // triggers mid-select. aj-next only wakes here, which is enough
    // until background-task UI lands.
    if world.core.task_registry.has_notices(id) || world.core.message_queues.has_pending(id) {
        let policy = turn_policy(id, &world.config);
        spawn_wake_turn(
            id,
            &world.core,
            &world.run_config,
            policy,
            &mut world.turns,
            &mut world.turn_cancels,
        );
    }
    match result {
        Ok(()) => Ok(()),
        Err(TurnError::Aborted) => {
            // The agent already emitted the synthetic aborted
            // `MessageEnd`s, so the transcript is consistent. A brief
            // notice confirms Ctrl+C took effect.
            fold_notice(world, "Turn cancelled.");
            Ok(())
        }
        Err(TurnError::Recoverable(_)) => {
            // A recoverable failure already rendered in transcript
            // order from the turn's terminal `MessageEnd`
            // (`AssistantMessage.error`). Re-rendering it here would
            // float it above events still buffered in the channel, so
            // we only keep the session alive (matches `aj`).
            Ok(())
        }
        Err(TurnError::Fatal(err)) => Err(anyhow::Error::msg(err)),
    }
}

/// Ctrl+C acts on the agent the user is viewing: cancel its running
/// turn instead of quitting. Returns whether the press was absorbed
/// (something was running). `false` means the host should quit.
///
/// This is the phase-6 slice of `aj`'s Ctrl+C ladder: no overlay
/// stack, no pending-message yank, and no "press again to quit"
/// arming while other agents or background tasks run. Those arrive
/// with the keymap and overlay phases.
fn cancel_viewed_turn(world: &World) -> bool {
    let active = world.chat.borrow().active_view();
    if let Some(token) = world.turn_cancels.get(&active) {
        token.cancel();
        return true;
    }
    if world.core.is_running(active) {
        // A sub running its initial spawn is owned by the main turn.
        // Cancelling that token cascades to the child.
        if let Some(token) = world.turn_cancels.get(&AgentId::Main) {
            token.cancel();
        }
        return true;
    }
    false
}

/// The root widget: the base layout plus the editor submit plumbing.
struct Shell {
    layout: WidgetRef,
    /// Typed handle to the editor so `Init` can focus it and the host
    /// can restore refused submits.
    editor: Rc<RefCell<TextField>>,
    /// Latest submitted editor text, parked by the `on_submit`
    /// callback for the host loop to collect after dispatch. The
    /// callback can't spawn turns itself (it has no session access).
    submitted: Rc<RefCell<Option<String>>>,
}

impl Shell {
    fn new(chat: Rc<RefCell<ChatState>>, theme: &Theme, header: String, footer: String) -> Shell {
        let submitted: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let editor = Rc::new(RefCell::new(TextField::new()));
        {
            let slot = Rc::clone(&submitted);
            // The TextField clears itself on submit. The host restores
            // the text if the submit is refused.
            editor.borrow_mut().on_submit = Some(Box::new(move |_ctx, text| {
                *slot.borrow_mut() = Some(text.to_string());
            }));
        }
        let transcript = Rc::new(RefCell::new(TranscriptView::new(chat, theme)));
        let layout: WidgetRef = Rc::new(RefCell::new(FlexColumn {
            children: vec![
                FlexItem::init(Rc::new(RefCell::new(Text::new(&header))), 0),
                FlexItem::init(to_widget_ref(transcript), 1),
                FlexItem::init(to_widget_ref(Rc::clone(&editor)), 0),
                FlexItem::init(Rc::new(RefCell::new(Text::new(&footer))), 0),
            ],
        }));
        Shell {
            layout,
            editor,
            submitted,
        }
    }

    /// Collect a submit parked by the editor callback, if any.
    fn take_submitted(&self) -> Option<String> {
        self.submitted.borrow_mut().take()
    }

    /// Put refused submit text back into the (already cleared) editor.
    fn restore_editor_text(&self, text: &str) {
        self.editor.borrow_mut().insert_slice_at_cursor(text);
    }
}

impl Widget for Shell {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // The caller's draw_widget re-stamps the returned surface with
        // the Shell's identity, replacing the column's. The column
        // takes no events, and the children (transcript, editor) keep
        // their own identities for hit-testing.
        draw_widget(&self.layout, ctx)
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        if let Event::Init = event {
            ctx.request_focus(to_widget_ref(Rc::clone(&self.editor)));
            ctx.redraw = true;
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Whether `event` is the Ctrl+C chord. Intercepted host-side (before
/// widget dispatch) because its meaning depends on turn state the
/// widgets don't have.
fn is_ctrl_c(event: &Event) -> bool {
    matches!(event, Event::KeyPress(key) if key.matches(u32::from('c'), Modifiers::CTRL))
}

/// Whether `event` is the tools-expand chord. Hardcoded to alt+o,
/// the default of `aj_app::keybindings::ACTION_TOOLS_EXPAND`. The
/// real keymap engine that resolves configured bindings is phase 8.
fn is_tools_expand(event: &Event) -> bool {
    matches!(event, Event::KeyPress(key) if key.matches(u32::from('o'), Modifiers::ALT))
}

/// Runs the interactive shell until the user quits.
///
/// Restores the terminal via [`AsyncApp::shutdown`] on the way out,
/// then prints the usage banner and resume hint to the normal screen
/// (Spec E section 7). The driver's futures are `!Send`, so this must
/// run on a top-level `block_on` (the `#[tokio::main]` future), not a
/// spawned task.
pub async fn run(args: Args) -> Result<()> {
    // Configuration mirrors `aj`: user config overlaid with the
    // per-project layer, CLI > env > config precedence downstream.
    let (user_config, user_diagnostics) = Config::load();
    let (project_layer, project_diagnostics) = Config::load_project();
    let mut diagnostics = user_diagnostics;
    diagnostics.extend(project_diagnostics);
    let config = project_layer.overlay_onto(&user_config);

    let auth = AuthStorage::at_default_path().context("failed to open ~/.aj/auth.json")?;
    let sessions_dir = Config::get_sessions_dir_path()?;
    let persistence = ConversationPersistence::new(sessions_dir);

    let mut world = build_world(&args, config, &diagnostics, &auth, &persistence).await?;

    // Full theming (config-selected themes, hot reload) is a later
    // phase. The bundled dark palette at the detected color mode is
    // the phase-6 default.
    let theme = Theme::bundled_dark();
    let header = format!("aj-next — {}", world.core.session_id);
    let footer = {
        let chat = world.chat.borrow();
        match chat.footers().model_line(AgentId::Main) {
            Some(line) => format!("{line} — ctrl+c to quit"),
            None => "ctrl+c to quit".to_string(),
        }
    };
    let shell = Rc::new(RefCell::new(Shell::new(
        Rc::clone(&world.chat),
        &theme,
        header,
        footer,
    )));
    let root: WidgetRef = to_widget_ref(Rc::clone(&shell));

    let tty = PosixTty::new()?;
    let reader = tty.open_reader()?;
    let mut app = AsyncApp::new(Vaxis::new(VaxisOptions::default()), Box::new(tty), reader);
    app.init(Rc::clone(&root), Options::default()).await?;

    // Restore the terminal even when the loop exits with a render
    // error, otherwise the user is left stuck on the alt screen.
    let result = drive(&mut app, &root, &shell, &mut world).await;

    // Kill the background-task tree before tearing down turns, so
    // detached process groups are killed and reaped. Then wind down
    // any in-flight turn tasks.
    aj_app::shutdown_background_tasks(&world.core.task_registry).await;
    world.turns.shutdown().await;
    app.shutdown().await;

    // The alt screen wiped the conversation from the terminal, so the
    // normal screen gets the usage banner and the resume hint.
    print_exit_banner(&world).await;
    result
}

/// The host select loop: turn joins, agent events, terminal input, and
/// widget timers. Later phases add their own arms (theme reloads, task
/// wake triggers) to this exact select.
async fn drive(
    app: &mut AsyncApp,
    root: &WidgetRef,
    shell: &Rc<RefCell<Shell>>,
    world: &mut World,
) -> Result<()> {
    loop {
        // Compute the tick deadline before the select so no arm holds
        // a borrow of `app` another arm needs. The sleep expression is
        // evaluated even when the guard is false, hence the fallback.
        let deadline = app.next_tick_deadline();
        tokio::select! {
            biased;

            // --- Agent turn finished ---
            joined = join_next_or_pending(&mut world.turns) => {
                handle_turn_join(world, joined)?;
                app.request_redraw();
            }

            // --- Agent bus event ---
            maybe_event = world.core.event_rx.recv() => {
                // `None` (channel closed) can't happen while the core
                // holds its forwarder subscription. Treat it as a
                // no-op rather than tearing the session down.
                if let Some(event) = maybe_event
                    && drain_events(world, event)
                {
                    app.request_redraw();
                }
            }

            // --- Terminal input ---
            event = app.next_input() => {
                match event {
                    Some(event) => {
                        // Ctrl+C is intercepted before widget dispatch:
                        // cancel the viewed agent's turn if one runs,
                        // quit otherwise.
                        if is_ctrl_c(&event) {
                            if cancel_viewed_turn(world) {
                                continue;
                            }
                            break;
                        }
                        // Alt+O flips the session-wide tool-output
                        // expansion flag. Handled host-side because
                        // the flag lives on the chat model, which the
                        // widgets read at draw time.
                        if is_tools_expand(&event) {
                            {
                                let mut chat = world.chat.borrow_mut();
                                chat.tools_expanded = !chat.tools_expanded;
                            }
                            app.request_redraw();
                        } else {
                            if app.handle_input(event).quit {
                                break;
                            }
                            if let Some(text) = shell.borrow().take_submitted()
                                && let Submit::Busy(text) = handle_submit(world, text)
                            {
                                shell.borrow().restore_editor_text(&text);
                            }
                        }
                    }
                    // The reader ended (EOF or a read error), so no
                    // further input can arrive.
                    None => break,
                }
            }

            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now).into()),
                if deadline.is_some() =>
            {
                if app.fire_due_timers().quit {
                    break;
                }
            }
        }
        app.render_if_needed(root)?;
    }

    Ok(())
}

/// Print the end-of-session usage banner and resume hint to stdout,
/// dimmed and indented like `aj`'s shutdown banner. Call after the alt
/// screen is torn down and with no turn in flight (reading the agent's
/// usage locks it).
async fn print_exit_banner(world: &World) {
    fn dim(s: &str) -> String {
        format!("\x1b[2m{s}\x1b[22m")
    }
    let summary = world.core.usage_summary().await;
    println!();
    for line in format_usage_summary(&summary).lines() {
        println!(" {}", dim(line));
    }
    println!();
    // Only sessions with at least one persisted user-thread leaf are
    // worth resuming. A fresh session the user quit without typing
    // anything gets no hint.
    let resume_eligible = {
        let log = world.core.log.lock().await;
        log.latest_leaf(ThreadFilter::USER).is_some()
    };
    if resume_eligible {
        println!(" {}", dim(&format_resume_hint(&world.core.session_id)));
        println!();
    }
}

#[cfg(test)]
mod tests {
    use std::io::{PipeWriter, Write};
    use std::sync::Arc;

    use aj_app::chat::EntryKind;
    use clap::Parser;
    use tempfile::TempDir;
    use vaxis::tty::TestTty;
    use vaxis::vxfw::{MaxSize, Size};

    use super::*;

    fn empty_chat() -> Rc<RefCell<ChatState>> {
        Rc::new(RefCell::new(ChatState::new(
            aj_agent::events::AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            0,
            Arc::new(Vec::new()),
        )))
    }

    fn test_shell() -> Rc<RefCell<Shell>> {
        Rc::new(RefCell::new(Shell::new(
            empty_chat(),
            &Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor),
            "aj-next".to_string(),
            "ctrl+c to quit".to_string(),
        )))
    }

    /// Builds and initializes an `AsyncApp` over a `TestTty`, with a
    /// pipe as the read source. Keep the returned writer alive or the
    /// reader sees EOF.
    async fn init_app() -> (AsyncApp, PipeWriter, Rc<RefCell<Shell>>, WidgetRef) {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        // Answer the DA1 probe up front so init's capability wait
        // returns as soon as the reader consumes the reply instead of
        // after its timeout.
        writer.write_all(b"\x1b[?c").expect("write DA1 reply");

        let shell = test_shell();
        let root: WidgetRef = to_widget_ref(Rc::clone(&shell));
        let mut app = AsyncApp::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
            reader.into(),
        );
        app.init(Rc::clone(&root), Options::default())
            .await
            .expect("init");
        (app, writer, shell, root)
    }

    /// Build a [`World`] over a scripted provider through the real
    /// setup path (`build_initial_run_config` + `SessionCore::build`),
    /// with persistence and auth confined to a tempdir.
    async fn scripted_world(dir: &TempDir, demo: &str) -> World {
        let args = Args::parse_from(["aj-next", "--scripted", demo]);
        let auth = AuthStorage::new(dir.path().join("auth.json"));
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        build_world(&args, Config::default(), &[], &auth, &persistence)
            .await
            .expect("build world")
    }

    #[tokio::test]
    async fn typed_key_reaches_the_editor_and_latches_redraw() {
        let (mut app, mut writer, shell, _root) = init_app().await;
        assert!(!app.needs_redraw(), "init's first draw clears the latch");

        writer.write_all(b"j").expect("write key byte");
        let event = app.next_input().await.expect("input event");
        let frame = app.handle_input(event);

        assert!(!frame.quit);
        assert!(app.needs_redraw());
        // Init focused the editor, so the typed grapheme landed there.
        assert_eq!(shell.borrow().editor.borrow().graphemes_before_cursor(), 1);
    }

    #[tokio::test]
    async fn enter_parks_the_editor_text_for_the_host() {
        let (mut app, mut writer, shell, _root) = init_app().await;

        writer.write_all(b"hi\r").expect("write prompt + enter");
        for _ in 0..3 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }

        assert_eq!(shell.borrow().take_submitted().as_deref(), Some("hi"));
        // The TextField cleared itself on submit.
        assert_eq!(shell.borrow().editor.borrow().graphemes_before_cursor(), 0);
    }

    #[test]
    fn ctrl_c_is_recognized_host_side() {
        let key = vaxis::key::Key {
            codepoint: u32::from('c'),
            mods: Modifiers::CTRL,
            ..vaxis::key::Key::default()
        };
        assert!(is_ctrl_c(&Event::KeyPress(key)));
        assert!(!is_ctrl_c(&Event::KeyPress(vaxis::key::Key {
            codepoint: u32::from('c'),
            ..vaxis::key::Key::default()
        })));
    }

    #[test]
    fn tools_expand_matches_alt_o_only() {
        let alt_o = vaxis::key::Key {
            codepoint: u32::from('o'),
            mods: Modifiers::ALT,
            ..vaxis::key::Key::default()
        };
        assert!(is_tools_expand(&Event::KeyPress(alt_o)));
        // A bare `o` (normal typing) must reach the editor.
        assert!(!is_tools_expand(&Event::KeyPress(vaxis::key::Key {
            codepoint: u32::from('o'),
            ..vaxis::key::Key::default()
        })));
        // Extra modifiers make it a different chord.
        assert!(!is_tools_expand(&Event::KeyPress(vaxis::key::Key {
            codepoint: u32::from('o'),
            shifted_codepoint: Some(u32::from('O')),
            mods: Modifiers::ALT | Modifiers::SHIFT,
            text: Some("O".into()),
            ..vaxis::key::Key::default()
        })));
        assert!(!is_tools_expand(&Event::KeyPress(vaxis::key::Key {
            codepoint: u32::from('o'),
            mods: Modifiers::ALT | Modifiers::CTRL,
            ..vaxis::key::Key::default()
        })));
    }

    /// End-to-end over the real session path: submit a prompt into a
    /// scripted session, pump the loop arms by hand, and check the
    /// chat model holds the user prompt plus a finalized assistant
    /// reply. A full transcript render over the result must not panic.
    #[tokio::test]
    async fn scripted_prompt_streams_into_the_chat_model() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        assert!(matches!(
            handle_submit(&mut world, "hi there".to_string()),
            Submit::Handled
        ));
        assert!(world.turn_cancels.contains_key(&AgentId::Main));

        // Turn-join arm.
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("turn settles cleanly");
        assert!(world.turn_cancels.is_empty());

        // Event arm: everything the turn emitted is buffered now.
        let first = world.core.event_rx.try_recv().expect("events buffered");
        assert!(drain_events(&mut world, first));

        {
            let chat = world.chat.borrow();
            let entries = chat
                .transcript(AgentId::Main)
                .expect("main transcript")
                .entries();
            let user = entries.iter().find_map(|e| match &e.kind {
                EntryKind::User(u) => Some(u.joined_text()),
                _ => None,
            });
            assert_eq!(user.as_deref(), Some("hi there"));
            let assistant = entries
                .iter()
                .find_map(|e| match &e.kind {
                    EntryKind::Assistant(a) => Some(a),
                    _ => None,
                })
                .expect("assistant entry");
            assert!(assistant.finalized, "assistant entry finalized");
            assert!(!lifecycle_running(&world), "main agent idle after turn");
        }

        // A full render pass over the populated model.
        let mut view = TranscriptView::new(
            Rc::clone(&world.chat),
            &Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor),
        );
        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(80),
                height: Some(24),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: vaxis::gwidth::Method::Unicode,
        };
        let surface = view.draw(&ctx);
        assert_eq!(surface.size.height, 24);
    }

    fn lifecycle_running(world: &World) -> bool {
        world.core.is_running(AgentId::Main)
    }

    /// A busy submit is refused and handed back for the editor.
    #[tokio::test]
    async fn submit_while_running_is_refused_with_the_text() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        assert!(matches!(
            handle_submit(&mut world, "first".to_string()),
            Submit::Handled
        ));
        match handle_submit(&mut world, "second".to_string()) {
            Submit::Busy(text) => assert_eq!(text, "second"),
            Submit::Handled => panic!("busy submit must be refused"),
        }

        // Wind the spawned turn down so the test doesn't leak it.
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("turn settles");
    }

    /// Ctrl+C with a driven turn cancels it (and is absorbed); with
    /// nothing running it falls through to quit.
    #[tokio::test]
    async fn ctrl_c_cancels_a_running_turn_before_quitting() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        assert!(!cancel_viewed_turn(&world), "idle: fall through to quit");

        assert!(matches!(
            handle_submit(&mut world, "go".to_string()),
            Submit::Handled
        ));
        assert!(cancel_viewed_turn(&world), "running turn is cancelled");

        let joined = join_next_or_pending(&mut world.turns).await;
        // The cancelled turn surfaces Aborted, which folds a notice
        // and keeps the session alive.
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
        let chat = world.chat.borrow();
        let entries = chat
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries();
        assert!(entries.iter().any(|e| matches!(
            &e.kind,
            EntryKind::Notice(n) if n.text == "Turn cancelled."
        )));
    }
}
