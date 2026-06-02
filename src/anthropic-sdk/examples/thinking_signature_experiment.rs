//! Experiment: does changing the thinking level (or model) invalidate the
//! signature on previously-produced thinking blocks?
//!
//! Context: `aj-models`' cross-provider transform (`transform.rs`) drops
//! thinking-block signatures on a *model* change, on the premise that
//! Anthropic signatures are cryptographically bound to the producing model.
//! There is, by contrast, no code that touches thinking blocks when only the
//! *thinking level* changes between turns of the same model. This program
//! probes the real Anthropic API to confirm both, across two regimes:
//!
//! * Budget-based reasoning models (`Thinking::Enabled { budget_tokens }`),
//!   e.g. claude-sonnet-4-5. "Thinking level" == `budget_tokens`.
//! * Adaptive reasoning models (`Thinking::Adaptive` + `output_config.effort`),
//!   e.g. claude-sonnet-4-6, claude-opus-4-8. "Thinking level" == `effort`.
//!
//! Each part includes a NEGATIVE CONTROL (a corrupted signature) so we can
//! tell whether the API is actually validating signatures in that position.
//! Anthropic only requires (and validates) thinking blocks on the last
//! assistant turn when that turn ends in `tool_use`, so the strict cases all
//! use a tool-call continuation.
//!
//! Run with:
//!   cargo run -p anthropic-sdk --example thinking_signature_experiment
//!
//! Auth: reads `ANTHROPIC_OAUTH_TOKEN` (or `ANTHROPIC_API_KEY`) from the
//! environment, falling back to `~/.aj/.env`.

use anthropic_sdk::client::Client;
use anthropic_sdk::messages::{
    ContentBlock, ContentBlockParam, Message, MessageParam, Messages, OutputConfig, OutputEffort,
    Thinking, ToolResultContent, ToolUnion,
};
use serde_json::json;

/// Budget-based (non-adaptive) producer. Lets us set explicit budgets.
const BUDGET_MODEL: &str = "claude-sonnet-4-5";
/// A different budget-based model, for the budget-regime model-change test.
const BUDGET_MODEL_OTHER: &str = "claude-haiku-4-5";
/// A budget-based model used as a cross-regime target (adaptive -> budget).
const BUDGET_MODEL_OPUS: &str = "claude-opus-4-1";

/// Adaptive producers requested by the experiment.
const ADAPTIVE_SONNET: &str = "claude-sonnet-4-6";
const ADAPTIVE_OPUS: &str = "claude-opus-4-8";

/// Minimum Anthropic thinking budget is 1024 tokens.
const BUDGET_LOW: u64 = 1024;
const BUDGET_HIGH: u64 = 4096;

/// How to drive thinking for a single request.
#[derive(Clone)]
enum Think {
    Disabled,
    /// Budget-based reasoning model.
    Budget(u64),
    /// Adaptive reasoning model with the given effort.
    Adaptive(OutputEffort),
}

impl Think {
    /// Returns `(thinking, output_config, max_tokens)` for the wire request.
    fn wire(&self) -> (Option<Thinking>, Option<OutputConfig>, u64) {
        match self {
            Think::Disabled => (Some(Thinking::Disabled), None, 2048),
            Think::Budget(b) => (
                Some(Thinking::Enabled {
                    budget_tokens: *b,
                    display: None,
                }),
                None,
                b + 2048,
            ),
            Think::Adaptive(effort) => (
                Some(Thinking::Adaptive { display: None }),
                Some(OutputConfig {
                    effort: Some(effort.clone()),
                    format: None,
                    task_budget: None,
                }),
                8192,
            ),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api_key = load_token()?;
    let client = Client::new(None, api_key);

    println!("=== Thinking-block signature experiment ===\n");

    budget_regime(&client).await?;
    adaptive_regime(&client, ADAPTIVE_SONNET, "E").await?;
    adaptive_regime(&client, ADAPTIVE_OPUS, "F").await?;

    // Cross-model within the adaptive regime, plus a cross-regime jump.
    println!("\n## Part G: adaptive cross-model replay (tool-use)\n");
    let weather_tool = weather_tool();
    let (g_user, g_assistant) = produce_turn(
        &client,
        ADAPTIVE_SONNET,
        Think::Adaptive(OutputEffort::Low),
        "What is the weather in Berlin right now? Use the get_weather tool.",
        std::slice::from_ref(&weather_tool),
    )
    .await?;
    if let Some(id) = find_tool_use_id(&g_assistant) {
        let history = with_tool_result(&g_user, &g_assistant, &id);
        run_case(
            &client,
            &format!("G1: {ADAPTIVE_SONNET} -> {ADAPTIVE_OPUS} (adaptive->adaptive), VALID sig"),
            ADAPTIVE_OPUS,
            Think::Adaptive(OutputEffort::Low),
            &history,
            std::slice::from_ref(&weather_tool),
        )
        .await;
        run_case(
            &client,
            &format!("G1-neg: {ADAPTIVE_OPUS} target, CORRUPTED sig (expect REJECT if validated)"),
            ADAPTIVE_OPUS,
            Think::Adaptive(OutputEffort::Low),
            &corrupt_sig(&history),
            std::slice::from_ref(&weather_tool),
        )
        .await;
        run_case(
            &client,
            &format!("G2: {ADAPTIVE_SONNET} -> {BUDGET_MODEL} (adaptive->budget), VALID sig"),
            BUDGET_MODEL,
            Think::Budget(BUDGET_LOW),
            &history,
            std::slice::from_ref(&weather_tool),
        )
        .await;
        run_case(
            &client,
            &format!("G2-neg: {BUDGET_MODEL} target, CORRUPTED sig (expect REJECT if validated)"),
            BUDGET_MODEL,
            Think::Budget(BUDGET_LOW),
            &corrupt_sig(&history),
            std::slice::from_ref(&weather_tool),
        )
        .await;
    }

    println!("\n=== done ===");
    Ok(())
}

/// Budget-based regime: parts A (text), B (tool-use), D (negative controls),
/// C (model change). Mirrors the original experiment.
async fn budget_regime(client: &Client) -> anyhow::Result<()> {
    println!("## Part A: text-only continuation (budget model, varying budget)\n");
    let (a_user, a_assistant) = produce_turn(
        client,
        BUDGET_MODEL,
        Think::Budget(BUDGET_LOW),
        "What is 27 * 34? Reason about it step by step.",
        &[],
    )
    .await?;
    let followup = MessageParam::new_user_message(vec![ContentBlockParam::new_text_block(
        "Now add 100 to that result.".to_string(),
    )]);
    let text_history = vec![a_user, a_assistant, followup];

    run_case(
        client,
        "A1 control: same model, same budget (1024)",
        BUDGET_MODEL,
        Think::Budget(BUDGET_LOW),
        &text_history,
        &[],
    )
    .await;
    run_case(
        client,
        "A2: same model, higher budget (1024 -> 4096)",
        BUDGET_MODEL,
        Think::Budget(BUDGET_HIGH),
        &text_history,
        &[],
    )
    .await;
    run_case(
        client,
        "A3: same model, thinking DISABLED",
        BUDGET_MODEL,
        Think::Disabled,
        &text_history,
        &[],
    )
    .await;

    println!("\n## Part B/D: tool-use continuation + negative controls (budget model)\n");
    let weather_tool = weather_tool();
    let (b_user, b_assistant) = produce_turn(
        client,
        BUDGET_MODEL,
        Think::Budget(BUDGET_LOW),
        "What is the weather in Paris right now? Use the get_weather tool.",
        std::slice::from_ref(&weather_tool),
    )
    .await?;
    let Some(id) = find_tool_use_id(&b_assistant) else {
        println!("B: no tool_use produced in turn 1; skipping tool-use cases.\n");
        return Ok(());
    };
    let tool_history = with_tool_result(&b_user, &b_assistant, &id);

    run_case(
        client,
        "B1 control: same model, same budget (1024)",
        BUDGET_MODEL,
        Think::Budget(BUDGET_LOW),
        &tool_history,
        std::slice::from_ref(&weather_tool),
    )
    .await;
    run_case(
        client,
        "B2: same model, higher budget (1024 -> 4096)",
        BUDGET_MODEL,
        Think::Budget(BUDGET_HIGH),
        &tool_history,
        std::slice::from_ref(&weather_tool),
    )
    .await;
    run_case(
        client,
        "B3: same model, thinking DISABLED",
        BUDGET_MODEL,
        Think::Disabled,
        &tool_history,
        std::slice::from_ref(&weather_tool),
    )
    .await;
    run_case(
        client,
        "D1: same model, CORRUPTED signature (expect REJECT if validated)",
        BUDGET_MODEL,
        Think::Budget(BUDGET_LOW),
        &corrupt_sig(&tool_history),
        std::slice::from_ref(&weather_tool),
    )
    .await;
    run_case(
        client,
        "D2: same model, edited thinking TEXT + original sig",
        BUDGET_MODEL,
        Think::Budget(BUDGET_LOW),
        &edit_thinking_text(&tool_history),
        std::slice::from_ref(&weather_tool),
    )
    .await;

    println!("\n## Part C: budget-model model change (tool-use + negative controls)\n");
    run_case(
        client,
        &format!("C1: {BUDGET_MODEL} -> {BUDGET_MODEL_OTHER}, VALID sig"),
        BUDGET_MODEL_OTHER,
        Think::Budget(BUDGET_LOW),
        &tool_history,
        std::slice::from_ref(&weather_tool),
    )
    .await;
    run_case(
        client,
        &format!("C1-neg: {BUDGET_MODEL_OTHER} target, CORRUPTED sig (expect REJECT if validated)"),
        BUDGET_MODEL_OTHER,
        Think::Budget(BUDGET_LOW),
        &corrupt_sig(&tool_history),
        std::slice::from_ref(&weather_tool),
    )
    .await;
    run_case(
        client,
        &format!("C3: {BUDGET_MODEL} -> {BUDGET_MODEL_OPUS}, VALID sig"),
        BUDGET_MODEL_OPUS,
        Think::Budget(BUDGET_LOW),
        &tool_history,
        std::slice::from_ref(&weather_tool),
    )
    .await;
    run_case(
        client,
        &format!("C3-neg: {BUDGET_MODEL_OPUS} target, CORRUPTED sig (expect REJECT if validated)"),
        BUDGET_MODEL_OPUS,
        Think::Budget(BUDGET_LOW),
        &corrupt_sig(&tool_history),
        std::slice::from_ref(&weather_tool),
    )
    .await;
    Ok(())
}

/// Adaptive regime for `model`: produce a tool-use turn that requires
/// reasoning (so the model actually emits a signed thinking block), then vary
/// effort (the adaptive "thinking level"), disable thinking, and run a
/// corrupted-signature negative control.
async fn adaptive_regime(client: &Client, model: &str, tag: &str) -> anyhow::Result<()> {
    println!("\n## Part {tag}: adaptive model {model} (tool-use, varying effort)\n");
    let weather_tool = weather_tool();
    // A multi-step prompt at high effort coaxes adaptive models into emitting
    // a thinking block; a trivial prompt lets them skip thinking entirely.
    let (user, assistant) = produce_turn(
        client,
        model,
        Think::Adaptive(OutputEffort::High),
        "Of Tokyo, Nairobi, and Reykjavik, which city is closest to the equator? \
         Reason about their latitudes step by step, then call get_weather for \
         exactly that city.",
        std::slice::from_ref(&weather_tool),
    )
    .await?;
    let Some(id) = find_tool_use_id(&assistant) else {
        println!("{tag}: no tool_use produced in turn 1; skipping.\n");
        return Ok(());
    };
    if !has_thinking_block(&assistant) {
        println!(
            "{tag}: NOTE - turn 1 emitted no thinking block, so the corrupted-sig \
             negative control below is uninformative for this model.\n"
        );
    }
    let history = with_tool_result(&user, &assistant, &id);

    run_case(
        client,
        &format!("{tag}1 control: same model, same effort (Low)"),
        model,
        Think::Adaptive(OutputEffort::Low),
        &history,
        std::slice::from_ref(&weather_tool),
    )
    .await;
    run_case(
        client,
        &format!("{tag}2: same model, effort Low -> High (level change)"),
        model,
        Think::Adaptive(OutputEffort::High),
        &history,
        std::slice::from_ref(&weather_tool),
    )
    .await;
    run_case(
        client,
        &format!("{tag}3: same model, effort Low -> Max (level change)"),
        model,
        Think::Adaptive(OutputEffort::Max),
        &history,
        std::slice::from_ref(&weather_tool),
    )
    .await;
    run_case(
        client,
        &format!("{tag}4: same model, thinking DISABLED"),
        model,
        Think::Disabled,
        &history,
        std::slice::from_ref(&weather_tool),
    )
    .await;
    run_case(
        client,
        &format!("{tag}-neg: same model, CORRUPTED sig (expect REJECT if validated)"),
        model,
        Think::Adaptive(OutputEffort::Low),
        &corrupt_sig(&history),
        std::slice::from_ref(&weather_tool),
    )
    .await;
    Ok(())
}

/// Run turn 1 and return `(user_message, assistant_message)` where the
/// assistant message preserves whatever thinking block(s) the model emitted.
async fn produce_turn(
    client: &Client,
    model: &str,
    think: Think,
    prompt: &str,
    tools: &[ToolUnion],
) -> anyhow::Result<(MessageParam, MessageParam)> {
    let (thinking, output_config, max_tokens) = think.wire();
    let user =
        MessageParam::new_user_message(vec![ContentBlockParam::new_text_block(prompt.to_string())]);
    let req = Messages {
        model: model.to_string(),
        max_tokens,
        messages: vec![user.clone()],
        thinking,
        output_config,
        tools: tools.to_vec(),
        ..Default::default()
    };
    let resp = client.messages(req).await?;
    describe_response(model, &resp);
    Ok((user, resp.into_message_param()))
}

/// Build a continuation request from `history` and report accept/reject.
async fn run_case(
    client: &Client,
    label: &str,
    model: &str,
    think: Think,
    history: &[MessageParam],
    tools: &[ToolUnion],
) {
    let (thinking, output_config, max_tokens) = think.wire();
    let req = Messages {
        model: model.to_string(),
        max_tokens,
        messages: history.to_vec(),
        thinking,
        output_config,
        tools: tools.to_vec(),
        ..Default::default()
    };
    match client.messages(req).await {
        Ok(resp) => {
            let summary: String = first_text(&resp)
                .unwrap_or_else(|| "(no text block)".to_string())
                .chars()
                .take(80)
                .collect();
            println!("[ACCEPTED] {label}\n           -> {summary}\n");
        }
        Err(e) => println!("[REJECTED] {label}\n           -> {e}\n"),
    }
}

fn weather_tool() -> ToolUnion {
    ToolUnion::Custom {
        name: "get_weather".to_string(),
        description: Some("Get the current weather for a city.".to_string()),
        input_schema: json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
        }),
        cache_control: None,
        allowed_callers: vec![],
        defer_loading: None,
        eager_input_streaming: None,
        input_examples: vec![],
        strict: None,
    }
}

/// `[user, assistant, tool_result]` continuation history.
fn with_tool_result(user: &MessageParam, assistant: &MessageParam, id: &str) -> Vec<MessageParam> {
    let tool_result = MessageParam::new_user_message(vec![ContentBlockParam::ToolResultBlock {
        tool_use_id: id.to_string(),
        cache_control: None,
        content: ToolResultContent::Text("18C, sunny".to_string()),
        is_error: false,
    }]);
    vec![user.clone(), assistant.clone(), tool_result]
}

/// Print thinking/redacted/tool blocks from a turn-1 response so the
/// transcript shows what we are about to replay.
fn describe_response(model: &str, resp: &Message) {
    println!("turn 1 produced on {model}:");
    for block in &resp.content {
        match block {
            ContentBlock::ThinkingBlock {
                signature,
                thinking,
            } => {
                let sig: String = signature.chars().take(24).collect();
                let txt: String = thinking.chars().take(50).collect();
                println!(
                    "  thinking: sig=\"{sig}...\" ({} chars), text=\"{txt}...\"",
                    signature.len()
                );
            }
            ContentBlock::RedactedThinkingBlock { data } => {
                println!("  redacted_thinking: data=({} chars)", data.len());
            }
            ContentBlock::TextBlock { text, .. } => {
                let txt: String = text.chars().take(50).collect();
                println!("  text: \"{txt}...\"");
            }
            ContentBlock::ToolUseBlock { name, id, .. } => {
                println!("  tool_use: name={name} id={id}");
            }
            _ => println!("  (other block)"),
        }
    }
    println!();
}

fn find_tool_use_id(msg: &MessageParam) -> Option<String> {
    msg.content.iter().find_map(|b| match b {
        ContentBlockParam::ToolUseBlock { id, .. } => Some(id.clone()),
        _ => None,
    })
}

fn has_thinking_block(msg: &MessageParam) -> bool {
    msg.content.iter().any(|b| {
        matches!(
            b,
            ContentBlockParam::ThinkingBlock { .. }
                | ContentBlockParam::RedactedThinkingBlock { .. }
        )
    })
}

fn first_text(resp: &Message) -> Option<String> {
    resp.content.iter().find_map(|b| match b {
        ContentBlock::TextBlock { text, .. } => Some(text.clone()),
        _ => None,
    })
}

/// Corrupt the signature-bearing field of the first thinking-ish block found,
/// covering both visible (`ThinkingBlock`) and redacted thinking. Keeps length
/// and character class so only the cryptographic check can fail.
fn corrupt_sig(history: &[MessageParam]) -> Vec<MessageParam> {
    let mut out = history.to_vec();
    'outer: for msg in &mut out {
        for block in &mut msg.content {
            match block {
                ContentBlockParam::ThinkingBlock { signature, .. } => {
                    *signature = flip_first_char(signature);
                    break 'outer;
                }
                ContentBlockParam::RedactedThinkingBlock { data } => {
                    *data = flip_first_char(data);
                    break 'outer;
                }
                _ => {}
            }
        }
    }
    out
}

/// Append to the visible thinking text of the first `ThinkingBlock`, leaving
/// its original signature in place.
fn edit_thinking_text(history: &[MessageParam]) -> Vec<MessageParam> {
    let mut out = history.to_vec();
    'outer: for msg in &mut out {
        for block in &mut msg.content {
            if let ContentBlockParam::ThinkingBlock { thinking, .. } = block {
                thinking.push_str(" (tampered)");
                break 'outer;
            }
        }
    }
    out
}

fn flip_first_char(s: &str) -> String {
    let mut chars: Vec<char> = s.chars().collect();
    if let Some(c) = chars.first_mut() {
        *c = if *c == 'A' { 'B' } else { 'A' };
    }
    chars.into_iter().collect()
}

/// Read the Anthropic token from the environment, falling back to parsing
/// `~/.aj/.env` (which may use `export KEY=value` lines).
fn load_token() -> anyhow::Result<String> {
    for var in ["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            return Ok(v);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let path = format!("{home}/.aj/.env");
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("no token in env and could not read {path}: {e}"))?;
    for line in contents.lines() {
        let trimmed = line.trim();
        let line = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        for var in ["ANTHROPIC_OAUTH_TOKEN", "ANTHROPIC_API_KEY"] {
            if let Some(rest) = line.strip_prefix(&format!("{var}=")) {
                let val = rest.trim().trim_matches('"').trim_matches('\'');
                if !val.is_empty() {
                    return Ok(val.to_string());
                }
            }
        }
    }
    anyhow::bail!("could not find ANTHROPIC_OAUTH_TOKEN or ANTHROPIC_API_KEY")
}
