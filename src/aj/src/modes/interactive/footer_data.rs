//! Per-agent state feeding the [`super::components::footer::Footer`].
//!
//! The event pump owns an [`AgentFooters`] store keyed by
//! [`AgentId`]: the Main entry always exists (seeded at
//! construction), Sub entries are created when a sub-agent starts
//! and kept for the pump's lifetime so finished sub-agents (still
//! selectable in the picker) render their final state. Read
//! accessors fall back to the Main entry when the requested agent
//! has none, so the footer always has something coherent to show.
//!
//! The split keeps the [`Footer`](super::components::footer::Footer)
//! component free of wire-level concerns (it accepts pre-built
//! views) and the pump free of display semantics (it forwards
//! events; this module decides what the store exposes).

use std::collections::HashMap;

use aj_agent::events::{AgentId, AgentSettings};
use aj_agent::types::TokenUsage;

use crate::modes::interactive::components::footer::ContextUsage;

/// Displayable state for one agent: its settings identity plus the
/// context-occupancy pair.
#[derive(Debug, Clone)]
struct AgentFooter {
    /// Next-turn settings (provider, model_id, thinking, speed).
    /// Speed is carried but not rendered.
    settings: AgentSettings,
    /// Context window of the settings' model, in tokens. Zero
    /// means unknown and suppresses the footer's occupancy
    /// indicator.
    context_window: u64,
    /// Prompt size of the agent's most recent turn, `None` until
    /// the first `TurnUsage` arrives.
    last_turn_context_tokens: Option<u64>,
}

/// Per-agent footer store: the single source of truth for "what
/// does agent `id` currently run with" plus its context occupancy.
///
/// Holds only strings and scalars; live provider handles stay with
/// the run configuration.
#[derive(Debug, Clone)]
pub struct AgentFooters {
    /// Keyed by agent. The Main entry always exists; Sub entries
    /// are inserted as sub-agents appear and never removed.
    agents: HashMap<AgentId, AgentFooter>,
}

impl AgentFooters {
    /// Build a store seeded with the Main agent's settings and
    /// context window. Main's `last_turn_context_tokens` starts as
    /// `None` so the footer initially renders `?/<window>` until
    /// the first assistant turn lands.
    pub fn new(main_settings: AgentSettings, main_context_window: u64) -> Self {
        let mut agents = HashMap::new();
        agents.insert(
            AgentId::Main,
            AgentFooter {
                settings: main_settings,
                context_window: main_context_window,
                last_turn_context_tokens: None,
            },
        );
        Self { agents }
    }

    /// Insert or replace the settings identity (and context-window
    /// denominator) for `id`, preserving an existing entry's
    /// `last_turn_context_tokens` — a model swap doesn't erase what
    /// the last prompt cost.
    pub fn note_settings(&mut self, id: AgentId, settings: AgentSettings, context_window: u64) {
        let last_turn_context_tokens = self
            .agents
            .get(&id)
            .and_then(|entry| entry.last_turn_context_tokens);
        self.agents.insert(
            id,
            AgentFooter {
                settings,
                context_window,
                last_turn_context_tokens,
            },
        );
    }

    /// Fold a freshly-arrived `TurnUsage` into `id`'s entry.
    ///
    /// The numerator we display is
    /// `turn_input + turn_cache_read + turn_cache_write` — the
    /// size of the prompt that produced the most recent assistant
    /// response. `turn_input` is the non-cached portion;
    /// `turn_cache_read` and `turn_cache_write` are the cached
    /// input tokens — together they sum to the full prompt size.
    /// The assistant's `turn_output` is intentionally excluded: a
    /// prompt's "context occupancy" is what was sent in, not the
    /// response that came back.
    ///
    /// A missing entry is created defensively with empty settings
    /// and an unknown window.
    pub fn record_turn_usage(&mut self, id: AgentId, usage: &TokenUsage) {
        let entry = self.agents.entry(id).or_insert_with(|| AgentFooter {
            settings: AgentSettings {
                provider: String::new(),
                model_id: String::new(),
                thinking: String::new(),
                speed: String::new(),
            },
            context_window: 0,
            last_turn_context_tokens: None,
        });
        entry.last_turn_context_tokens =
            Some(usage.turn_input + usage.turn_cache_read + usage.turn_cache_write);
    }

    /// Build a [`ContextUsage`] view for `id`, falling back to the
    /// Main entry (which always exists) when `id` has none.
    pub fn context_usage(&self, id: AgentId) -> ContextUsage {
        let entry = self.resolve(id);
        ContextUsage {
            tokens: entry.last_turn_context_tokens,
            context_window: entry.context_window,
        }
    }

    /// Overwrite `id`'s context-occupancy numerator. Used after a
    /// compaction reseeds the transcript: no `TurnUsage` follows a
    /// compaction, so without this the footer would keep showing the
    /// pre-compaction occupancy until the next real turn. A missing
    /// entry is left untouched (nothing to display against yet).
    pub fn set_context_tokens(&mut self, id: AgentId, tokens: u64) {
        if let Some(entry) = self.agents.get_mut(&id) {
            entry.last_turn_context_tokens = Some(tokens);
        }
    }

    /// Format the footer's model line, `"<model_id> <thinking>"`,
    /// for `id`, falling back to the Main entry when `id` has none.
    /// Returns `None` when the resolved entry's `model_id` is empty
    /// (e.g. a defensively-created entry) rather than rendering a
    /// garbage line.
    pub fn model_line(&self, id: AgentId) -> Option<String> {
        let settings = &self.resolve(id).settings;
        if settings.model_id.is_empty() {
            return None;
        }
        Some(format!("{} {}", settings.model_id, settings.thinking))
    }

    /// Read back the stored settings snapshot for `id`. No Main
    /// fallback here — callers decide what a missing entry means.
    pub fn settings(&self, id: AgentId) -> Option<&AgentSettings> {
        self.agents.get(&id).map(|entry| &entry.settings)
    }

    /// Entry for `id`, or Main's when `id` has none. Main always
    /// exists, so this never fails.
    fn resolve(&self, id: AgentId) -> &AgentFooter {
        self.agents
            .get(&id)
            .unwrap_or_else(|| &self.agents[&AgentId::Main])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `TokenUsage` snapshot carrying the supplied
    /// per-turn deltas. The `accumulated_*` fields are zeroed —
    /// matching the wire-level pre-add semantic for an agent that
    /// hasn't run any prior turns — but
    /// [`AgentFooters::record_turn_usage`] only reads `turn_*`, so
    /// the accumulator value is irrelevant to the tests below and
    /// we keep it constant for clarity.
    fn token_usage(input: u64, output: u64, cache_write: u64, cache_read: u64) -> TokenUsage {
        TokenUsage {
            accumulated_input: 0,
            turn_input: input,
            accumulated_output: 0,
            turn_output: output,
            accumulated_cache_write: 0,
            turn_cache_write: cache_write,
            accumulated_cache_read: 0,
            turn_cache_read: cache_read,
        }
    }

    fn settings(model_id: &str, thinking: &str) -> AgentSettings {
        AgentSettings {
            provider: "anthropic".to_string(),
            model_id: model_id.to_string(),
            thinking: thinking.to_string(),
            speed: "standard".to_string(),
        }
    }

    #[test]
    fn new_seeds_main_with_unknown_tokens_and_given_window() {
        let f = AgentFooters::new(settings("opus", "high"), 200_000);
        let cu = f.context_usage(AgentId::Main);
        assert_eq!(cu.tokens, None);
        assert_eq!(cu.context_window, 200_000);
    }

    #[test]
    fn record_turn_usage_folds_per_agent() {
        let mut f = AgentFooters::new(settings("opus", "high"), 200_000);
        f.note_settings(AgentId::Sub(1), settings("haiku", "off"), 100_000);

        f.record_turn_usage(AgentId::Sub(1), &token_usage(1_000, 0, 50, 200));
        assert_eq!(f.context_usage(AgentId::Main).tokens, None);
        assert_eq!(f.context_usage(AgentId::Sub(1)).tokens, Some(1_250));

        f.record_turn_usage(AgentId::Main, &token_usage(3_000, 0, 0, 0));
        assert_eq!(f.context_usage(AgentId::Main).tokens, Some(3_000));
        assert_eq!(f.context_usage(AgentId::Sub(1)).tokens, Some(1_250));

        // Last-wins per agent.
        f.record_turn_usage(AgentId::Sub(1), &token_usage(2_000, 0, 100, 300));
        assert_eq!(f.context_usage(AgentId::Sub(1)).tokens, Some(2_400));
        assert_eq!(f.context_usage(AgentId::Main).tokens, Some(3_000));
    }

    #[test]
    fn note_settings_preserves_existing_numerator() {
        let mut f = AgentFooters::new(settings("opus", "high"), 200_000);
        f.record_turn_usage(AgentId::Main, &token_usage(1_000, 0, 0, 0));
        f.note_settings(AgentId::Main, settings("sonnet", "low"), 100_000);
        let cu = f.context_usage(AgentId::Main);
        assert_eq!(cu.tokens, Some(1_000));
        assert_eq!(cu.context_window, 100_000);
    }

    #[test]
    fn context_usage_falls_back_to_main_for_unknown_id() {
        let mut f = AgentFooters::new(settings("opus", "high"), 200_000);
        f.record_turn_usage(AgentId::Main, &token_usage(1_000, 0, 0, 0));
        let cu = f.context_usage(AgentId::Sub(7));
        assert_eq!(cu.tokens, Some(1_000));
        assert_eq!(cu.context_window, 200_000);
    }

    #[test]
    fn model_line_formats_and_falls_back() {
        let mut f = AgentFooters::new(settings("opus", "high"), 200_000);
        assert_eq!(f.model_line(AgentId::Main).as_deref(), Some("opus high"));
        // Unknown id falls back to Main.
        assert_eq!(f.model_line(AgentId::Sub(3)).as_deref(), Some("opus high"));
        // An entry created defensively by usage has an empty
        // model_id and yields no line.
        f.record_turn_usage(AgentId::Sub(1), &token_usage(1, 0, 0, 0));
        assert_eq!(f.model_line(AgentId::Sub(1)), None);
    }

    #[test]
    fn settings_returns_snapshot_without_main_fallback() {
        let mut f = AgentFooters::new(settings("opus", "high"), 200_000);
        f.note_settings(AgentId::Sub(2), settings("haiku", "off"), 100_000);
        assert_eq!(f.settings(AgentId::Sub(2)), Some(&settings("haiku", "off")));
        assert_eq!(f.settings(AgentId::Sub(9)), None);
    }
}
