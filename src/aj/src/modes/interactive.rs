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

use aj_agent::bus::SubscriptionHandle;
use aj_agent::events::AgentEvent;
use aj_agent::{Agent, TurnError};
use aj_conf::{AgentEnv, Config, ConfigSpeed, display_path};
use aj_models::messages::Speed;
use aj_models::registry::ModelRegistry;
use aj_session::{
    ConversationLog, ConversationPersistence, ThreadFilter, persistence_listener,
    repair_interrupted_tool_uses, replay,
};
use aj_tools::get_builtin_tools;
use aj_tui::EditorComponent;
use aj_tui::components::editor::Editor;
use aj_tui::container::Container;
use aj_tui::terminal::ProcessTerminal;
use aj_tui::tui::{OverlayAnchor, OverlayHandle, OverlayOptions, SizeValue, Tui, TuiEvent};
use anyhow::{Context, Result};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::SYSTEM_PROMPT;
use crate::cli::args::{Args, Command};
use crate::config::slash_commands::{
    SlashAction, build_autocomplete_provider, dispatch as slash_dispatch, load_model_catalog,
    thinking_level_name,
};
use crate::config::theme::{
    Theme, ThemeHandle, chat_theme, editor_border_color_for_thinking, select_list_theme,
    watch_user_theme,
};
use crate::model::{ResolvedModel, from_model_info};
use crate::modes::interactive::components::footer::Footer;
use crate::modes::interactive::components::header::Header;
use crate::modes::interactive::components::model_selector::{
    ModelIdentityRef, ModelSelectorComponent, ModelSelectorOutcome,
    OutcomeHandle as ModelOutcomeHandle,
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

/// Driver for a single interactive session. Owns the
/// [`aj_tui::tui::Tui`], the registered listeners on the agent's
/// bus, and the [`aj_session::ConversationLog`] for this thread.
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
        // (CLI > env > config.toml > defaults).
        let config = Config::load().unwrap_or_else(|e| {
            tracing::warn!("failed to load config.toml: {e}");
            Config::default()
        });

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
        // `current_model_key` keeps `(provider_id, model_id)` around
        // for the `/model` selector overlay so it can pre-select the
        // active row when opened and track the swap target on
        // confirm.
        let mut agent: Agent;
        let current_model_key: Arc<std::sync::Mutex<(String, String)>>;
        if let Some(name) = &self.args.scripted {
            let model = crate::scripted::resolve_or_explain(name)?;
            let current_provider = self
                .args
                .model_api
                .clone()
                .or_else(|| config.model_api.clone())
                .unwrap_or_else(|| crate::model::DEFAULT_PROVIDER_ID.to_string());
            let current_id = model.model_name();
            current_model_key = Arc::new(std::sync::Mutex::new((current_provider, current_id)));
            // ---- Tools (legacy / scripted path) -----------------------
            let mut tools = get_builtin_tools();
            if !config.disabled_tools.is_empty() {
                tools.retain(|tool| !config.disabled_tools.contains(&tool.name));
            }
            let env = AgentEnv::new();
            agent = Agent::new(
                env,
                SYSTEM_PROMPT,
                tools,
                config.disabled_tools.clone(),
                Arc::clone(&model),
                config.thinking,
            );
        } else {
            // Load the registry once at startup; the same handle
            // also feeds the `/model` selector's model catalog.
            // `ModelRegistry::load` is cheap (single JSON read) so
            // the duplication versus `load_model_catalog` below is
            // not worth deduplicating today.
            let registry = ModelRegistry::load();
            let resolved = crate::model::resolve(
                &registry,
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
            current_model_key = Arc::new(std::sync::Mutex::new((
                resolved.model_info.provider.clone(),
                resolved.model_info.id.clone(),
            )));
            // ---- Tools (registry / real-provider path) ----------------
            let mut tools = get_builtin_tools();
            if !config.disabled_tools.is_empty() {
                tools.retain(|tool| !config.disabled_tools.contains(&tool.name));
            }
            let env = AgentEnv::new();
            let ResolvedModel {
                provider,
                model_info,
                stream_options,
            } = resolved;
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
        }

        // Snapshot the model catalog up-front so the `/model`
        // selector overlay and the editor's argument completer
        // share a single load (registry::load reads JSON twice
        // otherwise — once per consumer).
        let model_catalog = load_model_catalog();

        // ---- Conversation log: resume or create -----------------------
        let threads_dir = Config::get_threads_dir_path()?;
        let conversation_persistence = ConversationPersistence::new(threads_dir);
        let resuming = matches!(self.args.command, Some(Command::Continue { .. }));
        let mut log = match self.args.command {
            Some(Command::Continue {
                thread_id: Some(id),
                prompt: _,
            }) => ConversationLog::resume(&conversation_persistence, &id)?,
            Some(Command::Continue {
                thread_id: None,
                prompt: _,
            }) => match conversation_persistence.get_latest_thread_id()? {
                Some(latest) => ConversationLog::resume(&conversation_persistence, &latest)?,
                None => {
                    eprintln!("No latest conversation to resume; starting a fresh thread.");
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
            let messages: Vec<_> = conversation.messages();
            agent.seed_messages(messages);
        }

        // ---- Theme ----------------------------------------------------
        // Loaded once at startup from `config.theme` (default `dark`).
        // The handle is reused everywhere a component needs theme
        // colors: layout, event pump, selector overlays. A runtime
        // swap re-points the inner [`Theme`] without rebuilding any
        // component — every theme closure resolves through the
        // shared [`RwLock`] on each call.
        let configured_theme = config.theme.as_deref().unwrap_or("light").to_string();
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

        // Install the slash-command autocomplete provider on the
        // editor. Every recognised command (/thinking, /model, …)
        // pops up as a suggestion when the user types `/` at the
        // start of the prompt; argument completion (e.g. `/thinking
        // m` → medium) flows through the same provider.
        if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
            let provider = build_autocomplete_provider(
                agent.env().working_directory.clone(),
                Arc::clone(&model_catalog),
            );
            editor.set_autocomplete_provider(Arc::new(provider));
        }

        // Bootstrap the editor's prompt-history ring from every
        // `*.jsonl` file under the project's threads directory so
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

        // Header: thread id + resume/start banner. Footer: model +
        // working directory (initial snapshot; richer state lands
        // alongside `footer_data` in a follow-up).
        if let Some(header) = tui.get_mut_as::<Header>(SlotIndex::Header.idx()) {
            header.set_thread_id(Some(log.thread_id().to_string()));
            header.set_notice(Some(if resuming {
                "Resuming conversation".to_string()
            } else {
                "Chat with AJ — Ctrl+C to quit".to_string()
            }));
        }
        if let Some(footer) = tui.get_mut_as::<Footer>(SlotIndex::Footer.idx()) {
            let info = agent.model_info();
            footer.set_model(Some(format!("{} @ {}", info.id, info.base_url)));
            footer.set_cwd(Some(format!("{}", agent.env().working_directory.display())));
        }

        // Drive replay events through the pump (post-layout so the
        // chat container has a slot to receive them). Replay never
        // hits the bus, so persistence isn't double-written.
        let mut pump = EventPump::new(chat_theme(&theme), config.hide_thinking_block);
        for event in replay(&log) {
            pump.handle(&mut tui, &event);
        }

        // Startup notices: list the AGENTS.md / CLAUDE.md files
        // stitched into the system prompt, then (unless suppressed)
        // print the sandbox warning. Both flow through the pump's
        // existing `Notice` / `Warning` arms so they appear as dim
        // chat-scrollback rows just above the editor — close enough
        // to where the user starts typing that they're hard to
        // miss, but out of the way of replayed history or the
        // header/footer status panes. Placed *after* replay so a
        // resumed thread's historical content stays on top.
        pump.handle(&mut tui, &notice_event(&build_context_notice(agent.env())));
        if sandbox_warning_enabled() {
            pump.handle(&mut tui, &warning_event(SANDBOX_WARNING));
        }

        // ---- Wrap log + agent in shared handles -----------------------
        // After replay we hand the log to the persistence listener
        // and to any future code path that wants to read the thread
        // id back. Both `log` and `persistence_handle` are bound
        // mutably so the `/session` selector can swap them out for
        // the resumed thread without restarting the binary.
        // The agent goes behind a TokioMutex because the
        // submit-handler spawns a task that holds it across an
        // `agent.prompt(...).await`.
        let mut log = Arc::new(TokioMutex::new(log));
        let agent = Arc::new(TokioMutex::new(agent));
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
        let mut running_task: Option<tokio::task::JoinHandle<Result<(), TurnError>>> = None;
        let mut run_result: Result<()> = Ok(());

        // Slash commands like `/thinking` open an overlay selector.
        // While an overlay is up the editor is not focused, but
        // `tui.show_overlay` already routes input to the overlay,
        // so the main loop's job is just to poll the outcome slot
        // after every input event and close the overlay on result.
        let mut open_selector: Option<OpenSelector> = None;

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
                    set_editor_submit_enabled(&mut tui, true);
                    match join_outcome {
                        Ok(Ok(())) => {}
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
                            // Ctrl+C exits. Future commits will
                            // bind this to "interrupt the running
                            // turn" via the agent's
                            // CancellationToken; for now the
                            // simple "quit" semantics matches the
                            // legacy `aj` binary.
                            if input.is_ctrl('c') || input.is_ctrl('d') {
                                break;
                            }
                            // Ctrl+T toggles the thinking-block
                            // render mode for the session.
                            // Intercepted before `tui.handle_input`
                            // so the editor never sees the keystroke
                            // — it isn't bound there, but the early
                            // intercept also avoids a stray render
                            // pass on the editor side. See
                            // `docs/aj-next-plan.md` §4.4.
                            if input.is_ctrl('t') {
                                let new_value = !pump.hide_thinking_block();
                                pump.set_hide_thinking_block(&mut tui, new_value);
                                let notice = if new_value {
                                    "Thinking blocks: hidden"
                                } else {
                                    "Thinking blocks: visible"
                                };
                                pump.handle(&mut tui, &notice_event(notice));
                                continue;
                            }
                            tui.handle_input(&input);

                            // If a selector overlay is open, the
                            // input was just routed to it. Poll
                            // the overlay's outcome slot; on a
                            // confirm/cancel, close it and apply
                            // the result.
                            if let Some(sel) = open_selector.take() {
                                match handle_selector_outcome(
                                    &mut tui,
                                    sel,
                                    Arc::clone(&agent),
                                    Arc::clone(&current_model_key),
                                    &mut log,
                                    &mut persistence_handle,
                                    &mut pump,
                                    &conversation_persistence,
                                    &theme,
                                ).await {
                                    SelectorPollOutcome::StillOpen(reopened) => {
                                        open_selector = Some(reopened);
                                    }
                                    SelectorPollOutcome::Closed { notice } => {
                                        if let Some(text) = notice {
                                            pump.handle(&mut tui, &notice_event(&text));
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

                                // Slash-command branch — dispatch
                                // before checking `running_task`
                                // so `/quit` works even mid-turn.
                                if trimmed.starts_with('/') {
                                    if let Some(editor) = tui.get_mut_as::<Editor>(
                                        SlotIndex::Editor.idx()
                                    ) {
                                        editor.set_text("");
                                    }
                                    match handle_slash_command(
                                        &mut tui,
                                        Arc::clone(&agent),
                                        Arc::clone(&model_catalog),
                                        Arc::clone(&current_model_key),
                                        Arc::clone(&log),
                                        conversation_persistence.clone(),
                                        &theme,
                                        &trimmed,
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
                                running_task = Some(tokio::spawn(async move {
                                    let mut a = agent_for_run.lock().await;
                                    a.prompt(trimmed).await
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
        // resume hint pointing at the active thread id. Printed
        // *after* [`Tui::stop`] so the bytes land in the user's
        // regular shell scrollback rather than the alternate-screen
        // TUI buffer that gets cleared on exit. Mirrors the legacy
        // `aj` binary's shutdown output (see
        // `aj/src/main.rs::display_usage_summary` + the
        // `Thread:` notice it emits right after).
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

        // Resume hint is gated on "the thread is worth resuming",
        // i.e. it has at least one persisted user-thread leaf.
        // Fresh threads where the user quit without typing
        // anything don't get a hint — there's nothing meaningful
        // to come back to. The check covers both the "user
        // submitted at least one prompt this session" and "we
        // resumed a thread that already had content" paths in one
        // shot since the persistence listener writes user
        // messages inline before the run returns.
        let (resume_eligible, thread_id) = {
            let l = log.lock().await;
            (
                l.latest_leaf(ThreadFilter::USER).is_some(),
                l.thread_id().to_string(),
            )
        };
        if resume_eligible {
            print_resume_hint(&thread_id);
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
    },
    Model {
        handle: OverlayHandle,
        outcome: ModelOutcomeHandle,
    },
    Session {
        handle: OverlayHandle,
        outcome: SessionOutcomeHandle,
    },
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
    /// status line in the chat scrollback.
    Closed { notice: Option<String> },
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

/// Dispatch a freshly-submitted slash-prefixed line.
#[allow(clippy::too_many_arguments)]
async fn handle_slash_command(
    tui: &mut Tui,
    agent: Arc<TokioMutex<Agent>>,
    model_catalog: Arc<Vec<aj_models::registry::ModelInfo>>,
    current_model_key: Arc<std::sync::Mutex<(String, String)>>,
    log: Arc<TokioMutex<ConversationLog>>,
    conversation_persistence: ConversationPersistence,
    theme: &ThemeHandle,
    text: &str,
) -> SlashHandled {
    match slash_dispatch(text) {
        SlashAction::OpenThinkingSelector => {
            let current = {
                let a = agent.lock().await;
                a.default_thinking()
            };
            let component = ThinkingSelectorComponent::new(select_list_theme(theme), current);
            let outcome = component.outcome_handle();
            let options = OverlayOptions {
                anchor: OverlayAnchor::Center,
                width: Some(SizeValue::Absolute(60)),
                ..OverlayOptions::default()
            };
            let handle = tui.show_overlay(Box::new(component), options);
            SlashHandled::Continue {
                selector: Some(OpenSelector::Thinking { handle, outcome }),
                notice: None,
            }
        }
        SlashAction::SetThinking(level) => {
            {
                let mut a = agent.lock().await;
                a.set_default_thinking(level.clone());
            }
            // Mirror the change onto the editor's border tint so
            // the visual cue tracks the active reasoning mode.
            apply_editor_border_for_thinking(tui, theme, level.as_ref());
            let name = thinking_level_name(&level);
            SlashHandled::Continue {
                selector: None,
                notice: Some(format!("Thinking level set to {name}.")),
            }
        }
        SlashAction::OpenModelSelector { initial_query } => {
            // Snapshot the current (provider, id) pair so the
            // overlay can pre-select the active row. The mutex lock
            // is brief — we only need it to read the pair.
            let (provider, id) = {
                let key = current_model_key
                    .lock()
                    .expect("current_model_key mutex poisoned");
                (key.0.clone(), key.1.clone())
            };
            let identity = ModelIdentityRef {
                provider: &provider,
                id: &id,
            };
            // Clone the catalog into the component — the component
            // owns it for the lifetime of the overlay so we don't
            // pay an extra Arc indirection on every rebuild.
            let component = ModelSelectorComponent::new(
                select_list_theme(theme),
                (*model_catalog).clone(),
                Some(&identity),
                initial_query,
            );
            let outcome = component.outcome_handle();
            // Model picker needs more width than the thinking
            // overlay — long ids like `claude-sonnet-4-20250514`
            // benefit from a roomier primary column.
            let options = OverlayOptions {
                anchor: OverlayAnchor::Center,
                width: Some(SizeValue::Absolute(80)),
                ..OverlayOptions::default()
            };
            let handle = tui.show_overlay(Box::new(component), options);
            SlashHandled::Continue {
                selector: Some(OpenSelector::Model { handle, outcome }),
                notice: None,
            }
        }
        SlashAction::OpenSessionSelector { initial_query } => {
            // Read the current thread id so the overlay pre-selects
            // the active row.
            let current_thread_id = {
                let l = log.lock().await;
                l.thread_id().to_string()
            };

            // Load previews synchronously. At our scale (a few
            // hundred threads per project on a busy machine) this
            // is sub-second; if/when that stops being true we'll
            // move it onto `tokio::task::spawn_blocking` with the
            // [`ConversationPersistence::list_thread_previews`]
            // progress callback wired into a header indicator.
            let previews = match conversation_persistence.list_thread_previews(|_, _| {}) {
                Ok(p) => p,
                Err(err) => {
                    return SlashHandled::Continue {
                        selector: None,
                        notice: Some(format!("Failed to list threads: {err}")),
                    };
                }
            };

            if previews.is_empty() {
                return SlashHandled::Continue {
                    selector: None,
                    notice: Some("No threads to switch to in this project.".to_string()),
                };
            }

            let component = SessionSelectorComponent::new(
                select_list_theme(theme),
                previews,
                Some(current_thread_id),
                initial_query,
            );
            let outcome = component.outcome_handle();
            // Session picker carries the richest per-row metadata
            // (preview prefix + msg count + creation date + last-
            // message age), so it gets the most room. 80% of the
            // terminal width on wide terminals lets the description
            // triplet breathe; an 80-column floor keeps the
            // overlay readable on narrower terminals, and the
            // 80%-of-height cap keeps the picker from spilling
            // beyond the visible region on short terminals.
            // `resolve_overlay_layout` clamps the resolved width
            // to the terminal's actual width after the floor is
            // applied, so the picker degrades gracefully when the
            // floor exceeds the terminal width.
            let options = OverlayOptions {
                anchor: OverlayAnchor::Center,
                width: Some(SizeValue::Percent(80.0)),
                min_width: Some(80),
                max_height: Some(SizeValue::Percent(80.0)),
                ..OverlayOptions::default()
            };
            let handle = tui.show_overlay(Box::new(component), options);
            SlashHandled::Continue {
                selector: Some(OpenSelector::Session { handle, outcome }),
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
        SlashAction::InvalidArgument { message, .. } => SlashHandled::Continue {
            selector: None,
            notice: Some(message),
        },
    }
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
    agent: Arc<TokioMutex<Agent>>,
    current_model_key: Arc<std::sync::Mutex<(String, String)>>,
    log: &mut Arc<TokioMutex<ConversationLog>>,
    persistence_handle: &mut SubscriptionHandle,
    pump: &mut EventPump,
    conversation_persistence: &ConversationPersistence,
    theme: &ThemeHandle,
) -> SelectorPollOutcome {
    match selector {
        OpenSelector::Thinking { handle, outcome } => {
            let outcome_value = outcome.lock().expect("thinking outcome poisoned").take();
            match outcome_value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Thinking { handle, outcome }),
                Some(ThinkingSelectorOutcome::Confirmed(level)) => {
                    tui.hide_overlay(&handle);
                    {
                        let mut a = agent.lock().await;
                        a.set_default_thinking(level.clone());
                    }
                    // Mirror the change onto the editor's border
                    // tint so the visual cue tracks the active
                    // reasoning mode.
                    apply_editor_border_for_thinking(tui, theme, level.as_ref());
                    let name = thinking_level_name(&level);
                    SelectorPollOutcome::Closed {
                        notice: Some(format!("Thinking level set to {name}.")),
                    }
                }
                Some(ThinkingSelectorOutcome::Cancelled) => {
                    tui.hide_overlay(&handle);
                    SelectorPollOutcome::Closed { notice: None }
                }
            }
        }
        OpenSelector::Model { handle, outcome } => {
            let outcome_value = outcome.lock().expect("model outcome poisoned").take();
            match outcome_value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Model { handle, outcome }),
                Some(ModelSelectorOutcome::Confirmed(info)) => {
                    tui.hide_overlay(&handle);
                    // Construct a fresh provider handle from the
                    // picked catalog entry. Speed is intentionally
                    // left unset on selector-driven swaps; users who
                    // want the Anthropic fast-inference beta pass
                    // `--speed fast` at startup (it survives the
                    // swap if they re-pick a model that supports
                    // it; otherwise it silently degrades).
                    match from_model_info(info.clone(), None) {
                        Ok(ResolvedModel {
                            provider,
                            model_info,
                            stream_options,
                        }) => {
                            let new_id = model_info.id.clone();
                            let new_url = model_info.base_url.clone();
                            {
                                let mut a = agent.lock().await;
                                a.set_provider(
                                    Arc::clone(&provider),
                                    Arc::clone(&model_info),
                                    stream_options,
                                );
                            }
                            // Track the new key so a re-open of the
                            // selector pre-selects the freshly-
                            // active row.
                            {
                                let mut key = current_model_key
                                    .lock()
                                    .expect("current_model_key mutex poisoned");
                                *key = (info.provider.clone(), info.id.clone());
                            }
                            // Refresh the footer's model line so
                            // the user has visual confirmation of
                            // the swap without waiting for the next
                            // turn boundary.
                            if let Some(footer) = tui.get_mut_as::<Footer>(SlotIndex::Footer.idx())
                            {
                                footer.set_model(Some(format!("{} @ {}", new_id, new_url)));
                            }
                            SelectorPollOutcome::Closed {
                                notice: Some(format!(
                                    "Model set to {} ({}/{}).",
                                    info.name, info.provider, info.id
                                )),
                            }
                        }
                        Err(err) => SelectorPollOutcome::Closed {
                            notice: Some(format!("Failed to switch to {}: {err}", info.name)),
                        },
                    }
                }
                Some(ModelSelectorOutcome::Cancelled) => {
                    tui.hide_overlay(&handle);
                    SelectorPollOutcome::Closed { notice: None }
                }
            }
        }
        OpenSelector::Session { handle, outcome } => {
            let outcome_value = outcome.lock().expect("session outcome poisoned").take();
            match outcome_value {
                None => SelectorPollOutcome::StillOpen(OpenSelector::Session { handle, outcome }),
                Some(SessionSelectorOutcome::Confirmed(preview)) => {
                    tui.hide_overlay(&handle);
                    // No-op when the user picks the row that's
                    // already active. Saves the swap dance (and
                    // the chat-container clear that would briefly
                    // hide the user's scrollback).
                    let is_current = {
                        let l = log.lock().await;
                        l.thread_id() == preview.thread_id
                    };
                    if is_current {
                        return SelectorPollOutcome::Closed {
                            notice: Some(format!("Already on thread {}.", preview.thread_id)),
                        };
                    }
                    match perform_thread_swap(
                        tui,
                        Arc::clone(&agent),
                        log,
                        persistence_handle,
                        pump,
                        conversation_persistence,
                        theme,
                        &preview.thread_id,
                    )
                    .await
                    {
                        Ok(()) => SelectorPollOutcome::Closed {
                            notice: Some(format!("Switched to thread {}.", preview.thread_id)),
                        },
                        Err(err) => SelectorPollOutcome::Closed {
                            notice: Some(format!(
                                "Failed to switch to thread {}: {err}",
                                preview.thread_id
                            )),
                        },
                    }
                }
                Some(SessionSelectorOutcome::Cancelled) => {
                    tui.hide_overlay(&handle);
                    SelectorPollOutcome::Closed { notice: None }
                }
            }
        }
    }
}

/// Swap the agent + log + persistence wiring over to the thread
/// identified by `thread_id`.
///
/// This is the in-process equivalent of `aj continue <id>`:
/// the binary stays up and the user keeps their open editor, but
/// the agent's transcript and the persistence target both rebind
/// to the new thread file.
///
/// Steps:
/// 1. Resume the new [`ConversationLog`] from disk.
/// 2. Repair any interrupted tool uses recorded in that log so
///    the resumed transcript ends cleanly on a user-role message.
/// 3. Build a fresh `Conversation` view from the repaired log and
///    seed it into the agent (`seed_messages`,
///    `seed_sub_agent_counter`).
/// 4. Resolve a system prompt for the new thread: reuse the one
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
/// 7. Refresh the header's thread-id line.
///
/// Returns `Err` if any of the file-level operations fail; in
/// that case the agent's previous bindings are left intact (we
/// only replace `*log` and `*persistence_handle` after the new
/// log has been built and the seed/system-prompt steps have
/// succeeded).
#[allow(clippy::too_many_arguments)]
async fn perform_thread_swap(
    tui: &mut Tui,
    agent: Arc<TokioMutex<Agent>>,
    log: &mut Arc<TokioMutex<ConversationLog>>,
    persistence_handle: &mut SubscriptionHandle,
    pump: &mut EventPump,
    conversation_persistence: &ConversationPersistence,
    theme: &ThemeHandle,
    thread_id: &str,
) -> Result<()> {
    // 1. Resume the new log from disk.
    let mut new_log = ConversationLog::resume(conversation_persistence, thread_id)
        .with_context(|| format!("failed to resume thread {thread_id}"))?;

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
        new_log.linearize(&head, ThreadFilter::USER).messages()
    } else {
        Vec::new()
    };
    let max_agent_id = new_log.max_agent_id();

    // 4. Resolve the system prompt for the new thread. If the
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
    //    between turns (no in-flight prompt is possible here —
    //    `disable_submit` blocks editor submissions while a turn
    //    is running, so no slash command can fire mid-turn), but
    //    the ordering keeps the invariant honest.
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
    //    `Ctrl+T` toggle the user pressed before the swap is
    //    still in effect afterwards.
    if let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx()) {
        chat.clear();
    }
    let hide_thinking_block = pump.hide_thinking_block();
    *pump = EventPump::new(chat_theme(theme), hide_thinking_block);
    {
        let l = log.lock().await;
        for event in replay(&l) {
            pump.handle(tui, &event);
        }
    }

    // 7. Refresh the header so the new thread id is visible in
    //    the top bar without waiting for the next turn boundary.
    if let Some(header) = tui.get_mut_as::<Header>(SlotIndex::Header.idx()) {
        header.set_thread_id(Some(thread_id.to_string()));
        header.set_notice(Some("Resumed conversation".to_string()));
    }

    tui.request_render();
    Ok(())
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
}
