//! OAuth "stealth mode" helpers.
//!
//! When authenticating with an Anthropic OAuth token (prefix `sk-ant-oat`),
//! the API expects the request to look like it came from the Claude Code
//! CLI. The client must:
//!
//! 1. Prepend a fixed Claude Code identity block to the system prompt.
//! 2. Rename custom tools to Claude Code canonical casing on the request
//!    path, and reverse-map the canonical names back to whatever casing
//!    the caller used on the response path so the caller never sees the
//!    rewriting.
//!
//! The identity prompt and tool-name list track what the Claude Code CLI
//! actually sends. They should be revisited when Claude Code ships a
//! new major version that adds, removes, or renames tools.
//!
//! Authentication-related identifiers and beta headers (`user-agent`,
//! `x-app`, `claude-code-20250219`, `oauth-2025-04-20`) live in
//! [`crate::client`]; this module covers the request-body and
//! response-body transformations only.

use crate::messages::{
    ContentBlock, ContentBlockParam, Message, Messages, ServerSentEvent, ToolUnion,
};

/// The Claude Code identity system prompt.
///
/// Prepended as the first system content block in OAuth mode so the
/// request mimics Claude Code's identity preamble.
pub(crate) const CLAUDE_CODE_IDENTITY_PROMPT: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

/// The default Claude Code CLI version used for the `user-agent` header
/// in OAuth mode. Picked as a recent value the Anthropic server accepts
/// as a recognized Claude Code client.
///
/// If this drifts too far behind the real CLI, the server may reject the
/// request as an unrecognized client (an auth/forbidden-class failure).
/// That only affects OAuth mode, since API-key requests don't send this
/// header. Bump it if OAuth turns start failing authorization for no
/// other apparent reason.
pub(crate) const CLAUDE_CODE_VERSION: &str = "2.1.75";

/// Canonical tool names from Claude Code 2.x.
///
/// On the request path, any caller-supplied tool whose name matches one
/// of these case-insensitively is rewritten to this exact casing. On
/// the response path, tool names from the model are looked up against
/// the caller's tool list (case-insensitively) and rewritten back to
/// the caller's casing.
///
/// Update this list when Claude Code ships a major version that
/// changes its tool surface.
const CLAUDE_CODE_TOOL_NAMES: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

/// Forward map: caller tool name → Claude Code canonical casing if
/// known, otherwise the name unchanged.
fn to_canonical_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    for canonical in CLAUDE_CODE_TOOL_NAMES {
        if canonical.to_ascii_lowercase() == lower {
            return (*canonical).to_string();
        }
    }
    name.to_string()
}

/// Reverse map: canonical name from a model response → the caller's
/// original casing if any caller tool matches case-insensitively;
/// otherwise the name unchanged. Pass-through is intentional so a
/// model returning a server-side or unknown tool name still works.
fn from_canonical_name(name: &str, caller_tool_names: &[String]) -> String {
    let lower = name.to_ascii_lowercase();
    for caller in caller_tool_names {
        if caller.to_ascii_lowercase() == lower {
            return caller.clone();
        }
    }
    name.to_string()
}

/// Collect the caller's custom tool names from a request body. Used to
/// build the reverse-mapping table applied on the response path.
///
/// Only [`ToolUnion::Custom`] entries are considered: every other
/// `ToolUnion` variant has a fixed Anthropic-defined `name` that we
/// never rewrite.
pub(crate) fn collect_caller_tool_names(messages: &Messages) -> Vec<String> {
    messages
        .tools
        .iter()
        .filter_map(|tool| match tool {
            ToolUnion::Custom { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Apply request-side stealth transformations in place:
///
/// 1. Prepend the Claude Code identity system block. Existing system
///    blocks are preserved after it.
/// 2. Rewrite custom tool names in `messages.tools` to canonical
///    casing.
/// 3. Rewrite tool names in any historical `tool_use` blocks in
///    `messages.messages` to canonical casing — the caller stored the
///    response-side casing they last saw, which round-trips back here.
pub(crate) fn apply_request_transformations(messages: &mut Messages) {
    // 1. Identity prompt prepended to the system blocks.
    let mut system_blocks = messages.system.take().unwrap_or_default();
    system_blocks.insert(
        0,
        ContentBlockParam::TextBlock {
            text: CLAUDE_CODE_IDENTITY_PROMPT.to_string(),
            cache_control: None,
            citations: None,
        },
    );
    messages.system = Some(system_blocks);

    // 2. Custom tool names → canonical casing.
    for tool in messages.tools.iter_mut() {
        if let ToolUnion::Custom { name, .. } = tool {
            *name = to_canonical_name(name);
        }
    }

    // 3. Historical tool_use blocks in the conversation also get
    //    forwarded so the model sees consistent canonical names.
    for message in messages.messages.iter_mut() {
        for block in message.content.iter_mut() {
            if let ContentBlockParam::ToolUseBlock { name, .. } = block {
                *name = to_canonical_name(name);
            }
        }
    }
}

/// Reverse-map any tool names in a non-streaming response message so
/// the caller sees their original casing.
pub(crate) fn reverse_map_message(message: &mut Message, caller_tool_names: &[String]) {
    for block in message.content.iter_mut() {
        if let ContentBlock::ToolUseBlock { name, .. } = block {
            *name = from_canonical_name(name, caller_tool_names);
        }
    }
}

/// Reverse-map any tool names in a streaming event so the caller sees
/// their original casing. Only `content_block_start` events for
/// `tool_use` blocks carry a name; deltas don't.
pub(crate) fn reverse_map_event(event: &mut ServerSentEvent, caller_tool_names: &[String]) {
    if let ServerSentEvent::ContentBlockStart {
        content_block: ContentBlock::ToolUseBlock { name, .. },
        ..
    } = event
    {
        *name = from_canonical_name(name, caller_tool_names);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{MessageParam, Role};
    use serde_json::json;

    fn custom_tool(name: &str) -> ToolUnion {
        ToolUnion::Custom {
            name: name.to_string(),
            description: None,
            input_schema: json!({}),
            cache_control: None,
            allowed_callers: Vec::new(),
            defer_loading: None,
            eager_input_streaming: None,
            input_examples: Vec::new(),
            strict: None,
        }
    }

    #[test]
    fn forward_map_canonicalizes_known_names() {
        assert_eq!(to_canonical_name("edit"), "Edit");
        assert_eq!(to_canonical_name("READ"), "Read");
        assert_eq!(to_canonical_name("ToDoWrite"), "TodoWrite");
    }

    #[test]
    fn forward_map_passes_unknown_names_through() {
        assert_eq!(to_canonical_name("my_custom_tool"), "my_custom_tool");
        assert_eq!(to_canonical_name(""), "");
    }

    #[test]
    fn reverse_map_uses_caller_casing_when_known() {
        let callers = vec!["edit".to_string(), "read".to_string()];
        assert_eq!(from_canonical_name("Edit", &callers), "edit");
        assert_eq!(from_canonical_name("Read", &callers), "read");
    }

    #[test]
    fn reverse_map_passes_through_when_unknown() {
        let callers = vec!["edit".to_string()];
        // Server-side / MCP tool names won't be in the caller list:
        assert_eq!(from_canonical_name("web_search", &callers), "web_search");
    }

    #[test]
    fn collect_caller_tool_names_only_returns_custom() {
        let messages = Messages {
            tools: vec![
                custom_tool("edit"),
                custom_tool("Bash"),
                ToolUnion::WebSearch {
                    name: crate::messages::WebSearchToolName::WebSearch,
                    cache_control: None,
                    allowed_callers: Vec::new(),
                    allowed_domains: Vec::new(),
                    blocked_domains: Vec::new(),
                    defer_loading: None,
                    max_uses: None,
                    strict: None,
                    user_location: None,
                },
            ],
            ..Default::default()
        };

        let names = collect_caller_tool_names(&messages);
        assert_eq!(names, vec!["edit".to_string(), "Bash".to_string()]);
    }

    #[test]
    fn apply_request_transformations_prepends_identity_and_canonicalizes() {
        let mut messages = Messages {
            system: Some(vec![ContentBlockParam::TextBlock {
                text: "Caller system prompt".to_string(),
                cache_control: None,
                citations: None,
            }]),
            tools: vec![custom_tool("edit"), custom_tool("MyToolX")],
            messages: vec![MessageParam {
                role: Role::Assistant,
                content: vec![ContentBlockParam::ToolUseBlock {
                    id: "toolu_1".to_string(),
                    input: json!({"path": "x"}),
                    name: "edit".to_string(),
                    cache_control: None,
                    caller: None,
                }],
            }],
            ..Default::default()
        };

        apply_request_transformations(&mut messages);

        // Identity prompt is the first system block.
        let system = messages.system.as_ref().unwrap();
        assert_eq!(system.len(), 2);
        match &system[0] {
            ContentBlockParam::TextBlock { text, .. } => {
                assert_eq!(text, CLAUDE_CODE_IDENTITY_PROMPT);
            }
            _ => panic!("expected text block"),
        }
        match &system[1] {
            ContentBlockParam::TextBlock { text, .. } => {
                assert_eq!(text, "Caller system prompt");
            }
            _ => panic!("expected text block"),
        }

        // Custom tool names canonicalized; unknown names passed through.
        match &messages.tools[0] {
            ToolUnion::Custom { name, .. } => assert_eq!(name, "Edit"),
            _ => panic!("expected custom tool"),
        }
        match &messages.tools[1] {
            ToolUnion::Custom { name, .. } => assert_eq!(name, "MyToolX"),
            _ => panic!("expected custom tool"),
        }

        // Historical tool_use names canonicalized.
        match &messages.messages[0].content[0] {
            ContentBlockParam::ToolUseBlock { name, .. } => assert_eq!(name, "Edit"),
            _ => panic!("expected tool_use block"),
        }
    }

    #[test]
    fn apply_request_transformations_handles_no_system() {
        let mut messages = Messages::default();
        apply_request_transformations(&mut messages);
        let system = messages.system.as_ref().unwrap();
        assert_eq!(system.len(), 1);
        match &system[0] {
            ContentBlockParam::TextBlock { text, .. } => {
                assert_eq!(text, CLAUDE_CODE_IDENTITY_PROMPT);
            }
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn reverse_map_message_rewrites_tool_use_names() {
        use crate::messages::{MessageType, Usage};

        let mut message = Message {
            id: "msg_1".to_string(),
            r#type: MessageType::Message,
            role: Role::Assistant,
            content: vec![
                ContentBlock::TextBlock {
                    text: "ok".to_string(),
                    citations: Vec::new(),
                },
                ContentBlock::ToolUseBlock {
                    id: "toolu_1".to_string(),
                    input: json!({}),
                    name: "Edit".to_string(),
                    caller: None,
                },
            ],
            model: "claude-sonnet".to_string(),
            stop_reason: None,
            stop_sequence: None,
            stop_details: None,
            usage: Usage::default(),
            container: None,
            context_management: None,
        };

        let callers = vec!["edit".to_string()];
        reverse_map_message(&mut message, &callers);

        match &message.content[1] {
            ContentBlock::ToolUseBlock { name, .. } => assert_eq!(name, "edit"),
            _ => panic!("expected tool_use block"),
        }
    }

    #[test]
    fn reverse_map_event_rewrites_content_block_start() {
        let mut event = ServerSentEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::ToolUseBlock {
                id: "toolu_1".to_string(),
                input: json!({}),
                name: "Edit".to_string(),
                caller: None,
            },
        };

        let callers = vec!["edit".to_string()];
        reverse_map_event(&mut event, &callers);

        match event {
            ServerSentEvent::ContentBlockStart {
                content_block: ContentBlock::ToolUseBlock { name, .. },
                ..
            } => assert_eq!(name, "edit"),
            _ => panic!("expected content_block_start with tool_use"),
        }
    }
}
