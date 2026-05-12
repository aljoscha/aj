//! Named demo scripts for the `--scripted` CLI flag.
//!
//! Each demo is a [`Vec<Script>`] — one [`Script`](super::Script) per agent
//! inference. The agent loop runs an inference, observes the resulting
//! `FinalizedMessage`, executes any tool calls, then runs the next
//! inference; demos that span multiple turns (e.g. tool-use → result →
//! follow-up text) supply one script per turn.
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
//!
//! # Future extensions
//!
//! Adding a TOML/JSON demo loader is straightforward — the [`Script`] and
//! [`ScriptStep`](super::ScriptStep) shapes are serde-friendly modulo the
//! `StreamingEvent` enum, which would need a stable on-disk representation.
//! Not implemented yet; the in-process demos cover the eyeballing use case
//! we have today.

use std::time::Duration;

use crate::messages::{ContentBlock, StopReason};
use crate::streaming::StreamingEvent;

use super::{Script, ScriptStep, finalized_message, stream_text_steps, stream_thinking_steps};

const MODEL: &str = "scripted";

/// Pacing used by the demo library — slow enough that the TUI's live render
/// is observable to a human eye, fast enough that the demo doesn't drag.
const CHUNK_MS: u64 = 25;
/// Pause between major sections (thinking → text, tool result → follow-up)
/// so the user can tell sections apart visually.
const SECTION_MS: u64 = 200;

fn chunk_delay() -> Duration {
    Duration::from_millis(CHUNK_MS)
}

fn section_delay() -> Duration {
    Duration::from_millis(SECTION_MS)
}

/// Lookup table for the `--scripted <NAME>` flag.
///
/// Returning a `Vec<(name, summary, builder)>` keeps the catalog data-driven:
/// the CLI lists names and one-line summaries via the same source the lookup
/// uses, so a new demo only needs one entry here to be visible everywhere.
pub fn catalog() -> Vec<(&'static str, &'static str, fn() -> Vec<Script>)> {
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
pub fn lookup(name: &str) -> Option<Vec<Script>> {
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
fn thinking_basic() -> Vec<Script> {
    let thinking = "\
The user asked a simple question. Let me think about what they want — \
a quick acknowledgement should be fine here. I'll keep the response short \
and friendly.";
    let response = "Hello! I'm a scripted demo. Thinking blocks should render \
above this message.";

    let mut steps = Vec::new();
    steps.extend(stream_thinking_steps(thinking, 6, chunk_delay()));
    steps.extend(delay_first(
        stream_text_steps(response, 8, chunk_delay()),
        section_delay(),
    ));
    steps.push(ScriptStep::immediate(StreamingEvent::FinalizedMessage {
        message: finalized_message(
            MODEL,
            vec![
                ContentBlock::ThinkingBlock {
                    signature: "scripted-sig".to_string(),
                    thinking: thinking.to_string(),
                },
                ContentBlock::new_text_block(response.to_string()),
            ],
            StopReason::EndTurn,
        ),
    }));

    vec![Script { steps }]
}

/// `thinking-long`: long multi-paragraph thinking, then text.
fn thinking_long() -> Vec<Script> {
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

    let mut steps = Vec::new();
    steps.extend(stream_thinking_steps(thinking, 4, chunk_delay()));
    steps.extend(delay_first(
        stream_text_steps(response, 10, chunk_delay()),
        section_delay(),
    ));
    steps.push(ScriptStep::immediate(StreamingEvent::FinalizedMessage {
        message: finalized_message(
            MODEL,
            vec![
                ContentBlock::ThinkingBlock {
                    signature: "scripted-sig".to_string(),
                    thinking: thinking.to_string(),
                },
                ContentBlock::new_text_block(response.to_string()),
            ],
            StopReason::EndTurn,
        ),
    }));

    vec![Script { steps }]
}

/// `thinking-then-tool`: thinking → tool_use → (real tool runs) → follow-up
/// thinking + text.
fn thinking_then_tool() -> Vec<Script> {
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

    let mut steps_1 = Vec::new();
    steps_1.extend(stream_thinking_steps(thinking_a, 6, chunk_delay()));
    steps_1.push(ScriptStep::new(
        section_delay(),
        StreamingEvent::FinalizedMessage {
            message: finalized_message(
                MODEL,
                vec![
                    ContentBlock::ThinkingBlock {
                        signature: "scripted-sig".to_string(),
                        thinking: thinking_a.to_string(),
                    },
                    ContentBlock::ToolUseBlock {
                        id: "tu-demo-1".to_string(),
                        name: "bash".to_string(),
                        input: tool_input,
                        caller: None,
                    },
                ],
                StopReason::ToolUse,
            ),
        },
    ));

    let mut steps_2 = Vec::new();
    steps_2.extend(stream_thinking_steps(thinking_b, 6, chunk_delay()));
    steps_2.extend(delay_first(
        stream_text_steps(final_text, 8, chunk_delay()),
        section_delay(),
    ));
    steps_2.push(ScriptStep::immediate(StreamingEvent::FinalizedMessage {
        message: finalized_message(
            MODEL,
            vec![
                ContentBlock::ThinkingBlock {
                    signature: "scripted-sig".to_string(),
                    thinking: thinking_b.to_string(),
                },
                ContentBlock::new_text_block(final_text.to_string()),
            ],
            StopReason::EndTurn,
        ),
    }));

    vec![Script { steps: steps_1 }, Script { steps: steps_2 }]
}

/// `interleaved`: text → thinking → text → thinking → text.
fn interleaved() -> Vec<Script> {
    let t1 = "First thought before any reply text. Tests that an initial \
thinking block at content_index 0 renders correctly.";
    let r1 = "First reply chunk after the opening thought.";
    let t2 = "Mid-stream thinking inserted between two text blocks. The \
renderer should keep both text blocks distinct rather than concatenating \
them across the thinking divide.";
    let r2 = "Second reply chunk. If you can read this on its own line \
below the second thinking block, the interleaving renders correctly.";

    let mut steps = Vec::new();
    steps.extend(stream_thinking_steps(t1, 6, chunk_delay()));
    steps.extend(delay_first(
        stream_text_steps(r1, 8, chunk_delay()),
        section_delay(),
    ));
    steps.extend(delay_first(
        stream_thinking_steps(t2, 6, chunk_delay()),
        section_delay(),
    ));
    steps.extend(delay_first(
        stream_text_steps(r2, 8, chunk_delay()),
        section_delay(),
    ));
    steps.push(ScriptStep::immediate(StreamingEvent::FinalizedMessage {
        message: finalized_message(
            MODEL,
            vec![
                ContentBlock::ThinkingBlock {
                    signature: "scripted-sig".to_string(),
                    thinking: t1.to_string(),
                },
                ContentBlock::new_text_block(r1.to_string()),
                ContentBlock::ThinkingBlock {
                    signature: "scripted-sig".to_string(),
                    thinking: t2.to_string(),
                },
                ContentBlock::new_text_block(r2.to_string()),
            ],
            StopReason::EndTurn,
        ),
    }));

    vec![Script { steps }]
}

/// `tool-error`: bash with a non-zero exit, then a follow-up.
fn tool_error() -> Vec<Script> {
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

    let mut steps_1 = stream_text_steps(intro, 8, chunk_delay());
    steps_1.push(ScriptStep::new(
        section_delay(),
        StreamingEvent::FinalizedMessage {
            message: finalized_message(
                MODEL,
                vec![
                    ContentBlock::new_text_block(intro.to_string()),
                    ContentBlock::ToolUseBlock {
                        id: "tu-demo-err".to_string(),
                        name: "bash".to_string(),
                        input: tool_input,
                        caller: None,
                    },
                ],
                StopReason::ToolUse,
            ),
        },
    ));

    let mut steps_2 = stream_text_steps(follow_up, 8, chunk_delay());
    steps_2.push(ScriptStep::immediate(StreamingEvent::FinalizedMessage {
        message: finalized_message(
            MODEL,
            vec![ContentBlock::new_text_block(follow_up.to_string())],
            StopReason::EndTurn,
        ),
    }));

    vec![Script { steps: steps_1 }, Script { steps: steps_2 }]
}

/// `protocol-error`: emit a protocol error mid-stream.
///
/// The agent surfaces this as an [`AgentEvent::Error`] and continues to
/// produce a finalized message for the turn, so the renderer should show an
/// error notice followed by whatever text we managed to assemble.
fn protocol_error() -> Vec<Script> {
    let preamble = "I'll start replying, then the provider stream will \
emit a protocol error to verify the renderer's error path.";

    let mut steps = stream_text_steps(preamble, 8, chunk_delay());
    steps.push(ScriptStep::new(
        section_delay(),
        StreamingEvent::ProtocolError {
            error: "scripted: synthetic protocol error for demo".to_string(),
        },
    ));
    steps.push(ScriptStep::immediate(StreamingEvent::FinalizedMessage {
        message: finalized_message(
            MODEL,
            vec![ContentBlock::new_text_block(preamble.to_string())],
            StopReason::EndTurn,
        ),
    }));

    vec![Script { steps }]
}

/// `tool-use-parse-error`: emit a malformed tool_use the agent can't parse.
///
/// The agent synthesizes a paired error tool_result and continues; the
/// renderer should show the error notice plus the synthesized result.
fn tool_use_parse_error() -> Vec<Script> {
    let preamble = "I'll attempt a tool call with malformed JSON arguments.";
    let final_text = "The agent recovered by synthesizing an error tool_result. The \
renderer should have shown the parse-error notice and the error result.";

    let mut steps_1 = stream_text_steps(preamble, 8, chunk_delay());
    steps_1.push(ScriptStep::new(
        section_delay(),
        StreamingEvent::ToolUseParseError {
            id: "tu-demo-parse".to_string(),
            name: "bash".to_string(),
            error: "expected object, got truncated stream".to_string(),
            raw_data: "{\"command\": \"echo".to_string(),
        },
    ));
    steps_1.push(ScriptStep::immediate(StreamingEvent::FinalizedMessage {
        message: finalized_message(
            MODEL,
            // The agent strips ToolUseParseError'd blocks from the content,
            // so we leave only the preamble text here.
            vec![ContentBlock::new_text_block(preamble.to_string())],
            StopReason::ToolUse,
        ),
    }));

    let mut steps_2 = stream_text_steps(final_text, 8, chunk_delay());
    steps_2.push(ScriptStep::immediate(StreamingEvent::FinalizedMessage {
        message: finalized_message(
            MODEL,
            vec![ContentBlock::new_text_block(final_text.to_string())],
            StopReason::EndTurn,
        ),
    }));

    vec![Script { steps: steps_1 }, Script { steps: steps_2 }]
}

/// `streaming-text`: plain text streaming, no thinking. Control case.
fn streaming_text() -> Vec<Script> {
    let response = "This is a plain text-only demo. No thinking, no tool \
calls — just a few sentences streamed in chunks so you can verify the \
baseline text rendering path. If this looks fine but the thinking demos \
don't, the bug is on the thinking channel rather than the renderer itself.";

    let mut steps = stream_text_steps(response, 8, chunk_delay());
    steps.push(ScriptStep::immediate(StreamingEvent::FinalizedMessage {
        message: finalized_message(
            MODEL,
            vec![ContentBlock::new_text_block(response.to_string())],
            StopReason::EndTurn,
        ),
    }));

    vec![Script { steps }]
}

/// `multi-tool`: two sequential tool calls (`bash echo`, `bash date`) and a
/// wrap-up text response.
fn multi_tool() -> Vec<Script> {
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

    let mut steps_1 = stream_text_steps(intro, 8, chunk_delay());
    steps_1.push(ScriptStep::new(
        section_delay(),
        StreamingEvent::FinalizedMessage {
            message: finalized_message(
                MODEL,
                vec![
                    ContentBlock::new_text_block(intro.to_string()),
                    ContentBlock::ToolUseBlock {
                        id: "tu-demo-multi-1".to_string(),
                        name: "bash".to_string(),
                        input: echo_input,
                        caller: None,
                    },
                ],
                StopReason::ToolUse,
            ),
        },
    ));

    let mut steps_2 = stream_text_steps(mid, 8, chunk_delay());
    steps_2.push(ScriptStep::new(
        section_delay(),
        StreamingEvent::FinalizedMessage {
            message: finalized_message(
                MODEL,
                vec![
                    ContentBlock::new_text_block(mid.to_string()),
                    ContentBlock::ToolUseBlock {
                        id: "tu-demo-multi-2".to_string(),
                        name: "bash".to_string(),
                        input: date_input,
                        caller: None,
                    },
                ],
                StopReason::ToolUse,
            ),
        },
    ));

    let mut steps_3 = stream_text_steps(wrap, 8, chunk_delay());
    steps_3.push(ScriptStep::immediate(StreamingEvent::FinalizedMessage {
        message: finalized_message(
            MODEL,
            vec![ContentBlock::new_text_block(wrap.to_string())],
            StopReason::EndTurn,
        ),
    }));

    vec![
        Script { steps: steps_1 },
        Script { steps: steps_2 },
        Script { steps: steps_3 },
    ]
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Set the leading delay on the first step of `steps`, leaving subsequent
/// step delays untouched. Cheaper than splicing an extra spacer event into
/// the stream and avoids emitting a redundant Start marker between sections.
fn delay_first(mut steps: Vec<ScriptStep>, delay: Duration) -> Vec<ScriptStep> {
    if let Some(first) = steps.first_mut() {
        first.delay = delay;
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity: every catalog entry produces at least one script with at
    /// least one event, and the last script ends with a finalized message.
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
            }
            let last = scripts.last().expect("non-empty");
            let last_step = last.steps.last().expect("non-empty");
            assert!(
                matches!(last_step.event, StreamingEvent::FinalizedMessage { .. }),
                "demo {name} doesn't end with FinalizedMessage"
            );
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
}
