//! [`clap`]-derived argument parsing for the `aj` binary.
//!
//! The `--print` / `--json` toggles select the non-interactive
//! print mode; otherwise the binary runs the interactive
//! TUI. Subcommands (`list-sessions`, `continue`, `update-models`)
//! short-circuit before mode dispatch.

use clap::{Parser, Subcommand, ValueEnum};

/// Top-level CLI for the `aj` binary.
#[derive(Parser, Debug)]
#[command(name = "aj")]
#[command(about = "AI-driven agent for software engineering")]
#[command(flatten_help = true)]
pub struct Args {
    /// Model API to use (e.g. `anthropic`, `openai`, `openai-codex`,
    /// `openrouter`).
    #[arg(long, env = "MODEL_API")]
    pub model_api: Option<String>,

    /// Override the model endpoint URL.
    #[arg(long, env = "MODEL_URL")]
    pub model_url: Option<String>,

    /// Model name to use (provider-specific identifier).
    #[arg(long, env = "MODEL_NAME")]
    pub model_name: Option<String>,

    /// API key for the resolved provider, applied as a runtime
    /// override for this run only. Takes precedence over env vars
    /// and any credential stored in `~/.aj/auth.json`, and is never
    /// written to disk. Intentionally has no `env =` binding so the
    /// only way to supply it is the explicit flag (provider-specific
    /// env vars like `ANTHROPIC_API_KEY` remain the env path).
    #[arg(long)]
    pub api_key: Option<String>,

    /// Inference speed mode: `standard` (default) or `fast`. Fast mode
    /// is Anthropic-only — it sends `speed: "fast"` in the request body
    /// together with the `fast-mode-2026-02-01` beta header. Models
    /// that don't support fast mode reject the request.
    #[arg(long, env = "AJ_SPEED")]
    pub speed: Option<String>,

    /// Run in non-interactive print mode: stream events to stdout
    /// and exit when the agent reports `AgentEnd`. The trailing
    /// positional `prompt` is required in this mode.
    #[arg(long)]
    pub print: bool,

    /// Output format for print mode. `text` (default) renders
    /// human-readable lines; `json` writes one JSONL event per
    /// line. Implies `--print` when set.
    #[arg(long, value_enum, default_value_t = PrintFormat::Text)]
    pub format: PrintFormat,

    /// Free-form launch input. Each positional argument is either a
    /// `@file` attachment (its contents are wrapped in a `<file>` block
    /// and images are attached inline) or a message; the messages are
    /// joined and combined with the file content into a single launch
    /// turn, which both print and interactive mode auto-submit. See
    /// [`crate::cli::initial_input`] for the full rules.
    pub prompt: Vec<String>,

    /// Replace the live model with a scripted fake that replays a
    /// canned
    /// [`AssistantMessageEvent`](aj_models::streaming::AssistantMessageEvent)
    /// sequence. Useful for eyeballing how the TUI / print mode
    /// renders thinking blocks, tool calls, errors, and the like,
    /// without spending a real API round-trip.
    ///
    /// The argument is the demo name. Pass `--scripted help` (or any
    /// unknown name) to see the catalog. When set the binary skips
    /// registry-driven provider construction entirely and registers a
    /// [`ScriptedProvider`](aj_models::scripted::ScriptedProvider)
    /// in its place; every other code path (TUI, persistence, tools,
    /// commands) runs unchanged so the eyeball test exercises
    /// the real surface.
    #[arg(long)]
    pub scripted: Option<String>,

    /// Subcommand selector for the non-conversational utilities
    /// (`list-sessions`, `continue`, `update-models`).
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Output formats supported by print mode.
#[derive(ValueEnum, Copy, Clone, Eq, PartialEq, Debug, Default)]
#[value(rename_all = "lowercase")]
pub enum PrintFormat {
    /// Human-readable text — same look as the interactive mode's
    /// scrollback, minus colour and progressive updates.
    #[default]
    Text,
    /// One JSONL [`aj_agent::events::AgentEvent`] per line. Stable
    /// shape suitable for piping into another process.
    Json,
}

/// Non-conversational subcommands.
#[derive(Subcommand, Debug)]
#[command(flatten_help = true)]
pub enum Command {
    /// List existing conversation sessions for this project.
    ListSessions,
    /// Continue a conversation session (latest if no id given).
    ///
    /// Accepts optional positional launch input after the session id:
    /// `aj continue ID <args...>` resumes the session and auto-submits
    /// the args (messages and `@file` attachments) as the next turn.
    /// With no session id, the latest session for the current project
    /// is resumed; supplying input without a session id is ambiguous,
    /// so callers wanting "latest + prompt" should pass the session id
    /// explicitly (e.g. via `aj list-sessions`).
    Continue {
        /// Conversation ID to continue. If absent, the latest
        /// session for this project is resumed.
        session_id: Option<String>,
        /// Launch input for the resumed run, interpreted exactly like
        /// the top-level [`Args::prompt`]: a mix of `@file` attachments
        /// and messages, auto-submitted as the next turn.
        prompt: Vec<String>,
    },
    /// Refresh the user model catalog at `~/.aj/models.json` from
    /// `https://models.dev/api.json`.
    UpdateModels,
}
