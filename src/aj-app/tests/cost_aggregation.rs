//! Cost as the `/info` overlay reports it, over a real persisted log.
//!
//! Pricing itself is pinned in the provider adapters, which is one
//! layer below anything a user reads. What these pin is the stretch
//! above it: per-response dollar amounts survive aggregation and
//! rendering unchanged, and a session that burned tokens never renders
//! as free.

use aj_agent::message::AgentMessage;
use aj_app::session_info::{InfoRow, digest};
use aj_models::registry::{ModelCost, calculate_cost};
use aj_models::types::{
    AssistantContent, AssistantMessage, Message, StopReason, TextContent, Usage, UserMessage,
};
use aj_session::{ConversationEntryKind, ConversationLog, ConversationPersistence, ThreadKind};
use tempfile::TempDir;

/// Dollars per million tokens, the shape the catalog stores rates in.
fn rates(input: f64, output: f64, cache_read: f64, cache_write: f64) -> ModelCost {
    ModelCost {
        input,
        output,
        cache_read,
        cache_write,
        tiers: Vec::new(),
    }
}

/// An assistant response priced the way an adapter prices one: this
/// response's own tokens against the rates of the model that served it.
fn priced(
    provider: &str,
    model: &str,
    rates: &ModelCost,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
) -> Message {
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        ..Usage::default()
    };
    calculate_cost(rates, &mut usage);
    Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: "ok".to_string(),
            text_signature: None,
        })],
        api: "test".to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        account: None,
        response_id: None,
        usage,
        stop_reason: StopReason::Stop,
        error: None,
        timestamp: 0,
    })
}

/// A persisted log carrying `responses` on the user thread, preceded by
/// a prompt. The `TempDir` owns the scratch directory the log writes
/// into and is returned so it outlives the log.
fn log_with(responses: Vec<Message>) -> (TempDir, ConversationLog) {
    let dir = tempfile::tempdir().expect("scratch dir");
    let persistence = ConversationPersistence::new(dir.path().to_path_buf());
    let mut log = ConversationLog::create(&persistence).expect("create log");
    log.set_system_prompt("system".to_string())
        .expect("system prompt");
    let mut head = log.system_prompt_id().cloned().expect("system prompt id");
    head = log
        .append(
            Some(head),
            ThreadKind::User,
            None,
            ConversationEntryKind::Message {
                message: AgentMessage::wire(Message::User(UserMessage::text("go"))),
            },
        )
        .expect("prompt")
        .id;
    for response in responses {
        head = log
            .append(
                Some(head),
                ThreadKind::User,
                None,
                ConversationEntryKind::Message {
                    message: AgentMessage::wire(response),
                },
            )
            .expect("response")
            .id;
    }
    (dir, log)
}

/// The value the digest renders for `key`.
fn row(rows: &[InfoRow], key: &str) -> String {
    rows.iter()
        .find_map(|r| match r {
            InfoRow::Kv { key: k, value } if k == key => Some(value.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {key} row in the digest"))
}

/// A session whose turns ran on two models at different rates totals to
/// the sum of the per-response amounts.
///
/// The rates and token counts are chosen so that every wrong way of
/// reaching a total lands somewhere else: pricing the pooled tokens at
/// the first model's rates gives $42.00 and at the second's gives
/// $2.42, against the correct $7.20. Folding cache writes into the
/// input rate gives $6.70. So the assertion below fails for a
/// re-pricing bug, for a rate mix-up, and for a category fold alike,
/// rather than only for a lost addend.
#[test]
fn a_mixed_model_session_totals_the_sum_of_its_per_response_costs() {
    let dear = rates(10.0, 50.0, 1.0, 12.5);
    let cheap = rates(1.0, 2.0, 0.1, 0.0);

    // $1.00 + $0.50 + $1.00 + $2.50 = $5.00
    let on_dear = priced(
        "anthropic",
        "model-dear",
        &dear,
        100_000,
        10_000,
        1_000_000,
        200_000,
    );
    // $1.00 + $1.00 + $0.20 + $0.00 = $2.20
    let on_cheap = priced(
        "openai",
        "model-cheap",
        &cheap,
        1_000_000,
        500_000,
        2_000_000,
        0,
    );

    let (_dir, log) = log_with(vec![on_dear, on_cheap]);
    let stats = log.stats();
    let rows = digest(&stats, None);

    assert_eq!(
        row(&rows, "cost"),
        "$7.2000",
        "a two-model session must total the sum of what each response cost, \
         not the pooled tokens at either model's rates"
    );
    assert_eq!(
        row(&rows, "total tokens"),
        "4810000",
        "1,310,000 tokens on the first response and 3,500,000 on the second"
    );
}

/// Tokens and dollars have to agree about whether a session did any
/// work. A digest reporting millions of tokens beside no cost is the
/// shape a silent pricing failure takes, and it reads as a free
/// session.
#[test]
fn a_session_that_burned_tokens_never_reports_zero_cost() {
    let model = rates(3.0, 15.0, 0.3, 3.75);
    let (_dir, log) = log_with(vec![
        priced(
            "anthropic",
            "model-a",
            &model,
            200_000,
            20_000,
            4_000_000,
            100_000,
        ),
        priced(
            "anthropic",
            "model-a",
            &model,
            100_000,
            40_000,
            2_000_000,
            50_000,
        ),
    ]);
    let stats = log.stats();
    let rows = digest(&stats, None);

    let tokens = row(&rows, "total tokens");
    assert_ne!(
        tokens, "0",
        "the fixture must report tokens or this test measures nothing"
    );

    assert_ne!(
        row(&rows, "cost"),
        "$0.0000",
        "the digest reports {tokens} tokens and no dollars, which is how a \
         pricing failure reaches the user: as a session that looks free"
    );
    assert_eq!(
        row(&rows, "cost"),
        "$4.1625",
        "the two responses priced at their model's rates"
    );
}
