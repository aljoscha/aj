//! Yield until input or a background result arrives.

use aj_agent::tool::{ToolContext, ToolDefinition, ToolDetails, ToolOutcome};
use aj_models::types::UserContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct WaitTool;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitInput {}

impl ToolDefinition for WaitTool {
    type Input = WaitInput;

    fn execution_mode(&self) -> aj_agent::tool::ExecutionMode {
        aj_agent::tool::ExecutionMode::Control
    }

    fn name(&self) -> &'static str {
        "wait"
    }

    fn description(&self) -> &'static str {
        "Yield until new user input or a background task or agent result arrives. Call this tool alone. In a batch with other tool calls, all results are returned normally without waiting. Use only when there is no useful work to do until an update arrives. There is no timeout. Takes no arguments."
    }

    async fn execute(
        &self,
        ctx: &mut dyn ToolContext,
        _: Self::Input,
    ) -> Result<ToolOutcome, aj_agent::BoxError> {
        let (text, is_error) = match ctx.request_wait() {
            Ok(()) => ("Wait requested.".to_string(), false),
            Err(error) => (format!("Cannot wait: {error}"), true),
        };
        Ok(ToolOutcome {
            structured_content: None,
            content: vec![UserContent::text(text.clone())],
            details: ToolDetails::Text {
                summary: "Wait".into(),
                body: text,
            },
            is_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::DummyToolContext;

    #[test]
    fn input_is_an_empty_object_without_extra_arguments() {
        let schema = WaitTool.input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"], serde_json::json!({}));
        assert_eq!(schema["additionalProperties"], false);
        assert!(serde_json::from_value::<WaitInput>(serde_json::json!({})).is_ok());
        for input in [
            serde_json::json!({"timeout": 10}),
            serde_json::json!({"tasks": []}),
            serde_json::Value::Null,
        ] {
            assert!(serde_json::from_value::<WaitInput>(input).is_err());
        }
    }

    #[tokio::test]
    async fn unsupported_wait_is_a_recoverable_tool_result() {
        let mut ctx = DummyToolContext::default();
        let reason = ctx.request_wait().unwrap_err().to_string();
        let result = WaitTool.execute(&mut ctx, WaitInput {}).await.unwrap();
        assert!(result.is_error);
        let ToolDetails::Text { body, .. } = result.details else {
            panic!("wait must return text details")
        };
        assert_eq!(body, format!("Cannot wait: {reason}"));
        let [UserContent::Text(content)] = result.content.as_slice() else {
            panic!("wait must return one text content block")
        };
        assert_eq!(content.text, body);
    }
}
