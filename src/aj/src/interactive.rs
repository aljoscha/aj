//! The interactive alt-screen shell, driven by `vxfw::AsyncApp`.
//!
//! The base layout from the alt-screen UX spec: a one-line header, a
//! flex-filling transcript, an editor, and a one-line footer, stacked
//! in a `FlexColumn`. A session host backs the shell: this frontend is the
//! host's first client, attached in process (spec section 5). Prompts and
//! every other mutation go out as host commands, the frames the host
//! publishes fold into the [`ChatState`] model through
//! [`SessionClient`](aj_app::client::SessionClient), and the
//! [`TranscriptView`] renders it with follow-tail.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use aj_agent::TaskRegistry;
use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_agent::tool::TaskId;
use aj_agent::types::UsageSummary;
use aj_app::actions::AjAction;
use aj_app::chat::ChatState;
use aj_app::cli::args::{Args, Command as CliCommand, TAG_WITHOUT_A_CREATE};
use aj_app::client::SessionClient;
use aj_app::commands::CommandAction;
use aj_app::directory::SessionDirectory;
use aj_app::host::{
    AttachRequest, Command, CommandOutcome, HeadTarget, LocalHandles, QueueOp, SettingsAxis,
    SettingsChange,
};
use aj_app::keybindings::fixed_keys;
use aj_app::session::{SessionExit, SessionRequest};
use aj_app::session_setup::{ComposedHost, compose_host};
use aj_app::settings::{ConfigLayers, ConfigTarget, PersistAction};
use aj_app::shutdown::{format_resume_hint, format_session_usage_header, format_usage_summary};
use aj_app::theme::{
    ColorMode, Theme, ThemeBg, ThemeColor, ThemeHandle, ThemeWatcherGuard, watch_user_theme,
};
use aj_app::turn::running_work_counts;
use aj_conf::skills::Skill;
use aj_conf::{
    AgentEnv, Config, ConfigDiagnostic, ConfigThinkingDisplay, ConfigVerbosity, Severity,
};
use aj_models::auth::{AuthError, AuthStorage};
use aj_models::registry::ModelInfo;
use aj_models::types::UserContent;
use aj_models::usage::default_reset_sources;
use aj_models::{ThinkingConfig, speed_from_name, thinking_config_from_name};
use aj_session::{ConversationPersistence, PromptEntry, SessionPreview, ThreadFilter};
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use chrono::Utc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use vaxis::cell::{Color, Style};
use vaxis::key::{Key, Modifiers};
use vaxis::tty::PosixTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    AsyncApp, AutocompleteDelivery, DrawContext, EditorTheme, Event, EventContext,
    FilterableSelect, FlexColumn, FlexItem, FlexRow, FrameStats, KeymapController, ListView,
    MaxSize, Options, PopupStyle, RelativePoint, Size, SubSurface, Surface, Text, TextArea,
    UserEvent, Widget, WidgetRef, draw_widget, to_widget_ref,
};

use crate::agent_picker::{AgentPickerOutcome, PickerSnapshot, open_agent_picker};
use crate::connect::{ConnectTarget, Connected};
use crate::content_overlay::{ContentStyles, Row, auth_rows, session_info_rows, set_rows};
use crate::control::{Control, ControlError, ControlFrame, Stream};
use crate::footer::FooterLine;
use crate::frame_stats_box::FrameStatsBox;
use crate::image_store::ImageStore;
use crate::keymap::{HostCtx, build_keymap};
use crate::login::{
    AuthPickerRequest, AuthRow, DialogCallbacks, LoginDialogState, open_login_dialog,
    open_login_picker, open_logout_picker,
};
use crate::overlay::{MouseBlocker, OverlayChrome, OverlayStack, Scrim, close_key_label};
use crate::palette::{FetchKind, PendingFetch, open_palette};
use crate::pending::PendingBox;
use crate::prompt_history::{HistoryFetch, HistoryScope, MAX_ENTRIES, open_prompt_history};
use crate::quit_hint::QuitHint;
use crate::selection_copied::SelectionCopied;
use crate::session_selector::{SessionScan, extend_session_scan, open_session_selector};
use crate::session_tag::{TagEdit, open_session_tag};
use crate::session_tree::{build_tree_rows, open_session_tree};
use crate::settings_ui::{
    MODEL_SETTING_ID, SelectorActivity, SettingsCatalogs, SettingsUi, SettingsValues, SkillRow,
    SkillsFill, UNSET_VALUE, build_skill_rows, open_model, open_settings, open_skills,
    open_thinking, skills_placeholder_row,
};
use crate::sidebar::{
    MIN_COLS_WITH_SIDEBAR, SIDEBAR_COLS, SessionSidebar, SidebarState, StripGesture, step_session,
};
#[cfg(test)]
use crate::sidebar::{RowStatus, SidebarRow};
use crate::splash::{SPLASH_WAKE_EVENT, Splash};
use crate::status::{Connection, STATUS_WAKE_EVENT, StatusLine, StatusState};
use crate::task_output::{TaskBacking, TaskOutputView, open_task_output};
use crate::terminal::TerminalCaps;
use crate::toasts::{ToastBody, ToastStack, Toasts, busy_refusal};
use crate::transcript::{TranscriptStyles, TranscriptView, vaxis_color};
use crate::usage_overlay::open_usage_overlay;

/// App-event name the drive loop posts after opening an overlay outside
/// dispatch. The Shell handles it by moving focus onto the top overlay: the
/// drive loop owns the session world but has no [`EventContext`] to move focus
/// itself, so it delegates the focus move to the shell via this event.
const REFOCUS_OVERLAY_EVENT: &str = "aj.refocus-overlay";

/// App-event name the host posts after a session switch so the Shell
/// retitles the terminal from its capturing phase. The switch runs in the
/// drive loop, which has no [`EventContext`] to queue the title command
/// itself, so it delegates to the shell the same way [`REFOCUS_OVERLAY_EVENT`]
/// delegates the focus move.
const SET_TITLE_EVENT: &str = "aj.set-title";

/// The app name shown in the terminal window title, lowercase.
const APP_TITLE: &str = "aj";

/// Everything the select loop mutates besides the `AsyncApp`: the host this
/// frontend is a client of, this client's view of the focused session, and
/// the shared chat model.
///
/// Kept separate from the [`Shell`] widget so the loop's arm helpers
/// are drivable headlessly in tests, without a terminal.
struct World {
    /// The host this frontend drives, in process or over the control port,
    /// and the only path to mutating a session (spec section 5). Which of the
    /// two it is decides nothing above [`Control`] except the handful of
    /// gestures spec 9.1 leaves out of connect mode.
    control: Control,
    /// Every session this peer offers: the `list` rows, and a fold plus a
    /// transcript for each session this client attached. Which one is focused
    /// lives here too, so it cannot drift from the fold that serves it.
    directory: SessionDirectory,
    /// The one stream serving every attached session. Changing the attach set
    /// means reopening it (spec 6.5), which is what a first focus does.
    stream: Stream,
    /// Paces the retry of the reads an attach block obliges, so a peer that
    /// fails them cannot make the loop spend every iteration in a request
    /// (see [`refresh_client_reads`]).
    reads_retry: Retry,
    /// Direct handles into the focused session, for the reads no frame
    /// carries: the log the tree and export walks read, the run config the
    /// startup credential check names, the task registry behind the footer's
    /// queued-notice count. A read surface only, every mutation goes through
    /// `control` (see [`LocalHandles`]).
    ///
    /// `None` in connect mode, where the session lives in another process.
    /// Every reader either has a wire equivalent to fall back on or is a
    /// gesture connect mode refuses outright.
    local: Option<LocalHandles>,
    /// The directory the focused session runs in: this process's own for a
    /// local run, the host's (from `hello`) for a connection.
    working_directory: PathBuf,
    /// Where this client stands with its host. Mirrored into the status chrome
    /// by [`sync_status`].
    connection: Connection,
    /// The chat model, shared with the [`TranscriptView`]. Only the
    /// loop mutates it (via the client fold and the arm helpers). The view
    /// reads it at draw time. Never borrowed across an await.
    chat: Rc<RefCell<ChatState>>,
    /// Mirror of the lifecycle bits the status chrome (loader,
    /// footer) reads at draw time, shared with those widgets and
    /// refreshed by [`sync_status`] once per loop iteration. The
    /// lifecycle itself lives on `client`, whose fold is its writer.
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
    /// Credential store, shared with the async read-only overlays (auth
    /// status, usage) whose fetches run detached off the drive loop.
    auth: AuthStorage,
    /// The project's sessions store, shared with the prompt-history scan
    /// (run detached on a blocking thread off the drive loop).
    persistence: ConversationPersistence,
}

/// Which session the process opens with.
enum StartupSession {
    /// Mint a fresh one.
    Create,
    /// Resume the identified one from disk.
    Resume(String),
}

/// Build the session world: the host, the session it opens with, and the
/// chat model seeded from that session's attach block.
///
/// The host owns session assembly now, so what happens here is the client
/// half: attach, fold the block the host serves (which is what replaces the
/// startup replay), discharge the reads the block obliges, then fold the
/// startup notices so the first frame shows them.
async fn build_world(
    args: &Args,
    layers: ConfigLayers,
    diagnostics: &[ConfigDiagnostic],
    auth: &AuthStorage,
    persistence: &ConversationPersistence,
    idle_grace: Option<Duration>,
) -> Result<World> {
    let config = layers.effective();

    // Install the user's `[keybindings]` overrides before the keymap is built
    // or any hint renders. Rejected entries are surfaced as a startup warning
    // in the notice block below.
    let keybinding_problems = aj_app::actions::install_keybindings(config.keybindings.clone());

    // `aj continue` with neither an explicit id nor a latest session
    // on disk degrades to a fresh session, matching `aj`.
    let startup = match &args.command {
        Some(CliCommand::Continue {
            session_id: Some(id),
            prompt: _,
        }) => StartupSession::Resume(id.clone()),
        Some(CliCommand::Continue {
            session_id: None,
            prompt: _,
        }) => match persistence.get_latest_session_id()? {
            Some(latest) => StartupSession::Resume(latest),
            None => {
                eprintln!("No latest conversation to resume; starting a fresh session.");
                StartupSession::Create
            }
        },
        _ => StartupSession::Create,
    };

    let ComposedHost {
        host,
        config,
        layers: config_layers,
        catalog,
    } = compose_host(args, layers, auth, persistence, idle_grace)?;

    // A resume attaches, which materializes the session on the way in. A
    // create mints one and holds it live.
    let fresh = matches!(startup, StartupSession::Create);
    // Refused before the host is asked to mint anything, so an illegal label
    // costs no session and reports on the normal screen.
    let tag = args.launch_tag().map_err(|err| anyhow!("--tag: {err}"))?;
    let session = match startup {
        StartupSession::Create => host.create_with(None, None, tag).await?,
        StartupSession::Resume(id) => id,
    };
    let control = Control::local(host);
    let mut directory = SessionDirectory::new(session.clone());
    // The stream before the handles: its attachment is what stops the host from
    // releasing the session in between (spec section 5), which would leave the
    // world holding handles into a core nothing drives.
    let stream = open_stream(&control, &mut directory).await?;
    let handles = control
        .host()
        .expect("a local run holds the host")
        .local_handles(&session)
        .await?;
    // Read off the handles before they move into the world: the notices below
    // are folded after the attach block, and this is the local-only half of
    // startup either way.
    let env = handles.env.clone();
    let restore_notices = handles.restore_notices.clone();
    let (settings, context_window) = {
        let cfg = handles
            .run_config
            .lock()
            .expect("run config mutex poisoned");
        (cfg.settings(), cfg.model_info.context_window)
    };
    let provider_id = settings.provider.clone();
    let chat = seeded_chat(&config, settings, context_window, &catalog);
    let mut world = World {
        control,
        directory,
        stream,
        reads_retry: Retry::default(),
        local: Some(handles),
        working_directory: env.working_directory.clone(),
        connection: Connection::Connected,
        chat: Rc::new(RefCell::new(chat)),
        status: Rc::new(RefCell::new(StatusState::default())),
        config,
        config_layers,
        catalog,
        auth: auth.clone(),
        persistence: persistence.clone(),
    };
    // Awaited rather than drained: the block is producer-paced, and the
    // resumed history has to be in the model before the first frame is drawn.
    fold_attach_block(&mut world).await;
    refresh_client_reads(&mut world).await;

    // Startup notices, after the attach block so resumed history stays on
    // top. Order mirrors aj: config diagnostics, then (fresh session only)
    // the context listing followed by the skill warnings, then sandbox,
    // auth, tmux, then the resume-restore notices.
    fold_startup_diagnostics(&mut world, diagnostics, &keybinding_problems);
    // The context listing and skill warnings describe the freshly-loaded env,
    // which governs only a fresh session, so `fresh_env_notices` returns them
    // for a create and nothing for a resume. Folding them here, ahead of the
    // sandbox/auth/tmux warnings, keeps the context leading the fresh-session
    // block. The same helper feeds the in-process new-session path, so a
    // `/new` surfaces identical env context and skill problems.
    for event in fresh_env_notices(fresh, &env) {
        fold_event(&mut world, event);
    }
    if aj_app::notices::sandbox_warning_enabled() {
        fold_warning(&mut world, aj_app::notices::SANDBOX_WARNING);
    }
    // Apply a `--api-key` runtime override to the resolved provider, then
    // nudge toward logging in when no credential is configured. Both are
    // skipped for the scripted fake provider, which needs no credentials.
    if args.scripted.is_none() {
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
            fold_warning(&mut world, &text);
        }
    }
    if let Some(warning) = aj_app::tmux::options().and_then(aj_app::tmux::build_warning) {
        fold_warning(&mut world, &warning);
    }
    if !fresh && args.tag.is_some() {
        fold_warning(&mut world, TAG_WITHOUT_A_CREATE);
    }
    for notice in restore_notices {
        fold_notice(&mut world, &notice);
    }

    Ok(world)
}

/// Fold the diagnostics every mode reports at startup: the config problems
/// found while loading the layers, then the keybinding overrides that had no
/// effect.
///
/// Folded after the attach block so a resumed session's history stays above
/// them.
fn fold_startup_diagnostics(
    world: &mut World,
    diagnostics: &[ConfigDiagnostic],
    keybinding_problems: &[aj_app::actions::KeybindingProblem],
) {
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
        fold_event(world, event);
    }
    if !keybinding_problems.is_empty() {
        let mut msg = String::from("Some keybindings in config.toml had no effect:");
        for problem in keybinding_problems {
            msg.push_str(&format!("\n  - {problem}"));
        }
        fold_warning(world, &msg);
    }
}

/// Build the session world against a remote host: the connect-mode half of
/// [`build_world`].
///
/// The handshake and the session selection already happened (see
/// [`crate::connect`]), so what is left is the client half: attach, fold the
/// block, discharge the reads it obliges, then the startup notices. The
/// restored-settings summary is rendered here from the first attach's own
/// `state` frame rather than published by the host, which is what keeps it
/// from repeating on every reconnect (spec 9.1).
async fn build_connect_world(
    args: &Args,
    connected: Connected,
    layers: ConfigLayers,
    diagnostics: &[ConfigDiagnostic],
    auth: &AuthStorage,
    persistence: &ConversationPersistence,
) -> Result<World> {
    let config = layers.effective();
    let keybinding_problems = aj_app::actions::install_keybindings(config.keybindings.clone());
    let catalog = aj_app::commands::load_model_catalog();
    let config = Arc::new(StdMutex::new(config));
    let config_layers = Arc::new(StdMutex::new(layers));

    let Connected {
        control,
        session,
        working_directory,
        created,
    } = connected;
    let mut directory = SessionDirectory::new(session.clone());
    // No run config to seed from: the block's opening `state` frame carries
    // the host's own settings and lands before the first paint.
    let chat = seeded_chat(&config, unknown_settings(), 0, &catalog);
    let stream = open_stream(&control, &mut directory).await?;
    let mut world = World {
        control,
        directory,
        stream,
        reads_retry: Retry::default(),
        local: None,
        // The host's directory, not ours: the session runs there, and its
        // `@file` completions and tool output all name paths on that machine.
        working_directory: working_directory.unwrap_or_default(),
        connection: Connection::Connected,
        chat: Rc::new(RefCell::new(chat)),
        status: Rc::new(RefCell::new(StatusState::default())),
        config,
        config_layers,
        catalog,
        auth: auth.clone(),
        persistence: persistence.clone(),
    };
    fold_attach_block(&mut world).await;
    refresh_client_reads(&mut world).await;

    fold_startup_diagnostics(&mut world, diagnostics, &keybinding_problems);
    if args.listen.is_some() {
        fold_warning(
            &mut world,
            "--listen has nothing to serve in connect mode: the sessions live on the host.",
        );
    }
    if !created && args.tag.is_some() {
        fold_warning(&mut world, TAG_WITHOUT_A_CREATE);
    }
    let dialed = format!("Connected to {}.", connect_url(&world));
    fold_notice(&mut world, &dialed);
    if let Some(settings) = world.client_mut().take_first_attach_settings() {
        let summary = format!(
            "Session settings: model {}/{}, thinking {}, thinking display {}, speed {}, \
             verbosity {}.",
            settings.provider,
            settings.model_id,
            settings.thinking,
            settings.thinking_display,
            settings.speed,
            settings.verbosity,
        );
        let notice = if created {
            format!("Created session {}. {summary}", world.session())
        } else {
            format!("Attached session {}. {summary}", world.session())
        };
        fold_notice(&mut world, &notice);
    }

    Ok(world)
}

/// The url this world's host was dialed at, for the header and the connect
/// notice. Empty for a local run, which has no url.
fn connect_url(world: &World) -> String {
    world.control.base_url().unwrap_or_default().to_string()
}

/// A fresh chat model seeded with the settings identity and context window
/// its next main turn runs against, plus the config-driven display flags.
///
/// The seed is needed before any frame has been folded, so it comes off the
/// live run config for a local run. A connection has no such handle, and its
/// attach block opens with a `state` frame carrying the same identity, which
/// is folded before the first paint.
fn seeded_chat(
    config: &Arc<StdMutex<Config>>,
    settings: AgentSettings,
    context_window: u64,
    catalog: &Arc<Vec<ModelInfo>>,
) -> ChatState {
    let mut chat = ChatState::new(settings, context_window, Arc::clone(catalog));
    let config = config.lock().expect("config mutex poisoned");
    chat.show_thinking_block = config.show_thinking_block;
    chat.show_token_usage = config.show_token_usage;
    chat.compact_transcript = config.compact_transcript;
    chat.show_image_in_terminal = config.show_image_in_terminal;
    chat.syntax_highlight = config.syntax_highlighting;
    chat
}

/// The settings a local session's next main turn runs against.
fn local_settings_seed(handles: &LocalHandles) -> (AgentSettings, u64) {
    let cfg = handles
        .run_config
        .lock()
        .expect("run config mutex poisoned");
    (cfg.settings(), cfg.model_info.context_window)
}

/// The settings placeholder a connect-mode chat model starts on, replaced by
/// the attach block's opening `state` frame before the first paint.
///
/// Deliberately empty rather than a plausible guess: a frame that never
/// arrived must not be mistakable for the host's own settings.
fn unknown_settings() -> AgentSettings {
    AgentSettings {
        provider: String::new(),
        model_id: String::new(),
        thinking: String::new(),
        thinking_display: String::new(),
        speed: String::new(),
        verbosity: String::new(),
    }
}

impl World {
    /// The session this frontend renders.
    fn session(&self) -> &str {
        self.directory.focused()
    }

    /// The focused session's fold, and the owner of its agent lifecycle,
    /// cursor, and settings view.
    fn client(&self) -> &SessionClient {
        self.directory.client()
    }

    /// The focused session's fold, mutably.
    fn client_mut(&mut self) -> &mut SessionClient {
        self.directory.client_mut()
    }
}

#[cfg(test)]
impl World {
    /// The focused session's direct handles, for the local-mode tests that
    /// assert on host-side state (the live queues, the task registry, the log)
    /// or stage what no command can reach.
    fn handles(&self) -> &LocalHandles {
        self.local
            .as_ref()
            .expect("a local run holds the session's handles")
    }

    /// The in-process host, for the tests that drive it directly (shutting it
    /// down, reading its session list).
    fn host(&self) -> &aj_app::host::SessionHost {
        self.control
            .host()
            .expect("a local run holds the session host")
    }
}

/// Open the one stream serving every session the directory folds, offering
/// each session's own cursor, and arm every fold the peer reports it served.
///
/// One stream per client, not one per session: the ordering guarantees are per
/// stream, and changing the attach set means reopening it (spec 6.5). So this
/// is both the first attach and the re-attach, and a first focus of a session
/// is a reopen over the grown set.
///
/// Arming follows what the peer reports it attached, never what was asked for,
/// and covers the whole set in one call (see
/// [`SessionDirectory::expect_attach`]).
///
/// The caller folds the blocks with [`fold_attach_block`], which awaits the
/// focused session's: blocks are producer-paced (spec 6.9), so they are not
/// necessarily queued when this returns. The cursors offered are the clients'
/// own, so a re-attach after a head switch is served under the new epoch and
/// one within an epoch is served the suffix.
async fn open_stream(
    control: &Control,
    directory: &mut SessionDirectory,
) -> Result<Stream, ControlError> {
    let stream = control.attach_all(&directory.attach_requests(None)).await?;
    directory.expect_attach(|session| stream.attached(session));
    Ok(stream)
}

/// Attach the working set a focus on `session` will leave, falling back to
/// `session` alone if the whole set is refused.
///
/// An attach is all-or-nothing (spec 6.5), and the set is requested on every
/// reopen, so one background session the host will not serve would otherwise
/// refuse every reopen for every session, including the one the user is looking
/// at. A session that cannot be attached has no business holding the client
/// hostage, so it falls out of the working set the same way a displaced one
/// does. The narrowed attach is the retry, not a second refusal to report: if
/// that fails too, the reason is about the session being focused and the caller
/// words it.
///
/// The sessions dropped this way are not named here. Their entries stay in the
/// directory folding nothing, and the next reopen offers them again, so a
/// transient refusal costs one narrowed attach rather than a permanent
/// eviction.
async fn attach_admitting(world: &World, session: &str) -> Result<Attachment, ControlError> {
    let requests = world.directory.attach_requests(Some(session));
    match world.control.attach_all(&requests).await {
        Ok(stream) => Ok(Attachment {
            stream,
            narrowed: false,
        }),
        Err(_) if requests.len() > 1 => {
            tracing::warn!(
                "attaching {} session(s) was refused, narrowing to {session}",
                requests.len(),
            );
            // Keep the cursor the wide attach would have offered, so the
            // narrowed one still resumes incrementally rather than replaying
            // the session from its start.
            let narrowed = requests
                .into_iter()
                .find(|request| request.session == session)
                .unwrap_or_else(|| AttachRequest {
                    session: session.to_string(),
                    cursor: None,
                });
            let stream = world.control.attach_all(&[narrowed]).await?;
            Ok(Attachment {
                stream,
                narrowed: true,
            })
        }
        Err(err) => Err(err),
    }
}

/// A freshly opened stream, and whether the peer refused to serve the whole
/// working set so the attach had to narrow to the one session being focused.
///
/// The flag is load-bearing: a narrowed attach leaves every other member
/// detached on the peer, and the client has to be told so it re-attaches them
/// later rather than swapping onto a view nothing feeds.
struct Attachment {
    stream: Stream,
    narrowed: bool,
}

/// Read the focused session's tree from the host and open the tree overlay
/// over it, answering the built select.
///
/// The read carries the current head alongside the segments (spec 6.7), and
/// the overlay needs both: the head selects the row the session is on, and
/// makes confirming that row a no-op rather than a switch onto itself. It is
/// the one read that materializes a session, which an attached one already
/// is.
async fn open_tree_overlay(
    world: &World,
    shell: &Rc<RefCell<Shell>>,
) -> Result<Rc<RefCell<FilterableSelect>>, ControlError> {
    let tree = world.control.tree(world.session()).await?;
    let rows = build_tree_rows(&tree, Utc::now());
    let handles = shell.borrow().overlay_handles();
    Ok(open_session_tree(&handles, rows, tree.head))
}

/// Fold every frame already queued on the focused session's stream.
///
/// Called once per drive-loop iteration, so the chrome mirrors never lag the
/// host by a frame (a command's own frames are queued by the time it
/// returns) and a streaming burst collapses into one redraw rather than one
/// per chunk. Returns whether anything renderable changed.
///
/// A stream that failed mid-drain reports nothing here: the failure is held
/// for the loop's frame arm, which is the one place a lost stream is handled.
fn fold_ready_frames(world: &mut World) -> bool {
    let mut redraw = false;
    while let Some(frame) = world.stream.try_recv() {
        redraw |= world.directory.apply(&mut world.chat.borrow_mut(), frame).0;
    }
    redraw
}

/// Fold the attach block the host is writing, up to and including its
/// `caught_up`, and report whether the block completed.
///
/// The block is producer-paced (spec 6.9): it is generated at the pace the
/// client reads it rather than queued before the attach returns, so draining
/// only what is ready would paint the first frame against an empty
/// transcript. Awaiting the block's end is also what makes the reads it
/// obliges observe the state it established.
///
/// A stream that closes mid-block leaves what was applied in place and
/// answers `false`: the caller owes another attach. It stays silent about the
/// reason, which the caller reports.
async fn fold_attach_block(world: &mut World) -> bool {
    loop {
        let ControlFrame::Frame(frame) = world.stream.recv().await else {
            return false;
        };
        let ends_block = matches!(
            &frame,
            aj_wire::Frame::CaughtUp { session, .. } if *session == world.session()
        );
        let _ = world.directory.apply(&mut world.chat.borrow_mut(), frame);
        if ends_block {
            return true;
        }
    }
}

/// Discharge the reads an attach block obliges: neither the task table nor
/// the pending-message queues are replayable, so a backfill regenerates
/// neither (spec 6.7).
///
/// Both reads land in the shared chat model, which is what every frontend
/// renders from, so the local and the remote path stay one path.
///
/// A failed read leaves the obligation standing and paces the retry
/// (`world.reads_retry`): the loop calls this every iteration and each call
/// awaits a request, so a peer that stopped answering would otherwise put a
/// request that times out into every iteration.
async fn refresh_client_reads(world: &mut World) {
    if !world.reads_retry.ready() {
        return;
    }
    let mut failed = false;
    if world.client().needs_task_refetch() {
        match world.control.tasks(world.session()).await {
            Ok(tasks) => {
                let mut chat = world.chat.borrow_mut();
                world.directory.client_mut().set_tasks(&mut chat, tasks);
            }
            Err(err) => {
                failed = true;
                tracing::warn!("could not read the session's task table: {err}");
            }
        }
    }
    if world.client().needs_queue_refetch() {
        match world.control.queue(world.session()).await {
            Ok(queue) => {
                let mut chat = world.chat.borrow_mut();
                world.directory.client_mut().set_queue(&mut chat, queue);
            }
            Err(err) => {
                failed = true;
                tracing::warn!("could not read the session's message queues: {err}");
            }
        }
    }
    if failed {
        world.reads_retry.failed();
    } else {
        world.reads_retry.clear();
    }
}

/// Whether the client still owes the reads an attach block obliged, which is
/// what decides if the loop has to wake for their paced retry.
fn owes_client_reads(world: &World) -> bool {
    world.client().needs_task_refetch() || world.client().needs_queue_refetch()
}

/// The notice a gesture with no connect-mode path folds, naming why (spec
/// 9.1: such a gesture must never silently do nothing).
fn remote_unsupported_notice(what: &str, why: &str) -> String {
    format!("Can't {what} over a connection: {why}.")
}

/// A session change the drive loop broke out for.
enum FocusRequest {
    /// Mint a session and focus it.
    Create,
    /// Focus a session that already exists.
    Resume(String),
    /// Move the focused session's head, then re-attach onto the branch that
    /// leaves, auto-submitting `prompt` as its first turn when one was
    /// handed off.
    Branch {
        target: HeadTarget,
        prompt: Option<String>,
    },
}

/// Whether a focus change moved to another session, which is what decides
/// if the outgoing session's usage belongs in the exit banner.
enum Focus {
    Moved,
    Same,
}

/// Apply a session change: point the frontend at another session, or move
/// the focused one's head and re-attach.
///
/// A refused change (an unknown session, a lock held by another writer, a
/// head switch the host refuses while work is live) folds its failure
/// notice and stays where it is. Nothing is torn down either way: the
/// outgoing session stays live in the host.
///
/// Every arm works in both modes. Creating and resuming go through the control
/// surface, and the session they land on is attached the same way whether the
/// host is in this process or across a connection.
async fn apply_focus_request(
    app: &mut AsyncApp,
    shell: &Rc<RefCell<Shell>>,
    world: &mut World,
    request: FocusRequest,
) -> Focus {
    match request {
        FocusRequest::Create => {
            let created = match world.control.create(None, None, None).await {
                Ok(session) => {
                    let notice = format!("Started a fresh session ({session}).");
                    focus_session(
                        app,
                        shell,
                        world,
                        session,
                        true,
                        vec![notice_event(&notice)],
                    )
                    .await
                }
                Err(err) => Err(err),
            };
            match created {
                Ok(()) => Focus::Moved,
                Err(err) => {
                    fold_notice(world, &format!("Failed to start a fresh session: {err}"));
                    Focus::Same
                }
            }
        }
        FocusRequest::Resume(session) if session == world.session() => {
            // Nothing to do, and doing it anyway would show a switch notice for
            // a switch that did not happen, discard an armed branch anchor and
            // reset the scroll. Reachable from a stepping chord answered off a
            // mirror that has not caught up with the last switch yet.
            Focus::Same
        }
        FocusRequest::Resume(session) => {
            let notice = format!("Switched to session {session}.");
            match focus_session(
                app,
                shell,
                world,
                session.clone(),
                false,
                vec![notice_event(&notice)],
            )
            .await
            {
                Ok(()) => Focus::Moved,
                Err(err) => {
                    fold_notice(
                        world,
                        &format!("Failed to switch to session {session}: {err}"),
                    );
                    Focus::Same
                }
            }
        }
        FocusRequest::Branch { target, prompt } => {
            branch_focused_session(app, shell, world, target, prompt).await;
            // A branch stays inside its session, so the banner keeps
            // counting it as the live one.
            Focus::Same
        }
    }
}

/// Point the frontend at `session`, folding `lead` (the switch
/// confirmation) plus that session's own startup notices on top of the
/// history its attach block carries.
///
/// Rebind by replace-contents: the `chat` and `status` cells keep their
/// identity across the swap (every chrome widget and the keymap's dispatch
/// closure hold clones of these Rcs, captured once at [`Shell::new`]), so
/// overwriting their contents repoints the whole UI at the new session
/// without rebuilding a widget or re-initializing the app. Only the handles
/// a content swap cannot reach are repointed in [`Shell::rebind`].
///
/// The world is mutated only once the new session's stream is open, so a
/// refused attach (an unknown session, a lock another writer holds) leaves
/// the current one running and hands the reason back for the caller to word.
async fn focus_session(
    app: &mut AsyncApp,
    shell: &Rc<RefCell<Shell>>,
    world: &mut World,
    session: String,
    fresh: bool,
    lead: Vec<AgentEvent>,
) -> Result<(), ControlError> {
    let host = world.control.host().cloned();
    // A session already attached is a view swap: its frames have been folding
    // in the background all along, so there is no stream to reopen and no
    // block to wait for (spec 9.2).
    let attaching = !world.directory.is_attached(&session);
    // Unless it owes a re-attach. A `reset` for a background session stops its
    // fold until one is served, so swapping onto it would paint the branch the
    // reset abandoned. The loop discharges these set-wide, but focusing can
    // arrive first, and the reopen is what makes the switch show the session as
    // it now is.
    let reopening = attaching
        || world
            .directory
            .client_for(&session)
            .is_some_and(|client| client.needs_reattach());
    // The stream first: its attachment is what stops the host from releasing
    // the session between here and the handles below (spec section 5,
    // attachment is the retention signal). Taking the handles first would leave
    // a window where an idle session past its grace is torn down in between,
    // and the world would hold handles into a core nothing drives while the
    // attach materialized a second one.
    //
    // Changing the attach set means reopening the stream (spec 6.5), so a first
    // focus re-attaches every session in the working set alongside the new one.
    // Each offers its own cursor, so the sessions carried over are served their
    // suffix rather than a fresh backfill, and the session this one displaces
    // from the set is detached by going unnamed here.
    let attachment = if reopening {
        Some(attach_admitting(world, &session).await?)
    } else {
        None
    };
    // Direct handles are a read surface into a session in this process, so a
    // connection has none and everything they feed has a wire form: the attach
    // block's opening `state` frame carries the settings the footer seeds from,
    // and the host's working directory arrived with `hello` and is the same for
    // every session on it.
    let handles = match &host {
        Some(host) => Some(host.local_handles(&session).await?),
        None => None,
    };

    let (settings, context_window) = match &handles {
        Some(handles) => local_settings_seed(handles),
        None => (unknown_settings(), 0),
    };
    let minted =
        attaching.then(|| seeded_chat(&world.config, settings, context_window, &world.catalog));
    if let Some(handles) = &handles {
        world.working_directory = handles.env.working_directory.clone();
    }
    world.local = handles;
    // Parks the outgoing session's transcript and brings the incoming one into
    // the cell the widgets read, which is what makes a switch back instant.
    world
        .directory
        .focus(&mut world.chat.borrow_mut(), &session, || {
            minted.expect("a session focused for the first time was minted a transcript")
        });
    if let Some(Attachment { stream, narrowed }) = attachment {
        // Armed after the focus, so the session just inserted is in the set.
        world
            .directory
            .expect_attach(|session| stream.attached(session));
        world.stream = stream;
        if narrowed {
            // The peer serves one session now, so every other member of the
            // working set is detached in fact. Dropping them says so, and a
            // later focus re-attaches instead of swapping onto a view no stream
            // feeds. Their rows stay, which is how the user keeps seeing them.
            // Safe here and not before the focus: the session being kept is the
            // focused one, so the parked-transcript invariant holds.
            let dropped = world.directory.drop_all_but(&session);
            if !dropped.is_empty() {
                tracing::warn!("detached by the narrowed attach: {}", dropped.join(", "));
            }
        }
    }
    // Status is resynced from the client once per iteration; reset it so
    // the frame between install and the next sync shows idle chrome.
    *world.status.borrow_mut() = StatusState::default();
    // Clear any armed branch anchor: the shell and its slots survive session
    // changes, so without this a stale anchor could resolve against the new
    // session's log (and with legacy 8-hex ids even hit a wrong entry).
    shell.borrow().disarm_branch();
    // Start the switched-to session's splash box at the top: a prior session's
    // wheel scroll must not carry over.
    shell.borrow().splash.borrow_mut().reset_scroll();
    shell.borrow_mut().rebind(world);
    // Reconcile the editor chrome onto the freshly focused session. The
    // per-iteration reconcile runs at the bottom of `drive`, but `drive`
    // re-enters here with no prior render and paints its first frame at the top
    // of the loop, one iteration before that reconcile. The editor widget
    // persists across the chat swap, so without this the first frame would show
    // the outgoing session's baked border tint and stale `agent N` marker. This
    // mirrors the `world.status` reset above, which resets chrome for the same
    // install-to-first-draw window.
    // Fold the attach block before the chrome reconcile below, so the first
    // frame is drawn against the session's real history and settings. A swap
    // onto an already-attached session has no block coming, and awaiting one
    // would hang the loop on a `caught_up` the host has no reason to send.
    if reopening {
        fold_attach_block(world).await;
    }
    refresh_client_reads(world).await;
    sync_editor_chrome(world, shell);
    // Folded after the attach block so they land on top of the replayed
    // history: the confirmation, then a fresh session's env notices, then
    // whatever resume-time restoration did.
    for event in lead {
        fold_event(world, event);
    }
    // Both describe this process reading a session off its own disk, so neither
    // has anything to say about a session running on another machine.
    let startup = world
        .local
        .as_ref()
        .map(|handles| (handles.env.clone(), handles.restore_notices.clone()));
    if let Some((env, restore_notices)) = startup {
        for event in fresh_env_notices(fresh, &env) {
            fold_event(world, event);
        }
        // Only onto a transcript this focus built. A swap restores one that
        // already shows them, and they are startup facts about the session, not
        // about the switch, so re-folding them would stack a copy per visit.
        if attaching {
            for notice in restore_notices {
                fold_notice(world, &notice);
            }
        }
    }
    // The outgoing session's transmitted image ids belong to its terminal
    // graphics memory. Free them and empty the store so the new session
    // starts clean.
    free_session_images(app, shell);
    // Retitle the terminal for the switched-to session, and hand focus back
    // to the editor: `rebind` closed the overlay stack, so the widget that
    // held focus may be gone. Both run off the loop with no event context,
    // so they ride app events (see `REFOCUS_OVERLAY_EVENT`).
    app.post_app_event(UserEvent {
        name: SET_TITLE_EVENT.to_string(),
        data: None,
    });
    app.post_app_event(UserEvent {
        name: REFOCUS_OVERLAY_EVENT.to_string(),
        data: None,
    });
    app.request_redraw();
    Ok(())
}

/// Move the focused session's head and re-attach onto the branch that
/// leaves, handing `prompt` to it under the prompt-safety invariant.
///
/// The head switch is the host's: it refuses while work is live, clears the
/// abandoned branch's queues, mints a fresh epoch and publishes `reset`.
/// Re-attaching under the client we already hold is what adopts that epoch,
/// which is what drops the abandoned branch's transcript (spec 6.5).
async fn branch_focused_session(
    app: &mut AsyncApp,
    shell: &Rc<RefCell<Shell>>,
    world: &mut World,
    target: HeadTarget,
    prompt: Option<String>,
) {
    let session = world.session().to_string();
    let branching = matches!(target, HeadTarget::Before(_));
    if let Err(err) = world
        .control
        .command(&session, Command::Head { target })
        .await
    {
        fold_notice(world, &head_refusal(branching, &err));
        // The head did not move, so the prompt would run against the branch
        // the user meant to leave. Restore it verbatim instead; it is
        // already in prompt history (recorded at the submit site), so it is
        // never lost either way.
        if let Some(prompt) = prompt {
            shell.borrow().editor.borrow_mut().set_text(&prompt);
            fold_notice(
                world,
                "Branch failed. Your message was restored to the editor.",
            );
        }
        app.request_redraw();
        return;
    }
    if let Err(err) = reattach(world, shell).await {
        // The switch took but the stream did not reopen, so the transcript
        // on screen describes a branch the session left.
        fold_warning(world, &format!("Lost the session's event stream: {err}"));
    }
    fold_notice(world, branch_switch_notice(prompt.is_some()));
    if let Some(prompt) = prompt {
        auto_submit_launch(world, vec![UserContent::text(prompt)]).await;
    }
    free_session_images(app, shell);
    app.request_redraw();
}

/// What a refused head switch tells the user.
///
/// The host quotes the entry id it was handed, which is a hash the user has
/// never seen. The caller knows whether it pointed at a message to branch
/// before (`branching`) or at a branch tip to switch to, so it says that.
/// Anything the two arms below do not recognize is reported in the peer's own
/// words rather than guessed at.
///
/// NOTE: the malformed arm reads as "the first message" because the branch
/// gesture only arms on a main-thread user message, so the root is the only
/// entry whose parent the host can refuse. A gesture that could branch before
/// a sub-agent entry would have to tell the two 400s apart.
fn head_refusal(branching: bool, err: &ControlError) -> String {
    if branching && err.unknown_entry() {
        "Can't branch: that message is no longer in this session.".to_string()
    } else if branching && err.invalid() {
        "Can't branch at the first message.".to_string()
    } else if err.unknown_entry() {
        "Can't switch: that branch is no longer in this session.".to_string()
    } else {
        format!("Failed to branch the conversation: {err}")
    }
}

/// Re-attach the focused session after its continuity broke.
///
/// The client is the one we already hold, so a fresh epoch is adopted
/// through the block's opening `state` frame, which resets the chat model
/// before the new branch's backfill lands. A `reset` frame still queued on
/// the outgoing stream goes away with it.
///
/// Answers whether the attach block completed. A stream that dropped inside
/// it leaves the client owing another attach, which the caller retries.
async fn reattach(world: &mut World, shell: &Rc<RefCell<Shell>>) -> Result<bool, ControlError> {
    let mut stream = open_stream(&world.control, &mut world.directory).await?;
    std::mem::swap(&mut world.stream, &mut stream);
    drop(stream);
    refresh_local_handles(world, shell).await?;
    let complete = fold_attach_block(world).await;
    refresh_client_reads(world).await;
    // Adopting an epoch resets the chat model, and both the transcript's
    // render cache and the image store are keyed by entry id, which is a
    // per-transcript counter that starts over with it. Dropping the view back
    // to the tail is what clears that cache, so a row of the branch we left
    // cannot be reused for a different entry of the one we joined.
    shell.borrow().transcript.borrow_mut().reset_to_tail();
    Ok(complete)
}

/// Re-read the focused session's direct handles, for a local run.
///
/// A stream that had to be reopened can land on a fresh materialization: the
/// host releases a session once it is idle and unattached (spec section 5), and
/// a client whose stream died is not attached. The handles the world holds then
/// name a core nothing drives, so the footer's task table and every overlay
/// that reads the log would be frozen at the state the old materialization
/// ended on. Ordered after the attach, whose registration is what keeps the
/// session from going again underneath this.
///
/// A no-op in connect mode, which holds no handles.
async fn refresh_local_handles(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
) -> Result<(), ControlError> {
    let Some(host) = world.control.host().cloned() else {
        return Ok(());
    };
    world.local = Some(host.local_handles(world.session()).await?);
    shell.borrow().rebind_handles(world);
    Ok(())
}

/// The confirmation for a branch, chosen from whether a prompt is handed
/// off: the `b`-submit flow (the prompt auto-submits as the branch's first
/// turn) vs a tree-view switch that only moved the head.
fn branch_switch_notice(prompt_present: bool) -> &'static str {
    if prompt_present {
        "Branched the conversation from an earlier message."
    } else {
        "Switched to the selected branch."
    }
}

/// Transmit the images the just-drawn frame recorded as pending, so the next
/// frame places them. Lazy, draw-driven transmission: only entries drawn this
/// frame are pending, which bounds terminal graphics memory to images actually
/// viewed and handles live, replayed, and scrolled-into-view images uniformly.
///
/// For each pending key it finds the tool entry, base64-decodes the first
/// image in its content, transmits the raw image bytes, and records the
/// returned id. The gate records a pending key only after the kitty-graphics
/// capability is confirmed, so a `load_image` error here can only be a decode
/// failure (a corrupt or unsupported payload), never a missing capability. On
/// that failure, or on undecodable base64, the entry is marked failed so it
/// falls back to text and is not re-attempted every frame. A redraw is
/// requested whenever the store changed, so the frame that places the image or
/// shows the fallback runs.
fn drain_pending_images(app: &mut AsyncApp, world: &World, shell: &Rc<RefCell<Shell>>) {
    let pending = shell.borrow().image_store.borrow_mut().take_pending();
    if pending.is_empty() {
        return;
    }
    let mut dirtied = false;
    {
        let chat = world.chat.borrow();
        for (agent, entry_id) in pending {
            let entry = chat
                .transcript(agent)
                .and_then(|t| t.entries().iter().find(|e| e.id == entry_id));
            let Some(entry) = entry else {
                // The entry vanished between recording and draining. Nothing to
                // transmit and nothing to mark: it is gone from the transcript.
                continue;
            };
            let Some(bytes) = image_entry_bytes(entry) else {
                // A recorded key is always a tool image, so undecodable bytes
                // here mean a corrupt base64 payload. Mark it failed so it falls
                // back to text and is not re-attempted every frame.
                shell
                    .borrow()
                    .image_store
                    .borrow_mut()
                    .mark_failed(agent, entry_id);
                dirtied = true;
                continue;
            };
            // `Source::Mem` takes the raw encoded image bytes; vaxis re-encodes
            // to PNG on transmit, so we hand it the decoded file bytes, not the
            // base64 string.
            match app.load_image(vaxis::image::Source::Mem(bytes)) {
                Ok(img) => {
                    shell
                        .borrow()
                        .image_store
                        .borrow_mut()
                        .insert(agent, entry_id, img.id());
                    dirtied = true;
                }
                Err(_) => {
                    // The base64 decoded but is not a valid image. Terminal:
                    // mark it failed so it falls back to text and is not
                    // re-attempted every frame.
                    shell
                        .borrow()
                        .image_store
                        .borrow_mut()
                        .mark_failed(agent, entry_id);
                    dirtied = true;
                }
            }
        }
    }
    if dirtied {
        app.request_redraw();
    }
}

/// The decoded bytes of the first `UserContent::Image` in a tool-result
/// entry, or `None` when the entry is not a tool cell, carries no image, or
/// the base64 fails to decode.
fn image_entry_bytes(entry: &aj_app::chat::Entry) -> Option<Vec<u8>> {
    let aj_app::chat::EntryKind::Tool(tool) = &entry.kind else {
        return None;
    };
    let data = tool.content.iter().find_map(|c| match c {
        UserContent::Image(img) => Some(&img.data),
        UserContent::Text(_) => None,
    })?;
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// Free every transmitted image id and empty the store, on a session switch.
/// The ids belong to the outgoing session's terminal graphics memory, so
/// releasing them here bounds it to the live session.
fn free_session_images(app: &mut AsyncApp, shell: &Rc<RefCell<Shell>>) {
    let ids = shell.borrow().image_store.borrow_mut().drain_ids();
    for id in ids {
        app.free_image(id);
    }
}

/// Strike hook for [`aj_app::notices::build_context_notice`], wrapping a
/// disabled skill's row in the SGR strikethrough markers (`ESC[9m` on,
/// `ESC[29m` off). The transcript notice renderer parses these into struck
/// spans. We spell the markers out here rather than depend on a shared style
/// helper, keeping this crate's markdown-marker use self-contained.
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

/// Notices describing the freshly-loaded env for a fresh session: the
/// `Context:` listing as an Info notice, followed by one warning per
/// skill-discovery diagnostic. Empty for a resume, whose assembled prompt is
/// fixed in its log and so is not governed by the env read now.
///
/// Both the process-start path ([`build_world`]) and the in-process
/// new-session path ([`focus_session`]) fold these, so a `/new` surfaces the
/// same context listing and skill problems a cold start does. The splash box
/// shows warning- and error-level notices only, so the Info context listing
/// stays in scrollback while a skill warning can surface in the box.
fn fresh_env_notices(fresh: bool, env: &AgentEnv) -> Vec<AgentEvent> {
    if !fresh {
        return Vec::new();
    }
    let mut events = vec![notice_event(&aj_app::notices::build_context_notice(
        env,
        strikethrough,
    ))];
    events.extend(
        env.skill_diagnostics
            .iter()
            .map(|d| warning_event(&d.to_string())),
    );
    events
}

/// Fold an event this frontend raised itself into the chat model.
///
/// It goes through the client rather than straight to the reducer so the
/// client's lifecycle sets stay the only ones (see
/// [`SessionClient::apply_local`]).
fn fold_event(world: &mut World, event: AgentEvent) {
    let _ = world
        .directory
        .client_mut()
        .apply_local(&mut world.chat.borrow_mut(), event);
}

/// Fold `text` into the chat model as a Main-agent notice row.
fn fold_notice(world: &mut World, text: &str) {
    fold_event(world, notice_event(text));
}

/// Fold `text` into the chat model as a Main-agent warning row, for
/// failures the user should notice (e.g. a login that errored out).
fn fold_warning(world: &mut World, text: &str) {
    fold_event(world, warning_event(text));
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
        let caps = shell.borrow().terminal_caps();
        let snapshot = theme.read();
        open_login_dialog(
            &handles.stack,
            &handles.chrome,
            &snapshot,
            caps,
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

/// Whether the viewed agent is busy, as this client sees it.
///
/// The host is the authority: it refuses a gesture it cannot serve, and its
/// `working` flag on every `state` frame is what a client seeds. This mirror
/// exists for the keymap's predicates and the overlay confirms, where the
/// answer is needed synchronously at draw or dispatch time and where a stale
/// one costs at most a refusal the host would have made anyway.
///
/// `working` covers the main agent only (spec 6.3), which is also the one
/// case the lifecycle can lag: the host marks it busy the moment it spawns
/// the turn, before the turn's `AgentStart` reaches anyone.
fn view_busy(world: &World, view: AgentId) -> bool {
    world.client().lifecycle().is_running(view)
        || (view == AgentId::Main && world.client().working())
}

/// The background work the focused session is carrying, as
/// `(agents, tasks)`.
///
/// The turn count is the main agent's flag rather than the host's driven-turn
/// count, which no frame carries: the two differ only for a sub-agent
/// continuation the host drives, and this feeds a hint line and the
/// refuse-while-busy gestures the host re-checks anyway.
///
/// The tasks come off the chat model, which every client keeps from the task
/// events plus the tasks read (spec 6.7), rather than off a live registry no
/// remote client has.
fn running_work(world: &World) -> (usize, usize) {
    let chat = world.chat.borrow();
    running_work_counts(
        usize::from(world.client().working()),
        chat.tasks().values().map(|task| (&task.kind, task.status)),
    )
}

/// Mirror the lifecycle bits the status chrome reads into the shared
/// [`StatusState`] cell, returning whether the animation tick should run
/// (see [`StatusState::animating`]). Called once per loop iteration right
/// before rendering, so every mutation path (the frame fold, submits)
/// shares one sync point and the mirror can't silently drift.
fn sync_status(world: &World) -> bool {
    let active = world.chat.borrow().active_view();
    let life = world.client().lifecycle();
    let next = StatusState {
        running: life.is_running(active),
        compacting: life.is_compacting(active),
        sub_agents_running: life
            .running_agents()
            .into_iter()
            .filter(|a| matches!(a, AgentId::Sub(_)))
            .count(),
        connection: world.connection,
    };
    *world.status.borrow_mut() = next;
    next.animating()
}

/// Mirror the session directory into the sidebar's draw state.
///
/// Called once per drive-loop iteration, like [`sync_status`], because the
/// widget cannot reach the directory and the loop is its only writer.
///
/// Rows come from the peer's `list` frames, so a session the client has never
/// attached is listed too and that is where its attention glyph comes from
/// (spec 6.8). Order is the peer's, which is activity-ordered, so the row a
/// user wants is near the top without this having to sort.
fn sync_sidebar(world: &World, shell: &Rc<RefCell<Shell>>) {
    // A peer that has sent no `list` frame yet leaves the strip empty rather
    // than inventing a row for the focused session: the next frame fills it,
    // and a fabricated row would carry no status.
    let rows = crate::sidebar::rows_for_display(
        world.directory.rows(),
        world.session(),
        |row| world.directory.is_unseen(row),
        |id| world.directory.is_attached(id),
    );
    let sidebar = Rc::clone(&shell.borrow().sidebar);
    let mut state = sidebar.borrow_mut();
    // Showing itself once the peer offers a choice is the default, and an
    // explicit toggle outranks it for the rest of the process (spec 9.2).
    if !state.toggled {
        state.visible = rows.len() > 1;
    }
    state.rows = rows;
}

/// Park a session change for the drive loop, which owns the world.
///
/// The one path the chrome's session gestures take. The stepping chords name a
/// session by walking the strip's displayed order and a click names one by the
/// row it landed on, but a pointer gesture is a second trigger for the action
/// the chord dispatches rather than a second way into the switch, so both land
/// here (spec 9.2).
fn park_session_request(
    slot: &Rc<RefCell<Option<SessionRequest>>>,
    ctx: &mut EventContext,
    request: SessionRequest,
) {
    *slot.borrow_mut() = Some(request);
    ctx.redraw = true;
}

/// Arm a branch anchor: record the branched-from user message's stable id.
/// A submit resolves it against the log to find the branch point (the
/// message's parent), and the transcript reads it to keep the highlight box on
/// that message while the editor holds focus. `Some` iff a branch is armed.
fn arm_branch(anchor: &Rc<RefCell<Option<String>>>, message_id: String) {
    *anchor.borrow_mut() = Some(message_id);
}

/// Prefill the editor with the branched-from `message`, preserving whatever
/// the user was typing by first pushing that draft onto the recall history
/// (up / Ctrl+P) so it is not lost. A blank draft is skipped by
/// `add_to_history`.
fn prefill_branch_editor(editor: &Rc<RefCell<TextArea>>, message: &str) {
    let mut editor = editor.borrow_mut();
    let draft = editor.text();
    editor.add_to_history(&draft);
    editor.set_text(message);
}

/// The notice folded when a gesture incoherent with an armed branch anchor
/// (steer, dequeue) is attempted: it points the user at Esc to cancel.
fn branch_armed_notice(what: &str) -> String {
    format!(
        "Can't {what} while branching \u{2014} press {} to cancel the branch first.",
        close_key_label()
    )
}

/// Handle an editor submit: hand the text to the host, which runs a prompt
/// turn on the viewed agent if it is idle and queues it as a follow-up
/// while it is busy.
///
/// A queued message shows in the pending box (which reads the live queue
/// snapshot at draw) and is delivered by the host's post-turn wake. History
/// is recorded by the callers (the drive loop and [`handle_steer`]), which
/// own the editor. Returns whether the message was accepted for delivery.
async fn handle_submit(world: &mut World, text: String) -> bool {
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return false;
    }
    let target = world.chat.borrow().active_view();
    // The user's message row arrives back as a `MessageEnd { User }` frame,
    // so nothing is inserted into the model here.
    prompt_host(
        world,
        Command::Prompt {
            agent: target,
            content: vec![UserContent::text(trimmed)],
        },
    )
    .await
}

/// Send a prompt command and fold the host's reason when it refuses.
///
/// Every refusal here is one the host alone can decide: an agent with no
/// live handle (a resumed sub-agent), an attachment that cannot be queued
/// behind a busy agent. Its wording is user-facing, and a remote client
/// shows the same string.
async fn prompt_host(world: &mut World, command: Command) -> bool {
    match world.control.command(world.session(), command).await {
        Ok(_) => true,
        Err(err) => {
            fold_notice(world, &err.to_string());
            false
        }
    }
}

/// Submit editor text and return the transcript to its live tail when accepted.
async fn handle_editor_submit(world: &mut World, shell: &Rc<RefCell<Shell>>, text: String) {
    if handle_submit(world, text).await {
        shell.borrow().transcript.borrow_mut().resume_follow_tail();
    }
}

/// Auto-submit the launch prompt (`aj <msg>` / `@file ...`) as the
/// initial session's first Main turn. Empty content spawns nothing.
///
/// Kept as a standalone step called from `run` outside the outer session
/// loop, so only the initial session submits and an in-process session
/// switch never resubmits. The branch flow calls it too, for the prompt it
/// hands to the branch it just opened. The launch turn is not recorded into
/// the editor's prompt history, matching `aj`.
async fn auto_submit_launch(world: &mut World, content: Vec<UserContent>) {
    if content.is_empty() {
        return;
    }
    prompt_host(
        world,
        Command::Prompt {
            agent: AgentId::Main,
            content,
        },
    )
    .await;
}

/// Cancel the viewed agent's running turn. Returns whether anything was
/// cancelled. Fired by the keymap's `CancelTurn` action, whose predicate
/// keeps it off the dispatch path while nothing runs.
///
/// The host owns the cascade: cancelling a sub-agent that runs inside its
/// parent's turn fires the parent's token.
async fn cancel_viewed_turn(world: &World) -> bool {
    let active = world.chat.borrow().active_view();
    if !view_busy(world, active) {
        return false;
    }
    if let Err(err) = world
        .control
        .command(world.session(), Command::Cancel { agent: active })
        .await
    {
        tracing::warn!("the host refused a cancel: {err}");
        return false;
    }
    true
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
    let (agents, tasks) = running_work(world);
    running_work_summary(agents, tasks)
}

/// Pull the viewed agent's queued message back into the editor,
/// prepending it to whatever is currently typed (blank-line joined).
/// Returns whether anything was yanked. Used by the dequeue chord and the
/// per-view cancel restore.
///
/// The withdrawal is the host's, which is what makes the same gesture work
/// for a client that cannot reach the queues: the command hands back the
/// text it removed (spec 6.6).
async fn yank_pending_into_editor(world: &World, shell: &Rc<RefCell<Shell>>) -> bool {
    let target = world.chat.borrow().active_view();
    let withdrawn = world
        .control
        .command(
            world.session(),
            Command::Queue(QueueOp::Remove { agent: target }),
        )
        .await;
    let text = match withdrawn {
        Ok(CommandOutcome::Withdrawn(Some(text))) => text,
        Ok(_) => return false,
        Err(err) => {
            tracing::warn!("the host refused a queue withdrawal: {err}");
            return false;
        }
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

/// The steer gesture (Alt+Enter): while the viewed agent is busy, queue the
/// editor text as steering (or promote the pending follow-up when the editor
/// is empty). While idle there is nothing to steer yet, so a non-empty
/// editor starts a normal turn.
///
/// Which of those three the gesture means is the host's decision (spec 6.6).
/// The editor-side effects depend only on whether there was text: a draft
/// that went somewhere is recorded in history and returns the transcript to
/// its tail, an empty one promotes silently.
async fn handle_steer(world: &mut World, shell: &Rc<RefCell<Shell>>) {
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
    let steered = world
        .control
        .command(
            world.session(),
            Command::Steer {
                agent: target,
                text: text.clone(),
            },
        )
        .await;
    match steered {
        Ok(_) if !text.is_empty() => {
            shell.borrow().editor.borrow_mut().add_to_history(&text);
            shell.borrow().transcript.borrow_mut().resume_follow_tail();
        }
        Ok(_) => {}
        Err(err) => fold_notice(world, &err.to_string()),
    }
}

/// Execute a keymap action that needs the session world, parked by the
/// controller's handler for the drive loop (widgets can reach neither the
/// host nor the queues). Returns whether renderable state changed.
async fn handle_host_action(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    action: AjAction,
) -> bool {
    match action {
        AjAction::CancelTurn => {
            // Nothing running: the chord falls through to the quit ladder,
            // and a message still queued stays queued.
            let active = world.chat.borrow().active_view();
            if !view_busy(world, active) {
                return false;
            }
            // Don't discard what the user lined up: pull any queued message
            // back into the editor (matching `aj`). Withdrawn *before* the
            // cancel, because the host reaps the cancelled turn itself and its
            // post-turn wake delivers whatever is still queued when it does.
            let yanked = yank_pending_into_editor(world, shell).await;
            cancel_viewed_turn(world).await;
            yanked
        }
        AjAction::Steer => {
            // Steering is incoherent with an armed branch anchor: it would
            // consume the branch prompt as steering for the branch being
            // abandoned. Refuse and keep the anchor and editor text intact.
            if shell.borrow().branch_anchor.borrow().is_some() {
                fold_notice(world, &branch_armed_notice("steer"));
                return true;
            }
            handle_steer(world, shell).await;
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
            yank_pending_into_editor(world, shell).await
        }
        // Read the clipboard image to a tempfile and insert its path at the
        // editor cursor. Silent when there is no image, matching `aj`.
        AjAction::PasteImage => paste_clipboard_image(shell),
        // The direct chords open the same overlays as their palette commands.
        // Park the matching command so the host's `apply_command_action` opens
        // it on the next drive-loop step (which owns the refocus move).
        // Nothing renders here yet, so no redraw.
        AjAction::HistoryOpen => {
            *shell.borrow().command_slot.borrow_mut() = Some(CommandAction::OpenPromptHistory);
            false
        }
        AjAction::AgentPickerOpen => {
            *shell.borrow().command_slot.borrow_mut() = Some(CommandAction::OpenAgentPicker);
            false
        }
        AjAction::SessionTag => {
            *shell.borrow().command_slot.borrow_mut() = Some(CommandAction::OpenSessionTag);
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
        | AjAction::SidebarToggle
        | AjAction::SessionNext
        | AjAction::SessionPrev
        | AjAction::SessionNew
        | AjAction::Quit => false,
    }
}

/// The notice `aj` folds when a command that needs an idle agent is
/// chosen mid-turn.
fn session_busy_notice(what: &str) -> String {
    format!(
        "Can't {what} while a turn is running. Press {} to cancel it first.",
        fixed_keys::CTRL_C
    )
}

/// The outcome of a submit made while a branch anchor is armed.
enum ArmedSubmit {
    /// Stay in the current session: the submit was refused (empty, or busy).
    /// The anchor and any needed notice or toast are handled inside, the
    /// caller only redraws.
    Stay,
    /// Resolved: break the drive loop, move the session's head to `target`,
    /// and run `prompt` as the branch's first turn.
    Branch { target: HeadTarget, prompt: String },
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
        fold_notice(
            world,
            &format!(
                "Type a message to branch, or press {} to cancel.",
                close_key_label()
            ),
        );
        return ArmedSubmit::Stay;
    }
    // Busy: refuse and keep the anchor and text. A head switch mid-turn
    // would let the running turn persist onto the branch being left, so the
    // host refuses one while a turn or background work is live. We check the
    // mirror here too, for the toast wording and so the editor's text is
    // kept rather than handed to a refusal one layer down.
    let (agents, bash) = running_work(world);
    if agents + bash > 0 {
        shell.borrow().editor.borrow_mut().set_text(&text);
        shell.borrow().show_toast(busy_refusal("branch"));
        return ArmedSubmit::Stay;
    }
    let message_id = shell.borrow().branch_anchor.borrow().clone();
    let Some(message_id) = message_id else {
        // No anchor: the caller gates on `is_some`, so this is unreachable in
        // practice. Treat it as a plain submit rather than panicking.
        handle_submit(world, text).await;
        return ArmedSubmit::Stay;
    };
    // The anchor names the message, and the host moves the head to its
    // parent: a branch replaces the message rather than continuing after it
    // (spec 6.6). Resolving it here would need the log, which a connection
    // does not have, and would race an append besides.
    shell.borrow().disarm_branch();
    ArmedSubmit::Branch {
        target: HeadTarget::Before(message_id),
        prompt: text,
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

/// The tag the peer reports for the focused session, `None` when it has none
/// or the peer has not published a row for it yet.
///
/// Read off the directory rather than the store: the directory is the one
/// answer both modes have, and it is the same row the sidebar draws, so the
/// editor opens on the label the user is looking at.
fn focused_tag(world: &World) -> Option<String> {
    world
        .directory
        .rows()
        .iter()
        .find(|row| row.id == world.session())
        .and_then(|row| row.tag.clone())
}

/// Every label the peer reports, by session id, for the surfaces that show a
/// list of sessions.
///
/// A snapshot: the selector is a view of one moment, and re-reading it per
/// streamed batch would let rows in one list disagree about a session.
fn session_tags(world: &World) -> HashMap<String, String> {
    world
        .directory
        .rows()
        .iter()
        .filter_map(|row| Some((row.id.clone(), row.tag.clone()?)))
        .collect()
}

/// Send a confirmed tag edit to the peer that owns the session.
///
/// The one path for both modes: the host applies it under the session's own
/// lock and republishes the row, so the sidebar shows the new label without
/// this loop touching a sidecar. A refusal is folded in the peer's words.
async fn apply_tag_edit(world: &mut World, edit: TagEdit) {
    if let Err(err) = world
        .control
        .command(world.session(), Command::Tag { tag: edit.tag })
        .await
    {
        fold_notice(world, &format!("Could not set the session tag: {err}"));
    }
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
            // `/compact` runs as a tracked turn on the main agent, so the
            // host refuses it while one is already running. Its own wording
            // is the protocol's; the local notice points at the chord that
            // cancels the turn first, which is why the conflict is the one
            // refusal reworded here.
            match world
                .control
                .command(world.session(), Command::Compact { instructions: None })
                .await
            {
                Ok(_) => {}
                Err(err) if err.conflict() => {
                    fold_notice(world, &session_busy_notice("compact"));
                }
                Err(err) => fold_notice(world, &err.to_string()),
            }
            ActionEffect::Redraw
        }
        CommandAction::ExportHtml => {
            // Rendering the whole session to HTML (CPU) plus the file write
            // would park the single drive loop, so `spawn_session_export` runs
            // them off the loop and delivers the result notice to the export
            // fill arm. The action just spawns and returns. See that helper for
            // the log-lock reasoning.
            let Some(log) = world.local.as_ref().map(|local| Arc::clone(&local.log)) else {
                fold_notice(
                    world,
                    &remote_unsupported_notice(
                        "export the session",
                        "the log and the written file are the host's, so run the export there",
                    ),
                );
                return ActionEffect::Redraw;
            };
            spawn_session_export(&log, world.session(), export_tx);
            ActionEffect::None
        }
        CommandAction::OpenThinkingSelector => {
            let target = world.chat.borrow().active_view();
            let current = viewed_thinking(world, target);
            let (provider, model_id) = viewed_model(world, target);
            let supported = world
                .catalog
                .iter()
                .find(|m| m.provider == provider && m.id == model_id)
                .map(aj_app::commands::thinking_levels_for)
                .unwrap_or_else(|| aj_app::commands::THINKING_LEVELS.iter().collect());
            let handles = shell.borrow().overlay_handles();
            open_thinking(
                &handles.stack,
                &handles.editor,
                &handles.chrome,
                &handles.activity,
                target,
                current,
                supported,
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
            // NOTE: the previews come off this process's own session store, so
            // over a connection this overlay describes the wrong machine. The
            // sidebar is the surface that lists a peer's sessions (spec 9.2),
            // and it reads the `list` rows instead.
            if world.control.is_remote() {
                fold_notice(
                    world,
                    &remote_unsupported_notice(
                        "browse this machine's sessions",
                        "a connection's sessions are the ones in the sidebar",
                    ),
                );
                return ActionEffect::Redraw;
            }
            let handles = shell.borrow().overlay_handles();
            open_session_selector(&handles, world.session().to_string(), session_tags(world));
            ActionEffect::OpenedOverlay
        }
        CommandAction::OpenSessionTree => match open_tree_overlay(world, shell).await {
            Ok(_) => ActionEffect::OpenedOverlay,
            Err(err) => {
                fold_notice(world, &format!("Could not read the session tree: {err}"));
                ActionEffect::Redraw
            }
        },
        // Read-only to open and safe mid-work: relabelling a session touches
        // nothing a turn is using. The peer's own row is what the editor is
        // prefilled from, so a local and a connected session start from the
        // label the sidebar is showing.
        CommandAction::OpenSessionTag => {
            let current = focused_tag(world);
            let handles = shell.borrow().overlay_handles();
            open_session_tag(&handles, current.as_deref());
            ActionEffect::OpenedOverlay
        }
        CommandAction::NewSession => {
            // Live work is no reason to refuse. The session we leave stays
            // attached and keeps folding, so its turn finishes whether or not
            // anyone is looking at it. No overlay to close either, so the
            // request parks straight away and the drive loop's post-input
            // check turns it into `SessionExit::New`.
            *shell.borrow().session_request.borrow_mut() = Some(SessionRequest::New);
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
/// killing a task goes out as a host command and folds a notice.
async fn apply_picker_outcome(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    outcome: AgentPickerOutcome,
) -> ActionEffect {
    match outcome {
        AgentPickerOutcome::Observe(id) => {
            // Every sub-agent's transcript is already in the model: an attach
            // block projects sub threads eagerly, so there is nothing to
            // materialize on demand.
            //
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
            // line for the viewer header. A task the model no longer tracks
            // has nothing to show.
            let command = world
                .chat
                .borrow()
                .tasks()
                .get(&id)
                .and_then(|task| match &task.kind {
                    aj_agent::tool::TaskKind::Bash { command } => Some(command.clone()),
                    aj_agent::tool::TaskKind::Agent { .. } => None,
                });
            match command {
                Some(command) => {
                    open_task_viewer(world, shell, id, command);
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
            let notice = kill_task(world, id).await;
            fold_notice(world, &notice);
            ActionEffect::Redraw
        }
    }
}

/// Open the task-output viewer for `id`, backed by the live registry locally
/// and by the per-task read over a connection (spec 6.7). A remote viewer's
/// handle is kept so the drive loop can push the snapshots it polls.
fn open_task_viewer(world: &World, shell: &Rc<RefCell<Shell>>, id: TaskId, command: String) {
    let handles = shell.borrow().overlay_handles();
    let backing = match world.local.as_ref() {
        Some(local) => TaskBacking::Local(local.task_registry.clone()),
        None => TaskBacking::Remote(Rc::clone(&handles.task_kill)),
    };
    let view = open_task_output(
        &handles.stack,
        &handles.editor,
        &handles.chrome,
        backing,
        id,
        command,
    );
    if world.control.is_remote() {
        *shell.borrow().task_view.borrow_mut() = Some(view);
    }
}

/// Kill background task `id`, answering the notice to fold.
///
/// The status the model holds is what tells the three outcomes apart, which
/// the command alone cannot (a kill of a task that already finished is
/// accepted and does nothing). The picker's rows are a snapshot from open
/// time, so the model is consulted afresh: the task may have finished while
/// the picker was up.
async fn kill_task(world: &World, id: TaskId) -> String {
    let live = world.chat.borrow().tasks().get(&id).map(|task| task.status);
    match live {
        Some(aj_agent::tool::TaskStatus::Running) => {
            match world
                .control
                .command(world.session(), Command::KillTask { task: id })
                .await
            {
                Ok(_) => format!("Killing background task #{id}."),
                Err(err) => err.to_string(),
            }
        }
        Some(_) => format!("Background task #{id} already finished."),
        None => format!("Background task #{id} is not in the registry (already gone?)."),
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
/// back to the session's active settings when it has no entry.
fn viewed_thinking(world: &World, target: AgentId) -> Option<ThinkingConfig> {
    world
        .chat
        .borrow()
        .footers()
        .settings(target)
        .or_else(|| world.client().settings())
        .and_then(|s| thinking_config_from_name(&s.thinking))
        .flatten()
}

/// The viewed agent's current `(provider, id)`, from its footer entry, falling
/// back to the session's active settings.
fn viewed_model(world: &World, target: AgentId) -> (String, String) {
    let settings = world.chat.borrow().footers().settings(target).cloned();
    settings
        .as_ref()
        .or_else(|| world.client().settings())
        .map(|s| (s.provider.clone(), s.model_id.clone()))
        .unwrap_or_default()
}

/// The `provider/id` of the session's active model, as the settings window
/// spells it in its model row.
fn active_model(world: &World) -> String {
    let (provider, model_id) = viewed_model(world, AgentId::Main);
    format!("{provider}/{model_id}")
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
                // Session-scoped: the selectors leave `config.toml` alone and
                // rely on the session log's record to survive a resume.
                if let Some(notice) =
                    confirm_thinking(world, target, PersistAction::None, level).await
                {
                    fold_notice(world, &notice);
                }
            }
            SelectorActivity::ModelConfirmed { target, info } => {
                if let Some(notice) = confirm_model(world, target, PersistAction::None, *info).await
                {
                    fold_notice(world, &notice);
                }
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

/// Record the main agent's settings identity into the chat model so the
/// footer's model line and context gauge reflect a change without waiting for
/// the next turn.
///
/// Read off the focused session's run config, which is what the host just
/// staged and what its refreshed `state` frame reports. The footer widget
/// reads the chat model's footer table rather than the client's settings, so
/// the change has to be noted there.
///
/// A connection has no run config to read: the refreshed `state` frame is on
/// its way and the fold notes the footer from it, which is the same value one
/// round trip later.
fn note_main_footer(world: &World) {
    let Some(handles) = world.local.as_ref() else {
        return;
    };
    let (settings, context_window) = local_settings_seed(handles);
    world
        .chat
        .borrow_mut()
        .footers_mut()
        .note_settings(AgentId::Main, settings, context_window);
}

/// Send a settings change to the host, returning the note to fold when it
/// applied and the refusal to fold when it did not.
///
/// An accepted change folds nothing of substance here: the host stages it,
/// records it on the session log, and publishes the notice that record
/// projects plus a refreshed `state` frame, so the transcript row arrives on
/// the stream like any other. The one thing the host cannot report is that a
/// change meant to outlive the session did not persist, which is what the
/// `Ok` note carries.
async fn command_settings(
    world: &World,
    agent: AgentId,
    persist: PersistAction,
    axis: SettingsAxis,
) -> Result<Option<String>, String> {
    let persisting = persist != PersistAction::None;
    let change = SettingsChange {
        agent,
        persist,
        axis,
    };
    match world
        .control
        .command(world.session(), Command::Settings(change))
        .await
    {
        Ok(_) => Ok(unpersisted_note(world, persisting)),
        Err(err) => Err(err.to_string()),
    }
}

/// The note a persisting settings change earns over a connection: the wire
/// carries no persist axis (spec 6.6), because the config files a default
/// would be written to are the host's own, not this client's.
fn unpersisted_note(world: &World, persisting: bool) -> Option<String> {
    (persisting && world.control.is_remote()).then(|| {
        "Applied to this session only: a settings default can't be saved over a \
         connection."
            .to_string()
    })
}

/// Apply a confirmed thinking pick and reconcile the footer entry it moved.
async fn confirm_thinking(
    world: &World,
    target: AgentId,
    persist: PersistAction,
    level: Option<ThinkingConfig>,
) -> Option<String> {
    let name = aj_app::commands::thinking_level_name(&level).to_string();
    let note = match command_settings(world, target, persist, SettingsAxis::Thinking(level)).await {
        Ok(note) => note,
        Err(refusal) => return Some(refusal),
    };
    match target {
        AgentId::Main => note_main_footer(world),
        AgentId::Sub(_) => patch_sub_footer(world, target, |settings| settings.thinking = name),
    }
    note
}

/// Apply a confirmed model pick and reconcile the footer entry it moved.
async fn confirm_model(
    world: &World,
    target: AgentId,
    persist: PersistAction,
    info: ModelInfo,
) -> Option<String> {
    let note =
        match command_settings(world, target, persist, SettingsAxis::Model(info.clone())).await {
            Ok(note) => note,
            Err(refusal) => return Some(refusal),
        };
    match target {
        AgentId::Main => note_main_footer(world),
        AgentId::Sub(_) => {
            // The host rebuilds a sub's bundle at the session's speed, so
            // that is the speed to show for it.
            let speed = world
                .client()
                .settings()
                .map(|settings| settings.speed.clone());
            let window = info.context_window;
            patch_sub_footer_window(world, target, window, |settings| {
                settings.provider = info.provider.clone();
                settings.model_id = info.id.clone();
                if let Some(speed) = speed {
                    settings.speed = speed;
                }
            });
        }
    }
    note
}

/// Patch the footer entry the frontend tracks for `target`, keeping its
/// context window.
///
/// A sub-agent's settings live in its own override map, which no run config
/// and no `state` frame carries, so the axis that moved is written onto the
/// entry the footer already holds. A target with no entry yet has no footer
/// row to correct.
fn patch_sub_footer(world: &World, target: AgentId, patch: impl FnOnce(&mut AgentSettings)) {
    let window = world
        .chat
        .borrow()
        .footers()
        .context_usage(target)
        .context_window;
    patch_sub_footer_window(world, target, window, patch);
}

/// [`patch_sub_footer`] with a fresh context window, for a change that moves
/// the model the gauge measures against.
fn patch_sub_footer_window(
    world: &World,
    target: AgentId,
    context_window: u64,
    patch: impl FnOnce(&mut AgentSettings),
) {
    let Some(mut settings) = world.chat.borrow().footers().settings(target).cloned() else {
        return;
    };
    patch(&mut settings);
    world
        .chat
        .borrow_mut()
        .footers_mut()
        .note_settings(target, settings, context_window);
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
        ui.set_value(id, value);
    }
}

/// Apply one settings-window change (or project clear) to the running session
/// and persist it per `persist`. Returns the user-facing notice, `None` when
/// the change is one the host announces itself.
///
/// The four session settings (model, thinking, speed, verbosity) go out as
/// host commands, which is what makes them visible to every other client;
/// the render toggles mutate the chat model; the theme row reloads the
/// palette and re-tints live; the rest are plain config-backed values
/// persisted with a "takes effect" note. A refused apply reverts the row's
/// display through [`revert_setting_row`], so the window never shows a value
/// that is not actually active.
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
                revert_setting_row(shell, MODEL_SETTING_ID, &active_model(world));
                return Some(format!("Unknown model {value}."));
            };
            let refused = confirm_model(world, AgentId::Main, persist, info).await;
            // A refusal stages nothing, so the row is reverted to the model
            // that is actually active. Compared rather than assumed, because
            // the staged key is the only authority on what took.
            let active = active_model(world);
            if active != value {
                revert_setting_row(shell, MODEL_SETTING_ID, &active);
            }
            refused
        }
        "thinking" => match thinking_config_from_name(value) {
            Some(level) => confirm_thinking(world, AgentId::Main, persist, level).await,
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
            // A real settings axis: the host stages it onto the session's
            // stream options, persists it per `persist`, and publishes the
            // confirmation itself (it is live-only, so nothing durable
            // records it and the notice rides an untagged frame).
            command_settings(
                world,
                AgentId::Main,
                persist,
                SettingsAxis::ThinkingDisplay(display),
            )
            .await
            .unwrap_or_else(Some)
        }
        "speed" => match speed_from_name(value) {
            Some(speed) => {
                match command_settings(world, AgentId::Main, persist, SettingsAxis::Speed(speed))
                    .await
                {
                    Ok(note) => {
                        note_main_footer(world);
                        note
                    }
                    // The rebuild failed, so nothing was staged: revert the
                    // row to the speed still in force.
                    Err(notice) => {
                        let previous = world
                            .client()
                            .settings()
                            .map(|settings| settings.speed.clone());
                        if let Some(previous) = previous {
                            revert_setting_row(shell, "speed", &previous);
                        }
                        Some(notice)
                    }
                }
            }
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
            command_settings(
                world,
                AgentId::Main,
                persist,
                SettingsAxis::Verbosity(verbosity),
            )
            .await
            .unwrap_or_else(Some)
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
        "show_thinking_block" => {
            let show = value == "true";
            world.chat.borrow_mut().show_thinking_block = show;
            let save = aj_app::settings::persist_setting(
                &world.config_layers,
                &world.config,
                persist,
                "show_thinking_block",
                Some(value),
                |c| c.show_thinking_block = show,
            );
            Some(join_notice(
                format!(
                    "Thinking blocks {}.",
                    if show { "expanded" } else { "hidden" }
                ),
                save,
            ))
        }
        "show_token_usage" => {
            let show = value == "true";
            world.chat.borrow_mut().show_token_usage = show;
            let save = aj_app::settings::persist_setting(
                &world.config_layers,
                &world.config,
                persist,
                "show_token_usage",
                Some(value),
                |c| c.show_token_usage = show,
            );
            Some(join_notice(
                format!(
                    "Token-usage rows {}.",
                    if show { "shown" } else { "hidden" }
                ),
                save,
            ))
        }
        "compact_transcript" => {
            let on = value == "true";
            world.chat.borrow_mut().compact_transcript = on;
            let save = aj_app::settings::persist_setting(
                &world.config_layers,
                &world.config,
                persist,
                "compact_transcript",
                Some(value),
                |c| c.compact_transcript = on,
            );
            Some(join_notice(
                format!(
                    "Compact transcript {}.",
                    if on { "enabled" } else { "disabled" }
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
        "show_image_in_terminal" => {
            let show = value == "true";
            world.chat.borrow_mut().show_image_in_terminal = show;
            let save = aj_app::settings::persist_setting(
                &world.config_layers,
                &world.config,
                persist,
                "show_image_in_terminal",
                Some(value),
                |c| c.show_image_in_terminal = show,
            );
            Some(join_notice(
                format!("show_image_in_terminal set to {show}."),
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
            // The label lives beside the log, and the peer's row is where both
            // modes can read it, so it is taken here rather than inside the
            // spawned read.
            let tag = focused_tag(world);
            // Not supported over the wire in v1 (spec 9.1): the stats come off
            // the host's own log. The overlay is already open, so the refusal
            // fills it rather than folding a notice behind it.
            let Some(log) = world.local.as_ref().map(|local| Arc::clone(&local.log)) else {
                let rows = vec![crate::content_overlay::plain(remote_unsupported_notice(
                    "show session info",
                    "it reads the host's own log, so run it there",
                ))];
                let _ = tx.send((FetchKind::SessionInfo, rows));
                return;
            };
            tokio::spawn(async move {
                let stats = { log.lock().await.stats() };
                let _ = tx.send((
                    FetchKind::SessionInfo,
                    session_info_rows(&stats, tag.as_deref()),
                ));
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
            // Stop the walk once the receiver is gone (overlay closed or the
            // app is quitting): the scan runs on the blocking pool and would
            // otherwise pin process shutdown until it finished reading.
            let cancel = || tx.is_closed();
            let mut emit = |batch: Vec<PromptEntry>| {
                let _ = tx.send(ScanMsg::Batch(batch));
            };
            match scope {
                HistoryScope::Workspace => aj_session::workspace_history_streaming(
                    &persistence,
                    MAX_ENTRIES,
                    &cancel,
                    &mut emit,
                ),
                HistoryScope::All => match Config::get_sessions_base_dir_path() {
                    Ok(base) => aj_session::all_workspaces_history_streaming(
                        &base,
                        MAX_ENTRIES,
                        &cancel,
                        &mut emit,
                    ),
                    // Fall back to the current workspace so the toggle still
                    // shows something when the base dir can't be resolved.
                    Err(err) => {
                        tracing::debug!("could not resolve sessions base dir: {err}");
                        aj_session::workspace_history_streaming(
                            &persistence,
                            MAX_ENTRIES,
                            &cancel,
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
        {
            // Stop the walk once the receiver is gone (overlay closed or the
            // app is quitting): the scan runs on the blocking pool and would
            // otherwise pin process shutdown until it finished reading. The
            // current session is the largest and is scanned first, so an
            // in-file cancellation check (inside the streaming scan) is what
            // actually bounds the stall.
            let cancel = || tx.is_closed();
            let mut emit = |batch: Vec<SessionPreview>| {
                let _ = tx.send(ScanMsg::Batch(batch));
            };
            persistence.list_session_previews_streaming(&cancel, &mut emit);
        }
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
fn spawn_session_export(
    log: &Arc<tokio::sync::Mutex<aj_session::ConversationLog>>,
    session: &str,
    tx: &UnboundedSender<String>,
) {
    let tx = tx.clone();
    let log = Arc::clone(log);
    let session_id = session.to_string();
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
        // Stop the walk once the receiver is gone (the app is quitting before
        // the ring was seeded): the scan runs on the blocking pool and would
        // otherwise pin process shutdown.
        let cancel = || tx.is_closed();
        let mut entries: Vec<String> =
            aj_session::workspace_history(&persistence, TextArea::HISTORY_LIMIT, &cancel)
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
    /// Where a remote task-output viewer parks a `Ctrl+K` kill, since it has
    /// no registry to kill through (spec 6.6's task-kill command).
    pub(crate) task_kill: Rc<RefCell<Option<TaskId>>>,
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
    /// Where the session-tag editor parks a confirmed label.
    pub(crate) tag_edit: Rc<RefCell<Option<TagEdit>>>,
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
            task_kill: Rc::new(RefCell::new(None)),
            history_fetch: Rc::new(RefCell::new(None)),
            skills_fill: Rc::new(RefCell::new(None)),
            recall_slot: Rc::new(RefCell::new(None)),
            session_scan: Rc::new(RefCell::new(None)),
            session_request: Rc::new(RefCell::new(None)),
            auth_request: Rc::new(RefCell::new(None)),
            tag_edit: Rc::new(RefCell::new(None)),
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
    /// sequence is armed. Drawn straight from the live keymap state. Plain
    /// `RefCell` (no `Rc`): never shared, the cell exists for the `&self`
    /// restyle path.
    quit_hint: RefCell<QuitHint>,
    /// The quit-arm hint's running-work warning, refreshed by the drive
    /// loop on the arming edge (it owns the task registry the widgets
    /// can't reach). Shared with the [`QuitHint`], which reads it at draw.
    quit_hint_warning: Rc<RefCell<Option<String>>>,
    /// The frame-statistics debug overlay, floated in the top-right corner
    /// when `show_frame_stats` is on. Reads the `frame_stats` snapshot below.
    /// Plain `RefCell` like `quit_hint`.
    frame_stats_box: RefCell<FrameStatsBox>,
    /// What the session sidebar draws, refreshed from the session directory by
    /// [`sync_sidebar`] once per loop iteration and read by the strip at draw
    /// time. Also owns whether the strip is shown at all, which the toggle
    /// action flips.
    sidebar: Rc<RefCell<SidebarState>>,
    /// The toast-stack widget, drawn bottom-right every frame: stacked above
    /// the quit hint when no modal is open, floated over the scrim/overlay
    /// (z 3) otherwise. Reads the `toasts` stack below. Plain `RefCell` like
    /// `quit_hint`.
    toast_box: RefCell<Toasts>,
    /// The live toast records, shared with the writers (the drive loop's
    /// select-to-copy fold, the overlay confirm closures, [`Shell::show_toast`])
    /// and the `toast_box` that draws them. The drive loop prunes it and
    /// wakes at the earliest live deadline.
    toasts: ToastStack,
    /// The last select-to-copy record, written by the transcript (which the
    /// unified toast stack deliberately leaves untouched). The drive loop
    /// edge-detects fresh records by their timestamp and folds each into
    /// `toasts`. Copy payload, so a `Cell` not a `RefCell`.
    selection_copied: Rc<Cell<Option<SelectionCopied>>>,
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
    /// A task kill parked by a remote task-output viewer, for the drive loop
    /// to send as a command.
    task_kill: Rc<RefCell<Option<TaskId>>>,
    /// The open remote task-output viewer, so the drive loop can push the
    /// per-task read's snapshots into it. `None` whenever no such viewer is
    /// open: the close-all chord and a session rebind clear it, and the loop
    /// retires it when the overlay stack empties (see [`poll_task_output`]).
    task_view: Rc<RefCell<Option<Rc<RefCell<TaskOutputView>>>>>,
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
    /// Where the session-tag editor parks a confirmed label, read by the drive
    /// loop, which owns the control surface the tag command travels over.
    tag_edit: Rc<RefCell<Option<TagEdit>>>,
    /// The armed branch anchor: the branched-from user message's stable id,
    /// `Some` while the user is composing a branch (after `b`). The
    /// `on_action` handler arms it, the drive loop resolves it on submit, the
    /// transcript reads it to keep the highlight box on that message, and any
    /// session install clears it so a stale anchor can't resolve against a
    /// different session's log.
    branch_anchor: Rc<RefCell<Option<String>>>,
    /// Set by the Esc handler when it cancels an armed anchor, so the drive
    /// loop folds the cancel notice (the Shell can't reach the chat model's
    /// lifecycle). A plain flag, drained once per input event.
    branch_cancelled: Rc<Cell<bool>>,
    /// The per-session image store, shared with the [`TranscriptView`]'s entry
    /// builder (which records pending images and reads transmitted ids) and
    /// the host loop (which transmits after each frame and frees on a session
    /// switch).
    image_store: Rc<RefCell<ImageStore>>,
    /// The probed terminal capabilities, unknown at construction (the probe
    /// runs after `app.init`). Set once by [`Shell::set_terminal_caps`] and
    /// read via [`Shell::terminal_caps`] and [`Shell::restyle`] so a rebuild
    /// reflects the probed caps.
    /// Interior mutability, like the theme handle, so the const-`&self`
    /// restyle path can read them.
    terminal_caps: Cell<TerminalCaps>,
}

impl Shell {
    fn new(
        chat: Rc<RefCell<ChatState>>,
        status: Rc<RefCell<StatusState>>,
        task_registry: Option<TaskRegistry>,
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
        let selection_copied: Rc<Cell<Option<SelectionCopied>>> = Rc::new(Cell::new(None));
        // The toast stack, shared between its writers (the drive loop's copy
        // fold, the overlay confirm closures, `Shell::show_toast`) and the
        // `Toasts` box that draws it. The drive loop prunes it and wakes at
        // the earliest live deadline.
        let toasts: ToastStack = Rc::new(RefCell::new(Vec::new()));
        // The global busy flag, refreshed each drive-loop iteration. Read by
        // the session-overlay confirm closures to refuse a switch mid-work.
        let busy: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // Branch-anchor slot, following the parked-slot pattern: `on_action`
        // arms it on `b`, the drive loop resolves it on submit, the transcript
        // reads it to keep the highlight box on the branched-from message, and
        // the Esc handler flips `branch_cancelled` so the drive loop folds the
        // cancel notice. Created here so the closures and the transcript all
        // share the same cell.
        let branch_anchor: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let branch_cancelled: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        // The per-session image store, shared between the transcript builder
        // (which records pending images and reads transmitted ids) and the
        // host loop (which transmits and frees). Created here so both share
        // the same handle.
        let image_store: Rc<RefCell<ImageStore>> = Rc::new(RefCell::new(ImageStore::default()));
        // Resolve the initial styles and chrome from a single snapshot of
        // the theme, then keep the handle for the runtime re-style path.
        // Caps are unknown here (the probe runs after `app.init`), so styles
        // start with the default caps and `restyle` refreshes them.
        let (styles, transcript, chrome) = {
            let t = theme.read();
            let styles = Rc::new(TranscriptStyles::from_theme(&t, TerminalCaps::default()));
            let transcript = Rc::new(RefCell::new(TranscriptView::new(
                Rc::clone(&chat),
                &t,
                Rc::clone(&focus_mode),
                Rc::clone(&branch_anchor),
                Rc::clone(&selection_copied),
                Rc::clone(&image_store),
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
        let quit_hint = RefCell::new(QuitHint::new(
            Rc::clone(&styles),
            Rc::clone(&chrome),
            Rc::clone(&quit_hint_warning),
        ));
        // Off by default; the run loop seeds it from `config.show_frame_stats`
        // after building the Shell, matching the other display toggles.
        let show_frame_stats = Rc::new(Cell::new(false));
        let frame_stats: Rc<Cell<Option<FrameStats>>> = Rc::new(Cell::new(None));
        let frame_stats_box = RefCell::new(FrameStatsBox::new(
            Rc::clone(&styles),
            Rc::clone(&chrome),
            Rc::clone(&frame_stats),
        ));
        let toast_box = RefCell::new(Toasts::new(
            Rc::clone(&styles),
            Rc::clone(&chrome),
            Rc::clone(&toasts),
        ));
        let pending = Rc::new(RefCell::new(PendingBox::new(
            Rc::clone(&chat),
            Rc::clone(&styles),
        )));
        let footer = Rc::new(RefCell::new(FooterLine::new(
            Rc::clone(&chat),
            status,
            Rc::clone(&styles),
            cwd_display,
            task_registry,
        )));
        // The empty-state splash and the transcript share the chat slot. The
        // `ChatSlot` wrapper draws whichever fits the current state, so the
        // transcript's focus and scroll wiring is untouched while it is shown.
        let splash = Splash::new(Rc::clone(&chat), Rc::clone(&styles), theme.color_mode());
        let chat_slot = Rc::new(RefCell::new(ChatSlot {
            chat: Rc::clone(&chat),
            splash: Rc::clone(&splash),
            transcript: Rc::clone(&transcript),
        }));
        // Slot order mirrors `aj`'s layout: header, chat (flex), status,
        // pending, editor, footer. The status and pending slots collapse to
        // zero height while idle/empty, so the editor sits flush under the chat
        // between turns.
        let header_line = Rc::new(RefCell::new(Text::new(&header)));
        let column: WidgetRef = Rc::new(RefCell::new(FlexColumn {
            children: vec![
                FlexItem::init(to_widget_ref(Rc::clone(&header_line)), 0),
                FlexItem::init(to_widget_ref(Rc::clone(&chat_slot)), 1),
                FlexItem::init(to_widget_ref(Rc::clone(&status_line)), 0),
                FlexItem::init(to_widget_ref(Rc::clone(&pending)), 0),
                FlexItem::init(to_widget_ref(Rc::clone(&editor)), 0),
                FlexItem::init(to_widget_ref(Rc::clone(&footer)), 0),
            ],
        }));
        // The strip runs the full height beside the column, not beside the chat
        // alone: it is about the connection rather than the transcript, and a
        // strip that stopped at the editor would leave the session list looking
        // like part of the conversation. `flex == 0` gives it its own fixed
        // width and the column everything left (see `SIDEBAR_COLS`), and it
        // draws nothing at all while hidden, so plain local use pays no width.
        let sidebar = Rc::new(RefCell::new(SidebarState::default()));
        let sidebar_strip = Rc::new(RefCell::new(SessionSidebar::new(
            Rc::clone(&sidebar),
            Rc::clone(&styles),
            // The band the pointer leaves on a row is the same one every
            // pick list draws under its cursor, so "the pointer is here"
            // reads the same everywhere in the app.
            chrome.borrow().select.selected_bg,
        )));
        let layout: WidgetRef = Rc::new(RefCell::new(FlexRow {
            children: vec![
                FlexItem::init(to_widget_ref(Rc::clone(&sidebar_strip)), 0),
                FlexItem::init(column, 1),
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
        let task_kill: Rc<RefCell<Option<TaskId>>> = Rc::new(RefCell::new(None));
        let task_view: Rc<RefCell<Option<Rc<RefCell<TaskOutputView>>>>> =
            Rc::new(RefCell::new(None));
        let history_fetch: Rc<RefCell<Option<HistoryFetch>>> = Rc::new(RefCell::new(None));
        let skills_fill: Rc<RefCell<Option<SkillsFill>>> = Rc::new(RefCell::new(None));
        let recall_slot: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let session_scan: Rc<RefCell<Option<SessionScan>>> = Rc::new(RefCell::new(None));
        let session_request: Rc<RefCell<Option<SessionRequest>>> = Rc::new(RefCell::new(None));
        let auth_request: Rc<RefCell<Option<AuthPickerRequest>>> = Rc::new(RefCell::new(None));
        let tag_edit: Rc<RefCell<Option<TagEdit>>> = Rc::new(RefCell::new(None));
        let keymap_ctx = Rc::new(RefCell::new(HostCtx {
            overlays: Rc::clone(&overlays),
            editor: Rc::clone(&editor),
            focus_mode: Rc::clone(&focus_mode),
            turn_running: false,
            login_active: false,
            chat: Rc::clone(&chat),
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
            let task_view_for_actions = Rc::clone(&task_view);
            let transcript_for_actions = Rc::clone(&transcript);
            let transcript_widget: WidgetRef = to_widget_ref(Rc::clone(&transcript));
            let editor_for_actions = Rc::clone(&editor);
            let branch_anchor_for_actions = Rc::clone(&branch_anchor);
            let action_slot = Rc::clone(&host_action);
            let theme_for_actions = theme.clone();
            let sidebar_for_actions = Rc::clone(&sidebar);
            let session_request_for_actions = Rc::clone(&session_request);
            Box::new(move |ctx, action| match action {
                AjAction::SidebarToggle => {
                    let mut state = sidebar_for_actions.borrow_mut();
                    state.visible = !state.visible;
                    // An explicit ask outranks the row-count default for the
                    // rest of the process, in both directions: a user who hid
                    // the strip does not want it back when a session appears.
                    state.toggled = true;
                    ctx.redraw = true;
                }
                // Stepping the sidebar's order rather than the working set's:
                // the strip is what the user is reading, so next means the row
                // below the one highlighted, whether or not that session has
                // ever been attached. Parked as a session request, which is how
                // every session change reaches the loop that owns the world.
                AjAction::SessionNext | AjAction::SessionPrev => {
                    let forward = matches!(action, AjAction::SessionNext);
                    if let Some(session) = step_session(&sidebar_for_actions.borrow(), forward) {
                        park_session_request(
                            &session_request_for_actions,
                            ctx,
                            SessionRequest::Resume(session),
                        );
                    }
                }
                AjAction::SessionNew => {
                    park_session_request(&session_request_for_actions, ctx, SessionRequest::New);
                }
                AjAction::ThinkingToggle => {
                    // Matches aj's `aj.thinking.toggle` handler: flip the
                    // visibility flag, no notice (the transcript shows the new
                    // state).
                    let mut chat = chat.borrow_mut();
                    chat.show_thinking_block = !chat.show_thinking_block;
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
                    // never re-tinted or reverted by the host, and the same for
                    // a task viewer, so nothing is polled for a closed overlay.
                    *settings_ui_for_actions.borrow_mut() = None;
                    *task_view_for_actions.borrow_mut() = None;
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
                        // Prefill the editor with the message (preserving the
                        // user's draft on the recall history), arm the anchor,
                        // and move focus to the editor so the user edits the
                        // branch prompt. The focus move's `FocusOut` exits
                        // transcript focus, but the transcript keeps the
                        // highlight box on the branched-from message by reading
                        // the armed anchor.
                        prefill_branch_editor(&editor_for_actions, &text);
                        arm_branch(&branch_anchor_for_actions, message_id);
                        ctx.request_focus(Rc::clone(&editor_widget));
                        ctx.redraw = true;
                    }
                }
                AjAction::CancelTurn
                | AjAction::Steer
                | AjAction::Dequeue
                | AjAction::PasteImage
                | AjAction::HistoryOpen
                | AjAction::AgentPickerOpen
                | AjAction::SessionTag => {
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
        // Route transcript box clicks through the picker outcome slot, so the
        // drive loop applies the normal observe behavior.
        {
            let picker_outcome = Rc::clone(&picker_outcome);
            transcript
                .borrow_mut()
                .set_on_observe_agent(Box::new(move |id| {
                    *picker_outcome.borrow_mut() = Some(AgentPickerOutcome::Observe(id));
                }));
        }
        // A pointer gesture on the strip parks the request the stepping and
        // create chords park, through the function they both call, so a click
        // triggers the action rather than reimplementing it (spec 9.2).
        {
            let session_request = Rc::clone(&session_request);
            sidebar_strip
                .borrow_mut()
                .set_on_gesture(Box::new(move |ctx, gesture| {
                    let request = match gesture {
                        StripGesture::Focus(session) => SessionRequest::Resume(session),
                        StripGesture::New => SessionRequest::New,
                    };
                    park_session_request(&session_request, ctx, request);
                }));
        }
        let keymap =
            KeymapController::new(build_keymap(), Rc::clone(&keymap_ctx), layout, on_action);

        Shell {
            keymap,
            keymap_ctx,
            editor,
            status_line,
            sidebar,
            quit_hint,
            quit_hint_warning,
            frame_stats_box,
            toast_box,
            toasts,
            selection_copied,
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
            task_kill,
            task_view,
            history_fetch,
            skills_fill,
            recall_slot,
            header: header_line,
            window_title,
            session_scan,
            session_request,
            auth_request,
            tag_edit,
            branch_anchor,
            branch_cancelled,
            image_store,
            terminal_caps: Cell::new(TerminalCaps::default()),
        }
    }

    /// Collect a submit parked by the editor callback, if any.
    fn take_submitted(&self) -> Option<String> {
        self.submitted.borrow_mut().take()
    }

    /// Clear the armed branch anchor.
    fn disarm_branch(&self) {
        *self.branch_anchor.borrow_mut() = None;
    }

    /// Raise a transient bottom-right toast with `body`. Live toasts
    /// stack, each with its own timer. The caller still owns the repaint (the
    /// drive loop schedules the clearing repaint at the toast's deadline).
    fn show_toast(&self, body: impl Into<ToastBody>) {
        crate::toasts::show_toast(&self.toasts, body);
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

    /// Collect a confirmed tag edit parked by the session-tag editor, if any.
    fn take_tag_edit(&self) -> Option<TagEdit> {
        self.tag_edit.borrow_mut().take()
    }

    /// The shared handles the drive loop needs to open an overlay: the stack
    /// it pushes onto, the editor (focus fallback), a live chrome snapshot,
    /// the parked-request slots, and the busy flag plus toast stack the
    /// session-changing confirms read and raise into.
    /// Columns the sidebar takes from the left of the base column, which
    /// anything floated over that column has to clear.
    fn sidebar_cols(&self) -> u16 {
        if self.sidebar.borrow().shown() {
            SIDEBAR_COLS
        } else {
            0
        }
    }

    /// Hold the strip back on a terminal with no width to spare.
    ///
    /// The strip is inflexible, so on a narrow terminal it would take its full
    /// width off a transcript that has none to give and leave the column
    /// nothing. Kept apart from `visible` so the user's ask survives a resize
    /// and comes back when the width does.
    fn suppress_sidebar_if_too_narrow(&self, terminal_cols: u16) {
        self.sidebar.borrow_mut().too_narrow = terminal_cols < MIN_COLS_WITH_SIDEBAR;
    }

    fn overlay_handles(&self) -> OverlayHandles {
        OverlayHandles {
            stack: Rc::clone(&self.overlays),
            editor: to_widget_ref(Rc::clone(&self.editor)),
            chrome: self.chrome.borrow().clone(),
            activity: Rc::clone(&self.selector_activity),
            settings_ui: Rc::clone(&self.settings_ui),
            picker_outcome: Rc::clone(&self.picker_outcome),
            task_kill: Rc::clone(&self.task_kill),
            history_fetch: Rc::clone(&self.history_fetch),
            skills_fill: Rc::clone(&self.skills_fill),
            recall_slot: Rc::clone(&self.recall_slot),
            session_scan: Rc::clone(&self.session_scan),
            session_request: Rc::clone(&self.session_request),
            auth_request: Rc::clone(&self.auth_request),
            tag_edit: Rc::clone(&self.tag_edit),
            busy: Rc::clone(&self.busy),
            toasts: Rc::clone(&self.toasts),
        }
    }

    /// Record the probed terminal capabilities, read once after `app.init`.
    /// The caller runs [`restyle`](Self::restyle) afterward so the styles pick
    /// up the probed `images` gate.
    fn set_terminal_caps(&self, caps: TerminalCaps) {
        self.terminal_caps.set(caps);
    }

    /// The probed terminal capabilities, for components built outside the
    /// `restyle` path (e.g. the login dialog) that must honor the same caps.
    fn terminal_caps(&self) -> TerminalCaps {
        self.terminal_caps.get()
    }

    /// Rebuild every style struct from the current theme, for a runtime
    /// swap (hot-reload, or the settings window's theme row). Every
    /// palette-consuming widget is rebuilt in place, so editor text and
    /// transcript scroll survive the swap. An open settings window is
    /// re-tinted live (its list band and its window chrome); other overlays
    /// opened before the swap keep their baked styles until reopened.
    fn restyle(&self) {
        let t = self.theme.read();
        let styles = Rc::new(TranscriptStyles::from_theme(&t, self.terminal_caps()));
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
    /// model with nothing to do here. What does need repointing is every
    /// handle into the session itself: the pending box's message queues and
    /// the footer's task registry belong to the session that owns them, and
    /// the old clones would observe a session nobody is looking at. Plus the
    /// header id and the window title. We also drop the transcript back to
    /// follow-tail so the next session opens pinned to the bottom.
    ///
    /// NOTE: the root `Shell` instance and the `AsyncApp` are deliberately
    /// left untouched: the app's mouse/focus handlers hold the root Shell Rc
    /// captured at `init`, so rebuilding the root or re-initializing the app
    /// would strand them. We swap the Shell's innards, never the Shell.
    fn rebind(&mut self, world: &World) {
        // Every open overlay is scoped to the session it was opened over: a
        // task viewer holds that session's task registry, a settings window a
        // handle this swap cannot reach. Closing the stack is what keeps one
        // from surviving a session change pointed at the previous session.
        self.overlays.borrow_mut().close_all();
        *self.settings_ui.borrow_mut() = None;
        *self.task_view.borrow_mut() = None;
        self.rebind_handles(world);
        self.header.borrow_mut().text = format!("{APP_TITLE} - session {}", world.session());
        self.window_title =
            aj_app::session::window_title(APP_TITLE, world.session(), &world.working_directory);
        self.transcript.borrow_mut().reset_to_tail();
    }

    /// Point the chrome that reads a session's own handles at the ones the
    /// world now holds.
    ///
    /// Split out of [`Self::rebind`] because a re-attach can land on a fresh
    /// materialization of the same session, which needs this and nothing else
    /// rebind does: closing the overlay stack and dropping the view to the tail
    /// belong to a session change.
    fn rebind_handles(&self, world: &World) {
        self.footer.borrow_mut().set_task_registry(
            world
                .local
                .as_ref()
                .map(|local| local.task_registry.clone()),
        );
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
        // Same reason, for the sidebar: only the Shell learns the terminal
        // width, because a flex row measures the strip under an unbounded one.
        self.suppress_sidebar_if_too_narrow(ctx.max.size().width);

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
            // The popup belongs to the editor, so it starts where the editor
            // does. The sidebar indents the whole base column, and a popup left
            // at column zero would sit over the strip instead of under the text
            // it completes.
            let indent = self.sidebar_cols();
            let popup_width = term.width.saturating_sub(indent);
            if let Some(popup) = editor.draw_autocomplete_popup_surface(popup_width, max_rows) {
                let popup = block_mouse(popup, &self.transcript);
                // Anchor so the popup's bottom edge abuts the editor's top.
                let anchor = editor_top.saturating_sub(popup.size.height);
                inner.children.push(SubSurface {
                    origin: RelativePoint {
                        row: i32::from(anchor),
                        col: i32::from(indent),
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
/// binary-driven turn in the cancel map, or a running initial
/// sub-agent spawn), mirrored into the keymap context. Called from the
/// drive loop's per-iteration sync point, its single writer.
fn sync_keymap_ctx(world: &World, shell: &Rc<RefCell<Shell>>) {
    let active = world.chat.borrow().active_view();
    let busy = view_busy(world, active);
    // The global busy flag the session-overlay confirm closures read: any
    // in-flight turn OR background work (background sub-agents + bash tasks),
    // not just the viewed agent. Distinct from `turn_running` above, which is
    // per-view and gates the keymap's steer/dequeue chords.
    let (agents, bash) = running_work(world);
    let shell = shell.borrow();
    shell.busy.set(agents + bash > 0);
    let mut ctx = shell.keymap_ctx.borrow_mut();
    ctx.turn_running = busy;
    ctx.active_view = active;
}

/// Reconcile the editor's border tint and top-bar label from the active view
/// and branch state. The border follows the viewed agent's thinking level
/// (aj's color-bar parity). The label reads `branching` while a branch is
/// armed (the salient mode), else `agent N` for a sub-agent, cleared for the
/// main agent. This is the single writer: the drive loop calls it once per
/// iteration and once before the first paint, so no view-switch, arm, or
/// thinking-change path has to remember to retint.
fn sync_editor_chrome(world: &World, shell: &Rc<RefCell<Shell>>) {
    let active = world.chat.borrow().active_view();
    let level = viewed_thinking(world, active);
    let shell = shell.borrow();
    let color = editor_border_color(&shell.theme.read(), level.as_ref());
    // A pending branch overrides the agent marker: while composing the branch
    // prompt the mode matters more than which view is behind it.
    let label = if shell.branch_anchor.borrow().is_some() {
        Some("branching".to_string())
    } else {
        match active {
            AgentId::Main => None,
            AgentId::Sub(n) => Some(format!("agent {n}")),
        }
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

    // Resolve launch attachments before terminal setup so failures leave
    // the terminal untouched. Image handling uses the effective project-over-user
    // setting, just like images read later through the read_file tool.
    let launch_content = aj_app::cli::initial_input(
        &args,
        &std::env::current_dir().unwrap_or_default(),
        layers.effective().image_auto_resize,
    )?
    .into_content();

    let auth = AuthStorage::at_default_path().context("failed to open ~/.aj/auth.json")?;
    let sessions_dir = Config::get_sessions_dir_path()?;
    let persistence = ConversationPersistence::new(sessions_dir);

    // Connect mode dials the host before anything touches the terminal, so an
    // unreachable host or a protocol mismatch reports on the normal screen
    // (spec 9.1).
    let mut world = match &args.command {
        Some(CliCommand::Connect {
            url,
            session_id,
            new,
            prompt: _,
        }) => {
            // Statedness has to come from the layers, not from the effective
            // config: only a create sends stated axes (spec section 8), and the
            // effective config cannot tell a written entry from a fallback.
            let (user_layer, _) = Config::load_layer();
            let stated = crate::connect::Stated::new(user_layer, layers.project.clone());
            let connected = crate::connect::connect(
                &args,
                &layers.effective(),
                &stated,
                ConnectTarget {
                    url,
                    session_id: session_id.as_deref(),
                    new: *new,
                },
            )
            .await?;
            build_connect_world(&args, connected, layers, &diagnostics, &auth, &persistence).await?
        }
        _ => build_world(&args, layers, &diagnostics, &auth, &persistence, None).await?,
    };

    // The control port serves the very host this shell renders, so a remote
    // client and this terminal are peers over one host rather than two
    // processes contending for one session store. Started before the
    // terminal is taken over, so a refused bind (an address the identity
    // gate will not serve unauthenticated) reports on the normal screen.
    // Connect mode has no host of its own to serve.
    let server = match world.control.host() {
        Some(host) => match crate::serve::start_server(&args, host).await {
            Ok(server) => server,
            Err(err) => {
                shut_down_host(&world).await;
                return Err(err);
            }
        },
        None => None,
    };

    // Auto-submit the launch prompt as the initial session's first turn.
    // This sits before the outer session loop below, so an in-process
    // session change never resubmits, matching `aj`.
    auto_submit_launch(&mut world, launch_content).await;

    // Resolve the configured theme (default `light`, matching `aj`) and
    // load it at the env-detected color mode. `AsyncApp::init` runs the
    // async DA1 probe, so the true-color capability isn't known until
    // after init; we reconcile the mode below once it is. Building the
    // theme now with `ColorMode::detect` is the documented fallback for
    // "theme built before init".
    let theme_name = resolve_theme_name(world_config_theme(&world).as_deref()).to_string();
    let env_mode = ColorMode::detect();
    let theme = ThemeHandle::new(Theme::load_with_mode(&theme_name, env_mode));
    let header = format!("{APP_TITLE} - session {}", world.session());
    let cwd = world.working_directory.clone();
    let shell = Rc::new(RefCell::new(Shell::new(
        Rc::clone(&world.chat),
        Rc::clone(&world.status),
        world
            .local
            .as_ref()
            .map(|local| local.task_registry.clone()),
        theme.clone(),
        header,
        world.session(),
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

    // Probe the runtime terminal capabilities now that init's detection ran.
    // `images` comes from the real kitty-graphics probe; `hyperlinks` stays
    // optimistic (vaxis has no OSC 8 probe). See [`TerminalCaps`].
    let caps = TerminalCaps {
        images: app.vaxis().caps.kitty_graphics,
        ..TerminalCaps::default()
    };
    shell.borrow().set_terminal_caps(caps);

    // Reconcile the color mode against the terminal's probed capability.
    // A positive `caps.rgb` (the terminal affirmed truecolor during the
    // init probe) upgrades an env guess of Color256, but a negative probe
    // never downgrades the env guess: most terminals don't answer the
    // truecolor query at all, and the env heuristic is the better signal
    // for them. When the mode actually changes we reload the theme.
    let probed_mode = if app.vaxis().caps.rgb {
        ColorMode::Truecolor
    } else {
        env_mode
    };
    if probed_mode != theme.color_mode() {
        theme.replace(Theme::load_with_mode(&theme_name, probed_mode));
    }
    // Restyle unconditionally: the styles must reflect both the reconciled
    // color mode and the probed caps (`images`), and the caps are only known
    // now. One all-miss frame is cheap, so we don't gate this on a change.
    shell.borrow().restyle();
    app.request_redraw();

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

    // Outer session loop. Each iteration drives one focused session; a
    // new-session, resume or branch request exits `drive` with the matching
    // `SessionExit`, whereupon the focus moves over the same Shell (see
    // `focus_session`). Quit and fatal errors break out. The usage of each
    // session the process leaves is snapshotted for the shutdown banner so a
    // multi-session process itemizes every session, matching `aj`.
    let mut completed_sessions: Vec<(String, UsageSummary)> = Vec::new();
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

        let request = match exit {
            Ok(SessionExit::Quit) => break Ok(()),
            Err(fatal) => break Err(fatal),
            Ok(SessionExit::New) => FocusRequest::Create,
            Ok(SessionExit::Switch(session_id)) => FocusRequest::Resume(session_id),
            Ok(SessionExit::Branch { target, prompt }) => FocusRequest::Branch { target, prompt },
        };

        // Read the outgoing session's usage before the change and record it
        // only once the change took: nothing is torn down here (the host
        // keeps every session it materialized live), so a refused change
        // leaves the same session focused and its usage still growing.
        let previous = world.session().to_string();
        let usage = session_usage(&world, &previous).await;
        let moved = apply_focus_request(&mut app, &shell, &mut world, request).await;
        match moved {
            Focus::Moved => match usage {
                Some(usage) => completed_sessions.push((previous, usage)),
                None => {
                    tracing::warn!("could not read {previous}'s usage for the exit banner");
                }
            },
            // A branch stays in its session, and a refusal never left it, so
            // the banner keeps counting this session as the live one.
            Focus::Same => {}
        }
    };

    // Read what the banner needs before the host tears its sessions down:
    // afterwards there is no live session to ask.
    let banner = ExitBanner::collect(&world, completed_sessions).await;
    // Remote clients lose the connection when their host departs (spec
    // section 5), so the port closes before the sessions behind it do.
    if let Some(server) = server {
        server.shutdown().await;
    }
    // Cancels every turn through the graceful path, quiesces the background
    // tasks, flushes the logs, and releases the session locks. Connect mode
    // has no host to wind down: dropping its stream is what deregisters it.
    shut_down_host(&world).await;
    app.shutdown().await;

    // The alt screen wiped the conversation from the terminal, so the
    // normal screen gets the usage banner and the resume hint.
    banner.print();
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

/// The select loop for the focused session: the host's frames, terminal
/// input, widget timers, theme reloads, and async overlay fills.
///
/// Returns the reason it stopped driving this session: `Quit` when the user
/// quits or input ends, `New` / `Switch(id)` / `Branch` when a session change
/// is requested. The outer loop in [`run`] moves the focus and re-enters.
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
/// The transcript writes the shared `selection_copied` cell (the unified toast
/// stack deliberately leaves it in place), so the drive loop edge-detects fresh
/// records here by their timestamp: each new copy pushes exactly one toast
/// with the copy-toast look and its own timer. Returns whether a toast was
/// pushed, so the caller requests the showing repaint.
fn fold_selection_copied_record(shell: &Shell, seen: &mut Option<Instant>) -> bool {
    let Some(copied) = shell.selection_copied.get() else {
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

/// How long the first failed attempt waits, doubling from there. Short,
/// because a stream that dropped because the host restarted is usually back
/// before a longer delay would have elapsed.
const RETRY_BACKOFF_MIN: Duration = Duration::from_millis(200);

/// The ceiling on a paced retry. A client whose host is gone for good keeps
/// trying at this rate, which is what makes the shell survive a host restart
/// without the user reaching for the shell's history.
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Paces a retry the drive loop has nothing else to wake it for.
///
/// Every failure doubles the delay up to [`RETRY_BACKOFF_MAX`], so a peer that
/// stopped answering cannot make the loop spend each iteration in a request
/// that will fail. Nothing here is aware of what is being retried: the pacing
/// is the same whether the failing thing is an attach, a read, or a poll.
struct Retry {
    /// How long the next failure waits. Doubles per failure, back to
    /// [`RETRY_BACKOFF_MIN`] whenever the pacing is dropped.
    delay: Duration,
    /// When the next attempt is allowed, `None` while one is due now. The
    /// loop merges this into its wake deadline, which is what makes a paced
    /// retry happen on time even when nothing else is going on.
    due: Option<Instant>,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            delay: RETRY_BACKOFF_MIN,
            due: None,
        }
    }
}

impl Retry {
    /// Whether the next attempt may run.
    fn ready(&self) -> bool {
        self.due.is_none_or(|due| Instant::now() >= due)
    }

    /// When the next attempt comes due, `None` while one is due now.
    fn due(&self) -> Option<Instant> {
        self.due
    }

    /// Note a failed attempt, holding the next one back.
    fn failed(&mut self) {
        self.due = Some(Instant::now() + self.delay);
        self.delay = (self.delay * 2).min(RETRY_BACKOFF_MAX);
    }

    /// Drop the pacing: whatever comes next is due at once.
    fn clear(&mut self) {
        *self = Self::default();
    }

    /// Drop the pacing but hold the next attempt back by `delay`, for a caller
    /// that polls on a cadence of its own.
    fn again_in(&mut self, delay: Duration) {
        *self = Self {
            due: Some(Instant::now() + delay),
            ..Self::default()
        };
    }
}

/// Where a client stands in getting its frame stream back.
///
/// Two steps rather than one, because the "catching up" state has to be
/// *seen*: the attach block is producer-paced, so folding it parks the loop
/// for as long as the backfill takes, and the paint that shows the state has
/// to happen before that. The loop therefore opens the stream in one
/// iteration and folds the block in the next.
struct Resume {
    step: ResumeStep,
    /// Pacing for the whole recovery rather than for one step of it: a stream
    /// that keeps dying inside its attach block has to back off exactly like
    /// one that cannot be opened at all, because every attempt costs the host
    /// a full projection and a client's cursor does not move until the block
    /// completes (spec 6.5).
    retry: Retry,
}

enum ResumeStep {
    /// Waiting to re-open the stream.
    Waiting,
    /// The stream is open and its attach block still has to be folded.
    CatchingUp,
}

impl Resume {
    /// The state a lost stream leaves: the first attempt is due at once.
    fn lost() -> Resume {
        Resume {
            step: ResumeStep::Waiting,
            retry: Retry::default(),
        }
    }

    /// Whether the next step may run.
    fn ready(&self) -> bool {
        self.retry.ready()
    }

    /// When the loop next has work to do for this state. Now, unless a
    /// failure is holding the next attempt back.
    fn due(&self) -> Instant {
        self.retry.due().unwrap_or_else(Instant::now)
    }

    fn connection(&self) -> Connection {
        match self.step {
            ResumeStep::Waiting => Connection::Reconnecting,
            ResumeStep::CatchingUp => Connection::CatchingUp,
        }
    }

    /// Note a failed step: the recovery starts over from the open, after the
    /// backoff.
    fn failed(&mut self) {
        self.retry.failed();
        self.step = ResumeStep::Waiting;
    }
}

/// How often an open task-output overlay is refreshed from the per-task read
/// in connect mode. A tail the user is watching should move visibly without
/// the read becoming a load of its own.
const TASK_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Refresh the open remote task-output viewer from the per-task read (spec
/// 6.7), answering whether anything was pushed into it.
///
/// `retry` paces the read: at most one per [`TASK_POLL_INTERVAL`] while such a
/// viewer is open, and a backed-off retry while the host is failing it, so a
/// read that takes its full timeout cannot be re-issued the moment it returns.
/// A local viewer re-reads its registry at draw time and never reaches here.
async fn poll_task_output(world: &World, shell: &Rc<RefCell<Shell>>, retry: &mut Retry) -> bool {
    // The viewer's own close callback cannot reach the slot (the widget knows
    // nothing about it), so an empty overlay stack is what retires it. The
    // picker drops out before the viewer opens, so the viewer is the only
    // overlay up and its Esc empties the stack. A stale handle under some
    // other overlay would only mean refreshing an invisible view until it
    // does.
    if !shell.borrow().overlays.borrow().is_open() {
        *shell.borrow().task_view.borrow_mut() = None;
    }
    let view = shell.borrow().task_view.borrow().clone();
    let Some(view) = view else {
        retry.clear();
        return false;
    };
    if !retry.ready() {
        return false;
    }
    let task = view.borrow().task();
    match world.control.task_details(world.session(), task).await {
        Ok(details) => {
            retry.again_in(TASK_POLL_INTERVAL);
            view.borrow_mut().apply_details(details);
            true
        }
        Err(err) => {
            // The task may simply be gone from the host's registry. The viewer
            // keeps its last-known body, matching the local one.
            retry.failed();
            tracing::debug!("could not read background task {task}: {err}");
            false
        }
    }
}

/// Advance a pending re-attach by one step, answering the state that is left
/// (`None` once the stream is back and caught up).
///
/// This is the recovery of a stream that *ended*, and the one fatal case lives
/// here: the shell's own host no longer serving a session it was holding means
/// the host is gone, and the shell has nothing left to drive, since the agent,
/// the log and the tools all live in it. Every other failure, a connection's
/// included, is a backed-off retry.
///
/// A re-attach owed while the stream is still live is a different question and
/// never fatal, see [`discharge_reattach`].
async fn advance_resume(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    mut state: Resume,
) -> Result<Option<Resume>, ControlError> {
    match state.step {
        ResumeStep::Waiting => {
            match open_stream(&world.control, &mut world.directory).await {
                Ok(stream) => {
                    world.stream = stream;
                    // The reopened stream may name a fresh materialization, so
                    // the handles are re-read before the block is folded.
                    refresh_local_handles(world, shell).await?;
                    state.step = ResumeStep::CatchingUp;
                    Ok(Some(state))
                }
                Err(err) if !world.control.is_remote() => Err(err),
                Err(err) => {
                    tracing::warn!("could not re-attach the session: {err}");
                    state.failed();
                    Ok(Some(state))
                }
            }
        }
        ResumeStep::CatchingUp => {
            let complete = fold_attach_block(world).await;
            refresh_client_reads(world).await;
            // The block may have been served under a fresh epoch (a host
            // restart mints one), which resets the chat model and restarts its
            // entry ids. Dropping the view to the tail is what clears the
            // render cache keyed by them.
            shell.borrow().transcript.borrow_mut().reset_to_tail();
            if !complete {
                // The stream died inside the block. What was applied stays,
                // and the next attach serves the rest from our cursor.
                state.failed();
                return Ok(Some(state));
            }
            fold_notice(world, reattached_notice(&world.control));
            Ok(None)
        }
    }
}

/// The confirmation a completed re-attach folds: a connection came back, an
/// in-process shell only got its subscription back.
fn reattached_notice(control: &Control) -> &'static str {
    if control.is_remote() {
        "Reconnected to the host."
    } else {
        "Re-attached to the session."
    }
}

/// Discharge the re-attach a broken continuity obliges, answering the resume
/// state that is left (`None` when nothing is pending).
///
/// Leaving the obligation undischarged would silently freeze the transcript:
/// every later frame carries an epoch the fold filters out. A refused attach
/// keeps the obligation, so `retry` paces the next attempt: without it a peer
/// that keeps refusing turns into one attach (and one warning row) per loop
/// iteration. Nothing gives up, so a peer that keeps refusing keeps being asked
/// at [`RETRY_BACKOFF_MAX`], which is what makes the shell outlast a session
/// its host is slow to hand back.
async fn discharge_reattach(
    world: &mut World,
    shell: &Rc<RefCell<Shell>>,
    retry: &mut Retry,
) -> Option<Resume> {
    match reattach(world, shell).await {
        // A block that did not complete leaves a dead stream, which the frame
        // arm picks up as a loss like any other.
        Ok(_) => {
            retry.clear();
            None
        }
        Err(err) => {
            fold_warning(world, &format!("Lost the session's event stream: {err}"));
            retry.failed();
            // A connection hands the obligation to the resume machinery, which
            // paces its own attempts and paints the connection state while it
            // does. An in-process host keeps retrying right here instead,
            // because the resume path reads a failed local open as the host
            // being gone and ends the shell (see `advance_resume`): this
            // obligation arrives on a stream that is still live, so a refusal
            // says the session moved rather than that the host did, and it must
            // not cost the user the buffer they were typing in.
            world.control.is_remote().then(Resume::lost)
        }
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
    let mut selection_copied_seen: Option<Instant> =
        shell.borrow().selection_copied.get().map(|c| c.at);
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
    // Set while the focused session's stream is down, in either mode: a
    // subscription is lost for ordinary reasons even in process (see the frame
    // arm), and the re-attach is what tells that apart from a host that is
    // gone.
    let mut resume: Option<Resume> = None;
    // Paces the per-task read behind an open remote task-output overlay: the
    // steady cadence while it answers, a backoff while it does not. Cleared
    // while no such viewer is open, which is what bounds the poll to an
    // overlay that can show its answer.
    let mut task_poll = Retry::default();
    // Paces the re-attach a broken continuity obliges, so a peer that keeps
    // refusing it cannot turn into one attempt (and one warning row) per
    // iteration.
    let mut reattach_retry = Retry::default();
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
        // The sidebar mirror leads the iteration, ahead of both the paint and
        // the input read. A session request breaks out of the input arm below,
        // so a mirror refreshed at the foot of the iteration would leave a key
        // already buffered when the loop is re-entered to be answered from rows
        // that predate the switch, and a stepping chord would name the session
        // just landed on.
        sync_sidebar(world, shell);
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
            // The frame just drawn recorded any visible-but-untransmitted
            // images as pending. Transmit them now so the next frame places
            // them (lazy, draw-driven transmission, see `drain_pending_images`).
            drain_pending_images(app, world, shell);
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
        // Toasts have no self-timer, so wake at the earliest toast deadline
        // (past deadlines included, so an expired-but-unpruned toast still
        // wakes us): the per-iteration prune below drops what expired and
        // requests the clearing repaint, so each toast vanishes exactly on
        // time even while others stay live.
        let toast_deadline = crate::toasts::earliest_toast_deadline(&shell.borrow().toasts);
        // A pending re-attach, an open remote task viewer, a paced read retry
        // and an undischarged re-attach all have work due at a known time, and
        // none has an event to wake the loop.
        let resume_deadline = resume.as_ref().map(Resume::due);
        let poll_deadline = task_poll.due();
        let reads_deadline = owes_client_reads(world)
            .then(|| world.reads_retry.due())
            .flatten();
        let reattach_deadline = world
            .directory
            .needs_reattach()
            .then(|| reattach_retry.due())
            .flatten();
        let deadline = [
            tick_deadline,
            frame_deadline,
            toast_deadline,
            resume_deadline,
            poll_deadline,
            reads_deadline,
            reattach_deadline,
        ]
        .into_iter()
        .flatten()
        .min();
        tokio::select! {
            biased;

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
                                    ArmedSubmit::Branch { target, prompt } => {
                                        break Ok(SessionExit::Branch {
                                            target,
                                            prompt: Some(prompt),
                                        });
                                    }
                                }
                            } else {
                                handle_editor_submit(world, shell, text).await;
                            }
                        }
                        // An Esc that cancelled an armed branch anchor: fold
                        // the cancel notice (the Shell can't reach the chat
                        // lifecycle) and redraw so it shows.
                        if shell.borrow().take_branch_cancelled() {
                            fold_notice(world, "Branch cancelled.");
                            app.request_redraw();
                        }
                        // Bind the take out of the borrow first: the action
                        // handlers await on the host.
                        let host_action = shell.borrow().take_host_action();
                        if let Some(action) = host_action
                            && handle_host_action(world, shell, action).await
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
                        // A confirmed session-tag edit. Bound out of the borrow
                        // first: the command awaits on the peer.
                        let tag_edit = shell.borrow().take_tag_edit();
                        if let Some(edit) = tag_edit {
                            apply_tag_edit(world, edit).await;
                            app.request_redraw();
                        }
                        // A parked session change (the `NewSession` command, a
                        // confirmed resume pick, a tree-view branch switch).
                        // The request sites refuse while work is live up front
                        // and the host refuses a head switch it cannot serve,
                        // so there is nothing to recheck here. Bind the take
                        // out of the borrow first so no RefCell ref outlives
                        // the statement.
                        let session_request = shell.borrow().take_session_request();
                        if let Some(request) = session_request {
                            break Ok(request.into_exit());
                        }
                    }
                    // The reader ended (EOF or a read error), so no
                    // further input can arrive.
                    None => break Ok(SessionExit::Quit),
                }
            }

            // --- Host frame ---
            // This arm sits BELOW the input arm on purpose. A fast streaming
            // turn floods the session's frame stream, and under `biased` an arm
            // above input would keep winning and starve typing until the turn
            // quiesced, so a typed follow-up or steer would render late. Below
            // input, typed input always wins. The per-iteration drain at the
            // loop's bottom still folds the rest of the batch, so a burst
            // collapses into one redraw.
            //
            // Gated on there being a live stream: a dead one is permanently
            // ready, so polling it while a re-attach is pending would spin the
            // loop.
            frame = world.stream.recv(), if resume.is_none() => {
                match frame {
                    ControlFrame::Frame(frame) => {
                        if world.directory.apply(&mut world.chat.borrow_mut(), frame).0 {
                            app.request_redraw();
                        }
                    }
                    // A stream ends for ordinary reasons in either mode: a
                    // connection drops, and an in-process subscriber is evicted
                    // when this loop stopped draining long enough for the
                    // host's reliable fan-out to overflow (spec 6.9). Both
                    // recover the same way, by re-attaching with a cursor
                    // (spec 6.5). Only a local attach that then fails means
                    // the host itself is gone, and `advance_resume` is where
                    // that becomes the shell's exit.
                    lost => {
                        if let ControlFrame::Lost(err) = lost {
                            fold_warning(world, &format!("Lost the connection: {err}"));
                        }
                        resume = Some(Resume::lost());
                        world.connection = Connection::Reconnecting;
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
        // Fold whatever else the host published, including the frames a
        // command this iteration issued (they are queued by the time it
        // returns). One drain per iteration is what keeps the chrome mirrors
        // below from lagging the host by a frame.
        //
        // Suspended while a re-attach is pending: the attach block a resumed
        // stream carries is folded whole by the step below, and draining it
        // here would swallow the `caught_up` that step is waiting for.
        if resume.is_none() && fold_ready_frames(world) {
            app.request_redraw();
        }
        // A task kill parked by the remote task viewer, which has no registry
        // to kill through.
        let killed = shell.borrow().task_kill.borrow_mut().take();
        if let Some(task) = killed {
            let notice = kill_task(world, task).await;
            fold_notice(world, &notice);
            app.request_redraw();
        }
        // Advance a pending re-attach, one step per iteration so the paint at
        // the top of the loop shows each connection state.
        if resume.as_ref().is_some_and(Resume::ready) {
            let state = resume.take().expect("checked just above");
            match advance_resume(world, shell, state).await {
                Ok(next) => resume = next,
                // Only a local run reaches this: its own host refused to serve
                // a session it was holding, so the host is gone and the shell
                // goes with it.
                Err(err) => break Err(anyhow::anyhow!("the session host is gone: {err}")),
            }
            world.connection = resume
                .as_ref()
                .map_or(Connection::Connected, Resume::connection);
            app.request_redraw();
        }
        // Refresh the open remote task viewer from the per-task read.
        if poll_task_output(world, shell, &mut task_poll).await {
            app.request_redraw();
        }
        // Continuity broke (a head switch this process did not make), so the
        // client owes a re-attach. A pending re-attach already owes one, and
        // its own attach discharges this.
        if resume.is_none() && world.directory.needs_reattach() && reattach_retry.ready() {
            resume = discharge_reattach(world, shell, &mut reattach_retry).await;
            if resume.is_some() {
                world.connection = Connection::Reconnecting;
            }
            app.request_redraw();
        }
        // The attach block a re-attach served may have obliged the reads
        // again.
        refresh_client_reads(world).await;
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
            if fold_selection_copied_record(&shell, &mut selection_copied_seen) {
                app.request_redraw();
            }
            if crate::toasts::prune_expired(&shell.toasts) {
                app.request_redraw();
            }
        }
    };

    exit
}

/// One session's accumulated usage for the exit banner.
///
/// The host owns it for a local run (its agents hold the running totals). A
/// connection has no such read and renders the banner from the accounting its
/// own fold derived from the event stream (spec 9.1), which covers exactly
/// the frames this client saw.
async fn session_usage(world: &World, session: &str) -> Option<UsageSummary> {
    // Asking the host locks the session's agent, and a turn holds that lock
    // for its whole duration, so a busy session would park this loop until the
    // turn ended: no paint, no input, not even a cancel. Fall back to the
    // client's own event-derived accounting, which is what a connection uses
    // for the same banner (spec 9.1). A session with work in flight has no
    // final usage to report anyway.
    let (agents, bash) = running_work(world);
    let busy = agents + bash > 0;
    match world.control.host() {
        Some(host) if !busy => match host.usage(session).await {
            Ok(usage) => usage,
            Err(err) => {
                tracing::warn!("could not read {session}'s usage for the exit banner: {err}");
                None
            }
        },
        // `running_work` answers for the focused session, so the fallback is
        // only sound for that one. Both callers ask about it.
        _ if session == world.session() => Some(world.chat.borrow().usage_summary()),
        _ => None,
    }
}

/// Tear down the in-process host, if this run has one.
async fn shut_down_host(world: &World) {
    if let Some(host) = world.control.host() {
        host.shutdown().await;
    }
}

/// The end-of-run usage banner and resume hint.
///
/// Collected while the host is still up (reading a session's usage needs its
/// agent) and printed once the alt screen is gone, since the alt screen wiped
/// the conversation from the terminal.
struct ExitBanner {
    /// Each session the process left, in the order it left them, with the
    /// usage read at the moment it lost focus.
    completed: Vec<(String, UsageSummary)>,
    /// The session that was focused at the end, and its usage. `None` when
    /// the host could not answer, which leaves the block out rather than
    /// printing zeroes.
    live: Option<(String, UsageSummary)>,
    /// The `aj continue <id>` hint, present only for a session worth
    /// resuming.
    resume_hint: Option<String>,
}

impl ExitBanner {
    /// Read the banner's data off the host. Call with no turn in flight
    /// (reading a session's usage locks its agent).
    ///
    /// A connection reads neither: its usage comes from this client's own
    /// event-derived accounting (spec 9.1), and the resume hint is left out
    /// because `aj continue` would resume a session on the *host*, not here.
    async fn collect(world: &World, completed: Vec<(String, UsageSummary)>) -> ExitBanner {
        let live = session_usage(world, world.session())
            .await
            .map(|usage| (world.session().to_string(), usage));
        // Only sessions with at least one persisted user-thread leaf are
        // worth resuming. A fresh session the user quit without typing
        // anything gets no hint.
        let resume_eligible = match world.local.as_ref() {
            Some(handles) => handles
                .log
                .lock()
                .await
                .latest_leaf(ThreadFilter::USER)
                .is_some(),
            None => false,
        };
        ExitBanner {
            completed,
            live,
            resume_hint: resume_eligible.then(|| format_resume_hint(world.session())),
        }
    }

    /// Print the banner to stdout, dimmed and indented like `aj`'s shutdown
    /// banner.
    ///
    /// A single-session process prints one bare usage block. When the process
    /// spanned several sessions (new-session / resume), each session it left
    /// is itemized first, in order, under a dim `Session: <id>` header, then
    /// the live one's block, matching `aj`.
    fn print(&self) {
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

        for (session_id, summary) in &self.completed {
            print_block(Some(&format_session_usage_header(session_id)), summary);
        }
        if let Some((session_id, summary)) = &self.live {
            let header =
                (!self.completed.is_empty()).then(|| format_session_usage_header(session_id));
            print_block(header.as_deref(), summary);
        }
        if let Some(hint) = &self.resume_hint {
            println!(" {}", dim(hint));
            println!();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{PipeWriter, Write};
    use std::sync::Arc;

    use aj_app::chat::{EntryKind, NoticeLevel, SubAgentStatus, ToolStatus, reduce};
    use aj_app::session::AgentLifecycle;
    use aj_app::test_support::CanonicalState;
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
                thinking_display: "default".into(),
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
            Some(TaskRegistry::default()),
            ThemeHandle::new(Theme::bundled_dark_with_mode(
                aj_app::theme::ColorMode::Truecolor,
            )),
            "aj".to_string(),
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
            Some(TaskRegistry::default()),
            ThemeHandle::new(Theme::bundled_dark_with_mode(
                aj_app::theme::ColorMode::Truecolor,
            )),
            format!("aj - session {session_id}"),
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
            world.session(),
            &world.handles().env.working_directory,
        );
        assert_eq!(shell.window_title, expected);
        assert_ne!(
            shell.window_title, "aj - old-session - oldproj",
            "rebind must retitle for the switched-to session"
        );
    }

    /// A session switch must repoint the footer at the new session's task
    /// registry. The registry is session-scoped, so without the swap the
    /// footer keeps reading the torn-down session's queues and the
    /// pending-notice indicator silently never fires again.
    #[tokio::test]
    async fn rebind_repoints_the_footer_at_the_new_task_registry() {
        let dir = TempDir::new().expect("tempdir");
        let world = scripted_world(&dir, "streaming-text").await;
        world
            .handles()
            .task_registry
            .push_notice(aj_agent::tool::TaskNotice {
                owner: AgentId::Main,
                task_id: 1,
                kind: aj_agent::tool::TaskKind::Bash {
                    command: "make".into(),
                },
                label: "make".into(),
                status: aj_agent::tool::TaskStatus::Exited(Some(0)),
                body: "exit 0".into(),
            });

        let mut shell = titled_shell("old-session", "/home/me/oldproj");
        assert_eq!(
            shell.footer.borrow().pending_notices(AgentId::Main),
            0,
            "a fresh shell holds its own empty registry"
        );

        shell.rebind(&world);
        assert_eq!(
            shell.footer.borrow().pending_notices(AgentId::Main),
            1,
            "rebind must repoint the footer at the switched-to session's registry"
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
            Some(TaskRegistry::default()),
            ThemeHandle::new(Theme::bundled_dark_with_mode(
                aj_app::theme::ColorMode::Truecolor,
            )),
            "aj".to_string(),
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
        let mut lifecycle = AgentLifecycle::default();
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
                    thinking_display: "default".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
            None,
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

    /// Where a test's bash spill files belong: inside the caller's temp dir, so
    /// its guard takes them. A background task's spill is persisted by
    /// contract, so left at the ambient temp directory it would outlive the
    /// test that started the task.
    fn spill_dir_in(dir: &TempDir) -> Option<String> {
        Some(dir.path().join("spill").to_string_lossy().into_owned())
    }

    /// [`default_layers`] with the spill directory aimed inside `dir`.
    fn layers_spilling_into(dir: &TempDir) -> ConfigLayers {
        ConfigLayers {
            user: Config {
                spill_dir: spill_dir_in(dir),
                ..Config::default()
            },
            ..default_layers()
        }
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
        scripted_world_with(dir, demo, layers, None).await
    }

    /// A scripted world whose host releases an idle session after
    /// `idle_grace`, for the tests about what a session switch leaves behind.
    async fn scripted_world_with(
        dir: &TempDir,
        demo: &str,
        mut layers: ConfigLayers,
        idle_grace: Option<Duration>,
    ) -> World {
        layers.user.spill_dir = spill_dir_in(dir);
        let args = Args::parse_from(["aj", "--scripted", demo]);
        let auth = AuthStorage::new(dir.path().join("auth.json"));
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        build_world(&args, layers, &[], &auth, &persistence, idle_grace)
            .await
            .expect("build world")
    }

    /// A `[keybindings]` override for an unknown action is rejected at startup
    /// and surfaced as a warning notice in the transcript, which the splash box
    /// shows. The override being invalid means the process-global store stays
    /// empty, so this does not disturb other tests' default resolution.
    #[tokio::test]
    async fn bad_keybindings_override_surfaces_a_startup_warning() {
        let dir = TempDir::new().expect("tempdir");
        let mut layers = default_layers();
        layers
            .user
            .keybindings
            .insert("aj.not.a.real.action".to_string(), "ctrl+z".to_string());
        let world = scripted_world_with_layers(&dir, "streaming-text", layers).await;

        let chat = world.chat.borrow();
        let has_warning = chat
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .any(|e| {
                matches!(
                    &e.kind,
                    EntryKind::Notice(n)
                        if n.level == NoticeLevel::Warning && n.text.contains("keybindings")
                )
            });
        assert!(has_warning, "expected a keybindings warning notice");
    }

    /// Every wait on the host in these tests is bounded by this, so a wedged
    /// session fails a test instead of hanging CI.
    const SETTLE_DEADLINE: Duration = Duration::from_secs(20);

    /// Fold frames until the focused session reports itself idle with no live
    /// background task, which is what the drive loop's frame arm plus its
    /// per-iteration drain do while a turn runs.
    ///
    /// The liveness question goes to the host (through whichever transport
    /// this world drives) rather than to the client's lifecycle, so a demo
    /// whose background sub-agent outlives its parent turn is covered too.
    async fn settle(world: &mut World) {
        let deadline = Instant::now() + SETTLE_DEADLINE;
        loop {
            fold_ready_frames(world);
            let quiet = world
                .control
                .sessions()
                .await
                .expect("session list")
                .sessions
                .iter()
                .find(|entry| entry.id == world.session())
                .is_some_and(|entry| !entry.working && entry.tasks == 0);
            // The client's own view has to have caught up with the host's,
            // not just the host be idle: over a connection the frames of the
            // work that just finished can still be in flight (in process they
            // are already queued, so this converges at once).
            if quiet && !world.client().working() {
                fold_ready_frames(world);
                return;
            }
            assert!(Instant::now() < deadline, "the session never went quiet",);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Discharge the task and queue reads into the client model, the way an
    /// attach block obliges (spec 6.7).
    ///
    /// Tests that stage host-side state through the live handles need it: a
    /// direct enqueue or task registration publishes no frame, so the model
    /// (which every client renders from) would not learn about it. A real
    /// client learns the same state from these two reads.
    async fn read_host_state(world: &mut World) {
        let tasks = world
            .control
            .tasks(world.session())
            .await
            .expect("the tasks read");
        let queue = world
            .control
            .queue(world.session())
            .await
            .expect("the queue read");
        let mut chat = world.chat.borrow_mut();
        world.directory.client_mut().set_tasks(&mut chat, tasks);
        world.directory.client_mut().set_queue(&mut chat, queue);
    }

    /// Queue a follow-up for `agent` on the host and let the client model see
    /// it, which is the two halves of what a real enqueue does.
    async fn stage_pending(world: &mut World, agent: AgentId, text: &str) {
        world.handles().queues.append_follow_up(agent, text);
        read_host_state(world).await;
    }

    /// Drive one scripted turn to completion so the session's log lands on disk
    /// and can be resumed by the session-switch paths.
    async fn persist_session(world: &mut World) {
        handle_submit(world, "persist me".to_string()).await;
        settle(world).await;
    }

    /// Point the focused session at a slowly streamed script, so a turn
    /// started against it is still in flight while the test asserts on it.
    ///
    /// The scripted demos hold a fixed number of inferences and end a turn
    /// immediately once they run out, so a test that needs a session to stay
    /// busy installs its own script. It goes in through the live run config,
    /// which is what the host stamps onto the agent at every turn start.
    fn install_busy_script(handles: &LocalHandles) {
        let messages = vec![aj_app::test_support::finalized_text_message(
            "a slowly streamed answer",
        )];
        let mut cfg = handles
            .run_config
            .lock()
            .expect("run config mutex poisoned");
        cfg.provider = Arc::new(aj_models::scripted::ScriptedProvider::from_messages(
            messages,
            1,
            Duration::from_millis(50),
        ));
    }

    /// Shut a world's host down, which releases the advisory locks its live
    /// sessions hold and flushes their logs.
    ///
    /// A second host over the same store (what [`resumed_world`] builds)
    /// refuses to materialize a session another host holds live, so a test
    /// that resumes what it just wrote has to let go of it first.
    ///
    /// The chat model outlives it: it is a plain `Rc` the host never sees, so
    /// a test can shut the host down and then assert on what it left behind,
    /// which is also how it avoids holding a `RefCell` borrow across the
    /// await.
    async fn shut_down(world: &World) {
        world.host().shutdown().await;
    }

    /// A resumed world plus the Shell and app the focus-change paths need.
    async fn resumed_world_shell_app(
        dir: &TempDir,
        demo: &str,
        session_id: &str,
    ) -> (World, Rc<RefCell<Shell>>, AsyncApp, PipeWriter, WidgetRef) {
        let world = resumed_world(dir, demo, session_id).await;
        let shell = shell_for(&world);
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

    /// A world resumed from `session_id`, reusing `dir`'s persistence so the
    /// session written by a prior [`scripted_world`] is found on disk.
    async fn resumed_world(dir: &TempDir, demo: &str, session_id: &str) -> World {
        let args = Args::parse_from(["aj", "--scripted", demo, "continue", session_id]);
        let auth = AuthStorage::new(dir.path().join("auth.json"));
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        build_world(&args, default_layers(), &[], &auth, &persistence, None)
            .await
            .expect("build resumed world")
    }

    /// Submit a prompt and drive a scripted demo, including its sub-agent
    /// turns and the wakes they earn, to full completion so the persisted log
    /// holds complete sub-agent runs.
    async fn drive_demo_to_completion(world: &mut World) {
        handle_submit(world, "run the demo".to_string()).await;
        settle(world).await;
    }

    /// A shell wrapping `world`'s chat and queues, for the resume/observe
    /// tests that need one but build the world by resuming rather than via
    /// [`world_and_shell`].
    fn shell_for(world: &World) -> Rc<RefCell<Shell>> {
        Rc::new(RefCell::new(Shell::new(
            Rc::clone(&world.chat),
            Rc::clone(&world.status),
            Some(world.handles().task_registry.clone()),
            ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor)),
            "aj".to_string(),
            "",
            PathBuf::from("/tmp"),
        )))
    }

    /// The lowest sub-agent index the Main transcript holds a box for.
    fn first_sub(chat: &ChatState) -> usize {
        sub_boxes(chat)
            .into_iter()
            .map(|(n, ..)| n)
            .min()
            .expect("a sub-agent box")
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
        TaskNotification {
            label: String,
            outcome: aj_agent::message::TaskOutcome,
            body: String,
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
            EntryKind::TaskNotification(n) => EntryShape::TaskNotification {
                label: n.label.clone(),
                outcome: n.outcome,
                body: n.body.clone(),
            },
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
                thinking_display: "default".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            0,
            Arc::new(Vec::new()),
        );
        let mut life = AgentLifecycle::default();
        {
            let log = world.handles().log.lock().await;
            for event in aj_session::replay(&log) {
                let _ = reduce(&mut eager, &mut life, event, None);
            }
        }
        eager.set_active_view(view);
        eager
    }

    /// Resume a session and confirm every sub-agent box is present, `Done`,
    /// and reporting, with its transcript already projected.
    ///
    /// The attach block projects sub threads eagerly, so a resumed session
    /// has nothing left to materialize on demand (spec 6.5, and section 13's
    /// accepted backfill cost).
    #[tokio::test]
    async fn a_resume_projects_every_subagent_transcript() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "parallel-agents").await;
        drive_demo_to_completion(&mut world).await;
        let session_id = world.session().to_string();
        shut_down(&world).await;

        let resumed = resumed_world(&dir, "parallel-agents", &session_id).await;
        let boxes = sub_boxes(&resumed.chat.borrow());
        assert!(
            !boxes.is_empty(),
            "the demo spawns sub-agents, so the resume has boxes to project"
        );

        for (n, status, report, task) in &boxes {
            assert_eq!(
                *status,
                SubAgentStatus::Done,
                "resumed box Sub({n}) is Done (the projection closes the bracket)"
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
                !transcript.entries().is_empty(),
                "Sub({n})'s transcript is projected, not deferred"
            );
        }
        shut_down(&resumed).await;
    }

    /// Observing a resumed sub-agent switches the view to it and shows the
    /// same entry shape the eager replay path builds.
    #[tokio::test]
    async fn observing_a_resumed_subagent_matches_the_eager_replay() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "parallel-agents").await;
        drive_demo_to_completion(&mut world).await;
        let session_id = world.session().to_string();
        shut_down(&world).await;

        let mut resumed = resumed_world(&dir, "parallel-agents", &session_id).await;
        let shell = shell_for(&resumed);
        let n = first_sub(&resumed.chat.borrow());

        // What the eager path produces for Sub(n): full `replay` reduced into
        // a throwaway chat over the same log, with Sub(n) set active so its
        // tool cells reconcile to `header_only == false` exactly as the
        // attached world will once observe makes Sub(n) active. Comparing
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

        // The box report the attach block produced, captured before observe so
        // we can prove observe does not touch it.
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

        assert_eq!(
            resumed.chat.borrow().active_view(),
            AgentId::Sub(n),
            "observe switches the active view to the sub-agent"
        );

        // Let the host go before reading the model it left behind, so no
        // borrow spans the teardown await.
        shut_down(&resumed).await;
        let chat = resumed.chat.borrow();
        let transcript = chat
            .transcript(AgentId::Sub(n))
            .expect("Sub(n) transcript materialized");
        assert!(
            !transcript.entries().is_empty(),
            "the projected transcript has entries"
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
        // Sub(n). So the backfilled transcript equals what a full replay
        // would build.
        assert_eq!(
            transcript_shape(&chat, AgentId::Sub(n)),
            eager_shape,
            "the backfilled transcript matches the eager replay on kind, \
             header_only, finalized, and payload"
        );
        // Observe is a pure view switch: the report is unchanged, so it still
        // equals both its attach-time value and the eager replay's.
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

    /// Re-observing a sub-agent is a no-op: its transcript is unchanged.
    #[tokio::test]
    async fn re_observe_is_idempotent() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "parallel-agents").await;
        drive_demo_to_completion(&mut world).await;
        let session_id = world.session().to_string();
        shut_down(&world).await;

        let mut resumed = resumed_world(&dir, "parallel-agents", &session_id).await;
        let shell = shell_for(&resumed);
        let n = first_sub(&resumed.chat.borrow());

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

        let count_after_second = resumed
            .chat
            .borrow()
            .transcript(AgentId::Sub(n))
            .expect("still projected")
            .entries()
            .len();
        assert_eq!(
            count_after_first, count_after_second,
            "re-observe touches no transcript, so it is intact"
        );
        shut_down(&resumed).await;
    }

    /// Switching the active view away from a resumed sub-agent and back flips
    /// its tool cells' `header_only` flags exactly as the eager path leaves
    /// them with the sub active. This pins the reconcile in
    /// `set_active_view` for a backfilled transcript.
    #[tokio::test]
    async fn header_only_reconciles_across_view_switches() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "parallel-agents").await;
        drive_demo_to_completion(&mut world).await;
        let session_id = world.session().to_string();
        shut_down(&world).await;

        let mut resumed = resumed_world(&dir, "parallel-agents", &session_id).await;
        let shell = shell_for(&resumed);
        let n = first_sub(&resumed.chat.borrow());

        // Eager reference: Sub(n) active, so its tool cells are expanded.
        let eager_header_only = {
            let eager = eager_chat(&resumed, AgentId::Sub(n)).await;
            tool_header_only(&eager, AgentId::Sub(n))
        };
        assert!(
            !eager_header_only.is_empty(),
            "the sub-agent has tool cells to reconcile"
        );

        // Observe makes Sub(n) active, expanding its tool cells.
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
        shut_down(&resumed).await;
    }

    /// Switching from a session full of sub-agents to a fresh one replaces
    /// the transcripts wholesale, so no box or child thread of the outgoing
    /// session can leak into the new one's view.
    #[tokio::test]
    async fn a_session_switch_replaces_the_subagent_transcripts() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "parallel-agents", default_layers()).await;
        drive_demo_to_completion(&mut world).await;
        assert!(
            !sub_boxes(&world.chat.borrow()).is_empty(),
            "the demo spawned sub-agents to leave behind"
        );

        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved));

        shut_down(&world).await;
        let chat = world.chat.borrow();
        assert!(
            sub_boxes(&chat).is_empty(),
            "a fresh session shows none of the previous session's boxes"
        );
        assert!(
            chat.transcript(AgentId::Sub(1)).is_none(),
            "and none of its child transcripts"
        );
    }

    /// A session aborted mid sub-agent run (a torn final line and a log cut
    /// short) still resumes: the repair drops the torn record, every box is
    /// `Done`, and the torn sub's flushed history projects without panicking.
    ///
    // TODO(aljoscha): flaky under load, roughly 2 in 25 runs. Both sightings
    // were `Sub(2)` resuming `Failed` instead of `Done`, and both were whole
    // suite runs. It does not reproduce on its own (0 of 10) or over the
    // binary's own suite (0 of 6), so it needs the machine busy.
    //
    // The suspicion is a race in this setup rather than in the resume under
    // test: `drive_demo_to_completion` returns once the demo's observable
    // work is done, and under contention one of `parallel-agents`'
    // sub-agents can still record a failure before the truncation below
    // reads the log, so the box resumes from a `Failed` record that really
    // was written. If you hit it, dump the log bytes before truncating and
    // check whether the failure is already on disk. If it is, the fix is in
    // the wait, not in the repair.
    #[tokio::test]
    async fn aborted_session_resume_loads_and_observes() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "parallel-agents").await;
        drive_demo_to_completion(&mut world).await;
        let session_id = world.session().to_string();
        let log_path = {
            let log = world.handles().log.lock().await;
            log.path().to_path_buf()
        };
        // Let the host go so nothing holds the log open while it is rewritten
        // on disk, and so the resume below can take the session's lock.
        shut_down(&world).await;

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

        // The truncation cuts sub `torn_sub` mid run. Fail loudly rather than
        // silently skip the parity check if the truncation ever stops
        // covering a sub-agent.
        let n = torn_sub;
        assert!(
            sub_boxes(&resumed.chat.borrow())
                .iter()
                .any(|(m, ..)| *m == n),
            "the truncated log still holds the torn sub-agent Sub({n})"
        );

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

        // Report parity: the attach block's projection and the eager replay
        // agree on the box report. Sub(n) is tool-concluding here (its last
        // flushed entry is a tool result, its concluding assistant text was
        // torn off), so per spec both show an empty report (a thin box). This
        // is the "the report matches, per spec" guarantee.
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

        // The backfill projected the actual flushed history from the repaired
        // log: the transcript equals the eager resume's, entry for entry,
        // including tool args and `header_only`.
        assert_eq!(
            transcript_shape(&resumed.chat.borrow(), AgentId::Sub(n)),
            eager_shape,
            "the projected transcript equals the eager resume's flushed history"
        );

        // Observe is a pure view switch: it does not rewrite Sub(n)'s box
        // report. `parallel-agents` runs
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
        shut_down(&resumed).await;
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
        let session = world.session().to_string();
        shut_down(&world).await;
        let resumed = resumed_world(&dir, "streaming-text", &session).await;

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

    /// A world built from `argv`, so a launch-flag test can state the command
    /// line it is about. The store stays inside `dir`.
    async fn world_from_argv(dir: &TempDir, argv: &[&str]) -> Result<World> {
        let args = Args::parse_from(argv);
        let auth = AuthStorage::new(dir.path().join("auth.json"));
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        build_world(
            &args,
            layers_spilling_into(dir),
            &[],
            &auth,
            &persistence,
            None,
        )
        .await
    }

    /// This run's session store, for reading a sidecar back.
    fn store_in(dir: &TempDir) -> ConversationPersistence {
        ConversationPersistence::new(dir.path().join("sessions"))
    }

    /// `--tag` names the session a local run creates: the label lands on the
    /// created session, through the host's own create rather than beside it.
    #[tokio::test]
    async fn the_tag_flag_names_the_session_a_local_run_creates() {
        let dir = TempDir::new().expect("tempdir");
        let world = world_from_argv(
            &dir,
            &["aj", "--scripted", "streaming-text", "--tag", "fix-auth"],
        )
        .await
        .expect("build world");

        assert_eq!(
            store_in(&dir)
                .read_tag(world.session())
                .expect("read the sidecar"),
            Some("fix-auth".to_string()),
        );
        shut_down(&world).await;
    }

    /// An illegal label is refused before anything is minted, so the run
    /// reports on the normal screen and leaves no session behind.
    #[tokio::test]
    async fn an_illegal_launch_tag_refuses_the_run_and_creates_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let error = match world_from_argv(
            &dir,
            &["aj", "--scripted", "streaming-text", "--tag", "two\nlines"],
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("an illegal tag refuses the run"),
        };

        let message = format!("{error:#}");
        assert!(message.contains("--tag"), "names the flag: {message}");
        assert!(
            message.contains("single line"),
            "and says what is wrong with it: {message}",
        );
        assert_eq!(
            store_in(&dir)
                .get_latest_session_id()
                .expect("read the store"),
            None,
            "a refused run leaves no session behind",
        );
    }

    /// A resume has no created session for `--tag` to name, so the flag is
    /// reported rather than dropped.
    #[tokio::test]
    async fn a_launch_tag_on_a_resume_says_it_has_nothing_to_name() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;
        persist_session(&mut world).await;
        let session = world.session().to_string();
        shut_down(&world).await;

        let resumed = world_from_argv(
            &dir,
            &[
                "aj",
                "--scripted",
                "streaming-text",
                "--tag",
                "fix-auth",
                "continue",
                &session,
            ],
        )
        .await
        .expect("build the resumed world");
        assert!(
            main_notices(&resumed)
                .iter()
                .any(|n| n.contains("--tag has nothing to name")),
            "{:?}",
            main_notices(&resumed),
        );
        assert_eq!(
            store_in(&dir).read_tag(&session).expect("read the sidecar"),
            None,
            "and the resumed session is not relabelled",
        );
        shut_down(&resumed).await;
    }

    /// `fresh_env_notices` produces the fresh-session env block: a leading Info
    /// context listing followed by one warning per skill diagnostic for a
    /// fresh session, and nothing for a resume (whose prompt is fixed in its
    /// log). This is the shared unit both the cold-start and `/new` paths
    /// fold, so a skill problem introduced before a `/new` surfaces just as it
    /// does on a cold start.
    #[test]
    fn fresh_env_notices_carries_context_and_skill_warnings_for_create_only() {
        let env = AgentEnv {
            working_directory: std::path::PathBuf::from("/tmp/project"),
            git_root_directory: None,
            operating_system: "linux".to_string(),
            today_date: "2026-01-01".to_string(),
            system_prompt: aj_conf::SystemPrompt {
                content: "prompt".to_string(),
                source: aj_conf::SystemPromptSource::Builtin,
            },
            context_files: Vec::new(),
            skills: Vec::new(),
            skill_diagnostics: vec![aj_conf::skills::SkillDiagnostic {
                path: std::path::PathBuf::from("/tmp/project/.aj/skills/broken"),
                message: "missing description".to_string(),
            }],
        };

        let create = fresh_env_notices(true, &env);
        assert!(
            matches!(&create[0], AgentEvent::Notice { text, .. } if text.contains("Context:")),
            "the context listing leads as an Info notice: {create:?}"
        );
        assert!(
            create.iter().any(|e| matches!(
                e,
                AgentEvent::Warning { text, .. } if text.contains("missing description")
            )),
            "a skill diagnostic folds as a warning: {create:?}"
        );

        let resume = fresh_env_notices(false, &env);
        assert!(
            resume.is_empty(),
            "a resume folds no env notices: {resume:?}"
        );
    }

    /// A session change folds the fresh session's context as an Info notice: a
    /// created session carries the context string into its scrollback, a
    /// resume does not, and a refused change folds neither (it never left the
    /// session it was in).
    #[tokio::test]
    async fn session_switch_folds_context_for_fresh_only() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        // Persist the session so the resume path below has a log on disk.
        persist_session(&mut world).await;
        let resumable = world.session().to_string();
        // Counted, not merely looked for: switching back restores the parked
        // transcript, which already carries the context this session folded at
        // startup, so what a resume must not do is fold a second one.
        let contexts = |world: &World| {
            main_notices(world)
                .iter()
                .filter(|n| n.contains("Context:"))
                .count()
        };
        assert_eq!(contexts(&world), 1, "startup folded this session's context");

        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved));
        assert_eq!(
            contexts(&world),
            1,
            "a created session folds its context: {:?}",
            main_notices(&world),
        );

        // Back onto the persisted session: a resume's prompt is fixed in its
        // log, so nothing describes the env read now.
        let moved = apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Resume(resumable.to_string()),
        )
        .await;
        assert!(matches!(moved, Focus::Moved));
        assert_eq!(world.session(), resumable);
        assert_eq!(
            contexts(&world),
            1,
            "a resume folded a second context onto the transcript it restored: {:?}",
            main_notices(&world),
        );

        // A refused change stays in the session it was in, so it folds the
        // failure and no env block.
        let stayed = apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Resume("no-such-session".to_string()),
        )
        .await;
        assert!(matches!(stayed, Focus::Same));
        assert_eq!(world.session(), resumable, "the focus did not move");
        assert_eq!(
            contexts(&world),
            1,
            "the refusal folds no context: {:?}",
            main_notices(&world),
        );
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n.contains("Failed to switch to session no-such-session")),
            "the refusal says what failed: {:?}",
            main_notices(&world),
        );
        shut_down(&world).await;
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
            // Bind out of the borrow first, as the drive loop does: the
            // submit awaits on the host.
            let submitted = shell.borrow().take_submitted();
            if let Some(text) = submitted {
                shell.borrow().editor.borrow_mut().add_to_history(&text);
                handle_submit(&mut world, text).await;
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

        // Settle the turn so the teardown below is clean.
        cancel_viewed_turn(&world).await;
        settle(&mut world).await;
        shut_down(&world).await;
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
        let initial = chat.borrow().show_thinking_block;

        writer.write_all(b"\x1bt").expect("write alt+t");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(chat.borrow().show_thinking_block, !initial);

        writer.write_all(b"\x1bt").expect("write alt+t");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(chat.borrow().show_thinking_block, initial);
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
        stage_pending(&mut world, AgentId::Main, "queued").await;

        // Plain Up (CSI A) parks Dequeue in the capture phase without touching
        // the editor, then the drive-loop handler performs the yank.
        press(b"\x1b[A").await;
        assert_eq!(shell.borrow().take_host_action(), Some(AjAction::Dequeue));
        assert_eq!(
            shell.borrow().editor.borrow().cursor(),
            (0, 0),
            "the recall chord never reached the editor",
        );
        assert!(handle_host_action(&mut world, &shell, AjAction::Dequeue).await);
        assert_eq!(shell.borrow().editor.borrow().text(), "queued");
        assert!(!world.handles().queues.has_pending(AgentId::Main));

        // Ctrl+P (0x10) does the same. Re-queue and clear the editor first.
        stage_pending(&mut world, AgentId::Main, "again").await;
        shell.borrow().editor.borrow_mut().clear();
        press(&[0x10]).await;
        assert_eq!(shell.borrow().take_host_action(), Some(AjAction::Dequeue));
        assert!(handle_host_action(&mut world, &shell, AjAction::Dequeue).await);
        assert_eq!(shell.borrow().editor.borrow().text(), "again");
        shut_down(&world).await;
    }

    /// With a draft in the editor, plain Up does NOT recall: the stricter gate
    /// declines, so the key falls through to the editor and the pending message
    /// stays queued (mirroring `aj`).
    #[tokio::test]
    async fn up_does_not_recall_with_a_draft_in_the_editor() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, mut writer, mut world, shell, _root) =
            init_app_with_world(&dir, "streaming-text").await;

        stage_pending(&mut world, AgentId::Main, "queued").await;
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
            world.handles().queues.has_pending(AgentId::Main),
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
    /// loop's `fold_selection_copied_record`) and shows in `Shell::draw`; the
    /// same record folds only once.
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

        shell.borrow().selection_copied.set(Some(SelectionCopied {
            chars: 7,
            at: Instant::now(),
        }));
        let mut seen = None;
        assert!(
            fold_selection_copied_record(&shell.borrow(), &mut seen),
            "a fresh record pushes a toast"
        );
        assert!(
            !fold_selection_copied_record(&shell.borrow(), &mut seen),
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

    /// The drive loop seeds `selection_copied_seen` from the restored record's
    /// `at` before its first iteration, so a record carried over from a
    /// previous session folds NO toast. A fresh record (a new `at`) still
    /// toasts once.
    #[test]
    fn preseeded_copy_record_does_not_retoast() {
        let shell = test_shell_with_chat(empty_chat());
        let at = Instant::now();
        shell
            .borrow()
            .selection_copied
            .set(Some(SelectionCopied { chars: 7, at }));

        // The loop-start seed: `seen` already holds the record's timestamp.
        let mut seen = Some(at);
        assert!(
            !fold_selection_copied_record(&shell.borrow(), &mut seen),
            "the seeded record folds no toast"
        );
        assert!(
            crate::toasts::toast_texts(&shell.borrow().toasts).is_empty(),
            "no toast for a previous session's copy"
        );

        shell.borrow().selection_copied.set(Some(SelectionCopied {
            chars: 9,
            at: at + std::time::Duration::from_millis(1),
        }));
        assert!(
            fold_selection_copied_record(&shell.borrow(), &mut seen),
            "a fresh record still toasts"
        );
        assert_eq!(
            crate::toasts::toast_texts(&shell.borrow().toasts).len(),
            1,
            "exactly one toast for the fresh record"
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

        handle_submit(&mut world, "go".to_string()).await;
        fold_ready_frames(&mut world);
        assert_eq!(
            quit_arm_running_work(&world).as_deref(),
            Some("1 agent still running")
        );

        cancel_viewed_turn(&world).await;
        settle(&mut world).await;
        assert_eq!(quit_arm_running_work(&world), None);
        shut_down(&world).await;
    }

    /// End-to-end over the real session path: submit a prompt into a
    /// scripted session, fold the frames the host publishes for it, and check
    /// the chat model holds the user prompt plus a finalized assistant reply.
    /// A full transcript render over the result must not panic.
    #[tokio::test]
    async fn scripted_prompt_streams_into_the_chat_model() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        handle_submit(&mut world, "hi there".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "the host accepted the prompt");

        // The frame arm, until the turn is over.
        settle(&mut world).await;
        assert!(!world.client().working(), "no turn left in flight");

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
            Rc::new(std::cell::RefCell::new(None)),
            Rc::new(std::cell::Cell::new(None)),
            Rc::new(std::cell::RefCell::new(
                crate::image_store::ImageStore::default(),
            )),
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
        shut_down(&world).await;
    }

    fn lifecycle_running(world: &World) -> bool {
        world.client().lifecycle().is_running(AgentId::Main)
    }

    /// A non-empty launch prompt runs a Main turn, so the initial session
    /// drives it without the user typing anything.
    #[tokio::test]
    async fn launch_prompt_spawns_a_main_turn() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        auto_submit_launch(&mut world, vec![UserContent::text("launch me")]).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "the launch prompt started a turn");

        settle(&mut world).await;
        let prompts: Vec<String> = world
            .chat
            .borrow()
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::User(u) => Some(u.joined_text()),
                _ => None,
            })
            .collect();
        assert_eq!(prompts, vec!["launch me"], "and it ran as a user prompt");
        shut_down(&world).await;
    }

    /// An empty launch prompt (no positionals, no `@file`) spawns nothing,
    /// so a bare `aj` starts on the idle splash.
    #[tokio::test]
    async fn empty_launch_prompt_spawns_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        auto_submit_launch(&mut world, Vec::new()).await;
        fold_ready_frames(&mut world);
        assert!(!world.client().working());
        assert!(!world.chat.borrow().has_conversation());
        shut_down(&world).await;
    }

    // ---- The host seam ----

    /// The text blocks of a wire user message, concatenated.
    fn user_text(message: &aj_models::types::UserMessage) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                UserContent::Text(text) => Some(text.text.as_str()),
                UserContent::Image(_) => None,
            })
            .collect()
    }

    /// The user rows of the Main transcript, in order.
    fn user_rows(world: &World) -> Vec<String> {
        world
            .chat
            .borrow()
            .transcript(AgentId::Main)
            .map(|transcript| {
                transcript
                    .entries()
                    .iter()
                    .filter_map(|entry| match &entry.kind {
                        EntryKind::User(user) => Some(user.joined_text()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A submit reaches the host, and the frames the host publishes for it are
    /// what build the transcript: the submit itself renders nothing, and the
    /// message is on the session's log before any frame is folded.
    #[tokio::test]
    async fn a_prompt_reaches_the_host_and_its_frames_build_the_transcript() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;

        handle_submit(&mut world, "hi there".to_string()).await;
        assert!(
            user_rows(&world).is_empty(),
            "the submit writes no row of its own: {:?}",
            user_rows(&world),
        );

        settle(&mut world).await;
        assert_eq!(
            user_rows(&world),
            vec!["hi there"],
            "the row arrived on the frame stream",
        );
        // And it went through the host, not around it: the session's log
        // carries the message the turn ran.
        let logged = {
            let log = world.handles().log.lock().await;
            let head = log.latest_leaf(ThreadFilter::USER).expect("a head");
            log.linearize(&head, ThreadFilter::USER)
                .agent_messages()
                .iter()
                .filter_map(|message| match message.as_stored_wire() {
                    Some(aj_models::types::Message::User(user)) => Some(user_text(user)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert!(
            logged.iter().any(|text| text == "hi there"),
            "the host persisted the prompt: {logged:?}",
        );
        shut_down(&world).await;
    }

    /// `/compact` while a turn runs is refused by the host, and the refusal
    /// keeps the local wording (which names the chord that cancels the turn
    /// first) rather than the protocol's. While idle it runs.
    #[tokio::test]
    async fn compact_is_refused_while_busy_and_runs_when_idle() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        install_busy_script(world.handles());
        handle_submit(&mut world, "go".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "the session is busy");

        apply_command(&mut world, &shell, CommandAction::Compact).await;
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| *n == session_busy_notice("compact")),
            "the busy refusal keeps its wording: {:?}",
            main_notices(&world),
        );

        cancel_viewed_turn(&world).await;
        settle(&mut world).await;
        let before = main_notices(&world).len();
        apply_command(&mut world, &shell, CommandAction::Compact).await;
        settle(&mut world).await;
        let folded = main_notices(&world)[before..].to_vec();
        assert!(
            !folded.iter().any(|n| *n == session_busy_notice("compact")),
            "an idle compact is not refused: {folded:?}",
        );
        // The compaction ran as a turn and reported what it found, which is
        // only something the host's compaction path can say.
        assert!(
            folded.iter().any(|n| n.starts_with("Nothing to compact")),
            "the compaction turn ran: {folded:?}",
        );
        shut_down(&world).await;
    }

    /// A turn's fatal error belongs to its session, not to this client (spec
    /// section 5): it shows up as an error row and the session stays usable,
    /// where the pre-host drive loop ended the session on it.
    ///
    /// The fault is a log that cannot be opened: the first punctuating append
    /// creates the file with `create_new`, so a path that is already taken
    /// makes that append fail inside the inline persistence listener, which is
    /// exactly a fatal turn error.
    #[tokio::test]
    async fn a_fatal_turn_error_shows_an_error_and_keeps_the_session_usable() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "streaming-text").await;
        let log_path = dir
            .path()
            .join("sessions")
            .join(format!("{}.jsonl", world.session()));
        std::fs::write(&log_path, "").expect("take the path the log wants");

        handle_submit(&mut world, "hi".to_string()).await;
        settle(&mut world).await;
        let errors: Vec<String> = world
            .chat
            .borrow()
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Notice(notice) if notice.level == NoticeLevel::Error => {
                    Some(notice.text.clone())
                }
                _ => None,
            })
            .collect();
        assert!(
            errors.iter().any(|text| text.starts_with("IO error")),
            "the failed append surfaced as an error row: {errors:?}",
        );

        // Still usable: once the fault clears, the next prompt runs a turn.
        std::fs::remove_file(&log_path).expect("clear the fault");
        install_busy_script(world.handles());
        handle_submit(&mut world, "again".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "the session survived the error");
        settle(&mut world).await;
        assert!(
            user_rows(&world).iter().any(|text| text == "again"),
            "and the second prompt ran: {:?}",
            user_rows(&world),
        );
        shut_down(&world).await;
    }

    /// A `reset` frame (a head switch this client did not make) leaves the
    /// client owing a re-attach, and the re-attach rebuilds the transcript
    /// under the new epoch without duplicating what it already had.
    ///
    /// The drive loop discharges the same obligation at the bottom of each
    /// iteration. Nothing in phase 1 moves a head behind this client's back,
    /// so the command below stands in for the second writer that will.
    #[tokio::test]
    async fn a_reset_is_discharged_by_a_re_attach() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        let before = user_rows(&world);
        assert_eq!(before, vec!["seed"]);

        let head = world
            .handles()
            .log
            .lock()
            .await
            .head()
            .cloned()
            .expect("a persisted head");
        world
            .host()
            .command(
                world.session(),
                Command::Head {
                    target: HeadTarget::Entry(head),
                },
            )
            .await
            .expect("the head switch is accepted");
        fold_ready_frames(&mut world);
        assert!(
            world.client().needs_reattach(),
            "the reset frame left the client owing a re-attach",
        );

        reattach(&mut world, &shell).await.expect("re-attach");

        assert!(
            !world.client().needs_reattach(),
            "asking for the attach discharged it",
        );
        assert_eq!(
            user_rows(&world),
            before,
            "the new epoch's backfill rebuilt the transcript rather than \
             doubling it",
        );
        shut_down(&world).await;
    }

    /// A re-attach the host refuses is retried on a backoff, not once per loop
    /// iteration, and a refusal never ends the shell.
    ///
    /// The obligation stands until an attach is served, and the loop reaches
    /// this block every time around, so an unpaced retry folds a warning row
    /// per iteration and re-asks a host that is refusing at redraw speed.
    ///
    /// It also arrives on a stream that is still live, so an in-process refusal
    /// says the session moved rather than that the host is gone: it stays here on
    /// the backoff instead of arming the resume machinery, whose failed local
    /// open is the shell's exit
    /// ([`a_local_re_attach_after_the_host_is_gone_is_fatal`]).
    #[tokio::test]
    async fn a_refused_re_attach_is_paced() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;

        let head = world
            .handles()
            .log
            .lock()
            .await
            .head()
            .cloned()
            .expect("a persisted head");
        world
            .host()
            .command(
                world.session(),
                Command::Head {
                    target: HeadTarget::Entry(head),
                },
            )
            .await
            .expect("the head switch is accepted");
        fold_ready_frames(&mut world);
        assert!(
            world.client().needs_reattach(),
            "the reset left an obligation"
        );

        // Point the world at a session the host does not have, which is what a
        // refused attach looks like from here.
        let live_session = world
            .directory
            .rename_focused("no-such-session".to_string());
        let mut retry = Retry::default();

        assert!(
            discharge_reattach(&mut world, &shell, &mut retry)
                .await
                .is_none(),
            "an in-process host's refusal stays here rather than arming a \
             reconnect",
        );
        let warned = main_notices(&world)
            .iter()
            .filter(|text| text.contains("Lost the session's event stream"))
            .count();
        assert_eq!(warned, 1, "the refusal is reported once");
        assert!(
            world.client().needs_reattach(),
            "and the obligation still stands",
        );
        assert!(!retry.ready(), "the next attempt is held back");
        let due = retry.due().expect("a paced retry has a due time");

        // The loop's next iterations find it not ready, so nothing is re-asked
        // and no second row is folded.
        assert!(!retry.ready());
        assert_eq!(
            main_notices(&world)
                .iter()
                .filter(|text| text.contains("Lost the session's event stream"))
                .count(),
            warned,
        );

        // Once the delay is out it does try again, and the delay grows.
        tokio::time::sleep_until(due.into()).await;
        assert!(retry.ready(), "the retry is not abandoned, only paced");
        assert!(
            discharge_reattach(&mut world, &shell, &mut retry)
                .await
                .is_none()
        );
        let grown = retry.due().expect("a second failure paces again");
        assert!(
            grown.saturating_duration_since(Instant::now())
                > due.saturating_duration_since(Instant::now()),
            "a repeated refusal did not back off further",
        );

        world.directory.rename_focused(live_session);
        shut_down(&world).await;
    }

    /// Overflow the focused session's in-process subscription until the host's
    /// reliable fan-out evicts it, which is how a local stream ends with the
    /// host still there (spec 6.9).
    ///
    /// The overflow is real: every queue command publishes a reliable
    /// `QueueUpdate`, and nothing drains the stream while they run. The stream
    /// keeps reporting `Closed` afterwards, so consuming the first one here
    /// leaves it for the drive loop's frame arm too.
    async fn evict_local_stream(world: &mut World) {
        // Past the fan-out's per-client bound, with nothing reading.
        for _ in 0..300 {
            world
                .control
                .command(
                    world.session(),
                    Command::Queue(QueueOp::Remove {
                        agent: AgentId::Main,
                    }),
                )
                .await
                .expect("a queue withdrawal is always accepted");
        }
        assert!(
            matches!(world.stream.recv().await, ControlFrame::Closed),
            "the overflow evicted this subscriber",
        );
    }

    /// An in-process subscriber is evicted like any other when the shell stops
    /// draining and the host's reliable fan-out overflows (spec 6.9), so a
    /// closed local stream is a re-attach, not the end of the shell.
    ///
    /// This covers the step the loop drives; that the loop routes a local
    /// `Closed` here at all is
    /// [`the_loop_re_attaches_an_evicted_local_stream`].
    #[tokio::test]
    async fn an_evicted_local_stream_is_re_attached() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        assert_eq!(user_rows(&world), vec!["seed"]);
        let before = CanonicalState::of(&world.chat.borrow(), world.client());

        evict_local_stream(&mut world).await;

        // What the drive loop does with it: re-attach with the cursor, exactly
        // as a connection would.
        let mut resume = Some(Resume::lost());
        let deadline = Instant::now() + SETTLE_DEADLINE;
        while let Some(state) = resume.take() {
            assert!(Instant::now() < deadline, "the re-attach never settled");
            resume = advance_resume(&mut world, &shell, state)
                .await
                .expect("the host is still there, so the attach is served");
        }
        assert!(
            main_notices(&world)
                .iter()
                .any(|text| text == "Re-attached to the session."),
            "the re-attach is surfaced: {:?}",
            main_notices(&world),
        );
        // Every row the host published, not just the prompts: the backfill
        // re-serves the durable entries above the client's cursor, and a
        // doubled assistant row is exactly as wrong as a doubled prompt.
        assert_eq!(
            host_entries(&CanonicalState::of(&world.chat.borrow(), world.client())),
            host_entries(&before),
            "the backfill rebuilt the transcript rather than doubling it",
        );

        // And the shell is a live client again.
        run_prompt(&mut world, "again").await;
        assert_eq!(user_rows(&world), vec!["seed", "again"]);
        shut_down(&world).await;
    }

    /// The host really being gone still ends the shell: the re-attach is what
    /// tells that apart from an eviction, because a host that is gone refuses
    /// it.
    ///
    /// This is the path a stream that *ended* takes. A re-attach owed while the
    /// stream is still live never reaches it, see
    /// [`a_refused_re_attach_is_paced`].
    #[tokio::test]
    async fn a_local_re_attach_after_the_host_is_gone_is_fatal() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        shut_down(&world).await;

        assert!(
            matches!(world.stream.recv().await, ControlFrame::Closed),
            "the shutdown closed the stream",
        );
        assert!(
            advance_resume(&mut world, &shell, Resume::lost())
                .await
                .is_err(),
            "a local host that cannot serve its own session is gone",
        );
    }

    /// A re-attach that keeps dying inside its attach block backs off like a
    /// failed open does.
    ///
    /// Each attempt costs the host a full projection and is served the
    /// identical suffix (a client's cursor does not move until the block
    /// completes, spec 6.5), so a flat retry is a livelock on a busy host whose
    /// fan-out keeps evicting a client that is still attaching.
    #[test]
    fn a_failure_inside_the_attach_block_backs_off() {
        let mut state = Resume::lost();
        assert!(state.ready(), "the first attempt is due at once");
        assert_eq!(state.retry.delay, RETRY_BACKOFF_MIN);

        // What the next failure holds the following attempt back by, tracked
        // alongside the state so each iteration compares against the doubling
        // this one owes rather than against the previous reading. The delay is
        // the assertion's subject on purpose: a due time minus a fresh
        // `Instant::now()` differs from the recorded delay by the microseconds
        // between two clock reads, which is the same whether the delay grew or
        // stood still.
        let mut applied = RETRY_BACKOFF_MIN;
        for attempt in 1..=4 {
            // The stream opened, then died inside the block.
            state.step = ResumeStep::CatchingUp;
            let at = Instant::now();
            state.failed();
            assert!(!state.ready(), "attempt {attempt} left nothing holding it");
            assert!(
                state.due().saturating_duration_since(at) >= applied,
                "attempt {attempt} came due inside its own {applied:?} delay",
            );
            applied = (applied * 2).min(RETRY_BACKOFF_MAX);
            assert_eq!(
                state.retry.delay, applied,
                "a repeated failure inside the block did not back off further",
            );
        }

        for _ in 0..10 {
            state.failed();
        }
        assert_eq!(
            state.retry.delay, RETRY_BACKOFF_MAX,
            "the backoff is capped",
        );
        assert!(
            state.due().saturating_duration_since(Instant::now()) <= RETRY_BACKOFF_MAX,
            "the backoff is capped",
        );
    }

    /// Run the real drive loop over `world` until `stop` resolves, answering how
    /// the loop exited and what `stop` observed.
    ///
    /// Recovery and pacing have two halves, and calling `advance_resume`,
    /// `discharge_reattach` or `refresh_client_reads` directly reaches only one:
    /// the other is the loop's gate on when each may run and the wake deadline
    /// that brings the loop back for one. The tests below drive an otherwise
    /// quiet session, so an attempt that happens at all is one the merged
    /// deadline woke the loop for, and its timing is what the gate let through.
    ///
    /// `stop` is handed the loop's input pipe and ends the loop by dropping it:
    /// the reader sees EOF, which the input arm quits on. It runs on this task
    /// alongside the loop, so it observes through the shared chat model and must
    /// not hold a borrow of it across an await.
    async fn drive_until<F, Fut, T>(
        world: &mut World,
        shell: &Rc<RefCell<Shell>>,
        stop: F,
    ) -> (Result<SessionExit>, T)
    where
        F: FnOnce(PipeWriter) -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let (mut app, writer, root) = app_over(shell).await;
        // No watcher: nothing here writes a theme file, and an inert watch is
        // what a bundled palette runs with anyway.
        let mut theme_watch = ThemeWatch {
            _guard: None,
            rx: None,
        };
        let mut prompt_history_rx = None;
        let mut autocomplete_rx = shell
            .borrow()
            .editor
            .borrow_mut()
            .take_autocomplete_rx()
            .expect("editor hands out its autocomplete receiver exactly once");
        tokio::join!(
            drive(
                &mut app,
                &root,
                shell,
                world,
                &mut theme_watch,
                &mut prompt_history_rx,
                &mut autocomplete_rx,
            ),
            stop(writer),
        )
    }

    /// Poll `observed` until it answers `Some`, bounded by [`SETTLE_DEADLINE`] so
    /// a loop that never gets there fails a test instead of hanging it.
    async fn poll_for<T>(mut observed: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + SETTLE_DEADLINE;
        while Instant::now() < deadline {
            if let Some(value) = observed() {
                return Some(value);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        None
    }

    /// How many refused re-attaches a chat model was told about, one warning row
    /// per attempt.
    fn refusals(chat: &ChatState) -> usize {
        notices_of(chat)
            .iter()
            .filter(|text| text.contains("Lost the session's event stream"))
            .count()
    }

    /// A world that owes a re-attach no attach will ever discharge, plus a Shell
    /// over it.
    ///
    /// The obligation comes from a head switch this client did not make, and the
    /// session id is then pointed at one the host does not have, which is what a
    /// permanently refused attach looks like from here. Nothing else is left for
    /// the drive loop to do: the session is idle, its frames are drained and the
    /// reads the startup attach obliged are discharged, so the loop has to wake
    /// itself for the retry.
    async fn a_world_owing_a_refused_re_attach(
        dir: &TempDir,
    ) -> (World, Rc<RefCell<Shell>>, String) {
        let (mut world, shell) = world_and_shell(dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;

        let head = world
            .handles()
            .log
            .lock()
            .await
            .head()
            .cloned()
            .expect("a persisted head");
        world
            .host()
            .command(
                world.session(),
                Command::Head {
                    target: HeadTarget::Entry(head),
                },
            )
            .await
            .expect("the head switch is accepted");
        // Wait the host's `list` coalescing out (spec 6.8) before draining, so
        // no frame of the switch is still to come.
        tokio::time::sleep(Duration::from_millis(400)).await;
        fold_ready_frames(&mut world);
        refresh_client_reads(&mut world).await;
        assert!(
            world.client().needs_reattach(),
            "the reset left an obligation",
        );
        assert!(
            !owes_client_reads(&world),
            "and no read is owed alongside it",
        );

        let live_session = world
            .directory
            .rename_focused("no-such-session".to_string());
        (world, shell, live_session)
    }

    /// The focused session's durable high-water mark, as the host reports it.
    ///
    /// The directory is the test's window onto the host's own bookkeeping. A
    /// client may not turn a position it read there into a cursor (spec 6.5),
    /// and nothing here does.
    async fn host_mark(world: &World) -> u64 {
        world
            .control
            .sessions()
            .await
            .expect("session list")
            .sessions
            .iter()
            .find(|entry| entry.id == world.session())
            .expect("the focused session is in the host's directory")
            .last_seq
            .expect("a live session's row reports its position")
    }

    /// The loop routes a closed local stream into the same recovery a lost
    /// connection takes, instead of ending the shell over it.
    ///
    /// The frame arm is the only place that is decided, so a test that hands
    /// `advance_resume` a `Resume` of its own never reaches it.
    #[tokio::test]
    async fn the_loop_re_attaches_an_evicted_local_stream() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        let before = CanonicalState::of(&world.chat.borrow(), world.client());
        // The comparison below is not vacuous: the client's cursor sits under
        // the host's durable mark, so the re-attach really re-serves an entry
        // the fold has to recognize rather than append again.
        let cursor = world
            .client()
            .cursor()
            .expect("the startup attach committed a cursor")
            .seq;
        let mark = host_mark(&world).await;
        assert!(
            cursor < mark,
            "the cursor is already at the host's mark ({cursor} of {mark})",
        );

        evict_local_stream(&mut world).await;

        let chat = Rc::clone(&world.chat);
        let (exit, reattached) = drive_until(&mut world, &shell, |writer| async move {
            let reattached = poll_for(|| {
                notices_of(&chat.borrow())
                    .iter()
                    .any(|text| text == "Re-attached to the session.")
                    .then_some(())
            })
            .await;
            drop(writer);
            reattached
        })
        .await;

        match exit {
            Ok(SessionExit::Quit) => {}
            Ok(_) => panic!("the loop left the session over an evicted stream"),
            Err(err) => panic!("an evicted local stream ended the shell: {err}"),
        }
        assert!(
            reattached.is_some(),
            "the loop never re-attached: {:?}",
            main_notices(&world),
        );
        assert_eq!(
            host_entries(&CanonicalState::of(&world.chat.borrow(), world.client())),
            host_entries(&before),
            "the loop's re-attach rebuilt the transcript rather than doubling it",
        );
        shut_down(&world).await;
    }

    /// The loop comes back for a refused re-attach with nothing else going on.
    ///
    /// With the obligation standing and the session quiet, the loop parks on a
    /// frame stream that will not speak again, so the retry happens only if the
    /// due time reached the loop's wake deadline. Without it the first refusal is
    /// also the last, and the transcript stays frozen for good (every later frame
    /// carries an epoch the fold filters out).
    #[tokio::test]
    async fn the_loop_wakes_for_a_refused_re_attach() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, live_session) = a_world_owing_a_refused_re_attach(&dir).await;

        let chat = Rc::clone(&world.chat);
        let (exit, retried) = drive_until(&mut world, &shell, |writer| async move {
            let retried = poll_for(|| (refusals(&chat.borrow()) >= 2).then_some(())).await;
            drop(writer);
            retried
        })
        .await;

        assert!(
            matches!(exit, Ok(SessionExit::Quit)),
            "a refused local re-attach is not the shell's exit",
        );
        assert!(
            retried.is_some(),
            "the loop stopped retrying after {} attempt(s), so it never woke for \
             the next one",
            refusals(&world.chat.borrow()),
        );
        assert!(
            world.client().needs_reattach(),
            "the obligation still stands, so every iteration had one to discharge",
        );
        world.directory.rename_focused(live_session);
        shut_down(&world).await;
    }

    /// How many paced attempts a backoff from [`RETRY_BACKOFF_MIN`] allows inside
    /// `window`: the first is due at once, and each failure doubles the delay up
    /// to [`RETRY_BACKOFF_MAX`].
    ///
    /// An attempt due exactly at the window's end counts, so this is an upper
    /// bound on what a correctly paced loop can get through.
    fn allowed_attempts(window: Duration) -> usize {
        let mut due = Duration::ZERO;
        let mut delay = RETRY_BACKOFF_MIN;
        let mut attempts = 0;
        while due <= window {
            attempts += 1;
            due += delay;
            delay = (delay * 2).min(RETRY_BACKOFF_MAX);
        }
        attempts
    }

    /// A refused re-attach stays held back while the loop is busy with other
    /// things.
    ///
    /// The loop reaches the discharge block once per iteration whatever woke it,
    /// so without the gate a peer that keeps refusing turns into one attach (and
    /// one warning row) per keystroke. The keystrokes here are that wake source:
    /// the pacing has to survive them.
    ///
    /// The bound is computed from the window that actually elapsed, so a machine
    /// that stretches the typing out earns the attempts the backoff owes it and
    /// nothing more.
    #[tokio::test]
    async fn the_loop_holds_a_refused_re_attach_back_while_it_iterates() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, live_session) = a_world_owing_a_refused_re_attach(&dir).await;

        let wakes = 120;
        let spacing = Duration::from_millis(5);
        let started = Instant::now();
        let (exit, ()) = drive_until(&mut world, &shell, |mut writer| async move {
            for _ in 0..wakes {
                writer.write_all(b"x").expect("write a key byte");
                tokio::time::sleep(spacing).await;
            }
            drop(writer);
        })
        .await;
        let window = started.elapsed();

        assert!(matches!(exit, Ok(SessionExit::Quit)));
        let refused = refusals(&world.chat.borrow());
        let allowed = allowed_attempts(window);
        assert!(
            refused <= allowed,
            "{refused} refused re-attaches over {wakes} wakes {spacing:?} apart, \
             where a {window:?} window allows {allowed}: the loop re-asked on \
             iterations rather than on the backoff",
        );
        assert!(
            refused >= 1,
            "the loop never attempted the re-attach it owed",
        );
        world.directory.rename_focused(live_session);
        shut_down(&world).await;
    }

    /// The loop's half of pacing the reads an attach block obliges: with nothing
    /// else going on, a retry only happens if the loop wakes itself for it.
    ///
    /// The recorded delay is the observable: it doubles once per failed attempt,
    /// so what it holds when the loop stops says how many attempts the loop got
    /// around to.
    #[tokio::test]
    async fn the_loop_paces_the_reads_an_attach_obliged() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;

        // Attach without discharging the reads, which is the state every
        // `caught_up` leaves the client in (spec 6.7).
        world.stream = open_stream(&world.control, &mut world.directory)
            .await
            .expect("re-attach");
        assert!(fold_attach_block(&mut world).await, "the block completed");
        assert!(owes_client_reads(&world), "the block obliged both reads");
        assert!(
            !world.client().needs_reattach(),
            "and nothing else is owed alongside them",
        );
        // Wait the host's `list` coalescing out (spec 6.8) and drain, so the
        // stream the loop parks on has nothing left to say. A frame would wake
        // the loop for free and the retry would ride along on it.
        tokio::time::sleep(Duration::from_millis(400)).await;
        fold_ready_frames(&mut world);

        // Point the world at a session the host does not have, so every read is
        // refused.
        world
            .directory
            .rename_focused("no-such-session".to_string());

        // Room for well over the first three attempts (at once, then 200ms, then
        // a further 400ms), so a busy machine still gets through them.
        let window = RETRY_BACKOFF_MIN * 15;
        let (exit, ()) = drive_until(&mut world, &shell, |writer| async move {
            tokio::time::sleep(window).await;
            drop(writer);
        })
        .await;

        assert!(matches!(exit, Ok(SessionExit::Quit)));
        assert!(
            owes_client_reads(&world),
            "a failed read keeps the obligation",
        );
        // Three attempts leave `RETRY_BACKOFF_MIN << 3`. Anything less is a loop
        // that stopped coming back for the retry.
        assert!(
            world.reads_retry.delay >= RETRY_BACKOFF_MIN * 8,
            "the reads paced out to {:?} over a {window:?} window, so the loop \
             woke for fewer than three attempts",
            world.reads_retry.delay,
        );
        shut_down(&world).await;
    }

    /// An attach block obliges the task and queue reads (neither is
    /// replayable), and the loop discharges them.
    ///
    /// The local views read the live handles, so nothing on screen depends on
    /// this. The client's own model does, and it is the fold connect mode
    /// uses, so leaving it stale would leave the two paths unequal.
    #[tokio::test]
    async fn an_attach_discharges_the_task_and_queue_reads() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        let task = register_bash_task(&mut world, "sleep 100").await;
        world
            .handles()
            .queues
            .append_follow_up(AgentId::Main, "queued");

        // Re-attach the focused session, the way the branch path does.
        reattach(&mut world, &shell).await.expect("re-attach");

        assert!(
            !world.client().needs_task_refetch(),
            "the task read is discharged",
        );
        assert!(
            !world.client().needs_queue_refetch(),
            "and the queue read too",
        );
        {
            let chat = world.chat.borrow();
            assert_eq!(
                chat.tasks().keys().copied().collect::<Vec<_>>(),
                vec![task],
                "the task table came off the read",
            );
            assert_eq!(chat.queue().queues.len(), 1);
            assert_eq!(
                chat.queue().queues[0]
                    .follow_up
                    .iter()
                    .filter_map(|message| match message.as_stored_wire() {
                        Some(aj_models::types::Message::User(user)) => Some(user_text(user)),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                vec!["queued"],
                "and so did the queue snapshot",
            );
        }
        shut_down(&world).await;
    }

    /// End-to-end over the `background-task` demo: the launch turn
    /// spawns a real background bash task, its completion triggers a
    /// wake turn, and the wake delivers the collapsible
    /// task-notification plus the wrap-up response.
    #[tokio::test]
    async fn background_task_completion_wakes_the_agent() {
        let dir = TempDir::new().expect("tempdir");
        let mut world = scripted_world(&dir, "background-task").await;

        handle_submit(&mut world, "run it".to_string()).await;
        // The host spawns the wake turn itself, either at the prompt turn's
        // join (the task finished while it still streamed, so its notice was
        // already queued) or off the `TaskEnd` it publishes. Settling covers
        // both: it waits for the session to report no turn and no live task.
        settle(&mut world).await;
        // Read the model the host left behind after the teardown, so no borrow
        // spans an await.
        shut_down(&world).await;

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
        // The completion notice arrived as a typed task-notification
        // entry (not a user prompt), so navigation and export branch on
        // it.
        assert!(
            entries
                .iter()
                .any(|e| matches!(&e.kind, EntryKind::TaskNotification(_))),
            "task-notification entry present",
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

        handle_submit(&mut world, "first".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working());

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
        let deadline = Instant::now() + SETTLE_DEADLINE;
        while !saw_prompt(&world) {
            assert!(Instant::now() < deadline, "the prompt never landed");
            let received = tokio::time::timeout(SETTLE_DEADLINE, world.stream.recv())
                .await
                .expect("a frame arrives before the timeout");
            let ControlFrame::Frame(frame) = received else {
                panic!("the stream is open");
            };
            let _ = world.directory.apply(&mut world.chat.borrow_mut(), frame);
        }

        handle_submit(&mut world, "second".to_string()).await;
        let snapshot = world.handles().queues.snapshot(AgentId::Main);
        assert_eq!(
            snapshot.kind,
            Some(aj_agent::queue::PendingKind::FollowUp),
            "busy submit queues instead of spawning",
        );
        assert_eq!(snapshot.text, "second");
        fold_ready_frames(&mut world);
        assert_eq!(
            world.chat.borrow().queue().queues.len(),
            1,
            "and the client learns about it from the queue frame the host \
             publishes on the enqueue side",
        );

        // The host's post-turn wake consumes the queue and delivers the
        // message.
        settle(&mut world).await;
        assert!(
            world
                .handles()
                .queues
                .snapshot(AgentId::Main)
                .kind
                .is_none(),
            "queue drained by the wake",
        );
        shut_down(&world).await;
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

        assert!(
            !cancel_viewed_turn(&world).await,
            "idle: fall through to quit"
        );

        handle_submit(&mut world, "go".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(
            cancel_viewed_turn(&world).await,
            "running turn is cancelled"
        );

        // The cancelled turn surfaces Aborted, which the host publishes as
        // the same notice, and the session stays alive.
        settle(&mut world).await;
        shut_down(&world).await;
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
            Some(world.handles().task_registry.clone()),
            ThemeHandle::new(Theme::bundled_dark_with_mode(
                aj_app::theme::ColorMode::Truecolor,
            )),
            "aj".to_string(),
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
            Some(world.handles().task_registry.clone()),
            ThemeHandle::new(Theme::bundled_dark_with_mode(
                aj_app::theme::ColorMode::Truecolor,
            )),
            "aj".to_string(),
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

        handle_submit(&mut world, "first".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "busy");

        // Busy + editor text: queue as steering, clear the editor.
        shell
            .borrow()
            .editor
            .borrow_mut()
            .insert_at_cursor("steer this");
        assert!(handle_host_action(&mut world, &shell, AjAction::Steer).await);
        let snapshot = world.handles().queues.snapshot(AgentId::Main);
        assert_eq!(snapshot.kind, Some(aj_agent::queue::PendingKind::Steering));
        assert_eq!(snapshot.text, "steer this");
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "",
            "the steered text left the editor"
        );

        // Busy + empty editor + pending follow-up: promote to steering.
        world.handles().queues.clear(AgentId::Main);
        world
            .handles()
            .queues
            .append_follow_up(AgentId::Main, "follow-up");
        assert!(handle_host_action(&mut world, &shell, AjAction::Steer).await);
        let snapshot = world.handles().queues.snapshot(AgentId::Main);
        assert_eq!(
            snapshot.kind,
            Some(aj_agent::queue::PendingKind::Steering),
            "the pending follow-up escalated"
        );
        assert_eq!(snapshot.text, "follow-up");

        // Settle the turn so the teardown below is clean. Drop the queue
        // first so the host's post-turn wake has nothing to deliver.
        world.handles().queues.clear(AgentId::Main);
        cancel_viewed_turn(&world).await;
        settle(&mut world).await;
        shut_down(&world).await;
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
        assert!(handle_host_action(&mut world, &shell, AjAction::Steer).await);
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "idle steer spawned a prompt turn");
        assert!(
            world
                .handles()
                .queues
                .snapshot(AgentId::Main)
                .kind
                .is_none(),
            "nothing queued"
        );

        settle(&mut world).await;
        shut_down(&world).await;
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

        handle_submit(&mut world, "earlier prompt".to_string()).await;
        settle(&mut world).await;
        for i in 0..40 {
            fold_notice(&mut world, &format!("historical notice {i}"));
        }
        app.request_redraw();
        app.render(&root).expect("render populated transcript");
        // The turn has to still be in flight when the Alt+Enter lands below.
        install_busy_script(world.handles());
        handle_submit(&mut world, "running prompt".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "the session is busy");

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
        assert!(handle_host_action(&mut world, &shell, action).await);

        let snapshot = world.handles().queues.snapshot(AgentId::Main);
        assert_eq!(snapshot.kind, Some(aj_agent::queue::PendingKind::Steering));
        assert_eq!(snapshot.text, "steer draft");
        let _ = transcript.borrow_mut().draw(&transcript_ctx);
        assert!(
            transcript.borrow().is_at_bottom(),
            "accepted text follows tail"
        );

        world.handles().queues.clear(AgentId::Main);
        cancel_viewed_turn(&world).await;
        settle(&mut world).await;
        shut_down(&world).await;
    }

    /// Alt+Enter is editor-local: with transcript focus, an idle draft is
    /// preserved and no host action or turn is produced.
    #[tokio::test]
    async fn focused_idle_alt_enter_does_not_submit() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, mut writer, mut world, shell, root) =
            init_app_with_world(&dir, "streaming-text").await;

        handle_submit(&mut world, "earlier prompt".to_string()).await;
        settle(&mut world).await;
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
        assert!(!world.client().working(), "no turn was spawned");
        assert!(transcript.borrow().in_focus_mode(), "focus is preserved");
        let _ = transcript.borrow_mut().draw(&transcript_ctx);
        assert!(!transcript.borrow().is_at_bottom(), "scroll is preserved");
        shut_down(&world).await;
    }

    /// Alt+Enter is also inert outside the editor while a turn is busy, so it
    /// cannot consume the draft or mutate the steering queue.
    #[tokio::test]
    async fn focused_busy_alt_enter_does_not_steer() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, mut writer, mut world, shell, root) =
            init_app_with_world(&dir, "streaming-text").await;

        handle_submit(&mut world, "earlier prompt".to_string()).await;
        settle(&mut world).await;
        for i in 0..40 {
            fold_notice(&mut world, &format!("historical notice {i}"));
        }
        app.request_redraw();
        app.render(&root).expect("render populated transcript");
        // The turn has to still be in flight when the Alt+Enter lands below.
        install_busy_script(world.handles());
        handle_submit(&mut world, "running prompt".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "the session is busy");

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
        assert!(!world.handles().queues.has_pending(AgentId::Main));
        assert!(transcript.borrow().in_focus_mode(), "focus is preserved");
        let _ = transcript.borrow_mut().draw(&transcript_ctx);
        assert!(!transcript.borrow().is_at_bottom(), "scroll is preserved");

        cancel_viewed_turn(&world).await;
        settle(&mut world).await;
        shut_down(&world).await;
    }

    /// Alt+Up's dequeue action pulls the queued message back into the
    /// editor, prepending it to the current draft (blank-line joined),
    /// and empties the queue.
    #[tokio::test]
    async fn dequeue_action_yanks_the_pending_message_into_the_editor() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        handle_submit(&mut world, "first".to_string()).await;
        fold_ready_frames(&mut world);
        handle_submit(&mut world, "queued line".to_string()).await;
        assert_eq!(
            world.handles().queues.snapshot(AgentId::Main).text,
            "queued line"
        );

        shell.borrow().editor.borrow_mut().insert_at_cursor("draft");
        assert!(handle_host_action(&mut world, &shell, AjAction::Dequeue).await);
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "queued line\n\ndraft"
        );
        assert!(!world.handles().queues.has_pending(AgentId::Main));

        // Nothing pending: the withdrawal reports no change.
        assert!(!handle_host_action(&mut world, &shell, AjAction::Dequeue).await);

        cancel_viewed_turn(&world).await;
        settle(&mut world).await;
        shut_down(&world).await;
    }

    /// Cancelling a turn restores the queued message into the editor
    /// (matching aj) instead of letting the post-turn wake deliver it.
    #[tokio::test]
    async fn cancel_action_yanks_the_pending_message_back() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        handle_submit(&mut world, "first".to_string()).await;
        fold_ready_frames(&mut world);
        handle_submit(&mut world, "second".to_string()).await;

        assert!(handle_host_action(&mut world, &shell, AjAction::CancelTurn).await);
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "second",
            "the queued follow-up came back to the editor"
        );
        assert!(!world.handles().queues.has_pending(AgentId::Main));

        settle(&mut world).await;
        assert!(
            !world.client().working(),
            "no wake spawned, the queue was empty"
        );
        shut_down(&world).await;
    }

    /// The two overlay openers park their command for the host to open the
    /// overlay on the next drive-loop step.
    #[tokio::test]
    async fn opener_host_actions_park_their_commands() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;

        handle_host_action(&mut world, &shell, AjAction::HistoryOpen).await;
        assert_eq!(
            shell.borrow().take_command(),
            Some(CommandAction::OpenPromptHistory)
        );
        handle_host_action(&mut world, &shell, AjAction::AgentPickerOpen).await;
        assert_eq!(
            shell.borrow().take_command(),
            Some(CommandAction::OpenAgentPicker)
        );
        shut_down(&world).await;
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

        let mut lifecycle = AgentLifecycle::default();
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
            None,
        );
        for i in 0..count {
            let _ = reduce(
                &mut chat.borrow_mut(),
                &mut lifecycle,
                notice_event(&format!("line-{i:03}")),
                None,
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

    fn wheel_down_at(row: i16, col: i16) -> Event {
        Event::Mouse(vaxis::mouse::Mouse {
            col,
            row,
            xoffset: 0,
            yoffset: 0,
            button: vaxis::mouse::Button::WheelDown,
            mods: vaxis::mouse::Modifiers::empty(),
            kind: vaxis::mouse::Type::Press,
        })
    }

    /// A buttonless pointer move, which is what a terminal in any-event
    /// tracking reports as the pointer crosses the screen.
    fn motion_at(row: i16, col: i16) -> Event {
        Event::Mouse(vaxis::mouse::Mouse {
            col,
            row,
            xoffset: 0,
            yoffset: 0,
            button: vaxis::mouse::Button::None,
            mods: vaxis::mouse::Modifiers::empty(),
            kind: vaxis::mouse::Type::Motion,
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
            aj_app::keybindings::action_shortcut(aj_app::keybindings::ACTION_PALETTE_OPEN)
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
                thinking_display: "default".into(),
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
            Some(TaskRegistry::default()),
            theme.clone(),
            "aj".to_string(),
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
            arm_branch(&shell.branch_anchor, "m1".to_string());
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
        world_shell_app_with_idle_grace(dir, demo, layers, None).await
    }

    /// [`world_shell_app`] over a host that releases an idle, unattached
    /// session after `idle_grace`.
    async fn world_shell_app_with_idle_grace(
        dir: &TempDir,
        demo: &str,
        layers: ConfigLayers,
        idle_grace: Option<Duration>,
    ) -> (World, Rc<RefCell<Shell>>, AsyncApp, PipeWriter, WidgetRef) {
        let world = scripted_world_with(dir, demo, layers, idle_grace).await;
        let shell = Rc::new(RefCell::new(Shell::new(
            Rc::clone(&world.chat),
            Rc::clone(&world.status),
            Some(world.handles().task_registry.clone()),
            ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor)),
            "aj".to_string(),
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

    // ---- Inline images (lazy transmit + free) ----

    /// A valid 2x2 RGB PNG, base64-encoded, as a `read_file` tool result would
    /// carry it. Generated once and pasted in: aj has no image encoder.
    const PNG_2X2_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAADklEQVR4nGOoBwMGCAUAKboF9Rf06ToAAAAASUVORK5CYII=";

    /// Reduce a tool-result image entry (start + end) into `chat` and return
    /// its `EntryId`. Seeds a user message first so the chat slot shows the
    /// transcript rather than the empty-state splash.
    fn seed_image_entry(chat: &Rc<RefCell<ChatState>>) -> aj_app::chat::EntryId {
        seed_image_entry_with(chat, PNG_2X2_B64)
    }

    /// Like [`seed_image_entry`] but with an explicit base64 image payload, so a
    /// test can seed a corrupt image to exercise the transmit-failure path.
    fn seed_image_entry_with(chat: &Rc<RefCell<ChatState>>, data: &str) -> aj_app::chat::EntryId {
        use aj_agent::message::AgentMessage;
        use aj_models::types::{Message, UserMessage};

        let mut life = AgentLifecycle::default();
        let _ = reduce(
            &mut chat.borrow_mut(),
            &mut life,
            AgentEvent::MessageEnd {
                agent_id: AgentId::Main,
                message: AgentMessage::wire(Message::User(UserMessage::text("show me the png"))),
            },
            None,
        );
        let _ = reduce(
            &mut chat.borrow_mut(),
            &mut life,
            AgentEvent::ToolExecutionStart {
                agent_id: AgentId::Main,
                call_id: "img-1".into(),
                tool: "read_file".into(),
                args: serde_json::json!({"path": "/tmp/pic.png"}),
            },
            None,
        );
        let _ = reduce(
            &mut chat.borrow_mut(),
            &mut life,
            AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Main,
                call_id: "img-1".into(),
                tool: "read_file".into(),
                result: aj_agent::tool::ToolDetails::Image {
                    summary: "/tmp/pic.png".into(),
                    mime_type: "image/png".into(),
                    original_dimensions: (2, 2),
                    displayed_dimensions: (2, 2),
                },
                content: vec![UserContent::Image(aj_models::types::ImageContent {
                    data: data.into(),
                    mime_type: "image/png".into(),
                })]
                .into(),
                is_error: false,
            },
            None,
        );
        let chat = chat.borrow();
        chat.transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .find(|e| matches!(&e.kind, EntryKind::Tool(_)))
            .expect("tool image entry")
            .id
    }

    /// A graphics-capable app + shell over `chat`, with `caps.kitty_graphics`
    /// set before init so the probe leaves it on, and `images` threaded into
    /// the styles via `set_terminal_caps` + `restyle`.
    async fn graphics_shell_app(
        chat: Rc<RefCell<ChatState>>,
    ) -> (AsyncApp, PipeWriter, Rc<RefCell<Shell>>, WidgetRef) {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        writer.write_all(b"\x1b[?c").expect("write DA1 reply");
        let shell = test_shell_with_chat(chat);
        let root: WidgetRef = to_widget_ref(Rc::clone(&shell));
        let mut vx = Vaxis::new(VaxisOptions::default());
        vx.caps.kitty_graphics = true;
        let mut app = AsyncApp::new(vx, Box::new(TestTty::new()), reader.into());
        app.init(Rc::clone(&root), Options::default())
            .await
            .expect("init");
        shell.borrow().set_terminal_caps(TerminalCaps {
            images: true,
            ..TerminalCaps::default()
        });
        shell.borrow().restyle();
        (app, writer, shell, root)
    }

    /// Rendering one frame over a visible image entry and draining the pending
    /// set transmits it (the store gains an id); a direct `free_session_images`
    /// then empties the store.
    ///
    /// The free path is exercised directly rather than through the production
    /// session-switch site (`install_next_session` then `free_session_images`),
    /// which needs a full second session to build. So this pins the store
    /// lifecycle, not the switch-site call wiring. The `drive`-loop transmit
    /// call site is pinned separately by [`drive_loop_transmits_visible_image`].
    #[tokio::test]
    async fn drive_transmits_visible_image_and_free_drains_the_store() {
        let dir = TempDir::new().expect("tempdir");
        let world = scripted_world(&dir, "streaming-text").await;
        let entry_id = seed_image_entry(&world.chat);
        let (mut app, _writer, shell, root) = graphics_shell_app(Rc::clone(&world.chat)).await;

        // The frame records the visible image as pending; the drain transmits.
        app.request_redraw();
        app.render(&root).expect("render");
        drain_pending_images(&mut app, &world, &shell);

        assert!(
            shell
                .borrow()
                .image_store
                .borrow()
                .get(AgentId::Main, entry_id)
                .is_some(),
            "the visible image transmitted and gained an id",
        );

        // A session switch frees the ids and empties the store. This pins the
        // store lifecycle only: the boxed `dyn Tty` inside `AppCore` cannot be
        // downcast, so the emitted kitty delete escape is not observable here.
        // That byte sequence is asserted at the vaxis layer
        // (`free_image_emits_delete_by_id`).
        free_session_images(&mut app, &shell);
        assert!(
            shell
                .borrow()
                .image_store
                .borrow()
                .get(AgentId::Main, entry_id)
                .is_none(),
            "free_session_images empties the store",
        );
        // Keep `world` alive so its chat outlives the shell's borrows above.
        world.chat.borrow();
    }

    /// A visible image whose bytes will not decode is marked failed by the
    /// drain rather than left pending, so it falls back to text and is not
    /// re-attempted every frame. Dropping the `mark_failed` call on the
    /// transmit-error arm leaves the entry unmarked (and re-recorded pending
    /// next frame), reddening this test.
    #[tokio::test]
    async fn drain_marks_undecodable_image_failed() {
        use base64::Engine;

        let dir = TempDir::new().expect("tempdir");
        let world = scripted_world(&dir, "streaming-text").await;
        // Valid base64, but the decoded bytes are not a valid image, so
        // `load_image` errors and the entry is marked failed.
        let corrupt = base64::engine::general_purpose::STANDARD.encode(b"not a real image");
        let entry_id = seed_image_entry_with(&world.chat, &corrupt);
        let (mut app, _writer, shell, root) = graphics_shell_app(Rc::clone(&world.chat)).await;

        app.request_redraw();
        app.render(&root).expect("render");
        drain_pending_images(&mut app, &world, &shell);

        assert!(
            shell
                .borrow()
                .image_store
                .borrow()
                .is_failed(AgentId::Main, entry_id),
            "the drain marked the undecodable image failed",
        );
        assert!(
            shell
                .borrow()
                .image_store
                .borrow()
                .get(AgentId::Main, entry_id)
                .is_none(),
            "an undecodable image is not transmitted",
        );
        // Keep `world` alive so its chat outlives the shell's borrows above.
        world.chat.borrow();
    }

    /// A graphics-capable app + shell over a scripted `world`, mirroring
    /// [`init_app_with_world`] but with `caps.kitty_graphics` set before init so
    /// the probe leaves it on, and `images` threaded into the styles via
    /// `set_terminal_caps` + `restyle`. Returns the write end so the caller
    /// keeps the reader from seeing EOF until it drops it.
    async fn graphics_world_shell_app(
        dir: &TempDir,
        demo: &str,
    ) -> (AsyncApp, PipeWriter, World, Rc<RefCell<Shell>>, WidgetRef) {
        let world = scripted_world(dir, demo).await;
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        writer.write_all(b"\x1b[?c").expect("write DA1 reply");
        let shell = Rc::new(RefCell::new(Shell::new(
            Rc::clone(&world.chat),
            Rc::clone(&world.status),
            Some(world.handles().task_registry.clone()),
            ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor)),
            "aj".to_string(),
            "",
            PathBuf::from("/tmp"),
        )));
        let root: WidgetRef = to_widget_ref(Rc::clone(&shell));
        let mut vx = Vaxis::new(VaxisOptions::default());
        vx.caps.kitty_graphics = true;
        let mut app = AsyncApp::new(vx, Box::new(TestTty::new()), reader.into());
        app.init(Rc::clone(&root), Options::default())
            .await
            .expect("init");
        shell.borrow().set_terminal_caps(TerminalCaps {
            images: true,
            ..TerminalCaps::default()
        });
        shell.borrow().restyle();
        (app, writer, world, shell, root)
    }

    /// Driving the real `drive` loop one iteration over a visible tool image
    /// entry transmits it: the top-of-iteration render draws the image (which
    /// records the pending key) and the loop's post-render
    /// `drain_pending_images` call transmits it, so the store gains an id.
    /// Deleting that call site leaves the store empty and reddens this test.
    #[tokio::test]
    async fn drive_loop_transmits_visible_image() {
        let dir = TempDir::new().expect("tempdir");
        let (mut app, mut writer, mut world, shell, root) =
            graphics_world_shell_app(&dir, "streaming-text").await;
        let entry_id = seed_image_entry(&world.chat);

        let mut theme_watch = inert_theme_watch();
        let mut prompt_history_rx: Option<UnboundedReceiver<Vec<String>>> = None;
        let mut autocomplete_rx = shell
            .borrow()
            .editor
            .borrow_mut()
            .take_autocomplete_rx()
            .expect("editor hands out its autocomplete receiver once");

        // One benign key forces a full loop iteration (whose top-of-iteration
        // render draws the image entry and drains the pending set), then EOF
        // (the dropped writer) quits.
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
            shell
                .borrow()
                .image_store
                .borrow()
                .get(AgentId::Main, entry_id)
                .is_some(),
            "the loop's drain_pending_images transmitted the visible image",
        );
        // Keep `world` alive so its chat outlives the shell's borrows above.
        world.chat.borrow();
    }

    /// `image_entry_bytes` decodes the first image in a tool entry, and yields
    /// `None` for a non-image or a corrupt payload.
    #[test]
    fn image_entry_bytes_decodes_tool_image_content() {
        use aj_app::chat::{ToolEntry, Transcript};

        let image_entry = |content: Vec<UserContent>| {
            let mut t = Transcript::default();
            let id = t.append(EntryKind::Tool(ToolEntry {
                call_id: "c1".into(),
                tool: "read_file".into(),
                args: serde_json::json!({}),
                status: ToolStatus::Done { is_error: false },
                details: None,
                content: content.into(),
                task: None,
                header_only: false,
            }));
            (t, id)
        };

        let (t, id) = image_entry(vec![UserContent::Image(aj_models::types::ImageContent {
            data: PNG_2X2_B64.into(),
            mime_type: "image/png".into(),
        })]);
        let bytes = image_entry_bytes(t.get(id).expect("entry")).expect("decoded bytes");
        assert_eq!(
            &bytes[..8],
            b"\x89PNG\r\n\x1a\n",
            "raw PNG bytes, not base64"
        );

        // No image content -> None.
        let (t, id) = image_entry(vec![UserContent::text("hi")]);
        assert!(image_entry_bytes(t.get(id).expect("entry")).is_none());

        // Corrupt base64 -> None.
        let (t, id) = image_entry(vec![UserContent::Image(aj_models::types::ImageContent {
            data: "not valid base64!!!".into(),
            mime_type: "image/png".into(),
        })]);
        assert!(image_entry_bytes(t.get(id).expect("entry")).is_none());
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
            world.handles().run_config.lock().unwrap().thinking,
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
                .handles()
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
            world.handles().run_config.lock().unwrap().thinking,
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
            ui.as_ref().unwrap().value_of("auto_compact")
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
            ui.as_ref().unwrap().set_value("speed", "fast");
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
            ui.as_ref().unwrap().value_of("speed")
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
    ///
    /// A registration made straight on the registry publishes no `TaskStart`,
    /// so the tasks read is discharged afterwards: that is how a client learns
    /// about a task it did not see start, and it is what the frontend's task
    /// table is built from.
    async fn register_bash_task(world: &mut World, command: &str) -> aj_agent::tool::TaskId {
        let (id, _cancel) = world.handles().task_registry.register(
            AgentId::Main,
            "test-call".to_string(),
            aj_agent::tool::TaskKind::Bash {
                command: command.to_string(),
            },
            command.to_string(),
            Arc::new(NoOutput),
        );
        read_host_state(world).await;
        id
    }

    /// Fold a running sub-agent and a running bash task into the world's
    /// chat model through the reducer, so a picker snapshot lists both.
    fn seed_sub_and_task(world: &mut World) {
        let settings = aj_agent::events::AgentSettings {
            provider: "scripted".into(),
            model_id: "scripted".into(),
            thinking: "off".into(),
            thinking_display: "default".into(),
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
            fold_event(world, event);
        }
    }

    fn main_notices(world: &World) -> Vec<String> {
        notices_of(&world.chat.borrow())
    }

    /// The user rows of the focused Main transcript, in order.
    fn user_messages(world: &World) -> Vec<String> {
        world
            .chat
            .borrow()
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::User(user) => Some(user.joined_text()),
                _ => None,
            })
            .collect()
    }

    /// The notice rows of a chat model's Main transcript, in order.
    ///
    /// Takes the model rather than the world, so a test observing a drive loop
    /// that holds `&mut World` can read it through its own `Rc` handle.
    fn notices_of(chat: &ChatState) -> Vec<String> {
        chat.transcript(AgentId::Main)
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
        fold_event(
            &mut world,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "xhigh".into(),
                    thinking_display: "default".into(),
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
        fold_event(
            &mut world,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "xhigh".into(),
                    thinking_display: "default".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            },
        );
        fold_event(
            &mut world,
            AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                report: "done".into(),
                conclusion: aj_agent::events::SubAgentConclusion::Completed,
            },
        );
        fold_event(
            &mut world,
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
        fold_event(
            &mut world,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "standard".into(),
                    thinking_display: "default".into(),
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
        fold_event(
            &mut world,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "standard".into(),
                    thinking_display: "default".into(),
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
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        let theme = Theme::bundled_dark_with_mode(ColorMode::Truecolor);

        // Observe a sub-agent at a level distinct from the default (a fresh
        // main view falls back to the run config's level), so the editor
        // carries both an `agent 1` marker and a border tint that visibly moves
        // across the switch.
        fold_event(
            &mut world,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "minimal".into(),
                    thinking_display: "default".into(),
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

        // Focus a fresh session. Attaching one lands on the main view.
        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved));

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
        shut_down(&world).await;
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
        fold_event(
            &mut world,
            AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(1),
                task: "reason harder".into(),
                background: false,
                settings: aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "xhigh".into(),
                    thinking_display: "default".into(),
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
        let id = register_bash_task(&mut world, "cargo build").await;
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
        assert!(world.handles().task_registry.summary(id).is_some());

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
        let id = register_bash_task(&mut world, "cargo test").await;

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
        let id = register_bash_task(&mut world, "sleep 100").await;

        apply_picker_outcome(&mut world, &shell, AgentPickerOutcome::Kill(id)).await;
        world
            .handles()
            .task_registry
            .set_status(id, aj_agent::tool::TaskStatus::Killed);
        // A real run learns the flip from the task's own `TaskEnd`; a status
        // set straight on the registry publishes none, so the read stands in.
        read_host_state(&mut world).await;
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
        let id = register_bash_task(&mut world, "echo hello").await;
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
        let id = world.session().to_string();
        // The host holds the session live (and its lock) until it shuts down,
        // so a caller that wants to resume the id has to be handed a session
        // nobody holds.
        shut_down(&world).await;
        id
    }

    /// Submit `prompt` into `world` and settle the turn, so its messages
    /// reach the chat model and the log.
    async fn run_prompt(world: &mut World, prompt: &str) {
        handle_submit(world, prompt.to_string()).await;
        settle(world).await;
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

    /// Neither gesture is gated on live work: the selector opens mid-turn and
    /// `NewSession` parks its request, because the session left behind keeps
    /// folding and finishes its turn unwatched (spec 9.2).
    #[tokio::test]
    async fn new_session_and_the_selector_are_open_mid_turn() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        handle_submit(&mut world, "go".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "busy");

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

        // NewSession parks mid-turn rather than refusing.
        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::NewSession).await,
            ActionEffect::Redraw
        ));
        assert_eq!(
            shell.borrow().take_session_request(),
            Some(SessionRequest::New),
            "a new session is parked mid-turn",
        );
        assert!(
            crate::toasts::toast_texts(&shell.borrow().toasts).is_empty(),
            "and nothing was refused: {:?}",
            crate::toasts::toast_texts(&shell.borrow().toasts)
        );

        // Settle the turn so teardown is clean.
        cancel_viewed_turn(&world).await;
        settle(&mut world).await;
        shut_down(&world).await;
    }

    /// A running background task does not hold the user in a session either: it
    /// keeps running behind the switch, exactly as a turn does.
    #[tokio::test]
    async fn new_session_is_allowed_while_background_work_runs() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        assert!(!world.client().working(), "no turn in flight");
        let _task = register_bash_task(&mut world, "sleep 100").await;

        assert!(matches!(
            apply_command(&mut world, &shell, CommandAction::NewSession).await,
            ActionEffect::Redraw
        ));
        assert_eq!(
            shell.borrow().take_session_request(),
            Some(SessionRequest::New),
            "a live task does not refuse a new session",
        );
        assert!(
            crate::toasts::toast_texts(&shell.borrow().toasts).is_empty(),
            "and nothing was refused: {:?}",
            crate::toasts::toast_texts(&shell.borrow().toasts)
        );
        shut_down(&world).await;
    }

    /// The session tree opens read-only even mid-turn (the branch switch it
    /// leads to is refused at confirm time, not by refusing to open).
    #[tokio::test]
    async fn session_tree_opens_mid_turn() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        handle_submit(&mut world, "go".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "busy");

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
        cancel_viewed_turn(&world).await;
        settle(&mut world).await;
        shut_down(&world).await;
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
            .list_session_previews_streaming(&|| false, &mut |batch| previews.extend(batch));
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
            .list_session_previews_streaming(&|| false, &mut |batch| previews.extend(batch));
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

    /// The switch path: focusing another session re-attaches over the same
    /// Shell, rebinding by content-swap so the transcript renders the new
    /// session's model and the pending box reads the new session's queues. The
    /// session left behind accumulates its usage for the shutdown banner.
    #[tokio::test]
    async fn switch_rebuilds_the_session_and_accumulates_usage() {
        let dir = TempDir::new().expect("tempdir");
        let beta = create_disk_session(&dir, "beta session prompt").await;

        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "alpha session prompt").await;
        let alpha_id = world.session().to_string();

        // Snapshot the outgoing usage, as the outer loop does before it
        // changes focus.
        let mut completed: Vec<(String, UsageSummary)> = Vec::new();
        completed.push((
            alpha_id.clone(),
            world
                .host()
                .usage(&alpha_id)
                .await
                .expect("usage")
                .expect("a live session"),
        ));

        // Switch to beta over the same Shell.
        let moved = apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Resume(beta.clone()),
        )
        .await;
        assert!(matches!(moved, Focus::Moved));

        assert_eq!(world.session(), beta, "the frontend re-attached onto beta");
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
            format!("aj - session {beta}")
        );
        // The pending box reads the focused session's queue out of the chat
        // model, which the swap replaced, so a message queued on the new
        // session previews.
        stage_pending(&mut world, AgentId::Main, "queued after switch").await;
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
        world.handles().queues.clear(AgentId::Main);

        // Switch again, this time to a fresh session; usage keeps
        // accumulating and the new session's transcript is empty.
        completed.push((
            world.session().to_string(),
            world
                .host()
                .usage(world.session())
                .await
                .expect("usage")
                .expect("a live session"),
        ));
        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved));

        assert_ne!(world.session(), beta, "a fresh session was minted");
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
        // Collecting and formatting the banner over the accumulated list must
        // not panic, and it itemizes the live session too.
        let banner = ExitBanner::collect(&world, completed).await;
        assert_eq!(banner.completed.len(), 2);
        assert_eq!(
            banner.live.as_ref().map(|(id, _)| id.as_str()),
            Some(world.session()),
        );
        banner.print();
        shut_down(&world).await;
    }

    // --- Branch flow (Phase 3) ---

    /// The clean-rebuild branch confirmation distinguishes the `b`-submit
    /// flow (a prompt is handed off) from a tree-view switch (a bare head
    /// move).
    #[test]
    fn branch_switch_notice_distinguishes_b_flow_from_tree_switch() {
        assert_eq!(
            branch_switch_notice(true),
            "Branched the conversation from an earlier message."
        );
        assert_eq!(
            branch_switch_notice(false),
            "Switched to the selected branch."
        );
    }

    /// Arming records the branched-from message id; re-arming replaces it;
    /// disarming clears it. The transcript reads this cell to keep the
    /// highlight box on the branched-from message (that rendering is covered
    /// in the transcript tests).
    #[tokio::test]
    async fn arming_sets_anchor_and_rearm_replaces_and_disarm_clears() {
        let dir = TempDir::new().expect("tempdir");
        let (_world, shell) = world_and_shell(&dir, "streaming-text").await;
        arm_branch(&shell.borrow().branch_anchor, "m1".to_string());
        assert_eq!(
            shell.borrow().branch_anchor.borrow().clone(),
            Some("m1".to_string())
        );

        // Re-arming replaces the anchor.
        arm_branch(&shell.borrow().branch_anchor, "m2".to_string());
        assert_eq!(
            shell.borrow().branch_anchor.borrow().clone(),
            Some("m2".to_string())
        );

        // Disarming clears it.
        shell.borrow().disarm_branch();
        assert!(shell.borrow().branch_anchor.borrow().is_none());
    }

    /// While a branch is armed the editor chrome reads `branching`; disarming
    /// restores the agent marker (empty on the main view).
    #[tokio::test]
    async fn editor_chrome_shows_branching_while_armed() {
        let dir = TempDir::new().expect("tempdir");
        let (world, shell) = world_and_shell(&dir, "streaming-text").await;

        sync_editor_chrome(&world, &shell);
        assert!(
            !editor_top_bar_text(&shell).contains("branching"),
            "no marker before arming"
        );

        arm_branch(&shell.borrow().branch_anchor, "m1".to_string());
        sync_editor_chrome(&world, &shell);
        assert!(
            editor_top_bar_text(&shell).contains("branching"),
            "armed: chrome shows the branching marker: {}",
            editor_top_bar_text(&shell),
        );

        shell.borrow().disarm_branch();
        sync_editor_chrome(&world, &shell);
        assert!(
            !editor_top_bar_text(&shell).contains("branching"),
            "disarming clears the marker"
        );
    }

    /// Arming prefills the editor with the branched-from message but preserves
    /// the user's in-progress draft on the recall history, so clearing the
    /// prefill and pressing up brings the draft back rather than losing it.
    #[tokio::test]
    async fn branch_prefill_preserves_the_draft_for_recall() {
        let dir = TempDir::new().expect("tempdir");
        let (_world, shell) = world_and_shell(&dir, "streaming-text").await;
        let editor = Rc::clone(&shell.borrow().editor);
        editor.borrow_mut().set_text("my unsent draft");

        // The arm handler's prefill: the draft goes onto history, the message
        // fills the editor.
        prefill_branch_editor(&editor, "branched message");
        assert_eq!(editor.borrow().text(), "branched message");

        // Clearing the prefill and pressing up recalls the preserved draft.
        editor.borrow_mut().set_text("");
        editor.borrow_mut().handle_event(
            &mut EventContext::new(),
            &Event::KeyPress(Key {
                codepoint: Key::UP,
                mods: Modifiers::empty(),
                ..Key::default()
            }),
        );
        assert_eq!(
            editor.borrow().text(),
            "my unsent draft",
            "up recalls the draft the branch prefill would have discarded"
        );
    }

    /// Esc cancels an armed anchor: it clears the anchor (dropping the
    /// transcript's highlight box), flags
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
            arm_branch(&sh.branch_anchor, "m1".to_string());
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
            arm_branch(&sh.branch_anchor, "m1".to_string());
        }
        // Steer is refused: the editor keeps its draft and the anchor stays.
        assert!(handle_host_action(&mut world, &shell, AjAction::Steer).await);
        assert_eq!(shell.borrow().editor.borrow().text(), "branch draft");
        assert!(shell.borrow().branch_anchor.borrow().is_some());
        // Dequeue is refused the same way.
        assert!(handle_host_action(&mut world, &shell, AjAction::Dequeue).await);
        assert_eq!(shell.borrow().editor.borrow().text(), "branch draft");
        assert!(shell.borrow().branch_anchor.borrow().is_some());
        shut_down(&world).await;
    }

    /// An empty (post-trim) submit while armed is refused and keeps the
    /// anchor: the head must not move for a prompt that would be dropped.
    #[tokio::test]
    async fn empty_submit_refused_keeps_anchor() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        {
            let sh = shell.borrow();
            arm_branch(&sh.branch_anchor, "m1".to_string());
        }
        let outcome = submit_with_armed_anchor(&mut world, &shell, "   ".to_string()).await;
        assert!(matches!(outcome, ArmedSubmit::Stay));
        assert!(
            shell.borrow().branch_anchor.borrow().is_some(),
            "the anchor is kept on an empty submit"
        );
        shut_down(&world).await;
    }

    /// A submit while busy (here a live turn) is refused with a toast, keeping
    /// the anchor and restoring the editor text the submit cleared, and spawns
    /// no rebuild or new turn.
    #[tokio::test]
    async fn busy_submit_refused_toasts_keeps_anchor_and_restores_text() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        // Start a turn so the session is busy.
        handle_submit(&mut world, "first".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "a turn is in flight");
        {
            let sh = shell.borrow();
            arm_branch(&sh.branch_anchor, "m1".to_string());
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
        // Settle the turn so the teardown below is clean.
        cancel_viewed_turn(&world).await;
        settle(&mut world).await;
        let prompts: Vec<String> = world
            .chat
            .borrow()
            .transcript(AgentId::Main)
            .expect("main transcript")
            .entries()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::User(u) => Some(u.joined_text()),
                _ => None,
            })
            .collect();
        assert_eq!(
            prompts,
            vec!["first"],
            "the refused submit never reached the host",
        );
        shut_down(&world).await;
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
        assert!(!world.client().working(), "no turn in flight");
        let task = register_bash_task(&mut world, "sleep 100").await;
        {
            let sh = shell.borrow();
            arm_branch(&sh.branch_anchor, "m1".to_string());
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
            .handles()
            .task_registry
            .set_status(task, aj_agent::tool::TaskStatus::Killed);
    }

    /// Submitting with an anchor armed on a persisted user message resolves to
    /// a branch exit naming that message as the one to branch before, so the
    /// host moves the head to its parent, carrying the edited prompt. The
    /// anchor is disarmed on resolution.
    #[tokio::test]
    async fn armed_submit_branches_before_the_anchored_message() {
        use aj_models::types::Message;
        use aj_session::ConversationEntryKind;

        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        persist_session(&mut world).await;

        // The first user message on disk, plus its parent (a settings entry),
        // which is where the host has to land.
        let (message_id, expected_head) = {
            let log = world.handles().log.lock().await;
            let entry = log
                .entries_in_order()
                .into_iter()
                .find(|e| {
                    matches!(
                        &e.entry,
                        ConversationEntryKind::Message { message }
                            if matches!(message.as_stored_wire(), Some(Message::User(_)))
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
        let anchored = message_id.clone();
        {
            let sh = shell.borrow();
            arm_branch(&sh.branch_anchor, message_id);
        }
        match submit_with_armed_anchor(&mut world, &shell, "edited prompt".to_string()).await {
            ArmedSubmit::Branch { target, prompt } => {
                let HeadTarget::Before(named) = target else {
                    panic!("the branch gesture names the message, not a head");
                };
                assert_eq!(named, anchored, "the anchor's own message travels");
                assert_eq!(prompt, "edited prompt");
                // And the host resolves that to the message's parent.
                world
                    .host()
                    .command(
                        world.session(),
                        Command::Head {
                            target: HeadTarget::Before(named),
                        },
                    )
                    .await
                    .expect("the branch switch is accepted");
                assert_eq!(
                    world
                        .handles()
                        .log
                        .lock()
                        .await
                        .head()
                        .cloned()
                        .expect("a head"),
                    expected_head,
                );
            }
            ArmedSubmit::Stay => panic!("expected a branch exit"),
        }
        assert!(
            shell.borrow().branch_anchor.borrow().is_none(),
            "the anchor is disarmed on resolution"
        );
    }

    /// A branch switch is refused while a turn or a background task is live,
    /// and proceeds once idle.
    ///
    /// The refusal is the host's: a mid-turn head switch would let the running
    /// turn persist onto the branch being left. The frontend surfaces the
    /// reason as the branch-failure notice.
    #[tokio::test]
    async fn branch_switch_refused_while_busy_and_proceeds_when_idle() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "seed").await;
        let head = world
            .handles()
            .log
            .lock()
            .await
            .head()
            .cloned()
            .expect("a persisted head");
        let branch = || FocusRequest::Branch {
            target: HeadTarget::Entry(head.clone()),
            prompt: None,
        };

        // A live turn refuses the switch.
        install_busy_script(world.handles());
        handle_submit(&mut world, "busy".to_string()).await;
        fold_ready_frames(&mut world);
        assert!(world.client().working(), "a turn is in flight");
        apply_focus_request(&mut app, &shell, &mut world, branch()).await;
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n.contains("Failed to branch the conversation") && n.contains("running")),
            "a live turn refuses the branch switch: {:?}",
            main_notices(&world),
        );
        cancel_viewed_turn(&world).await;
        settle(&mut world).await;

        // A running background task refuses it too, even with no turn.
        let task = register_bash_task(&mut world, "cargo build").await;
        let before = main_notices(&world).len();
        apply_focus_request(&mut app, &shell, &mut world, branch()).await;
        assert!(
            main_notices(&world)[before..]
                .iter()
                .any(|n| n.contains("Failed to branch the conversation")),
            "a running background task refuses the branch switch: {:?}",
            main_notices(&world),
        );

        // Idle (turn settled, task terminal): the switch takes.
        world
            .handles()
            .task_registry
            .set_status(task, aj_agent::tool::TaskStatus::Killed);
        apply_focus_request(&mut app, &shell, &mut world, branch()).await;
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n == "Switched to the selected branch."),
            "an idle branch switch confirms: {:?}",
            main_notices(&world),
        );
        shut_down(&world).await;
    }

    /// A session change leaves the outgoing session's background work running:
    /// the switch tears nothing down, and the host holds a session with live
    /// work whatever its idle grace says (spec section 5).
    ///
    /// The up-front refusals (the session overlays' confirms, the
    /// `NewSession` command) are what keep a user from walking away from live
    /// work by accident. There is no recheck at the consumption site, because
    /// with nothing torn down there is nothing left to protect.
    #[tokio::test]
    async fn a_switch_leaves_the_outgoing_sessions_work_running() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "seed").await;
        let outgoing = world.session().to_string();
        let task = register_bash_task(&mut world, "sleep 100").await;

        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved));
        assert_ne!(world.session(), outgoing, "the focus moved");

        let live = world
            .host()
            .sessions()
            .await
            .expect("session list")
            .sessions
            .into_iter()
            .find(|entry| entry.id == outgoing)
            .expect("the outgoing session is still listed");
        assert!(live.live, "and the host still holds it");
        assert_eq!(live.tasks, 1, "with its background task still running");
        // The new session has its own registry, so the task is not visible
        // through the handles the frontend now holds.
        assert!(
            world.handles().task_registry.status(task).is_none(),
            "the focused session's task table is its own",
        );
        shut_down(&world).await;
    }

    /// A backgrounded session keeps folding. Its frames arrive on the same
    /// stream while another session is focused, its own transcript takes them,
    /// and switching back is a swap onto state that is already current rather
    /// than a rebuild off the log (spec 9.2).
    ///
    /// The cursor is what makes this observable: it advances only if that
    /// session's fold ran, and a switch that dropped the outgoing client the way
    /// a rebuild does would leave nothing there to advance.
    #[tokio::test]
    async fn a_backgrounded_session_keeps_folding() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "in the first session").await;
        let first = world.session().to_string();
        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved));
        assert!(
            world.directory.is_attached(&first),
            "the outgoing session is still folded, not dropped",
        );

        // Work in the backgrounded session, commanded straight at the host so
        // the focused session is not the one doing it.
        world
            .host()
            .command(
                &first,
                Command::Prompt {
                    agent: AgentId::Main,
                    content: vec![UserContent::text("while in the background")],
                },
            )
            .await
            .expect("the backgrounded session accepts a prompt");

        // Drain until the turn shows up in that session's own parked transcript.
        //
        // Watching its cursor instead would not wait for anything: the switch
        // reopened the stream, so this session is being served a re-attach block
        // of its own, and that block advances the cursor whether or not the
        // prompt below was ever folded. The transcript content can only come
        // from the background fold.
        let folded_prompt = |world: &World| {
            world
                .directory
                .parked_chat(&first)
                .and_then(|chat| chat.transcript(AgentId::Main))
                .is_some_and(|transcript| {
                    transcript.entries().iter().any(|entry| {
                        matches!(&entry.kind, EntryKind::User(user)
                            if user.joined_text() == "while in the background")
                    })
                })
        };
        let deadline = Instant::now() + SETTLE_DEADLINE;
        while !folded_prompt(&world) {
            assert!(
                Instant::now() < deadline,
                "the backgrounded session never folded the turn it was given",
            );
            fold_ready_frames(&mut world);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // None of it reached the transcript on screen: that one is the created
        // session's.
        assert!(
            !main_notices(&world)
                .iter()
                .any(|text| text.contains("while in the background")),
            "a background fold leaked into the focused transcript: {:?}",
            main_notices(&world),
        );

        // Switching back brings up the transcript that was folding all along,
        // carrying both the turn from before the switch and the one from after.
        let moved = apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Resume(first.clone()),
        )
        .await;
        assert!(matches!(moved, Focus::Moved));
        assert_eq!(world.session(), first);
        let users = user_messages(&world);
        assert!(
            users.iter().any(|text| text == "in the first session"),
            "the turn from before the switch: {users:?}",
        );
        assert!(
            users.iter().any(|text| text == "while in the background"),
            "and the one folded while it was backgrounded: {users:?}",
        );
        shut_down(&world).await;
    }

    /// Read the stream until it goes quiet for `QUIET`, folding everything that
    /// arrives.
    ///
    /// Stronger than [`settle`], which only takes what is already queued. Attach
    /// blocks are producer-paced (spec 6.9), so a block nobody has read yet is
    /// generated on demand and is still there to be found. Draining is what
    /// makes "no block is coming" true rather than merely "no block is queued".
    async fn drain_stream(world: &mut World) {
        const QUIET: Duration = Duration::from_millis(150);
        let deadline = Instant::now() + SETTLE_DEADLINE;
        while Instant::now() < deadline {
            let Ok(received) = tokio::time::timeout(QUIET, world.stream.recv()).await else {
                return;
            };
            let ControlFrame::Frame(frame) = received else {
                return;
            };
            let _ = world.directory.apply(&mut world.chat.borrow_mut(), frame);
        }
        panic!("the stream never went quiet");
    }

    /// A swap onto an already-attached session must not await an attach block.
    /// No block is coming for it, so awaiting one parks the drive loop on a
    /// `caught_up` the host has no reason to send and the TUI stops responding.
    ///
    /// Bounded explicitly, because the failure mode is a hang: without the
    /// timeout a regression here shows up as a test suite that never finishes
    /// rather than one that fails.
    #[tokio::test]
    async fn a_swap_onto_an_attached_session_does_not_await_a_block() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        persist_session(&mut world).await;
        let first = world.session().to_string();

        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved));
        // Drain first. The switch reopened the stream over both sessions, so
        // `first`'s own re-attach block is still to come, and a swap that
        // wrongly awaited a block would find that one and return without
        // hanging.
        drain_stream(&mut world).await;

        // Back onto a session that is attached and owes nothing, which is the
        // pure swap. Generous enough that a slow box does not trip it, short
        // enough that a wait for a block that never comes does.
        let swap = apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Resume(first.clone()),
        );
        let moved = tokio::time::timeout(Duration::from_secs(10), swap)
            .await
            .expect("a swap onto an attached session waits for nothing");
        assert!(matches!(moved, Focus::Moved));
        assert_eq!(world.session(), first);
        shut_down(&world).await;
    }

    /// The drive loop discharges a background session's re-attach without the
    /// user going anywhere. Its obligation has to be visible to the loop's
    /// set-wide check, or the session sits frozen until something else happens
    /// to reopen the stream.
    #[tokio::test]
    async fn the_loop_discharges_a_background_session_s_re_attach() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "on the first branch").await;
        let first = world.session().to_string();
        let head = world
            .handles()
            .log
            .lock()
            .await
            .head()
            .cloned()
            .expect("a persisted head");

        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved));
        world
            .host()
            .command(
                &first,
                Command::Head {
                    target: HeadTarget::Entry(head),
                },
            )
            .await
            .expect("the head switch is accepted");

        let deadline = Instant::now() + SETTLE_DEADLINE;
        while !world.directory.needs_reattach() {
            assert!(
                Instant::now() < deadline,
                "the background session's reset was never folded",
            );
            fold_ready_frames(&mut world);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !world.client().needs_reattach(),
            "the obligation is the backgrounded session's, not the focused one's",
        );

        let focused = world.session().to_string();
        // Hand it to the loop and go nowhere. The focused session is idle and
        // drained, so this re-attach is the only work left. The wait is a plain
        // bounded one because a successful background re-attach changes nothing
        // the observer can see from here, and the loop holds the world. It is
        // not a race in the direction that matters: if the loop's check only
        // ever asked the focused client, no amount of waiting discharges this.
        let (exit, _) = drive_until(&mut world, &shell, |writer| async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            drop(writer);
        })
        .await;
        assert!(matches!(exit, Ok(SessionExit::Quit)));
        assert!(
            !world.directory.needs_reattach(),
            "the loop never noticed a background session owed a re-attach",
        );
        assert_eq!(
            world.session(),
            focused,
            "and it discharged it without moving the user",
        );
        shut_down(&world).await;
    }

    /// A `reset` for a *background* session is discharged too. Another peer
    /// switching that session's head mints a fresh epoch, and until a re-attach
    /// is served its fold filters out every later frame, so a switch onto it
    /// would paint the branch the reset abandoned (spec 6.5).
    #[tokio::test]
    async fn a_background_session_reset_by_another_peer_recovers() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "on the first branch").await;
        let first = world.session().to_string();
        let head = world
            .handles()
            .log
            .lock()
            .await
            .head()
            .cloned()
            .expect("a persisted head");

        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved));

        // Another writer moves the backgrounded session's head. The host mints a
        // fresh epoch and publishes `reset` for it.
        world
            .host()
            .command(
                &first,
                Command::Head {
                    target: HeadTarget::Entry(head),
                },
            )
            .await
            .expect("the head switch is accepted");

        let deadline = Instant::now() + SETTLE_DEADLINE;
        while !world.directory.needs_reattach() {
            assert!(
                Instant::now() < deadline,
                "the background session's reset was never folded",
            );
            fold_ready_frames(&mut world);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The obligation is a background session's, so a check that only ever
        // asked the focused client would report nothing to do here.
        assert!(
            !world.client().needs_reattach(),
            "the focused session was not the one reset",
        );

        // Switching onto it serves the re-attach, so what paints is the session
        // as it now is rather than a fold frozen on the old epoch.
        let moved = apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Resume(first.clone()),
        )
        .await;
        assert!(matches!(moved, Focus::Moved));
        assert_eq!(world.session(), first);
        assert!(
            !world.directory.needs_reattach(),
            "the switch discharged the obligation",
        );
        assert_eq!(
            user_messages(&world),
            vec!["on the first branch"],
            "and the transcript is the branch the head now names",
        );
        shut_down(&world).await;
    }

    /// A switch keeps the outgoing session attached, so the host holds it past
    /// its idle grace and its live frames keep arriving for the transcript the
    /// client parked (spec 9.2).
    ///
    /// The retained lock is the point, not an oversight: spec section 5 counts a
    /// deliberately attached background session as use, so a client that keeps
    /// one keeps its lock. What the user gets for it is an instant switch back
    /// onto a transcript that stayed current while they were away.
    #[tokio::test]
    async fn a_switch_keeps_the_outgoing_session_attached_and_live() {
        const GRACE: Duration = Duration::from_millis(200);
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app_with_idle_grace(&dir, "streaming-text", default_layers(), Some(GRACE))
                .await;
        // Punctuate the outgoing session's log: a session with nothing on disk
        // is deliberately never released, which would pass this test for the
        // wrong reason.
        run_prompt(&mut world, "seed").await;
        let outgoing = world.session().to_string();

        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved));
        assert_ne!(world.session(), outgoing, "the focus moved");

        // Well past the grace that would release an unattached session.
        tokio::time::sleep(GRACE * 4).await;
        let rows = world
            .host()
            .sessions()
            .await
            .expect("session list")
            .sessions;
        let parked = rows
            .iter()
            .find(|entry| entry.id == outgoing)
            .expect("the outgoing session is still in the directory");
        assert!(
            parked.live,
            "the outgoing session was released, so the switch detached it",
        );
        assert!(
            parked.last_seq.is_some(),
            "a live row carries its position (spec 6.8)",
        );
        assert!(
            rows.iter()
                .any(|entry| entry.id == world.session() && entry.live),
            "and the focused session is live too",
        );
        shut_down(&world).await;
    }

    /// A re-attach that lands on a fresh materialization repoints the world's
    /// direct handles. A stream that ended leaves the session unattached, so the
    /// host is free to release it, and the handles the world holds then name a
    /// core nothing drives: the footer's task table, the tree and session-info
    /// overlays and the export all read through them.
    #[tokio::test]
    async fn a_reattach_after_a_release_repoints_the_handles() {
        const GRACE: Duration = Duration::from_millis(200);
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, _app, _writer, _root) =
            world_shell_app_with_idle_grace(&dir, "streaming-text", default_layers(), Some(GRACE))
                .await;
        // Punctuate the log: a session with nothing on disk is never released.
        run_prompt(&mut world, "seed").await;
        let session = world.session().to_string();
        let stale = Arc::clone(&world.handles().log);

        // The focused session's stream goes elsewhere, which is what an evicted
        // subscriber leaves behind: nothing holds the session and the grace
        // applies to it.
        let elsewhere = world.host().create().await.expect("a scratch session");
        world.stream = world
            .control
            .attach_all(&[AttachRequest {
                session: elsewhere.clone(),
                cursor: None,
            }])
            .await
            .expect("attach elsewhere");

        let deadline = Instant::now() + SETTLE_DEADLINE;
        loop {
            let listed = world
                .host()
                .sessions()
                .await
                .expect("session list")
                .sessions
                .into_iter()
                .find(|entry| entry.id == session)
                .expect("still in the directory");
            if !listed.live {
                break;
            }
            assert!(Instant::now() < deadline, "the session was never released");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        reattach(&mut world, &shell).await.expect("re-attach");
        let now = world
            .host()
            .local_handles(&session)
            .await
            .expect("the host's own handles");
        assert!(
            !Arc::ptr_eq(&world.handles().log, &stale),
            "the re-attach landed on a fresh materialization, which is the case under test",
        );
        assert!(
            Arc::ptr_eq(&world.handles().log, &now.log),
            "the world holds the handles of the core the host now drives",
        );
        shut_down(&world).await;
    }

    /// End-to-end tree switch (the prompt-`None` run-loop path): a two-branch
    /// session on disk, a selector confirm parks `SessionRequest::Branch`, its
    /// `into_exit` yields a prompt-less `SessionExit::Branch`, and the head
    /// switch plus re-attach land on the chosen head without auto-submitting.
    /// Switching back onto the other head shows that branch instead, the
    /// spec's round-trip check.
    ///
    /// Driving the whole `run()` loop is impractical in the harness, so we
    /// assert the chain up to `into_exit()` plus the focus change `run()`
    /// makes of it.
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
                .expect("shared")
                .id;
            let fork = log
                .append(
                    Some(shared),
                    ThreadKind::User,
                    None,
                    assistant("shared answer"),
                )
                .expect("fork")
                .id;
            let branch_a = log
                .append(
                    Some(fork.clone()),
                    ThreadKind::User,
                    None,
                    user("branch A prompt"),
                )
                .expect("branch A")
                .id;
            let branch_b = log
                .append(Some(fork), ThreadKind::User, None, user("branch B prompt"))
                .expect("branch B")
                .id;
            (log.session_id().to_string(), branch_a, branch_b)
        };

        let (mut world, shell, mut app, _writer, _root) =
            resumed_world_shell_app(&dir, "streaming-text", &session_id).await;

        // A tree-selector confirm parks a branch switch for branch A's head;
        // the drive loop maps it to a prompt-less branch exit.
        match (SessionRequest::Branch {
            head: branch_a.clone(),
        })
        .into_exit()
        {
            SessionExit::Branch { target, prompt } => {
                let HeadTarget::Entry(head) = target else {
                    panic!("a tree switch names a head directly");
                };
                assert_eq!(head, branch_a, "the switch targets the selected head");
                assert!(prompt.is_none(), "a tree switch carries no prompt");
            }
            _ => panic!("a branch request maps to a branch exit"),
        }

        // Move the head onto branch A (the run loop's branch path with no
        // prompt).
        apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Branch {
                target: HeadTarget::Entry(branch_a.clone()),
                prompt: None,
            },
        )
        .await;

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
            !world.client().working(),
            "a prompt-less tree switch spawns no turn"
        );

        // Switch back via the tree onto branch B: the transcript matches that
        // branch instead, proving each head re-attaches faithfully.
        apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Branch {
                target: HeadTarget::Entry(branch_b.clone()),
                prompt: None,
            },
        )
        .await;
        let rows = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(
            rows.contains("branch B prompt"),
            "branch B content shown after switching back: {rows}"
        );
        assert!(
            !rows.contains("branch A prompt"),
            "branch A content absent after switching to B: {rows}"
        );
        assert!(!world.client().working(), "still no turn spawned");
        shut_down(&world).await;
    }

    /// A branch onto a stale head is refused by the host, so the session stays
    /// where it is, the failure notice names the reason, and the pending prompt
    /// is restored into the editor rather than run against a head that did not
    /// move.
    #[tokio::test]
    async fn a_stale_head_keeps_the_session_and_restores_the_prompt() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "seed").await;
        let session = world.session().to_string();

        apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Branch {
                target: HeadTarget::Entry("does-not-exist".to_string()),
                prompt: Some("edited prompt".to_string()),
            },
        )
        .await;

        assert_eq!(world.session(), session, "the session is unchanged");
        let notices = main_notices(&world);
        assert!(
            notices
                .iter()
                .any(|n| n == "Can't switch: that branch is no longer in this session."),
            "the failure notice names the reason: {notices:?}",
        );
        assert!(
            !notices.iter().any(|n| n.contains("does-not-exist")),
            "in the gesture's words, not by quoting the host's entry id: {notices:?}",
        );
        assert!(
            notices.iter().any(|n| n.contains("Branch failed")),
            "and says the message came back: {notices:?}",
        );
        assert_eq!(shell.borrow().editor.borrow().text(), "edited prompt");
        assert!(!world.client().working(), "no turn spawned");
        shut_down(&world).await;
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
                .expect("append the root user message")
                .id;
            (log.session_id().to_string(), root_id)
        };
        let mut world = resumed_world(&dir, "streaming-text", &session_id).await;
        let shell = shell_for(&world);
        {
            let sh = shell.borrow();
            arm_branch(&sh.branch_anchor, root_id);
        }
        let outcome =
            submit_with_armed_anchor(&mut world, &shell, "edited root prompt".to_string()).await;
        let ArmedSubmit::Branch { target, prompt } = outcome else {
            panic!("the submit hands the anchor to the host");
        };
        assert!(
            shell.borrow().branch_anchor.borrow().is_none(),
            "the anchor is disarmed once it is handed off"
        );

        // The host owns the resolution, so the refusal comes from there.
        let (mut app, _writer, _root) = app_over(&shell).await;
        apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Branch {
                target,
                prompt: Some(prompt),
            },
        )
        .await;
        assert!(
            main_notices(&world)
                .iter()
                .any(|notice| notice == "Can't branch at the first message."),
            "the refusal is worded for a human, not quoted from the host: {:?}",
            main_notices(&world),
        );
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "edited root prompt",
            "the edited prompt is restored into the editor on a root refusal"
        );
        shut_down(&world).await;
    }

    /// A real session change (here onto a fresh session) clears the armed
    /// branch anchor, so it can never resolve against a different session's
    /// log. This drives `focus_session` rather than calling `disarm_branch`
    /// directly, so a regression removing the clear fails here.
    #[tokio::test]
    async fn focusing_a_session_clears_the_armed_anchor() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        {
            let sh = shell.borrow();
            arm_branch(&sh.branch_anchor, "m1".to_string());
        }
        assert!(
            shell.borrow().branch_anchor.borrow().is_some(),
            "armed before the change"
        );

        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved));

        assert!(
            shell.borrow().branch_anchor.borrow().is_none(),
            "the change clears the armed anchor"
        );
        shut_down(&world).await;
    }

    /// A branch that resolved auto-submits the prompt handed to it as the new
    /// branch's first turn, under the confirmation that says so.
    ///
    /// The restore path (a head that did not move) is covered by
    /// `a_stale_head_keeps_the_session_and_restores_the_prompt`.
    #[tokio::test]
    async fn a_branch_submits_the_handed_off_prompt() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "seed").await;
        let head = world
            .handles()
            .log
            .lock()
            .await
            .head()
            .cloned()
            .expect("a persisted head");

        // The handed-off prompt has to still be running when we look.
        install_busy_script(world.handles());
        apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Branch {
                target: HeadTarget::Entry(head),
                prompt: Some("branch turn".to_string()),
            },
        )
        .await;

        fold_ready_frames(&mut world);
        assert!(world.client().working(), "the branch prompt ran as a turn");
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n == "Branched the conversation from an earlier message."),
            "under the branch-with-prompt confirmation: {:?}",
            main_notices(&world),
        );
        assert_eq!(
            shell.borrow().editor.borrow().text(),
            "",
            "the prompt was submitted, not restored",
        );

        settle(&mut world).await;
        shut_down(&world).await;
    }

    // ---- Connect mode (spec 9.1, 11.7) ----

    /// A scripted host served on a loopback control port, for the connect-mode
    /// tests: the same composition a local run builds, reached over the real
    /// HTTP stack.
    struct RemoteHost {
        host: aj_app::host::SessionHost,
        server: crate::remote::RemoteServer,
    }

    impl RemoteHost {
        async fn start(dir: &TempDir, demo: &str) -> RemoteHost {
            let args = Args::parse_from(["aj", "--scripted", demo]);
            let auth = AuthStorage::new(dir.path().join("auth.json"));
            let persistence = ConversationPersistence::new(dir.path().join("sessions"));
            let ComposedHost { host, .. } =
                compose_host(&args, layers_spilling_into(dir), &auth, &persistence, None)
                    .expect("compose a host");
            let server = crate::remote::RemoteServer::bind(
                host.clone(),
                "127.0.0.1:0".parse().expect("a loopback address"),
                crate::remote::IdentityGate::local(),
            )
            .await
            .expect("bind a loopback control port");
            RemoteHost { host, server }
        }

        fn url(&self) -> String {
            self.server.url()
        }

        /// Host first: an attached stream ends when the host closes it.
        async fn shutdown(self) {
            self.host.shutdown().await;
            self.server.shutdown().await;
        }
    }

    /// An initialized app over `shell`, for the paths that need an
    /// `AsyncApp` but drive the world themselves.
    async fn app_over(shell: &Rc<RefCell<Shell>>) -> (AsyncApp, PipeWriter, WidgetRef) {
        let (reader, mut writer) = std::io::pipe().expect("pipe");
        writer.write_all(b"\x1b[?c").expect("write DA1 reply");
        let root: WidgetRef = to_widget_ref(Rc::clone(shell));
        let mut app = AsyncApp::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
            reader.into(),
        );
        app.init(Rc::clone(&root), Options::default())
            .await
            .expect("init");
        (app, writer, root)
    }

    /// The config a stock connect-mode test client runs with: the built-in
    /// one, untouched. Nothing in it is *stated*, so nothing travels with a
    /// create and the host defaults every axis against its own model (spec
    /// section 8), which is what lets these tests run against a scripted host
    /// whose model supports no thinking effort at all.
    fn client_config() -> Config {
        Config::default()
    }

    /// A client whose user wrote nothing in any config file.
    fn nothing_stated() -> crate::connect::Stated {
        use aj_conf::ConfigLayer;
        crate::connect::Stated::new(ConfigLayer::default(), ConfigLayer::default())
    }

    /// A client whose user wrote `key = value` in their config file.
    fn stated(key: &str, value: &str) -> crate::connect::Stated {
        use aj_conf::ConfigLayer;
        let mut layer = ConfigLayer::default();
        layer
            .set_str(key, value)
            .unwrap_or_else(|err| panic!("fixture sets {key:?}: {err}"));
        crate::connect::Stated::new(layer, ConfigLayer::default())
    }

    /// Dial `remote` the way `aj connect <url> [args...]` does.
    async fn dial(
        remote: &RemoteHost,
        config: &Config,
        stated: &crate::connect::Stated,
        argv: &[&str],
    ) -> Result<crate::connect::Connected> {
        let mut args = vec!["aj", "connect"];
        let url = remote.url();
        args.push(&url);
        args.extend_from_slice(argv);
        let args = Args::parse_from(args);
        let Some(CliCommand::Connect {
            url,
            session_id,
            new,
            ..
        }) = &args.command
        else {
            panic!("connect args parse as connect");
        };
        crate::connect::connect(
            &args,
            config,
            stated,
            ConnectTarget {
                url,
                session_id: session_id.as_deref(),
                new: *new,
            },
        )
        .await
    }

    /// Build the connect-mode world `aj connect <url> [args...]` builds, with
    /// this client's own config and store confined to `dir`.
    async fn connect_world(dir: &TempDir, remote: &RemoteHost, argv: &[&str]) -> World {
        let mut args = vec!["aj", "connect"];
        let url = remote.url();
        args.push(&url);
        args.extend_from_slice(argv);
        let args = Args::parse_from(args);
        // A stock install states nothing beyond its argv, so the host defaults
        // every axis it is not told about.
        let connected = dial(remote, &client_config(), &nothing_stated(), argv)
            .await
            .expect("connect to the scripted host");
        let auth = AuthStorage::new(dir.path().join("client-auth.json"));
        let persistence = ConversationPersistence::new(dir.path().join("client-sessions"));
        build_connect_world(&args, connected, default_layers(), &[], &auth, &persistence)
            .await
            .expect("build the connect-mode world")
    }

    /// A connect-mode world plus a Shell over it, for the action paths.
    async fn connect_world_and_shell(
        dir: &TempDir,
        remote: &RemoteHost,
        argv: &[&str],
    ) -> (World, Rc<RefCell<Shell>>) {
        let world = connect_world(dir, remote, argv).await;
        let shell = Rc::new(RefCell::new(Shell::new(
            Rc::clone(&world.chat),
            Rc::clone(&world.status),
            None,
            ThemeHandle::new(Theme::bundled_dark_with_mode(ColorMode::Truecolor)),
            "aj".to_string(),
            world.session(),
            PathBuf::from("/tmp"),
        )));
        (world, shell)
    }

    /// A canonical state's transcript entries per agent, with the notices a
    /// client rendered itself dropped.
    ///
    /// Those notices (the connect summary, a reconnect) belong to one client's
    /// own session with the host, so two clients of one session legitimately
    /// differ on them. Everything the host published has to agree.
    fn host_entries(
        state: &CanonicalState,
    ) -> Vec<(AgentId, Vec<aj_app::test_support::CanonicalEntry>)> {
        state
            .agents
            .iter()
            .map(|agent| {
                let entries = agent
                    .entries
                    .iter()
                    .filter(|entry| {
                        !matches!(entry, aj_app::test_support::CanonicalEntry::Notice { .. })
                    })
                    .cloned()
                    .collect();
                (agent.agent, entries)
            })
            .collect()
    }

    /// The assistant rows of a world's Main transcript, in order.
    fn assistant_rows(world: &World) -> Vec<String> {
        world
            .chat
            .borrow()
            .transcript(AgentId::Main)
            .map(|transcript| {
                transcript
                    .entries()
                    .iter()
                    .filter_map(|entry| match &entry.kind {
                        EntryKind::Assistant(assistant) => Some(assistant_text(&assistant.message)),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The footer row of a shell, which is where the host's settings surface
    /// for a remote client.
    fn footer_row(shell: &Rc<RefCell<Shell>>) -> String {
        let footer = Rc::clone(&shell.borrow().footer);
        crate::test_support::rows(
            &footer
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(120, None)),
        )
        .join("\n")
    }

    /// The sidebar's rows as the composed shell paints them, read out of the
    /// root surface's leftmost columns.
    ///
    /// Drawn through the root on purpose. Asking the strip widget to draw itself
    /// proves the widget works while saying nothing about whether the layout
    /// still contains it, and leaving it out of the layout is exactly the defect
    /// worth catching.
    ///
    /// The separator column is dropped: it runs the strip's whole height, so
    /// keeping it would make every blank line below the strip's content look
    /// like a row.
    fn sidebar_rows(shell: &Rc<RefCell<Shell>>) -> Vec<String> {
        painted_rows(shell, 100, 40)
            .into_iter()
            .map(|row| {
                row.chars()
                    .take(usize::from(SIDEBAR_COLS) - 1)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .filter(|row| !row.is_empty())
            .collect()
    }

    /// Every painted line of the composed shell, at the given terminal size.
    fn painted_rows(shell: &Rc<RefCell<Shell>>, width: u16, height: u16) -> Vec<String> {
        let root: WidgetRef = to_widget_ref(Rc::clone(shell));
        crate::test_support::rows(&draw_widget(
            &root,
            &crate::test_support::draw_ctx(width, Some(height)),
        ))
    }

    /// The columns the strip takes off the transcript, measured by where the
    /// header lands in the painted frame rather than by asking the widget.
    fn sidebar_width(shell: &Rc<RefCell<Shell>>) -> u16 {
        sidebar_width_at(shell, 100)
    }

    /// The strip's drawn width at a given terminal width.
    fn sidebar_width_at(shell: &Rc<RefCell<Shell>>, width: u16) -> u16 {
        let painted = painted_rows(shell, width, 40);
        let header = painted
            .iter()
            .find(|row| row.contains(APP_TITLE))
            .expect("the header is painted");
        // Where the header's own text starts, in columns, rather than where the
        // line stops being blank: the strip paints a separator rule in its last
        // column, and that rule is a multi-byte grapheme.
        let at = header.find(APP_TITLE).expect("the header names the app");
        let indent = header[..at].chars().count();
        u16::try_from(indent).expect("an indent within a terminal width")
    }

    /// The bytes a terminal sends for an action's own default chord.
    ///
    /// Only the alt+char and plain-char classes, which is all the sidebar
    /// gestures use. `every_default_chord_survives_the_terminal` is what proves
    /// a chord is typeable at all.
    fn chord_bytes(action: AjAction) -> Vec<u8> {
        let chord = aj_app::actions::parse_chord(
            aj_app::keybindings::effective_chord(action.action_id().expect("a bound action"))
                .expect("a default chord"),
        )
        .expect("the chord parses");
        let aj_app::actions::ChordKey::Char(c) = chord.key else {
            panic!("{action:?} is not a plain-character chord");
        };
        assert!(!chord.ctrl, "{action:?} is not a ctrl chord");
        let mut bytes = Vec::new();
        if chord.alt {
            bytes.push(0x1b);
        }
        bytes.push(u8::try_from(c).expect("an ascii chord key"));
        bytes
    }

    /// Poll until the sidebar's row for `session` satisfies `wanted`, folding
    /// and re-mirroring each time round. The host debounces `list` frames, so a
    /// row's status arrives a coalescing tick behind the change.
    async fn poll_row(
        world: &mut World,
        shell: &Rc<RefCell<Shell>>,
        session: &str,
        wanted: impl Fn(&SidebarRow) -> bool,
    ) -> bool {
        let deadline = Instant::now() + SETTLE_DEADLINE;
        while Instant::now() < deadline {
            fold_ready_frames(world);
            sync_sidebar(world, shell);
            if shell
                .borrow()
                .sidebar
                .borrow()
                .rows
                .iter()
                .any(|row| row.id == session && wanted(row))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// A session with a turn running wears the working glyph, and a session that
    /// moved on while the user was looking elsewhere wears the unseen one (spec
    /// 6.8, 9.2).
    #[tokio::test]
    async fn a_row_shows_what_its_session_is_doing() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        let first = world.session().to_string();

        let other = world
            .control
            .create(None, None, None)
            .await
            .expect("a second session");
        // Its row has to exist before the visit below: the viewed stamp is the
        // one the row carried when the user left, so leaving a session the rows
        // do not mention yet records nothing and the never-viewed rule answers
        // for it forever after.
        assert!(
            poll_row(&mut world, &shell, &other, |_| true).await,
            "the second session never appeared in the rows",
        );

        // Visit it and come back, which is what records the stamp.
        let (mut app, _writer, _root) = app_over(&shell).await;
        for target in [other.clone(), first.clone()] {
            let moved =
                apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Resume(target))
                    .await;
            assert!(matches!(moved, Focus::Moved));
        }
        assert!(
            poll_row(&mut world, &shell, &other, |row| row.status
                == RowStatus::Idle)
            .await,
            "the session starts idle",
        );

        // Work in it while the user is elsewhere.
        world
            .host()
            .command(
                &other,
                Command::Prompt {
                    agent: AgentId::Main,
                    content: vec![UserContent::text("while away")],
                },
            )
            .await
            .expect("the backgrounded session accepts a prompt");
        // What remains once it stops is that it moved since the user looked. The
        // working glyph is not asserted here: a scripted turn can finish inside
        // one `list` coalescing tick, so whether any frame reports it running is
        // a race. `a_rows_status_follows_its_precedence` owns that rule.
        assert!(
            poll_row(&mut world, &shell, &other, |row| row.status
                == RowStatus::Unseen)
            .await,
            "output the user has not seen shows as unseen: {:?}",
            sidebar_rows(&shell),
        );

        // And the focused session, which the user is looking at, never does.
        assert!(
            shell
                .borrow()
                .sidebar
                .borrow()
                .rows
                .iter()
                .any(|row| row.id == first && row.focused && row.status == RowStatus::Idle),
            "the focused row is idle and marked focused: {:?}",
            sidebar_rows(&shell),
        );
        shut_down(&world).await;
    }

    /// The style of each painted strip line's first label cell, paired with the
    /// line's text. Reads the composited frame, so it sees what a terminal
    /// would.
    ///
    /// One entry per painted line, the blank ones below the strip's content
    /// included, so an index into this is the line index the layout produced.
    fn sidebar_row_styles(shell: &Rc<RefCell<Shell>>) -> Vec<(String, vaxis::cell::Style)> {
        let root: WidgetRef = to_widget_ref(Rc::clone(shell));
        let surface = draw_widget(&root, &crate::test_support::draw_ctx(100, Some(40)));
        crate::test_support::flatten(&surface)
            .iter()
            .filter_map(|row| {
                let strip: Vec<_> = row.iter().take(usize::from(SIDEBAR_COLS)).collect();
                let text: String = strip.iter().map(|c| c.char.grapheme()).collect();
                // Column 3 is the label's first cell: marker, glyph, space,
                // then the label field.
                strip
                    .get(3)
                    .map(|cell| (text.trim_end().to_string(), cell.style))
            })
            .collect()
    }

    /// The focused row is drawn differently from the rest, so the user can see
    /// which session they are in without reading the header.
    #[tokio::test]
    async fn the_focused_row_is_marked_apart_from_the_others() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        world
            .control
            .create(None, None, None)
            .await
            .expect("a second session");
        let deadline = Instant::now() + SETTLE_DEADLINE;
        loop {
            fold_ready_frames(&mut world);
            sync_sidebar(&world, &shell);
            if shell.borrow().sidebar.borrow().rows.len() >= 2 {
                break;
            }
            assert!(Instant::now() < deadline, "the rows never arrived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // By row position, not by label: two sessions minted in the same second
        // share a time-of-day label, so matching on text would be a coin flip.
        let focused_index = {
            let sidebar = Rc::clone(&shell.borrow().sidebar);
            let state = sidebar.borrow();
            state
                .rows
                .iter()
                .position(|row| row.focused)
                .expect("a focused row")
        };

        // Two sessions show the strip by default, so nothing has to be toggled.
        let styles = sidebar_row_styles(&shell);
        assert!(styles.len() >= 2, "two rows are painted: {styles:?}");
        let focused_style = styles[focused_index].1;
        for (index, (text, style)) in styles.iter().enumerate() {
            if index == focused_index {
                continue;
            }
            assert_ne!(
                focused_style, *style,
                "row {index} ({text:?}) is drawn the same as the focused row",
            );
        }
        shut_down(&world).await;
    }

    /// The strip's focus and working-set axes, read off the composed frame:
    /// the marker in the leftmost column says which session is on screen, and
    /// the label's brightness says what the client holds open (spec 9.2).
    ///
    /// The three brightnesses need all three working-set states at once, which
    /// is what the visit-and-return is for: it leaves the second session
    /// attached in the background while the third is only listed.
    #[tokio::test]
    async fn the_strip_paints_focus_and_the_working_set() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        run_prompt(&mut world, "seed").await;
        let focused = world.session().to_string();
        let background = world
            .control
            .create(None, None, None)
            .await
            .expect("a second session");
        let listed = world
            .control
            .create(None, None, None)
            .await
            .expect("a third session");
        for session in [&background, &listed] {
            assert!(
                poll_row(&mut world, &shell, session, |_| true).await,
                "{session} never appeared in the rows",
            );
        }

        let (mut app, _writer, _root) = app_over(&shell).await;
        for target in [background.clone(), focused.clone()] {
            let moved =
                apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Resume(target))
                    .await;
            assert!(matches!(moved, Focus::Moved));
        }
        sync_sidebar(&world, &shell);

        // By line, not by label: sessions minted in the same second share a
        // time-of-day label, so matching on text would be a coin flip. With no
        // hosts in play the strip draws no headers, so a row's position in the
        // mirror is its line.
        let line_of = |session: &str| {
            let sidebar = Rc::clone(&shell.borrow().sidebar);
            let state = sidebar.borrow();
            state
                .rows
                .iter()
                .position(|row| row.id == session)
                .unwrap_or_else(|| panic!("{session} has a row"))
        };
        let styles = TranscriptStyles::from_theme(
            &shell.borrow().theme.read(),
            crate::terminal::TerminalCaps::default(),
        );
        let painted = sidebar_row_styles(&shell);
        assert_eq!(
            painted[line_of(&focused)].1,
            styles.accent,
            "the session on screen is the accent: {painted:?}",
        );
        assert_eq!(
            painted[line_of(&background)].1,
            styles.text,
            "one attached in the background is plain text: {painted:?}",
        );
        assert_eq!(
            painted[line_of(&listed)].1,
            styles.dim,
            "one the client has only listed is dim: {painted:?}",
        );

        let marked: Vec<usize> = painted_rows(&shell, 100, 40)
            .iter()
            .enumerate()
            .filter(|(_, row)| row.starts_with('▌'))
            .map(|(line, _)| line)
            .collect();
        assert_eq!(
            marked,
            vec![line_of(&focused)],
            "exactly the focused line wears the marker: {:?}",
            sidebar_rows(&shell),
        );
        shut_down(&world).await;
    }

    /// Resuming the session already focused does nothing at all.
    ///
    /// Not merely "moves nowhere": running the switch body would fold a notice
    /// for a switch that did not happen, discard an armed branch anchor, reset
    /// the scroll, and count the session twice in the exit banner.
    #[tokio::test]
    async fn resuming_the_focused_session_changes_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "seed").await;
        let session = world.session().to_string();
        let head = world
            .handles()
            .log
            .lock()
            .await
            .latest_leaf(ThreadFilter::USER)
            .expect("a persisted user message");
        arm_branch(&shell.borrow().branch_anchor, head.clone());
        let before = main_notices(&world).len();

        let moved = apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Resume(session.clone()),
        )
        .await;

        assert!(matches!(moved, Focus::Same), "the focus did not move");
        assert_eq!(world.session(), session, "and the session is untouched");
        assert_eq!(
            main_notices(&world).len(),
            before,
            "no switch notice: {:?}",
            main_notices(&world),
        );
        assert_eq!(
            shell.borrow().branch_anchor.borrow().clone(),
            Some(head),
            "an armed branch survives a switch that did not happen",
        );
        shut_down(&world).await;
    }

    /// Two stepping chords in a row walk two sessions.
    ///
    /// The second keystroke is the one that bites. A session request breaks out
    /// of the input arm, which sits above the per-iteration mirror refresh, so a
    /// key already buffered when the loop is re-entered is handled before the
    /// rows have caught up with the switch. Answered off the stale mirror, the
    /// step names the session just landed on, and the user holding the chord
    /// walks exactly one session and jams there.
    ///
    /// Driven through the real loop, because what is under test is where the
    /// refresh sits relative to the input arm.
    #[tokio::test]
    async fn holding_the_step_chord_keeps_walking() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "seed").await;
        for _ in 0..2 {
            world
                .control
                .create(None, None, None)
                .await
                .expect("another session");
        }
        let deadline = Instant::now() + SETTLE_DEADLINE;
        loop {
            fold_ready_frames(&mut world);
            sync_sidebar(&world, &shell);
            if shell.borrow().sidebar.borrow().rows.len() >= 3 {
                break;
            }
            assert!(Instant::now() < deadline, "the rows never arrived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let mut theme_watch = inert_theme_watch();
        let mut prompt_history_rx: Option<UnboundedReceiver<Vec<String>>> = None;
        let mut autocomplete_rx = shell
            .borrow()
            .editor
            .borrow_mut()
            .take_autocomplete_rx()
            .expect("editor hands out its autocomplete receiver once");

        // Both chords are in the buffer before the loop reads either, which is
        // what a held key does.
        let chord = chord_bytes(AjAction::SessionNext);
        writer.write_all(&chord).expect("first chord");
        writer.write_all(&chord).expect("second chord");

        let mut visited = vec![world.session().to_string()];
        for step in 0..2 {
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
            let SessionExit::Switch(target) = exit else {
                panic!("step {step} did not break out for a switch");
            };
            let moved =
                apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Resume(target))
                    .await;
            assert!(matches!(moved, Focus::Moved), "step {step} moved");
            // No mirror refresh here on purpose: the loop's own is what has to
            // be in the right place.
            visited.push(world.session().to_string());
        }

        let unique: std::collections::BTreeSet<&String> = visited.iter().collect();
        assert_eq!(
            unique.len(),
            3,
            "two steps visit two new sessions, not the same one twice: {visited:?}",
        );
        shut_down(&world).await;
    }

    /// An explicit toggle outranks the row-count default for the rest of the
    /// process, in both directions (spec 9.2).
    ///
    /// The mirror runs every drive-loop iteration and is what applies the
    /// default, so the claim is only worth anything across a re-sync.
    #[tokio::test]
    async fn an_explicit_toggle_survives_the_mirror() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        let mut toggle = async || {
            writer
                .write_all(&chord_bytes(AjAction::SidebarToggle))
                .expect("write chord");
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        };

        // The rows arrive with the first `list` frame, and the default only has
        // something to say once they do.
        let deadline = Instant::now() + SETTLE_DEADLINE;
        loop {
            fold_ready_frames(&mut world);
            sync_sidebar(&world, &shell);
            if !shell.borrow().sidebar.borrow().rows.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "the first row never arrived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Asked for, with one session, which the default would hide.
        toggle().await;
        sync_sidebar(&world, &shell);
        assert_eq!(shell.borrow().sidebar.borrow().rows.len(), 1, "one session");
        assert!(
            shell.borrow().sidebar.borrow().shown(),
            "the ask outlives the mirror that would hide it",
        );

        // Dismissed, with two sessions, which the default would show.
        toggle().await;
        world
            .control
            .create(None, None, None)
            .await
            .expect("a second session");
        let deadline = Instant::now() + SETTLE_DEADLINE;
        loop {
            fold_ready_frames(&mut world);
            sync_sidebar(&world, &shell);
            if shell.borrow().sidebar.borrow().rows.len() >= 2 {
                break;
            }
            assert!(Instant::now() < deadline, "the rows never arrived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !shell.borrow().sidebar.borrow().shown(),
            "a user who dismissed the strip does not get it back when a session appears",
        );
        shut_down(&world).await;
    }

    /// On a terminal with no width to spare the strip holds itself back, rather
    /// than taking its fixed width off a transcript that has none to give.
    ///
    /// The user's ask survives: it is the drawn width that yields, not
    /// `visible`, so widening the terminal brings the strip back.
    #[tokio::test]
    async fn a_narrow_terminal_holds_the_strip_back() {
        let dir = TempDir::new().expect("tempdir");
        let (world, shell) = world_and_shell(&dir, "streaming-text").await;
        shell.borrow().sidebar.borrow_mut().visible = true;
        shell.borrow().sidebar.borrow_mut().toggled = true;

        let narrow = painted_rows(&shell, MIN_COLS_WITH_SIDEBAR - 1, 20);
        let header = narrow
            .iter()
            .find(|row| row.contains("aj"))
            .expect("the header is painted even when narrow");
        assert_eq!(
            header.len() - header.trim_start().len(),
            0,
            "the transcript keeps the full width: {header:?}",
        );
        assert!(
            narrow
                .iter()
                .all(|row| row.chars().count() <= usize::from(MIN_COLS_WITH_SIDEBAR - 1)),
            "and nothing painted past the screen",
        );
        assert!(
            shell.borrow().sidebar.borrow().visible,
            "the user's ask is untouched",
        );

        // One more column than the minimum, and it comes back.
        assert_eq!(
            sidebar_width_at(&shell, MIN_COLS_WITH_SIDEBAR),
            SIDEBAR_COLS,
            "the strip returns once the width is there",
        );
        shut_down(&world).await;
    }

    /// The completion popup belongs to the editor, so it starts where the editor
    /// does. Left at column zero it would cover the strip and sit adrift of the
    /// text it completes.
    #[tokio::test]
    async fn the_completion_popup_clears_the_strip() {
        let tmp = TempDir::new().expect("tempdir");
        for n in 0..4 {
            std::fs::write(tmp.path().join(format!("file{n}.rs")), "x").expect("write file");
        }
        let (mut app, mut writer, shell, _root) = init_app_in_dir(tmp.path().to_path_buf()).await;
        shell.borrow().sidebar.borrow_mut().visible = true;
        shell.borrow().sidebar.borrow_mut().toggled = true;

        for byte in b"@file" {
            type_and_settle_autocomplete(&mut app, &mut writer, &shell, *byte).await;
        }
        assert!(
            shell.borrow().editor.borrow().is_showing_autocomplete(),
            "the completion popup is open",
        );
        let composed = shell.borrow_mut().draw(&draw_ctx(100, 30));
        let popup = popup_overlay(&composed).expect("a popup floats above the layout");
        assert_eq!(
            popup.origin.col,
            i32::from(SIDEBAR_COLS),
            "the popup starts where the editor does, clear of the strip",
        );
        assert!(
            popup.origin.col + i32::from(popup.surface.size.width) <= 100,
            "and still fits the terminal",
        );
    }

    /// The next/previous chords step the displayed order, and they do it from
    /// real key bytes through the input path a user's keystroke takes.
    ///
    /// Three rows is the smallest set that can tell the two directions apart:
    /// with two, next and previous both wrap onto the same row.
    #[tokio::test]
    async fn the_session_chords_step_the_sidebar_s_order() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        run_prompt(&mut world, "seed").await;
        let mut press = async |bytes: Vec<u8>| {
            writer.write_all(&bytes).expect("write chord");
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        };

        // One session: nothing to step to.
        sync_sidebar(&world, &shell);
        press(chord_bytes(AjAction::SessionNext)).await;
        assert!(
            shell.borrow().take_session_request().is_none(),
            "a lone session has nowhere to step",
        );

        for _ in 0..2 {
            world
                .control
                .create(None, None, None)
                .await
                .expect("another session");
        }
        let deadline = Instant::now() + SETTLE_DEADLINE;
        loop {
            fold_ready_frames(&mut world);
            sync_sidebar(&world, &shell);
            if shell.borrow().sidebar.borrow().rows.len() >= 3 {
                break;
            }
            assert!(Instant::now() < deadline, "the rows never arrived");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Read the order off the mirror and derive what each direction must
        // name, so the assertion cannot agree with a reversed implementation.
        let (order, at) = {
            let sidebar = Rc::clone(&shell.borrow().sidebar);
            let state = sidebar.borrow();
            let order: Vec<String> = state.rows.iter().map(|row| row.id.clone()).collect();
            let at = state
                .rows
                .iter()
                .position(|row| row.focused)
                .expect("a focused row");
            (order, at)
        };
        let len = order.len();
        let expected_next = order[(at + 1) % len].clone();
        let expected_prev = order[(at + len - 1) % len].clone();
        assert_ne!(
            expected_next, expected_prev,
            "three rows must distinguish the directions",
        );

        press(chord_bytes(AjAction::SessionNext)).await;
        assert_eq!(
            shell.borrow().take_session_request(),
            Some(SessionRequest::Resume(expected_next)),
            "next names the row below the focused one",
        );
        press(chord_bytes(AjAction::SessionPrev)).await;
        assert_eq!(
            shell.borrow().take_session_request(),
            Some(SessionRequest::Resume(expected_prev)),
            "previous names the row above it",
        );
        shut_down(&world).await;
    }

    /// The create chord parks the same request the `new` command does, so it
    /// reaches the loop that owns the world by the one path every session change
    /// takes.
    #[tokio::test]
    async fn the_new_session_chord_parks_a_create() {
        let dir = TempDir::new().expect("tempdir");
        let (world, shell, mut app, mut writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;

        writer
            .write_all(&chord_bytes(AjAction::SessionNew))
            .expect("write chord");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().take_session_request(),
            Some(SessionRequest::New),
        );
        shut_down(&world).await;
    }

    // ---- Session tag (spec 6.8) ----

    /// Run the two production steps the drive loop takes between a parked
    /// global action and an open overlay: the host handler, then whatever
    /// command it parked. `None` when nothing was parked.
    async fn drain_parked_action(
        world: &mut World,
        shell: &Rc<RefCell<Shell>>,
    ) -> Option<ActionEffect> {
        let action = shell.borrow().take_host_action()?;
        handle_host_action(world, shell, action).await;
        let command = shell.borrow().take_command()?;
        Some(apply_command(world, shell, command).await)
    }

    /// Send `bytes` as one keystroke and dispatch the event it decodes into.
    async fn press(app: &mut AsyncApp, writer: &mut PipeWriter, bytes: &[u8]) {
        writer.write_all(bytes).expect("write a chord");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
    }

    /// Type `text` one byte at a time, dispatching each keystroke.
    async fn type_text(app: &mut AsyncApp, writer: &mut PipeWriter, text: &str) {
        writer.write_all(text.as_bytes()).expect("write text");
        for _ in 0..text.len() {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }
    }

    /// Ctrl+U, which the one-line editor reads as "delete to start", so a test
    /// can replace a prefilled label rather than append to it.
    const CTRL_U: &[u8] = &[0x15];

    /// Pin the strip open so a test can read the label it paints. The mirror is
    /// still filled from the directory by [`sync_sidebar`].
    fn pin_sidebar_open(shell: &Rc<RefCell<Shell>>) {
        let sidebar = Rc::clone(&shell.borrow().sidebar);
        let mut state = sidebar.borrow_mut();
        state.visible = true;
        state.toggled = true;
    }

    /// Set the focused session's tag through the peer and wait for the row to
    /// come back carrying it, which is what the editor prefills from.
    async fn seed_tag(world: &mut World, shell: &Rc<RefCell<Shell>>, tag: &str) {
        let session = world.session().to_string();
        world
            .control
            .command(
                &session,
                Command::Tag {
                    tag: Some(tag.to_string()),
                },
            )
            .await
            .expect("the peer accepts the tag");
        assert!(
            poll_row(world, shell, &session, |row| row.tag.as_deref()
                == Some(tag))
            .await,
            "the peer republished the row with its label",
        );
    }

    /// The full gesture from real key bytes: the chord opens the editor
    /// prefilled with the label the session carries, and a submit relabels the
    /// row the strip paints.
    ///
    /// Nothing here calls the open function or the tag command directly. The
    /// keystroke goes through the composed tree, the action through the host
    /// handler and the command slot, and the submitted label through the same
    /// control surface a connection uses, so dropping any link fails this.
    #[tokio::test]
    async fn the_tag_chord_opens_the_prefilled_editor_and_a_submit_relabels_the_row() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        seed_tag(&mut world, &shell, "fix-auth").await;

        press(&mut app, &mut writer, &chord_bytes(AjAction::SessionTag)).await;
        let effect = drain_parked_action(&mut world, &shell)
            .await
            .expect("the chord parked the tag command");
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        focus_overlay(&mut app, &root);

        // The strip is still hidden here, so the only thing that can be
        // painting the label is the editor itself.
        let painted = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(
            painted.contains("Session tag"),
            "the editor is titled: {painted}"
        );
        assert!(
            painted.contains("fix-auth"),
            "and prefilled with the current label: {painted}",
        );

        press(&mut app, &mut writer, CTRL_U).await;
        type_text(&mut app, &mut writer, "ship-it\r").await;
        assert_eq!(shell.borrow().overlays.borrow().depth(), 0, "editor closed");
        let edit = shell
            .borrow()
            .take_tag_edit()
            .expect("the submit parked a tag edit");
        assert_eq!(edit.tag.as_deref(), Some("ship-it"));
        apply_tag_edit(&mut world, edit).await;

        pin_sidebar_open(&shell);
        let session = world.session().to_string();
        assert!(
            poll_row(&mut world, &shell, &session, |row| row.tag.as_deref()
                == Some("ship-it"))
            .await,
            "the peer republished the row under the new label",
        );
        assert!(
            strip_labels(&shell).contains(&"ship-it".to_string()),
            "and the strip paints it: {:?}",
            strip_labels(&shell),
        );
        shut_down(&world).await;
    }

    /// The palette command opens the same prefilled editor the chord does, so
    /// the two are one gesture with two triggers.
    #[tokio::test]
    async fn the_tag_palette_command_opens_the_same_prefilled_editor() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        seed_tag(&mut world, &shell, "fix-auth").await;

        // Ctrl+O opens the palette; `tag` filters to the one row and Enter
        // confirms it, which parks the command for the loop.
        press(&mut app, &mut writer, &[0x0f]).await;
        app.render(&root).expect("render");
        type_text(&mut app, &mut writer, "tag\r").await;
        let command = shell.borrow().take_command();
        assert_eq!(command, Some(CommandAction::OpenSessionTag));
        let effect = apply_command(&mut world, &shell, command.expect("a command")).await;
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        focus_overlay(&mut app, &root);

        let painted = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(
            painted.contains("Session tag") && painted.contains("fix-auth"),
            "the palette opens the prefilled editor: {painted}",
        );
        shut_down(&world).await;
    }

    /// An empty submission clears the label, which is the same "blank clears"
    /// rule the wire and the launch flag follow.
    #[tokio::test]
    async fn an_empty_tag_submission_clears_the_label() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        pin_sidebar_open(&shell);
        seed_tag(&mut world, &shell, "fix-auth").await;

        press(&mut app, &mut writer, &chord_bytes(AjAction::SessionTag)).await;
        drain_parked_action(&mut world, &shell)
            .await
            .expect("the chord parked the tag command");
        focus_overlay(&mut app, &root);

        press(&mut app, &mut writer, CTRL_U).await;
        type_text(&mut app, &mut writer, "\r").await;
        let edit = shell
            .borrow()
            .take_tag_edit()
            .expect("an empty submit still parks an edit");
        assert_eq!(edit.tag, None, "blank asks for the label to be removed");
        apply_tag_edit(&mut world, edit).await;

        let session = world.session().to_string();
        assert!(
            poll_row(&mut world, &shell, &session, |row| row.tag.is_none()).await,
            "the peer republished the row without a label",
        );
        assert!(
            !strip_labels(&shell).contains(&"fix-auth".to_string()),
            "and the strip stopped painting it: {:?}",
            strip_labels(&shell),
        );
        shut_down(&world).await;
    }

    /// The real drive loop carries the whole gesture: the chord opens the
    /// editor, the typed label submits, and the loop's own drain sends it to
    /// the peer. Every byte goes into the input buffer before the loop reads
    /// any of them, exactly as a fast typist leaves them.
    ///
    /// This is the wiring test. Nothing here reaches into the shell between
    /// keystrokes, so a missing drain, a chord that parks nothing, or an
    /// overlay that never takes focus leaves the label unset and fails here.
    /// The trailing create chord is only how the loop is made to return.
    #[tokio::test]
    async fn the_drive_loop_carries_a_tag_edit_from_the_keystroke_to_the_peer() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        let session = world.session().to_string();

        let mut theme_watch = inert_theme_watch();
        let mut prompt_history_rx: Option<UnboundedReceiver<Vec<String>>> = None;
        let mut autocomplete_rx = shell
            .borrow()
            .editor
            .borrow_mut()
            .take_autocomplete_rx()
            .expect("editor hands out its autocomplete receiver once");

        writer
            .write_all(&chord_bytes(AjAction::SessionTag))
            .expect("the tag chord");
        writer.write_all(b"from-the-loop\r").expect("the label");
        writer
            .write_all(&chord_bytes(AjAction::SessionNew))
            .expect("the chord that ends the loop");

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
        assert!(
            matches!(exit, SessionExit::New),
            "the create chord is what ends the loop",
        );

        assert!(
            poll_row(&mut world, &shell, &session, |row| row.tag.as_deref()
                == Some("from-the-loop"))
            .await,
            "the loop sent the typed label to the peer",
        );
        shut_down(&world).await;
    }

    /// The session-info page shows the label, which it can only get from the
    /// peer's row: the digest reads a log, and a tag is not in one.
    #[tokio::test]
    async fn the_session_info_page_shows_the_label() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        seed_tag(&mut world, &shell, "fix-auth").await;

        let (tx, mut rx) = unbounded_channel();
        let styles = ContentStyles::from_theme(&shell.borrow().theme.read());
        spawn_overlay_fetch(&world, FetchKind::SessionInfo, styles, &tx);
        let (kind, rows) = rx.recv().await.expect("the fetch delivered rows");
        assert_eq!(kind, FetchKind::SessionInfo);

        let page = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            page.lines()
                .any(|line| line.trim_start().starts_with("tag") && line.contains("fix-auth")),
            "the page carries a tag row: {page}",
        );
        shut_down(&world).await;
    }

    /// The session selector labels its rows from the peer's directory, so a
    /// tag typed into the filter finds the session it names.
    #[tokio::test]
    async fn the_session_selector_labels_and_indexes_a_tagged_row() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell) = world_and_shell(&dir, "streaming-text").await;
        seed_tag(&mut world, &shell, "fix-auth").await;

        let effect = apply_command(&mut world, &shell, CommandAction::OpenSessionSelector).await;
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        let scan = shell
            .borrow()
            .take_session_scan()
            .expect("the selector parked a scan");

        // The scan's own walk is off the loop, so hand it the preview the
        // store would have produced for the focused session.
        let session = world.session().to_string();
        let preview = SessionPreview {
            session_id: session.clone(),
            modified: Utc::now(),
            created_at: Utc::now(),
            last_message_at: Utc::now(),
            size_bytes: 0,
            message_count: 1,
            first_user_message: Some("a prompt".to_string()),
        };
        extend_session_scan(&scan, &[preview], Utc::now(), true, true);

        let row = scan
            .select
            .borrow()
            .selected()
            .expect("the focused session's row");
        assert!(
            row.description
                .as_deref()
                .is_some_and(|d| d.starts_with("fix-auth · ")),
            "the label leads the metadata column: {:?}",
            row.description,
        );
        assert!(
            row.filter_key.contains("fix-auth"),
            "and typing it finds the row: {}",
            row.filter_key,
        );
        shut_down(&world).await;
    }

    /// A label the store would not keep is reported in a toast and changes
    /// nothing. The editor stays open, so the refusal is not a dead end.
    #[tokio::test]
    async fn a_refused_tag_toasts_and_leaves_the_label_alone() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, mut app, mut writer, root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        seed_tag(&mut world, &shell, "fix-auth").await;

        press(&mut app, &mut writer, &chord_bytes(AjAction::SessionTag)).await;
        drain_parked_action(&mut world, &shell)
            .await
            .expect("the chord parked the tag command");
        focus_overlay(&mut app, &root);

        press(&mut app, &mut writer, CTRL_U).await;
        let overlong = "a".repeat(aj_session::MAX_TAG_BYTES + 1);
        type_text(&mut app, &mut writer, &format!("{overlong}\r")).await;

        assert!(
            shell.borrow().take_tag_edit().is_none(),
            "a refused label parks nothing for the loop",
        );
        let toasts = crate::toasts::toast_texts(&shell.borrow().toasts);
        assert!(
            toasts.iter().any(|t| t.starts_with("Tag not set:")
                && t.contains(&format!("at most {} bytes", aj_session::MAX_TAG_BYTES))),
            "the refusal is toasted in the store's own words: {toasts:?}",
        );
        assert_eq!(
            shell.borrow().overlays.borrow().depth(),
            1,
            "the editor stays open so the label can be fixed",
        );
        assert_eq!(
            focused_tag(&world).as_deref(),
            Some("fix-auth"),
            "and the session keeps the label it had",
        );
        shut_down(&world).await;
    }

    /// The rows a pointer test drives, injected straight into the mirror.
    ///
    /// The strip's content has to be known exactly for a line index to mean
    /// anything, and what fills the mirror from the directory is
    /// [`sync_sidebar`]'s business, which its own tests cover.
    fn show_sidebar(shell: &Rc<RefCell<Shell>>, rows: Vec<SidebarRow>) {
        let sidebar = Rc::clone(&shell.borrow().sidebar);
        let mut state = sidebar.borrow_mut();
        state.visible = true;
        state.toggled = true;
        state.rows = rows;
    }

    fn sidebar_row(id: &str, host: Option<&str>, focused: bool) -> SidebarRow {
        SidebarRow {
            id: id.to_string(),
            tag: None,
            host: host.map(str::to_string),
            status: RowStatus::Idle,
            focused,
            attached: focused,
        }
    }

    /// A strip of `count` sessions, the first one focused.
    fn sidebar_rows_named(count: usize) -> Vec<SidebarRow> {
        (0..count)
            .map(|at| sidebar_row(&format!("s-{at:02}"), None, at == 0))
            .collect()
    }

    /// The strip's painted lines as the composed shell draws them at the
    /// terminal size the pointer tests click against, so a line index here is
    /// the row a mouse report names.
    fn strip_lines_painted(shell: &Rc<RefCell<Shell>>) -> Vec<Vec<vaxis::cell::Cell>> {
        let root: WidgetRef = to_widget_ref(Rc::clone(shell));
        let surface = draw_widget(&root, &crate::test_support::draw_ctx(80, Some(40)));
        crate::test_support::flatten(&surface)
            .into_iter()
            .map(|row| row.into_iter().take(usize::from(SIDEBAR_COLS)).collect())
            .collect()
    }

    /// The text of each painted strip line: the status glyph and the label
    /// field, without the focus marker in the first column or the rule in the
    /// last, so a line reads as what it says.
    fn strip_labels(shell: &Rc<RefCell<Shell>>) -> Vec<String> {
        strip_lines_painted(shell)
            .iter()
            .map(|row| {
                row[1..usize::from(SIDEBAR_COLS) - 1]
                    .iter()
                    .map(|cell| cell.char.grapheme())
                    .collect::<String>()
                    .trim()
                    .to_string()
            })
            .collect()
    }

    /// The strip lines the composed frame paints with the hover band.
    fn banded_strip_lines(shell: &Rc<RefCell<Shell>>) -> Vec<usize> {
        let band = shell.borrow().chrome.borrow().select.selected_bg;
        assert_ne!(
            band,
            vaxis::cell::Color::Default,
            "an unset band would match every plain cell",
        );
        strip_lines_painted(shell)
            .iter()
            .enumerate()
            .filter(|(_, row)| row[3].style.bg == band)
            .map(|(line, _)| line)
            .collect()
    }

    /// A click on a row parks exactly what the stepping chord parks. Both go
    /// through the one function that parks a session change, so a pointer
    /// gesture triggers the action rather than reaching into the switch on its
    /// own (spec 9.2).
    ///
    /// Driven through the composed frame: the press is hit-tested against the
    /// surface the shell actually paints, so a strip left out of the layout,
    /// or one that takes no events, fails here.
    #[tokio::test]
    async fn a_click_on_a_row_parks_what_the_chord_parks() {
        let (mut app, mut writer, shell, root) = init_app().await;
        show_sidebar(&shell, sidebar_rows_named(3));
        app.render(&root).expect("render");
        assert_eq!(
            strip_labels(&shell)[..4],
            ["s-00", "s-01", "s-02", "+ new"],
            "the strip paints one line per row, then the create row",
        );

        writer
            .write_all(&chord_bytes(AjAction::SessionNext))
            .expect("write chord");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        let by_chord = shell.borrow().take_session_request();
        assert_eq!(
            by_chord,
            Some(SessionRequest::Resume("s-01".to_string())),
            "the chord steps to the row below the focused one",
        );

        app.handle_input(left_mouse_at(1, 2, vaxis::mouse::Type::Press));
        assert_eq!(
            shell.borrow().take_session_request(),
            by_chord,
            "and a click on that row asks for the same thing",
        );
    }

    /// A click on the create row parks the create the chord parks.
    #[tokio::test]
    async fn a_click_on_the_create_row_parks_a_create() {
        let (mut app, _writer, shell, root) = init_app().await;
        show_sidebar(&shell, sidebar_rows_named(2));
        app.render(&root).expect("render");
        assert_eq!(strip_labels(&shell)[2], "+ new");

        app.handle_input(left_mouse_at(2, 2, vaxis::mouse::Type::Press));
        assert_eq!(
            shell.borrow().take_session_request(),
            Some(SessionRequest::New),
        );
    }

    /// A host header and the overflow count report rather than offer, so a
    /// click on either asks for nothing.
    #[tokio::test]
    async fn a_click_on_a_header_or_the_overflow_count_parks_nothing() {
        let (mut app, _writer, shell, root) = init_app().await;
        show_sidebar(
            &shell,
            vec![
                sidebar_row("s-00", Some("laptop"), true),
                sidebar_row("s-01", Some("builder-1"), false),
            ],
        );
        app.render(&root).expect("render");
        assert!(
            strip_labels(&shell)[0].starts_with("~ laptop"),
            "a header leads the strip: {:?}",
            strip_labels(&shell),
        );
        app.handle_input(left_mouse_at(0, 2, vaxis::mouse::Type::Press));
        assert!(shell.borrow().take_session_request().is_none());

        // More rows than the terminal has lines, so the strip counts what it
        // left out on the line above the create row.
        show_sidebar(&shell, sidebar_rows_named(60));
        app.render(&root).expect("render");
        let labels = strip_labels(&shell);
        assert!(
            labels[38].ends_with("more"),
            "the overflow count sits above the create row: {labels:?}",
        );
        app.handle_input(left_mouse_at(38, 2, vaxis::mouse::Type::Press));
        assert!(shell.borrow().take_session_request().is_none());
    }

    /// An overlay floating above the strip keeps the pointer. The scrim is the
    /// deepest hit, so it consumes the press at target while the strip only
    /// sees it in the capturing phase, where the strip stays out of the way
    /// and drops its band.
    ///
    /// NOTE: the band is read after the overlay closes. The test compositor
    /// blits a child's whole grid, blank cells included, so a full-viewport
    /// scrim wipes the base content there in a way a real render (which skips
    /// an empty buffer) does not.
    #[tokio::test]
    async fn an_overlay_above_the_strip_keeps_the_click() {
        let (mut app, mut writer, shell, root) = init_app().await;
        show_sidebar(&shell, sidebar_rows_named(3));
        app.render(&root).expect("render");
        app.handle_input(motion_at(1, 2));
        app.render(&root).expect("render");
        assert_eq!(banded_strip_lines(&shell), vec![1], "the row is banded");

        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(shell.borrow().overlays.borrow().is_open(), "the palette");
        app.render(&root).expect("render");

        app.handle_input(left_mouse_at(1, 2, vaxis::mouse::Type::Press));
        assert!(
            shell.borrow().take_session_request().is_none(),
            "the press belongs to the overlay, not to the strip",
        );

        writer.write_all(b"\x1b").expect("write esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(!shell.borrow().overlays.borrow().is_open(), "esc closes it");
        app.render(&root).expect("render");
        assert!(
            banded_strip_lines(&shell).is_empty(),
            "and the strip comes back with no band on it: {:?}",
            banded_strip_lines(&shell),
        );
    }

    /// Hovering a row asks for exactly one frame, and a pointer holding still
    /// asks for none.
    ///
    /// The band lives in the widget and the widget's identity is stable across
    /// frames, so the enter/leave diffing the runtime runs against every fresh
    /// surface finds nothing to say. A band whose surface got a new identity
    /// each frame, or one that redrew for every pointer report, would keep the
    /// frame loop awake for as long as the pointer rested on the strip.
    #[tokio::test]
    async fn hovering_a_row_settles_in_one_frame() {
        let (mut app, _writer, shell, root) = init_app().await;
        show_sidebar(&shell, sidebar_rows_named(3));
        app.render(&root).expect("render");
        assert!(!app.needs_redraw(), "the frame starts settled");

        app.handle_input(motion_at(1, 2));
        assert!(app.needs_redraw(), "the band moved onto the row");
        app.render(&root).expect("render");
        assert!(
            !app.needs_redraw(),
            "one frame settles it: the hover does not feed itself",
        );
        assert_eq!(banded_strip_lines(&shell), vec![1]);

        app.handle_input(motion_at(1, 2));
        assert!(
            !app.needs_redraw(),
            "a pointer holding still asks for nothing",
        );

        // Onto a blank line below the strip's content, which is no row at all.
        app.handle_input(motion_at(39, 2));
        assert!(app.needs_redraw(), "the band had to come off");
        app.render(&root).expect("render");
        assert!(!app.needs_redraw(), "and that settled in one frame too");
        assert!(banded_strip_lines(&shell).is_empty());
    }

    /// The wheel over the strip scrolls it. The offset's own rules (where it
    /// stops, what a focus change does to it) are the strip's, this is the
    /// wiring.
    #[tokio::test]
    async fn the_wheel_over_the_strip_scrolls_it() {
        let (mut app, _writer, shell, root) = init_app().await;
        show_sidebar(&shell, sidebar_rows_named(60));
        app.render(&root).expect("render");
        assert_eq!(strip_labels(&shell)[0], "s-00");

        app.handle_input(wheel_down_at(1, 2));
        app.render(&root).expect("render");
        assert_eq!(
            strip_labels(&shell)[0],
            "s-01",
            "the wheel moved the run down a row",
        );

        app.handle_input(wheel_up_at(1, 2));
        app.render(&root).expect("render");
        assert_eq!(strip_labels(&shell)[0], "s-00", "and back up again");

        // A strip whose rows all fit has nowhere to scroll.
        show_sidebar(&shell, sidebar_rows_named(3));
        app.render(&root).expect("render");
        app.handle_input(wheel_down_at(1, 2));
        app.render(&root).expect("render");
        assert_eq!(strip_labels(&shell)[..3], ["s-00", "s-01", "s-02"]);
    }

    /// With one session the strip stays hidden and costs the transcript nothing,
    /// and the toggle chord shows it (spec 9.2). Measured through the composed
    /// frame, so a strip missing from the layout fails here.
    #[tokio::test]
    async fn the_sidebar_is_hidden_for_a_lone_session_until_the_toggle_asks() {
        let dir = TempDir::new().expect("tempdir");
        let (world, shell, mut app, mut writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        let mut toggle = async || {
            writer
                .write_all(&chord_bytes(AjAction::SidebarToggle))
                .expect("write chord");
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        };

        assert!(
            !shell.borrow().sidebar.borrow().shown(),
            "hidden for a lone session",
        );
        assert_eq!(
            sidebar_width(&shell),
            0,
            "and takes no width from the transcript",
        );

        toggle().await;
        assert!(
            shell.borrow().sidebar.borrow().shown(),
            "the toggle shows it"
        );
        assert_eq!(sidebar_width(&shell), SIDEBAR_COLS);

        toggle().await;
        assert!(
            !shell.borrow().sidebar.borrow().shown(),
            "and hides it again",
        );
        assert_eq!(sidebar_width(&shell), 0);
        shut_down(&world).await;
    }

    /// The connect-mode smoke test (spec 11.7): a prompt submitted over the
    /// wire streams the host's answer into the transcript, and the footer
    /// shows the host's settings rather than this client's.
    #[tokio::test]
    async fn connect_mode_prompt_streams_the_hosts_answer() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;
        let (mut world, shell) = connect_world_and_shell(&dir, &remote, &[]).await;

        // A fresh host holds nothing, so bare connect created the session.
        assert!(!world.session().is_empty());
        // The host's scripted identity, which this client's config knows
        // nothing about, so it can only have come from a `state` frame.
        let footer = footer_row(&shell);
        assert!(
            footer.contains("streaming-text"),
            "the footer shows the host's settings: {footer}"
        );

        assert!(handle_submit(&mut world, "hello".to_string()).await);
        settle(&mut world).await;

        let rows = user_rows(&world);
        assert_eq!(rows, vec!["hello"], "the prompt landed as a user row");

        let answer = assistant_rows(&world).join(" ");
        assert!(
            !answer.is_empty(),
            "the scripted answer streamed into the transcript"
        );
        assert!(
            answer.contains("plain text-only demo"),
            "the host's own script produced it: {answer}"
        );
        remote.shutdown().await;
    }

    /// Session selection per spec 9.1: an explicit id attaches it, `--new`
    /// creates, bare connect takes the host's latest, and a host with no
    /// sessions gets one created.
    #[tokio::test]
    async fn connect_mode_resolves_the_session_to_attach() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;

        // Create-when-empty: the host holds nothing yet.
        let created = connect_world(&dir, &remote, &[]).await;
        let first = created.session().to_string();
        assert!(
            remote
                .host
                .sessions()
                .await
                .expect("session list")
                .sessions
                .iter()
                .any(|entry| entry.id == first),
            "the created session is in the host's directory"
        );

        // Explicit id: attaches exactly that one.
        let explicit = connect_world(&dir, &remote, &[&first]).await;
        assert_eq!(explicit.session(), first);

        // `--new`: creates another one.
        let fresh = connect_world(&dir, &remote, &["--new"]).await;
        assert_ne!(fresh.session(), first, "--new minted a second session");
        let second = fresh.session().to_string();

        // Bare: the host's most recently modified session, which is the one
        // that just ran a turn.
        let mut working = connect_world(&dir, &remote, &[&first]).await;
        assert!(handle_submit(&mut working, "wake the older one".to_string()).await);
        settle(&mut working).await;
        let latest = connect_world(&dir, &remote, &[]).await;
        assert_eq!(
            latest.session(),
            first,
            "bare connect took the most recently modified session, not {second}"
        );
        remote.shutdown().await;
    }

    /// A stream that drops mid-turn reconnects rather than killing the shell,
    /// and converges on the same state a client attaching fresh would build.
    #[tokio::test]
    async fn connect_mode_reconnects_after_a_dropped_stream() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;
        let (mut world, shell) = connect_world_and_shell(&dir, &remote, &[]).await;
        let session = world.session().to_string();

        assert!(handle_submit(&mut world, "hello".to_string()).await);
        fold_ready_frames(&mut world);
        // Cut the connection mid-turn: the state the transport error leaves
        // the stream in.
        world.stream.cut();
        assert!(
            matches!(world.stream.recv().await, ControlFrame::Lost(_)),
            "the cut stream reports the loss"
        );

        // The drive loop's own recovery: re-attach with the client's cursor,
        // fold the block, discharge the reads.
        let mut resume = Some(Resume::lost());
        world.connection = Connection::Reconnecting;
        let deadline = Instant::now() + SETTLE_DEADLINE;
        while let Some(state) = resume.take() {
            assert!(Instant::now() < deadline, "the re-attach never settled");
            assert_eq!(
                world.connection,
                state.connection(),
                "the status line names the connection state"
            );
            resume = advance_resume(&mut world, &shell, state)
                .await
                .expect("a connection's re-attach is never fatal");
            world.connection = resume
                .as_ref()
                .map_or(Connection::Connected, Resume::connection);
        }
        assert_eq!(world.connection, Connection::Connected);
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n == "Reconnected to the host."),
            "the reconnect is surfaced: {:?}",
            main_notices(&world)
        );

        settle(&mut world).await;
        // A client that attached fresh, after the fact, is the oracle: the
        // reconnected fold has to agree with a full backfill.
        let fresh = connect_world(&dir, &remote, &[&session]).await;
        let mine = CanonicalState::of(&world.chat.borrow(), world.client());
        let theirs = CanonicalState::of(&fresh.chat.borrow(), fresh.client());
        assert_eq!(
            host_entries(&mine),
            host_entries(&theirs),
            "the reconnected client converged on the fresh attach",
        );
        assert_eq!(mine.running, theirs.running, "and on the lifecycle");
        assert_eq!(mine.tasks, theirs.tasks);
        assert_eq!(mine.queue, theirs.queue);
        assert_eq!(
            mine.agents
                .iter()
                .map(|agent| agent.settings.clone())
                .collect::<Vec<_>>(),
            theirs
                .agents
                .iter()
                .map(|agent| agent.settings.clone())
                .collect::<Vec<_>>(),
            "and on the settings the host reported",
        );
        assert_eq!(user_rows(&world), vec!["hello"], "no duplicated prompt row");
        remote.shutdown().await;
    }

    /// A loopback address with nothing behind it: bound to take a free port,
    /// then released, so a client dialing it is refused rather than left
    /// waiting.
    async fn dead_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("the bound address");
        drop(listener);
        format!("http://{addr}")
    }

    /// A read the host fails leaves the obligation standing and paces the
    /// retry.
    ///
    /// The loop discharges these reads at the bottom of every iteration and
    /// each call awaits a request, so an unpaced retry puts a request that
    /// fails into every iteration for as long as the host stays quiet.
    #[tokio::test]
    async fn a_failing_client_read_is_paced() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;
        let mut world = connect_world(&dir, &remote, &[]).await;

        // Attach without discharging the reads, which is the state every
        // `caught_up` leaves the client in (spec 6.7).
        world.stream = open_stream(&world.control, &mut world.directory)
            .await
            .expect("re-attach");
        assert!(fold_attach_block(&mut world).await, "the block completed");
        assert!(
            world.client().needs_task_refetch() && world.client().needs_queue_refetch(),
            "the block obliged both reads",
        );

        // Point the client at a peer that is not there.
        world.control =
            Control::remote(crate::remote::RemoteClient::new(&dead_url().await).expect("a client"));

        refresh_client_reads(&mut world).await;
        assert!(
            world.client().needs_task_refetch() && world.client().needs_queue_refetch(),
            "a failed read keeps the obligation",
        );
        assert!(!world.reads_retry.ready(), "the next attempt is held back");
        let due = world
            .reads_retry
            .due()
            .expect("a paced retry has a due time");

        // A call inside the delay attempts nothing: an attempt would fail and
        // pace again, moving the due time out.
        refresh_client_reads(&mut world).await;
        assert_eq!(
            world.reads_retry.due(),
            Some(due),
            "the read was re-issued inside its own delay",
        );

        // And once the delay is out it does try again, backing off further.
        tokio::time::sleep_until(due.into()).await;
        refresh_client_reads(&mut world).await;
        let grown = world.reads_retry.due().expect("a second failure paces too");
        assert!(
            grown > due,
            "the retry is not abandoned, and a repeated failure backs off",
        );

        remote.shutdown().await;
    }

    /// A gesture connect mode has no path for folds a notice naming why, rather
    /// than silently doing nothing (spec 9.1).
    ///
    /// What is left is the two that are genuinely about this machine: an export
    /// writes a file where the log is, and the session selector previews this
    /// process's own store. Session switching and creation are no longer among
    /// them, see `connect_mode_creates_and_switches_sessions`.
    #[tokio::test]
    async fn connect_mode_refuses_the_gestures_about_this_machine() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;
        let (mut world, shell) = connect_world_and_shell(&dir, &remote, &[]).await;

        // The reason is pinned, not just the fact of a refusal: each names the
        // host-local thing it cannot reach, and the selector points at what the
        // user should reach for instead.
        for (action, reason) in [
            (CommandAction::ExportHtml, "run the export there"),
            (
                CommandAction::OpenSessionSelector,
                "a connection's sessions are the ones in the sidebar",
            ),
        ] {
            let before = main_notices(&world).len();
            apply_command(&mut world, &shell, action).await;
            let notices = main_notices(&world);
            assert!(
                notices.len() > before
                    && notices
                        .last()
                        .is_some_and(|n| n.contains("over a connection") && n.contains(reason)),
                "{action:?} folds a refusal explaining {reason:?}: {notices:?}"
            );
            assert!(
                !shell.borrow().overlays.borrow().is_open(),
                "{action:?} opened no overlay"
            );
        }
        remote.shutdown().await;
    }

    /// Creating and switching sessions work over a connection, which is what the
    /// sidebar drives (spec 9.2). Both go through the control surface, so the
    /// session they land on attaches the same way it would in process.
    #[tokio::test]
    async fn connect_mode_creates_and_switches_sessions() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;
        let (mut world, shell) = connect_world_and_shell(&dir, &remote, &[]).await;
        let first = world.session().to_string();
        let (mut app, _writer, _root) = app_over(&shell).await;

        // A create over the wire mints a session on the host and focuses it.
        let moved = apply_focus_request(&mut app, &shell, &mut world, FocusRequest::Create).await;
        assert!(matches!(moved, Focus::Moved), "the create was refused");
        let created = world.session().to_string();
        assert_ne!(created, first, "a fresh session, not the one we were on");
        assert!(
            world.local.is_none(),
            "a connection holds no direct handles into the session",
        );

        // It is a real session on the host, not just a local id.
        let listed = world
            .control
            .sessions()
            .await
            .expect("the host lists its sessions")
            .sessions;
        assert!(
            listed.iter().any(|row| row.id == created && row.live),
            "the host does not have the session the create claimed: {:?}",
            listed.iter().map(|row| &row.id).collect::<Vec<_>>(),
        );

        // Work in it, so switching back has something to come back to.
        assert!(handle_submit(&mut world, "over the wire".to_string()).await);
        settle(&mut world).await;
        assert_eq!(
            user_rows(&world),
            vec!["over the wire"],
            "the created session takes a turn",
        );

        // And back to the first, which is a swap onto a transcript that kept
        // folding while we were away.
        let moved = apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Resume(first.clone()),
        )
        .await;
        assert!(matches!(moved, Focus::Moved), "the switch was refused");
        assert_eq!(world.session(), first);
        assert!(
            !user_rows(&world).iter().any(|t| t == "over the wire"),
            "the other session's turn is not in this transcript: {:?}",
            user_rows(&world),
        );
        assert!(
            world.directory.is_attached(&created),
            "the session left behind stays in the working set",
        );
        remote.shutdown().await;
    }

    /// Provenance decides what travels with a create (spec section 8). A level
    /// the user actually wrote is honored strictly, so a host whose model
    /// cannot serve it refuses in its own words, before any terminal setup. The
    /// same level left unstated does not travel at all, and the host defaults
    /// the axis against the model it really runs.
    #[tokio::test]
    async fn connect_mode_sends_only_stated_settings() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;

        let refused = dial(
            &remote,
            &client_config(),
            &stated("thinking", "xhigh"),
            &["--new"],
        )
        .await;
        let err = match refused {
            Err(err) => err,
            Ok(_) => panic!("a stated level the model cannot serve is refused"),
        };
        let reported = format!("{err:#}");
        assert!(
            reported.contains("does not support thinking level"),
            "the host's reason reaches the CLI: {reported}"
        );

        // Same client, same built-in fallback, nothing written: the create goes
        // through because no opinion was ever expressed.
        let connected = dial(&remote, &client_config(), &nothing_stated(), &["--new"])
            .await
            .expect("a stock client creates a session");
        assert!(connected.created);
        remote.shutdown().await;
    }

    /// `--tag` rides the create request a `connect --new` sends, so the host's
    /// own row carries the label. The client validates it first, which is what
    /// makes an illegal one a CLI error rather than a round trip.
    #[tokio::test]
    async fn connect_new_carries_the_tag_flag_to_the_host() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;

        let connected = dial(
            &remote,
            &client_config(),
            &nothing_stated(),
            &["--new", "--tag", "fix-auth"],
        )
        .await
        .expect("create a tagged session on the host");
        assert!(connected.created);

        let listed = remote.host.sessions().await.expect("the host's rows");
        let row = listed
            .sessions
            .iter()
            .find(|row| row.id == connected.session)
            .expect("the created session is listed");
        assert_eq!(row.tag.as_deref(), Some("fix-auth"));

        let refused = match dial(
            &remote,
            &client_config(),
            &nothing_stated(),
            &["--new", "--tag", "two\nlines"],
        )
        .await
        {
            Err(err) => err,
            Ok(_) => panic!("an illegal tag refuses the connect"),
        };
        let reported = format!("{refused:#}");
        assert!(reported.contains("--tag"), "names the flag: {reported}");
        assert_eq!(
            remote
                .host
                .sessions()
                .await
                .expect("the host's rows")
                .sessions
                .len(),
            listed.sessions.len(),
            "a refused tag costs the host no session",
        );
        remote.shutdown().await;
    }

    /// The tag gesture is the same one over a connection: the chord opens the
    /// editor prefilled from the peer's row, and the submit travels as the
    /// wire's tag request, so the host's own store ends up carrying the label.
    #[tokio::test]
    async fn the_tag_gesture_relabels_a_connected_session() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;
        let (mut world, shell) = connect_world_and_shell(&dir, &remote, &["--new"]).await;
        let (mut app, mut writer, root) = app_over(&shell).await;
        seed_tag(&mut world, &shell, "over-the-wire").await;

        press(&mut app, &mut writer, &chord_bytes(AjAction::SessionTag)).await;
        let effect = drain_parked_action(&mut world, &shell)
            .await
            .expect("the chord parked the tag command");
        assert!(matches!(effect, ActionEffect::OpenedOverlay));
        focus_overlay(&mut app, &root);
        let painted = flatten(&shell.borrow_mut().draw(&full_draw_ctx())).join("\n");
        assert!(
            painted.contains("over-the-wire"),
            "the editor prefills from the peer's row: {painted}",
        );

        press(&mut app, &mut writer, CTRL_U).await;
        type_text(&mut app, &mut writer, "renamed\r").await;
        let edit = shell
            .borrow()
            .take_tag_edit()
            .expect("the submit parked a tag edit");
        apply_tag_edit(&mut world, edit).await;

        let session = world.session().to_string();
        assert!(
            poll_row(&mut world, &shell, &session, |row| row.tag.as_deref()
                == Some("renamed"))
            .await,
            "the host republished the row under the new label",
        );
        let listed = remote.host.sessions().await.expect("the host's rows");
        assert_eq!(
            listed
                .sessions
                .iter()
                .find(|row| row.id == session)
                .and_then(|row| row.tag.as_deref()),
            Some("renamed"),
            "and the label landed on the host, not on this client",
        );
        remote.shutdown().await;
    }

    /// A command the peer refuses folds the peer's own reason, and a refusal
    /// the peer classes as a conflict keeps its local wording.
    #[tokio::test]
    async fn connect_mode_folds_a_refused_commands_reason() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;
        let (mut world, shell) = connect_world_and_shell(&dir, &remote, &[]).await;

        // A model this host has no catalog row for: the peer's wording is
        // what the user sees, because only the host can know it.
        let unservable = ModelInfo {
            provider: "nowhere".to_string(),
            id: "no-such-model".to_string(),
            ..aj_app::test_support::scripted_model_info()
        };
        let notice = confirm_model(&world, AgentId::Main, PersistAction::None, unservable)
            .await
            .expect("the host refuses a model it cannot serve");
        assert!(
            notice.contains("nowhere/no-such-model") && notice.contains("catalog"),
            "the peer's reason is folded verbatim: {notice}"
        );

        // A conflict survives the transport as one, so the busy refusal keeps
        // its local wording (the chord that cancels the turn first).
        let handles = remote
            .host
            .local_handles(world.session())
            .await
            .expect("the host's own handles");
        install_busy_script(&handles);
        assert!(handle_submit(&mut world, "keep busy".to_string()).await);
        fold_ready_frames(&mut world);
        apply_command(&mut world, &shell, CommandAction::Compact).await;
        assert!(
            main_notices(&world)
                .iter()
                .any(|n| n == &session_busy_notice("compact")),
            "{:?}",
            main_notices(&world)
        );

        cancel_viewed_turn(&world).await;
        settle(&mut world).await;
        remote.shutdown().await;
    }

    /// The task-output overlay in connect mode is backed by the per-task read:
    /// the drive loop polls it and pushes each snapshot into the open viewer
    /// (spec 6.7).
    #[tokio::test]
    async fn connect_mode_task_overlay_renders_the_per_task_read() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "background-task").await;
        let (mut world, shell) = connect_world_and_shell(&dir, &remote, &[]).await;

        assert!(handle_submit(&mut world, "run something".to_string()).await);
        // Wait for the background task to show up in the client's model.
        let deadline = Instant::now() + SETTLE_DEADLINE;
        loop {
            fold_ready_frames(&mut world);
            let known = world.chat.borrow().tasks().keys().next().copied();
            if let Some(task) = known {
                let command = match &world.chat.borrow().tasks()[&task].kind {
                    aj_agent::tool::TaskKind::Bash { command } => command.clone(),
                    aj_agent::tool::TaskKind::Agent { .. } => panic!("a bash task"),
                };
                open_task_viewer(&world, &shell, task, command);
                break;
            }
            assert!(Instant::now() < deadline, "no background task started");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            shell.borrow().task_view.borrow().is_some(),
            "the remote viewer is kept for the poll"
        );

        // The first poll fills the viewer from the read.
        let mut polled = Retry::default();
        assert!(
            poll_task_output(&world, &shell, &mut polled).await,
            "the per-task read fed the viewer"
        );
        // And it is bounded: a second poll inside the interval does nothing.
        assert!(!poll_task_output(&world, &shell, &mut polled).await);

        // Closing the viewer retires the poll: an empty overlay stack is what
        // the loop reads, since the widget cannot clear the slot itself.
        shell.borrow().overlays.borrow_mut().close_all();
        assert!(!poll_task_output(&world, &shell, &mut polled).await);
        assert!(
            shell.borrow().task_view.borrow().is_none(),
            "the closed viewer was retired"
        );
        assert!(
            polled.due().is_none(),
            "the poll clock resets with the viewer"
        );

        settle(&mut world).await;
        remote.shutdown().await;
    }

    /// Every entry from `entry` up to the log's root, `entry` included.
    async fn ancestors(
        host: &aj_app::host::SessionHost,
        session: &str,
        entry: &str,
    ) -> Vec<String> {
        let handles = host
            .local_handles(session)
            .await
            .expect("the host holds the session live");
        let log = handles.log.lock().await;
        let mut walk = vec![entry.to_string()];
        while let Some(parent) = log.parent_of(walk.last().expect("non-empty")) {
            walk.push(parent.clone());
        }
        walk
    }

    /// One stream serves every session it attached: both get their own
    /// attach block, and `attached` answers for both. This is what lets the
    /// client keep background sessions live on a single connection (spec
    /// 6.5, 9.2).
    #[tokio::test]
    async fn one_stream_carries_several_sessions() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;
        let (world, _shell) = connect_world_and_shell(&dir, &remote, &[]).await;
        let second = world
            .control
            .create(None, None, None)
            .await
            .expect("a second session");

        let mut stream = world
            .control
            .attach_all(&[
                AttachRequest {
                    session: world.session().to_string(),
                    cursor: None,
                },
                AttachRequest {
                    session: second.clone(),
                    cursor: None,
                },
            ])
            .await
            .expect("one attach covering both");
        assert!(stream.attached(world.session()));
        assert!(stream.attached(&second));

        // Both blocks arrive on the one stream. Reading until each session
        // has been caught up proves the host really served two blocks rather
        // than one, and that the frames carry the session that owns them.
        let mut caught_up: Vec<String> = Vec::new();
        let deadline = Instant::now() + SETTLE_DEADLINE;
        while caught_up.len() < 2 {
            assert!(Instant::now() < deadline, "only saw {caught_up:?}");
            match stream.recv().await {
                ControlFrame::Frame(aj_wire::Frame::CaughtUp { session, .. }) => {
                    if !caught_up.contains(&session) {
                        caught_up.push(session);
                    }
                }
                ControlFrame::Frame(_) => {}
                other => panic!(
                    "the stream ended early: {}",
                    match other {
                        ControlFrame::Lost(err) => err.to_string(),
                        _ => "closed".to_string(),
                    }
                ),
            }
        }
        caught_up.sort();
        let mut expected = vec![world.session().to_string(), second];
        expected.sort();
        assert_eq!(caught_up, expected);

        drop(stream);
        remote.shutdown().await;
    }

    /// The head the tree read carries reaches the overlay, so the row the
    /// session is on opens selected and confirming it is a no-op.
    ///
    /// Without the head the overlay falls back to its default cursor, and
    /// confirming that row parks a real branch request, so the user switches
    /// to the branch they were already on (spec 6.7).
    #[tokio::test]
    async fn the_tree_overlay_opens_on_the_head_the_read_carries() {
        let dir = TempDir::new().expect("tempdir");
        let (mut world, shell, _app, _writer, _root) =
            world_shell_app(&dir, "streaming-text", default_layers()).await;
        assert!(handle_submit(&mut world, "first".to_string()).await);
        settle(&mut world).await;

        let select = open_tree_overlay(&world, &shell)
            .await
            .expect("the tree read");
        let picked = select.borrow().selected().expect("a row is selected");
        if let Some(confirm) = select.borrow_mut().on_confirm.as_mut() {
            let mut ctx = EventContext::new();
            confirm(&mut ctx, &picked);
        }
        assert!(
            shell.borrow().take_session_request().is_none(),
            "confirming the row the session is already on parks no switch",
        );
        shut_down(&world).await;
    }

    /// A stream answers `attached` per session, for both a local host and a
    /// connection: true for what it carries and false for anything else.
    ///
    /// A client arms its attach-block fold from this (`open_stream`), so a
    /// stream that claimed every session would arm folds for blocks that
    /// never arrive, and the next on-change `state` frame would be mistaken
    /// for one. Invisible while a client holds one single-session stream,
    /// which is why it is pinned here before the sidebar holds several.
    #[tokio::test]
    async fn a_stream_reports_attachment_per_session() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;
        let (world, _shell) = connect_world_and_shell(&dir, &remote, &[]).await;

        // A second session the host really has, but this stream does not
        // carry, so "does the host know it" and "is it on this stream" cannot
        // be confused.
        let other = world
            .control
            .create(None, None, None)
            .await
            .expect("a second session");
        assert_ne!(other, world.session());

        let stream = world
            .control
            .attach_all(&[AttachRequest {
                session: world.session().to_string(),
                cursor: None,
            }])
            .await
            .expect("attach");
        assert!(
            stream.attached(world.session()),
            "the stream carries the session it named",
        );
        assert!(
            !stream.attached(&other),
            "a session this stream did not name is not attached on it",
        );
        assert!(!stream.attached("no-such-session"));
        drop(stream);

        remote.shutdown().await;
    }

    /// The tree view and the branch gesture work over a connection: the tree
    /// read carries the head the overlay pre-selects, and the branch anchor
    /// travels as a `before` target the host resolves to the message's parent
    /// (spec 6.6, 6.7).
    #[tokio::test]
    async fn connect_mode_browses_the_tree_and_branches() {
        let dir = TempDir::new().expect("tempdir");
        let remote = RemoteHost::start(&dir, "streaming-text").await;
        let (mut world, shell) = connect_world_and_shell(&dir, &remote, &[]).await;

        assert!(handle_submit(&mut world, "first".to_string()).await);
        settle(&mut world).await;

        // The tree opens against the host's read rather than a local log.
        apply_command(&mut world, &shell, CommandAction::OpenSessionTree).await;
        assert!(
            shell.borrow().overlays.borrow().is_open(),
            "the tree view opened over the connection: {:?}",
            main_notices(&world),
        );
        shell.borrow().overlays.borrow_mut().close_all();

        let tree = world
            .control
            .tree(world.session())
            .await
            .expect("the tree read");
        let head = tree.head.clone().expect("a session with a turn has a head");

        // Branch before the user message, which the host resolves to its
        // parent, so the head really moves.
        let message = world
            .chat
            .borrow()
            .transcript(AgentId::Main)
            .expect("the main transcript")
            .entries()
            .iter()
            .find_map(|entry| match &entry.kind {
                aj_app::chat::EntryKind::User(user) => user.message_id.clone(),
                _ => None,
            })
            .expect("the user message carries its entry id");
        let parent = {
            let handles = remote
                .host
                .local_handles(world.session())
                .await
                .expect("the host holds the session live");
            let log = handles.log.lock().await;
            log.parent_of(&message)
                .expect("a user message has a parent")
                .clone()
        };
        // Drive the real gesture: arm the anchor on the message and submit,
        // which is the seam a connect-mode refusal would sit at.
        {
            let sh = shell.borrow();
            arm_branch(&sh.branch_anchor, message.clone());
        }
        let outcome =
            submit_with_armed_anchor(&mut world, &shell, "edited prompt".to_string()).await;
        let ArmedSubmit::Branch { target, prompt } = outcome else {
            panic!(
                "an armed submit branches over a connection: {:?}",
                main_notices(&world)
            );
        };
        assert!(
            matches!(&target, HeadTarget::Before(entry) if entry == &message),
            "the anchor travels as the message to branch before",
        );

        let (mut app, _writer, _root) = app_over(&shell).await;
        apply_focus_request(
            &mut app,
            &shell,
            &mut world,
            FocusRequest::Branch {
                target,
                prompt: Some(prompt),
            },
        )
        .await;
        assert!(
            main_notices(&world)
                .iter()
                .any(|notice| notice.contains("Branched the conversation")),
            "{:?}",
            main_notices(&world),
        );

        // A branch replaces the message it was taken from rather than
        // continuing after it (spec 6.6), so the original prompt is gone from
        // the branch's transcript and the edited one stands in its place. An
        // `entry` target would land on the message and keep both.
        settle(&mut world).await;
        // The branch's prompt reaches this client over the stream the reset
        // obliged it to reopen, and the host can still be appending it when
        // that attach block is generated. So wait for a prompt to land rather
        // than assume the block carried one. Waiting for any prompt, not the
        // expected one: a branch that wrongly kept both messages satisfies
        // this and then fails the assertion below, which is the point.
        let deadline = Instant::now() + SETTLE_DEADLINE;
        let prompts = loop {
            fold_ready_frames(&mut world);
            let prompts: Vec<String> = world
                .chat
                .borrow()
                .transcript(AgentId::Main)
                .expect("the main transcript")
                .entries()
                .iter()
                .filter_map(|entry| match &entry.kind {
                    aj_app::chat::EntryKind::User(user) => Some(user.joined_text()),
                    _ => None,
                })
                .collect();
            if !prompts.is_empty() {
                break prompts;
            }
            assert!(
                Instant::now() < deadline,
                "the branch's transcript never reached the client",
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert_eq!(
            prompts,
            vec!["edited prompt".to_string()],
            "the branch replaced the message it was taken from",
        );

        // And the head really moved off it, onto the new branch below the
        // parent the host resolved.
        let moved = world
            .control
            .tree(world.session())
            .await
            .expect("the tree read")
            .head
            .expect("a head");
        assert_ne!(moved, head, "the head moved off the branched-from message");
        assert_ne!(moved, message, "the head is not the message itself");
        assert!(
            ancestors(&remote.host, world.session(), &moved)
                .await
                .contains(&parent),
            "the new branch hangs off the message's parent",
        );

        settle(&mut world).await;
        remote.shutdown().await;
    }
}
