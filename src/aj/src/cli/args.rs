//! [`clap`]-derived argument parsing for the `aj` binary.
//!
//! The `--print` / `--json` toggles select the non-interactive
//! print mode (§4.2); otherwise the binary runs the interactive
//! TUI. Subcommands (`list-threads`, `continue`, `models`)
//! short-circuit before mode dispatch.

use clap::{Parser, Subcommand, ValueEnum};

/// Top-level CLI for the `aj` binary.
#[derive(Parser, Debug)]
#[command(name = "aj")]
#[command(about = "AI-driven agent for software engineering")]
#[command(flatten_help = true)]
pub struct Args {
    /// Model API to use (e.g. `anthropic`, `openai`, `openai-codex`).
    #[arg(long, env = "MODEL_API")]
    pub model_api: Option<String>,

    /// Override the model endpoint URL.
    #[arg(long, env = "MODEL_URL")]
    pub model_url: Option<String>,

    /// Model name to use (provider-specific identifier).
    #[arg(long, env = "MODEL_NAME")]
    pub model_name: Option<String>,

    /// Inference speed mode: `standard` (default) or `fast`
    /// (Anthropic beta `speed` parameter; requires the
    /// `fast-inference-2025-10-02` beta header).
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

    /// Free-form initial prompt. In print mode this is the entire
    /// input; in interactive mode it pre-fills the editor's first
    /// turn. Multiple positional args are joined with spaces; any
    /// `@path` token is expanded by [`crate::cli::file_args`].
    pub prompt: Vec<String>,

    /// Replace the live model with a scripted fake that replays a
    /// canned [`StreamingEvent`](aj_models::streaming::StreamingEvent)
    /// sequence. Useful for eyeballing how the TUI / print mode
    /// renders thinking blocks, tool calls, errors, and the like,
    /// without spending a real API round-trip.
    ///
    /// The argument is the demo name. Pass `--scripted help` (or any
    /// unknown name) to see the catalog. When set the binary skips
    /// `aj_models::create_model` entirely and registers a
    /// [`aj_models::scripted::ScriptedModel`] in its place; every
    /// other code path (TUI, persistence, tools, slash commands)
    /// runs unchanged so the eyeball test exercises the real
    /// surface.
    #[arg(long)]
    pub scripted: Option<String>,

    /// Subcommand selector for the non-conversational utilities
    /// (`list-threads`, `continue`, `models`).
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Output formats supported by print mode (§4.2).
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
    /// List existing conversation threads for this project.
    ListThreads,
    /// Continue a conversation thread (latest if no id given).
    ///
    /// Accepts an optional positional prompt after the thread id:
    /// `aj continue ID prompt words...` resumes the thread
    /// and (in `--print` mode) runs the supplied prompt as the
    /// next turn. With no thread id, the latest thread for the
    /// current project is resumed; supplying a prompt without a
    /// thread id is ambiguous, so callers wanting "latest +
    /// prompt" should pass the thread id explicitly (e.g. via
    /// `aj list-threads`).
    Continue {
        /// Conversation ID to continue. If absent, the latest
        /// thread for this project is resumed.
        thread_id: Option<String>,
        /// Free-form prompt for the resumed run. Only consulted
        /// in `--print` mode today; interactive mode prompts for
        /// input from the editor regardless. Multiple positional
        /// args are joined with spaces; any `@path` token is
        /// expanded by [`crate::cli::file_args`].
        prompt: Vec<String>,
    },
    /// Manage the bundled model catalog at `~/.aj/models.json`.
    Models {
        #[command(subcommand)]
        command: ModelsCommand,
    },
}

/// `aj models <subcommand>` dispatch.
#[derive(Subcommand, Debug)]
#[command(flatten_help = true)]
pub enum ModelsCommand {
    /// Refresh the user model catalog at `~/.aj/models.json` from
    /// `https://models.dev/api.json`.
    Update,
}
