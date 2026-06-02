//! Slash-command registry and dispatcher.
//!
//! Two responsibilities live in this module:
//!
//! 1. **Command catalog.** [`BUILTIN_COMMANDS`] enumerates every
//!    recognised top-level slash command with its category,
//!    description, optional argument hint, and optional keyboard
//!    shortcut. The catalog is consumed by the command-palette
//!    overlay (which groups by category and supports fuzzy search)
//!    and by the help overlay (which lists every entry).
//! 2. **Submit-time dispatch.** [`dispatch`] parses a freshly-
//!    submitted line and returns a [`SlashAction`] describing what
//!    the host should do — open a selector overlay, apply an
//!    inline change (e.g. `/thinking high`), or surface a notice.
//!
//! Adding a new command means adding it to [`BUILTIN_COMMANDS`]
//! *and* to the match arm in [`dispatch`]; both halves live in this
//! file so the pairing stays honest.

use std::sync::Arc;

use aj_models::ThinkingConfig;
use aj_models::registry::{ModelInfo, ModelRegistry};

/// One entry in the static catalog. A static `&str` keeps the list
/// declarable as a `const` and avoids per-startup allocation.
///
/// Field contracts:
/// - `name`: command token (without the leading `/`). Also the
///   dispatch key and the string the palette re-feeds through
///   [`dispatch`] on confirm.
/// - `title`: friendly label shown as the primary column in the
///   command palette and help overlay. Decoupled from `name` so the
///   UI can read cleanly (e.g. category `model` + title `switch`)
///   without the token having to carry the whole phrase.
/// - `category`: short label grouping commands in the palette UI;
///   currently one of `"model"`, `"session"`, `"prompt"`, or `"aj"`.
///   Also part of the palette's fuzzy-search key, so typing a
///   category surfaces the whole group.
/// - `description`: one-line human-readable summary.
/// - `action_id`: optional action ID in the keybindings manager
///   whose bound key globally invokes this command. When set, the
///   palette and help UI resolve the current binding at render
///   time and display it in the shortcut column, so rebinding the
///   action in `~/.aj/config.toml` automatically updates the
///   visible label. `None` means the command has no global
///   keyboard trigger.
pub struct BuiltinCommand {
    pub name: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    pub description: &'static str,
    pub action_id: Option<&'static str>,
}

/// Every recognised top-level slash command.
///
/// Adding a command here is half the work — the matching arm in
/// [`dispatch`] decides what actually happens when the user submits
/// it. Keeping both in this file means a stale arm shows up
/// immediately as a "not yet implemented" branch rather than as a
/// silent no-op.
///
/// All commands are zero-argument: they either open a selector
/// overlay or perform an inline action. The palette and help UI are
/// the discovery surface, so there is no `/command <arg>` syntax to
/// advertise.
pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "thinking",
        title: "thinking",
        category: "model",
        description: "Set the reasoning effort for new turns.",
        action_id: None,
    },
    BuiltinCommand {
        name: "model",
        title: "switch",
        category: "model",
        description: "Switch the active model.",
        action_id: None,
    },
    BuiltinCommand {
        name: "resume",
        title: "resume",
        category: "session",
        description: "Resume a different conversation session.",
        action_id: None,
    },
    BuiltinCommand {
        name: "new",
        title: "new",
        category: "session",
        description: "Start a fresh session (kept on disk).",
        action_id: None,
    },
    BuiltinCommand {
        name: "history",
        title: "history",
        category: "prompt",
        description: "Search and recall a previous prompt.",
        action_id: Some(crate::config::keybindings::ACTION_HISTORY_OPEN),
    },
    BuiltinCommand {
        name: "help",
        title: "help",
        category: "aj",
        description: "Show the command reference.",
        action_id: None,
    },
    BuiltinCommand {
        name: "palette",
        title: "palette",
        category: "aj",
        description: "Open the command palette.",
        action_id: Some(crate::config::keybindings::ACTION_PALETTE_OPEN),
    },
    BuiltinCommand {
        name: "quit",
        title: "quit",
        category: "aj",
        description: "Exit the interactive session.",
        action_id: None,
    },
];

/// Snapshot the model catalog into a flat vector for sharing with
/// the [`SlashAction::OpenModelSelector`] overlay. Loads from
/// [`ModelRegistry::load`] (bundled seed plus optional user cache,
/// plus overrides) and flattens by provider in catalog order so the
/// resulting list preserves the registry's intentional ordering.
pub fn load_model_catalog() -> Arc<Vec<ModelInfo>> {
    let registry = ModelRegistry::load();
    let mut models = Vec::new();
    for provider in registry.providers() {
        for info in registry.models(provider) {
            models.push(info.clone());
        }
    }
    Arc::new(models)
}

/// Parsed outcome of a submitted slash-prefixed line.
///
/// The interactive host applies the action: opening an overlay,
/// mutating agent state inline, or surfacing a notice. Variants
/// not yet wired return [`SlashAction::NotYetImplemented`] so the
/// user sees a clear "soon, not silently dropped" message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAction {
    /// Open the global command palette overlay.
    OpenCommandPalette,
    /// Open the thinking-effort selector overlay. The current
    /// level is highlighted; `Esc` cancels, `Enter` applies.
    OpenThinkingSelector,
    /// Open the model selector overlay. The current model is
    /// pre-selected; `Esc` cancels, `Enter` applies.
    OpenModelSelector,
    /// Open the session selector overlay. The currently-active
    /// session is pre-selected; `Enter` swaps the agent over to the
    /// chosen session, `Esc` cancels.
    OpenSessionSelector,
    /// Open the prompt-history search overlay. `Enter` recalls the
    /// chosen prompt into the editor; `Esc` cancels.
    OpenPromptHistory,
    /// Start a fresh session. The current session is preserved on
    /// disk; the host creates a new [`ConversationLog`], swaps it
    /// in, seeds the agent's transcript empty, and clears the
    /// scrollback.
    NewSession,
    /// Show the slash-command reference. The host opens the help
    /// overlay listing every entry in [`BUILTIN_COMMANDS`].
    Help,
    /// User typed a recognised command whose UI lives in a
    /// follow-up commit. No builtin command maps here today; the
    /// variant is preserved so future deferred commands can land
    /// without re-introducing the type.
    NotYetImplemented {
        command: &'static str,
        message: &'static str,
    },
    /// User asked to quit. The host breaks out of its main loop.
    Quit,
    /// User typed a slash command we don't recognise. The host
    /// renders the embedded suggestion text and clears the editor.
    Unknown { input: String },
}

/// Parse one freshly-submitted editor line and return the
/// corresponding [`SlashAction`].
///
/// The caller is expected to have already trimmed whitespace and
/// verified that the input starts with `'/'`; this function
/// re-asserts via a leading-slash strip so it remains safe to call
/// on any input.
///
/// Commands are zero-argument: only the first whitespace-delimited
/// token is significant, and any trailing tokens are ignored (so
/// stray text after a command name degrades gracefully to opening
/// the relevant selector).
pub fn dispatch(input: &str) -> SlashAction {
    let raw = input.trim();
    let body = raw.strip_prefix('/').unwrap_or(raw);
    let head = body.split_whitespace().next().unwrap_or("");

    match head {
        "thinking" => SlashAction::OpenThinkingSelector,
        "model" => SlashAction::OpenModelSelector,
        "resume" => SlashAction::OpenSessionSelector,
        "history" => SlashAction::OpenPromptHistory,
        "palette" => SlashAction::OpenCommandPalette,
        "new" => SlashAction::NewSession,
        "help" => SlashAction::Help,
        "quit" => SlashAction::Quit,
        _ => SlashAction::Unknown {
            input: raw.to_string(),
        },
    }
}

/// One row in the thinking-level catalog.
///
/// Held as a static so the selector overlay and the status-notice
/// formatter share the same human-readable descriptions without
/// duplicating the table.
pub struct ThinkingLevel {
    pub name: &'static str,
    pub description: &'static str,
    pub config: Option<ThinkingConfig>,
}

/// Levels offered by `/thinking` and the selector overlay, in the
/// order the user sees them. `off` first because it's the cheapest
/// option; the rest ascend in cost.
pub const THINKING_LEVELS: &[ThinkingLevel] = &[
    ThinkingLevel {
        name: "off",
        description: "No extended reasoning",
        config: None,
    },
    ThinkingLevel {
        name: "low",
        description: "Light thinking effort",
        config: Some(ThinkingConfig::Low),
    },
    ThinkingLevel {
        name: "medium",
        description: "Moderate thinking effort",
        config: Some(ThinkingConfig::Medium),
    },
    ThinkingLevel {
        name: "high",
        description: "Deep thinking effort",
        config: Some(ThinkingConfig::High),
    },
    ThinkingLevel {
        name: "xhigh",
        description: "Extended-deep thinking effort",
        config: Some(ThinkingConfig::XHigh),
    },
    ThinkingLevel {
        name: "max",
        description: "Maximum thinking effort",
        config: Some(ThinkingConfig::Max),
    },
];

/// Look up the [`ThinkingConfig`] for a level name. Returns
/// `Some(None)` for `"off"` (i.e. a recognised level whose config
/// is `None`), `Some(Some(...))` for the rest, and `None` for an
/// unrecognised name. Case-insensitive.
pub fn parse_thinking_level(name: &str) -> Option<Option<ThinkingConfig>> {
    let needle = name.to_lowercase();
    THINKING_LEVELS
        .iter()
        .find(|l| l.name == needle)
        .map(|l| l.config.clone())
}

/// Render a [`ThinkingConfig`] back to its catalog name. Used by
/// the selector to highlight the currently-active level on open
/// and by status notices like `Thinking level: medium`.
pub fn thinking_level_name(level: &Option<ThinkingConfig>) -> &'static str {
    match level {
        None => "off",
        Some(ThinkingConfig::Low) => "low",
        Some(ThinkingConfig::Medium) => "medium",
        Some(ThinkingConfig::High) => "high",
        Some(ThinkingConfig::XHigh) => "xhigh",
        Some(ThinkingConfig::Max) => "max",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_quit_and_unknown() {
        assert_eq!(dispatch("/quit"), SlashAction::Quit);
        match dispatch("/nope") {
            SlashAction::Unknown { input } => assert_eq!(input, "/nope"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_thinking_opens_selector() {
        assert_eq!(dispatch("/thinking"), SlashAction::OpenThinkingSelector);
        assert_eq!(dispatch("  /thinking  "), SlashAction::OpenThinkingSelector);
        // Trailing tokens are ignored — commands are zero-argument.
        assert_eq!(
            dispatch("/thinking high"),
            SlashAction::OpenThinkingSelector
        );
    }

    #[test]
    fn dispatch_new_returns_new_session_action() {
        assert_eq!(dispatch("/new"), SlashAction::NewSession);
        // Trailing whitespace and arguments are ignored — `/new`
        // takes no arguments and any trailing tokens are dropped.
        assert_eq!(dispatch("  /new  "), SlashAction::NewSession);
        assert_eq!(dispatch("/new extra"), SlashAction::NewSession);
    }

    #[test]
    fn dispatch_help_returns_help_action() {
        assert_eq!(dispatch("/help"), SlashAction::Help);
        assert_eq!(dispatch("  /help  "), SlashAction::Help);
        assert_eq!(dispatch("/help thinking"), SlashAction::Help);
    }

    #[test]
    fn dispatch_history_opens_prompt_history() {
        assert_eq!(dispatch("/history"), SlashAction::OpenPromptHistory);
        assert_eq!(dispatch("  /history  "), SlashAction::OpenPromptHistory);
    }

    #[test]
    fn dispatch_resume_opens_session_selector() {
        assert_eq!(dispatch("/resume"), SlashAction::OpenSessionSelector);
        assert_eq!(dispatch("  /resume  "), SlashAction::OpenSessionSelector);
        // Trailing tokens are ignored.
        assert_eq!(
            dispatch("/resume fix bug"),
            SlashAction::OpenSessionSelector
        );
    }

    #[test]
    fn dispatch_model_opens_selector() {
        assert_eq!(dispatch("/model"), SlashAction::OpenModelSelector);
        assert_eq!(dispatch("  /model  "), SlashAction::OpenModelSelector);
        // Trailing tokens are ignored.
        assert_eq!(dispatch("/model sonnet"), SlashAction::OpenModelSelector);
    }

    #[test]
    fn parse_thinking_level_handles_all_levels() {
        assert!(matches!(parse_thinking_level("off"), Some(None)));
        assert!(matches!(
            parse_thinking_level("low"),
            Some(Some(ThinkingConfig::Low))
        ));
        assert!(matches!(
            parse_thinking_level("MEDIUM"),
            Some(Some(ThinkingConfig::Medium))
        ));
        assert!(parse_thinking_level("nonsense").is_none());
    }

    #[test]
    fn thinking_level_name_round_trips() {
        for level in THINKING_LEVELS {
            assert_eq!(thinking_level_name(&level.config), level.name);
        }
    }
}
