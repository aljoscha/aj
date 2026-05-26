//! Snapshot data feeding the [`super::components::footer::Footer`].
//!
//! The event pump owns a [`FooterData`] snapshot that aggregates
//! displayable state the footer needs but doesn't itself track —
//! today the active model's context window and the size of the
//! last main-agent prompt. The pump folds main-agent
//! [`aj_agent::events::AgentEvent::TurnUsage`] events into the
//! snapshot, swaps the context window on model changes, then
//! pushes a [`ContextUsage`] view through
//! [`super::components::footer::Footer::set_context_usage`].
//!
//! The split keeps the [`Footer`](super::components::footer::Footer)
//! component free of wire-level concerns (it accepts a pre-built
//! snapshot) and the pump free of display semantics (it forwards
//! events; this module decides what fields the snapshot exposes).

use aj_agent::types::TokenUsage;

use crate::modes::interactive::components::footer::ContextUsage;

/// Mutable snapshot the event pump keeps in sync with the agent.
///
/// Cheap to copy — fields are scalars — so callers don't need
/// shared ownership; the pump holds one instance and rebuilds the
/// [`ContextUsage`] view on every push.
#[derive(Debug, Clone, Copy)]
pub struct FooterData {
    /// Context window of the active model, in tokens. Zero
    /// suppresses the footer's occupancy indicator entirely (see
    /// [`super::components::footer`]).
    context_window: u64,
    /// Tokens occupying the context after the most recent
    /// main-agent turn. `None` until the first `TurnUsage`
    /// arrives; rendered as `?` by the footer.
    last_turn_context_tokens: Option<u64>,
}

impl FooterData {
    /// Build a snapshot seeded with the active model's context
    /// window. `last_turn_context_tokens` starts as `None` so the
    /// footer initially renders `?/<window>` until the first
    /// assistant turn lands.
    pub fn new(context_window: u64) -> Self {
        Self {
            context_window,
            last_turn_context_tokens: None,
        }
    }

    /// Swap the context-window denominator. Called on every
    /// model change so the indicator stays accurate without
    /// waiting for the next turn.
    pub fn set_context_window(&mut self, context_window: u64) {
        self.context_window = context_window;
    }

    /// Fold a freshly-arrived main-agent `TurnUsage` into the
    /// snapshot.
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
    pub fn record_turn_usage(&mut self, usage: &TokenUsage) {
        self.last_turn_context_tokens =
            Some(usage.turn_input + usage.turn_cache_read + usage.turn_cache_write);
    }

    /// Build a [`ContextUsage`] view suitable for handing to the
    /// footer component.
    pub fn context_usage(&self) -> ContextUsage {
        ContextUsage {
            tokens: self.last_turn_context_tokens,
            context_window: self.context_window,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `TokenUsage` snapshot carrying the supplied
    /// per-turn deltas. The `accumulated_*` fields are zeroed —
    /// matching the wire-level pre-add semantic for an agent that
    /// hasn't run any prior turns — but `FooterData::record_turn_usage`
    /// only reads `turn_*`, so the accumulator value is irrelevant
    /// to the tests below and we keep it constant for clarity.
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

    #[test]
    fn fresh_snapshot_reports_unknown_tokens() {
        let d = FooterData::new(200_000);
        let cu = d.context_usage();
        assert_eq!(cu.tokens, None);
        assert_eq!(cu.context_window, 200_000);
    }

    #[test]
    fn record_turn_usage_sums_prompt_components() {
        // Prompt size = non-cached input + cached read + cached
        // write = 1_000 + 200 + 50 = 1_250. Output is excluded.
        let mut d = FooterData::new(200_000);
        d.record_turn_usage(&token_usage(1_000, 999, 50, 200));
        assert_eq!(d.context_usage().tokens, Some(1_250));
    }

    #[test]
    fn set_context_window_updates_denominator_only() {
        let mut d = FooterData::new(200_000);
        d.record_turn_usage(&token_usage(1_000, 0, 0, 0));
        d.set_context_window(100_000);
        let cu = d.context_usage();
        assert_eq!(cu.tokens, Some(1_000));
        assert_eq!(cu.context_window, 100_000);
    }

    /// Subsequent `record_turn_usage` calls replace the previous
    /// numerator outright — the snapshot tracks the *last* turn,
    /// not a running total.
    #[test]
    fn record_turn_usage_replaces_previous_value() {
        let mut d = FooterData::new(200_000);
        d.record_turn_usage(&token_usage(1_000, 0, 0, 0));
        d.record_turn_usage(&token_usage(2_000, 0, 100, 300));
        assert_eq!(d.context_usage().tokens, Some(2_400));
    }
}
