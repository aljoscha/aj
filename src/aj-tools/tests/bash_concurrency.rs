//! Concurrent shell processes through the real agent and Code Mode evaluator.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::Agent;
use aj_agent::events::AgentEvent;
use aj_agent::tool::ToolDetails;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::types::{AssistantContent, AssistantMessage, StopReason, StreamOptions, ToolCall};
use aj_tools::BashTool;
use serde_json::json;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

async fn concurrent_commands(code_mode: bool) {
    let directory = TempDir::new().unwrap();
    // Neither command can succeed until the other process has started. The
    // deadline bounds a regression, rather than measuring execution speed.
    let inputs: Vec<_> = [("first", "second"), ("second", "first")]
        .into_iter()
        .map(|(label, peer)| {
            json!({
                "command": format!(
                    "touch {label}; while [ ! -f {peer} ]; do sleep 0.01; done; \
                     printf '{label} stdout'; printf '{label} stderr' >&2"
                ),
                "description": format!("Wait for {peer}"),
                "timeout": 5,
            })
        })
        .collect();
    let mut calls = AssistantMessage::empty();
    calls.stop_reason = StopReason::ToolUse;
    calls.content = if code_mode {
        vec![AssistantContent::ToolCall(ToolCall {
            id: "exec".into(),
            name: "exec".into(),
            is_raw: true,
            arguments: json!(format!(
                "await Promise.all({}.map(async input => text(await tools.bash(input))));",
                serde_json::to_string(&inputs).unwrap()
            )),
        })]
    } else {
        inputs
            .into_iter()
            .enumerate()
            .map(|(i, arguments)| {
                AssistantContent::ToolCall(ToolCall {
                    id: format!("bash-{i}"),
                    name: "bash".into(),
                    is_raw: false,
                    arguments,
                })
            })
            .collect()
    };
    let mut done = AssistantMessage::empty();
    done.stop_reason = StopReason::Stop;
    let provider = ScriptedProvider::from_messages(vec![calls, done], 1024, Duration::ZERO)
        .on_exhausted(ExhaustedBehavior::Panic);
    let model = ModelInfo {
        id: "gpt-6-astra".into(),
        name: "bash concurrency test".into(),
        family: None,
        api: "openai-responses".into(),
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
    let mut agent = Agent::with_provider(
        directory.path().into(),
        vec![BashTool::new(false, Some(directory.path().into())).into()],
        vec![],
        Arc::new(provider),
        Arc::new(model),
        StreamOptions::default(),
        None,
    );
    agent.set_code_mode(code_mode);
    let results = Arc::new(Mutex::new(vec![]));
    let sink = Arc::clone(&results);
    let _subscription = agent.subscribe(Arc::new(move |event| {
        if let AgentEvent::ToolExecutionEnd { tool, result, .. } = event
            && tool == "bash"
        {
            sink.lock().unwrap().push(result.clone());
        }
        Box::pin(async { Ok(()) })
    }));
    agent
        .prompt("run both commands".into(), CancellationToken::new())
        .await
        .unwrap();
    agent.reset_code_mode(Default::default()).await.unwrap();

    let results = results.lock().unwrap();
    assert_eq!(results.len(), 2);
    for label in ["first", "second"] {
        assert!(
            directory.path().join(label).exists(),
            "{label} never started"
        );
        assert!(
            results.iter().any(|result| matches!(
                result,
                ToolDetails::Bash { stdout, stderr, exit_code: Some(0), .. }
                    if stdout == &format!("{label} stdout") && stderr == &format!("{label} stderr")
            )),
            "{label} must finish with its own captured output: {results:?}"
        );
    }
}

#[tokio::test]
async fn direct_bash_calls_overlap_with_isolated_output() {
    concurrent_commands(false).await;
}

#[tokio::test]
async fn promise_all_bash_calls_overlap_with_isolated_output() {
    concurrent_commands(true).await;
}
