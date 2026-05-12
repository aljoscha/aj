//! Named demo scripts for the `--scripted` CLI flag, authored against the
//! unified [`AssistantMessageEvent`] streaming protocol.
//!
//! Each demo is a [`Vec<ProviderScript>`](super::ProviderScript) — one
//! script per agent inference. The agent loop runs an inference, observes
//! the resulting `Done`/`Error` event, executes any tool calls produced by
//! the finalized [`AssistantMessage`], then runs the next inference; demos
//! that span multiple turns (e.g. tool-use → result → follow-up text)
//! supply one script per turn.
//!
//! The library is hand-rolled (not loaded from disk) so each demo can be
//! tuned for its rendering target: thinking-only flows have realistic
//! multi-paragraph thinking text, the tool-use demo picks a known-safe
//! builtin (`bash echo`) that always succeeds, etc.
//!
//! Add new demos here as the rendering surface grows. Keep them
//! self-contained: a demo should run identically regardless of working
//! directory, environment, or which builtins are enabled (modulo the
//! `--disabled-tools` config, which the binary applies uniformly).

use std::time::Duration;

use serde_json::Value;

use crate::streaming::{DoneReason, ErrorReason};
use crate::types::{AssistantError, ErrorCategory};

use super::{ProviderScript, ScriptBuilder};

/// Identity stamped onto every emitted [`AssistantMessage`] partial. The
/// `--scripted` resolver overrides the human-facing name with the demo
/// label (e.g. `scripted/thinking-basic`) when building the model handle.
const API: &str = "scripted";
const PROVIDER: &str = "scripted";
const MODEL: &str = "scripted";

/// Per-chunk pacing for streaming demos. Slow enough that the live render
/// is observable to a human eye, fast enough that the demo doesn't drag.
const CHUNK_MS: u64 = 25;
/// Pause between major sections (thinking → text, tool result → follow-up)
/// so the user can tell sections apart visually.
const SECTION_MS: u64 = 200;

/// Approximate character width per text delta. Picked so a sentence
/// streams in several chunks rather than landing in one frame.
const TEXT_CHUNK: usize = 8;
/// Smaller chunks for thinking blocks so the collapsible thinking widget
/// gets exercised on its live-update path.
const THINKING_CHUNK: usize = 6;

/// Opaque signature attached to every demo thinking block. Real providers
/// carry provider-specific reasoning signatures here (Anthropic requires
/// them for multi-turn replay); scripted runs only need a stable string.
const THINKING_SIG: &str = "scripted-sig";

fn chunk_delay() -> Duration {
    Duration::from_millis(CHUNK_MS)
}

fn section_delay() -> Duration {
    Duration::from_millis(SECTION_MS)
}

/// Construct a freshly-configured [`ScriptBuilder`] with the demo identity
/// and default chunking applied. Each demo calls this once per inference
/// script it wants to build.
fn builder() -> ScriptBuilder {
    ScriptBuilder::new(API, PROVIDER, MODEL)
        .with_chunk_size(TEXT_CHUNK)
        .with_chunk_delay(chunk_delay())
}

/// Lookup table for the `--scripted <NAME>` flag.
///
/// Returning a `Vec<(name, summary, builder)>` keeps the catalog
/// data-driven: the CLI lists names and one-line summaries via the same
/// source the lookup uses, so a new demo only needs one entry here to be
/// visible everywhere.
pub fn catalog() -> Vec<(&'static str, &'static str, fn() -> Vec<ProviderScript>)> {
    vec![
        (
            "thinking-basic",
            "single thinking block followed by a short text response",
            thinking_basic,
        ),
        (
            "thinking-long",
            "multi-paragraph thinking streamed slowly, then a paragraph of text",
            thinking_long,
        ),
        (
            "thinking-then-tool",
            "thinking → bash echo tool_use → tool result → more thinking → final text",
            thinking_then_tool,
        ),
        (
            "interleaved",
            "alternating text and thinking blocks (worst-case ordering)",
            interleaved,
        ),
        (
            "tool-error",
            "tool call to bash with a non-zero exit, then a follow-up text",
            tool_error,
        ),
        (
            "protocol-error",
            "provider emits a protocol error mid-stream (renders as Error event)",
            protocol_error,
        ),
        (
            "tool-use-parse-error",
            "tool_use block with malformed input JSON (synthesized error result)",
            tool_use_parse_error,
        ),
        (
            "streaming-text",
            "plain text streaming with no thinking (control case)",
            streaming_text,
        ),
        (
            "multi-tool",
            "two sequential tool calls (bash echo, bash date) and a wrap-up text",
            multi_tool,
        ),
    ]
}

/// Look up a demo by name, returning the per-inference scripts.
pub fn lookup(name: &str) -> Option<Vec<ProviderScript>> {
    catalog()
        .into_iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, _, build)| build())
}

/// Names of every demo, useful for "I don't know what to pick" CLI errors.
pub fn names() -> Vec<&'static str> {
    catalog().into_iter().map(|(n, _, _)| n).collect()
}

// ===========================================================================
// Demos.
// ===========================================================================

/// `thinking-basic`: one thinking block, one text response.
fn thinking_basic() -> Vec<ProviderScript> {
    let thinking = "\
The user asked a simple question. Let me think about what they want — \
a quick acknowledgement should be fine here. I'll keep the response short \
and friendly.";
    let response = "Hello! I'm a scripted demo. Thinking blocks should render \
above this message.";

    let script = builder()
        .start()
        .thinking_block_chunked(
            thinking,
            Some(THINKING_SIG.into()),
            THINKING_CHUNK,
            chunk_delay(),
        )
        .delay(section_delay())
        .text_block(response)
        .done(DoneReason::Stop);
    vec![script]
}

/// `thinking-long`: long multi-paragraph thinking, then text.
fn thinking_long() -> Vec<ProviderScript> {
    let thinking = "\
First, let me consider what the user is actually asking. They want me to \
walk through a non-trivial reasoning chain so they can see the thinking \
block collapse, expand, and scroll smoothly when its contents exceed the \
visible viewport. That means I need at least three or four paragraphs of \
plausible-looking reasoning text.

Second paragraph: the renderer's collapsible behaviour kicks in once the \
thinking block exceeds a height threshold, so this script should produce \
enough content to trigger that path. The user can then press Ctrl+T to \
toggle collapse/expand and visually confirm the transition is correct.

Third paragraph: streaming should arrive in roughly word-sized chunks so \
the per-token render path gets exercised. Real models tend to emit larger \
chunks than this, but small chunks stress the renderer more reliably and \
expose any width-recomputation or wrapping regressions.

Finally, after this thinking block finishes I'll emit a short text response \
so the renderer transitions from the thinking channel back to the text \
channel — another path worth visually verifying.";
    let response = "I've finished thinking. The block above should be \
collapsible with Ctrl+T, and this paragraph should appear immediately below \
it without any extra blank lines.";

    let script = builder()
        .start()
        .thinking_block_chunked(
            thinking,
            Some(THINKING_SIG.into()),
            THINKING_CHUNK,
            chunk_delay(),
        )
        .delay(section_delay())
        .text_block(response)
        .done(DoneReason::Stop);
    vec![script]
}

/// `thinking-then-tool`: thinking → tool_use → (real tool runs) → follow-up
/// thinking + text.
fn thinking_then_tool() -> Vec<ProviderScript> {
    let thinking_a = "The user wants me to demonstrate a tool call. I'll \
run `bash` with a harmless echo so the renderer shows the full \
tool-execution flow: start, streaming body, end with the captured stdout.";
    let thinking_b = "The tool returned. Now I'll wrap up with a short \
confirmation message so the demo ends cleanly.";
    let final_text = "Tool call complete. The renderer should have shown a thinking \
block, a tool execution panel, and now this text — all in sequence.";

    let tool_input = serde_json::json!({
        "command": "echo 'hello from a scripted tool call'",
        "timeout": 5,
        "description": "Demo bash invocation for the scripted runner."
    });

    let script_1 = builder()
        .start()
        .thinking_block_chunked(
            thinking_a,
            Some(THINKING_SIG.into()),
            THINKING_CHUNK,
            chunk_delay(),
        )
        .delay(section_delay())
        .tool_call_block_chunked("tu-demo-1", "bash", tool_input, 0, Duration::ZERO)
        .done(DoneReason::ToolUse);

    let script_2 = builder()
        .start()
        .thinking_block_chunked(
            thinking_b,
            Some(THINKING_SIG.into()),
            THINKING_CHUNK,
            chunk_delay(),
        )
        .delay(section_delay())
        .text_block(final_text)
        .done(DoneReason::Stop);

    vec![script_1, script_2]
}

/// `interleaved`: text → thinking → text → thinking → text.
fn interleaved() -> Vec<ProviderScript> {
    let t1 = "First thought before any reply text. Tests that an initial \
thinking block at content_index 0 renders correctly.";
    let r1 = "First reply chunk after the opening thought.";
    let t2 = "Mid-stream thinking inserted between two text blocks. The \
renderer should keep both text blocks distinct rather than concatenating \
them across the thinking divide.";
    let r2 = "Second reply chunk. If you can read this on its own line \
below the second thinking block, the interleaving renders correctly.";

    let script = builder()
        .start()
        .thinking_block_chunked(t1, Some(THINKING_SIG.into()), THINKING_CHUNK, chunk_delay())
        .delay(section_delay())
        .text_block(r1)
        .delay(section_delay())
        .thinking_block_chunked(t2, Some(THINKING_SIG.into()), THINKING_CHUNK, chunk_delay())
        .delay(section_delay())
        .text_block(r2)
        .done(DoneReason::Stop);

    vec![script]
}

/// `tool-error`: bash with a non-zero exit, then a follow-up.
fn tool_error() -> Vec<ProviderScript> {
    let intro = "I'll demonstrate a failing tool call. The bash invocation \
below exits non-zero; the renderer should mark the tool result as an \
error.";
    let follow_up = "As expected, the tool reported a non-zero exit. The \
result panel above should be styled as an error.";

    let tool_input = serde_json::json!({
        "command": "exit 1",
        "timeout": 5,
        "description": "Demo bash invocation that intentionally fails."
    });

    let script_1 = builder()
        .start()
        .text_block(intro)
        .delay(section_delay())
        .tool_call_block_chunked("tu-demo-err", "bash", tool_input, 0, Duration::ZERO)
        .done(DoneReason::ToolUse);

    let script_2 = builder()
        .start()
        .text_block(follow_up)
        .done(DoneReason::Stop);

    vec![script_1, script_2]
}

/// `protocol-error`: emit a protocol error mid-stream.
///
/// Maps onto the unified protocol's terminal [`AssistantMessageEvent::Error`]
/// event: the provider streams a preamble, then surfaces a transient
/// failure that the agent's retry layer treats as a recoverable turn
/// error. The renderer should show whatever text was emitted before the
/// error plus an error notice.
fn protocol_error() -> Vec<ProviderScript> {
    let preamble = "I'll start replying, then the provider stream will \
emit a protocol error to verify the renderer's error path.";

    let script = builder()
        .start()
        .text_block(preamble)
        .delay(section_delay())
        .error(
            ErrorReason::Error,
            AssistantError::new(
                ErrorCategory::Transient,
                "scripted: synthetic protocol error for demo",
            ),
        );

    vec![script]
}

/// `tool-use-parse-error`: emit a tool_use block whose arguments the agent
/// can't satisfy, exercising the synthesized-error tool_result path.
///
/// The unified protocol doesn't carry a dedicated `ToolUseParseError`
/// event; instead, the [`ToolCallEnd`](AssistantMessageEvent::ToolCallEnd)
/// event surfaces whatever arguments survived parsing. Here we emit a
/// tool call with a [`Value::Null`] argument payload, which the bash
/// tool's input-schema validation rejects: the agent synthesizes an
/// `is_error: true` tool_result so the renderer's error-panel path runs.
fn tool_use_parse_error() -> Vec<ProviderScript> {
    let preamble = "I'll attempt a tool call with malformed JSON arguments.";
    let final_text = "The agent recovered by synthesizing an error tool_result. The \
renderer should have shown the parse-error notice and the error result.";

    let script_1 = builder()
        .start()
        .text_block(preamble)
        .delay(section_delay())
        .tool_call_block_chunked("tu-demo-parse", "bash", Value::Null, 0, Duration::ZERO)
        .done(DoneReason::ToolUse);

    let script_2 = builder()
        .start()
        .text_block(final_text)
        .done(DoneReason::Stop);

    vec![script_1, script_2]
}

/// `streaming-text`: plain text streaming, no thinking. Control case.
fn streaming_text() -> Vec<ProviderScript> {
    let response = "This is a plain text-only demo. No thinking, no tool \
calls — just a few sentences streamed in chunks so you can verify the \
baseline text rendering path. If this looks fine but the thinking demos \
don't, the bug is on the thinking channel rather than the renderer itself.";

    let script = builder()
        .start()
        .text_block(response)
        .done(DoneReason::Stop);
    vec![script]
}

/// `multi-tool`: two sequential tool calls (`bash echo`, `bash date`) and a
/// wrap-up text response.
fn multi_tool() -> Vec<ProviderScript> {
    let intro = "I'll run two bash commands back-to-back so you can see the \
renderer transition between tool panels.";
    let mid = "First tool returned. Running the second one now.";
    let wrap = "Both tools have finished. The renderer should have shown \
two distinct tool panels with their respective outputs.";

    let echo_input = serde_json::json!({
        "command": "echo first",
        "timeout": 5,
        "description": "First demo bash call."
    });
    let date_input = serde_json::json!({
        "command": "date -u +%Y-%m-%d",
        "timeout": 5,
        "description": "Second demo bash call (UTC date)."
    });

    let script_1 = builder()
        .start()
        .text_block(intro)
        .delay(section_delay())
        .tool_call_block_chunked("tu-demo-multi-1", "bash", echo_input, 0, Duration::ZERO)
        .done(DoneReason::ToolUse);

    let script_2 = builder()
        .start()
        .text_block(mid)
        .delay(section_delay())
        .tool_call_block_chunked("tu-demo-multi-2", "bash", date_input, 0, Duration::ZERO)
        .done(DoneReason::ToolUse);

    let script_3 = builder().start().text_block(wrap).done(DoneReason::Stop);

    vec![script_1, script_2, script_3]
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::AssistantMessageEvent;

    /// Sanity: every catalog entry produces at least one script with at
    /// least one event, and each script ends with a terminal event
    /// ([`AssistantMessageEvent::Done`] or [`AssistantMessageEvent::Error`])
    /// so the provider's stream-drive contract holds.
    #[test]
    fn catalog_demos_are_well_formed() {
        for (name, _summary, build) in catalog() {
            let scripts = build();
            assert!(!scripts.is_empty(), "demo {name} produced zero scripts");
            for (idx, script) in scripts.iter().enumerate() {
                assert!(
                    !script.steps.is_empty(),
                    "demo {name} script {idx} is empty"
                );
                let last_step = script.steps.last().expect("non-empty");
                assert!(
                    matches!(
                        last_step.event,
                        AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
                    ),
                    "demo {name} script {idx} doesn't end with a terminal event"
                );
            }
        }
    }

    #[test]
    fn lookup_returns_some_for_known_names() {
        for name in names() {
            assert!(lookup(name).is_some(), "missing demo: {name}");
        }
    }

    #[test]
    fn lookup_returns_none_for_unknown() {
        assert!(lookup("definitely-not-a-real-demo").is_none());
    }

    #[test]
    fn catalog_names_match_legacy_demo_set() {
        // The new provider-based demos must share the same name set as
        // the legacy `scripted::demos` catalog so the `--scripted` CLI
        // flag can swap resolvers without breaking documented invocations.
        let new_names: Vec<&str> = names();
        let legacy_names: Vec<&str> = crate::scripted::demos::names();
        assert_eq!(
            new_names, legacy_names,
            "provider demo catalog drifted from the legacy demo catalog"
        );
    }
}
