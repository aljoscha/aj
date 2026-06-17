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
//! - editor extensions ([`editor_ext`]) that bolt `@file`
//!   autocomplete onto the shared [`aj_tui::EditorComponent`];
//! - the keybinding map ([`keys`]).
//!
//! [`AgentEvent`]: aj_agent::events::AgentEvent

pub mod components;
pub mod editor_ext;
pub mod event_pump;
pub mod footer_data;
pub mod keys;
pub mod layout;
pub mod render_settings;
pub mod session;
pub mod shutdown;
#[cfg(test)]
pub(crate) mod test_support;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::queue::MessageQueues;
use aj_agent::types::UsageSummary;
use aj_agent::{Agent, SharedAgent, SubAgentRegistry, TurnError};
use aj_conf::{
    AgentEnv, Config, ConfigSpeed, ConfigThinkingDisplay, ConfigThinkingLevel, Severity,
    SystemPromptSource, display_path,
};
use aj_models::auth::AuthStorage;
use aj_models::provider::Provider;
use aj_models::registry::{ModelInfo, ModelRegistry, validate_thinking_level};
use aj_models::types::{Speed, StreamOptions, UserContent};
use aj_models::{ThinkingConfig, speed_from_name, speed_name, thinking_config_from_name};
use aj_session::{ConversationPersistence, ThreadFilter};
use aj_tools::{BuiltinToolOptions, get_builtin_tools};
use aj_tui::EditorComponent;
use aj_tui::components::editor::Editor;
use aj_tui::terminal::ProcessTerminal;
use aj_tui::tui::{OverlayAnchor, OverlayHandle, OverlayOptions, SizeValue, Tui, TuiEvent};
use anyhow::{Context, Result};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::cli::args::{Args, Command};
use crate::config::commands::{CommandAction, load_model_catalog, thinking_level_name};
use crate::config::theme::{
    Theme, ThemeHandle, ThemeWatcherGuard, editor_border_color_for_thinking, select_list_theme,
    settings_list_theme, watch_user_theme,
};
use crate::model::{ResolvedModel, from_model_info};
use crate::modes::interactive::components::agent_picker::{
    AgentPickerComponent, AgentPickerOutcome, AgentPickerOutcomeHandle,
};
use crate::modes::interactive::components::auth_picker::{
    AuthPickerComponent, AuthProviderItem, OutcomeHandle as AuthPickerOutcomeHandle,
};
use crate::modes::interactive::components::auth_status::{
    AuthStatusComponent, AuthStatusOutcomeHandle,
};
use crate::modes::interactive::components::command_palette::CommandPaletteOutcomeHandle;
use crate::modes::interactive::components::footer::Footer;
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
use crate::modes::interactive::components::settings_window::{
    ChangesHandle as SettingsChangesHandle, CorrectionsHandle as SettingsCorrectionsHandle,
    MODEL_SETTING_ID, OutcomeHandle as SettingsOutcomeHandle, SettingsCurrentValues,
    SettingsSubmenu, SettingsWindowComponent, SettingsWindowOutcome, UNSET_VALUE,
};
use crate::modes::interactive::components::skills_window::{
    ChangesHandle as SkillsChangesHandle, OutcomeHandle as SkillsOutcomeHandle, SkillRow,
    SkillsWindowComponent, SkillsWindowOutcome,
};
use crate::modes::interactive::components::task_output::{
    TaskOutputComponent, TaskOutputOutcome, TaskOutputOutcomeHandle,
};
use crate::modes::interactive::components::thinking_selector::{
    OutcomeHandle as ThinkingOutcomeHandle, ThinkingSelectorComponent, ThinkingSelectorOutcome,
};
use crate::modes::interactive::components::usage_status::{
    UsageStatusComponent, UsageStatusOutcomeHandle,
};
use crate::modes::interactive::editor_ext::{DEFAULT_MAX_ENTRIES, PromptHistory};
use crate::modes::interactive::event_pump::{
    EventPump, set_editor_submit_enabled, take_submitted_prompt,
};
use crate::modes::interactive::layout::{SlotIndex, build_layout};
use crate::modes::interactive::render_settings::RenderSettings;
use crate::modes::interactive::session::{RestoreContext, SessionEntry, SessionSpec, SessionWorld};
use crate::modes::interactive::shutdown::{
    print_resume_hint, print_session_usage, print_usage_summary,
};
use crate::turn::{TurnPolicy, TurnStart};

/// Loop-side snapshot of the agent's run configuration.
///
/// The interactive loop spawns each turn into a task that holds the
/// agent `TokioMutex` for the turn's entire duration, so the loop
/// itself must never `agent.lock().await` — that would suspend the
/// whole `select!` (including its Ctrl+C arm) until the turn ends.
///
/// This snapshot is therefore the loop-side source of truth for "what
/// the next turn runs against". The model and thinking selectors
/// mutate it without touching the agent; the footer renders the active
/// model and effort from it; and the submit handler copies it into the
/// agent just before each turn starts (while holding the turn's own
/// lock, which is uncontended because no turn is in flight yet). A
/// model or thinking change made mid-turn is thus accepted — and shown
/// in the footer — immediately, but only takes effect on the *next*
/// turn: the in-flight turn keeps the config it captured when it
/// started.
pub(crate) struct RunConfigSnapshot {
    /// Provider handle the next turn streams against.
    provider: Arc<dyn Provider>,
    /// Registry (or scripted) metadata for `provider`'s model.
    model_info: Arc<ModelInfo>,
    /// Per-call stream options (thinking-display mode, etc.).
    stream_options: StreamOptions,
    /// Default thinking effort for the next turn.
    thinking: Option<ThinkingConfig>,
    /// Inference speed mode baked into `stream_options`' headers.
    /// Tracked explicitly so bundle rebuilds (model swap, resume
    /// restore) preserve it and so it can be recorded in the
    /// session log. `None` means standard.
    speed: Option<Speed>,
    /// `(provider_id, model_id)` the model selector pre-selects.
    /// Tracked explicitly rather than read off `model_info` because
    /// the scripted path's provider id (from `--model-api`) differs
    /// from `model_info.provider`, which is always `"scripted"`.
    model_key: (String, String),
}

/// Loop-side staged settings for one sub-agent. Each axis is
/// `Some(..)` only if the user changed it for this agent; axes left
/// `None` keep whatever the agent itself holds (its spawn-time
/// inheritance). The `Option<Option<..>>` split on thinking/speed
/// matters: `Some(None)` means "explicitly set to off/standard".
///
/// Entries live in `SessionWorld::sub_overrides` and are re-applied
/// idempotently at every turn start of the agent they belong to —
/// an entry is the user's standing choice for that agent.
#[derive(Default)]
pub(crate) struct SubAgentOverrides {
    /// Full bundle swap from a model-selector confirm: provider handle,
    /// model info, stream options, and the `(provider, id)` key.
    pub(crate) bundle: Option<(
        Arc<dyn Provider>,
        Arc<ModelInfo>,
        StreamOptions,
        (String, String),
    )>,
    pub(crate) thinking: Option<Option<ThinkingConfig>>,
    pub(crate) speed: Option<Option<Speed>>,
}

/// Construct the loop-side run-config snapshot from a resolved
/// provider bundle.
///
/// The snapshot lives for the whole process and is the source of
/// truth both for the next turn's configuration and for the agents
/// built per session world (see [`session::SessionWorld::build`]).
fn build_run_config(
    config: &Config,
    provider: Arc<dyn Provider>,
    model_info: Arc<ModelInfo>,
    mut stream_options: StreamOptions,
    model_key: (String, String),
    speed: Option<Speed>,
) -> RunConfigSnapshot {
    crate::model::apply_thinking_display(&mut stream_options, config.thinking_display);
    RunConfigSnapshot {
        provider,
        model_info,
        stream_options,
        thinking: default_thinking_from_config(config.thinking),
        speed,
        model_key,
    }
}

/// Map a persisted `config.toml` thinking level onto the wire-level
/// default. Mirrors the mapping `Agent::with_provider` applies
/// (`Off` collapses to `None`); [`config_thinking_level`] is the
/// exact inverse.
fn default_thinking_from_config(level: Option<ConfigThinkingLevel>) -> Option<ThinkingConfig> {
    level.and_then(|level| match level {
        ConfigThinkingLevel::Off => None,
        ConfigThinkingLevel::Low => Some(ThinkingConfig::Low),
        ConfigThinkingLevel::Medium => Some(ThinkingConfig::Medium),
        ConfigThinkingLevel::High => Some(ThinkingConfig::High),
        ConfigThinkingLevel::XHigh => Some(ThinkingConfig::XHigh),
        ConfigThinkingLevel::Max => Some(ThinkingConfig::Max),
    })
}

/// User-facing notice shown when a session-changing command
/// (resume, new) is invoked while a turn is in flight.
///
/// A session change tears down the current world — agent, bus
/// subscriptions, pump — and rebuilds it from scratch, which must
/// never abort live work, so the commands are refused mid-turn.
/// `what` names the action, e.g. `"switch sessions"`.
fn session_busy_notice(what: &str) -> String {
    let cancel = crate::config::keybindings::fixed_keys::CTRL_C;
    format!("Can't {what} while a turn is running — press {cancel} to cancel it first.")
}

/// Counts of running work a quit would tear down, for the Ctrl+C
/// quit-arming guard: (agents, bash tasks).
///
/// Binary-driven turns plus running agent-backed tasks (background
/// sub-agent runs, which the `turns` JoinSet doesn't track) make up
/// the agent count; running bash tasks the task count. The
/// classification mirrors the footer's — an agent-backed task counts
/// as an agent, never as a task — though the agent counts can differ
/// (the footer counts running agents from pump events; this counts
/// the work a quit tears down).
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

/// Quit-arming notice for a Ctrl+C on an idle view while other work
/// runs: `"N agents / M tasks still running — press Ctrl+C again to
/// quit"`, each part present only when nonzero. Callers ensure at
/// least one count is nonzero.
fn quit_arm_notice(agents: usize, tasks: usize) -> String {
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
    let quit = crate::config::keybindings::fixed_keys::CTRL_C;
    format!(
        "{} still running — press {quit} again to quit",
        parts.join(" / ")
    )
}

/// Driver for the interactive TUI. Startup builds the
/// process-lifetime [`Shell`]; an outer loop in
/// [`InteractiveMode::run`] then builds, runs, and tears down one
/// [`SessionWorld`] per session.
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
        // effort, and `(provider_id, model_id)` for the model
        // selector to pre-select). The selectors mutate it without
        // locking the agent; the submit handler copies it into the
        // agent just before each turn. See [`RunConfigSnapshot`].
        // Credential store backing API-key resolution and the login /
        // logout / auth-status overlays. Cheap to clone
        // (`Arc`-backed); the resolver installed in
        // `crate::model::from_model_info` captures a clone and reads
        // it on every inference, so a mid-session login takes effect
        // without a restart.
        let auth = AuthStorage::at_default_path().context("failed to open ~/.aj/auth.json")?;

        // Resume-time settings restoration needs the registry +
        // auth store; scripted mode runs without either and skips
        // restoration entirely.
        let mut restore_context: Option<RestoreContext> = None;

        let run_config = if let Some(name) = &self.args.scripted {
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
            build_run_config(
                &config,
                provider,
                model_info,
                StreamOptions::default(),
                (current_provider, current_id),
                speed,
            )
        } else {
            // Load the registry once at startup; the same handle
            // also feeds resume-time settings restoration via the
            // `RestoreContext` below. (`load_model_catalog` further
            // down does its own cheap JSON read for the model
            // selector's snapshot.)
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
            restore_context = Some(RestoreContext {
                registry: Arc::new(registry),
                auth: auth.clone(),
            });
            let ResolvedModel {
                provider,
                model_info,
                stream_options,
            } = resolved;
            let model_key = (model_info.provider.clone(), model_info.id.clone());
            build_run_config(
                &config,
                provider,
                model_info,
                stream_options,
                model_key,
                speed,
            )
        };
        let run_config = Arc::new(std::sync::Mutex::new(run_config));

        // Apply a `--api-key` runtime override (if supplied) to the
        // resolved provider, then check whether *any* credential is
        // configured so we can nudge the user toward logging in when
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

        // Probe tmux for the options aj's rendering relies on
        // (synchronized output, OSC 8 hyperlinks, escape passthrough).
        // `None` when we're not in tmux or everything's already on; the
        // warning is emitted via the pump further below, like the auth
        // one above.
        let tmux_warning = crate::tmux_notice::startup_warning();

        // Snapshot the model catalog up-front so the model
        // selector overlay and the editor's argument completer
        // share a single load (registry::load reads JSON twice
        // otherwise — once per consumer).
        let model_catalog = load_model_catalog();

        // ---- Conversation log: resume or create -----------------------
        // `aj continue` with neither an explicit id nor a latest
        // session on disk degrades to a fresh session; that
        // resolution happens here, before the spec is built.
        let sessions_dir = Config::get_sessions_dir_path()?;
        let conversation_persistence = ConversationPersistence::new(sessions_dir);

        // Resolve the launch positionals (free-form messages plus `@file`
        // attachments) into the content to auto-submit, before the match
        // below moves `self.args.command`. Paths resolve relative to the
        // process working directory — where the user typed the command.
        // Consumed by the first `run_session` call via `mem::take`, so a
        // later in-process session switch starts clean.
        let mut launch_content = {
            let cwd = std::env::current_dir().unwrap_or_default();
            crate::cli::initial_input(&self.args, &cwd)?.into_content()
        };

        let spec = match self.args.command {
            Some(Command::Continue {
                session_id: Some(id),
                prompt: _,
            }) => SessionSpec::Resume {
                session_id: id,
                entry: SessionEntry::Startup,
            },
            Some(Command::Continue {
                session_id: None,
                prompt: _,
            }) => match conversation_persistence.get_latest_session_id()? {
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
        // The watcher follows the *configured* theme; a runtime theme
        // switch through the settings window reinstalls it for the
        // newly chosen name.
        let mut theme_watch = ThemeWatch::install(&configured_theme);

        // ---- First session world --------------------------------------
        // One shared render-settings handle for the whole process:
        // each session's pump gets a clone, so `alt+t` / `alt+o`
        // toggles survive a new-session or resume.
        let render_settings = RenderSettings::new(
            config.hide_thinking_block,
            false,
            config.image_show_in_terminal,
        );
        let mut world = SessionWorld::build(
            &config,
            &run_config,
            &render_settings,
            &theme,
            &conversation_persistence,
            &spec,
            restore_context.as_ref(),
            Arc::clone(&model_catalog),
        )?;

        // ---- Build the TUI --------------------------------------------
        let mut tui = Tui::new(Box::new(ProcessTerminal::new()));
        tui.start()
            .map_err(|e| anyhow::anyhow!("failed to start terminal: {e}"))?;
        build_layout(&mut tui, &theme, config.syntax_highlighting);

        // Tint the editor border to match the initial thinking level
        // so the user sees the active reasoning mode at a glance the
        // moment the TUI comes up. Updates flow through the same
        // helper on every thinking-effort change.
        let startup_thinking = {
            let cfg = run_config.lock().expect("run config mutex poisoned");
            cfg.thinking.clone()
        };
        apply_editor_border_for_thinking(&mut tui, &theme, startup_thinking.as_ref());

        // Install the path/symbol autocomplete provider on the
        // editor. The `@filename` fuzzy file picker and direct
        // path completion live here. Typing `/` at the empty prompt
        // opens the command palette overlay (see the editor's
        // palette trigger), not an inline popup.
        let working_directory = {
            let a = world.agent.lock().await;
            a.env().working_directory.clone()
        };
        if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
            let provider =
                aj_tui::autocomplete::CombinedAutocompleteProvider::new(working_directory);
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
        // polls it after `tui.handle_input` and runs
        // [`CommandAction::OpenCommandPalette`], so all palette-open
        // paths (leading `/`, `Ctrl+O`) converge on the same mounting
        // code.
        let palette_open_request: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        // Set when the user hits `aj.overlay.close_all` (default
        // `ctrl+c`) while any overlay is up. Drained after
        // `tui.handle_input` to tear down the entire overlay
        // back-stack — distinct from Esc's one-level pop.
        let close_all_request: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        // Set by the global `aj.history.open` chord (default
        // `ctrl+r`). Drained after `tui.handle_input` to run
        // [`CommandAction::OpenPromptHistory`], mirroring the
        // `palette_open_request` path. Dispatched without a parent
        // palette so `Esc` closes the overlay back to the editor.
        let history_open_request: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        // Set by the global `aj.agent.open` chord (default `alt+a`).
        // Drained after `tui.handle_input` to run
        // [`CommandAction::OpenAgentPicker`] (mirroring the history
        // chord), opening the agent picker.
        let agent_picker_open_request: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        {
            let flag = Arc::clone(&palette_open_request);
            if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
                editor.set_on_palette_trigger(Some(Arc::new(move || {
                    flag.store(true, Ordering::Relaxed);
                })));
            }
        }

        // Footer: working directory. The model line and context
        // indicator are pushed by `SessionWorld::install`'s footer
        // sync; the header's session id + banner are set by
        // `install` below as well.
        let footer_cwd = {
            let a = world.agent.lock().await;
            format!("{}", a.env().working_directory.display())
        };
        if let Some(footer) = tui.get_mut_as::<Footer>(SlotIndex::Footer.idx()) {
            footer.set_cwd(Some(footer_cwd));
        }

        // Bind the world to the TUI: chat reset, replay of any
        // resumed history, header session id + banner.
        world.install(&mut tui, &spec).await;

        // Startup notices: surface config-load diagnostics (parse
        // errors, unknown keys) first so a user with a broken
        // `config.toml` sees that before any other chrome, then
        // list the context stitched into the system prompt (base
        // prompt plus AGENTS.md / CLAUDE.md files), then (unless
        // suppressed) print the sandbox warning. All flow through
        // the pump's existing
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
            world.pump.handle(&mut tui, &event);
        }
        // The context notice only applies to fresh sessions: a
        // resumed session keeps the assembled prompt persisted in
        // its log, so the freshly-loaded env the notice describes
        // doesn't govern what's actually sent. Skill-discovery
        // warnings ride along under the same rule.
        if matches!(spec, SessionSpec::Create { .. }) {
            let (context_notice, skill_warnings) = {
                let a = world.agent.lock().await;
                let env = a.env();
                let warnings: Vec<String> = env
                    .skill_diagnostics
                    .iter()
                    .map(|d| d.to_string())
                    .collect();
                (build_context_notice(env), warnings)
            };
            world.pump.handle(&mut tui, &notice_event(&context_notice));
            for warning in &skill_warnings {
                world.pump.handle(&mut tui, &warning_event(warning));
            }
        }
        if sandbox_warning_enabled() {
            world.pump.handle(&mut tui, &warning_event(SANDBOX_WARNING));
        }
        if let Some(warning) = &startup_auth_warning {
            world.pump.handle(&mut tui, &warning_event(warning));
        }
        if let Some(warning) = &tmux_warning {
            world.pump.handle(&mut tui, &warning_event(warning));
        }
        // Settings restored from a resumed session's log (or the
        // reasons restoration fell back) surface like any other
        // startup notice.
        for notice in std::mem::take(&mut world.restore_notices) {
            world.pump.handle(&mut tui, &notice_event(&notice));
        }

        // Shared, mutable view of the on-disk config. Selector
        // outcomes (model / thinking / the settings window) mutate
        // this and persist it via `persist_config` so a choice made
        // in the TUI survives a restart. Held behind a std mutex
        // because the write is a quick synchronous read-merge-write
        // (`Config::persist_changed`) done off the guard, never awaited
        // across.
        let config = Arc::new(std::sync::Mutex::new(config));

        // Everything with process lifetime moves into the shell;
        // session worlds are rebuilt around it on every new-session /
        // resume.
        let mut shell = Shell {
            tui,
            theme,
            config,
            auth,
            model_catalog,
            run_config,
            conversation_persistence,
            render_settings,
            completed_sessions: Vec::new(),
            restore_context,
            palette_open_request,
            close_all_request,
            history_open_request,
            agent_picker_open_request,
        };

        // ---- Outer session loop ---------------------------------------
        // Each iteration drives one session world to completion.
        // A new-session or resume exits the per-session loop; the world
        // is torn down wholesale and a fresh one is built and
        // installed in its place. Quit and fatal errors break out,
        // carrying the final world (when one is still alive) for the
        // shutdown banner below.
        let (final_world, run_result): (Option<SessionWorld>, Result<()>) = loop {
            let spec = match run_session(
                &mut shell,
                &mut world,
                &mut theme_watch,
                std::mem::take(&mut launch_content),
            )
            .await
            {
                Ok(SessionExit::Quit) => break (Some(world), Ok(())),
                Err(fatal) => break (Some(world), Err(fatal)),
                Ok(SessionExit::New) => SessionSpec::Create {
                    entry: SessionEntry::Switch,
                },
                Ok(SessionExit::Switch(session_id)) => SessionSpec::Resume {
                    session_id,
                    entry: SessionEntry::Switch,
                },
            };

            // Snapshot the outgoing world's usage for the shutdown
            // banner before it is dropped. The replacement world's
            // usage starts at zero, so nothing is double-counted —
            // including on the fallback path below, which resumes the
            // same session in a brand-new world.
            let usage = world.usage_summary().await;
            shell
                .completed_sessions
                .push((world.session_id.clone(), usage));
            let previous_id = world.session_id.clone();

            let config_snapshot = shell.config.lock().expect("config mutex poisoned").clone();
            match build_next_world(
                &config_snapshot,
                &shell.run_config,
                &shell.render_settings,
                &shell.theme,
                &shell.conversation_persistence,
                spec,
                &previous_id,
                shell.restore_context.as_ref(),
                Arc::clone(&shell.model_catalog),
            ) {
                Ok(mut next) => {
                    next.world.install(&mut shell.tui, &next.spec).await;
                    // A resume may have restored the session's
                    // recorded settings into the run config; the
                    // install's footer sync already mirrors them, so
                    // only the editor border needs re-applying here.
                    // The view is Main after an install, so this
                    // resolves to the run config's thinking.
                    apply_editor_border_for_view(
                        &mut shell.tui,
                        &shell.theme,
                        &next.world.pump,
                        &shell.run_config,
                        AgentId::Main,
                    );
                    for notice in &next.notices {
                        next.world
                            .pump
                            .handle(&mut shell.tui, &notice_event(notice));
                    }
                    world = next.world;
                }
                Err(err) => break (None, Err(err)),
            }
        };

        // Drop the watcher explicitly so its guard's `Drop` tears
        // down the notify watcher before the runtime exits. Without
        // this the variable would still be live across the
        // `tui.stop()` call below and trigger a clippy warning
        // about meaningless drops if we later wanted to be explicit.
        drop(theme_watch);

        shell.tui.stop();

        // End-of-process banner: token-usage breakdown plus a resume
        // hint pointing at the live session id. Printed *after*
        // [`Tui::stop`] so the bytes land in the user's regular
        // shell scrollback rather than the alternate-screen TUI
        // buffer that gets cleared on exit.
        //
        // Reading the agent + log behind their `TokioMutex` is safe
        // here: in-flight turns were shut down before `run_session`
        // returned, the event-channel forwarder lives on its own
        // task that doesn't touch these mutexes, and the persistence
        // listener is no-op-and-quick when no events are firing.
        //
        // When the process spanned several sessions (new-session /
        // resume), each torn-down world's usage was snapshotted
        // into `completed_sessions`; itemize them in order, each
        // under a dim `Session: <id>` header, then the live world's
        // block. A single-session process prints one bare block.
        match final_world {
            Some(world) => {
                let summary = world.usage_summary().await;
                if shell.completed_sessions.is_empty() {
                    print_usage_summary(&summary);
                } else {
                    for (session_id, completed) in &shell.completed_sessions {
                        print_session_usage(session_id, completed);
                    }
                    print_session_usage(&world.session_id, &summary);
                }

                // Resume hint is gated on "the session is worth resuming",
                // i.e. it has at least one persisted user-thread leaf.
                // Fresh sessions where the user quit without typing
                // anything don't get a hint — there's nothing meaningful
                // to come back to. The check covers both the "user
                // submitted at least one prompt this session" and "we
                // resumed a session that already had content" paths in one
                // shot since the persistence listener writes user
                // messages inline before the run returns.
                let resume_eligible = {
                    let l = world.log.lock().await;
                    l.latest_leaf(ThreadFilter::USER).is_some()
                };
                if resume_eligible {
                    print_resume_hint(&world.session_id);
                }
            }
            // A fallback rebuild failed, so no world survived the
            // loop: print what the completed sessions accumulated and
            // skip the live block and the resume hint.
            None => {
                for (session_id, completed) in &shell.completed_sessions {
                    print_session_usage(session_id, completed);
                }
            }
        }

        run_result
    }
}

/// Process-lifetime state: everything that survives a session
/// switch. Session worlds ([`SessionWorld`]) are rebuilt around the
/// shell on every new-session or resume.
struct Shell {
    /// Terminal, layout, and editor. Never torn down on a session
    /// switch, so the editor draft, prompt-history ring, and raw
    /// mode survive without flicker.
    tui: Tui,
    /// Shared theme handle; a runtime reload re-points it in place.
    theme: ThemeHandle,
    /// Shared, mutable view of the on-disk config; selector
    /// outcomes mutate and persist it.
    config: Arc<std::sync::Mutex<Config>>,
    /// Credential store backing API-key resolution and login.
    auth: AuthStorage,
    /// Model catalog shared by the model selector and the
    /// editor's argument completer; loaded once at startup.
    model_catalog: Arc<Vec<ModelInfo>>,
    /// Loop-side snapshot of the next turn's run configuration;
    /// model / thinking choices made mid-process survive
    /// session switches through it.
    run_config: Arc<std::sync::Mutex<RunConfigSnapshot>>,
    /// Sessions-directory handle used to build session worlds and
    /// feed the session / prompt-history overlays.
    conversation_persistence: ConversationPersistence,
    /// Render toggles (`alt+t` / `alt+o`); each session's pump gets
    /// a clone, so the toggles survive switches.
    render_settings: RenderSettings,
    /// Usage snapshots of torn-down session worlds, in order, for
    /// the per-session shutdown banner.
    completed_sessions: Vec<(String, UsageSummary)>,
    /// Registry + auth store backing resume-time settings
    /// restoration; `None` in scripted mode (restoration disabled).
    restore_context: Option<RestoreContext>,
    /// Tripped by the editor's `/`-at-empty-prompt callback and the
    /// `aj.palette.open` chord; drained by the session loop.
    palette_open_request: Arc<AtomicBool>,
    /// Tripped by the `aj.overlay.close_all` chord while an overlay
    /// is up; drained by the session loop.
    close_all_request: Arc<AtomicBool>,
    /// Tripped by the `aj.history.open` chord; drained by the
    /// session loop.
    history_open_request: Arc<AtomicBool>,
    /// Tripped by the `aj.agent.open` chord; drained by the session
    /// loop.
    agent_picker_open_request: Arc<AtomicBool>,
}

/// Outcome of building the next session world after a switch
/// request: the world to install, the spec it was built for (the
/// requested one, or the fallback onto the previous session), and
/// the chat notices to pump after install (switch confirmation, or
/// the failure text followed by nothing — the fallback world's
/// install already announces itself).
struct NextWorld {
    world: SessionWorld,
    spec: SessionSpec,
    notices: Vec<String>,
}

/// Build the session world a new-session or resume request asks for.
///
/// If the requested build fails, falls back to resuming
/// `previous_session_id` — the session that just ended, whose log is
/// on disk and current — and reports the failure as the notice
/// instead. Returns `Err` only when the fallback build fails too,
/// which the outer session loop treats as fatal. Touches no TUI
/// state: installing the returned world and pumping its notices stay
/// with the caller.
#[allow(clippy::too_many_arguments)]
fn build_next_world(
    config: &Config,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    render_settings: &RenderSettings,
    theme: &ThemeHandle,
    persistence: &ConversationPersistence,
    requested: SessionSpec,
    previous_session_id: &str,
    restore: Option<&RestoreContext>,
    catalog: Arc<Vec<aj_models::registry::ModelInfo>>,
) -> Result<NextWorld> {
    match SessionWorld::build(
        config,
        run_config,
        render_settings,
        theme,
        persistence,
        &requested,
        restore,
        Arc::clone(&catalog),
    ) {
        Ok(mut world) => {
            let notice = match &requested {
                SessionSpec::Create { .. } => {
                    format!("Started a fresh session ({}).", world.session_id)
                }
                SessionSpec::Resume { session_id, .. } => {
                    format!("Switched to session {session_id}.")
                }
            };
            let mut notices = vec![notice];
            notices.append(&mut world.restore_notices);
            Ok(NextWorld {
                world,
                spec: requested,
                notices,
            })
        }
        Err(err) => {
            let failure = match &requested {
                SessionSpec::Create { .. } => {
                    format!("Failed to start a fresh session: {err}")
                }
                SessionSpec::Resume { session_id, .. } => {
                    format!("Failed to switch to session {session_id}: {err}")
                }
            };
            let fallback = SessionSpec::Resume {
                session_id: previous_session_id.to_string(),
                entry: SessionEntry::Switch,
            };
            let mut world = SessionWorld::build(
                config,
                run_config,
                render_settings,
                theme,
                persistence,
                &fallback,
                restore,
                catalog,
            )?;
            let mut notices = vec![failure];
            notices.append(&mut world.restore_notices);
            Ok(NextWorld {
                world,
                spec: fallback,
                notices,
            })
        }
    }
}

/// Why [`run_session`]'s per-session loop returned.
enum SessionExit {
    /// The user quit (Ctrl+C / Ctrl+D / the quit command, or the terminal
    /// stream ended); the process shuts down.
    Quit,
    /// A resume pick: rebuild onto the identified session.
    Switch(String),
    /// New session: rebuild onto a freshly minted session.
    New,
}

/// A session change requested by a command or selector. The
/// per-session loop maps it onto a [`SessionExit`] so the outer
/// loop in [`InteractiveMode::run`] can tear down the current world
/// and build the next one. Only emitted with no turn in flight.
enum SessionRequest {
    New,
    Resume(String),
}

impl SessionRequest {
    fn into_exit(self) -> SessionExit {
        match self {
            SessionRequest::New => SessionExit::New,
            SessionRequest::Resume(id) => SessionExit::Switch(id),
        }
    }
}

/// Drive one session world until the user quits, a session change
/// is requested, or a fatal error occurs.
///
/// Owns the per-session UI loop state — in-flight turns, the open
/// selector, an in-flight OAuth login. None of it can outlive the
/// session: a session change can only be requested while no turn,
/// overlay, or login is active. Whatever the exit reason, every
/// in-flight turn is shut down before this returns, so the caller
/// may drop the world without aborting live work.
async fn run_session(
    shell: &mut Shell,
    world: &mut SessionWorld,
    theme_watch: &mut ThemeWatch,
    launch_content: Vec<UserContent>,
) -> Result<SessionExit> {
    // ---- Main event loop ------------------------------------------
    // In-flight turns keyed by the agent running them. `JoinSet`
    // gives completion-as-they-finish and preserves panic detection
    // (`join_next` yields `Err(JoinError)`); `turn_cancels` holds the
    // binary's clone of each turn's cancel token, and its key set is
    // exactly "agents the binary is currently driving".
    let mut turns: JoinSet<(AgentId, Result<(), TurnError>)> = JoinSet::new();
    let mut turn_cancels: HashMap<AgentId, CancellationToken> = HashMap::new();
    // Implements the "press Ctrl+C again to quit" guard when the
    // viewed agent is idle but other agents or background tasks are
    // still running.
    let mut quit_armed = false;

    // A command like the thinking selector opens an overlay selector.
    // While an overlay is up the editor is not focused, but
    // `shell.tui.show_overlay` already routes input to the overlay,
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
    let mut login_task: Option<tokio::task::JoinHandle<Result<(), aj_models::auth::AuthError>>> =
        None;

    // Auto-submit the launch prompt (`aj <msg>` / `aj @file ...`) as the
    // first turn. Empty for any in-process session switch after the first.
    if !launch_content.is_empty() {
        spawn_turn(
            world,
            &shell.run_config,
            AgentId::Main,
            TurnStart::Content(launch_content),
            turn_policy(AgentId::Main, &shell.config),
            &mut turns,
            &mut turn_cancels,
        );
        sync_editor_enabled(&mut shell.tui);
    }

    let exit: Result<SessionExit> = loop {
        tokio::select! {
            biased;

            // --- Agent run finished ---
            joined = join_next_or_pending(&mut turns) => {
                match joined {
                    Ok((id, result)) => {
                        turn_cancels.remove(&id);
                        world.pump.mark_idle(&mut shell.tui, id);
                        // Main-turn completion bounds every nested
                        // initial spawn it started. Drain any sub
                        // still marked running that the binary is NOT
                        // independently driving (∉ turn_cancels) so a
                        // leaked sub-agent can't pin the
                        // footer/spinner. Independent continuations
                        // are in turn_cancels and survive.
                        if id == AgentId::Main {
                            for sub in world.pump.running_agents() {
                                if matches!(sub, AgentId::Sub(_))
                                    && !turn_cancels.contains_key(&sub)
                                {
                                    world.pump.mark_idle(&mut shell.tui, sub);
                                }
                            }
                        }
                        sync_editor_enabled(&mut shell.tui);
                        // Post-turn wake: deliver queued task notices
                        // and follow-up messages the moment the agent
                        // goes idle. This is the single wake path \u2014 the
                        // driver doesn't deliver queued work itself.
                        // (Steering was already drained mid-turn by the
                        // agent; this is the deferred work plus any
                        // notice that landed during an aborted turn.)
                        // `Agent::wake` is a no-op when nothing is
                        // pending, so a racing trigger is cheap.
                        if world.task_registry.has_notices(id)
                            || world.message_queues.has_pending(id)
                        {
                            spawn_wake_turn(
                                id,
                                world,
                                &shell.run_config,
                                turn_policy(id, &shell.config),
                                &mut turns,
                                &mut turn_cancels,
                            );
                            sync_editor_enabled(&mut shell.tui);
                        }
                        match result {
                            // The driver settled every automatic
                            // continuation (overflow recovery,
                            // queued-work delivery, threshold compaction)
                            // before returning, so the completion arm
                            // only renders the terminal outcome.
                            Ok(()) => {}
                            Err(TurnError::Aborted) => {
                                // The agent already emitted the synthetic
                                // aborted `MessageEnd`s, so the scrollback
                                // is consistent; a brief notice confirms
                                // Ctrl+C took effect and the session stays
                                // alive.
                                world.pump.handle(&mut shell.tui, &notice_event("Turn cancelled."));
                            }
                            Err(TurnError::Recoverable(err)) => {
                                // The driver already exhausted overflow
                                // recovery (folding the give-up message
                                // into the error chain) before handing
                                // this back, so surface it and keep the
                                // session going.
                                world.pump.handle(
                                    &mut shell.tui,
                                    &AgentEvent::Error {
                                        agent_id: id,
                                        text: format!("agent error: {err:#}"),
                                    },
                                );
                            }
                            Err(TurnError::Fatal(err)) => {
                                break Err(err);
                            }
                        }
                    }
                    Err(join_err) => {
                        break Err(anyhow::anyhow!("agent task panicked: {join_err}"));
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
                    shell.tui.hide_overlay(&session.handle);
                    let name = session.provider_name;
                    match login_outcome {
                        Ok(Ok(())) => {
                            world.pump.handle(
                                &mut shell.tui,
                                &notice_event(&format!("Logged in to {name}.")),
                            );
                        }
                        Ok(Err(err)) => {
                            world.pump.handle(
                                &mut shell.tui,
                                &warning_event(&format!("Login to {name} failed: {err}")),
                            );
                        }
                        // Aborted on Ctrl+C / Esc: the cancel-poll
                        // arm already surfaced a "cancelled" notice.
                        Err(join_err) if join_err.is_cancelled() => {}
                        Err(join_err) => {
                            world.pump.handle(
                                &mut shell.tui,
                                &warning_event(&format!("Login task error: {join_err}")),
                            );
                        }
                    }
                }
            }

            // --- TUI input / render ---
            maybe_event = shell.tui.next_event() => {
                let Some(event) = maybe_event else {
                    // Terminal stream ended: equivalent to
                    // Ctrl-C/Ctrl-D from the user's POV.
                    break Ok(SessionExit::Quit);
                };
                match event {
                    TuiEvent::Render => shell.tui.render(),
                    TuiEvent::Input(input) => {
                        // Ctrl+C semantics, in priority order.
                        // A visible overlay always wins: a Ctrl+C
                        // aimed at a modal dismisses the modal and
                        // leaves any turn running behind it intact.
                        //
                        // 1. Overlay up (`open_selector` is
                        //    `Some`): dismiss the overlay. Don't
                        //    break or cancel the turn; fall
                        //    through to the
                        //    `ACTION_OVERLAY_CLOSE_ALL`
                        //    interception below, which matches
                        //    `ctrl+c` by default and tears the
                        //    overlay stack down.
                        // 2. Login dialog up (`login_session` is
                        //    `Some`): the OAuth dialog is also a
                        //    modal, so it takes precedence over a
                        //    turn. Signal cancel; the cancel-poll
                        //    below tears the dialog down and
                        //    aborts the task.
                        // 3. Otherwise act on the agent you are
                        //    *viewing* (spec §4.5):
                        //    - Viewed agent has a binary-driven
                        //      turn (`turn_cancels`): cancel just
                        //      it. The cancel handle is the
                        //      binary's clone of the per-turn
                        //      `CancellationToken` passed to
                        //      `agent.prompt`; firing it propagates
                        //      to the agent's `execute_turn`
                        //      `select!`s and to every provider /
                        //      tool subscribed to the same token,
                        //      including the bash tool's process
                        //      group.
                        //    - Viewed agent is a sub running its
                        //      initial spawn (running but not in
                        //      `turn_cancels`): cancel the main
                        //      turn that owns it; the child token
                        //      cascades.
                        //    - Viewed agent idle but other agents
                        //      or background tasks still run:
                        //      don't cancel them; arm "press
                        //      Ctrl+C again to quit" and exit on
                        //      the second press.
                        //    - Nothing running anywhere: exit.
                        //
                        // Ctrl+D always exits regardless. The
                        // terminal is in raw mode so neither
                        // chord raises SIGINT; both arrive here
                        // as ordinary key events.

                        // Any non-Ctrl+C key disarms a pending
                        // "press again to quit".
                        if !input.is_ctrl('c') {
                            quit_armed = false;
                        }
                        if input.is_ctrl('c') {
                            if open_selector.is_some() {
                                // Overlay up: fall through to the
                                // close-all interception below.
                            } else if let Some(session) = login_session.as_ref() {
                                session.cancel.store(true, Ordering::Relaxed);
                                continue;
                            } else {
                                // Per-view Ctrl+C (spec §4.5): act on the agent you're viewing.
                                let active = world.pump.active_view(&mut shell.tui);
                                if let Some(token) = turn_cancels.get(&active) {
                                    // Viewed agent has a binary-driven turn: cancel just it.
                                    token.cancel();
                                    // Don't discard what the user lined
                                    // up: pull any queued message back
                                    // into the editor.
                                    yank_pending_into_editor(
                                        &mut shell.tui,
                                        &world.pump,
                                        &world.message_queues,
                                        active,
                                    );
                                    quit_armed = false;
                                    continue;
                                } else if world.pump.is_running(active) {
                                    // Viewed agent is a sub running its initial spawn, owned by
                                    // the main turn: cancel the main turn (the child token
                                    // cascades to the sub).
                                    if let Some(token) = turn_cancels.get(&AgentId::Main) {
                                        token.cancel();
                                    }
                                    yank_pending_into_editor(
                                        &mut shell.tui,
                                        &world.pump,
                                        &world.message_queues,
                                        active,
                                    );
                                    quit_armed = false;
                                    continue;
                                }
                                // Viewed agent idle: anything else
                                // still running — other agents'
                                // turns, background agent runs, or
                                // background bash tasks — arms the
                                // quit guard instead of being
                                // cancelled; a bare exit only when
                                // nothing runs anywhere.
                                let (agents, tasks) = running_work_counts(
                                    turns.len(),
                                    &world.task_registry.snapshot(),
                                );
                                if agents + tasks > 0 {
                                    if quit_armed {
                                        break Ok(SessionExit::Quit);
                                    }
                                    quit_armed = true;
                                    world.pump.handle(
                                        &mut shell.tui,
                                        &notice_event(&quit_arm_notice(agents, tasks)),
                                    );
                                    continue;
                                } else {
                                    // Nothing running anywhere: exit.
                                    break Ok(SessionExit::Quit);
                                }
                            }
                        }
                        if input.is_ctrl('d') {
                            break Ok(SessionExit::Quit);
                        }
                        // Toggle the thinking-block render mode for
                        // the session. Bound via `aj.thinking.toggle`
                        // (default `alt+t`); intercepted before
                        // `shell.tui.handle_input` so the editor never sees
                        // the keystroke. See `docs/aj-next-plan.md` §4.4.
                        {
                            let kb = aj_tui::keybindings::get();
                            if kb.matches(
                                &input,
                                crate::config::keybindings::ACTION_THINKING_TOGGLE,
                            ) {
                                drop(kb);
                                let new_value = !world.pump.hide_thinking_block();
                                world.pump.set_hide_thinking_block(&mut shell.tui, new_value);
                                // Don't post a "hidden/visible"
                                // notice — the transcript above
                                // already shows the new state.
                                continue;
                            }
                        }
                        // Toggle the tool-output render mode for the
                        // session. Bound via `aj.tools.expand`
                        // (default `alt+o`); intercepted before
                        // `shell.tui.handle_input` so the editor never sees
                        // the keystroke.
                        {
                            let kb = aj_tui::keybindings::get();
                            if kb.matches(
                                &input,
                                crate::config::keybindings::ACTION_TOOLS_EXPAND,
                            ) {
                                drop(kb);
                                let new_value = !world.pump.tools_expanded();
                                world.pump.set_tools_expanded(&mut shell.tui, new_value);
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
                        // Because we bypass `shell.tui.handle_input` for
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
                                    && let Some(editor) = shell.tui.get_mut_as::<Editor>(
                                        SlotIndex::Editor.idx(),
                                    )
                                {
                                    editor.insert_text_at_cursor(
                                        &path.display().to_string(),
                                    );
                                }
                                shell.tui.request_render();
                                continue;
                            }
                        }
                        // Submit as a steering message. Bound via
                        // `aj.message.steer` (default `alt+enter`);
                        // intercepted before `shell.tui.handle_input` so
                        // the editor never treats it as a newline. While
                        // the viewed agent is busy this queues the
                        // editor text as steering (escalating a pending
                        // follow-up, or promoting it when the editor is
                        // empty); while idle it starts a normal turn —
                        // there is nothing to steer yet.
                        {
                            let kb = aj_tui::keybindings::get();
                            let matched = kb.matches(
                                &input,
                                crate::config::keybindings::ACTION_SUBMIT_STEERING,
                            );
                            drop(kb);
                            if matched && open_selector.is_none() && login_session.is_none() {
                                let target = world.pump.active_view(&mut shell.tui);
                                let text = shell
                                    .tui
                                    .get_mut_as::<Editor>(SlotIndex::Editor.idx())
                                    .map(|e| e.get_expanded_text().trim().to_string())
                                    .unwrap_or_default();
                                let busy = turn_cancels.contains_key(&target)
                                    || world.pump.is_running(target);
                                if busy {
                                    if text.is_empty() {
                                        world.message_queues.promote(target);
                                    } else {
                                        world.message_queues.append_steering(target, &text);
                                        if let Some(editor) = shell
                                            .tui
                                            .get_mut_as::<Editor>(SlotIndex::Editor.idx())
                                        {
                                            editor.add_to_history(&text);
                                            editor.set_text("");
                                        }
                                    }
                                    world.pump.sync_pending(&mut shell.tui);
                                } else if !text.is_empty() {
                                    if spawn_prompt_turn(
                                        &mut shell.tui,
                                        world,
                                        &shell.run_config,
                                        target,
                                        text,
                                        turn_policy(target, &shell.config),
                                        &mut turns,
                                        &mut turn_cancels,
                                    ) {
                                        sync_editor_enabled(&mut shell.tui);
                                    } else {
                                        world.pump.handle(
                                            &mut shell.tui,
                                            &notice_event("This agent can't be prompted."),
                                        );
                                    }
                                }
                                shell.tui.request_render();
                                continue;
                            }
                        }
                        // Pull the queued message back into the editor.
                        // Bound via `aj.message.dequeue` (default
                        // `alt+up`); yanks regardless of editor contents,
                        // prepending to the current draft.
                        {
                            let kb = aj_tui::keybindings::get();
                            let matched =
                                kb.matches(&input, crate::config::keybindings::ACTION_DEQUEUE);
                            drop(kb);
                            if matched && open_selector.is_none() && login_session.is_none() {
                                let target = world.pump.active_view(&mut shell.tui);
                                yank_pending_into_editor(
                                    &mut shell.tui,
                                    &world.pump,
                                    &world.message_queues,
                                    target,
                                );
                                shell.tui.request_render();
                                continue;
                            }
                        }
                        // Up / Ctrl+P with an empty editor and a pending
                        // message yanks it (same restore as `alt+up`)
                        // rather than navigating history. With a
                        // non-empty editor it falls through to the
                        // editor's normal history-up.
                        {
                            let kb = aj_tui::keybindings::get();
                            let is_up = kb.matches(&input, "tui.editor.cursorUp");
                            drop(kb);
                            if is_up && open_selector.is_none() && login_session.is_none() {
                                let target = world.pump.active_view(&mut shell.tui);
                                let editor_empty = shell
                                    .tui
                                    .get_mut_as::<Editor>(SlotIndex::Editor.idx())
                                    .map(|e| e.get_text().is_empty())
                                    .unwrap_or(false);
                                if editor_empty && world.message_queues.has_pending(target) {
                                    yank_pending_into_editor(
                                        &mut shell.tui,
                                        &world.pump,
                                        &world.message_queues,
                                        target,
                                    );
                                    shell.tui.request_render();
                                    continue;
                                }
                            }
                        }
                        // Global command-palette chord. Bound via
                        // `aj.palette.open` (default `ctrl+o`).
                        // Intercepted before `shell.tui.handle_input` so
                        // no component sees the keystroke. The
                        // actual overlay mount happens after
                        // `shell.tui.handle_input` via the shared
                        // `shell.palette_open_request` flag, so both the
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
                                shell.close_all_request.store(true, Ordering::Relaxed);
                                // Consume: skip `shell.tui.handle_input`
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
                                shell.palette_open_request.store(true, Ordering::Relaxed);
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
                                shell.history_open_request.store(true, Ordering::Relaxed);
                                // Consume: the editor binds no
                                // ctrl+r, but skipping handle_input
                                // keeps the chord from reaching any
                                // future binding and matches the
                                // close-all interception style.
                                consume_event = true;
                            } else if open_selector.is_none()
                                && login_session.is_none()
                                && kb.matches(
                                    &input,
                                    crate::config::keybindings::ACTION_AGENT_PICKER,
                                )
                            {
                                drop(kb);
                                shell.agent_picker_open_request.store(true, Ordering::Relaxed);
                                // Consume: the editor binds no alt+a;
                                // skipping handle_input keeps the
                                // chord from reaching any future
                                // binding, like the history chord.
                                consume_event = true;
                            }
                        }
                        if !consume_event {
                            shell.tui.handle_input(&input);
                        }

                        // Close-all: tear down current selector
                        // and any parent palette in one shot.
                        // Done before the palette-open dispatch
                        // and the per-tick outcome poll so we
                        // never re-enter the back-stack logic
                        // on a unwound state.
                        if shell.close_all_request.swap(false, Ordering::Relaxed) {
                            if let Some(sel) = open_selector.take() {
                                close_all_overlays(&mut shell.tui, sel);
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
                            shell.tui.hide_overlay(&session.handle);
                            if let Some(task) = login_task.take() {
                                task.abort();
                            }
                            let name = session.provider_name.clone();
                            login_session = None;
                            world.pump.handle(
                                &mut shell.tui,
                                &notice_event(&format!("Login to {name} cancelled.")),
                            );
                            continue;
                        }

                        // Global palette open: fired either by the
                        // editor's `/`-at-empty-prompt callback
                        // (handled inside `shell.tui.handle_input` above)
                        // or by the `Ctrl+O` chord intercepted
                        // below. Dispatched here, after routing,
                        // so the editor's `/` swallow has already
                        // landed and so we can `await` the command
                        // handler. Gated on `open_selector` to
                        // be inert while another selector is up.
                        if shell.palette_open_request.swap(false, Ordering::Relaxed)
                            && open_selector.is_none()
                            && login_session.is_none()
                        {
                            match handle_command(
                                &mut shell.tui,
                                &shell.auth,
                                Arc::clone(&shell.model_catalog),
                                Arc::clone(&shell.run_config),
                                &shell.config,
                                &shell.render_settings,
                                world,
                                &shell.conversation_persistence,
                                &shell.theme,
                                CommandAction::OpenCommandPalette,
                                None,
                                !turns.is_empty(),
                            ).await {
                                CommandOutcome::Continue { selector, notice } => {
                                    if let Some(text) = notice {
                                        world.pump.handle(&mut shell.tui, &notice_event(&text));
                                    }
                                    if let Some(sel) = selector {
                                        open_selector = Some(sel);
                                    }
                                }
                                CommandOutcome::SessionChange(request) => {
                                    debug_assert!(turns.is_empty(), "session change requested mid-turn");
                                    break Ok(request.into_exit());
                                }
                                CommandOutcome::Quit => break Ok(SessionExit::Quit),
                            }
                            continue;
                        }

                        // Global prompt-history open: fired by the
                        // `Ctrl+R` chord intercepted above. Runs
                        // [`CommandAction::OpenPromptHistory`] with no
                        // parent palette, so the overlay's `Esc` closes
                        // straight back to the editor. Gated on
                        // `open_selector` to be inert while another
                        // selector is up.
                        if shell.history_open_request.swap(false, Ordering::Relaxed)
                            && open_selector.is_none()
                            && login_session.is_none()
                        {
                            match handle_command(
                                &mut shell.tui,
                                &shell.auth,
                                Arc::clone(&shell.model_catalog),
                                Arc::clone(&shell.run_config),
                                &shell.config,
                                &shell.render_settings,
                                world,
                                &shell.conversation_persistence,
                                &shell.theme,
                                CommandAction::OpenPromptHistory,
                                None,
                                !turns.is_empty(),
                            ).await {
                                CommandOutcome::Continue { selector, notice } => {
                                    if let Some(text) = notice {
                                        world.pump.handle(&mut shell.tui, &notice_event(&text));
                                    }
                                    if let Some(sel) = selector {
                                        open_selector = Some(sel);
                                    }
                                }
                                CommandOutcome::SessionChange(request) => {
                                    debug_assert!(turns.is_empty(), "session change requested mid-turn");
                                    break Ok(request.into_exit());
                                }
                                CommandOutcome::Quit => break Ok(SessionExit::Quit),
                            }
                            continue;
                        }

                        // Global agent-picker open: fired by the
                        // `Alt+A` chord intercepted above. Runs
                        // [`CommandAction::OpenAgentPicker`] with no
                        // parent palette, so the overlay's `Esc` closes
                        // straight back to the editor. Gated on
                        // `open_selector` to be inert while another
                        // selector is up.
                        if shell.agent_picker_open_request.swap(false, Ordering::Relaxed)
                            && open_selector.is_none()
                            && login_session.is_none()
                        {
                            match handle_command(
                                &mut shell.tui,
                                &shell.auth,
                                Arc::clone(&shell.model_catalog),
                                Arc::clone(&shell.run_config),
                                &shell.config,
                                &shell.render_settings,
                                world,
                                &shell.conversation_persistence,
                                &shell.theme,
                                CommandAction::OpenAgentPicker,
                                None,
                                !turns.is_empty(),
                            ).await {
                                CommandOutcome::Continue { selector, notice } => {
                                    if let Some(text) = notice {
                                        world.pump.handle(&mut shell.tui, &notice_event(&text));
                                    }
                                    if let Some(sel) = selector {
                                        open_selector = Some(sel);
                                    }
                                }
                                CommandOutcome::SessionChange(request) => {
                                    debug_assert!(turns.is_empty(), "session change requested mid-turn");
                                    break Ok(request.into_exit());
                                }
                                CommandOutcome::Quit => break Ok(SessionExit::Quit),
                            }
                            continue;
                        }
                        // input was just routed to it. Poll
                        // the overlay's outcome slot; on a
                        // confirm/cancel, close it and apply
                        // the result.
                        if let Some(sel) = open_selector.take() {
                            match handle_selector_outcome(
                                &mut shell.tui,
                                sel,
                                &shell.auth,
                                Arc::clone(&shell.run_config),
                                Arc::clone(&shell.config),
                                &shell.model_catalog,
                                world,
                                &shell.theme,
                                &shell.render_settings,
                                theme_watch,
                            ).await {
                                SelectorPollOutcome::StillOpen(reopened) => {
                                    open_selector = Some(reopened);
                                }
                                SelectorPollOutcome::Closed {
                                    notice,
                                    follow_up,
                                    start_login,
                                    session_request,
                                } => {
                                    if let Some(text) = notice {
                                        world.pump.handle(&mut shell.tui, &notice_event(&text));
                                    }
                                    // A confirmed session pick exits the
                                    // per-session loop; the outer loop
                                    // in `InteractiveMode::run` rebuilds
                                    // onto the chosen session.
                                    if let Some(request) = session_request {
                                        debug_assert!(
                                            turns.is_empty(),
                                            "session change requested mid-turn"
                                        );
                                        break Ok(request.into_exit());
                                    }
                                    // A confirmed login provider
                                    // pick asks the host to launch
                                    // the async browser flow: mount
                                    // the dialog overlay and spawn
                                    // the login task (polled by the
                                    // login `select!` arm).
                                    if let Some(provider_id) = start_login {
                                        match start_login_session(
                                            &mut shell.tui,
                                            &shell.auth,
                                            &shell.theme,
                                            &provider_id,
                                        )
                                        .await
                                        {
                                            Ok((session, task)) => {
                                                login_session = Some(session);
                                                login_task = Some(task);
                                            }
                                            Err(err) => world.pump.handle(
                                                &mut shell.tui,
                                                &warning_event(&format!(
                                                    "Couldn't start login: {err}"
                                                )),
                                            ),
                                        }
                                    }
                                    // The command palette chains into
                                    // the selected command by emitting a
                                    // `follow_up`. Re-feed it through
                                    // `handle_command` so the dispatch
                                    // path is identical to triggering the
                                    // command by its keyboard shortcut.
                                    if let Some(follow_up) = follow_up {
                                        // `/compact` runs as a tracked task
                                        // (like a turn), so the loop — which
                                        // owns `turns` / `turn_cancels` —
                                        // drives it rather than
                                        // `handle_command`, which can't spawn.
                                        if matches!(follow_up.action, CommandAction::Compact) {
                                            if turn_cancels.contains_key(&AgentId::Main)
                                                || world.pump.is_running(AgentId::Main)
                                            {
                                                world.pump.handle(
                                                    &mut shell.tui,
                                                    &notice_event(&session_busy_notice("compact")),
                                                );
                                            } else {
                                                spawn_turn(
                                                    world,
                                                    &shell.run_config,
                                                    AgentId::Main,
                                                    TurnStart::Compact {
                                                        reason:
                                                            aj_agent::events::CompactionReason::Manual,
                                                        instructions: None,
                                                    },
                                                    turn_policy(AgentId::Main, &shell.config),
                                                    &mut turns,
                                                    &mut turn_cancels,
                                                );
                                                sync_editor_enabled(&mut shell.tui);
                                            }
                                        } else {
                                        match handle_command(
                                            &mut shell.tui,
                                            &shell.auth,
                                            Arc::clone(&shell.model_catalog),
                                            Arc::clone(&shell.run_config),
                                            &shell.config,
                                            &shell.render_settings,
                                            world,
                                            &shell.conversation_persistence,
                                            &shell.theme,
                                            follow_up.action,
                                            Some(follow_up.parent_palette),
                                            !turns.is_empty(),
                                        )
                                        .await
                                        {
                                            CommandOutcome::Continue { selector, notice } => {
                                                if let Some(text) = notice {
                                                    world.pump.handle(&mut shell.tui, &notice_event(&text));
                                                }
                                                if let Some(sel) = selector {
                                                    open_selector = Some(sel);
                                                }
                                            }
                                            CommandOutcome::SessionChange(request) => {
                                                debug_assert!(turns.is_empty(), "session change requested mid-turn");
                                                break Ok(request.into_exit());
                                            }
                                            CommandOutcome::Quit => break Ok(SessionExit::Quit),
                                        }
                                        }
                                    }
                                }
                            }
                            sync_editor_enabled(&mut shell.tui);
                            continue;
                        }

                        // The editor swallows printable
                        // input and re-emits a `Submit` when
                        // the user presses Enter. Drain it
                        // and dispatch.
                        if let Some(text) = take_submitted_prompt(&mut shell.tui) {
                            let trimmed = text.trim().to_string();
                            if trimmed.is_empty() {
                                continue;
                            }

                            let target = world.pump.active_view(&mut shell.tui);

                            // Per-agent routing: while the viewed agent
                            // is busy (a binary-driven turn or a nested
                            // initial spawn), a plain-Enter submit is
                            // queued as a follow-up instead of starting
                            // a turn; the agent's wake path delivers it
                            // when the turn ends. The editor already
                            // cleared itself on submit, so we only
                            // record the history entry.
                            if turn_cancels.contains_key(&target) || world.pump.is_running(target) {
                                if let Some(editor) =
                                    shell.tui.get_mut_as::<Editor>(SlotIndex::Editor.idx())
                                {
                                    editor.add_to_history(&trimmed);
                                }
                                world.message_queues.append_follow_up(target, &trimmed);
                                world.pump.sync_pending(&mut shell.tui);
                                continue;
                            }

                            // Idle: start a turn. `spawn_prompt_turn`
                            // clears the editor, records history, mints
                            // the per-turn cancel token (kept in
                            // `turn_cancels` so the Ctrl+C arm can fire
                            // it without locking the agent), and spawns
                            // onto `turns`. A non-promptable target
                            // (resumed sub with no handle) returns false
                            // with the editor left intact.
                            if spawn_prompt_turn(
                                &mut shell.tui,
                                world,
                                &shell.run_config,
                                target,
                                trimmed,
                                turn_policy(target, &shell.config),
                                &mut turns,
                                &mut turn_cancels,
                            ) {
                                sync_editor_enabled(&mut shell.tui);
                            } else {
                                world.pump.handle(
                                    &mut shell.tui,
                                    &notice_event("This agent can't be prompted."),
                                );
                            }
                        }
                    }
                }
            }

            // --- Agent bus event ---
            maybe_evt = recv_event(&mut world.event_rx) => {
                let Some(event) = maybe_evt else { continue };
                world.pump.handle(&mut shell.tui, &event);
                // Wake trigger 1: a background task finished. If its
                // owner is idle, wake it so the completion notice
                // reaches the model; a busy owner picks the notice up
                // at its next drain point instead.
                if let AgentEvent::TaskEnd { agent_id, .. } = &event {
                    spawn_wake_turn(
                        *agent_id,
                        world,
                        &shell.run_config,
                        turn_policy(*agent_id, &shell.config),
                        &mut turns,
                        &mut turn_cancels,
                    );
                }
                // A sub-agent's initial run is nested inside the
                // parent's turn, not driven through the JoinSet, so
                // the turn-completion trigger never sees it end. If a
                // task notice arrived after that run's last drain
                // point it would rot until the next prompt — catch it
                // here on the run's AgentEnd. The pump has already
                // processed the event, so the owner reads as idle and
                // the gate inside spawn_wake_turn is open.
                if let AgentEvent::AgentEnd { agent_id, .. } = &event
                    && (world.task_registry.has_notices(*agent_id)
                        || world.message_queues.has_pending(*agent_id))
                {
                    spawn_wake_turn(
                        *agent_id,
                        world,
                        &shell.run_config,
                        turn_policy(*agent_id, &shell.config),
                        &mut turns,
                        &mut turn_cancels,
                    );
                }
                sync_editor_enabled(&mut shell.tui);
            }

            // --- Theme reload (fs-watcher) ---
            // Coalesced re-parses of `~/.aj/themes/<name>.json`
            // flow through here. The receiver is `None` when no
            // watcher is active (bundled theme name with no
            // override, missing `$HOME`, or the notify backend
            // declined to start); the helper folds that into a
            // pending-forever future so the select arm is
            // harmless in those cases.
            maybe_new_theme = recv_theme(theme_watch.rx.as_mut()) => {
                let Some(new_theme) = maybe_new_theme else { continue };
                let name = new_theme.name().to_string();
                shell.theme.replace(new_theme);
                // `Tui::invalidate` walks the root + every overlay
                // and clears each component's cached render
                // output. The closures still in flight resolve
                // through the shared lock so the next render
                // paints with the new palette automatically.
                shell.tui.invalidate();
                shell.tui.request_render();
                world.pump.handle(
                    &mut shell.tui,
                    &notice_event(&format!("Theme '{name}' reloaded.")),
                );
            }
        }
    };

    // Kill the background-task tree before tearing down turns:
    // cancelling the registry root makes every detached driver
    // SIGKILL its process group promptly; the bounded quiesce makes
    // sure those groups are actually killed and reaped before we
    // proceed. Runs on every exit — quit, fatal error, *and* session
    // switches — so an abandoned world never leaks tasks.
    crate::modes::shutdown_background_tasks(&world.task_registry).await;

    // Wind down in-flight turns before handing control back to the
    // outer session loop. A session change is only requested with no
    // turn in flight, so this only does work on quit and fatal-error
    // exits; `shutdown` aborts every task in the set and awaits them.
    turns.shutdown().await;

    exit
}

/// Pull one event off the agent's bus channel. Wrapped in a tiny
/// helper so the `tokio::select!` arm reads cleanly. Returns `None`
/// if the channel closes (the agent dropped) — the main loop treats
/// that as a transient blip and keeps the TUI alive.
async fn recv_event(rx: &mut UnboundedReceiver<AgentEvent>) -> Option<AgentEvent> {
    rx.recv().await
}

/// Apply the loop-side staged settings to the agent about to run a
/// turn.
///
/// **Main** stamps the full [`RunConfigSnapshot`]: the run config is
/// the main agent's configuration — the selectors stage into it and
/// it persists to `config.toml` — so a main turn picks up any model /
/// thinking change made since the last turn.
///
/// **Sub-agents** own their settings (inherited from the parent at
/// spawn); only the axes the user explicitly staged in
/// `sub_overrides` are applied. Entries are kept (not drained) and
/// re-applied idempotently each turn — an entry is the user's
/// standing choice for that agent. A sub-agent with no entry stamps
/// nothing and runs with whatever it already holds.
fn apply_turn_config(
    target: AgentId,
    agent: &mut Agent,
    run_config: &std::sync::Mutex<RunConfigSnapshot>,
    sub_overrides: &std::sync::Mutex<HashMap<usize, SubAgentOverrides>>,
) {
    match target {
        AgentId::Main => {
            let cfg = run_config.lock().expect("run config mutex poisoned");
            agent.set_provider(
                Arc::clone(&cfg.provider),
                Arc::clone(&cfg.model_info),
                cfg.stream_options.clone(),
            );
            agent.set_default_thinking(cfg.thinking.clone());
            agent.set_speed(cfg.speed);
        }
        AgentId::Sub(n) => {
            let overrides = sub_overrides.lock().expect("sub overrides mutex poisoned");
            let Some(entry) = overrides.get(&n) else {
                return;
            };
            if let Some((provider, model_info, stream_options, _)) = &entry.bundle {
                agent.set_provider(
                    Arc::clone(provider),
                    Arc::clone(model_info),
                    stream_options.clone(),
                );
            }
            if let Some(thinking) = &entry.thinking {
                agent.set_default_thinking(thinking.clone());
            }
            if let Some(speed) = entry.speed {
                agent.set_speed(speed);
            }
        }
    }
}

/// Resolve an `AgentId` to its live handle: the main agent for
/// `Main`, a retained sub-agent for `Sub(n)` (None if no live
/// handle, e.g. a resumed sub-agent).
fn resolve_agent(
    id: AgentId,
    main: &Arc<TokioMutex<Agent>>,
    registry: &SubAgentRegistry,
) -> Option<SharedAgent> {
    match id {
        AgentId::Main => Some(Arc::clone(main)),
        AgentId::Sub(n) => registry.get(n),
    }
}

/// Build the per-agent [`TurnPolicy`]. The Main agent gets reactive
/// overflow recovery and threshold compaction (both gated on
/// `auto_compact`); a sub-agent continuation gets neither, since
/// compaction operates on the log's USER (Main) thread. Queued-work
/// delivery is not a policy knob — the loop wakes idle agents directly.
fn turn_policy(target: AgentId, config: &Arc<std::sync::Mutex<Config>>) -> TurnPolicy {
    let c = config.lock().expect("config mutex poisoned");
    let main = target == AgentId::Main;
    TurnPolicy {
        recover_overflow: main && c.auto_compact,
        auto_threshold: (main && c.auto_compact).then_some(c.compact_threshold),
        keep_recent: c.compact_keep_recent,
    }
}

/// Spawn a turn sequence for `target` onto `turns`: resolve the agent
/// handle, mint the per-sequence cancel token (into `turn_cancels`,
/// which Ctrl+C fires), and drive `start` plus its automatic
/// continuations via [`crate::turn::drive_turn`]. The spawned task
/// re-stamps the staged run config before each inference. Returns
/// `false` without spawning when `target` has no live handle (e.g. a
/// resumed sub-agent).
fn spawn_turn(
    world: &SessionWorld,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    target: AgentId,
    start: TurnStart,
    policy: TurnPolicy,
    turns: &mut JoinSet<(AgentId, Result<(), TurnError>)>,
    turn_cancels: &mut HashMap<AgentId, CancellationToken>,
) -> bool {
    let Some(handle) = resolve_agent(target, &world.agent, &world.registry) else {
        return false;
    };
    let run_config_for_turn = Arc::clone(run_config);
    let sub_overrides_for_turn = Arc::clone(&world.sub_overrides);
    let log = Arc::clone(&world.log);
    let turn_cancel = CancellationToken::new();
    turn_cancels.insert(target, turn_cancel.clone());
    turns.spawn(async move {
        let mut a = handle.lock().await;
        let result = crate::turn::drive_turn(
            &mut a,
            &log,
            &policy,
            start,
            |agent: &mut Agent| {
                apply_turn_config(target, agent, &run_config_for_turn, &sub_overrides_for_turn);
            },
            turn_cancel,
        )
        .await;
        (target, result)
    });
    true
}

/// Spawn a wake turn on `owner` if it is idle, delivering queued
/// notices / messages. This is the single post-turn wake path: the
/// driver itself does not deliver queued work, so the loop starts a
/// wake here whenever an agent has work pending and no turn in flight.
/// A busy owner is left alone (its running turn drains steering
/// mid-flight). Both wake triggers may fire for the same notice;
/// `Agent::wake` returns `Empty` (emitting nothing) once the queue is
/// drained, so the loser is a cheap no-op.
fn spawn_wake_turn(
    owner: AgentId,
    world: &SessionWorld,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    policy: TurnPolicy,
    turns: &mut JoinSet<(AgentId, Result<(), TurnError>)>,
    turn_cancels: &mut HashMap<AgentId, CancellationToken>,
) {
    if turn_cancels.contains_key(&owner) || world.pump.is_running(owner) {
        return;
    }
    spawn_turn(
        world,
        run_config,
        owner,
        TurnStart::Wake,
        policy,
        turns,
        turn_cancels,
    );
}

/// Spawn a user-prompt turn for `target`. Resolves the handle first and
/// leaves the editor intact on a miss (returning `false`) so the caller
/// can surface a notice and the user keeps their text; otherwise clears
/// the editor, records history, and dispatches a [`TurnStart::Prompt`]
/// sequence.
fn spawn_prompt_turn(
    tui: &mut Tui,
    world: &SessionWorld,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    target: AgentId,
    text: String,
    policy: TurnPolicy,
    turns: &mut JoinSet<(AgentId, Result<(), TurnError>)>,
    turn_cancels: &mut HashMap<AgentId, CancellationToken>,
) -> bool {
    if resolve_agent(target, &world.agent, &world.registry).is_none() {
        return false;
    }
    if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
        editor.set_text("");
        editor.add_to_history(&text);
    }
    spawn_turn(
        world,
        run_config,
        target,
        TurnStart::Prompt(text),
        policy,
        turns,
        turn_cancels,
    )
}

/// prepending it to whatever is currently typed (blank-line joined),
/// and repaint the pending box. Returns whether anything was yanked.
/// Used by the dequeue chord, the empty-editor Up/Ctrl+P yank, and the
/// per-view cancel restore.
fn yank_pending_into_editor(
    tui: &mut Tui,
    pump: &EventPump,
    queues: &MessageQueues,
    target: AgentId,
) -> bool {
    let Some(text) = queues.take_pending(target) else {
        return false;
    };
    if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
        let current = editor.get_text();
        let combined = if current.trim().is_empty() {
            text
        } else {
            format!("{text}\n\n{current}")
        };
        editor.set_text(&combined);
    }
    pump.sync_pending(tui);
    true
}

/// Keep the editor's submit enabled.
///
/// A submit while the viewed agent is busy is routed to the message
/// queue by the submit handler rather than refused, so the editor is
/// never gated on busy state. Retained as the single choke point in
/// case a future state needs to hard-disable submit.
fn sync_editor_enabled(tui: &mut Tui) {
    set_editor_submit_enabled(tui, true);
}

/// Await the next completed turn, or pend forever when no turn is
/// in flight (mirrors the old `task_done` future so the select arm
/// stays simple).
async fn join_next_or_pending(
    turns: &mut JoinSet<(AgentId, Result<(), TurnError>)>,
) -> Result<(AgentId, Result<(), TurnError>), tokio::task::JoinError> {
    if turns.is_empty() {
        std::future::pending().await
    } else {
        turns
            .join_next()
            .await
            .expect("non-empty JoinSet yields Some")
    }
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
        /// Agent the confirm applies to, captured from the active
        /// view at open time so a view switch while the overlay is
        /// up doesn't redirect the change.
        target: AgentId,
        /// When opened via the command palette, the (hidden)
        /// palette's handle + outcome slot. On cancel we un-hide
        /// it and restore host-side polling so the palette stays
        /// usable; on confirm we tear it down. `None` when the
        /// selector was reached directly (not via the palette).
        parent_palette: Option<ParentPalette>,
    },
    Model {
        handle: OverlayHandle,
        outcome: ModelOutcomeHandle,
        /// Agent the confirm applies to, captured at open time
        /// like [`OpenSelector::Thinking::target`].
        target: AgentId,
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
    /// Agent picker overlay. `Enter` switches the chat view to the
    /// chosen agent's transcript (and sets the editor's observing
    /// marker); `Esc` (or back-stack pop) closes it.
    AgentPicker {
        handle: OverlayHandle,
        outcome: AgentPickerOutcomeHandle,
        parent_palette: Option<ParentPalette>,
    },
    /// Read-only viewer for a background bash task's output, opened by
    /// confirming a task row in the agent picker. Both Esc and Enter
    /// close straight to chat; it carries no parent back-stack.
    TaskOutput {
        handle: OverlayHandle,
        outcome: TaskOutputOutcomeHandle,
    },
    Palette {
        handle: OverlayHandle,
        outcome: CommandPaletteOutcomeHandle,
    },
    /// Read-only help overlay. Both Esc and Enter close it; if
    /// opened via the palette, the parent palette is restored on
    /// close so the back-stack stays coherent.
    Help {
        handle: OverlayHandle,
        outcome: crate::modes::interactive::components::help_overlay::HelpOverlayOutcomeHandle,
        parent_palette: Option<ParentPalette>,
    },
    /// Provider picker for login / logout. `mode` decides what
    /// confirming a provider does: start the OAuth browser flow, or
    /// remove the stored credential.
    AuthPicker {
        handle: OverlayHandle,
        outcome: AuthPickerOutcomeHandle,
        parent_palette: Option<ParentPalette>,
        mode: AuthPickerMode,
    },
    /// Read-only auth-status overlay. Both Esc and Enter close it.
    AuthStatus {
        handle: OverlayHandle,
        outcome: AuthStatusOutcomeHandle,
        parent_palette: Option<ParentPalette>,
    },
    /// Read-only usage overlay. Both Esc and Enter close it. The
    /// usage reports stream in from a background fetch after the
    /// overlay opens; closing early just drops the fetch's receiver.
    UsageStatus {
        handle: OverlayHandle,
        outcome: UsageStatusOutcomeHandle,
        parent_palette: Option<ParentPalette>,
    },
    /// Settings window. Stays open across changes: the
    /// host drains `changes` after every input event, applying and
    /// persisting each entry (and pushing a display fix through
    /// `corrections` when an apply fails); `outcome` only ever
    /// reports the close.
    Settings {
        handle: OverlayHandle,
        outcome: SettingsOutcomeHandle,
        changes: SettingsChangesHandle,
        corrections: SettingsCorrectionsHandle,
        parent_palette: Option<ParentPalette>,
    },
    /// Skills window. Stays open across changes: the host
    /// drains `changes` after every input event, persisting each
    /// enable/disable toggle into the `disabled_skills` config option;
    /// `outcome` only ever reports the close.
    Skills {
        handle: OverlayHandle,
        outcome: SkillsOutcomeHandle,
        changes: SkillsChangesHandle,
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
enum CommandOutcome {
    /// Stay in the session loop. Optionally present a transient
    /// notice to the chat scrollback and/or open a selector overlay.
    Continue {
        selector: Option<OpenSelector>,
        notice: Option<String>,
    },
    /// A new-session change for the outer session loop to
    /// perform; the per-session loop exits with the matching
    /// [`SessionExit`]. Only emitted when no turn is in flight.
    SessionChange(SessionRequest),
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
    /// is the command the main loop should run via [`handle_command`]
    /// — the command palette uses this to chain into a sub-selector
    /// (e.g. the user picked the model command from the palette, so
    /// the main loop now opens the model selector for real). The
    /// follow-up path keeps the recursion at the main-loop level
    /// rather than calling `handle_command` from inside
    /// [`handle_selector_outcome`] — which would require threading
    /// the model catalog and other command-only dependencies
    /// through the outcome handler.
    Closed {
        notice: Option<String>,
        follow_up: Option<PaletteFollowUp>,
        /// When set, a login provider pick the host should turn
        /// into a launched OAuth flow (the picker can't spawn the
        /// task itself — that's the main loop's job, where the login
        /// session state and the task `select!` arm live).
        start_login: Option<String>,
        /// When set, a confirmed session pick the outer session loop
        /// should perform by rebuilding the world. Only emitted when
        /// no turn is in flight.
        session_request: Option<SessionRequest>,
    },
}

/// Deferred chain from the command palette to the command it
/// selected. Carries the palette's (hidden) overlay handle + outcome
/// slot so the main loop can stash both on the sub-selector for the
/// back-stack (and so the palette stays pollable after a cancel
/// returns to it), or tear it down if the follow-up doesn't open a
/// sub-selector.
struct PaletteFollowUp {
    action: CommandAction,
    parent_palette: ParentPalette,
}

/// Wrap a notice string in the [`AgentEvent::Notice`] shape so we
/// can route it through the existing event pump for rendering. The
/// pump's `Notice` arm appends a dim text row to the chat slot,
/// which is exactly the look we want for command status
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

/// The live theme file watcher: the notify guard plus the receiver
/// the main loop's reload arm polls. Bundled to a single owner so a
/// runtime theme switch (the settings window) can re-point the
/// watcher at the new theme's file by reinstalling the pair in
/// place.
struct ThemeWatch {
    /// Keeps the notify watcher alive; dropping it tears the
    /// watcher down. Never read — held purely for its `Drop`.
    _guard: Option<ThemeWatcherGuard>,
    rx: Option<UnboundedReceiver<Theme>>,
}

impl ThemeWatch {
    /// Install a watcher for `name`. Only user-supplied themes get
    /// one; bundled `dark` / `light` palettes live inside the binary
    /// and have no on-disk source to edit. `watch_user_theme`
    /// short-circuits on missing file / unset `$HOME` and silently
    /// degrades to "no hot-reload" when the notify backend can't
    /// start — both fields are then `None` and the reload arm is
    /// inert.
    fn install(name: &str) -> Self {
        match watch_user_theme(name) {
            Some((guard, rx)) => Self {
                _guard: Some(guard),
                rx: Some(rx),
            },
            None => Self {
                _guard: None,
                rx: None,
            },
        }
    }
}

/// Build the chat-scrollback "Context:" notice listing everything
/// stitched into the agent's system prompt: the base prompt (builtin
/// or override file), every agents.md-style instruction file, and
/// every discovered skill, one row each formatted as
/// `  - <tildified path> (<label>)` so the user can verify which
/// guidance is actually active. Skill rows carry the skill name and a
/// marker when the skill is excluded from the model's listing — either
/// `disabled` (the user's `disabled_skills` config) or
/// `model-invocation disabled` (the skill's own frontmatter). Disabled
/// rows are additionally struck through so they read as inactive at a
/// glance.
fn build_context_notice(env: &AgentEnv) -> String {
    let mut lines = String::from("Context:");
    let source = &env.system_prompt.source;
    match source {
        SystemPromptSource::Builtin => {
            lines.push_str(&format!(
                "\n  - builtin ({}; override with ~/.agents/SYSTEM_PROMPT.md)",
                source.label()
            ));
        }
        SystemPromptSource::Override(path) => {
            lines.push_str(&format!(
                "\n  - {} ({})",
                display_path(path),
                source.label()
            ));
        }
    }
    for file in &env.context_files {
        lines.push_str(&format!(
            "\n  - {} ({})",
            display_path(&file.path),
            file.kind.label()
        ));
    }
    for skill in &env.skills {
        let marker = if !skill.enabled {
            ", disabled"
        } else if skill.disable_model_invocation {
            ", model-invocation disabled"
        } else {
            ""
        };
        let row = format!(
            "{} (skill: {}{marker})",
            display_path(&skill.path),
            skill.name,
        );
        // A disabled skill is excluded from the model's listing
        // entirely, so we strike its row through to set it apart at a
        // glance. The notice renderer's dim wrap and this strikethrough
        // are independent SGR attributes, so they nest cleanly; the
        // `, disabled` marker stays legible through the line.
        let row = if skill.enabled {
            row
        } else {
            aj_tui::style::strikethrough(&row)
        };
        lines.push_str(&format!("\n  - {row}"));
    }
    lines
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

/// Resolve the thinking effort the editor border should display
/// for the agent under view: the agent's footer-settings thinking
/// string when an entry exists and parses, else the run config's
/// session default. The fallback covers agents with no footer entry
/// and replayed legacy entries whose thinking string is empty.
fn resolve_view_thinking(
    settings: Option<&aj_agent::events::AgentSettings>,
    fallback: &Option<ThinkingConfig>,
) -> Option<ThinkingConfig> {
    settings
        .and_then(|s| thinking_config_from_name(&s.thinking))
        .unwrap_or_else(|| fallback.clone())
}

/// Re-tint the editor border for the agent the chat view observes:
/// resolve the view's thinking via [`resolve_view_thinking`] and
/// push it through [`apply_editor_border_for_thinking`]. Called on
/// view switches and after a session install (where the view is
/// Main).
fn apply_editor_border_for_view(
    tui: &mut Tui,
    theme: &ThemeHandle,
    pump: &crate::modes::interactive::event_pump::EventPump,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    id: AgentId,
) {
    let fallback = {
        let cfg = run_config.lock().expect("run config mutex poisoned");
        cfg.thinking.clone()
    };
    let level = resolve_view_thinking(pump.agent_settings(id), &fallback);
    apply_editor_border_for_thinking(tui, theme, level.as_ref());
}

/// Reflect the observed agent in the editor's top-bar label: an
/// `agent N` marker for a sub-agent, cleared for the main agent.
/// Called when the agent picker confirms a switch and when a session
/// reset returns the view to the main agent.
fn apply_editor_agent_marker(tui: &mut Tui, id: AgentId) {
    let label = match id {
        AgentId::Main => None,
        AgentId::Sub(n) => Some(format!("agent {n}")),
    };
    if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
        editor.set_top_bar_label(label);
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
/// The write goes through [`Config::persist_changed`], which merges
/// only the options this change touched onto the latest on-disk file
/// under a cross-process lock — so a second `aj` editing a different
/// key isn't clobbered. The in-memory mutation is applied first and
/// the guard dropped before the write, so the live change takes effect
/// regardless and the (best-effort) persistence never holds the config
/// mutex across file I/O.
///
/// A save failure returns a user-facing notice (to append to the
/// action's confirmation) rather than an error. Returns `None` on
/// success.
fn persist_config(
    config: &Arc<std::sync::Mutex<Config>>,
    mutate: impl FnOnce(&mut Config),
) -> Option<String> {
    let (baseline, updated) = {
        let mut cfg = config.lock().expect("config mutex poisoned");
        let baseline = cfg.clone();
        mutate(&mut cfg);
        (baseline, cfg.clone())
    };
    match updated.persist_changed(&baseline) {
        Ok(()) => None,
        Err(err) => Some(format!("(couldn't save to config.toml: {err})")),
    }
}

/// Inner-content row count for the compact overlays (palette, help,
/// model / thinking pickers). Total rendered height including chrome
/// is `PALETTE_OVERLAY_INNER_ROWS + 4` (23 rows), which still fits
/// comfortably on a standard 24-row terminal. Sized so the command
/// palette shows its whole catalog without scrolling: the palette
/// reserves three of these rows for its search box, separator, and
/// scroll indicator (see `CommandPaletteComponent::set_available_height`),
/// leaving enough list rows for every builtin command. The
/// content-heavy overlays (session switcher, prompt history) size their
/// rows dynamically instead — see [`large_overlay_inner_rows`].
const PALETTE_OVERLAY_INNER_ROWS: usize = 19;

/// Sizing/anchor used by the command palette and the compact pickers
/// (model / thinking / help). Centered, fills ~75% of the terminal
/// width with a 72-col floor and a 100-col ceiling so the box doesn't
/// stretch uncomfortably wide on large monitors. The ceiling is sized
/// for the widest read-only page (usage: provider prefix + window
/// label + a "resets ... (Europe/Berlin)" description) to fit without
/// truncation. Height is fixed at `PALETTE_OVERLAY_INNER_ROWS + 4` to
/// match the stable height the
/// [`aj_tui::components::overlay_window::OverlayWindow`] renders;
/// pinning the compositor's height to the exact value keeps narrow
/// terminals from reserving extra rows.
fn palette_overlay_options() -> OverlayOptions {
    OverlayOptions {
        anchor: OverlayAnchor::Center,
        width: Some(SizeValue::Percent(75.0)),
        min_width: Some(72),
        max_width: Some(100),
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

/// Per-frame subtitle for the agent picker, resolved by the overlay
/// via `with_dynamic_subtitle`: the scope-toggle hint names the scope
/// the chord would switch *to*, so it flips along with the list, and
/// the task-kill hint only appears when the picker has a running
/// task row the chord could act on.
fn subtitle_agent_picker(child: &dyn aj_tui::component::Component) -> String {
    let picker = child.as_any().downcast_ref::<AgentPickerComponent>();
    let showing_all = picker.is_some_and(AgentPickerComponent::showing_all);
    let has_tasks = picker.is_some_and(AgentPickerComponent::has_killable_tasks);
    let confirm = aj_tui::keybindings::format_action_shortcut("tui.input.submit")
        .unwrap_or_else(|| "Enter".to_string());
    let cancel = aj_tui::keybindings::format_action_shortcut("tui.select.cancel")
        .unwrap_or_else(|| "Esc".to_string());
    let scope = aj_tui::keybindings::format_action_shortcut(
        crate::config::keybindings::ACTION_AGENT_TOGGLE_SCOPE,
    )
    .unwrap_or_else(|| "Ctrl+T".to_string());
    let scope_target = if showing_all {
        "running agents"
    } else {
        "all agents"
    };
    let kill_hint = if has_tasks {
        let kill = aj_tui::keybindings::format_action_shortcut(
            crate::config::keybindings::ACTION_TASK_KILL,
        )
        .unwrap_or_else(|| "Ctrl+K".to_string());
        format!("{kill} kill task  \u{2022}  ")
    } else {
        String::new()
    };
    format!(
        "{confirm} to observe  \u{2022}  {scope} {scope_target}  \u{2022}  {kill_hint}{cancel} to close"
    )
}

/// Per-frame subtitle for the prompt-history overlay, resolved by the
/// overlay via `with_dynamic_subtitle`: the scope-toggle hint names
/// the scope the chord would switch *to*, so it flips along with the
/// list.
fn subtitle_prompt_history(child: &dyn aj_tui::component::Component) -> String {
    let showing_all = child
        .as_any()
        .downcast_ref::<PromptHistorySearchComponent>()
        .is_some_and(PromptHistorySearchComponent::showing_all_workspaces);
    let confirm = aj_tui::keybindings::format_action_shortcut("tui.input.submit")
        .unwrap_or_else(|| "Enter".to_string());
    let cancel = aj_tui::keybindings::format_action_shortcut("tui.select.cancel")
        .unwrap_or_else(|| "Esc".to_string());
    let scope = aj_tui::keybindings::format_action_shortcut(
        crate::config::keybindings::ACTION_HISTORY_TOGGLE_SCOPE,
    )
    .unwrap_or_else(|| "Ctrl+T".to_string());
    let scope_target = if showing_all {
        "this workspace"
    } else {
        "all workspaces"
    };
    format!("{confirm} to recall  \u{2022}  {scope} {scope_target}  \u{2022}  {cancel} to close")
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

/// Subtitle for the task-output viewer: scroll, kill, and close hints,
/// with key labels resolved from the keybindings manager.
fn subtitle_task_output() -> String {
    let up = aj_tui::keybindings::format_action_shortcut("tui.select.up")
        .unwrap_or_else(|| "Up".to_string());
    let down = aj_tui::keybindings::format_action_shortcut("tui.select.down")
        .unwrap_or_else(|| "Down".to_string());
    let kill =
        aj_tui::keybindings::format_action_shortcut(crate::config::keybindings::ACTION_TASK_KILL)
            .unwrap_or_else(|| "Ctrl+K".to_string());
    let cancel = aj_tui::keybindings::format_action_shortcut("tui.select.cancel")
        .unwrap_or_else(|| "Esc".to_string());
    format!("{up}/{down} scroll  \u{2022}  {kill} kill  \u{2022}  {cancel} to close")
}

/// Subtitle for stay-open editing overlays (settings, skills): how to
/// change/toggle the highlighted value and how to close. `verb` is the
/// per-window activation word (`"change"`, `"toggle"`). Space is a
/// hardcoded activation alias in the settings list, so it's surfaced
/// alongside the resolved confirm key.
fn subtitle_change_close(verb: &str) -> String {
    let confirm = aj_tui::keybindings::format_action_shortcut("tui.select.confirm")
        .unwrap_or_else(|| "Enter".to_string());
    let cancel = aj_tui::keybindings::format_action_shortcut("tui.select.cancel")
        .unwrap_or_else(|| "Esc".to_string());
    let close_all = aj_tui::keybindings::format_action_shortcut(
        crate::config::keybindings::ACTION_OVERLAY_CLOSE_ALL,
    );
    match close_all {
        Some(k) if k != cancel => {
            format!("{confirm}/Space to {verb}  \u{2022}  {cancel} back  \u{2022}  {k} close")
        }
        _ => format!("{confirm}/Space to {verb}  \u{2022}  {cancel} to close"),
    }
}

/// Per-frame subtitle for the settings window, resolved by the overlay
/// via [`OverlayWindow::with_dynamic_subtitle`]: while a submenu is
/// open the keys mean different things than on the main list, so the
/// hint follows the active submenu kind.
///
/// [`OverlayWindow::with_dynamic_subtitle`]:
///     aj_tui::components::overlay_window::OverlayWindow::with_dynamic_subtitle
fn subtitle_settings_window(child: &dyn aj_tui::component::Component) -> String {
    let submenu = child
        .as_any()
        .downcast_ref::<SettingsWindowComponent>()
        .map(SettingsWindowComponent::active_submenu)
        .unwrap_or(SettingsSubmenu::None);
    let confirm = aj_tui::keybindings::format_action_shortcut("tui.select.confirm")
        .unwrap_or_else(|| "Enter".to_string());
    let submit = aj_tui::keybindings::format_action_shortcut("tui.input.submit")
        .unwrap_or_else(|| "Enter".to_string());
    let cancel = aj_tui::keybindings::format_action_shortcut("tui.select.cancel")
        .unwrap_or_else(|| "Esc".to_string());
    match submenu {
        SettingsSubmenu::None => subtitle_change_close("change"),
        SettingsSubmenu::Picker => format!("{confirm} to confirm  \u{2022}  {cancel} back"),
        SettingsSubmenu::TextEdit => format!("{submit} to apply  \u{2022}  {cancel} back"),
        SettingsSubmenu::Toggles => {
            format!("{confirm}/Space to toggle  \u{2022}  {cancel} back")
        }
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
        OpenSelector::TaskOutput { handle, .. } => {
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
        }
        | OpenSelector::UsageStatus {
            handle,
            parent_palette,
            ..
        }
        | OpenSelector::AgentPicker {
            handle,
            parent_palette,
            ..
        }
        | OpenSelector::Settings {
            handle,
            parent_palette,
            ..
        }
        | OpenSelector::Skills {
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
    let copy = crate::config::keybindings::fixed_keys::CTRL_Y;
    format!(
        "{copy} to copy URL  \u{2022}  {submit} to submit pasted code  \u{2022}  {cancel} to cancel"
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

/// Apply a [`CommandAction`] chosen from the palette, a keyboard
/// shortcut, or a palette follow-up.
#[allow(clippy::too_many_arguments)]
async fn handle_command(
    tui: &mut Tui,
    auth: &AuthStorage,
    model_catalog: Arc<Vec<aj_models::registry::ModelInfo>>,
    run_config: Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: &Arc<std::sync::Mutex<Config>>,
    render_settings: &RenderSettings,
    world: &SessionWorld,
    conversation_persistence: &ConversationPersistence,
    theme: &ThemeHandle,
    action: CommandAction,
    parent_palette: Option<ParentPalette>,
    turn_running: bool,
) -> CommandOutcome {
    let result = match action {
        CommandAction::OpenCommandPalette => {
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
            CommandOutcome::Continue {
                selector: Some(OpenSelector::Palette { handle, outcome }),
                notice: None,
            }
        }
        CommandAction::OpenThinkingSelector => {
            // The selector targets the agent the user is viewing;
            // pre-select its tracked thinking level, falling back to
            // the run config for ids with no footer entry (which
            // shouldn't happen, but degrade gracefully).
            let target = world.pump.active_view(tui);
            let current = world
                .pump
                .agent_settings(target)
                .and_then(|s| thinking_config_from_name(&s.thinking))
                .unwrap_or_else(|| {
                    run_config
                        .lock()
                        .expect("run config mutex poisoned")
                        .thinking
                        .clone()
                });
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
            CommandOutcome::Continue {
                selector: Some(OpenSelector::Thinking {
                    handle,
                    outcome,
                    target,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        CommandAction::OpenModelSelector => {
            // The selector targets the agent the user is viewing;
            // pre-select its tracked (provider, id) pair, falling
            // back to the run config for ids with no footer entry.
            // Never touches the agent, so it's safe mid-turn.
            let target = world.pump.active_view(tui);
            let (provider, id) = world
                .pump
                .agent_settings(target)
                .map(|s| (s.provider.clone(), s.model_id.clone()))
                .unwrap_or_else(|| {
                    let cfg = run_config.lock().expect("run config mutex poisoned");
                    cfg.model_key.clone()
                });
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
            CommandOutcome::Continue {
                selector: Some(OpenSelector::Model {
                    handle,
                    outcome,
                    target,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        CommandAction::OpenLoginSelector => {
            let providers = auth.oauth_provider_ids().await;
            if providers.is_empty() {
                CommandOutcome::Continue {
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
                CommandOutcome::Continue {
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
        CommandAction::OpenLogoutSelector => {
            let mut stored = auth.list().await.unwrap_or_default();
            if stored.is_empty() {
                CommandOutcome::Continue {
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
                CommandOutcome::Continue {
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
        CommandAction::OpenAuthStatus => {
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
            CommandOutcome::Continue {
                selector: Some(OpenSelector::AuthStatus {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        CommandAction::OpenUsageStatus => {
            // The fetch hits the network, so it runs detached: the
            // overlay opens immediately in its loading state and the
            // task pokes the render loop when the reports land. If
            // the user closes the overlay first, the send fails and
            // the result is simply dropped.
            let (tx, rx) = tokio::sync::oneshot::channel();
            let fetch_auth = auth.clone();
            let render = tui.handle();
            tokio::spawn(async move {
                let statuses = crate::usage::collect_usage(&fetch_auth).await;
                if tx.send(statuses).is_ok() {
                    render.request_render();
                }
            });

            let inner = UsageStatusComponent::new(select_list_theme(theme), rx);
            let outcome = inner.outcome_handle();
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Usage",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                PALETTE_OVERLAY_INNER_ROWS,
            )
            .with_subtitle(&subtitle_close());
            let handle = tui.show_overlay(Box::new(window), palette_overlay_options());
            CommandOutcome::Continue {
                selector: Some(OpenSelector::UsageStatus {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        // Session-changing commands tear down the current world and
        // rebuild it, which must never abort in-flight work, so
        // refuse them mid-turn. The user can cancel the turn and
        // retry.
        CommandAction::OpenSessionSelector if turn_running => CommandOutcome::Continue {
            selector: None,
            notice: Some(session_busy_notice("switch sessions")),
        },
        CommandAction::OpenSessionSelector => {
            // The current session id lets the overlay pre-select the
            // active row once it streams in.
            let current_session_id = world.session_id.clone();

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
            CommandOutcome::Continue {
                selector: Some(OpenSelector::Session {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        CommandAction::OpenPromptHistory => {
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
            .with_dynamic_subtitle(subtitle_prompt_history);
            let handle = tui.show_overlay(Box::new(window), large_overlay_options());
            CommandOutcome::Continue {
                selector: Some(OpenSelector::PromptHistory {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        CommandAction::OpenAgentPicker => {
            // Snapshot the known agents and the active view from the
            // pump (reads through the `ChatView`); never touches the
            // agent, so it's safe mid-turn. Tasks come from the
            // pump's transient task tracking (registry-independent).
            let agents = world.pump.agents(tui);
            let tasks = world.pump.tasks();
            let active = world.pump.active_view(tui);
            let inner = AgentPickerComponent::new(select_list_theme(theme), agents, tasks, active);
            let outcome = inner.outcome_handle();
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Agents",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                PALETTE_OVERLAY_INNER_ROWS,
            )
            .with_dynamic_subtitle(subtitle_agent_picker);
            let handle = tui.show_overlay(Box::new(window), palette_overlay_options());
            CommandOutcome::Continue {
                selector: Some(OpenSelector::AgentPicker {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        CommandAction::NewSession if turn_running => CommandOutcome::Continue {
            selector: None,
            notice: Some(session_busy_notice("start a new session")),
        },
        CommandAction::NewSession => CommandOutcome::SessionChange(SessionRequest::New),
        // The interactive loop intercepts `Compact` before reaching
        // `handle_command` (it needs the turn machinery this function
        // doesn't have), so this arm only exists for exhaustiveness.
        CommandAction::Compact => CommandOutcome::Continue {
            selector: None,
            notice: None,
        },
        CommandAction::OpenSettings => {
            // Snapshot the live values the window opens with. Model /
            // thinking / speed come from the run config (the loop-side
            // truth for the next turn); the render toggles from the
            // shared handle; the rest from the persisted config.
            let current = {
                let run_cfg = run_config.lock().expect("run config mutex poisoned");
                let cfg = config.lock().expect("config mutex poisoned");
                SettingsCurrentValues {
                    model_key: run_cfg.model_key.clone(),
                    model_url: cfg.model_url.clone(),
                    thinking: thinking_level_name(&run_cfg.thinking).to_string(),
                    thinking_display: cfg.thinking_display.map(|d| d.to_string()),
                    speed: speed_name(run_cfg.speed).to_string(),
                    theme: resolve_theme_name(cfg.theme.as_deref()).to_string(),
                    disabled_tools: cfg.disabled_tools.clone(),
                    disabled_skills: cfg.disabled_skills.clone(),
                    hide_thinking_block: render_settings.hide_thinking_block(),
                    image_auto_resize: cfg.image_auto_resize,
                    image_show_in_terminal: render_settings.show_image_in_terminal(),
                    image_block: cfg.image_block,
                    syntax_highlighting: cfg.syntax_highlighting,
                    auto_compact: cfg.auto_compact,
                    compact_threshold: cfg.compact_threshold.to_string(),
                    compact_keep_recent: cfg.compact_keep_recent.to_string(),
                }
            };
            // Builtin tool names for the disabled-tools toggle list.
            // Constructing the tools just for their names is mildly
            // wasteful but matches what a session build does, and
            // keeps the list sourced from the actual registry.
            let tool_names: Vec<String> = get_builtin_tools(&BuiltinToolOptions::default())
                .into_iter()
                .map(|tool| tool.name)
                .collect();
            // Skill names for the disabled-skills toggle list, from a
            // fresh discovery scan so newly added skills are togglable
            // without restarting.
            let skill_names: Vec<String> = aj_conf::skills::discover_skills(&[])
                .0
                .into_iter()
                .map(|skill| skill.name)
                .collect();
            let inner = SettingsWindowComponent::new(
                settings_list_theme(theme),
                select_list_theme(theme),
                (*model_catalog).clone(),
                Theme::available(),
                tool_names,
                skill_names,
                current,
            );
            let outcome = inner.outcome_handle();
            let changes = inner.changes_handle();
            let corrections = inner.corrections_handle();
            let initial_inner_rows = large_overlay_inner_rows(usize::from(tui.terminal().rows()));
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Settings",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                initial_inner_rows,
            )
            .with_dynamic_height(tui.handle(), large_overlay_inner_rows)
            .with_dynamic_subtitle(subtitle_settings_window);
            let handle = tui.show_overlay(Box::new(window), large_overlay_options());
            CommandOutcome::Continue {
                selector: Some(OpenSelector::Settings {
                    handle,
                    outcome,
                    changes,
                    corrections,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        CommandAction::OpenSkills => {
            // Rediscover skills at open time so the window reflects the
            // on-disk state (and the current `disabled_skills` value)
            // rather than the session-frozen env snapshot. Discovery is
            // a small directory scan, cheap enough to redo per open.
            let (skills, _diagnostics) = {
                let cfg = config.lock().expect("config mutex poisoned");
                aj_conf::skills::discover_skills(&cfg.disabled_skills)
            };
            if skills.is_empty() {
                CommandOutcome::Continue {
                    selector: None,
                    notice: Some(
                        "No skills found. Put skills in ~/.agents/skills/ or \
                         .agents/skills/ (also: .aj/, .claude/)."
                            .to_string(),
                    ),
                }
            } else {
                let rows: Vec<SkillRow> = skills
                    .into_iter()
                    .map(|s| SkillRow {
                        name: s.name,
                        description: s.description,
                        path: display_path(&s.path),
                        enabled: s.enabled,
                        disable_model_invocation: s.disable_model_invocation,
                    })
                    .collect();
                let inner = SkillsWindowComponent::new(settings_list_theme(theme), rows);
                let outcome = inner.outcome_handle();
                let changes = inner.changes_handle();
                let initial_inner_rows =
                    large_overlay_inner_rows(usize::from(tui.terminal().rows()));
                let window = aj_tui::components::overlay_window::OverlayWindow::new(
                    "Skills",
                    Box::new(inner),
                    crate::config::theme::overlay_window_theme(theme),
                    initial_inner_rows,
                )
                .with_dynamic_height(tui.handle(), large_overlay_inner_rows)
                .with_subtitle(subtitle_change_close("toggle"));
                let handle = tui.show_overlay(Box::new(window), large_overlay_options());
                CommandOutcome::Continue {
                    selector: Some(OpenSelector::Skills {
                        handle,
                        outcome,
                        changes,
                        parent_palette: parent_palette.clone(),
                    }),
                    notice: None,
                }
            }
        }
        CommandAction::Help => {
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
            CommandOutcome::Continue {
                selector: Some(OpenSelector::Help {
                    handle,
                    outcome,
                    parent_palette: parent_palette.clone(),
                }),
                notice: None,
            }
        }
        CommandAction::NotYetImplemented { message, .. } => CommandOutcome::Continue {
            selector: None,
            notice: Some(message.to_string()),
        },
        CommandAction::Quit => CommandOutcome::Quit,
    };

    // If we were dispatched as a palette follow-up, the palette
    // is hidden on the overlay stack. The arms that mount a
    // sub-selector consume `parent_palette` (passing it onto the
    // new `OpenSelector`), in which case the resulting
    // `CommandOutcome::Continue` carries one of those variants.
    // For every other arm — `NewSession`, `Quit`, errors, etc. —
    // the palette would otherwise leak, so tear it down here.
    if let Some(palette) = parent_palette {
        let consumed = matches!(
            &result,
            CommandOutcome::Continue {
                selector: Some(
                    OpenSelector::Thinking { .. }
                        | OpenSelector::Model { .. }
                        | OpenSelector::Session { .. }
                        | OpenSelector::PromptHistory { .. }
                        | OpenSelector::AgentPicker { .. }
                        | OpenSelector::Help { .. }
                        | OpenSelector::AuthPicker { .. }
                        | OpenSelector::AuthStatus { .. }
                        | OpenSelector::UsageStatus { .. }
                        | OpenSelector::Settings { .. }
                        | OpenSelector::Skills { .. },
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

/// Apply a confirmed thinking pick to the main agent: stage into
/// the run config, persist to `config.toml`, record on the session
/// log's user thread, and refresh footer + border. Returns the
/// user-facing notice.
async fn confirm_thinking_for_main(
    tui: &mut Tui,
    level: Option<ThinkingConfig>,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: &Arc<std::sync::Mutex<Config>>,
    world: &mut SessionWorld,
    theme: &ThemeHandle,
) -> String {
    // Stage the new thinking effort into the loop-side snapshot; the
    // next turn applies it. Never locks the agent, so it's safe while
    // a turn is running (the in-flight turn keeps its effort; the
    // change takes effect next turn). Read the rest of the settings
    // identity back for the footer entry.
    let (settings, window) = {
        let mut cfg = run_config.lock().expect("run config mutex poisoned");
        cfg.thinking = level.clone();
        (
            aj_agent::events::AgentSettings {
                provider: cfg.model_key.0.clone(),
                model_id: cfg.model_key.1.clone(),
                thinking: thinking_level_name(&level).to_string(),
                speed: speed_name(cfg.speed).to_string(),
            },
            cfg.model_info.context_window,
        )
    };
    // Mirror the change onto the editor's border tint so the visual
    // cue tracks the active reasoning mode — but only when the user
    // is viewing the agent the change applies to.
    if world.pump.active_view(tui) == AgentId::Main {
        apply_editor_border_for_thinking(tui, theme, level.as_ref());
    }
    // Footer surfaces the active thinking effort; record the new
    // settings so the change is visible without waiting for a turn.
    world
        .pump
        .note_agent_settings(tui, AgentId::Main, settings, window);
    let name = thinking_level_name(&level);
    // Record the change on the session log's user thread so a later
    // resume restores this level.
    let log_note = {
        let mut log = world.log.lock().await;
        log.append_thinking_change(ThreadFilter::USER, name)
            .err()
            .map(|err| format!("(couldn't record in session log: {err})"))
    };
    // Persist the choice so it survives a restart.
    let save_note = persist_config(config, |c| {
        c.thinking = Some(config_thinking_level(level.as_ref()));
    });
    let mut notice = format!("Thinking effort set to {name}.");
    for note in [save_note, log_note].into_iter().flatten() {
        notice.push(' ');
        notice.push_str(&note);
    }
    notice
}

/// Apply a confirmed thinking pick to sub-agent `n`: validate
/// against the target's model, stage into the world's override map
/// (applied at the sub's next turn start), record on the sub's log
/// thread, and refresh its footer entry. Deliberately does not touch
/// `config.toml` or the run config — those record the session
/// default, which is main's concern. Returns the user-facing notice.
async fn confirm_thinking_for_sub(
    tui: &mut Tui,
    level: Option<ThinkingConfig>,
    n: usize,
    model_catalog: &[ModelInfo],
    world: &mut SessionWorld,
    theme: &ThemeHandle,
) -> String {
    let target = AgentId::Sub(n);
    if resolve_agent(target, &world.agent, &world.registry).is_none() {
        return "This agent can't be prompted.".to_string();
    }
    let name = thinking_level_name(&level);
    // Validate against the target's model: the staged bundle
    // override's info if present, else a catalog lookup by the
    // target's settings keys, else skip (same lenient posture as
    // scripted mode).
    if let Some(tc) = level.as_ref() {
        let target_info: Option<Arc<ModelInfo>> = {
            let overrides = world
                .sub_overrides
                .lock()
                .expect("sub overrides mutex poisoned");
            overrides
                .get(&n)
                .and_then(|o| o.bundle.as_ref())
                .map(|(_, info, _, _)| Arc::clone(info))
        }
        .or_else(|| {
            world.pump.agent_settings(target).and_then(|s| {
                model_catalog
                    .iter()
                    .find(|m| m.provider == s.provider && m.id == s.model_id)
                    .cloned()
                    .map(Arc::new)
            })
        });
        if let Some(info) = target_info
            && let Err(msg) = validate_thinking_level(
                &info,
                &crate::modes::interactive::session::thinking_level_for(tc),
            )
        {
            return format!("Can't set thinking level {name:?} for agent {n}: {msg}");
        }
    }
    // Stage the standing choice; the sub's next turn applies it.
    world
        .sub_overrides
        .lock()
        .expect("sub overrides mutex poisoned")
        .entry(n)
        .or_default()
        .thinking = Some(level.clone());
    // Refresh the target's footer entry: same identity, new
    // thinking string, window unchanged.
    if let Some(mut settings) = world.pump.agent_settings(target).cloned() {
        settings.thinking = name.to_string();
        let window = world.pump.agent_context_window(target);
        world
            .pump
            .note_agent_settings(tui, target, settings, window);
    }
    if world.pump.active_view(tui) == target {
        apply_editor_border_for_thinking(tui, theme, level.as_ref());
    }
    // Record the change on the sub-agent's log thread so a resumed
    // transcript reflects it.
    let log_note = {
        let mut log = world.log.lock().await;
        log.append_thinking_change(ThreadFilter::subagent(n), name)
            .err()
            .map(|err| format!("(couldn't record in session log: {err})"))
    };
    let mut notice = format!("Thinking effort set to {name} for agent {n}.");
    if let Some(note) = log_note {
        notice.push(' ');
        notice.push_str(&note);
    }
    notice
}

/// Apply a confirmed model pick to the main agent: rebuild the
/// bundle, stage into the run config, persist to `config.toml`,
/// record on the session log's user thread, and refresh the footer.
/// Returns the user-facing notice.
async fn confirm_model_for_main(
    tui: &mut Tui,
    info: ModelInfo,
    auth: &AuthStorage,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: &Arc<std::sync::Mutex<Config>>,
    world: &mut SessionWorld,
) -> String {
    // Construct a fresh provider handle from the picked catalog
    // entry, carrying the active speed over so e.g. `--speed fast`
    // survives a model pick (degrading silently on providers
    // that ignore it).
    let speed = {
        let cfg = run_config.lock().expect("run config mutex poisoned");
        cfg.speed
    };
    match from_model_info(auth, info.clone(), speed) {
        Ok(ResolvedModel {
            provider,
            model_info,
            mut stream_options,
        }) => {
            // Re-apply the configured thinking-display mode: the
            // rebuilt baseline options would otherwise silently drop
            // it on every model swap.
            let display = config
                .lock()
                .expect("config mutex poisoned")
                .thinking_display;
            crate::model::apply_thinking_display(&mut stream_options, display);
            // Stage the swap into the loop-side snapshot (provider +
            // model + options + the pre-select key); the next turn
            // applies it. Never locks the agent, so it's safe
            // mid-turn — the in-flight turn keeps its model and the
            // swap takes effect next turn. Thinking effort is
            // preserved; read it back for the footer entry.
            let current_thinking = {
                let mut cfg = run_config.lock().expect("run config mutex poisoned");
                cfg.provider = provider;
                cfg.model_info = model_info;
                cfg.stream_options = stream_options;
                cfg.model_key = (info.provider.clone(), info.id.clone());
                cfg.thinking.clone()
            };
            // Record the new settings identity so the footer's model
            // line and context-window denominator reflect the swap
            // immediately rather than waiting for the next turn.
            let settings = aj_agent::events::AgentSettings {
                provider: info.provider.clone(),
                model_id: info.id.clone(),
                thinking: thinking_level_name(&current_thinking).to_string(),
                speed: speed_name(speed).to_string(),
            };
            world
                .pump
                .note_agent_settings(tui, AgentId::Main, settings, info.context_window);
            // Record the change on the session log's user thread so a
            // later resume restores this model.
            let log_note = {
                let mut log = world.log.lock().await;
                log.append_model_change(ThreadFilter::USER, &info.provider, &info.id)
                    .err()
                    .map(|err| format!("(couldn't record in session log: {err})"))
            };
            // Persist the model choice (provider + id) so it survives
            // a restart. `model_url` is intentionally left untouched:
            // it's a user-supplied endpoint override, not part of
            // "which model", and pinning the catalog's base URL into
            // it would freeze out future `models.json` updates.
            let save_note = persist_config(config, |c| {
                c.model_api = Some(info.provider.clone());
                c.model_name = Some(info.id.clone());
            });
            let mut notice = format!(
                "Model set to {} ({}/{}).",
                info.name, info.provider, info.id
            );
            for note in [save_note, log_note].into_iter().flatten() {
                notice.push(' ');
                notice.push_str(&note);
            }
            notice
        }
        Err(err) => format!("Failed to switch to {}: {err}", info.name),
    }
}

/// Apply a confirmed model pick to sub-agent `n`: rebuild the
/// bundle at the target's effective speed, stage it into the world's
/// override map (applied at the sub's next turn start), record on
/// the sub's log thread, and refresh its footer entry. Deliberately
/// does not touch `config.toml` or the run config. Returns the
/// user-facing notice.
async fn confirm_model_for_sub(
    tui: &mut Tui,
    info: ModelInfo,
    n: usize,
    auth: &AuthStorage,
    world: &mut SessionWorld,
) -> String {
    let target = AgentId::Sub(n);
    if resolve_agent(target, &world.agent, &world.registry).is_none() {
        return "This agent can't be prompted.".to_string();
    }
    // Effective speed: the staged override for this agent if
    // present, else the target's tracked settings string.
    let staged_speed = {
        let overrides = world
            .sub_overrides
            .lock()
            .expect("sub overrides mutex poisoned");
        overrides.get(&n).and_then(|o| o.speed)
    };
    let effective_speed = match staged_speed {
        Some(speed) => speed,
        None => world
            .pump
            .agent_settings(target)
            .and_then(|s| speed_from_name(&s.speed))
            .flatten(),
    };
    match from_model_info(auth, info.clone(), effective_speed) {
        Ok(ResolvedModel {
            provider,
            model_info,
            stream_options,
        }) => {
            // Stage the standing bundle choice; the sub's next turn
            // applies it.
            world
                .sub_overrides
                .lock()
                .expect("sub overrides mutex poisoned")
                .entry(n)
                .or_default()
                .bundle = Some((
                provider,
                model_info,
                stream_options,
                (info.provider.clone(), info.id.clone()),
            ));
            // Refresh the target's footer entry: new identity, the
            // catalog entry's window, thinking string preserved.
            let preserved_thinking = world
                .pump
                .agent_settings(target)
                .map(|s| s.thinking.clone())
                .unwrap_or_else(|| "off".to_string());
            let settings = aj_agent::events::AgentSettings {
                provider: info.provider.clone(),
                model_id: info.id.clone(),
                thinking: preserved_thinking,
                speed: speed_name(effective_speed).to_string(),
            };
            world
                .pump
                .note_agent_settings(tui, target, settings, info.context_window);
            // Record the change on the sub-agent's log thread so a
            // resumed transcript reflects it.
            let log_note = {
                let mut log = world.log.lock().await;
                log.append_model_change(ThreadFilter::subagent(n), &info.provider, &info.id)
                    .err()
                    .map(|err| format!("(couldn't record in session log: {err})"))
            };
            let mut notice = format!(
                "Model set to {} ({}/{}) for agent {n}.",
                info.name, info.provider, info.id
            );
            if let Some(note) = log_note {
                notice.push(' ');
                notice.push_str(&note);
            }
            notice
        }
        Err(err) => format!("Failed to switch to {}: {err}", info.name),
    }
}

/// Apply one settings-window change to the running session and
/// persist it. Returns the user-facing notice.
///
/// Live-appliable settings reuse the same confirm paths as their
/// dedicated selectors (model, thinking) or stage into the run
/// config / render settings; the agent- and tool-construction
/// settings are persisted with a "takes effect for new sessions /
/// on restart" note. When an apply fails the row's displayed value
/// is reverted through `corrections` so the window never shows a
/// value that isn't actually active.
#[allow(clippy::too_many_arguments)]
async fn apply_setting_change(
    tui: &mut Tui,
    id: &str,
    value: &str,
    auth: &AuthStorage,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: &Arc<std::sync::Mutex<Config>>,
    model_catalog: &[ModelInfo],
    world: &mut SessionWorld,
    theme: &ThemeHandle,
    theme_watch: &mut ThemeWatch,
    render_settings: &RenderSettings,
    corrections: &SettingsCorrectionsHandle,
) -> Option<String> {
    match id {
        MODEL_SETTING_ID => {
            // The picker only emits catalog rows, so the lookup is
            // effectively infallible; degrade with a notice anyway.
            let Some(info) = value.split_once('/').and_then(|(provider, model_id)| {
                model_catalog
                    .iter()
                    .find(|m| m.provider == provider && m.id == model_id)
                    .cloned()
            }) else {
                let active = {
                    let cfg = run_config.lock().expect("run config mutex poisoned");
                    format!("{}/{}", cfg.model_key.0, cfg.model_key.1)
                };
                push_correction(corrections, tui, MODEL_SETTING_ID, active);
                return Some(format!("Unknown model {value}."));
            };
            let notice = confirm_model_for_main(tui, info, auth, run_config, config, world).await;
            // `confirm_model_for_main` reports a rebuild failure only
            // as notice text; compare the staged key instead so the
            // row reverts to the model that's actually active.
            let active = {
                let cfg = run_config.lock().expect("run config mutex poisoned");
                format!("{}/{}", cfg.model_key.0, cfg.model_key.1)
            };
            if active != value {
                push_correction(corrections, tui, MODEL_SETTING_ID, active);
            }
            Some(notice)
        }
        "thinking" => match thinking_config_from_name(value) {
            Some(level) => {
                Some(confirm_thinking_for_main(tui, level, run_config, config, world, theme).await)
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
                let mut cfg = run_config.lock().expect("run config mutex poisoned");
                crate::model::apply_thinking_display(&mut cfg.stream_options, display);
            }
            let save_note = persist_config(config, |c| c.thinking_display = display);
            Some(join_notice(
                format!("Thinking display set to {value}. Takes effect next turn."),
                save_note,
            ))
        }
        "speed" => match speed_from_name(value) {
            Some(speed) => Some(
                confirm_speed_for_main(tui, speed, auth, run_config, config, world, corrections)
                    .await,
            ),
            None => Some(format!("Unknown speed {value:?}.")),
        },
        "theme" => {
            // Strict load so a broken user theme surfaces instead of
            // silently falling back to the bundled dark palette.
            match Theme::load_strict(value) {
                Ok(loaded) => {
                    theme.replace(loaded);
                    tui.invalidate();
                    tui.request_render();
                    // Re-point the hot-reload watcher at the newly
                    // configured theme's file.
                    *theme_watch = ThemeWatch::install(value);
                    let save_note = persist_config(config, |c| c.theme = Some(value.to_string()));
                    Some(join_notice(format!("Theme set to {value}."), save_note))
                }
                Err(err) => {
                    let active = {
                        let cfg = config.lock().expect("config mutex poisoned");
                        resolve_theme_name(cfg.theme.as_deref()).to_string()
                    };
                    push_correction(corrections, tui, "theme", active);
                    Some(format!("Couldn't load theme {value:?}: {err}"))
                }
            }
        }
        "hide_thinking_block" => {
            let hide = value == "true";
            render_settings.set_hide_thinking_block(hide);
            tui.request_render();
            let save_note = persist_config(config, |c| c.hide_thinking_block = hide);
            Some(join_notice(
                format!(
                    "Thinking blocks {}.",
                    if hide { "hidden" } else { "expanded" }
                ),
                save_note,
            ))
        }
        "image_show_in_terminal" => {
            let show = value == "true";
            render_settings.set_show_image_in_terminal(show);
            tui.request_render();
            let save_note = persist_config(config, |c| c.image_show_in_terminal = show);
            Some(join_notice(
                format!("image_show_in_terminal set to {show}."),
                save_note,
            ))
        }
        "image_auto_resize" => {
            let on = value == "true";
            let save_note = persist_config(config, |c| c.image_auto_resize = on);
            Some(join_notice(
                format!("image_auto_resize set to {on}. Takes effect for new sessions."),
                save_note,
            ))
        }
        "image_block" => {
            let on = value == "true";
            let save_note = persist_config(config, |c| c.image_block = on);
            Some(join_notice(
                format!("image_block set to {on}. Takes effect for new sessions."),
                save_note,
            ))
        }
        "syntax_highlighting" => {
            let on = value == "true";
            let save_note = persist_config(config, |c| c.syntax_highlighting = on);
            Some(join_notice(
                format!("syntax_highlighting set to {on}. Takes effect for new sessions."),
                save_note,
            ))
        }
        "model_url" => {
            let url = (!value.is_empty()).then(|| value.to_string());
            let save_note = persist_config(config, |c| c.model_url = url.clone());
            let what = match &url {
                Some(u) => format!("set to {u}"),
                None => "unset".to_string(),
            };
            Some(join_notice(
                format!("model_url {what}. Takes effect on restart."),
                save_note,
            ))
        }
        "disabled_tools" => {
            let tools: Vec<String> = value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            let save_note = persist_config(config, |c| c.disabled_tools = tools.clone());
            let what = if tools.is_empty() {
                "cleared".to_string()
            } else {
                format!("set to {}", tools.join(", "))
            };
            Some(join_notice(
                format!("disabled_tools {what}. Takes effect for new sessions."),
                save_note,
            ))
        }
        "disabled_skills" => {
            let skills: Vec<String> = value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            let save_note = persist_config(config, |c| c.disabled_skills = skills.clone());
            let what = if skills.is_empty() {
                "cleared".to_string()
            } else {
                format!("set to {}", skills.join(", "))
            };
            Some(join_notice(
                format!("disabled_skills {what}. Takes effect for new sessions."),
                save_note,
            ))
        }
        other => {
            // Any other key that's a real schema option is a plain
            // config-backed value with no extra side effects: the
            // options that need a provider rebuild, theme reload, or a
            // live render update are intercepted by the arms above.
            // Route the rest through the schema so a freshly-added
            // option is editable from the settings window without a
            // bespoke arm here. `apply_str` validates the value; the
            // `every_option_applies_its_string_value` guard in `aj-conf`
            // keeps every option round-trippable through this path.
            let Some(option) = Config::option(other) else {
                return Some(format!("Unknown setting {other:?}."));
            };
            let (baseline, updated) = {
                let mut cfg = config.lock().expect("config mutex poisoned");
                let baseline = cfg.clone();
                if let Err(err) = option.apply_str(value, &mut cfg) {
                    // Reject without half-applying: restore the pre-edit
                    // config and report why.
                    *cfg = baseline;
                    return Some(format!("Can't set {other}: {err}"));
                }
                (baseline, cfg.clone())
            };
            let save_note = match updated.persist_changed(&baseline) {
                Ok(()) => None,
                Err(err) => Some(format!("(couldn't save to config.toml: {err})")),
            };
            Some(join_notice(format!("{other} set to {value}."), save_note))
        }
    }
}

/// Apply a speed change to the main agent: rebuild the provider
/// bundle at the current model so the speed-derived headers are
/// re-stamped, stage it into the run config, persist to
/// `config.toml`, record on the session log's user thread, and
/// refresh the footer. On a rebuild failure (e.g. scripted mode,
/// whose provider isn't in the registry) nothing is staged and the
/// settings row is reverted via `corrections`. Returns the
/// user-facing notice.
async fn confirm_speed_for_main(
    tui: &mut Tui,
    speed: Option<Speed>,
    auth: &AuthStorage,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: &Arc<std::sync::Mutex<Config>>,
    world: &mut SessionWorld,
    corrections: &SettingsCorrectionsHandle,
) -> String {
    let name = speed_name(speed);
    let (model_info, prev_speed) = {
        let cfg = run_config.lock().expect("run config mutex poisoned");
        ((*cfg.model_info).clone(), cfg.speed)
    };
    match from_model_info(auth, model_info, speed) {
        Ok(ResolvedModel {
            provider,
            model_info,
            mut stream_options,
        }) => {
            // The rebuilt baseline options would otherwise drop the
            // configured thinking-display mode.
            let display = config
                .lock()
                .expect("config mutex poisoned")
                .thinking_display;
            crate::model::apply_thinking_display(&mut stream_options, display);
            // Stage into the loop-side snapshot; the next turn
            // applies it. Never locks the agent, so it's safe
            // mid-turn.
            let (settings, window) = {
                let mut cfg = run_config.lock().expect("run config mutex poisoned");
                cfg.provider = provider;
                cfg.model_info = model_info;
                cfg.stream_options = stream_options;
                cfg.speed = speed;
                (
                    aj_agent::events::AgentSettings {
                        provider: cfg.model_key.0.clone(),
                        model_id: cfg.model_key.1.clone(),
                        thinking: thinking_level_name(&cfg.thinking).to_string(),
                        speed: name.to_string(),
                    },
                    cfg.model_info.context_window,
                )
            };
            world
                .pump
                .note_agent_settings(tui, AgentId::Main, settings, window);
            // Record the change on the session log's user thread so a
            // later resume restores this speed.
            let log_note = {
                let mut log = world.log.lock().await;
                log.append_speed_change(ThreadFilter::USER, name)
                    .err()
                    .map(|err| format!("(couldn't record in session log: {err})"))
            };
            // "standard" persists as key removal: it's the default,
            // and `speed_from_name` maps it to `None` on the wire.
            let save_note = persist_config(config, |c| {
                c.speed = match speed {
                    None | Some(Speed::Standard) => None,
                    Some(Speed::Fast) => Some(ConfigSpeed::Fast),
                };
            });
            let mut notice = format!("Speed set to {name}. Takes effect next turn.");
            for note in [save_note, log_note].into_iter().flatten() {
                notice.push(' ');
                notice.push_str(&note);
            }
            notice
        }
        Err(err) => {
            push_correction(
                corrections,
                tui,
                "speed",
                speed_name(prev_speed).to_string(),
            );
            format!("Failed to set speed {name}: {err}")
        }
    }
}

/// Queue a display fix for a settings-window row and schedule a
/// repaint so the component drains it promptly.
fn push_correction(
    corrections: &SettingsCorrectionsHandle,
    tui: &mut Tui,
    id: &str,
    value: String,
) {
    corrections
        .lock()
        .expect("settings corrections poisoned")
        .push((id.to_string(), value));
    tui.request_render();
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
    run_config: Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: Arc<std::sync::Mutex<Config>>,
    model_catalog: &[ModelInfo],
    world: &mut SessionWorld,
    theme: &ThemeHandle,
    render_settings: &RenderSettings,
    theme_watch: &mut ThemeWatch,
) -> SelectorPollOutcome {
    match selector {
        OpenSelector::Thinking {
            handle,
            outcome,
            target,
            parent_palette,
        } => {
            let outcome_value = outcome.lock().expect("thinking outcome poisoned").take();
            match outcome_value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Thinking {
                    handle,
                    outcome,
                    target,
                    parent_palette,
                }),
                Some(ThinkingSelectorOutcome::Confirmed(level)) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.hide_overlay(&parent.handle);
                    }
                    let notice = match target {
                        AgentId::Main => {
                            confirm_thinking_for_main(
                                tui,
                                level,
                                &run_config,
                                &config,
                                world,
                                theme,
                            )
                            .await
                        }
                        AgentId::Sub(n) => {
                            confirm_thinking_for_sub(tui, level, n, model_catalog, world, theme)
                                .await
                        }
                    };
                    SelectorPollOutcome::Closed {
                        notice: Some(notice),
                        follow_up: None,
                        start_login: None,
                        session_request: None,
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
                        session_request: None,
                    }
                }
            }
        }
        OpenSelector::Model {
            handle,
            outcome,
            target,
            parent_palette,
        } => {
            let outcome_value = outcome.lock().expect("model outcome poisoned").take();
            match outcome_value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Model {
                    handle,
                    outcome,
                    target,
                    parent_palette,
                }),
                Some(ModelSelectorOutcome::Confirmed(info)) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.hide_overlay(&parent.handle);
                    }
                    let notice = match target {
                        AgentId::Main => {
                            confirm_model_for_main(tui, info, auth, &run_config, &config, world)
                                .await
                        }
                        AgentId::Sub(n) => confirm_model_for_sub(tui, info, n, auth, world).await,
                    };
                    SelectorPollOutcome::Closed {
                        notice: Some(notice),
                        follow_up: None,
                        start_login: None,
                        session_request: None,
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
                        session_request: None,
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
                    // already active. Saves the rebuild (and the
                    // chat-container clear that would briefly hide
                    // the user's scrollback).
                    if world.session_id == preview.session_id {
                        return SelectorPollOutcome::Closed {
                            notice: Some(format!("Already on session {}.", preview.session_id)),
                            follow_up: None,
                            start_login: None,
                            session_request: None,
                        };
                    }
                    // Hand the pick to the outer session loop, which
                    // tears down the current world and rebuilds onto
                    // the chosen session (and emits the switch notice
                    // after the new world is installed).
                    SelectorPollOutcome::Closed {
                        notice: None,
                        follow_up: None,
                        start_login: None,
                        session_request: Some(SessionRequest::Resume(preview.session_id)),
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
                        session_request: None,
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
                    session_request: None,
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
                    session_request: None,
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
                        session_request: None,
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
                        session_request: None,
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
                            session_request: None,
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
                                session_request: None,
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
                        session_request: None,
                    }
                }
            }
        }
        OpenSelector::UsageStatus {
            handle,
            outcome,
            parent_palette,
        } => {
            use crate::modes::interactive::components::usage_status::UsageStatusOutcome;
            match outcome.take() {
                None => SelectorPollOutcome::StillOpen(OpenSelector::UsageStatus {
                    handle,
                    outcome,
                    parent_palette,
                }),
                Some(UsageStatusOutcome::Closed) => {
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
                        session_request: None,
                    }
                }
            }
        }
        OpenSelector::AgentPicker {
            handle,
            outcome,
            parent_palette,
        } => {
            let outcome_value = outcome
                .lock()
                .expect("agent picker outcome poisoned")
                .take();
            match outcome_value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::AgentPicker {
                    handle,
                    outcome,
                    parent_palette,
                }),
                Some(AgentPickerOutcome::Confirmed(id)) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.hide_overlay(&parent.handle);
                    }
                    // Switch the chat view to the chosen agent and mark
                    // the editor so the user sees which agent they're
                    // observing (cleared when switching back to main).
                    world.pump.set_active_view(tui, id);
                    apply_editor_agent_marker(tui, id);
                    apply_editor_border_for_view(tui, theme, &world.pump, &run_config, id);
                    SelectorPollOutcome::Closed {
                        notice: None,
                        follow_up: None,
                        start_login: None,
                        session_request: None,
                    }
                }
                Some(AgentPickerOutcome::ConfirmedTask(id)) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.hide_overlay(&parent.handle);
                    }
                    // The picker only lists bash tasks, so resolve the
                    // command line for the viewer header. If the task is
                    // gone from the registry, there is nothing to show;
                    // close to chat with a notice.
                    let command = world.task_registry.summary(id).and_then(|s| match s.kind {
                        aj_agent::tool::TaskKind::Bash { command } => Some(command),
                        aj_agent::tool::TaskKind::Agent { .. } => None,
                    });
                    match command {
                        Some(command) => {
                            let initial_inner_rows =
                                large_overlay_inner_rows(usize::from(tui.terminal().rows()));
                            let inner =
                                TaskOutputComponent::new(world.task_registry.clone(), id, command);
                            let outcome = inner.outcome_handle();
                            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                                format!("Task #{id}"),
                                Box::new(inner),
                                crate::config::theme::overlay_window_theme(theme),
                                initial_inner_rows,
                            )
                            .with_dynamic_height(tui.handle(), large_overlay_inner_rows)
                            .with_subtitle(&subtitle_task_output());
                            let handle =
                                tui.show_overlay(Box::new(window), large_overlay_options());
                            // Open the viewer in place of the picker and
                            // keep polling it. This is the one overlay
                            // opened from inside the poll handler rather
                            // than `handle_command`.
                            SelectorPollOutcome::StillOpen(OpenSelector::TaskOutput {
                                handle,
                                outcome,
                            })
                        }
                        None => SelectorPollOutcome::Closed {
                            notice: Some(format!("Background task #{id} is no longer available.")),
                            follow_up: None,
                            start_login: None,
                            session_request: None,
                        },
                    }
                }
                Some(AgentPickerOutcome::KillTask(id)) => {
                    tui.hide_overlay(&handle);
                    if let Some(parent) = parent_palette {
                        tui.hide_overlay(&parent.handle);
                    }
                    // The registry cancels the task's token; the
                    // driver kills the process group, flips the
                    // status, and the resulting `TaskEnd` freezes the
                    // cell — no pump bookkeeping to update here. The
                    // picker rows are a snapshot from open time, so
                    // consult the live status: the task may have
                    // finished while the picker was up.
                    let live_status = world
                        .task_registry
                        .snapshot()
                        .into_iter()
                        .find(|t| t.id == id)
                        .map(|t| t.status);
                    let notice = match live_status {
                        Some(aj_agent::tool::TaskStatus::Running) => {
                            world.task_registry.kill(id);
                            format!("Killing background task #{id}.")
                        }
                        Some(_) => format!("Background task #{id} already finished."),
                        None => {
                            format!("Background task #{id} is not in the registry (already gone?).")
                        }
                    };
                    SelectorPollOutcome::Closed {
                        notice: Some(notice),
                        follow_up: None,
                        start_login: None,
                        session_request: None,
                    }
                }
                Some(AgentPickerOutcome::Cancelled) => {
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
                        session_request: None,
                    }
                }
            }
        }
        OpenSelector::TaskOutput { handle, outcome } => match outcome.take() {
            None => SelectorPollOutcome::StillOpen(OpenSelector::TaskOutput { handle, outcome }),
            Some(TaskOutputOutcome::Closed) => {
                tui.hide_overlay(&handle);
                SelectorPollOutcome::Closed {
                    notice: None,
                    follow_up: None,
                    start_login: None,
                    session_request: None,
                }
            }
        },
        OpenSelector::Settings {
            handle,
            outcome,
            changes,
            corrections,
            parent_palette,
        } => {
            // Apply queued changes first — the window stays open
            // while the user keeps editing, so changes and the
            // eventual close arrive through separate channels.
            let drained: Vec<(String, String)> =
                std::mem::take(&mut *changes.lock().expect("settings changes poisoned"));
            for (id, value) in drained {
                let notice = apply_setting_change(
                    tui,
                    &id,
                    &value,
                    auth,
                    &run_config,
                    &config,
                    model_catalog,
                    world,
                    theme,
                    theme_watch,
                    render_settings,
                    &corrections,
                )
                .await;
                if let Some(text) = notice {
                    world.pump.handle(tui, &notice_event(&text));
                }
            }
            let outcome_value = outcome.lock().expect("settings outcome poisoned").take();
            match outcome_value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Settings {
                    handle,
                    outcome,
                    changes,
                    corrections,
                    parent_palette,
                }),
                Some(SettingsWindowOutcome::Closed) => {
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
                        session_request: None,
                    }
                }
            }
        }
        OpenSelector::Skills {
            handle,
            outcome,
            changes,
            parent_palette,
        } => {
            // Persist queued toggles first — the window stays open
            // while the user keeps toggling, so changes and the
            // eventual close arrive through separate channels.
            let drained: Vec<(String, String)> =
                std::mem::take(&mut *changes.lock().expect("skills changes poisoned"));
            for (name, value) in drained {
                let disable = value == "disabled";
                let save_note = persist_config(&config, |c| {
                    if disable {
                        if !c.disabled_skills.contains(&name) {
                            c.disabled_skills.push(name.clone());
                        }
                    } else {
                        c.disabled_skills.retain(|n| n != &name);
                    }
                });
                let notice = join_notice(
                    format!("Skill {name} {value}. Takes effect for new sessions."),
                    save_note,
                );
                world.pump.handle(tui, &notice_event(&notice));
            }
            let outcome_value = outcome.lock().expect("skills outcome poisoned").take();
            match outcome_value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Skills {
                    handle,
                    outcome,
                    changes,
                    parent_palette,
                }),
                Some(SkillsWindowOutcome::Closed) => {
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
                        session_request: None,
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
                        session_request: None,
                    }
                }
                Some(CommandPaletteOutcome::Confirmed { action }) => {
                    // Hide (don't remove) the palette so we can
                    // restore it if the follow-up command opens a
                    // sub-selector and the user cancels back. If
                    // the follow-up doesn't open a sub-selector,
                    // `handle_command` tears the palette
                    // down once it finishes dispatching. The
                    // outcome handle is cloned into the follow-up
                    // so the host can re-install
                    // `OpenSelector::Palette` after a child cancel,
                    // keeping the palette pollable.
                    tui.set_overlay_hidden(&handle, true);
                    SelectorPollOutcome::Closed {
                        notice: None,
                        follow_up: Some(PaletteFollowUp {
                            action,
                            parent_palette: ParentPalette {
                                handle,
                                outcome: outcome.clone(),
                            },
                        }),
                        start_login: None,
                        session_request: None,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use aj_conf::{AgentEnv, ContextFile, ContextFileKind, SystemPrompt, SystemPromptSource};
    use tempfile::TempDir;

    use super::*;
    use crate::modes::interactive::test_support::{
        build_test_world, create_spec, drive_turn, finalized_text_message, one_turn_session,
        resume_spec, scripted_model_info, scripted_run_config,
    };

    /// Build an [`AgentEnv`] for use in the helper tests below.
    /// Working directory / OS / date / git root are all stubbed —
    /// only `system_prompt` and `context_files` matter for the
    /// startup-notice builders.
    fn env_with(context_files: Vec<ContextFile>) -> AgentEnv {
        AgentEnv {
            working_directory: PathBuf::from("/tmp"),
            git_root_directory: None,
            operating_system: "linux".to_string(),
            today_date: "2025-01-01".to_string(),
            system_prompt: SystemPrompt {
                content: "builtin prompt".to_string(),
                source: SystemPromptSource::Builtin,
            },
            context_files,
            skills: Vec::new(),
            skill_diagnostics: Vec::new(),
        }
    }

    #[test]
    fn resolve_theme_name_defaults_to_light_when_unset() {
        assert_eq!(resolve_theme_name(None), "light");
    }

    #[test]
    fn quit_arm_notice_shows_each_part_only_when_nonzero() {
        assert_eq!(
            quit_arm_notice(1, 0),
            "1 agent still running — press Ctrl+C again to quit"
        );
        assert_eq!(
            quit_arm_notice(2, 0),
            "2 agents still running — press Ctrl+C again to quit"
        );
        assert_eq!(
            quit_arm_notice(0, 1),
            "1 task still running — press Ctrl+C again to quit"
        );
        assert_eq!(
            quit_arm_notice(0, 3),
            "3 tasks still running — press Ctrl+C again to quit"
        );
        assert_eq!(
            quit_arm_notice(2, 1),
            "2 agents / 1 task still running — press Ctrl+C again to quit"
        );
    }

    #[test]
    fn running_work_counts_splits_agents_and_bash_tasks() {
        use aj_agent::TaskSummary;
        use aj_agent::tool::{TaskKind, TaskStatus};

        let summary = |id: usize, kind: TaskKind, status: TaskStatus| TaskSummary {
            id,
            owner: AgentId::Main,
            kind,
            label: "label".to_string(),
            status,
            started_at: std::time::Instant::now(),
        };
        let bash = |id, status| {
            summary(
                id,
                TaskKind::Bash {
                    command: "sleep 5".to_string(),
                },
                status,
            )
        };
        let agent_task = |id, status| {
            summary(
                id,
                TaskKind::Agent {
                    agent_id: id,
                    task: "explore".to_string(),
                },
                status,
            )
        };

        assert_eq!(running_work_counts(0, &[]), (0, 0));
        // Running bash tasks count as tasks; terminal ones don't
        // count at all.
        let tasks = vec![
            bash(1, TaskStatus::Running),
            bash(2, TaskStatus::Exited(Some(0))),
            bash(3, TaskStatus::Killed),
        ];
        assert_eq!(running_work_counts(1, &tasks), (1, 1));
        // A running agent-backed task counts as an agent (matching
        // the footer), on top of the binary-driven turns.
        let tasks = vec![
            agent_task(4, TaskStatus::Running),
            bash(5, TaskStatus::Running),
        ];
        assert_eq!(running_work_counts(2, &tasks), (3, 1));
    }

    #[test]
    fn resolve_view_thinking_prefers_parsed_settings_over_fallback() {
        let settings = aj_agent::events::AgentSettings {
            provider: "anthropic".into(),
            model_id: "claude-x".into(),
            thinking: "high".into(),
            speed: "standard".into(),
        };
        let fallback = Some(ThinkingConfig::Low);
        assert_eq!(
            resolve_view_thinking(Some(&settings), &fallback),
            Some(ThinkingConfig::High)
        );
        // An explicit "off" wins over the fallback: the parse
        // yields `Some(None)`.
        let off = aj_agent::events::AgentSettings {
            thinking: "off".into(),
            ..settings.clone()
        };
        assert_eq!(resolve_view_thinking(Some(&off), &fallback), None);
    }

    #[test]
    fn resolve_view_thinking_falls_back_on_missing_or_unparseable_entry() {
        let fallback = Some(ThinkingConfig::Medium);
        assert_eq!(
            resolve_view_thinking(None, &fallback),
            Some(ThinkingConfig::Medium)
        );
        // Replayed legacy entries can carry an empty thinking
        // string; that parses to nothing and falls back too.
        let garbage = aj_agent::events::AgentSettings {
            provider: String::new(),
            model_id: String::new(),
            thinking: String::new(),
            speed: "standard".into(),
        };
        assert_eq!(
            resolve_view_thinking(Some(&garbage), &fallback),
            Some(ThinkingConfig::Medium)
        );
    }

    #[test]
    fn resolve_theme_name_passes_explicit_name_through() {
        assert_eq!(resolve_theme_name(Some("dark")), "dark");
        assert_eq!(resolve_theme_name(Some("solarized")), "solarized");
    }

    #[test]
    fn build_context_notice_without_files_lists_only_the_system_prompt() {
        let env = env_with(Vec::new());
        assert_eq!(
            build_context_notice(&env),
            "Context:\n  - builtin (system prompt; override with ~/.agents/SYSTEM_PROMPT.md)"
        );
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
        let expected = "Context:\n  \
             - builtin (system prompt; override with ~/.agents/SYSTEM_PROMPT.md)\n  \
             - ~/.agents/AGENTS.md (user instructions)\n  \
             - /var/project/AGENTS.md (project instructions)";
        assert_eq!(notice, expected);
    }

    #[test]
    fn build_context_notice_override_shows_tildified_prompt_path() {
        // `display_path` tildifies under `$HOME`, so build the path
        // off the live `HOME` env var to keep the assertion stable
        // across machines.
        let home = std::env::var("HOME").expect("HOME set in test env");
        let path = PathBuf::from(&home).join(".agents/SYSTEM_PROMPT.md");
        let mut env = env_with(Vec::new());
        env.system_prompt = SystemPrompt {
            content: "override prompt".to_string(),
            source: SystemPromptSource::Override(path),
        };
        assert_eq!(
            build_context_notice(&env),
            "Context:\n  - ~/.agents/SYSTEM_PROMPT.md (system prompt)"
        );
    }

    #[test]
    fn build_context_notice_lists_skills_with_status_markers() {
        let skill = |name: &str, enabled: bool, dmi: bool| aj_conf::skills::Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            path: PathBuf::from(format!("/var/skills/{name}/SKILL.md")),
            enabled,
            disable_model_invocation: dmi,
        };
        let mut env = env_with(Vec::new());
        env.skills = vec![
            skill("alpha", true, false),
            skill("beta", false, false),
            skill("gamma", true, true),
        ];

        let notice = build_context_notice(&env);
        let beta =
            aj_tui::style::strikethrough("/var/skills/beta/SKILL.md (skill: beta, disabled)");
        let expected = format!(
            "Context:\n  \
             - builtin (system prompt; override with ~/.agents/SYSTEM_PROMPT.md)\n  \
             - /var/skills/alpha/SKILL.md (skill: alpha)\n  \
             - {beta}\n  \
             - /var/skills/gamma/SKILL.md (skill: gamma, model-invocation disabled)"
        );
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

    /// [`build_next_world`] with a default config, bundled theme,
    /// fixed render settings, and a scripted run config with no
    /// scripted replies — building a world never runs inference.
    fn next_world(
        persistence: &ConversationPersistence,
        requested: SessionSpec,
        previous_session_id: &str,
    ) -> Result<NextWorld> {
        build_next_world(
            &Config::default(),
            &scripted_run_config(Vec::new()),
            &RenderSettings::new(false, false, true),
            &ThemeHandle::new(Theme::bundled_dark()),
            persistence,
            requested,
            previous_session_id,
            None,
            Arc::new(Vec::new()),
        )
    }

    #[tokio::test]
    async fn build_next_world_create_returns_fresh_world_and_notice() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let previous_id = one_turn_session(&persistence, "hello there", "scripted reply").await;

        let next = next_world(
            &persistence,
            SessionSpec::Create {
                entry: SessionEntry::Switch,
            },
            &previous_id,
        )
        .expect("create request succeeds");

        assert!(
            matches!(
                next.spec,
                SessionSpec::Create {
                    entry: SessionEntry::Switch
                }
            ),
            "requested spec carried through for install"
        );
        assert_ne!(
            next.world.session_id, previous_id,
            "fresh world gets a new session id"
        );
        assert_eq!(
            next.notices,
            vec![format!(
                "Started a fresh session ({}).",
                next.world.session_id
            )]
        );
    }

    #[tokio::test]
    async fn build_next_world_resume_returns_target_world_and_notice() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let first_id = one_turn_session(&persistence, "first prompt", "first reply").await;
        let second_id = one_turn_session(&persistence, "second prompt", "second reply").await;

        let next = next_world(&persistence, resume_spec(&first_id), &second_id)
            .expect("resume request succeeds");

        assert_eq!(
            next.world.session_id, first_id,
            "world bound to the requested session"
        );
        assert!(
            matches!(
                &next.spec,
                SessionSpec::Resume {
                    session_id,
                    entry: SessionEntry::Switch
                } if *session_id == first_id
            ),
            "requested spec carried through for install"
        );
        assert_eq!(
            next.notices,
            vec![format!("Switched to session {first_id}.")]
        );
    }

    #[tokio::test]
    async fn build_next_world_falls_back_to_previous_on_failure() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let previous_id = one_turn_session(&persistence, "hello there", "scripted reply").await;

        let next = next_world(&persistence, resume_spec("does-not-exist"), &previous_id)
            .expect("fallback onto the previous session succeeds");

        assert_eq!(
            next.world.session_id, previous_id,
            "fallback world resumes the previous session"
        );
        assert!(
            matches!(
                &next.spec,
                SessionSpec::Resume {
                    session_id,
                    entry: SessionEntry::Switch
                } if *session_id == previous_id
            ),
            "fallback spec carried through for install"
        );
        assert_eq!(next.notices.len(), 1, "only the failure notice is pumped");
        assert!(
            next.notices[0].starts_with("Failed to switch to session does-not-exist:"),
            "unexpected failure notice: {:?}",
            next.notices[0]
        );
    }

    #[test]
    fn build_next_world_is_fatal_when_fallback_also_fails() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());

        let result = next_world(&persistence, resume_spec("nope"), "also-nope");
        assert!(
            result.is_err(),
            "no fallback world exists, so the transition is fatal"
        );
    }

    /// Scripted assistant message that calls the `agent` tool, so a
    /// driven turn spawns a sub-agent off the world's main agent.
    fn agent_tool_call_message(task: &str) -> aj_models::types::AssistantMessage {
        use aj_models::types::{AssistantContent, StopReason, ToolCall};
        aj_models::types::AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: "tu-1".to_string(),
                name: "agent".to_string(),
                arguments: serde_json::json!({ "task": task }),
            })],
            api: "scripted".to_string(),
            provider: "scripted".to_string(),
            model: "scripted".to_string(),
            response_id: Some("test-tool-msg".to_string()),
            usage: Default::default(),
            stop_reason: StopReason::ToolUse,
            error: None,
            timestamp: 0,
        }
    }

    /// The pre-turn config stamp is main-only: after the global run
    /// config changes, a sub-agent continuation keeps its spawn-time
    /// settings while a main turn picks up the new config.
    #[tokio::test]
    async fn apply_turn_config_stamps_main_and_leaves_subs_alone() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());

        // Shared scripted provider, consumed in run order: the
        // parent's tool call, the sub-agent's report, the parent's
        // wrap-up.
        let run_config = scripted_run_config(vec![
            agent_tool_call_message("look into it"),
            finalized_text_message("sub report"),
            finalized_text_message("parent done"),
        ]);
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        drive_turn(&world, "delegate").await;

        let sub = world
            .registry
            .get(1)
            .expect("sub-agent retained under id 1");
        {
            let s = sub.lock().await;
            assert_eq!(s.model_info().id, "scripted", "spawn-time model inherited");
            assert_eq!(s.default_thinking(), None, "spawn-time thinking inherited");
        }

        // The user changes the global run config after the spawn.
        {
            let mut cfg = run_config.lock().expect("run config mutex poisoned");
            cfg.model_info = Arc::new(ModelInfo {
                id: "changed".to_string(),
                ..scripted_model_info()
            });
            cfg.thinking = Some(ThinkingConfig::High);
        }

        // A sub continuation turn stamps nothing without overrides.
        let no_overrides = std::sync::Mutex::new(HashMap::new());
        {
            let mut s = sub.lock().await;
            apply_turn_config(AgentId::Sub(1), &mut s, &run_config, &no_overrides);
            assert_eq!(s.model_info().id, "scripted", "sub keeps its model");
            assert_eq!(s.default_thinking(), None, "sub keeps its thinking");
        }

        // A main turn picks up the new config.
        {
            let mut m = world.agent.lock().await;
            apply_turn_config(AgentId::Main, &mut m, &run_config, &no_overrides);
            assert_eq!(m.model_info().id, "changed");
            assert_eq!(m.default_thinking(), Some(ThinkingConfig::High));
        }
    }

    /// Staged per-sub overrides are applied at the sub's turn start,
    /// axis by axis: an entry with only a thinking override leaves
    /// the spawn-time model alone, a later bundle override swaps the
    /// model, and entries are re-applied idempotently.
    #[tokio::test]
    async fn apply_turn_config_applies_staged_sub_overrides() {
        use std::time::Duration;

        use aj_models::scripted::ScriptedProvider;

        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![
            agent_tool_call_message("look into it"),
            finalized_text_message("sub report"),
            finalized_text_message("parent done"),
        ]);
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        drive_turn(&world, "delegate").await;
        let sub = world
            .registry
            .get(1)
            .expect("sub-agent retained under id 1");

        // Stage a thinking + speed override only: the model axis is
        // untouched.
        {
            let mut overrides = world.sub_overrides.lock().expect("overrides poisoned");
            let entry = overrides.entry(1).or_default();
            entry.thinking = Some(Some(ThinkingConfig::High));
            entry.speed = Some(Some(Speed::Fast));
        }
        {
            let mut s = sub.lock().await;
            apply_turn_config(AgentId::Sub(1), &mut s, &run_config, &world.sub_overrides);
            assert_eq!(s.default_thinking(), Some(ThinkingConfig::High));
            assert_eq!(
                s.model_info().id,
                "scripted",
                "no bundle override: spawn-time model kept"
            );
        }

        // Stage a bundle override on top: the model swaps too.
        {
            let mut overrides = world.sub_overrides.lock().expect("overrides poisoned");
            overrides.entry(1).or_default().bundle = Some((
                Arc::new(ScriptedProvider::from_messages(
                    Vec::new(),
                    0,
                    Duration::ZERO,
                )),
                Arc::new(ModelInfo {
                    id: "override-model".to_string(),
                    ..scripted_model_info()
                }),
                StreamOptions::default(),
                ("scripted".to_string(), "override-model".to_string()),
            ));
        }
        {
            let mut s = sub.lock().await;
            // Applied twice: the entry is a standing choice and
            // re-applies idempotently.
            apply_turn_config(AgentId::Sub(1), &mut s, &run_config, &world.sub_overrides);
            apply_turn_config(AgentId::Sub(1), &mut s, &run_config, &world.sub_overrides);
            assert_eq!(s.model_info().id, "override-model");
            assert_eq!(s.default_thinking(), Some(ThinkingConfig::High));
        }

        // The global run config never moved.
        let cfg = run_config.lock().expect("run config mutex poisoned");
        assert_eq!(cfg.thinking, None);
        assert_eq!(cfg.model_info.id, "scripted");
    }

    use aj_session::ConversationLog;

    use crate::modes::interactive::components::thinking_selector::ThinkingSelectorComponent;
    use crate::modes::interactive::layout::build_layout;
    use crate::modes::interactive::test_support::StubTerminal;

    /// Build a world whose main turn spawned sub-agent 1, plus a
    /// headless TUI with the layout installed and every bus event
    /// pumped (so the pump holds the sub's footer entry).
    async fn world_with_sub(
        persistence: &ConversationPersistence,
    ) -> (SessionWorld, Arc<std::sync::Mutex<RunConfigSnapshot>>, Tui) {
        let run_config = scripted_run_config(vec![
            agent_tool_call_message("look into it"),
            finalized_text_message("sub report"),
            finalized_text_message("parent done"),
        ]);
        let mut world =
            build_test_world(persistence, &run_config, &create_spec()).expect("create world");
        drive_turn(&world, "delegate").await;
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &ThemeHandle::new(Theme::bundled_dark()), true);
        while let Ok(event) = world.event_rx.try_recv() {
            world.pump.handle(&mut tui, &event);
        }
        (world, run_config, tui)
    }

    /// Mount a thinking selector with a pre-filled outcome and poll
    /// it through [`handle_selector_outcome`] for `target`.
    async fn confirm_thinking(
        tui: &mut Tui,
        world: &mut SessionWorld,
        run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
        target: AgentId,
        level: Option<ThinkingConfig>,
    ) -> SelectorPollOutcome {
        let theme = ThemeHandle::new(Theme::bundled_dark());
        let inner = ThinkingSelectorComponent::new(select_list_theme(&theme), None);
        let outcome = inner.outcome_handle();
        let handle = tui.show_overlay(Box::new(inner), palette_overlay_options());
        *outcome.lock().expect("outcome poisoned") =
            Some(ThinkingSelectorOutcome::Confirmed(level));
        let dir = TempDir::new().expect("tempdir");
        let auth = AuthStorage::new(dir.path().join("auth.json"));
        handle_selector_outcome(
            tui,
            OpenSelector::Thinking {
                handle,
                outcome,
                target,
                parent_palette: None,
            },
            &auth,
            Arc::clone(run_config),
            Arc::new(std::sync::Mutex::new(Config::default())),
            &[],
            world,
            &theme,
            &RenderSettings::new(false, false, true),
            &mut ThemeWatch {
                _guard: None,
                rx: None,
            },
        )
        .await
    }

    /// Read the settings the sub-agent's log thread folds to.
    async fn sub_thread_settings(
        log: &Arc<TokioMutex<ConversationLog>>,
        n: usize,
    ) -> aj_session::SessionSettings {
        let log = log.lock().await;
        let filter = ThreadFilter::subagent(n);
        let head = log.latest_leaf(filter).expect("sub thread has a leaf");
        log.linearize(&head, filter).settings()
    }

    /// Confirming a thinking pick while targeting a live sub-agent
    /// stages an override, records the change on the sub's log
    /// thread, refreshes the sub's footer entry, and leaves the run
    /// config alone.
    #[tokio::test]
    async fn thinking_confirm_for_sub_stages_override_and_logs_on_sub_thread() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let (mut world, run_config, mut tui) = world_with_sub(&persistence).await;

        let outcome = confirm_thinking(
            &mut tui,
            &mut world,
            &run_config,
            AgentId::Sub(1),
            Some(ThinkingConfig::High),
        )
        .await;

        match outcome {
            SelectorPollOutcome::Closed { notice, .. } => assert_eq!(
                notice.as_deref(),
                Some("Thinking effort set to high for agent 1.")
            ),
            _ => panic!("expected the selector to close"),
        }
        {
            let overrides = world.sub_overrides.lock().expect("overrides poisoned");
            assert_eq!(
                overrides.get(&1).and_then(|o| o.thinking.clone()),
                Some(Some(ThinkingConfig::High)),
                "override staged for sub 1"
            );
        }
        assert_eq!(
            world
                .pump
                .agent_settings(AgentId::Sub(1))
                .map(|s| s.thinking.clone()),
            Some("high".to_string()),
            "footer entry updated"
        );
        assert_eq!(
            sub_thread_settings(&world.log, 1).await.thinking.as_deref(),
            Some("high"),
            "change recorded on the sub thread"
        );
        let cfg = run_config.lock().expect("run config mutex poisoned");
        assert_eq!(cfg.thinking, None, "run config untouched");
    }

    /// A non-promptable target (no live registry entry) yields the
    /// can't-be-prompted notice and stages nothing.
    #[tokio::test]
    async fn thinking_confirm_for_unpromptable_sub_stages_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let (mut world, run_config, mut tui) = world_with_sub(&persistence).await;

        let outcome = confirm_thinking(
            &mut tui,
            &mut world,
            &run_config,
            AgentId::Sub(99),
            Some(ThinkingConfig::High),
        )
        .await;

        match outcome {
            SelectorPollOutcome::Closed { notice, .. } => {
                assert_eq!(notice.as_deref(), Some("This agent can't be prompted."));
            }
            _ => panic!("expected the selector to close"),
        }
        assert!(
            world
                .sub_overrides
                .lock()
                .expect("overrides poisoned")
                .is_empty(),
            "nothing staged"
        );
        let cfg = run_config.lock().expect("run config mutex poisoned");
        assert_eq!(cfg.thinking, None, "run config untouched");
    }

    /// Confirming a model pick while targeting a live sub-agent stages a
    /// bundle override keyed to the picked model, records the change
    /// on the sub's log thread, and refreshes the sub's footer entry
    /// (preserving its thinking string) without touching the run
    /// config.
    #[tokio::test]
    async fn model_confirm_for_sub_stages_bundle_and_logs_on_sub_thread() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let (mut world, run_config, mut tui) = world_with_sub(&persistence).await;

        // A pickable catalog entry whose api has a registered
        // provider; key resolution is lazy, so no credentials are
        // needed to build the bundle.
        let info = ModelInfo {
            id: "claude-x".to_string(),
            name: "claude-x".to_string(),
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://example.invalid".to_string(),
            context_window: 1_000,
            ..scripted_model_info()
        };
        let theme = ThemeHandle::new(Theme::bundled_dark());
        let auth = AuthStorage::new(dir.path().join("auth.json"));
        use crate::modes::interactive::components::model_selector::ModelSelectorComponent;
        let inner =
            ModelSelectorComponent::new(select_list_theme(&theme), vec![info.clone()], None, None);
        let outcome = inner.outcome_handle();
        let handle = tui.show_overlay(Box::new(inner), palette_overlay_options());
        *outcome.lock().expect("outcome poisoned") =
            Some(ModelSelectorOutcome::Confirmed(info.clone()));

        let result = handle_selector_outcome(
            &mut tui,
            OpenSelector::Model {
                handle,
                outcome,
                target: AgentId::Sub(1),
                parent_palette: None,
            },
            &auth,
            Arc::clone(&run_config),
            Arc::new(std::sync::Mutex::new(Config::default())),
            &[],
            &mut world,
            &theme,
            &RenderSettings::new(false, false, true),
            &mut ThemeWatch {
                _guard: None,
                rx: None,
            },
        )
        .await;

        match result {
            SelectorPollOutcome::Closed { notice, .. } => assert_eq!(
                notice.as_deref(),
                Some("Model set to claude-x (anthropic/claude-x) for agent 1.")
            ),
            _ => panic!("expected the selector to close"),
        }
        {
            let overrides = world.sub_overrides.lock().expect("overrides poisoned");
            let bundle = overrides
                .get(&1)
                .and_then(|o| o.bundle.as_ref())
                .expect("bundle staged for sub 1");
            assert_eq!(
                bundle.3,
                ("anthropic".to_string(), "claude-x".to_string()),
                "staged bundle carries the picked key"
            );
        }
        let settings = world
            .pump
            .agent_settings(AgentId::Sub(1))
            .cloned()
            .expect("footer entry present");
        assert_eq!(settings.model_id, "claude-x");
        assert_eq!(settings.thinking, "off", "thinking string preserved");
        assert_eq!(
            sub_thread_settings(&world.log, 1).await.model,
            Some(("anthropic".to_string(), "claude-x".to_string())),
            "change recorded on the sub thread"
        );
        let cfg = run_config.lock().expect("run config mutex poisoned");
        assert_eq!(cfg.model_info.id, "scripted", "run config untouched");
    }

    // ---- Agent picker: background tasks ------------------------------------

    /// The picker's kill outcome routes through the task registry:
    /// the task's cancel token fires and the close notice names the
    /// task. (The driver, not the host, flips the status and emits
    /// `TaskEnd` — with no driver attached the status stays
    /// `Running` here, which is fine: we assert the cancellation.)
    #[tokio::test]
    async fn agent_picker_kill_outcome_cancels_the_registry_task() {
        struct NoOutput;
        impl aj_agent::tool::TaskOutputSource for NoOutput {
            fn snapshot(&self) -> aj_agent::tool::TaskRead {
                aj_agent::tool::TaskRead::default()
            }
        }

        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("unused")]);
        let mut world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &ThemeHandle::new(Theme::bundled_dark()), true);

        let (task_id, cancel) = world.task_registry.register(
            AgentId::Main,
            aj_agent::tool::TaskKind::Bash {
                command: "sleep 5".into(),
            },
            "sleep 5".into(),
            Arc::new(NoOutput),
        );
        assert!(!cancel.is_cancelled());

        let theme = ThemeHandle::new(Theme::bundled_dark());
        let inner = AgentPickerComponent::new(
            select_list_theme(&theme),
            Vec::new(),
            Vec::new(),
            AgentId::Main,
        );
        let outcome = inner.outcome_handle();
        let handle = tui.show_overlay(Box::new(inner), palette_overlay_options());
        *outcome.lock().expect("outcome poisoned") = Some(AgentPickerOutcome::KillTask(task_id));

        let auth = AuthStorage::new(dir.path().join("auth.json"));
        let result = handle_selector_outcome(
            &mut tui,
            OpenSelector::AgentPicker {
                handle,
                outcome,
                parent_palette: None,
            },
            &auth,
            Arc::clone(&run_config),
            Arc::new(std::sync::Mutex::new(Config::default())),
            &[],
            &mut world,
            &theme,
            &RenderSettings::new(false, false, true),
            &mut ThemeWatch {
                _guard: None,
                rx: None,
            },
        )
        .await;

        assert!(cancel.is_cancelled(), "kill cancels the task's token");
        match result {
            SelectorPollOutcome::Closed { notice, .. } => {
                assert_eq!(notice, Some(format!("Killing background task #{task_id}.")))
            }
            _ => panic!("expected the selector to close"),
        }
    }

    // ---- Wake triggers ----------------------------------------------------

    fn bash_notice(owner: AgentId, task_id: usize, body: &str) -> aj_agent::tool::TaskNotice {
        aj_agent::tool::TaskNotice {
            owner,
            task_id,
            kind: aj_agent::tool::TaskKind::Bash {
                command: "cargo build".to_string(),
            },
            label: "cargo build".to_string(),
            status: aj_agent::tool::TaskStatus::Exited(Some(0)),
            body: body.to_string(),
        }
    }

    /// A default [`TurnPolicy`] for the spawn-helper tests: queued-work
    /// delivery on, compaction off (these tests don't drive the overflow
    /// or threshold paths).
    fn test_policy() -> TurnPolicy {
        TurnPolicy {
            recover_overflow: false,
            auto_threshold: None,
            keep_recent: 20_000,
        }
    }

    /// An idle owner with a queued notice gets a wake turn through
    /// the normal per-agent machinery: gated via `turn_cancels`,
    /// spawned on the `turns` JoinSet, and the wake drains the notice
    /// into the transcript before the scripted reply.
    #[tokio::test]
    async fn spawn_wake_turn_wakes_idle_owner() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("woke and reacted")]);
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        world
            .task_registry
            .push_notice(bash_notice(AgentId::Main, 1, "task #1 done"));

        let mut turns: JoinSet<(AgentId, Result<(), TurnError>)> = JoinSet::new();
        let mut turn_cancels: HashMap<AgentId, CancellationToken> = HashMap::new();
        spawn_wake_turn(
            AgentId::Main,
            &world,
            &run_config,
            test_policy(),
            &mut turns,
            &mut turn_cancels,
        );

        assert!(
            turn_cancels.contains_key(&AgentId::Main),
            "wake turn registered in turn_cancels"
        );
        let (id, result) = turns
            .join_next()
            .await
            .expect("one wake turn spawned")
            .expect("wake turn did not panic");
        assert_eq!(id, AgentId::Main);
        result.expect("wake turn succeeds");

        let agent = world.agent.lock().await;
        let transcript = format!("{:?}", agent.messages());
        assert!(
            transcript.contains("<task-notification>\\ntask #1 done\\n</task-notification>"),
            "notice drained into the transcript: {transcript}"
        );
        assert!(
            transcript.contains("woke and reacted"),
            "wake turn ran inference: {transcript}"
        );
        assert!(!world.task_registry.has_notices(AgentId::Main));
    }

    /// A busy owner (already in `turn_cancels`) is left alone — the
    /// in-flight turn's drain points or the turn-completion trigger
    /// deliver the notice instead.
    #[tokio::test]
    async fn spawn_wake_turn_skips_busy_owner() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(Vec::new());
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        world
            .task_registry
            .push_notice(bash_notice(AgentId::Main, 1, "task #1 done"));

        let mut turns: JoinSet<(AgentId, Result<(), TurnError>)> = JoinSet::new();
        let mut turn_cancels: HashMap<AgentId, CancellationToken> = HashMap::new();
        turn_cancels.insert(AgentId::Main, CancellationToken::new());

        spawn_wake_turn(
            AgentId::Main,
            &world,
            &run_config,
            test_policy(),
            &mut turns,
            &mut turn_cancels,
        );
        assert!(turns.is_empty(), "busy owner must not get a wake turn");
        assert!(
            world.task_registry.has_notices(AgentId::Main),
            "notice stays queued for the busy owner's next drain point"
        );
    }

    /// Racing triggers are safe: a wake spawned after the queue was
    /// already drained resolves as `WakeOutcome::Empty` — no
    /// inference, no transcript change (the strict-mode provider
    /// would panic on an unscripted inference).
    #[tokio::test]
    async fn spawn_wake_turn_with_empty_queue_is_a_noop() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(Vec::new());
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");

        let mut turns: JoinSet<(AgentId, Result<(), TurnError>)> = JoinSet::new();
        let mut turn_cancels: HashMap<AgentId, CancellationToken> = HashMap::new();
        spawn_wake_turn(
            AgentId::Main,
            &world,
            &run_config,
            test_policy(),
            &mut turns,
            &mut turn_cancels,
        );

        let (id, result) = turns
            .join_next()
            .await
            .expect("wake turn spawned")
            .expect("wake turn did not panic");
        assert_eq!(id, AgentId::Main);
        result.expect("empty wake succeeds");
        let agent = world.agent.lock().await;
        assert!(agent.messages().is_empty(), "no-op wake leaves no trace");
    }

    // ---- message queues: submit routing, yank, wake delivery -------------

    /// `spawn_prompt_turn` for an idle, promptable target clears the
    /// editor, registers the turn in `turn_cancels`, and runs the
    /// prompt against the agent.
    #[tokio::test]
    async fn spawn_prompt_turn_starts_a_turn_and_clears_editor() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("hi back")]);
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &ThemeHandle::new(Theme::bundled_dark()), true);
        if let Some(e) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
            e.set_text("draft text");
        }

        let mut turns: JoinSet<(AgentId, Result<(), TurnError>)> = JoinSet::new();
        let mut turn_cancels: HashMap<AgentId, CancellationToken> = HashMap::new();
        let spawned = spawn_prompt_turn(
            &mut tui,
            &world,
            &run_config,
            AgentId::Main,
            "do the thing".to_string(),
            test_policy(),
            &mut turns,
            &mut turn_cancels,
        );
        assert!(spawned);
        assert!(turn_cancels.contains_key(&AgentId::Main));
        let editor_text = tui
            .get_mut_as::<Editor>(SlotIndex::Editor.idx())
            .map(|e| e.get_text())
            .unwrap();
        assert!(editor_text.is_empty(), "editor cleared on spawn");

        let (id, result) = turns
            .join_next()
            .await
            .expect("one turn spawned")
            .expect("turn did not panic");
        assert_eq!(id, AgentId::Main);
        result.expect("turn succeeds");
        let agent = world.agent.lock().await;
        assert!(format!("{:?}", agent.messages()).contains("do the thing"));
    }

    /// `spawn_prompt_turn` for a non-promptable target (a sub-agent
    /// with no live handle) spawns nothing and leaves the editor
    /// intact so the caller can surface a notice without losing the
    /// user's text.
    #[tokio::test]
    async fn spawn_prompt_turn_declines_unpromptable_target() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(Vec::new());
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &ThemeHandle::new(Theme::bundled_dark()), true);
        if let Some(e) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
            e.set_text("keep me");
        }

        let mut turns: JoinSet<(AgentId, Result<(), TurnError>)> = JoinSet::new();
        let mut turn_cancels: HashMap<AgentId, CancellationToken> = HashMap::new();
        let spawned = spawn_prompt_turn(
            &mut tui,
            &world,
            &run_config,
            AgentId::Sub(99),
            "x".to_string(),
            test_policy(),
            &mut turns,
            &mut turn_cancels,
        );
        assert!(!spawned);
        assert!(turns.is_empty());
        assert!(turn_cancels.is_empty());
        let editor_text = tui
            .get_mut_as::<Editor>(SlotIndex::Editor.idx())
            .map(|e| e.get_text())
            .unwrap();
        assert_eq!(editor_text, "keep me", "editor untouched on decline");
    }

    /// `yank_pending_into_editor` moves the queued message into the
    /// editor and empties the queue; with nothing pending it is a
    /// no-op returning `false`.
    #[tokio::test]
    async fn yank_pending_into_editor_restores_and_empties() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(Vec::new());
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &ThemeHandle::new(Theme::bundled_dark()), true);

        world
            .message_queues
            .append_follow_up(AgentId::Main, "queued line");
        let yanked =
            yank_pending_into_editor(&mut tui, &world.pump, &world.message_queues, AgentId::Main);
        assert!(yanked);
        let editor_text = tui
            .get_mut_as::<Editor>(SlotIndex::Editor.idx())
            .map(|e| e.get_text())
            .unwrap();
        assert_eq!(editor_text, "queued line");
        assert!(!world.message_queues.has_pending(AgentId::Main));

        assert!(
            !yank_pending_into_editor(&mut tui, &world.pump, &world.message_queues, AgentId::Main),
            "nothing pending → false"
        );
    }

    /// A finished turn with a pending follow-up is delivered by the
    /// wake path: `spawn_wake_turn` runs it as a fresh turn whose user
    /// message is the queued text, and the queue ends empty. (No task
    /// notice is queued — the follow-up alone opens the wake gate.)
    #[tokio::test]
    async fn spawn_wake_turn_delivers_queued_follow_up() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("on it")]);
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        world
            .message_queues
            .append_follow_up(AgentId::Main, "then tidy up");

        let mut turns: JoinSet<(AgentId, Result<(), TurnError>)> = JoinSet::new();
        let mut turn_cancels: HashMap<AgentId, CancellationToken> = HashMap::new();
        spawn_wake_turn(
            AgentId::Main,
            &world,
            &run_config,
            test_policy(),
            &mut turns,
            &mut turn_cancels,
        );
        assert!(
            turn_cancels.contains_key(&AgentId::Main),
            "follow-up alone opens the wake gate"
        );
        let (id, result) = turns
            .join_next()
            .await
            .expect("one wake turn spawned")
            .expect("wake turn did not panic");
        assert_eq!(id, AgentId::Main);
        result.expect("wake turn succeeds");

        let agent = world.agent.lock().await;
        let transcript = format!("{:?}", agent.messages());
        assert!(
            transcript.contains("then tidy up"),
            "follow-up delivered: {transcript}"
        );
        assert!(!world.message_queues.has_pending(AgentId::Main));
    }
}
