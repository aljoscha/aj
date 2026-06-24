//! Live experiment: do model APIs require previously-produced thinking /
//! reasoning blocks to be replayed back correctly, or does inference fail?
//!
//! `transform.rs` encodes an assumption (see its module docs and
//! `signatures_portable`): Anthropic thinking signatures are validated as
//! standalone tokens, and OpenAI-family reasoning items carry an encrypted
//! payload the API checks on replay. The premise underneath all of it is
//! that when a prior assistant turn ended in a tool call, the provider cares
//! about the thinking/reasoning block that preceded that tool call when it
//! rides along on the next request. This test probes that premise against
//! three live providers through the exact path the binary uses
//! (`complete_simple` -> `transform_messages` -> provider adapter).
//!
//! For each model we produce a turn that reasons and then calls a tool
//! (capturing the signed thinking block plus the tool call), then run two
//! phases over a `[user, assistant(thinking + tool_call), tool_result]`
//! continuation:
//!
//!   - Phase A (pipeline): replay the turn exactly as a frontend would
//!     persist it and capture what our own pipeline puts on the wire. This
//!     surfaces cases where `transform_messages` drops the prior reasoning
//!     before it is ever sent.
//!   - Phase B (provider probe): normalize the producing model id so the
//!     prior thinking actually rides on the wire, then replay it three ways
//!     and record ACCEPTED vs REJECTED:
//!       - INTACT:   thinking block replayed exactly as produced.
//!       - STRIPPED: thinking block removed from the assistant turn.
//!       - TAMPERED: the signature / encrypted reasoning payload corrupted
//!         (one character flipped) while kept well-formed. Skipped when the
//!         block carries no integrity-protected payload (see `Payload`).
//!
//! The outgoing request body is captured via `on_payload` so each variant's
//! verdict is backed by what was actually sent, not just what we intended.
//!
//! Gated behind `#[ignore]` and live credentials so it never runs in CI.
//! Run a single provider with, e.g.:
//!
//! ```text
//! cargo test -p aj-models --test thinking_replay_live \
//!     anthropic_opus_thinking_replay -- --ignored --nocapture
//! ```
//!
//! Credentials are resolved through `AuthStorage` (the same store the binary
//! uses): Anthropic OAuth from `~/.aj/auth.json`, OpenAI from
//! `OPENAI_API_KEY`, OpenRouter from `OPENROUTER_API_KEY`.

use aj_models::auth::AuthStorage;
use aj_models::provider::complete_simple;
use aj_models::registry::{ModelInfo, ModelRegistry};
use aj_models::types::{
    AssistantContent, AssistantMessage, Context, ErrorCategory, Message, OnPayload,
    SimpleStreamOptions, StopReason, StreamOptions, ThinkingLevel, ToolCall, ToolChoice,
    ToolDefinition, ToolResultMessage, UserMessage,
};
use std::sync::{Arc, Mutex};

const SYSTEM: &str = "You are a careful assistant. Think step by step before using tools.";
const PROMPT: &str = "Of Tokyo, Nairobi, and Reykjavik, which city is closest to the equator? \
     Reason about their latitudes step by step, then call get_weather for exactly that city.";

/// The single tool the model is offered. Calling it produces the tool-use
/// turn whose preceding thinking block is the subject of the experiment.
fn weather_tool() -> ToolDefinition {
    ToolDefinition {
        name: "get_weather".into(),
        description: "Get the current weather for a city.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
        }),
    }
}

/// How one replay variant fared against the live API.
#[derive(Debug)]
enum Outcome {
    Accepted {
        summary: String,
    },
    Rejected {
        category: ErrorCategory,
        http_status: Option<u16>,
        message: String,
    },
}

impl Outcome {
    fn is_accepted(&self) -> bool {
        matches!(self, Outcome::Accepted { .. })
    }

    fn describe(&self) -> String {
        match self {
            Outcome::Accepted { summary } => {
                format!("ACCEPTED -> {:?}", truncate(summary, 80))
            }
            Outcome::Rejected {
                category,
                http_status,
                message,
            } => format!(
                "REJECTED ({category:?}, http={}) -> {:?}",
                http_status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "-".into()),
                truncate(message, 160),
            ),
        }
    }
}

/// Classify a completed turn as accept/reject. A populated `error` with an
/// `Error`/`Aborted` stop reason is a rejection; anything else (the model
/// answered, called a tool, or hit the length cap) is an acceptance.
fn classify(msg: AssistantMessage) -> Outcome {
    match msg.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            let err = msg.error.unwrap_or_else(|| {
                aj_models::types::AssistantError::new(ErrorCategory::Unknown, "(no error detail)")
            });
            Outcome::Rejected {
                category: err.category,
                http_status: err.http_status,
                message: err.message,
            }
        }
        _ => Outcome::Accepted {
            summary: first_text(&msg).unwrap_or_else(|| "(no text block)".into()),
        },
    }
}

/// Load the catalog `ModelInfo` for `(provider, id)`, cloning it out of the
/// active registry so the test uses the real base URL, adaptive-thinking
/// flag, and token limits rather than a hand-built stand-in.
fn registry_model(provider: &str, id: &str) -> ModelInfo {
    ModelRegistry::load()
        .get(provider, id)
        .unwrap_or_else(|| panic!("model {provider}/{id} not found in the active catalog"))
        .clone()
}

/// Resolve the bearer credential for `provider_id` through the shared auth
/// store (handles OAuth refresh and env-var fallbacks).
async fn resolve_key(provider_id: &str) -> String {
    let auth = AuthStorage::at_default_path().expect("auth.json path (HOME unset?)");
    auth.get_api_key(provider_id)
        .await
        .unwrap_or_else(|e| panic!("failed to resolve credentials for {provider_id:?}: {e}"))
        .unwrap_or_else(|| {
            panic!("no credential available for {provider_id:?}; log in or set the env var")
        })
}

fn options(key: &str, reasoning: ThinkingLevel) -> SimpleStreamOptions {
    SimpleStreamOptions {
        base: StreamOptions {
            api_key: Some(key.to_string()),
            max_tokens: Some(2048),
            tool_choice: Some(ToolChoice::Auto),
            ..Default::default()
        },
        reasoning: Some(reasoning),
    }
}

/// Run turn 1: ask the model to reason and then call the weather tool.
async fn produce_turn(model: &ModelInfo, key: &str, reasoning: ThinkingLevel) -> AssistantMessage {
    let ctx = Context {
        system_prompt: Some(SYSTEM.into()),
        messages: vec![Message::User(UserMessage::text(PROMPT))],
        tools: vec![weather_tool()],
    };
    complete_simple(model, &ctx, &options(key, reasoning)).await
}

/// What the outgoing request actually carried for the prior-turn
/// thinking/reasoning, scraped from the on-wire body. Lets us prove the
/// TAMPERED variant delivered a corrupted-but-present block rather than
/// silently dropping it (which would make it indistinguishable from
/// STRIPPED).
#[derive(Debug, Default)]
struct WireInfo {
    /// Number of thinking/reasoning blocks in the request body.
    blocks: usize,
    /// First 16 chars of the first block's signature / encrypted payload.
    head: Option<String>,
}

/// Replay `history` and report how the API responded, alongside what the
/// outgoing request body carried for prior-turn thinking.
async fn run_case(
    model: &ModelInfo,
    key: &str,
    reasoning: ThinkingLevel,
    history: &[Message],
) -> (Outcome, WireInfo) {
    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);

    let mut opts = options(key, reasoning);
    opts.base.on_payload = Some(OnPayload::new(move |body: &serde_json::Value| {
        sink.lock().unwrap().push(body.clone());
    }));

    let ctx = Context {
        system_prompt: Some(SYSTEM.into()),
        messages: history.to_vec(),
        tools: vec![weather_tool()],
    };
    let outcome = classify(complete_simple(model, &ctx, &opts).await);
    let wire = captured
        .lock()
        .unwrap()
        .first()
        .map(wire_thinking_info)
        .unwrap_or_default();
    (outcome, wire)
}

/// Extract the prior-turn thinking footprint from an outgoing request body,
/// covering both the Anthropic Messages shape (`messages[].content[]` with
/// `thinking`/`redacted_thinking` blocks) and the OpenAI Responses shape
/// (`input[]` with `reasoning` items).
fn wire_thinking_info(body: &serde_json::Value) -> WireInfo {
    let mut info = WireInfo::default();
    let mut record = |s: Option<&str>| {
        info.blocks += 1;
        if info.head.is_none() {
            info.head = s.map(|s| s.chars().take(16).collect());
        }
    };

    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
                continue;
            };
            for block in content {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("thinking") => record(block.get("signature").and_then(|s| s.as_str())),
                    Some("redacted_thinking") => record(block.get("data").and_then(|s| s.as_str())),
                    _ => {}
                }
            }
        }
    }
    if let Some(input) = body.get("input").and_then(|i| i.as_array()) {
        for item in input {
            if item.get("type").and_then(|t| t.as_str()) == Some("reasoning") {
                record(item.get("encrypted_content").and_then(|s| s.as_str()));
            }
        }
    }
    info
}

/// `[user, assistant(produced), tool_result]` continuation.
fn build_history(assistant: &AssistantMessage, call: &ToolCall) -> Vec<Message> {
    vec![
        Message::User(UserMessage::text(PROMPT)),
        Message::Assistant(assistant.clone()),
        Message::ToolResult(ToolResultMessage::text(
            &call.id,
            &call.name,
            "18C, sunny, light wind",
            false,
        )),
    ]
}

/// Drop every thinking block from the assistant turn, leaving the tool call
/// (and any text) without its preceding reasoning.
fn strip_thinking(history: &[Message]) -> Vec<Message> {
    let mut out = history.to_vec();
    for msg in &mut out {
        if let Message::Assistant(a) = msg {
            a.content
                .retain(|c| !matches!(c, AssistantContent::Thinking(_)));
        }
    }
    out
}

/// Corrupt the first signed thinking block's signature, keeping it
/// structurally valid so only the cryptographic / encrypted-payload check
/// can fail. Anthropic stores a raw base64 signature; OpenAI-family stores a
/// JSON reasoning item whose `encrypted_content` is the validated field.
fn corrupt_thinking(history: &[Message]) -> Vec<Message> {
    let mut out = history.to_vec();
    'outer: for msg in &mut out {
        if let Message::Assistant(a) = msg {
            for c in &mut a.content {
                if let AssistantContent::Thinking(th) = c {
                    if let Some(sig) = th.thinking_signature.as_ref() {
                        th.thinking_signature = Some(corrupt_signature(sig));
                        break 'outer;
                    }
                }
            }
        }
    }
    out
}

/// The kind of integrity-protected payload a prior thinking block carries,
/// which determines whether "tampering" is even meaningful.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Payload {
    /// Anthropic: an opaque base64 signature string.
    Signature,
    /// OpenAI Responses: a reasoning item with a non-empty `encrypted_content`.
    Encrypted,
    /// A reasoning item with no signed/encrypted field (e.g. OpenRouter's
    /// GLM, which returns plaintext `content`/`summary` only). There is
    /// nothing for the provider to validate.
    None,
}

/// Classify what a produced `thinking_signature` carries.
fn payload_kind(sig: &str) -> Payload {
    match serde_json::from_str::<serde_json::Value>(sig) {
        Ok(v) => {
            if v.get("encrypted_content")
                .and_then(|e| e.as_str())
                .is_some_and(|s| !s.is_empty())
            {
                Payload::Encrypted
            } else {
                Payload::None
            }
        }
        Err(_) => Payload::Signature,
    }
}

/// Flip one character of a thinking signature. If the signature is a JSON
/// reasoning item (OpenAI / OpenRouter), corrupt its `encrypted_content`
/// in place; otherwise treat it as a raw base64 token (Anthropic). Only
/// called for signatures that carry a payload to corrupt (see [`Payload`]),
/// so the JSON path always finds `encrypted_content`.
fn corrupt_signature(sig: &str) -> String {
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(sig)
        && let Some(enc) = v.get("encrypted_content").and_then(|e| e.as_str())
    {
        v["encrypted_content"] = serde_json::Value::String(flip_first_char(enc));
        return v.to_string();
    }
    flip_first_char(sig)
}

fn flip_first_char(s: &str) -> String {
    let mut chars: Vec<char> = s.chars().collect();
    if let Some(c) = chars.first_mut() {
        *c = if *c == 'A' { 'B' } else { 'A' };
    }
    chars.into_iter().collect()
}

fn first_text(msg: &AssistantMessage) -> Option<String> {
    msg.content.iter().find_map(|c| match c {
        AssistantContent::Text(t) if !t.text.is_empty() => Some(t.text.clone()),
        _ => None,
    })
}

fn first_tool_call(msg: &AssistantMessage) -> Option<ToolCall> {
    msg.content.iter().find_map(|c| match c {
        AssistantContent::ToolCall(tc) => Some(tc.clone()),
        _ => None,
    })
}

fn signed_thinking(msg: &AssistantMessage) -> Option<&aj_models::types::ThinkingContent> {
    msg.content.iter().find_map(|c| match c {
        AssistantContent::Thinking(th) if th.thinking_signature.is_some() => Some(th),
        _ => None,
    })
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// Drive the full experiment for one model and print a verdict.
///
/// Hard-asserts only the baseline: the INTACT replay must be accepted, since
/// the strip/tamper results are meaningless if even a faithful replay fails
/// (e.g. due to missing model access). The strip/tamper outcomes are
/// reported as findings.
async fn run_experiment(provider: &str, model_id: &str, reasoning: ThinkingLevel) {
    let model = registry_model(provider, model_id);
    let key = resolve_key(provider).await;

    println!(
        "\n=== Experiment: {provider}/{model_id} (api={}) ===",
        model.api
    );

    let produced = produce_turn(&model, &key, reasoning.clone()).await;
    let tool_call = first_tool_call(&produced);
    let signed = signed_thinking(&produced);

    println!(
        "produce: api={:?} provider={:?} model={:?}",
        produced.api, produced.provider, produced.model
    );
    println!(
        "         stop_reason={:?}, signed_thinking={}, tool_call={}",
        produced.stop_reason,
        signed.is_some(),
        tool_call
            .as_ref()
            .map(|t| format!("{} (id={})", t.name, t.id))
            .unwrap_or_else(|| "none".into()),
    );
    if let Some(th) = signed {
        let sig = th.thinking_signature.as_deref().unwrap_or("");
        println!(
            "         thinking text={:?}, signature {} chars (json={})",
            truncate(&th.thinking, 60),
            sig.len(),
            sig.trim_start().starts_with('{'),
        );
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(sig) {
            let keys: Vec<&str> = v
                .as_object()
                .map(|o| o.keys().map(String::as_str).collect())
                .unwrap_or_default();
            println!(
                "         signature JSON type={:?}, keys={:?}",
                v.get("type").and_then(|t| t.as_str()),
                keys,
            );
        } else {
            println!("         signature head={:?}", truncate(sig, 80));
        }
    }

    let Some(call) = tool_call else {
        panic!(
            "produce turn did not call a tool (stop_reason={:?}); the experiment needs a \
             tool-use turn to probe thinking-block validation. Re-run, or adjust the prompt.",
            produced.stop_reason
        );
    };
    let had_signed_thinking = signed.is_some();
    if !had_signed_thinking {
        println!(
            "         NOTE: turn 1 produced no signed thinking block, so STRIPPED/TAMPERED \
             are uninformative for this provider."
        );
    }

    let line = |o: &Outcome, w: &WireInfo| {
        format!(
            "{}  [wire: {} thinking block(s), head={:?}]",
            o.describe(),
            w.blocks,
            w.head.as_deref().unwrap_or("-")
        )
    };

    // -- Phase A: natural same-session replay -------------------------------
    // Replay the assistant turn exactly as a frontend would persist it, model
    // id and all. This shows what our own pipeline (`transform_messages` plus
    // the provider adapter) actually puts on the wire for prior reasoning.
    let natural = build_history(&produced, &call);
    let (_a_outcome, natural_wire) = run_case(&model, &key, reasoning.clone(), &natural).await;
    println!(
        "  [PIPELINE] natural replay carried {} prior-thinking block(s) on the wire",
        natural_wire.blocks
    );
    if had_signed_thinking && natural_wire.blocks == 0 {
        println!(
            "             NOTE: our transform dropped the prior reasoning before sending. \
             produced model id {:?} != catalog id {:?}, and signatures are not portable for \
             this provider, so `is_same_model` is false and the encrypted reasoning is demoted \
             to plain text.",
            produced.model, model.id
        );
    }

    // -- Phase B: forced-replay provider probe ------------------------------
    // Normalize the producing model id to the target so `is_same_model` holds
    // and the signed thinking/reasoning actually rides on the wire. Only then
    // can we observe how the *provider* reacts to a correct, stripped, or
    // tampered prior thinking block.
    let mut normalized = produced.clone();
    normalized.model = model.id.clone();
    let history = build_history(&normalized, &call);

    let payload = signed.map(|th| payload_kind(th.thinking_signature.as_deref().unwrap_or("")));
    if let Some(p) = payload {
        println!("         payload kind: {p:?}");
    }

    let (intact, intact_wire) = run_case(&model, &key, reasoning.clone(), &history).await;
    let (stripped, stripped_wire) =
        run_case(&model, &key, reasoning.clone(), &strip_thinking(&history)).await;
    println!("  [INTACT]   {}", line(&intact, &intact_wire));
    println!("  [STRIPPED] {}", line(&stripped, &stripped_wire));

    // Tampering is only meaningful when the prior block carries a payload the
    // provider could validate (Anthropic signature, OpenAI encrypted_content).
    // A plaintext-only reasoning item (GLM) has nothing to corrupt.
    let tampered = if matches!(payload, Some(Payload::Signature | Payload::Encrypted)) {
        let (t, tw) = run_case(&model, &key, reasoning.clone(), &corrupt_thinking(&history)).await;
        println!("  [TAMPERED] {}", line(&t, &tw));
        Some((t, tw))
    } else {
        println!("  [TAMPERED] N/A (reasoning item carries no validated payload to corrupt)");
        None
    };

    // The accept/reject reading is only interpretable once we have confirmed
    // the variants shaped the wire as intended: intact carries the block,
    // stripped carries none, tampered carries a changed but still-present one.
    let probe_valid = had_signed_thinking && intact_wire.blocks > 0;
    if probe_valid {
        assert_eq!(
            stripped_wire.blocks, 0,
            "STRIPPED variant should send no thinking block"
        );
        if let Some((_, tw)) = &tampered {
            assert_eq!(
                tw.blocks, intact_wire.blocks,
                "TAMPERED variant should keep the thinking block present, only corrupt it"
            );
            assert_ne!(
                tw.head, intact_wire.head,
                "TAMPERED variant should change the signature/encrypted payload on the wire"
            );
        }
    }

    let verdict = if !had_signed_thinking {
        "INCONCLUSIVE: no signed thinking block to strip/tamper".to_string()
    } else if intact_wire.blocks == 0 {
        "INCONCLUSIVE: even the intact forced replay carried no thinking block on the wire"
            .to_string()
    } else if !intact.is_accepted() {
        "INCONCLUSIVE: baseline (intact) replay was itself rejected".to_string()
    } else {
        let stripped_note = if stripped.is_accepted() {
            "stripping the block was tolerated"
        } else {
            "stripping the block was REJECTED"
        };
        match &tampered {
            None => format!(
                "NO VALIDATED PAYLOAD: reasoning item has no signature/encrypted_content, \
                 so integrity cannot be enforced; {stripped_note}."
            ),
            Some((t, _)) => {
                let tamper_note = if t.is_accepted() {
                    "tampering the payload was tolerated"
                } else {
                    "tampering the payload was REJECTED (integrity enforced)"
                };
                format!("{tamper_note}; {stripped_note}.")
            }
        }
    };
    println!("  VERDICT: {verdict}");

    assert!(
        intact.is_accepted(),
        "baseline INTACT replay must be accepted for the experiment to be valid; got {intact:?}"
    );

    // Core assumption under test: when a prior thinking block carries an
    // integrity-protected payload and that payload is corrupted, the provider
    // must reject the request. (Providers do not, however, require the block
    // to be present at all, so we deliberately do not assert on STRIPPED.)
    if let Some((t, _)) = &tampered
        && intact.is_accepted()
    {
        assert!(
            !t.is_accepted(),
            "expected the provider to reject a tampered {payload:?} payload, but it was \
             accepted: {t:?}"
        );
    }
}

#[tokio::test]
#[ignore = "live network; requires Anthropic OAuth in ~/.aj/auth.json"]
async fn anthropic_opus_thinking_replay() {
    run_experiment("anthropic", "claude-opus-4-8", ThinkingLevel::High).await;
}

#[tokio::test]
#[ignore = "live network; requires OPENAI_API_KEY with gpt-5.5 Responses access"]
async fn openai_gpt55_thinking_replay() {
    run_experiment("openai", "gpt-5.5", ThinkingLevel::Medium).await;
}

#[tokio::test]
#[ignore = "live network; requires OPENROUTER_API_KEY"]
async fn openrouter_glm52_thinking_replay() {
    run_experiment("openrouter", "z-ai/glm-5.2", ThinkingLevel::Medium).await;
}
