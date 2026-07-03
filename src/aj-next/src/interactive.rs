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
use aj_agent::queue::MessageQueues;
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
    AsyncApp, DrawContext, Event, EventContext, FilterableSelect, FlexColumn, FlexItem, MaxSize,
    Options, OverlayWindow, RelativePoint, Size, SubSurface, Surface, Text, TextField, UserEvent,
    Widget, WidgetRef, draw_widget, to_widget_ref,
};

use crate::footer::FooterLine;
use crate::overlay::{
    OpenOverlay, OverlayChrome, OverlayPlacement, OverlayStack, Scrim, close_top,
    placeholder_palette_items,
};
use crate::pending::PendingBox;
use crate::status::{STATUS_WAKE_EVENT, StatusLine, StatusState};
use crate::transcript::{TranscriptStyles, TranscriptView};

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
        status: Rc::new(RefCell::new(StatusState::default())),
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
/// both spawn a wake when `message_queues.has_pending`. Steering
/// (Alt+Enter) and dequeue-to-edit (Up / Alt+Up) are phase 8, as is
/// editor history (the vaxis `TextField` has none yet, where `aj`
/// records the queued text into its editor history here).
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

/// The root widget: the base layout, the editor submit plumbing, and the
/// overlay stack drawn above everything while it is open.
struct Shell {
    layout: WidgetRef,
    /// Typed handle to the editor so `Init` can focus it.
    editor: Rc<RefCell<TextField>>,
    /// Typed handle to the loader line so host-posted app events (the
    /// busy-edge wake, see [`drive`]) reach it. The loader is not on
    /// the focus path, so the Shell forwards from its capturing phase.
    status_line: Rc<RefCell<StatusLine>>,
    /// Latest submitted editor text, parked by the `on_submit`
    /// callback for the host loop to collect after dispatch. The
    /// callback can't spawn turns itself (it has no session access).
    submitted: Rc<RefCell<Option<String>>>,
    /// The modal stack. Shared: overlay callbacks (confirm/cancel) mutate
    /// it from inside dispatch while the Shell reads it at draw time.
    overlays: Rc<RefCell<OverlayStack>>,
    /// The scrim widget, kept across frames so its identity is stable for
    /// hit-testing.
    scrim: Rc<RefCell<Scrim>>,
    /// Frame styles for overlay windows, resolved from the theme.
    overlay_chrome: OverlayChrome,
    /// Label of the palette row the user confirmed, parked by the palette's
    /// `on_confirm` for the host loop to collect after dispatch (the same
    /// slot pattern as `submitted`).
    palette_selection: Rc<RefCell<Option<String>>>,
}

impl Shell {
    fn new(
        chat: Rc<RefCell<ChatState>>,
        status: Rc<RefCell<StatusState>>,
        queues: MessageQueues,
        theme: &Theme,
        header: String,
        cwd: String,
    ) -> Shell {
        let submitted: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let editor = Rc::new(RefCell::new(TextField::new()));
        {
            let slot = Rc::clone(&submitted);
            // The TextField clears itself on submit. A busy-agent
            // submit is queued (not restored), so the clear is right
            // either way.
            editor.borrow_mut().on_submit = Some(Box::new(move |_ctx, text| {
                *slot.borrow_mut() = Some(text.to_string());
            }));
        }
        let styles = Rc::new(TranscriptStyles::from_theme(theme));
        let transcript = Rc::new(RefCell::new(TranscriptView::new(Rc::clone(&chat), theme)));
        let status_line = StatusLine::new(Rc::clone(&chat), Rc::clone(&status), Rc::clone(&styles));
        let pending = Rc::new(RefCell::new(PendingBox::new(
            Rc::clone(&chat),
            queues,
            Rc::clone(&styles),
        )));
        let footer = Rc::new(RefCell::new(FooterLine::new(chat, status, styles, cwd)));
        // Slot order mirrors `aj`'s layout: header, chat (flex),
        // status, pending, editor, footer. The status and pending
        // slots collapse to zero height while idle/empty, so the
        // editor sits flush under the chat between turns.
        let layout: WidgetRef = Rc::new(RefCell::new(FlexColumn {
            children: vec![
                FlexItem::init(Rc::new(RefCell::new(Text::new(&header))), 0),
                FlexItem::init(to_widget_ref(transcript), 1),
                FlexItem::init(to_widget_ref(Rc::clone(&status_line)), 0),
                FlexItem::init(to_widget_ref(pending), 0),
                FlexItem::init(to_widget_ref(Rc::clone(&editor)), 0),
                FlexItem::init(to_widget_ref(footer), 0),
            ],
        }));
        Shell {
            layout,
            editor,
            status_line,
            submitted,
            overlays: Rc::new(RefCell::new(OverlayStack::default())),
            scrim: Rc::new(RefCell::new(Scrim)),
            overlay_chrome: OverlayChrome::from_theme(theme),
            palette_selection: Rc::new(RefCell::new(None)),
        }
    }

    /// Collect a submit parked by the editor callback, if any.
    fn take_submitted(&self) -> Option<String> {
        self.submitted.borrow_mut().take()
    }

    /// Collect a palette confirmation parked by its callback, if any.
    fn take_palette_selection(&self) -> Option<String> {
        self.palette_selection.borrow_mut().take()
    }

    /// Opens the placeholder command palette (a small centered
    /// filterable-select overlay) and moves focus into its filter field.
    ///
    /// The confirm/cancel callbacks capture shared handles instead of the
    /// Shell itself: they run while an overlay widget is borrowed during
    /// dispatch, so they must not re-enter the Shell. Confirm parks the
    /// picked label in the selection slot for the host loop and closes;
    /// cancel just closes. Both restore focus through `close_top`.
    fn open_palette(&self, ctx: &mut EventContext) {
        let select = Rc::new(RefCell::new(FilterableSelect::new(
            placeholder_palette_items(),
        )));
        let focus = select.borrow().focus_target();
        {
            let mut select = select.borrow_mut();
            let stack = Rc::clone(&self.overlays);
            let editor: WidgetRef = to_widget_ref(Rc::clone(&self.editor));
            let slot = Rc::clone(&self.palette_selection);
            select.on_confirm = Some(Box::new(move |ctx, item| {
                *slot.borrow_mut() = Some(item.label.trim().to_string());
                close_top(&stack, ctx, &editor);
            }));
            let stack = Rc::clone(&self.overlays);
            let editor: WidgetRef = to_widget_ref(Rc::clone(&self.editor));
            select.on_cancel = Some(Box::new(move |ctx| close_top(&stack, ctx, &editor)));
        }
        let mut window = OverlayWindow::new("Commands", to_widget_ref(select));
        window.subtitle = "Enter to confirm  \u{2022}  Esc to close".to_string();
        window.border_style = self.overlay_chrome.border;
        window.title_style = self.overlay_chrome.title;
        window.subtitle_style = self.overlay_chrome.subtitle;
        self.overlays.borrow_mut().push(OpenOverlay {
            widget: to_widget_ref(Rc::new(RefCell::new(window))),
            focus: Rc::clone(&focus),
            placement: OverlayPlacement::Small,
        });
        ctx.request_focus(focus);
        ctx.redraw = true;
    }
}

impl Widget for Shell {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // The caller's draw_widget re-stamps the returned surface with
        // the Shell's identity, replacing the column's. The column
        // takes no events, and the children (transcript, editor) keep
        // their own identities for hit-testing. When the keymap
        // controller lands as the root's wrapper, this surface becomes
        // its child and the overlay children below stay descendants of
        // the Shell's surface, so the composition is unchanged.
        let mut surface = draw_widget(&self.layout, ctx);
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
            surface.children.push(SubSurface {
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
            surface.children.push(SubSurface {
                origin,
                surface: draw_widget(&top.widget, &overlay_ctx),
                z_index: 2,
            });
        }
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        // Host-posted app events target the focused widget (the
        // editor), but the loader is what needs them. The Shell is
        // the root of every focus path, so forward from the capturing
        // phase without consuming.
        if let Event::App(_) = event {
            self.status_line.borrow_mut().handle_event(ctx, event);
        }
        // TEMPORARY: the palette-open chord, hardcoded to ctrl+o, the
        // default of `aj_app::keybindings::ACTION_PALETTE_OPEN`. The keymap
        // controller that resolves configured bindings in this same
        // capturing position replaces this in the next phase. Inert while
        // an overlay is up, matching `aj`.
        if let Event::KeyPress(key) = event
            && key.matches(u32::from('o'), Modifiers::CTRL)
        {
            if !self.overlays.borrow().is_open() {
                self.open_palette(ctx);
            }
            ctx.consume_and_redraw();
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
    let cwd = format!("{}", world.core.env.working_directory.display());
    let shell = Rc::new(RefCell::new(Shell::new(
        Rc::clone(&world.chat),
        Rc::clone(&world.status),
        world.core.message_queues.clone(),
        &theme,
        header,
        cwd,
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
    // Rising-edge tracker for the loader's animation: the tick chain
    // is armed once per idle-to-busy transition, not per iteration.
    let mut was_busy = false;
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
                if let Some(event) = maybe_event {
                    let (redraw, wake_targets) = drain_events(world, event);
                    spawn_wakes(world, wake_targets);
                    if redraw {
                        app.request_redraw();
                    }
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
                            if let Some(text) = shell.borrow().take_submitted() {
                                handle_submit(world, text);
                            }
                            // A confirmed palette row becomes a chat notice.
                            // Placeholder effect: the real command dispatch
                            // arrives with the selector port.
                            if let Some(row) = shell.borrow().take_palette_selection() {
                                fold_notice(world, &format!("{row} selected"));
                                app.request_redraw();
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
        // One status sync per iteration, whatever the arm did. On the
        // idle-to-busy edge, post the loader wake: widgets can only
        // schedule ticks from an event handler, so the host hands the
        // loader an app event to arm its animation chain (the Shell
        // forwards it, see `Shell::capture_event`).
        let busy = sync_status(world);
        if busy && !was_busy {
            let _ = app.post_app_event(UserEvent {
                name: STATUS_WAKE_EVENT.to_string(),
                data: None,
            });
        }
        was_busy = busy;
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

    fn test_shell_with_chat(chat: Rc<RefCell<ChatState>>) -> Rc<RefCell<Shell>> {
        Rc::new(RefCell::new(Shell::new(
            chat,
            Rc::new(RefCell::new(StatusState::default())),
            MessageQueues::default(),
            &Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor),
            "aj-next".to_string(),
            "/tmp".to_string(),
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
        let mut lifecycle = aj_app::session::AgentLifecycle::default();
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

    /// The Shell's draw appends the scrim and the top overlay above the
    /// base layout, in z order and at the ported placement, and the
    /// deepest hit at base-layout coordinates is the scrim.
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
        shell.borrow_mut().open_palette(&mut ev_ctx);
        assert!(shell.borrow().overlays.borrow().is_open());

        let surface = shell.borrow_mut().draw(&ctx);
        let scrim = surface
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
        let overlay = surface
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
            shell.borrow().editor.borrow().graphemes_before_cursor(),
            0,
            "typed key went to the palette filter, not the editor"
        );

        writer.write_all(b"\x1b").expect("write esc");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert!(!shell.borrow().overlays.borrow().is_open(), "esc closes");
        assert!(shell.borrow().take_palette_selection().is_none());
        app.render(&root).expect("render");

        writer.write_all(b"x").expect("write key");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().editor.borrow().graphemes_before_cursor(),
            1,
            "focus is back in the editor"
        );
    }

    /// Typing narrows the palette rows and Enter confirms the highlighted
    /// one: the selection is parked for the host loop, the overlay closes,
    /// and focus returns to the editor.
    #[tokio::test]
    async fn palette_filter_narrows_and_enter_confirms() {
        let (mut app, mut writer, shell, root) = init_app().await;

        writer.write_all(&[0x0f]).expect("write ctrl+o");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        app.render(&root).expect("render");

        // "quit" matches only the Quit row's `{category} {title}` filter
        // key, so Enter must confirm it rather than the first catalog row.
        writer.write_all(b"quit\r").expect("write query + enter");
        for _ in 0..5 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }

        let selection = shell
            .borrow()
            .take_palette_selection()
            .expect("confirm parked the row");
        assert!(selection.contains("Quit"), "selection: {selection:?}");
        assert!(!shell.borrow().overlays.borrow().is_open());
        app.render(&root).expect("render");

        writer.write_all(b"x").expect("write key");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);
        assert_eq!(
            shell.borrow().editor.borrow().graphemes_before_cursor(),
            1,
            "focus is back in the editor"
        );
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
}
