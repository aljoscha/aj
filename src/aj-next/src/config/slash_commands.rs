//! Slash-command registry and dispatcher.
//!
//! Two responsibilities live in this module:
//!
//! 1. **Autocomplete catalog.** [`build_autocomplete_provider`]
//!    constructs an [`aj_tui::autocomplete::CombinedAutocompleteProvider`]
//!    seeded with every recognised top-level command. The
//!    interactive editor consults this provider to populate its
//!    pop-up suggestions while the user types a `/` prefix.
//! 2. **Submit-time dispatch.** [`dispatch`] parses a freshly-
//!    submitted line and returns a [`SlashAction`] describing what
//!    the host should do — open a selector overlay, run the
//!    inline form of a command (e.g. `/thinking high`), or surface
//!    a "not yet implemented" notice for commands deferred to a
//!    later commit.
//!
//! The autocomplete catalog and the dispatcher are intentionally
//! decoupled: the catalog is purely a UX hint for typing, while
//! dispatch is a flat match over the trimmed input. Adding a new
//! command means adding it to [`BUILTIN_COMMANDS`] *and* to the
//! match arm in [`dispatch`]; both halves live in this file so the
//! pairing stays honest.

use std::path::PathBuf;

use aj_models::ThinkingConfig;
use aj_tui::autocomplete::{
    AutocompleteItem, CombinedAutocompleteProvider, CommandEntry, SlashCommand,
};

/// One entry in the static catalog. A static `&str` keeps the list
/// declarable as a `const` and avoids per-startup allocation.
pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub argument_hint: Option<&'static str>,
}

/// Every recognised top-level slash command.
///
/// Adding a command here is half the work — the matching arm in
/// [`dispatch`] decides what actually happens when the user submits
/// it. Keeping both in this file means a stale arm shows up
/// immediately as a "not yet implemented" branch rather than as a
/// silent no-op in the autocomplete pop-up.
pub const BUILTIN_COMMANDS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: "thinking",
        description: "Set the default reasoning budget (off / low / medium / high / xhigh / max).",
        argument_hint: Some("[level]"),
    },
    BuiltinCommand {
        name: "model",
        description: "Switch the active model.",
        argument_hint: Some("[search]"),
    },
    BuiltinCommand {
        name: "session",
        description: "Resume a different conversation thread.",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "clear",
        description: "Start a fresh thread (the current one is preserved on disk).",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "help",
        description: "Show the slash-command reference.",
        argument_hint: None,
    },
    BuiltinCommand {
        name: "quit",
        description: "Exit the interactive session.",
        argument_hint: None,
    },
];

/// Build the autocomplete provider seeded with [`BUILTIN_COMMANDS`].
///
/// `working_directory` feeds the `@`-fuzzy and direct-path branches
/// of the underlying [`CombinedAutocompleteProvider`]; the slash
/// catalog is shared regardless of the current working directory.
///
/// `/thinking` carries an inline argument completer that fuzzy-
/// matches against the level names so typing `/thinking m` proposes
/// `medium` and `max` without the user opening the overlay. Other
/// commands have no argument completer (yet) — the catalog's
/// `argument_hint` is the only UX hint there.
pub fn build_autocomplete_provider(working_directory: PathBuf) -> CombinedAutocompleteProvider {
    let entries: Vec<CommandEntry> = BUILTIN_COMMANDS
        .iter()
        .map(|cmd| {
            let mut sc = SlashCommand::new(cmd.name).with_description(cmd.description);
            if let Some(hint) = cmd.argument_hint {
                sc = sc.with_argument_hint(hint);
            }
            if cmd.name == "thinking" {
                sc = sc.with_argument_completions(thinking_argument_completions);
            }
            CommandEntry::Command(sc)
        })
        .collect();
    CombinedAutocompleteProvider::new(entries, working_directory)
}

/// Argument completer for `/thinking`. Returns the levels whose
/// names start with `partial` (case-insensitive). The matcher is
/// deliberately a prefix match rather than fuzzy — the level list
/// is short and predictable, and a strict prefix avoids surprising
/// the user with `xhigh` matches when they typed `m`.
fn thinking_argument_completions(partial: &str) -> Vec<AutocompleteItem> {
    let needle = partial.to_lowercase();
    THINKING_LEVELS
        .iter()
        .filter(|l| l.name.starts_with(&needle))
        .map(|l| AutocompleteItem::new(l.name, l.name).with_description(l.description))
        .collect()
}

/// Parsed outcome of a submitted slash-prefixed line.
///
/// The interactive host applies the action: opening an overlay,
/// mutating agent state inline, or surfacing a notice. Variants
/// not yet wired return [`SlashAction::NotYetImplemented`] so the
/// user sees a clear "soon, not silently dropped" message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAction {
    /// Open the thinking-budget selector overlay. The current
    /// level is highlighted; `Esc` cancels, `Enter` applies.
    OpenThinkingSelector,
    /// Apply the supplied thinking level inline (no overlay) and
    /// surface a status notice. Used for `/thinking <level>`.
    SetThinking(Option<ThinkingConfig>),
    /// User typed a recognised command whose UI lives in a
    /// follow-up commit. The host renders the embedded message as
    /// a notice and clears the editor.
    NotYetImplemented {
        command: &'static str,
        message: &'static str,
    },
    /// User asked to quit. The host breaks out of its main loop.
    Quit,
    /// User typed a slash command we don't recognise. The host
    /// renders the embedded suggestion text and clears the editor.
    Unknown { input: String },
    /// User typed an invalid argument for a recognised command —
    /// e.g. `/thinking nope`. Same handling as [`Self::Unknown`]
    /// but with a more specific message.
    InvalidArgument {
        command: &'static str,
        message: String,
    },
}

/// Parse one freshly-submitted editor line and return the
/// corresponding [`SlashAction`].
///
/// The caller is expected to have already trimmed whitespace and
/// verified that the input starts with `'/'`; this function
/// re-asserts via a leading-slash strip so it remains safe to call
/// on any input.
pub fn dispatch(input: &str) -> SlashAction {
    let raw = input.trim();
    let body = raw.strip_prefix('/').unwrap_or(raw);
    // Split off the first whitespace-delimited token so `/thinking
    // medium` and `/thinking medium  ` behave identically.
    let (head, rest) = match body.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (body, ""),
    };

    match head {
        "thinking" => match parse_thinking_argument(rest) {
            ThinkingArgument::None => SlashAction::OpenThinkingSelector,
            ThinkingArgument::Set(level) => SlashAction::SetThinking(level),
            ThinkingArgument::Invalid(name) => SlashAction::InvalidArgument {
                command: "thinking",
                message: format!(
                    "unknown thinking level '{name}'; expected one of: off, low, medium, high, xhigh, max"
                ),
            },
        },
        "model" => SlashAction::NotYetImplemented {
            command: "model",
            message: "/model: selector not yet implemented; restart with --model-name to switch.",
        },
        "session" => SlashAction::NotYetImplemented {
            command: "session",
            message: "/session: selector not yet implemented; use `aj-next continue <id>` instead.",
        },
        "clear" => SlashAction::NotYetImplemented {
            command: "clear",
            message: "/clear: not yet implemented; restart `aj-next` to begin a fresh thread.",
        },
        "help" => SlashAction::NotYetImplemented {
            command: "help",
            message: "/help: try /thinking. Other commands land in follow-up commits.",
        },
        "quit" => SlashAction::Quit,
        _ => SlashAction::Unknown {
            input: raw.to_string(),
        },
    }
}

/// Argument-parse outcome for `/thinking`.
enum ThinkingArgument {
    /// No argument supplied: open the selector overlay.
    None,
    /// Successfully parsed; `None` means "off".
    Set(Option<ThinkingConfig>),
    /// Argument supplied but not a recognised level.
    Invalid(String),
}

fn parse_thinking_argument(arg: &str) -> ThinkingArgument {
    if arg.is_empty() {
        return ThinkingArgument::None;
    }
    match parse_thinking_level(arg) {
        Some(level) => ThinkingArgument::Set(level),
        None => ThinkingArgument::Invalid(arg.to_string()),
    }
}

/// One row in the thinking-level catalog.
///
/// Held as a static so both the autocomplete completer and the
/// selector overlay surface the same human-readable descriptions
/// without duplicating the table.
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
        description: "Light reasoning budget",
        config: Some(ThinkingConfig::Low),
    },
    ThinkingLevel {
        name: "medium",
        description: "Moderate reasoning budget",
        config: Some(ThinkingConfig::Medium),
    },
    ThinkingLevel {
        name: "high",
        description: "Deep reasoning budget",
        config: Some(ThinkingConfig::High),
    },
    ThinkingLevel {
        name: "xhigh",
        description: "Extended-deep reasoning budget",
        config: Some(ThinkingConfig::XHigh),
    },
    ThinkingLevel {
        name: "max",
        description: "Maximum reasoning budget",
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
    fn dispatch_thinking_no_arg_opens_selector() {
        assert_eq!(dispatch("/thinking"), SlashAction::OpenThinkingSelector);
        assert_eq!(dispatch("  /thinking  "), SlashAction::OpenThinkingSelector);
    }

    #[test]
    fn dispatch_thinking_with_valid_levels() {
        assert_eq!(dispatch("/thinking off"), SlashAction::SetThinking(None));
        assert_eq!(
            dispatch("/thinking high"),
            SlashAction::SetThinking(Some(ThinkingConfig::High))
        );
        // Case-insensitive.
        assert_eq!(
            dispatch("/thinking HIGH"),
            SlashAction::SetThinking(Some(ThinkingConfig::High))
        );
        // Trailing whitespace is fine.
        assert_eq!(
            dispatch("/thinking max  "),
            SlashAction::SetThinking(Some(ThinkingConfig::Max))
        );
    }

    #[test]
    fn dispatch_thinking_invalid_argument() {
        match dispatch("/thinking nope") {
            SlashAction::InvalidArgument { command, message } => {
                assert_eq!(command, "thinking");
                assert!(message.contains("nope"), "got {message:?}");
                assert!(message.contains("low"), "got {message:?}");
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_deferred_commands_return_not_yet_implemented() {
        for (input, expected_command) in [
            ("/model", "model"),
            ("/session", "session"),
            ("/clear", "clear"),
            ("/help", "help"),
        ] {
            match dispatch(input) {
                SlashAction::NotYetImplemented { command, .. } => {
                    assert_eq!(command, expected_command);
                }
                other => panic!("expected NotYetImplemented for {input}, got {other:?}"),
            }
        }
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

    #[test]
    fn thinking_argument_completions_prefix_matches() {
        let items = thinking_argument_completions("m");
        let names: Vec<_> = items.iter().map(|i| i.value.as_str()).collect();
        assert!(names.contains(&"medium"));
        assert!(names.contains(&"max"));
        assert!(!names.contains(&"low"));

        let exact = thinking_argument_completions("off");
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].value, "off");

        // Empty input returns every level.
        let all = thinking_argument_completions("");
        assert_eq!(all.len(), THINKING_LEVELS.len());
    }

    #[test]
    fn build_autocomplete_provider_includes_every_builtin() {
        // `CombinedAutocompleteProvider` doesn't expose its
        // command list directly, but constructing one is a safe
        // smoke test that every entry produces a valid
        // `SlashCommand`.
        let _provider = build_autocomplete_provider(PathBuf::from("/tmp"));
        // Sanity-check the catalog itself: every command name is
        // unique and non-empty.
        let mut seen = std::collections::HashSet::new();
        for cmd in BUILTIN_COMMANDS {
            assert!(!cmd.name.is_empty(), "command name is empty");
            assert!(seen.insert(cmd.name), "duplicate command name {}", cmd.name);
        }
    }
}
