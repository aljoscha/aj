//! Frontend-agnostic footer data: the per-agent footer store and the
//! context-window occupancy view it produces.
//!
//! A frontend's footer component owns an [`AgentFooters`] store keyed by
//! [`AgentId`]: the Main entry always exists (seeded at construction),
//! Sub entries are created when a sub-agent starts and kept so finished
//! sub-agents (still selectable in the picker) render their final state.
//! Read accessors fall back to the Main entry when the requested agent
//! has none, so the footer always has something coherent to show.
//!
//! The store holds only strings and scalars and exposes pre-built views
//! ([`ContextUsage`], model lines), so the rendering component stays free
//! of wire-level concerns and whatever forwards events stays free of
//! display semantics.

use std::collections::HashMap;

use aj_agent::events::{AgentId, AgentSettings};
use aj_agent::types::TokenUsage;
use serde::Serialize;

/// Snapshot describing how full the active model's context window is. A
/// footer renders exact usage as `tokens/window (percent%)`, an incomplete
/// nonzero subtotal as `≥tokens/window`, and incomplete zero as `?/window`.
///
/// `tokens.None` means "not yet known" — typically a fresh session
/// before the first assistant turn — and renders as `?`. A
/// `context_window` of `0` suppresses the indicator entirely so the
/// footer stays silent for models with no published window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    /// Whether `tokens` is a recorded lower bound rather than an exact
    /// provider disclosure.
    pub incomplete: bool,
}

/// How urgently a footer should color the occupancy percentage.
/// Thresholds are shared across frontends: `Warning` strictly above
/// 70% occupancy, `Critical` strictly above 90%.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSeverity {
    Normal,
    Warning,
    Critical,
}

/// Display form of a [`ContextUsage`], split so a frontend can apply its own
/// color to the percentage substring: `ratio` is the `12.3k/200k`,
/// `≥12.3k/200k`, or `?/200k` prefix, `percent` the `(6.1%)` part (`None` when
/// exact occupancy is unknown), and `severity` the threshold classification of
/// that percentage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUsageDisplay {
    pub ratio: String,
    pub percent: Option<String>,
    pub severity: UsageSeverity,
}

/// Build the display form of `usage`. Returns `None` when
/// `context_window` is 0; the footer has nothing meaningful to say in
/// that case and the indicator drops out of the row.
///
/// Token counts large enough to lose precision in the `u64 -> f64`
/// cast (>2^53 tokens) are well past any model's published context
/// window.
#[allow(clippy::as_conversions)]
pub fn context_usage_display(usage: ContextUsage) -> Option<ContextUsageDisplay> {
    if usage.context_window == 0 {
        return None;
    }
    let window_str = format_tokens(usage.context_window);
    match usage.tokens {
        None => Some(ContextUsageDisplay {
            ratio: format!("?/{window_str}"),
            percent: None,
            severity: UsageSeverity::Normal,
        }),
        Some(0) if usage.incomplete => Some(ContextUsageDisplay {
            ratio: format!("?/{window_str}"),
            percent: None,
            severity: UsageSeverity::Normal,
        }),
        Some(tokens) if usage.incomplete => Some(ContextUsageDisplay {
            ratio: format!("≥{}/{window_str}", format_tokens(tokens)),
            percent: None,
            severity: UsageSeverity::Normal,
        }),
        Some(tokens) => {
            let percent = (tokens as f64 / usage.context_window as f64) * 100.0;
            let severity = if percent > 90.0 {
                UsageSeverity::Critical
            } else if percent > 70.0 {
                UsageSeverity::Warning
            } else {
                UsageSeverity::Normal
            };
            Some(ContextUsageDisplay {
                ratio: format!("{}/{window_str}", format_tokens(tokens)),
                percent: Some(format!("({percent:.1}%)")),
                severity,
            })
        }
    }
}

/// Compact token-count formatter: `987` → `"987"`, `2_500` →
/// `"2.5k"`, `247_321` → `"247k"`, `2_500_000` → `"2.5M"`,
/// `12_000_000` → `"12M"`. One decimal at the low end of each
/// scale, integer at the high end. That keeps the rendered string
/// narrow without losing useful precision.
///
/// Counts large enough to lose precision in the `u64 -> f64`
/// cast (>2^53) are well past anything a context window or a
/// realistic session would reach.
#[allow(clippy::as_conversions)]
pub fn format_tokens(n: u64) -> String {
    if n < 1_000 {
        format!("{n}")
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else if n < 1_000_000 {
        // Half-up rounding to the nearest thousand. Integer
        // division truncates, so adding 500 first picks the
        // closer thousand without floating-point.
        format!("{}k", (n + 500) / 1_000)
    } else if n < 10_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{}M", (n + 500_000) / 1_000_000)
    }
}

/// Format the footer's agent-activity part as `"1 agent (hint)"` /
/// `"2 agents, 1 task (hint)"`. Each count appears only when nonzero;
/// `agents == 0 && tasks == 0` is never passed (callers suppress the
/// part entirely). `open_hint` is the frontend-resolved key label
/// that opens the agent picker.
pub fn format_agent_activity(agents: usize, tasks: usize, open_hint: &str) -> String {
    let mut parts = Vec::new();
    if agents > 0 {
        let noun = if agents == 1 { "agent" } else { "agents" };
        parts.push(format!("{agents} {noun}"));
    }
    if tasks > 0 {
        let noun = if tasks == 1 { "task" } else { "tasks" };
        parts.push(format!("{tasks} {noun}"));
    }
    format!("{} ({open_hint})", parts.join(", "))
}

/// Format the footer's pending-notice part as `"1 notice pending"` /
/// `"3 notices pending"`. Returns `None` for a count of 0 so the
/// caller drops the part entirely.
///
/// A notice is queued when a background task finishes and delivered
/// to the agent at its next drain point, so a nonzero count means the
/// task is done but the agent has not been told yet.
pub fn format_pending_notices(count: usize) -> Option<String> {
    if count == 0 {
        return None;
    }
    let noun = if count == 1 { "notice" } else { "notices" };
    Some(format!("{count} {noun} pending"))
}

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
    /// the first `UsageUpdate` arrives.
    last_turn_context_tokens: Option<u64>,
    /// Whether the latest prompt size is only a recorded lower bound.
    last_turn_incomplete: bool,
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
                last_turn_incomplete: false,
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
        let last_turn_incomplete = self
            .agents
            .get(&id)
            .is_some_and(|entry| entry.last_turn_incomplete);
        self.agents.insert(
            id,
            AgentFooter {
                settings,
                context_window,
                last_turn_context_tokens,
                last_turn_incomplete,
            },
        );
    }

    /// Fold a freshly-arrived `UsageUpdate` into `id`'s entry.
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
                thinking_display: String::new(),
                speed: String::new(),
                verbosity: String::new(),
            },
            context_window: 0,
            last_turn_context_tokens: None,
            last_turn_incomplete: false,
        });
        entry.last_turn_context_tokens =
            Some(usage.turn_input + usage.turn_cache_read + usage.turn_cache_write);
        entry.last_turn_incomplete = usage.turn_incomplete;
    }

    /// Build a [`ContextUsage`] view for `id`, falling back to the
    /// Main entry (which always exists) when `id` has none.
    pub fn context_usage(&self, id: AgentId) -> ContextUsage {
        let entry = self.resolve(id);
        ContextUsage {
            tokens: entry.last_turn_context_tokens,
            context_window: entry.context_window,
            incomplete: entry.last_turn_incomplete,
        }
    }

    /// Overwrite `id`'s context-occupancy numerator. Used after a
    /// compaction reseeds the transcript: no `UsageUpdate` follows a
    /// compaction, so without this the footer would keep showing the
    /// pre-compaction occupancy until the next real turn. A missing
    /// entry is left untouched (nothing to display against yet).
    pub fn set_context_tokens(&mut self, id: AgentId, tokens: u64) {
        if let Some(entry) = self.agents.get_mut(&id) {
            entry.last_turn_context_tokens = Some(tokens);
            entry.last_turn_incomplete = false;
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

    /// Drop every agent's footer but Main's, and forget Main's occupancy
    /// numerator.
    ///
    /// Used when a client rebuilds its fold from scratch
    /// ([`ChatState::reset`](crate::chat::ChatState::reset)): the
    /// sub-agents and the measured turns belong to the history it dropped,
    /// while Main's settings identity was never stream-derived and stays.
    pub fn retain_main(&mut self) {
        self.agents.retain(|id, _| *id == AgentId::Main);
        if let Some(main) = self.agents.get_mut(&AgentId::Main) {
            main.last_turn_context_tokens = None;
            main.last_turn_incomplete = false;
        }
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
            turn_incomplete: false,
            accumulated_incomplete: false,
        }
    }

    fn settings(model_id: &str, thinking: &str) -> AgentSettings {
        AgentSettings {
            provider: "anthropic".to_string(),
            model_id: model_id.to_string(),
            thinking: thinking.to_string(),
            thinking_display: "default".to_string(),
            speed: "standard".to_string(),
            verbosity: "default".to_string(),
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
    fn latest_turn_completeness_survives_settings_and_clears_on_new_evidence() {
        let mut f = AgentFooters::new(settings("opus", "high"), 200_000);
        let mut partial = token_usage(20_000, 0, 0, 0);
        partial.turn_incomplete = true;
        f.record_turn_usage(AgentId::Main, &partial);
        f.note_settings(AgentId::Main, settings("sonnet", "low"), 100_000);
        assert_eq!(
            context_usage_display(f.context_usage(AgentId::Main))
                .expect("known window")
                .ratio,
            "≥20k/100k"
        );

        f.set_context_tokens(AgentId::Main, 3_000);
        assert!(!f.context_usage(AgentId::Main).incomplete);

        let complete = token_usage(4_000, 0, 0, 0);
        f.record_turn_usage(AgentId::Main, &complete);
        assert_eq!(f.context_usage(AgentId::Main).tokens, Some(4_000));
        assert!(!f.context_usage(AgentId::Main).incomplete);
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

    /// Sanity-check the scale-aware token formatter at each band
    /// boundary so a refactor can't silently change displayed values.
    #[test]
    fn format_tokens_spans_all_bands() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(987), "987");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(2_500), "2.5k");
        assert_eq!(format_tokens(9_999), "10.0k");
        assert_eq!(format_tokens(10_000), "10k");
        assert_eq!(format_tokens(247_321), "247k");
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(2_500_000), "2.5M");
        assert_eq!(format_tokens(12_000_000), "12M");
    }

    #[test]
    fn context_usage_display_renders_unknown_tokens_as_question_mark() {
        let d = context_usage_display(ContextUsage {
            tokens: None,
            context_window: 200_000,
            incomplete: false,
        })
        .expect("non-empty window should render");
        assert_eq!(d.ratio, "?/200k");
        assert_eq!(d.percent, None);
        assert_eq!(d.severity, UsageSeverity::Normal);
    }

    #[test]
    fn context_usage_display_marks_incomplete_zero_and_nonzero_without_percentages() {
        let zero = context_usage_display(ContextUsage {
            tokens: Some(0),
            context_window: 200_000,
            incomplete: true,
        })
        .expect("known window");
        assert_eq!(zero.ratio, "?/200k");
        assert_eq!(zero.percent, None);

        let lower_bound = context_usage_display(ContextUsage {
            tokens: Some(20_000),
            context_window: 200_000,
            incomplete: true,
        })
        .expect("known window");
        assert_eq!(lower_bound.ratio, "≥20k/200k");
        assert_eq!(lower_bound.percent, None);
    }

    #[test]
    fn context_usage_display_suppresses_zero_window() {
        assert!(
            context_usage_display(ContextUsage {
                tokens: Some(1_000),
                context_window: 0,
                incomplete: false,
            })
            .is_none(),
            "a 0-token context window suppresses the indicator",
        );
    }

    /// The severity thresholds are strict: exactly 70% / 90% stay a
    /// band lower, one token past crosses.
    #[test]
    fn context_usage_display_classifies_thresholds() {
        let severity = |tokens: u64| {
            context_usage_display(ContextUsage {
                tokens: Some(tokens),
                context_window: 200_000,
                incomplete: false,
            })
            .expect("rendered")
            .severity
        };
        assert_eq!(severity(20_000), UsageSeverity::Normal);
        assert_eq!(severity(140_000), UsageSeverity::Normal);
        assert_eq!(severity(140_001), UsageSeverity::Warning);
        assert_eq!(severity(180_000), UsageSeverity::Warning);
        assert_eq!(severity(180_001), UsageSeverity::Critical);
    }

    #[test]
    fn context_usage_display_formats_ratio_and_percent() {
        let d = context_usage_display(ContextUsage {
            tokens: Some(20_000),
            context_window: 200_000,
            incomplete: false,
        })
        .expect("rendered");
        assert_eq!(d.ratio, "20k/200k");
        assert_eq!(d.percent.as_deref(), Some("(10.0%)"));
    }

    #[test]
    fn format_pending_notices_pluralizes_and_drops_zero() {
        assert_eq!(format_pending_notices(0), None);
        assert_eq!(
            format_pending_notices(1).as_deref(),
            Some("1 notice pending"),
        );
        assert_eq!(
            format_pending_notices(3).as_deref(),
            Some("3 notices pending"),
        );
    }

    #[test]
    fn format_agent_activity_handles_counts_and_plurals() {
        assert_eq!(format_agent_activity(1, 0, "alt+a"), "1 agent (alt+a)");
        assert_eq!(format_agent_activity(3, 0, "alt+a"), "3 agents (alt+a)");
        assert_eq!(format_agent_activity(0, 1, "alt+a"), "1 task (alt+a)");
        assert_eq!(
            format_agent_activity(2, 2, "alt+a"),
            "2 agents, 2 tasks (alt+a)",
        );
    }
}
