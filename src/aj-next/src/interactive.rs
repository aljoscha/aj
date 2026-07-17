//! The interactive alt-screen shell, driven by `vxfw::AsyncApp`.
//!
//! The base layout from the alt-screen UX spec: a one-line header, a
//! flex-filling transcript, an editor, and a one-line footer, stacked
//! in a `FlexColumn`. A real agent session backs the shell: prompts
//! submitted from the editor spawn turns through the shared
//! `aj_app::turn` helpers, agent events fold into the [`ChatState`]
//! model, and the [`TranscriptView`] renders it with follow-tail.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
use aj_models::{ThinkingConfig, speed_from_name, speed_name, thinking_config_from_name};
use aj_session::{
    ConversationPersistence, PromptEntry, SessionPreview, ThreadFilter, project_thread,
    replay_deferring_subs,
};
use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use vaxis::cell::{Color, Style};
use vaxis::key::{Key, Modifiers};
use vaxis::tty::PosixTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    AsyncApp, AutocompleteDelivery, DrawContext, EditorTheme, Event, EventContext,
    FilterableSelect, FlexColumn, FlexItem, FrameStats, KeymapController, ListView, MaxSize,
    Options, PopupStyle, RelativePoint, Size, SubSurface, Surface, Text, TextArea, UserEvent,
    Widget, WidgetRef, draw_widget, to_widget_ref,
};

use crate::agent_picker::{AgentPickerOutcome, PickerSnapshot, open_agent_picker};
use crate::content_overlay::{ContentStyles, Row, auth_rows, session_info_rows, set_rows};
use crate::copied_toast::Copied;
use crate::footer::FooterLine;
use crate::frame_stats_box::FrameStatsBox;
use crate::keymap::{HostCtx, build_keymap};
use crate::login::{
    AuthPickerRequest, AuthRow, DialogCallbacks, LoginDialogState, open_login_dialog,
    open_login_picker, open_logout_picker,
};
use crate::overlay::{MouseBlocker, OverlayChrome, OverlayStack, Scrim};
use crate::palette::{FetchKind, PendingFetch, open_palette};
use crate::pending::PendingBox;
use crate::prompt_history::{HistoryFetch, HistoryScope, MAX_ENTRIES, open_prompt_history};
use crate::quit_hint::QuitHint;
use crate::session_selector::{SessionScan, extend_session_scan, open_session_selector};
use crate::session_tree::{build_tree_rows, open_session_tree};
use crate::settings_ui::{
    MODEL_SETTING_ID, SelectorActivity, SettingsCatalogs, SettingsUi, SettingsValues, SkillRow,
    SkillsFill, UNSET_VALUE, build_skill_rows, open_model, open_settings, open_skills,
    open_thinking, skills_placeholder_row,
};
use crate::splash::{SPLASH_WAKE_EVENT, Splash};
use crate::status::{STATUS_WAKE_EVENT, StatusLine, StatusState};
use crate::task_output::open_task_output;
use crate::toasts::{ToastStack, Toasts, busy_refusal};
use crate::transcript::{TranscriptStyles, TranscriptView, vaxis_color};
use crate::usage_overlay::open_usage_overlay;

/// App-event name the drive loop posts after opening an overlay outside
/// dispatch. The Shell handles it by moving focus onto the top overlay: the
/// drive loop owns the session world but has no [`EventContext`] to move focus
/// itself, so it delegates the focus move to the shell via this event.
const REFOCUS_OVERLAY_EVENT: &str = "aj-next.refocus-overlay";

/// App-event name the host posts after a session switch so the Shell
/// retitles the terminal from its capturing phase. The switch runs in the
/// drive loop, which has no [`EventContext`] to queue the title command
/// itself, so it delegates to the shell the same way [`REFOCUS_OVERLAY_EVENT`]
/// delegates the focus move.
const SET_TITLE_EVENT: &str = "aj-next.set-title";

/// The app name aj-next brands its terminal window title with, lowercase.
const APP_TITLE: &str = "aj";

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
    /// The resumed sub-agent indices whose transcript is not yet
    /// materialized. A sub-agent enters the set in the resume drain and
    /// leaves it the first time it is observed. Live-spawned sub-agents
    /// are never in it, since they build their transcript from the live
    /// event stream.
    deferred_subs: HashSet<usize>,
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
            head: None,
        },
        Some(Command::Continue {
            session_id: None,
            prompt: _,
        }) => match persistence.get_latest_session_id()? {
            Some(latest) => SessionSpec::Resume {
                session_id: latest,
                entry: SessionEntry::Startup,
                head: None,
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
    //
    // Deferred replay withholds each sub-agent's content events but still
    // emits its `SubAgentStart`/`SubAgentEnd`, so every sub-agent box is
    // built (and `Done` with its report) while its transcript stays empty
    // until observed. We record which indices were deferred so Observe
    // can materialize them on demand.
    let mut deferred_subs = HashSet::new();
    {
        let log = Arc::clone(&core.log);
        let log = log.lock().await;
        for event in replay_deferring_subs(&log) {
            if let AgentEvent::SubAgentStart {
                child: AgentId::Sub(n),
                ..
            } = &event
            {
                deferred_subs.insert(*n);
            }
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
        deferred_subs,
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
    /// The resumed sub-agent indices whose transcript is deferred, seeded
    /// by this build's drain and swapped onto the world in
    /// [`install_next_session`]. See [`World::deferred_subs`].
    deferred_subs: HashSet<usize>,
    notices: Vec<String>,
    /// Whether the requested build failed and this session is the
    /// previous-session fallback. The branch flow reads it (with
    /// `head_override_applied`) to decide whether auto-submitting the branch
    /// prompt is safe.
    fell_back: bool,
    /// The build's head-override outcome, forwarded from
    /// [`aj_app::session::MainAgentSeed`]. `None` when none was requested,
    /// `Some(true)` when installed, `Some(false)` when stale.
    head_override_applied: Option<bool>,
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
    branch: bool,
) -> Result<NextSession> {
    let config = world.config.lock().expect("config mutex poisoned").clone();
    let (mut core, seed, notice, is_fresh, fell_back) = match SessionCore::build(
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
                false,
            )
        }
        Err(err) => {
            // The requested build failed. Fall back to the session that
            // just ended so the user keeps a live world, and report why.
            let failure = switch_failure_notice(&spec, &err, branch);
            let fallback = SessionSpec::Resume {
                session_id: previous_id.to_string(),
                entry: SessionEntry::Switch,
                head: None,
            };
            let (core, seed) = SessionCore::build(
                &config,
                &world.run_config,
                &world.persistence,
                &fallback,
                world.restore.as_ref(),
            )?;
            // The fallback resumes an existing session, so it is never fresh.
            (core, seed, failure, false, true)
        }
    };
    let head_override_applied = seed.head_override_applied;

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
    // Deferred replay withholds sub-agent content but still builds every
    // sub-agent box, so the seeded set records which indices Observe must
    // materialize (see `build_world`).
    let mut deferred_subs = HashSet::new();
    {
        let log = Arc::clone(&core.log);
        let log = log.lock().await;
        for event in replay_deferring_subs(&log) {
            if let AgentEvent::SubAgentStart {
                child: AgentId::Sub(n),
                ..
            } = &event
            {
                deferred_subs.insert(*n);
            }
            let _ = reduce(&mut chat, &mut core.lifecycle, event);
        }
    }

    // Order: the switch/create confirmation, then (for a fresh switch) the
    // context listing folded as an Info notice, then any resume-restore
    // notices. The confirmation is the switch acknowledgment, so context
    // follows it rather than leading. The caller folds these after install so
    // they sit on top of the replayed history.
    //
    // A successful branch build leads with no confirmation: its wording
    // depends on the prompt handoff and the head-apply outcome, which only the
    // run loop knows, so the run loop inserts it after this build returns (see
    // `apply_branch_switch_notice`). A build fallback still leads with its
    // failure notice.
    let mut notices = Vec::new();
    if !(branch && !fell_back) {
        notices.push(notice);
    }
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
        deferred_subs,
        notices,
        fell_back,
        head_override_applied,
    })
}

/// Confirmation notice for a successful New/Switch session change, matching
/// aj. A branch rebuild's confirmation is decided by the run loop instead (see
/// [`apply_branch_switch_notice`]), so it never flows through here.
fn switch_notice(spec: &SessionSpec, session_id: &str) -> String {
    match spec {
        SessionSpec::Create { .. } => format!("Started a fresh session ({session_id})."),
        SessionSpec::Resume { session_id, .. } => format!("Switched to session {session_id}."),
    }
}

/// Failure notice when a requested session change couldn't be built (the
/// host falls back to resuming the previous session), matching aj.
fn switch_failure_notice(spec: &SessionSpec, err: &anyhow::Error, branch: bool) -> String {
    if branch {
        return format!("Failed to branch the conversation: {err}");
    }
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
    // Swap the deferred-sub set together with the chat, so a prior
    // session's indices can't leak into the new session's Observe path.
    world.deferred_subs = next.deferred_subs;
    // Status is resynced from the new core once per iteration; reset it so
    // the frame between install and the next sync shows idle chrome.
    *world.status.borrow_mut() = StatusState::default();
    world.core = next.core;
    // Clear any armed branch anchor: the shell and its slots survive session
    // rebuilds, so without this a stale anchor could resolve against the new
    // session's log (and with legacy 8-hex ids even hit a wrong entry). Covers
    // every install path (New, Switch, and the branch rebuild itself).
    shell.borrow().disarm_branch();
    // A session change is only requested with no turn in flight (the outer
    // loop shut the outgoing turns down, and the guard refuses mid-turn
    // requests), so this is already empty; clear defensively.
    world.turn_cancels.clear();
    // Start the switched-to session's splash box at the top: a prior session's
    // wheel scroll must not carry over.
    shell.borrow().splash.borrow_mut().reset_scroll();
    shell.borrow_mut().rebind(world);
    // Reconcile the editor chrome onto the freshly installed session. The
    // per-iteration reconcile runs at the bottom of `drive`, but `drive`
    // re-enters here with no prior render and paints its first frame at the top
    // of the loop, one iteration before that reconcile. The editor widget
    // persists across the chat swap, so without this the first frame would show
    // the outgoing session's baked border tint and stale `agent N` marker. This
    // mirrors the `world.status` reset above, which resets chrome for the same
    // install-to-first-draw window.
    sync_editor_chrome(world, shell);
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
/// [`StatusState`] cell, returning whether the animation tick should run
/// (see [`StatusState::animating`]). Called once per loop iteration right
/// before rendering, so every mutation path (event batch, turn join,
/// submits) shares one sync point and the mirror can't silently drift.
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
    next.animating()
}

/// Character budget for the branch-anchor footer preview, past which the
/// prefilled message is truncated with an ellipsis.
const BRANCH_PREVIEW_CHARS: usize = 40;

/// An armed branch anchor: the user pressed `b` on a focused user message, the
/// editor now holds that message, and a submit will rebuild the session at the
/// message's parent. Lives in a shell slot (the parked-slot pattern) so the
/// footer indicator reads it while the drive loop resolves it on submit.
#[derive(Clone)]
struct BranchAnchor {
    /// The focused user message's stable id, resolved against the log on
    /// submit to find the branch point (the message's parent).
    message_id: String,
}

/// Arm a branch anchor: store the message id and the footer indicator preview.
/// The two slots move in lockstep, so a set indicator is exactly "an anchor is
/// armed".
fn arm_branch(
    anchor: &Rc<RefCell<Option<BranchAnchor>>>,
    indicator: &Rc<RefCell<Option<String>>>,
    message_id: String,
    preview: String,
) {
    *anchor.borrow_mut() = Some(BranchAnchor { message_id });
    *indicator.borrow_mut() = Some(preview);
}

/// Clear both the branch-anchor resolution state and the footer indicator.
fn clear_branch(
    anchor: &Rc<RefCell<Option<BranchAnchor>>>,
    indicator: &Rc<RefCell<Option<String>>>,
) {
    *anchor.borrow_mut() = None;
    *indicator.borrow_mut() = None;
}

/// The footer indicator for an armed anchor: a label plus a one-line,
/// truncated preview of the prefilled message, so the pending branch behavior
/// is never a surprise.
fn branch_indicator_text(message: &str) -> String {
    let first_line = message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let chars: Vec<char> = first_line.chars().collect();
    let preview = if chars.len() > BRANCH_PREVIEW_CHARS {
        let mut s: String = chars[..BRANCH_PREVIEW_CHARS.saturating_sub(1)]
            .iter()
            .collect();
        s.push('\u{2026}');
        s
    } else {
        first_line.to_string()
    };
    format!("branching from message: {preview}")
}

/// The notice folded when a gesture incoherent with an armed branch anchor
/// (steer, dequeue) is attempted: it points the user at Esc to cancel.
fn branch_armed_notice(what: &str) -> String {
    format!("Can't {what} while branching \u{2014} press Esc to cancel the branch first.")
}

/// Handle an editor submit: spawn a prompt turn on the viewed agent
/// if it is idle, or queue the text as a follow-up while it is busy.
///
/// A queued message shows in the pending box (which reads the live
/// queue snapshot at draw) and is delivered by the post-turn wake:
/// `handle_turn_join` and the `AgentEnd` trigger in [`drain_events`]
/// both spawn a wake when `message_queues.has_pending`. History is
/// recorded by the callers (the drive loop and [`handle_steer`]), which
/// own the editor. Returns whether the message was accepted for delivery.
fn handle_submit(world: &mut World, text: String) -> bool {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return false;
    }
    let target = world.chat.borrow().active_view();
    if world.turn_cancels.contains_key(&target) || world.core.is_running(target) {
        world.core.message_queues.append_follow_up(target, &trimmed);
        return true;
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
    spawned
}

/// Submit editor text and return the transcript to its live tail when accepted.
fn handle_editor_submit(world: &mut World, shell: &Rc<RefCell<Shell>>, text: String) {
    if handle_submit(world, text) {
        shell.borrow().transcript.borrow_mut().resume_follow_tail();
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

/// Decide what a parked session request does at the drive-loop consumption
/// site: the [`SessionExit`] to break the loop with, or `None` to stay in the
/// session.
///
/// Every request (a new session, a selector resume, a tree-view branch
/// switch) rebuilds the session, which shuts its turns and background work
/// down. We refuse any of them while a turn or background task/sub-agent is
/// live rather than tear live work down silently. The request sites refuse
/// while busy up front (the overlays at confirm time, the `NewSession`
/// command in `apply_command_action`), but a request can still slip through
/// with work live: a background sub-agent finishing between that check and
/// this consumption spawns a parent wake turn (so `world.turns` is
/// non-empty), and the earlier `busy` snapshot is one drive-loop iteration
/// stale. This is the authoritative recheck.
fn consume_session_request(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    request: SessionRequest,
) -> Option<SessionExit> {
    let (agents, bash) =
        running_work_counts(world.turns.len(), &world.core.task_registry.snapshot());
    if agents + bash > 0 {
        // A toast, matching the request sites' up-front refuse.
        let what = match &request {
            SessionRequest::Branch { .. } => "switch branches",
            SessionRequest::Resume(_) => "switch sessions",
            SessionRequest::New => "start a new session",
        };
        shell.borrow().show_toast(busy_refusal(what));
        return None;
    }
    Some(request.into_exit())
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

/// Handle the clipboard-image paste chord: read the clipboard image to a
/// tempfile and insert its path at the editor cursor. Returns whether
/// anything changed.
///
/// Silent on failure (no clipboard image, unsupported backend), matching
/// `aj`.
fn paste_clipboard_image(shell: &Rc<RefCell<Shell>>) -> bool {
    let Some(path) = aj_app::clipboard::read_image_to_tempfile() else {
        tracing::debug!("clipboard: no image to paste");
        return false;
    };
    insert_pasted_image_path(&shell.borrow().editor, &path)
}

/// Insert a pasted clipboard-image path at the editor cursor as plain text
/// and return `true` (the editor changed).
///
/// NOTE: We insert the bare path, not an inline image attachment. The agent
/// opens it with `read_file` on submit, matching `aj`.
fn insert_pasted_image_path(editor: &Rc<RefCell<TextArea>>, path: &Path) -> bool {
    editor
        .borrow_mut()
        .insert_at_cursor(&path.display().to_string());
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
            shell.borrow().transcript.borrow_mut().resume_follow_tail();
        }
    } else if !text.is_empty() {
        shell.borrow().editor.borrow_mut().add_to_history(&text);
        handle_editor_submit(world, shell, text);
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
            // Steering is incoherent with an armed branch anchor: it would
            // consume the branch prompt as steering for the branch being
            // abandoned. Refuse and keep the anchor and editor text intact.
            if shell.borrow().branch_anchor.borrow().is_some() {
                fold_notice(world, &branch_armed_notice("steer"));
                return true;
            }
            handle_steer(world, shell);
            true
        }
        AjAction::Dequeue => {
            // Dequeueing is incoherent with an armed branch anchor: it would
            // splice queued text into the prefilled branch prompt. Refuse and
            // keep the anchor and editor text intact.
            if shell.borrow().branch_anchor.borrow().is_some() {
                fold_notice(world, &branch_armed_notice("dequeue a message"));
                return true;
            }
            yank_pending_into_editor(world, shell)
        }
        // Read the clipboard image to a tempfile and insert its path at the
        // editor cursor. Silent when there is no image, matching `aj`.
        AjAction::PasteImage => paste_clipboard_image(shell),
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
        | AjAction::BranchMessage
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

/// Where an armed branch anchor resolves against the log.
enum BranchTarget {
    /// The message resolved: rebuild on this head (the message's parent).
    Head(String),
    /// The message id is not in the log (a stale anchor, or a wrong-session
    /// resolve that the install-time clear should have prevented).
    Missing,
    /// The message is the file root: there is nothing to branch from.
    Root,
}

/// Resolve a branch anchor's message id to the new head by scanning the log's
/// entries for it and taking its `parent_id`. Locks the log, so call with no
/// turn in flight.
async fn resolve_branch_head(world: &World, message_id: &str) -> BranchTarget {
    let log = world.core.log.lock().await;
    match log
        .entries_in_order()
        .into_iter()
        .find(|e| e.id == message_id)
    {
        None => BranchTarget::Missing,
        Some(entry) => match &entry.parent_id {
            None => BranchTarget::Root,
            Some(parent) => BranchTarget::Head(parent.clone()),
        },
    }
}

/// The outcome of a submit made while a branch anchor is armed.
enum ArmedSubmit {
    /// Stay in the current session: the submit was refused (empty, or busy)
    /// or the resolution failed (missing / root). The anchor and any needed
    /// notice/toast are handled inside; the caller only redraws.
    Stay,
    /// Resolved: break the drive loop and rebuild the session onto `head`,
    /// running `prompt` as the branch's first turn.
    Branch { head: String, prompt: String },
}

/// Decide what an armed-anchor submit does. Called at the drive-loop submit
/// site only when an anchor is armed (the prompt has already been recorded to
/// history by the caller, so it survives a failed branch).
///
/// Refusals keep the anchor and restore the editor text (submit clears the
/// editor). Arming is always allowed; only the submit is gated here.
async fn submit_with_armed_anchor(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    text: String,
) -> ArmedSubmit {
    // Empty (post-trim): refuse and keep the anchor. The head must not move
    // for a prompt that would be dropped. The editor is already empty, so
    // there is nothing to restore.
    if text.trim().is_empty() {
        fold_notice(world, "Type a message to branch, or press Esc to cancel.");
        return ArmedSubmit::Stay;
    }
    // Busy: refuse and keep the anchor and text. A branch rebuilds the
    // session, which tears down any in-flight turn AND background work
    // (background sub-agents, detached bash tasks). We refuse while busy
    // rather than kill live work, matching the session-changing overlays.
    // Raise the toast and restore the text the submit cleared so the user
    // keeps it.
    let (agents, bash) =
        running_work_counts(world.turns.len(), &world.core.task_registry.snapshot());
    if agents + bash > 0 {
        shell.borrow().editor.borrow_mut().set_text(&text);
        shell.borrow().show_toast(busy_refusal("branch"));
        return ArmedSubmit::Stay;
    }
    // Resolve the anchor against the log.
    let message_id = shell
        .borrow()
        .branch_anchor
        .borrow()
        .as_ref()
        .map(|a| a.message_id.clone());
    let Some(message_id) = message_id else {
        // No anchor: the caller gates on `is_some`, so this is unreachable in
        // practice. Treat it as a plain submit rather than panicking.
        handle_submit(world, text);
        return ArmedSubmit::Stay;
    };
    match resolve_branch_head(world, &message_id).await {
        // The anchor is invalid (stale id, or the first message with no
        // parent), so we disarm rather than keep it, unlike the empty/mid-turn
        // arms above. We still restore the editor text the submit cleared, so
        // the user's edited prompt is not silently dropped.
        BranchTarget::Missing => {
            shell.borrow().editor.borrow_mut().set_text(&text);
            shell.borrow().disarm_branch();
            fold_notice(
                world,
                "Can't branch: that message is no longer in this session.",
            );
            ArmedSubmit::Stay
        }
        BranchTarget::Root => {
            shell.borrow().editor.borrow_mut().set_text(&text);
            shell.borrow().disarm_branch();
            fold_notice(world, "Can't branch at the first message.");
            ArmedSubmit::Stay
        }
        BranchTarget::Head(head) => {
            shell.borrow().disarm_branch();
            ArmedSubmit::Branch { head, prompt: text }
        }
    }
}

/// The prompt-safety invariant for the branch flow: auto-submit the branch
/// prompt only when the rebuild landed on the intended head, i.e. the build
/// did not fall back to the previous session AND the head override resolved
/// and was installed. Any other outcome restores the prompt into the editor
/// instead, never submitting it against the wrong head.
fn branch_prompt_should_submit(fell_back: bool, head_override_applied: Option<bool>) -> bool {
    !fell_back && head_override_applied == Some(true)
}

/// The confirmation for a branch rebuild on a successful build, chosen from
/// whether a prompt is handed off and whether the head installed cleanly.
/// `None` means the run loop folds nothing here, because the prompt handoff
/// folds its own restore notice.
///
/// - prompt + clean apply: the `b`-submit flow succeeded, the prompt
///   auto-submits as the branch's first turn.
/// - no prompt + clean apply: a tree-view switch that only moved the head.
/// - prompt + stale head: the `b` flow restores the prompt and folds "Branch
///   failed ...", so we add nothing.
/// - no prompt + stale head: a tree-view switch that could not move the head;
///   nothing else reports it, so we do.
fn branch_switch_notice(prompt_present: bool, head_applied_cleanly: bool) -> Option<&'static str> {
    match (prompt_present, head_applied_cleanly) {
        (true, true) => Some("Branched the conversation from an earlier message."),
        (false, true) => Some("Switched to the selected branch."),
        (true, false) => None,
        (false, false) => Some("Couldn't switch to that branch."),
    }
}

/// Prepend the branch rebuild's confirmation to `next.notices`, so it lands
/// ahead of any restore notices when the run loop folds them after install. A
/// no-op for a non-branch build and for a build fallback (which keeps its own
/// failure notice). See [`branch_switch_notice`] for the wording.
fn apply_branch_switch_notice(next: &mut NextSession, is_branch: bool, prompt_present: bool) {
    if is_branch && !next.fell_back {
        let clean = branch_prompt_should_submit(next.fell_back, next.head_override_applied);
        if let Some(notice) = branch_switch_notice(prompt_present, clean) {
            next.notices.insert(0, notice.to_string());
        }
    }
}

/// Hand the branch prompt to the freshly rebuilt session, under the
/// prompt-safety invariant. On a clean rebuild (see
/// [`branch_prompt_should_submit`]) the prompt is auto-submitted as the
/// branch's first turn. On any other outcome (stale head, or a build
/// fallback) it is restored verbatim into the editor with a notice and
/// never submitted, so it can't run against the wrong head. The prompt is
/// already in prompt-history (recorded at the submit site), so it is never
/// lost either way.
///
/// Returns whether the prompt was submitted, so callers (and tests) can
/// distinguish the submit path from the restore path.
fn hand_off_branch_prompt(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    prompt: String,
    fell_back: bool,
    head_override_applied: Option<bool>,
) -> bool {
    if branch_prompt_should_submit(fell_back, head_override_applied) {
        auto_submit_launch(world, vec![UserContent::text(prompt)]);
        true
    } else {
        shell.borrow().editor.borrow_mut().set_text(&prompt);
        fold_notice(
            world,
            "Branch failed; your message was restored to the editor.",
        );
        false
    }
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
            // NOTE: the global window shows the PERSISTED user-layer config,
            // the config a fresh session loads as its user layer. That can
            // differ from an unpersisted runtime toggle, which is intended:
            // this window edits `~/.aj/config.toml`, it is not a view of the
            // live session.
            let user = world
                .config_layers
                .lock()
                .expect("config layers mutex poisoned")
                .user
                .clone();
            let values = SettingsValues::from_config(&user, &world.catalog);
            // The user window has no inherited layer and no project keys; the
            // clear path is inert there, so a second view of the same layer is
            // a valid (unused) `inherited` set.
            let inherited = SettingsValues::from_config(&user, &world.catalog);
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
        // The session-selector and session-tree overlays open READ-ONLY at any
        // time, even mid-work. Switching (Enter) is refused at confirm time
        // with a toast (see their `on_confirm` closures), so there is no
        // open-time busy guard here. `NewSession` below refuses while busy up
        // front instead, since it opens no overlay.
        CommandAction::OpenSessionSelector => {
            let handles = shell.borrow().overlay_handles();
            open_session_selector(&handles, world.core.session_id.clone());
            ActionEffect::OpenedOverlay
        }
        CommandAction::OpenSessionTree => {
            // Building the tree is cheap and in-memory, so lock the log, snapshot
            // the rows and the current head, and drop the lock before opening.
            let (rows, current_head) = {
                let log = world.core.log.lock().await;
                (
                    build_tree_rows(&log.session_tree(), Utc::now()),
                    log.head().cloned(),
                )
            };
            let handles = shell.borrow().overlay_handles();
            open_session_tree(&handles, rows, current_head);
            ActionEffect::OpenedOverlay
        }
        CommandAction::NewSession => {
            // A new session rebuilds the world, which tears down any turn AND
            // background work, so it joins the refuse-while-busy rule of the
            // other session-changing requests. `consume_session_request`
            // rechecks at consumption.
            let (agents, bash) =
                running_work_counts(world.turns.len(), &world.core.task_registry.snapshot());
            if agents + bash > 0 {
                shell
                    .borrow()
                    .show_toast(busy_refusal("start a new session"));
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
///
/// Async because Observe may materialize a still-deferred resumed
/// sub-agent, which locks the log for the read.
async fn apply_picker_outcome(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    outcome: AgentPickerOutcome,
) -> ActionEffect {
    match outcome {
        AgentPickerOutcome::Observe(id) => {
            // A resumed sub-agent's transcript is deferred (see
            // `replay_deferring_subs`), so materialize it before switching
            // the view. Doing it first lets `set_active_view` reconcile
            // `header_only` against the now-present tool cells.
            if let AgentId::Sub(n) = id
                && world.deferred_subs.contains(&n)
            {
                // Lock only for the read. `linearize` returns an owned
                // `Conversation`, so we drop the lock before the projection
                // and reduce, which would otherwise stall a concurrent live
                // turn's inline persistence on a large sub-agent.
                let conv = {
                    let log = world.core.log.lock().await;
                    log.latest_leaf(ThreadFilter::subagent(n))
                        .map(|head| log.linearize(&head, ThreadFilter::subagent(n)))
                };
                if let Some(conv) = conv {
                    let mut chat = world.chat.borrow_mut();
                    for event in project_thread(&conv, AgentId::Sub(n)) {
                        let _ = reduce(&mut chat, &mut world.core.lifecycle, event);
                    }
                }
                world.deferred_subs.remove(&n);
            }
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

/// Drains and applies the picker outcome parked during input dispatch.
async fn apply_pending_picker_outcome(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    app: &mut AsyncApp,
) {
    // Drop the Shell borrow before a refocus event dispatches through it.
    let outcome = shell.borrow().take_picker_outcome();
    if let Some(outcome) = outcome {
        match apply_picker_outcome(world, shell, outcome).await {
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
        "show_frame_stats" => {
            let on = value == "true";
            // Live-apply by flipping the shared Shell cell the box reads at
            // draw, so the overlay appears or clears without a restart.
            shell.borrow().show_frame_stats.set(on);
            let save = aj_app::settings::persist_setting(
                &world.config_layers,
                &world.config,
                persist,
                "show_frame_stats",
                Some(value),
                |c| c.show_frame_stats = on,
            );
            Some(join_notice(
                format!(
                    "Frame-stats overlay {}.",
                    if on { "shown" } else { "hidden" }
                ),
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

/// Resolve the editor-border [`Color`] for a thinking `level` against the
/// current palette.
///
/// Mirrors `aj`'s thinking-level border tint: the level selects a
/// [`ThemeColor`] token (via [`aj_app::theme::thinking_color_token`]) that the
/// palette resolves and [`vaxis_color`] bakes into a concrete color for the
/// active color mode.
fn editor_border_color(theme: &Theme, level: Option<&ThinkingConfig>) -> Color {
    vaxis_color(
        theme.fg_color(aj_app::theme::thinking_color_token(level)),
        theme.color_mode(),
    )
}

/// Build the editor's border theme from the shared palette (Spec D structured
/// colors), the same way the other chrome resolves its styles.
///
/// The `border_color` seeded here is only a resting default (the `ThinkingOff`
/// token, which shares a value with `borderMuted` in the bundled themes). The
/// host overrides it per active view through [`sync_editor_chrome`], which
/// tints the border by the viewed agent's thinking level to match `aj`. That
/// reconcile runs once per drive-loop iteration and once before the first
/// paint, so this default never flashes.
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

/// The shared handles the drive loop hands to an overlay's open function.
/// Gathered from the shell in one borrow so the open call site never holds a
/// shell borrow across it.
pub(crate) struct OverlayHandles {
    pub(crate) stack: Rc<RefCell<OverlayStack>>,
    pub(crate) editor: WidgetRef,
    pub(crate) chrome: OverlayChrome,
    pub(crate) activity: Rc<RefCell<Vec<SelectorActivity>>>,
    pub(crate) settings_ui: Rc<RefCell<Option<SettingsUi>>>,
    /// Where the agent picker parks its confirmed pick / kill.
    pub(crate) picker_outcome: Rc<RefCell<Option<AgentPickerOutcome>>>,
    /// Where the prompt-history overlay parks a scan request.
    pub(crate) history_fetch: Rc<RefCell<Option<HistoryFetch>>>,
    /// Where the skills window parks its fill handle on open, for the drive
    /// loop to stream discovered rows into.
    pub(crate) skills_fill: Rc<RefCell<Option<SkillsFill>>>,
    /// Where the prompt-history overlay parks a recalled prompt.
    pub(crate) recall_slot: Rc<RefCell<Option<String>>>,
    /// Where the session selector parks its preview-scan request.
    pub(crate) session_scan: Rc<RefCell<Option<SessionScan>>>,
    /// Where the session selector parks a confirmed resume request.
    pub(crate) session_request: Rc<RefCell<Option<SessionRequest>>>,
    /// Where the login/logout picker parks a confirmed provider action.
    pub(crate) auth_request: Rc<RefCell<Option<AuthPickerRequest>>>,
    /// The shared work-in-flight flag the session-changing overlays read at
    /// confirm time to refuse a switch mid-work.
    pub(crate) busy: Rc<Cell<bool>>,
    /// The shared toast stack those refusals raise into.
    pub(crate) toasts: ToastStack,
}

#[cfg(test)]
impl OverlayHandles {
    /// A free-standing handle bundle for widget tests that drive an open
    /// function without a Shell: fresh slots, an inert editor, and the
    /// bundled dark chrome.
    pub(crate) fn for_tests() -> OverlayHandles {
        let t = Theme::bundled_dark_with_mode(ColorMode::Truecolor);
        OverlayHandles {
            stack: Rc::new(RefCell::new(OverlayStack::default())),
            editor: to_widget_ref(Rc::new(RefCell::new(Text::new("")))),
            chrome: OverlayChrome::from_theme(&t),
            activity: Rc::new(RefCell::new(Vec::new())),
            settings_ui: Rc::new(RefCell::new(None)),
            picker_outcome: Rc::new(RefCell::new(None)),
            history_fetch: Rc::new(RefCell::new(None)),
            skills_fill: Rc::new(RefCell::new(None)),
            recall_slot: Rc::new(RefCell::new(None)),
            session_scan: Rc::new(RefCell::new(None)),
            session_request: Rc::new(RefCell::new(None)),
            auth_request: Rc::new(RefCell::new(None)),
            busy: Rc::new(Cell::new(false)),
            toasts: Rc::new(RefCell::new(Vec::new())),
        }
    }
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
    /// The frame-statistics debug overlay, floated in the top-right corner
    /// when `show_frame_stats` is on. Reads the `frame_stats` snapshot below.
    frame_stats_box: Rc<RefCell<FrameStatsBox>>,
    /// The toast-stack widget, drawn bottom-right every frame: stacked above
    /// the quit hint when no modal is open, floated over the scrim/overlay
    /// (z 3) otherwise. Reads the `toasts` stack below.
    toast_box: Rc<RefCell<Toasts>>,
    /// The live toast records, shared with the writers (the drive loop's
    /// select-to-copy fold, the overlay confirm closures, [`Shell::show_toast`])
    /// and the `toast_box` that draws them. The drive loop prunes it and
    /// wakes at the earliest live deadline.
    toasts: ToastStack,
    /// The last select-to-copy record, written by the transcript (which the
    /// unified toast stack deliberately leaves untouched). The drive loop
    /// edge-detects fresh records by their timestamp and folds each into
    /// `toasts`. Copy payload, so a `Cell` not a `RefCell`.
    copied: Rc<Cell<Option<Copied>>>,
    /// Whether any work is in flight (an in-flight turn OR background
    /// sub-agents / bash tasks). Refreshed every drive-loop iteration by
    /// [`sync_keymap_ctx`] from `running_work_counts`. The session-overlay
    /// confirm closures read it at Enter time to refuse a switch mid-work
    /// while still opening read-only.
    busy: Rc<Cell<bool>>,
    /// Whether the frame-stats overlay is shown. Seeded from
    /// `config.show_frame_stats` at build time and flipped live by the
    /// settings window through `apply_setting_change`, which shares this cell.
    show_frame_stats: Rc<Cell<bool>>,
    /// The latest frame-render snapshot, written by the drive loop just before
    /// each paint so the box shows the previous frame's numbers. `None` before
    /// the first frame. Copy payload, so a `Cell` rather than a `RefCell`.
    frame_stats: Rc<Cell<Option<FrameStats>>>,
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
    /// The modal scrim, kept across frames so its identity is stable for
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
    /// The terminal window title (`"AJ - <session id> - <cwd basename>"`).
    /// Recomputed in [`Shell::rebind`] on a session switch and pushed to the
    /// terminal via [`SET_TITLE_EVENT`] (switch) and the `Init` handler
    /// (startup), never per draw.
    window_title: String,
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
    /// The armed branch anchor, `Some` while the user is composing a branch
    /// (after `b`). The `on_action` handler arms it, the drive loop resolves
    /// it on submit, and any session install clears it so a stale anchor can't
    /// resolve against a different session's log.
    branch_anchor: Rc<RefCell<Option<BranchAnchor>>>,
    /// The footer's branch indicator, in lockstep with `branch_anchor`: the
    /// short "branching from message" preview shown while armed, `None`
    /// otherwise. Shared with the [`FooterLine`], which reads it at draw.
    branch_indicator: Rc<RefCell<Option<String>>>,
    /// Set by the Esc handler when it cancels an armed anchor, so the drive
    /// loop folds the cancel notice (the Shell can't reach the chat model's
    /// lifecycle). A plain flag, drained once per input event.
    branch_cancelled: Rc<Cell<bool>>,
}

impl Shell {
    fn new(
        chat: Rc<RefCell<ChatState>>,
        status: Rc<RefCell<StatusState>>,
        queues: MessageQueues,
        theme: ThemeHandle,
        header: String,
        session_id: &str,
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
        // The terminal window title, matching aj's format. Recomputed on a
        // session switch in `rebind`, so we only need the initial world's id
        // and cwd here.
        let window_title = aj_app::session::window_title(APP_TITLE, session_id, &cwd);
        // The transcript-focus flag, shared between the transcript (its single
        // writer, via focus in/out) and the keymap host context (which reads it
        // to gate the copy chord). Created here so both get the same cell.
        let focus_mode = Rc::new(std::cell::Cell::new(false));
        // The select-to-copy record, shared between the transcript (its single
        // writer, on a copy) and the drive loop, which edge-detects fresh
        // records and folds each into the toast stack. Created here so both
        // see the same cell.
        let copied: Rc<Cell<Option<Copied>>> = Rc::new(Cell::new(None));
        // The toast stack, shared between its writers (the drive loop's copy
        // fold, the overlay confirm closures, `Shell::show_toast`) and the
        // `Toasts` box that draws it. The drive loop prunes it and wakes at
        // the earliest live deadline.
        let toasts: ToastStack = Rc::new(RefCell::new(Vec::new()));
        // The global busy flag, refreshed each drive-loop iteration. Read by
        // the session-overlay confirm closures to refuse a switch mid-work.
        let busy: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Branch-anchor slots, following the parked-slot pattern: `on_action`
        // arms them on `b`, the drive loop resolves them on submit, the footer
        // reads the indicator at draw, and the Esc handler flips
        // `branch_cancelled` so the drive loop folds the cancel notice. Created
        // here so the closures and the footer all share the same cells.
        let branch_anchor: Rc<RefCell<Option<BranchAnchor>>> = Rc::new(RefCell::new(None));
        let branch_indicator: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let branch_cancelled: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Resolve the initial styles and chrome from a single snapshot of
        // the theme, then keep the handle for the runtime re-style path.
        let (styles, transcript, chrome) = {
            let t = theme.read();
            let styles = Rc::new(TranscriptStyles::from_theme(&t));
            let transcript = Rc::new(RefCell::new(TranscriptView::new(
                Rc::clone(&chat),
                &t,
                Rc::clone(&focus_mode),
                Rc::clone(&copied),
            )));
            editor.borrow_mut().set_theme(editor_theme_from_theme(&t));
            (styles, transcript, OverlayChrome::from_theme(&t))
        };
        let chrome = Rc::new(RefCell::new(chrome));
        // Give the transcript its own `WidgetRef` (weak) so focus navigation
        // can schedule ticks targeting it to drive the smooth focus scroll.
        transcript
            .borrow_mut()
            .set_widget_ref(Rc::downgrade(&transcript));
        let status_line = StatusLine::new(Rc::clone(&chat), Rc::clone(&status), Rc::clone(&styles));
        let quit_hint_warning: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let quit_hint = Rc::new(RefCell::new(QuitHint::new(
            Rc::clone(&styles),
            Rc::clone(&chrome),
            Rc::clone(&quit_hint_warning),
        )));
        // Off by default; the run loop seeds it from `config.show_frame_stats`
        // after building the Shell, matching the other display toggles.
        let show_frame_stats = Rc::new(Cell::new(false));
        let frame_stats: Rc<Cell<Option<FrameStats>>> = Rc::new(Cell::new(None));
        let frame_stats_box = Rc::new(RefCell::new(FrameStatsBox::new(
            Rc::clone(&styles),
            Rc::clone(&chrome),
            Rc::clone(&frame_stats),
        )));
        let toast_box = Rc::new(RefCell::new(Toasts::new(
            Rc::clone(&styles),
            Rc::clone(&chrome),
            Rc::clone(&toasts),
        )));
        let pending = Rc::new(RefCell::new(PendingBox::new(
            Rc::clone(&chat),
            queues.clone(),
            Rc::clone(&styles),
        )));
        let footer = Rc::new(RefCell::new(FooterLine::new(
            Rc::clone(&chat),
            status,
            Rc::clone(&styles),
            cwd_display,
            Rc::clone(&branch_indicator),
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
            message_queues: queues.clone(),
            active_view: chat.borrow().active_view(),
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
            let editor_for_actions = Rc::clone(&editor);
            let branch_anchor_for_actions = Rc::clone(&branch_anchor);
            let branch_indicator_for_actions = Rc::clone(&branch_indicator);
            let action_slot = Rc::clone(&host_action);
            let theme_for_actions = theme.clone();
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
                    let content_styles = ContentStyles::from_theme(&theme_for_actions.read());
                    open_palette(
                        &overlays_for_actions,
                        &editor_widget,
                        &chrome_for_actions,
                        &command_slot_for_actions,
                        &fetch_slot_for_actions,
                        content_styles,
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
                    transcript_for_actions.borrow_mut().page_up(ctx);
                    ctx.redraw = true;
                }
                AjAction::ChatPageDown => {
                    transcript_for_actions.borrow_mut().page_down(ctx);
                    ctx.redraw = true;
                }
                AjAction::ChatScrollToTop => {
                    transcript_for_actions.borrow_mut().scroll_to_top(ctx);
                    ctx.redraw = true;
                }
                AjAction::ChatScrollToBottom => {
                    transcript_for_actions.borrow_mut().scroll_to_bottom(ctx);
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
                    let mut transcript = transcript_for_actions.borrow_mut();
                    if transcript.in_focus_mode() {
                        transcript.focus_prev_user_message(ctx);
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
                AjAction::BranchMessage => {
                    // `focused_message_id` already gates on focus mode, the
                    // cursor sitting on a user message, and the active view
                    // being Main (a sub-agent user message is not a branch
                    // point), so a `Some` here means the gesture is valid.
                    // Inert otherwise.
                    let transcript = transcript_for_actions.borrow();
                    let anchor = transcript
                        .focused_message_id()
                        .zip(transcript.focused_message_text());
                    drop(transcript);
                    if let Some((message_id, text)) = anchor {
                        // Prefill the editor with the message, arm the anchor,
                        // and move focus to the editor. The focus move's
                        // `FocusOut` exits transcript focus, matching the copy
                        // shortcut's contract of overwriting the editor.
                        editor_for_actions.borrow_mut().set_text(&text);
                        arm_branch(
                            &branch_anchor_for_actions,
                            &branch_indicator_for_actions,
                            message_id,
                            branch_indicator_text(&text),
                        );
                        ctx.request_focus(Rc::clone(&editor_widget));
                        ctx.redraw = true;
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
            let theme_c = theme.clone();
            editor.borrow_mut().on_palette_trigger = Some(Box::new(move |ctx| {
                let content_styles = ContentStyles::from_theme(&theme_c.read());
                open_palette(
                    &overlays_c,
                    &editor_widget,
                    &chrome_c,
                    &command_slot_c,
                    &fetch_slot_c,
                    content_styles,
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
        // Route transcript box clicks through the picker outcome slot. The
        // drive loop then applies the normal observe behavior, including
        // materializing deferred transcripts from resumed sessions.
        {
            let picker_outcome = Rc::clone(&picker_outcome);
            transcript
                .borrow_mut()
                .set_on_observe_agent(Box::new(move |id| {
                    *picker_outcome.borrow_mut() = Some(AgentPickerOutcome::Observe(id));
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
            frame_stats_box,
            toast_box,
            toasts,
            copied,
            busy,
            show_frame_stats,
            frame_stats,
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
            window_title,
            session_scan,
            session_request,
            auth_request,
            branch_anchor,
            branch_indicator,
            branch_cancelled,
        }
    }

    /// Collect a submit parked by the editor callback, if any.
    fn take_submitted(&self) -> Option<String> {
        self.submitted.borrow_mut().take()
    }

    /// Clear the armed branch anchor and its footer indicator.
    fn disarm_branch(&self) {
        clear_branch(&self.branch_anchor, &self.branch_indicator);
    }

    /// Raise a transient bottom-right toast with `message`. Live toasts
    /// stack, each with its own timer. The caller still owns the repaint (the
    /// drive loop schedules the clearing repaint at the toast's deadline).
    fn show_toast(&self, message: impl Into<String>) {
        crate::toasts::show_toast(&self.toasts, message);
    }

    /// Take the "an Esc cancelled the armed anchor" flag, so the drive loop
    /// folds the cancel notice exactly once.
    fn take_branch_cancelled(&self) -> bool {
        self.branch_cancelled.replace(false)
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

    /// The shared handles the drive loop needs to open an overlay: the stack
    /// it pushes onto, the editor (focus fallback), a live chrome snapshot,
    /// the parked-request slots, and the busy flag plus toast stack the
    /// session-changing confirms read and raise into.
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
            busy: Rc::clone(&self.busy),
            toasts: Rc::clone(&self.toasts),
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
        self.frame_stats_box
            .borrow_mut()
            .set_styles(Rc::clone(&styles));
        self.toast_box.borrow_mut().set_styles(Rc::clone(&styles));
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
    /// and apply it. The layout owns the height budget (see [`editor_row_cap`]).
    /// Called from [`Shell::draw`] each frame so the editor's growth ceiling
    /// tracks the live terminal height.
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
    fn rebind(&mut self, world: &World) {
        self.pending
            .borrow_mut()
            .set_queues(world.core.message_queues.clone());
        self.header.borrow_mut().text = format!("aj-next - session {}", world.core.session_id);
        self.window_title = aj_app::session::window_title(
            APP_TITLE,
            &world.core.session_id,
            &world.core.env.working_directory,
        );
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

        // The editor's visible-row cap is a pure function of terminal height, so
        // resolve it from the frame here. Applying it before the layout draws
        // means the editor grows against the current frame, and the first
        // painted frame is already correct.
        self.set_editor_row_cap(usize::from(ctx.max.size().height));

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
                let popup = block_mouse(popup, &self.transcript);
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

        // Corner boxes stacked above the editor, flush to the right edge and
        // built bottom-up: the Ctrl+C quit-arm hint at the bottom, then the
        // live toasts (oldest closest to the bottom). The quit hint is
        // suppressed under a modal (a quit never arms there); the toast stack
        // is drawn in the modal branch below instead, same spot, higher z.
        //
        // The quit hint is drawn straight from the live keymap state, so it
        // appears and clears with the armed state, no mirror. The keymap's
        // only sequence is the ctrl+c/ctrl+c quit chord, so a pending sequence
        // is exactly this armed state. Safe to borrow the keymap here:
        // `draw_widget` above already released its mutable borrow.
        if self.overlays.borrow().top().is_none() {
            let term = ctx.max.size();
            let editor_top = term
                .height
                .saturating_sub(FOOTER_ROWS)
                .saturating_sub(self.editor.borrow().drawn_height());
            // The next box's bottom row, moved up as boxes stack. Each box is
            // bounded by the room left above it, keeping the header on screen,
            // and its `draw` returns `None` when it can't fit.
            let mut stack_bottom = editor_top;

            let quit_armed = self.keymap.borrow().pending_sequence().is_some();
            if quit_armed {
                let avail = Size {
                    width: term.width,
                    height: stack_bottom.saturating_sub(HEADER_ROWS),
                };
                if let Some(hint) = self.quit_hint.borrow().draw(ctx, avail) {
                    let hint = block_mouse(hint, &self.transcript);
                    stack_bottom = push_corner_box(&mut inner, term.width, stack_bottom, hint, 1);
                }
            }

            let avail = Size {
                width: term.width,
                height: stack_bottom.saturating_sub(HEADER_ROWS),
            };
            for toast in self.toast_box.borrow().draw_stack(ctx, avail) {
                let toast = block_mouse(toast, &self.transcript);
                stack_bottom = push_corner_box(&mut inner, term.width, stack_bottom, toast, 1);
            }
        }

        // Frame-statistics debug overlay. Opt-in (off by default), floated in
        // the top-right corner above the base content (z 1, like the popup and
        // quit hint) so it stays visible during interaction. The box never
        // joins the focus path. A transparent blocker over its exact bounds
        // stops pointer-button input from reaching obscured content. It shows the
        // previous frame's numbers and freezes when idle.
        if self.show_frame_stats.get() {
            let term = ctx.max.size();
            // Room below the fixed header row, where the box is anchored.
            let avail = Size {
                width: term.width,
                height: term.height.saturating_sub(HEADER_ROWS),
            };
            if let Some(surf) = self.frame_stats_box.borrow().draw(ctx, avail) {
                let surf = block_mouse(surf, &self.transcript);
                let anchor_col = term.width.saturating_sub(surf.size.width);
                inner.children.push(SubSurface {
                    origin: RelativePoint {
                        row: i32::from(HEADER_ROWS),
                        col: i32::from(anchor_col),
                    },
                    surface: surf,
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
            // The toast stack floats above an open overlay too: a
            // session-overlay confirm that refuses a switch mid-work raises a
            // toast and keeps the overlay open, so toasts must sit above the
            // scrim and the overlay (z 3). Same bottom-right spot and stacking
            // as the no-modal branch above; only the quit hint stays
            // suppressed under modals.
            let editor_top = term
                .height
                .saturating_sub(FOOTER_ROWS)
                .saturating_sub(self.editor.borrow().drawn_height());
            let mut stack_bottom = editor_top;
            let avail = Size {
                width: term.width,
                height: editor_top.saturating_sub(HEADER_ROWS),
            };
            for toast in self.toast_box.borrow().draw_stack(ctx, avail) {
                let toast = block_mouse(toast, &self.transcript);
                stack_bottom = push_corner_box(&mut inner, term.width, stack_bottom, toast, 3);
            }
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
        if matches!(event, Event::KeyPress(_))
            || matches!(event, Event::Mouse(_)) && self.overlays.borrow().top().is_some()
        {
            self.transcript.borrow_mut().cancel_agent_click();
        }
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
            } else if user.name == SET_TITLE_EVENT {
                // A session switch rebound `window_title` off the loop. Push it
                // to the terminal now that we have an event context.
                ctx.set_title(self.window_title.clone());
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
            // `Init` dispatch drains the ctx command queue before the first
            // frame, so the title applies ahead of the initial paint.
            ctx.set_title(self.window_title.clone());
            ctx.redraw = true;
            return;
        }
        // Higher-priority owners consume Esc before it reaches this bubble
        // handler. Keep the explicit guards so a leaked overlay or transcript
        // focus event cannot alter editor-mode state.
        if let Event::KeyPress(key) = event
            && key.matches(Key::ESCAPE, Modifiers::empty())
            && !self.overlays.borrow().is_open()
            && !self.transcript.borrow().in_focus_mode()
        {
            if self.branch_anchor.borrow().is_some() {
                self.disarm_branch();
                self.branch_cancelled.set(true);
                ctx.consume_and_redraw();
                return;
            }
            if self.transcript.borrow_mut().handle_unfocused_escape() {
                ctx.consume_and_redraw();
            }
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
    // The global busy flag the session-overlay confirm closures read: any
    // in-flight turn OR background work (background sub-agents + bash tasks),
    // not just the viewed agent. Distinct from `turn_running` above, which is
    // per-view and gates the keymap's steer/dequeue chords.
    let (agents, bash) =
        running_work_counts(world.turns.len(), &world.core.task_registry.snapshot());
    let shell = shell.borrow();
    shell.busy.set(agents + bash > 0);
    let mut ctx = shell.keymap_ctx.borrow_mut();
    ctx.turn_running = busy;
    // The queue handle is swapped on session change (`world.core` is replaced),
    // so re-clone it here rather than relying on the one captured at Shell::new.
    ctx.message_queues = world.core.message_queues.clone();
    ctx.active_view = active;
}

/// Reconcile the editor's border tint and agent marker from the active view.
/// The border follows the viewed agent's thinking level (aj's color-bar
/// parity) and the top-bar label reads `agent N` for a sub-agent, cleared for
/// the main agent. This is the single writer: the drive loop calls it once per
/// iteration and once before the first paint, so no view-switch or
/// thinking-change path has to remember to retint.
fn sync_editor_chrome(world: &World, shell: &Rc<RefCell<Shell>>) {
    let active = world.chat.borrow().active_view();
    let level = viewed_thinking(world, active);
    let shell = shell.borrow();
    let color = editor_border_color(&shell.theme.read(), level.as_ref());
    let label = match active {
        AgentId::Main => None,
        AgentId::Sub(n) => Some(format!("agent {n}")),
    };
    let mut editor = shell.editor.borrow_mut();
    editor.set_border_color(color);
    editor.set_top_bar_label(label);
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
    let header = format!("aj-next - session {}", world.core.session_id);
    let cwd = world.core.env.working_directory.clone();
    let shell = Rc::new(RefCell::new(Shell::new(
        Rc::clone(&world.chat),
        Rc::clone(&world.status),
        world.core.message_queues.clone(),
        theme.clone(),
        header,
        &world.core.session_id,
        cwd,
    )));
    let root: WidgetRef = to_widget_ref(Rc::clone(&shell));

    // Seed the frame-stats overlay toggle from config (off by default). The
    // cell lives on the Shell, and `Shell::new` has no config handle, so we
    // seed it here once. It persists across session rebinds.
    shell.borrow().show_frame_stats.set(
        world
            .config
            .lock()
            .expect("config mutex poisoned")
            .show_frame_stats,
    );

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

    // Seed the editor's border and agent marker from the initial view before
    // the first drive-loop frame. `app.init` already painted one frame, and the
    // drive loop draws at the top of its iteration but reconciles the chrome at
    // the bottom, so without this seed the first drive-loop paint would show the
    // resting default. The shell's build and the color-mode reconcile above both
    // leave the chrome at that default.
    sync_editor_chrome(&world, &shell);

    // Hot-reload watcher for a user theme (bundled names have no on-disk
    // source, so this is inert for `dark` / `light` with no override).
    let mut theme_watch = ThemeWatch::install(&theme_name);

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

        // A branch rebuild reuses the switch machinery but is a same-session
        // resume with a head override plus an optional prompt to hand off. We
        // track both across the match so the build and the post-install prompt
        // handoff below can act on them.
        let mut is_branch = false;
        let mut branch_prompt: Option<String> = None;

        let spec = match exit {
            Ok(SessionExit::Quit) => break Ok(()),
            Err(fatal) => break Err(fatal),
            Ok(SessionExit::New) => SessionSpec::Create {
                entry: SessionEntry::Switch,
            },
            Ok(SessionExit::Switch(session_id)) => SessionSpec::Resume {
                session_id,
                entry: SessionEntry::Switch,
                head: None,
            },
            Ok(SessionExit::Branch { head, prompt }) => {
                // Flush the abandoned branch's buffered non-punctuation entries
                // to disk before the re-resume: they belong to that branch and
                // must survive the resume-from-disk (but must not follow the
                // user to the new branch). Runs here, after turn and
                // background-task shutdown, so a racing sub-agent shutdown
                // append is included and not stranded (spec Part 4).
                if let Err(err) = world.core.log.lock().await.flush_pending() {
                    tracing::warn!(
                        "failed to flush buffered log entries before branch rebuild: {err}"
                    );
                }
                is_branch = true;
                branch_prompt = prompt;
                SessionSpec::Resume {
                    session_id: world.core.session_id.clone(),
                    entry: SessionEntry::Switch,
                    head: Some(head),
                }
            }
        };

        // Snapshot the outgoing session's usage for the banner before we
        // rebuild over it. The replacement session's usage starts at zero,
        // so nothing is double-counted (including on the fallback path,
        // which resumes the same session in a fresh world). A same-session
        // branch rebuild must not push a duplicate banner entry for the id
        // it is rebuilding onto, so we guard the push.
        let usage = world.core.usage_summary().await;
        if !is_branch {
            completed_sessions.push((world.core.session_id.clone(), usage));
        }
        let previous_id = world.core.session_id.clone();

        match build_next_session(&world, spec, &previous_id, is_branch).await {
            Ok(mut next) => {
                // Read the prompt-safety inputs before `install` consumes `next`.
                let fell_back = next.fell_back;
                let head_applied = next.head_override_applied;
                // A successful branch build leads with no confirmation; insert
                // the accurate one here. It depends on whether a prompt is
                // handed off (the `b`-submit flow) or only the head moved (a
                // tree-view switch), and on whether the head installed cleanly.
                // A `b`-flow failure inserts nothing: its prompt handoff below
                // folds the restore notice instead.
                apply_branch_switch_notice(&mut next, is_branch, branch_prompt.is_some());
                install_next_session(&mut world, &shell, next);
                // Retitle the terminal for the switched-to session. The switch
                // ran off the loop with no event context, so we ride an app
                // event, mirroring the refocus delegation.
                app.post_app_event(UserEvent {
                    name: SET_TITLE_EVENT.to_string(),
                    data: None,
                });
                app.request_redraw();
                // Branch prompt handoff, under the prompt-safety invariant:
                // auto-submit only on a clean rebuild, otherwise restore the
                // prompt verbatim into the editor with a notice (never
                // submitting it against the wrong head). The prompt is already
                // in prompt-history (recorded at the submit site), so it is
                // never lost.
                if let Some(prompt) = branch_prompt {
                    hand_off_branch_prompt(&mut world, &shell, prompt, fell_back, head_applied);
                    app.request_redraw();
                }
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

/// Adds a transparent event target over `surface` without changing its paint.
fn block_mouse(mut surface: Surface, transcript: &Rc<RefCell<TranscriptView>>) -> Surface {
    let transcript = Rc::downgrade(transcript);
    let blocker = MouseBlocker::new(Box::new(move || {
        if let Some(transcript) = transcript.upgrade() {
            transcript.borrow_mut().cancel_agent_click();
        }
    }));
    surface.widget = Some(Rc::new(RefCell::new(blocker)));
    surface
}

/// Fold a fresh select-to-copy record into the toast stack.
///
/// The transcript writes the shared `copied` cell (the unified toast stack
/// deliberately leaves it in place), so the drive loop edge-detects fresh
/// records here by their timestamp: each new copy pushes exactly one toast
/// with the copy-toast look and its own timer. Returns whether a toast was
/// pushed, so the caller requests the showing repaint.
fn fold_copied_record(shell: &Shell, seen: &mut Option<Instant>) -> bool {
    let Some(copied) = shell.copied.get() else {
        return false;
    };
    if *seen == Some(copied.at) {
        return false;
    }
    *seen = Some(copied.at);
    crate::toasts::push_copy_toast(&shell.toasts, copied.chars);
    true
}

/// Anchor a corner box flush to the right edge with its bottom at `bottom`,
/// pushing it onto `inner` at `z` (1 over the base layout like the
/// autocomplete popup, 3 over an open modal for the toast stack). Returns the
/// box's top row, the bottom edge for the next box stacked above it.
fn push_corner_box(
    inner: &mut Surface,
    term_width: u16,
    bottom: u16,
    surface: Surface,
    z: u8,
) -> u16 {
    let anchor_row = bottom.saturating_sub(surface.size.height);
    let anchor_col = term_width.saturating_sub(surface.size.width);
    inner.children.push(SubSurface {
        origin: RelativePoint {
            row: i32::from(anchor_row),
            col: i32::from(anchor_col),
        },
        surface,
        z_index: z,
    });
    anchor_row
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
    let mut was_animating = false;
    // Edge tracker for the quit-arm hint's warning: the keymap's only
    // sequence is the ctrl+c/ctrl+c quit chord, so a pending sequence means the
    // quit is armed. We refresh the hint's running-work warning on each edge
    // (set it on arm, clear it on disarm).
    let mut quit_was_armed = false;
    // Edge tracker for the transcript's select-to-copy record: the transcript
    // writes the shared cell (it stays the cell's single writer), and the
    // per-iteration fold below pushes each fresh record onto the toast stack.
    // Seeded from the current record so a copy already reported by a previous
    // session's drive loop isn't re-toasted.
    let mut copied_seen: Option<Instant> = shell.borrow().copied.get().map(|c| c.at);
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
            // The frame-stats overlay shows the previous frame's numbers, so we
            // snapshot before rendering this frame. Only when the overlay is on:
            // nothing reads the cell otherwise, and skipping the write avoids
            // churn.
            if shell.borrow().show_frame_stats.get() {
                shell.borrow().frame_stats.set(Some(app.frame_stats()));
            }
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
        // Toasts have no self-timer, so wake at the earliest live toast's
        // deadline: the per-iteration prune below drops what expired then and
        // requests the clearing repaint, so each toast vanishes exactly on
        // time even while others stay live.
        let toast_deadline = crate::toasts::earliest_toast_deadline(&shell.borrow().toasts);
        let deadline = earliest_deadline(
            earliest_deadline(tick_deadline, frame_deadline),
            toast_deadline,
        );
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
                            // The per-iteration `sync_editor_chrome` runs after
                            // this arm and recomputes the border from the freshly
                            // swapped `shell.theme`, so the chrome reconciles
                            // through the loop sync, not here.
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
                        // Every global chord (the ctrl+c ladder, the
                        // toggles, the overlay openers) is matched by
                        // the keymap controller inside this dispatch.
                        // The host only collects what the handlers
                        // parked.
                        if app.handle_input(event).quit {
                            break Ok(SessionExit::Quit);
                        }
                        // Bind the submitted text out of the borrow first so
                        // no `RefCell` ref is held across the await below.
                        let submitted = shell.borrow().take_submitted();
                        if let Some(text) = submitted {
                            // Record the submitted prompt into the editor's
                            // history ring, idle or busy (matching aj). The
                            // text is already trimmed by the editor's submit;
                            // `add_to_history` ignores a whitespace-only or
                            // duplicate entry. In-session submissions stay the
                            // most-recent entries an Up press reaches, with the
                            // disk seed spliced in beneath (see
                            // `spawn_prompt_history_bootstrap`). Recording it
                            // before the branch resolution below means the
                            // prompt survives a failed branch.
                            shell.borrow().editor.borrow_mut().add_to_history(&text);
                            // With a branch anchor armed, submit resolves the
                            // branch instead of starting a turn: a refusal
                            // stays in the session, a resolution breaks out
                            // with `SessionExit::Branch`.
                            let armed = shell.borrow().branch_anchor.borrow().is_some();
                            if armed {
                                match submit_with_armed_anchor(world, shell, text).await {
                                    ArmedSubmit::Stay => app.request_redraw(),
                                    ArmedSubmit::Branch { head, prompt } => {
                                        break Ok(SessionExit::Branch {
                                            head,
                                            prompt: Some(prompt),
                                        });
                                    }
                                }
                            } else {
                                handle_editor_submit(world, shell, text);
                            }
                        }
                        // An Esc that cancelled an armed branch anchor: fold
                        // the cancel notice (the Shell can't reach the chat
                        // lifecycle) and redraw so the indicator clears.
                        if shell.borrow().take_branch_cancelled() {
                            fold_notice(world, "Branch cancelled.");
                            app.request_redraw();
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
                        apply_pending_picker_outcome(world, shell, app).await;
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
                        // or a confirmed resume pick). Bind the take out of the
                        // borrow first, so no RefCell ref is held across
                        // `consume_session_request` (which borrows the shell to
                        // raise its refuse toast). Any request can be parked
                        // with background work live, so all are rechecked and
                        // refused there rather than consumed (see
                        // `consume_session_request`).
                        let session_request = shell.borrow().take_session_request();
                        if let Some(request) = session_request {
                            match consume_session_request(world, shell, request) {
                                Some(exit) => break Ok(exit),
                                None => app.request_redraw(),
                            }
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
        // idle-to-animating edge, post the loader wake: widgets can only
        // schedule ticks from an event handler, so the host hands the
        // loader an app event to arm its animation chain (the Shell
        // forwards it, see `Shell::capture_event`). The edge tracks
        // animating, not just busy, so a background sub-agent starting while
        // the viewed agent is idle still arms the box-spinner redraw pump.
        let animating = sync_status(world);
        // Advance the editor's autocomplete once per iteration. The delivery
        // arm above wakes the loop as streaming matches and one-shot results
        // land, but a narrowing keystroke re-scores an already-walked tree in
        // place, which need not emit a fresh wake. Pumping here ticks the
        // active session and rebuilds the popup from its latest snapshot. It is
        // a no-op when no session is open. The widget still owns the pipeline,
        // the host just drives the tick from its own loop.
        shell.borrow().editor.borrow_mut().pump_autocomplete();
        sync_keymap_ctx(world, shell);
        sync_editor_chrome(world, shell);
        // The close-all chord must not pre-empt the login dialog's own
        // Esc/Ctrl+C teardown, so mirror the login liveness into the
        // keymap context. This loop is the field's single writer.
        shell.borrow().keymap_ctx.borrow_mut().login_active = login_session.is_some();
        if animating && !was_animating {
            let _ = app.post_app_event(UserEvent {
                name: STATUS_WAKE_EVENT.to_string(),
                data: None,
            });
        }
        was_animating = animating;
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
        // Fold a fresh select-to-copy into the toast stack (the transcript
        // wrote the shared record during dispatch), then prune expired toasts
        // so the repaint the earliest-deadline wake scheduled clears exactly
        // the boxes whose time is up. Other raise sites request their own
        // redraws.
        {
            let shell = shell.borrow();
            if fold_copied_record(&shell, &mut copied_seen) {
                app.request_redraw();
            }
            if crate::toasts::prune_expired(&shell.toasts) {
                app.request_redraw();
            }
        }
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

    use aj_app::chat::{EntryKind, NoticeLevel, SubAgentStatus, ToolStatus};
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
            "",
            PathBuf::from("/tmp"),
        )))
    }

    /// A `Shell` bound to a known session id and cwd, for the window-title
    /// tests. The header is irrelevant here, only the title inputs matter.
    fn titled_shell(session_id: &str, cwd: &str) -> Shell {
        Shell::new(
            empty_chat(),
            Rc::new(RefCell::new(StatusState::default())),
            MessageQueues::default(),
            ThemeHandle::new(Theme::bundled_dark_with_mode(
                aj_app::theme::ColorMode::Truecolor,
            )),
            format!("aj-next - session {session_id}"),
            session_id,
            PathBuf::from(cwd),
        )
    }

    /// The title command a handler queued, if any.
    fn queued_title(cmds: &[vaxis::vxfw::Command]) -> Option<String> {
        cmds.iter().find_map(|c| match c {
            vaxis::vxfw::Command::SetTitle(t) => Some(t.clone()),
            _ => None,
        })
    }

    /// `Shell::new` computes the terminal title from the session id and the
    /// cwd basename, matching aj's format.
    #[test]
    fn shell_new_computes_window_title() {
        let shell = titled_shell("sess-1", "/home/me/myproj");
        assert_eq!(shell.window_title, "aj - sess-1 - myproj");
    }

    /// A session switch reruns [`Shell::rebind`], which recomputes the title
    /// off the new world's id and cwd.
    #[tokio::test]
    async fn rebind_updates_window_title_on_session_switch() {
        let dir = TempDir::new().expect("tempdir");
        let world = scripted_world(&dir, "streaming-text").await;

        let mut shell = titled_shell("old-session", "/home/me/oldproj");
        assert_eq!(shell.window_title, "aj - old-session - oldproj");

        shell.rebind(&world);
        let expected = aj_app::session::window_title(
            APP_TITLE,
            &world.core.session_id,
            &world.core.env.working_directory,
        );
        assert_eq!(shell.window_title, expected);
        assert_ne!(
            shell.window_title, "aj - old-session - oldproj",
            "rebind must retitle for the switched-to session"
        );
    }

    /// The `Init` handler queues the terminal title so it applies before the
    /// first frame.
    #[test]
    fn init_sets_terminal_title() {
        let mut shell = titled_shell("sess-1", "/home/me/myproj");
        let mut ctx = EventContext::new();
        shell.handle_event(&mut ctx, &Event::Init);
        assert_eq!(
            queued_title(&ctx.cmds).as_deref(),
            Some("aj - sess-1 - myproj")
        );
    }

    /// The switch path posts [`SET_TITLE_EVENT`]. The Shell's capture handler
    /// turns it into a title command for the switched-to session.
    #[test]
    fn set_title_event_retitles_terminal() {
        let mut shell = titled_shell("sess-1", "/home/me/myproj");
        let mut ctx = EventContext::new();
        let event = Event::App(UserEvent {
            name: SET_TITLE_EVENT.to_string(),
            data: None,
        });
        shell.capture_event(&mut ctx, &event);
        assert_eq!(
            queued_title(&ctx.cmds).as_deref(),
            Some("aj - sess-1 - myproj")
        );
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
            "",
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
        let mut hits = Vec::new();
        composed.hit_test(
            vaxis::vxfw::Point {
                row: u16::try_from(popup.origin.row).expect("popup row is non-negative"),
                col: 1,
            },
            &mut hits,
        );
        let blocker_name = std::any::type_name::<MouseBlocker>();
        assert_eq!(
            hits.last()
                .expect("popup point has a target")
                .widget
                .borrow()
                .debug_label(),
            blocker_name,
        );
        let mut hits = Vec::new();
        composed.hit_test(
            vaxis::vxfw::Point {
                row: u16::try_from(popup.origin.row - 1).expect("row above popup is visible"),
                col: 1,
            },
            &mut hits,
        );
        assert!(
            hits.iter()
                .all(|hit| hit.widget.borrow().debug_label() != blocker_name),
            "the blocker is limited to the popup bounds",
        );
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

    #[test]
    fn transcript_box_click_parks_shell_picker_outcome() {
        let chat = empty_chat();
        let mut lifecycle = aj_app::session::AgentLifecycle::default();
        let _ = reduce(
            &mut chat.borrow_mut(),
            &mut lifecycle,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(7),
                task: "inspect the picker wiring".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "off".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
        );
        let shell = test_shell_with_chat(chat);
        let surface = shell
            .borrow()
            .transcript
            .borrow_mut()
            .draw(&draw_ctx(40, 20));
        let row = crate::test_support::rows(&surface)
            .iter()
            .position(|row| row.contains("agent 7"))
            .expect("the sub-agent box is visible");
        let mouse = |kind| {
            Event::Mouse(vaxis::mouse::Mouse {
                col: 5,
                row: i16::try_from(row).expect("row fits"),
                xoffset: 0,
                yoffset: 0,
                button: vaxis::mouse::Button::Left,
                mods: vaxis::mouse::Modifiers::empty(),
                kind,
            })
        };

        let mut ctx = EventContext::new();
        shell
            .borrow()
            .transcript
            .borrow_mut()
            .handle_event(&mut ctx, &mouse(vaxis::mouse::Type::Press));
        shell
            .borrow()
            .transcript
            .borrow_mut()
            .handle_event(&mut ctx, &mouse(vaxis::mouse::Type::Release));

        assert_eq!(
            shell.borrow().take_picker_outcome(),
            Some(AgentPickerOutcome::Observe(AgentId::Sub(7))),
        );

        shell
            .borrow()
            .transcript
            .borrow_mut()
            .handle_event(&mut ctx, &mouse(vaxis::mouse::Type::Press));
        shell.borrow_mut().capture_event(
            &mut ctx,
            &Event::KeyPress(Key {
                codepoint: u32::from('x'),
                ..Key::default()
            }),
        );
        shell
            .borrow()
            .transcript
            .borrow_mut()
            .handle_event(&mut ctx, &mouse(vaxis::mouse::Type::Release));
        assert_eq!(
            shell.borrow().take_picker_outcome(),
            None,
            "keyboard input interrupts a pending mouse click",
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

    /// Submit a prompt and drive a scripted demo, including its sub-agent
    /// turns, to full completion so the persisted log holds complete
    /// sub-agent runs.
    ///
    /// Loops join + drain, spawning any earned post-turn wakes, until no
    /// turn is in flight. A single join + drain (as in [`persist_session`])
    /// is enough for a one-turn demo, but a multi-agent demo can leave
    /// wake turns behind, so we settle the whole join set.
    async fn drive_demo_to_completion(world: &mut World) {
        handle_submit(world, "run the demo".to_string());
        loop {
            if !world.turns.is_empty() {
                let joined = join_next_or_pending(&mut world.turns).await;
                handle_turn_join(world, joined).expect("turn settles cleanly");
            }
            let mut wakes = Vec::new();
            if let Ok(first) = world.core.event_rx.try_recv() {
                let (_, targets) = drain_events(world, first);
                wakes = targets;
            }
            spawn_wakes(world, wakes);
            if world.turns.is_empty() {
                break;
            }
        }
    }

    /// A shell wrapping `world`'s chat and queues, for the resume/observe
    /// tests that need one but build the world by resuming rather than via
    /// [`world_and_shell`].
    fn shell_for(world: &World) -> Rc<RefCell<Shell>> {
        Rc::new(RefCell::new(Shell::new(
            Rc::clone(&world.chat),
            Rc::clone(&world.status),
            world.core.message_queues.clone(),
            ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor)),
            "aj-next".to_string(),
            "",
            PathBuf::from("/tmp"),
        )))
    }

    /// The sub-agent boxes in the Main transcript, as `(child, status,
    /// report, task)` tuples in append order.
    fn sub_boxes(chat: &ChatState) -> Vec<(usize, SubAgentStatus, Option<String>, String)> {
        chat.transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::SubAgent(s) => {
                    Some((s.child, s.status, s.report.clone(), s.task.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// A richer projection of a transcript entry than its kind alone, for
    /// parity comparisons between the eager and lazily materialized paths. It
    /// captures the payload fields those paths could diverge on: assistant
    /// text and its `finalized` flag, tool name / args / status / `header_only`.
    /// Comparing shapes (not just kinds) catches a bug in tool args, message
    /// text, or `header_only` that a kind-only comparison would pass.
    #[derive(Debug, PartialEq)]
    enum EntryShape {
        User {
            text: String,
            collapsible: bool,
        },
        Assistant {
            text: String,
            finalized: bool,
        },
        Tool {
            tool: String,
            args: serde_json::Value,
            status: ToolStatus,
            header_only: bool,
        },
        SubAgent {
            child: usize,
            status: SubAgentStatus,
            report: Option<String>,
        },
        Compaction {
            summary: String,
        },
        Notice {
            level: NoticeLevel,
            text: String,
        },
        TurnUsage {
            line: String,
        },
    }

    /// The concatenated text blocks of an assistant message, matching how the
    /// box report and `capture_sub_report` fold assistant content.
    fn assistant_text(msg: &aj_models::types::AssistantMessage) -> String {
        msg.content
            .iter()
            .filter_map(|b| match b {
                aj_models::types::AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn entry_shape(kind: &EntryKind) -> EntryShape {
        match kind {
            EntryKind::User(u) => EntryShape::User {
                text: u.joined_text(),
                collapsible: u.collapsible,
            },
            EntryKind::Assistant(a) => EntryShape::Assistant {
                text: assistant_text(&a.message),
                finalized: a.finalized,
            },
            EntryKind::Tool(t) => EntryShape::Tool {
                tool: t.tool.clone(),
                args: t.args.clone(),
                status: t.status,
                header_only: t.header_only,
            },
            EntryKind::SubAgent(s) => EntryShape::SubAgent {
                child: s.child,
                status: s.status,
                report: s.report.clone(),
            },
            EntryKind::Compaction(c) => EntryShape::Compaction {
                summary: c.summary.clone(),
            },
            EntryKind::Notice(n) => EntryShape::Notice {
                level: n.level,
                text: n.text.clone(),
            },
            EntryKind::TurnUsage(u) => EntryShape::TurnUsage { line: u.line() },
        }
    }

    /// The transcript for `id` as a sequence of [`EntryShape`]s.
    fn transcript_shape(chat: &ChatState, id: AgentId) -> Vec<EntryShape> {
        chat.transcript(id)
            .map(|t| t.entries().iter().map(|e| entry_shape(&e.kind)).collect())
            .unwrap_or_default()
    }

    /// The `header_only` flag of each tool cell in `id`'s transcript, in
    /// append order. Used to check the view-switch reconcile.
    fn tool_header_only(chat: &ChatState, id: AgentId) -> Vec<bool> {
        chat.transcript(id)
            .map(|t| {
                t.entries()
                    .iter()
                    .filter_map(|e| match &e.kind {
                        EntryKind::Tool(tool) => Some(tool.header_only),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The chat state the eager (full-replay) path builds for `world`'s log,
    /// with `view` set active so `header_only` reconciles exactly as a
    /// lazily materialized world viewing `view` does. Settings only drive
    /// footer chrome, not transcript entries, so a dummy chat suffices for
    /// the entry-shape, report, and `header_only` comparisons these tests
    /// make.
    async fn eager_chat(world: &World, view: AgentId) -> ChatState {
        let mut eager = ChatState::new(
            aj_agent::events::AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            0,
            Arc::new(Vec::new()),
        );
        let mut life = aj_app::session::AgentLifecycle::default();
        {
            let log = world.core.log.lock().await;
            for event in aj_session::replay(&log) {
                let _ = reduce(&mut eager, &mut life, event);
            }
        }
        eager.set_active_view(view);
        eager
    }

    /// Resume a session and confirm every sub-agent box is present, `Done`,
    /// and reporting, while its transcript stays empty until observed: the
    /// deferred-replay contract.
    #[tokio::test]
    async fn resume_defers_subagent_transcripts() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "parallel-agents").await;
        drive_demo_to_completion(&mut world).await;
        let session_id = world.core.session_id.clone();

        let resumed = resumed_world(&dir, "parallel-agents", &session_id).await;
        let boxes = sub_boxes(&resumed.chat.borrow());

        assert!(
            !resumed.deferred_subs.is_empty(),
            "the demo spawns sub-agents, so the resume defers them"
        );
        let box_indices: HashSet<usize> = boxes.iter().map(|(n, _, _, _)| *n).collect();
        assert_eq!(
            resumed.deferred_subs, box_indices,
            "deferred set is exactly the resumed sub-agent boxes"
        );

        for (n, status, report, task) in &boxes {
            assert_eq!(
                *status,
                SubAgentStatus::Done,
                "resumed box Sub({n}) is Done (replay closes the bracket)"
            );
            assert!(
                report.as_deref().is_some_and(|r| !r.is_empty()),
                "resumed box Sub({n}) carries a non-empty report: {report:?}"
            );
            assert!(
                !task.is_empty(),
                "resumed box Sub({n}) carries its spawn task: {task:?}"
            );
            let chat = resumed.chat.borrow();
            let transcript = chat
                .transcript(AgentId::Sub(*n))
                .unwrap_or_else(|| panic!("Sub({n}) transcript slot present"));
            assert!(
                transcript.entries().is_empty(),
                "Sub({n}) transcript is deferred, so empty until observed"
            );
        }
    }

    /// Observing a deferred sub-agent materializes its transcript on demand,
    /// switches the view to it, and produces the same entry shape the eager
    /// replay path builds.
    #[tokio::test]
    async fn observe_materializes_a_deferred_subagent() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "parallel-agents").await;
        drive_demo_to_completion(&mut world).await;
        let session_id = world.core.session_id.clone();

        let mut resumed = resumed_world(&dir, "parallel-agents", &session_id).await;
        let shell = shell_for(&resumed);
        let n = *resumed
            .deferred_subs
            .iter()
            .min()
            .expect("a deferred sub-agent to observe");

        // What the eager path produces for Sub(n): full `replay` reduced into
        // a throwaway chat over the same log, with Sub(n) set active so its
        // tool cells reconcile to `header_only == false` exactly as the
        // materialized world will once observe makes Sub(n) active. Comparing
        // the richer shape (not just the kind) catches a divergence in tool
        // args, message text, `finalized`, or `header_only`.
        let (eager_shape, eager_report) = {
            let eager = eager_chat(&resumed, AgentId::Sub(n)).await;
            let report = sub_boxes(&eager)
                .into_iter()
                .find(|(m, _, _, _)| *m == n)
                .map(|(_, _, report, _)| report)
                .expect("eager box for Sub(n)");
            (transcript_shape(&eager, AgentId::Sub(n)), report)
        };

        // The box report the resume produced, captured before observe so we can
        // prove observe does not touch it.
        let resume_report = sub_boxes(&resumed.chat.borrow())
            .into_iter()
            .find(|(m, _, _, _)| *m == n)
            .map(|(_, _, report, _)| report)
            .expect("resumed box for Sub(n)");
        assert_eq!(
            resume_report, eager_report,
            "the resumed box report matches the eager resume's"
        );

        let effect = apply_picker_outcome(
            &mut resumed,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Sub(n)),
        )
        .await;
        assert!(matches!(effect, ActionEffect::Redraw));

        assert!(
            !resumed.deferred_subs.contains(&n),
            "Sub({n}) left the deferred set on observe"
        );
        assert_eq!(
            resumed.chat.borrow().active_view(),
            AgentId::Sub(n),
            "observe switches the active view to the sub-agent"
        );

        let chat = resumed.chat.borrow();
        let transcript = chat
            .transcript(AgentId::Sub(n))
            .expect("Sub(n) transcript materialized");
        assert!(
            !transcript.entries().is_empty(),
            "materialized transcript has entries"
        );
        assert!(
            transcript
                .entries()
                .iter()
                .any(|e| matches!(e.kind, EntryKind::Tool(_))),
            "the sub-agent's bash call materialized as a tool cell"
        );
        assert!(
            transcript
                .entries()
                .iter()
                .any(|e| matches!(e.kind, EntryKind::Assistant(_))),
            "the sub-agent's assistant messages materialized"
        );
        // Parity with the eager path: same entries and payloads, with
        // `header_only` reconciled the same way now that both views point at
        // Sub(n). So the materialized transcript equals what a full resume
        // would build.
        assert_eq!(
            transcript_shape(&chat, AgentId::Sub(n)),
            eager_shape,
            "materialized transcript matches the eager replay on kind, \
             header_only, finalized, and payload"
        );
        // Observe is a pure read of box metadata: the report is unchanged by
        // materializing the transcript, so it still equals both its resume-time
        // value and the eager resume's.
        let post_observe_report = sub_boxes(&chat)
            .into_iter()
            .find(|(m, _, _, _)| *m == n)
            .map(|(_, _, report, _)| report)
            .expect("observed box for Sub(n)");
        assert_eq!(
            post_observe_report, resume_report,
            "observe leaves the box report unchanged from resume"
        );
        assert_eq!(
            post_observe_report, eager_report,
            "the observed box report matches the eager resume's"
        );
    }

    /// Re-observing a materialized sub-agent is a no-op: its transcript is
    /// unchanged and it stays out of the deferred set.
    #[tokio::test]
    async fn re_observe_is_idempotent() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "parallel-agents").await;
        drive_demo_to_completion(&mut world).await;
        let session_id = world.core.session_id.clone();

        let mut resumed = resumed_world(&dir, "parallel-agents", &session_id).await;
        let shell = shell_for(&resumed);
        let n = *resumed
            .deferred_subs
            .iter()
            .min()
            .expect("a deferred sub-agent to observe");

        apply_picker_outcome(
            &mut resumed,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Sub(n)),
        )
        .await;
        let count_after_first = resumed
            .chat
            .borrow()
            .transcript(AgentId::Sub(n))
            .expect("materialized")
            .entries()
            .len();

        // Switch to Main, then observe the same sub again.
        apply_picker_outcome(
            &mut resumed,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Main),
        )
        .await;
        apply_picker_outcome(
            &mut resumed,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Sub(n)),
        )
        .await;

        assert!(
            !resumed.deferred_subs.contains(&n),
            "re-observe leaves Sub({n}) out of the deferred set"
        );
        let count_after_second = resumed
            .chat
            .borrow()
            .transcript(AgentId::Sub(n))
            .expect("still materialized")
            .entries()
            .len();
        assert_eq!(
            count_after_first, count_after_second,
            "re-observe does no materialize work, so the transcript is intact"
        );
    }

    /// After materializing a sub-agent, switching the active view away and
    /// back flips its tool cells' `header_only` flags exactly as the eager
    /// path leaves them with the sub active. This pins the reconcile in
    /// `set_active_view` for a resumed, lazily materialized transcript.
    #[tokio::test]
    async fn header_only_reconciles_across_view_switches() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "parallel-agents").await;
        drive_demo_to_completion(&mut world).await;
        let session_id = world.core.session_id.clone();

        let mut resumed = resumed_world(&dir, "parallel-agents", &session_id).await;
        let shell = shell_for(&resumed);
        let n = *resumed
            .deferred_subs
            .iter()
            .min()
            .expect("a deferred sub-agent to observe");

        // Eager reference: Sub(n) active, so its tool cells are expanded.
        let eager_header_only = {
            let eager = eager_chat(&resumed, AgentId::Sub(n)).await;
            tool_header_only(&eager, AgentId::Sub(n))
        };
        assert!(
            !eager_header_only.is_empty(),
            "the sub-agent has tool cells to reconcile"
        );

        // Observe makes Sub(n) active and materializes it, expanding its
        // tool cells.
        apply_picker_outcome(
            &mut resumed,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Sub(n)),
        )
        .await;
        assert!(
            tool_header_only(&resumed.chat.borrow(), AgentId::Sub(n))
                .iter()
                .all(|h| !h),
            "observing the sub expands its tool cells"
        );

        // Switch to Main: the sub's tool cells collapse.
        resumed.chat.borrow_mut().set_active_view(AgentId::Main);
        assert!(
            tool_header_only(&resumed.chat.borrow(), AgentId::Sub(n))
                .iter()
                .all(|h| *h),
            "viewing Main collapses the sub's tool cells"
        );

        // Switch back to Sub(n): the flags must land exactly where the eager
        // path leaves them.
        resumed.chat.borrow_mut().set_active_view(AgentId::Sub(n));
        assert_eq!(
            tool_header_only(&resumed.chat.borrow(), AgentId::Sub(n)),
            eager_header_only,
            "returning to the sub reconciles header_only to the eager result"
        );
    }

    /// Switching from a session with deferred sub-agents to a fresh session
    /// replaces the deferred set, so the old indices cannot leak.
    #[tokio::test]
    async fn session_switch_replaces_deferred_subs() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "parallel-agents").await;
        drive_demo_to_completion(&mut world).await;
        let previous_id = world.core.session_id.clone();

        // Resume the sub-agent session so the world carries a non-empty
        // deferred set, then switch onto a fresh session over it.
        let mut resumed = resumed_world(&dir, "parallel-agents", &previous_id).await;
        assert!(
            !resumed.deferred_subs.is_empty(),
            "the resumed session has deferred sub-agents"
        );
        let shell = shell_for(&resumed);

        let fresh = build_next_session(
            &resumed,
            SessionSpec::Create {
                entry: SessionEntry::Switch,
            },
            &previous_id,
            false,
        )
        .await
        .expect("build fresh next session");
        install_next_session(&mut resumed, &shell, fresh);

        assert!(
            resumed.deferred_subs.is_empty(),
            "a fresh session has no deferred subs, and the old set did not leak"
        );
    }

    /// A session aborted mid sub-agent run (a torn final line and a log cut
    /// short) still resumes: the repair drops the torn record, every box is
    /// `Done`, and observing one materializes its flushed history without
    /// panicking.
    #[tokio::test]
    async fn aborted_session_resume_loads_and_observes() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "parallel-agents").await;
        drive_demo_to_completion(&mut world).await;
        let session_id = world.core.session_id.clone();
        let log_path = {
            let log = world.core.log.lock().await;
            log.path().to_path_buf()
        };
        // Drop the world so no open handle races the on-disk rewrite.
        drop(world);

        // Truncate mid sub-agent run: keep every complete line up to the last
        // sub-agent line, then tear that line in half. `ConversationLog::resume`
        // repairs the log by dropping the torn final line, so that sub-agent's
        // run is cut short with only its earlier messages flushed. This is the
        // aborted shape the spec wants (no terminal record, torn final line).
        let bytes = std::fs::read(&log_path).expect("read persisted log");
        let (keep, torn_sub) = {
            let text = std::str::from_utf8(&bytes).expect("log is utf8");
            let mut off = 0usize;
            let mut last_sub: Option<(usize, usize, usize)> = None;
            for line in text.lines() {
                let start = off;
                let end = off + line.len();
                off = end + 1; // one '\n' terminator per line
                let v: serde_json::Value =
                    serde_json::from_str(line).expect("each log line is JSON");
                if v.get("thread").and_then(|t| t.as_str()) == Some("subagent") {
                    let agent = v
                        .get("agent_id")
                        .and_then(|a| a.as_u64())
                        .and_then(|a| usize::try_from(a).ok())
                        .expect("a subagent line carries its agent id");
                    last_sub = Some((start, end, agent));
                }
            }
            let (start, end, agent) = last_sub.expect("the demo persisted sub-agent lines");
            (start + (end - start) / 2, agent)
        };
        std::fs::write(&log_path, &bytes[..keep]).expect("truncate persisted log");

        let mut resumed = resumed_world(&dir, "parallel-agents", &session_id).await;

        // The main transcript loads (at least the seeded user turn).
        assert!(
            resumed
                .chat
                .borrow()
                .transcript(AgentId::Main)
                .is_some_and(|t| !t.entries().is_empty()),
            "the main transcript resumes from the truncated log"
        );
        for (n, status, _, _) in sub_boxes(&resumed.chat.borrow()) {
            assert_eq!(
                status,
                SubAgentStatus::Done,
                "an aborted sub-agent box Sub({n}) still resumes Done"
            );
        }

        // The truncation cuts sub `torn_sub` mid run, so its box is deferred.
        // Fail loudly rather than silently skip the materialize check if the
        // truncation ever stops covering a sub-agent.
        assert!(
            !resumed.deferred_subs.is_empty(),
            "the truncated log still holds deferred sub-agents"
        );
        assert!(
            resumed.deferred_subs.contains(&torn_sub),
            "the torn sub-agent Sub({torn_sub}) is deferred"
        );
        let n = torn_sub;

        // The eager (full-replay) resume over the SAME truncated, repaired log,
        // with Sub(n) set active. This is the reference a non-lazy resume would
        // build.
        let eager = eager_chat(&resumed, AgentId::Sub(n)).await;
        let eager_shape = transcript_shape(&eager, AgentId::Sub(n));
        let eager_report = sub_boxes(&eager)
            .into_iter()
            .find(|(m, _, _, _)| *m == n)
            .map(|(_, _, report, _)| report)
            .expect("eager box for Sub(n)");

        // Resume-time report parity: the lazy resume and the eager resume agree
        // on the box report. Sub(n) is tool-concluding here (its last flushed
        // entry is a tool result, its concluding assistant text was torn off),
        // so per spec both show an empty report (a thin box). This is the "the
        // report matches, per spec" guarantee.
        let resumed_report = sub_boxes(&resumed.chat.borrow())
            .into_iter()
            .find(|(m, _, _, _)| *m == n)
            .map(|(_, _, report, _)| report)
            .expect("resumed box for Sub(n)");
        assert_eq!(
            resumed_report, eager_report,
            "the aborted box report matches the eager resume"
        );

        let shell = shell_for(&resumed);
        apply_picker_outcome(
            &mut resumed,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Sub(n)),
        )
        .await;
        assert!(
            !resumed.deferred_subs.contains(&n),
            "observe materialized Sub({n})"
        );

        // Observing reads the actual flushed history from the repaired log:
        // the materialized transcript equals the eager resume's, entry for
        // entry, including tool args and `header_only`.
        assert_eq!(
            transcript_shape(&resumed.chat.borrow(), AgentId::Sub(n)),
            eager_shape,
            "the materialized transcript equals the eager resume's flushed history"
        );

        // Observe is a pure read of box metadata: materializing Sub(n)'s
        // transcript does not rewrite its box report. `parallel-agents` runs
        // its two subs concurrently, so their entries interleave in the log's
        // append order and replay opens/closes each bracket several times. For
        // such an interleaved sub the report set by `SubAgentEnd` (bracket
        // order) differs from the thread-order last assistant text that
        // materializing replays through `reduce_assistant_end`. The reducer
        // refreshes the report only while the box is `Running`, so a Done box
        // keeps its authoritative resume-time report through observe.
        let post_observe_report = sub_boxes(&resumed.chat.borrow())
            .into_iter()
            .find(|(m, _, _, _)| *m == n)
            .map(|(_, _, report, _)| report)
            .expect("observed box for Sub(n)");
        assert_eq!(
            post_observe_report, eager_report,
            "observe leaves the box report equal to the eager resume's"
        );
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
            false,
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
                head: None,
            },
            &previous_id,
            false,
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
                head: None,
            },
            &resumable,
            false,
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
            false,
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

    /// `Shell::draw` resolves the editor's visible-row cap from the frame
    /// height, so a taller frame reveals more editor rows before scrolling. The
    /// editor holds more content lines than either cap, so its drawn height is
    /// cap-limited at both heights. The difference between the two drawn heights
    /// isolates the cap because the editor's border chrome is constant, so it
    /// must equal the difference of the two caps.
    #[tokio::test]
    async fn draw_caps_the_editor_from_the_frame_height() {
        let shell = test_shell_with_chat(empty_chat());
        // Forty lines exceed both caps under test (7 at height 24, 15 at height
        // 50), so the editor is cap-limited, not content-limited, at both.
        shell.borrow().editor.borrow_mut().insert_at_cursor(
            &(1..=40)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );

        let short = draw_ctx(80, 24);
        shell.borrow_mut().draw(&short);
        let drawn_short = shell.borrow().editor.borrow().drawn_height();

        let tall = draw_ctx(80, 50);
        shell.borrow_mut().draw(&tall);
        let drawn_tall = shell.borrow().editor.borrow().drawn_height();

        let expected = u16::try_from(editor_row_cap(50) - editor_row_cap(24)).unwrap();
        assert_eq!(
            drawn_tall - drawn_short,
            expected,
            "the editor's drawn rows must track the cap resolved from the frame height",
        );
        // Pin one absolute height too: the border chrome adds a constant two
        // rows, so the short frame is exactly its cap plus chrome. This catches
        // a chrome or formula drift that the difference alone would cancel.
        assert_eq!(
            drawn_short,
            u16::try_from(editor_row_cap(24) + 2).unwrap(),
            "short frame caps the editor at its row cap plus border chrome",
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

    /// `editor_border_color` routes each thinking level to its palette token.
    /// Spot-checks the ends of the range and the shared top tint so a mis-wired
    /// mapping surfaces here rather than as a wrong border color on screen.
    #[test]
    fn editor_border_color_resolves_each_thinking_token() {
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);
        let mode = theme.color_mode();
        assert_eq!(
            editor_border_color(&theme, None),
            vaxis_color(theme.fg_color(ThemeColor::ThinkingOff), mode),
            "None routes to the ThinkingOff tint"
        );
        assert_eq!(
            editor_border_color(&theme, Some(&ThinkingConfig::XHigh)),
            vaxis_color(theme.fg_color(ThemeColor::ThinkingXhigh), mode),
            "XHigh routes to the ThinkingXhigh tint"
        );
        // `Max` shares the top `Xhigh` tint (the schema tops out there).
        assert_eq!(
            editor_border_color(&theme, Some(&ThinkingConfig::Max)),
            editor_border_color(&theme, Some(&ThinkingConfig::XHigh)),
            "Max shares the XHigh tint"
        );
        // The off tint and the strongest tint differ, so the border visibly
        // moves across the range.
        assert_ne!(
            editor_border_color(&theme, None),
            editor_border_color(&theme, Some(&ThinkingConfig::XHigh)),
        );
    }

    /// Reads the editor's top-border foreground color from a fresh draw. The
    /// top rule is row 0 and `draw_rule` paints every cell with the border
    /// style, so the corner cell carries the current border color.
    fn editor_border_fg(shell: &Rc<RefCell<Shell>>) -> Color {
        let surf = shell.borrow().editor.borrow_mut().draw(&draw_ctx(100, 30));
        surf.read_cell(0, 0).style.fg
    }

    /// The editor border tints to the active view's thinking level, and a
    /// thinking change for that view re-tints it. This is the aj color-bar
    /// parity: the border tracks the reasoning budget of the agent under view.
    #[tokio::test]
    async fn editor_border_tracks_the_active_view_thinking_level() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, _app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);

        // Seed the border the way the startup path does, then check it matches
        // the active view's current level.
        let initial = viewed_thinking(&world, AgentId::Main);
        sync_editor_chrome(&world, &shell);
        assert_eq!(
            editor_border_fg(&shell),
            editor_border_color(&theme, initial.as_ref()),
            "startup seeds the border to the active view's thinking tint"
        );

        // Confirm `minimal` through the real apply path, then reconcile the
        // chrome the way the drive loop does.
        let mut watch = inert_theme_watch();
        apply_selector_activity(
            &mut world,
            &shell,
            &mut watch,
            vec![SelectorActivity::ThinkingConfirmed {
                target: AgentId::Main,
                level: Some(ThinkingConfig::Minimal),
            }],
        )
        .await;
        sync_editor_chrome(&world, &shell);
        let minimal = editor_border_fg(&shell);
        assert_eq!(
            minimal,
            editor_border_color(&theme, Some(&ThinkingConfig::Minimal)),
            "confirming minimal tints the border to the minimal token"
        );

        // Confirm `xhigh`: the border moves to the stronger tint.
        apply_selector_activity(
            &mut world,
            &shell,
            &mut watch,
            vec![SelectorActivity::ThinkingConfirmed {
                target: AgentId::Main,
                level: Some(ThinkingConfig::XHigh),
            }],
        )
        .await;
        sync_editor_chrome(&world, &shell);
        let xhigh = editor_border_fg(&shell);
        assert_eq!(
            xhigh,
            editor_border_color(&theme, Some(&ThinkingConfig::XHigh)),
            "a thinking change re-tints the editor border"
        );
        assert_ne!(minimal, xhigh, "the tint changed with the level");
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

    /// Plain Up with an empty editor and a pending message recalls it (the same
    /// `Dequeue` yank alt+up does), mirroring `aj`. Ctrl+P recalls the same way.
    /// Driven through the app so the capture-phase intercept ahead of the editor
    /// runs.
    #[tokio::test]
    async fn up_and_ctrl_p_recall_pending_when_editor_empty() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, mut writer, mut world, shell, _root) =
            init_app_with_world(&dir, "streaming-text").await;

        let mut press = async |bytes: &[u8]| {
            writer.write_all(bytes).expect("write key");
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        };

        // Queue a pending follow-up for the viewed agent, editor left empty.
        world
            .core
            .message_queues
            .append_follow_up(AgentId::Main, "queued");

        // Plain Up (CSI A) parks Dequeue in the capture phase without touching
        // the editor, then the drive-loop handler performs the yank.
        press(b"\x1b[A").await;
        assert_eq!(shell.borrow().take_host_action(), Some(AjAction::Dequeue));
        assert_eq!(
            shell.borrow().editor.borrow().cursor(),
            (0, 0),
            "the recall chord never reached the editor",
        );
        assert!(handle_host_action(&mut world, &shell, AjAction::Dequeue));
        assert_eq!(shell.borrow().editor.borrow().text(), "queued");
        assert!(!world.core.message_queues.has_pending(AgentId::Main));

        // Ctrl+P (0x10) does the same. Re-queue and clear the editor first.
        world
            .core
            .message_queues
            .append_follow_up(AgentId::Main, "again");
        shell.borrow().editor.borrow_mut().clear();
        press(&[0x10]).await;
        assert_eq!(shell.borrow().take_host_action(), Some(AjAction::Dequeue));
        assert!(handle_host_action(&mut world, &shell, AjAction::Dequeue));
        assert_eq!(shell.borrow().editor.borrow().text(), "again");
    }

    /// With a draft in the editor, plain Up does NOT recall: the stricter gate
    /// declines, so the key falls through to the editor and the pending message
    /// stays queued (mirroring `aj`).
    #[tokio::test]
    async fn up_does_not_recall_with_a_draft_in_the_editor() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, mut writer, world, shell, _root) =
            init_app_with_world(&dir, "streaming-text").await;

        world
            .core
            .message_queues
            .append_follow_up(AgentId::Main, "queued");
        shell.borrow().editor.borrow_mut().insert_at_cursor("draft");

        writer.write_all(b"\x1b[A").expect("write up");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);

        assert_eq!(
            shell.borrow().take_host_action(),
            None,
            "the recall did not fire with a draft in the editor",
        );
        assert!(
            world.core.message_queues.has_pending(AgentId::Main),
            "the pending message is still queued, not recalled",
        );
    }

    /// With nothing pending, plain Up is normal history navigation: the recall
    /// gate declines and the key descends to the editor, which recalls the
    /// newest history entry. This is the end-to-end proof that a declined
    /// capture single does not swallow the key.
    #[tokio::test]
    async fn up_navigates_history_when_nothing_pending() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, mut writer, _world, shell, _root) =
            init_app_with_world(&dir, "streaming-text").await;
        shell
            .borrow()
            .editor
            .borrow_mut()
            .seed_history(&["older".to_string(), "newer".to_string()]);

        writer.write_all(b"\x1b[A").expect("write up");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);

        assert_eq!(
            shell.borrow().take_host_action(),
            None,
            "no recall fired, the key descended to the editor",
        );
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "newer",
            "the editor handled Up as history navigation",
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

    /// While the quit sequence is armed, the Shell floats the hint box above
    /// the editor: it shows the ladder and, when work runs, the running-work
    /// warning read from the shared cell. Disarming clears it.
    #[tokio::test]
    async fn armed_quit_renders_the_hint_box() {
        let (mut app, mut writer, shell, _root) = init_app().await;
        let ctx = draw_ctx(80, 24);

        // Idle: no hint box.
        let idle = crate::test_support::rows(&shell.borrow_mut().draw(&ctx)).join("\n");
        assert!(!idle.contains("Ctrl+C then"), "no hint while idle: {idle}");

        // Seed the running-work warning the drive loop would compute on the
        // arming edge, then arm with the first Ctrl+C.
        *shell.borrow().quit_hint_warning.borrow_mut() =
            Some("2 agents / 1 task still running".to_string());
        writer.write_all(&[0x03]).expect("write ctrl+c");
        let event = app.next_input().await.expect("input event");
        assert!(!app.handle_input(event).quit);
        assert!(shell.borrow().keymap.borrow().pending_sequence().is_some());

        let armed = crate::test_support::rows(&shell.borrow_mut().draw(&ctx)).join("\n");
        assert!(
            armed.contains("Ctrl+C then"),
            "hint box present when armed: {armed}"
        );
        assert!(armed.contains("quit"), "the quit rung: {armed}");
        assert!(
            armed.contains("2 agents / 1 task still running"),
            "the warning row reads the shared cell: {armed}"
        );

        // A non-Ctrl+C key disarms the sequence, so the box clears.
        writer.write_all(b"x").expect("write key");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(shell.borrow().keymap.borrow().pending_sequence().is_none());
        let disarmed = crate::test_support::rows(&shell.borrow_mut().draw(&ctx)).join("\n");
        assert!(
            !disarmed.contains("Ctrl+C then"),
            "hint cleared on disarm: {disarmed}"
        );
    }

    /// The frame-stats overlay is gated on the `show_frame_stats` cell: off
    /// (the default) the box never appears; on, with a snapshot set, a
    /// top-right "frame stats" box shows the numbers. A mouse blocker owns the
    /// box bounds so input cannot pass through to content behind it.
    #[test]
    fn frame_stats_overlay_gates_on_the_flag() {
        let shell = test_shell_with_chat(empty_chat());
        let ctx = draw_ctx(100, 30);

        // Reconstruct a surface's own (non-composited) cells, row by row.
        fn own_text(s: &Surface) -> String {
            let w = usize::from(s.size.width);
            if w == 0 {
                return String::new();
            }
            s.buffer
                .chunks(w)
                .map(|row| row.iter().map(|c| c.char.grapheme()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }
        // The box's own surface (the `OverlayWindow`) carries the title on its
        // top border, so we find it by its own buffer rather than composited.
        fn find_box(s: &Surface) -> Option<&Surface> {
            if own_text(s).contains("frame stats") {
                return Some(s);
            }
            s.children.iter().find_map(|c| find_box(&c.surface))
        }

        // Off by default: no box, even with a snapshot available.
        shell.borrow().frame_stats.set(Some(FrameStats {
            last: Duration::from_micros(1200),
            avg: Duration::from_micros(900),
            max: Duration::from_micros(3400),
            frames: 120,
            fps: 62.0,
            last_cells: 1234,
            size: (30, 100),
        }));
        let off = shell.borrow_mut().draw(&ctx);
        assert!(
            find_box(&off).is_none(),
            "no box while the flag is off: {:?}",
            crate::test_support::rows(&off)
        );

        // On, with the snapshot set: the box shows the numbers.
        shell.borrow().show_frame_stats.set(true);
        let on = shell.borrow_mut().draw(&ctx);
        let box_surf = find_box(&on).expect("box present when the flag is on");
        let body = crate::test_support::rows(&on).join("\n");
        assert!(body.contains("last  1.2ms"), "{body}");
        assert!(body.contains("fps   62"), "{body}");
        // Pin the whole-number fps format: no other row introduces "62.".
        assert!(
            !body.contains("62."),
            "fps renders without decimals: {body}"
        );
        assert!(body.contains("cells 1234"), "{body}");
        assert!(body.contains("size  100x30"), "{body}");
        let blocker = box_surf.widget.as_ref().expect("box has a mouse blocker");
        assert_eq!(
            blocker.borrow().debug_label(),
            std::any::type_name::<MouseBlocker>()
        );
        assert!(blocker.borrow().wants_events());

        // The box is flush to the right edge (top-right corner).
        let composited = crate::test_support::flatten(&on);
        let title_row = composited
            .iter()
            .position(|row| {
                row.iter()
                    .map(|c| c.char.grapheme())
                    .collect::<String>()
                    .contains("frame stats")
            })
            .expect("a row carries the title");
        let last_col_glyph = composited[title_row]
            .last()
            .map(|c| c.char.grapheme())
            .unwrap_or_default();
        assert_eq!(last_col_glyph, "╮", "box hugs the right edge: {body}");
    }

    /// `push_corner_box` stacks boxes upward, flush to the right edge: the
    /// second box sits strictly above the first, which is how a toast lands
    /// on top of the Ctrl+C quit hint when both are up.
    #[test]
    fn corner_boxes_stack_upward_flush_right() {
        let mut inner = Surface::with_size(Size {
            width: 80,
            height: 24,
        });
        let editor_top = 20u16;

        // Bottom box (quit hint): its bottom edge is the editor top.
        let hint = Surface::with_size(Size {
            width: 10,
            height: 3,
        });
        let hint_top = push_corner_box(&mut inner, 80, editor_top, hint, 1);
        assert_eq!(hint_top, editor_top - 3);

        // Top box (toast): anchored with its bottom at the hint's top edge.
        let toast = Surface::with_size(Size {
            width: 14,
            height: 4,
        });
        let toast_top = push_corner_box(&mut inner, 80, hint_top, toast, 1);
        assert_eq!(toast_top, hint_top - 4);

        assert_eq!(inner.children.len(), 2);
        for (child, width) in inner.children.iter().zip([10u16, 14]) {
            assert_eq!(child.z_index, 1, "drawn over the base layout");
            assert_eq!(
                child.origin.col,
                i32::from(80 - width),
                "flush to the right edge",
            );
        }
        // The toast (second) sits strictly above the quit hint (first).
        assert!(
            inner.children[1].origin.row < inner.children[0].origin.row,
            "toast stacks above the quit hint",
        );
    }

    /// A fresh select-to-copy record folds into the toast stack (the drive
    /// loop's `fold_copied_record`) and shows in `Shell::draw`; the same
    /// record folds only once.
    #[test]
    fn copy_toast_shows_when_a_copy_is_folded() {
        let shell = test_shell_with_chat(empty_chat());
        let ctx = draw_ctx(100, 30);

        let before = shell.borrow_mut().draw(&ctx);
        assert!(
            !crate::test_support::rows(&before)
                .join("\n")
                .contains("copied to clipboard"),
            "no toast without a copy",
        );

        shell.borrow().copied.set(Some(Copied {
            chars: 7,
            at: Instant::now(),
        }));
        let mut seen = None;
        assert!(
            fold_copied_record(&shell.borrow(), &mut seen),
            "a fresh record pushes a toast"
        );
        assert!(
            !fold_copied_record(&shell.borrow(), &mut seen),
            "the same record folds only once"
        );
        let after = shell.borrow_mut().draw(&ctx);
        let body = crate::test_support::rows(&after).join("\n");
        assert!(body.contains("7 characters copied to clipboard"), "{body}");
        assert_eq!(
            crate::toasts::toast_texts(&shell.borrow().toasts).len(),
            1,
            "exactly one toast on the stack"
        );
    }

    /// A raised toast shows in `Shell::draw`; without one there is no box.
    /// Several live toasts stack, oldest closest to the bottom.
    #[test]
    fn toasts_show_when_raised_and_stack_oldest_at_the_bottom() {
        let shell = test_shell_with_chat(empty_chat());
        let ctx = draw_ctx(100, 30);

        let before = shell.borrow_mut().draw(&ctx);
        assert!(
            !crate::test_support::rows(&before)
                .join("\n")
                .contains("work is running"),
            "no toast without a raise",
        );

        shell.borrow().show_toast("older toast message");
        shell.borrow().show_toast("newer toast message");
        let after = shell.borrow_mut().draw(&ctx);
        let rows = crate::test_support::rows(&after);
        let row_of = |needle: &str| {
            rows.iter()
                .position(|r| r.contains(needle))
                .unwrap_or_else(|| panic!("{needle} not drawn: {rows:?}"))
        };
        assert!(
            row_of("older toast message") > row_of("newer toast message"),
            "the oldest toast sits closest to the bottom: {rows:?}"
        );
    }

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
            Rc::new(std::cell::Cell::new(None)),
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
            "",
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
            "",
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
        assert!(handle_host_action(&mut world, &shell, AjAction::Steer));
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
        assert!(handle_host_action(&mut world, &shell, AjAction::Steer));
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
        assert!(handle_host_action(&mut world, &shell, AjAction::Steer));
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

    /// Editor-focused busy Alt+Enter returns a scrolled transcript to the live
    /// tail after the host accepts the steering text.
    #[tokio::test]
    async fn editor_focused_busy_alt_enter_follows_the_transcript_tail() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, mut writer, mut world, shell, root) =
            init_app_with_world(&dir, "streaming-text").await;

        handle_submit(&mut world, "earlier prompt".to_string());
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("turn settles");
        let first = world.core.event_rx.try_recv().expect("events buffered");
        drain_events(&mut world, first);
        for i in 0..40 {
            fold_notice(&mut world, &format!("historical notice {i}"));
        }
        app.request_redraw();
        app.render(&root).expect("render populated transcript");
        handle_submit(&mut world, "running prompt".to_string());

        let transcript = Rc::clone(&shell.borrow().transcript);
        let transcript_ctx = draw_ctx(40, 8);
        transcript
            .borrow_mut()
            .scroll_to_top(&mut EventContext::new());
        let _ = transcript.borrow_mut().draw(&transcript_ctx);
        assert!(!transcript.borrow().is_at_bottom(), "starts in history");
        assert!(!transcript.borrow().in_focus_mode(), "editor owns focus");
        shell.borrow().editor.borrow_mut().set_text("steer draft");

        writer.write_all(b"\x1b\r").expect("write Alt+Enter");
        let event = app.next_input().await.expect("Alt+Enter event");
        app.handle_input(event);
        let action = shell
            .borrow()
            .take_host_action()
            .expect("editor Alt+Enter parks a host action");
        assert_eq!(action, AjAction::Steer);
        assert!(handle_host_action(&mut world, &shell, action));

        let snapshot = world.core.message_queues.snapshot(AgentId::Main);
        assert_eq!(snapshot.kind, Some(aj_agent::queue::PendingKind::Steering));
        assert_eq!(snapshot.text, "steer draft");
        let _ = transcript.borrow_mut().draw(&transcript_ctx);
        assert!(
            transcript.borrow().is_at_bottom(),
            "accepted text follows tail"
        );

        world.core.message_queues.clear(AgentId::Main);
        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
    }

    /// Alt+Enter is editor-local: with transcript focus, an idle draft is
    /// preserved and no host action or turn is produced.
    #[tokio::test]
    async fn focused_idle_alt_enter_does_not_submit() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, mut writer, mut world, shell, root) =
            init_app_with_world(&dir, "streaming-text").await;

        handle_submit(&mut world, "earlier prompt".to_string());
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("turn settles");
        let first = world.core.event_rx.try_recv().expect("events buffered");
        drain_events(&mut world, first);
        for i in 0..40 {
            fold_notice(&mut world, &format!("historical notice {i}"));
        }
        app.request_redraw();
        app.render(&root).expect("render populated transcript");

        let transcript = Rc::clone(&shell.borrow().transcript);
        let transcript_ctx = draw_ctx(40, 8);
        transcript
            .borrow_mut()
            .scroll_to_top(&mut EventContext::new());
        let _ = transcript.borrow_mut().draw(&transcript_ctx);
        assert!(!transcript.borrow().is_at_bottom(), "starts in history");
        shell.borrow().editor.borrow_mut().set_text("new prompt");

        writer.write_all(b"\t").expect("write Tab");
        let event = app.next_input().await.expect("Tab event");
        app.handle_input(event);
        assert!(transcript.borrow().in_focus_mode());

        writer.write_all(b"\x1b\r").expect("write Alt+Enter");
        let event = app.next_input().await.expect("Alt+Enter event");
        app.handle_input(event);

        assert_eq!(shell.borrow().take_host_action(), None);
        assert_eq!(shell.borrow().editor.borrow().text(), "new prompt");
        assert!(world.turns.is_empty(), "no turn was spawned");
        assert!(transcript.borrow().in_focus_mode(), "focus is preserved");
        let _ = transcript.borrow_mut().draw(&transcript_ctx);
        assert!(!transcript.borrow().is_at_bottom(), "scroll is preserved");
    }

    /// Alt+Enter is also inert outside the editor while a turn is busy, so it
    /// cannot consume the draft or mutate the steering queue.
    #[tokio::test]
    async fn focused_busy_alt_enter_does_not_steer() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, mut writer, mut world, shell, root) =
            init_app_with_world(&dir, "streaming-text").await;

        handle_submit(&mut world, "earlier prompt".to_string());
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("turn settles");
        let first = world.core.event_rx.try_recv().expect("events buffered");
        drain_events(&mut world, first);
        for i in 0..40 {
            fold_notice(&mut world, &format!("historical notice {i}"));
        }
        app.request_redraw();
        app.render(&root).expect("render populated transcript");
        handle_submit(&mut world, "running prompt".to_string());

        let transcript = Rc::clone(&shell.borrow().transcript);
        let transcript_ctx = draw_ctx(40, 8);
        transcript
            .borrow_mut()
            .scroll_to_top(&mut EventContext::new());
        let _ = transcript.borrow_mut().draw(&transcript_ctx);
        shell.borrow().editor.borrow_mut().set_text("steer draft");

        writer.write_all(b"\t").expect("write Tab");
        let event = app.next_input().await.expect("Tab event");
        app.handle_input(event);
        writer.write_all(b"\x1b\r").expect("write Alt+Enter");
        let event = app.next_input().await.expect("Alt+Enter event");
        app.handle_input(event);

        assert_eq!(shell.borrow().take_host_action(), None);
        assert_eq!(shell.borrow().editor.borrow().text(), "steer draft");
        assert!(!world.core.message_queues.has_pending(AgentId::Main));
        assert!(transcript.borrow().in_focus_mode(), "focus is preserved");
        let _ = transcript.borrow_mut().draw(&transcript_ctx);
        assert!(!transcript.borrow().is_at_bottom(), "scroll is preserved");

        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
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

    /// The two overlay openers park their command for the host to open the
    /// overlay on the next drive-loop step.
    #[tokio::test]
    async fn opener_host_actions_park_their_commands() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

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
    }

    /// The clipboard-paste insertion drops the tempfile path at the editor
    /// cursor as a bare reference (no `@` prefix, no attachment) and reports
    /// the editor changed. We test this against a bare editor because the
    /// real clipboard read is environment-dependent and cannot run headless.
    #[test]
    fn insert_pasted_image_path_inserts_bare_path() {
        let editor = TextArea::new();
        // Seed an in-progress draft so the assertion proves the path lands at
        // the cursor without clobbering existing text. A whole-buffer set_text
        // would drop the draft and fail here.
        editor.borrow_mut().set_text("draft ");
        let path = PathBuf::from("/tmp/aj-clipboard-test.png");
        assert!(insert_pasted_image_path(&editor, &path));
        assert_eq!(editor.borrow().text(), "draft /tmp/aj-clipboard-test.png");
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

    fn left_mouse_at(row: i16, col: i16, kind: vaxis::mouse::Type) -> Event {
        Event::Mouse(vaxis::mouse::Mouse {
            col,
            row,
            xoffset: 0,
            yoffset: 0,
            button: vaxis::mouse::Button::Left,
            mods: vaxis::mouse::Modifiers::empty(),
            kind,
        })
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
            let content_styles = ContentStyles::from_theme(&shell.theme.read());
            open_palette(
                &shell.overlays,
                &editor,
                &shell.chrome,
                &shell.command_slot,
                &shell.fetch_slot,
                content_styles,
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

        // The help is taller than its fixed-height overlay and the palette-open
        // row sits in a lower section, so scroll to the bottom to bring the
        // resolved shortcut on screen. End maps to scroll-to-bottom in the
        // content overlay.
        writer.write_all(b"\x1bOF").expect("write end");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
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
            "",
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

    /// With the editor focused, Esc resumes follow-tail after a manual scroll.
    /// The same gesture in transcript-focus mode is handled by the transcript
    /// itself before the event can bubble here.
    #[tokio::test]
    async fn esc_resumes_follow_tail_after_wheel_scroll() {
        let chat = empty_chat();
        fold_lines(&chat, 80);
        let (mut app, mut writer, shell, _root) = init_app_with_chat(chat).await;
        let transcript = Rc::clone(&shell.borrow().transcript);

        for _ in 0..2 {
            app.handle_input(wheel_up_at(3, 3));
        }
        let _ = shell.borrow_mut().draw(&full_draw_ctx());
        assert!(
            !transcript.borrow().is_at_bottom(),
            "wheel-up leaves the transcript in history"
        );
        assert!(
            !transcript.borrow().is_following_tail(),
            "wheel-up disengages follow-tail"
        );

        writer.write_all(b"\x1b").expect("write esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(
            transcript.borrow().is_following_tail(),
            "Esc re-engages follow-tail"
        );
        let _ = shell.borrow_mut().draw(&full_draw_ctx());
        assert!(
            transcript.borrow().is_at_bottom(),
            "Esc returns the transcript to its live tail"
        );
    }

    /// Branch cancellation owns the first Esc and leaves a detached viewport
    /// untouched. A later Esc can resume following.
    #[tokio::test]
    async fn armed_branch_escape_cancels_before_resuming_follow_tail() {
        let chat = empty_chat();
        fold_lines(&chat, 80);
        let (mut app, mut writer, shell, _root) = init_app_with_chat(chat).await;
        let transcript = Rc::clone(&shell.borrow().transcript);

        for _ in 0..2 {
            app.handle_input(wheel_up_at(3, 3));
        }
        let _ = shell.borrow_mut().draw(&full_draw_ctx());
        {
            let shell = shell.borrow();
            arm_branch(
                &shell.branch_anchor,
                &shell.branch_indicator,
                "m1".to_string(),
                branch_indicator_text("branch draft"),
            );
        }

        writer.write_all(b"\x1b").expect("write first esc");
        let event = app.next_input().await.expect("first input event");
        app.handle_input(event);
        let _ = shell.borrow_mut().draw(&full_draw_ctx());
        assert!(shell.borrow().branch_anchor.borrow().is_none());
        assert!(shell.borrow().take_branch_cancelled());
        assert!(
            !transcript.borrow().is_following_tail(),
            "branch cancellation preserves detached follow state"
        );
        assert!(
            !transcript.borrow().is_at_bottom(),
            "branch cancellation preserves the historical viewport"
        );

        writer.write_all(b"\x1b").expect("write second esc");
        let event = app.next_input().await.expect("second input event");
        app.handle_input(event);
        assert!(
            transcript.borrow().is_following_tail(),
            "a later Esc resumes following"
        );
    }

    /// Mouse selection keeps editor focus. Its first Esc clears the selection
    /// without moving the historical viewport, and the next resumes following.
    #[tokio::test]
    async fn editor_focused_selection_clears_before_following_resumes() {
        let chat = empty_chat();
        fold_lines(&chat, 80);
        let (mut app, mut writer, shell, _root) = init_app_with_chat(chat).await;
        let transcript = Rc::clone(&shell.borrow().transcript);

        for _ in 0..2 {
            app.handle_input(wheel_up_at(3, 3));
        }
        let _ = shell.borrow_mut().draw(&full_draw_ctx());
        app.handle_input(left_mouse_at(3, 3, vaxis::mouse::Type::Press));
        app.handle_input(left_mouse_at(3, 9, vaxis::mouse::Type::Drag));
        app.handle_input(left_mouse_at(3, 9, vaxis::mouse::Type::Release));
        assert!(transcript.borrow().has_selection(), "selection is live");
        assert!(
            !transcript.borrow().in_focus_mode(),
            "mouse selection leaves the editor focused"
        );

        writer.write_all(b"\x1b").expect("write first esc");
        let event = app.next_input().await.expect("first input event");
        app.handle_input(event);
        let _ = shell.borrow_mut().draw(&full_draw_ctx());
        assert!(
            !transcript.borrow().has_selection(),
            "first Esc clears the selection"
        );
        assert!(
            !transcript.borrow().is_following_tail(),
            "clearing the selection does not resume following"
        );
        assert!(
            !transcript.borrow().is_at_bottom(),
            "clearing the selection preserves the historical viewport"
        );

        writer.write_all(b"\x1b").expect("write second esc");
        let event = app.next_input().await.expect("second input event");
        app.handle_input(event);
        assert!(
            transcript.borrow().is_following_tail(),
            "second Esc resumes following"
        );
    }

    /// Shell leaves an otherwise unclaimed Esc alone when follow-tail is
    /// already engaged.
    #[test]
    fn already_following_escape_is_not_consumed_or_redrawn() {
        let shell = test_shell_with_chat(empty_chat());
        assert!(shell.borrow().transcript.borrow().is_following_tail());
        let mut ctx = EventContext::new();
        shell.borrow_mut().handle_event(
            &mut ctx,
            &Event::KeyPress(Key {
                codepoint: Key::ESCAPE,
                mods: Modifiers::empty(),
                ..Key::default()
            }),
        );
        assert!(!ctx.consume_event, "idempotent Esc remains unclaimed");
        assert!(!ctx.redraw, "idempotent Esc does not redraw");
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
            "",
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

    /// The global settings window shows the PERSISTED user-layer value, not the
    /// effective (project-overlaid) one. This is the window that edits
    /// `~/.aj/config.toml`, so it must reflect the user layer even when a
    /// project override diverges from it.
    #[tokio::test]
    async fn global_settings_window_shows_the_user_layer_not_the_effective() {
        let dir = TempDir::new().expect("tempdir");
        // User default is `auto_compact = true`; the project overrides it to
        // `false`, so the user layer and the effective config diverge.
        let mut project = aj_conf::ConfigLayer::default();
        project
            .set_str("auto_compact", "false")
            .expect("valid override");
        let layers = ConfigLayers {
            user: Config::default(),
            project,
            project_path: Some(dir.path().join("repo").join(".aj").join("config.toml")),
        };
        let (mut world, shell, _app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", layers).await;

        // Sanity: the layers really do diverge on this option.
        assert!(
            world.config_layers.lock().unwrap().user.auto_compact,
            "user layer default is true"
        );
        assert!(
            !world.config.lock().unwrap().auto_compact,
            "effective config picks up the project override"
        );

        apply_command(&mut world, &shell, CommandAction::OpenSettings).await;
        let shown = {
            let shell = shell.borrow();
            let ui = shell.settings_ui.borrow();
            ui.as_ref().unwrap().list.borrow().value_of("auto_compact")
        };
        assert_eq!(
            shown.as_deref(),
            Some("true"),
            "the global window shows the user value, not the effective override"
        );
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
        )
        .await;
        assert!(matches!(effect, ActionEffect::Redraw));
        assert_eq!(world.chat.borrow().active_view(), AgentId::Sub(2));
    }

    /// Observing an agent whose thinking level differs from the current view
    /// re-tints the editor border to the newly viewed agent's level. The picker
    /// only switches the view. The reconcile the test runs after it is what
    /// recomputes the border, so this pins that the reconcile follows the active
    /// view's thinking tint across an Observe switch.
    #[tokio::test]
    async fn agent_picker_observe_retints_the_editor_border() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);

        // Pin the main view to a low level, then seed an observable sub-agent
        // at a high level, so the two views resolve to distinct border tints.
        let mut watch = inert_theme_watch();
        apply_selector_activity(
            &mut world,
            &shell,
            &mut watch,
            vec![SelectorActivity::ThinkingConfirmed {
                target: AgentId::Main,
                level: Some(ThinkingConfig::Minimal),
            }],
        )
        .await;
        let _ = reduce(
            &mut world.chat.borrow_mut(),
            &mut world.core.lifecycle,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "xhigh".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
        );

        // The border rests on the main view's minimal tint before the switch.
        // The reconcile is the single writer, so seed through it here.
        sync_editor_chrome(&world, &shell);
        let before = editor_border_fg(&shell);
        assert_eq!(
            before,
            editor_border_color(&theme, Some(&ThinkingConfig::Minimal)),
            "border seeded to the main view's minimal tint"
        );

        let effect = apply_picker_outcome(
            &mut world,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Sub(1)),
        )
        .await;
        assert!(matches!(effect, ActionEffect::Redraw));
        assert_eq!(world.chat.borrow().active_view(), AgentId::Sub(1));

        // The picker only switches the view; the per-iteration reconcile is what
        // recomputes the border to the newly viewed sub-agent's tint.
        sync_editor_chrome(&world, &shell);
        let after = editor_border_fg(&shell);
        assert_eq!(
            after,
            editor_border_color(&theme, Some(&ThinkingConfig::XHigh)),
            "the view switch re-tints the border to the sub-agent's xhigh tint"
        );
        assert_ne!(before, after, "the border moved with the active view");
    }

    /// The border of a FINISHED sub-agent still follows the sub's thinking
    /// level, not main's. Footer settings are retained past `AgentEnd`, so the
    /// reconcile must resolve the viewed sub's level even after it completes.
    #[tokio::test]
    async fn border_tracks_a_finished_sub_agents_thinking_level() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);

        // Pin the main view to a low level so it resolves to a distinct tint.
        let mut watch = inert_theme_watch();
        apply_selector_activity(
            &mut world,
            &shell,
            &mut watch,
            vec![SelectorActivity::ThinkingConfirmed {
                target: AgentId::Main,
                level: Some(ThinkingConfig::Minimal),
            }],
        )
        .await;

        // Seed a sub-agent at a distinct high level, then finish it. The
        // reducer's `SubAgentEnd`/`AgentEnd` arms mark the box done but leave
        // the footer settings in place, so the sub's level must survive.
        let _ = reduce(
            &mut world.chat.borrow_mut(),
            &mut world.core.lifecycle,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "xhigh".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
        );
        let _ = reduce(
            &mut world.chat.borrow_mut(),
            &mut world.core.lifecycle,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                report: "done".into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
        );
        let _ = reduce(
            &mut world.chat.borrow_mut(),
            &mut world.core.lifecycle,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );

        // Observe the finished sub-agent and reconcile the chrome.
        let _ = apply_picker_outcome(
            &mut world,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Sub(1)),
        )
        .await;
        sync_editor_chrome(&world, &shell);

        let border = editor_border_fg(&shell);
        assert_eq!(
            border,
            editor_border_color(&theme, Some(&ThinkingConfig::XHigh)),
            "a finished sub-agent's border still follows its own thinking level"
        );
        assert_ne!(
            border,
            editor_border_color(&theme, Some(&ThinkingConfig::Minimal)),
            "the border did not fall back to the main view's tint"
        );
    }

    /// Reads the editor's top-border row as a single string. The top rule is
    /// row 0, so the inlaid agent marker (if any) lands there.
    fn editor_top_bar_text(shell: &Rc<RefCell<Shell>>) -> String {
        let surf = shell.borrow().editor.borrow_mut().draw(&draw_ctx(100, 30));
        (0..surf.size.width)
            .map(|c| surf.read_cell(c, 0).char.grapheme().to_string())
            .collect()
    }

    /// Observing a sub-agent inlays an `agent N` marker into the editor's top
    /// bar, and observing the main agent clears it. This pins both halves of
    /// the marker through the real Observe apply path.
    #[tokio::test]
    async fn agent_picker_observe_marks_the_editor_top_bar() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        // Seed an observable sub-agent to switch the view onto.
        let _ = reduce(
            &mut world.chat.borrow_mut(),
            &mut world.core.lifecycle,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "standard".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
        );

        // Observing the sub-agent inlays its `agent N` marker.
        let effect = apply_picker_outcome(
            &mut world,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Sub(1)),
        )
        .await;
        assert!(matches!(effect, ActionEffect::Redraw));
        // The picker switches the view; the reconcile writes the marker.
        sync_editor_chrome(&world, &shell);
        // Pin the exact marker text, not just a substring: `sub-agent 1` also
        // contains `agent 1`, so the `!contains("sub-agent")` guard is what
        // rejects a wrong `sub-agent {n}` label.
        let top = editor_top_bar_text(&shell);
        assert!(
            top.contains("agent 1") && !top.contains("sub-agent"),
            "observing a sub-agent inlays the `agent N` marker"
        );

        // Observing the main agent clears the marker again.
        let effect = apply_picker_outcome(
            &mut world,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Main),
        )
        .await;
        assert!(matches!(effect, ActionEffect::Redraw));
        sync_editor_chrome(&world, &shell);
        assert!(
            !editor_top_bar_text(&shell).contains("agent"),
            "observing the main agent clears the marker"
        );
    }

    /// The reconcile clears a stale `agent N` marker once the active view
    /// returns to the main agent, which is the view a fresh-session install
    /// lands on. Covers both halves: setting the marker for an observed
    /// sub-agent and clearing it back on the main view.
    #[tokio::test]
    async fn sync_editor_chrome_clears_the_marker_on_the_main_view() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        // Observe a sub-agent so the editor carries an `agent 1` marker.
        let _ = reduce(
            &mut world.chat.borrow_mut(),
            &mut world.core.lifecycle,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "standard".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
        );
        let _ = apply_picker_outcome(
            &mut world,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Sub(1)),
        )
        .await;
        sync_editor_chrome(&world, &shell);
        assert!(
            editor_top_bar_text(&shell).contains("agent 1"),
            "sub-agent observed, marker set"
        );

        // A fresh session lands on the main view; the reconcile clears the
        // stale marker once the active view is back on Main.
        world.chat.borrow_mut().set_active_view(AgentId::Main);
        sync_editor_chrome(&world, &shell);
        assert!(
            !editor_top_bar_text(&shell).contains("agent"),
            "the reconcile clears the stale sub-agent marker on the main view"
        );
    }

    /// A session install reconciles the editor chrome onto the new session, so
    /// its first paint is correct without waiting for the drive loop's
    /// bottom-of-iteration reconcile. Observing a sub-agent bakes an `agent N`
    /// marker and the sub's tint into the editor, and the editor persists across
    /// the chat swap, so after the install the marker must be gone and the
    /// border must match the fresh main view's tint. Asserted without a manual
    /// reconcile: the install is the single writer for this window.
    #[tokio::test]
    async fn install_reconciles_the_editor_chrome_onto_the_new_session() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);

        // Observe a sub-agent at a level distinct from the default (a fresh
        // main view falls back to the run config's level), so the editor
        // carries both an `agent 1` marker and a border tint that visibly moves
        // across the switch.
        let _ = reduce(
            &mut world.chat.borrow_mut(),
            &mut world.core.lifecycle,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "minimal".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
        );
        let _ = apply_picker_outcome(
            &mut world,
            &shell,
            AgentPickerOutcome::Observe(AgentId::Sub(1)),
        )
        .await;
        sync_editor_chrome(&world, &shell);
        let sub_tint = editor_border_fg(&shell);
        assert!(
            editor_top_bar_text(&shell).contains("agent 1"),
            "sub-agent observed, marker set before the switch"
        );
        assert_eq!(
            sub_tint,
            editor_border_color(&theme, Some(&ThinkingConfig::Minimal)),
            "border carries the observed sub-agent's tint before the switch"
        );

        // Build and install a fresh session. Installing lands on the main view.
        let previous_id = world.core.session_id.clone();
        let next = build_next_session(
            &world,
            SessionSpec::Create {
                entry: SessionEntry::Switch,
            },
            &previous_id,
            false,
        )
        .await
        .expect("build fresh next session");
        install_next_session(&mut world, &shell, next);

        // No manual `sync_editor_chrome`: the install already reconciled the
        // chrome, so the first post-install paint shows the fresh main view.
        let fresh_main_tint =
            editor_border_color(&theme, viewed_thinking(&world, AgentId::Main).as_ref());
        assert_ne!(
            fresh_main_tint, sub_tint,
            "the fresh main tint must differ from the sub's tint for this test to bite"
        );
        assert!(
            !editor_top_bar_text(&shell).contains("agent"),
            "the install cleared the stale `agent N` marker"
        );
        assert_eq!(
            editor_border_fg(&shell),
            fresh_main_tint,
            "the install re-tinted the border to the fresh main view"
        );
    }

    /// The drive loop reconciles the editor chrome once per iteration, so a
    /// view change that lands outside an explicit reconcile still shows up. We
    /// point the active view at a sub-agent without touching the chrome, drive
    /// one benign key through the real loop to an EOF quit, and confirm the
    /// loop's bottom-of-iteration `sync_editor_chrome` baked the `agent 1`
    /// marker and the sub's tint. This pins the per-iteration call: delete it
    /// and the chrome stays at the resting default.
    #[tokio::test]
    async fn drive_loop_reconciles_the_editor_chrome_each_iteration() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);

        // Seed a sub-agent and point the active view at it, leaving the editor
        // chrome at its resting default. Only the loop's reconcile should move
        // it.
        let _ = reduce(
            &mut world.chat.borrow_mut(),
            &mut world.core.lifecycle,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "xhigh".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
        );
        world.chat.borrow_mut().set_active_view(AgentId::Sub(1));

        let mut theme_watch = inert_theme_watch();
        let mut prompt_history_rx: Option<UnboundedReceiver<Vec<String>>> = None;
        let mut autocomplete_rx = shell
            .borrow()
            .editor
            .borrow_mut()
            .take_autocomplete_rx()
            .expect("editor hands out its autocomplete receiver once");

        // One benign key forces a full loop iteration, whose
        // bottom-of-iteration reconcile runs, then EOF (the dropped writer)
        // quits on the next iteration before it reaches that reconcile.
        writer.write_all(b"x").expect("benign key");
        drop(writer);
        let exit = drive(
            &mut app,
            &root,
            &shell,
            &mut world,
            &mut theme_watch,
            &mut prompt_history_rx,
            &mut autocomplete_rx,
        )
        .await
        .expect("drive exits without a fatal error");
        assert!(matches!(exit, SessionExit::Quit), "EOF quit the loop");

        assert!(
            editor_top_bar_text(&shell).contains("agent 1"),
            "the loop's reconcile inlaid the observed sub-agent's marker"
        );
        assert_eq!(
            editor_border_fg(&shell),
            editor_border_color(&theme, Some(&ThinkingConfig::XHigh)),
            "the loop's reconcile tinted the border to the sub-agent's level"
        );
    }

    /// A confirmed task pick opens and refocuses its viewer before another
    /// queued key can dispatch through the closed picker path.
    #[tokio::test]
    async fn draining_open_task_outcome_refocuses_the_viewer() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        let id = register_bash_task(&world, "cargo build");
        seed_sub_and_task(&mut world);
        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::OpenAgentPicker).await,
            ActionEffect::OpenedOverlay
        ));
        focus_overlay(&mut app, &root);

        writer
            .write_all(b"cargo build\r")
            .expect("filter and confirm task");
        for _ in 0..12 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "confirm closed picker"
        );

        apply_pending_picker_outcome(&mut world, &shell, &mut app).await;
        assert!(shell.borrow().overlays.borrow().is_open(), "viewer open");

        writer.write_all(&[0x0b]).expect("write ctrl+k");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(
            shell.borrow().overlays.borrow().is_open(),
            "viewer handled Ctrl+K without the stale picker closing it"
        );
        assert!(
            shell.borrow().take_picker_outcome().is_none(),
            "stale picker did not park another outcome"
        );
        assert!(world.core.task_registry.summary(id).is_some());

        writer.write_all(b"\x1b").expect("write esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(
            !shell.borrow().overlays.borrow().is_open(),
            "focused viewer handled Esc"
        );
    }

    /// Drilling into a task opens the viewer overlay; an id that has left
    /// the registry folds a notice instead.
    #[tokio::test]
    async fn agent_picker_open_task_opens_the_viewer() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let id = register_bash_task(&world, "cargo test");

        let effect =
            apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::OpenTask(id)).await;
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        assert!(shell.borrow().overlays.borrow().is_open(), "viewer open");

        let effect =
            apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::OpenTask(9_999)).await;
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

        apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::Kill(id)).await;
        world
            .core
            .task_registry
            .set_status(id, aj_agent::tool::TaskStatus::Killed);
        apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::Kill(id)).await;
        apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::Kill(9_999)).await;

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
        apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::OpenTask(id)).await;
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

    /// The session selector opens read-only even mid-turn (the switch is
    /// refused at confirm time, not by refusing to open). `NewSession` still
    /// refuses mid-turn (it starts a fresh session with no overlay to gate
    /// the switch), raising a toast and parking nothing.
    #[tokio::test]
    async fn new_session_refused_mid_turn_selector_opens() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        handle_submit(&mut world, "go".to_string());
        assert!(world.turn_cancels.contains_key(&AgentId::Main), "busy");

        // The selector opens read-only mid-turn.
        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::OpenSessionSelector).await,
            ActionEffect::OpenedOverlay
        ));
        assert!(
            shell.borrow().overlays.borrow().is_open(),
            "the selector opens read-only mid-turn"
        );
        assert!(
            !main_notices(&world)
                .iter()
                .any(|n| n.contains("switch sessions")),
            "no open-time refusal notice: {:?}",
            main_notices(&world)
        );
        shell.borrow().overlays.borrow_mut().close_all();

        // NewSession is still refused mid-turn.
        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::NewSession).await,
            ActionEffect::Redraw
        ));
        assert!(
            shell.borrow().take_session_request().is_none(),
            "no new-session parked mid-turn"
        );
        assert!(
            crate::toasts::toast_texts(&shell.borrow().toasts)
                .iter()
                .any(|m| m.contains("Can't start a new session while work is running")),
            "{:?}",
            crate::toasts::toast_texts(&shell.borrow().toasts)
        );

        // Settle the turn so teardown is clean.
        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
    }

    /// `NewSession` joins the refuse-while-busy rule for BACKGROUND work too
    /// (no turn in flight): the command refuses with a toast and parks
    /// nothing, and the consumption-site recheck refuses a `New` request that
    /// slipped through, then proceeds once idle.
    #[tokio::test]
    async fn new_session_refused_while_background_work_runs() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        assert!(world.turns.is_empty(), "no turn in flight");
        let task = register_bash_task(&world, "sleep 100");

        // The command refuses up front.
        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::NewSession).await,
            ActionEffect::Redraw
        ));
        assert!(
            shell.borrow().take_session_request().is_none(),
            "no new-session parked while background work runs"
        );
        assert!(
            crate::toasts::toast_texts(&shell.borrow().toasts)
                .iter()
                .any(|m| m.contains("Can't start a new session while work is running")),
            "the refusal raises the toast"
        );

        // The consumption site rechecks a request that slipped through.
        assert!(
            consume_session_request(&mut world, &shell, SessionRequest::New).is_none(),
            "a running background task refuses the parked new-session request"
        );

        // Idle (task terminal): the request proceeds to a new-session exit.
        world
            .core
            .task_registry
            .set_status(task, aj_agent::tool::TaskStatus::Killed);
        assert!(matches!(
            consume_session_request(&mut world, &shell, SessionRequest::New),
            Some(SessionExit::New)
        ));
    }

    /// The session tree opens read-only even mid-turn (the branch switch it
    /// leads to is refused at confirm time, not by refusing to open).
    #[tokio::test]
    async fn session_tree_opens_mid_turn() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        handle_submit(&mut world, "go".to_string());
        assert!(world.turn_cancels.contains_key(&AgentId::Main), "busy");

        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::OpenSessionTree).await,
            ActionEffect::OpenedOverlay
        ));
        assert!(
            shell.borrow().overlays.borrow().is_open(),
            "the tree opens read-only mid-turn"
        );
        assert!(
            !main_notices(&world)
                .iter()
                .any(|n| n.contains("open the session tree")),
            "no open-time refusal notice: {:?}",
            main_notices(&world)
        );

        // Settle the turn so teardown is clean.
        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
    }

    /// While idle the session tree opens read-only, listing the current
    /// session's single branch (its first user message).
    #[tokio::test]
    async fn session_tree_opens_and_lists_the_branch_when_idle() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "tree branch prompt").await;

        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::OpenSessionTree).await,
            ActionEffect::OpenedOverlay
        ));
        assert!(
            shell.borrow().overlays.borrow().is_open(),
            "the tree overlay opened"
        );
    }

    /// The toast stack renders ABOVE an open session overlay: the busy-refuse
    /// keeps the overlay open, so the toast must float over it (z above the
    /// scrim/overlay) rather than being suppressed like the quit hint.
    #[tokio::test]
    async fn toast_stack_renders_over_an_open_overlay() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        apply_command(&mut world, &shell, CommandAction::OpenSessionTree).await;
        assert!(
            shell.borrow().overlays.borrow().is_open(),
            "the overlay is open"
        );
        shell
            .borrow()
            .show_toast("Can't switch branches while work is running.");
        let body = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(
            body.contains("Session tree"),
            "the overlay is drawn: {body}"
        );
        assert!(
            body.contains("work is running"),
            "the toast floats over the overlay: {body}"
        );
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
                head: None,
            },
            &alpha_id,
            false,
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
            format!("aj-next - session {beta}")
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
            false,
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

    // --- Branch flow (Phase 3) ---

    /// The footer branch indicator labels and truncates the prefilled message
    /// to one line.
    #[test]
    fn branch_indicator_text_labels_and_truncates() {
        let short = branch_indicator_text("hello world");
        assert_eq!(short, "branching from message: hello world");
        // A multi-line message collapses to its first non-empty line.
        let multi = branch_indicator_text("\n  first line  \nsecond line");
        assert_eq!(multi, "branching from message: first line");
        // A long line is truncated with an ellipsis.
        let long = branch_indicator_text(&"x".repeat(100));
        assert!(long.ends_with('\u{2026}'), "truncated: {long}");
        assert!(long.chars().count() < 100);
    }

    /// The prompt-safety invariant: auto-submit only on a clean rebuild (no
    /// fallback and the head override installed); every other outcome restores.
    #[test]
    fn branch_prompt_submits_only_on_clean_head_apply() {
        assert!(branch_prompt_should_submit(false, Some(true)));
        assert!(
            !branch_prompt_should_submit(true, Some(true)),
            "a build fallback must not submit against the wrong head"
        );
        assert!(
            !branch_prompt_should_submit(false, Some(false)),
            "a stale head override must not submit"
        );
        assert!(
            !branch_prompt_should_submit(false, None),
            "no override requested is never a branch submit"
        );
    }

    /// The branch confirmation distinguishes the `b`-submit flow (a prompt is
    /// handed off) from a tree-view switch (a bare head move), and reports a
    /// stale-head tree switch that the prompt handoff would otherwise leave
    /// silent.
    #[test]
    fn branch_switch_notice_distinguishes_b_flow_from_tree_switch() {
        // `b`-flow success and tree-switch success get distinct wording.
        assert_eq!(
            branch_switch_notice(true, true),
            Some("Branched the conversation from an earlier message.")
        );
        assert_eq!(
            branch_switch_notice(false, true),
            Some("Switched to the selected branch.")
        );
        // A `b`-flow stale head folds nothing here: its prompt handoff folds
        // the restore notice instead.
        assert_eq!(branch_switch_notice(true, false), None);
        // A tree-view stale head has no prompt handoff, so it reports here.
        assert_eq!(
            branch_switch_notice(false, false),
            Some("Couldn't switch to that branch.")
        );
    }

    /// Arming sets the anchor and the footer indicator; re-arming replaces
    /// both; disarming clears them.
    #[tokio::test]
    async fn arming_sets_indicator_and_rearm_replaces_and_disarm_clears() {
        let dir = TempDir::new().expect("tempdir");
        let (_world, shell) = world_and_shell(&dir, "streaming-text").await;
        {
            let sh = shell.borrow();
            arm_branch(
                &sh.branch_anchor,
                &sh.branch_indicator,
                "m1".to_string(),
                branch_indicator_text("first message"),
            );
        }
        assert_eq!(
            shell
                .borrow()
                .branch_anchor
                .borrow()
                .as_ref()
                .map(|a| a.message_id.clone()),
            Some("m1".to_string())
        );
        // The footer renders the indicator.
        let row = {
            let sh = shell.borrow();
            let surface = sh
                .footer
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(120, None));
            crate::test_support::rows(&surface).join("\n")
        };
        assert!(
            row.contains("branching from message: first message"),
            "footer shows the indicator: {row:?}"
        );

        // Re-arming replaces the anchor and indicator.
        {
            let sh = shell.borrow();
            arm_branch(
                &sh.branch_anchor,
                &sh.branch_indicator,
                "m2".to_string(),
                branch_indicator_text("second message"),
            );
        }
        assert_eq!(
            shell
                .borrow()
                .branch_anchor
                .borrow()
                .as_ref()
                .map(|a| a.message_id.clone()),
            Some("m2".to_string())
        );
        assert_eq!(
            shell.borrow().branch_indicator.borrow().clone(),
            Some("branching from message: second message".to_string())
        );

        // Disarming clears both slots.
        shell.borrow().disarm_branch();
        assert!(shell.borrow().branch_anchor.borrow().is_none());
        assert!(shell.borrow().branch_indicator.borrow().is_none());
    }

    /// Esc cancels an armed anchor: it clears the anchor and indicator, flags
    /// the drive loop to fold the cancel notice, and consumes the key. The
    /// editor text is left untouched (the user's to keep). The popup-first
    /// priority is structural: the editor consumes Esc at the target phase
    /// when its autocomplete popup is open, so this bubble-phase handler only
    /// runs with the popup closed.
    #[tokio::test]
    async fn esc_cancels_the_armed_anchor() {
        let dir = TempDir::new().expect("tempdir");
        let (_world, shell) = world_and_shell(&dir, "streaming-text").await;
        shell.borrow().editor.borrow_mut().set_text("kept draft");
        {
            let sh = shell.borrow();
            arm_branch(
                &sh.branch_anchor,
                &sh.branch_indicator,
                "m1".to_string(),
                branch_indicator_text("kept draft"),
            );
        }
        let mut ctx = EventContext::new();
        shell.borrow_mut().handle_event(
            &mut ctx,
            &Event::KeyPress(Key {
                codepoint: Key::ESCAPE,
                mods: Modifiers::empty(),
                ..Key::default()
            }),
        );
        assert!(ctx.consume_event, "Esc is consumed");
        assert!(
            shell.borrow().branch_anchor.borrow().is_none(),
            "anchor cleared"
        );
        assert!(
            shell.borrow().branch_indicator.borrow().is_none(),
            "indicator cleared"
        );
        assert!(
            shell.borrow().take_branch_cancelled(),
            "cancel flag set for the drive loop's notice"
        );
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "kept draft",
            "the editor text stays after cancel"
        );
    }

    /// Steer and dequeue are refused while a branch anchor is armed, keeping
    /// the anchor and the editor draft intact (both are incoherent with a
    /// pending branch).
    #[tokio::test]
    async fn steer_and_dequeue_refused_while_branch_armed() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        shell.borrow().editor.borrow_mut().set_text("branch draft");
        {
            let sh = shell.borrow();
            arm_branch(
                &sh.branch_anchor,
                &sh.branch_indicator,
                "m1".to_string(),
                branch_indicator_text("branch draft"),
            );
        }
        // Steer is refused: the editor keeps its draft and the anchor stays.
        assert!(handle_host_action(&mut world, &shell, AjAction::Steer));
        assert_eq!(shell.borrow().editor.borrow().text(), "branch draft");
        assert!(shell.borrow().branch_anchor.borrow().is_some());
        // Dequeue is refused the same way.
        assert!(handle_host_action(&mut world, &shell, AjAction::Dequeue));
        assert_eq!(shell.borrow().editor.borrow().text(), "branch draft");
        assert!(shell.borrow().branch_anchor.borrow().is_some());
    }

    /// An empty (post-trim) submit while armed is refused and keeps the
    /// anchor: the head must not move for a prompt that would be dropped.
    #[tokio::test]
    async fn empty_submit_refused_keeps_anchor() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        {
            let sh = shell.borrow();
            arm_branch(
                &sh.branch_anchor,
                &sh.branch_indicator,
                "m1".to_string(),
                branch_indicator_text("x"),
            );
        }
        let outcome = submit_with_armed_anchor(&mut world, &shell, "   ".to_string()).await;
        assert!(matches!(outcome, ArmedSubmit::Stay));
        assert!(
            shell.borrow().branch_anchor.borrow().is_some(),
            "the anchor is kept on an empty submit"
        );
    }

    /// A submit while busy (here a live turn) is refused with a toast, keeping
    /// the anchor and restoring the editor text the submit cleared, and spawns
    /// no rebuild or new turn.
    #[tokio::test]
    async fn busy_submit_refused_toasts_keeps_anchor_and_restores_text() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        // Start a turn so the world is busy.
        handle_submit(&mut world, "first".to_string());
        assert!(!world.turns.is_empty(), "a turn is in flight");
        let turns_before = world.turns.len();
        {
            let sh = shell.borrow();
            arm_branch(
                &sh.branch_anchor,
                &sh.branch_indicator,
                "m1".to_string(),
                branch_indicator_text("edited"),
            );
        }
        let outcome = submit_with_armed_anchor(&mut world, &shell, "edited".to_string()).await;
        assert!(matches!(outcome, ArmedSubmit::Stay));
        assert!(
            shell.borrow().branch_anchor.borrow().is_some(),
            "the anchor is kept on a busy submit"
        );
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "edited",
            "the text is restored into the editor"
        );
        assert!(
            crate::toasts::toast_texts(&shell.borrow().toasts)
                .iter()
                .any(|m| m.contains("Can't branch while work is running")),
            "the refusal raises the branch toast: {:?}",
            crate::toasts::toast_texts(&shell.borrow().toasts),
        );
        assert_eq!(
            world.turns.len(),
            turns_before,
            "no new turn spawned by the refused submit"
        );

        // Settle the turn so world teardown is clean.
        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");
    }

    /// A submit while a background bash task runs (no turn in flight) is
    /// refused the same way: a toast, the anchor kept, the text restored, and
    /// no branch resolved. This is the case the removed two-step
    /// background-task confirm used to cover.
    #[tokio::test]
    async fn background_task_submit_refused_toasts_keeps_anchor() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        assert!(world.turns.is_empty(), "no turn in flight");
        let task = register_bash_task(&world, "sleep 100");
        {
            let sh = shell.borrow();
            arm_branch(
                &sh.branch_anchor,
                &sh.branch_indicator,
                "m1".to_string(),
                branch_indicator_text("edited"),
            );
        }
        let outcome = submit_with_armed_anchor(&mut world, &shell, "edited".to_string()).await;
        assert!(matches!(outcome, ArmedSubmit::Stay));
        assert!(
            shell.borrow().branch_anchor.borrow().is_some(),
            "the anchor is kept on a background-task submit"
        );
        assert_eq!(shell.borrow().editor.borrow().text(), "edited");
        assert!(
            crate::toasts::toast_texts(&shell.borrow().toasts)
                .iter()
                .any(|m| m.contains("Can't branch while work is running")),
            "the refusal raises the branch toast"
        );
        world
            .core
            .task_registry
            .set_status(task, aj_agent::tool::TaskStatus::Killed);
    }

    /// Submitting with an anchor armed on a persisted user message resolves to
    /// a branch exit whose head is that message's parent, carrying the edited
    /// prompt. The anchor is disarmed on resolution.
    #[tokio::test]
    async fn armed_submit_branches_at_the_messages_parent() {
        use aj_models::types::Message;
        use aj_session::ConversationEntryKind;

        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        persist_session(&mut world).await;

        // The first user message on disk, plus its parent (a settings entry).
        let (message_id, expected_head) = {
            let log = world.core.log.lock().await;
            let entry = log
                .entries_in_order()
                .into_iter()
                .find(|e| {
                    matches!(
                        &e.entry,
                        ConversationEntryKind::Message { message }
                            if matches!(message.as_wire(), Some(Message::User(_)))
                    )
                })
                .expect("a persisted user message");
            (
                entry.id.clone(),
                entry
                    .parent_id
                    .clone()
                    .expect("the user message has a parent"),
            )
        };
        {
            let sh = shell.borrow();
            arm_branch(
                &sh.branch_anchor,
                &sh.branch_indicator,
                message_id,
                branch_indicator_text("persist me"),
            );
        }
        match submit_with_armed_anchor(&mut world, &shell, "edited prompt".to_string()).await {
            ArmedSubmit::Branch { head, prompt } => {
                assert_eq!(head, expected_head, "branches at the message's parent");
                assert_eq!(prompt, "edited prompt");
            }
            ArmedSubmit::Stay => panic!("expected a branch exit"),
        }
        assert!(
            shell.borrow().branch_anchor.borrow().is_none(),
            "the anchor is disarmed on resolution"
        );
    }

    /// A parked tree-view branch switch is refused (a toast raised, no exit)
    /// while a turn or a background task is live, and proceeds to a branch exit
    /// once idle. This drives the drive loop's request-consumption decision
    /// (`consume_session_request`) directly, at the layer the harness exposes.
    #[tokio::test]
    async fn parked_branch_switch_refused_while_busy_and_proceeds_when_idle() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        let head = world
            .core
            .log
            .lock()
            .await
            .head()
            .cloned()
            .expect("a persisted head");

        // A live turn refuses the switch and folds a notice.
        handle_submit(&mut world, "busy".to_string());
        assert!(!world.turns.is_empty(), "a turn is in flight");
        assert!(
            consume_session_request(
                &mut world,
                &shell,
                SessionRequest::Branch { head: head.clone() }
            )
            .is_none(),
            "a live turn refuses the branch switch"
        );
        assert!(
            crate::toasts::toast_texts(&shell.borrow().toasts)
                .iter()
                .any(|m| m.contains("Can't switch branches while work is running")),
            "the refusal raises the branch toast: {:?}",
            crate::toasts::toast_texts(&shell.borrow().toasts)
        );
        cancel_viewed_turn(&world);
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("abort is non-fatal");

        // A running background task refuses the switch too, even with no turn.
        let task = register_bash_task(&world, "cargo build");
        assert!(
            consume_session_request(
                &mut world,
                &shell,
                SessionRequest::Branch { head: head.clone() }
            )
            .is_none(),
            "a running background task refuses the branch switch"
        );

        // Idle (turn settled, task terminal): the switch proceeds to a
        // prompt-less branch exit for the selected head.
        world
            .core
            .task_registry
            .set_status(task, aj_agent::tool::TaskStatus::Killed);
        assert!(
            matches!(
                consume_session_request(
                    &mut world,
                    &shell,
                    SessionRequest::Branch { head: head.clone() }
                ),
                Some(SessionExit::Branch { head: h, prompt: None }) if h == head
            ),
            "an idle branch request maps to a prompt-less branch exit"
        );
    }

    /// The safety-net recheck for a resume: a `SessionRequest::Resume` that
    /// slipped through with background work live (a wake turn spawned between
    /// the selector's confirm and this consumption) is refused with a toast and
    /// no exit, then proceeds once idle. `consume_session_request` is the
    /// authoritative recheck since the confirm-time `busy` snapshot is stale.
    #[tokio::test]
    async fn parked_resume_refused_while_busy_and_proceeds_when_idle() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;

        // A running background task refuses the resume with a toast, no exit.
        let task = register_bash_task(&world, "cargo build");
        assert!(
            consume_session_request(
                &mut world,
                &shell,
                SessionRequest::Resume("other-session".to_string())
            )
            .is_none(),
            "a running background task refuses the resume"
        );
        assert!(
            crate::toasts::toast_texts(&shell.borrow().toasts)
                .iter()
                .any(|m| m.contains("Can't switch sessions while work is running")),
            "the refusal raises the switch toast"
        );

        // Idle (task terminal): the resume proceeds to a switch exit.
        world
            .core
            .task_registry
            .set_status(task, aj_agent::tool::TaskStatus::Killed);
        assert!(
            matches!(
                consume_session_request(
                    &mut world,
                    &shell,
                    SessionRequest::Resume("other-session".to_string())
                ),
                Some(SessionExit::Switch(id)) if id == "other-session"
            ),
            "an idle resume request maps to a switch exit"
        );
    }

    /// End-to-end tree switch (the prompt-`None` run-loop path): a two-branch
    /// session on disk, a selector confirm parks `SessionRequest::Branch`, its
    /// `into_exit` yields a prompt-less `SessionExit::Branch`, and the rebuild
    /// lands on the chosen head without auto-submitting. Switching back onto
    /// the other head shows that branch instead, the spec's round-trip check.
    ///
    /// Driving the whole `run()` loop is impractical in the harness, so we
    /// assert the chain up to `into_exit()` plus the rebuild-onto-head using
    /// `build_next_session` / `install_next_session`, folding the run loop's
    /// tree-switch notice via `apply_branch_switch_notice` exactly as `run()`
    /// does.
    #[tokio::test]
    async fn tree_switch_rebuilds_onto_the_selected_head_without_submitting() {
        use aj_agent::message::AgentMessage;
        use aj_models::types::{
            AssistantContent, AssistantMessage, Message, TextContent, UserMessage,
        };
        use aj_session::{ConversationEntryKind, ConversationLog, ThreadKind};

        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));

        let user = |t: &str| ConversationEntryKind::Message {
            message: AgentMessage::wire(Message::User(UserMessage::text(t))),
        };
        let assistant = |t: &str| ConversationEntryKind::Message {
            message: AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::Text(TextContent {
                    text: t.to_string(),
                    text_signature: None,
                })],
                ..AssistantMessage::empty()
            })),
        };

        // A fork on disk: shared prefix, then branch A and branch B off it.
        let (session_id, branch_a, branch_b) = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            log.set_system_prompt("prompt".to_string())
                .expect("system prompt");
            let sp = log.system_prompt_id().cloned().expect("system prompt id");
            let shared = log
                .append(Some(sp), ThreadKind::User, None, user("shared question"))
                .expect("shared");
            let fork = log
                .append(
                    Some(shared),
                    ThreadKind::User,
                    None,
                    assistant("shared answer"),
                )
                .expect("fork");
            let branch_a = log
                .append(
                    Some(fork.clone()),
                    ThreadKind::User,
                    None,
                    user("branch A prompt"),
                )
                .expect("branch A");
            let branch_b = log
                .append(Some(fork), ThreadKind::User, None, user("branch B prompt"))
                .expect("branch B");
            (log.session_id().to_string(), branch_a, branch_b)
        };

        let mut world = resumed_world(&dir, "streaming-text", &session_id).await;
        let shell = shell_for(&world);
        let previous_id = world.core.session_id.clone();

        // A tree-selector confirm parks a branch switch for branch A's head;
        // the drive loop maps it to a prompt-less branch exit.
        match (SessionRequest::Branch {
            head: branch_a.clone(),
        })
        .into_exit()
        {
            SessionExit::Branch { head, prompt } => {
                assert_eq!(head, branch_a, "the switch targets the selected head");
                assert!(prompt.is_none(), "a tree switch carries no prompt");
            }
            _ => panic!("a branch request maps to a branch exit"),
        }

        // Rebuild onto branch A (the run loop's branch path with no prompt).
        let mut next = build_next_session(
            &world,
            SessionSpec::Resume {
                session_id: session_id.clone(),
                entry: SessionEntry::Switch,
                head: Some(branch_a.clone()),
            },
            &previous_id,
            true,
        )
        .await
        .expect("build onto branch A");
        apply_branch_switch_notice(&mut next, true, false);
        install_next_session(&mut world, &shell, next);

        let rows = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(
            rows.contains("branch A prompt"),
            "branch A content shown: {rows}"
        );
        assert!(
            !rows.contains("branch B prompt"),
            "branch B content absent after switching to A: {rows}"
        );
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n == "Switched to the selected branch."),
            "the tree-switch notice is folded: {:?}",
            main_notices(&world)
        );
        assert!(
            world.turns.is_empty(),
            "a prompt-less tree switch spawns no turn"
        );

        // Switch back via the tree onto branch B: the transcript matches that
        // branch instead, proving each head rebuilds faithfully.
        let current_id = world.core.session_id.clone();
        let next = build_next_session(
            &world,
            SessionSpec::Resume {
                session_id: session_id.clone(),
                entry: SessionEntry::Switch,
                head: Some(branch_b.clone()),
            },
            &current_id,
            true,
        )
        .await
        .expect("build onto branch B");
        install_next_session(&mut world, &shell, next);
        let rows = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(
            rows.contains("branch B prompt"),
            "branch B content shown after switching back: {rows}"
        );
        assert!(
            !rows.contains("branch A prompt"),
            "branch A content absent after switching to B: {rows}"
        );
        assert!(world.turns.is_empty(), "still no turn spawned");
    }

    /// Submitting with an anchor armed on the file root (a user message with no
    /// parent, as in an ancient file) is refused, and the anchor is disarmed.
    /// Like the other refusals, it restores the edited text the submit cleared
    /// so the user's prompt is not silently dropped.
    #[tokio::test]
    async fn armed_submit_refused_at_root_message() {
        use aj_agent::message::AgentMessage;
        use aj_models::types::{Message, UserMessage};
        use aj_session::{ConversationEntryKind, ConversationLog, ThreadKind};

        let dir = TempDir::new().expect("tempdir");
        // Hand-build an ancient-file fixture: a single root user message with
        // no system prompt and no seeded settings, so its `parent_id` is None.
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let (session_id, root_id) = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            let root_id = log
                .append(
                    None,
                    ThreadKind::User,
                    None,
                    ConversationEntryKind::Message {
                        message: AgentMessage::wire(Message::User(UserMessage::text("root"))),
                    },
                )
                .expect("append the root user message");
            (log.session_id().to_string(), root_id)
        };
        let mut world = resumed_world(&dir, "streaming-text", &session_id).await;
        let shell = shell_for(&world);
        {
            let sh = shell.borrow();
            arm_branch(
                &sh.branch_anchor,
                &sh.branch_indicator,
                root_id,
                branch_indicator_text("root"),
            );
        }
        let outcome =
            submit_with_armed_anchor(&mut world, &shell, "edited root prompt".to_string()).await;
        assert!(
            matches!(outcome, ArmedSubmit::Stay),
            "root branch is refused"
        );
        assert!(
            shell.borrow().branch_anchor.borrow().is_none(),
            "the anchor is disarmed on a root refusal"
        );
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "edited root prompt",
            "the edited prompt is restored into the editor on a root refusal"
        );
    }

    /// A real session install (here a fresh session) clears the armed branch
    /// anchor, so it can never resolve against a different session's log. This
    /// drives `install_next_session` rather than calling `disarm_branch`
    /// directly, so a regression removing the install-time clear fails here.
    #[tokio::test]
    async fn install_next_session_clears_the_armed_anchor() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        let previous_id = world.core.session_id.clone();
        {
            let sh = shell.borrow();
            arm_branch(
                &sh.branch_anchor,
                &sh.branch_indicator,
                "m1".to_string(),
                branch_indicator_text("armed draft"),
            );
        }
        assert!(
            shell.borrow().branch_anchor.borrow().is_some(),
            "armed before the install"
        );

        let next = build_next_session(
            &world,
            SessionSpec::Create {
                entry: SessionEntry::Switch,
            },
            &previous_id,
            false,
        )
        .await
        .expect("build a fresh session");
        install_next_session(&mut world, &shell, next);

        assert!(
            shell.borrow().branch_anchor.borrow().is_none(),
            "install clears the armed anchor"
        );
        assert!(
            shell.borrow().branch_indicator.borrow().is_none(),
            "and its footer indicator"
        );
    }

    /// The post-rebuild branch handoff restores the prompt into the editor on a
    /// non-clean rebuild (stale head, or a build fallback), folds the failure
    /// notice, and spawns no turn. The prompt is recorded to prompt-history at
    /// the drive-loop submit site (before the branch breaks out), so the
    /// handoff itself only decides submit-vs-restore.
    #[tokio::test]
    async fn branch_handoff_restores_the_prompt_on_a_non_clean_rebuild() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        // Stale head: the build succeeded (fell_back == false) but the override
        // did not install (Some(false)). Restore, do not submit.
        let submitted = hand_off_branch_prompt(
            &mut world,
            &shell,
            "edited branch prompt".to_string(),
            false,
            Some(false),
        );
        assert!(!submitted, "a stale-head rebuild must not submit");
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "edited branch prompt",
            "the prompt is restored verbatim into the editor"
        );
        assert!(
            world.turns.is_empty(),
            "no turn was spawned on the restore path"
        );
        let restored_notice = world
            .chat
            .borrow()
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .any(|e| matches!(&e.kind, EntryKind::Notice(n) if n.text.contains("Branch failed")));
        assert!(restored_notice, "the failure/restore notice is folded");

        // A build fallback (fell_back == true) also restores and never submits.
        let submitted =
            hand_off_branch_prompt(&mut world, &shell, "another prompt".to_string(), true, None);
        assert!(!submitted, "a build fallback must not submit");
        assert!(world.turns.is_empty(), "still no turn spawned");
    }

    /// The clean-apply handoff auto-submits the branch prompt as the branch's
    /// first turn (the positive counterpart to the restore path).
    #[tokio::test]
    async fn branch_handoff_submits_on_a_clean_apply() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        let submitted = hand_off_branch_prompt(
            &mut world,
            &shell,
            "branch turn".to_string(),
            false,
            Some(true),
        );
        assert!(submitted, "a clean apply submits the prompt");
        assert!(!world.turns.is_empty(), "a turn was spawned");

        // Settle the spawned turn so world teardown is clean.
        let joined = join_next_or_pending(&mut world.turns).await;
        handle_turn_join(&mut world, joined).expect("turn settles");
    }
}
