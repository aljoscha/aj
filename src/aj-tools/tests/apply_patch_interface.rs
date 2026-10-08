//! Patch calls through the real agent, native evaluator, and tool hooks.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::Agent;
use aj_agent::hooks::BeforeToolCallOutcome;
use aj_agent::message::AgentMessageKind;
use aj_models::provider::Provider;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::streaming::AssistantMessageEventStream;
use aj_models::types::{
    AssistantContent, AssistantMessage, Context, Message, SimpleStreamOptions, StopReason,
    StreamOptions, ToolCall,
};
use aj_tools::ApplyPatchTool;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const PATCH: &str = "*** Begin Patch\n*** Add File: result.txt\n+original\n*** End Patch";

struct Recorder {
    inner: ScriptedProvider,
    interface: Box<dyn Provider>,
    contexts: Mutex<Vec<Context>>,
}

impl Provider for Recorder {
    fn supports_freeform_tools(&self) -> bool {
        self.interface.supports_freeform_tools()
    }

    fn stream(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &StreamOptions,
    ) -> AssistantMessageEventStream {
        self.inner.stream(model, context, options)
    }
    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.contexts.lock().unwrap().push(context.clone());
        self.inner.stream_simple(model, context, options)
    }
}

fn setup(api: &str, code_mode: bool, arguments: Value) -> (TempDir, Agent, Arc<Recorder>) {
    setup_with_interface(
        api,
        code_mode,
        arguments,
        aj_models::provider::provider_for(api).unwrap(),
    )
}

fn setup_with_interface(
    api: &str,
    code_mode: bool,
    arguments: Value,
    interface: Box<dyn Provider>,
) -> (TempDir, Agent, Arc<Recorder>) {
    let directory = TempDir::new().unwrap();
    let model = ModelInfo {
        id: "gpt-6-astra".into(),
        name: "patch interface test".into(),
        family: None,
        api: api.into(),
        provider: "test".into(),
        base_url: String::new(),
        reasoning: false,
        reasoning_options: vec![],
        supports_verbosity: false,
        default_verbosity: None,
        speed_modes: vec![],
        default_speed: None,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 100_000,
        max_tokens: 1000,
    };
    let mut call = AssistantMessage::empty();
    call.stop_reason = StopReason::ToolUse;
    call.content.push(AssistantContent::ToolCall(ToolCall {
        id: "patch-call".into(),
        name: if code_mode { "exec" } else { "apply_patch" }.into(),
        is_raw: arguments.is_string(),
        arguments,
    }));
    let mut done = AssistantMessage::empty();
    done.stop_reason = StopReason::Stop;
    let provider = Arc::new(Recorder {
        interface,
        inner: ScriptedProvider::from_messages(vec![call, done], 1024, Duration::ZERO)
            .on_exhausted(ExhaustedBehavior::Panic),
        contexts: Mutex::new(vec![]),
    });
    let mut agent = Agent::with_provider(
        directory.path().into(),
        vec![ApplyPatchTool.into()],
        vec![],
        Arc::<Recorder>::clone(&provider),
        Arc::new(model),
        StreamOptions::default(),
        None,
    );
    agent.set_code_mode(code_mode);
    (directory, agent, provider)
}

async fn run(agent: &mut Agent) {
    tokio::time::timeout(Duration::from_secs(20), async {
        agent
            .prompt("apply the patch".into(), CancellationToken::new())
            .await
            .unwrap();
        agent.reset_code_mode(Default::default()).await.unwrap();
    })
    .await
    .expect("patch call did not finish");
}

#[tokio::test]
async fn direct_raw_and_json_fallback_calls_keep_their_advertised_shapes() {
    for (api, raw) in [
        ("openai-responses", true),
        ("openai-codex-responses", true),
        ("anthropic-messages", false),
        ("openai-completions", false),
    ] {
        let args = if raw {
            json!(PATCH)
        } else {
            json!({"patchText": PATCH})
        };
        let (directory, mut agent, provider) = setup(api, false, args.clone());
        let seen = Arc::new(Mutex::new(vec![]));
        let capture = Arc::clone(&seen);
        agent.set_before_tool_call(Some(Arc::new(move |_, args| {
            capture.lock().unwrap().push(args.clone());
            Box::pin(async { BeforeToolCallOutcome::Proceed { args } })
        })));
        run(&mut agent).await;
        assert_eq!(
            std::fs::read(directory.path().join("result.txt")).unwrap(),
            b"original\n"
        );
        assert_eq!(*seen.lock().unwrap(), [args]);
        let contexts = provider.contexts.lock().unwrap();
        let definition = &contexts[0].tools[0];
        assert_eq!(definition.input_format.is_some(), raw);
        assert_eq!(definition.description.contains("FREEFORM"), raw);
        if !raw {
            assert!(definition.parameters["properties"]["patchText"].is_object());
        }
        let call = contexts[1]
            .messages
            .iter()
            .find_map(|message| match message {
                Message::Assistant(message) => message.content.iter().find_map(|part| match part {
                    AssistantContent::ToolCall(call) => Some(call),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap();
        assert_eq!(call.is_raw, raw);
    }
}

#[tokio::test]
async fn selected_provider_controls_raw_tools_and_code_mode_not_the_api_label() {
    for (label, interface, supported) in [
        ("openai-responses", "anthropic-messages", false),
        ("custom-api", "openai-responses", true),
    ] {
        for requested in [false, true] {
            let native = requested && supported;
            let arguments = if native {
                json!(format!("text(await tools.apply_patch({}));", json!(PATCH)))
            } else if supported {
                json!(PATCH)
            } else {
                json!({"patchText":PATCH})
            };
            let (directory, mut agent, provider) = setup_with_interface(
                label,
                native,
                arguments,
                aj_models::provider::provider_for(interface).unwrap(),
            );
            agent.set_code_mode(requested);
            run(&mut agent).await;
            assert_eq!(
                std::fs::read(directory.path().join("result.txt")).unwrap(),
                b"original\n"
            );
            let contexts = provider.contexts.lock().unwrap();
            let tools = &contexts[0].tools;
            assert_eq!(tools.iter().any(|tool| tool.name == "exec"), native);
            let tool = tools
                .iter()
                .find(|tool| tool.name == if native { "exec" } else { "apply_patch" })
                .unwrap();
            assert_eq!(tool.input_format.is_some(), supported);
        }
    }
}

#[tokio::test]
async fn code_mode_accepts_patch_strings_and_preserves_hook_rewrites_and_audit_kind() {
    let source = format!(
        "const result = await tools.apply_patch({}); text(result);",
        json!(PATCH)
    );
    let (directory, mut agent, provider) = setup("openai-responses", true, json!(source));
    let audit = Arc::new(Mutex::new(Vec::new()));
    let capture = Arc::clone(&audit);
    let _subscription = agent.subscribe(aj_agent::bus::listener_from_sync(move |event| {
        if let aj_agent::events::AgentEvent::MessageEnd { message, .. } = event {
            capture.lock().unwrap().push(message.clone());
        }
    }));
    agent.set_before_tool_call(Some(Arc::new(|context, args| {
        Box::pin(async move {
            let args = if context.tool_name == "apply_patch" {
                json!(
                    args.as_str()
                        .expect("patch hook receives raw text")
                        .replace("+original", "+rewritten")
                )
            } else {
                args
            };
            BeforeToolCallOutcome::Proceed { args }
        })
    })));
    run(&mut agent).await;
    assert_eq!(
        std::fs::read(directory.path().join("result.txt")).unwrap(),
        b"rewritten\n"
    );
    let contexts = provider.contexts.lock().unwrap();
    let exec = contexts[0]
        .tools
        .iter()
        .find(|tool| tool.name == "exec")
        .unwrap();
    assert!(
        exec.description.contains("apply_patch(input: string)"),
        "{}",
        exec.description
    );
    assert!(
        !contexts[0]
            .tools
            .iter()
            .any(|tool| tool.name == "apply_patch")
    );
    let has_raw_patch_call = audit.lock().unwrap().iter().any(|message| {
        let AgentMessageKind::ToolActivity(activity) = &message.kind else {
            return false;
        };
        let Message::Assistant(message) = &activity.message else {
            return false;
        };
        message.content.iter().any(|part| {
            matches!(part,
                AssistantContent::ToolCall(call) if call.name == "apply_patch" && call.is_raw
            )
        })
    });
    assert!(has_raw_patch_call);
}

#[tokio::test]
async fn policy_can_deny_raw_patch_calls_before_any_file_changes() {
    for code_mode in [false, true] {
        let arguments = if code_mode {
            json!(format!(
                "try {{ await tools.apply_patch({}); }} catch (e) {{ text(String(e)); }}",
                json!(PATCH)
            ))
        } else {
            json!(PATCH)
        };
        let (directory, mut agent, _) = setup("openai-responses", code_mode, arguments);
        let denied = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = Arc::clone(&denied);
        agent.set_before_tool_call(Some(Arc::new(move |context, args| {
            let observed = Arc::clone(&observed);
            Box::pin(async move {
                if context.tool_name == "apply_patch" {
                    assert_eq!(args, json!(PATCH));
                    observed.store(true, std::sync::atomic::Ordering::SeqCst);
                    return BeforeToolCallOutcome::ShortCircuit {
                        outcome: aj_agent::tool::ToolOutcome {
                            structured_content: None,
                            content: vec![aj_models::types::UserContent::text("policy denied")],
                            details: aj_agent::tool::ToolDetails::Text {
                                summary: "Denied".into(),
                                body: "policy denied".into(),
                            },
                            is_error: true,
                        },
                    };
                }
                BeforeToolCallOutcome::Proceed { args }
            })
        })));
        run(&mut agent).await;
        assert!(denied.load(std::sync::atomic::Ordering::SeqCst));
        assert!(!directory.path().join("result.txt").exists());
    }
}
