//! What the `/info` overlay reports for a session whose turns did not
//! all end cleanly.
//!
//! The fixtures here are messages a provider adapter produced, replayed
//! from SSE frames, rather than messages the test built. That is what
//! makes these tests sensitive to where pricing happens: a turn cut off
//! before its terminal frame has to arrive at the log already priced,
//! because nothing downstream of the adapter knows the rates.

use aj_agent::message::AgentMessage;
use aj_app::session_info::{InfoRow, digest};
use aj_models::anthropic::provider::replay_sse_events;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::types::{AssistantMessage, Message, Usage, UserMessage};
use aj_session::{
    ConversationEntryKind, ConversationLog, ConversationPersistence, ThreadFilter, ThreadKind,
};
use anthropic_sdk::messages::{
    ContentBlock as AContentBlock, ContentBlockDelta as AContentBlockDelta, Message as AMessage,
    MessageType, Role as ARole, ServerSentEvent, Usage as AUsage,
};
use tempfile::TempDir;

fn model() -> ModelInfo {
    ModelInfo {
        id: "claude-test".into(),
        name: "Claude Test".into(),
        family: None,
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: "https://api.anthropic.com".into(),
        reasoning: false,
        reasoning_options: Vec::new(),
        supports_verbosity: false,
        input: vec![InputModality::Text],
        cost: ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
            tiers: Vec::new(),
        },
        context_window: 200_000,
        max_tokens: 8_192,
    }
}

/// The opening frame, carrying the counts the request is billed for
/// before any output exists.
fn message_start() -> ServerSentEvent {
    ServerSentEvent::MessageStart {
        message: AMessage {
            id: "msg_1".into(),
            r#type: MessageType::Message,
            role: ARole::Assistant,
            content: Vec::new(),
            model: "claude-test".into(),
            stop_reason: None,
            stop_sequence: None,
            stop_details: None,
            usage: AUsage {
                input_tokens: 1_000,
                output_tokens: 0,
                cache_creation_input_tokens: Some(500),
                cache_read_input_tokens: Some(2_000),
                ..Default::default()
            },
            container: None,
            context_management: None,
        },
    }
}

fn text_frames() -> Vec<ServerSentEvent> {
    vec![
        ServerSentEvent::ContentBlockStart {
            index: 0,
            content_block: AContentBlock::TextBlock {
                text: String::new(),
                citations: Vec::new(),
            },
        },
        ServerSentEvent::ContentBlockDelta {
            index: 0,
            delta: AContentBlockDelta::TextDelta {
                text: "partial".into(),
            },
        },
    ]
}

/// A turn whose byte stream ended before the terminal frame: the shape
/// a dropped connection or a cancel leaves behind.
fn truncated_turn() -> Message {
    let mut frames = vec![message_start()];
    frames.extend(text_frames());
    Message::Assistant(replay_sse_events(&model(), frames))
}

/// The same turn, closed properly.
fn completed_response() -> AssistantMessage {
    let mut frames = vec![message_start()];
    frames.extend(text_frames());
    frames.push(ServerSentEvent::ContentBlockStop { index: 0 });
    frames.push(ServerSentEvent::MessageStop);
    replay_sse_events(&model(), frames)
}

fn completed_turn() -> Message {
    Message::Assistant(completed_response())
}

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

/// `log_with`'s log, with a compaction checkpoint carrying `usage`
/// appended after the responses: the shape a session has once a
/// summarizer has run.
///
/// `first_kept_entry_id` is the system prompt, so nothing is dropped.
/// The cut is not what this fixture is about, and `stats()` reads only
/// the entry's `usage`.
fn log_with_compaction(responses: Vec<Message>, usage: Usage) -> (TempDir, ConversationLog) {
    let (dir, mut log) = log_with(responses);
    let first_kept = log.system_prompt_id().cloned().expect("system prompt id");
    log.append_compaction(
        ThreadFilter::USER,
        "summary".into(),
        first_kept,
        1_000,
        None,
        Some(usage),
    )
    .expect("compaction");
    (dir, log)
}

fn row(rows: &[InfoRow], key: &str) -> String {
    rows.iter()
        .find_map(|r| match r {
            InfoRow::Kv { key: k, value } if k == key => Some(value.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no {key} row in the digest"))
}

fn number(rows: &[InfoRow], key: &str) -> u64 {
    row(rows, key).parse().expect("a token row is a number")
}

/// A turn that ended without its terminal frame still contributes its
/// tokens and its dollars to the session total.
///
/// The truncated turn is the only assistant message in this log, so a
/// cost of zero here means the whole session reads as free.
#[test]
fn a_truncated_turn_contributes_tokens_and_dollars() {
    let (_dir, log) = log_with(vec![truncated_turn()]);
    let stats = log.stats();

    assert_eq!(
        stats.usage.input, 1_000,
        "the fixture must carry the opening frame's counts, or this test measures nothing"
    );
    assert!(
        stats.usage.cost.total > 0.0,
        "a truncated turn spent {} tokens and must not report $0",
        stats.usage.total_tokens
    );
    // 1000 * 3.0/1e6 + 2000 * 0.3/1e6 + 500 * 3.75/1e6
    let expected = 0.003 + 0.000_6 + 0.001_875;
    assert!(
        (stats.usage.cost.total - expected).abs() < 1e-12,
        "priced at the model's rates: got {} expected {expected}",
        stats.usage.cost.total
    );
}

/// The overlay's four token rows add up to the total tokens row it
/// prints beneath them.
///
/// `Usage::accumulate` documents this invariant and argues that summing
/// preserves it, which holds only if every response arrives with its
/// own total already computed. An unsealed exit breaks the premise, and
/// the break is visible here: the reproduction recipe is to add the
/// four rows by hand and compare.
#[test]
fn the_overlay_token_rows_sum_to_the_total_it_prints() {
    let (_dir, log) = log_with(vec![completed_turn(), truncated_turn(), completed_turn()]);
    let stats = log.stats();
    let rows = digest(&stats, None);

    let parts = number(&rows, "input")
        + number(&rows, "output")
        + number(&rows, "cache read")
        + number(&rows, "cache write");
    let total = number(&rows, "total tokens");

    assert_ne!(
        total, 0,
        "the fixture must report tokens or this proves nothing"
    );
    assert_eq!(
        parts, total,
        "the overlay prints four token rows summing to {parts} above a total of {total}"
    );
}

/// The four token rows still sum to the total when a compaction spent
/// money.
///
/// A summarizer exchange is never a message entry, so its spend reaches
/// `stats()` through the compaction entry and not through the fold over
/// assistant messages. That is a second door into the same aggregate,
/// and a fold that takes the total and the dollars while leaving the
/// four counts behind breaks the invariant exactly as an unsealed exit
/// did. The fixture above cannot see it: it holds no compaction.
#[test]
fn the_overlay_token_rows_sum_when_a_compaction_spent() {
    let summarizer = completed_response().usage;
    let turn_tokens = summarizer.total_tokens;
    let (_dir, log) = log_with_compaction(vec![completed_turn()], summarizer);
    let stats = log.stats();
    let rows = digest(&stats, None);

    let total = number(&rows, "total tokens");
    // The compaction's spend has to be IN the total, or the rows sum
    // over the assistant turn alone and the compaction door is untested.
    // One turn plus a compaction carrying that same turn's usage.
    assert_eq!(
        total,
        2 * turn_tokens,
        "the compaction's {turn_tokens} tokens must reach the aggregate beside the turn's, \
         or this test measures nothing"
    );

    let parts = number(&rows, "input")
        + number(&rows, "output")
        + number(&rows, "cache read")
        + number(&rows, "cache write");
    assert_eq!(
        parts, total,
        "the overlay prints four token rows summing to {parts} above a total of {total}"
    );
}
