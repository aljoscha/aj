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

use std::sync::Arc;

use aj_agent::events::AgentEvent;
use aj_agent::{Agent, TurnError};
use aj_conf::{AgentEnv, Config, ConfigSpeed};
use aj_models::messages::Speed;
use aj_models::{ModelArgs, create_model};
use aj_session::{
    ConversationLog, ConversationPersistence, ThreadFilter, persistence_listener,
    repair_interrupted_tool_uses, replay,
};
use aj_tools::get_builtin_tools;
use aj_tui::components::editor::Editor;
use aj_tui::terminal::ProcessTerminal;
use aj_tui::tui::{Tui, TuiEvent};
use anyhow::{Context, Result};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::SYSTEM_PROMPT;
use crate::cli::args::{Args, Command};
use crate::config::theme::markdown_theme;
use crate::modes::interactive::components::footer::Footer;
use crate::modes::interactive::components::header::Header;
use crate::modes::interactive::event_pump::{
    EventPump, set_editor_submit_enabled, take_submitted_prompt,
};
use crate::modes::interactive::layout::{SlotIndex, build_layout};

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

        let model_args = ModelArgs {
            api: self
                .args
                .model_api
                .clone()
                .or_else(|| config.model_api.clone())
                .unwrap_or_else(|| "anthropic".to_string()),
            url: self
                .args
                .model_url
                .clone()
                .or_else(|| config.model_url.clone()),
            model_name: self
                .args
                .model_name
                .clone()
                .or_else(|| config.model_name.clone()),
            speed,
        };
        let model = create_model(model_args).context("failed to construct model handle")?;

        // ---- Tools -----------------------------------------------------
        let mut tools = get_builtin_tools();
        if !config.disabled_tools.is_empty() {
            tools.retain(|tool| !config.disabled_tools.contains(&tool.name));
        }

        // ---- Conversation log: resume or create -----------------------
        let threads_dir = Config::get_threads_dir_path()?;
        let conversation_persistence = ConversationPersistence::new(threads_dir);
        let resuming = matches!(self.args.command, Some(Command::Continue { .. }));
        let mut log = match self.args.command {
            Some(Command::Continue {
                thread_id: Some(id),
            }) => ConversationLog::resume(&conversation_persistence, &id)?,
            Some(Command::Continue { thread_id: None }) => {
                match conversation_persistence.get_latest_thread_id()? {
                    Some(latest) => ConversationLog::resume(&conversation_persistence, &latest)?,
                    None => {
                        eprintln!("No latest conversation to resume; starting a fresh thread.");
                        ConversationLog::create(&conversation_persistence)?
                    }
                }
            }
            _ => ConversationLog::create(&conversation_persistence)?,
        };

        // ---- Build the agent ------------------------------------------
        let env = AgentEnv::new();
        let mut agent = Agent::new(
            env,
            SYSTEM_PROMPT,
            tools,
            config.disabled_tools.clone(),
            Arc::clone(&model),
            config.thinking,
        );

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
            let messages: Vec<_> = conversation.messages().into_iter().cloned().collect();
            agent.seed_messages(messages);
        }

        // ---- Build the TUI --------------------------------------------
        let mut tui = Tui::new(Box::new(ProcessTerminal::new()));
        tui.start()
            .map_err(|e| anyhow::anyhow!("failed to start terminal: {e}"))?;
        build_layout(&mut tui);

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
            footer.set_model(Some(format!(
                "{} @ {}",
                agent.model().model_name(),
                agent.model().model_url()
            )));
            footer.set_cwd(Some(format!("{}", agent.env().working_directory.display())));
        }

        // Drive replay events through the pump (post-layout so the
        // chat container has a slot to receive them). Replay never
        // hits the bus, so persistence isn't double-written.
        let mut pump = EventPump::new(markdown_theme());
        for event in replay(&log) {
            pump.handle(&mut tui, &event);
        }

        // ---- Wrap log + agent in shared handles -----------------------
        // After replay we hand the log to the persistence listener
        // and to any future code path that wants to read the thread
        // id back. The agent goes behind a TokioMutex because the
        // submit-handler spawns a task that holds it across an
        // `agent.prompt(...).await`.
        let log = Arc::new(TokioMutex::new(log));
        let _persistence_handle = agent.subscribe(persistence_listener(Arc::clone(&log)));
        let agent = Arc::new(TokioMutex::new(agent));

        // ---- Main event loop ------------------------------------------
        // `running_task` holds the in-flight `agent.prompt(...)`
        // future. We poll it concurrently with the TUI's input and
        // render events; while it's running the editor's
        // `disable_submit` flag is set so a second submission can't
        // queue up before steering/follow-up land.
        let mut running_task: Option<tokio::task::JoinHandle<Result<(), TurnError>>> = None;
        let mut run_result: Result<()> = Ok(());

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
                            tui.handle_input(&input);

                            // The editor swallows printable
                            // input and re-emits a `Submit` when
                            // the user presses Enter. Drain it
                            // and dispatch.
                            if let Some(text) = take_submitted_prompt(&mut tui) {
                                let trimmed = text.trim().to_string();
                                if trimmed.is_empty() {
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
            }
        }

        // Give the spawned task a chance to wind down on shutdown.
        // We don't wait long: the user has already asked to quit,
        // and the agent will be stopped by the dropped subscription.
        if let Some(handle) = running_task {
            handle.abort();
            let _ = handle.await;
        }

        tui.stop();
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
