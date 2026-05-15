use std::sync::{Arc, Mutex};

use aj::SYSTEM_PROMPT;
use aj::cli::AjCli;
use aj::event_bridge::EventBridgeListener;
use aj::prompt_history::{DEFAULT_MAX_ENTRIES, PromptHistory};
use aj_agent::{Agent, TurnError};
use aj_conf::{AgentEnv, Config, ConfigSpeed, display_path};
use aj_models::messages::{ContentBlockParam, Role, Speed};
use aj_models::{ModelArgs, create_model};
use aj_session::{
    ConversationLog, ConversationPersistence, ThreadFilter, persistence_listener,
    repair_interrupted_tool_uses,
};
use aj_tools::get_builtin_tools;
use aj_ui::{AjUi, SubAgentUsage, UsageSummary};
use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::sync::Mutex as TokioMutex;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "aj")]
#[command(about = "AI-driven agent for software engineering")]
#[command(flatten_help = true)]
struct Cli {
    /// Model API to use.
    #[arg(long, env)]
    model_api: Option<String>,

    /// Model endpoint URL.
    #[arg(long, env)]
    model_url: Option<String>,

    /// Model name to use.
    #[arg(long, env)]
    model_name: Option<String>,

    /// Inference speed mode: `standard` (default) or `fast` (Anthropic
    /// beta `speed` parameter; requires the `fast-inference-2025-10-02`
    /// beta header).
    #[arg(long, env)]
    speed: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
#[command(flatten_help = true)]
enum Commands {
    /// List existing conversation threads.
    ListThreads,
    /// Continue a conversation thread (latest if no id given).
    Continue {
        /// Conversation ID to continue (if not provided, continues latest
        /// thread).
        thread_id: Option<String>,
    },
    /// Manage the bundled model catalog.
    Models {
        #[command(subcommand)]
        command: ModelsCommands,
    },
}

#[derive(Subcommand)]
#[command(flatten_help = true)]
enum ModelsCommands {
    /// Refresh the user model catalog at `~/.aj/models.json` from
    /// `https://models.dev/api.json`. Filters to tool-capable Anthropic
    /// and OpenAI models, applies the bundled overrides, and writes the
    /// result atomically. On any fetch or parse failure the existing
    /// cache is left untouched.
    Update,
}

/// A harness that's setting up our logging, environment variables,
/// etc. and drives the [`Agent`] turn-by-turn against a
/// [`ConversationLog`] owned by this binary.
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(true)
        .init();

    // Load config.toml first (lowest priority).
    let config = Config::load().unwrap_or_else(|e| {
        tracing::warn!("failed to load config.toml: {e}");
        Config::default()
    });

    // Load .env files (these set env vars, which are medium priority).
    if let Ok(dotenv_path) = Config::get_dotenv_file_path() {
        tracing::info!("loading .env from {:?}", dotenv_path);
        dotenv::from_path(dotenv_path).ok();
    } else {
        tracing::info!("no .env in config directory");
    }

    dotenv::dotenv().ok();

    // Parse CLI flags (highest priority).
    let cli = Cli::parse();

    // `models update` is a standalone catalog-management subcommand; it
    // doesn't need an API key, history, or agent. Short-circuit before
    // any of that setup, both to keep startup fast and to let users run
    // the command without configured credentials.
    if let Some(Commands::Models { command }) = &cli.command {
        return handle_models_command(command).await;
    }

    let threads_dir = Config::get_threads_dir_path()?;

    // Resolve settings with precedence: CLI flags > env vars > config.toml > defaults.
    let env = AgentEnv::new();
    let conversation_persistence = ConversationPersistence::new(threads_dir);

    // Bootstrap the prompt history from the project's JSONL conversation
    // logs. The resulting `Arc<Mutex<_>>` is shared (via shallow_clone)
    // between the harness UI and the cloned UI handed to the bridge
    // listener, so submissions made during the session immediately show
    // up in the editor's up-arrow stack.
    let mut ui = AjCli::new(Arc::new(Mutex::new(PromptHistory::bootstrap(
        &conversation_persistence,
        DEFAULT_MAX_ENTRIES,
    ))));

    let speed = match cli.speed {
        Some(s) => Some(s.parse::<ConfigSpeed>().map_err(anyhow::Error::msg)?),
        None => config.speed,
    }
    .map(|s| match s {
        ConfigSpeed::Standard => Speed::Standard,
        ConfigSpeed::Fast => Speed::Fast,
    });

    let model_args = ModelArgs {
        api: cli
            .model_api
            .or(config.model_api)
            .unwrap_or_else(|| "anthropic".to_string()),
        url: cli.model_url.or(config.model_url),
        model_name: cli.model_name.or(config.model_name),
        speed,
    };
    let model = create_model(model_args)?;

    let mut tools = get_builtin_tools();
    if !config.disabled_tools.is_empty() {
        tools.retain(|tool| !config.disabled_tools.contains(&tool.name));
        tracing::info!(disabled = ?config.disabled_tools, "filtered disabled tools");
    }

    let mut agent = Agent::new(
        env,
        SYSTEM_PROMPT,
        tools,
        config.disabled_tools.clone(),
        Arc::clone(&model),
        config.thinking,
    );

    // Register the bus -> AjCli rendering bridge before any turns
    // run, so every event the agent emits during inference flows
    // back into the existing renderer. The listener owns its own
    // shallow-cloned `AjCli` (sharing the prompt-history `Arc`) plus
    // lazily-created `SubAgentCli`s for any sub-agent the session
    // spawns; per `docs/aj-next-plan.md` §1.6 sub-agents share the
    // parent's bus, so all of their events flow through this same
    // listener too. The handle stays alive for the rest of the
    // process: we hold it on the stack until `main` returns.
    let _bridge_handle =
        agent.subscribe(EventBridgeListener::new(ui.shallow_clone()).into_listener());

    match cli.command {
        Some(Commands::ListThreads) => {
            list_threads(&conversation_persistence)?;
        }
        Some(Commands::Continue { thread_id }) => {
            let log = if let Some(id) = thread_id {
                ConversationLog::resume(&conversation_persistence, &id)?
            } else if let Some(latest) = conversation_persistence.get_latest_thread_id()? {
                ConversationLog::resume(&conversation_persistence, &latest)?
            } else {
                ui.display_notice("No latest conversation to resume");
                return Ok(());
            };
            run_session(&mut agent, &mut ui, log, /* resuming = */ true).await?;
        }
        Some(Commands::Models { .. }) => {
            // Handled earlier in `main` before agent setup. Reaching
            // this arm would mean we forgot to short-circuit above.
            unreachable!("models subcommand handled before agent setup");
        }
        None => {
            // Default behavior: start a fresh log and run the agent.
            let log = ConversationLog::create(&conversation_persistence)?;
            run_session(&mut agent, &mut ui, log, /* resuming = */ false).await?;
        }
    }

    Ok(())
}

/// Drive a single conversation session: resolve the system prompt,
/// optionally replay history, register the persistence listener, then
/// own the readline loop calling [`Agent::run_turn`] turn-by-turn.
///
/// Per `docs/aj-next-plan.md` §2.4b the agent doesn't reach into the
/// conversation log at all; the binary linearizes the user thread,
/// repairs any interrupted tool calls, seeds the agent's transcript
/// once, and otherwise watches the bus for updates.
async fn run_session(
    agent: &mut Agent,
    ui: &mut AjCli,
    mut log: ConversationLog,
    resuming: bool,
) -> Result<()> {
    // Resolve the system prompt: reuse a persisted one (cache-warm
    // resume) or assemble fresh from the env. On a fresh log we
    // freeze the assembled prompt as the root entry so future
    // resumes reuse the same bytes.
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
    // session don't collide with sub-agent subtrees already
    // persisted in the log.
    if let Some(max_id) = log.max_agent_id() {
        agent.seed_sub_agent_counter(max_id);
    }

    // Replay user-thread history through the renderer (resume only)
    // and seed the agent's in-memory transcript. We read the user
    // thread, then run `repair_interrupted_tool_uses` to write
    // synthesized tool_results for any dangling `tool_use` ids
    // (process-killed-mid-batch recovery), then re-linearize so the
    // seed sees the post-repair view.
    let resumed_thread_id = if let Some(head) = log.latest_leaf(ThreadFilter::USER) {
        let conversation = log.linearize(&head, ThreadFilter::USER);
        if resuming {
            display_conversation_history(ui, &conversation);
        }
        repair_interrupted_tool_uses(&mut log, &conversation)?;
        // Re-linearize after repair to capture any synthesized
        // tool_result message.
        let head = log
            .latest_leaf(ThreadFilter::USER)
            .expect("post-repair head exists when pre-repair head did");
        let conversation = log.linearize(&head, ThreadFilter::USER);
        let messages: Vec<_> = conversation.messages().into_iter().cloned().collect();
        agent.seed_messages(messages);
        Some(log.thread_id().to_string())
    } else {
        None
    };

    // Start-of-session notices.
    if resuming {
        let id = log.thread_id().to_string();
        ui.display_notice(&format!(
            "Resuming conversation {id} (use 'ctrl-c' or 'ctrl-d' to quit)"
        ));
    } else {
        ui.display_notice("Chat with AJ (use 'ctrl-c' or 'ctrl-d' to quit)");
    }

    {
        let model = agent.model();
        ui.display_notice(&format!(
            "Model: {}, at {}",
            model.model_name(),
            model.model_url()
        ));
    }

    display_context(ui, agent.env());

    if std::env::var("AJ_DISABLE_SANDBOX_WARNING").is_err() {
        ui.display_warning(
            "WARNING: AJ has no sandboxing or permission checks. The agent can execute \
             arbitrary commands on your system. Do not use AJ if you don't understand what \
             this means. Set AJ_DISABLE_SANDBOX_WARNING=1 to suppress this warning.",
        );
    }

    // Wrap the log in an Arc<TokioMutex<_>> for the persistence
    // listener; we lock briefly only for the trailing
    // resume-hint thread-id read.
    let log = Arc::new(TokioMutex::new(log));
    // Register the persistence listener AFTER the bridge listener
    // so a disk-write failure (which the listener surfaces as
    // `Err`) aborts the run before the bridge gets to render
    // anything stale. Sub-agents share the parent's bus per §1.6,
    // so this single registration covers nested runs too.
    let _persistence_handle = agent.subscribe(persistence_listener(Arc::clone(&log)));

    // Main turn loop. The binary owns this loop; the agent's
    // `run_turn` runs one assistant cycle and returns. We decide
    // whether to ask for user input by inspecting the last message
    // in the agent's transcript: if the assistant just spoke, we
    // need new input; if the last message is from the user (e.g.
    // a synthesized tool_result on the recovery path), we
    // continue without prompting.
    let mut force_user_input = false;
    let mut sent_any_input = resumed_thread_id.is_some();
    loop {
        let need_user_input = force_user_input
            || match agent.messages().last() {
                Some(last) => matches!(last.role, Role::Assistant),
                None => true,
            };
        force_user_input = false;

        let prompt: Option<String> = if need_user_input {
            match ui.get_user_input() {
                Some(text) => {
                    sent_any_input = true;
                    Some(text)
                }
                None => {
                    // Ctrl-C / Ctrl-D / empty: print the closing
                    // notices and exit.
                    display_usage_summary(ui, agent);
                    if sent_any_input {
                        let id = log.lock().await.thread_id().to_string();
                        ui.display_notice(&format!("Thread: {id} (resume with: aj continue {id})"));
                    }
                    break;
                }
            }
        } else {
            None
        };

        match agent.run_turn(prompt).await {
            Ok(()) => {}
            Err(TurnError::Recoverable(err)) => {
                ui.display_error(&format!("{err:#}"));
                // The pending user message is on disk and in the
                // transcript. Force a fresh prompt next iteration
                // so we don't immediately re-send the same broken
                // request to the model. The user can type a
                // follow-up (which will be appended) or quit.
                force_user_input = true;
                continue;
            }
            Err(TurnError::Fatal(err)) => return Err(err),
        }
    }

    Ok(())
}

/// Render replayed conversation history through the legacy CLI
/// helpers. The agent emits live events through the bus; resumed
/// sessions need to display the prior turns to give the user a
/// visual anchor before the next prompt.
fn display_conversation_history(ui: &mut AjCli, conversation: &aj_session::Conversation) {
    if conversation.is_empty() {
        return;
    }

    for entry in conversation.entries() {
        match &entry.entry {
            aj_session::ConversationEntryKind::Message(msg) => match msg.role {
                Role::User => {
                    let text_content = msg
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            ContentBlockParam::TextBlock { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !text_content.is_empty() {
                        ui.user_text_start("");
                        ui.user_text_stop(&text_content);
                    }
                }
                Role::Assistant => {
                    for content in &msg.content {
                        if let ContentBlockParam::ThinkingBlock { thinking, .. } = content {
                            ui.agent_thinking_start(thinking);
                            ui.agent_thinking_stop();
                        } else if let ContentBlockParam::RedactedThinkingBlock { data } = content {
                            ui.agent_thinking_start(&format!("[Redacted thinking: {data}]"));
                            ui.agent_thinking_stop();
                        }
                    }
                    let text_content = msg
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            ContentBlockParam::TextBlock { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !text_content.is_empty() {
                        ui.agent_text_start("");
                        ui.agent_text_stop(&text_content);
                    }
                }
            },
            aj_session::ConversationEntryKind::UserOutput(out) => match out {
                aj_ui::UserOutput::Notice(msg) => ui.display_notice(msg),
                aj_ui::UserOutput::Error(msg) => ui.display_error(msg),
                aj_ui::UserOutput::ToolResult {
                    tool_name,
                    input,
                    output,
                } => ui.display_tool_result(tool_name, input, output),
                aj_ui::UserOutput::ToolResultDiff {
                    tool_name,
                    input,
                    before,
                    after,
                } => ui.display_tool_result_diff(tool_name, input, before, after),
                aj_ui::UserOutput::ToolError {
                    tool_name,
                    input,
                    error,
                } => ui.display_tool_error(tool_name, input, error),
                aj_ui::UserOutput::TokenUsage(usage) => ui.display_token_usage(usage),
                aj_ui::UserOutput::TokenUsageSummary(summary) => {
                    ui.display_token_usage_summary(summary)
                }
            },
            aj_session::ConversationEntryKind::SystemPrompt { .. } => {
                // Model-facing metadata; not shown to the user.
            }
        }
    }

    ui.display_notice("--- End of conversation history ---");
}

/// Render the startup `Context:` notice listing every agents.md
/// file injected into the agent's system prompt. Mirrors the
/// pre-§2.4b behavior of `Agent::display_context` so the user
/// continues to see which context files are in play.
fn display_context(ui: &mut AjCli, env: &AgentEnv) {
    let text = if env.context_files.is_empty() {
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
    };
    ui.display_notice(&text);
}

/// Render the end-of-session token-usage summary by reading the
/// agent's accumulated counts and per-sub-agent breakdown. Mirrors
/// the pre-§2.4b `Agent::display_usage_summary`.
fn display_usage_summary(ui: &mut AjCli, agent: &Agent) {
    let main = agent.accumulated_usage();
    let main_agent_usage = SubAgentUsage {
        agent_id: None,
        input_tokens: main.input_tokens,
        output_tokens: main.output_tokens,
        cache_creation_tokens: main.cache_creation_input_tokens.unwrap_or(0),
        cache_read_tokens: main.cache_read_input_tokens.unwrap_or(0),
    };

    let mut sub_agent_usage = Vec::new();
    let mut total_sub_input = 0;
    let mut total_sub_output = 0;
    let mut total_sub_cache_creation = 0;
    let mut total_sub_cache_read = 0;

    // Sort by sub-agent id for deterministic output regardless of
    // HashMap iteration order.
    let mut subs: Vec<(usize, &aj_models::messages::Usage)> = agent
        .sub_agent_usage()
        .iter()
        .map(|(id, usage)| (*id, usage))
        .collect();
    subs.sort_by_key(|(id, _)| *id);

    for (agent_id, usage) in subs {
        let sub_usage = SubAgentUsage {
            agent_id: Some(agent_id),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
            cache_read_tokens: usage.cache_read_input_tokens.unwrap_or(0),
        };
        total_sub_input += sub_usage.input_tokens;
        total_sub_output += sub_usage.output_tokens;
        total_sub_cache_creation += sub_usage.cache_creation_tokens;
        total_sub_cache_read += sub_usage.cache_read_tokens;
        sub_agent_usage.push(sub_usage);
    }

    let total_usage = SubAgentUsage {
        agent_id: None,
        input_tokens: main_agent_usage.input_tokens + total_sub_input,
        output_tokens: main_agent_usage.output_tokens + total_sub_output,
        cache_creation_tokens: main_agent_usage.cache_creation_tokens + total_sub_cache_creation,
        cache_read_tokens: main_agent_usage.cache_read_tokens + total_sub_cache_read,
    };

    let summary = UsageSummary {
        main_agent_usage,
        sub_agent_usage,
        total_usage,
    };

    ui.display_token_usage_summary(&summary);
}

fn list_threads(conversation_persistence: &ConversationPersistence) -> Result<()> {
    let threads = conversation_persistence.list_threads()?;

    if threads.is_empty() {
        println!("No conversation threads found for this project.");
        return Ok(());
    }

    for thread in threads {
        println!(
            "{} (modified: {}, {})",
            thread.thread_id, thread.modified, thread.size_display
        );
    }

    Ok(())
}

/// Dispatch for `aj models <subcommand>`. Currently only `update`,
/// but the layer is in place for future catalog management commands.
async fn handle_models_command(command: &ModelsCommands) -> Result<()> {
    match command {
        ModelsCommands::Update => {
            let summary = aj_models::refresh::refresh_user_cache().await?;
            println!("{}", summary.one_line());
            Ok(())
        }
    }
}
