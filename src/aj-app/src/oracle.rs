//! Oracle composes an advisory child from the session's independent model settings.

use aj_agent::tool::{ErasedToolDefinition, SpawnAgentConfig};
use aj_conf::Config;
use aj_tools::{OracleTool, builtin_tools_for_model};

use crate::session_setup::{RunConfigSnapshot, builtin_tool_options, thinking_display_name};

/// Oracle defaults use the ordinary model resolver, never the main agent's choices.
pub fn defaults(config: &Config) -> Config {
    Config {
        model_api: config.oracle_model_api.clone(),
        model_name: config.oracle_model_name.clone(),
        model_url: config.oracle_model_url.clone(),
        thinking: config.oracle_thinking,
        speed: config.oracle_speed,
        verbosity: config.oracle_verbosity,
        ..config.clone()
    }
}

/// Replace the catalog placeholder without re-enabling a disabled Oracle tool.
pub(crate) fn configure_tool(
    tools: &mut [ErasedToolDefinition],
    config: &Config,
    run: &RunConfigSnapshot,
) {
    let Some(tool) = tools.iter_mut().find(|tool| tool.name == "oracle") else {
        return;
    };
    let model = &run.oracle;
    let mut child_tools = builtin_tools_for_model(
        &builtin_tool_options(config),
        &config.disabled_tools,
        model.model_info.family.as_deref(),
    );
    child_tools.retain(|tool| {
        !matches!(
            tool.name.as_str(),
            "agent" | "oracle" | "apply_patch" | "edit_file" | "write_file" | "todo_write"
        )
    });
    let mut stream_options = model.stream_options.clone();
    stream_options.session_id = run.session_id.clone();
    *tool = OracleTool::new(SpawnAgentConfig {
        provider: std::sync::Arc::clone(&model.provider),
        model_info: std::sync::Arc::clone(&model.model_info),
        stream_options,
        thinking: model.thinking.clone(),
        speed: model.speed,
        thinking_display: thinking_display_name(model.thinking_display).to_string(),
        tools: child_tools,
        system_prompt_suffix: aj_tools::tools::oracle::ORACLE_PROMPT.to_string(),
    })
    .into();
}

/// Check Oracle's provider credentials without repeating Main's warning.
/// Credentials remain lazy so login takes effect without rebuilding the session.
pub async fn warning(
    run: &RunConfigSnapshot,
    auth: &aj_models::auth::AuthStorage,
    check_credentials: bool,
) -> Option<String> {
    crate::model::credential_warning(
        auth,
        &run.oracle.model_info.provider,
        check_credentials && run.oracle.model_info.provider != run.main.model_info.provider,
    )
    .await
    .map(|warning| format!("Oracle: {warning}"))
}
