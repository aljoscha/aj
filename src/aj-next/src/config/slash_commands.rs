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
use std::sync::Arc;

use aj_models::ThinkingConfig;
use aj_models::registry::{ModelInfo, ModelRegistry};
use aj_tui::autocomplete::{
    AutocompleteItem, CombinedAutocompleteProvider, CommandEntry, SlashCommand,
};
use aj_tui::fuzzy::FuzzyMatcher;

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
        description: "Switch the active model (Enter to commit; type to filter).",
        argument_hint: Some("[search]"),
    },
    BuiltinCommand {
        name: "session",
        description: "Resume a different conversation thread.",
        argument_hint: Some("[search]"),
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
/// `models` is the catalog used by the `/model` argument completer
/// (passed in rather than loaded here so the caller controls when
/// the registry-load cost is paid and can share the catalog with
/// the selector overlay).
///
/// `/thinking` carries an inline argument completer that fuzzy-
/// matches against the level names so typing `/thinking m` proposes
/// `medium` and `max` without the user opening the overlay. `/model`
/// carries a fuzzy completer over the supplied model catalog so
/// typing `/model sonn` proposes every model whose provider/id/name
/// includes "sonn" as a subsequence. Other commands have no
/// argument completer (yet) — the catalog's `argument_hint` is the
/// only UX hint there.
pub fn build_autocomplete_provider(
    working_directory: PathBuf,
    models: Arc<Vec<ModelInfo>>,
) -> CombinedAutocompleteProvider {
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
            if cmd.name == "model" {
                let models_for_completer = Arc::clone(&models);
                sc = sc.with_argument_completions(move |partial| {
                    model_argument_completions(&models_for_completer, partial)
                });
            }
            CommandEntry::Command(sc)
        })
        .collect();
    CombinedAutocompleteProvider::new(entries, working_directory)
}

/// Snapshot the model catalog into a flat vector for sharing across
/// the autocomplete provider and the [`SlashAction::OpenModelSelector`]
/// overlay. Loads from [`ModelRegistry::load`] (bundled seed plus
/// optional user cache, plus overrides) and flattens by provider in
/// catalog order so the resulting list preserves the registry's
/// intentional ordering.
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

/// Argument completer for `/model`. Returns models whose searchable
/// blob (`provider id name`) fuzzy-matches `partial`. Empty input
/// returns the full catalog so the user can browse.
///
/// Items use `provider/id` as the autocomplete value (the same key
/// the selector commits) so typing `/model an` → accepting the
/// `anthropic/claude-...` suggestion produces an unambiguous
/// dispatch target.
fn model_argument_completions(models: &[ModelInfo], partial: &str) -> Vec<AutocompleteItem> {
    if models.is_empty() {
        return Vec::new();
    }
    let mut matcher = FuzzyMatcher::new();
    let query = partial.trim();
    let mut scored: Vec<(usize, u32)> = Vec::new();
    if query.is_empty() {
        scored.extend((0..models.len()).map(|i| (i, 0u32)));
    } else {
        for (idx, info) in models.iter().enumerate() {
            let haystack = format!("{} {} {}", info.provider, info.id, info.name);
            if let Some(score) = matcher.score(query, &haystack) {
                scored.push((idx, u32::from(score)));
            }
        }
        // Highest score first; tiebreak by catalog order so equally
        // strong matches stay in the registry's intentional sequence.
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    }
    scored
        .into_iter()
        .map(|(idx, _)| {
            let info = &models[idx];
            AutocompleteItem::new(
                &format!("{}/{}", info.provider, info.id),
                &format!("{}/{}", info.provider, info.id),
            )
            .with_description(&info.name)
        })
        .collect()
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
    /// Open the model selector overlay. The current model is
    /// pre-selected; `Esc` cancels, `Enter` applies. `initial_query`
    /// pre-fills the search box so `/model sonn` opens the overlay
    /// already filtered.
    OpenModelSelector { initial_query: Option<String> },
    /// Open the session selector overlay. The currently-active
    /// thread is pre-selected; `Enter` swaps the agent over to the
    /// chosen thread, `Esc` cancels. `initial_query` pre-fills the
    /// search box so `/session fix bug` opens already filtered.
    OpenSessionSelector { initial_query: Option<String> },
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
        "model" => SlashAction::OpenModelSelector {
            initial_query: if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            },
        },
        "session" => SlashAction::OpenSessionSelector {
            initial_query: if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            },
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
        for (input, expected_command) in [("/clear", "clear"), ("/help", "help")] {
            match dispatch(input) {
                SlashAction::NotYetImplemented { command, .. } => {
                    assert_eq!(command, expected_command);
                }
                other => panic!("expected NotYetImplemented for {input}, got {other:?}"),
            }
        }
    }

    #[test]
    fn dispatch_session_no_arg_opens_selector() {
        assert_eq!(
            dispatch("/session"),
            SlashAction::OpenSessionSelector {
                initial_query: None
            }
        );
        assert_eq!(
            dispatch("  /session  "),
            SlashAction::OpenSessionSelector {
                initial_query: None
            }
        );
    }

    #[test]
    fn dispatch_session_with_query_pre_fills_search() {
        assert_eq!(
            dispatch("/session fix bug"),
            SlashAction::OpenSessionSelector {
                initial_query: Some("fix bug".to_string())
            }
        );
    }

    #[test]
    fn dispatch_model_no_arg_opens_selector() {
        assert_eq!(
            dispatch("/model"),
            SlashAction::OpenModelSelector {
                initial_query: None
            }
        );
        assert_eq!(
            dispatch("  /model  "),
            SlashAction::OpenModelSelector {
                initial_query: None
            }
        );
    }

    #[test]
    fn dispatch_model_with_query_pre_fills_search() {
        assert_eq!(
            dispatch("/model sonnet"),
            SlashAction::OpenModelSelector {
                initial_query: Some("sonnet".to_string())
            }
        );
        // Multi-word queries pass through verbatim so the search
        // box receives the same string the user typed.
        assert_eq!(
            dispatch("/model claude opus 4"),
            SlashAction::OpenModelSelector {
                initial_query: Some("claude opus 4".to_string())
            }
        );
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
        let models = Arc::new(Vec::new());
        let _provider = build_autocomplete_provider(PathBuf::from("/tmp"), models);
        // Sanity-check the catalog itself: every command name is
        // unique and non-empty.
        let mut seen = std::collections::HashSet::new();
        for cmd in BUILTIN_COMMANDS {
            assert!(!cmd.name.is_empty(), "command name is empty");
            assert!(seen.insert(cmd.name), "duplicate command name {}", cmd.name);
        }
    }

    #[test]
    fn model_argument_completions_fuzzy_matches_across_provider_id_name() {
        let models = vec![
            ModelInfo {
                id: "claude-sonnet-4-20250514".into(),
                name: "Claude Sonnet 4".into(),
                api: "anthropic-messages".into(),
                provider: "anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
                reasoning: true,
                supports_xhigh: false,
                supports_adaptive_thinking: false,
                input: vec![],
                cost: aj_models::registry::ModelCost::default(),
                context_window: 200000,
                max_tokens: 8192,
                headers: None,
            },
            ModelInfo {
                id: "gpt-5".into(),
                name: "GPT-5".into(),
                api: "openai-responses".into(),
                provider: "openai".into(),
                base_url: "https://api.openai.com".into(),
                reasoning: false,
                supports_xhigh: false,
                supports_adaptive_thinking: false,
                input: vec![],
                cost: aj_models::registry::ModelCost::default(),
                context_window: 200000,
                max_tokens: 8192,
                headers: None,
            },
        ];

        // Substring on the name matches.
        let by_name = model_argument_completions(&models, "sonnet");
        assert!(
            by_name
                .iter()
                .any(|i| i.value == "anthropic/claude-sonnet-4-20250514"),
            "got {by_name:?}"
        );

        // Provider name matches.
        let by_provider = model_argument_completions(&models, "openai");
        assert!(
            by_provider.iter().any(|i| i.value == "openai/gpt-5"),
            "got {by_provider:?}"
        );

        // Empty query returns the full catalog in order.
        let all = model_argument_completions(&models, "");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].value, "anthropic/claude-sonnet-4-20250514");
        assert_eq!(all[1].value, "openai/gpt-5");

        // Nonsense query returns nothing.
        let none = model_argument_completions(&models, "xyzzy");
        assert!(none.is_empty(), "got {none:?}");
    }
}
