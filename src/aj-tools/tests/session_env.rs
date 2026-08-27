//! Session environment overlays through real Agent and BashTool paths.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::bus::{SubscriptionHandle, listener_from_sync};
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::tool::{ErasedToolDefinition, TaskStatus};
use aj_agent::{Agent, AgentSeed, TaskRegistry};
use aj_models::provider::Provider;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::scripted::{ExhaustedBehavior, ProviderScript, ScriptBuilder, ScriptedProvider};
use aj_models::streaming::DoneReason;
use aj_models::types::{StreamOptions, UserContent};
use aj_tools::{AgentTool, BashTool};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const SCRIPTED: &str = "scripted";

#[derive(Clone, Debug)]
struct ToolOutput {
    agent_id: AgentId,
    tool: String,
    text: String,
}

type ToolOutputs = Arc<Mutex<Vec<ToolOutput>>>;

fn model_info() -> ModelInfo {
    ModelInfo {
        id: SCRIPTED.to_string(),
        name: SCRIPTED.to_string(),
        family: None,
        api: SCRIPTED.to_string(),
        provider: SCRIPTED.to_string(),
        base_url: "scripted://internal".to_string(),
        reasoning: false,
        reasoning_options: Vec::new(),
        supports_verbosity: false,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
    }
}

fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ProviderScript {
    ScriptBuilder::new(SCRIPTED, SCRIPTED, SCRIPTED)
        .start()
        .tool_call_block(id, name, arguments)
        .done(DoneReason::ToolUse)
}

fn text(body: &str) -> ProviderScript {
    ScriptBuilder::new(SCRIPTED, SCRIPTED, SCRIPTED)
        .start()
        .text_block(body)
        .done(DoneReason::Stop)
}

fn bash_turn(command: &str, background: bool) -> Vec<ProviderScript> {
    vec![
        tool_call(
            "bash-call",
            "bash",
            serde_json::json!({
                "command": command,
                "description": "read session environment",
                "timeout": 30,
                "run_in_background": background,
            }),
        ),
        text("done"),
    ]
}

fn agent_with(
    working_directory: &Path,
    tools: Vec<ErasedToolDefinition>,
    scripts: Vec<ProviderScript>,
) -> Agent {
    let provider: Arc<dyn Provider> =
        Arc::new(ScriptedProvider::new(scripts).on_exhausted(ExhaustedBehavior::Panic));
    let mut agent = Agent::with_provider(
        working_directory.to_path_buf(),
        tools,
        Vec::new(),
        provider,
        Arc::new(model_info()),
        StreamOptions::default(),
        None,
    );
    agent.seed_session(AgentSeed {
        assembled_system_prompt: Some("test system prompt".to_string()),
        ..AgentSeed::default()
    });
    agent
}

fn text_of(content: &[UserContent]) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            UserContent::Text(text) => Some(text.text.as_str()),
            UserContent::Image(_) => None,
        })
        .collect()
}

fn capture_tool_outputs(agent: &Agent) -> (SubscriptionHandle, ToolOutputs) {
    let outputs = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&outputs);
    let handle = agent.subscribe(listener_from_sync(move |event| {
        if let AgentEvent::ToolExecutionEnd {
            agent_id,
            tool,
            content,
            ..
        } = event
        {
            sink.lock().unwrap().push(ToolOutput {
                agent_id: *agent_id,
                tool: tool.clone(),
                text: text_of(content),
            });
        }
    }));
    (handle, outputs)
}

fn only_output(outputs: &ToolOutputs, agent_id: AgentId, tool: &str) -> String {
    let outputs = outputs.lock().unwrap();
    let matching: Vec<_> = outputs
        .iter()
        .filter(|output| output.agent_id == agent_id && output.tool == tool)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected one {tool} output from {agent_id:?}, saw {outputs:?}"
    );
    matching[0].text.clone()
}

fn assert_host_env_absent(key: &str) {
    assert!(
        std::env::var_os(key).is_none(),
        "the host already defines {key}, so the fixture cannot distinguish an absent overlay"
    );
}

#[tokio::test]
async fn session_env_layers_between_process_inheritance_and_fixed_bash_overrides() {
    let host_path = std::env::var("PATH").expect("the test process has PATH");
    let host_home = std::env::var("HOME").expect("the test process has HOME");
    let session_home = "/session-env-test/home";
    assert_ne!(
        host_home, session_home,
        "the session HOME must differ or the inheritance assertion measures nothing"
    );
    let dir = TempDir::new().expect("tempdir");
    let tool = BashTool::new(false, Some(dir.path().to_path_buf())).into();
    let mut agent = agent_with(
        dir.path(),
        vec![tool],
        bash_turn(
            r#"printf '%s\n%s\n%s\n%s\n%s\n' "$PATH" "$HOME" "$TERM" "$NO_COLOR" "$AGENT""#,
            false,
        ),
    );
    agent.set_session_env(BTreeMap::from([
        ("HOME".to_string(), session_home.to_string()),
        ("TERM".to_string(), "session-term".to_string()),
        ("NO_COLOR".to_string(), "session-color".to_string()),
        ("AGENT".to_string(), "session-agent".to_string()),
    ]));
    let (_subscription, outputs) = capture_tool_outputs(&agent);

    agent
        .prompt("read it".to_string(), CancellationToken::new())
        .await
        .expect("turn");

    assert_eq!(
        only_output(&outputs, AgentId::Main, "bash"),
        format!("{host_path}\n/session-env-test/home\ndumb\n1\naj\n"),
        "the child must preserve inheritance, overlay the session map, then apply every fixed override"
    );
}

#[tokio::test]
async fn one_shared_bash_tool_reads_each_agents_own_session_env() {
    const KEY: &str = "AJ_SESSION_ENV_SHARED_TOOL_CONTEXT_TEST";
    assert_host_env_absent(KEY);
    let dir = TempDir::new().expect("tempdir");
    let shared: ErasedToolDefinition = BashTool::new(false, Some(dir.path().to_path_buf())).into();
    let first_tool = shared.clone();
    let second_tool = shared.clone();
    assert!(
        Arc::ptr_eq(&first_tool.func, &second_tool.func),
        "the test must share one erased tool Arc or it cannot distinguish the context seam"
    );
    let command = format!(r#"printf '%s\n' "${{{KEY}-<unset>}}""#);
    let mut first = agent_with(dir.path(), vec![first_tool], bash_turn(&command, false));
    let mut second = agent_with(dir.path(), vec![second_tool], bash_turn(&command, false));
    first.set_session_env(BTreeMap::from([(
        KEY.to_string(),
        "first-session".to_string(),
    )]));
    let (_first_subscription, first_outputs) = capture_tool_outputs(&first);
    let (_second_subscription, second_outputs) = capture_tool_outputs(&second);

    first
        .prompt("read it".to_string(), CancellationToken::new())
        .await
        .expect("first turn");
    second
        .prompt("read it".to_string(), CancellationToken::new())
        .await
        .expect("second turn");

    assert_eq!(
        only_output(&first_outputs, AgentId::Main, "bash"),
        "first-session\n"
    );
    assert_eq!(
        only_output(&second_outputs, AgentId::Main, "bash"),
        "<unset>\n",
        "the second agent's empty context must not inherit state from the shared tool"
    );
}

#[tokio::test]
async fn spawned_sub_agents_bash_child_inherits_the_parent_session_env() {
    const KEY: &str = "AJ_SESSION_ENV_SUB_AGENT_TEST";
    assert_host_env_absent(KEY);
    let dir = TempDir::new().expect("tempdir");
    let command = format!(r#"printf '%s\n' "${{{KEY}-<unset>}}""#);
    let scripts = vec![
        tool_call(
            "parent-agent",
            "agent",
            serde_json::json!({
                "task": "read the session environment",
                "description": "read environment",
                "run_in_background": false,
            }),
        ),
        tool_call(
            "child-bash",
            "bash",
            serde_json::json!({
                "command": command,
                "description": "read inherited session environment",
                "timeout": 30,
                "run_in_background": false,
            }),
        ),
        text("child done"),
        text("parent done"),
    ];
    let mut agent = agent_with(
        dir.path(),
        vec![
            AgentTool.into(),
            BashTool::new(false, Some(dir.path().to_path_buf())).into(),
        ],
        scripts,
    );
    agent.set_session_env(BTreeMap::from([(
        KEY.to_string(),
        "parent-session".to_string(),
    )]));
    let (_subscription, outputs) = capture_tool_outputs(&agent);

    agent
        .prompt("delegate".to_string(), CancellationToken::new())
        .await
        .expect("parent turn");

    assert_eq!(
        only_output(&outputs, AgentId::Sub(1), "bash"),
        "parent-session\n",
        "the real spawn path must copy the parent overlay onto the child's context"
    );
}

#[tokio::test]
async fn background_bash_child_observes_the_session_env_in_its_spill() {
    const KEY: &str = "AJ_SESSION_ENV_BACKGROUND_BASH_TEST";
    assert_host_env_absent(KEY);
    let dir = TempDir::new().expect("tempdir");
    let command = format!(r#"printf '%s\n' "${{{KEY}-<unset>}}""#);
    let mut agent = agent_with(
        dir.path(),
        vec![BashTool::new(false, Some(dir.path().to_path_buf())).into()],
        bash_turn(&command, true),
    );
    agent.set_session_env(BTreeMap::from([(
        KEY.to_string(),
        "background-session".to_string(),
    )]));
    let registry = TaskRegistry::default();
    agent.set_task_registry(registry.clone());

    agent
        .prompt("start it".to_string(), CancellationToken::new())
        .await
        .expect("turn");

    let status = tokio::time::timeout(Duration::from_secs(5), registry.wait_terminal(1))
        .await
        .expect("background command reached a terminal state")
        .expect("the real bash path registered task 1");
    assert_eq!(status, TaskStatus::Exited(Some(0)));
    let (_, output) = registry.read(1).expect("the task remains readable");
    let spill = output
        .spill_path
        .expect("background bash always persists its full output");
    assert_eq!(
        std::fs::read_to_string(spill).expect("read background spill"),
        "background-session\n",
        "the background child's canonical output must carry the session overlay"
    );
}
