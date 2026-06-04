//! Interactive TUI mode.
//!
//! Per `docs/aj-next-plan.md` §4 the interactive mode owns:
//!
//! - the [`aj_tui::tui::Tui`] event loop (input, render throttle);
//! - a [`layout`] of named slots that components register into;
//! - an [`event_pump`] that maps each [`AgentEvent`] onto a
//!   component update;
//! - a registry of [`components`] (assistant message, tool
//!   execution, footer, header, selectors, etc.);
//! - editor extensions ([`editor_ext`]) that bolt slash-command
//!   completion and `@file` autocomplete onto the shared
//!   [`aj_tui::EditorComponent`];
//! - the keybinding map ([`keys`]).
//!
//! [`AgentEvent`]: aj_agent::events::AgentEvent

pub mod components;
pub mod editor_ext;
pub mod event_pump;
pub mod footer_data;
pub mod keys;
pub mod layout;
pub mod shutdown;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aj_agent::bus::SubscriptionHandle;
use aj_agent::events::AgentEvent;
use aj_agent::{Agent, TurnError};
use aj_conf::{AgentEnv, Config, ConfigSpeed, ConfigThinkingLevel, Severity, display_path};
use aj_models::ThinkingConfig;
use aj_models::auth::AuthStorage;
use aj_models::provider::Provider;
use aj_models::registry::{ModelInfo, ModelRegistry};
use aj_models::types::{Speed, StreamOptions};
use aj_session::{
    ConversationLog, ConversationPersistence, ThreadFilter, persistence_listener,
    repair_interrupted_tool_uses, replay,
};
use aj_tools::{BuiltinToolOptions, get_builtin_tools};
use aj_tui::EditorComponent;
use aj_tui::components::editor::Editor;
use aj_tui::container::Container;
use aj_tui::terminal::ProcessTerminal;
use aj_tui::tui::{OverlayAnchor, OverlayHandle, OverlayOptions, SizeValue, Tui, TuiEvent};
use anyhow::{Context, Result};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_util::sync::CancellationToken;

use crate::SYSTEM_PROMPT;
use crate::cli::args::{Args, Command};
use crate::config::slash_commands::{
    SlashAction, dispatch as slash_dispatch, load_model_catalog, thinking_level_name,
};
use crate::config::theme::{
    Theme, ThemeHandle, chat_theme, editor_border_color_for_thinking, select_list_theme,
    watch_user_theme,
};
use crate::model::{ResolvedModel, from_model_info};
use crate::modes::interactive::components::auth_picker::{
    AuthPickerComponent, AuthProviderItem, OutcomeHandle as AuthPickerOutcomeHandle,
};
use crate::modes::interactive::components::auth_status::{
    AuthStatusComponent, AuthStatusOutcomeHandle,
};
use crate::modes::interactive::components::command_palette::CommandPaletteOutcomeHandle;
use crate::modes::interactive::components::footer::Footer;
use crate::modes::interactive::components::header::Header;
use crate::modes::interactive::components::login_dialog::{
    LoginDialogComponent, LoginDialogState, LoginLine, TuiOAuthCallbacks,
};
use crate::modes::interactive::components::model_selector::{
    ModelIdentityRef, ModelSelectorComponent, ModelSelectorOutcome,
    OutcomeHandle as ModelOutcomeHandle,
};
use crate::modes::interactive::components::prompt_history::{
    PromptHistoryOutcome, PromptHistoryOutcomeHandle, PromptHistorySearchComponent,
    all_workspaces_history_streaming, workspace_history_streaming,
};
use crate::modes::interactive::components::session_selector::{
    OutcomeHandle as SessionOutcomeHandle, SessionSelectorComponent, SessionSelectorOutcome,
};
use crate::modes::interactive::components::thinking_selector::{
    OutcomeHandle as ThinkingOutcomeHandle, ThinkingSelectorComponent, ThinkingSelectorOutcome,
};
use crate::modes::interactive::editor_ext::{DEFAULT_MAX_ENTRIES, PromptHistory};
use crate::modes::interactive::event_pump::{
    EventPump, set_editor_submit_enabled, take_submitted_prompt,
};
use crate::modes::interactive::layout::{SlotIndex, build_layout};
use crate::modes::interactive::shutdown::{
    build_usage_summary, print_resume_hint, print_usage_summary,
};

/// Loop-side snapshot of the agent's run configuration.
///
/// The interactive loop spawns each turn into a task that holds the
/// agent `TokioMutex` for the turn's entire duration, so the loop
/// itself must never `agent.lock().await` — that would suspend the
/// whole `select!` (including its Ctrl+C arm) until the turn ends.
///
/// This snapshot is therefore the loop-side source of truth for "what
/// the next turn runs against". The `/model` and `/thinking` selectors
/// mutate it without touching the agent; the footer renders the active
/// model and effort from it; and the submit handler copies it into the
/// agent just before each turn starts (while holding the turn's own
/// lock, which is uncontended because no turn is in flight yet). A
/// model or thinking change made mid-turn is thus accepted — and shown
/// in the footer — immediately, but only takes effect on the *next*
/// turn: the in-flight turn keeps the config it captured when it
/// started.
struct RunConfigSnapshot {
    /// Provider handle the next turn streams against.
    provider: Arc<dyn Provider>,
    /// Registry (or scripted) metadata for `provider`'s model.
    model_info: Arc<ModelInfo>,
    /// Per-call stream options (thinking-display mode, etc.).
    stream_options: StreamOptions,
    /// Default thinking effort for the next turn.
    thinking: Option<ThinkingConfig>,
    /// `(provider_id, model_id)` the `/model` selector pre-selects.
    /// Tracked explicitly rather than read off `model_info` because
    /// the scripted path's provider id (from `--model-api`) differs
    /// from `model_info.provider`, which is always `"scripted"`.
    model_key: (String, String),
}

/// User-facing notice shown when a session-changing command
/// (`/resume`, `/new`) is invoked while a turn is in flight.
///
/// Those commands reseed the agent's transcript, which requires
/// locking the agent. The loop must not lock the agent while a turn
/// holds it (that freezes the `select!`, including its Ctrl+C arm), so
/// they are refused mid-turn. `what` names the action, e.g.
/// `"switch sessions"`.
fn session_busy_notice(what: &str) -> String {
    format!("Can't {what} while a turn is running — press Ctrl+C to cancel it first.")
}

/// Driver for a single interactive session. Owns the
/// [`aj_tui::tui::Tui`], the registered listeners on the agent's
/// bus, and the [`aj_session::ConversationLog`] for this session.
pub struct InteractiveMode {
    args: Args,
}

impl InteractiveMode {
    /// Build an [`InteractiveMode`] from the parsed CLI [`Args`].
    pub fn from_args(args: Args) -> Result<Self> {
        Ok(Self { args })
    }

    /// Run the TUI to completion. Returns when the user quits or
    /// the agent reports a fatal error.
    pub async fn run(self) -> Result<()> {
        // ---- Configuration & model setup -----------------------------
        // Mirrors `print::run` so a user moving between `--print`
        // and the interactive shell sees the same precedence
        // (CLI > env > config.toml > defaults). Any config-load
        // diagnostics (parse errors, unknown keys) are stashed here
        // and pumped onto the chat scrollback once the TUI is built;
        // we can't `eprintln!` them like print mode does because the
        // alternate screen will eat them.
        let (config, config_diagnostics) = Config::load();

        // Install the `tui.*` + `aj.*` keybindings registry before any
        // component looks up a key. Currently no user overrides are
        // loaded from `config.toml`; defaults supply `alt+t` for
        // `aj.thinking.toggle`, etc.
        crate::config::keybindings::install_global_manager_defaults();

        let speed = match self.args.speed.as_deref() {
            Some(s) => Some(s.parse::<ConfigSpeed>().map_err(anyhow::Error::msg)?),
            None => config.speed,
        }
        .map(|s| match s {
            ConfigSpeed::Standard => Speed::Standard,
            ConfigSpeed::Fast => Speed::Fast,
        });

        // Resolve the model in one of two ways depending on the
        // `--scripted` flag. The scripted path keeps the legacy
        // `Arc<dyn Model>` surface (step 6.8 of
        // `docs/aj-next-progress.md` will port it onto
        // `ScriptedProvider`); the real-model path goes through the
        // registry so the binary owns provider dispatch + API key
        // resolution + speed-driven beta headers, and the agent only
        // sees the resulting `(Provider, ModelInfo, StreamOptions)`
        // bundle.
        //
        // `run_config` is the loop-side snapshot of what the next
        // turn runs against (provider, model, stream options, thinking
        // effort, and `(provider_id, model_id)` for the `/model`
        // selector to pre-select). The selectors mutate it without
        // locking the agent; the submit handler copies it into the
        // agent just before each turn. See [`RunConfigSnapshot`].
        // Credential store backing API-key resolution and the
        // `/login` / `/logout` / `/auth` overlays. Cheap to clone
        // (`Arc`-backed); the resolver installed in
        // `crate::model::from_model_info` captures a clone and reads
        // it on every inference, so a mid-session login takes effect
        // without a restart.
        let auth = AuthStorage::at_default_path().context("failed to open ~/.aj/auth.json")?;

        let mut agent: Agent;
        let run_config: Arc<std::sync::Mutex<RunConfigSnapshot>>;
        if let Some(name) = &self.args.scripted {
            let crate::scripted::ResolvedScriptedModel {
                provider,
                model_info,
            } = crate::scripted::resolve_or_explain(name)?;
            let current_provider = self
                .args
                .model_api
                .clone()
                .or_else(|| config.model_api.clone())
                .unwrap_or_else(|| crate::model::DEFAULT_PROVIDER_ID.to_string());
            let current_id = model_info.id.clone();
            // ---- Tools (legacy / scripted path) -----------------------
            let mut tools = get_builtin_tools(&BuiltinToolOptions {
                image_auto_resize: config.image_auto_resize,
            });
            if !config.disabled_tools.is_empty() {
                tools.retain(|tool| !config.disabled_tools.contains(&tool.name));
            }
            let env = AgentEnv::new();
            let mut stream_options = aj_models::types::StreamOptions::default();
            crate::model::apply_thinking_display(&mut stream_options, config.thinking_display);
            // Clone the run config into the loop-side snapshot before
            // the agent takes ownership; `thinking` is read back from
            // the agent below so it reflects the config-level mapping
            // `Agent::with_provider` applies (e.g. `Off` -> `None`).
            let snapshot_provider = Arc::clone(&provider);
            let snapshot_model_info = Arc::clone(&model_info);
            let snapshot_stream_options = stream_options.clone();
            agent = Agent::with_provider(
                env,
                SYSTEM_PROMPT,
                tools,
                config.disabled_tools.clone(),
                provider,
                model_info,
                stream_options,
                config.thinking,
            );
            run_config = Arc::new(std::sync::Mutex::new(RunConfigSnapshot {
                provider: snapshot_provider,
                model_info: snapshot_model_info,
                stream_options: snapshot_stream_options,
                thinking: agent.default_thinking(),
                model_key: (current_provider, current_id),
            }));
        } else {
            // Load the registry once at startup; the same handle
            // also feeds the `/model` selector's model catalog.
            // `ModelRegistry::load` is cheap (single JSON read) so
            // the duplication versus `load_model_catalog` below is
            // not worth deduplicating today.
            let registry = ModelRegistry::load();
            let resolved = crate::model::resolve(
                &registry,
                &auth,
                self.args
                    .model_api
                    .as_deref()
                    .or(config.model_api.as_deref()),
                self.args
                    .model_name
                    .as_deref()
                    .or(config.model_name.as_deref()),
                self.args
                    .model_url
                    .as_deref()
                    .or(config.model_url.as_deref()),
                speed,
            )
            .context("failed to resolve model from registry")?;
            // ---- Tools (registry / real-provider path) ----------------
            let mut tools = get_builtin_tools(&BuiltinToolOptions {
                image_auto_resize: config.image_auto_resize,
            });
            if !config.disabled_tools.is_empty() {
                tools.retain(|tool| !config.disabled_tools.contains(&tool.name));
            }
            let env = AgentEnv::new();
            let ResolvedModel {
                provider,
                model_info,
                stream_options,
            } = resolved;
            let mut stream_options = stream_options;
            crate::model::apply_thinking_display(&mut stream_options, config.thinking_display);
            let model_key = (model_info.provider.clone(), model_info.id.clone());
            // Clone the run config into the loop-side snapshot before
            // the agent takes ownership (see the scripted branch).
            let snapshot_provider = Arc::clone(&provider);
            let snapshot_model_info = Arc::clone(&model_info);
            let snapshot_stream_options = stream_options.clone();
            agent = Agent::with_provider(
                env,
                SYSTEM_PROMPT,
                tools,
                config.disabled_tools.clone(),
                provider,
                model_info,
                stream_options,
                config.thinking,
            );
            run_config = Arc::new(std::sync::Mutex::new(RunConfigSnapshot {
                provider: snapshot_provider,
                model_info: snapshot_model_info,
                stream_options: snapshot_stream_options,
                thinking: agent.default_thinking(),
                model_key,
            }));
        }
        agent.set_block_images(config.image_block);

        // Apply a `--api-key` runtime override (if supplied) to the
        // resolved provider, then check whether *any* credential is
        // configured so we can nudge the user toward `/login` when
        // none is. Both are skipped for the scripted fake provider,
        // which needs no real credentials. The warning is emitted via
        // the pump further below (it doesn't exist yet here).
        let mut startup_auth_warning: Option<String> = None;
        if self.args.scripted.is_none() {
            let provider_id = {
                let cfg = run_config.lock().expect("run config mutex poisoned");
                cfg.model_key.0.clone()
            };
            if let Some(key) = self.args.api_key.clone() {
                auth.set_runtime_api_key(&provider_id, key).await;
            }
            match auth.has_auth(&provider_id).await {
                Ok(true) => {}
                Ok(false) => {
                    startup_auth_warning = Some(format!(
                        "Heads up: {}",
                        crate::model::missing_key_message(&provider_id)
                    ));
                }
                Err(err) => {
                    startup_auth_warning = Some(format!(
                        "Couldn't check credentials for {provider_id:?}: {err}"
                    ));
                }
            }
        }

        // Snapshot the model catalog up-front so the `/model`
        // selector overlay and the editor's argument completer
        // share a single load (registry::load reads JSON twice
        // otherwise — once per consumer).
        let model_catalog = load_model_catalog();

        // ---- Conversation log: resume or create -----------------------
        let sessions_dir = Config::get_sessions_dir_path()?;
        let conversation_persistence = ConversationPersistence::new(sessions_dir);
        let resuming = matches!(self.args.command, Some(Command::Continue { .. }));
        let mut log = match self.args.command {
            Some(Command::Continue {
                session_id: Some(id),
                prompt: _,
            }) => ConversationLog::resume(&conversation_persistence, &id)?,
            Some(Command::Continue {
                session_id: None,
                prompt: _,
            }) => match conversation_persistence.get_latest_session_id()? {
                Some(latest) => ConversationLog::resume(&conversation_persistence, &latest)?,
                None => {
                    eprintln!("No latest conversation to resume; starting a fresh session.");
                    ConversationLog::create(&conversation_persistence)?
                }
            },
            _ => ConversationLog::create(&conversation_persistence)?,
        };

        // (The agent was built above alongside the model resolution
        // step so the legacy / registry split could pick its own
        // tool list and env. Nothing else to do here.)

        // Resolve system prompt: reuse a persisted one (cache-warm
        // resume) or assemble fresh from the env. On a fresh log,
        // freeze the assembled prompt as the root entry.
        let system_prompt = if let Some(persisted) = log.system_prompt() {
            persisted.to_string()
        } else {
            let assembled = agent.assemble_system_prompt();
            if log.is_empty() {
                log.set_system_prompt(assembled.clone())?;
            }
            assembled
        };
        agent.set_assembled_system_prompt(system_prompt);

        // Seed the sub-agent counter so freshly-minted ids in this
        // session don't collide with persisted sub-agent subtrees.
        if let Some(max_id) = log.max_agent_id() {
            agent.seed_sub_agent_counter(max_id);
        }

        // ---- Bus subscriptions: channel forwarder + persistence ------
        // The channel forwarder feeds the event pump in the main
        // loop; subscribing on `&self` doesn't disturb the agent's
        // mutability so we can still call `seed_messages` below.
        let (_event_handle, mut event_rx) = agent.subscribe_channel();

        // Replay persisted history through the event pump *before*
        // wrapping the log in an Arc<Mutex>; the binary writes a
        // few synchronous repair-tool-use entries during this phase
        // and we keep exclusive access to the log until that's done.
        if let Some(head) = log.latest_leaf(ThreadFilter::USER) {
            let conversation = log.linearize(&head, ThreadFilter::USER);
            repair_interrupted_tool_uses(&mut log, &conversation)?;
            // Re-linearize after repair to capture any synthesized
            // tool_result message.
            let head = log
                .latest_leaf(ThreadFilter::USER)
                .expect("post-repair head exists when pre-repair head did");
            let conversation = log.linearize(&head, ThreadFilter::USER);
            let messages: Vec<_> = conversation.agent_messages();
            agent.seed_messages(messages);
        }

        // ---- Theme ----------------------------------------------------
        // Loaded once at startup from `config.theme` (default `light`).
        // The handle is reused everywhere a component needs theme
        // colors: layout, event pump, selector overlays. A runtime
        // swap re-points the inner [`Theme`] without rebuilding any
        // component — every theme closure resolves through the
        // shared [`RwLock`] on each call.
        let configured_theme = resolve_theme_name(config.theme.as_deref()).to_string();
        let theme = ThemeHandle::new(Theme::load(&configured_theme));

        // ---- Theme file watcher (hot-reload) -------------------------
        // Only user-supplied themes get a watcher; bundled `dark` /
        // `light` palettes live inside the binary and have no on-disk
        // source to edit. `watch_user_theme` short-circuits on missing
        // file / unset `$HOME` and silently degrades to "no
        // hot-reload" when the notify backend can't start.
        let (theme_watcher_guard, mut theme_rx) = match watch_user_theme(&configured_theme) {
            Some((guard, rx)) => (Some(guard), Some(rx)),
            None => (None, None),
        };

        // ---- Build the TUI --------------------------------------------
        let mut tui = Tui::new(Box::new(ProcessTerminal::new()));
        tui.start()
            .map_err(|e| anyhow::anyhow!("failed to start terminal: {e}"))?;
        build_layout(&mut tui, &theme);

        // Tint the editor border to match the agent's initial
        // thinking level so the user sees the active reasoning mode
        // at a glance the moment the TUI comes up. Updates flow
        // through the same helper on every `/thinking` change.
        apply_editor_border_for_thinking(&mut tui, &theme, agent.default_thinking().as_ref());

        // Install the path/symbol autocomplete provider on the
        // editor. The `@filename` fuzzy file picker and direct
        // path completion live here; slash commands no longer have
        // an inline popup (typing `/` at the empty prompt opens
        // the command palette overlay instead).
        if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
            let provider = aj_tui::autocomplete::CombinedAutocompleteProvider::new(
                agent.env().working_directory.clone(),
            );
            editor.set_autocomplete_provider(Arc::new(provider));
        }

        // Bootstrap the editor's prompt-history ring from every
        // `*.jsonl` file under the project's sessions directory so
        // pressing Up surfaces cross-session prompts the user has
        // ever submitted in this project. Live submissions update
        // the same ring through
        // [`aj_tui::components::editor::Editor::add_to_history`]
        // in the submit branch below; this bootstrap covers the
        // "I just relaunched the binary" case where the in-memory
        // ring would otherwise start empty. No persistence layer
        // is involved — the conversation log files are the source
        // of truth, so two `aj` processes running side by
        // side can't clobber each other's history.
        let prompt_history =
            PromptHistory::bootstrap(&conversation_persistence, DEFAULT_MAX_ENTRIES);
        if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
            prompt_history.install(editor);
        }

        // Shared flag tripped by the editor's `/`-at-empty-prompt
        // callback and by the global `Ctrl+O` chord. The main loop
        // polls it after `tui.handle_input` and routes a synthetic
        // `/palette` through the slash dispatcher, so all three
        // palette-open paths (typed `/palette`, leading `/`,
        // `Ctrl+O`) converge on the same mounting code.
        let palette_open_request: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        // Set when the user hits `aj.overlay.close_all` (default
        // `ctrl+c`) while any overlay is up. Drained after
        // `tui.handle_input` to tear down the entire overlay
        // back-stack — distinct from Esc's one-level pop.
        let close_all_request: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        // Set by the global `aj.history.open` chord (default
        // `ctrl+r`). Drained after `tui.handle_input` to route a
        // synthetic `/history` through the slash dispatcher, mirroring
        // the `palette_open_request` path. Dispatched without a parent
        // palette so `Esc` closes the overlay back to the editor.
        let history_open_request: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        {
            let flag = Arc::clone(&palette_open_request);
            if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
                editor.set_on_palette_trigger(Some(Arc::new(move || {
                    flag.store(true, Ordering::Relaxed);
                })));
            }
        }

        // Header: session id + resume/start banner. Footer: model +
        // working directory (initial snapshot; richer state lands
        // alongside `footer_data` in a follow-up).
        if let Some(header) = tui.get_mut_as::<Header>(SlotIndex::Header.idx()) {
            header.set_session_id(Some(log.session_id().to_string()));
            header.set_notice(Some(if resuming {
                "Resuming conversation".to_string()
            } else {
                "Chat with AJ — Ctrl+C to quit".to_string()
            }));
        }
        if let Some(footer) = tui.get_mut_as::<Footer>(SlotIndex::Footer.idx()) {
            let info = agent.model_info();
            footer.set_model(Some(format_footer_model(
                &info.id,
                &agent.default_thinking(),
            )));
            footer.set_cwd(Some(format!("{}", agent.env().working_directory.display())));
        }

        // Drive replay events through the pump (post-layout so the
        // chat container has a slot to receive them). Replay never
        // hits the bus, so persistence isn't double-written.
        let context_window = agent.model_info().context_window;
        let mut pump = EventPump::new(
            chat_theme(&theme),
            config.hide_thinking_block,
            false,
            config.image_show_in_terminal,
            context_window,
        );
        // Push the initial `?/<window>` so the indicator is
        // visible before any turn lands. Replay (next) may
        // overwrite the numerator with the last persisted turn's
        // prompt size; the denominator stays as set here.
        pump.sync_footer(&mut tui);
        for event in replay(&log) {
            pump.handle(&mut tui, &event);
        }

        // Startup notices: surface config-load diagnostics (parse
        // errors, unknown keys) first so a user with a broken
        // `config.toml` sees that before any other chrome, then
        // list the AGENTS.md / CLAUDE.md files stitched into the
        // system prompt, then (unless suppressed) print the sandbox
        // warning. All flow through the pump's existing
        // `Notice` / `Warning` / `Error` arms so they appear as dim
        // chat-scrollback rows just above the editor — close enough
        // to where the user starts typing that they're hard to
        // miss, but out of the way of replayed history or the
        // header/footer status panes. Placed *after* replay so a
        // resumed session's historical content stays on top.
        for d in &config_diagnostics {
            let text = d.to_string();
            let event = match d.severity() {
                Severity::Warning => warning_event(&text),
                Severity::Error => error_event(&text),
            };
            pump.handle(&mut tui, &event);
        }
        pump.handle(&mut tui, &notice_event(&build_context_notice(agent.env())));
        if sandbox_warning_enabled() {
            pump.handle(&mut tui, &warning_event(SANDBOX_WARNING));
        }
        if let Some(warning) = &startup_auth_warning {
            pump.handle(&mut tui, &warning_event(warning));
        }

        // ---- Wrap log + agent in shared handles -----------------------
        // After replay we hand the log to the persistence listener
        // and to any future code path that wants to read the session
        // id back. Both `log` and `persistence_handle` are bound
        // mutably so the `/resume` selector can swap them out for
        // the resumed session without restarting the binary.
        // The agent goes behind a TokioMutex because the
        // submit-handler spawns a task that holds it across an
        // `agent.prompt(...).await`.
        let mut log = Arc::new(TokioMutex::new(log));
        let agent = Arc::new(TokioMutex::new(agent));
        // Shared, mutable view of the on-disk config. Selector
        // outcomes (model / thinking, and future settings popups)
        // mutate this and persist it via `persist_config` so a choice
        // made in the TUI survives a restart. Held behind a std mutex
        // because `Config::save` is a quick synchronous file write,
        // never awaited across.
        let config = Arc::new(std::sync::Mutex::new(config));
        let mut persistence_handle: SubscriptionHandle = {
            let a = agent.lock().await;
            a.subscribe(persistence_listener(Arc::clone(&log)))
        };

        // ---- Main event loop ------------------------------------------
        // `running_task` holds the in-flight `agent.prompt(...)`
        // future. We poll it concurrently with the TUI's input and
        // render events; while it's running the editor's
        // `disable_submit` flag is set so a second submission can't
        // queue up before steering/follow-up land.
        //
        // `current_turn_cancel` holds the per-turn cancellation
        // handle the binary owns. It's a clone of the same
        // `CancellationToken` we passed to `agent.prompt` —
        // `CancellationToken` is `Arc`-backed so cloning shares
        // cancellation state. Ctrl+C while a turn is running fires
        // this handle, which the agent's `execute_turn` `select!`s
        // on; Ctrl+C while idle exits the program.
        let mut running_task: Option<tokio::task::JoinHandle<Result<(), TurnError>>> = None;
        let mut current_turn_cancel: Option<CancellationToken> = None;
        let mut run_result: Result<()> = Ok(());

        // Slash commands like `/thinking` open an overlay selector.
        // While an overlay is up the editor is not focused, but
        // `tui.show_overlay` already routes input to the overlay,
        // so the main loop's job is just to poll the outcome slot
        // after every input event and close the overlay on result.
        let mut open_selector: Option<OpenSelector> = None;

        // An in-flight OAuth login: the dialog overlay + a cancel flag
        // the dialog (Esc) and Ctrl+C set, plus the spawned login task
        // whose `JoinHandle` we poll alongside the agent turn. Kept
        // separate from `open_selector` because the flow is async and
        // long-running rather than a synchronous confirm/cancel
        // selector.
        let mut login_session: Option<LoginSession> = None;
        let mut login_task: Option<
            tokio::task::JoinHandle<Result<(), aj_models::auth::AuthError>>,
        > = None;

        loop {
            // Build a future that resolves when the agent task
            // finishes, or pends forever if no task is running.
            // `tokio::task::JoinHandle: Future` so we can await it
            // directly under the `tokio::select!` arm; using
            // `&mut running_task` means we keep the same handle
            // alive across multiple iterations until it completes.
            let task_done = async {
                match running_task.as_mut() {
                    Some(handle) => handle.await,
                    None => std::future::pending::<
                        Result<Result<(), TurnError>, tokio::task::JoinError>,
                    >()
                    .await,
                }
            };

            tokio::select! {
                biased;

                // --- Agent run finished ---
                join_outcome = task_done => {
                    running_task = None;
                    current_turn_cancel = None;
                    set_editor_submit_enabled(&mut tui, true);
                    match join_outcome {
                        Ok(Ok(())) => {}
                        Ok(Err(TurnError::Aborted)) => {
                            // The agent has already emitted the
                            // synthetic aborted `MessageEnd` and any
                            // cancelled tool-result `MessageEnd`s, so
                            // the chat scrollback is consistent.
                            // Surface a brief notice so the user has
                            // visible confirmation that Ctrl+C took
                            // effect; the session stays alive.
                            pump.handle(&mut tui, &notice_event("Turn cancelled."));
                        }
                        Ok(Err(TurnError::Recoverable(err))) => {
                            // Surface to the user; the chat
                            // container's notice line keeps the
                            // session going. The agent's own
                            // bus already wrote whatever
                            // partial assistant message it
                            // produced.
                            pump.handle(
                                &mut tui,
                                &AgentEvent::Error {
                                    agent_id: aj_agent::events::AgentId::Main,
                                    text: format!("agent error: {err:#}"),
                                },
                            );
                        }
                        Ok(Err(TurnError::Fatal(err))) => {
                            run_result = Err(err);
                            break;
                        }
                        Err(join_err) => {
                            run_result = Err(anyhow::anyhow!(
                                "agent task panicked: {join_err}"
                            ));
                            break;
                        }
                    }
                }

                // --- OAuth login task finished ---
                login_outcome = async {
                    match login_task.as_mut() {
                        Some(handle) => handle.await,
                        None => std::future::pending::<
                            Result<Result<(), aj_models::auth::AuthError>, tokio::task::JoinError>,
                        >()
                        .await,
                    }
                } => {
                    login_task = None;
                    if let Some(session) = login_session.take() {
                        tui.hide_overlay(&session.handle);
                        let name = session.provider_name;
                        match login_outcome {
                            Ok(Ok(())) => {
                                pump.handle(
                                    &mut tui,
                                    &notice_event(&format!("Logged in to {name}.")),
                                );
                            }
                            Ok(Err(err)) => {
                                pump.handle(
                                    &mut tui,
                                    &warning_event(&format!("Login to {name} failed: {err}")),
                                );
                            }
                            // Aborted on Ctrl+C / Esc: the cancel-poll
                            // arm already surfaced a "cancelled" notice.
                            Err(join_err) if join_err.is_cancelled() => {}
                            Err(join_err) => {
                                pump.handle(
                                    &mut tui,
                                    &warning_event(&format!("Login task error: {join_err}")),
                                );
                            }
                        }
                    }
                }

                // --- TUI input / render ---
                maybe_event = tui.next_event() => {
                    let Some(event) = maybe_event else {
                        // Terminal stream ended: equivalent to
                        // Ctrl-C/Ctrl-D from the user's POV.
                        break;
                    };
                    match event {
                        TuiEvent::Render => tui.render(),
                        TuiEvent::Input(input) => {
                            // Ctrl+C semantics, in priority order:
                            //
                            // 1. Turn in flight (`current_turn_cancel`
                            //    is `Some`): cancel the turn. The
                            //    cancel handle is the binary's clone
                            //    of the per-turn `CancellationToken`
                            //    we passed to `agent.prompt`; firing
                            //    it propagates to the agent's
                            //    `execute_turn` `select!`s and to
                            //    every provider / tool subscribed to
                            //    the same token, including the bash
                            //    tool's process group.
                            // 2. Overlay up (`open_selector` is
                            //    `Some`): fall through to the
                            //    `ACTION_OVERLAY_CLOSE_ALL`
                            //    interception below, which matches
                            //    `ctrl+c` by default and tears the
                            //    overlay stack down.
                            // 3. Neither: exit the binary. This is
                            //    the legacy behavior — Ctrl+C with
                            //    no turn and no overlay quits.
                            //
                            // Ctrl+D always exits regardless. The
                            // terminal is in raw mode so neither
                            // chord raises SIGINT; both arrive here
                            // as ordinary key events.
                            if input.is_ctrl('c') {
                                if let Some(token) = current_turn_cancel.as_ref() {
                                    token.cancel();
                                    continue;
                                }
                                if let Some(session) = login_session.as_ref() {
                                    // Login in flight: signal cancel.
                                    // The cancel-poll below tears the
                                    // dialog down and aborts the task.
                                    session.cancel.store(true, Ordering::Relaxed);
                                    continue;
                                }
                                if open_selector.is_none() {
                                    break;
                                }
                                // Overlay is up: don't break. The
                                // overlay close-all interception
                                // below catches the same keystroke
                                // and tears the stack down.
                            }
                            if input.is_ctrl('d') {
                                break;
                            }
                            // Toggle the thinking-block render mode for
                            // the session. Bound via `aj.thinking.toggle`
                            // (default `alt+t`); intercepted before
                            // `tui.handle_input` so the editor never sees
                            // the keystroke. See `docs/aj-next-plan.md` §4.4.
                            {
                                let kb = aj_tui::keybindings::get();
                                if kb.matches(
                                    &input,
                                    crate::config::keybindings::ACTION_THINKING_TOGGLE,
                                ) {
                                    drop(kb);
                                    let new_value = !pump.hide_thinking_block();
                                    pump.set_hide_thinking_block(&mut tui, new_value);
                                    // Don't post a "hidden/visible"
                                    // notice — the transcript above
                                    // already shows the new state.
                                    continue;
                                }
                            }
                            // Toggle the tool-output render mode for the
                            // session. Bound via `aj.tools.expand`
                            // (default `alt+o`); intercepted before
                            // `tui.handle_input` so the editor never sees
                            // the keystroke.
                            {
                                let kb = aj_tui::keybindings::get();
                                if kb.matches(
                                    &input,
                                    crate::config::keybindings::ACTION_TOOLS_EXPAND,
                                ) {
                                    drop(kb);
                                    let new_value = !pump.tools_expanded();
                                    pump.set_tools_expanded(&mut tui, new_value);
                                    continue;
                                }
                            }
                            // Clipboard image paste. Bound via
                            // `aj.clipboard.paste_image` (default
                            // `ctrl+v`); intercepted before the editor
                            // sees the keystroke so it doesn't receive
                            // a literal control byte. On a successful
                            // clipboard image read, the temp-file path
                            // is inserted at the cursor as plain text —
                            // the model uses `read_file` on submit to
                            // look at it. Any failure (no image, no
                            // clipboard backend, etc.) is silent.
                            //
                            // Because we bypass `tui.handle_input` for
                            // this chord, we must request a render
                            // ourselves; otherwise the inserted path
                            // sits in the editor buffer until the next
                            // keystroke happens to trigger a paint.
                            {
                                let kb = aj_tui::keybindings::get();
                                if kb.matches(
                                    &input,
                                    crate::config::keybindings::ACTION_CLIPBOARD_PASTE_IMAGE,
                                ) {
                                    drop(kb);
                                    if let Some(path) =
                                        crate::clipboard::read_image_to_tempfile()
                                        && let Some(editor) = tui.get_mut_as::<Editor>(
                                            SlotIndex::Editor.idx(),
                                        )
                                    {
                                        editor.insert_text_at_cursor(
                                            &path.display().to_string(),
                                        );
                                    }
                                    tui.request_render();
                                    continue;
                                }
                            }
                            // Global command-palette chord. Bound via
                            // `aj.palette.open` (default `ctrl+o`).
                            // Intercepted before `tui.handle_input` so
                            // no component sees the keystroke. The
                            // actual overlay mount happens after
                            // `tui.handle_input` via the shared
                            // `palette_open_request` flag, so both the
                            // editor-`/` path and this chord converge
                            // on the same dispatcher arm. Inert while
                            // a selector is already up so the chord
                            // can't interrupt an open modal.
                            //
                            // `aj.overlay.close_all` (default `ctrl+c`)
                            // is the symmetric tear-down chord: when
                            // an overlay is up, intercept and consume
                            // the event so the selector's own cancel
                            // path doesn't also fire.
                            let mut consume_event = false;
                            {
                                let kb = aj_tui::keybindings::get();
                                if open_selector.is_some()
                                    && kb.matches(
                                        &input,
                                        crate::config::keybindings::ACTION_OVERLAY_CLOSE_ALL,
                                    )
                                {
                                    drop(kb);
                                    close_all_request.store(true, Ordering::Relaxed);
                                    // Consume: skip `tui.handle_input`
                                    // entirely so the selector doesn't
                                    // also see Ctrl+C as a cancel and
                                    // write a cancel-outcome that would
                                    // then drive a stale one-level
                                    // back-stack restore underneath
                                    // our explicit teardown.
                                    consume_event = true;
                                } else if open_selector.is_none()
                                    && login_session.is_none()
                                    && kb.matches(
                                        &input,
                                        crate::config::keybindings::ACTION_PALETTE_OPEN,
                                    )
                                {
                                    drop(kb);
                                    palette_open_request.store(true, Ordering::Relaxed);
                                    // Fall through to the dispatcher
                                    // arm below by letting handle_input
                                    // run (it's a no-op for this chord
                                    // since no component binds ctrl+o).
                                } else if open_selector.is_none()
                                    && login_session.is_none()
                                    && kb.matches(
                                        &input,
                                        crate::config::keybindings::ACTION_HISTORY_OPEN,
                                    )
                                {
                                    drop(kb);
                                    history_open_request.store(true, Ordering::Relaxed);
                                    // Consume: the editor binds no
                                    // ctrl+r, but skipping handle_input
                                    // keeps the chord from reaching any
                                    // future binding and matches the
                                    // close-all interception style.
                                    consume_event = true;
                                }
                            }
                            if !consume_event {
                                tui.handle_input(&input);
                            }

                            // Close-all: tear down current selector
                            // and any parent palette in one shot.
                            // Done before the palette-open dispatch
                            // and the per-tick outcome poll so we
                            // never re-enter the back-stack logic
                            // on a unwound state.
                            if close_all_request.swap(false, Ordering::Relaxed) {
                                if let Some(sel) = open_selector.take() {
                                    close_all_overlays(&mut tui, sel);
                                }
                                continue;
                            }

                            // Login cancellation: the dialog's Esc (or
                            // Ctrl+C above) flips the shared flag. Tear
                            // the dialog down and abort the login task;
                            // the task's `JoinHandle` arm sees the
                            // cancellation and stays quiet.
                            if let Some(session) = login_session.as_ref()
                                && session.cancel.load(Ordering::Relaxed)
                            {
                                tui.hide_overlay(&session.handle);
                                if let Some(task) = login_task.take() {
                                    task.abort();
                                }
                                let name = session.provider_name.clone();
                                login_session = None;
                                pump.handle(
                                    &mut tui,
                                    &notice_event(&format!("Login to {name} cancelled.")),
                                );
                                continue;
                            }

                            // Global palette open: fired either by the
                            // editor's `/`-at-empty-prompt callback
                            // (handled inside `tui.handle_input` above)
                            // or by the `Ctrl+O` chord intercepted
                            // below. Dispatched here, after routing,
                            // so the editor's `/` swallow has already
                            // landed and so we can `await` the slash
                            // dispatcher. Gated on `open_selector` to
                            // be inert while another selector is up.
                            if palette_open_request.swap(false, Ordering::Relaxed)
                                && open_selector.is_none()
                                && login_session.is_none()
                            {
                                match handle_slash_command(
                                    &mut tui,
                                    &auth,
                                    Arc::clone(&agent),
                                    Arc::clone(&model_catalog),
                                    Arc::clone(&run_config),
                                    &mut log,
                                    &mut persistence_handle,
                                    &mut pump,
                                    &conversation_persistence,
                                    &theme,
                                    "/palette",
                                    None,
                                    running_task.is_some(),
                                ).await {
                                    SlashHandled::Continue { selector, notice } => {
                                        if let Some(text) = notice {
                                            pump.handle(&mut tui, &notice_event(&text));
                                        }
                                        if let Some(sel) = selector {
                                            open_selector = Some(sel);
                                        }
                                    }
                                    SlashHandled::Quit => break,
                                }
                                continue;
                            }

                            // Global prompt-history open: fired by the
                            // `Ctrl+R` chord intercepted above. Routes a
                            // synthetic `/history` through the same
                            // dispatcher with no parent palette, so the
                            // overlay's `Esc` closes straight back to
                            // the editor. Gated on `open_selector` to be
                            // inert while another selector is up.
                            if history_open_request.swap(false, Ordering::Relaxed)
                                && open_selector.is_none()
                                && login_session.is_none()
                            {
                                match handle_slash_command(
                                    &mut tui,
                                    &auth,
                                    Arc::clone(&agent),
                                    Arc::clone(&model_catalog),
                                    Arc::clone(&run_config),
                                    &mut log,
                                    &mut persistence_handle,
                                    &mut pump,
                                    &conversation_persistence,
                                    &theme,
                                    "/history",
                                    None,
                                    running_task.is_some(),
                                ).await {
                                    SlashHandled::Continue { selector, notice } => {
                                        if let Some(text) = notice {
                                            pump.handle(&mut tui, &notice_event(&text));
                                        }
                                        if let Some(sel) = selector {
                                            open_selector = Some(sel);
                                        }
                                    }
                                    SlashHandled::Quit => break,
                                }
                                continue;
                            }
                            // input was just routed to it. Poll
                            // the overlay's outcome slot; on a
                            // confirm/cancel, close it and apply
                            // the result.
                            if let Some(sel) = open_selector.take() {
                                match handle_selector_outcome(
                                    &mut tui,
                                    sel,
                                    &auth,
                                    Arc::clone(&agent),
                                    Arc::clone(&run_config),
                                    Arc::clone(&config),
                                    &mut log,
                                    &mut persistence_handle,
                                    &mut pump,
                                    &conversation_persistence,
                                    &theme,
                                ).await {
                                    SelectorPollOutcome::StillOpen(reopened) => {
                                        open_selector = Some(reopened);
                                    }
                                    SelectorPollOutcome::Closed { notice, follow_up, start_login } => {
                                        if let Some(text) = notice {
                                            pump.handle(&mut tui, &notice_event(&text));
                                        }
                                        // A confirmed `/login` provider
                                        // pick asks the host to launch
                                        // the async browser flow: mount
                                        // the dialog overlay and spawn
                                        // the login task (polled by the
                                        // login `select!` arm).
                                        if let Some(provider_id) = start_login {
                                            match start_login_session(
                                                &mut tui,
                                                &auth,
                                                &theme,
                                                &provider_id,
                                            )
                                            .await
                                            {
                                                Ok((session, task)) => {
                                                    login_session = Some(session);
                                                    login_task = Some(task);
                                                }
                                                Err(err) => pump.handle(
                                                    &mut tui,
                                                    &warning_event(&format!(
                                                        "Couldn't start login: {err}"
                                                    )),
                                                ),
                                            }
                                        }
                                        // The command palette chains into a
                                        // real slash command by emitting a
                                        // `follow_up`. Re-feed it through
                                        // `handle_slash_command` so the
                                        // dispatch path is identical to a
                                        // user-typed `/...` line.
                                        if let Some(follow_up) = follow_up {
                                            match handle_slash_command(
                                                &mut tui,
                                                &auth,
                                                Arc::clone(&agent),
                                                Arc::clone(&model_catalog),
                                                Arc::clone(&run_config),
                                                &mut log,
                                                &mut persistence_handle,
                                                &mut pump,
                                                &conversation_persistence,
                                                &theme,
                                                &follow_up.input,
                                                Some(follow_up.parent_palette),
                                                running_task.is_some(),
                                            )
                                            .await
                                            {
                                                SlashHandled::Continue { selector, notice } => {
                                                    if let Some(text) = notice {
                                                        pump.handle(&mut tui, &notice_event(&text));
                                                    }
                                                    if let Some(sel) = selector {
                                                        open_selector = Some(sel);
                                                    }
                                                }
                                                SlashHandled::Quit => break,
                                            }
                                        }
                                    }
                                }
                                continue;
                            }

                            // The editor swallows printable
                            // input and re-emits a `Submit` when
                            // the user presses Enter. Drain it
                            // and dispatch.
                            if let Some(text) = take_submitted_prompt(&mut tui) {
                                let trimmed = text.trim().to_string();
                                if trimmed.is_empty() {
                                    continue;
                                }

                                // Slash-command branch: a submitted
                                // `/...` line is dispatched as a command
                                // (not queued as a prompt). `disable_submit`
                                // gates editor submits while a turn runs,
                                // so this path only fires between turns;
                                // mid-turn settings changes go through the
                                // palette/selectors, which never block on
                                // the agent. `handle_slash_command` still
                                // gets the live `running_task` state so it
                                // can refuse session-changing commands if
                                // one ever reaches here mid-turn.
                                if trimmed.starts_with('/') {
                                    if let Some(editor) = tui.get_mut_as::<Editor>(
                                        SlotIndex::Editor.idx()
                                    ) {
                                        editor.set_text("");
                                    }
                                    match handle_slash_command(
                                        &mut tui,
                                        &auth,
                                        Arc::clone(&agent),
                                        Arc::clone(&model_catalog),
                                        Arc::clone(&run_config),
                                        &mut log,
                                        &mut persistence_handle,
                                        &mut pump,
                                        &conversation_persistence,
                                        &theme,
                                        &trimmed,
                                        None,
                                        running_task.is_some(),
                                    ).await {
                                        SlashHandled::Continue { selector, notice } => {
                                            if let Some(text) = notice {
                                                pump.handle(&mut tui, &notice_event(&text));
                                            }
                                            if let Some(sel) = selector {
                                                open_selector = Some(sel);
                                            }
                                        }
                                        SlashHandled::Quit => break,
                                    }
                                    continue;
                                }

                                if running_task.is_some() {
                                    // Already running a turn;
                                    // ignore the second submit.
                                    // Steering/follow-up queues
                                    // land in a follow-up.
                                    continue;
                                }
                                // Clear the editor's text and
                                // disable further submits until
                                // the agent reports the run
                                // ended.
                                if let Some(editor) = tui.get_mut_as::<Editor>(
                                    SlotIndex::Editor.idx()
                                ) {
                                    editor.set_text("");
                                    editor.add_to_history(&trimmed);
                                }
                                set_editor_submit_enabled(&mut tui, false);
                                let agent_for_run = Arc::clone(&agent);
                                let run_config_for_turn = Arc::clone(&run_config);
                                // Mint a fresh per-turn cancellation
                                // token. The binary keeps one clone in
                                // `current_turn_cancel` so the Ctrl+C
                                // arm can fire `.cancel()` without
                                // locking the agent mutex; the spawned
                                // task hands its own clone to
                                // `agent.prompt`. Both clones share
                                // cancellation state because the inner
                                // is `Arc`-backed.
                                let turn_cancel = CancellationToken::new();
                                current_turn_cancel = Some(turn_cancel.clone());
                                running_task = Some(tokio::spawn(async move {
                                    let mut a = agent_for_run.lock().await;
                                    // Apply the loop-side run-config
                                    // snapshot before the turn so a
                                    // model / thinking change the user
                                    // made since the last turn takes
                                    // effect now. Done under the turn's
                                    // own lock — uncontended, since no
                                    // turn is in flight yet — so the
                                    // loop never blocks on it.
                                    {
                                        let cfg = run_config_for_turn
                                            .lock()
                                            .expect("run config mutex poisoned");
                                        a.set_provider(
                                            Arc::clone(&cfg.provider),
                                            Arc::clone(&cfg.model_info),
                                            cfg.stream_options.clone(),
                                        );
                                        a.set_default_thinking(cfg.thinking.clone());
                                    }
                                    a.prompt(trimmed, turn_cancel).await
                                }));
                            }
                        }
                    }
                }

                // --- Agent bus event ---
                maybe_evt = recv_event(&mut event_rx) => {
                    let Some(event) = maybe_evt else { continue };
                    pump.handle(&mut tui, &event);
                }

                // --- Theme reload (fs-watcher) ---
                // Coalesced re-parses of `~/.aj/themes/<name>.json`
                // flow through here. `theme_rx` is `None` when no
                // watcher is active (bundled theme name with no
                // override, missing `$HOME`, or the notify backend
                // declined to start); the helper folds that into a
                // pending-forever future so the select arm is
                // harmless in those cases.
                maybe_new_theme = recv_theme(theme_rx.as_mut()) => {
                    let Some(new_theme) = maybe_new_theme else { continue };
                    let name = new_theme.name().to_string();
                    theme.replace(new_theme);
                    // `Tui::invalidate` walks the root + every overlay
                    // and clears each component's cached render
                    // output. The closures still in flight resolve
                    // through the shared lock so the next render
                    // paints with the new palette automatically.
                    tui.invalidate();
                    tui.request_render();
                    pump.handle(
                        &mut tui,
                        &notice_event(&format!("Theme '{name}' reloaded.")),
                    );
                }
            }
        }

        // Give the spawned task a chance to wind down on shutdown.
        // We don't wait long: the user has already asked to quit,
        // and the agent will be stopped by the dropped subscription.
        if let Some(handle) = running_task {
            handle.abort();
            let _ = handle.await;
        }

        // Drop the watcher guard explicitly so its `Drop`
        // tears down the notify watcher before the runtime exits.
        // Without this the variable would still be live across the
        // `tui.stop()` call below and trigger a clippy warning
        // about meaningless drops if we later wanted to be explicit.
        drop(theme_watcher_guard);

        tui.stop();

        // End-of-session banner: token-usage breakdown plus a
        // resume hint pointing at the active session id. Printed
        // *after* [`Tui::stop`] so the bytes land in the user's
        // regular shell scrollback rather than the alternate-screen
        // TUI buffer that gets cleared on exit. Mirrors the legacy
        // `aj` binary's shutdown output (see
        // `aj/src/main.rs::display_usage_summary` + the
        // `Session:` notice it emits right after).
        //
        // Reading the agent + log behind their `TokioMutex` is
        // safe here: the running task was aborted above, the
        // event-channel forwarder lives on its own task that
        // doesn't touch these mutexes, and the persistence
        // listener is no-op-and-quick when no events are firing.
        let summary = {
            let a = agent.lock().await;
            build_usage_summary(&a)
        };
        print_usage_summary(&summary);

        // Resume hint is gated on "the session is worth resuming",
        // i.e. it has at least one persisted user-thread leaf.
        // Fresh sessions where the user quit without typing
        // anything don't get a hint — there's nothing meaningful
        // to come back to. The check covers both the "user
        // submitted at least one prompt this session" and "we
        // resumed a session that already had content" paths in one
        // shot since the persistence listener writes user
        // messages inline before the run returns.
        let (resume_eligible, session_id) = {
            let l = log.lock().await;
            (
                l.latest_leaf(ThreadFilter::USER).is_some(),
                l.session_id().to_string(),
            )
        };
        if resume_eligible {
            print_resume_hint(&session_id);
        }

        run_result
    }
}

/// Pull one event off the agent's bus channel. Wrapped in a tiny
/// helper so the `tokio::select!` arm reads cleanly. Returns `None`
/// if the channel closes (the agent dropped) — the main loop treats
/// that as a transient blip and keeps the TUI alive.
async fn recv_event(rx: &mut UnboundedReceiver<AgentEvent>) -> Option<AgentEvent> {
    rx.recv().await
}

/// Pull one [`Theme`] off the theme-watcher channel. Mirrors the
/// shape of [`recv_event`] but accepts an `Option<&mut
/// UnboundedReceiver<Theme>>` so the `tokio::select!` arm in
/// [`InteractiveMode::run`] stays clean whether or not the
/// fs-watcher started successfully. When the receiver is absent
/// (no watcher / `None`), the future pends forever — the arm
/// effectively becomes a no-op in the `select!`.
async fn recv_theme(rx: Option<&mut UnboundedReceiver<Theme>>) -> Option<Theme> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// State for an active selector overlay.
///
/// Each variant pairs the overlay's [`OverlayHandle`] (so the host
/// can hide it on completion) with a typed outcome handle that the
/// component populates when the user confirms or cancels.
enum OpenSelector {
    Thinking {
        handle: OverlayHandle,
        outcome: ThinkingOutcomeHandle,
        /// When opened via the command palette, the (hidden)
        /// palette's handle + outcome slot. On cancel we un-hide
        /// it and restore host-side polling so the palette stays
        /// usable; on confirm we tear it down. `None` when the
        /// selector was reached directly via `/thinking`.
        parent_palette: Option<ParentPalette>,
    },
    Model {
        handle: OverlayHandle,
        outcome: ModelOutcomeHandle,
        parent_palette: Option<ParentPalette>,
    },
    Session {
        handle: OverlayHandle,
        outcome: SessionOutcomeHandle,
        parent_palette: Option<ParentPalette>,
    },
    /// Prompt-history search overlay. `Enter` recalls the chosen
    /// prompt into the editor; `Esc` (or back-stack pop) closes it.
    PromptHistory {
        handle: OverlayHandle,
        outcome: PromptHistoryOutcomeHandle,
        parent_palette: Option<ParentPalette>,
    },
    Palette {
        handle: OverlayHandle,
        outcome: CommandPaletteOutcomeHandle,
    },
    /// Read-only `/help` overlay. Both Esc and Enter close it; if
    /// opened via the palette, the parent palette is restored on
    /// close so the back-stack stays coherent.
    Help {
        handle: OverlayHandle,
        outcome: crate::modes::interactive::components::help_overlay::HelpOverlayOutcomeHandle,
        parent_palette: Option<ParentPalette>,
    },
    /// Provider picker for `/login` / `/logout`. `mode` decides what
    /// confirming a provider does: start the OAuth browser flow, or
    /// remove the stored credential.
    AuthPicker {
        handle: OverlayHandle,
        outcome: AuthPickerOutcomeHandle,
        parent_palette: Option<ParentPalette>,
        mode: AuthPickerMode,
    },
    /// Read-only `/auth` status overlay. Both Esc and Enter close it.
    AuthStatus {
        handle: OverlayHandle,
        outcome: AuthStatusOutcomeHandle,
        parent_palette: Option<ParentPalette>,
    },
}

/// What confirming a provider in the [`OpenSelector::AuthPicker`]
/// overlay should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthPickerMode {
    /// Start the provider's OAuth browser login flow.
    Login,
    /// Remove the provider's stored `auth.json` credential.
    Logout,
}

/// An in-flight OAuth login the host is tracking.
///
/// The spawned login task lives in the main loop's `login_task`
/// `JoinHandle`; this struct carries everything the host needs to
/// tear the UI down: the dialog's overlay handle, the provider's
/// display name (for the completion notice), and the cancel flag the
/// dialog (Esc) and Ctrl+C flip.
struct LoginSession {
    provider_name: String,
    handle: OverlayHandle,
    cancel: Arc<AtomicBool>,
}

/// Snapshot of a palette pushed underneath a sub-selector. Held by
/// each child selector so that on cancel we can both un-hide the
/// palette and restore the host's `OpenSelector::Palette` tracking
/// (without re-installing this, the palette becomes input-wedged
/// because nothing polls its outcome slot).
#[derive(Clone)]
struct ParentPalette {
    handle: OverlayHandle,
    outcome: CommandPaletteOutcomeHandle,
}

/// Result of dispatching a `/...`-prefixed editor submission.
enum SlashHandled {
    /// Stay in the main loop. Optionally present a transient notice
    /// to the chat scrollback and/or open a selector overlay.
    Continue {
        selector: Option<OpenSelector>,
        notice: Option<String>,
    },
    /// User asked to quit; the host breaks out of the loop.
    Quit,
}

/// Result of polling an open selector after a TUI input event.
enum SelectorPollOutcome {
    /// The selector is still waiting for input.
    StillOpen(OpenSelector),
    /// The selector closed (confirmed or cancelled). The optional
    /// notice describes what happened so the host can render a
    /// status line in the chat scrollback. `follow_up`, when set,
    /// is a slash-prefixed string the main loop should re-feed
    /// through [`handle_slash_command`] — the command palette
    /// uses this to chain into a sub-selector (e.g. the user
    /// picked `/model` from the palette, so the main loop now
    /// dispatches `/model` for real). The follow-up path keeps
    /// the recursion at the main-loop level rather than calling
    /// `handle_slash_command` from inside
    /// [`handle_selector_outcome`] — which would require threading
    /// the model catalog and other slash-only dependencies
    /// through the outcome handler.
    Closed {
        notice: Option<String>,
        follow_up: Option<PaletteFollowUp>,
        /// When set, a `/login` provider pick the host should turn
        /// into a launched OAuth flow (the picker can't spawn the
        /// task itself — that's the main loop's job, where the login
        /// session state and the task `select!` arm live).
        start_login: Option<String>,
    },
}

/// Deferred chain from the command palette to a real slash
/// command. Carries the palette's (hidden) overlay handle + outcome
/// slot so the main loop can stash both on the sub-selector for the
/// back-stack (and so the palette stays pollable after a cancel
/// returns to it), or tear it down if the follow-up doesn't open a
/// sub-selector.
struct PaletteFollowUp {
    input: String,
    parent_palette: ParentPalette,
}

/// Wrap a notice string in the [`AgentEvent::Notice`] shape so we
/// can route it through the existing event pump for rendering. The
/// pump's `Notice` arm appends a dim text row to the chat slot,
/// which is exactly the look we want for slash-command status
/// feedback.
fn notice_event(text: &str) -> AgentEvent {
    AgentEvent::Notice {
        agent_id: aj_agent::events::AgentId::Main,
        text: text.to_string(),
    }
}

/// Wrap a warning string in the [`AgentEvent::Warning`] shape so
/// the event pump renders it with the warning style (yellow dim
/// text row in the chat scrollback).
fn warning_event(text: &str) -> AgentEvent {
    AgentEvent::Warning {
        agent_id: aj_agent::events::AgentId::Main,
        text: text.to_string(),
    }
}

/// Wrap an error string in the [`AgentEvent::Error`] shape so the
/// event pump renders it with the error style (red dim text row in
/// the chat scrollback). Used for startup diagnostics that mean a
/// user-supplied input was rejected wholesale (e.g. an unparseable
/// `config.toml`).
fn error_event(text: &str) -> AgentEvent {
    AgentEvent::Error {
        agent_id: aj_agent::events::AgentId::Main,
        text: text.to_string(),
    }
}

/// Resolve the startup theme name from `config.theme`. When the key
/// is unset the interactive TUI defaults to `light`; an explicit
/// name passes through unchanged. (A failed *load* of that name is a
/// separate concern handled by [`Theme::load`], which falls back to
/// the bundled `dark` palette.)
fn resolve_theme_name(configured: Option<&str>) -> &str {
    configured.unwrap_or("light")
}

/// Build the chat-scrollback "Context:" notice listing every
/// agents.md-style file the agent injected into its system prompt.
/// Returns either `"Context: (none)"` (when no instruction files
/// were discovered) or a multi-line listing with one row per file
/// formatted as `  - <tildified path> (<kind label>)` so the user
/// can verify which guidance is actually active.
fn build_context_notice(env: &AgentEnv) -> String {
    if env.context_files.is_empty() {
        "Context: (none)".to_string()
    } else {
        let mut lines = String::from("Context:");
        for file in &env.context_files {
            lines.push_str(&format!(
                "\n  - {} ({})",
                display_path(&file.path),
                file.kind.label()
            ));
        }
        lines
    }
}

/// The exact sandbox-warning string the binary emits at startup
/// unless `AJ_DISABLE_SANDBOX_WARNING` is set in the environment.
/// Kept in a `const` so it's easy to assert on in tests.
const SANDBOX_WARNING: &str = "WARNING: AJ has no sandboxing or permission checks. The agent can execute \
     arbitrary commands on your system. Do not use AJ if you don't understand what \
     this means. Set AJ_DISABLE_SANDBOX_WARNING=1 to suppress this warning.";

/// Returns `true` when the sandbox warning should be shown — i.e.
/// when `AJ_DISABLE_SANDBOX_WARNING` is unset in the environment.
/// Mirrors the legacy binary's
/// `std::env::var("AJ_DISABLE_SANDBOX_WARNING").is_err()` check
/// exactly: setting the var to *any* value (including the empty
/// string) suppresses the warning.
fn sandbox_warning_enabled() -> bool {
    std::env::var("AJ_DISABLE_SANDBOX_WARNING").is_err()
}

/// Push the editor's border tint for the given thinking level.
///
/// Builds a fresh closure off the shared [`ThemeHandle`] and hands
/// it to [`EditorComponent::set_border_color`]; the editor drops
/// its render cache so the next frame paints with the new tint.
/// No-op if the editor slot is missing (e.g. during test setup).
///
/// The closure resolves through the [`ThemeHandle`] on each call
/// so a runtime theme reload (the fs-watcher arm of the main
/// select loop) reskins the border automatically without
/// re-invoking this helper.
fn apply_editor_border_for_thinking(
    tui: &mut Tui,
    theme: &ThemeHandle,
    level: Option<&aj_models::ThinkingConfig>,
) {
    if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
        editor.set_border_color(editor_border_color_for_thinking(theme, level));
    }
}

/// Format the footer's model field as `"<model-id> <thinking-effort>"`.
///
/// The thinking effort (e.g. `"off"`, `"medium"`, `"max"`) is more
/// useful to surface persistently than the base URL — the URL is
/// stable per provider, while the effort changes per session.
fn format_footer_model(model_id: &str, thinking: &Option<aj_models::ThinkingConfig>) -> String {
    format!("{} {}", model_id, thinking_level_name(thinking))
}

/// Refresh the footer's model line so it reflects the agent's current
/// `(model id, thinking effort)`. No-op if the footer slot is missing.
fn refresh_footer_model(
    tui: &mut Tui,
    model_id: &str,
    thinking: &Option<aj_models::ThinkingConfig>,
) {
    if let Some(footer) = tui.get_mut_as::<Footer>(SlotIndex::Footer.idx()) {
        footer.set_model(Some(format_footer_model(model_id, thinking)));
    }
}

/// Map the agent's live default thinking back onto its persisted
/// `config.toml` representation. The forward map in
/// `Agent::with_provider` collapses [`ConfigThinkingLevel::Off`] to
/// `None`; this is its exact inverse, so a popup choice round-trips
/// through `config.toml` unchanged.
fn config_thinking_level(thinking: Option<&aj_models::ThinkingConfig>) -> ConfigThinkingLevel {
    use aj_models::ThinkingConfig;
    match thinking {
        None => ConfigThinkingLevel::Off,
        Some(ThinkingConfig::Low) => ConfigThinkingLevel::Low,
        Some(ThinkingConfig::Medium) => ConfigThinkingLevel::Medium,
        Some(ThinkingConfig::High) => ConfigThinkingLevel::High,
        Some(ThinkingConfig::XHigh) => ConfigThinkingLevel::XHigh,
        Some(ThinkingConfig::Max) => ConfigThinkingLevel::Max,
    }
}

/// Apply `mutate` to the shared [`Config`] and persist it to
/// `~/.aj/config.toml`. Selector outcomes change live agent/TUI state
/// for the running session; this mirrors the change into the
/// persisted config so it survives a restart.
///
/// Persistence is best-effort: a save failure returns a user-facing
/// notice (to append to the action's confirmation) rather than an
/// error, since the in-memory change already took effect. Returns
/// `None` on success.
fn persist_config(
    config: &Arc<std::sync::Mutex<Config>>,
    mutate: impl FnOnce(&mut Config),
) -> Option<String> {
    let mut cfg = config.lock().expect("config mutex poisoned");
    mutate(&mut cfg);
    match cfg.save() {
        Ok(()) => None,
        Err(err) => Some(format!("(couldn't save to config.toml: {err})")),
    }
}

/// Inner-content row count for the compact overlays (palette, help,
/// model / thinking pickers). Total rendered height including chrome
/// is `PALETTE_OVERLAY_INNER_ROWS + 4`. Tuned to fit comfortably on a
/// standard 24-row terminal. The content-heavy overlays (session
/// switcher, prompt history) size their rows dynamically instead — see
/// [`large_overlay_inner_rows`].
const PALETTE_OVERLAY_INNER_ROWS: usize = 17;

/// Sizing/anchor used by the command palette and the compact pickers
/// (model / thinking / help). Centered, fills ~70% of the terminal
/// width with a 72-col floor and an 88-col ceiling so the box doesn't
/// stretch uncomfortably wide on large monitors. Height is fixed at
/// `PALETTE_OVERLAY_INNER_ROWS + 4` to match the stable height the
/// [`aj_tui::components::overlay_window::OverlayWindow`] renders;
/// pinning the compositor's height to the exact value keeps narrow
/// terminals from reserving extra rows.
fn palette_overlay_options() -> OverlayOptions {
    OverlayOptions {
        anchor: OverlayAnchor::Center,
        width: Some(SizeValue::Percent(70.0)),
        min_width: Some(72),
        max_width: Some(88),
        max_height: Some(SizeValue::Absolute(PALETTE_OVERLAY_INNER_ROWS + 4)),
        ..OverlayOptions::default()
    }
}

/// Floor / ceiling for the inner-content row count of a large
/// overlay. The floor keeps the box usable on a standard 24-row
/// terminal; the ceiling stops it from swallowing the whole screen
/// on a very tall one.
const LARGE_OVERLAY_MIN_INNER_ROWS: usize = 14;
const LARGE_OVERLAY_MAX_INNER_ROWS: usize = 32;

/// Height policy for the two content-heavy overlays (session switcher
/// and prompt history): the inner-content row budget the
/// [`aj_tui::components::overlay_window::OverlayWindow`] renders, given
/// the live terminal height. Plugged into the window via
/// [`aj_tui::components::overlay_window::OverlayWindow::with_dynamic_height`]
/// so the box height tracks terminal resizes.
///
/// Scales to ~80% of `term_rows`, subtracts the window chrome, and
/// clamps the result to `[LARGE_OVERLAY_MIN_INNER_ROWS,
/// LARGE_OVERLAY_MAX_INNER_ROWS]` so the box stays usable on a 24-row
/// terminal without swallowing a very tall one.
fn large_overlay_inner_rows(term_rows: usize) -> usize {
    SizeValue::Percent(80.0)
        .resolve(term_rows)
        .saturating_sub(OVERLAY_WINDOW_CHROME_ROWS)
        .clamp(LARGE_OVERLAY_MIN_INNER_ROWS, LARGE_OVERLAY_MAX_INNER_ROWS)
}

/// Compositor options for the two content-heavy overlays.
///
/// Width fills ~85% of the terminal (72-col floor, 120-col ceiling).
/// The [`OverlayWindow`] sizes its own height reactively via
/// [`large_overlay_inner_rows`], so `max_height` here is only a safety
/// net: `Percent(100)` resolves to the available terminal height, which
/// clamps (truncates) the box on a terminal too short to hold it while
/// never capping it on a roomy one. A frozen `Absolute` would instead
/// truncate the box on a terminal that grew after the overlay opened.
fn large_overlay_options() -> OverlayOptions {
    OverlayOptions {
        anchor: OverlayAnchor::Center,
        width: Some(SizeValue::Percent(85.0)),
        min_width: Some(72),
        max_width: Some(120),
        max_height: Some(SizeValue::Percent(100.0)),
        ..OverlayOptions::default()
    }
}

/// Chrome rows an [`aj_tui::components::overlay_window::OverlayWindow`]
/// adds around its inner content: top + bottom border and top + bottom
/// blank padding.
const OVERLAY_WINDOW_CHROME_ROWS: usize = 4;

/// Subtitle for overlays that accept a selection: `"Enter to
/// confirm  •  Esc to close"`, with both key labels resolved from
/// the keybindings manager so user rebindings of
/// `tui.input.submit` / `tui.select.cancel` flow through to the
/// hint text. Falls back to the default labels when the actions
/// are somehow unbound. The same wording (`close`) is used for
/// every confirmable overlay — palette, thinking, model, session
/// — so the visual language stays uniform.
fn subtitle_confirm_close() -> String {
    let confirm = aj_tui::keybindings::format_action_shortcut("tui.input.submit")
        .unwrap_or_else(|| "Enter".to_string());
    let cancel = aj_tui::keybindings::format_action_shortcut("tui.select.cancel")
        .unwrap_or_else(|| "Esc".to_string());
    let close_all = aj_tui::keybindings::format_action_shortcut(
        crate::config::keybindings::ACTION_OVERLAY_CLOSE_ALL,
    );
    match close_all {
        // Surface the close-all chord only when it differs from the
        // cancel chord — otherwise the hint duplicates itself.
        Some(k) if k != cancel => {
            format!("{confirm} to confirm  \u{2022}  {cancel} back  \u{2022}  {k} close")
        }
        _ => format!("{confirm} to confirm  \u{2022}  {cancel} to close"),
    }
}

/// Subtitle for read-only overlays (the help screen): just the
/// resolved cancel key + `"to close"`.
fn subtitle_close() -> String {
    let cancel = aj_tui::keybindings::format_action_shortcut("tui.select.cancel")
        .unwrap_or_else(|| "Esc".to_string());
    let close_all = aj_tui::keybindings::format_action_shortcut(
        crate::config::keybindings::ACTION_OVERLAY_CLOSE_ALL,
    );
    match close_all {
        Some(k) if k != cancel => format!("{cancel} back  \u{2022}  {k} close"),
        _ => format!("{cancel} to close"),
    }
}

/// Tear down every visible overlay: the current selector plus its
/// parent palette (if any). Returns control to the chat editor.
///
/// Distinct from the per-variant Esc/cancel path in
/// [`handle_selector_outcome`], which pops one level (sub-selector
/// back to palette). This helper is invoked by the
/// `aj.overlay.close_all` chord and unconditionally removes both
/// the current overlay and its parent from the overlay stack.
fn close_all_overlays(tui: &mut Tui, sel: OpenSelector) {
    match sel {
        OpenSelector::Palette { handle, .. } => {
            tui.hide_overlay(&handle);
        }
        OpenSelector::Thinking {
            handle,
            parent_palette,
            ..
        }
        | OpenSelector::Model {
            handle,
            parent_palette,
            ..
        }
        | OpenSelector::Session {
            handle,
            parent_palette,
            ..
        }
        | OpenSelector::PromptHistory {
            handle,
            parent_palette,
            ..
        }
        | OpenSelector::Help {
            handle,
            parent_palette,
            ..
        }
        | OpenSelector::AuthPicker {
            handle,
            parent_palette,
            ..
        }
        | OpenSelector::AuthStatus {
            handle,
            parent_palette,
            ..
        } => {
            tui.hide_overlay(&handle);
            if let Some(parent) = parent_palette {
                tui.hide_overlay(&parent.handle);
            }
        }
    }
}

/// Subtitle for the OAuth login dialog overlay: how to submit a
/// pasted code and how to cancel, with key labels resolved from the
/// keybindings manager.
fn subtitle_login() -> String {
    let submit = aj_tui::keybindings::format_action_shortcut("tui.input.submit")
        .unwrap_or_else(|| "Enter".to_string());
    let cancel = aj_tui::keybindings::format_action_shortcut("tui.select.cancel")
        .unwrap_or_else(|| "Esc".to_string());
    format!(
        "Ctrl+Y to copy URL  \u{2022}  {submit} to submit pasted code  \u{2022}  {cancel} to cancel"
    )
}

/// Mount the OAuth login dialog overlay for `provider_id` and spawn
/// the provider's login flow on a task.
///
/// The dialog and the flow's [`TuiOAuthCallbacks`] share a
/// [`LoginDialogState`] (display lines + pending input), a pending-
/// input sender slot, and a cancel flag. The returned
/// [`LoginSession`] + `JoinHandle` are tracked by the main loop: its
/// login `select!` arm surfaces the result and hides the overlay, and
/// the cancel-poll aborts the task when the flag flips.
async fn start_login_session(
    tui: &mut Tui,
    auth: &AuthStorage,
    theme: &ThemeHandle,
    provider_id: &str,
) -> Result<(
    LoginSession,
    tokio::task::JoinHandle<Result<(), aj_models::auth::AuthError>>,
)> {
    let provider_name = auth
        .oauth_provider_ids()
        .await
        .into_iter()
        .find(|(id, _)| id == provider_id)
        .map(|(_, name)| name)
        .unwrap_or_else(|| provider_id.to_string());

    // Shared handles: the dialog (UI thread) holds clones; the
    // originals move into the login task's callbacks.
    let state = Arc::new(std::sync::Mutex::new(LoginDialogState::default()));
    let pending_input = Arc::new(std::sync::Mutex::new(None));
    let cancel = Arc::new(AtomicBool::new(false));

    // Seed a line so the dialog isn't blank before the flow's first
    // callback lands.
    state
        .lock()
        .expect("login dialog state poisoned")
        .lines
        .push(LoginLine::Progress("Starting login…".to_string()));

    let dialog = LoginDialogComponent::new(
        theme,
        Arc::clone(&state),
        Arc::clone(&pending_input),
        Arc::clone(&cancel),
    );
    let window = aj_tui::components::overlay_window::OverlayWindow::new(
        &format!("Log in — {provider_name}"),
        Box::new(dialog),
        crate::config::theme::overlay_window_theme(theme),
        PALETTE_OVERLAY_INNER_ROWS,
    )
    .with_subtitle(&subtitle_login());
    let handle = tui.show_overlay(Box::new(window), palette_overlay_options());

    let render = tui.handle();
    let auth_for_task = auth.clone();
    let provider_for_task = provider_id.to_string();
    let task = tokio::spawn(async move {
        let callbacks = TuiOAuthCallbacks::new(state, pending_input, render);
        auth_for_task.login(&provider_for_task, &callbacks).await
    });

    Ok((
        LoginSession {
            provider_name,
            handle,
            cancel,
        },
        task,
    ))
}

/// Dispatch a freshly-submitted slash-prefixed line.
#[allow(clippy::too_many_arguments)]
async fn handle_slash_command(
    tui: &mut Tui,
    auth: &AuthStorage,
    agent: Arc<TokioMutex<Agent>>,
    model_catalog: Arc<Vec<aj_models::registry::ModelInfo>>,
    run_config: Arc<std::sync::Mutex<RunConfigSnapshot>>,
    log: &mut Arc<TokioMutex<ConversationLog>>,
    persistence_handle: &mut SubscriptionHandle,
    pump: &mut EventPump,
    conversation_persistence: &ConversationPersistence,
    theme: &ThemeHandle,
    text: &str,
    parent_palette: Option<ParentPalette>,
    turn_running: bool,
) -> SlashHandled {
    let result = match slash_dispatch(text) {
        SlashAction::OpenCommandPalette => {
            debug_assert!(
                parent_palette.is_none(),
                "command palette has no parent palette"
            );
            use crate::modes::interactive::components::command_palette::CommandPaletteComponent;
            let inner = CommandPaletteComponent::new(select_list_theme(theme), 13);
            let outcome = inner.outcome_handle();
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Command Palette",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                PALETTE_OVERLAY_INNER_ROWS,
            )
            .with_subtitle(&subtitle_confirm_close());
            let handle = tui.show_overlay(Box::new(window), palette_overlay_options());
            SlashHandled::Continue {
                selector: Some(OpenSelector::Palette { handle, outcome }),
                notice: None,
            }
        }
        SlashAction::OpenThinkingSelector => {
            let current = run_config
                .lock()
                .expect("run config mutex poisoned")
                .thinking
                .clone();
            let inner = ThinkingSelectorComponent::new(select_list_theme(theme), current);
            let outcome = inner.outcome_handle();
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Thinking effort",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                PALETTE_OVERLAY_INNER_ROWS,
            )
            .with_subtitle(&subtitle_confirm_close());
            let handle = tui.show_overlay(Box::new(window), palette_overlay_options());
            SlashHandled::Continue {
                selector: Some(OpenSelector::Thinking {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        SlashAction::OpenModelSelector => {
            // Read the active (provider, id) pair from the loop-side
            // snapshot so the overlay can pre-select the active row.
            // Never touches the agent, so it's safe mid-turn.
            let (provider, id) = {
                let cfg = run_config.lock().expect("run config mutex poisoned");
                (cfg.model_key.0.clone(), cfg.model_key.1.clone())
            };
            let identity = ModelIdentityRef {
                provider: &provider,
                id: &id,
            };
            // Clone the catalog into the component — the component
            // owns it for the lifetime of the overlay so we don't
            // pay an extra Arc indirection on every rebuild.
            let inner = ModelSelectorComponent::new(
                select_list_theme(theme),
                (*model_catalog).clone(),
                Some(&identity),
                None,
            );
            let outcome = inner.outcome_handle();
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Select model",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                PALETTE_OVERLAY_INNER_ROWS,
            )
            .with_subtitle(&subtitle_confirm_close());
            let handle = tui.show_overlay(Box::new(window), palette_overlay_options());
            SlashHandled::Continue {
                selector: Some(OpenSelector::Model {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        SlashAction::OpenLoginSelector => {
            let providers = auth.oauth_provider_ids().await;
            if providers.is_empty() {
                SlashHandled::Continue {
                    selector: None,
                    notice: Some("No OAuth providers are available to log in to.".to_string()),
                }
            } else {
                let mut items = Vec::with_capacity(providers.len());
                for (id, name) in &providers {
                    let status = crate::auth::provider_status(auth, id, Some(name)).await;
                    items.push(AuthProviderItem {
                        provider_id: id.clone(),
                        label: name.clone(),
                        description: status.summary,
                    });
                }
                let inner = AuthPickerComponent::new(select_list_theme(theme), items);
                let outcome = inner.outcome_handle();
                let window = aj_tui::components::overlay_window::OverlayWindow::new(
                    "Log in",
                    Box::new(inner),
                    crate::config::theme::overlay_window_theme(theme),
                    PALETTE_OVERLAY_INNER_ROWS,
                )
                .with_subtitle(&subtitle_confirm_close());
                let handle = tui.show_overlay(Box::new(window), palette_overlay_options());
                SlashHandled::Continue {
                    selector: Some(OpenSelector::AuthPicker {
                        handle,
                        outcome,
                        parent_palette: parent_palette.clone(),
                        mode: AuthPickerMode::Login,
                    }),
                    notice: None,
                }
            }
        }
        SlashAction::OpenLogoutSelector => {
            let mut stored = auth.list().await.unwrap_or_default();
            if stored.is_empty() {
                SlashHandled::Continue {
                    selector: None,
                    notice: Some(
                        "No stored credentials to remove. (Env vars and --api-key aren't \
                         stored and can't be logged out.)"
                            .to_string(),
                    ),
                }
            } else {
                stored.sort();
                let oauth = auth.oauth_provider_ids().await;
                let mut items = Vec::with_capacity(stored.len());
                for id in &stored {
                    let name = oauth
                        .iter()
                        .find(|(pid, _)| pid == id)
                        .map(|(_, n)| n.as_str());
                    let status = crate::auth::provider_status(auth, id, name).await;
                    items.push(AuthProviderItem {
                        provider_id: id.clone(),
                        label: name.map(|n| n.to_string()).unwrap_or_else(|| id.clone()),
                        description: status.summary,
                    });
                }
                let inner = AuthPickerComponent::new(select_list_theme(theme), items);
                let outcome = inner.outcome_handle();
                let window = aj_tui::components::overlay_window::OverlayWindow::new(
                    "Log out",
                    Box::new(inner),
                    crate::config::theme::overlay_window_theme(theme),
                    PALETTE_OVERLAY_INNER_ROWS,
                )
                .with_subtitle(&subtitle_confirm_close());
                let handle = tui.show_overlay(Box::new(window), palette_overlay_options());
                SlashHandled::Continue {
                    selector: Some(OpenSelector::AuthPicker {
                        handle,
                        outcome,
                        parent_palette: parent_palette.clone(),
                        mode: AuthPickerMode::Logout,
                    }),
                    notice: None,
                }
            }
        }
        SlashAction::OpenAuthStatus => {
            let statuses = crate::auth::collect_statuses(auth).await;
            let inner = AuthStatusComponent::new(select_list_theme(theme), statuses);
            let outcome = inner.outcome_handle();
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Auth status",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                PALETTE_OVERLAY_INNER_ROWS,
            )
            .with_subtitle(&subtitle_close());
            let handle = tui.show_overlay(Box::new(window), palette_overlay_options());
            SlashHandled::Continue {
                selector: Some(OpenSelector::AuthStatus {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        // Session-changing commands reseed the agent's transcript
        // (an agent-locking op), so refuse them mid-turn rather than
        // freeze the loop. The user can cancel the turn and retry.
        SlashAction::OpenSessionSelector if turn_running => SlashHandled::Continue {
            selector: None,
            notice: Some(session_busy_notice("switch sessions")),
        },
        SlashAction::OpenSessionSelector => {
            // Read the current session id so the overlay pre-selects
            // the active row once it streams in.
            let current_session_id = {
                let l = log.lock().await;
                l.session_id().to_string()
            };

            // Scan previews on a blocking thread so the overlay opens
            // immediately and fills in incrementally as files are read.
            let scan = {
                let persistence = conversation_persistence.clone();
                move |emit: &mut dyn FnMut(Vec<_>)| {
                    persistence.list_session_previews_streaming(emit)
                }
            };

            let initial_inner_rows = large_overlay_inner_rows(usize::from(tui.terminal().rows()));
            // Session-selector chrome above the list: search input +
            // blank separator + the list's own scroll-info line.
            let session_max_rows = initial_inner_rows.saturating_sub(3).max(1);
            let inner = SessionSelectorComponent::new(
                select_list_theme(theme),
                Some(current_session_id),
                None,
                session_max_rows,
                tui.handle(),
                scan,
            );
            let outcome = inner.outcome_handle();
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Resume session",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                initial_inner_rows,
            )
            .with_dynamic_height(tui.handle(), large_overlay_inner_rows)
            .with_subtitle(&subtitle_confirm_close());
            let handle = tui.show_overlay(Box::new(window), large_overlay_options());
            SlashHandled::Continue {
                selector: Some(OpenSelector::Session {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        SlashAction::OpenPromptHistory => {
            // Both scans run on a blocking thread so the overlay opens
            // immediately and fills in incrementally. The
            // current-workspace scan starts on construction; the
            // all-workspaces scan is deferred to the first scope toggle.
            let workspace_scan = {
                let persistence = conversation_persistence.clone();
                move |emit: &mut dyn FnMut(Vec<_>)| workspace_history_streaming(&persistence, emit)
            };
            let all_scan = {
                let persistence = conversation_persistence.clone();
                move |emit: &mut dyn FnMut(Vec<_>)| match Config::get_sessions_base_dir_path() {
                    Ok(base) => all_workspaces_history_streaming(&base, emit),
                    Err(err) => {
                        tracing::debug!("could not resolve sessions base dir: {err}");
                        // Fall back to the current workspace so the
                        // toggle still shows something.
                        workspace_history_streaming(&persistence, emit)
                    }
                }
            };
            let initial_inner_rows = large_overlay_inner_rows(usize::from(tui.terminal().rows()));
            // Prompt-history chrome above the list: search input +
            // scope line + blank separator + the list's scroll-info
            // line.
            let history_max_rows = initial_inner_rows.saturating_sub(4).max(1);
            let inner = PromptHistorySearchComponent::new(
                select_list_theme(theme),
                history_max_rows,
                tui.handle(),
                workspace_scan,
                all_scan,
            );
            let outcome = inner.outcome_handle();
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Prompt history",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                initial_inner_rows,
            )
            .with_dynamic_height(tui.handle(), large_overlay_inner_rows)
            .with_subtitle(&subtitle_confirm_close());
            let handle = tui.show_overlay(Box::new(window), large_overlay_options());
            SlashHandled::Continue {
                selector: Some(OpenSelector::PromptHistory {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        SlashAction::NewSession if turn_running => SlashHandled::Continue {
            selector: None,
            notice: Some(session_busy_notice("start a new session")),
        },
        SlashAction::NewSession => match perform_new_session(
            tui,
            Arc::clone(&agent),
            log,
            persistence_handle,
            pump,
            conversation_persistence,
            theme,
        )
        .await
        {
            Ok(session_id) => SlashHandled::Continue {
                selector: None,
                notice: Some(format!("Started a fresh session ({session_id}).")),
            },
            Err(err) => SlashHandled::Continue {
                selector: None,
                notice: Some(format!("Failed to start a fresh session: {err}")),
            },
        },
        SlashAction::Help => {
            use crate::modes::interactive::components::help_overlay::HelpOverlayComponent;
            let inner = HelpOverlayComponent::new(select_list_theme(theme));
            let outcome = inner.outcome_handle();
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Help",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                PALETTE_OVERLAY_INNER_ROWS,
            )
            .with_subtitle(&subtitle_close());
            let handle = tui.show_overlay(Box::new(window), palette_overlay_options());
            SlashHandled::Continue {
                selector: Some(OpenSelector::Help {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        SlashAction::NotYetImplemented { message, .. } => SlashHandled::Continue {
            selector: None,
            notice: Some(message.to_string()),
        },
        SlashAction::Quit => SlashHandled::Quit,
        SlashAction::Unknown { input } => SlashHandled::Continue {
            selector: None,
            notice: Some(format!(
                "Unknown command: {input}. Try /help for available commands."
            )),
        },
    };

    // If we were dispatched as a palette follow-up, the palette
    // is hidden on the overlay stack. The arms that mount a
    // sub-selector consume `parent_palette` (passing it onto the
    // new `OpenSelector`), in which case the resulting
    // `SlashHandled::Continue` carries one of those variants.
    // For every other arm — `NewSession`, `Quit`, errors, etc. —
    // the palette would otherwise leak, so tear it down here.
    if let Some(palette) = parent_palette {
        let consumed = matches!(
            &result,
            SlashHandled::Continue {
                selector: Some(
                    OpenSelector::Thinking { .. }
                        | OpenSelector::Model { .. }
                        | OpenSelector::Session { .. }
                        | OpenSelector::PromptHistory { .. }
                        | OpenSelector::Help { .. }
                        | OpenSelector::AuthPicker { .. }
                        | OpenSelector::AuthStatus { .. },
                ),
                ..
            },
        );
        if !consumed {
            tui.hide_overlay(&palette.handle);
        }
    }
    result
}

/// Poll an open selector for its outcome and apply the result.
///
/// Returns [`SelectorPollOutcome::StillOpen`] if the user hasn't
/// pressed Enter or Esc yet; [`SelectorPollOutcome::Closed`] if the
/// overlay completed, with an optional notice describing what
/// happened.
#[allow(clippy::too_many_arguments)]
async fn handle_selector_outcome(
    tui: &mut Tui,
    selector: OpenSelector,
    auth: &AuthStorage,
    agent: Arc<TokioMutex<Agent>>,
    run_config: Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: Arc<std::sync::Mutex<Config>>,
    log: &mut Arc<TokioMutex<ConversationLog>>,
    persistence_handle: &mut SubscriptionHandle,
    pump: &mut EventPump,
    conversation_persistence: &ConversationPersistence,
    theme: &ThemeHandle,
) -> SelectorPollOutcome {
    match selector {
        OpenSelector::Thinking {
            handle,
            outcome,
            parent_palette,
        } => {
            let outcome_value = outcome.lock().expect("thinking outcome poisoned").take();
            match outcome_value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Thinking {
                    handle,
                    outcome,
                    parent_palette,
                }),
                Some(ThinkingSelectorOutcome::Confirmed(level)) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.hide_overlay(&parent.handle);
                    }
                    // Stage the new thinking effort into the loop-side
                    // snapshot; the next turn applies it. Never locks
                    // the agent, so it's safe while a turn is running
                    // (the in-flight turn keeps its effort; the change
                    // takes effect next turn). Read the model id back
                    // for the footer from the same snapshot.
                    let model_id = {
                        let mut cfg = run_config.lock().expect("run config mutex poisoned");
                        cfg.thinking = level.clone();
                        cfg.model_key.1.clone()
                    };
                    // Mirror the change onto the editor's border
                    // tint so the visual cue tracks the active
                    // reasoning mode.
                    apply_editor_border_for_thinking(tui, theme, level.as_ref());
                    // Footer surfaces the active thinking effort;
                    // refresh it so the change is visible without
                    // waiting for a turn.
                    refresh_footer_model(tui, &model_id, &level);
                    let name = thinking_level_name(&level);
                    // Persist the choice so it survives a restart.
                    let save_note = persist_config(&config, |c| {
                        c.thinking = Some(config_thinking_level(level.as_ref()));
                    });
                    let notice = match save_note {
                        Some(note) => format!("Thinking effort set to {name}. {note}"),
                        None => format!("Thinking effort set to {name}."),
                    };
                    SelectorPollOutcome::Closed {
                        notice: Some(notice),
                        follow_up: None,
                        start_login: None,
                    }
                }
                Some(ThinkingSelectorOutcome::Cancelled) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        // Pop back to the palette. Un-hide it on the
                        // overlay stack and restore the host-side
                        // `OpenSelector::Palette` tracking so its
                        // outcome slot is polled again — without
                        // restoring tracking the palette becomes
                        // input-wedged.
                        tui.set_overlay_hidden(&parent.handle, false);
                        return SelectorPollOutcome::StillOpen(OpenSelector::Palette {
                            handle: parent.handle,
                            outcome: parent.outcome,
                        });
                    }
                    SelectorPollOutcome::Closed {
                        notice: None,
                        follow_up: None,
                        start_login: None,
                    }
                }
            }
        }
        OpenSelector::Model {
            handle,
            outcome,
            parent_palette,
        } => {
            let outcome_value = outcome.lock().expect("model outcome poisoned").take();
            match outcome_value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Model {
                    handle,
                    outcome,
                    parent_palette,
                }),
                Some(ModelSelectorOutcome::Confirmed(info)) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.hide_overlay(&parent.handle);
                    }
                    // Construct a fresh provider handle from the
                    // picked catalog entry. Speed is intentionally
                    // left unset on selector-driven swaps; users who
                    // want the Anthropic fast-inference beta pass
                    // `--speed fast` at startup (it survives the
                    // swap if they re-pick a model that supports
                    // it; otherwise it silently degrades).
                    match from_model_info(auth, info.clone(), None) {
                        Ok(ResolvedModel {
                            provider,
                            model_info,
                            stream_options,
                        }) => {
                            let new_id = model_info.id.clone();
                            // Stage the swap into the loop-side snapshot
                            // (provider + model + options + the
                            // pre-select key); the next turn applies it.
                            // Never locks the agent, so it's safe
                            // mid-turn — the in-flight turn keeps its
                            // model and the swap takes effect next turn.
                            // Thinking effort is preserved; read it back
                            // for the footer format.
                            let current_thinking = {
                                let mut cfg = run_config.lock().expect("run config mutex poisoned");
                                cfg.provider = provider;
                                cfg.model_info = model_info;
                                cfg.stream_options = stream_options;
                                cfg.model_key = (info.provider.clone(), info.id.clone());
                                cfg.thinking.clone()
                            };
                            // Refresh the footer's model line so
                            // the user has visual confirmation of
                            // the swap without waiting for the next
                            // turn boundary.
                            refresh_footer_model(tui, &new_id, &current_thinking);
                            // Same idea for the footer's context-occupancy
                            // indicator: the new model has its own window
                            // size, so update the denominator immediately
                            // rather than waiting for the next turn.
                            pump.set_context_window(tui, info.context_window);
                            // Persist the model choice (provider + id)
                            // so it survives a restart. `model_url` is
                            // intentionally left untouched: it's a
                            // user-supplied endpoint override, not part
                            // of "which model", and pinning the
                            // catalog's base URL into it would freeze
                            // out future `models.json` updates.
                            let save_note = persist_config(&config, |c| {
                                c.model_api = Some(info.provider.clone());
                                c.model_name = Some(info.id.clone());
                            });
                            let confirm = format!(
                                "Model set to {} ({}/{}).",
                                info.name, info.provider, info.id
                            );
                            let notice = match save_note {
                                Some(note) => format!("{confirm} {note}"),
                                None => confirm,
                            };
                            SelectorPollOutcome::Closed {
                                notice: Some(notice),
                                follow_up: None,
                                start_login: None,
                            }
                        }
                        Err(err) => SelectorPollOutcome::Closed {
                            notice: Some(format!("Failed to switch to {}: {err}", info.name)),
                            follow_up: None,
                            start_login: None,
                        },
                    }
                }
                Some(ModelSelectorOutcome::Cancelled) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.set_overlay_hidden(&parent.handle, false);
                        return SelectorPollOutcome::StillOpen(OpenSelector::Palette {
                            handle: parent.handle,
                            outcome: parent.outcome,
                        });
                    }
                    SelectorPollOutcome::Closed {
                        notice: None,
                        follow_up: None,
                        start_login: None,
                    }
                }
            }
        }
        OpenSelector::Session {
            handle,
            outcome,
            parent_palette,
        } => {
            let outcome_value = outcome.lock().expect("session outcome poisoned").take();
            match outcome_value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Session {
                    handle,
                    outcome,
                    parent_palette,
                }),
                Some(SessionSelectorOutcome::Confirmed(preview)) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.hide_overlay(&parent.handle);
                    }
                    // No-op when the user picks the row that's
                    // already active. Saves the swap dance (and
                    // the chat-container clear that would briefly
                    // hide the user's scrollback).
                    let is_current = {
                        let l = log.lock().await;
                        l.session_id() == preview.session_id
                    };
                    if is_current {
                        return SelectorPollOutcome::Closed {
                            notice: Some(format!("Already on session {}.", preview.session_id)),
                            follow_up: None,
                            start_login: None,
                        };
                    }
                    match perform_session_swap(
                        tui,
                        Arc::clone(&agent),
                        log,
                        persistence_handle,
                        pump,
                        conversation_persistence,
                        theme,
                        &preview.session_id,
                    )
                    .await
                    {
                        Ok(()) => SelectorPollOutcome::Closed {
                            notice: Some(format!("Switched to session {}.", preview.session_id)),
                            follow_up: None,
                            start_login: None,
                        },
                        Err(err) => SelectorPollOutcome::Closed {
                            notice: Some(format!(
                                "Failed to switch to session {}: {err}",
                                preview.session_id
                            )),
                            follow_up: None,
                            start_login: None,
                        },
                    }
                }
                Some(SessionSelectorOutcome::Cancelled) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.set_overlay_hidden(&parent.handle, false);
                        return SelectorPollOutcome::StillOpen(OpenSelector::Palette {
                            handle: parent.handle,
                            outcome: parent.outcome,
                        });
                    }
                    SelectorPollOutcome::Closed {
                        notice: None,
                        follow_up: None,
                        start_login: None,
                    }
                }
            }
        }
        OpenSelector::PromptHistory {
            handle,
            outcome,
            parent_palette,
        } => match outcome.take() {
            None => SelectorPollOutcome::StillOpen(OpenSelector::PromptHistory {
                handle,
                outcome,
                parent_palette,
            }),
            Some(PromptHistoryOutcome::Recalled { text }) => {
                tui.hide_overlay(&handle);
                if let Some(parent) = parent_palette {
                    tui.hide_overlay(&parent.handle);
                }
                // Recall replaces the editor buffer (it does not
                // submit) so the user can edit before sending.
                if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
                    editor.set_text(&text);
                }
                tui.request_render();
                SelectorPollOutcome::Closed {
                    notice: None,
                    follow_up: None,
                    start_login: None,
                }
            }
            Some(PromptHistoryOutcome::Cancelled) => {
                tui.hide_overlay(&handle);
                if let Some(parent) = parent_palette {
                    tui.set_overlay_hidden(&parent.handle, false);
                    return SelectorPollOutcome::StillOpen(OpenSelector::Palette {
                        handle: parent.handle,
                        outcome: parent.outcome,
                    });
                }
                SelectorPollOutcome::Closed {
                    notice: None,
                    follow_up: None,
                    start_login: None,
                }
            }
        },
        OpenSelector::Help {
            handle,
            outcome,
            parent_palette,
        } => {
            use crate::modes::interactive::components::help_overlay::HelpOverlayOutcome;
            match outcome.take() {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Help {
                    handle,
                    outcome,
                    parent_palette,
                }),
                Some(HelpOverlayOutcome::Closed) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.set_overlay_hidden(&parent.handle, false);
                        return SelectorPollOutcome::StillOpen(OpenSelector::Palette {
                            handle: parent.handle,
                            outcome: parent.outcome,
                        });
                    }
                    SelectorPollOutcome::Closed {
                        notice: None,
                        follow_up: None,
                        start_login: None,
                    }
                }
            }
        }
        OpenSelector::AuthPicker {
            handle,
            outcome,
            parent_palette,
            mode,
        } => {
            use crate::modes::interactive::components::auth_picker::AuthPickerOutcome;
            let value = outcome.lock().expect("auth picker outcome poisoned").take();
            match value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::AuthPicker {
                    handle,
                    outcome,
                    parent_palette,
                    mode,
                }),
                Some(AuthPickerOutcome::Cancelled) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.set_overlay_hidden(&parent.handle, false);
                        return SelectorPollOutcome::StillOpen(OpenSelector::Palette {
                            handle: parent.handle,
                            outcome: parent.outcome,
                        });
                    }
                    SelectorPollOutcome::Closed {
                        notice: None,
                        follow_up: None,
                        start_login: None,
                    }
                }
                Some(AuthPickerOutcome::Confirmed(provider_id)) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.hide_overlay(&parent.handle);
                    }
                    match mode {
                        // Login is async + long-running: hand the
                        // provider id back so the main loop mounts the
                        // dialog and spawns the flow.
                        AuthPickerMode::Login => SelectorPollOutcome::Closed {
                            notice: None,
                            follow_up: None,
                            start_login: Some(provider_id),
                        },
                        // Logout is a quick disk write we can do inline.
                        AuthPickerMode::Logout => {
                            let notice = match auth.logout(&provider_id).await {
                                Ok(()) => format!("Logged out of {provider_id}."),
                                Err(err) => {
                                    format!("Failed to log out of {provider_id}: {err}")
                                }
                            };
                            SelectorPollOutcome::Closed {
                                notice: Some(notice),
                                follow_up: None,
                                start_login: None,
                            }
                        }
                    }
                }
            }
        }
        OpenSelector::AuthStatus {
            handle,
            outcome,
            parent_palette,
        } => {
            use crate::modes::interactive::components::auth_status::AuthStatusOutcome;
            match outcome.take() {
                None => SelectorPollOutcome::StillOpen(OpenSelector::AuthStatus {
                    handle,
                    outcome,
                    parent_palette,
                }),
                Some(AuthStatusOutcome::Closed) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.set_overlay_hidden(&parent.handle, false);
                        return SelectorPollOutcome::StillOpen(OpenSelector::Palette {
                            handle: parent.handle,
                            outcome: parent.outcome,
                        });
                    }
                    SelectorPollOutcome::Closed {
                        notice: None,
                        follow_up: None,
                        start_login: None,
                    }
                }
            }
        }
        OpenSelector::Palette { handle, outcome } => {
            use crate::modes::interactive::components::command_palette::CommandPaletteOutcome;
            match outcome.take() {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Palette { handle, outcome }),
                Some(CommandPaletteOutcome::Cancelled) => {
                    tui.hide_overlay(&handle);
                    SelectorPollOutcome::Closed {
                        notice: None,
                        follow_up: None,
                        start_login: None,
                    }
                }
                Some(CommandPaletteOutcome::Confirmed { input }) => {
                    // Hide (don't remove) the palette so we can
                    // restore it if the follow-up command opens a
                    // sub-selector and the user cancels back. If
                    // the follow-up doesn't open a sub-selector,
                    // `handle_slash_command` tears the palette
                    // down once it finishes dispatching. The
                    // outcome handle is cloned into the follow-up
                    // so the host can re-install
                    // `OpenSelector::Palette` after a child cancel,
                    // keeping the palette pollable.
                    tui.set_overlay_hidden(&handle, true);
                    SelectorPollOutcome::Closed {
                        notice: None,
                        follow_up: Some(PaletteFollowUp {
                            input,
                            parent_palette: ParentPalette {
                                handle,
                                outcome: outcome.clone(),
                            },
                        }),
                        start_login: None,
                    }
                }
            }
        }
    }
}

/// Swap the agent + log + persistence wiring over to the session
/// identified by `session_id`.
///
/// This is the in-process equivalent of `aj continue <id>`:
/// the binary stays up and the user keeps their open editor, but
/// the agent's transcript and the persistence target both rebind
/// to the new session file.
///
/// Steps:
/// 1. Resume the new [`ConversationLog`] from disk.
/// 2. Repair any interrupted tool uses recorded in that log so
///    the resumed transcript ends cleanly on a user-role message.
/// 3. Build a fresh `Conversation` view from the repaired log and
///    seed it into the agent (`seed_messages`,
///    `seed_sub_agent_counter`).
/// 4. Resolve a system prompt for the new session: reuse the one
///    persisted at the log's root if present, otherwise assemble
///    fresh from the current environment and freeze it onto the
///    log so subsequent resumes hit the same cache.
/// 5. Replace `*log` with the new `Arc<TokioMutex<…>>` and
///    `*persistence_handle` with a fresh subscription so future
///    events flow into the new file. The old handle's `Drop` runs
///    here, detaching the previous persistence listener.
/// 6. Clear the chat container, build a fresh [`EventPump`], and
///    re-replay the new log through it so the user sees the
///    swapped transcript.
/// 7. Refresh the header's session-id line.
///
/// Returns `Err` if any of the file-level operations fail; in
/// that case the agent's previous bindings are left intact (we
/// only replace `*log` and `*persistence_handle` after the new
/// log has been built and the seed/system-prompt steps have
/// succeeded).
#[allow(clippy::too_many_arguments)]
async fn perform_session_swap(
    tui: &mut Tui,
    agent: Arc<TokioMutex<Agent>>,
    log: &mut Arc<TokioMutex<ConversationLog>>,
    persistence_handle: &mut SubscriptionHandle,
    pump: &mut EventPump,
    conversation_persistence: &ConversationPersistence,
    theme: &ThemeHandle,
    session_id: &str,
) -> Result<()> {
    // 1. Resume the new log from disk.
    let mut new_log = ConversationLog::resume(conversation_persistence, session_id)
        .with_context(|| format!("failed to resume session {session_id}"))?;

    // 2. Repair any interrupted tool uses so the resumed
    //    transcript ends on a user-role message — same dance the
    //    startup path does.
    if let Some(head) = new_log.latest_leaf(ThreadFilter::USER) {
        let conversation = new_log.linearize(&head, ThreadFilter::USER);
        repair_interrupted_tool_uses(&mut new_log, &conversation)?;
    }

    // 3. Build the post-repair transcript and seed it into the
    //    agent. Also reseed the sub-agent counter so freshly-
    //    minted ids in this session don't collide with any
    //    sub-agent subtrees already on the log.
    let messages: Vec<_> = if let Some(head) = new_log.latest_leaf(ThreadFilter::USER) {
        new_log
            .linearize(&head, ThreadFilter::USER)
            .agent_messages()
    } else {
        Vec::new()
    };
    let max_agent_id = new_log.max_agent_id();

    // 4. Resolve the system prompt for the new session. If the
    //    log already carries one (persisted at the root entry),
    //    reuse it verbatim to keep the model's prompt-cache warm.
    //    Otherwise assemble fresh from the agent's env and
    //    freeze it onto the new log so subsequent resumes hit
    //    the same cache.
    let prompt = if let Some(persisted) = new_log.system_prompt() {
        persisted.to_string()
    } else {
        let assembled = {
            let a = agent.lock().await;
            a.assemble_system_prompt()
        };
        if new_log.is_empty() {
            new_log.set_system_prompt(assembled.clone())?;
        }
        assembled
    };

    {
        let mut a = agent.lock().await;
        a.seed_messages(messages);
        if let Some(max_id) = max_agent_id {
            a.seed_sub_agent_counter(max_id);
        }
        a.set_assembled_system_prompt(prompt);
    }

    // 5. Swap the shared log + persistence handle. Order matters:
    //    build the new persistence subscription *before* dropping
    //    the old `persistence_handle`, otherwise a stray bus
    //    event landing in the window would have no listener and
    //    silently fail to persist. In practice the bus is quiet
    //    here: `handle_slash_command` refuses `/resume` while a turn
    //    is running, and a turn can't start while the session
    //    selector is open (the editor isn't focused), so no turn is
    //    in flight during a swap. The ordering keeps the invariant
    //    honest regardless.
    let new_log_arc = Arc::new(TokioMutex::new(new_log));
    let new_handle = {
        let a = agent.lock().await;
        a.subscribe(persistence_listener(Arc::clone(&new_log_arc)))
    };
    *persistence_handle = new_handle;
    *log = new_log_arc;

    // 6. Clear the chat container and replay the new log
    //    through a fresh event pump so the user sees the swapped
    //    transcript in the scrollback. Carry the current
    //    `hide_thinking_block` mode across the swap so a
    //    `aj.thinking.toggle` toggle the user pressed before the swap is
    //    still in effect afterwards.
    if let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx()) {
        chat.clear();
    }
    let hide_thinking_block = pump.hide_thinking_block();
    let show_image_in_terminal = pump.show_image_in_terminal();
    let context_window = agent.lock().await.model_info().context_window;
    *pump = EventPump::new(
        chat_theme(theme),
        hide_thinking_block,
        false,
        show_image_in_terminal,
        context_window,
    );
    // Seed the footer indicator before replay so the row shows
    // `?/<window>` even for resumed sessions where replay produces
    // no `TurnUsage` events (e.g. a log with only a user prompt).
    pump.sync_footer(tui);
    {
        let l = log.lock().await;
        for event in replay(&l) {
            pump.handle(tui, &event);
        }
    }

    // 7. Refresh the header so the new session id is visible in
    //    the top bar without waiting for the next turn boundary.
    if let Some(header) = tui.get_mut_as::<Header>(SlotIndex::Header.idx()) {
        header.set_session_id(Some(session_id.to_string()));
        header.set_notice(Some("Resumed conversation".to_string()));
    }

    tui.request_render();
    Ok(())
}

/// Start a fresh session.
///
/// Symmetric to [`perform_session_swap`] but creates instead of
/// resuming: mint a new [`ConversationLog`], freeze a fresh
/// system prompt onto it, swap it into the shared `log` slot,
/// rebind the persistence subscription, clear the chat, and
/// refresh the header. The previous session is preserved on disk
/// untouched.
///
/// Returns the new session id on success so the caller can surface
/// it in a status notice.
async fn perform_new_session(
    tui: &mut Tui,
    agent: Arc<TokioMutex<Agent>>,
    log: &mut Arc<TokioMutex<ConversationLog>>,
    persistence_handle: &mut SubscriptionHandle,
    pump: &mut EventPump,
    conversation_persistence: &ConversationPersistence,
    theme: &ThemeHandle,
) -> Result<String> {
    // 1. Mint a new log on disk.
    let mut new_log = ConversationLog::create(conversation_persistence)
        .context("failed to create a fresh conversation log")?;
    let new_session_id = new_log.session_id().to_string();

    // 2. Assemble a system prompt from the agent's env and freeze
    //    it onto the new log so a future resume picks it up.
    let prompt = {
        let a = agent.lock().await;
        a.assemble_system_prompt()
    };
    new_log
        .set_system_prompt(prompt.clone())
        .context("failed to persist system prompt on new session")?;

    // 3. Reset the agent's in-memory state: empty transcript, the
    //    freshly-assembled system prompt, sub-agent counter back
    //    to zero (no persisted subtrees on this new log).
    {
        let mut a = agent.lock().await;
        a.seed_messages(Vec::new());
        a.set_assembled_system_prompt(prompt);
        a.seed_sub_agent_counter(0);
    }

    // 4. Swap the shared log and persistence handle. Same ordering
    //    rule as [`perform_session_swap`]: build the new subscription
    //    before dropping the old one so a stray bus event can't
    //    arrive without a listener. `handle_slash_command` refuses
    //    `/new` while a turn is running, so we can't be mid-turn
    //    here, but the ordering keeps the invariant honest regardless.
    let new_log_arc = Arc::new(TokioMutex::new(new_log));
    let new_handle = {
        let a = agent.lock().await;
        a.subscribe(persistence_listener(Arc::clone(&new_log_arc)))
    };
    *persistence_handle = new_handle;
    *log = new_log_arc;

    // 5. Clear the chat container and rebuild the event pump so any
    //    in-flight assistant/tool-execution components don't leak
    //    into the fresh session. Carry `hide_thinking_block` across
    //    so a prior `aj.thinking.toggle` toggle stays in effect.
    if let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx()) {
        chat.clear();
    }
    let hide_thinking_block = pump.hide_thinking_block();
    let show_image_in_terminal = pump.show_image_in_terminal();
    let context_window = agent.lock().await.model_info().context_window;
    *pump = EventPump::new(
        chat_theme(theme),
        hide_thinking_block,
        false,
        show_image_in_terminal,
        context_window,
    );
    // Seed the footer indicator so the row shows `?/<window>`
    // on the fresh session before the first turn lands.
    pump.sync_footer(tui);

    // 6. Refresh the header with the new session id and a friendly
    //    notice so the user sees the swap in the top bar.
    if let Some(header) = tui.get_mut_as::<Header>(SlotIndex::Header.idx()) {
        header.set_session_id(Some(new_session_id.clone()));
        header.set_notice(Some("Fresh session".to_string()));
    }

    tui.request_render();
    Ok(new_session_id)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use aj_conf::{AgentEnv, ContextFile, ContextFileKind};

    use super::*;

    /// Build an [`AgentEnv`] for use in the helper tests below.
    /// Working directory / OS / date / git root are all stubbed —
    /// only `context_files` matters for the `Context:` notice
    /// builder.
    fn env_with(context_files: Vec<ContextFile>) -> AgentEnv {
        AgentEnv {
            working_directory: PathBuf::from("/tmp"),
            git_root_directory: None,
            operating_system: "linux".to_string(),
            today_date: "2025-01-01".to_string(),
            context_files,
        }
    }

    #[test]
    fn resolve_theme_name_defaults_to_light_when_unset() {
        assert_eq!(resolve_theme_name(None), "light");
    }

    #[test]
    fn resolve_theme_name_passes_explicit_name_through() {
        assert_eq!(resolve_theme_name(Some("dark")), "dark");
        assert_eq!(resolve_theme_name(Some("solarized")), "solarized");
    }

    #[test]
    fn build_context_notice_empty_renders_none_marker() {
        let env = env_with(Vec::new());
        assert_eq!(build_context_notice(&env), "Context: (none)");
    }

    #[test]
    fn build_context_notice_lists_files_with_label_and_tildified_path() {
        // `display_path` tildifies under `$HOME`, so build the path
        // off the live `HOME` env var to keep the assertion stable
        // across machines.
        let home = std::env::var("HOME").expect("HOME set in test env");
        let user_path = PathBuf::from(&home).join(".agents/AGENTS.md");
        let project_path = PathBuf::from("/var/project/AGENTS.md");
        let env = env_with(vec![
            ContextFile {
                path: user_path,
                kind: ContextFileKind::UserInstructions,
                content: String::new(),
            },
            ContextFile {
                path: project_path,
                kind: ContextFileKind::ProjectInstructions,
                content: String::new(),
            },
        ]);

        let notice = build_context_notice(&env);
        let expected = "Context:\n  - ~/.agents/AGENTS.md (user instructions)\n  \
             - /var/project/AGENTS.md (project instructions)";
        assert_eq!(notice, expected);
    }

    #[test]
    fn sandbox_warning_enabled_tracks_env_var_presence() {
        // SAFETY: tests in this module run on a single thread so
        // mutating the process env is fine. We save/restore the
        // pre-existing value so other tests aren't disturbed.
        let prev = std::env::var("AJ_DISABLE_SANDBOX_WARNING").ok();

        // SAFETY: single-threaded test runner per `cargo test`'s
        // default; no other test reads this var concurrently.
        unsafe {
            std::env::remove_var("AJ_DISABLE_SANDBOX_WARNING");
        }
        assert!(
            sandbox_warning_enabled(),
            "warning should show when the var is absent"
        );

        // SAFETY: same scope as above.
        unsafe {
            std::env::set_var("AJ_DISABLE_SANDBOX_WARNING", "1");
        }
        assert!(
            !sandbox_warning_enabled(),
            "warning should be suppressed when the var is set"
        );

        // Matches legacy `is_err()` semantics: even an empty value
        // counts as "set" and suppresses the warning.
        // SAFETY: same scope as above.
        unsafe {
            std::env::set_var("AJ_DISABLE_SANDBOX_WARNING", "");
        }
        assert!(
            !sandbox_warning_enabled(),
            "warning should stay suppressed when the var is set to the empty string"
        );

        // Restore.
        // SAFETY: same scope as above.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AJ_DISABLE_SANDBOX_WARNING", v),
                None => std::env::remove_var("AJ_DISABLE_SANDBOX_WARNING"),
            }
        }
    }

    #[test]
    fn notice_event_carries_main_agent_id() {
        let evt = notice_event("hi");
        match evt {
            AgentEvent::Notice { agent_id, text } => {
                assert_eq!(agent_id, aj_agent::events::AgentId::Main);
                assert_eq!(text, "hi");
            }
            other => panic!("expected Notice, got {other:?}"),
        }
    }

    #[test]
    fn warning_event_carries_main_agent_id() {
        let evt = warning_event(SANDBOX_WARNING);
        match evt {
            AgentEvent::Warning { agent_id, text } => {
                assert_eq!(agent_id, aj_agent::events::AgentId::Main);
                assert_eq!(text, SANDBOX_WARNING);
            }
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    #[test]
    fn session_busy_notice_names_the_action_and_points_at_cancel() {
        assert_eq!(
            session_busy_notice("switch sessions"),
            "Can't switch sessions while a turn is running — press Ctrl+C to cancel it first."
        );
        assert_eq!(
            session_busy_notice("start a new session"),
            "Can't start a new session while a turn is running — press Ctrl+C to cancel it first."
        );
    }
}
