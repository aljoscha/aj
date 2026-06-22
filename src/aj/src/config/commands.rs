//! Command catalog for the command palette and help overlay.
//!
//! [`COMMANDS`] enumerates every command the palette and the help
//! overlay surface, each carrying the [`CommandAction`] the host
//! performs when the command is chosen. The catalog is consumed by
//! the command-palette overlay (which groups by category and
//! supports fuzzy search) and by the help overlay (which lists every
//! entry). The host applies a [`CommandAction`] in `handle_command`.
//!
//! Because each entry carries its own action, adding a command is a
//! single edit here: append a [`Command`] with the appropriate
//! [`CommandAction`]. There is no separate parser to keep in sync.

use std::sync::Arc;

use aj_models::ThinkingConfig;
use aj_models::registry::{ModelInfo, ModelRegistry};

/// One entry in the static catalog. A static `&str` keeps the list
/// declarable as a `const` and avoids per-startup allocation.
///
/// Field contracts:
/// - `name`: stable command token. The catalog key, the palette's
///   stable item id, and the fuzzy-search anchor.
/// - `title`: friendly label shown as the primary column in the
///   command palette and help overlay. Decoupled from `name` so the
///   UI can read cleanly (e.g. category `model` + title `use`)
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
/// - `action`: what the host does when this command is chosen.
pub struct Command {
    pub name: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    pub description: &'static str,
    pub action_id: Option<&'static str>,
    pub action: CommandAction,
}

/// Every command the palette and help overlay surface.
///
/// All commands are zero-argument: choosing one either opens a
/// selector overlay or performs an inline action. The palette and
/// help UI are the discovery surface.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "thinking",
        title: "thinking effort",
        category: "model",
        description: "Set the reasoning effort for new turns.",
        action_id: None,
        action: CommandAction::OpenThinkingSelector,
    },
    Command {
        name: "model",
        title: "use",
        category: "model",
        description: "Use a different model.",
        action_id: None,
        action: CommandAction::OpenModelSelector,
    },
    Command {
        name: "login",
        title: "login",
        category: "auth",
        description: "Log in to a provider via OAuth (browser flow).",
        action_id: None,
        action: CommandAction::OpenLoginSelector,
    },
    Command {
        name: "logout",
        title: "logout",
        category: "auth",
        description: "Remove a provider's stored credentials.",
        action_id: None,
        action: CommandAction::OpenLogoutSelector,
    },
    Command {
        name: "auth",
        title: "status",
        category: "auth",
        description: "Show authentication status for each provider.",
        action_id: None,
        action: CommandAction::OpenAuthStatus,
    },
    Command {
        name: "usage",
        title: "usage",
        category: "auth",
        description: "Show plan usage and rate-limit status for each provider.",
        action_id: None,
        action: CommandAction::OpenUsageStatus,
    },
    Command {
        name: "resume",
        title: "resume",
        category: "session",
        description: "Resume a different conversation session.",
        action_id: None,
        action: CommandAction::OpenSessionSelector,
    },
    Command {
        name: "new",
        title: "new",
        category: "session",
        description: "Start a fresh session (kept on disk).",
        action_id: None,
        action: CommandAction::NewSession,
    },
    Command {
        name: "info",
        title: "info",
        category: "session",
        description: "Show details and statistics for the current session.",
        action_id: None,
        action: CommandAction::OpenSessionInfo,
    },
    Command {
        name: "compact",
        title: "compact",
        category: "session",
        description: "Summarize earlier context to free up the window.",
        action_id: None,
        action: CommandAction::Compact,
    },
    Command {
        name: "history",
        title: "history",
        category: "prompt",
        description: "Search and recall a previous prompt.",
        action_id: Some(crate::config::keybindings::ACTION_HISTORY_OPEN),
        action: CommandAction::OpenPromptHistory,
    },
    Command {
        name: "agents",
        title: "switch",
        category: "agent",
        description: "Switch which agent's transcript is shown.",
        action_id: Some(crate::config::keybindings::ACTION_AGENT_PICKER),
        action: CommandAction::OpenAgentPicker,
    },
    Command {
        name: "settings",
        title: "settings",
        category: "aj",
        description: "Open the settings window.",
        action_id: None,
        action: CommandAction::OpenSettings,
    },
    Command {
        name: "skills",
        title: "skills",
        category: "aj",
        description: "List discovered skills; toggle to enable or disable.",
        action_id: None,
        action: CommandAction::OpenSkills,
    },
    Command {
        name: "help",
        title: "help",
        category: "aj",
        description: "Show the command reference.",
        action_id: None,
        action: CommandAction::Help,
    },
    Command {
        name: "palette",
        title: "palette",
        category: "aj",
        description: "Open the command palette.",
        action_id: Some(crate::config::keybindings::ACTION_PALETTE_OPEN),
        action: CommandAction::OpenCommandPalette,
    },
    Command {
        name: "quit",
        title: "quit",
        category: "aj",
        description: "Exit the interactive session.",
        action_id: None,
        action: CommandAction::Quit,
    },
];

/// Snapshot the model catalog into a flat vector for sharing with
/// the [`CommandAction::OpenModelSelector`] overlay. Loads from
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

/// What the host does when a command is chosen from the palette (or
/// triggered by its keyboard shortcut).
///
/// The interactive host applies the action: opening an overlay,
/// mutating agent state inline, or surfacing a notice.
///
/// A few variants are internal dispatch targets rather than catalog
/// commands: they carry data, are emitted by the host (not chosen by
/// the user), and are absent from [`COMMANDS`]. `OpenTaskOutput` is
/// one, dispatched by the agent picker drilling into a task's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAction {
    /// Open the global command palette overlay.
    OpenCommandPalette,
    /// Open the thinking-effort selector overlay. The current
    /// level is highlighted; `Esc` cancels, `Enter` applies.
    OpenThinkingSelector,
    /// Open the model selector overlay. The current model is
    /// pre-selected; `Esc` cancels, `Enter` applies.
    OpenModelSelector,
    /// Open the OAuth login provider picker. On confirm the host
    /// starts the provider's browser login flow in a dialog overlay.
    OpenLoginSelector,
    /// Open the logout provider picker (only providers with stored
    /// credentials are listed). On confirm the host removes the
    /// chosen provider's `auth.json` entry.
    OpenLogoutSelector,
    /// Open the read-only authentication-status overlay listing each
    /// provider's credential method and source.
    OpenAuthStatus,
    /// Open the read-only usage overlay listing each provider's plan
    /// usage and rate-limit windows. The reports load asynchronously
    /// after the overlay opens.
    OpenUsageStatus,
    /// Open the read-only session-info overlay: the current session's
    /// id, on-disk path, message and tool-call counts, and recorded
    /// settings. Read-only, so it's safe mid-turn.
    OpenSessionInfo,
    /// Open the session selector overlay. The currently-active
    /// session is pre-selected; `Enter` swaps the agent over to the
    /// chosen session, `Esc` cancels.
    OpenSessionSelector,
    /// Open the prompt-history search overlay. `Enter` recalls the
    /// chosen prompt into the editor; `Esc` cancels.
    OpenPromptHistory,
    /// Open the agent picker overlay. `Enter` switches the chat view
    /// to the chosen agent's transcript; `Esc` cancels.
    OpenAgentPicker,
    /// Open the read-only output viewer for a background bash task,
    /// drilled into from the agent picker. Not a catalog command: it
    /// carries the task id and is dispatched only by the picker's
    /// confirm, never surfaced in the palette or help.
    OpenTaskOutput { id: usize },
    /// Open the settings window overlay. Changes apply (and persist
    /// to `config.toml`) as the user makes them; `Esc` closes.
    OpenSettings,
    /// Open the skills window overlay listing every discovered skill.
    /// Toggles persist to the `disabled_skills` config option as the
    /// user makes them; `Esc` closes.
    OpenSkills,
    /// Start a fresh session. The current session is preserved on
    /// disk; the host creates a new [`ConversationLog`], swaps it
    /// in, seeds the agent's transcript empty, and clears the
    /// scrollback.
    NewSession,
    /// Compact the current session: summarize earlier context and
    /// reseed the agent with the reduced transcript. The interactive
    /// loop intercepts this action and runs it as a tracked task (it
    /// owns the turn machinery `handle_command` lacks), so the
    /// `handle_command` arm for it is a no-op.
    Compact,
    /// Show the command reference. The host opens the help overlay
    /// listing every entry in [`COMMANDS`].
    Help,
    /// User asked to quit. The host breaks out of its main loop.
    Quit,
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

/// Levels offered by the thinking selector, in the order the user
/// sees them. `off` first because it's the cheapest option; the rest
/// ascend in cost.
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
/// and by status notices like `Thinking level: medium`. Delegates
/// to [`aj_models::thinking_config_name`], the canonical vocabulary.
pub fn thinking_level_name(level: &Option<ThinkingConfig>) -> &'static str {
    aj_models::thinking_config_name(level.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

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
