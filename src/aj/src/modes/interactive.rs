//! Interactive TUI mode.
//!
//! The interactive mode owns:
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
pub mod layout;
pub mod render_settings;
pub mod session;
pub mod shutdown;
#[cfg(test)]
pub(crate) mod test_support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use aj_agent::TurnError;
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::queue::MessageQueues;
use aj_agent::types::UsageSummary;
use aj_app::settings::{
    ConfigLayers, ConfigTarget, FooterUpdate, MainConfirm, PersistAction, SpeedConfirm, SubConfirm,
    persist_setting, persist_user,
};
use aj_conf::{
    Config, ConfigSpeed, ConfigThinkingDisplay, ConfigVerbosity, Severity, display_path,
};
use aj_models::auth::AuthStorage;
use aj_models::registry::ModelInfo;
use aj_models::types::{Speed, UserContent};
use aj_models::{
    ThinkingConfig, speed_from_name, speed_name, thinking_config_from_name, verbosity_name,
};
use aj_session::{ConversationPersistence, ThreadFilter};
use aj_tools::{BuiltinToolOptions, get_builtin_tools};
use aj_tui::EditorComponent;
use aj_tui::components::editor::Editor;
use aj_tui::terminal::ProcessTerminal;
use aj_tui::tui::{OverlayAnchor, OverlayHandle, OverlayOptions, SizeValue, Tui, TuiEvent};
use anyhow::{Context, Result};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::auth::LoginLine;
use crate::cli::args::{Args, Command};
use crate::config::commands::{CommandAction, load_model_catalog, thinking_level_name};
use crate::config::theme::{
    Theme, ThemeHandle, ThemeWatcherGuard, editor_border_color_for_thinking, select_list_theme,
    settings_list_theme, watch_user_theme,
};
use crate::modes::interactive::components::agent_picker::{
    AgentPickerComponent, AgentPickerOutcome, AgentPickerOutcomeHandle,
};
use crate::modes::interactive::components::auth_picker::{
    AuthPickerComponent, AuthProviderItem, OutcomeHandle as AuthPickerOutcomeHandle,
};
use crate::modes::interactive::components::auth_status::AuthStatusOutcomeHandle;
use crate::modes::interactive::components::command_palette::CommandPaletteOutcomeHandle;
use crate::modes::interactive::components::footer::Footer;
use crate::modes::interactive::components::login_dialog::{
    LoginDialogComponent, LoginDialogState, TuiOAuthCallbacks,
};
use crate::modes::interactive::components::model_selector::{
    ModelIdentityRef, ModelSelectorComponent, ModelSelectorOutcome,
    OutcomeHandle as ModelOutcomeHandle,
};
use crate::modes::interactive::components::prompt_history::{
    PromptHistoryOutcome, PromptHistoryOutcomeHandle, PromptHistorySearchComponent,
    all_workspaces_history_streaming, workspace_history_streaming,
};
use crate::modes::interactive::components::session_info::SessionInfoOutcomeHandle;
use crate::modes::interactive::components::session_selector::{
    OutcomeHandle as SessionOutcomeHandle, SessionSelectorComponent, SessionSelectorOutcome,
};
use crate::modes::interactive::components::settings_window::{
    ChangesHandle as SettingsChangesHandle, ClearsHandle as SettingsClearsHandle,
    CorrectionsHandle as SettingsCorrectionsHandle, MODEL_SETTING_ID,
    OutcomeHandle as SettingsOutcomeHandle, SettingsCurrentValues, SettingsSubmenu,
    SettingsWindowComponent, SettingsWindowOutcome, UNSET_VALUE,
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
    UsageActionDeps, UsageStatusComponent, UsageStatusOutcomeHandle,
};
use crate::modes::interactive::editor_ext::{DEFAULT_MAX_ENTRIES, PromptHistory};
use crate::modes::interactive::event_pump::{
    EventPump, set_editor_submit_enabled, take_submitted_prompt,
};
use crate::modes::interactive::layout::{SlotIndex, build_layout};
use crate::modes::interactive::render_settings::RenderSettings;
use crate::modes::interactive::session::{
    SessionCore, SessionEntry, SessionExit, SessionRequest, SessionSpec, SessionWorld,
};
use crate::modes::interactive::shutdown::{
    print_resume_hint, print_session_usage, print_usage_summary,
};
use crate::session_setup::{RestoreContext, RunConfigSnapshot, build_initial_run_config};
use crate::turn::{TurnPolicy, TurnStart, Turns, running_work_counts};

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
        // Load the user config (`~/.aj/config.toml`), then the
        // per-project overlay (`<git-root>/.aj/config.toml`). The
        // running session reads `config`, the effective merge of the
        // two; the settings windows edit one layer each. CLI flags and
        // env vars still overlay on top of `config` downstream, so
        // precedence stays CLI > env > project > user > defaults.
        let (user_config, user_diagnostics) = Config::load();
        let (project_layer, project_diagnostics) = Config::load_project();
        let project_config_path = Config::project_config_file_path();
        let mut config_diagnostics = user_diagnostics;
        config_diagnostics.extend(project_diagnostics);
        let config = project_layer.overlay_onto(&user_config);

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

        // Credential store backing API-key resolution and the login /
        // logout / auth-status overlays. Cheap to clone (`Arc`-backed);
        // the resolver installed in `crate::model::from_model_info`
        // captures a clone and reads it on every inference, so a
        // mid-session login takes effect without a restart.
        let auth = AuthStorage::at_default_path().context("failed to open ~/.aj/auth.json")?;

        // Resolve the initial run config (provider / model / thinking /
        // speed, merged CLI > env > config) plus the resume-time
        // `RestoreContext` the registry path needs; scripted mode skips
        // restoration. `run_config` is the loop-side snapshot of what
        // the next turn runs against: the selectors mutate it without
        // locking the agent, and the submit handler copies it into the
        // agent just before each turn. See [`RunConfigSnapshot`].
        let (run_config, restore_context) =
            build_initial_run_config(&self.args, &config, &auth, speed)?;
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
                head: None,
            },
            Some(Command::Continue {
                session_id: None,
                prompt: _,
            }) => match conversation_persistence.get_latest_session_id()? {
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
        let working_directory = world.core.env.working_directory.clone();
        if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
            let provider =
                aj_tui::autocomplete::CombinedAutocompleteProvider::new(working_directory);
            editor.set_autocomplete_provider(Arc::new(provider));
        }

        // Bootstrap the editor's prompt-history ring from the
        // project's `*.jsonl` session logs so pressing Up surfaces
        // cross-session prompts the user has ever submitted here. The
        // scan runs on a background thread (see
        // [`spawn_prompt_history_bootstrap`]) and the result is
        // installed by the session loop's select arm once it lands, so
        // a large session backlog never delays first paint. Live
        // submissions update the same ring through
        // [`aj_tui::components::editor::Editor::add_to_history`] in the
        // submit branch below. The backgrounded seed lands *beneath*
        // anything typed in the meantime (see
        // [`aj_tui::components::editor::Editor::seed_history`]). No
        // persistence layer is involved. The conversation log files
        // are the source of truth, so two `aj` processes running side
        // by side can't clobber each other's history.
        let mut prompt_history_rx = Some(spawn_prompt_history_bootstrap(
            conversation_persistence.clone(),
            DEFAULT_MAX_ENTRIES,
        ));

        // Shared flag tripped by the editor's `/`-at-empty-prompt
        // callback and by the global `Ctrl+O` chord. The main loop
        // polls it after `tui.handle_input` and runs
        // [`CommandAction::OpenCommandPalette`], so all palette-open
        // paths (leading `/`, `Ctrl+O`) converge on the same mounting
        // code.
        let palette_open_request: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        // Set when the user hits `aj.overlay.close_all` (default
        // `ctrl+c`) while any overlay is up. Drained after
        // `tui.handle_input` to tear down the whole selector stack,
        // distinct from Esc's one-level pop.
        let close_all_request: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        // Set by the global `aj.history.open` chord (default
        // `ctrl+r`). Drained after `tui.handle_input` to run
        // [`CommandAction::OpenPromptHistory`], mirroring the
        // `palette_open_request` path. Opens as a top-level overlay
        // so `Esc` closes it back to the editor.
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
        let footer_cwd = format!("{}", world.core.env.working_directory.display());
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
            world
                .pump
                .handle(&mut world.core.lifecycle, &mut tui, &event);
        }
        // The context notice only applies to fresh sessions: a
        // resumed session keeps the assembled prompt persisted in
        // its log, so the freshly-loaded env the notice describes
        // doesn't govern what's actually sent. Skill-discovery
        // warnings ride along under the same rule.
        if matches!(spec, SessionSpec::Create { .. }) {
            let env = &world.core.env;
            let skill_warnings: Vec<String> = env
                .skill_diagnostics
                .iter()
                .map(|d| d.to_string())
                .collect();
            let context_notice =
                aj_app::notices::build_context_notice(env, aj_tui::style::strikethrough);
            world.pump.handle(
                &mut world.core.lifecycle,
                &mut tui,
                &notice_event(&context_notice),
            );
            for warning in &skill_warnings {
                world
                    .pump
                    .handle(&mut world.core.lifecycle, &mut tui, &warning_event(warning));
            }
        }
        if aj_app::notices::sandbox_warning_enabled() {
            world.pump.handle(
                &mut world.core.lifecycle,
                &mut tui,
                &warning_event(aj_app::notices::SANDBOX_WARNING),
            );
        }
        if let Some(warning) = &startup_auth_warning {
            world
                .pump
                .handle(&mut world.core.lifecycle, &mut tui, &warning_event(warning));
        }
        if let Some(warning) = &tmux_warning {
            world
                .pump
                .handle(&mut world.core.lifecycle, &mut tui, &warning_event(warning));
        }
        // Settings restored from a resumed session's log (or the
        // reasons restoration fell back) surface like any other
        // startup notice.
        for notice in std::mem::take(&mut world.core.restore_notices) {
            world
                .pump
                .handle(&mut world.core.lifecycle, &mut tui, &notice_event(&notice));
        }

        // Shared, mutable view of the on-disk config. Selector
        // outcomes (model / thinking / the settings window) mutate
        // this and persist it via `persist_user` so a choice made
        // in the TUI survives a restart. Held behind a std mutex
        // because the write is a quick synchronous read-merge-write
        // (`Config::persist_changed`) done off the guard, never awaited
        // across.
        let config = Arc::new(std::sync::Mutex::new(config));

        // The editable config layers behind the effective `config`
        // above. The settings windows mutate one layer, recompute the
        // effective config into `config`, and persist that layer's
        // file. Held behind a std mutex like `config` (the write is a
        // quick synchronous read-merge-write done off the guard).
        let config_layers = Arc::new(std::sync::Mutex::new(ConfigLayers {
            user: user_config,
            project: project_layer,
            project_path: project_config_path,
        }));

        // Everything with process lifetime moves into the shell;
        // session worlds are rebuilt around it on every new-session /
        // resume.
        let mut shell = Shell {
            tui,
            theme,
            config,
            config_layers,
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
                &mut prompt_history_rx,
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
                    head: None,
                },
                // The `aj` frontend does not implement branching (it is an
                // `aj-next`-only gesture), so its session loop never emits
                // `Branch`. The variant is shared through `aj_app`, hence this
                // arm exists only to keep the match total.
                Ok(SessionExit::Branch { .. }) => {
                    unreachable!("aj never emits SessionExit::Branch")
                }
            };

            // Snapshot the outgoing world's usage for the shutdown
            // banner before it is dropped. The replacement world's
            // usage starts at zero, so nothing is double-counted —
            // including on the fallback path below, which resumes the
            // same session in a brand-new world.
            let usage = world.usage_summary().await;
            shell
                .completed_sessions
                .push((world.core.session_id.clone(), usage));
            let previous_id = world.core.session_id.clone();

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
                        next.world.pump.handle(
                            &mut next.world.core.lifecycle,
                            &mut shell.tui,
                            &notice_event(notice),
                        );
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
                    print_session_usage(&world.core.session_id, &summary);
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
                    let l = world.core.log.lock().await;
                    l.latest_leaf(ThreadFilter::USER).is_some()
                };
                if resume_eligible {
                    print_resume_hint(&world.core.session_id);
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
    /// The editable config layers (user + project) behind the
    /// effective [`Self::config`]. The settings windows edit one layer
    /// and recompute `config`; persistence targets that layer's file.
    config_layers: Arc<std::sync::Mutex<ConfigLayers>>,
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
                    format!("Started a fresh session ({}).", world.core.session_id)
                }
                SessionSpec::Resume { session_id, .. } => {
                    format!("Switched to session {session_id}.")
                }
            };
            let mut notices = vec![notice];
            notices.append(&mut world.core.restore_notices);
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
                head: None,
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
            notices.append(&mut world.core.restore_notices);
            Ok(NextWorld {
                world,
                spec: fallback,
                notices,
            })
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
    prompt_history_rx: &mut Option<UnboundedReceiver<PromptHistory>>,
    launch_content: Vec<UserContent>,
) -> Result<SessionExit> {
    // ---- Main event loop ------------------------------------------
    // In-flight turns keyed by the agent running them. The `Turns`
    // JoinSet gives completion-as-they-finish and preserves panic
    // detection (`join_next` yields `Err(JoinError)`); its cancel map
    // holds the binary's clone of each turn's cancel token, and its
    // key set is exactly "agents the binary is currently driving".
    let mut turns = Turns::new();
    // Implements the "press Ctrl+C again to quit" guard when the
    // viewed agent is idle but other agents or background tasks are
    // still running.
    let mut quit_armed = false;

    // A command like the thinking selector opens an overlay
    // selector. While an overlay is up the editor is not focused, but
    // `shell.tui.show_overlay` already routes input to the overlay,
    // so the main loop's job is just to poll the top of the stack
    // after every input event and move the stack on the result.
    // Nesting (a child opened over the palette, the task viewer over
    // the picker) is the depth of this stack. It usually holds one.
    let mut selectors = SelectorStack::default();

    // An in-flight OAuth login: the dialog overlay + a cancel flag
    // the dialog (Esc) and Ctrl+C set, plus the spawned login task
    // whose `JoinHandle` we poll alongside the agent turn. Kept
    // separate from `selectors` because the flow is async and
    // long-running rather than a synchronous confirm/cancel
    // selector.
    let mut login_session: Option<LoginSession> = None;
    let mut login_task: Option<tokio::task::JoinHandle<Result<(), aj_models::auth::AuthError>>> =
        None;

    // Auto-submit the launch prompt (`aj <msg>` / `aj @file ...`) as the
    // first turn. Empty for any in-process session switch after the first.
    if !launch_content.is_empty() {
        turns.spawn(
            &world.core,
            &shell.run_config,
            AgentId::Main,
            TurnStart::Content(launch_content),
            crate::turn::turn_policy(AgentId::Main, &shell.config),
        );
        sync_editor_enabled(&mut shell.tui);
    }

    let exit: Result<SessionExit> = loop {
        tokio::select! {
            biased;

            // --- Agent run finished ---
            joined = turns.join_next() => {
                match joined {
                    Ok((id, result)) => {
                        for idle in turns.reap(&mut world.core.lifecycle, &world.core.task_registry, id) {
                            world.pump.note_idle(&world.core.lifecycle, &mut shell.tui, idle);
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
                        if world.core.task_registry.has_notices(id)
                            || world.core.message_queues.has_pending(id)
                        {
                            turns.spawn_wake(
                                id,
                                &world.core,
                                &shell.run_config,
                                crate::turn::turn_policy(id, &shell.config),
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
                                world.pump.handle(&mut world.core.lifecycle, &mut shell.tui, &notice_event("Turn cancelled."));
                            }
                            Err(TurnError::Recoverable(_)) => {
                                // A recoverable failure already rendered
                                // in transcript order from the turn's
                                // terminal `MessageEnd`
                                // (`AssistantMessage.error`). For an
                                // overflow give-up the driver also emitted
                                // its guidance on the bus. Re-rendering the
                                // error here would float it above events
                                // still buffered in the event channel, so
                                // we only keep the session alive and let
                                // the in-band error stand.
                            }
                            Err(TurnError::Fatal(err)) => {
                                break Err(anyhow::Error::msg(err));
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
                            world.pump.handle(&mut world.core.lifecycle,
                                &mut shell.tui,
                                &notice_event(&format!("Logged in to {name}.")),
                            );
                        }
                        Ok(Err(err)) => {
                            world.pump.handle(&mut world.core.lifecycle,
                                &mut shell.tui,
                                &warning_event(&format!("Login to {name} failed: {err}")),
                            );
                        }
                        // Aborted on Ctrl+C / Esc: the cancel-poll
                        // arm already surfaced a "cancelled" notice.
                        Err(join_err) if join_err.is_cancelled() => {}
                        Err(join_err) => {
                            world.pump.handle(&mut world.core.lifecycle,
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
                    // Terminal stream ended: treat it as a quit.
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
                        // 1. Overlay up (`selectors` is
                        //    non-empty): dismiss the overlay. Don't
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
                        //    *viewing*:
                        //    - Viewed agent has a binary-driven
                        //      turn (in the cancel map): cancel just
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
                        //      the cancel map): cancel the main
                        //      turn that owns it; the child token
                        //      cascades.
                        //    - Viewed agent idle but other agents
                        //      or background tasks still run:
                        //      don't cancel them; arm "press
                        //      Ctrl+C again to quit" and exit on
                        //      the second press.
                        //    - Nothing running anywhere: exit.
                        //
                        // The terminal is in raw mode, so Ctrl+C
                        // doesn't raise SIGINT. It arrives here as
                        // an ordinary key event.

                        // Any non-Ctrl+C key disarms a pending
                        // "press again to quit".
                        if !input.is_ctrl('c') {
                            quit_armed = false;
                        }
                        if input.is_ctrl('c') {
                            if !selectors.is_empty() {
                                // Overlay up: fall through to the
                                // close-all interception below.
                            } else if let Some(session) = login_session.as_ref() {
                                session.cancel.store(true, Ordering::Relaxed);
                                continue;
                            } else {
                                // Per-view Ctrl+C: act on the agent you're viewing.
                                let active = world.pump.active_view(&mut shell.tui);
                                if turns.cancel(active) {
                                    // Viewed agent has a binary-driven turn: cancel just it.
                                    // Don't discard what the user lined
                                    // up: pull any queued message back
                                    // into the editor.
                                    yank_pending_into_editor(
                                        &mut shell.tui,
                                        &world.pump,
                                        &world.core.message_queues,
                                        active,
                                    );
                                    quit_armed = false;
                                    continue;
                                } else if world.core.is_running(active) {
                                    // Viewed agent is a sub running its initial spawn, owned by
                                    // the main turn: cancel the main turn (the child token
                                    // cascades to the sub).
                                    turns.cancel(AgentId::Main);
                                    yank_pending_into_editor(
                                        &mut shell.tui,
                                        &world.pump,
                                        &world.core.message_queues,
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
                                    turns.driven(),
                                    &world.core.task_registry.snapshot(),
                                );
                                if agents + tasks > 0 {
                                    if quit_armed {
                                        break Ok(SessionExit::Quit);
                                    }
                                    quit_armed = true;
                                    world.pump.handle(&mut world.core.lifecycle,
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
                        // Toggle the thinking-block render mode for
                        // the session. Bound via `aj.thinking.toggle`
                        // (default `alt+t`); intercepted before
                        // `shell.tui.handle_input` so the editor never sees
                        // the keystroke.
                        {
                            let kb = aj_tui::keybindings::get();
                            if kb.matches(
                                &input,
                                crate::config::keybindings::ACTION_THINKING_TOGGLE,
                            ) {
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
                            if matched && selectors.is_empty() && login_session.is_none() {
                                let target = world.pump.active_view(&mut shell.tui);
                                let text = shell
                                    .tui
                                    .get_mut_as::<Editor>(SlotIndex::Editor.idx())
                                    .map(|e| e.get_expanded_text().trim().to_string())
                                    .unwrap_or_default();
                                let busy = turns.is_busy(&world.core.lifecycle, target);
                                if busy {
                                    if text.is_empty() {
                                        world.core.message_queues.promote(target);
                                    } else {
                                        world.core.message_queues.append_steering(target, &text);
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
                                        &world.core,
                                        &shell.run_config,
                                        target,
                                        text,
                                        crate::turn::turn_policy(target, &shell.config),
                                        &mut turns,
                                    ) {
                                        sync_editor_enabled(&mut shell.tui);
                                    } else {
                                        world.pump.handle(&mut world.core.lifecycle,
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
                            if matched && selectors.is_empty() && login_session.is_none() {
                                let target = world.pump.active_view(&mut shell.tui);
                                yank_pending_into_editor(
                                    &mut shell.tui,
                                    &world.pump,
                                    &world.core.message_queues,
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
                            if is_up && selectors.is_empty() && login_session.is_none() {
                                let target = world.pump.active_view(&mut shell.tui);
                                let editor_empty = shell
                                    .tui
                                    .get_mut_as::<Editor>(SlotIndex::Editor.idx())
                                    .map(|e| e.get_text().is_empty())
                                    .unwrap_or(false);
                                if editor_empty && world.core.message_queues.has_pending(target) {
                                    yank_pending_into_editor(
                                        &mut shell.tui,
                                        &world.pump,
                                        &world.core.message_queues,
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
                            if !selectors.is_empty()
                                && kb.matches(
                                    &input,
                                    crate::config::keybindings::ACTION_OVERLAY_CLOSE_ALL,
                                )
                            {
                                shell.close_all_request.store(true, Ordering::Relaxed);
                                // Consume: skip `shell.tui.handle_input`
                                // entirely so the selector doesn't
                                // also see Ctrl+C as a cancel and
                                // write a cancel-outcome that would
                                // then drive a stale one-level Back
                                // underneath our explicit teardown.
                                consume_event = true;
                            } else if selectors.is_empty()
                                && login_session.is_none()
                                && kb.matches(
                                    &input,
                                    crate::config::keybindings::ACTION_PALETTE_OPEN,
                                )
                            {
                                shell.palette_open_request.store(true, Ordering::Relaxed);
                                // Fall through to the dispatcher
                                // arm below by letting handle_input
                                // run (it's a no-op for this chord
                                // since no component binds ctrl+o).
                            } else if selectors.is_empty()
                                && login_session.is_none()
                                && kb.matches(
                                    &input,
                                    crate::config::keybindings::ACTION_HISTORY_OPEN,
                                )
                            {
                                shell.history_open_request.store(true, Ordering::Relaxed);
                                // Consume: the editor binds no
                                // ctrl+r, but skipping handle_input
                                // keeps the chord from reaching any
                                // future binding and matches the
                                // close-all interception style.
                                consume_event = true;
                            } else if selectors.is_empty()
                                && login_session.is_none()
                                && kb.matches(
                                    &input,
                                    crate::config::keybindings::ACTION_AGENT_PICKER,
                                )
                            {
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

                        // Close-all: tear the whole selector stack
                        // down in one shot, back to the chat editor.
                        // Done before the open dispatch and the
                        // per-tick poll so we never act on a
                        // half-unwound stack.
                        if shell.close_all_request.swap(false, Ordering::Relaxed) {
                            selectors.close_all(&mut shell.tui);
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
                            world.pump.handle(&mut world.core.lifecycle,
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
                        // handler. Gated on an empty selector stack so
                        // it's inert while another selector is up.
                        if shell.palette_open_request.swap(false, Ordering::Relaxed)
                            && selectors.is_empty()
                            && login_session.is_none()
                        {
                            match handle_command(
                                &mut shell.tui,
                                &shell.auth,
                                Arc::clone(&shell.model_catalog),
                                Arc::clone(&shell.run_config),
                                &shell.config,
                                &shell.config_layers,
                                &shell.render_settings,
                                world,
                                &shell.conversation_persistence,
                                &shell.theme,
                                CommandAction::OpenCommandPalette,
                                !turns.is_empty(),
                            ).await {
                                CommandOutcome::Continue { selector, notice } => {
                                    if let Some(text) = notice {
                                        world.pump.handle(&mut world.core.lifecycle, &mut shell.tui, &notice_event(&text));
                                    }
                                    if let Some(sel) = selector {
                                        selectors.push(&mut shell.tui, sel);
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
                        // [`CommandAction::OpenPromptHistory`] as a
                        // top-level overlay, so the overlay's `Esc`
                        // closes straight back to the editor. Gated on
                        // an empty selector stack so it's inert while
                        // another selector is up.
                        if shell.history_open_request.swap(false, Ordering::Relaxed)
                            && selectors.is_empty()
                            && login_session.is_none()
                        {
                            match handle_command(
                                &mut shell.tui,
                                &shell.auth,
                                Arc::clone(&shell.model_catalog),
                                Arc::clone(&shell.run_config),
                                &shell.config,
                                &shell.config_layers,
                                &shell.render_settings,
                                world,
                                &shell.conversation_persistence,
                                &shell.theme,
                                CommandAction::OpenPromptHistory,
                                !turns.is_empty(),
                            ).await {
                                CommandOutcome::Continue { selector, notice } => {
                                    if let Some(text) = notice {
                                        world.pump.handle(&mut world.core.lifecycle, &mut shell.tui, &notice_event(&text));
                                    }
                                    if let Some(sel) = selector {
                                        selectors.push(&mut shell.tui, sel);
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
                        // [`CommandAction::OpenAgentPicker`] as a
                        // top-level overlay, so the overlay's `Esc`
                        // closes straight back to the editor. Gated on
                        // an empty selector stack so it's inert while
                        // another selector is up.
                        if shell.agent_picker_open_request.swap(false, Ordering::Relaxed)
                            && selectors.is_empty()
                            && login_session.is_none()
                        {
                            match handle_command(
                                &mut shell.tui,
                                &shell.auth,
                                Arc::clone(&shell.model_catalog),
                                Arc::clone(&shell.run_config),
                                &shell.config,
                                &shell.config_layers,
                                &shell.render_settings,
                                world,
                                &shell.conversation_persistence,
                                &shell.theme,
                                CommandAction::OpenAgentPicker,
                                !turns.is_empty(),
                            ).await {
                                CommandOutcome::Continue { selector, notice } => {
                                    if let Some(text) = notice {
                                        world.pump.handle(&mut world.core.lifecycle, &mut shell.tui, &notice_event(&text));
                                    }
                                    if let Some(sel) = selector {
                                        selectors.push(&mut shell.tui, sel);
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

                        // A selector overlay is up and just got the
                        // input event. Poll the top of the stack and
                        // apply the transition it returns to the stack
                        // and the compositor.
                        if !selectors.is_empty() {
                            let transition = handle_selector_outcome(
                                &mut shell.tui,
                                selectors.top().expect("selector stack non-empty"),
                                &shell.auth,
                                Arc::clone(&shell.run_config),
                                Arc::clone(&shell.config),
                                &shell.config_layers,
                                &shell.model_catalog,
                                world,
                                &shell.theme,
                                &shell.render_settings,
                                theme_watch,
                            )
                            .await;
                            match transition {
                                SelectorTransition::Stay => {}
                                SelectorTransition::Back => selectors.back(&mut shell.tui),
                                SelectorTransition::Close(effects) => {
                                    selectors.close_all(&mut shell.tui);
                                    if let Some(text) = effects.notice {
                                        world.pump.handle(&mut world.core.lifecycle, &mut shell.tui, &notice_event(&text));
                                    }
                                    // A confirmed session pick exits the
                                    // per-session loop. The outer loop in
                                    // `InteractiveMode::run` rebuilds onto
                                    // the chosen session.
                                    if let Some(request) = effects.session_request {
                                        debug_assert!(
                                            turns.is_empty(),
                                            "session change requested mid-turn"
                                        );
                                        break Ok(request.into_exit());
                                    }
                                    // A confirmed login provider pick asks
                                    // the host to launch the async browser
                                    // flow: mount the dialog overlay and
                                    // spawn the login task (polled by the
                                    // login `select!` arm).
                                    if let Some(provider_id) = effects.start_login {
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
                                            Err(err) => world.pump.handle(&mut world.core.lifecycle,
                                                &mut shell.tui,
                                                &warning_event(&format!(
                                                    "Couldn't start login: {err}"
                                                )),
                                            ),
                                        }
                                    }
                                }
                                SelectorTransition::Open {
                                    action,
                                    keep_parents,
                                } => {
                                    // A drill-down (keep_parents false)
                                    // tears the stack down first so the
                                    // child has no parent to return to.
                                    // Chaining from the palette leaves it
                                    // on the stack, and `push` hides it
                                    // under the child so a cancel returns
                                    // to it.
                                    if !keep_parents {
                                        selectors.close_all(&mut shell.tui);
                                    }
                                    // `/compact` runs as a tracked task
                                    // (like a turn), so the loop (which
                                    // owns `turns`) drives it rather
                                    // than `handle_command`, which
                                    // can't spawn. It opens no child,
                                    // so any kept palette closes back to chat.
                                    if matches!(action, CommandAction::Compact) {
                                        if turns.is_busy(&world.core.lifecycle, AgentId::Main)
                                        {
                                            world.pump.handle(&mut world.core.lifecycle,
                                                &mut shell.tui,
                                                &notice_event(&session_busy_notice("compact")),
                                            );
                                        } else {
                                            turns.spawn(
                                                &world.core,
                                                &shell.run_config,
                                                AgentId::Main,
                                                TurnStart::Compact {
                                                    reason:
                                                        aj_agent::events::CompactionReason::Manual,
                                                    instructions: None,
                                                },
                                                crate::turn::turn_policy(AgentId::Main, &shell.config),
                                            );
                                        }
                                        selectors.close_all(&mut shell.tui);
                                    } else {
                                        match handle_command(
                                            &mut shell.tui,
                                            &shell.auth,
                                            Arc::clone(&shell.model_catalog),
                                            Arc::clone(&shell.run_config),
                                            &shell.config,
                                            &shell.config_layers,
                                            &shell.render_settings,
                                            world,
                                            &shell.conversation_persistence,
                                            &shell.theme,
                                            action,
                                            !turns.is_empty(),
                                        )
                                        .await
                                        {
                                            CommandOutcome::Continue { selector, notice } => {
                                                if let Some(text) = notice {
                                                    world.pump.handle(&mut world.core.lifecycle, &mut shell.tui, &notice_event(&text));
                                                }
                                                match selector {
                                                    // `push` hides any kept
                                                    // palette under the child.
                                                    Some(sel) => {
                                                        selectors.push(&mut shell.tui, sel)
                                                    }
                                                    // No child opened, so a
                                                    // kept palette is done.
                                                    None => selectors.close_all(&mut shell.tui),
                                                }
                                            }
                                            // Tear the stack down before
                                            // leaving the loop so a kept
                                            // palette can't leak into the
                                            // next session.
                                            CommandOutcome::SessionChange(request) => {
                                                debug_assert!(turns.is_empty(), "session change requested mid-turn");
                                                selectors.close_all(&mut shell.tui);
                                                break Ok(request.into_exit());
                                            }
                                            CommandOutcome::Quit => {
                                                selectors.close_all(&mut shell.tui);
                                                break Ok(SessionExit::Quit);
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
                            if turns.is_busy(&world.core.lifecycle, target) {
                                if let Some(editor) =
                                    shell.tui.get_mut_as::<Editor>(SlotIndex::Editor.idx())
                                {
                                    editor.add_to_history(&trimmed);
                                }
                                world.core.message_queues.append_follow_up(target, &trimmed);
                                world.pump.sync_pending(&mut shell.tui);
                                continue;
                            }

                            // Idle: start a turn. `spawn_prompt_turn`
                            // clears the editor, records history, mints
                            // the per-turn cancel token (kept in the
                            // cancel map so the Ctrl+C arm can fire
                            // it without locking the agent), and spawns
                            // onto `turns`. A non-promptable target
                            // (resumed sub with no handle) returns false
                            // with the editor left intact.
                            if spawn_prompt_turn(
                                &mut shell.tui,
                                &world.core,
                                &shell.run_config,
                                target,
                                trimmed,
                                crate::turn::turn_policy(target, &shell.config),
                                &mut turns,
                            ) {
                                sync_editor_enabled(&mut shell.tui);
                            } else {
                                world.pump.handle(&mut world.core.lifecycle,
                                    &mut shell.tui,
                                    &notice_event("This agent can't be prompted."),
                                );
                            }
                        }
                    }
                }
            }

            // --- Agent bus event ---
            maybe_evt = recv_event(&mut world.core.event_rx) => {
                let Some(event) = maybe_evt else { continue };
                world.pump.handle(&mut world.core.lifecycle, &mut shell.tui, &event);
                // Wake trigger 1: a background task finished. If its
                // owner is idle, wake it so the completion notice
                // reaches the model; a busy owner picks the notice up
                // at its next drain point instead.
                if let AgentEvent::TaskEnd { agent_id, .. } = &event {
                    turns.spawn_wake(
                        *agent_id,
                        &world.core,
                        &shell.run_config,
                        crate::turn::turn_policy(*agent_id, &shell.config),
                    );
                }
                // A sub-agent's initial run is nested inside the
                // parent's turn, not driven through the JoinSet, so
                // the turn-completion trigger never sees it end. If a
                // task notice arrived after that run's last drain
                // point it would rot until the next prompt — catch it
                // here on the run's AgentEnd. The pump has already
                // processed the event, so the owner reads as idle and
                // the gate inside spawn_wake is open.
                if let AgentEvent::AgentEnd { agent_id, .. } = &event
                    && (world.core.task_registry.has_notices(*agent_id)
                        || world.core.message_queues.has_pending(*agent_id))
                {
                    turns.spawn_wake(
                        *agent_id,
                        &world.core,
                        &shell.run_config,
                        crate::turn::turn_policy(*agent_id, &shell.config),
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
                world.pump.handle(&mut world.core.lifecycle,
                    &mut shell.tui,
                    &notice_event(&format!("Theme '{name}' reloaded.")),
                );
            }

            // --- Prompt-history bootstrap delivered ---
            // The cross-session Up-arrow ring is scanned off-thread at
            // startup (a large session backlog would otherwise block
            // first paint). When it lands, seed it beneath any prompts
            // already submitted this session, then drop the receiver so
            // the arm pends forever for the rest of the process.
            seeded = recv_prompt_history(prompt_history_rx.as_mut()) => {
                // Seeding the ring changes nothing on screen (history
                // only surfaces on Up), so no render is requested.
                if let Some(history) = seeded
                    && let Some(editor) =
                        shell.tui.get_mut_as::<Editor>(SlotIndex::Editor.idx())
                {
                    history.install(editor);
                }
                *prompt_history_rx = None;
            }
        }
    };

    // Kill the background-task tree before tearing down turns:
    // cancelling the registry root makes every detached driver
    // SIGKILL its process group promptly; the bounded quiesce makes
    // sure those groups are actually killed and reaped before we
    // proceed. Runs on every exit — quit, fatal error, *and* session
    // switches — so an abandoned world never leaks tasks.
    crate::modes::shutdown_background_tasks(&world.core.task_registry).await;

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

/// Spawn a user-prompt turn for `target`. Resolves the handle first and
/// leaves the editor intact on a miss (returning `false`) so the caller
/// can surface a notice and the user keeps their text; otherwise clears
/// the editor, records history, and dispatches a [`TurnStart::Prompt`]
/// sequence.
fn spawn_prompt_turn(
    tui: &mut Tui,
    core: &SessionCore,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    target: AgentId,
    text: String,
    policy: TurnPolicy,
    turns: &mut Turns,
) -> bool {
    if core.resolve_agent(target).is_none() {
        return false;
    }
    if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
        editor.set_text("");
        editor.add_to_history(&text);
    }
    turns.spawn(core, run_config, target, TurnStart::Prompt(text), policy)
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

/// Spawn the prompt-history bootstrap on a blocking thread, delivering
/// the finished [`PromptHistory`] over the returned channel.
///
/// The scan is bounded (newest-first, stops at `max`), but we still
/// keep it off the startup path so first paint never waits on disk.
/// The session loop polls the receiver via [`recv_prompt_history`] and
/// seeds the editor when the result arrives.
fn spawn_prompt_history_bootstrap(
    persistence: ConversationPersistence,
    max: usize,
) -> UnboundedReceiver<PromptHistory> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::task::spawn_blocking(move || {
        let _ = tx.send(PromptHistory::bootstrap(&persistence, max));
    });
    rx
}

/// Await the backgrounded prompt-history bootstrap. Mirrors
/// [`recv_theme`]: an absent receiver (already delivered, so the host
/// cleared it) pends forever, making the `select!` arm a no-op once
/// the ring is seeded.
async fn recv_prompt_history(
    rx: Option<&mut UnboundedReceiver<PromptHistory>>,
) -> Option<PromptHistory> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// An overlay the host is tracking on the [`SelectorStack`].
///
/// Each variant pairs the overlay's [`OverlayHandle`] (so the host
/// can hide or reveal it) with a typed outcome handle the component
/// populates on confirm or cancel. There is no per-variant parent
/// pointer: nesting (a child opened over the palette, the task
/// viewer drilled into from the picker) is positional, the entry
/// beneath a selector on the stack is its parent.
enum OpenSelector {
    Thinking {
        handle: OverlayHandle,
        outcome: ThinkingOutcomeHandle,
        /// Agent the confirm applies to, captured from the active
        /// view at open time so a view switch while the overlay is
        /// up doesn't redirect the change.
        target: AgentId,
    },
    Model {
        handle: OverlayHandle,
        outcome: ModelOutcomeHandle,
        /// Agent the confirm applies to, captured at open time
        /// like [`OpenSelector::Thinking::target`].
        target: AgentId,
    },
    Session {
        handle: OverlayHandle,
        outcome: SessionOutcomeHandle,
    },
    /// Prompt-history search overlay. `Enter` recalls the chosen
    /// prompt into the editor. `Esc` closes it.
    PromptHistory {
        handle: OverlayHandle,
        outcome: PromptHistoryOutcomeHandle,
    },
    /// Agent picker overlay. `Enter` switches the chat view to the
    /// chosen agent's transcript (and sets the editor's observing
    /// marker). `Esc` closes it.
    AgentPicker {
        handle: OverlayHandle,
        outcome: AgentPickerOutcomeHandle,
    },
    /// Read-only viewer for a background bash task's output, drilled
    /// into from the agent picker. Both Esc and Enter close it.
    TaskOutput {
        handle: OverlayHandle,
        outcome: TaskOutputOutcomeHandle,
    },
    Palette {
        handle: OverlayHandle,
        outcome: CommandPaletteOutcomeHandle,
    },
    /// Read-only help overlay. Both Esc and Enter close it.
    Help {
        handle: OverlayHandle,
        outcome: crate::modes::interactive::components::help_overlay::HelpOverlayOutcomeHandle,
    },
    /// Provider picker for login / logout. `mode` decides what
    /// confirming a provider does: start the OAuth browser flow, or
    /// remove the stored credential.
    AuthPicker {
        handle: OverlayHandle,
        outcome: AuthPickerOutcomeHandle,
        mode: AuthPickerMode,
    },
    /// Read-only auth-status overlay. Both Esc and Enter close it.
    AuthStatus {
        handle: OverlayHandle,
        outcome: AuthStatusOutcomeHandle,
    },
    /// Read-only session-info overlay. Both Esc and Enter close it.
    SessionInfo {
        handle: OverlayHandle,
        outcome: SessionInfoOutcomeHandle,
    },
    /// Read-only usage overlay. Both Esc and Enter close it. The
    /// usage reports stream in from a background fetch after the
    /// overlay opens; closing early just drops the fetch's receiver.
    UsageStatus {
        handle: OverlayHandle,
        outcome: UsageStatusOutcomeHandle,
    },
    /// Settings window (user or project). Stays open across changes:
    /// the host drains `changes` after every input event, applying and
    /// persisting each entry to the layer named by `target` (and
    /// pushing a display fix through `corrections` when an apply
    /// fails). `clears` carries per-key clears from the project window
    /// (the inherited value the live effect reverts to); empty for the
    /// user window. `outcome` only ever reports the close.
    Settings {
        handle: OverlayHandle,
        outcome: SettingsOutcomeHandle,
        changes: SettingsChangesHandle,
        corrections: SettingsCorrectionsHandle,
        clears: SettingsClearsHandle,
        target: ConfigTarget,
    },
    /// Skills window. Stays open across changes: the host
    /// drains `changes` after every input event, persisting each
    /// enable/disable toggle into the `disabled_skills` config option;
    /// `outcome` only ever reports the close.
    Skills {
        handle: OverlayHandle,
        outcome: SkillsOutcomeHandle,
        changes: SkillsChangesHandle,
    },
}

impl OpenSelector {
    /// The overlay handle this selector tracks, used to hide or
    /// reveal it on the compositor's overlay stack.
    fn handle(&self) -> OverlayHandle {
        match self {
            OpenSelector::Thinking { handle, .. }
            | OpenSelector::Model { handle, .. }
            | OpenSelector::Session { handle, .. }
            | OpenSelector::PromptHistory { handle, .. }
            | OpenSelector::AgentPicker { handle, .. }
            | OpenSelector::TaskOutput { handle, .. }
            | OpenSelector::Palette { handle, .. }
            | OpenSelector::Help { handle, .. }
            | OpenSelector::AuthPicker { handle, .. }
            | OpenSelector::AuthStatus { handle, .. }
            | OpenSelector::SessionInfo { handle, .. }
            | OpenSelector::UsageStatus { handle, .. }
            | OpenSelector::Settings { handle, .. }
            | OpenSelector::Skills { handle, .. } => *handle,
        }
    }
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

/// The host's stack of open selector overlays, mirroring the
/// compositor's overlay z-order. The top is the active selector,
/// polled after each input event. Entries beneath it are parents,
/// hidden via [`Tui::set_overlay_hidden`] and revealed when the
/// selector above them is dismissed.
///
/// The stack owns stack-to-compositor sync: [`push`] shows the child
/// on top and hides the parent beneath it, [`back`] reveals that
/// parent again, and [`close_all`] tears everything down. Callers
/// show the overlay (which auto-focuses it) and hand it here.
///
/// [`push`]: SelectorStack::push
/// [`back`]: SelectorStack::back
/// [`close_all`]: SelectorStack::close_all
#[derive(Default)]
struct SelectorStack {
    stack: Vec<OpenSelector>,
}

impl SelectorStack {
    fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    /// The active selector, or `None` when nothing is open.
    fn top(&self) -> Option<&OpenSelector> {
        self.stack.last()
    }

    /// Push a freshly shown overlay as the new top, hiding the parent
    /// beneath it so only the top stays visible. The caller has
    /// already shown the overlay, which auto-focused it.
    fn push(&mut self, tui: &mut Tui, selector: OpenSelector) {
        if let Some(parent) = self.stack.last() {
            tui.set_overlay_hidden(&parent.handle(), true);
        }
        self.stack.push(selector);
    }

    /// Pop and hide the top overlay, then reveal the parent beneath
    /// it. With no parent this returns to the chat. Used for Esc /
    /// cancel and for stay-open windows closing.
    fn back(&mut self, tui: &mut Tui) {
        if let Some(top) = self.stack.pop() {
            tui.hide_overlay(&top.handle());
        }
        if let Some(parent) = self.stack.last() {
            tui.set_overlay_hidden(&parent.handle(), false);
        }
    }

    /// Tear the whole stack down, hiding every overlay, back to the
    /// chat. Used on a terminal confirm and on the close-all chord.
    fn close_all(&mut self, tui: &mut Tui) {
        while let Some(top) = self.stack.pop() {
            tui.hide_overlay(&top.handle());
        }
    }
}

/// What polling the top selector decided. The main loop applies this
/// to the [`SelectorStack`] and the compositor; the poll handler
/// itself never touches overlay state.
enum SelectorTransition {
    /// The outcome slot is still empty; leave the stack untouched.
    Stay,
    /// Esc / cancel (or a stay-open window closing): pop the top and
    /// reveal the parent beneath it, or return to chat if it was the
    /// only level.
    Back,
    /// A terminal confirm: tear the whole stack down to chat and
    /// apply these host-side effects.
    Close(CloseEffects),
    /// Open `action` as a child overlay. The palette chains into the
    /// command it picked (`keep_parents: true`, so a cancel returns
    /// to the palette); the agent picker drills into the task viewer
    /// (`keep_parents: false`, so the picker is torn down first). The
    /// open runs through [`handle_command`] in the main loop, which
    /// has the command-only dependencies the poll handler lacks.
    Open {
        action: CommandAction,
        keep_parents: bool,
    },
}

/// Host-side effects a terminal confirm asks the main loop to apply
/// after it drains the selector stack. The fields are independent
/// and usually only one is set.
#[derive(Default)]
struct CloseEffects {
    /// A status line to render in the chat scrollback.
    notice: Option<String>,
    /// A login provider pick the host should turn into a launched
    /// OAuth flow. The picker can't spawn the task itself, that's the
    /// main loop's job, where the login session state and the task
    /// `select!` arm live.
    start_login: Option<String>,
    /// A confirmed session pick the outer session loop should perform
    /// by rebuilding the world. Only emitted when no turn is in
    /// flight.
    session_request: Option<SessionRequest>,
}

impl CloseEffects {
    /// A confirm that closes with just a status notice.
    fn notice(text: String) -> Self {
        CloseEffects {
            notice: Some(text),
            ..CloseEffects::default()
        }
    }
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

/// Resolve the `(provider, id)` model key a config layer names,
/// applying the same fallbacks startup uses when a field is unset:
/// [`DEFAULT_PROVIDER_ID`] for the provider, and the first catalog
/// entry for that provider as the model.
///
/// Used to fill the model row of the project settings window, where the
/// value shown is a config-layer view rather than the live run config.
///
/// [`DEFAULT_PROVIDER_ID`]: crate::model::DEFAULT_PROVIDER_ID
fn config_model_key(config: &Config, catalog: &[ModelInfo]) -> (String, String) {
    let provider = config
        .model_api
        .clone()
        .unwrap_or_else(|| crate::model::DEFAULT_PROVIDER_ID.to_string());
    let id = config.model_name.clone().unwrap_or_else(|| {
        catalog
            .iter()
            .find(|m| m.provider == provider)
            .map(|m| m.id.clone())
            .unwrap_or_default()
    });
    (provider, id)
}

/// Project a [`Config`] onto the [`SettingsCurrentValues`] the settings
/// window renders, using the same canonical vocabulary the window's
/// apply path parses.
///
/// This is the config-layer (not live run-config) view, used by the
/// project settings window: `current` comes from the effective config
/// and `inherited` from the user config, so a project-set row shows
/// exactly what the file pins and a clear knows what to revert to.
fn settings_values_from_config(config: &Config, catalog: &[ModelInfo]) -> SettingsCurrentValues {
    SettingsCurrentValues {
        model_key: config_model_key(config, catalog),
        model_url: config.model_url.clone(),
        thinking: config
            .thinking
            .map(|l| l.to_string())
            .unwrap_or_else(|| "off".to_string()),
        thinking_display: config.thinking_display.map(|d| d.to_string()),
        speed: config
            .speed
            .map(|s| s.to_string())
            .unwrap_or_else(|| "standard".to_string()),
        verbosity: config.verbosity.map(|v| v.to_string()),
        theme: resolve_theme_name(config.theme.as_deref()).to_string(),
        disabled_tools: config.disabled_tools.clone(),
        disabled_skills: config.disabled_skills.clone(),
        hide_thinking_block: config.hide_thinking_block,
        show_frame_stats: config.show_frame_stats,
        image_auto_resize: config.image_auto_resize,
        image_show_in_terminal: config.image_show_in_terminal,
        image_block: config.image_block,
        syntax_highlighting: config.syntax_highlighting,
        auto_compact: config.auto_compact,
        bash_rtk: config.bash_rtk,
        compact_threshold: config.compact_threshold.to_string(),
        compact_keep_recent: config.compact_keep_recent.to_string(),
    }
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

/// Inner-content row count for the compact overlays (palette, help,
/// model / thinking pickers, the read-only auth / usage / session-info
/// pages). Total rendered height including chrome is
/// `PALETTE_OVERLAY_INNER_ROWS + 4`, so this value plus four is the box's
/// footprint on screen. Sized so the command palette shows its whole
/// catalog without scrolling: the palette reserves three of these rows
/// for its search box, separator, and scroll indicator (see
/// `CommandPaletteComponent::set_available_height`), so the budget must
/// stay at least `COMMANDS.len() + 3`. The content-heavy overlays
/// (session switcher, prompt history) size their rows dynamically
/// instead. See [`large_overlay_inner_rows`].
const PALETTE_OVERLAY_INNER_ROWS: usize = 23;

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
pub(crate) fn subtitle_close() -> String {
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

/// Per-frame subtitle for the project settings window. Like
/// [`subtitle_settings_window`], but the main-list hint also explains
/// the override marker and advertises the clear-override chord (project
/// rows can revert to the inherited user value).
fn subtitle_project_settings_window(child: &dyn aj_tui::component::Component) -> String {
    let submenu = child
        .as_any()
        .downcast_ref::<SettingsWindowComponent>()
        .map(SettingsWindowComponent::active_submenu)
        .unwrap_or(SettingsSubmenu::None);
    if submenu != SettingsSubmenu::None {
        // Inside a submenu the keys mean the same as the user window.
        return subtitle_settings_window(child);
    }
    let confirm = aj_tui::keybindings::format_action_shortcut("tui.select.confirm")
        .unwrap_or_else(|| "Enter".to_string());
    let cancel = aj_tui::keybindings::format_action_shortcut("tui.select.cancel")
        .unwrap_or_else(|| "Esc".to_string());
    let clear = aj_tui::keybindings::format_action_shortcut(
        crate::config::keybindings::ACTION_SETTINGS_CLEAR,
    )
    .unwrap_or_else(|| "Ctrl+X".to_string());
    format!(
        "\u{25cf} set here  \u{2022}  {confirm} change  \u{2022}  {clear} clear  \u{2022}  {cancel} close"
    )
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

/// Write the rendered session HTML to `~/.aj/exports/aj-session-<id>.html`,
/// creating the directory if needed. Returns the path written.
///
/// We keep exports under the managed config dir rather than the
/// working directory so a `export` from inside a git repo doesn't drop
/// an untracked file into the user's tree. The notice reports the full
/// path, so it stays discoverable.
fn write_session_export(session_id: &str, html: &str) -> Result<PathBuf> {
    let dir = Config::get_config_dir()
        .context("failed to resolve ~/.aj")?
        .join("exports");
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = dir.join(format!("aj-session-{session_id}.html"));
    std::fs::write(&path, html).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
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
    config_layers: &Arc<std::sync::Mutex<ConfigLayers>>,
    render_settings: &RenderSettings,
    world: &SessionWorld,
    conversation_persistence: &ConversationPersistence,
    theme: &ThemeHandle,
    action: CommandAction,
    turn_running: bool,
) -> CommandOutcome {
    match action {
        CommandAction::OpenCommandPalette => {
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
                        mode: AuthPickerMode::Logout,
                    }),
                    notice: None,
                }
            }
        }
        CommandAction::OpenAuthStatus => {
            let statuses = crate::auth::collect_statuses(auth).await;
            let inner = crate::modes::interactive::components::auth_status::build_overlay(
                select_list_theme(theme),
                statuses,
            );
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
                selector: Some(OpenSelector::AuthStatus { handle, outcome }),
                notice: None,
            }
        }
        CommandAction::OpenSessionInfo => {
            // Read-only snapshot: lock the log, compute the digest, and
            // drop the guard at the end of the statement so it is never
            // held across the overlay's lifetime. Safe mid-turn.
            let stats = world.core.log.lock().await.stats();
            let inner = crate::modes::interactive::components::session_info::build_overlay(
                select_list_theme(theme),
                stats,
            );
            let outcome = inner.outcome_handle();
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Session info",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                PALETTE_OVERLAY_INNER_ROWS,
            )
            .with_subtitle(&subtitle_close());
            let handle = tui.show_overlay(Box::new(window), palette_overlay_options());
            CommandOutcome::Continue {
                selector: Some(OpenSelector::SessionInfo { handle, outcome }),
                notice: None,
            }
        }
        CommandAction::ExportHtml => {
            // Render under the lock (read-only, so it can't deadlock a
            // turn) as a string, then write the file with the guard
            // already dropped at the end of the statement. Mirrors
            // `OpenSessionInfo`, which also reads the log under the
            // lock. Both success and failure surface as a notice.
            let html = crate::export::render_session_html(&*world.core.log.lock().await);
            let notice = match write_session_export(&world.core.session_id, &html) {
                Ok(path) => format!("Exported session to {}", display_path(&path)),
                Err(e) => format!("Export failed: {e}"),
            };
            CommandOutcome::Continue {
                selector: None,
                notice: Some(notice),
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

            // Dependencies for the in-overlay reset-credit action: it
            // spends a credit and refetches without leaving the overlay.
            let deps = UsageActionDeps {
                auth: auth.clone(),
                reset_sources: aj_models::usage::default_reset_sources(),
                runtime: tokio::runtime::Handle::current(),
                render: tui.handle(),
            };
            let inner = UsageStatusComponent::new(select_list_theme(theme), rx, deps);
            let outcome = inner.outcome_handle();
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Usage",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                PALETTE_OVERLAY_INNER_ROWS,
            )
            // The hint tracks the reset-credit state machine, so resolve
            // it from the component each frame.
            .with_dynamic_subtitle(|child| {
                child
                    .as_any()
                    .downcast_ref::<UsageStatusComponent>()
                    .map(UsageStatusComponent::footer_hint)
                    .unwrap_or_default()
            });
            let handle = tui.show_overlay(Box::new(window), palette_overlay_options());
            CommandOutcome::Continue {
                selector: Some(OpenSelector::UsageStatus { handle, outcome }),
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
            let current_session_id = world.core.session_id.clone();

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
                selector: Some(OpenSelector::Session { handle, outcome }),
                notice: None,
            }
        }
        // The session-tree overlay is an aj-next surface; this frontend has no
        // renderer for it, so the palette entry folds a notice rather than
        // opening anything.
        CommandAction::OpenSessionTree => CommandOutcome::Continue {
            selector: None,
            notice: Some("The session tree is available in aj-next.".to_string()),
        },
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
                selector: Some(OpenSelector::PromptHistory { handle, outcome }),
                notice: None,
            }
        }
        CommandAction::OpenTaskOutput { id } => {
            // Drilled into from the agent picker, never the palette.
            // The picker only lists bash tasks, so resolve the command
            // line for the viewer header; if the task has left the
            // registry there is nothing to show.
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
                    let initial_inner_rows =
                        large_overlay_inner_rows(usize::from(tui.terminal().rows()));
                    let inner =
                        TaskOutputComponent::new(world.core.task_registry.clone(), id, command);
                    let outcome = inner.outcome_handle();
                    let window = aj_tui::components::overlay_window::OverlayWindow::new(
                        format!("Task #{id}"),
                        Box::new(inner),
                        crate::config::theme::overlay_window_theme(theme),
                        initial_inner_rows,
                    )
                    .with_dynamic_height(tui.handle(), large_overlay_inner_rows)
                    .with_subtitle(&subtitle_task_output());
                    let handle = tui.show_overlay(Box::new(window), large_overlay_options());
                    CommandOutcome::Continue {
                        selector: Some(OpenSelector::TaskOutput { handle, outcome }),
                        notice: None,
                    }
                }
                None => CommandOutcome::Continue {
                    selector: None,
                    notice: Some(format!("Background task #{id} is no longer available.")),
                },
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
                selector: Some(OpenSelector::AgentPicker { handle, outcome }),
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
                    verbosity: run_cfg
                        .stream_options
                        .verbosity
                        .map(|v| verbosity_name(Some(v)).to_string()),
                    theme: resolve_theme_name(cfg.theme.as_deref()).to_string(),
                    disabled_tools: cfg.disabled_tools.clone(),
                    disabled_skills: cfg.disabled_skills.clone(),
                    hide_thinking_block: render_settings.hide_thinking_block(),
                    show_frame_stats: cfg.show_frame_stats,
                    image_auto_resize: cfg.image_auto_resize,
                    image_show_in_terminal: render_settings.show_image_in_terminal(),
                    image_block: cfg.image_block,
                    syntax_highlighting: cfg.syntax_highlighting,
                    auto_compact: cfg.auto_compact,
                    bash_rtk: cfg.bash_rtk,
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
            let clears = inner.clears_handle();
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
                    clears,
                    target: ConfigTarget::User,
                }),
                notice: None,
            }
        }
        CommandAction::OpenProjectSettings => {
            // Per-project settings (`<git-root>/.aj/config.toml`),
            // layered over the user config. Unavailable outside a git
            // repository.
            let (effective_config, user_config, set_keys) = {
                let l = config_layers.lock().expect("config layers mutex poisoned");
                if l.project_path.is_none() {
                    return CommandOutcome::Continue {
                        selector: None,
                        notice: Some(
                            "Project settings need a git repository (no .git found above the \
                             working directory)."
                                .to_string(),
                        ),
                    };
                }
                let effective = config.lock().expect("config mutex poisoned").clone();
                let set_keys: std::collections::BTreeSet<String> =
                    l.project.set_keys().map(String::from).collect();
                (effective, l.user.clone(), set_keys)
            };
            // `current` shows the effective value per row (the project
            // value where the project sets one, otherwise the inherited
            // user value); `inherited` is the user-only value a clear
            // reverts to. Both are config-layer views (not the live run
            // config), so a project-set row reflects exactly what the
            // file pins.
            let current = settings_values_from_config(&effective_config, &model_catalog);
            let inherited = settings_values_from_config(&user_config, &model_catalog);
            let tool_names: Vec<String> = get_builtin_tools(&BuiltinToolOptions::default())
                .into_iter()
                .map(|tool| tool.name)
                .collect();
            let skill_names: Vec<String> = aj_conf::skills::discover_skills(&[])
                .0
                .into_iter()
                .map(|skill| skill.name)
                .collect();
            let inner = SettingsWindowComponent::new_project(
                settings_list_theme(theme),
                select_list_theme(theme),
                (*model_catalog).clone(),
                Theme::available(),
                tool_names,
                skill_names,
                current,
                inherited,
                set_keys,
            );
            let outcome = inner.outcome_handle();
            let changes = inner.changes_handle();
            let corrections = inner.corrections_handle();
            let clears = inner.clears_handle();
            let initial_inner_rows = large_overlay_inner_rows(usize::from(tui.terminal().rows()));
            let window = aj_tui::components::overlay_window::OverlayWindow::new(
                "Project Settings",
                Box::new(inner),
                crate::config::theme::overlay_window_theme(theme),
                initial_inner_rows,
            )
            .with_dynamic_height(tui.handle(), large_overlay_inner_rows)
            .with_dynamic_subtitle(subtitle_project_settings_window);
            let handle = tui.show_overlay(Box::new(window), large_overlay_options());
            CommandOutcome::Continue {
                selector: Some(OpenSelector::Settings {
                    handle,
                    outcome,
                    changes,
                    corrections,
                    clears,
                    target: ConfigTarget::Project,
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
                    }),
                    notice: None,
                }
            }
        }
        CommandAction::Help => {
            let inner = crate::modes::interactive::components::help_overlay::build_overlay(
                select_list_theme(theme),
            );
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
                selector: Some(OpenSelector::Help { handle, outcome }),
                notice: None,
            }
        }
        CommandAction::Quit => CommandOutcome::Quit,
    }
}

/// Apply a confirmed thinking pick to the main agent, then reconcile
/// the view. The shared [`aj_app::settings::confirm_thinking_for_main`]
/// core stages the run config, records the change on the session log's
/// user thread, and persists it per `persist`. This wrapper applies the
/// editor border tint and the footer note. Returns the user-facing
/// notice.
async fn confirm_thinking_for_main(
    tui: &mut Tui,
    level: Option<ThinkingConfig>,
    persist: PersistAction,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: &Arc<std::sync::Mutex<Config>>,
    layers: &Arc<std::sync::Mutex<ConfigLayers>>,
    world: &mut SessionWorld,
    theme: &ThemeHandle,
) -> String {
    // Mirror the change onto the editor's border tint so the visual cue
    // tracks the active reasoning mode, but only when the user is
    // viewing the agent the change applies to. Independent of the config
    // staging in the core, so we do it up front and keep `level`.
    if world.pump.active_view(tui) == AgentId::Main {
        apply_editor_border_for_thinking(tui, theme, level.as_ref());
    }
    let MainConfirm { footer, notice } = aj_app::settings::confirm_thinking_for_main(
        level,
        persist,
        run_config,
        config,
        layers,
        &world.core,
    )
    .await;
    // Footer surfaces the active thinking effort; record the new
    // settings so the change is visible without waiting for a turn.
    if let Some(FooterUpdate {
        settings,
        context_window,
    }) = footer
    {
        world
            .pump
            .note_agent_settings(tui, AgentId::Main, settings, context_window);
    }
    notice
}

/// Apply a confirmed thinking pick to sub-agent `n`, then reconcile the
/// view. The shared [`aj_app::settings::confirm_thinking_for_sub`] core
/// validates against the target's model, stages the override, and
/// records the change on the sub's log thread. This wrapper resolves the
/// validation fallback from the frontend's tracked model, refreshes the
/// sub's footer entry, and re-tints the border. Returns the user-facing
/// notice.
async fn confirm_thinking_for_sub(
    tui: &mut Tui,
    level: Option<ThinkingConfig>,
    n: usize,
    model_catalog: &[ModelInfo],
    world: &mut SessionWorld,
    theme: &ThemeHandle,
) -> String {
    let target = AgentId::Sub(n);
    // The model the footer currently shows for the target, resolved to a
    // catalog entry. The core uses it as the validation fallback when no
    // bundle override is staged for the agent. Reading it is side-effect
    // free, so we compute it up front regardless of whether the core
    // ends up needing it.
    let tracked_model = world.pump.agent_settings(target).and_then(|s| {
        model_catalog
            .iter()
            .find(|m| m.provider == s.provider && m.id == s.model_id)
            .cloned()
            .map(Arc::new)
    });
    let SubConfirm { notice, applied } =
        aj_app::settings::confirm_thinking_for_sub(level.clone(), n, tracked_model, &world.core)
            .await;
    if applied {
        let name = thinking_level_name(&level);
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
    }
    notice
}

/// Apply a confirmed model pick to the main agent, then reconcile the
/// view. The shared [`aj_app::settings::confirm_model_for_main`] core
/// rebuilds the bundle, stages the run config, records the change on the
/// session log's user thread, and persists it per `persist`. This
/// wrapper notes the new footer identity (skipped on a rebuild failure).
/// Returns the user-facing notice.
async fn confirm_model_for_main(
    tui: &mut Tui,
    info: ModelInfo,
    persist: PersistAction,
    auth: &AuthStorage,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: &Arc<std::sync::Mutex<Config>>,
    layers: &Arc<std::sync::Mutex<ConfigLayers>>,
    world: &mut SessionWorld,
) -> String {
    let MainConfirm { footer, notice } = aj_app::settings::confirm_model_for_main(
        info,
        persist,
        auth,
        run_config,
        config,
        layers,
        &world.core,
    )
    .await;
    // Record the new settings identity so the footer's model line and
    // context-window denominator reflect the swap immediately rather
    // than waiting for the next turn.
    if let Some(FooterUpdate {
        settings,
        context_window,
    }) = footer
    {
        world
            .pump
            .note_agent_settings(tui, AgentId::Main, settings, context_window);
    }
    notice
}

/// Apply a confirmed model pick to sub-agent `n`, then reconcile the
/// view. The shared [`aj_app::settings::confirm_model_for_sub`] core
/// rebuilds the bundle at the resolved effective speed, stages it into
/// the override map, and records the change on the sub's log thread.
/// This wrapper resolves the effective speed from the frontend's tracked
/// state and refreshes the sub's footer entry (preserving its thinking
/// and verbosity strings). Returns the user-facing notice.
async fn confirm_model_for_sub(
    tui: &mut Tui,
    info: ModelInfo,
    n: usize,
    auth: &AuthStorage,
    world: &mut SessionWorld,
) -> String {
    let target = AgentId::Sub(n);
    // Effective speed: the staged override for this agent if present,
    // else the target's tracked settings string. The rebuilt bundle
    // re-stamps this speed's headers.
    let staged_speed = {
        let overrides = world
            .core
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
    let SubConfirm { notice, applied } =
        aj_app::settings::confirm_model_for_sub(&info, n, auth, effective_speed, &world.core).await;
    if applied {
        // Refresh the target's footer entry: new identity, the catalog
        // entry's window, thinking and verbosity strings preserved.
        let preserved_thinking = world
            .pump
            .agent_settings(target)
            .map(|s| s.thinking.clone())
            .unwrap_or_else(|| "off".to_string());
        let preserved_verbosity = world
            .pump
            .agent_settings(target)
            .map(|s| s.verbosity.clone())
            .unwrap_or_else(|| "default".to_string());
        let settings = aj_agent::events::AgentSettings {
            provider: info.provider.clone(),
            model_id: info.id.clone(),
            thinking: preserved_thinking,
            speed: speed_name(effective_speed).to_string(),
            verbosity: preserved_verbosity,
        };
        world
            .pump
            .note_agent_settings(tui, target, settings, info.context_window);
    }
    notice
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
    persist: PersistAction,
    id: &str,
    value: &str,
    auth: &AuthStorage,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: &Arc<std::sync::Mutex<Config>>,
    layers: &Arc<std::sync::Mutex<ConfigLayers>>,
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
            let notice =
                confirm_model_for_main(tui, info, persist, auth, run_config, config, layers, world)
                    .await;
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
            Some(level) => Some(
                confirm_thinking_for_main(
                    tui, level, persist, run_config, config, layers, world, theme,
                )
                .await,
            ),
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
            // The "default" sentinel means "leave it unset", which for
            // either layer is a key removal.
            let value_opt = (value != UNSET_VALUE).then_some(value);
            let save_note = persist_setting(
                layers,
                config,
                persist,
                "thinking_display",
                value_opt,
                |c| c.thinking_display = display,
            );
            Some(join_notice(
                format!("Thinking display set to {value}. Takes effect next turn."),
                save_note,
            ))
        }
        "speed" => match speed_from_name(value) {
            Some(speed) => Some(
                confirm_speed_for_main(
                    tui,
                    speed,
                    persist,
                    auth,
                    run_config,
                    config,
                    layers,
                    world,
                    corrections,
                )
                .await,
            ),
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
                    run_config,
                    config,
                    layers,
                    &world.core,
                )
                .await,
            )
        }
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
                    let save_note =
                        persist_setting(layers, config, persist, "theme", Some(value), |c| {
                            c.theme = Some(value.to_string())
                        });
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
            let save_note = persist_setting(
                layers,
                config,
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
                save_note,
            ))
        }
        "image_show_in_terminal" => {
            let show = value == "true";
            render_settings.set_show_image_in_terminal(show);
            tui.request_render();
            let save_note = persist_setting(
                layers,
                config,
                persist,
                "image_show_in_terminal",
                Some(value),
                |c| c.image_show_in_terminal = show,
            );
            Some(join_notice(
                format!("image_show_in_terminal set to {show}."),
                save_note,
            ))
        }
        "image_auto_resize" => {
            let on = value == "true";
            let save_note = persist_setting(
                layers,
                config,
                persist,
                "image_auto_resize",
                Some(value),
                |c| c.image_auto_resize = on,
            );
            Some(join_notice(
                format!("image_auto_resize set to {on}. Takes effect for new sessions."),
                save_note,
            ))
        }
        "image_block" => {
            let on = value == "true";
            let save_note =
                persist_setting(layers, config, persist, "image_block", Some(value), |c| {
                    c.image_block = on
                });
            Some(join_notice(
                format!("image_block set to {on}. Takes effect for new sessions."),
                save_note,
            ))
        }
        "syntax_highlighting" => {
            let on = value == "true";
            let save_note = persist_setting(
                layers,
                config,
                persist,
                "syntax_highlighting",
                Some(value),
                |c| c.syntax_highlighting = on,
            );
            Some(join_notice(
                format!("syntax_highlighting set to {on}. Takes effect for new sessions."),
                save_note,
            ))
        }
        "model_url" => {
            let url = (!value.is_empty()).then(|| value.to_string());
            let save_note =
                persist_setting(layers, config, persist, "model_url", url.as_deref(), |c| {
                    c.model_url = url.clone()
                });
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
            let save_note = persist_setting(
                layers,
                config,
                persist,
                "disabled_tools",
                Some(value),
                |c| c.disabled_tools = tools.clone(),
            );
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
            let save_note = persist_setting(
                layers,
                config,
                persist,
                "disabled_skills",
                Some(value),
                |c| c.disabled_skills = skills.clone(),
            );
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
            // config-backed value with no extra live side effects: the
            // options that need a provider rebuild, theme reload, or a
            // live render update are intercepted by the arms above.
            // Route the rest through the schema so a freshly-added
            // option is editable from the settings windows without a
            // bespoke arm here.
            let Some(option) = Config::option(other) else {
                return Some(format!("Unknown setting {other:?}."));
            };
            // Validate before touching any layer so a bad value is
            // rejected with a clean message. A project clear carries an
            // already-valid inherited value, so it needs no check.
            if persist != PersistAction::ProjectClear {
                if let Err(err) = option.apply_str(value, &mut Config::default()) {
                    return Some(format!("Can't set {other}: {err}"));
                }
            }
            let save_note = persist_setting(layers, config, persist, other, Some(value), |c| {
                // Pre-validated above, so this can't fail.
                let _ = option.apply_str(value, c);
            });
            Some(join_notice(format!("{other} set to {value}."), save_note))
        }
    }
}

/// Apply a speed change to the main agent, then reconcile the view.
/// The shared [`aj_app::settings::confirm_speed_for_main`] core rebuilds
/// the provider bundle at the current model (re-stamping speed-derived
/// headers), stages the run config, records the change on the session
/// log's user thread, and persists it per `persist`. This wrapper notes
/// the new footer identity on success, or reverts the settings row via
/// `corrections` when the rebuild fails (e.g. scripted mode, whose
/// provider isn't in the registry). Returns the user-facing notice.
async fn confirm_speed_for_main(
    tui: &mut Tui,
    speed: Option<Speed>,
    persist: PersistAction,
    auth: &AuthStorage,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: &Arc<std::sync::Mutex<Config>>,
    layers: &Arc<std::sync::Mutex<ConfigLayers>>,
    world: &mut SessionWorld,
    corrections: &SettingsCorrectionsHandle,
) -> String {
    match aj_app::settings::confirm_speed_for_main(
        speed,
        persist,
        auth,
        run_config,
        config,
        layers,
        &world.core,
    )
    .await
    {
        SpeedConfirm::Applied {
            footer:
                FooterUpdate {
                    settings,
                    context_window,
                },
            notice,
        } => {
            world
                .pump
                .note_agent_settings(tui, AgentId::Main, settings, context_window);
            notice
        }
        SpeedConfirm::Failed { previous, notice } => {
            push_correction(corrections, tui, "speed", previous);
            notice
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

/// Poll the active selector's outcome slot after an input event and
/// decide how the host should move the selector stack.
///
/// This computes a [`SelectorTransition`] and performs the
/// variant-specific confirm work (staging a model/thinking change,
/// switching the chat view, killing a task, draining a stay-open
/// window's edits). It deliberately does not touch the compositor's
/// overlay stack: hiding and revealing overlays is the main loop's
/// job, applied through [`SelectorStack`] once this returns. The
/// confirm work runs before that hide, but rendering is deferred to
/// the top of the loop, so the overlay is gone before the next paint.
#[allow(clippy::too_many_arguments)]
async fn handle_selector_outcome(
    tui: &mut Tui,
    selector: &OpenSelector,
    auth: &AuthStorage,
    run_config: Arc<std::sync::Mutex<RunConfigSnapshot>>,
    config: Arc<std::sync::Mutex<Config>>,
    config_layers: &Arc<std::sync::Mutex<ConfigLayers>>,
    model_catalog: &[ModelInfo],
    world: &mut SessionWorld,
    theme: &ThemeHandle,
    render_settings: &RenderSettings,
    theme_watch: &mut ThemeWatch,
) -> SelectorTransition {
    match selector {
        OpenSelector::Thinking {
            outcome, target, ..
        } => {
            let outcome_value = outcome.take();
            match outcome_value {
                None => SelectorTransition::Stay,
                Some(ThinkingSelectorOutcome::Confirmed(level)) => {
                    let notice = match *target {
                        AgentId::Main => {
                            confirm_thinking_for_main(
                                tui,
                                level,
                                PersistAction::None,
                                &run_config,
                                &config,
                                config_layers,
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
                    SelectorTransition::Close(CloseEffects::notice(notice))
                }
                Some(ThinkingSelectorOutcome::Cancelled) => SelectorTransition::Back,
            }
        }
        OpenSelector::Model {
            outcome, target, ..
        } => {
            let outcome_value = outcome.take();
            match outcome_value {
                None => SelectorTransition::Stay,
                Some(ModelSelectorOutcome::Confirmed(info)) => {
                    let notice = match *target {
                        AgentId::Main => {
                            confirm_model_for_main(
                                tui,
                                info,
                                PersistAction::None,
                                auth,
                                &run_config,
                                &config,
                                config_layers,
                                world,
                            )
                            .await
                        }
                        AgentId::Sub(n) => confirm_model_for_sub(tui, info, n, auth, world).await,
                    };
                    SelectorTransition::Close(CloseEffects::notice(notice))
                }
                Some(ModelSelectorOutcome::Cancelled) => SelectorTransition::Back,
            }
        }
        OpenSelector::Session { outcome, .. } => {
            let outcome_value = outcome.take();
            match outcome_value {
                None => SelectorTransition::Stay,
                Some(SessionSelectorOutcome::Confirmed(session_id)) => {
                    // No-op when the user picks the row that's already
                    // active. Saves the rebuild (and the chat-container
                    // clear that would briefly hide the scrollback).
                    if world.core.session_id == session_id {
                        return SelectorTransition::Close(CloseEffects::notice(format!(
                            "Already on session {session_id}."
                        )));
                    }
                    // Hand the pick to the outer session loop, which
                    // tears down the current world and rebuilds onto the
                    // chosen session (and emits the switch notice after
                    // the new world is installed).
                    SelectorTransition::Close(CloseEffects {
                        session_request: Some(SessionRequest::Resume(session_id)),
                        ..CloseEffects::default()
                    })
                }
                Some(SessionSelectorOutcome::Cancelled) => SelectorTransition::Back,
            }
        }
        OpenSelector::PromptHistory { outcome, .. } => match outcome.take() {
            None => SelectorTransition::Stay,
            Some(PromptHistoryOutcome::Recalled { text }) => {
                // Recall replaces the editor buffer (it does not submit)
                // so the user can edit before sending.
                if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
                    editor.set_text(&text);
                }
                tui.request_render();
                SelectorTransition::Close(CloseEffects::default())
            }
            Some(PromptHistoryOutcome::Cancelled) => SelectorTransition::Back,
        },
        OpenSelector::Help { outcome, .. } => match outcome.take() {
            None => SelectorTransition::Stay,
            Some(()) => SelectorTransition::Back,
        },
        OpenSelector::AuthPicker { outcome, mode, .. } => {
            use crate::modes::interactive::components::auth_picker::AuthPickerOutcome;
            let value = outcome.take();
            match value {
                None => SelectorTransition::Stay,
                Some(AuthPickerOutcome::Cancelled) => SelectorTransition::Back,
                Some(AuthPickerOutcome::Confirmed(provider_id)) => match *mode {
                    // Login is async + long-running: hand the provider id
                    // back so the main loop mounts the dialog and spawns
                    // the flow.
                    AuthPickerMode::Login => SelectorTransition::Close(CloseEffects {
                        start_login: Some(provider_id),
                        ..CloseEffects::default()
                    }),
                    // Logout is a quick disk write we can do inline.
                    AuthPickerMode::Logout => {
                        let notice = match auth.logout(&provider_id).await {
                            Ok(()) => format!("Logged out of {provider_id}."),
                            Err(err) => format!("Failed to log out of {provider_id}: {err}"),
                        };
                        SelectorTransition::Close(CloseEffects::notice(notice))
                    }
                },
            }
        }
        OpenSelector::AuthStatus { outcome, .. } => match outcome.take() {
            None => SelectorTransition::Stay,
            Some(()) => SelectorTransition::Back,
        },
        OpenSelector::SessionInfo { outcome, .. } => match outcome.take() {
            None => SelectorTransition::Stay,
            Some(()) => SelectorTransition::Back,
        },
        OpenSelector::UsageStatus { outcome, .. } => {
            use crate::modes::interactive::components::usage_status::UsageStatusOutcome;
            match outcome.take() {
                None => SelectorTransition::Stay,
                Some(UsageStatusOutcome::Closed) => SelectorTransition::Back,
            }
        }
        OpenSelector::AgentPicker { outcome, .. } => {
            let outcome_value = outcome.take();
            match outcome_value {
                None => SelectorTransition::Stay,
                Some(AgentPickerOutcome::Confirmed(id)) => {
                    // Switch the chat view to the chosen agent and mark
                    // the editor so the user sees which agent they're
                    // observing (cleared when switching back to main).
                    world.pump.set_active_view(&world.core.lifecycle, tui, id);
                    apply_editor_agent_marker(tui, id);
                    apply_editor_border_for_view(tui, theme, &world.pump, &run_config, id);
                    SelectorTransition::Close(CloseEffects::default())
                }
                Some(AgentPickerOutcome::ConfirmedTask(id)) => {
                    // Drill into the task's output viewer, tearing the
                    // picker down first: Esc from the viewer returns to
                    // chat, not the picker. `handle_command` resolves the
                    // task and builds the overlay, or surfaces a notice
                    // if it's already gone.
                    SelectorTransition::Open {
                        action: CommandAction::OpenTaskOutput { id },
                        keep_parents: false,
                    }
                }
                Some(AgentPickerOutcome::KillTask(id)) => {
                    // The registry cancels the task's token; the driver
                    // kills the process group, flips the status, and the
                    // resulting `TaskEnd` freezes the cell. The picker
                    // rows are a snapshot from open time, so consult the
                    // live status: the task may have finished while the
                    // picker was up.
                    let live_status = world
                        .core
                        .task_registry
                        .snapshot()
                        .into_iter()
                        .find(|t| t.id == id)
                        .map(|t| t.status);
                    let notice = match live_status {
                        Some(aj_agent::tool::TaskStatus::Running) => {
                            world.core.task_registry.kill(id);
                            format!("Killing background task #{id}.")
                        }
                        Some(_) => format!("Background task #{id} already finished."),
                        None => {
                            format!("Background task #{id} is not in the registry (already gone?).")
                        }
                    };
                    SelectorTransition::Close(CloseEffects::notice(notice))
                }
                Some(AgentPickerOutcome::Cancelled) => SelectorTransition::Back,
            }
        }
        OpenSelector::TaskOutput { outcome, .. } => match outcome.take() {
            None => SelectorTransition::Stay,
            Some(TaskOutputOutcome::Closed) => SelectorTransition::Back,
        },
        OpenSelector::Settings {
            outcome,
            changes,
            corrections,
            clears,
            target,
            ..
        } => {
            // Apply queued changes first. The window stays open while
            // the user keeps editing, so changes, clears, and the
            // eventual close arrive through separate channels.
            let drained: Vec<(String, String)> =
                std::mem::take(&mut *changes.lock().expect("settings changes poisoned"));
            for (id, value) in drained {
                let notice = apply_setting_change(
                    tui,
                    PersistAction::set_for(*target),
                    &id,
                    &value,
                    auth,
                    &run_config,
                    &config,
                    config_layers,
                    model_catalog,
                    world,
                    theme,
                    theme_watch,
                    render_settings,
                    corrections,
                )
                .await;
                if let Some(text) = notice {
                    world
                        .pump
                        .handle(&mut world.core.lifecycle, tui, &notice_event(&text));
                }
            }
            // Clears (project window only) carry the inherited value so
            // the live effect reverts to it; persistence removes the
            // project override.
            let cleared: Vec<(String, String)> =
                std::mem::take(&mut *clears.lock().expect("settings clears poisoned"));
            for (id, inherited_value) in cleared {
                let notice = apply_setting_change(
                    tui,
                    PersistAction::ProjectClear,
                    &id,
                    &inherited_value,
                    auth,
                    &run_config,
                    &config,
                    config_layers,
                    model_catalog,
                    world,
                    theme,
                    theme_watch,
                    render_settings,
                    corrections,
                )
                .await;
                if let Some(text) = notice {
                    world
                        .pump
                        .handle(&mut world.core.lifecycle, tui, &notice_event(&text));
                }
            }
            let outcome_value = outcome.lock().expect("settings outcome poisoned").take();
            match outcome_value {
                None => SelectorTransition::Stay,
                Some(SettingsWindowOutcome::Closed) => SelectorTransition::Back,
            }
        }
        OpenSelector::Skills {
            outcome, changes, ..
        } => {
            // Persist queued toggles first. The window stays open while
            // the user keeps toggling, so changes and the eventual close
            // arrive through separate channels.
            let drained: Vec<(String, String)> =
                std::mem::take(&mut *changes.lock().expect("skills changes poisoned"));
            for (name, value) in drained {
                let disable = value == "disabled";
                let save_note = persist_user(config_layers, &config, |c| {
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
                world
                    .pump
                    .handle(&mut world.core.lifecycle, tui, &notice_event(&notice));
            }
            let outcome_value = outcome.lock().expect("skills outcome poisoned").take();
            match outcome_value {
                None => SelectorTransition::Stay,
                Some(SkillsWindowOutcome::Closed) => SelectorTransition::Back,
            }
        }
        OpenSelector::Palette { outcome, .. } => {
            use crate::modes::interactive::components::command_palette::CommandPaletteOutcome;
            match outcome.take() {
                None => SelectorTransition::Stay,
                Some(CommandPaletteOutcome::Cancelled) => SelectorTransition::Back,
                // Chain into the chosen command. The palette stays on the
                // stack (hidden) as the parent, so a cancel from the
                // child returns to it.
                Some(CommandPaletteOutcome::Confirmed { action }) => SelectorTransition::Open {
                    action,
                    keep_parents: true,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use aj_conf::{AgentEnv, ContextFile, SystemPrompt, SystemPromptSource};
    use aj_models::types::StreamOptions;
    use tempfile::TempDir;
    use tokio::sync::Mutex as TokioMutex;

    use super::*;
    use crate::modes::interactive::test_support::{
        build_test_world, create_spec, drive_turn, finalized_text_message, one_turn_session,
        resume_spec, scripted_model_info, scripted_run_config,
    };
    use crate::turn::apply_turn_config;

    /// Build an [`AgentEnv`] for use in the helper tests below.
    /// Working directory / OS / date / git root are stubbed: only
    /// `system_prompt`, `context_files`, and `skills` matter for the
    /// startup-notice builder.
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
            verbosity: "default".into(),
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
            verbosity: "default".into(),
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
    fn build_context_notice_strikes_disabled_skill_rows_through_the_tui_hook() {
        let skill = |name: &str, enabled: bool, dmi: bool| aj_conf::skills::Skill {
            name: name.to_string(),
            description: format!("{name} description"),
            path: PathBuf::from(format!("/var/skills/{name}/SKILL.md")),
            enabled,
            disable_model_invocation: dmi,
        };
        let mut env = env_with(Vec::new());
        env.skills = vec![skill("alpha", true, false), skill("beta", false, false)];

        // aj passes its ANSI strikethrough as the `strike` hook, so the
        // disabled row carries the `\x1b[9m` SGR pair and the enabled
        // row stays plain. This is aj's rendering choice, so it can't
        // live with the frontend-agnostic builder in `aj-app`.
        let notice = aj_app::notices::build_context_notice(&env, aj_tui::style::strikethrough);
        let beta =
            aj_tui::style::strikethrough("/var/skills/beta/SKILL.md (skill: beta, disabled)");
        assert!(notice.contains(&beta));
        assert!(notice.contains("/var/skills/alpha/SKILL.md (skill: alpha)"));
        assert_eq!(notice.matches("\x1b[9m").count(), 1);
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
        let evt = warning_event(aj_app::notices::SANDBOX_WARNING);
        match evt {
            AgentEvent::Warning { agent_id, text } => {
                assert_eq!(agent_id, aj_agent::events::AgentId::Main);
                assert_eq!(text, aj_app::notices::SANDBOX_WARNING);
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
            next.world.core.session_id, previous_id,
            "fresh world gets a new session id"
        );
        assert_eq!(
            next.notices,
            vec![format!(
                "Started a fresh session ({}).",
                next.world.core.session_id
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
            next.world.core.session_id, first_id,
            "world bound to the requested session"
        );
        assert!(
            matches!(
                &next.spec,
                SessionSpec::Resume {
                    session_id,
                    entry: SessionEntry::Switch,
                    ..
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
            next.world.core.session_id, previous_id,
            "fallback world resumes the previous session"
        );
        assert!(
            matches!(
                &next.spec,
                SessionSpec::Resume {
                    session_id,
                    entry: SessionEntry::Switch,
                    ..
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
            .core
            .registry
            .get(1)
            .expect("sub-agent retained under id 1");
        {
            let s = sub.lock().await;
            assert_eq!(s.model_info().id, "scripted", "spawn-time model inherited");
            assert_eq!(s.default_thinking(), None, "spawn-time thinking inherited");
            assert_eq!(
                s.session_id(),
                Some(format!("{}:sub:1", world.core.session_id).as_str()),
                "sub-agent cache key scoped to its id at spawn"
            );
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
            assert_eq!(
                s.session_id(),
                Some(format!("{}:sub:1", world.core.session_id).as_str()),
                "no override: sub keeps its scoped cache key"
            );
        }

        // A main turn picks up the new config.
        {
            let mut m = world.core.agent.lock().await;
            apply_turn_config(AgentId::Main, &mut m, &run_config, &no_overrides);
            assert_eq!(m.model_info().id, "changed");
            assert_eq!(m.default_thinking(), Some(ThinkingConfig::High));
            assert_eq!(
                m.session_id(),
                Some(world.core.session_id.as_str()),
                "main agent carries the bare session id as its cache key"
            );
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
            .core
            .registry
            .get(1)
            .expect("sub-agent retained under id 1");

        // Stage a thinking + speed override only: the model axis is
        // untouched.
        {
            let mut overrides = world.core.sub_overrides.lock().expect("overrides poisoned");
            let entry = overrides.entry(1).or_default();
            entry.thinking = Some(Some(ThinkingConfig::High));
            entry.speed = Some(Some(Speed::Fast));
        }
        {
            let mut s = sub.lock().await;
            apply_turn_config(
                AgentId::Sub(1),
                &mut s,
                &run_config,
                &world.core.sub_overrides,
            );
            assert_eq!(s.default_thinking(), Some(ThinkingConfig::High));
            assert_eq!(
                s.model_info().id,
                "scripted",
                "no bundle override: spawn-time model kept"
            );
        }

        // Stage a bundle override on top: the model swaps too.
        {
            let mut overrides = world.core.sub_overrides.lock().expect("overrides poisoned");
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
            apply_turn_config(
                AgentId::Sub(1),
                &mut s,
                &run_config,
                &world.core.sub_overrides,
            );
            apply_turn_config(
                AgentId::Sub(1),
                &mut s,
                &run_config,
                &world.core.sub_overrides,
            );
            assert_eq!(s.model_info().id, "override-model");
            assert_eq!(s.default_thinking(), Some(ThinkingConfig::High));
            assert_eq!(
                s.session_id(),
                Some(format!("{}:sub:1", world.core.session_id).as_str()),
                "bundle override re-scopes the cache key to the sub's id"
            );
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
        while let Ok(event) = world.core.event_rx.try_recv() {
            world
                .pump
                .handle(&mut world.core.lifecycle, &mut tui, &event);
        }
        (world, run_config, tui)
    }

    /// An empty config-layers handle for tests that drive
    /// [`handle_selector_outcome`] for session-scoped overlays (which
    /// never persist). The user layer is the default config, no project
    /// layer, and no project path.
    fn empty_layers() -> Arc<std::sync::Mutex<ConfigLayers>> {
        Arc::new(std::sync::Mutex::new(ConfigLayers {
            user: Config::default(),
            project: aj_conf::ConfigLayer::default(),
            project_path: None,
        }))
    }

    /// Mount a thinking selector with a pre-filled outcome and poll
    /// it through [`handle_selector_outcome`] for `target`, against
    /// the given default `config` so a test can assert whether the
    /// persisted default was touched.
    async fn confirm_thinking(
        tui: &mut Tui,
        world: &mut SessionWorld,
        run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
        config: &Arc<std::sync::Mutex<Config>>,
        target: AgentId,
        level: Option<ThinkingConfig>,
    ) -> SelectorTransition {
        let theme = ThemeHandle::new(Theme::bundled_dark());
        let inner = ThinkingSelectorComponent::new(select_list_theme(&theme), None);
        let outcome = inner.outcome_handle();
        let handle = tui.show_overlay(Box::new(inner), palette_overlay_options());
        outcome.set(ThinkingSelectorOutcome::Confirmed(level));
        let dir = TempDir::new().expect("tempdir");
        let auth = AuthStorage::new(dir.path().join("auth.json"));
        handle_selector_outcome(
            tui,
            &OpenSelector::Thinking {
                handle,
                outcome,
                target,
            },
            &auth,
            Arc::clone(run_config),
            Arc::clone(config),
            &empty_layers(),
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

    /// Read the settings the main agent's user thread folds to.
    async fn main_thread_settings(
        log: &Arc<TokioMutex<ConversationLog>>,
    ) -> aj_session::SessionSettings {
        let log = log.lock().await;
        let filter = ThreadFilter::USER;
        let head = log.latest_leaf(filter).expect("user thread has a leaf");
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
        let config = Arc::new(std::sync::Mutex::new(Config::default()));

        let outcome = confirm_thinking(
            &mut tui,
            &mut world,
            &run_config,
            &config,
            AgentId::Sub(1),
            Some(ThinkingConfig::High),
        )
        .await;

        match outcome {
            SelectorTransition::Close(effects) => assert_eq!(
                effects.notice.as_deref(),
                Some("Thinking effort set to high for agent 1.")
            ),
            _ => panic!("expected the selector to close"),
        }
        {
            let overrides = world.core.sub_overrides.lock().expect("overrides poisoned");
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
            sub_thread_settings(&world.core.log, 1)
                .await
                .thinking
                .as_deref(),
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
        let config = Arc::new(std::sync::Mutex::new(Config::default()));

        let outcome = confirm_thinking(
            &mut tui,
            &mut world,
            &run_config,
            &config,
            AgentId::Sub(99),
            Some(ThinkingConfig::High),
        )
        .await;

        match outcome {
            SelectorTransition::Close(effects) => {
                assert_eq!(
                    effects.notice.as_deref(),
                    Some("This agent can't be prompted.")
                );
            }
            _ => panic!("expected the selector to close"),
        }
        assert!(
            world
                .core
                .sub_overrides
                .lock()
                .expect("overrides poisoned")
                .is_empty(),
            "nothing staged"
        );
        let cfg = run_config.lock().expect("run config mutex poisoned");
        assert_eq!(cfg.thinking, None, "run config untouched");
    }

    /// The `/thinking` overlay command for the main agent is
    /// session-scoped: it stages into the run config and records on
    /// the user thread (so a resume of this session restores it) but
    /// leaves `config.toml`'s persisted default untouched.
    #[tokio::test]
    async fn thinking_confirm_for_main_is_session_scoped() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("unused")]);
        let mut world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &ThemeHandle::new(Theme::bundled_dark()), true);
        while let Ok(event) = world.core.event_rx.try_recv() {
            world
                .pump
                .handle(&mut world.core.lifecycle, &mut tui, &event);
        }

        let config = Arc::new(std::sync::Mutex::new(Config::default()));
        let baseline_thinking = config.lock().expect("config mutex poisoned").thinking;

        let outcome = confirm_thinking(
            &mut tui,
            &mut world,
            &run_config,
            &config,
            AgentId::Main,
            Some(ThinkingConfig::High),
        )
        .await;

        match outcome {
            SelectorTransition::Close(effects) => {
                assert_eq!(
                    effects.notice.as_deref(),
                    Some("Thinking effort set to high.")
                )
            }
            _ => panic!("expected the selector to close"),
        }
        assert_eq!(
            run_config
                .lock()
                .expect("run config mutex poisoned")
                .thinking,
            Some(ThinkingConfig::High),
            "run config staged for this session"
        );
        assert_eq!(
            main_thread_settings(&world.core.log)
                .await
                .thinking
                .as_deref(),
            Some("high"),
            "change recorded on the user thread so a resume restores it"
        );
        assert_eq!(
            config.lock().expect("config mutex poisoned").thinking,
            baseline_thinking,
            "config.toml default left unchanged"
        );
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
        outcome.set(ModelSelectorOutcome::Confirmed(info.clone()));

        let result = handle_selector_outcome(
            &mut tui,
            &OpenSelector::Model {
                handle,
                outcome,
                target: AgentId::Sub(1),
            },
            &auth,
            Arc::clone(&run_config),
            Arc::new(std::sync::Mutex::new(Config::default())),
            &empty_layers(),
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
            SelectorTransition::Close(effects) => assert_eq!(
                effects.notice.as_deref(),
                Some("Model set to claude-x (anthropic/claude-x) for agent 1.")
            ),
            _ => panic!("expected the selector to close"),
        }
        {
            let overrides = world.core.sub_overrides.lock().expect("overrides poisoned");
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
            sub_thread_settings(&world.core.log, 1).await.model,
            Some(("anthropic".to_string(), "claude-x".to_string())),
            "change recorded on the sub thread"
        );
        let cfg = run_config.lock().expect("run config mutex poisoned");
        assert_eq!(cfg.model_info.id, "scripted", "run config untouched");
    }

    /// The `/model` overlay command for the main agent is
    /// session-scoped: it stages the swap into the run config and
    /// records it on the user thread (so a resume of this session
    /// restores it) but leaves `config.toml`'s persisted default
    /// untouched.
    #[tokio::test]
    async fn model_confirm_for_main_is_session_scoped() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("unused")]);
        let mut world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        let theme = ThemeHandle::new(Theme::bundled_dark());
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &theme, true);
        while let Ok(event) = world.core.event_rx.try_recv() {
            world
                .pump
                .handle(&mut world.core.lifecycle, &mut tui, &event);
        }

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
        let auth = AuthStorage::new(dir.path().join("auth.json"));
        use crate::modes::interactive::components::model_selector::ModelSelectorComponent;
        let inner =
            ModelSelectorComponent::new(select_list_theme(&theme), vec![info.clone()], None, None);
        let outcome = inner.outcome_handle();
        let handle = tui.show_overlay(Box::new(inner), palette_overlay_options());
        outcome.set(ModelSelectorOutcome::Confirmed(info.clone()));

        let config = Arc::new(std::sync::Mutex::new(Config::default()));

        let result = handle_selector_outcome(
            &mut tui,
            &OpenSelector::Model {
                handle,
                outcome,
                target: AgentId::Main,
            },
            &auth,
            Arc::clone(&run_config),
            Arc::clone(&config),
            &empty_layers(),
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
            SelectorTransition::Close(effects) => assert_eq!(
                effects.notice.as_deref(),
                Some("Model set to claude-x (anthropic/claude-x).")
            ),
            _ => panic!("expected the selector to close"),
        }
        assert_eq!(
            run_config
                .lock()
                .expect("run config mutex poisoned")
                .model_key,
            ("anthropic".to_string(), "claude-x".to_string()),
            "run config staged for this session"
        );
        assert_eq!(
            main_thread_settings(&world.core.log).await.model,
            Some(("anthropic".to_string(), "claude-x".to_string())),
            "change recorded on the user thread so a resume restores it"
        );
        let cfg = config.lock().expect("config mutex poisoned");
        assert_eq!(cfg.model_api, None, "config.toml default left unchanged");
        assert_eq!(cfg.model_name, None, "config.toml default left unchanged");
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

        let (task_id, cancel) = world.core.task_registry.register(
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
        outcome.set(AgentPickerOutcome::KillTask(task_id));

        let auth = AuthStorage::new(dir.path().join("auth.json"));
        let result = handle_selector_outcome(
            &mut tui,
            &OpenSelector::AgentPicker { handle, outcome },
            &auth,
            Arc::clone(&run_config),
            Arc::new(std::sync::Mutex::new(Config::default())),
            &empty_layers(),
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
            SelectorTransition::Close(effects) => {
                assert_eq!(
                    effects.notice,
                    Some(format!("Killing background task #{task_id}."))
                )
            }
            _ => panic!("expected the selector to close"),
        }
    }

    // ---- Selector stack & transitions -------------------------------------

    /// Mount a simple read-only overlay (the help window) and return a
    /// tracking [`OpenSelector`] for it. Handy for exercising the
    /// stack's reveal/teardown mechanics without a full command flow.
    fn show_help_selector(tui: &mut Tui, theme: &ThemeHandle) -> OpenSelector {
        let inner = crate::modes::interactive::components::help_overlay::build_overlay(
            select_list_theme(theme),
        );
        let outcome = inner.outcome_handle();
        let handle = tui.show_overlay(Box::new(inner), palette_overlay_options());
        OpenSelector::Help { handle, outcome }
    }

    /// `back` pops the top overlay, reveals the parent beneath it, and
    /// returns to the chat once the last level is popped, mirroring a
    /// child opened over the palette and Esc'd twice.
    #[test]
    fn selector_stack_back_reveals_parent_then_returns_to_chat() {
        let theme = ThemeHandle::new(Theme::bundled_dark());
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &theme, true);

        let mut stack = SelectorStack::default();
        let parent = show_help_selector(&mut tui, &theme);
        let parent_handle = parent.handle();
        stack.push(&mut tui, parent);
        assert!(tui.is_overlay_focused(&parent_handle));

        // Opening a child over the parent hides the parent (push owns
        // that) and focuses the child.
        let child = show_help_selector(&mut tui, &theme);
        let child_handle = child.handle();
        stack.push(&mut tui, child);
        assert!(tui.is_overlay_focused(&child_handle));
        assert!(!tui.is_overlay_focused(&parent_handle));

        stack.back(&mut tui);
        assert!(
            tui.is_overlay_focused(&parent_handle),
            "popping the child reveals the parent"
        );
        assert!(!tui.is_overlay_focused(&child_handle));

        stack.back(&mut tui);
        assert!(stack.is_empty());
        assert!(
            !tui.is_overlay_focused(&parent_handle),
            "popping the last level returns to the chat"
        );
    }

    /// `close_all` tears every level down in one shot.
    #[test]
    fn selector_stack_close_all_drains_every_level() {
        let theme = ThemeHandle::new(Theme::bundled_dark());
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &theme, true);

        let mut stack = SelectorStack::default();
        let parent = show_help_selector(&mut tui, &theme);
        let parent_handle = parent.handle();
        stack.push(&mut tui, parent);
        let child = show_help_selector(&mut tui, &theme);
        let child_handle = child.handle();
        stack.push(&mut tui, child);

        stack.close_all(&mut tui);
        assert!(stack.is_empty());
        assert!(!tui.is_overlay_focused(&parent_handle));
        assert!(!tui.is_overlay_focused(&child_handle));
    }

    /// Poll an agent picker carrying `outcome_value` and return the
    /// transition the host would apply.
    async fn poll_agent_picker(outcome_value: AgentPickerOutcome) -> SelectorTransition {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("unused")]);
        let mut world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        let mut tui = Tui::new(Box::new(StubTerminal));
        let theme = ThemeHandle::new(Theme::bundled_dark());
        build_layout(&mut tui, &theme, true);

        let inner = AgentPickerComponent::new(
            select_list_theme(&theme),
            Vec::new(),
            Vec::new(),
            AgentId::Main,
        );
        let outcome = inner.outcome_handle();
        let handle = tui.show_overlay(Box::new(inner), palette_overlay_options());
        outcome.set(outcome_value);

        let auth = AuthStorage::new(dir.path().join("auth.json"));
        handle_selector_outcome(
            &mut tui,
            &OpenSelector::AgentPicker { handle, outcome },
            &auth,
            Arc::clone(&run_config),
            Arc::new(std::sync::Mutex::new(Config::default())),
            &empty_layers(),
            &[],
            &mut world,
            &theme,
            &RenderSettings::new(false, false, true),
            &mut ThemeWatch {
                _guard: None,
                rx: None,
            },
        )
        .await
    }

    /// Confirming a task row drills into the viewer: a drain-the-stack
    /// open of [`CommandAction::OpenTaskOutput`], so the picker is torn
    /// down rather than kept as a parent.
    #[tokio::test]
    async fn agent_picker_confirm_task_drills_into_the_viewer() {
        let transition = poll_agent_picker(AgentPickerOutcome::ConfirmedTask(7)).await;
        assert!(matches!(
            transition,
            SelectorTransition::Open {
                action: CommandAction::OpenTaskOutput { id: 7 },
                keep_parents: false,
            }
        ));
    }

    /// Cancelling the picker steps one level back.
    #[tokio::test]
    async fn agent_picker_cancel_steps_back() {
        let transition = poll_agent_picker(AgentPickerOutcome::Cancelled).await;
        assert!(matches!(transition, SelectorTransition::Back));
    }

    /// Drive a freshly-opened command palette with `key` (Enter to
    /// confirm the selected command, Esc to cancel) and return the
    /// transition the host would apply.
    async fn poll_command_palette(key: aj_tui::keys::InputEvent) -> SelectorTransition {
        use crate::modes::interactive::components::command_palette::CommandPaletteComponent;
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("unused")]);
        let mut world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        let theme = ThemeHandle::new(Theme::bundled_dark());
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &theme, true);

        let inner = CommandPaletteComponent::new(select_list_theme(&theme), 13);
        let outcome = inner.outcome_handle();
        let handle = tui.show_overlay(Box::new(inner), palette_overlay_options());
        // Route the key to the focused palette so it writes an outcome.
        tui.handle_input(&key);

        let auth = AuthStorage::new(dir.path().join("auth.json"));
        handle_selector_outcome(
            &mut tui,
            &OpenSelector::Palette { handle, outcome },
            &auth,
            Arc::clone(&run_config),
            Arc::new(std::sync::Mutex::new(Config::default())),
            &empty_layers(),
            &[],
            &mut world,
            &theme,
            &RenderSettings::new(false, false, true),
            &mut ThemeWatch {
                _guard: None,
                rx: None,
            },
        )
        .await
    }

    /// Confirming a command in the palette chains into it as a
    /// keep-parents Open, so a cancel from the child returns to the
    /// palette rather than the chat.
    #[tokio::test]
    async fn command_palette_confirm_chains_keeping_the_palette() {
        let transition = poll_command_palette(aj_tui::keys::Key::enter()).await;
        assert!(matches!(
            transition,
            SelectorTransition::Open {
                keep_parents: true,
                ..
            }
        ));
    }

    /// Cancelling the palette steps back to the chat.
    #[tokio::test]
    async fn command_palette_cancel_steps_back() {
        let transition = poll_command_palette(aj_tui::keys::Key::escape()).await;
        assert!(matches!(transition, SelectorTransition::Back));
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
    /// the normal per-agent machinery: gated on the busy check,
    /// spawned onto the driven set, and the wake drains the notice
    /// into the transcript before the scripted reply.
    #[tokio::test]
    async fn spawn_wake_wakes_idle_owner() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("woke and reacted")]);
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        world
            .core
            .task_registry
            .push_notice(bash_notice(AgentId::Main, 1, "task #1 done"));

        let mut turns = Turns::new();
        turns.spawn_wake(AgentId::Main, &world.core, &run_config, test_policy());

        assert!(
            turns.is_driving(AgentId::Main),
            "wake turn registered in the cancel map"
        );
        let (id, result) = turns.join_next().await.expect("wake turn did not panic");
        assert_eq!(id, AgentId::Main);
        result.expect("wake turn succeeds");

        let agent = world.core.agent.lock().await;
        let transcript = format!("{:?}", agent.messages());
        assert!(
            transcript.contains("<task-notification>\\ntask #1 done\\n</task-notification>"),
            "notice drained into the transcript: {transcript}"
        );
        assert!(
            transcript.contains("woke and reacted"),
            "wake turn ran inference: {transcript}"
        );
        assert!(!world.core.task_registry.has_notices(AgentId::Main));
    }

    /// Dual wake triggers may fire for the same notice (the mid-select
    /// `TaskEnd` trigger and the turn-join trigger). Right after the
    /// first spawn the wake task has not been polled, so `is_running`
    /// is still false and dedup rests entirely on the `is_driving`
    /// half of the busy gate. A double spawn would overwrite the first
    /// turn's cancel token, leaving it uncancellable.
    #[tokio::test]
    async fn spawn_wake_dedups_racing_triggers_via_is_driving() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("woke once")]);
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        world
            .core
            .task_registry
            .push_notice(bash_notice(AgentId::Main, 1, "task #1 done"));

        let mut turns = Turns::new();
        turns.spawn_wake(AgentId::Main, &world.core, &run_config, test_policy());
        turns.spawn_wake(AgentId::Main, &world.core, &run_config, test_policy());

        assert!(turns.is_driving(AgentId::Main));
        assert_eq!(turns.driven(), 1, "second trigger deduped on is_driving");

        let (id, result) = turns.join_next().await.expect("wake turn did not panic");
        assert_eq!(id, AgentId::Main);
        result.expect("wake turn succeeds");
    }

    /// A busy owner (marked running) is left alone — the
    /// in-flight turn's drain points or the turn-completion trigger
    /// deliver the notice instead.
    #[tokio::test]
    async fn spawn_wake_skips_busy_owner() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(Vec::new());
        let mut world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        world
            .core
            .task_registry
            .push_notice(bash_notice(AgentId::Main, 1, "task #1 done"));

        let mut turns = Turns::new();
        world.core.lifecycle.mark_running(AgentId::Main);

        turns.spawn_wake(AgentId::Main, &world.core, &run_config, test_policy());
        assert!(turns.is_empty(), "busy owner must not get a wake turn");
        assert!(
            world.core.task_registry.has_notices(AgentId::Main),
            "notice stays queued for the busy owner's next drain point"
        );
    }

    /// Racing triggers are safe: a wake spawned after the queue was
    /// already drained resolves as `WakeOutcome::Empty` — no
    /// inference, no transcript change (the strict-mode provider
    /// would panic on an unscripted inference).
    #[tokio::test]
    async fn spawn_wake_with_empty_queue_is_a_noop() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(Vec::new());
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");

        let mut turns = Turns::new();
        turns.spawn_wake(AgentId::Main, &world.core, &run_config, test_policy());

        let (id, result) = turns.join_next().await.expect("wake turn did not panic");
        assert_eq!(id, AgentId::Main);
        result.expect("empty wake succeeds");
        let agent = world.core.agent.lock().await;
        assert!(agent.messages().is_empty(), "no-op wake leaves no trace");
    }

    // ---- message queues: submit routing, yank, wake delivery -------------

    /// `spawn_prompt_turn` for an idle, promptable target clears the
    /// editor, registers the turn in the cancel map, and runs the
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

        let mut turns = Turns::new();
        let spawned = spawn_prompt_turn(
            &mut tui,
            &world.core,
            &run_config,
            AgentId::Main,
            "do the thing".to_string(),
            test_policy(),
            &mut turns,
        );
        assert!(spawned);
        assert!(turns.is_driving(AgentId::Main));
        let editor_text = tui
            .get_mut_as::<Editor>(SlotIndex::Editor.idx())
            .map(|e| e.get_text())
            .unwrap();
        assert!(editor_text.is_empty(), "editor cleared on spawn");

        let (id, result) = turns.join_next().await.expect("turn did not panic");
        assert_eq!(id, AgentId::Main);
        result.expect("turn succeeds");
        let agent = world.core.agent.lock().await;
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

        let mut turns = Turns::new();
        let spawned = spawn_prompt_turn(
            &mut tui,
            &world.core,
            &run_config,
            AgentId::Sub(99),
            "x".to_string(),
            test_policy(),
            &mut turns,
        );
        assert!(!spawned);
        assert!(turns.is_empty());
        assert!(!turns.is_driving(AgentId::Sub(99)));
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
            .core
            .message_queues
            .append_follow_up(AgentId::Main, "queued line");
        let yanked = yank_pending_into_editor(
            &mut tui,
            &world.pump,
            &world.core.message_queues,
            AgentId::Main,
        );
        assert!(yanked);
        let editor_text = tui
            .get_mut_as::<Editor>(SlotIndex::Editor.idx())
            .map(|e| e.get_text())
            .unwrap();
        assert_eq!(editor_text, "queued line");
        assert!(!world.core.message_queues.has_pending(AgentId::Main));

        assert!(
            !yank_pending_into_editor(
                &mut tui,
                &world.pump,
                &world.core.message_queues,
                AgentId::Main
            ),
            "nothing pending → false"
        );
    }

    /// A finished turn with a pending follow-up is delivered by the
    /// wake path: `Turns::spawn_wake` runs it as a fresh turn whose user
    /// message is the queued text, and the queue ends empty. (No task
    /// notice is queued — the follow-up alone opens the wake gate.)
    #[tokio::test]
    async fn spawn_wake_delivers_queued_follow_up() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(vec![finalized_text_message("on it")]);
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        world
            .core
            .message_queues
            .append_follow_up(AgentId::Main, "then tidy up");

        let mut turns = Turns::new();
        turns.spawn_wake(AgentId::Main, &world.core, &run_config, test_policy());
        assert!(
            turns.is_driving(AgentId::Main),
            "follow-up alone opens the wake gate"
        );
        let (id, result) = turns.join_next().await.expect("wake turn did not panic");
        assert_eq!(id, AgentId::Main);
        result.expect("wake turn succeeds");

        let agent = world.core.agent.lock().await;
        let transcript = format!("{:?}", agent.messages());
        assert!(
            transcript.contains("then tidy up"),
            "follow-up delivered: {transcript}"
        );
        assert!(!world.core.message_queues.has_pending(AgentId::Main));
    }
}

/// Integration tests that drive the per-session select loop
/// ([`run_session`]) end to end against a headless virtual terminal
/// and a scripted provider.
///
/// These are the binary's only tests that *enter* the loop, so they
/// guard the control flow the seam tests above can't reach: the
/// launch-turn auto-submit, the per-view Ctrl+C cancel/quit ladder,
/// and the agent-bus → pump → chat rendering path. The seam-level
/// behaviors (selector outcomes, session-world rebuild, wake
/// delivery) stay covered by the `tests` module above.
///
/// ## Why `start_paused`
///
/// The loop quits only on a Ctrl+C it reads while idle, but its
/// `biased` select prefers input over the agent-bus arm, so a naively
/// timed quit key could preempt the still-draining turn events and
/// race the assertion. Under `start_paused` the runtime auto-advances
/// the clock only once every task is parked, i.e. once the loop has
/// drained every bus event and gone idle. A feeder task whose
/// `sleep` gates the quit key therefore fires *after* the turn has
/// fully rendered, making the whole flow deterministic with no
/// wall-clock waits.
#[cfg(test)]
mod run_loop_tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
    use aj_models::types::{AssistantMessage, StreamOptions, UserContent};
    use aj_tui::component::Component;
    use aj_tui::keys::Key;
    use aj_tui::tui::Tui;
    use aj_tui_testkit::{VirtualTerminal, strip_ansi};
    use tempfile::TempDir;
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

    use super::*;
    use crate::modes::interactive::components::chat_view::ChatView;
    use crate::modes::interactive::test_support::{
        finalized_text_message, scripted_model_info, scripted_run_config,
    };

    const COLS: u16 = 100;
    const ROWS: u16 = 24;

    /// A [`Shell`] + [`SessionWorld`] wired to a headless virtual
    /// terminal, ready to hand to [`run_session`]. Holds the tempdirs
    /// and the synthetic-input sender so they outlive the run.
    struct RunLoopHarness {
        shell: Shell,
        world: SessionWorld,
        input: UnboundedSender<aj_tui::keys::InputEvent>,
        _sessions_dir: TempDir,
        _auth_dir: TempDir,
    }

    impl RunLoopHarness {
        /// Drive [`run_session`] to completion with `launch` as the
        /// auto-submitted launch turn.
        async fn run(&mut self, launch: Vec<UserContent>) -> SessionExit {
            let mut theme_watch = ThemeWatch {
                _guard: None,
                rx: None,
            };
            let mut history_rx: Option<UnboundedReceiver<PromptHistory>> = None;
            run_session(
                &mut self.shell,
                &mut self.world,
                &mut theme_watch,
                &mut history_rx,
                launch,
            )
            .await
            .expect("run_session returns Ok")
        }

        /// The chat container's rendered scrollback, ANSI stripped.
        fn chat_text(&mut self) -> String {
            let chat = self
                .shell
                .tui
                .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
                .expect("chat slot present");
            strip_ansi(
                &chat
                    .render(usize::from(COLS))
                    .iter()
                    .map(|l| l.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
    }

    /// A run-config snapshot over a [`ScriptedProvider`] whose every
    /// step waits `delay` before emitting. A large `delay` keeps a
    /// turn in flight (parked on the provider) until the cancel token
    /// fires, so a mid-turn Ctrl+C has something to cancel.
    fn scripted_run_config_with_delay(
        messages: Vec<AssistantMessage>,
        delay: Duration,
    ) -> Arc<std::sync::Mutex<RunConfigSnapshot>> {
        Arc::new(std::sync::Mutex::new(RunConfigSnapshot {
            provider: Arc::new(
                ScriptedProvider::from_messages(messages, 0, delay)
                    .on_exhausted(ExhaustedBehavior::Panic),
            ),
            model_info: Arc::new(scripted_model_info()),
            stream_options: StreamOptions::default(),
            thinking: None,
            speed: None,
            model_key: ("scripted".to_string(), "scripted".to_string()),
            session_id: None,
        }))
    }

    /// Build a `Create`-spec harness around `run_config`: a fresh
    /// world plus a started `Tui` over a [`VirtualTerminal`], with the
    /// world installed and a `Shell` assembled exactly as
    /// `InteractiveMode::run` would (minus the process-global config /
    /// auth / catalog loads, which the tests inject).
    async fn build_harness(run_config: Arc<std::sync::Mutex<RunConfigSnapshot>>) -> RunLoopHarness {
        // The chord interceptions look up the process-global
        // keybindings manager. Install the defaults so the `aj.*`
        // actions resolve. Idempotent across tests (serialized below).
        crate::config::keybindings::install_global_manager_defaults();

        let sessions_dir = TempDir::new().expect("sessions tempdir");
        let auth_dir = TempDir::new().expect("auth tempdir");
        let persistence = ConversationPersistence::new(sessions_dir.path().to_path_buf());
        let render_settings = RenderSettings::new(false, false, true);
        let theme = ThemeHandle::new(Theme::bundled_dark());
        let config = Config::default();
        let spec = SessionSpec::Create {
            entry: SessionEntry::Startup,
        };

        let mut world = SessionWorld::build(
            &config,
            &run_config,
            &render_settings,
            &theme,
            &persistence,
            &spec,
            None,
            Arc::new(Vec::new()),
        )
        .expect("build session world");

        let vt = VirtualTerminal::new(COLS, ROWS);
        let input = vt.input_sender();
        let mut tui = Tui::new(Box::new(vt));
        // No bootstrap render, the loop renders on demand. `start`
        // takes the terminal's synthetic-input stream so `next_event`
        // sees keys pushed through `input`.
        tui.set_initial_render(false);
        tui.start().expect("start virtual terminal");
        build_layout(&mut tui, &theme, true);
        world.install(&mut tui, &spec).await;

        let shell = Shell {
            tui,
            theme,
            config_layers: Arc::new(std::sync::Mutex::new(ConfigLayers {
                user: config.clone(),
                project: aj_conf::ConfigLayer::default(),
                project_path: None,
            })),
            config: Arc::new(std::sync::Mutex::new(config)),
            auth: AuthStorage::new(auth_dir.path().join("auth.json")),
            model_catalog: Arc::new(Vec::new()),
            run_config,
            conversation_persistence: persistence,
            render_settings,
            completed_sessions: Vec::new(),
            restore_context: None,
            palette_open_request: Arc::new(AtomicBool::new(false)),
            close_all_request: Arc::new(AtomicBool::new(false)),
            history_open_request: Arc::new(AtomicBool::new(false)),
            agent_picker_open_request: Arc::new(AtomicBool::new(false)),
        };

        RunLoopHarness {
            shell,
            world,
            input,
            _sessions_dir: sessions_dir,
            _auth_dir: auth_dir,
        }
    }

    /// A launch turn auto-submits, streams a scripted reply through
    /// the loop into the chat, the turn round-trips to the on-disk
    /// log, and an idle Ctrl+C quits cleanly.
    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn drives_a_scripted_turn_then_quits_on_idle_ctrl_c() {
        let run_config = scripted_run_config(vec![finalized_text_message("scripted reply here")]);
        let mut h = build_harness(run_config).await;

        // The loop drains the turn and parks. Auto-advance then fires
        // this sleep and the resulting Ctrl+C is read while idle.
        let input = h.input.clone();
        let feeder = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            let _ = input.send(Key::ctrl('c'));
        });

        let exit = h.run(vec![UserContent::text("hello agent")]).await;
        feeder.abort();

        assert!(matches!(exit, SessionExit::Quit));
        let chat = h.chat_text();
        assert!(
            chat.contains("hello agent"),
            "user prompt rendered:\n{chat}"
        );
        assert!(
            chat.contains("scripted reply here"),
            "assistant reply rendered:\n{chat}"
        );

        // The turn reached disk: re-resume the session from its
        // persistence root and confirm the log has a user-thread leaf.
        // The persistence listener flushes `Message` entries on write,
        // so a fresh resume sees the turn even with the world still live.
        let resumed = aj_session::ConversationLog::resume(
            &h.shell.conversation_persistence,
            &h.world.core.session_id,
        )
        .expect("resume the written log");
        assert!(
            resumed.latest_leaf(ThreadFilter::USER).is_some(),
            "the driven turn was persisted"
        );
    }

    /// With nothing running, a single Ctrl+C exits the loop with
    /// [`SessionExit::Quit`]. No feeder or paused clock needed: the
    /// pre-queued key is read on the first idle poll.
    #[tokio::test]
    #[serial_test::serial]
    async fn quits_on_ctrl_c_when_idle() {
        let mut h = build_harness(scripted_run_config(Vec::new())).await;
        h.input.send(Key::ctrl('c')).expect("queue ctrl-c");

        let exit = h.run(Vec::new()).await;
        assert!(matches!(exit, SessionExit::Quit));
    }

    /// A mid-turn Ctrl+C cancels the in-flight turn without freezing
    /// the loop (the R2/A3 lost-cancellation regression): the turn
    /// aborts, the "Turn cancelled." notice renders, the scripted
    /// reply never lands, and the still-live loop accepts a second
    /// Ctrl+C to quit.
    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn ctrl_c_cancels_in_flight_turn_and_keeps_session_alive() {
        // A long provider delay keeps the turn parked until the cancel
        // token fires, so the first Ctrl+C lands mid-turn.
        let run_config = scripted_run_config_with_delay(
            vec![finalized_text_message("late reply")],
            Duration::from_secs(3600),
        );
        let mut h = build_harness(run_config).await;

        let input = h.input.clone();
        let feeder = tokio::spawn(async move {
            // First Ctrl+C: fired once the turn is in flight (parked on
            // the provider). Auto-advance guarantees the abort cascade
            // settles before the second sleep, so the second Ctrl+C
            // sees an idle loop and quits.
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = input.send(Key::ctrl('c'));
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = input.send(Key::ctrl('c'));
        });

        let exit = h.run(vec![UserContent::text("do a slow thing")]).await;
        feeder.abort();

        assert!(matches!(exit, SessionExit::Quit));
        let chat = h.chat_text();
        assert!(
            chat.contains("do a slow thing"),
            "user prompt rendered before cancel:\n{chat}"
        );
        assert!(
            chat.contains("Turn cancelled."),
            "cancel notice rendered:\n{chat}"
        );
        assert!(
            !chat.contains("late reply"),
            "the aborted reply must never land:\n{chat}"
        );
    }
}
