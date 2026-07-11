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
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use aj_agent::TurnError;
use aj_agent::events::{AgentEvent, AgentId, CompactionReason};
use aj_agent::queue::MessageQueues;
use aj_agent::types::UsageSummary;
use aj_app::actions::AjAction;
use aj_app::chat::{ChatState, reduce};
use aj_app::cli::args::{Args, Command};
use aj_app::commands::{CommandAction, load_model_catalog};
use aj_app::keybindings::fixed_keys;
use aj_app::session::{SessionCore, SessionEntry, SessionExit, SessionRequest, SessionSpec};
use aj_app::session_setup::{RestoreContext, RunConfigSnapshot, build_initial_run_config};
use aj_app::settings::{
    ConfigLayers, ConfigTarget, FooterUpdate, MainConfirm, PersistAction, SpeedConfirm, SubConfirm,
};
use aj_app::shutdown::{format_resume_hint, format_session_usage_header, format_usage_summary};
use aj_app::theme::{
    ColorMode, Theme, ThemeBg, ThemeColor, ThemeHandle, ThemeWatcherGuard, watch_user_theme,
};
use aj_app::turn::{TurnStart, join_next_or_pending, spawn_turn, spawn_wake_turn, turn_policy};
use aj_conf::skills::Skill;
use aj_conf::{
    Config, ConfigDiagnostic, ConfigSpeed, ConfigThinkingDisplay, ConfigVerbosity, Severity,
};
use aj_models::auth::{AuthError, AuthStorage};
use aj_models::registry::ModelInfo;
use aj_models::types::{Speed, UserContent};
use aj_models::usage::default_reset_sources;
use aj_models::{
    ThinkingConfig, speed_from_name, speed_name, thinking_config_from_name, verbosity_name,
};
use aj_session::{ConversationPersistence, PromptEntry, SessionPreview, ThreadFilter, replay};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use vaxis::cell::Style;
use vaxis::tty::PosixTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    AsyncApp, AutocompleteDelivery, DrawContext, EditorTheme, Event, EventContext,
    FilterableSelect, FlexColumn, FlexItem, KeymapController, ListView, MaxSize, Options,
    PopupStyle, RelativePoint, Size, SubSurface, Surface, Text, TextArea, UserEvent, Widget,
    WidgetRef, draw_widget, to_widget_ref,
};

use crate::agent_picker::{AgentPickerOutcome, PickerSnapshot, open_agent_picker};
use crate::content_overlay::{ContentStyles, Row, auth_rows, session_info_rows, set_rows};
use crate::footer::FooterLine;
use crate::keymap::{HostCtx, build_keymap};
use crate::login::{
    AuthPickerRequest, AuthRow, DialogCallbacks, LoginDialogState, open_login_dialog,
    open_login_picker, open_logout_picker,
};
use crate::overlay::{OverlayChrome, OverlayStack, Scrim};
use crate::palette::{FetchKind, PendingFetch, open_palette};
use crate::pending::PendingBox;
use crate::prompt_history::{HistoryFetch, HistoryScope, MAX_ENTRIES, open_prompt_history};
use crate::quit_hint::QuitHint;
use crate::session_selector::{SessionScan, extend_session_scan, open_session_selector};
use crate::settings_ui::{
    MODEL_SETTING_ID, SelectorActivity, SettingsCatalogs, SettingsUi, SettingsValues, SkillRow,
    SkillsFill, UNSET_VALUE, build_skill_rows, open_model, open_settings, open_skills,
    open_thinking, skills_placeholder_row,
};
use crate::splash::{SPLASH_WAKE_EVENT, Splash};
use crate::status::{STATUS_WAKE_EVENT, StatusLine, StatusState};
use crate::task_output::open_task_output;
use crate::transcript::{TranscriptStyles, TranscriptView, vaxis_color};
use crate::usage_overlay::open_usage_overlay;

/// App-event name the drive loop posts after opening an overlay outside
/// dispatch. The Shell handles it by moving focus onto the top overlay: the
/// drive loop owns the session world but has no [`EventContext`] to move focus
/// itself, so it delegates the focus move to the shell via this event.
const REFOCUS_OVERLAY_EVENT: &str = "aj-next.refocus-overlay";

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
    /// Mirror of the lifecycle bits the status chrome (loader,
    /// footer) reads at draw time, shared with those widgets and
    /// refreshed by [`sync_status`] once per loop iteration. The
    /// `AgentLifecycle` itself stays on `core`, where the reducer and
    /// the turn-join arm mutate it.
    status: Rc<RefCell<StatusState>>,
    config: Arc<StdMutex<Config>>,
    /// The user + project config layers behind the effective `config`. The
    /// settings windows mutate one layer, recompute the effective config, and
    /// persist that layer's file (see [`aj_app::settings`]).
    config_layers: Arc<StdMutex<ConfigLayers>>,
    /// The model catalog, shared with the model selector and the settings
    /// window's model submenu. Also seeds [`ChatState`]'s context-window
    /// resolver.
    catalog: Arc<Vec<ModelInfo>>,
    run_config: Arc<StdMutex<RunConfigSnapshot>>,
    /// In-flight turns keyed by the agent running them, plus the
    /// host's clone of each turn's cancel token. The token map's key
    /// set is exactly "agents this host is currently driving".
    turns: JoinSet<(AgentId, Result<(), TurnError>)>,
    turn_cancels: HashMap<AgentId, CancellationToken>,
    /// Credential store, shared with the async read-only overlays (auth
    /// status, usage) whose fetches run detached off the drive loop.
    auth: AuthStorage,
    /// The project's sessions store, shared with the prompt-history scan
    /// (run detached on a blocking thread off the drive loop).
    persistence: ConversationPersistence,
    /// Resume-time settings-restoration context, resolved once at startup
    /// and reused when a session switch rebuilds onto another session so a
    /// resumed session's recorded model/thinking/speed are restored the
    /// same way the process's first session's are. `None` in scripted mode.
    restore: Option<RestoreContext>,
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
    layers: ConfigLayers,
    diagnostics: &[ConfigDiagnostic],
    auth: &AuthStorage,
    persistence: &ConversationPersistence,
) -> Result<World> {
    let config = layers.effective();
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
    let mut chat = ChatState::new(seed.settings, seed.context_window, Arc::clone(&catalog));
    chat.hide_thinking_block = config.hide_thinking_block;
    chat.show_image_in_terminal = config.image_show_in_terminal;
    chat.syntax_highlight = config.syntax_highlighting;

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
    // Order mirrors aj: config diagnostics, then (fresh session only) the
    // context listing followed by the skill warnings, then sandbox, auth,
    // tmux, then the resume-restore notices.
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
    // The context listing and skill warnings describe the freshly-loaded
    // env, which only governs a fresh session. A resumed session keeps its
    // assembled prompt in the log, so we skip them there. Context folds as an
    // Info notice leading the fresh-session block, ahead of the skill and
    // sandbox warnings, matching aj. Any config diagnostics above still precede
    // it. The splash box surfaces only warning-level notices, so context lives
    // in scrollback only, never the box.
    if matches!(spec, SessionSpec::Create { .. }) {
        let _ = reduce(
            &mut chat,
            &mut core.lifecycle,
            notice_event(&aj_app::notices::build_context_notice(
                &core.env,
                strikethrough,
            )),
        );
        for d in &core.env.skill_diagnostics {
            let _ = reduce(
                &mut chat,
                &mut core.lifecycle,
                warning_event(&d.to_string()),
            );
        }
    }
    if aj_app::notices::sandbox_warning_enabled() {
        let _ = reduce(
            &mut chat,
            &mut core.lifecycle,
            warning_event(aj_app::notices::SANDBOX_WARNING),
        );
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
    if let Some(warning) = aj_app::tmux::options().and_then(aj_app::tmux::build_warning) {
        let _ = reduce(&mut chat, &mut core.lifecycle, warning_event(&warning));
    }
    for notice in std::mem::take(&mut core.restore_notices) {
        let _ = reduce(&mut chat, &mut core.lifecycle, notice_event(&notice));
    }

    Ok(World {
        core,
        chat: Rc::new(RefCell::new(chat)),
        status: Rc::new(RefCell::new(StatusState::default())),
        config: Arc::new(StdMutex::new(config)),
        config_layers: Arc::new(StdMutex::new(layers)),
        catalog,
        run_config,
        turns: JoinSet::new(),
        turn_cancels: HashMap::new(),
        auth: auth.clone(),
        persistence: persistence.clone(),
        restore,
    })
}

/// A freshly built session ready to install over the running one: the new
/// core, the seeded chat model, and the notices to fold after install (the
/// switch/create confirmation, the fresh session's context listing, plus any
/// resume-restore notices).
struct NextSession {
    core: SessionCore,
    chat: ChatState,
    notices: Vec<String>,
}

/// Build the session a new-session or resume request asks for, reusing the
/// world's process-lifetime handles (config, run config, catalog,
/// persistence, restore context).
///
/// If the requested build fails, falls back to resuming `previous_id` (the
/// session that just ended, whose log is on disk and current) and reports
/// the failure as the notice instead. Returns `Err` only when the fallback
/// build fails too, which the outer loop treats as fatal. Touches no widget
/// state: installing the returned session stays with the caller.
async fn build_next_session(
    world: &World,
    spec: SessionSpec,
    previous_id: &str,
) -> Result<NextSession> {
    let config = world.config.lock().expect("config mutex poisoned").clone();
    let (mut core, seed, notice, is_fresh) = match SessionCore::build(
        &config,
        &world.run_config,
        &world.persistence,
        &spec,
        world.restore.as_ref(),
    ) {
        Ok((core, seed)) => {
            let notice = switch_notice(&spec, &core.session_id);
            (
                core,
                seed,
                notice,
                matches!(spec, SessionSpec::Create { .. }),
            )
        }
        Err(err) => {
            // The requested build failed. Fall back to the session that
            // just ended so the user keeps a live world, and report why.
            let failure = switch_failure_notice(&spec, &err);
            let fallback = SessionSpec::Resume {
                session_id: previous_id.to_string(),
                entry: SessionEntry::Switch,
            };
            let (core, seed) = SessionCore::build(
                &config,
                &world.run_config,
                &world.persistence,
                &fallback,
                world.restore.as_ref(),
            )?;
            // The fallback resumes an existing session, so it is never fresh.
            (core, seed, failure, false)
        }
    };

    // Seed a fresh chat from the built core, replaying a resumed session's
    // history through the same reducer the live events use. Replay never
    // hits the bus, so nothing is double-persisted; a fresh log replays
    // nothing.
    let mut chat = ChatState::new(
        seed.settings,
        seed.context_window,
        Arc::clone(&world.catalog),
    );
    chat.hide_thinking_block = config.hide_thinking_block;
    chat.show_image_in_terminal = config.image_show_in_terminal;
    chat.syntax_highlight = config.syntax_highlighting;
    {
        let log = Arc::clone(&core.log);
        let log = log.lock().await;
        for event in replay(&log) {
            let _ = reduce(&mut chat, &mut core.lifecycle, event);
        }
    }

    // The switch/create confirmation first, then (for a fresh switch) the
    // context listing folded as an Info notice right after it, then any
    // resume-restore notices. The confirmation is the switch acknowledgment, so
    // context follows it rather than leading. The caller folds these after
    // install so they sit on top of the replayed history.
    let mut notices = vec![notice];
    if is_fresh {
        notices.push(aj_app::notices::build_context_notice(
            &core.env,
            strikethrough,
        ));
    }
    notices.append(&mut core.restore_notices);
    Ok(NextSession {
        core,
        chat,
        notices,
    })
}

/// Confirmation notice for a successful session change, matching aj.
fn switch_notice(spec: &SessionSpec, session_id: &str) -> String {
    match spec {
        SessionSpec::Create { .. } => format!("Started a fresh session ({session_id})."),
        SessionSpec::Resume { session_id, .. } => format!("Switched to session {session_id}."),
    }
}

/// Failure notice when a requested session change couldn't be built (the
/// host falls back to resuming the previous session), matching aj.
fn switch_failure_notice(spec: &SessionSpec, err: &anyhow::Error) -> String {
    match spec {
        SessionSpec::Create { .. } => format!("Failed to start a fresh session: {err}"),
        SessionSpec::Resume { session_id, .. } => {
            format!("Failed to switch to session {session_id}: {err}")
        }
    }
}

/// Install a freshly built [`NextSession`] over the running world in place.
///
/// Rebind by replace-contents: the `chat` and `status` cells keep their
/// identity across the swap (every chrome widget and the keymap's dispatch
/// closure hold clones of these Rcs, captured once at [`Shell::new`]), so
/// overwriting their contents repoints the whole UI at the new session
/// without rebuilding a widget or re-initializing the app. Only the handles
/// a content swap can't reach are repointed in [`Shell::rebind`]: the
/// pending box's message queues (the new agent owns fresh ones) and the
/// header id.
fn install_next_session(world: &mut World, shell: &Rc<RefCell<Shell>>, next: NextSession) {
    *world.chat.borrow_mut() = next.chat;
    // Status is resynced from the new core once per iteration; reset it so
    // the frame between install and the next sync shows idle chrome.
    *world.status.borrow_mut() = StatusState::default();
    world.core = next.core;
    // A session change is only requested with no turn in flight (the outer
    // loop shut the outgoing turns down, and the guard refuses mid-turn
    // requests), so this is already empty; clear defensively.
    world.turn_cancels.clear();
    // Start the switched-to session's splash box at the top: a prior session's
    // wheel scroll must not carry over.
    shell.borrow().splash.borrow_mut().reset_scroll();
    shell.borrow().rebind(world);
    // Folded after the install so they land in the new session's chat, on
    // top of any replayed history.
    for notice in next.notices {
        fold_notice(world, &notice);
    }
}

/// Strike hook for [`aj_app::notices::build_context_notice`], wrapping a
/// disabled skill's row in the SGR strikethrough markers (`ESC[9m` on,
/// `ESC[29m` off). The transcript notice renderer parses these into struck
/// spans. aj-next cannot depend on `aj_tui`, so we spell the markers out here
/// rather than reuse `aj_tui::style::strikethrough`.
fn strikethrough(s: &str) -> String {
    format!("\x1b[9m{s}\x1b[29m")
}

/// Wrap a host-side notice in the [`AgentEvent::Notice`] shape so it
/// folds through the same reducer arm as bus notices.
fn notice_event(text: &str) -> AgentEvent {
    AgentEvent::Notice {
        agent_id: AgentId::Main,
        text: text.to_string(),
    }
}

/// Wrap a host-side warning in the [`AgentEvent::Warning`] shape so it
/// folds through the same reducer arm as bus warnings.
fn warning_event(text: &str) -> AgentEvent {
    AgentEvent::Warning {
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

/// Fold `text` into the chat model as a Main-agent warning row, for
/// failures the user should notice (e.g. a login that errored out).
fn fold_warning(world: &mut World, text: &str) {
    let _ = reduce(
        &mut world.chat.borrow_mut(),
        &mut world.core.lifecycle,
        AgentEvent::Warning {
            agent_id: AgentId::Main,
            text: text.to_string(),
        },
    );
}

/// An in-flight OAuth login the drive loop is tracking.
///
/// Kept outside the overlay stack because the flow is async and
/// long-running rather than a synchronous confirm/cancel selector, but
/// paired with the dialog overlay it pushed. Carries the provider's
/// display name (for the completion notice), the cancel flag the dialog
/// (Esc/Ctrl+C) flips, and the spawned task's handle the loop awaits.
struct LoginSession {
    provider_name: String,
    cancel: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<Result<(), AuthError>>,
}

/// Mount the login dialog and spawn the OAuth flow, tracking it in
/// `login_session`.
///
/// The dialog widget (Rc/RefCell) stays host-side; only the `Send` shared
/// handles (the `Arc<Mutex>` state + pending-input slot and the redraw
/// sender) cross into the spawned task via [`DialogCallbacks`], so the
/// `!Send` widget is never moved onto the tokio task.
fn start_login(
    world: &World,
    shell: &Rc<RefCell<Shell>>,
    app: &mut AsyncApp,
    login_session: &mut Option<LoginSession>,
    redraw_tx: &UnboundedSender<()>,
    provider_id: String,
    provider_name: String,
) {
    // Shared handles: the dialog (UI thread) holds clones; the originals
    // move into the login task's callbacks.
    let state = Arc::new(StdMutex::new(LoginDialogState::default()));
    // Seed a line so the dialog isn't blank before the flow's first
    // callback lands.
    state
        .lock()
        .expect("login dialog state poisoned")
        .lines
        .push(aj_app::auth::LoginLine::Progress(
            "Starting login\u{2026}".to_string(),
        ));
    let pending_input = Arc::new(StdMutex::new(None));
    let cancel = Arc::new(AtomicBool::new(false));

    {
        let handles = shell.borrow().overlay_handles();
        let theme = shell.borrow().theme.clone();
        let snapshot = theme.read();
        open_login_dialog(
            &handles.stack,
            &handles.chrome,
            &snapshot,
            &provider_name,
            Arc::clone(&state),
            Arc::clone(&pending_input),
            Arc::clone(&cancel),
        );
    }

    let auth = world.auth.clone();
    let redraw = redraw_tx.clone();
    let task_state = Arc::clone(&state);
    let task_pending = Arc::clone(&pending_input);
    let handle = tokio::spawn(async move {
        let callbacks = DialogCallbacks::new(task_state, task_pending, redraw);
        auth.login(&provider_id, &callbacks).await
    });

    *login_session = Some(LoginSession {
        provider_name,
        cancel,
        handle,
    });
    // The overlay was pushed from the host (no EventContext), so hand the
    // focus move to the shell, matching the other host-driven opens.
    app.post_app_event(UserEvent {
        name: REFOCUS_OVERLAY_EVENT.to_string(),
        data: None,
    });
    app.request_redraw();
}

/// Pop the login dialog (the top overlay) and hand focus back via the
/// refocus event, which lands on the parent overlay or the editor.
fn close_login_overlay(shell: &Rc<RefCell<Shell>>, app: &mut AsyncApp) {
    shell.borrow().overlays.borrow_mut().back();
    app.post_app_event(UserEvent {
        name: REFOCUS_OVERLAY_EVENT.to_string(),
        data: None,
    });
}

/// Apply a confirmed login/logout provider pick. Login mounts the dialog
/// and spawns the flow; logout is a quick disk write done inline.
async fn apply_auth_request(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    app: &mut AsyncApp,
    login_session: &mut Option<LoginSession>,
    redraw_tx: &UnboundedSender<()>,
    request: AuthPickerRequest,
) {
    match request {
        AuthPickerRequest::Login {
            provider_id,
            provider_name,
        } => start_login(
            world,
            shell,
            app,
            login_session,
            redraw_tx,
            provider_id,
            provider_name,
        ),
        AuthPickerRequest::Logout { provider_id } => {
            let notice = match world.auth.logout(&provider_id).await {
                Ok(()) => format!("Logged out of {provider_id}."),
                Err(err) => format!("Failed to log out of {provider_id}: {err}"),
            };
            fold_notice(world, &notice);
            app.request_redraw();
        }
    }
}

/// Tear down the login dialog when the shared cancel flag is set (the
/// dialog's Esc/Ctrl+C flipped it). Aborts the task and folds a notice;
/// a no-op when no login is in flight or the flag is clear.
///
/// We take `login_session` before aborting so the completion arm (which
/// keys off `login_session`) then sees `None` and its future pends,
/// avoiding a double-close of the overlay.
fn cancel_login(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    app: &mut AsyncApp,
    login_session: &mut Option<LoginSession>,
) {
    if !login_session
        .as_ref()
        .is_some_and(|s| s.cancel.load(Ordering::Relaxed))
    {
        return;
    }
    let session = login_session.take().expect("login session present");
    session.handle.abort();
    close_login_overlay(shell, app);
    fold_notice(
        world,
        &format!("Login to {} cancelled.", session.provider_name),
    );
    app.request_redraw();
}

/// Handle the login task completing: close the dialog, fold the outcome
/// notice, and clear the session.
///
/// The abort branch is effectively unreachable through the cancel-poll,
/// which takes `login_session` before aborting (so this select arm then
/// sees `None` and its future pends forever). We keep it so a task
/// cancelled by any other means stays quiet rather than surfacing a
/// spurious error.
fn finish_login(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    app: &mut AsyncApp,
    login_session: &mut Option<LoginSession>,
    outcome: Result<Result<(), AuthError>, tokio::task::JoinError>,
) {
    let Some(session) = login_session.take() else {
        return;
    };
    close_login_overlay(shell, app);
    let name = session.provider_name;
    match outcome {
        Ok(Ok(())) => fold_notice(world, &format!("Logged in to {name}.")),
        Ok(Err(err)) => fold_warning(world, &format!("Login to {name} failed: {err}")),
        Err(join) if join.is_cancelled() => {}
        Err(join) => fold_warning(world, &format!("Login task error: {join}")),
    }
    app.request_redraw();
}

/// Fold `first` plus everything else already buffered on the event
/// channel into the chat model.
///
/// Returns whether anything changed renderable state (so the caller
/// requests one redraw per batch, not one per streaming chunk) plus
/// the agents that earned a post-turn wake while draining. Wake
/// triggers, matching `aj`'s mid-select set:
///
/// - `TaskEnd`: the owner is woken unconditionally so the completion
///   notice reaches the model the moment the task finishes. The gate
///   inside `spawn_wake_turn` skips busy owners, which pick the
///   notice up at their next drain point instead.
/// - `AgentEnd` with queued notices or pending messages: a sub's
///   initial run is nested inside the parent's turn (not driven
///   through the JoinSet), so the turn-completion trigger never sees
///   it end. Without this, a notice arriving after that run's last
///   drain point would rot until the next prompt. The condition is
///   checked after the event reduced, so the owner reads as idle and
///   the gate inside `spawn_wake_turn` is open.
fn drain_events(world: &mut World, first: AgentEvent) -> (bool, Vec<AgentId>) {
    let mut redraw = false;
    let mut wake_targets = Vec::new();
    let mut next = Some(first);
    while let Some(event) = next {
        // Capture the trigger before the reducer consumes the event.
        let trigger = match &event {
            AgentEvent::TaskEnd { agent_id, .. } => Some((*agent_id, false)),
            AgentEvent::AgentEnd { agent_id, .. } => Some((*agent_id, true)),
            _ => None,
        };
        {
            let mut chat = world.chat.borrow_mut();
            redraw |= reduce(&mut chat, &mut world.core.lifecycle, event).0;
        }
        if let Some((id, conditional)) = trigger
            && (!conditional
                || world.core.task_registry.has_notices(id)
                || world.core.message_queues.has_pending(id))
        {
            wake_targets.push(id);
        }
        next = world.core.event_rx.try_recv().ok();
    }
    (redraw, wake_targets)
}

/// Spawn the post-turn wakes earned while draining a batch of events.
/// `spawn_wake_turn` gates on busy owners, so duplicate targets in
/// one batch are harmless.
fn spawn_wakes(world: &mut World, targets: Vec<AgentId>) {
    for id in targets {
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
}

/// Mirror the lifecycle bits the status chrome reads into the shared
/// [`StatusState`] cell, returning whether the viewed agent is busy.
/// Called once per loop iteration right before rendering, so every
/// mutation path (event batch, turn join, submits) shares one sync
/// point and the mirror can't silently drift.
fn sync_status(world: &World) -> bool {
    let active = world.chat.borrow().active_view();
    let life = &world.core.lifecycle;
    let next = StatusState {
        running: life.is_running(active),
        compacting: life.is_compacting(active),
        sub_agents_running: life
            .running_agents()
            .into_iter()
            .filter(|a| matches!(a, AgentId::Sub(_)))
            .count(),
    };
    *world.status.borrow_mut() = next;
    next.busy()
}

/// Handle an editor submit: spawn a prompt turn on the viewed agent
/// if it is idle, or queue the text as a follow-up while it is busy.
///
/// A queued message shows in the pending box (which reads the live
/// queue snapshot at draw) and is delivered by the post-turn wake:
/// `handle_turn_join` and the `AgentEnd` trigger in [`drain_events`]
/// both spawn a wake when `message_queues.has_pending`. History is
/// recorded by the callers (the drive loop and [`handle_steer`]), which
/// own the editor.
fn handle_submit(world: &mut World, text: String) {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return;
    }
    let target = world.chat.borrow().active_view();
    if world.turn_cancels.contains_key(&target) || world.core.is_running(target) {
        world.core.message_queues.append_follow_up(target, &trimmed);
        return;
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
}

/// Auto-submit the launch prompt (`aj-next <msg>` / `@file ...`) as the
/// initial session's first Main turn. Empty content spawns nothing.
///
/// Kept as a standalone step called from `run` outside the outer session
/// loop, so only the initial session submits and an in-process session
/// switch never resubmits. The launch turn is not recorded into the
/// editor's prompt history, matching `aj`.
fn auto_submit_launch(world: &mut World, content: Vec<UserContent>) {
    if content.is_empty() {
        return;
    }
    let policy = turn_policy(AgentId::Main, &world.config);
    spawn_turn(
        &world.core,
        &world.run_config,
        AgentId::Main,
        TurnStart::Content(content),
        policy,
        &mut world.turns,
        &mut world.turn_cancels,
    );
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
    // Live `TaskEnd`/`AgentEnd` events trigger the same wake mid-select
    // (see `drain_events`), covering tasks that finish between turns.
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

/// Cancel the viewed agent's running turn. Returns whether anything
/// was cancelled. Fired by the keymap's `CancelTurn` action, whose
/// predicate keeps it off the dispatch path while nothing runs.
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

/// Counts of running work a quit would tear down, for the Ctrl+C
/// quit-arming notice: (agents, bash tasks). Ported from `aj`.
///
/// Binary-driven turns plus running agent-backed tasks (background
/// sub-agent runs, which the `turns` JoinSet doesn't track) make up
/// the agent count, running bash tasks the task count.
fn running_work_counts(driven_turns: usize, tasks: &[aj_agent::TaskSummary]) -> (usize, usize) {
    let mut agents = driven_turns;
    let mut bash = 0;
    for task in tasks {
        if task.status != aj_agent::tool::TaskStatus::Running {
            continue;
        }
        match task.kind {
            aj_agent::tool::TaskKind::Agent { .. } => agents += 1,
            aj_agent::tool::TaskKind::Bash { .. } => bash += 1,
        }
    }
    (agents, bash)
}

/// Summarize the background work a quit would tear down, for the quit-arm
/// hint: `"N agents / M tasks still running"`, each part present only when
/// nonzero. `None` when nothing runs, so the hint shows only the ladder.
fn running_work_summary(agents: usize, tasks: usize) -> Option<String> {
    let mut parts = Vec::new();
    if agents > 0 {
        parts.push(format!(
            "{agents} agent{}",
            if agents == 1 { "" } else { "s" }
        ));
    }
    if tasks > 0 {
        parts.push(format!("{tasks} task{}", if tasks == 1 { "" } else { "s" }));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("{} still running", parts.join(" / ")))
}

/// The quit-arm hint's running-work warning for the current world: the
/// background agents and bash tasks a quit would tear down, or `None` when
/// nothing runs.
fn quit_arm_running_work(world: &World) -> Option<String> {
    let (agents, tasks) =
        running_work_counts(world.turns.len(), &world.core.task_registry.snapshot());
    running_work_summary(agents, tasks)
}

/// Pull the viewed agent's queued message back into the editor,
/// prepending it to whatever is currently typed (blank-line joined).
/// Returns whether anything was yanked. Ported from `aj`: used by the
/// dequeue chord and the per-view cancel restore.
fn yank_pending_into_editor(world: &World, shell: &Rc<RefCell<Shell>>) -> bool {
    let target = world.chat.borrow().active_view();
    let Some(text) = world.core.message_queues.take_pending(target) else {
        return false;
    };
    let shell = shell.borrow();
    let mut editor = shell.editor.borrow_mut();
    let current = editor.text();
    let combined = if current.trim().is_empty() {
        text
    } else {
        format!("{text}\n\n{current}")
    };
    // `set_text` replaces the document and drops the cursor at the end, the
    // sensible spot to keep editing the yanked-plus-draft text.
    editor.set_text(&combined);
    true
}

/// The steer gesture (Alt+Enter), ported from `aj`: while the viewed
/// agent is busy, queue the editor text as steering (or promote the
/// pending follow-up when the editor is empty). While idle there is
/// nothing to steer yet, so a non-empty editor starts a normal turn.
fn handle_steer(world: &mut World, shell: &Rc<RefCell<Shell>>) {
    let target = world.chat.borrow().active_view();
    // Read and clear the editor upfront: the queue and spawn branches both
    // consume the draft (matching `aj`), and the promote/no-op branches only
    // run when it was already empty, so clearing is right on every branch.
    let text = {
        let shell = shell.borrow();
        let mut editor = shell.editor.borrow_mut();
        let text = editor.text().trim().to_string();
        editor.clear();
        text
    };
    let busy = world.turn_cancels.contains_key(&target) || world.core.is_running(target);
    if busy {
        if text.is_empty() {
            world.core.message_queues.promote(target);
        } else {
            world.core.message_queues.append_steering(target, &text);
            shell.borrow().editor.borrow_mut().add_to_history(&text);
        }
    } else if !text.is_empty() {
        shell.borrow().editor.borrow_mut().add_to_history(&text);
        handle_submit(world, text);
    }
}

/// Execute a keymap action that needs the session world, parked by the
/// controller's handler for the host loop (widgets can't reach the
/// turn bookkeeping or the queues). Returns whether renderable state
/// changed.
fn handle_host_action(world: &mut World, shell: &Rc<RefCell<Shell>>, action: AjAction) -> bool {
    match action {
        AjAction::CancelTurn => {
            if cancel_viewed_turn(world) {
                // Don't discard what the user lined up: pull any queued
                // message back into the editor (matching `aj`). Without
                // this, the post-turn wake would deliver it right after
                // the cancel.
                return yank_pending_into_editor(world, shell);
            }
            false
        }
        AjAction::Steer => {
            handle_steer(world, shell);
            true
        }
        AjAction::Dequeue => yank_pending_into_editor(world, shell),
        // The clipboard paste arrives with the clipboard port in a later
        // chunk, so it still folds a placeholder notice.
        AjAction::PasteImage => {
            fold_notice(world, "Clipboard image paste is not wired up yet.");
            true
        }
        // The direct chords (ctrl+r, alt+a) open the same overlays as the
        // palette commands. Park the matching command so the host's
        // `apply_command_action` opens it on the next drive-loop step
        // (which owns the refocus move). Nothing renders here yet, so no
        // redraw.
        AjAction::HistoryOpen => {
            *shell.borrow().command_slot.borrow_mut() = Some(CommandAction::OpenPromptHistory);
            false
        }
        AjAction::AgentPickerOpen => {
            *shell.borrow().command_slot.borrow_mut() = Some(CommandAction::OpenAgentPicker);
            false
        }
        // Handled inside the controller's dispatch-side handler (see
        // `Shell::new`), never parked for the host.
        AjAction::ThinkingToggle
        | AjAction::ToolsExpand
        | AjAction::PaletteOpen
        | AjAction::CloseAllOverlays
        | AjAction::ChatPageUp
        | AjAction::ChatPageDown
        | AjAction::ChatScrollToTop
        | AjAction::ChatScrollToBottom
        | AjAction::TranscriptFocus
        | AjAction::CopyMessage
        | AjAction::Quit => false,
    }
}

/// The notice `aj` folds when a command that needs an idle agent is
/// chosen mid-turn.
fn session_busy_notice(what: &str) -> String {
    format!(
        "Can't {what} while a turn is running \u{2014} press {} to cancel it first.",
        fixed_keys::CTRL_C
    )
}

/// Write rendered session HTML to `~/.aj/exports/aj-session-<id>.html`,
/// creating the directory if needed. Returns the path written.
///
/// Exports live under the managed config dir rather than the working
/// directory so an export from inside a git repo doesn't drop an
/// untracked file into the user's tree. The notice reports the full path.
fn write_session_export(session_id: &str, html: &str) -> Result<std::path::PathBuf> {
    let dir = Config::get_config_dir()
        .context("failed to resolve ~/.aj")?
        .join("exports");
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join(format!("aj-session-{session_id}.html"));
    std::fs::write(&path, html).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

/// What applying a parked [`CommandAction`] did, so the drive loop knows
/// whether to redraw and whether to move focus onto a freshly opened overlay.
enum ActionEffect {
    /// Nothing to do.
    None,
    /// Renderable state changed; request a redraw.
    Redraw,
    /// An overlay was opened from the host loop; focus it and redraw.
    OpenedOverlay,
}

/// Apply a [`CommandAction`] the palette parked for the host loop.
///
/// The palette handles the read-only overlays (help, auth, session info,
/// usage), `Quit`, and re-open in its confirm callback, where the focus
/// context is available. The config-editing surfaces (thinking, model,
/// settings) need the session world, which only the host owns, so they open
/// here and rely on the [`REFOCUS_OVERLAY_EVENT`] the caller posts to move
/// focus onto them.
///
/// Skills discovery and HTML export do blocking work, so they don't run here.
/// Skills opens its window up front (with a loading placeholder) and parks a
/// fill handle the drive loop streams the discovered rows into once the
/// off-loop walk lands. HTML export spawns off the loop via `export_tx` and
/// its result notice comes back to the drive loop's fill arm.
async fn apply_command_action(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    action: CommandAction,
    export_tx: &UnboundedSender<String>,
    redraw_tx: &UnboundedSender<()>,
) -> ActionEffect {
    match action {
        CommandAction::Compact => {
            // `/compact` runs as a tracked turn (it owns the turn
            // machinery the palette confirm can't reach). Busy agents get
            // the same notice `aj` folds rather than a silent no-op.
            if world.turn_cancels.contains_key(&AgentId::Main)
                || world.core.is_running(AgentId::Main)
            {
                fold_notice(world, &session_busy_notice("compact"));
            } else {
                let policy = turn_policy(AgentId::Main, &world.config);
                spawn_turn(
                    &world.core,
                    &world.run_config,
                    AgentId::Main,
                    TurnStart::Compact {
                        reason: CompactionReason::Manual,
                        instructions: None,
                    },
                    policy,
                    &mut world.turns,
                    &mut world.turn_cancels,
                );
            }
            ActionEffect::Redraw
        }
        CommandAction::ExportHtml => {
            // Rendering the whole session to HTML (CPU) plus the file write
            // would park the single drive loop, so `spawn_session_export` runs
            // them off the loop and delivers the result notice to the export
            // fill arm. The action just spawns and returns. See that helper for
            // the log-lock reasoning.
            spawn_session_export(world, export_tx);
            ActionEffect::None
        }
        CommandAction::OpenThinkingSelector => {
            let target = world.chat.borrow().active_view();
            let current = viewed_thinking(world, target);
            let handles = shell.borrow().overlay_handles();
            open_thinking(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                &handles.activity,
                target,
                current,
            );
            ActionEffect::OpenedOverlay
        }
        CommandAction::OpenModelSelector => {
            let target = world.chat.borrow().active_view();
            let current = viewed_model(world, target);
            let handles = shell.borrow().overlay_handles();
            open_model(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                &handles.activity,
                Arc::clone(&world.catalog),
                target,
                Some(current),
            );
            ActionEffect::OpenedOverlay
        }
        CommandAction::OpenSettings => {
            let values = user_settings_values(world);
            // The user window has no inherited layer and no project keys;
            // the clear path is inert there.
            let inherited = user_settings_values(world);
            let handles = shell.borrow().overlay_handles();
            open_settings(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                &handles.activity,
                &handles.settings_ui,
                ConfigTarget::User,
                values,
                inherited,
                std::collections::BTreeSet::new(),
                settings_catalogs(world),
            );
            ActionEffect::OpenedOverlay
        }
        CommandAction::OpenProjectSettings => {
            // Per-project settings need a git repository. The effective
            // config-layer view shows what the project pins; the user layer is
            // what a clear reverts to.
            let has_project = world
                .config_layers
                .lock()
                .expect("config layers mutex poisoned")
                .project_path
                .is_some();
            if !has_project {
                fold_notice(
                    world,
                    "Project settings need a git repository (no .git found above the \
                     working directory).",
                );
                return ActionEffect::Redraw;
            }
            let (effective, user, set_keys) = {
                let l = world
                    .config_layers
                    .lock()
                    .expect("config layers mutex poisoned");
                let effective = world.config.lock().expect("config mutex poisoned").clone();
                let set_keys: std::collections::BTreeSet<String> =
                    l.project.set_keys().map(String::from).collect();
                (effective, l.user.clone(), set_keys)
            };
            let values = SettingsValues::from_config(&effective, &world.catalog);
            let inherited = SettingsValues::from_config(&user, &world.catalog);
            let handles = shell.borrow().overlay_handles();
            open_settings(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                &handles.activity,
                &handles.settings_ui,
                ConfigTarget::Project,
                values,
                inherited,
                set_keys,
                settings_catalogs(world),
            );
            ActionEffect::OpenedOverlay
        }
        CommandAction::OpenSkills => {
            // Open the skills window NOW, on top of the kept palette, showing a
            // loading placeholder, and park its fill handle. Discovery walks
            // the `SKILL.md` files up to the git root (blocking IO), so it runs
            // off the loop: the drive loop's drain spawns the walk and streams
            // the discovered rows into the parked handle once it lands. The
            // window shows up immediately and the UI stays responsive
            // meanwhile. Uniform with every other opener. Esc (`close_top`)
            // returns to the palette underneath.
            let handles = shell.borrow().overlay_handles();
            open_skills(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                &handles.activity,
                &handles.skills_fill,
            );
            ActionEffect::OpenedOverlay
        }
        // Session-changing commands tear down the current world and rebuild
        // it, which must never abort in-flight work, so refuse them mid-turn
        // (matching aj). The user can cancel the turn and retry.
        CommandAction::OpenSessionSelector => {
            // Any host-driven turn in flight (Main, a wake turn, or a
            // user-driven sub turn) blocks the switch, matching aj's
            // `!turns.is_empty()` guard and the `world.turns.is_empty()`
            // debug-assert the drive loop honors the request under.
            if !world.turns.is_empty() {
                fold_notice(world, &session_busy_notice("switch sessions"));
                return ActionEffect::Redraw;
            }
            let handles = shell.borrow().overlay_handles();
            open_session_selector(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                &handles.session_scan,
                &handles.session_request,
                world.core.session_id.clone(),
            );
            ActionEffect::OpenedOverlay
        }
        CommandAction::NewSession => {
            if !world.turns.is_empty() {
                fold_notice(world, &session_busy_notice("start a new session"));
            } else {
                // No overlay: park the request straight away. The drive
                // loop's post-input check turns it into `SessionExit::New`.
                *shell.borrow().session_request.borrow_mut() = Some(SessionRequest::New);
            }
            ActionEffect::Redraw
        }
        CommandAction::OpenPromptHistory => {
            let handles = shell.borrow().overlay_handles();
            open_prompt_history(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                &handles.history_fetch,
                &handles.recall_slot,
            );
            ActionEffect::OpenedOverlay
        }
        CommandAction::OpenAgentPicker => {
            let snapshot = {
                let chat = world.chat.borrow();
                PickerSnapshot::gather(&chat)
            };
            let handles = shell.borrow().overlay_handles();
            open_agent_picker(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                &handles.picker_outcome,
                snapshot,
            );
            ActionEffect::OpenedOverlay
        }
        CommandAction::OpenLoginSelector => {
            // The picker needs the OAuth provider list plus each one's
            // credential summary, both async. `apply_command_action` is
            // already async, so build the rows inline and open a fully
            // populated picker (no loading/fill dance needed).
            let providers = world.auth.oauth_provider_ids().await;
            if providers.is_empty() {
                fold_notice(world, "No OAuth providers are available to log in to.");
                return ActionEffect::Redraw;
            }
            let mut rows = Vec::with_capacity(providers.len());
            for (id, name) in &providers {
                let status = aj_app::auth::provider_status(&world.auth, id, Some(name)).await;
                rows.push(AuthRow {
                    provider_id: id.clone(),
                    label: name.clone(),
                    summary: status.summary,
                });
            }
            let handles = shell.borrow().overlay_handles();
            open_login_picker(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                &handles.auth_request,
                rows,
            );
            ActionEffect::OpenedOverlay
        }
        CommandAction::OpenLogoutSelector => {
            // Only stored credentials can be logged out: env vars and
            // --api-key aren't persisted, so they never appear here.
            let mut stored = world.auth.list().await.unwrap_or_default();
            if stored.is_empty() {
                fold_notice(
                    world,
                    "No stored credentials to remove. (Env vars and --api-key aren't \
                     stored and can't be logged out.)",
                );
                return ActionEffect::Redraw;
            }
            stored.sort();
            let oauth = world.auth.oauth_provider_ids().await;
            let mut rows = Vec::with_capacity(stored.len());
            for id in &stored {
                let name = oauth
                    .iter()
                    .find(|(pid, _)| pid == id)
                    .map(|(_, n)| n.as_str());
                let status = aj_app::auth::provider_status(&world.auth, id, name).await;
                rows.push(AuthRow {
                    provider_id: id.clone(),
                    label: name.map(|n| n.to_string()).unwrap_or_else(|| id.clone()),
                    summary: status.summary,
                });
            }
            let handles = shell.borrow().overlay_handles();
            open_logout_picker(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                &handles.auth_request,
                rows,
            );
            ActionEffect::OpenedOverlay
        }
        // The usage overlay is interactive (it carries the rate-limit-reset
        // flow), so it can't ride the read-only content-fill path. The host
        // builds it here where it owns the deps the widget needs: the
        // credential store, the theme snapshot, the shared redraw ping, and
        // the runtime handle it spawns its fetch/consume onto.
        CommandAction::OpenUsageStatus => {
            let handles = shell.borrow().overlay_handles();
            let styles = ContentStyles::from_theme(&shell.borrow().theme.read());
            open_usage_overlay(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                styles,
                world.auth.clone(),
                default_reset_sources(),
                tokio::runtime::Handle::current(),
                redraw_tx.clone(),
            );
            ActionEffect::OpenedOverlay
        }
        // Handled in the palette confirm (overlay openers, quit, re-open)
        // or never a catalog command (`OpenTaskOutput`). Nothing to do.
        CommandAction::Help
        | CommandAction::OpenAuthStatus
        | CommandAction::OpenSessionInfo
        | CommandAction::OpenCommandPalette
        | CommandAction::Quit
        | CommandAction::OpenTaskOutput { .. } => ActionEffect::None,
    }
}

/// The guidance shown in the skills window when discovery finds nothing. The
/// empty window conveys it as an inert placeholder row rather than a transcript
/// notice, so the open window is self-explanatory.
const NO_SKILLS_GUIDANCE: &str =
    "No skills found. Put skills in ~/.agents/skills/ or .agents/skills/ (also: .aj/, .claude/).";

/// Fill the open skills window once discovery lands, replacing its loading
/// placeholder with the discovered rows.
///
/// The window is already on the stack (opened up front with a loading
/// placeholder), so we target the captured `list` handle directly rather than
/// the stack's `top()`. That is what makes the open-then-fill flow safe. A
/// confirm of another opener from the still-interactive palette can't
/// misdirect this fill. An empty result fills a "no skills" placeholder so the
/// window conveys the guidance itself, the same way prompt history shows an
/// empty list, rather than folding a transcript notice.
///
/// The rows and the window widget are `!Send`, so they are built here on the
/// host after the discovery walk delivered the (Send) skills.
fn fill_skills_window(list: &SkillsFill, skills: Vec<Skill>) {
    let rows = if skills.is_empty() {
        vec![skills_placeholder_row(NO_SKILLS_GUIDANCE)]
    } else {
        let display: Vec<SkillRow> = skills
            .into_iter()
            .map(|s| SkillRow {
                name: s.name,
                description: s.description,
                path: aj_conf::display_path(&s.path),
                enabled: s.enabled,
                disable_model_invocation: s.disable_model_invocation,
            })
            .collect();
        build_skill_rows(display)
    };
    list.borrow().set_rows(rows);
}

/// Apply an agent-picker outcome the widget parked. Observing an agent
/// swaps the viewed transcript; opening a task drills into the read-only
/// task viewer (which opens a child overlay, hence [`ActionEffect::OpenedOverlay`]);
/// killing a task cancels it through the registry and folds a notice.
fn apply_picker_outcome(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    outcome: AgentPickerOutcome,
) -> ActionEffect {
    match outcome {
        AgentPickerOutcome::Observe(id) => {
            // The transcript view reads `active_view` at draw, and the
            // per-iteration status/keymap sync picks up the new view, so
            // switching plus a redraw is all it takes.
            world.chat.borrow_mut().set_active_view(id);
            // Each view opens at its bottom with follow-tail engaged (Spec E
            // section 1, per-view scroll). `reset_to_tail` also clears the
            // render cache, which the draw's active-view clear would do
            // anyway, so the two don't fight.
            shell.borrow().transcript.borrow_mut().reset_to_tail();
            ActionEffect::Redraw
        }
        AgentPickerOutcome::OpenTask(id) => {
            // The picker only lists bash tasks, so resolve the command
            // line for the viewer header. A task that left the registry
            // between the snapshot and now has nothing to show.
            let command = world
                .core
                .task_registry
                .summary(id)
                .and_then(|s| match s.kind {
                    aj_agent::tool::TaskKind::Bash { command } => Some(command),
                    aj_agent::tool::TaskKind::Agent { .. } => None,
                });
            match command {
                Some(command) => {
                    let handles = shell.borrow().overlay_handles();
                    open_task_output(
                        &handles.stack,
                        &handles.editor,
                        &handles.chrome,
                        world.core.task_registry.clone(),
                        id,
                        command,
                    );
                    ActionEffect::OpenedOverlay
                }
                None => {
                    fold_notice(
                        world,
                        &format!("Background task #{id} is no longer available."),
                    );
                    ActionEffect::Redraw
                }
            }
        }
        AgentPickerOutcome::Kill(id) => {
            // The picker rows are a snapshot from open time, so consult
            // the live status: the task may have finished while the picker
            // was up.
            let live = world
                .core
                .task_registry
                .snapshot()
                .into_iter()
                .find(|t| t.id == id)
                .map(|t| t.status);
            let notice = match live {
                Some(aj_agent::tool::TaskStatus::Running) => {
                    world.core.task_registry.kill(id);
                    format!("Killing background task #{id}.")
                }
                Some(_) => format!("Background task #{id} already finished."),
                None => format!("Background task #{id} is not in the registry (already gone?)."),
            };
            fold_notice(world, &notice);
            ActionEffect::Redraw
        }
    }
}

/// Recall a prompt-history pick into the editor, replacing whatever is
/// typed. Recall does not submit, so the user can edit before sending.
fn recall_into_editor(shell: &Rc<RefCell<Shell>>, text: &str) {
    let shell = shell.borrow();
    // `set_text` replaces the whole document and leaves the cursor at the end,
    // so the recalled prompt is ready to edit before sending.
    shell.editor.borrow_mut().set_text(text);
}

/// The viewed agent's current thinking level, from its footer entry, falling
/// back to the run config when it has no entry.
fn viewed_thinking(world: &World, target: AgentId) -> Option<ThinkingConfig> {
    world
        .chat
        .borrow()
        .footers()
        .settings(target)
        .and_then(|s| thinking_config_from_name(&s.thinking))
        .unwrap_or_else(|| {
            world
                .run_config
                .lock()
                .expect("run config mutex poisoned")
                .thinking
                .clone()
        })
}

/// The viewed agent's current `(provider, id)`, from its footer entry, falling
/// back to the run config's model key.
fn viewed_model(world: &World, target: AgentId) -> (String, String) {
    world
        .chat
        .borrow()
        .footers()
        .settings(target)
        .map(|s| (s.provider.clone(), s.model_id.clone()))
        .unwrap_or_else(|| {
            world
                .run_config
                .lock()
                .expect("run config mutex poisoned")
                .model_key
                .clone()
        })
}

/// The catalog and name sets the settings window's submenus need. Rediscovered
/// per open so newly added skills are togglable without a restart.
fn settings_catalogs(world: &World) -> SettingsCatalogs {
    let tools: Vec<String> = aj_tools::get_builtin_tools(&aj_tools::BuiltinToolOptions::default())
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    let skills: Vec<String> = aj_conf::skills::discover_skills(&[])
        .0
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    SettingsCatalogs {
        models: Arc::clone(&world.catalog),
        themes: Theme::available(),
        tools,
        skills,
    }
}

/// The live settings-window values the user window opens with: model /
/// thinking / speed / verbosity from the run config (the next-turn truth), the
/// render toggles from the chat model, the rest from the effective config.
fn user_settings_values(world: &World) -> SettingsValues {
    let run_cfg = world.run_config.lock().expect("run config mutex poisoned");
    let cfg = world.config.lock().expect("config mutex poisoned");
    let chat = world.chat.borrow();
    SettingsValues {
        model_key: run_cfg.model_key.clone(),
        model_url: cfg.model_url.clone(),
        thinking: aj_app::commands::thinking_level_name(&run_cfg.thinking).to_string(),
        thinking_display: cfg.thinking_display.map(|d| d.to_string()),
        speed: speed_name(run_cfg.speed).to_string(),
        verbosity: run_cfg
            .stream_options
            .verbosity
            .map(|v| verbosity_name(Some(v)).to_string()),
        theme: cfg.theme.clone().unwrap_or_else(|| "light".to_string()),
        disabled_tools: cfg.disabled_tools.clone(),
        disabled_skills: cfg.disabled_skills.clone(),
        hide_thinking_block: chat.hide_thinking_block,
        image_auto_resize: cfg.image_auto_resize,
        image_show_in_terminal: chat.show_image_in_terminal,
        image_block: cfg.image_block,
        bash_rtk: cfg.bash_rtk,
        syntax_highlighting: chat.syntax_highlight,
        auto_compact: cfg.auto_compact,
        compact_threshold: cfg.compact_threshold.to_string(),
        compact_keep_recent: cfg.compact_keep_recent.to_string(),
    }
}

/// Apply one batch of confirmed config edits parked by the overlays through
/// the shared settings core, reconciling the chat model and folding a notice
/// for each. Returns whether anything changed renderable state.
async fn apply_selector_activity(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    theme_watch: &mut ThemeWatch,
    activity: Vec<SelectorActivity>,
) -> bool {
    let mut changed = false;
    for item in activity {
        changed = true;
        match item {
            SelectorActivity::ThinkingConfirmed { target, level } => {
                let notice = confirm_thinking(world, target, level).await;
                fold_notice(world, &notice);
            }
            SelectorActivity::ModelConfirmed { target, info } => {
                let notice = confirm_model(world, target, *info).await;
                fold_notice(world, &notice);
            }
            SelectorActivity::SettingChange { target, id, value } => {
                let persist = PersistAction::set_for(target);
                if let Some(notice) =
                    apply_setting_change(world, shell, theme_watch, persist, &id, &value).await
                {
                    fold_notice(world, &notice);
                }
            }
            SelectorActivity::SettingClear { id, inherited } => {
                if let Some(notice) = apply_setting_change(
                    world,
                    shell,
                    theme_watch,
                    PersistAction::ProjectClear,
                    &id,
                    &inherited,
                )
                .await
                {
                    fold_notice(world, &notice);
                }
            }
            SelectorActivity::SkillToggle { name, disable } => {
                let notice = apply_skill_toggle(world, &name, disable);
                fold_notice(world, &notice);
            }
        }
    }
    changed
}

/// Record a main-agent footer update into the chat model so its model line and
/// context gauge reflect the change without waiting for the next turn.
fn note_main_footer(world: &World, footer: Option<FooterUpdate>) {
    if let Some(FooterUpdate {
        settings,
        context_window,
    }) = footer
    {
        world.chat.borrow_mut().footers_mut().note_settings(
            AgentId::Main,
            settings,
            context_window,
        );
    }
}

/// Apply a confirmed thinking pick (session-scoped) and reconcile the footer.
async fn confirm_thinking(world: &World, target: AgentId, level: Option<ThinkingConfig>) -> String {
    match target {
        AgentId::Main => {
            let MainConfirm { footer, notice } = aj_app::settings::confirm_thinking_for_main(
                level,
                PersistAction::None,
                &world.run_config,
                &world.config,
                &world.config_layers,
                &world.core,
            )
            .await;
            note_main_footer(world, footer);
            notice
        }
        AgentId::Sub(n) => confirm_thinking_sub(world, n, level).await,
    }
}

/// Apply a confirmed thinking pick to sub-agent `n`, refreshing its footer
/// entry on success. The validation fallback is the model the footer tracks.
async fn confirm_thinking_sub(world: &World, n: usize, level: Option<ThinkingConfig>) -> String {
    let target = AgentId::Sub(n);
    let tracked = world
        .chat
        .borrow()
        .footers()
        .settings(target)
        .and_then(|s| {
            world
                .catalog
                .iter()
                .find(|m| m.provider == s.provider && m.id == s.model_id)
                .cloned()
                .map(Arc::new)
        });
    let SubConfirm { notice, applied } =
        aj_app::settings::confirm_thinking_for_sub(level.clone(), n, tracked, &world.core).await;
    if applied {
        let name = aj_app::commands::thinking_level_name(&level).to_string();
        let entry = world.chat.borrow().footers().settings(target).cloned();
        if let Some(mut settings) = entry {
            let window = world
                .chat
                .borrow()
                .footers()
                .context_usage(target)
                .context_window;
            settings.thinking = name;
            world
                .chat
                .borrow_mut()
                .footers_mut()
                .note_settings(target, settings, window);
        }
    }
    notice
}

/// Apply a confirmed model pick (session-scoped) and reconcile the footer.
async fn confirm_model(world: &World, target: AgentId, info: ModelInfo) -> String {
    match target {
        AgentId::Main => {
            let MainConfirm { footer, notice } = aj_app::settings::confirm_model_for_main(
                info,
                PersistAction::None,
                &world.auth,
                &world.run_config,
                &world.config,
                &world.config_layers,
                &world.core,
            )
            .await;
            note_main_footer(world, footer);
            notice
        }
        AgentId::Sub(n) => confirm_model_sub(world, n, info).await,
    }
}

/// Apply a confirmed model pick to sub-agent `n`, refreshing its footer entry
/// on success at the speed the frontend tracks for it.
async fn confirm_model_sub(world: &World, n: usize, info: ModelInfo) -> String {
    let target = AgentId::Sub(n);
    let staged_speed = world
        .core
        .sub_overrides
        .lock()
        .expect("sub overrides mutex poisoned")
        .get(&n)
        .and_then(|o| o.speed);
    let effective_speed = match staged_speed {
        Some(speed) => speed,
        None => world
            .chat
            .borrow()
            .footers()
            .settings(target)
            .and_then(|s| speed_from_name(&s.speed))
            .flatten(),
    };
    let SubConfirm { notice, applied } = aj_app::settings::confirm_model_for_sub(
        &info,
        n,
        &world.auth,
        effective_speed,
        &world.core,
    )
    .await;
    if applied {
        let (thinking, verbosity) = {
            let chat = world.chat.borrow();
            let settings = chat.footers().settings(target);
            (
                settings
                    .map(|s| s.thinking.clone())
                    .unwrap_or_else(|| "off".to_string()),
                settings
                    .map(|s| s.verbosity.clone())
                    .unwrap_or_else(|| "default".to_string()),
            )
        };
        let settings = aj_agent::events::AgentSettings {
            provider: info.provider.clone(),
            model_id: info.id.clone(),
            thinking,
            speed: speed_name(effective_speed).to_string(),
            verbosity,
        };
        world
            .chat
            .borrow_mut()
            .footers_mut()
            .note_settings(target, settings, info.context_window);
    }
    notice
}

/// Persist a skills-window toggle into `disabled_skills` (user layer). Only
/// changes what new sessions list to the model; the running system prompt is
/// frozen, which the notice says.
fn apply_skill_toggle(world: &World, name: &str, disable: bool) -> String {
    let save = aj_app::settings::persist_user(&world.config_layers, &world.config, |c| {
        if disable {
            if !c.disabled_skills.iter().any(|n| n == name) {
                c.disabled_skills.push(name.to_string());
            }
        } else {
            c.disabled_skills.retain(|n| n != name);
        }
    });
    join_notice(
        format!(
            "Skill {name} {}. Takes effect for new sessions.",
            if disable { "disabled" } else { "enabled" }
        ),
        save,
    )
}

/// Append an optional follow-up note (e.g. a persist failure) to a
/// confirmation notice.
fn join_notice(mut notice: String, note: Option<String>) -> String {
    if let Some(note) = note {
        notice.push(' ');
        notice.push_str(&note);
    }
    notice
}

/// Revert a settings-window row's displayed value after a failed apply, so the
/// window never shows a value that isn't actually active. No-op when the
/// window has closed.
fn revert_setting_row(shell: &Rc<RefCell<Shell>>, id: &str, value: &str) {
    if let Some(ui) = shell.borrow().settings_ui.borrow().as_ref() {
        ui.list.borrow().set_value(id, value);
    }
}

/// Apply one settings-window change (or project clear) to the running session
/// and persist it per `persist`. Returns the user-facing notice.
///
/// Live-appliable settings reuse the same confirm cores as their dedicated
/// selectors (model, thinking, speed, verbosity); the render toggles mutate
/// the chat model; the theme row reloads the palette and re-tints live; the
/// rest are plain config-backed values persisted with a "takes effect" note.
/// A failed apply reverts the row's display through [`revert_setting_row`].
async fn apply_setting_change(
    world: &World,
    shell: &Rc<RefCell<Shell>>,
    theme_watch: &mut ThemeWatch,
    persist: PersistAction,
    id: &str,
    value: &str,
) -> Option<String> {
    match id {
        MODEL_SETTING_ID => {
            let Some(info) = value.split_once('/').and_then(|(provider, model_id)| {
                world
                    .catalog
                    .iter()
                    .find(|m| m.provider == provider && m.id == model_id)
                    .cloned()
            }) else {
                let active = {
                    let cfg = world.run_config.lock().expect("run config mutex poisoned");
                    format!("{}/{}", cfg.model_key.0, cfg.model_key.1)
                };
                revert_setting_row(shell, MODEL_SETTING_ID, &active);
                return Some(format!("Unknown model {value}."));
            };
            let MainConfirm { footer, notice } = aj_app::settings::confirm_model_for_main(
                info,
                persist,
                &world.auth,
                &world.run_config,
                &world.config,
                &world.config_layers,
                &world.core,
            )
            .await;
            note_main_footer(world, footer);
            // The core reports a rebuild failure only as notice text; compare
            // the staged key so the row reverts to the model actually active.
            let active = {
                let cfg = world.run_config.lock().expect("run config mutex poisoned");
                format!("{}/{}", cfg.model_key.0, cfg.model_key.1)
            };
            if active != value {
                revert_setting_row(shell, MODEL_SETTING_ID, &active);
            }
            Some(notice)
        }
        "thinking" => match thinking_config_from_name(value) {
            Some(level) => {
                let MainConfirm { footer, notice } = aj_app::settings::confirm_thinking_for_main(
                    level,
                    persist,
                    &world.run_config,
                    &world.config,
                    &world.config_layers,
                    &world.core,
                )
                .await;
                note_main_footer(world, footer);
                Some(notice)
            }
            None => Some(format!("Unknown thinking level {value:?}.")),
        },
        "thinking_display" => {
            let display = if value == UNSET_VALUE {
                None
            } else {
                match value.parse::<ConfigThinkingDisplay>() {
                    Ok(d) => Some(d),
                    Err(err) => return Some(format!("Can't set thinking_display: {err}")),
                }
            };
            {
                let mut cfg = world.run_config.lock().expect("run config mutex poisoned");
                aj_app::model::apply_thinking_display(&mut cfg.stream_options, display);
            }
            // The "default" sentinel unsets the key in either layer.
            let value_opt = (value != UNSET_VALUE).then_some(value);
            let save = aj_app::settings::persist_setting(
                &world.config_layers,
                &world.config,
                persist,
                "thinking_display",
                value_opt,
                |c| c.thinking_display = display,
            );
            Some(join_notice(
                format!("Thinking display set to {value}. Takes effect next turn."),
                save,
            ))
        }
        "speed" => match speed_from_name(value) {
            Some(speed) => match aj_app::settings::confirm_speed_for_main(
                speed,
                persist,
                &world.auth,
                &world.run_config,
                &world.config,
                &world.config_layers,
                &world.core,
            )
            .await
            {
                SpeedConfirm::Applied { footer, notice } => {
                    note_main_footer(world, Some(footer));
                    Some(notice)
                }
                SpeedConfirm::Failed { previous, notice } => {
                    revert_setting_row(shell, "speed", &previous);
                    Some(notice)
                }
            },
            None => Some(format!("Unknown speed {value:?}.")),
        },
        "verbosity" => {
            let verbosity = if value == UNSET_VALUE {
                None
            } else {
                match value.parse::<ConfigVerbosity>() {
                    Ok(v) => Some(v),
                    Err(err) => return Some(format!("Can't set verbosity: {err}")),
                }
            };
            Some(
                aj_app::settings::confirm_verbosity_for_main(
                    verbosity,
                    persist,
                    &world.run_config,
                    &world.config,
                    &world.config_layers,
                    &world.core,
                )
                .await,
            )
        }
        "theme" => {
            let mode = shell.borrow().theme.color_mode();
            match Theme::load_strict_with_mode(value, mode) {
                Ok(loaded) => {
                    {
                        let s = shell.borrow();
                        s.theme.replace(loaded);
                    }
                    // Re-tint the whole UI, including the open settings window.
                    shell.borrow().restyle();
                    // Re-point the hot-reload watcher at the new theme's file.
                    *theme_watch = ThemeWatch::install(value);
                    let save = aj_app::settings::persist_setting(
                        &world.config_layers,
                        &world.config,
                        persist,
                        "theme",
                        Some(value),
                        |c| c.theme = Some(value.to_string()),
                    );
                    Some(join_notice(format!("Theme set to {value}."), save))
                }
                Err(err) => {
                    let active = {
                        let cfg = world.config.lock().expect("config mutex poisoned");
                        cfg.theme.clone().unwrap_or_else(|| "light".to_string())
                    };
                    revert_setting_row(shell, "theme", &active);
                    Some(format!("Couldn't load theme {value:?}: {err}"))
                }
            }
        }
        "hide_thinking_block" => {
            let hide = value == "true";
            world.chat.borrow_mut().hide_thinking_block = hide;
            let save = aj_app::settings::persist_setting(
                &world.config_layers,
                &world.config,
                persist,
                "hide_thinking_block",
                Some(value),
                |c| c.hide_thinking_block = hide,
            );
            Some(join_notice(
                format!(
                    "Thinking blocks {}.",
                    if hide { "hidden" } else { "expanded" }
                ),
                save,
            ))
        }
        "syntax_highlighting" => {
            let on = value == "true";
            world.chat.borrow_mut().syntax_highlight = on;
            let save = aj_app::settings::persist_setting(
                &world.config_layers,
                &world.config,
                persist,
                "syntax_highlighting",
                Some(value),
                |c| c.syntax_highlighting = on,
            );
            Some(join_notice(
                format!(
                    "Syntax highlighting {}.",
                    if on { "enabled" } else { "disabled" }
                ),
                save,
            ))
        }
        "image_show_in_terminal" => {
            let show = value == "true";
            world.chat.borrow_mut().show_image_in_terminal = show;
            let save = aj_app::settings::persist_setting(
                &world.config_layers,
                &world.config,
                persist,
                "image_show_in_terminal",
                Some(value),
                |c| c.image_show_in_terminal = show,
            );
            Some(join_notice(
                format!("image_show_in_terminal set to {show}."),
                save,
            ))
        }
        "model_url" => {
            let url = (!value.is_empty()).then(|| value.to_string());
            let save = aj_app::settings::persist_setting(
                &world.config_layers,
                &world.config,
                persist,
                "model_url",
                url.as_deref(),
                |c| c.model_url = url.clone(),
            );
            let what = match &url {
                Some(u) => format!("set to {u}"),
                None => "unset".to_string(),
            };
            Some(join_notice(
                format!("model_url {what}. Takes effect on restart."),
                save,
            ))
        }
        // Everything else is a plain config-backed value with no extra live
        // side effect: route it through the schema so a freshly-added option
        // is editable without a bespoke arm here. A project clear carries an
        // already-valid inherited value, so it skips validation.
        other => {
            let Some(option) = Config::option(other) else {
                return Some(format!("Unknown setting {other:?}."));
            };
            if persist != PersistAction::ProjectClear
                && let Err(err) = option.apply_str(value, &mut Config::default())
            {
                return Some(format!("Can't set {other}: {err}"));
            }
            let save = aj_app::settings::persist_setting(
                &world.config_layers,
                &world.config,
                persist,
                other,
                Some(value),
                |c| {
                    // Pre-validated above, so this can't fail.
                    let _ = option.apply_str(value, c);
                },
            );
            Some(join_notice(format!("{other} set to {value}."), save))
        }
    }
}

/// Spawn the async fetch backing a read-only overlay, delivering its
/// rendered rows back to the drive loop over `tx`.
///
/// The row list is `!Send`, so it stays with the caller (the drive loop
/// remembers it and fills it when the result lands). Only the fetched
/// data crosses the task boundary, all of which is `Send`.
///
/// `styles` is the content-column tint snapshot for the auth page. It is
/// `Copy`, so the task captures it directly. See the call site for the
/// theme-reload staleness note.
fn spawn_overlay_fetch(
    world: &World,
    kind: FetchKind,
    styles: ContentStyles,
    tx: &UnboundedSender<(FetchKind, Vec<Row>)>,
) {
    let tx = tx.clone();
    match kind {
        FetchKind::Auth => {
            let auth = world.auth.clone();
            tokio::spawn(async move {
                let rows = auth_rows(&aj_app::auth::collect_statuses(&auth).await, &styles);
                let _ = tx.send((FetchKind::Auth, rows));
            });
        }
        FetchKind::SessionInfo => {
            let log = Arc::clone(&world.core.log);
            tokio::spawn(async move {
                let stats = { log.lock().await.stats() };
                let _ = tx.send((FetchKind::SessionInfo, session_info_rows(&stats)));
            });
        }
    }
}

/// Spawn the prompt-history scan for `scope` on a blocking thread,
/// streaming its per-file entry batches back to the drive loop over `tx`
/// and closing with [`ScanMsg::Done`].
///
/// The scan walks on-disk JSONL logs (blocking IO), so it runs on the
/// blocking pool rather than the loop. The select widget it fills is
/// `!Send`, so it stays on the host side; only the `Send` entries cross
/// the task boundary. Each open or scope toggle gets its own channel, so
/// a superseded scan's batches land on a dropped receiver and are
/// ignored, no scope tag needed to filter stale results.
fn spawn_history_scan(
    world: &World,
    scope: HistoryScope,
    tx: UnboundedSender<ScanMsg<PromptEntry>>,
) {
    let persistence = world.persistence.clone();
    tokio::task::spawn_blocking(move || {
        {
            let mut emit = |batch: Vec<PromptEntry>| {
                let _ = tx.send(ScanMsg::Batch(batch));
            };
            match scope {
                HistoryScope::Workspace => {
                    aj_session::workspace_history_streaming(&persistence, MAX_ENTRIES, &mut emit)
                }
                HistoryScope::All => match Config::get_sessions_base_dir_path() {
                    Ok(base) => {
                        aj_session::all_workspaces_history_streaming(&base, MAX_ENTRIES, &mut emit)
                    }
                    // Fall back to the current workspace so the toggle still
                    // shows something when the base dir can't be resolved.
                    Err(err) => {
                        tracing::debug!("could not resolve sessions base dir: {err}");
                        aj_session::workspace_history_streaming(
                            &persistence,
                            MAX_ENTRIES,
                            &mut emit,
                        )
                    }
                },
            }
        }
        let _ = tx.send(ScanMsg::Done);
    });
}

/// Scan the project's session previews on a blocking thread, streaming
/// per-file preview batches to the drive loop over `tx` and closing with
/// [`ScanMsg::Done`].
///
/// The scan walks on-disk JSONL logs (blocking IO), so it runs on the
/// blocking pool rather than the loop. The select it fills is `!Send`, so
/// it stays on the host side; only the `Send` previews cross the task
/// boundary. [`ConversationPersistence::list_session_previews_streaming`]
/// emits one batch per session file, newest-first, and the host appends
/// each as it lands, matching the loop's progressive-fill overlay pattern.
fn spawn_session_scan(world: &World, tx: UnboundedSender<ScanMsg<SessionPreview>>) {
    let persistence = world.persistence.clone();
    tokio::task::spawn_blocking(move || {
        persistence.list_session_previews_streaming(&mut |batch| {
            let _ = tx.send(ScanMsg::Batch(batch));
        });
        let _ = tx.send(ScanMsg::Done);
    });
}

/// Discover skills off the drive loop, delivering the discovered skills back
/// to the host over `tx`.
///
/// `discover_skills` walks the working directory up to the git root reading
/// `SKILL.md` files (blocking IO), so it runs on the blocking pool rather than
/// the loop. Only the discovered skills (all `Send`) cross the task boundary.
/// The host builds the `!Send` skills window in its fill arm once the walk
/// lands, so discovery never delays input or render.
fn spawn_skills_discovery(world: &World, tx: &UnboundedSender<Vec<Skill>>) {
    let tx = tx.clone();
    // Clone the disabled set out under a brief lock and drop the guard before
    // the blocking walk, so the config lock is never held off the loop.
    let disabled = world
        .config
        .lock()
        .expect("config mutex poisoned")
        .disabled_skills
        .clone();
    tokio::task::spawn_blocking(move || {
        let (skills, _diagnostics) = aj_conf::skills::discover_skills(&disabled);
        let _ = tx.send(skills);
    });
}

/// Render the session to HTML and write it out off the drive loop, delivering
/// the result notice back to the host over `tx`.
///
/// `render_session_html` walks the whole log (CPU) and the file write is
/// blocking IO, both of which would park the single drive loop. We run them on
/// the blocking pool and hold the log lock there for the render, so the UI
/// stays responsive. A concurrent turn wanting the log briefly waits on the
/// lock, which is acceptable for a rare manual export, and cheaper than
/// cloning the whole log. Only the notice string (Send) crosses back.
fn spawn_session_export(world: &World, tx: &UnboundedSender<String>) {
    let tx = tx.clone();
    let log = Arc::clone(&world.core.log);
    let session_id = world.core.session_id.clone();
    tokio::task::spawn_blocking(move || {
        // `blocking_lock` is safe here: this closure runs on the blocking
        // pool, not inside an async context.
        let html = {
            let guard = log.blocking_lock();
            aj_app::export::render_session_html(&guard)
        };
        let notice = match write_session_export(&session_id, &html) {
            Ok(path) => format!("Exported session to {}", aj_conf::display_path(&path)),
            Err(e) => format!("Export failed: {e}"),
        };
        let _ = tx.send(notice);
    });
}

/// Bootstrap the editor's prompt-history ring from the workspace's session
/// logs on a blocking thread, delivering the entries (oldest-first) to the
/// drive loop over the returned receiver.
///
/// The scan walks on-disk JSONL logs (blocking IO), so it runs off the loop
/// and never delays first paint. We reuse the shared
/// [`aj_session::workspace_history`] scanner (the same one the prompt-history
/// overlay uses), capped at the editor's own [`TextArea::HISTORY_LIMIT`]
/// since the ring keeps no more. The scanner returns entries newest-first;
/// we reverse to oldest-first for [`TextArea::seed_history`], which splices
/// them in beneath any prompts submitted this session so an Up press still
/// reaches the most-recent live submission first.
fn spawn_prompt_history_bootstrap(
    persistence: ConversationPersistence,
) -> UnboundedReceiver<Vec<String>> {
    let (tx, rx) = unbounded_channel();
    tokio::task::spawn_blocking(move || {
        let mut entries: Vec<String> =
            aj_session::workspace_history(&persistence, TextArea::HISTORY_LIMIT)
                .into_iter()
                .map(|e| e.text)
                .collect();
        entries.reverse();
        let _ = tx.send(entries);
    });
    rx
}

/// Await the backgrounded prompt-history bootstrap. Mirrors [`recv_theme`]: an
/// absent receiver (already delivered, so the drive loop cleared it) pends
/// forever, making the `select!` arm a no-op once the ring is seeded.
async fn recv_prompt_history(
    rx: Option<&mut UnboundedReceiver<Vec<String>>>,
) -> Option<Vec<String>> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// One message from a streaming overlay scan: a batch of rows in scan
/// order, then a single terminal [`ScanMsg::Done`]. A dropped sender
/// (superseded scan) closes the channel without a `Done`, which the fill
/// arm treats the same as `Done`.
enum ScanMsg<T> {
    Batch(Vec<T>),
    Done,
}

/// Await the next message from an optional scan receiver, pending forever
/// when there is no scan in flight. Mirrors [`recv_prompt_history`] so a
/// `tokio::select!` arm can poll an `Option<Receiver>` without a nested
/// match.
async fn recv_scan<T>(rx: Option<&mut UnboundedReceiver<ScanMsg<T>>>) -> Option<ScanMsg<T>> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The in-flight prompt-history scan the drive loop streams into. Holds the
/// `!Send` select on the host side, paired with the scan's own receiver so
/// a superseded scope toggle's channel is simply dropped.
struct HistoryFill {
    select: Rc<RefCell<FilterableSelect>>,
    rx: UnboundedReceiver<ScanMsg<PromptEntry>>,
    /// The first batch replaces the loading placeholder; later batches
    /// append in place.
    first: bool,
}

/// The in-flight session-preview scan the drive loop streams into. Holds
/// the `!Send` [`SessionScan`] plus the scan's own receiver.
struct SessionFill {
    scan: SessionScan,
    rx: UnboundedReceiver<ScanMsg<SessionPreview>>,
    /// The first batch replaces the loading placeholder; later batches
    /// append in place.
    first: bool,
    /// Chase the active session's row until it streams in, then stop so a
    /// late batch can't yank the cursor away from the user's navigation.
    select_current_pending: bool,
    /// The row the chase last parked the selection on. Once a later batch
    /// finds the selection sitting elsewhere, the user has navigated, so
    /// the chase gives up rather than yanking the cursor.
    anchor: Option<String>,
}

/// The editor's visible-row cap from the terminal height, `aj`'s
/// `max(5, floor(rows * 0.3))`. In `vxfw` the layout owns the height budget
/// (the editor sizes to its constraints), so the host computes the cap from
/// the current frame height and feeds it via
/// [`TextArea::set_max_visible_rows`]. The editor then grows with content up
/// to this many rows and scrolls beyond.
fn editor_row_cap(terminal_rows: usize) -> usize {
    // Integer multiply-then-divide floors naturally.
    ((terminal_rows * 3) / 10).max(5)
}

/// Rows the footer occupies below the editor in the shell's `FlexColumn`.
///
/// The footer is a single softwrap-off `RichText` line, so it is always exactly
/// one row. The autocomplete overlay anchor math in [`Shell`]'s `draw`
/// subtracts this to find the editor's top row. If the footer ever wraps to
/// more than one row, this must change.
const FOOTER_ROWS: u16 = 1;

/// Rows the header occupies at the top of the shell's `FlexColumn`.
///
/// The header is the single title line. The autocomplete overlay leaves at
/// least this many rows on screen above itself so the title stays visible.
const HEADER_ROWS: u16 = 1;

/// Build the editor's border theme from the shared palette (Spec D structured
/// colors), the same way the other chrome resolves its styles.
///
/// `aj` tints the editor border by the agent's thinking level (and a bash
/// mode), resolving through a live theme closure. We render the border with
/// the thinking-off token statically, which matches `aj`'s un-tinted default
/// (`thinkingOff` and `borderMuted` share a value in the bundled themes).
///
/// TODO(aljoscha): tint the border by thinking level via
/// [`TextArea::set_border_color`], recomputed on thinking changes and session
/// installs, to reach full parity with `aj`'s editor border.
fn editor_theme_from_theme(theme: &Theme) -> EditorTheme {
    let mode = theme.color_mode();
    // The autocomplete popup paints its selected row as a compact band over
    // `ThemeBg::SelectedBg`, sized to hug the entry text rather than spanning
    // the full editor width. That band is the selection idiom every other
    // selector in this shell uses (see `select_styles_from_theme`), so the
    // completion popup matches. The unselected `item` style keeps the default
    // background, which is the surface's own blank fill, so the popup stays
    // opaque over the transcript underneath.
    let popup = PopupStyle {
        item: Style {
            fg: vaxis_color(theme.fg_color(ThemeColor::Text), mode),
            ..Style::default()
        },
        selected: Style {
            fg: vaxis_color(theme.fg_color(ThemeColor::Text), mode),
            bg: vaxis_color(theme.bg_color(ThemeBg::SelectedBg), mode),
            ..Style::default()
        },
    };
    EditorTheme {
        border_color: vaxis_color(theme.fg_color(ThemeColor::ThinkingOff), mode),
        popup,
        ..EditorTheme::default()
    }
}

/// The shared handles the drive loop hands to a config-editing overlay's open
/// function. Gathered from the shell in one borrow so the open call site never
/// holds a shell borrow across it.
struct OverlayHandles {
    stack: Rc<RefCell<OverlayStack>>,
    editor: WidgetRef,
    chrome: OverlayChrome,
    activity: Rc<RefCell<Vec<SelectorActivity>>>,
    settings_ui: Rc<RefCell<Option<SettingsUi>>>,
    /// Where the agent picker parks its confirmed pick / kill.
    picker_outcome: Rc<RefCell<Option<AgentPickerOutcome>>>,
    /// Where the prompt-history overlay parks a scan request.
    history_fetch: Rc<RefCell<Option<HistoryFetch>>>,
    /// Where the skills window parks its fill handle on open, for the drive
    /// loop to stream discovered rows into.
    skills_fill: Rc<RefCell<Option<SkillsFill>>>,
    /// Where the prompt-history overlay parks a recalled prompt.
    recall_slot: Rc<RefCell<Option<String>>>,
    /// Where the session selector parks its preview-scan request.
    session_scan: Rc<RefCell<Option<SessionScan>>>,
    /// Where the session selector parks a confirmed resume request.
    session_request: Rc<RefCell<Option<SessionRequest>>>,
    /// Where the login/logout picker parks a confirmed provider action.
    auth_request: Rc<RefCell<Option<AuthPickerRequest>>>,
}

/// The chat slot: draws the empty-state [`Splash`] until the active view has a
/// user or assistant entry, then the [`TranscriptView`].
///
/// A thin wrapper so the flex-1 child can pick per draw without disturbing the
/// transcript's focus and scroll wiring. Whichever child it picks is drawn
/// through [`draw_widget`], so that child's stamped widget identity (and thus
/// its hit-testing and focus) survives on the surface tree exactly as if it
/// were the direct flex child. The slot itself is inert, so it stays off the
/// hit list.
struct ChatSlot {
    chat: Rc<RefCell<ChatState>>,
    splash: Rc<RefCell<Splash>>,
    transcript: Rc<RefCell<TranscriptView>>,
}

impl Widget for ChatSlot {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let child = if self.chat.borrow().has_conversation() {
            draw_widget(&to_widget_ref(Rc::clone(&self.transcript)), ctx)
        } else {
            draw_widget(&to_widget_ref(Rc::clone(&self.splash)), ctx)
        };
        // Wrap the child's surface rather than returning it, so the caller's
        // `draw_widget` re-stamps this slot's identity onto the wrapper and
        // leaves the child's stamp intact underneath.
        let mut surface = Surface::with_size(child.size);
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: child,
            z_index: 0,
        });
        surface
    }
}

/// The root widget: the keymap controller wrapping the base layout, the
/// editor submit plumbing, and the overlay stack drawn above everything
/// while it is open.
struct Shell {
    /// The keymap controller wrapping the base layout. Drawn (and thereby
    /// placed on the focus path) by [`Shell::draw`], which also appends
    /// the overlay children to its surface so an open overlay is a
    /// descendant and the controller's capture chords pre-empt it.
    keymap: Rc<RefCell<KeymapController<AjAction, HostCtx>>>,
    /// The context the keymap predicates read. Overlay liveness is live
    /// through the shared stack, `turn_running` is refreshed by the drive
    /// loop's per-iteration sync (see [`sync_keymap_ctx`]).
    keymap_ctx: Rc<RefCell<HostCtx>>,
    /// Typed handle to the editor so `Init` can focus it.
    editor: Rc<RefCell<TextArea>>,
    /// Typed handle to the loader line so host-posted app events (the
    /// busy-edge wake, see [`drive`]) reach it. The loader is not on
    /// the focus path, so the Shell forwards from its capturing phase.
    status_line: Rc<RefCell<StatusLine>>,
    /// The Ctrl+C quit-arm hint, floated above the editor while the quit
    /// sequence is armed. Drawn straight from the live keymap state.
    quit_hint: Rc<RefCell<QuitHint>>,
    /// The quit-arm hint's running-work warning, refreshed by the drive
    /// loop on the arming edge (it owns the task registry the widgets
    /// can't reach). Shared with the [`QuitHint`], which reads it at draw.
    quit_hint_warning: Rc<RefCell<Option<String>>>,
    /// Latest submitted editor text, parked by the `on_submit`
    /// callback for the host loop to collect after dispatch. The
    /// callback can't spawn turns itself (it has no session access).
    submitted: Rc<RefCell<Option<String>>>,
    /// The keymap action awaiting the host loop, parked by the
    /// controller's handler (the same slot pattern as `submitted`) for
    /// the actions that need the session world.
    host_action: Rc<RefCell<Option<AjAction>>>,
    /// The modal stack. Shared: overlay callbacks (confirm/cancel) mutate
    /// it from inside dispatch while the Shell reads it at draw time.
    overlays: Rc<RefCell<OverlayStack>>,
    /// The scrim widget, kept across frames so its identity is stable for
    /// hit-testing.
    scrim: Rc<RefCell<Scrim>>,
    /// A host-applied command parked by the palette's confirm (compact,
    /// export, and the not-yet-wired selectors), collected after dispatch.
    command_slot: Rc<RefCell<Option<CommandAction>>>,
    /// A request to fill a just-opened async read-only overlay, parked by
    /// the palette's confirm for the drive loop to fetch and deliver.
    fetch_slot: Rc<RefCell<Option<PendingFetch>>>,
    /// The live theme handle, read by [`Shell::restyle`] to rebuild every
    /// style struct after a runtime swap. Shared with the drive loop,
    /// which replaces it on a hot-reload.
    theme: ThemeHandle,
    /// Overlay frame styles, shared with the palette-open handler so a
    /// theme swap re-tints overlays opened afterward. Rebuilt by
    /// [`Shell::restyle`].
    chrome: Rc<RefCell<OverlayChrome>>,
    /// Typed handles to the palette-consuming widgets, kept so
    /// [`Shell::restyle`] can push rebuilt styles into them.
    transcript: Rc<RefCell<TranscriptView>>,
    /// The empty-state splash, kept so [`Shell::restyle`] can re-tint it and
    /// [`Shell::capture_event`] can forward the wake that starts its animation.
    /// Not on the focus path (it is non-interactive), so the Shell forwards to
    /// it the same way it does the loader.
    splash: Rc<RefCell<Splash>>,
    pending: Rc<RefCell<PendingBox>>,
    footer: Rc<RefCell<FooterLine>>,
    /// Confirmed config edits parked by the selector and settings overlays
    /// for the drive loop to apply through the shared settings core (the
    /// overlays can't reach the async cores or the session world). Drained
    /// after each input event.
    selector_activity: Rc<RefCell<Vec<SelectorActivity>>>,
    /// Live handles to an open settings window, so a failed apply can revert a
    /// row and a theme swap can re-tint the window. `None` when no settings
    /// window is open.
    settings_ui: Rc<RefCell<Option<SettingsUi>>>,
    /// The agent picker's confirmed pick / kill, parked for the drive
    /// loop (which owns the chat model and the task registry).
    picker_outcome: Rc<RefCell<Option<AgentPickerOutcome>>>,
    /// A prompt-history scan request parked by the overlay (on open and
    /// on scope toggle) for the drive loop to run and fill.
    history_fetch: Rc<RefCell<Option<HistoryFetch>>>,
    /// The skills window's fill handle, parked on open. The drive loop's
    /// skills fill arm replaces its loading placeholder with the discovered
    /// rows through this captured handle (never the stack's `top()`) once the
    /// off-loop walk lands.
    skills_fill: Rc<RefCell<Option<SkillsFill>>>,
    /// A recalled prompt parked by the prompt-history overlay, collected
    /// by the drive loop and dropped into the editor.
    recall_slot: Rc<RefCell<Option<String>>>,
    /// Typed handle to the header line, so a session rebuild can refresh
    /// the shown session id in place.
    header: Rc<RefCell<Text>>,
    /// A session-preview scan request parked by the session selector on
    /// open, for the drive loop to run off the loop and fill.
    session_scan: Rc<RefCell<Option<SessionScan>>>,
    /// A session change parked by the `NewSession` command or a confirmed
    /// session-selector pick. Drained after each input event; a `Some`
    /// exits the drive loop with the matching [`SessionExit`].
    session_request: Rc<RefCell<Option<SessionRequest>>>,
    /// A confirmed login/logout provider pick parked by the auth picker,
    /// drained by the drive loop (which owns the credential store and the
    /// login task machinery).
    auth_request: Rc<RefCell<Option<AuthPickerRequest>>>,
}

impl Shell {
    fn new(
        chat: Rc<RefCell<ChatState>>,
        status: Rc<RefCell<StatusState>>,
        queues: MessageQueues,
        theme: ThemeHandle,
        header: String,
        cwd: PathBuf,
    ) -> Shell {
        let submitted: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let editor = TextArea::new();
        {
            let slot = Rc::clone(&submitted);
            // The editor clears itself on submit. A busy-agent
            // submit is queued (not restored), so the clear is right
            // either way.
            editor.borrow_mut().on_submit = Some(Box::new(move |_ctx, text| {
                *slot.borrow_mut() = Some(text.to_string());
            }));
        }
        // Install the `@`-file autocomplete provider on the editor, rooted at
        // the session's working directory. The cwd is per-process and stable
        // across session switches, and the editor persists across session
        // rebinds (`rebind` swaps only session-scoped handles, never the
        // editor), so the provider is set once here and never reinstalled.
        // Typing `/` at the empty prompt still opens the command palette (see
        // `on_palette_trigger` below). There is no `/`- or `#`-completion
        // provider, only `@`-file completion.
        editor.borrow_mut().set_autocomplete_provider(Arc::new(
            crate::autocomplete::CombinedAutocompleteProvider::new(cwd.clone()),
        ));
        // Cap the popup at 20 rows, its fixed ceiling. The session already caps
        // matches at 20, and `Shell::draw` clamps per frame to the space above
        // the editor, so this only bounds a tall terminal. Set once here: the
        // editor persists across session rebinds, so it never needs resetting.
        editor.borrow_mut().set_autocomplete_max_visible(20);
        // The footer shows the working directory as text. The provider owns the
        // path itself.
        let cwd_display = cwd.display().to_string();
        // The transcript-focus flag, shared between the transcript (its single
        // writer, via focus in/out) and the keymap host context (which reads it
        // to gate the copy chord). Created here so both get the same cell.
        let focus_mode = Rc::new(std::cell::Cell::new(false));
        // Resolve the initial styles and chrome from a single snapshot of
        // the theme, then keep the handle for the runtime re-style path.
        let (styles, transcript, chrome) = {
            let t = theme.read();
            let styles = Rc::new(TranscriptStyles::from_theme(&t));
            let transcript = Rc::new(RefCell::new(TranscriptView::new(
                Rc::clone(&chat),
                &t,
                Rc::clone(&focus_mode),
            )));
            editor.borrow_mut().set_theme(editor_theme_from_theme(&t));
            (styles, transcript, OverlayChrome::from_theme(&t))
        };
        let chrome = Rc::new(RefCell::new(chrome));
        let status_line = StatusLine::new(Rc::clone(&chat), Rc::clone(&status), Rc::clone(&styles));
        let quit_hint_warning: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let quit_hint = Rc::new(RefCell::new(QuitHint::new(
            Rc::clone(&styles),
            Rc::clone(&chrome),
            Rc::clone(&quit_hint_warning),
        )));
        let pending = Rc::new(RefCell::new(PendingBox::new(
            Rc::clone(&chat),
            queues,
            Rc::clone(&styles),
        )));
        let footer = Rc::new(RefCell::new(FooterLine::new(
            Rc::clone(&chat),
            status,
            Rc::clone(&styles),
            cwd_display,
        )));
        // The empty-state splash and the transcript share the chat slot. The
        // `ChatSlot` wrapper draws whichever fits the current state, so the
        // transcript's focus and scroll wiring is untouched while it is shown.
        let splash = Splash::new(Rc::clone(&chat), styles, theme.color_mode());
        let chat_slot = Rc::new(RefCell::new(ChatSlot {
            chat: Rc::clone(&chat),
            splash: Rc::clone(&splash),
            transcript: Rc::clone(&transcript),
        }));
        // Slot order mirrors `aj`'s layout: header, chat (flex),
        // status, pending, editor, footer. The status and pending
        // slots collapse to zero height while idle/empty, so the
        // editor sits flush under the chat between turns.
        let header_line = Rc::new(RefCell::new(Text::new(&header)));
        let layout: WidgetRef = Rc::new(RefCell::new(FlexColumn {
            children: vec![
                FlexItem::init(to_widget_ref(Rc::clone(&header_line)), 0),
                FlexItem::init(to_widget_ref(Rc::clone(&chat_slot)), 1),
                FlexItem::init(to_widget_ref(Rc::clone(&status_line)), 0),
                FlexItem::init(to_widget_ref(Rc::clone(&pending)), 0),
                FlexItem::init(to_widget_ref(Rc::clone(&editor)), 0),
                FlexItem::init(to_widget_ref(Rc::clone(&footer)), 0),
            ],
        }));

        let overlays = Rc::new(RefCell::new(OverlayStack::default()));
        let command_slot: Rc<RefCell<Option<CommandAction>>> = Rc::new(RefCell::new(None));
        let fetch_slot: Rc<RefCell<Option<PendingFetch>>> = Rc::new(RefCell::new(None));
        let host_action: Rc<RefCell<Option<AjAction>>> = Rc::new(RefCell::new(None));
        let selector_activity: Rc<RefCell<Vec<SelectorActivity>>> =
            Rc::new(RefCell::new(Vec::new()));
        let settings_ui: Rc<RefCell<Option<SettingsUi>>> = Rc::new(RefCell::new(None));
        let picker_outcome: Rc<RefCell<Option<AgentPickerOutcome>>> = Rc::new(RefCell::new(None));
        let history_fetch: Rc<RefCell<Option<HistoryFetch>>> = Rc::new(RefCell::new(None));
        let skills_fill: Rc<RefCell<Option<SkillsFill>>> = Rc::new(RefCell::new(None));
        let recall_slot: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let session_scan: Rc<RefCell<Option<SessionScan>>> = Rc::new(RefCell::new(None));
        let session_request: Rc<RefCell<Option<SessionRequest>>> = Rc::new(RefCell::new(None));
        let auth_request: Rc<RefCell<Option<AuthPickerRequest>>> = Rc::new(RefCell::new(None));
        let keymap_ctx = Rc::new(RefCell::new(HostCtx {
            overlays: Rc::clone(&overlays),
            editor: Rc::clone(&editor),
            focus_mode: Rc::clone(&focus_mode),
            turn_running: false,
            login_active: false,
        }));

        // The controller's action handler. Actions whose effects are
        // reachable from widget land (the chat model, the overlay
        // stack, the quit flag) execute here, inside dispatch, where
        // the live `EventContext` can move focus. The rest park in the
        // `host_action` slot for the drive loop, which owns the world.
        let on_action: Box<dyn FnMut(&mut EventContext, &AjAction)> = {
            let chat = Rc::clone(&chat);
            let overlays_for_actions = Rc::clone(&overlays);
            let editor_widget: WidgetRef = to_widget_ref(Rc::clone(&editor));
            let chrome_for_actions = Rc::clone(&chrome);
            let command_slot_for_actions = Rc::clone(&command_slot);
            let fetch_slot_for_actions = Rc::clone(&fetch_slot);
            let settings_ui_for_actions = Rc::clone(&settings_ui);
            let transcript_for_actions = Rc::clone(&transcript);
            let transcript_widget: WidgetRef = to_widget_ref(Rc::clone(&transcript));
            let action_slot = Rc::clone(&host_action);
            Box::new(move |ctx, action| match action {
                AjAction::ThinkingToggle => {
                    // Matches aj's `aj.thinking.toggle` handler: flip the
                    // hide flag, no notice (the transcript shows the new
                    // state).
                    let mut chat = chat.borrow_mut();
                    chat.hide_thinking_block = !chat.hide_thinking_block;
                    ctx.redraw = true;
                }
                AjAction::ToolsExpand => {
                    let mut chat = chat.borrow_mut();
                    chat.tools_expanded = !chat.tools_expanded;
                    ctx.redraw = true;
                }
                AjAction::PaletteOpen => {
                    // The binding's predicate already gates on "no overlay
                    // open", matching aj's inert-while-modal behavior.
                    open_palette(
                        &overlays_for_actions,
                        &editor_widget,
                        &chrome_for_actions,
                        &command_slot_for_actions,
                        &fetch_slot_for_actions,
                        ctx,
                    );
                }
                AjAction::CloseAllOverlays => {
                    overlays_for_actions.borrow_mut().close_all();
                    // Release any settings-window handles so a closed window is
                    // never re-tinted or reverted by the host.
                    *settings_ui_for_actions.borrow_mut() = None;
                    ctx.request_focus(Rc::clone(&editor_widget));
                    ctx.redraw = true;
                }
                AjAction::Quit => ctx.quit = true,
                AjAction::ChatPageUp => {
                    // Chat scroll is reachable from widget land (the transcript
                    // owns its scroll state), so it runs here in dispatch rather
                    // than parking for the host loop.
                    transcript_for_actions.borrow_mut().page_up();
                    ctx.redraw = true;
                }
                AjAction::ChatPageDown => {
                    transcript_for_actions.borrow_mut().page_down();
                    ctx.redraw = true;
                }
                AjAction::ChatScrollToTop => {
                    transcript_for_actions.borrow_mut().scroll_to_top();
                    ctx.redraw = true;
                }
                AjAction::ChatScrollToBottom => {
                    transcript_for_actions.borrow_mut().scroll_to_bottom();
                    ctx.redraw = true;
                }
                AjAction::TranscriptFocus => {
                    // Tab has two meanings while the autocomplete popup is
                    // closed (its gate), split by whether the transcript is
                    // already focused (Spec E section 1). When focused, Tab
                    // steps to the next-older user message. Otherwise it engages
                    // focus mode, but only if there is a user message to land
                    // on: its `FocusIn` lands on the newest one. No user
                    // messages means Tab is a no-op and stays in the editor.
                    let transcript = transcript_for_actions.borrow_mut();
                    if transcript.in_focus_mode() {
                        transcript.focus_prev_user_message();
                        ctx.redraw = true;
                    } else if transcript.has_user_message() {
                        drop(transcript);
                        ctx.request_focus(Rc::clone(&transcript_widget));
                        ctx.redraw = true;
                    }
                }
                AjAction::CopyMessage => {
                    // The keymap gate guarantees the transcript is focused, but
                    // resolve the message defensively rather than assume it: a
                    // miss is a no-op. Copy goes through the same OSC 52 path
                    // the mouse select-to-copy uses.
                    if let Some(text) = transcript_for_actions.borrow().focused_message_text() {
                        ctx.copy_to_clipboard(text);
                    }
                }
                AjAction::CancelTurn
                | AjAction::Steer
                | AjAction::Dequeue
                | AjAction::PasteImage
                | AjAction::HistoryOpen
                | AjAction::AgentPickerOpen => {
                    *action_slot.borrow_mut() = Some(*action);
                }
            })
        };
        // The `/`-at-empty-prompt palette trigger. The editor swallows the
        // `/` and fires this instead of inserting it, opening the same
        // palette the global `PaletteOpen` chord (Ctrl+O) opens. The two
        // never double-fire: `/` reaches the editor at target/bubble while
        // the chord matches in the controller's capture phase, and the
        // editor only fires the trigger for a lone `/`.
        {
            let overlays_c = Rc::clone(&overlays);
            let editor_widget: WidgetRef = to_widget_ref(Rc::clone(&editor));
            let chrome_c = Rc::clone(&chrome);
            let command_slot_c = Rc::clone(&command_slot);
            let fetch_slot_c = Rc::clone(&fetch_slot);
            editor.borrow_mut().on_palette_trigger = Some(Box::new(move |ctx| {
                open_palette(
                    &overlays_c,
                    &editor_widget,
                    &chrome_c,
                    &command_slot_c,
                    &fetch_slot_c,
                    ctx,
                );
            }));
        }
        // Wire the transcript's Esc-to-exit callback to move focus back to the
        // editor. The resulting `FocusOut` clears the item cursor, exiting
        // transcript-focus mode (Spec E section 1).
        {
            let editor_widget: WidgetRef = to_widget_ref(Rc::clone(&editor));
            transcript
                .borrow_mut()
                .set_on_exit_focus(Box::new(move |ctx| {
                    ctx.request_focus(Rc::clone(&editor_widget));
                    ctx.redraw = true;
                }));
        }
        let keymap =
            KeymapController::new(build_keymap(), Rc::clone(&keymap_ctx), layout, on_action);

        Shell {
            keymap,
            keymap_ctx,
            editor,
            status_line,
            quit_hint,
            quit_hint_warning,
            submitted,
            host_action,
            overlays,
            scrim: Rc::new(RefCell::new(Scrim)),
            command_slot,
            fetch_slot,
            theme,
            chrome,
            transcript,
            splash,
            pending,
            footer,
            selector_activity,
            settings_ui,
            picker_outcome,
            history_fetch,
            skills_fill,
            recall_slot,
            header: header_line,
            session_scan,
            session_request,
            auth_request,
        }
    }

    /// Collect a submit parked by the editor callback, if any.
    fn take_submitted(&self) -> Option<String> {
        self.submitted.borrow_mut().take()
    }

    /// Collect a keymap action parked for the host loop, if any.
    fn take_host_action(&self) -> Option<AjAction> {
        self.host_action.borrow_mut().take()
    }

    /// Collect a host-applied command parked by the palette, if any.
    fn take_command(&self) -> Option<CommandAction> {
        self.command_slot.borrow_mut().take()
    }

    /// Collect an async-overlay fetch request parked by the palette, if any.
    fn take_fetch(&self) -> Option<PendingFetch> {
        self.fetch_slot.borrow_mut().take()
    }

    /// Drain the confirmed config edits parked by the selector and settings
    /// overlays, for the drive loop to apply. Empty when nothing was edited.
    fn take_activity(&self) -> Vec<SelectorActivity> {
        std::mem::take(&mut self.selector_activity.borrow_mut())
    }

    /// Collect an agent-picker outcome parked by the widget, if any.
    fn take_picker_outcome(&self) -> Option<AgentPickerOutcome> {
        self.picker_outcome.borrow_mut().take()
    }

    /// Collect a prompt-history scan request parked by the overlay, if any.
    fn take_history_fetch(&self) -> Option<HistoryFetch> {
        self.history_fetch.borrow_mut().take()
    }

    /// Collect the skills window's fill handle parked on open, if any. The
    /// drive loop drains it to kick off discovery and remember the list to
    /// fill.
    fn take_skills_fetch(&self) -> Option<SkillsFill> {
        self.skills_fill.borrow_mut().take()
    }

    /// Collect a recalled prompt parked by the prompt-history overlay, if any.
    fn take_recall(&self) -> Option<String> {
        self.recall_slot.borrow_mut().take()
    }

    /// Collect the session-preview scan request parked by the selector, if any.
    fn take_session_scan(&self) -> Option<SessionScan> {
        self.session_scan.borrow_mut().take()
    }

    /// Collect a parked session change (new session or a confirmed resume),
    /// if any. The drive loop turns a `Some` into the matching
    /// [`SessionExit`].
    fn take_session_request(&self) -> Option<SessionRequest> {
        self.session_request.borrow_mut().take()
    }

    /// Collect a confirmed login/logout provider pick parked by the auth
    /// picker, if any.
    fn take_auth_request(&self) -> Option<AuthPickerRequest> {
        self.auth_request.borrow_mut().take()
    }

    /// The shared handles the drive loop needs to open a config-editing
    /// overlay: the stack it pushes onto, the editor (focus fallback), a live
    /// chrome snapshot, and the activity / settings-window / picker /
    /// history / recall slots.
    fn overlay_handles(&self) -> OverlayHandles {
        OverlayHandles {
            stack: Rc::clone(&self.overlays),
            editor: to_widget_ref(Rc::clone(&self.editor)),
            chrome: self.chrome.borrow().clone(),
            activity: Rc::clone(&self.selector_activity),
            settings_ui: Rc::clone(&self.settings_ui),
            picker_outcome: Rc::clone(&self.picker_outcome),
            history_fetch: Rc::clone(&self.history_fetch),
            skills_fill: Rc::clone(&self.skills_fill),
            recall_slot: Rc::clone(&self.recall_slot),
            session_scan: Rc::clone(&self.session_scan),
            session_request: Rc::clone(&self.session_request),
            auth_request: Rc::clone(&self.auth_request),
        }
    }

    /// Rebuild every style struct from the current theme, for a runtime
    /// swap (hot-reload, or the settings window's theme row). Every
    /// palette-consuming widget is rebuilt in place, so editor text and
    /// transcript scroll survive the swap. An open settings window is
    /// re-tinted live (its list band and its window chrome); other overlays
    /// opened before the swap keep their baked styles until reopened.
    fn restyle(&self) {
        let t = self.theme.read();
        let styles = Rc::new(TranscriptStyles::from_theme(&t));
        self.transcript.borrow_mut().set_styles(Rc::clone(&styles));
        self.status_line.borrow_mut().set_styles(Rc::clone(&styles));
        self.quit_hint.borrow_mut().set_styles(Rc::clone(&styles));
        self.splash
            .borrow_mut()
            .set_styles(Rc::clone(&styles), t.color_mode());
        self.pending.borrow_mut().set_styles(Rc::clone(&styles));
        self.footer.borrow_mut().set_styles(styles);
        self.editor
            .borrow_mut()
            .set_theme(editor_theme_from_theme(&t));
        let chrome = OverlayChrome::from_theme(&t);
        if let Some(ui) = self.settings_ui.borrow().as_ref() {
            ui.restyle(&chrome);
        }
        *self.chrome.borrow_mut() = chrome;
    }

    /// Recompute the editor's visible-row cap for a `terminal_rows`-tall frame
    /// and apply it. The layout owns the height budget (see [`editor_row_cap`]):
    /// called once at startup and again on every resize so the editor's growth
    /// ceiling tracks the terminal height.
    fn set_editor_row_cap(&self, terminal_rows: usize) {
        self.editor
            .borrow_mut()
            .set_max_visible_rows(Some(editor_row_cap(terminal_rows)));
    }

    /// Repoint the session-scoped handles a replace-contents swap can't
    /// reach onto the freshly built `world`, and reset the transcript to a
    /// fresh-session view.
    ///
    /// The `chat` and `status` cells are shared by identity across sessions
    /// (the outer loop overwrites their contents in place), so the chrome
    /// widgets and the keymap's dispatch closure keep pointing at the live
    /// model with nothing to do here. Two handles do need repointing: the
    /// pending box's message queues, because `SessionCore::build` mints
    /// fresh queues wired into the new agent and the old clone would observe
    /// a detached queue, and the header id. We also drop the transcript back
    /// to follow-tail so the next session opens pinned to the bottom.
    ///
    /// NOTE: the root `Shell` instance and the `AsyncApp` are deliberately
    /// left untouched: the app's mouse/focus handlers hold the root Shell Rc
    /// captured at `init`, so rebuilding the root or re-initializing the app
    /// would strand them. We swap the Shell's innards, never the Shell.
    fn rebind(&self, world: &World) {
        self.pending
            .borrow_mut()
            .set_queues(world.core.message_queues.clone());
        self.header.borrow_mut().text = format!("aj-next — {}", world.core.session_id);
        self.transcript.borrow_mut().reset_to_tail();
    }
}

impl Widget for Shell {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // Draw through the keymap controller so its identity sits on
        // the focus path between the Shell and everything below. The
        // scrim and the top overlay are appended to the controller's
        // surface, not the Shell's, for the same reason: the
        // controller must be an ancestor of an open overlay so its
        // capture chords (close-all) run before the overlay's widgets
        // see the key.
        let mut inner = draw_widget(&to_widget_ref(Rc::clone(&self.keymap)), ctx);

        // Autocomplete popup overlay. We float the editor's popup as a
        // z-indexed subsurface OVER the transcript, anchored just above the
        // editor, rather than drawing it inside the editor's own surface. That
        // keeps every widget's height fixed: opening or closing the popup never
        // pushes the transcript up and never moves the input line or the
        // footer. The popup simply covers the transcript rows it overlaps and
        // uncovers them, unchanged, when it closes.
        //
        // We place it only when no modal overlay is open. A modal and the
        // editor popup are mutually exclusive: under a modal the editor is not
        // focused and cannot be driving a completion. That guard is also why
        // there is no z clash with the scrim (z 1) and modal (z 2) pushes
        // below, which run only when a modal IS open.
        if self.overlays.borrow().top().is_none() {
            let term = ctx.max.size();
            let editor = self.editor.borrow();
            // The editor sits directly above the footer in the `FlexColumn`,
            // and only the footer is below it, so the editor's top row is the
            // terminal height minus the footer and the editor's own block.
            let editor_top = term
                .height
                .saturating_sub(FOOTER_ROWS)
                .saturating_sub(editor.drawn_height());
            // Rows available above the editor, keeping the header on screen.
            let max_rows = usize::from(editor_top.saturating_sub(HEADER_ROWS));
            if let Some(popup) = editor.draw_autocomplete_popup_surface(term.width, max_rows) {
                // Anchor so the popup's bottom edge abuts the editor's top.
                let anchor = editor_top.saturating_sub(popup.size.height);
                inner.children.push(SubSurface {
                    origin: RelativePoint {
                        row: i32::from(anchor),
                        col: 0,
                    },
                    surface: popup,
                    // z 1 draws over the base `FlexColumn` (z 0), like the scrim.
                    z_index: 1,
                });
            }
        }

        // Ctrl+C quit-arm hint. While the quit sequence is armed (the first
        // Ctrl+C landed, the second is pending), float a small box above the
        // editor, flush to the right edge, spelling out the ladder. Read live
        // from the keymap so the box appears and clears with the armed state,
        // no mirror. Suppressed under a modal, where a quit never arms anyway.
        //
        // The keymap's only sequence is the ctrl+c/ctrl+c quit chord, so a
        // pending sequence is exactly this armed state. Safe to borrow the
        // keymap here: `draw_widget` above already released its mutable borrow.
        let quit_armed = self.keymap.borrow().pending_sequence().is_some();
        if quit_armed && self.overlays.borrow().top().is_none() {
            let term = ctx.max.size();
            let editor_top = term
                .height
                .saturating_sub(FOOTER_ROWS)
                .saturating_sub(self.editor.borrow().drawn_height());
            // Bound the box to the room above the editor, keeping the header
            // row on screen. `QuitHint::draw` returns `None` when it can't fit.
            let avail = Size {
                width: term.width,
                height: editor_top.saturating_sub(HEADER_ROWS),
            };
            if let Some(hint) = self.quit_hint.borrow().draw(ctx, avail) {
                let anchor_row = editor_top.saturating_sub(hint.size.height);
                let anchor_col = term.width.saturating_sub(hint.size.width);
                inner.children.push(SubSurface {
                    origin: RelativePoint {
                        row: i32::from(anchor_row),
                        col: i32::from(anchor_col),
                    },
                    surface: hint,
                    // z 1 draws over the base `FlexColumn` (z 0), like the popup.
                    z_index: 1,
                });
            }
        }

        let overlays = self.overlays.borrow();
        if let Some(top) = overlays.top() {
            let term = ctx.max.size();
            // The scrim above the base layout (z 1), the top overlay above
            // the scrim (z 2). Only the top of the stack is drawn: a pushed
            // child hides its parent, the scrim provides the backdrop.
            let scrim_ctx = ctx.with_constraints(
                Size {
                    width: 0,
                    height: 0,
                },
                MaxSize::from_size(term),
            );
            inner.children.push(SubSurface {
                origin: RelativePoint { row: 0, col: 0 },
                surface: draw_widget(&to_widget_ref(Rc::clone(&self.scrim)), &scrim_ctx),
                z_index: 1,
            });
            let (origin, size) = top.placement.resolve(term);
            let overlay_ctx = ctx.with_constraints(
                Size {
                    width: 0,
                    height: 0,
                },
                MaxSize::from_size(size),
            );
            inner.children.push(SubSurface {
                origin,
                surface: draw_widget(&top.widget, &overlay_ctx),
                z_index: 2,
            });
        }
        // Wrap the controller's surface instead of returning it: the
        // caller's draw_widget re-stamps whatever we return with the
        // Shell's identity, which would erase the controller's stamp
        // and drop it from the focus path.
        Surface {
            size: inner.size,
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children: vec![SubSurface {
                origin: RelativePoint { row: 0, col: 0 },
                surface: inner,
                z_index: 0,
            }],
        }
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        // Host-posted app events target the focused widget, but they're
        // meant for the Shell chrome. The Shell is the root of every focus
        // path, so it forwards them from the capturing phase without
        // consuming.
        if let Event::App(user) = event {
            if user.name == REFOCUS_OVERLAY_EVENT {
                // An overlay was opened from the drive loop, which has no
                // event context of its own. Move focus onto the top overlay
                // (or back to the editor when the stack is somehow empty).
                let target = self
                    .overlays
                    .borrow()
                    .top()
                    .map(|o| Rc::clone(&o.focus))
                    .unwrap_or_else(|| to_widget_ref(Rc::clone(&self.editor)));
                ctx.request_focus(target);
                ctx.redraw = true;
            } else {
                // The loader and the splash both animate off host-posted
                // wakes and neither sits on the focus path, so the Shell (the
                // focus-path root) forwards App events to both. Each ignores
                // the wake meant for the other.
                self.status_line.borrow_mut().handle_event(ctx, event);
                self.splash.borrow_mut().handle_event(ctx, event);
            }
        }
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

/// Whether the viewed agent is busy from the host's perspective (a
/// binary-driven turn in `turn_cancels`, or a running initial
/// sub-agent spawn), mirrored into the keymap context. Called from the
/// drive loop's per-iteration sync point, its single writer.
fn sync_keymap_ctx(world: &World, shell: &Rc<RefCell<Shell>>) {
    let active = world.chat.borrow().active_view();
    let busy = world.turn_cancels.contains_key(&active) || world.core.is_running(active);
    shell.borrow().keymap_ctx.borrow_mut().turn_running = busy;
}

/// Runs the interactive shell until the user quits.
///
/// Restores the terminal via [`AsyncApp::shutdown`] on the way out,
/// then prints the usage banner and resume hint to the normal screen
/// (Spec E section 7). The driver's futures are `!Send`, so this must
/// run on a top-level `block_on` (the `#[tokio::main]` future), not a
/// spawned task.
pub async fn run(args: Args) -> Result<()> {
    // Resolve the launch positionals (`aj-next <msg>` / `continue <id>
    // <msg>`, plus `@file` attachments) into the content to auto-submit.
    // We resolve here, before any terminal setup, so a missing `@file`
    // aborts via `?` while the terminal is still in its normal state
    // rather than leaving the user stranded on the alt screen.
    let launch_content =
        aj_app::cli::initial_input(&args, &std::env::current_dir().unwrap_or_default())?
            .into_content();

    // Configuration mirrors `aj`: user config overlaid with the
    // per-project layer, CLI > env > config precedence downstream. The
    // layers are kept editable behind [`ConfigLayers`] so the settings
    // windows can mutate one layer and persist its file.
    let (user_config, user_diagnostics) = Config::load();
    let (project_layer, project_diagnostics) = Config::load_project();
    let mut diagnostics = user_diagnostics;
    diagnostics.extend(project_diagnostics);
    let layers = ConfigLayers {
        user: user_config,
        project: project_layer,
        project_path: Config::project_config_file_path(),
    };

    let auth = AuthStorage::at_default_path().context("failed to open ~/.aj/auth.json")?;
    let sessions_dir = Config::get_sessions_dir_path()?;
    let persistence = ConversationPersistence::new(sessions_dir);

    let mut world = build_world(&args, layers, &diagnostics, &auth, &persistence).await?;

    // Auto-submit the launch prompt as the initial session's first turn.
    // This sits before the outer session loop below, so an in-process
    // session switch rebuilds the world but never resubmits, matching `aj`.
    auto_submit_launch(&mut world, launch_content);

    // Resolve the configured theme (default `light`, matching `aj`) and
    // load it at the env-detected color mode. `AsyncApp::init` runs the
    // async DA1 probe, so the true-color capability isn't known until
    // after init; we reconcile the mode below once it is. Building the
    // theme now with `ColorMode::detect` is the documented fallback for
    // "theme built before init".
    let theme_name = resolve_theme_name(world_config_theme(&world).as_deref()).to_string();
    let env_mode = ColorMode::detect();
    let theme = ThemeHandle::new(Theme::load_with_mode(&theme_name, env_mode));
    let header = format!("aj-next — {}", world.core.session_id);
    let cwd = world.core.env.working_directory.clone();
    let shell = Rc::new(RefCell::new(Shell::new(
        Rc::clone(&world.chat),
        Rc::clone(&world.status),
        world.core.message_queues.clone(),
        theme.clone(),
        header,
        cwd,
    )));
    let root: WidgetRef = to_widget_ref(Rc::clone(&shell));

    let tty = PosixTty::new()?;
    let reader = tty.open_reader()?;
    let mut app = AsyncApp::new(Vaxis::new(VaxisOptions::default()), Box::new(tty), reader);
    app.init(Rc::clone(&root), Options::default()).await?;

    // Reconcile the color mode against the terminal's probed capability.
    // A positive `caps.rgb` (the terminal affirmed truecolor during the
    // init probe) upgrades an env guess of Color256, but a negative probe
    // never downgrades the env guess: most terminals don't answer the
    // truecolor query at all, and the env heuristic is the better signal
    // for them. When the mode actually changes we reload the theme and
    // re-style every widget through the same path a hot-reload uses.
    let probed_mode = if app.vaxis().caps.rgb {
        ColorMode::Truecolor
    } else {
        env_mode
    };
    if probed_mode != theme.color_mode() {
        theme.replace(Theme::load_with_mode(&theme_name, probed_mode));
        shell.borrow().restyle();
        app.request_redraw();
    }

    // Hot-reload watcher for a user theme (bundled names have no on-disk
    // source, so this is inert for `dark` / `light` with no override).
    let mut theme_watch = ThemeWatch::install(&theme_name);

    // Seed the editor's visible-row cap from the startup frame height. The
    // layout owns the editor's height budget; resizes recompute it inside
    // `drive`.
    shell
        .borrow()
        .set_editor_row_cap(usize::from(app.vaxis().window().height));

    // Bootstrap the editor's prompt-history ring from the workspace's session
    // logs. The scan runs off the loop (blocking IO) and `drive`'s seed arm
    // installs the result once it lands, so a large backlog never delays first
    // paint. Spawned once here, not per session: the editor persists across
    // session switches, so re-seeding would duplicate entries. `drive` clears
    // the receiver after the one delivery, making its arm a no-op thereafter.
    let mut prompt_history_rx = Some(spawn_prompt_history_bootstrap(persistence.clone()));

    // Take the editor's autocomplete delivery receiver once, before the
    // session loop. The widget owns the async pipeline (it spawns the query
    // task, staleness-guards deliveries by request id and buffer snapshot, and
    // cancels on new input or dismiss). The host just drains this receiver in
    // `drive` and redraws. The editor, and thus its delivery sender, persists
    // across session rebinds, so the receiver is taken once here and threaded
    // through every `drive` call.
    let mut autocomplete_rx = shell
        .borrow()
        .editor
        .borrow_mut()
        .take_autocomplete_rx()
        .expect("editor hands out its autocomplete receiver exactly once");

    // Outer session loop. Each iteration drives one session to completion;
    // a new-session or resume request exits `drive` with the matching
    // `SessionExit`, whereupon we tear the outgoing session down and build
    // the next one over the same Shell (see `install_next_session`). Quit
    // and fatal errors break out. Usage of each torn-down session is
    // snapshotted for the shutdown banner so a multi-session process
    // itemizes every session, matching `aj`.
    let mut completed_sessions: Vec<(String, UsageSummary)> = Vec::new();
    // Whether a live session survived the loop. A fatal build failure (both
    // the requested build and its previous-session fallback failed) leaves
    // `world` pointing at the already-torn-down outgoing session, whose
    // usage was snapshotted into `completed_sessions` just above the build.
    // The banner then prints that list alone and skips the live block, so
    // the outgoing session isn't counted twice.
    let mut live_survived = true;
    let run_result: Result<()> = loop {
        // Restore the terminal even when the loop exits with a render error,
        // otherwise the user is left stuck on the alt screen.
        let exit = drive(
            &mut app,
            &root,
            &shell,
            &mut world,
            &mut theme_watch,
            &mut prompt_history_rx,
            &mut autocomplete_rx,
        )
        .await;

        // Wind down the outgoing session's work on every exit path (quit,
        // fatal, or switch): kill the background-task tree before tearing
        // down turns so detached process groups are killed and reaped, so
        // an abandoned session never leaks tasks. A session change is only
        // requested with no turn in flight, so the turn shutdown is a no-op
        // there.
        aj_app::shutdown_background_tasks(&world.core.task_registry).await;
        world.turns.shutdown().await;

        let spec = match exit {
            Ok(SessionExit::Quit) => break Ok(()),
            Err(fatal) => break Err(fatal),
            Ok(SessionExit::New) => SessionSpec::Create {
                entry: SessionEntry::Switch,
            },
            Ok(SessionExit::Switch(session_id)) => SessionSpec::Resume {
                session_id,
                entry: SessionEntry::Switch,
            },
        };

        // Snapshot the outgoing session's usage for the banner before we
        // rebuild over it. The replacement session's usage starts at zero,
        // so nothing is double-counted (including on the fallback path,
        // which resumes the same session in a fresh world).
        let usage = world.core.usage_summary().await;
        completed_sessions.push((world.core.session_id.clone(), usage));
        let previous_id = world.core.session_id.clone();

        match build_next_session(&world, spec, &previous_id).await {
            Ok(next) => {
                install_next_session(&mut world, &shell, next);
                app.request_redraw();
            }
            // Both the requested build and the fallback failed: no session
            // survived, so there is nothing to install. Break with the error
            // and let the banner itemize what the completed sessions hold.
            Err(err) => {
                live_survived = false;
                break Err(err);
            }
        }
    };

    app.shutdown().await;

    // The alt screen wiped the conversation from the terminal, so the
    // normal screen gets the usage banner and the resume hint.
    print_exit_banner(&world, &completed_sessions, live_survived).await;
    run_result
}

/// The configured theme name, if any, from the world's config layer.
fn world_config_theme(world: &World) -> Option<String> {
    world
        .config
        .lock()
        .expect("config mutex poisoned")
        .theme
        .clone()
}

/// Resolve the startup theme name from `config.theme`. An unset key
/// defaults to `light` (matching `aj`'s interactive default); an
/// explicit name passes through. A failed load of that name is a
/// separate concern handled by [`Theme::load_with_mode`], which falls
/// back to the bundled `dark` palette.
fn resolve_theme_name(configured: Option<&str>) -> &str {
    configured.unwrap_or("light")
}

/// Owns the theme fs-watcher for the session: the notify guard (held
/// for its `Drop`) plus the receiver the drive loop's reload arm polls.
struct ThemeWatch {
    /// Kept alive so the watcher runs; dropping it stops the watcher.
    /// Never read.
    _guard: Option<ThemeWatcherGuard>,
    rx: Option<UnboundedReceiver<Theme>>,
}

impl ThemeWatch {
    /// Install a watcher for `name`. Only user-supplied themes get one;
    /// bundled `dark` / `light` palettes live in the binary with no
    /// on-disk source. [`watch_user_theme`] short-circuits on a missing
    /// file or an unavailable notify backend, leaving both fields `None`
    /// so the reload arm is inert.
    fn install(name: &str) -> ThemeWatch {
        match watch_user_theme(name) {
            Some((guard, rx)) => ThemeWatch {
                _guard: Some(guard),
                rx: Some(rx),
            },
            None => ThemeWatch {
                _guard: None,
                rx: None,
            },
        }
    }
}

/// Pull one reparsed [`Theme`] off the watcher channel. When no watcher
/// is active the future pends forever, so the `select!` arm is a no-op.
async fn recv_theme(rx: Option<&mut UnboundedReceiver<Theme>>) -> Option<Theme> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The host select loop for one session: turn joins, agent events,
/// terminal input, widget timers, theme reloads, and async overlay fills.
///
/// Returns the reason the session ended: `Quit` when the user quits or
/// input ends, `New` / `Switch(id)` when a session change is requested (only
/// ever with no turn in flight). The outer loop in [`run`] tears the session
/// down and, for a change, builds the next one over the same Shell.
/// Upper bound on interactive redraws per second. The drive loop paces
/// `render_if_needed` to this rate so a fast redraw source (a streaming turn's
/// `MessageUpdate`s, say) cannot drive more than one full-tree relayout and
/// paint per frame budget. 60 keeps the UI smooth while capping the redundant
/// paints a burst would otherwise trigger.
const REDRAW_FPS_CAP: u32 = 60;

/// Whether the frame budget has elapsed since the last paint. A `None` last
/// paint (nothing painted yet this loop) always counts as elapsed, so the
/// first pending redraw paints immediately rather than waiting out a budget
/// measured from an arbitrary start point.
fn frame_budget_elapsed(last_render: Option<Instant>, interval: Duration) -> bool {
    last_render.is_none_or(|t| t.elapsed() >= interval)
}

/// The earlier of two optional deadlines, or whichever one is set.
fn earliest_deadline(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

async fn drive(
    app: &mut AsyncApp,
    root: &WidgetRef,
    shell: &Rc<RefCell<Shell>>,
    world: &mut World,
    theme_watch: &mut ThemeWatch,
    prompt_history_rx: &mut Option<UnboundedReceiver<Vec<String>>>,
    autocomplete_rx: &mut UnboundedReceiver<AutocompleteDelivery>,
) -> Result<SessionExit> {
    // Rising-edge tracker for the loader's animation: the tick chain
    // is armed once per idle-to-busy transition, not per iteration.
    let mut was_busy = false;
    // Edge tracker for the quit-arm hint's warning: the keymap's only
    // sequence is the ctrl+c/ctrl+c quit chord, so a pending sequence means the
    // quit is armed. We refresh the hint's running-work warning on each edge
    // (set it on arm, clear it on disarm).
    let mut quit_was_armed = false;
    // Async read-only overlay fills. The list handle is `!Send`, so it
    // stays here (paired with its `FetchKind`) while the detached fetch
    // sends only the rendered rows back over the channel.
    let (fetch_tx, mut fetch_rx) = unbounded_channel::<(FetchKind, Vec<Row>)>();
    let mut pending_fills: Vec<(FetchKind, Rc<RefCell<ListView>>)> = Vec::new();
    // Prompt-history scans stream their per-file entry batches here. The
    // select handle is `!Send`, so it stays paired with the scan's own
    // receiver on the host side (`pending_history`) while the blocking scan
    // sends only the (Send) entries back. Each open or scope toggle gets a
    // fresh channel, so a superseded scan's batches land on a dropped
    // receiver and never touch the current view.
    let mut pending_history: Option<HistoryFill> = None;
    // Session-selector preview scans stream their per-file preview batches
    // here. The `SessionScan` (holding the `!Send` select) stays on the host
    // side in `pending_session` while the blocking scan sends only the
    // (Send) previews back.
    let mut pending_session: Option<SessionFill> = None;
    // Skills discovery for the skills window runs off the loop. The walk reads
    // `SKILL.md` files (blocking IO); only the discovered skills (Send) come
    // back. The window is already open (with a loading placeholder) and its
    // `!Send` list handle stays here in `pending_skills`, so the fill arm below
    // targets that captured handle rather than the stack's `top()`.
    let (skills_tx, mut skills_rx) = unbounded_channel::<Vec<Skill>>();
    let mut pending_skills: Option<SkillsFill> = None;
    // Session HTML export renders and writes off the loop, delivering only its
    // result notice string back here.
    let (export_tx, mut export_rx) = unbounded_channel::<String>();
    // Shared UI redraw pings from off-thread work. Widgets that spawn
    // their own tasks (the OAuth login, the usage overlay's fetch and
    // consume) run on tokio, off this `!Send` thread, so they can't call
    // `request_redraw` themselves. Each ping they send turns into one
    // repaint via the select arm below. The sender lives for the whole
    // loop, so the receiver never observes a close.
    let (redraw_tx, mut redraw_rx) = unbounded_channel::<()>();
    // The in-flight OAuth login, if any. Tracked here (not on the overlay
    // stack) because it is async and long-running, but paired with the
    // dialog overlay it pushed.
    let mut login_session: Option<LoginSession> = None;
    // Frame pacing: cap redraws at `REDRAW_FPS_CAP`. Requests that arrive
    // within a frame budget coalesce into one paint (the redraw latch is a
    // single bool), and a request landing inside the current budget is
    // deferred, not dropped: the merged `deadline` below wakes the loop at the
    // budget expiry so the coalesced paint lands then.
    let frame_interval = Duration::from_secs(1) / REDRAW_FPS_CAP;
    // `None` means nothing has painted in this loop yet, so the first pending
    // redraw paints without waiting.
    let mut last_render: Option<Instant> = None;
    // Kick the splash animation for this session's empty state. Widgets can
    // only schedule ticks from an event handler, so the host posts the wake
    // and the Shell forwards it to the splash (see `Shell::capture_event`),
    // mirroring the loader's busy-edge wake. A session that opens with a
    // populated transcript shows it instead, so the splash's first tick finds
    // it hidden and the chain stops at once.
    let _ = app.post_app_event(UserEvent {
        name: SPLASH_WAKE_EVENT.to_string(),
        data: None,
    });
    let exit = loop {
        // Paint current state before blocking on the next event, subject to
        // the frame cap: whenever the loop is about to wait, the screen must
        // already reflect current state. This flushes redraws requested while
        // reacting to the previous event (the arms below and the post-select
        // sync block), and, crucially, state changed before we entered the
        // loop with nothing else to wake us: a session the outer loop just
        // rebuilt, or the startup color-mode reconcile. When a redraw is
        // pending but the frame budget has not elapsed, we leave the latch set
        // and let the frame arm of the select below wake us at budget expiry.
        // We paint only when the latch is set, so a clean iteration and
        // `init`'s already-drawn first frame never repaint. Cross-thread wakers
        // still ping their own channels (e.g. the login redraw ping) to get us
        // back here.
        if app.needs_redraw() && frame_budget_elapsed(last_render, frame_interval) {
            app.render(root)?;
            last_render = Some(Instant::now());
        }
        // Compute the wake deadline before the select so no arm holds a borrow
        // of `app` another arm needs. It merges the soonest widget tick with
        // the budget expiry of a redraw the cap is holding back, so a paced
        // redraw is never stranded waiting for an unrelated event. The sleep
        // expression is evaluated even when the guard is false, hence the
        // fallback.
        let tick_deadline = app.next_tick_deadline();
        let frame_deadline = app
            .needs_redraw()
            .then(|| last_render.map_or_else(Instant::now, |t| t + frame_interval));
        let deadline = earliest_deadline(tick_deadline, frame_deadline);
        tokio::select! {
            biased;

            // --- Agent turn finished ---
            joined = join_next_or_pending(&mut world.turns) => {
                handle_turn_join(world, joined)?;
                app.request_redraw();
            }

            // --- Theme reload (fs-watcher) ---
            // Coalesced re-parses of `~/.aj/themes/<name>.json` land
            // here. Replacing the handle and re-styling rebuilds every
            // widget's palette in place; a `None` means the watcher was
            // torn down, so we stop polling to avoid spinning.
            maybe_theme = recv_theme(theme_watch.rx.as_mut()) => {
                match maybe_theme {
                    Some(new_theme) => {
                        let name = new_theme.name().to_string();
                        {
                            let shell = shell.borrow();
                            shell.theme.replace(new_theme);
                            shell.restyle();
                        }
                        fold_notice(world, &format!("Theme '{name}' reloaded."));
                        app.request_redraw();
                    }
                    None => theme_watch.rx = None,
                }
            }

            // --- Prompt-history bootstrap seed ---
            // The one-shot disk scan spawned at startup. `seed_history`
            // splices the disk entries in beneath any prompts submitted this
            // session, so an Up press still reaches the most-recent live
            // submission first. We clear the receiver after any resolution so
            // this arm pends forever afterward (the seed happens once).
            maybe_seed = recv_prompt_history(prompt_history_rx.as_mut()) => {
                if let Some(entries) = maybe_seed {
                    shell.borrow().editor.borrow_mut().seed_history(&entries);
                    app.request_redraw();
                }
                *prompt_history_rx = None;
            }

            // --- Async read-only overlay fill ---
            maybe_fill = fetch_rx.recv() => {
                if let Some((kind, rows)) = maybe_fill
                    && let Some(pos) = pending_fills.iter().position(|(k, _)| *k == kind)
                {
                    let (_, list) = pending_fills.remove(pos);
                    set_rows(&list, rows);
                    app.request_redraw();
                }
            }

            // --- Skills window fill ---
            // Discovery finished off the loop. The window is already open (with
            // a loading placeholder) over the palette, so we fill its captured
            // list handle (`pending_skills`) rather than touching the stack's
            // `top()`. That is what keeps the flow safe. A confirm of another
            // opener from the still-interactive palette can't misdirect it.
            // `fill_skills_window` handles the empty result with a "no skills"
            // placeholder, so the window conveys that itself and the palette
            // stays underneath either way.
            maybe_skills = skills_rx.recv() => {
                if let Some(skills) = maybe_skills
                    && let Some(list) = pending_skills.take()
                {
                    fill_skills_window(&list, skills);
                    app.request_redraw();
                }
            }

            // --- Session export notice fill ---
            // The render + write finished off the loop. Fold its result notice.
            maybe_export = export_rx.recv() => {
                if let Some(notice) = maybe_export {
                    fold_notice(world, &notice);
                    app.request_redraw();
                }
            }

            // --- UI redraw ping ---
            // An off-thread widget task (the login flow pushing a line, the
            // usage overlay's fetch or consume landing) pinged for a
            // repaint. This is the cross-thread redraw wake: the task can't
            // call `request_redraw`, so it sends here.
            maybe_ping = redraw_rx.recv() => {
                if maybe_ping.is_some() {
                    app.request_redraw();
                }
            }

            // --- OAuth login task finished ---
            // Pends forever while no login is in flight. When one is, the
            // handle resolves on success, failure, or abort; `finish_login`
            // closes the dialog and folds the outcome.
            login_outcome = async {
                match login_session.as_mut() {
                    Some(session) => (&mut session.handle).await,
                    None => std::future::pending().await,
                }
            } => {
                finish_login(world, shell, app, &mut login_session, login_outcome);
            }

            // --- Terminal input ---
            event = app.next_input() => {
                match event {
                    Some(event) => {
                        // The layout owns the editor's height budget, so a
                        // terminal resize recomputes the visible-row cap. We
                        // read it off the event before `handle_input` consumes
                        // it and applies the internal resize.
                        if let Event::Winsize(ws) = &event {
                            shell.borrow().set_editor_row_cap(usize::from(ws.rows));
                        }
                        // Every global chord (the ctrl+c ladder, the
                        // toggles, the overlay openers) is matched by
                        // the keymap controller inside this dispatch.
                        // The host only collects what the handlers
                        // parked.
                        if app.handle_input(event).quit {
                            break Ok(SessionExit::Quit);
                        }
                        if let Some(text) = shell.borrow().take_submitted() {
                            // Record the submitted prompt into the editor's
                            // history ring, idle or busy (matching aj). The
                            // text is already trimmed by the editor's submit;
                            // `add_to_history` ignores a whitespace-only or
                            // duplicate entry. In-session submissions stay the
                            // most-recent entries an Up press reaches, with the
                            // disk seed spliced in beneath (see
                            // `spawn_prompt_history_bootstrap`).
                            shell.borrow().editor.borrow_mut().add_to_history(&text);
                            handle_submit(world, text);
                        }
                        if let Some(action) = shell.borrow().take_host_action()
                            && handle_host_action(world, shell, action)
                        {
                            app.request_redraw();
                        }
                        // A palette-confirmed command the host owns
                        // (compact, export, or a config-editing overlay to
                        // open). Bind the take out of the borrow first so no
                        // RefCell ref is held across the await below.
                        let command = shell.borrow().take_command();
                        if let Some(action) = command {
                            match apply_command_action(world, shell, action, &export_tx, &redraw_tx)
                                .await
                            {
                                ActionEffect::OpenedOverlay => {
                                    // The host pushed the overlay ON TOP of
                                    // the still-open palette, which the confirm
                                    // callback left on the stack. The refocus
                                    // event lands on `top()`, the new overlay,
                                    // so cancel from it returns to the palette.
                                    app.post_app_event(UserEvent {
                                        name: REFOCUS_OVERLAY_EVENT.to_string(),
                                        data: None,
                                    });
                                    app.request_redraw();
                                }
                                ActionEffect::None | ActionEffect::Redraw => {
                                    // The command opened no overlay (a pure
                                    // action, or an opener that declined and
                                    // folded a notice), so pop the palette the
                                    // confirm left on the stack and return to
                                    // the transcript. Within this one input
                                    // turn nothing else touched the stack, so
                                    // `top()` is the palette. A `back()` on an
                                    // empty stack is a safe no-op (it never
                                    // runs for the direct-chord entry, which
                                    // returns `OpenedOverlay`). Bind and drop
                                    // the borrow in this statement so no
                                    // RefCell ref outlives it.
                                    shell.borrow().overlays.borrow_mut().back();
                                    // Focus `top()` (now the uncovered parent,
                                    // or the editor when the stack emptied).
                                    app.post_app_event(UserEvent {
                                        name: REFOCUS_OVERLAY_EVENT.to_string(),
                                        data: None,
                                    });
                                    app.request_redraw();
                                }
                            }
                        }
                        // A just-opened async read-only overlay: kick off
                        // its fetch and remember the list to fill.
                        if let Some(fetch) = shell.borrow().take_fetch() {
                            // NOTE: Snapshot the content-column tints at
                            // fetch time. A theme hot-reload while the overlay
                            // is open won't re-tint its rows, the overlay
                            // re-tints on next open. This matches the chrome,
                            // which is also snapshotted at open. Acceptable for
                            // a transient read-only overlay.
                            let styles =
                                ContentStyles::from_theme(&shell.borrow().theme.read());
                            spawn_overlay_fetch(world, fetch.kind, styles, &fetch_tx);
                            pending_fills.push((fetch.kind, fetch.list));
                        }
                        // Config edits parked by a selector or settings
                        // overlay (this event may have confirmed one).
                        let activity = shell.borrow().take_activity();
                        if !activity.is_empty()
                            && apply_selector_activity(world, shell, theme_watch, activity)
                                .await
                        {
                            app.request_redraw();
                        }
                        // An agent-picker outcome (observe / drill into a
                        // task / kill). Opening the task viewer pushes a
                        // child overlay, so it takes the same refocus path
                        // as a host-opened selector.
                        if let Some(outcome) = shell.borrow().take_picker_outcome() {
                            match apply_picker_outcome(world, shell, outcome) {
                                // The picker resolves synchronously (it never
                                // defers a fill), so its outcome is one of
                                // these three. `OpenTask` opens the viewer
                                // overlay; observe/kill just redraw or no-op.
                                ActionEffect::None => {}
                                ActionEffect::Redraw => app.request_redraw(),
                                ActionEffect::OpenedOverlay => {
                                    app.post_app_event(UserEvent {
                                        name: REFOCUS_OVERLAY_EVENT.to_string(),
                                        data: None,
                                    });
                                    app.request_redraw();
                                }
                            }
                        }
                        // A prompt-history recall: drop the chosen text
                        // into the editor (it does not submit).
                        if let Some(text) = shell.borrow().take_recall() {
                            recall_into_editor(shell, &text);
                            app.request_redraw();
                        }
                        // A prompt-history scan request (open or scope
                        // toggle): give it a fresh channel, run the scan off
                        // the loop on a blocking thread, and remember the
                        // select to stream into. A prior in-flight scan's
                        // channel is dropped here, so its batches are ignored.
                        if let Some(fetch) = shell.borrow().take_history_fetch() {
                            let (tx, rx) = unbounded_channel();
                            spawn_history_scan(world, fetch.scope, tx);
                            pending_history = Some(HistoryFill {
                                select: fetch.select,
                                rx,
                                first: true,
                            });
                        }
                        // A just-opened skills window: kick off discovery off
                        // the loop and remember the captured list handle to fill
                        // (never the stack's `top()`). The window is already up
                        // with a loading placeholder; the fill arm streams the
                        // discovered rows in when the walk lands.
                        if let Some(fill) = shell.borrow().take_skills_fetch() {
                            spawn_skills_discovery(world, &skills_tx);
                            pending_skills = Some(fill);
                        }
                        // A session-selector open: give it a fresh channel,
                        // run the preview scan off the loop, and remember the
                        // selector to stream into.
                        if let Some(scan) = shell.borrow().take_session_scan() {
                            let (tx, rx) = unbounded_channel();
                            spawn_session_scan(world, tx);
                            pending_session = Some(SessionFill {
                                scan,
                                rx,
                                first: true,
                                select_current_pending: true,
                                anchor: None,
                            });
                        }
                        // A confirmed login/logout provider pick from the
                        // auth picker. Bind out of the borrow first so no
                        // RefCell ref is held across the await inside.
                        let auth_request = shell.borrow().take_auth_request();
                        if let Some(request) = auth_request {
                            apply_auth_request(
                                world,
                                shell,
                                app,
                                &mut login_session,
                                &redraw_tx,
                                request,
                            )
                            .await;
                        }
                        // Login cancellation: the dialog's Esc/Ctrl+C flipped
                        // the shared flag. Tear the dialog down and abort the
                        // task.
                        cancel_login(world, shell, app, &mut login_session);
                        // A parked session change (the `NewSession` command
                        // or a confirmed resume pick). A change is only ever
                        // requested with no turn in flight, so tearing the
                        // world down can't strand a running turn.
                        if let Some(request) = shell.borrow().take_session_request() {
                            debug_assert!(
                                world.turns.is_empty(),
                                "session change requested mid-turn"
                            );
                            break Ok(request.into_exit());
                        }
                    }
                    // The reader ended (EOF or a read error), so no
                    // further input can arrive.
                    None => break Ok(SessionExit::Quit),
                }
            }

            // --- Agent bus event ---
            // This arm sits BELOW the input arm on purpose. A fast streaming
            // turn floods the agent-event stream, and under `biased` an arm
            // above input would keep winning and starve typing until the turn
            // quiesced, so a typed follow-up or steer would render late. Below
            // input, typed input always wins. `drain_events` still coalesces the
            // whole channel into one batch, so a burst collapses into one redraw.
            maybe_event = world.core.event_rx.recv() => {
                // `None` (channel closed) can't happen while the core
                // holds its forwarder subscription. Treat it as a
                // no-op rather than tearing the session down.
                if let Some(event) = maybe_event {
                    let (redraw, wake_targets) = drain_events(world, event);
                    spawn_wakes(world, wake_targets);
                    if redraw {
                        app.request_redraw();
                    }
                }
            }

            // --- Autocomplete delivery ---
            // A completed one-shot query result or a streaming-session wake
            // from the editor's autocomplete pipeline. The widget spawned the
            // work and applies its own staleness guards inside
            // `apply_autocomplete_delivery`. The host is the single drain
            // point for the delivery channel (`draw` never drains).
            //
            // This arm sits BELOW the input arm on purpose. During a directory
            // walk the streaming session fires its `notify` repeatedly, flooding
            // this channel with `SessionProgressed` wakes. Under `biased`, an arm
            // above input would keep winning and starve typing until the walk
            // quiesced, so keystrokes would render late. Below input, typed input
            // always wins. The popup still catches up via the coalescing drain
            // here and the per-iteration `pump_autocomplete` at the loop bottom.
            Some(delivery) = autocomplete_rx.recv() => {
                {
                    // Coalesce the wake flood: apply the first delivery, then
                    // drain everything else already queued so a burst collapses
                    // into one iteration instead of one iteration per notify. The
                    // delivery kind is opaque to the host, so apply all. A
                    // one-shot Query carries results that must be applied. A
                    // SessionProgressed is only a wake, so applying it is a
                    // no-op, and the per-iteration `pump_autocomplete` at the
                    // loop bottom does the single tick. We hold the editor
                    // borrow across the drain (no await inside) and drop it
                    // before the single redraw below.
                    let shell = shell.borrow();
                    let mut editor = shell.editor.borrow_mut();
                    editor.apply_autocomplete_delivery(delivery);
                    while let Ok(delivery) = autocomplete_rx.try_recv() {
                        editor.apply_autocomplete_delivery(delivery);
                    }
                }
                app.request_redraw();
            }

            // --- Prompt-history scan fill ---
            // Sits BELOW the input arm on purpose. A scan of a large project
            // streams many per-file batches, and under `biased` an arm above
            // input would keep winning and starve typing until the scan
            // finished, so a keystroke in the filter field would render late.
            // Below input, typing always wins and the list still fills between
            // keystrokes.
            //
            // The first batch replaces the loading placeholder, later batches
            // append. Superseded scans (a scope toggle) sit on a dropped
            // channel, so their batches never reach here.
            maybe_history = recv_scan(pending_history.as_mut().map(|f| &mut f.rx)) => {
                if let Some(fill) = pending_history.as_mut() {
                    match maybe_history {
                        Some(ScanMsg::Batch(entries)) => {
                            let items = crate::prompt_history::build_items(&entries);
                            if fill.first {
                                fill.select.borrow().set_items(items);
                                fill.first = false;
                            } else {
                                fill.select.borrow().extend_items(items);
                            }
                            app.request_redraw();
                        }
                        // Done, or the sender was dropped: clear the loading
                        // placeholder if nothing streamed in, then retire the
                        // scan.
                        Some(ScanMsg::Done) | None => {
                            if fill.first {
                                fill.select.borrow().set_items(Vec::new());
                            }
                            pending_history = None;
                            app.request_redraw();
                        }
                    }
                }
            }

            // --- Session-selector preview fill ---
            // Sits BELOW the input arm on purpose, for the same reason as the
            // prompt-history fill above: many streamed batches must not starve
            // typing in the filter field under `biased`.
            //
            // Build the rows and append them (the first batch replaces the
            // loading placeholder), and chase the active session's row until it
            // appears, giving up once the user has navigated so a late batch
            // can't yank the cursor.
            maybe_sessions = recv_scan(pending_session.as_mut().map(|f| &mut f.rx)) => {
                if let Some(fill) = pending_session.as_mut() {
                    match maybe_sessions {
                        Some(ScanMsg::Batch(previews)) => {
                            // If the user moved the selection off where the
                            // chase last parked it, stop chasing so this batch's
                            // pre-select can't yank the cursor back. Skipped on
                            // the first batch, which establishes the anchor.
                            if !fill.first
                                && fill.select_current_pending
                                && fill.scan.selected_filter_key() != fill.anchor
                            {
                                fill.select_current_pending = false;
                            }
                            let selected = extend_session_scan(
                                &fill.scan,
                                &previews,
                                Utc::now(),
                                fill.first,
                                fill.select_current_pending,
                            );
                            fill.first = false;
                            if selected {
                                fill.select_current_pending = false;
                            }
                            // Re-anchor to wherever the selection now sits so
                            // later user movement is measured against it.
                            fill.anchor = fill.scan.selected_filter_key();
                            app.request_redraw();
                        }
                        // Done, or the sender was dropped: clear the loading
                        // placeholder if nothing streamed in, then retire the
                        // scan.
                        Some(ScanMsg::Done) | None => {
                            if fill.first {
                                extend_session_scan(&fill.scan, &[], Utc::now(), true, false);
                            }
                            pending_session = None;
                            app.request_redraw();
                        }
                    }
                }
            }

            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now).into()),
                if deadline.is_some() =>
            {
                // The merged deadline fires for a widget tick, a paced redraw
                // held back by the frame cap, or both. Firing due timers is a
                // no-op when none are due (a pure frame wake), and the paint at
                // the top of the loop flushes the deferred redraw.
                if app.fire_due_timers().quit {
                    break Ok(SessionExit::Quit);
                }
            }
        }
        // One status sync per iteration, whatever the arm did. On the
        // idle-to-busy edge, post the loader wake: widgets can only
        // schedule ticks from an event handler, so the host hands the
        // loader an app event to arm its animation chain (the Shell
        // forwards it, see `Shell::capture_event`).
        let busy = sync_status(world);
        // Advance the editor's autocomplete once per iteration. The delivery
        // arm above wakes the loop as streaming matches and one-shot results
        // land, but a narrowing keystroke re-scores an already-walked tree in
        // place, which need not emit a fresh wake. Pumping here ticks the
        // active session and rebuilds the popup from its latest snapshot. It is
        // a no-op when no session is open. The widget still owns the pipeline,
        // the host just drives the tick from its own loop.
        shell.borrow().editor.borrow_mut().pump_autocomplete();
        sync_keymap_ctx(world, shell);
        // The close-all chord must not pre-empt the login dialog's own
        // Esc/Ctrl+C teardown, so mirror the login liveness into the
        // keymap context. This loop is the field's single writer.
        shell.borrow().keymap_ctx.borrow_mut().login_active = login_session.is_some();
        if busy && !was_busy {
            let _ = app.post_app_event(UserEvent {
                name: STATUS_WAKE_EVENT.to_string(),
                data: None,
            });
        }
        was_busy = busy;
        // Surface the quit arming: the sequence-start is consumed silently by
        // the keymap engine, so on the arming edge the host refreshes the
        // hint's running-work warning (the background work a quit would tear
        // down) and asks for a repaint. The box itself is drawn by the Shell
        // straight from the live keymap state. The engine handles the disarm
        // side (timeout or another key); on that edge we clear the warning so a
        // later arm recomputes it.
        let quit_armed = shell.borrow().keymap.borrow().pending_sequence().is_some();
        if quit_armed != quit_was_armed {
            let warning = quit_armed.then(|| quit_arm_running_work(world)).flatten();
            *shell.borrow().quit_hint_warning.borrow_mut() = warning;
            app.request_redraw();
        }
        quit_was_armed = quit_armed;
    };

    exit
}

/// Print the end-of-session usage banner and resume hint to stdout,
/// dimmed and indented like `aj`'s shutdown banner. Call after the alt
/// screen is torn down and with no turn in flight (reading the agent's
/// usage locks it).
///
/// A single-session process prints one bare usage block. When the process
/// spanned several sessions (new-session / resume), each torn-down
/// session's usage was snapshotted into `completed` in order; itemize them
/// first, each under a dim `Session: <id>` header, then the live session's
/// block, matching `aj`.
///
/// `live_survived` is false only on the fatal build-failure path, where
/// `world` still points at the already-torn-down outgoing session (itself
/// the last entry in `completed`). We then print `completed` alone and skip
/// the live block and the resume hint, so that session isn't counted twice.
async fn print_exit_banner(
    world: &World,
    completed: &[(String, UsageSummary)],
    live_survived: bool,
) {
    fn dim(s: &str) -> String {
        format!("\x1b[2m{s}\x1b[22m")
    }
    fn print_block(header: Option<&str>, summary: &UsageSummary) {
        println!();
        if let Some(header) = header {
            println!(" {}", dim(header));
        }
        for line in format_usage_summary(summary).lines() {
            println!(" {}", dim(line));
        }
        println!();
    }

    // No live world survived the loop: the outgoing session's usage is
    // already in `completed`, so print that list and stop.
    if !live_survived {
        for (session_id, completed_summary) in completed {
            print_block(
                Some(&format_session_usage_header(session_id)),
                completed_summary,
            );
        }
        return;
    }

    let summary = world.core.usage_summary().await;
    if completed.is_empty() {
        print_block(None, &summary);
    } else {
        for (session_id, completed_summary) in completed {
            print_block(
                Some(&format_session_usage_header(session_id)),
                completed_summary,
            );
        }
        print_block(
            Some(&format_session_usage_header(&world.core.session_id)),
            &summary,
        );
    }
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

    use aj_app::chat::{EntryKind, NoticeLevel};
    use clap::Parser;
    use tempfile::TempDir;
    use vaxis::gwidth;
    use vaxis::key::{Key, Modifiers};
    use vaxis::tty::TestTty;
    use vaxis::vxfw::{MaxSize, Size};

    use super::*;

    /// The context strike hook wraps a disabled skill's row in the SGR
    /// strikethrough markers the transcript notice renderer parses back out.
    #[test]
    fn strikethrough_wraps_input_in_sgr_markers() {
        assert_eq!(strikethrough("row"), "\x1b[9mrow\x1b[29m");
    }

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

    fn test_shell_with_chat(chat: Rc<RefCell<ChatState>>) -> Rc<RefCell<Shell>> {
        Rc::new(RefCell::new(Shell::new(
            chat,
            Rc::new(RefCell::new(StatusState::default())),
            MessageQueues::default(),
            ThemeHandle::new(Theme::bundled_dark_with_mode(
                aj_app::theme::ColorMode::Truecolor,
            )),
            "aj-next".to_string(),
            PathBuf::from("/tmp"),
        )))
    }

    /// Builds and initializes an `AsyncApp` over a `TestTty`, with a
    /// pipe as the read source. Keep the returned writer alive or the
    /// reader sees EOF.
    async fn init_app() -> (AsyncApp, PipeWriter, Rc<RefCell<Shell>>, WidgetRef) {
        init_app_with_chat(empty_chat()).await
    }

    async fn init_app_with_chat(
        chat: Rc<RefCell<ChatState>>,
    ) -> (AsyncApp, PipeWriter, Rc<RefCell<Shell>>, WidgetRef) {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        // Answer the DA1 probe up front so init's capability wait
        // returns as soon as the reader consumes the reply instead of
        // after its timeout.
        writer.write_all(b"\x1b[?c").expect("write DA1 reply");

        let shell = test_shell_with_chat(chat);
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

    /// Like [`init_app`], but roots the shell's editor autocomplete provider
    /// at `cwd` so a filesystem-backed `@`-completion flow can be driven end to
    /// end against a temp directory with known files.
    async fn init_app_in_dir(
        cwd: PathBuf,
    ) -> (AsyncApp, PipeWriter, Rc<RefCell<Shell>>, WidgetRef) {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        writer.write_all(b"\x1b[?c").expect("write DA1 reply");
        let shell = Rc::new(RefCell::new(Shell::new(
            empty_chat(),
            Rc::new(RefCell::new(StatusState::default())),
            MessageQueues::default(),
            ThemeHandle::new(Theme::bundled_dark_with_mode(
                aj_app::theme::ColorMode::Truecolor,
            )),
            "aj-next".to_string(),
            cwd,
        )));
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

    /// Feed one byte to the app as the drive loop would, then pump the editor's
    /// autocomplete like the drive loop does once per iteration. Yields and
    /// sleeps a bounded number of times so the streaming walker and nucleo
    /// matcher settle before the caller inspects the popup. Bounded so a stuck
    /// session can never hang the test.
    async fn type_and_settle_autocomplete(
        app: &mut AsyncApp,
        writer: &mut PipeWriter,
        shell: &Rc<RefCell<Shell>>,
        byte: u8,
    ) {
        writer.write_all(&[byte]).expect("write key byte");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        for _ in 0..80 {
            tokio::task::yield_now().await;
            shell.borrow().editor.borrow_mut().pump_autocomplete();
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// A bounded draw context of `width x height` cells, matching the shape the
    /// app builds from the terminal window.
    fn draw_ctx(width: u16, height: u16) -> DrawContext {
        DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(width),
                height: Some(height),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        }
    }

    /// Finds the shell's autocomplete popup overlay in a composed `Shell`
    /// surface, if present. The shell wraps the keymap surface as its single
    /// z-0 child and pushes the popup onto that surface at z 1.
    fn popup_overlay(composed: &Surface) -> Option<&SubSurface> {
        composed.children[0]
            .surface
            .children
            .iter()
            .find(|c| c.z_index == 1)
    }

    /// The editor's autocomplete popup is floated as a z-indexed overlay ABOVE
    /// the editor, so opening it never changes the editor block's height and so
    /// never shrinks the flex transcript or moves the input line and footer.
    /// The popup shrinks to fit the space above the editor on a short terminal.
    #[tokio::test]
    async fn autocomplete_popup_is_an_overlay_above_the_fixed_editor() {
        let tmp = TempDir::new().unwrap();
        // Fifteen files all match the fuzzy query `file`, so the popup wants
        // more rows than a short terminal can give above the editor.
        for n in 0..15 {
            std::fs::write(tmp.path().join(format!("file{n:02}.rs")), "x").expect("write file");
        }
        let (mut app, mut writer, shell, _root) = init_app_in_dir(tmp.path().to_path_buf()).await;

        // Baseline: popup closed. Record the editor block height.
        let ctx = draw_ctx(100, 30);
        let closed = shell.borrow_mut().draw(&ctx);
        let editor_h_closed = shell.borrow().editor.borrow().drawn_height();
        assert!(!shell.borrow().editor.borrow().is_showing_autocomplete());
        assert!(
            popup_overlay(&closed).is_none(),
            "no popup overlay while the popup is closed",
        );

        // Open the popup with a query that matches every file.
        for byte in b"@file" {
            type_and_settle_autocomplete(&mut app, &mut writer, &shell, *byte).await;
        }
        assert!(
            shell.borrow().editor.borrow().is_showing_autocomplete(),
            "typing `@file` opens the file-completion popup",
        );

        // Tall terminal: the editor block height is unchanged by the popup, so
        // the transcript (the only flex child) keeps its size and the input
        // line and footer do not move.
        let composed = shell.borrow_mut().draw(&ctx);
        let editor_h_open = shell.borrow().editor.borrow().drawn_height();
        assert_eq!(
            editor_h_open, editor_h_closed,
            "opening the popup must not change the editor block height",
        );

        let popup = popup_overlay(&composed).expect("a popup overlay floats above the base layout");
        assert_eq!(popup.origin.col, 0);
        assert!(popup.surface.size.height >= 1, "the popup has rows");
        // The editor sits directly above the footer, so its top row is the
        // popup's bottom edge.
        let editor_top = 30 - FOOTER_ROWS - editor_h_open;
        assert_eq!(
            popup.origin.row + i32::from(popup.surface.size.height),
            i32::from(editor_top),
            "the popup's bottom edge abuts the editor's top row",
        );

        // Short terminal: the popup shrinks to the rows above the editor,
        // keeping the header, input line, and footer visible, and nothing
        // panics. Fifteen items would overflow, so the window is clamped.
        let short = draw_ctx(100, 16);
        let composed = shell.borrow_mut().draw(&short);
        let editor_h_short = shell.borrow().editor.borrow().drawn_height();
        let editor_top_short = 16 - FOOTER_ROWS - editor_h_short;
        let popup = popup_overlay(&composed).expect("a popup overlay on the short terminal");
        assert!(
            popup.origin.row >= i32::from(HEADER_ROWS),
            "the popup leaves the header row visible",
        );
        assert_eq!(
            popup.origin.row + i32::from(popup.surface.size.height),
            i32::from(editor_top_short),
            "the shrunk popup still abuts the editor's top row",
        );
        assert!(
            popup.surface.size.height < 15,
            "the popup shrank below the item count to fit the short terminal",
        );
    }

    #[tokio::test]
    async fn at_file_autocomplete_end_to_end_through_the_shell() {
        // End-to-end host wiring: `Shell::new` installs the `@`-file provider
        // rooted at the session cwd, typing `@` opens the streaming popup, the
        // narrowing keystrokes keep it, and Tab applies the fuzzy-matched path.
        // We mirror the drive loop's per-iteration `pump_autocomplete` between
        // keystrokes so the walker and matcher settle. Only `hello.md` matches
        // `hel`, and it is a file, so Tab yields `@hello.md ` (trailing space).
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.md"), "hi").expect("write file");
        std::fs::write(tmp.path().join("notes.txt"), "n").expect("write file");

        let (mut app, mut writer, shell, _root) = init_app_in_dir(tmp.path().to_path_buf()).await;
        assert!(!shell.borrow().editor.borrow().is_showing_autocomplete());

        for byte in b"@hel" {
            type_and_settle_autocomplete(&mut app, &mut writer, &shell, *byte).await;
        }
        assert!(
            shell.borrow().editor.borrow().is_showing_autocomplete(),
            "typing `@hel` opens the file-completion popup",
        );
        // `@` opens the file popup, not the `/`-command palette.
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "the command palette must not open on `@`",
        );

        type_and_settle_autocomplete(&mut app, &mut writer, &shell, b'\t').await;
        assert_eq!(shell.borrow().editor.borrow().text(), "@hello.md ");
        assert!(!shell.borrow().editor.borrow().is_showing_autocomplete());
    }

    /// setup path (`build_initial_run_config` + `SessionCore::build`),
    /// with persistence and auth confined to a tempdir. The config layers
    /// default to empty with no project path (persistence is unavailable).
    async fn scripted_world(dir: &TempDir, demo: &str) -> World {
        scripted_world_with_layers(dir, demo, default_layers()).await
    }

    /// Empty config layers with no project path, for tests that don't
    /// exercise persistence.
    fn default_layers() -> ConfigLayers {
        ConfigLayers {
            user: Config::default(),
            project: aj_conf::ConfigLayer::default(),
            project_path: None,
        }
    }

    async fn scripted_world_with_layers(dir: &TempDir, demo: &str, layers: ConfigLayers) -> World {
        let args = Args::parse_from(["aj-next", "--scripted", demo]);
        let auth = AuthStorage::new(dir.path().join("auth.json"));
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        build_world(&args, layers, &[], &auth, &persistence)
            .await
            .expect("build world")
    }

    /// Drive one scripted turn to completion so the session's log lands on disk
    /// and can be resumed by the session-switch paths.
    async fn persist_session(world: &mut World) {
        handle_submit(world, "persist me".to_string());
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(world, joined).expect("turn settles cleanly");
        if let Ok(first) = world.core.event_rx.try_recv() {
            let _ = drain_events(world, first);
        }
    }

    /// A world resumed from `session_id`, reusing `dir`'s persistence so the
    /// session written by a prior [`scripted_world`] is found on disk.
    async fn resumed_world(dir: &TempDir, demo: &str, session_id: &str) -> World {
        let args = Args::parse_from(["aj-next", "--scripted", demo, "continue", session_id]);
        let auth = AuthStorage::new(dir.path().join("auth.json"));
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        build_world(&args, default_layers(), &[], &auth, &persistence)
            .await
            .expect("build resumed world")
    }

    /// A fresh session folds the context listing as the leading Info notice,
    /// before the startup warnings. That is what makes it the first transcript
    /// message once the chat starts, while the warning-level notices below it
    /// are what the splash box surfaces.
    #[tokio::test]
    #[serial_test::serial]
    async fn build_world_folds_context_as_leading_info_before_warnings() {
        // Force the sandbox warning on so a warning-level notice deterministically
        // follows the context, independent of the ambient environment.
        let prev = std::env::var("AJ_DISABLE_SANDBOX_WARNING").ok();
        // SAFETY: `#[serial]` keeps other env-mutating tests out; restored below.
        unsafe {
            std::env::remove_var("AJ_DISABLE_SANDBOX_WARNING");
        }

        let dir = TempDir::new().expect("tempdir");
        let world = scripted_world(&dir, "streaming-text").await;

        let leading: Vec<(NoticeLevel, String)> = {
            let chat = world.chat.borrow();
            let transcript = chat
                .transcript(chat.active_view())
                .expect("main transcript");
            transcript
                .entries()
                .iter()
                .map_while(|e| match &e.kind {
                    EntryKind::Notice(n) => Some((n.level, n.text.clone())),
                    _ => None,
                })
                .collect()
        };

        // SAFETY: same serial scope as the remove above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AJ_DISABLE_SANDBOX_WARNING", v),
                None => std::env::remove_var("AJ_DISABLE_SANDBOX_WARNING"),
            }
        }

        let context_at = leading
            .iter()
            .position(|(level, text)| *level == NoticeLevel::Info && text.contains("Context:"))
            .expect("a leading Info notice carries the context listing");
        let first_warning = leading
            .iter()
            .position(|(level, _)| *level == NoticeLevel::Warning)
            .expect("a startup warning follows the context");
        assert!(
            context_at < first_warning,
            "context is folded before the warnings: {leading:?}"
        );
    }

    /// A resumed session keeps its assembled prompt in the log, so `build_world`
    /// folds no context notice into its scrollback.
    #[tokio::test]
    async fn build_world_resume_folds_no_context() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;
        persist_session(&mut world).await;
        let resumed = resumed_world(&dir, "streaming-text", &world.core.session_id).await;

        let chat = resumed.chat.borrow();
        let transcript = chat
            .transcript(chat.active_view())
            .expect("main transcript");
        let has_context = transcript
            .entries()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Notice(n) if n.text.contains("Context:")));
        assert!(!has_context, "a resumed session folds no context notice");
    }

    /// A session switch folds the fresh session's context as an Info notice: a
    /// fresh switch carries the context string in its deferred notices, a resume
    /// (and the resume-fallback) does not, and installing a fresh switch folds
    /// the context notice into the new session's scrollback.
    #[tokio::test]
    async fn session_switch_folds_context_for_fresh_only() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let previous_id = world.core.session_id.clone();
        let has_context = |notices: &[String]| notices.iter().any(|n| n.contains("Context:"));

        let fresh = build_next_session(
            &world,
            SessionSpec::Create {
                entry: SessionEntry::Switch,
            },
            &previous_id,
        )
        .await
        .expect("build fresh next session");
        assert!(
            has_context(&fresh.notices),
            "a fresh switch carries context: {:?}",
            fresh.notices
        );

        // Persist the session so the resume paths have a log on disk.
        persist_session(&mut world).await;
        let resumable = world.core.session_id.clone();

        let resumed = build_next_session(
            &world,
            SessionSpec::Resume {
                session_id: resumable.clone(),
                entry: SessionEntry::Switch,
            },
            &previous_id,
        )
        .await
        .expect("build resumed next session");
        assert!(
            !has_context(&resumed.notices),
            "a resume carries no context: {:?}",
            resumed.notices
        );

        // The resume-fallback path: the requested resume of a missing session
        // fails, we fall back to resuming `previous_id`, and that build is
        // never fresh, so it also carries no context.
        let fallback = build_next_session(
            &world,
            SessionSpec::Resume {
                session_id: "no-such-session".to_string(),
                entry: SessionEntry::Switch,
            },
            &resumable,
        )
        .await
        .expect("the fallback resumes the previous session");
        assert!(
            !has_context(&fallback.notices),
            "the resume fallback carries no context: {:?}",
            fallback.notices
        );

        // Installing a fresh switch folds its context notice into the new
        // session's scrollback.
        let fresh = build_next_session(
            &world,
            SessionSpec::Create {
                entry: SessionEntry::Switch,
            },
            &previous_id,
        )
        .await
        .expect("build fresh next session");
        install_next_session(&mut world, &shell, fresh);
        let folded = main_notices(&world);
        assert!(
            has_context(&folded),
            "install folds the fresh context: {folded:?}"
        );
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
        assert_eq!(shell.borrow().editor.borrow().cursor(), (0, 1));
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
        // The editor cleared itself on submit.
        assert_eq!(shell.borrow().editor.borrow().cursor(), (0, 0));
        assert_eq!(shell.borrow().editor.borrow().text(), "");
    }

    /// Shift+Enter inserts a newline into the multi-line editor rather than
    /// submitting: the document grows a line and the text carries the `\n`.
    #[tokio::test]
    async fn shift_enter_inserts_a_newline_in_the_editor() {
        let (_app, _writer, shell, _root) = init_app().await;
        let mut ctx = EventContext::new();
        {
            let shell = shell.borrow();
            let mut editor = shell.editor.borrow_mut();
            editor.insert_at_cursor("line1");
            editor.handle_event(
                &mut ctx,
                &Event::KeyPress(Key {
                    codepoint: Key::ENTER,
                    mods: Modifiers::SHIFT,
                    ..Key::default()
                }),
            );
            editor.insert_at_cursor("line2");
        }
        let editor = shell.borrow().editor.borrow().text();
        assert_eq!(editor, "line1\nline2");
        assert_eq!(shell.borrow().editor.borrow().cursor(), (1, 5));
    }

    /// History up recalls a seeded entry, newest first, without submitting.
    #[tokio::test]
    async fn history_up_recalls_a_seeded_entry() {
        let (_app, _writer, shell, _root) = init_app().await;
        shell
            .borrow()
            .editor
            .borrow_mut()
            .seed_history(&["older".to_string(), "newer".to_string()]);

        let up = || {
            let mut ctx = EventContext::new();
            shell.borrow().editor.borrow_mut().handle_event(
                &mut ctx,
                &Event::KeyPress(Key {
                    codepoint: Key::UP,
                    mods: Modifiers::empty(),
                    ..Key::default()
                }),
            );
        };
        up();
        assert_eq!(shell.borrow().editor.borrow().text(), "newer");
        up();
        assert_eq!(shell.borrow().editor.borrow().text(), "older");
    }

    /// A submitted prompt is recorded into the editor's history ring by the
    /// drive loop's submit path, so a later Up press recalls it. Drives the
    /// real submit through the app so the record site (not a test shortcut)
    /// runs.
    #[tokio::test]
    async fn submit_records_into_history_and_up_recalls_it() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, mut writer, mut world, shell, _root) =
            init_app_with_world(&dir, "streaming-text").await;

        // Type "recall me" and submit with CR. The loop pumps input events
        // until the editor's submit lands.
        writer.write_all(b"recall me\r").expect("write prompt");
        loop {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
            // Mirror the drive loop's submit path: record then spawn.
            if let Some(text) = shell.borrow().take_submitted() {
                shell.borrow().editor.borrow_mut().add_to_history(&text);
                handle_submit(&mut world, text);
                break;
            }
        }
        assert_eq!(shell.borrow().editor.borrow().text(), "");

        // Up recalls the just-submitted prompt.
        let mut ctx = EventContext::new();
        shell.borrow().editor.borrow_mut().handle_event(
            &mut ctx,
            &Event::KeyPress(Key {
                codepoint: Key::UP,
                mods: Modifiers::empty(),
                ..Key::default()
            }),
        );
        assert_eq!(shell.borrow().editor.borrow().text(), "recall me");

        // Settle the turn so world teardown is clean.
        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
    }

    /// The visible-row cap bounds the editor's drawn height: with more content
    /// lines than the cap, the editor draws exactly `cap + 2` rows (the two
    /// border rules) and scrolls the rest.
    #[test]
    fn height_cap_bounds_the_editor_growth() {
        let shell = test_shell_with_chat(empty_chat());
        let cap = 3;
        {
            let shell = shell.borrow();
            let mut editor = shell.editor.borrow_mut();
            editor.set_max_visible_rows(Some(cap));
            editor.insert_at_cursor(
                &(1..=20)
                    .map(|n| format!("line {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(40),
                height: Some(100),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        };
        let surface = shell.borrow().editor.borrow_mut().draw(&ctx);
        assert_eq!(
            surface.size.height,
            u16::try_from(cap + 2).unwrap(),
            "the cap plus the two border rows bound the editor height"
        );
    }

    /// `editor_row_cap` matches aj's `max(5, floor(rows * 0.3))`, with the
    /// floor of 5 on short terminals.
    #[test]
    fn editor_row_cap_matches_the_policy() {
        assert_eq!(editor_row_cap(0), 5);
        assert_eq!(editor_row_cap(24), 7);
        assert_eq!(editor_row_cap(10), 5);
        assert_eq!(editor_row_cap(50), 15);
    }

    /// The editor theme wires the autocomplete popup's selection band so the
    /// selected row reads as a visible band, not plain text. The selected style
    /// must carry a non-default background (the `SelectedBg` band) while the
    /// unselected item keeps the default background.
    #[test]
    fn editor_theme_wires_the_popup_selection_band() {
        let theme = Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor);
        let editor_theme = editor_theme_from_theme(&theme);
        assert_ne!(
            editor_theme.popup.selected.bg,
            Style::default().bg,
            "selected row needs a visible band background"
        );
        assert_eq!(
            editor_theme.popup.item.bg,
            Style::default().bg,
            "unselected rows keep the default background"
        );
        assert_ne!(editor_theme.popup.selected, editor_theme.popup.item);
    }

    /// The tools-expand chord (alt+o) flips the chat model's flag through
    /// the keymap controller's capture phase: the editor never sees the
    /// key, and a plain `o` still reaches it.
    #[tokio::test]
    async fn tools_expand_chord_flips_the_flag_via_the_keymap() {
        let chat = empty_chat();
        let (mut app, mut writer, shell, _root) = init_app_with_chat(Rc::clone(&chat)).await;

        // ESC-prefixed 'o' is the legacy encoding of alt+o.
        writer.write_all(b"\x1bo").expect("write alt+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(chat.borrow().tools_expanded);
        assert_eq!(
            shell.borrow().editor.borrow().cursor(),
            (0, 0),
            "the chord never reached the editor"
        );

        // A plain 'o' is normal typing.
        writer.write_all(b"o").expect("write o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(chat.borrow().tools_expanded, "unchanged by plain typing");
        assert_eq!(shell.borrow().editor.borrow().cursor(), (0, 1));
    }

    /// The thinking toggle (alt+t) flips thinking-block visibility, per
    /// aj's `aj.thinking.toggle` semantics.
    #[tokio::test]
    async fn thinking_toggle_chord_flips_visibility_via_the_keymap() {
        let chat = empty_chat();
        let (mut app, mut writer, _shell, _root) = init_app_with_chat(Rc::clone(&chat)).await;
        let initial = chat.borrow().hide_thinking_block;

        writer.write_all(b"\x1bt").expect("write alt+t");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(chat.borrow().hide_thinking_block, !initial);

        writer.write_all(b"\x1bt").expect("write alt+t");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(chat.borrow().hide_thinking_block, initial);
    }

    /// The world-facing chords park their action for the host loop.
    #[tokio::test]
    async fn host_actions_park_in_the_slot() {
        let (mut app, mut writer, shell, _root) = init_app().await;
        let mut press = async |bytes: &[u8]| {
            writer.write_all(bytes).expect("write chord");
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        };

        // Alt+Enter (ESC CR) steers, Alt+Up (CSI 1;3A) dequeues, Ctrl+V
        // pastes, Ctrl+R opens history, Alt+A the agent picker.
        press(b"\x1b\r").await;
        assert_eq!(shell.borrow().take_host_action(), Some(AjAction::Steer));
        press(b"\x1b[1;3A").await;
        assert_eq!(shell.borrow().take_host_action(), Some(AjAction::Dequeue));
        press(&[0x16]).await;
        assert_eq!(
            shell.borrow().take_host_action(),
            Some(AjAction::PasteImage)
        );
        press(&[0x12]).await;
        assert_eq!(
            shell.borrow().take_host_action(),
            Some(AjAction::HistoryOpen)
        );
        press(b"\x1ba").await;
        assert_eq!(
            shell.borrow().take_host_action(),
            Some(AjAction::AgentPickerOpen)
        );
        assert_eq!(
            shell.borrow().editor.borrow().cursor(),
            (0, 0),
            "none of the chords leaked into the editor"
        );
    }

    /// The ctrl+c ladder through real dispatch. While the viewed agent
    /// runs, ctrl+c parks `CancelTurn` and nothing arms. While idle, the
    /// first ctrl+c arms the quit sequence and the second quits.
    #[tokio::test]
    async fn ctrl_c_ladder_cancels_then_arms_then_quits() {
        let (mut app, mut writer, shell, _root) = init_app().await;

        // Running: cancel, no arming.
        shell.borrow().keymap_ctx.borrow_mut().turn_running = true;
        writer.write_all(&[0x03]).expect("write ctrl+c");
        let event = app.next_input().await.expect("input event");
        assert!(!app.handle_input(event).quit);
        assert_eq!(
            shell.borrow().take_host_action(),
            Some(AjAction::CancelTurn)
        );
        assert!(shell.borrow().keymap.borrow().pending_sequence().is_none());

        // Idle: the first press arms instead of quitting.
        shell.borrow().keymap_ctx.borrow_mut().turn_running = false;
        writer.write_all(&[0x03]).expect("write ctrl+c");
        let event = app.next_input().await.expect("input event");
        assert!(!app.handle_input(event).quit);
        assert!(shell.borrow().take_host_action().is_none());
        assert!(shell.borrow().keymap.borrow().pending_sequence().is_some());

        // The second press completes the quit sequence.
        writer.write_all(&[0x03]).expect("write ctrl+c");
        let event = app.next_input().await.expect("input event");
        assert!(app.handle_input(event).quit);
    }

    /// The 8A parity trap: an armed quit drops out when a turn starts,
    /// so the next ctrl+c cancels instead of quitting. The engine
    /// re-checks the sequence predicate on every advance.
    #[tokio::test]
    async fn armed_quit_falls_through_to_cancel_when_a_turn_starts() {
        let (mut app, mut writer, shell, _root) = init_app().await;

        writer.write_all(&[0x03]).expect("write ctrl+c");
        let event = app.next_input().await.expect("input event");
        assert!(!app.handle_input(event).quit);
        assert!(shell.borrow().keymap.borrow().pending_sequence().is_some());

        shell.borrow().keymap_ctx.borrow_mut().turn_running = true;
        writer.write_all(&[0x03]).expect("write ctrl+c");
        let event = app.next_input().await.expect("input event");
        assert!(!app.handle_input(event).quit, "did not quit");
        assert_eq!(
            shell.borrow().take_host_action(),
            Some(AjAction::CancelTurn)
        );
        assert!(shell.borrow().keymap.borrow().pending_sequence().is_none());
    }

    /// The quit sequence's timeout disarms it through the real timer
    /// machinery: after the tick fires, the next ctrl+c re-arms instead
    /// of quitting.
    #[tokio::test]
    async fn quit_arm_times_out_and_disarms() {
        let (mut app, mut writer, shell, _root) = init_app().await;
        // A zero timeout makes the scheduled disarm tick due immediately.
        shell.borrow().keymap.borrow_mut().timeout_ms = 0;

        writer.write_all(&[0x03]).expect("write ctrl+c");
        let event = app.next_input().await.expect("input event");
        assert!(!app.handle_input(event).quit);
        assert!(shell.borrow().keymap.borrow().pending_sequence().is_some());

        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(!app.fire_due_timers().quit);
        assert!(
            shell.borrow().keymap.borrow().pending_sequence().is_none(),
            "the timeout disarmed the sequence"
        );

        writer.write_all(&[0x03]).expect("write ctrl+c");
        let event = app.next_input().await.expect("input event");
        assert!(!app.handle_input(event).quit, "re-armed, did not quit");
        assert!(shell.borrow().keymap.borrow().pending_sequence().is_some());
    }

    /// With an overlay open, ctrl+c closes the whole stack instead of
    /// cancelling or arming, even while a turn runs, and focus returns
    /// to the editor.
    #[tokio::test]
    async fn ctrl_c_closes_overlays_instead_of_cancelling() {
        let (mut app, mut writer, shell, root) = init_app().await;

        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(shell.borrow().overlays.borrow().is_open());
        app.render(&root).expect("render");

        shell.borrow().keymap_ctx.borrow_mut().turn_running = true;
        writer.write_all(&[0x03]).expect("write ctrl+c");
        let event = app.next_input().await.expect("input event");
        assert!(!app.handle_input(event).quit);
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "close-all tore the stack down"
        );
        assert!(
            shell.borrow().take_host_action().is_none(),
            "the running turn was left alone"
        );
        assert!(shell.borrow().keymap.borrow().pending_sequence().is_none());
        app.render(&root).expect("render");

        writer.write_all(b"x").expect("write key");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().editor.borrow().cursor(),
            (0, 1),
            "focus is back in the editor"
        );
    }

    /// The quit-arm running-work summary: aj's wording without the
    /// press-again suffix (the hint box's ladder spells that out), and `None`
    /// when nothing runs.
    #[test]
    fn running_work_summary_wording() {
        assert_eq!(
            running_work_summary(1, 0).as_deref(),
            Some("1 agent still running")
        );
        assert_eq!(
            running_work_summary(2, 1).as_deref(),
            Some("2 agents / 1 task still running")
        );
        assert_eq!(
            running_work_summary(0, 3).as_deref(),
            Some("3 tasks still running")
        );
        assert_eq!(running_work_summary(0, 0), None);
    }

    /// `quit_arm_running_work` names the background work a quit would tear
    /// down when a turn runs, and is `None` when nothing runs.
    #[tokio::test]
    async fn quit_arm_running_work_reflects_running_work() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;
        assert_eq!(quit_arm_running_work(&world), None);

        handle_submit(&mut world, "go".to_string());
        assert_eq!(
            quit_arm_running_work(&world).as_deref(),
            Some("1 agent still running")
        );

        // Settle the turn so world teardown is clean.
        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
    }

    /// End-to-end over the real session path: submit a prompt into a
    /// scripted session, pump the loop arms by hand, and check the
    /// chat model holds the user prompt plus a finalized assistant
    /// reply. A full transcript render over the result must not panic.
    #[tokio::test]
    async fn scripted_prompt_streams_into_the_chat_model() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        handle_submit(&mut world, "hi there".to_string());
        assert!(world.turn_cancels.contains_key(&AgentId::Main));

        // Turn-join arm.
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("turn settles cleanly");
        assert!(world.turn_cancels.is_empty());

        // Event arm: everything the turn emitted is buffered now.
        let first = world.core.event_rx.try_recv().expect("events buffered");
        assert!(drain_events(&mut world, first).0);

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
            Rc::new(std::cell::Cell::new(false)),
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

    /// A non-empty launch prompt spawns a Main turn, so the initial
    /// session drives it without the user typing anything. The auto-submit
    /// registers a cancel token for `Main`, mirroring `handle_submit`.
    #[tokio::test]
    async fn launch_prompt_spawns_a_main_turn() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        auto_submit_launch(&mut world, vec![UserContent::text("launch me")]);
        assert!(world.turn_cancels.contains_key(&AgentId::Main));

        // Settle the turn so world teardown is clean.
        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
    }

    /// An empty launch prompt (no positionals, no `@file`) spawns nothing,
    /// so a bare `aj-next` starts on the idle splash.
    #[tokio::test]
    async fn empty_launch_prompt_spawns_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        auto_submit_launch(&mut world, Vec::new());
        assert!(world.turn_cancels.is_empty());
        assert!(world.turns.is_empty());
    }

    /// `drain_events` reports the wake targets `aj` triggers on
    /// mid-select: every `TaskEnd` unconditionally, `AgentEnd` only
    /// when the agent has queued notices or pending messages.
    #[tokio::test]
    async fn drain_events_reports_wake_triggers() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        let task_end = AgentEvent::TaskEnd {
            agent_id: AgentId::Main,
            task_id: 1,
            call_id: "tu-1".into(),
            status: aj_agent::tool::TaskStatus::Exited(Some(0)),
            label: "cmd".into(),
        };
        let (_, wake) = drain_events(&mut world, task_end);
        assert_eq!(wake, vec![AgentId::Main], "TaskEnd wakes unconditionally");

        let agent_end = || AgentEvent::AgentEnd {
            agent_id: AgentId::Main,
            messages: Vec::new(),
        };
        let (_, wake) = drain_events(&mut world, agent_end());
        assert!(wake.is_empty(), "idle AgentEnd with nothing queued");

        world
            .core
            .message_queues
            .append_follow_up(AgentId::Main, "queued follow-up");
        let (_, wake) = drain_events(&mut world, agent_end());
        assert_eq!(wake, vec![AgentId::Main], "AgentEnd with pending work");
    }

    /// End-to-end over the `background-task` demo: the launch turn
    /// spawns a real background bash task, its completion triggers a
    /// wake turn, and the wake delivers the collapsible
    /// task-notification plus the wrap-up response.
    #[tokio::test]
    async fn background_task_completion_wakes_the_agent() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "background-task").await;

        handle_submit(&mut world, "run it".to_string());
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("prompt turn settles");

        // The wake turn is spawned either by the turn join above (the
        // task finished while the turn still streamed, so its notice
        // was already queued) or by the TaskEnd trigger while draining
        // here. Loop until one of the two paths armed a turn.
        while world.turn_cancels.is_empty() {
            let event = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                world.core.event_rx.recv(),
            )
            .await
            .expect("an event arrives before the timeout")
            .expect("event channel open");
            let (_, wake_targets) = drain_events(&mut world, event);
            spawn_wakes(&mut world, wake_targets);
        }

        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("wake turn settles");
        // Fold the buffered tail. Wake targets are ignored on purpose:
        // the TaskEnd may sit in this tail, and re-waking the idle
        // agent with no notices left would only spawn a no-op turn the
        // test would have to join.
        while let Ok(event) = world.core.event_rx.try_recv() {
            let _ = drain_events(&mut world, event);
        }

        let chat = world.chat.borrow();
        let entries = chat
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries();
        // The launch cell persisted the task id in its bash payload.
        let launch = entries
            .iter()
            .find_map(|e| match &e.kind {
                EntryKind::Tool(t) => Some(t),
                _ => None,
            })
            .expect("launch tool cell");
        assert!(
            matches!(
                &launch.details,
                Some(aj_agent::tool::ToolDetails::Bash {
                    task_id: Some(_),
                    ..
                })
            ),
            "launch cell records the task id: {:?}",
            launch.details,
        );
        // The task reached its terminal status in the model.
        assert!(
            chat.tasks()
                .values()
                .any(|info| info.status == aj_agent::tool::TaskStatus::Exited(Some(0))),
            "task tracked to exited 0: {:?}",
            chat.tasks(),
        );
        // The completion notice arrived as a collapsible user bubble.
        assert!(
            entries
                .iter()
                .any(|e| matches!(&e.kind, EntryKind::User(u) if u.collapsible)),
            "collapsible task-notification entry present",
        );
        // The wrap-up response followed the notification.
        let wrap = entries
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                EntryKind::Assistant(a) => Some(a),
                _ => None,
            })
            .expect("wrap-up assistant entry");
        let text: String = wrap
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                aj_models::types::AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("The background task finished"),
            "wake turn consumed the wrap-up script: {text:?}",
        );
    }

    /// A submit while the viewed agent runs queues a follow-up: the
    /// pending snapshot fills, the post-turn wake consumes it, and
    /// the queued text lands in the transcript as a user entry.
    #[tokio::test]
    async fn submit_while_running_queues_and_the_wake_delivers_it() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        handle_submit(&mut world, "first".to_string());
        assert!(world.turn_cancels.contains_key(&AgentId::Main));

        // Wait until the prompt's own user message landed before
        // queueing: the turn drains the follow-up queue right at its
        // start (before appending the prompt), so a message queued
        // before that point would be delivered by the first turn
        // instead of the wake.
        let saw_prompt = |world: &World| {
            let chat = world.chat.borrow();
            chat.transcript(AgentId::Main)
                .expect("main transcript")
                .entries()
                .iter()
                .any(|e| matches!(&e.kind, EntryKind::User(u) if u.joined_text() == "first"))
        };
        while !saw_prompt(&world) {
            let event = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                world.core.event_rx.recv(),
            )
            .await
            .expect("an event arrives before the timeout")
            .expect("event channel open");
            let _ = drain_events(&mut world, event);
        }

        handle_submit(&mut world, "second".to_string());
        let snapshot = world.core.message_queues.snapshot(AgentId::Main);
        assert_eq!(
            snapshot.kind,
            Some(aj_agent::queue::PendingKind::FollowUp),
            "busy submit queues instead of spawning",
        );
        assert_eq!(snapshot.text, "second");

        // First turn settles; the join handler sees the pending
        // follow-up and spawns the wake turn.
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("prompt turn settles");
        assert!(
            world.turn_cancels.contains_key(&AgentId::Main),
            "post-turn wake spawned for the pending message",
        );

        // The wake consumes the queue and delivers the message.
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("wake turn settles");
        assert!(
            world
                .core
                .message_queues
                .snapshot(AgentId::Main)
                .kind
                .is_none(),
            "queue drained by the wake",
        );
        while let Ok(event) = world.core.event_rx.try_recv() {
            let _ = drain_events(&mut world, event);
        }
        let chat = world.chat.borrow();
        let entries = chat
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries();
        assert!(
            entries.iter().any(|e| matches!(
                &e.kind,
                EntryKind::User(u) if u.joined_text() == "second"
            )),
            "queued text landed as a user entry",
        );
    }

    /// Ctrl+C with a driven turn cancels it (and is absorbed); with
    /// nothing running it falls through to quit.
    #[tokio::test]
    async fn ctrl_c_cancels_a_running_turn_before_quitting() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        assert!(!cancel_viewed_turn(&world), "idle: fall through to quit");

        handle_submit(&mut world, "go".to_string());
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

    /// Shell over the world's own chat and queues, for the host-action
    /// tests that touch both (the editor lives on the Shell, the queues
    /// on the world).
    async fn world_and_shell(dir: &TempDir, demo: &str) -> (World, Rc<RefCell<Shell>>) {
        let world = scripted_world(dir, demo).await;
        let shell = Rc::new(RefCell::new(Shell::new(
            Rc::clone(&world.chat),
            Rc::clone(&world.status),
            world.core.message_queues.clone(),
            ThemeHandle::new(Theme::bundled_dark_with_mode(
                aj_app::theme::ColorMode::Truecolor,
            )),
            "aj-next".to_string(),
            PathBuf::from("/tmp"),
        )));
        (world, shell)
    }

    /// Invoke [`apply_command_action`] with a throwaway export delivery
    /// channel, for the tests that exercise actions other than `ExportHtml`
    /// (which is the only arm that sends on it). Tests that exercise the
    /// export offload keep the receiver and call `apply_command_action`
    /// directly.
    async fn apply_command(
        world: &mut World,
        shell: &Rc<RefCell<Shell>>,
        action: CommandAction,
    ) -> ActionEffect {
        let (export_tx, _export_rx) = unbounded_channel();
        let (redraw_tx, _redraw_rx) = unbounded_channel();
        apply_command_action(world, shell, action, &export_tx, &redraw_tx).await
    }
    /// bound to it, so tests can exercise the host arms that need all three
    /// of the app, the world, and the shell (the login/logout flow).
    async fn init_app_with_world(
        dir: &TempDir,
        demo: &str,
    ) -> (AsyncApp, PipeWriter, World, Rc<RefCell<Shell>>, WidgetRef) {
        let world = scripted_world(dir, demo).await;
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        writer.write_all(b"\x1b[?c").expect("write DA1 reply");
        let shell = Rc::new(RefCell::new(Shell::new(
            Rc::clone(&world.chat),
            Rc::clone(&world.status),
            world.core.message_queues.clone(),
            ThemeHandle::new(Theme::bundled_dark_with_mode(
                aj_app::theme::ColorMode::Truecolor,
            )),
            "aj-next".to_string(),
            PathBuf::from("/tmp"),
        )));
        let root: WidgetRef = to_widget_ref(Rc::clone(&shell));
        let mut app = AsyncApp::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
            reader.into(),
        );
        app.init(Rc::clone(&root), Options::default())
            .await
            .expect("init");
        (app, writer, world, shell, root)
    }

    /// Alt+Enter's steer action while the viewed agent is busy: editor
    /// text queues as steering (and the editor clears), an empty editor
    /// promotes the pending follow-up.
    #[tokio::test]
    async fn steer_action_queues_steering_and_promotes_follow_ups() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        handle_submit(&mut world, "first".to_string());
        assert!(world.turn_cancels.contains_key(&AgentId::Main), "busy");

        // Busy + editor text: queue as steering, clear the editor.
        shell
            .borrow()
            .editor
            .borrow_mut()
            .insert_at_cursor("steer this");
        handle_host_action(&mut world, &shell, AjAction::Steer);
        let snapshot = world.core.message_queues.snapshot(AgentId::Main);
        assert_eq!(snapshot.kind, Some(aj_agent::queue::PendingKind::Steering));
        assert_eq!(snapshot.text, "steer this");
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "",
            "the steered text left the editor"
        );

        // Busy + empty editor + pending follow-up: promote to steering.
        world.core.message_queues.clear(AgentId::Main);
        world
            .core
            .message_queues
            .append_follow_up(AgentId::Main, "follow-up");
        handle_host_action(&mut world, &shell, AjAction::Steer);
        let snapshot = world.core.message_queues.snapshot(AgentId::Main);
        assert_eq!(
            snapshot.kind,
            Some(aj_agent::queue::PendingKind::Steering),
            "the pending follow-up escalated"
        );
        assert_eq!(snapshot.text, "follow-up");

        // Settle the turn so world teardown is clean. Drop the queue
        // first so the join's wake gate has nothing to deliver.
        world.core.message_queues.clear(AgentId::Main);
        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
    }

    /// Alt+Enter's steer action while idle starts a normal turn (there
    /// is nothing to steer yet), matching aj.
    #[tokio::test]
    async fn steer_action_spawns_a_turn_while_idle() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        shell
            .borrow()
            .editor
            .borrow_mut()
            .insert_at_cursor("hi there");
        handle_host_action(&mut world, &shell, AjAction::Steer);
        assert!(
            world.turn_cancels.contains_key(&AgentId::Main),
            "idle steer spawned a prompt turn"
        );
        assert!(
            world
                .core
                .message_queues
                .snapshot(AgentId::Main)
                .kind
                .is_none(),
            "nothing queued"
        );

        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("turn settles");
        let first = world.core.event_rx.try_recv().expect("events buffered");
        drain_events(&mut world, first);
        let chat = world.chat.borrow();
        let entries = chat
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries();
        assert!(entries.iter().any(|e| matches!(
            &e.kind,
            EntryKind::User(u) if u.joined_text() == "hi there"
        )));
    }

    /// Alt+Up's dequeue action pulls the queued message back into the
    /// editor, prepending it to the current draft (blank-line joined),
    /// and empties the queue.
    #[tokio::test]
    async fn dequeue_action_yanks_the_pending_message_into_the_editor() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        handle_submit(&mut world, "first".to_string());
        handle_submit(&mut world, "queued line".to_string());
        assert_eq!(
            world.core.message_queues.snapshot(AgentId::Main).text,
            "queued line"
        );

        shell.borrow().editor.borrow_mut().insert_at_cursor("draft");
        assert!(handle_host_action(&mut world, &shell, AjAction::Dequeue));
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "queued line\n\ndraft"
        );
        assert!(!world.core.message_queues.has_pending(AgentId::Main));

        // Nothing pending: the yank reports no change.
        assert!(!handle_host_action(&mut world, &shell, AjAction::Dequeue));

        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
    }

    /// Cancelling a turn restores the queued message into the editor
    /// (matching aj) instead of letting the post-turn wake deliver it.
    #[tokio::test]
    async fn cancel_action_yanks_the_pending_message_back() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        handle_submit(&mut world, "first".to_string());
        handle_submit(&mut world, "second".to_string());

        assert!(handle_host_action(&mut world, &shell, AjAction::CancelTurn));
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "second",
            "the queued follow-up came back to the editor"
        );
        assert!(!world.core.message_queues.has_pending(AgentId::Main));

        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
        assert!(
            world.turn_cancels.is_empty(),
            "no wake spawned, the queue was empty"
        );
    }

    /// The remaining placeholder host action (clipboard paste) folds a
    /// notice; the two overlay openers instead park their command for the
    /// host to open the overlay.
    #[tokio::test]
    async fn placeholder_and_opener_host_actions() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        assert!(handle_host_action(&mut world, &shell, AjAction::PasteImage));
        assert_eq!(
            shell.borrow().take_command(),
            None,
            "paste is not an overlay opener"
        );

        // The overlay openers park the matching command (opened next step).
        handle_host_action(&mut world, &shell, AjAction::HistoryOpen);
        assert_eq!(
            shell.borrow().take_command(),
            Some(CommandAction::OpenPromptHistory)
        );
        handle_host_action(&mut world, &shell, AjAction::AgentPickerOpen);
        assert_eq!(
            shell.borrow().take_command(),
            Some(CommandAction::OpenAgentPicker)
        );

        let notices: Vec<String> = world
            .chat
            .borrow()
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Notice(n) => Some(n.text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notices.iter().any(|n| n.contains("image paste")),
            "{notices:?}"
        );
    }

    // ---- Overlay substrate ----

    /// The TestTty geometry every overlay test runs against.
    fn full_draw_ctx() -> DrawContext {
        DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(80),
                height: Some(40),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: vaxis::gwidth::Method::Unicode,
        }
    }

    /// Composites a surface tree into plain text rows, blitting buffers
    /// depth-first with children in ascending z order, the same order
    /// `Surface::render` paints. Lets tests read "what's on screen"
    /// without a terminal.
    fn flatten(surface: &Surface) -> Vec<String> {
        fn blit(surface: &Surface, row_off: i32, col_off: i32, grid: &mut [Vec<char>]) {
            let width = usize::from(surface.size.width);
            for (i, cell) in surface.buffer.iter().enumerate() {
                let row = row_off + i32::try_from(i / width).expect("row fits i32");
                let col = col_off + i32::try_from(i % width).expect("col fits i32");
                let (Ok(row), Ok(col)) = (usize::try_from(row), usize::try_from(col)) else {
                    continue;
                };
                if row >= grid.len() || col >= grid[row].len() {
                    continue;
                }
                grid[row][col] = cell.char.grapheme().chars().next().unwrap_or(' ');
            }
            let mut order: Vec<&SubSurface> = surface.children.iter().collect();
            order.sort_by_key(|child| child.z_index);
            for child in order {
                blit(
                    &child.surface,
                    row_off + child.origin.row,
                    col_off + child.origin.col,
                    grid,
                );
            }
        }
        let mut grid =
            vec![vec![' '; usize::from(surface.size.width)]; usize::from(surface.size.height)];
        blit(surface, 0, 0, &mut grid);
        grid.into_iter()
            .map(|row| row.into_iter().collect())
            .collect()
    }

    /// Fold `count` numbered notice rows into `chat` so the transcript
    /// overflows the 40-row test viewport.
    fn fold_lines(chat: &Rc<RefCell<ChatState>>, count: usize) {
        use aj_agent::message::AgentMessage;
        use aj_models::types::{Message, UserMessage};

        let mut lifecycle = aj_app::session::AgentLifecycle::default();
        // Seed a user message so the chat slot shows the transcript rather than
        // the empty-state splash. The notice rows below are the scroll content
        // these callers assert on.
        let _ = reduce(
            &mut chat.borrow_mut(),
            &mut lifecycle,
            AgentEvent::MessageEnd {
                agent_id: AgentId::Main,
                message: AgentMessage::wire(Message::User(UserMessage::text("session start"))),
            },
        );
        for i in 0..count {
            let _ = reduce(
                &mut chat.borrow_mut(),
                &mut lifecycle,
                notice_event(&format!("line-{i:03}")),
            );
        }
    }

    fn wheel_up_at(row: i16, col: i16) -> Event {
        Event::Mouse(vaxis::mouse::Mouse {
            col,
            row,
            xoffset: 0,
            yoffset: 0,
            button: vaxis::mouse::Button::WheelUp,
            mods: vaxis::mouse::Modifiers::empty(),
            kind: vaxis::mouse::Type::Press,
        })
    }

    /// The Shell's draw wraps the keymap controller's surface (so the
    /// controller sits on the focus path) and appends the scrim and the
    /// top overlay above the base layout, in z order and at the ported
    /// placement. The deepest hit at base-layout coordinates is the
    /// scrim.
    #[test]
    fn overlay_draw_appends_scrim_and_overlay_above_the_base() {
        use vaxis::vxfw::{Point, widget_eq};

        let chat = empty_chat();
        // Enough entries to fill the whole chat slot, so base content shows
        // in the rows above the overlay box.
        fold_lines(&chat, 40);
        let shell = test_shell_with_chat(chat);
        let ctx = full_draw_ctx();

        // Before opening: the base content is visible.
        let base = flatten(&shell.borrow_mut().draw(&ctx));
        assert!(
            base.iter().any(|row| row.contains("line-")),
            "base rows visible before the overlay: {base:?}"
        );

        let mut ev_ctx = EventContext::new();
        {
            let shell = shell.borrow();
            let editor: WidgetRef = to_widget_ref(Rc::clone(&shell.editor));
            open_palette(
                &shell.overlays,
                &editor,
                &shell.chrome,
                &shell.command_slot,
                &shell.fetch_slot,
                &mut ev_ctx,
            );
        }
        assert!(shell.borrow().overlays.borrow().is_open());

        let surface = shell.borrow_mut().draw(&ctx);
        // The wrapper's sole z-0 child is the keymap controller's
        // surface, which carries the base layout plus the overlay
        // children.
        assert_eq!(surface.children.len(), 1);
        let inner = &surface.children[0].surface;
        let controller = to_widget_ref(Rc::clone(&shell.borrow().keymap));
        assert!(widget_eq(
            inner.widget.as_ref().expect("controller stamped"),
            &controller,
        ));
        let scrim = inner
            .children
            .iter()
            .find(|c| c.z_index == 1)
            .expect("scrim child at z 1");
        assert_eq!(
            scrim.surface.size,
            Size {
                width: 80,
                height: 40
            },
            "scrim covers the viewport"
        );
        let scrim_widget = to_widget_ref(Rc::clone(&shell.borrow().scrim));
        assert!(widget_eq(
            scrim.surface.widget.as_ref().expect("scrim stamped"),
            &scrim_widget,
        ));
        // The Small placement at 80x40: 75% width rounds to 60, below the
        // 72-column floor, and 22 inner rows plus 4 chrome, centered.
        let overlay = inner
            .children
            .iter()
            .find(|c| c.z_index == 2)
            .expect("overlay child at z 2");
        assert_eq!(overlay.origin, RelativePoint { row: 7, col: 4 });
        assert_eq!(
            overlay.surface.size,
            Size {
                width: 72,
                height: 26
            }
        );

        // What's on screen: the chrome title where the overlay sits, and
        // the base content still visible around the floating window (the
        // scrim paints nothing). The overlay box starts at row 7, so the
        // rows above it show the transcript.
        let rows = flatten(&surface);
        assert!(rows[7].contains(" Commands "), "title row: {:?}", rows[7]);
        assert!(
            rows[..7].iter().any(|row| row.contains("line-")),
            "base content visible above the overlay: {:?}",
            &rows[..7]
        );

        // A point in the base layout hit-tests to the scrim (topmost-last),
        // so the scrim is the mouse target there.
        let mut hits = Vec::new();
        surface.hit_test(Point { row: 3, col: 3 }, &mut hits);
        let deepest = hits.last().expect("hits at base coords");
        assert!(widget_eq(&deepest.widget, &scrim_widget));

        // A point on the overlay's filter row targets the overlay's own
        // widgets, not the scrim.
        let mut hits = Vec::new();
        surface.hit_test(Point { row: 9, col: 10 }, &mut hits);
        let deepest = hits.last().expect("hits at overlay coords");
        assert!(!widget_eq(&deepest.widget, &scrim_widget));
    }

    /// Ctrl+O opens the palette and moves focus into its filter: keys
    /// typed while it is open never reach the editor. Esc closes it and
    /// returns focus, so the next key lands in the editor again.
    #[tokio::test]
    async fn ctrl_o_opens_the_palette_and_esc_returns_focus_to_the_editor() {
        let (mut app, mut writer, shell, root) = init_app().await;

        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(shell.borrow().overlays.borrow().is_open());
        // The focus change lands on the dispatch path at the next layout.
        app.render(&root).expect("render");

        writer.write_all(b"q").expect("write key");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().editor.borrow().cursor(),
            (0, 0),
            "typed key went to the palette filter, not the editor"
        );

        writer.write_all(b"\x1b").expect("write esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(!shell.borrow().overlays.borrow().is_open(), "esc closes");
        assert!(shell.borrow().take_command().is_none());
        app.render(&root).expect("render");

        writer.write_all(b"x").expect("write key");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().editor.borrow().cursor(),
            (0, 1),
            "focus is back in the editor"
        );
    }

    /// Typing narrows the palette rows and Enter confirms the highlighted
    /// one. Confirming a host-applied command (compact) parks its action for
    /// the drive loop and leaves the palette on the stack. The drive loop's
    /// no-overlay path then pops the palette back to the editor.
    #[tokio::test]
    async fn palette_filter_narrows_and_enter_confirms() {
        let (mut app, mut writer, shell, root) = init_app().await;

        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        app.render(&root).expect("render");

        // "compact" matches only the compact row's `{category} {title}`
        // filter key, so Enter confirms it rather than the first row.
        writer.write_all(b"compact\r").expect("write query + enter");
        for _ in 0..8 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }

        // The confirm callback only parks the command now: the palette stays
        // on the stack for the drive loop to resolve.
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            1,
            "palette stays open after parking a host command"
        );
        let action = shell
            .borrow()
            .take_command()
            .expect("confirm parked a host command");
        assert_eq!(action, CommandAction::Compact, "confirmed the compact row");
        app.render(&root).expect("render");

        // Compact opens no overlay (a pure action, drive-loop `Redraw`), so the
        // loop's no-overlay path pops the palette and returns to the editor.
        shell.borrow().overlays.borrow_mut().back();
        focus_overlay(&mut app, &root);
        assert!(!shell.borrow().overlays.borrow().is_open());

        writer.write_all(b"x").expect("write key");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().editor.borrow().cursor(),
            (0, 1),
            "focus is back in the editor"
        );
    }

    /// Confirming Help from the palette opens the help overlay on top of
    /// the palette (chaining): Esc returns to the palette, a second Esc
    /// returns to the editor.
    #[tokio::test]
    async fn palette_help_chains_and_esc_walks_back_to_the_editor() {
        let (mut app, mut writer, shell, root) = init_app().await;

        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        app.render(&root).expect("render");
        assert_eq!(shell.borrow().overlays.borrow().depth(), 1, "palette open");

        // Filter to "help" and confirm: the help overlay pushes on top of
        // the palette, which stays underneath.
        writer.write_all(b"help\r").expect("write query + enter");
        for _ in 0..5 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            2,
            "help pushed over the palette"
        );
        assert!(
            shell.borrow().take_command().is_none(),
            "an overlay-opening command is not parked for the host"
        );
        app.render(&root).expect("render");

        // Esc closes the help overlay back to the palette.
        writer.write_all(b"\x1b").expect("write esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            1,
            "esc returned to the palette"
        );
        app.render(&root).expect("render");

        // Esc closes the palette back to the editor.
        writer.write_all(b"\x1b").expect("write esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(!shell.borrow().overlays.borrow().is_open(), "editor again");
        app.render(&root).expect("render");
        writer.write_all(b"x").expect("write key");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().editor.borrow().cursor(),
            (0, 1),
            "focus is back in the editor"
        );
    }

    /// A non-content selector confirmed from the palette chains like Help: the
    /// drive loop pushes it over the palette so Esc/cancel (`close_top`)
    /// returns to the palette, while confirming a value (`close_all`) tears the
    /// whole stack down to the transcript.
    #[tokio::test]
    async fn palette_selector_cancel_returns_to_palette_confirm_returns_to_editor() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;

        // Open the palette (depth 1).
        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        app.render(&root).expect("render");
        assert_eq!(shell.borrow().overlays.borrow().depth(), 1, "palette open");

        // Confirm the thinking row. The callback only parks the opener and
        // leaves the palette on the stack (no pop in the callback).
        writer
            .write_all(b"thinking\r")
            .expect("write query + enter");
        for _ in 0..9 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            1,
            "palette stays open under the parked opener"
        );
        let action = shell.borrow().take_command().expect("opener parked");
        assert_eq!(action, CommandAction::OpenThinkingSelector);

        // Drive-loop `OpenedOverlay`: the host opens the selector ON TOP of the
        // palette (depth 2) and refocuses it.
        assert!(matches!(
            apply_command(&mut world, &shell, action).await,
            ActionEffect::OpenedOverlay
        ));
        focus_overlay(&mut app, &root);
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            2,
            "selector pushed over the palette"
        );

        // Esc/cancel from the selector uses `close_top`: it returns to the
        // palette underneath, not to the transcript.
        writer.write_all(b"\x1b").expect("write esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            1,
            "esc returned to the palette"
        );
        app.render(&root).expect("render");

        // Focus really landed on the palette: pressing Enter on the still
        // "thinking"-filtered palette re-parks the opener (an editor-focused
        // Enter would park nothing).
        writer.write_all(b"\r").expect("write enter");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        let action = shell
            .borrow()
            .take_command()
            .expect("palette had focus and re-parked the opener");
        assert_eq!(action, CommandAction::OpenThinkingSelector);

        // Re-open the selector over the palette (depth 2) and confirm a value.
        assert!(matches!(
            apply_command(&mut world, &shell, action).await,
            ActionEffect::OpenedOverlay
        ));
        focus_overlay(&mut app, &root);
        assert_eq!(shell.borrow().overlays.borrow().depth(), 2);

        // Confirm "high": the selector's `close_all` tears the whole stack down
        // (palette included) back to the transcript.
        writer.write_all(b"high\r").expect("write query + enter");
        for _ in 0..5 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "confirm tore the whole stack down to the transcript"
        );

        // The pick was recorded for the drive loop to apply.
        let activity = shell.borrow().take_activity();
        assert!(
            activity
                .iter()
                .any(|a| matches!(a, SelectorActivity::ThinkingConfirmed { .. })),
            "confirm parked a thinking change"
        );

        // Focus is back in the editor.
        app.render(&root).expect("render");
        writer.write_all(b"x").expect("write key");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(shell.borrow().editor.borrow().cursor(), (0, 1));
    }

    /// The agent picker chains under the palette like the other selectors:
    /// cancel (`close_top`) returns to the palette, confirm (`close_all`) tears
    /// the whole stack down. Because confirm clears the palette too, a drilled
    /// task viewer the drive loop opens afterward stands alone (its Esc returns
    /// to the transcript, not to a parent overlay).
    #[tokio::test]
    async fn palette_agent_picker_cancel_returns_to_palette_confirm_clears_stack() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        seed_sub_and_task(&mut world);

        // Open the palette (depth 1) and confirm the agent-picker row.
        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        app.render(&root).expect("render");
        writer.write_all(b"switch\r").expect("write query + enter");
        for _ in 0..7 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
        let action = shell.borrow().take_command().expect("opener parked");
        assert_eq!(action, CommandAction::OpenAgentPicker);
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            1,
            "palette stays open under the parked opener"
        );

        // Drive-loop `OpenedOverlay`: the picker pushes over the palette.
        assert!(matches!(
            apply_command(&mut world, &shell, action).await,
            ActionEffect::OpenedOverlay
        ));
        focus_overlay(&mut app, &root);
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            2,
            "picker over palette"
        );

        // Cancel returns to the palette (close_top), not the transcript.
        writer.write_all(b"\x1b").expect("write esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            1,
            "esc returned to the palette"
        );
        app.render(&root).expect("render");

        // The palette kept focus: Enter re-parks the opener (still filtered to
        // "switch"). Re-open the picker over the palette.
        writer.write_all(b"\r").expect("write enter");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        let action = shell
            .borrow()
            .take_command()
            .expect("palette had focus and re-parked the opener");
        assert_eq!(action, CommandAction::OpenAgentPicker);
        assert!(matches!(
            apply_command(&mut world, &shell, action).await,
            ActionEffect::OpenedOverlay
        ));
        focus_overlay(&mut app, &root);
        assert_eq!(shell.borrow().overlays.borrow().depth(), 2);

        // Confirm the highlighted row (the active agent): `close_all` tears the
        // whole stack down (palette and picker) to the transcript.
        writer.write_all(b"\r").expect("write enter");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "confirm cleared the whole stack"
        );
        assert!(
            shell.borrow().take_picker_outcome().is_some(),
            "confirm parked the pick"
        );
    }

    /// The help overlay renders shortcuts resolved from the keybinding
    /// data: the palette-open row shows the resolved chord, not a literal.
    #[tokio::test]
    async fn palette_help_renders_resolved_shortcuts() {
        use vaxis::vxfw::MaxSize;

        let (mut app, mut writer, shell, root) = init_app().await;
        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        writer.write_all(b"help\r").expect("write query + enter");
        for _ in 0..5 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
        app.render(&root).expect("render");

        // Draw the whole shell and read the help overlay's rows.
        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(120),
                height: Some(40),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: vaxis::gwidth::Method::Unicode,
        };
        let rows = flatten(&shell.borrow_mut().draw(&ctx)).join("\n");
        let resolved =
            aj_app::keybindings::default_action_shortcut(aj_app::keybindings::ACTION_PALETTE_OPEN)
                .expect("palette-open has a default chord");
        assert!(
            rows.contains(&resolved),
            "help overlay shows the resolved shortcut {resolved:?}: {rows}"
        );
    }

    /// Confirming Quit from the palette quits the app: the dispatch sets
    /// the quit flag on the confirming keystroke.
    #[tokio::test]
    async fn palette_quit_quits() {
        let (mut app, mut writer, _shell, root) = init_app().await;
        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        app.render(&root).expect("render");

        // "quit" narrows to the quit row; Enter confirms it.
        writer.write_all(b"quit").expect("write query");
        for _ in 0..4 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
        writer.write_all(b"\r").expect("write enter");
        let event = app.next_input().await.expect("input event");
        assert!(app.handle_input(event).quit, "confirming quit quits");
    }

    /// `/login` opens a picker over the OAuth providers (the default
    /// registry is non-empty, so it opens rather than folding a notice).
    #[tokio::test]
    async fn login_picker_opens_over_providers() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        let effect = apply_command(&mut world, &shell, CommandAction::OpenLoginSelector).await;
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        assert_eq!(shell.borrow().overlays.borrow().depth(), 1, "picker open");
    }

    /// `/logout` with nothing stored folds an explanatory notice instead
    /// of opening an empty picker.
    #[tokio::test]
    async fn logout_picker_empty_folds_notice() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        let effect = apply_command(&mut world, &shell, CommandAction::OpenLogoutSelector).await;
        assert!(matches!(effect, ActionEffect::Redraw));
        assert_eq!(shell.borrow().overlays.borrow().depth(), 0, "no picker");
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n.contains("No stored credentials")),
            "{:?}",
            main_notices(&world)
        );
    }

    /// `/logout` lists a stored credential, and confirming it (the drive
    /// loop's auth-request drain) removes it from `auth.json` and folds a
    /// notice.
    #[tokio::test]
    async fn logout_removes_stored_credential_and_notes_it() {
        use aj_models::auth::AuthCredential;
        use aj_models::oauth::OAuthCredentials;

        let dir = TempDir::new().expect("tempdir");
        let (mut app, _writer, mut world, shell, _root) =
            init_app_with_world(&dir, "streaming-text").await;
        world
            .auth
            .set(
                "anthropic",
                AuthCredential::OAuth(OAuthCredentials::new("r", "a", 0)),
            )
            .await
            .expect("seed stored credential");

        // The picker would list it.
        let effect = apply_command(&mut world, &shell, CommandAction::OpenLogoutSelector).await;
        assert!(matches!(effect, ActionEffect::OpenedOverlay));

        // Confirming parks a Logout request; drain it the way the loop does.
        let (tx, _rx) = unbounded_channel();
        let mut login_session = None;
        apply_auth_request(
            &mut world,
            &shell,
            &mut app,
            &mut login_session,
            &tx,
            AuthPickerRequest::Logout {
                provider_id: "anthropic".to_string(),
            },
        )
        .await;

        assert!(
            world.auth.get("anthropic").await.expect("read").is_none(),
            "credential removed"
        );
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n.contains("Logged out of anthropic")),
            "{:?}",
            main_notices(&world)
        );
    }

    /// Confirming a login provider mounts the dialog overlay, tracks a
    /// `LoginSession`, and seeds the "Starting login…" line. The spawned
    /// task is aborted before it can run the real OAuth flow.
    #[tokio::test]
    async fn start_login_mounts_dialog_and_tracks_session() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, _writer, world, shell, _root) =
            init_app_with_world(&dir, "streaming-text").await;

        let (tx, _rx) = unbounded_channel();
        let mut login_session = None;
        start_login(
            &world,
            &shell,
            &mut app,
            &mut login_session,
            &tx,
            "anthropic".to_string(),
            "Anthropic (Claude Pro/Max)".to_string(),
        );

        assert!(login_session.is_some(), "session tracked");
        assert_eq!(shell.borrow().overlays.borrow().depth(), 1, "dialog open");
        let body = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(body.contains("Starting login"), "seed line: {body}");
        assert!(body.contains("Anthropic"), "provider in title: {body}");

        // Abort the spawned flow before it (never having been polled) can
        // touch the network.
        login_session.take().unwrap().handle.abort();
    }

    /// The login-task completion arm closes the dialog and folds a success
    /// notice on `Ok(Ok(()))`.
    #[tokio::test]
    async fn finish_login_success_closes_dialog_and_notes() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, _writer, mut world, shell, _root) =
            init_app_with_world(&dir, "streaming-text").await;

        let (tx, _rx) = unbounded_channel();
        let mut login_session = None;
        start_login(
            &world,
            &shell,
            &mut app,
            &mut login_session,
            &tx,
            "anthropic".to_string(),
            "Anthropic".to_string(),
        );
        login_session.as_mut().unwrap().handle.abort();
        assert_eq!(shell.borrow().overlays.borrow().depth(), 1);

        finish_login(&mut world, &shell, &mut app, &mut login_session, Ok(Ok(())));
        assert!(login_session.is_none(), "session cleared");
        assert_eq!(shell.borrow().overlays.borrow().depth(), 0, "dialog closed");
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n.contains("Logged in to Anthropic")),
            "{:?}",
            main_notices(&world)
        );
    }

    /// A failed login folds a warning and still closes the dialog.
    #[tokio::test]
    async fn finish_login_failure_warns_and_closes() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, _writer, mut world, shell, _root) =
            init_app_with_world(&dir, "streaming-text").await;

        let (tx, _rx) = unbounded_channel();
        let mut login_session = None;
        start_login(
            &world,
            &shell,
            &mut app,
            &mut login_session,
            &tx,
            "anthropic".to_string(),
            "Anthropic".to_string(),
        );
        login_session.as_mut().unwrap().handle.abort();

        finish_login(
            &mut world,
            &shell,
            &mut app,
            &mut login_session,
            Ok(Err(AuthError::OAuth(
                aj_models::oauth::OAuthError::Cancelled,
            ))),
        );
        assert!(login_session.is_none());
        assert_eq!(shell.borrow().overlays.borrow().depth(), 0);
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n.contains("Login to Anthropic failed")),
            "{:?}",
            main_notices(&world)
        );
    }

    /// The cancel-poll aborts the task, closes the dialog, and folds the
    /// cancelled notice when the dialog flips the shared flag.
    #[tokio::test]
    async fn cancel_login_aborts_closes_and_notes() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, _writer, mut world, shell, _root) =
            init_app_with_world(&dir, "streaming-text").await;

        let (tx, _rx) = unbounded_channel();
        let mut login_session = None;
        start_login(
            &world,
            &shell,
            &mut app,
            &mut login_session,
            &tx,
            "anthropic".to_string(),
            "Anthropic".to_string(),
        );
        // The dialog would flip this on Esc/Ctrl+C.
        login_session
            .as_ref()
            .unwrap()
            .cancel
            .store(true, Ordering::Relaxed);

        cancel_login(&mut world, &shell, &mut app, &mut login_session);
        assert!(login_session.is_none(), "session cleared");
        assert_eq!(shell.borrow().overlays.borrow().depth(), 0, "dialog closed");
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n.contains("Login to Anthropic cancelled")),
            "{:?}",
            main_notices(&world)
        );
    }

    /// Confirming session info opens a "Loading…" overlay and parks a
    /// fetch; filling it (what the host does after the async lookup)
    /// replaces the body with the resolved content.
    #[tokio::test]
    async fn palette_session_info_opens_loading_then_fills() {
        let (mut app, mut writer, shell, root) = init_app().await;
        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        // Render so the palette's focus request lands before we type.
        app.render(&root).expect("render");
        writer.write_all(b"info\r").expect("write query + enter");
        for _ in 0..5 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            2,
            "info overlay open"
        );
        let fetch = shell
            .borrow()
            .take_fetch()
            .expect("session info parked an async fetch");
        assert_eq!(fetch.kind, FetchKind::SessionInfo);
        app.render(&root).expect("render");
        let loading = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(loading.contains("Loading"), "loading state: {loading}");

        // Fill the overlay the way the drive loop does once the fetch
        // returns.
        crate::content_overlay::set_rows(
            &fetch.list,
            vec![crate::content_overlay::plain("id  session-xyz")],
        );
        let filled = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(filled.contains("session-xyz"), "filled state: {filled}");
        assert!(!filled.contains("Loading"), "loading replaced: {filled}");
    }

    /// Confirming skills from the palette opens the window immediately, on top
    /// of the palette (depth 2), showing a loading placeholder and returning a
    /// plain `OpenedOverlay` like every other opener. Delivering the discovered
    /// skills via the fill path replaces the placeholder with the skill rows,
    /// and Esc returns to the palette underneath.
    #[tokio::test]
    async fn open_skills_opens_loading_over_palette_and_fills() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;

        // Open the palette (depth 1).
        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        app.render(&root).expect("render");
        assert_eq!(shell.borrow().overlays.borrow().depth(), 1, "palette open");

        // Confirm skills: the window opens NOW over the palette (depth 2).
        let effect = apply_command(&mut world, &shell, CommandAction::OpenSkills).await;
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        focus_overlay(&mut app, &root);
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            2,
            "skills window opened over the palette"
        );
        let loading = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(
            loading.contains("Loading skills"),
            "loading state: {loading}"
        );

        // The window parked its fill handle for the drive loop.
        let fill = shell
            .borrow()
            .take_skills_fetch()
            .expect("skills window parked a fill handle");

        // Deliver discovered skills the way the drive loop's fill arm does.
        fill_skills_window(
            &fill,
            vec![Skill {
                name: "demo".to_string(),
                description: "a demo skill".to_string(),
                path: PathBuf::from("/tmp/demo/SKILL.md"),
                enabled: true,
                disable_model_invocation: false,
            }],
        );
        let filled = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(filled.contains("demo"), "filled state: {filled}");
        assert!(
            !filled.contains("Loading skills"),
            "placeholder replaced: {filled}"
        );

        // Esc from the skills window uses `close_top`: it returns to the palette
        // underneath, not to the transcript.
        writer.write_all(b"\x1b").expect("write esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            1,
            "esc returned to the palette"
        );
    }

    /// An empty discovery result fills the "No skills found" placeholder: the
    /// window stays open over the palette and no transcript notice is folded
    /// (the open window conveys the guidance itself, matching how prompt
    /// history shows an empty list).
    #[tokio::test]
    async fn open_skills_empty_discovery_fills_the_no_skills_placeholder() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;

        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        app.render(&root).expect("render");

        let effect = apply_command(&mut world, &shell, CommandAction::OpenSkills).await;
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        focus_overlay(&mut app, &root);
        assert_eq!(shell.borrow().overlays.borrow().depth(), 2);
        let fill = shell
            .borrow()
            .take_skills_fetch()
            .expect("fill handle parked");

        let before = main_notices(&world).len();
        fill_skills_window(&fill, Vec::new());
        let rendered = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(
            rendered.contains("No skills found"),
            "empty state: {rendered}"
        );
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            2,
            "window still open over the palette"
        );
        assert_eq!(
            main_notices(&world).len(),
            before,
            "no transcript notice folded"
        );
    }

    /// The drive loop's drain kicks off discovery off the loop once the window
    /// parked its fill handle: `spawn_skills_discovery` delivers exactly one
    /// result over the shared channel (the walk runs on the blocking pool). We
    /// assert the offload, not a specific skill set, which depends on the
    /// environment.
    #[tokio::test]
    async fn open_skills_drain_spawns_discovery_off_the_loop() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let (skills_tx, mut skills_rx) = unbounded_channel();

        // Opening the skills window parks a fill handle for the drive loop.
        let effect = apply_command(&mut world, &shell, CommandAction::OpenSkills).await;
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        assert!(
            shell.borrow().take_skills_fetch().is_some(),
            "the window parked a fill handle"
        );

        // The drain then spawns discovery off the loop; it delivers one result.
        spawn_skills_discovery(&world, &skills_tx);
        assert!(
            skills_rx.recv().await.is_some(),
            "discovery delivered a result"
        );
    }

    /// `ExportHtml` no longer renders on the loop: the action spawns the
    /// render + write off the loop and returns immediately, and the resulting
    /// notice comes back over the channel the drive loop's fill arm folds.
    #[tokio::test]
    #[serial_test::serial]
    async fn export_html_spawns_and_delivers_notice() {
        let dir = TempDir::new().expect("tempdir");
        // The export writes under `$HOME/.aj/exports`, so redirect HOME into
        // the tempdir rather than the real home.
        let _home = HomeGuard::set(dir.path());
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let (export_tx, mut export_rx) = unbounded_channel();
        let (redraw_tx, _redraw_rx) = unbounded_channel();

        let effect = apply_command_action(
            &mut world,
            &shell,
            CommandAction::ExportHtml,
            &export_tx,
            &redraw_tx,
        )
        .await;

        assert!(matches!(effect, ActionEffect::None));
        let notice = export_rx.recv().await.expect("export delivered a notice");
        assert!(
            notice.starts_with("Exported session to"),
            "export succeeded: {notice}"
        );
    }

    /// The theme name resolves from config, defaulting to `light` like
    /// `aj` and passing an explicit name (bundled or user) through.
    #[test]
    fn resolve_theme_name_defaults_to_light() {
        assert_eq!(resolve_theme_name(None), "light");
        assert_eq!(resolve_theme_name(Some("dark")), "dark");
        assert_eq!(resolve_theme_name(Some("solarized")), "solarized");
    }

    /// A theme swap followed by `restyle` rebuilds the resolved styles:
    /// the overlay chrome and a widget's drawn colors change from the dark
    /// palette to the light one.
    ///
    /// The footer's base text is the faint attribute (theme-independent, the
    /// same as `aj`'s `style::dim`), so to prove the footer re-resolves we
    /// drive its context-usage into the Critical band, which colors the
    /// percentage cell with the themed `error` color (different dark vs light).
    #[test]
    fn theme_swap_restyles_chrome_and_widgets() {
        let theme = ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor));
        let chat = Rc::new(RefCell::new(ChatState::new(
            aj_agent::events::AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            200_000,
            Arc::new(Vec::new()),
        )));
        chat.borrow_mut()
            .footers_mut()
            .set_context_tokens(aj_agent::events::AgentId::Main, 190_000);
        let shell = Rc::new(RefCell::new(Shell::new(
            chat,
            Rc::new(RefCell::new(StatusState::default())),
            MessageQueues::default(),
            theme.clone(),
            "aj-next".to_string(),
            PathBuf::from("/tmp/project"),
        )));

        let dark_border = shell.borrow().chrome.borrow().border;
        let dark_title = shell.borrow().chrome.borrow().title;
        let dark_footer = footer_fg_colors(&shell);

        theme.replace(Theme::bundled_light_with_mode(ColorMode::Truecolor));
        shell.borrow().restyle();

        let light_border = shell.borrow().chrome.borrow().border;
        let light_title = shell.borrow().chrome.borrow().title;
        let light_footer = footer_fg_colors(&shell);

        assert_ne!(dark_border, light_border, "overlay border re-resolved");
        assert_ne!(dark_title, light_title, "overlay title re-resolved");
        assert_ne!(
            dark_footer, light_footer,
            "the footer widget's drawn colors changed with the theme"
        );
    }

    /// The set of foreground colors the footer widget draws, for the
    /// re-style assertion above.
    fn footer_fg_colors(shell: &Rc<RefCell<Shell>>) -> Vec<vaxis::cell::Color> {
        let footer = Rc::clone(&shell.borrow().footer);
        let surface = footer
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(80, None));
        crate::test_support::flatten(&surface)
            .into_iter()
            .flatten()
            .filter(|c| !c.char.grapheme().trim().is_empty())
            .map(|c| c.style.fg)
            .collect()
    }

    /// Control for the scrim test below: without an overlay, wheel-up at
    /// transcript coordinates scrolls the transcript, so the tail line
    /// leaves the viewport.
    #[tokio::test]
    async fn wheel_up_scrolls_the_transcript_without_an_overlay() {
        let chat = empty_chat();
        fold_lines(&chat, 80);
        let (mut app, _writer, shell, _root) = init_app_with_chat(chat).await;

        for _ in 0..2 {
            app.handle_input(wheel_up_at(3, 3));
        }
        let rows = flatten(&shell.borrow_mut().draw(&full_draw_ctx()));
        assert!(
            !rows.iter().any(|row| row.contains("line-079")),
            "tail scrolled out of view: {rows:?}"
        );
    }

    /// With the palette open, the same wheel-up at transcript coordinates
    /// targets the scrim, which consumes it: after closing the overlay the
    /// transcript still shows its tail, untouched by the wheel.
    ///
    /// NOTE: only the at-target/bubble consumption is blocked. Base widgets
    /// intersecting the point still observe the event in their capturing
    /// phase before the scrim consumes it (see the `Scrim` docs), which is
    /// why this asserts on the scroll position rather than follow-tail.
    #[tokio::test]
    async fn wheel_at_transcript_coords_is_blocked_while_the_palette_is_open() {
        let chat = empty_chat();
        fold_lines(&chat, 80);
        let (mut app, mut writer, shell, root) = init_app_with_chat(chat).await;

        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        app.render(&root).expect("render");

        // (3, 3) is base-layout territory: outside the centered 72x26
        // palette box, inside the transcript.
        for _ in 0..2 {
            app.handle_input(wheel_up_at(3, 3));
        }

        writer.write_all(b"\x1b").expect("write esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(!shell.borrow().overlays.borrow().is_open());

        let rows = flatten(&shell.borrow_mut().draw(&full_draw_ctx()));
        assert!(
            rows.iter().any(|row| row.contains("line-079")),
            "tail still in view, the wheel never reached the list: {rows:?}"
        );
    }

    // ---- Config selectors and settings windows (8D-2) ----

    /// Build a world, a shell over its chat, and an initialized app, so a test
    /// can open a config-editing overlay from the host path and then drive it
    /// through real key dispatch. The overlay is focused via the same refocus
    /// app event the drive loop posts.
    async fn world_shell_app(
        dir: &TempDir,
        demo: &str,
        layers: ConfigLayers,
    ) -> (World, Rc<RefCell<Shell>>, AsyncApp, PipeWriter, WidgetRef) {
        let world = scripted_world_with_layers(dir, demo, layers).await;
        let shell = Rc::new(RefCell::new(Shell::new(
            Rc::clone(&world.chat),
            Rc::clone(&world.status),
            world.core.message_queues.clone(),
            ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor)),
            "aj-next".to_string(),
            PathBuf::from("/tmp"),
        )));
        let root: WidgetRef = to_widget_ref(Rc::clone(&shell));
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        writer.write_all(b"\x1b[?c").expect("write DA1 reply");
        let mut app = AsyncApp::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
            reader.into(),
        );
        app.init(Rc::clone(&root), Options::default())
            .await
            .expect("init");
        (world, shell, app, writer, root)
    }

    /// Post the refocus app event and render, moving focus onto the overlay
    /// the host just opened (the drive loop does this after
    /// `ActionEffect::OpenedOverlay`).
    fn focus_overlay(app: &mut AsyncApp, root: &WidgetRef) {
        app.post_app_event(UserEvent {
            name: REFOCUS_OVERLAY_EVENT.to_string(),
            data: None,
        });
        app.render(root).expect("render");
    }

    /// A watcher for a bundled theme is inert (no on-disk source), which is
    /// all the apply path needs for tests that don't exercise reloads.
    fn inert_theme_watch() -> ThemeWatch {
        ThemeWatch::install("dark")
    }

    /// Pin `$HOME` to a scratch dir for the test, restoring it on drop, so
    /// user-layer persistence writes into a tempdir rather than the real
    /// `~/.aj`. Paired with `#[serial]` since env mutation is process-wide.
    struct HomeGuard {
        prior: Option<String>,
    }

    impl HomeGuard {
        fn set(path: &std::path::Path) -> HomeGuard {
            let prior = std::env::var("HOME").ok();
            // SAFETY: `#[serial]` keeps other threads out; Drop restores it.
            unsafe {
                std::env::set_var("HOME", path);
            }
            HomeGuard { prior }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prior {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    /// The thinking selector, driven through real dispatch: open from the
    /// host path, filter to `high`, confirm. The change updates the footer and
    /// stages the run config, is recorded on the session log, and (session
    /// scoped) leaves the user config untouched.
    #[tokio::test]
    async fn thinking_selector_updates_footer_and_is_session_scoped() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;

        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::OpenThinkingSelector).await,
            ActionEffect::OpenedOverlay
        ));
        focus_overlay(&mut app, &root);
        assert!(shell.borrow().overlays.borrow().is_open());

        // Filter to "high" (the exact match ranks first) and confirm.
        writer.write_all(b"high\r").expect("write query + enter");
        for _ in 0..5 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "confirm closed the selector"
        );

        let activity = shell.borrow().take_activity();
        assert_eq!(activity.len(), 1, "confirm parked one thinking change");
        let mut watch = inert_theme_watch();
        apply_selector_activity(&mut world, &shell, &mut watch, activity).await;

        // The footer reflects the pick immediately.
        assert_eq!(
            world
                .chat
                .borrow()
                .footers()
                .settings(AgentId::Main)
                .map(|s| s.thinking.clone()),
            Some("high".to_string())
        );
        // The run config staged it for the next turn.
        assert_eq!(
            world.run_config.lock().unwrap().thinking,
            Some(ThinkingConfig::High)
        );
        // Session-scoped: the user config layer's default is unchanged (still
        // the `Config::default` value, not the picked `high`).
        assert_eq!(
            world.config_layers.lock().unwrap().user.thinking,
            Config::default().thinking,
            "the persisted default was left untouched"
        );
        // But the session log records it so a resume restores it.
        let recorded = {
            world
                .core
                .log
                .lock()
                .await
                .latest_leaf(ThreadFilter::USER)
                .is_some()
        };
        assert!(recorded, "thinking change recorded on the session log");
    }

    /// The model selector's confirm updates the footer identity and is
    /// session-scoped (the user config layer stays untouched).
    #[tokio::test]
    async fn model_confirm_updates_footer_and_is_session_scoped() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, _app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        let info = world.catalog.first().cloned().expect("catalog non-empty");

        let mut watch = inert_theme_watch();
        apply_selector_activity(
            &mut world,
            &shell,
            &mut watch,
            vec![SelectorActivity::ModelConfirmed {
                target: AgentId::Main,
                info: Box::new(info.clone()),
            }],
        )
        .await;

        let settings = world
            .chat
            .borrow()
            .footers()
            .settings(AgentId::Main)
            .cloned()
            .expect("footer entry");
        assert_eq!(settings.provider, info.provider);
        assert_eq!(settings.model_id, info.id);
        // Session-scoped: no persisted default.
        let layers = world.config_layers.lock().unwrap();
        assert!(layers.user.model_api.is_none());
        assert!(layers.user.model_name.is_none());
    }

    /// The settings window, driven through real dispatch: open it, filter to
    /// the thinking row, open its picker submenu, pick `high`. The change
    /// stages the run config and persists `thinking = "high"` to the tempdir
    /// user config, while the window stays open.
    #[tokio::test]
    #[serial_test::serial]
    async fn settings_window_persists_thinking_to_user_config() {
        let dir = TempDir::new().expect("tempdir");
        let _home = HomeGuard::set(dir.path());
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;

        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::OpenSettings).await,
            ActionEffect::OpenedOverlay
        ));
        focus_overlay(&mut app, &root);

        // Filter to the thinking row and open its picker submenu.
        writer.write_all(b"thinking\r").expect("filter + enter");
        for _ in 0..9 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
        // The picker opened in dispatch and moved focus; render so its focus
        // path lands before the next keys.
        app.render(&root).expect("render");
        writer.write_all(b"high\r").expect("pick + enter");
        for _ in 0..5 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }

        let activity = shell.borrow().take_activity();
        assert_eq!(activity.len(), 1, "one settings change staged");
        let mut watch = inert_theme_watch();
        apply_selector_activity(&mut world, &shell, &mut watch, activity).await;

        // Staged into the run config for the next turn.
        assert_eq!(
            world.run_config.lock().unwrap().thinking,
            Some(ThinkingConfig::High)
        );
        // Persisted to the tempdir user config.toml.
        let config_path = dir.path().join(".aj").join("config.toml");
        let contents = std::fs::read_to_string(&config_path).expect("config.toml written");
        assert!(contents.contains("thinking = \"high\""), "got: {contents}");
        // The window stays open across an edit.
        assert!(shell.borrow().overlays.borrow().is_open());
    }

    /// The project settings window persists an override to the project config
    /// file, and the clear chord removes it, reverting the effective value to
    /// the user default.
    #[tokio::test]
    async fn project_settings_persist_then_clear() {
        let dir = TempDir::new().expect("tempdir");
        let project_path = dir.path().join("repo").join(".aj").join("config.toml");
        let layers = ConfigLayers {
            user: Config::default(),
            project: aj_conf::ConfigLayer::default(),
            project_path: Some(project_path.clone()),
        };
        let (world, shell, _app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", layers).await;
        let mut watch = inert_theme_watch();

        // Set a project override for a plain bool option.
        apply_setting_change(
            &world,
            &shell,
            &mut watch,
            PersistAction::ProjectSet,
            "auto_compact",
            "false",
        )
        .await;
        let contents = std::fs::read_to_string(&project_path).expect("project config written");
        assert!(contents.contains("auto_compact = false"), "got: {contents}");
        assert!(
            !world.config.lock().unwrap().auto_compact,
            "effective config picks up the override"
        );

        // Clear it: the key is removed and the effective value reverts to the
        // user default (true).
        apply_setting_change(
            &world,
            &shell,
            &mut watch,
            PersistAction::ProjectClear,
            "auto_compact",
            "true",
        )
        .await;
        let contents = std::fs::read_to_string(&project_path).expect("project config present");
        assert!(
            !contents.contains("auto_compact"),
            "override cleared: {contents}"
        );
        assert!(
            world.config.lock().unwrap().auto_compact,
            "effective reverts to the user default"
        );
    }

    /// Project settings outside a git repo (no project path) folds a notice
    /// rather than opening the window.
    #[tokio::test]
    async fn project_settings_without_a_repo_folds_a_notice() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::OpenProjectSettings).await,
            ActionEffect::Redraw
        ));
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "no window opened"
        );
        let notices: Vec<String> = world
            .chat
            .borrow()
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Notice(n) => Some(n.text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notices.iter().any(|n| n.contains("git repository")),
            "{notices:?}"
        );
    }

    /// A speed change whose provider rebuild fails (the scripted provider is
    /// not in the registry) reverts the settings row to the previous value.
    #[tokio::test]
    async fn speed_change_failure_reverts_the_row() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let mut watch = inert_theme_watch();

        // Open the settings window so the revert has a live row to fix.
        apply_command(&mut world, &shell, CommandAction::OpenSettings).await;
        // Simulate the widget's optimistic edit before the apply fails.
        {
            let ui = shell.borrow();
            let ui = ui.settings_ui.borrow();
            ui.as_ref()
                .unwrap()
                .list
                .borrow()
                .set_value("speed", "fast");
        }

        let notice = apply_setting_change(
            &world,
            &shell,
            &mut watch,
            PersistAction::User,
            "speed",
            "fast",
        )
        .await
        .expect("speed apply returns a notice");
        assert!(notice.contains("Failed to set speed"), "got: {notice}");

        // The row reverted to the still-active speed.
        let reverted = {
            let ui = shell.borrow();
            let ui = ui.settings_ui.borrow();
            ui.as_ref().unwrap().list.borrow().value_of("speed")
        };
        assert_eq!(reverted.as_deref(), Some("standard"));
    }

    /// A skills toggle persists into `disabled_skills` on the user layer.
    #[tokio::test]
    #[serial_test::serial]
    async fn skill_toggle_persists_to_disabled_skills() {
        let dir = TempDir::new().expect("tempdir");
        let _home = HomeGuard::set(dir.path());
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let mut watch = inert_theme_watch();

        apply_selector_activity(
            &mut world,
            &shell,
            &mut watch,
            vec![SelectorActivity::SkillToggle {
                name: "demo-skill".to_string(),
                disable: true,
            }],
        )
        .await;

        assert!(
            world
                .config_layers
                .lock()
                .unwrap()
                .user
                .disabled_skills
                .iter()
                .any(|s| s == "demo-skill"),
            "the skill is disabled in the user layer"
        );
        let config_path = dir.path().join(".aj").join("config.toml");
        let contents = std::fs::read_to_string(&config_path).expect("config.toml written");
        assert!(contents.contains("demo-skill"), "got: {contents}");
    }

    // ---- Agent picker, task viewer, prompt history (8D-3a) ----

    /// A no-op output source, enough for the registry to resolve a task.
    struct NoOutput;

    impl aj_agent::tool::TaskOutputSource for NoOutput {
        fn snapshot(&self) -> aj_agent::tool::TaskRead {
            aj_agent::tool::TaskRead::default()
        }
    }

    /// Register a running background bash task and return its id.
    fn register_bash_task(world: &World, command: &str) -> aj_agent::tool::TaskId {
        let (id, _cancel) = world.core.task_registry.register(
            AgentId::Main,
            aj_agent::tool::TaskKind::Bash {
                command: command.to_string(),
            },
            command.to_string(),
            Arc::new(NoOutput),
        );
        id
    }

    /// Fold a running sub-agent and a running bash task into the world's
    /// chat model through the reducer, so a picker snapshot lists both.
    fn seed_sub_and_task(world: &mut World) {
        let settings = aj_agent::events::AgentSettings {
            provider: "scripted".into(),
            model_id: "scripted".into(),
            thinking: "off".into(),
            speed: "standard".into(),
            verbosity: "default".into(),
        };
        let events = [
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "do the thing".into(),
                background: false,
                settings,
            },
            AgentEvent::TaskStart {
                agent_id: AgentId::Main,
                task_id: 1,
                call_id: "tu-1".into(),
                kind: aj_agent::tool::TaskKind::Bash {
                    command: "cargo build".into(),
                },
                label: "cargo build".into(),
            },
        ];
        for event in events {
            let _ = reduce(
                &mut world.chat.borrow_mut(),
                &mut world.core.lifecycle,
                event,
            );
        }
    }

    fn main_notices(world: &World) -> Vec<String> {
        world
            .chat
            .borrow()
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::Notice(n) => Some(n.text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The picker snapshot lists the main agent, a running sub-agent, and
    /// a running bash task, all from the chat model.
    #[tokio::test]
    async fn agent_picker_snapshot_lists_main_sub_and_running_task() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;
        seed_sub_and_task(&mut world);

        let snapshot = PickerSnapshot::gather(&world.chat.borrow());
        assert!(snapshot.agents.iter().any(|a| a.id == AgentId::Main));
        assert!(
            snapshot.agents.iter().any(|a| a.id == AgentId::Sub(1)
                && a.status == Some(aj_app::chat::SubAgentStatus::Running)),
            "running sub listed: {:?}",
            snapshot.agents
        );
        assert_eq!(snapshot.tasks.len(), 1, "one bash task");
        assert_eq!(
            snapshot.tasks[0].status,
            aj_agent::tool::TaskStatus::Running
        );
        assert!(snapshot.tasks[0].command.contains("cargo build"));
        assert_eq!(snapshot.active, AgentId::Main);
    }

    /// The `OpenAgentPicker` command opens the picker overlay from the
    /// host path.
    #[tokio::test]
    async fn open_agent_picker_command_opens_the_overlay() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        seed_sub_and_task(&mut world);
        let effect = apply_command(&mut world, &shell, CommandAction::OpenAgentPicker).await;
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        assert!(shell.borrow().overlays.borrow().is_open());
    }

    /// Observing an agent from the picker switches the viewed transcript.
    #[tokio::test]
    async fn agent_picker_observe_switches_the_view() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        assert_eq!(world.chat.borrow().active_view(), AgentId::Main);
        let effect = apply_picker_outcome(
            &mut world,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Sub(2)),
        );
        assert!(matches!(effect, ActionEffect::Redraw));
        assert_eq!(world.chat.borrow().active_view(), AgentId::Sub(2));
    }

    /// Drilling into a task opens the viewer overlay; an id that has left
    /// the registry folds a notice instead.
    #[tokio::test]
    async fn agent_picker_open_task_opens_the_viewer() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let id = register_bash_task(&world, "cargo test");

        let effect = apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::OpenTask(id));
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        assert!(shell.borrow().overlays.borrow().is_open(), "viewer open");

        let effect = apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::OpenTask(9_999));
        assert!(matches!(effect, ActionEffect::Redraw));
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n.contains("no longer available")),
            "gone task folds a notice"
        );
    }

    /// Killing a task consults the live status: running kills and notes
    /// it, terminal reports already-finished, unknown reports gone.
    #[tokio::test]
    async fn agent_picker_kill_folds_notice_by_live_status() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let id = register_bash_task(&world, "sleep 100");

        apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::Kill(id));
        world
            .core
            .task_registry
            .set_status(id, aj_agent::tool::TaskStatus::Killed);
        apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::Kill(id));
        apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::Kill(9_999));

        let notices = main_notices(&world);
        assert!(
            notices
                .iter()
                .any(|n| n.contains("Killing background task")),
            "{notices:?}"
        );
        assert!(
            notices.iter().any(|n| n.contains("already finished")),
            "{notices:?}"
        );
        assert!(
            notices.iter().any(|n| n.contains("not in the registry")),
            "{notices:?}"
        );
    }

    /// The task viewer, opened from the host path, renders the task's
    /// output and status. (`Ctrl+K`/close behavior is covered by the
    /// widget's own tests.)
    #[tokio::test]
    async fn task_viewer_renders_output_from_the_registry() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let id = register_bash_task(&world, "echo hello");
        apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::OpenTask(id));
        // The viewer shows the command header and a running status.
        let rendered = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(rendered.contains("echo hello"), "command: {rendered}");
        assert!(rendered.contains("running"), "status: {rendered}");
    }

    /// Prompt history opens showing a loading placeholder, fills from a
    /// scan, filters, and confirming recalls the full prompt into the
    /// editor without submitting.
    #[tokio::test]
    async fn prompt_history_opens_loading_fills_filters_and_recalls() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;

        let effect = apply_command(&mut world, &shell, CommandAction::OpenPromptHistory).await;
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        focus_overlay(&mut app, &root);

        // The initial current-workspace scan was parked; before it fills,
        // the list shows the loading placeholder.
        let fetch = shell
            .borrow()
            .take_history_fetch()
            .expect("initial scan parked");
        assert_eq!(fetch.scope, HistoryScope::Workspace);
        let loading = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(loading.contains("Loading"), "loading state: {loading}");

        // Fill the list as the drive loop would once the scan lands.
        fetch
            .select
            .borrow()
            .set_items(crate::prompt_history::build_items(&[
                PromptEntry {
                    text: "fix the bug".into(),
                    project: None,
                },
                PromptEntry {
                    text: "add a test".into(),
                    project: None,
                },
            ]));
        app.render(&root).expect("render");

        // Filter to "test" (only the second prompt matches) and confirm.
        writer.write_all(b"test\r").expect("query + enter");
        for _ in 0..5 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }

        let text = shell
            .borrow()
            .take_recall()
            .expect("confirm recalled a prompt");
        assert_eq!(text, "add a test");
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "confirm closed the overlay"
        );

        // Recall drops the full text into the editor (what the drive loop
        // does with the parked recall).
        recall_into_editor(&shell, &text);
        assert_eq!(
            shell.borrow().editor.borrow().cursor(),
            (0, "add a test".chars().count()),
            "the recalled prompt is in the editor"
        );
    }

    /// Ctrl+T in the prompt-history overlay reparks a scan for the other
    /// scope.
    #[tokio::test]
    async fn prompt_history_ctrl_t_reparks_the_all_scope_scan() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        apply_command(&mut world, &shell, CommandAction::OpenPromptHistory).await;
        focus_overlay(&mut app, &root);
        // Drop the initial workspace scan.
        shell.borrow().take_history_fetch();

        writer.write_all(&[0x14]).expect("ctrl+t");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);

        let fetch = shell
            .borrow()
            .take_history_fetch()
            .expect("toggle parked a scan");
        assert_eq!(fetch.scope, HistoryScope::All);
    }

    // ---- Session selector, new session, rebuild loop (8D-3b-i) ----

    /// Build a scripted session over `dir`'s shared persistence, run one
    /// prompt turn to completion so the session file carries a first user
    /// message, and return its session id. Seeds the selector and the
    /// rebuild loop with real on-disk sessions.
    async fn create_disk_session(dir: &TempDir, prompt: &str) -> String {
        let mut world = scripted_world(dir, "streaming-text").await;
        run_prompt(&mut world, prompt).await;
        let id = world.core.session_id.clone();
        aj_app::shutdown_background_tasks(&world.core.task_registry).await;
        world.turns.shutdown().await;
        id
    }

    /// Submit `prompt` into `world`, settle the turn, and drain its events
    /// into the chat model (and, via the persistence listener, to disk).
    async fn run_prompt(world: &mut World, prompt: &str) {
        handle_submit(world, prompt.to_string());
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(world, joined).expect("turn settles");
        while let Ok(event) = world.core.event_rx.try_recv() {
            let _ = drain_events(world, event);
        }
    }

    /// The `NewSession` command parks a new-session request while idle; the
    /// drive loop turns that into `SessionExit::New`.
    #[tokio::test]
    async fn new_session_command_parks_a_new_request() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::NewSession).await,
            ActionEffect::Redraw
        ));
        assert!(matches!(
            shell.borrow().take_session_request(),
            Some(SessionRequest::New)
        ));
    }

    /// Both session-changing commands are refused while a turn runs (aj's
    /// spirit: rebuilding the world under a live turn would strand it). The
    /// refusal folds a notice and parks nothing.
    #[tokio::test]
    async fn session_commands_refused_mid_turn() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        handle_submit(&mut world, "go".to_string());
        assert!(world.turn_cancels.contains_key(&AgentId::Main), "busy");

        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::OpenSessionSelector).await,
            ActionEffect::Redraw
        ));
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "no selector opened mid-turn"
        );
        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::NewSession).await,
            ActionEffect::Redraw
        ));
        assert!(
            shell.borrow().take_session_request().is_none(),
            "no switch parked mid-turn"
        );

        let notices = main_notices(&world);
        assert!(
            notices.iter().any(|n| n.contains("switch sessions")),
            "{notices:?}"
        );
        assert!(
            notices.iter().any(|n| n.contains("start a new session")),
            "{notices:?}"
        );

        // Settle the turn so teardown is clean.
        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
    }

    /// The selector opens showing a loading placeholder, fills from a real
    /// persistence scan, tags the current session, and confirming a
    /// different row parks a resume request the drive loop turns into
    /// `SessionExit::Switch`.
    #[tokio::test]
    async fn session_selector_fills_and_confirms_a_switch() {
        let dir = TempDir::new().expect("tempdir");
        let alpha = create_disk_session(&dir, "alpha session prompt").await;
        std::thread::sleep(std::time::Duration::from_millis(2));

        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        // Give the current session recognizable on-disk content so its row
        // scans in and can carry the `(current)` tag.
        run_prompt(&mut world, "current session prompt").await;

        let effect = apply_command(&mut world, &shell, CommandAction::OpenSessionSelector).await;
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        focus_overlay(&mut app, &root);
        let scan = shell
            .borrow()
            .take_session_scan()
            .expect("open parked a preview scan");
        let loading = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(loading.contains("Loading"), "loading state: {loading}");

        // Run the scan synchronously (what `spawn_session_scan` does off the
        // loop) and fill, as the drive loop's fill arm would.
        let mut previews = Vec::new();
        world
            .persistence
            .list_session_previews_streaming(&mut |batch| previews.extend(batch));
        assert!(
            previews.len() >= 2,
            "alpha + the current session are on disk: {}",
            previews.len()
        );
        extend_session_scan(&scan, &previews, Utc::now(), true, true);
        app.render(&root).expect("render");

        let rows = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(rows.contains("(current)"), "current session tagged: {rows}");

        // Filter to alpha and confirm; the switch request is parked and the
        // overlay closes.
        writer.write_all(b"alpha session\r").expect("query + enter");
        for _ in 0..14 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
        assert!(
            matches!(
                shell.borrow().take_session_request(),
                Some(SessionRequest::Resume(id)) if id == alpha
            ),
            "confirming a different session parks a resume for its id"
        );
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "confirm closed the selector"
        );
    }

    /// Confirming the pre-selected current session is a no-op close (parks
    /// nothing); Esc cancels the same way.
    #[tokio::test]
    async fn session_selector_current_is_noop_and_esc_cancels() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "current session prompt").await;

        // Confirm the pre-selected current row: no switch parked.
        apply_command(&mut world, &shell, CommandAction::OpenSessionSelector).await;
        focus_overlay(&mut app, &root);
        let scan = shell.borrow().take_session_scan().expect("scan parked");
        let mut previews = Vec::new();
        world
            .persistence
            .list_session_previews_streaming(&mut |batch| previews.extend(batch));
        extend_session_scan(&scan, &previews, Utc::now(), true, true);
        app.render(&root).expect("render");
        writer.write_all(b"\r").expect("enter on the current row");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(
            shell.borrow().take_session_request().is_none(),
            "the current row is a no-op"
        );
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "confirm closed the selector"
        );

        // Re-open and Esc: cancels without parking a switch.
        apply_command(&mut world, &shell, CommandAction::OpenSessionSelector).await;
        focus_overlay(&mut app, &root);
        shell.borrow().take_session_scan();
        writer.write_all(b"\x1b").expect("esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(
            shell.borrow().take_session_request().is_none(),
            "esc parks nothing"
        );
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "esc closed the selector"
        );
    }

    /// The rebuild path: a switch tears the running session down and builds
    /// the next over the same Shell, rebinding by content-swap so the
    /// transcript renders the new session's model and the pending box reads
    /// the new agent's queues. The outgoing session's usage accumulates for
    /// the shutdown banner.
    #[tokio::test]
    async fn switch_rebuilds_the_session_and_accumulates_usage() {
        let dir = TempDir::new().expect("tempdir");
        let beta = create_disk_session(&dir, "beta session prompt").await;
        std::thread::sleep(std::time::Duration::from_millis(2));

        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "alpha session prompt").await;
        let alpha_id = world.core.session_id.clone();

        // Snapshot the outgoing usage, as the outer loop does before it
        // rebuilds.
        let mut completed: Vec<(String, UsageSummary)> = Vec::new();
        completed.push((alpha_id.clone(), world.core.usage_summary().await));

        // Switch to beta: build then install over the same Shell.
        let next = build_next_session(
            &world,
            SessionSpec::Resume {
                session_id: beta.clone(),
                entry: SessionEntry::Switch,
            },
            &alpha_id,
        )
        .await
        .expect("build beta");
        install_next_session(&mut world, &shell, next);

        assert_eq!(world.core.session_id, beta, "world rebuilt onto beta");
        // The transcript renders beta's replayed content, not alpha's, which
        // proves every chrome widget follows the content-swapped chat cell.
        let rows = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(
            rows.contains("beta session prompt"),
            "beta content shown: {rows}"
        );
        assert!(
            !rows.contains("alpha session prompt"),
            "alpha content gone after the swap: {rows}"
        );
        // The header id followed the swap.
        assert_eq!(
            shell.borrow().header.borrow().text,
            format!("aj-next — {beta}")
        );
        // The pending box reads the new agent's queues (rebound on the
        // swap), so a message queued on the new core previews.
        world
            .core
            .message_queues
            .append_follow_up(AgentId::Main, "queued after switch");
        let pending = Rc::clone(&shell.borrow().pending);
        let pending_rows = crate::test_support::rows(
            &pending
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(80, None)),
        );
        assert!(
            pending_rows.join("\n").contains("queued after switch"),
            "pending box repointed to the new queues: {pending_rows:?}"
        );
        world.core.message_queues.clear(AgentId::Main);

        // Switch again, this time to a fresh session; usage keeps
        // accumulating and the new session's transcript is empty.
        completed.push((
            world.core.session_id.clone(),
            world.core.usage_summary().await,
        ));
        let prev = world.core.session_id.clone();
        let next = build_next_session(
            &world,
            SessionSpec::Create {
                entry: SessionEntry::Switch,
            },
            &prev,
        )
        .await
        .expect("build fresh");
        install_next_session(&mut world, &shell, next);

        assert_ne!(world.core.session_id, beta, "a fresh session was minted");
        let fresh_rows = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(
            !fresh_rows.contains("beta session prompt"),
            "fresh session opens empty: {fresh_rows}"
        );

        // The banner itemizes both completed sessions in order (aj's
        // accumulation), then the live one.
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0].0, alpha_id);
        assert_eq!(completed[1].0, beta);
        // Formatting the banner over the accumulated list must not panic.
        print_exit_banner(&world, &completed, true).await;
    }
}
