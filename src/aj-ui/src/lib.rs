/// Ui abstraction for AJ. The agent and all tools must use this when displaying
/// information to the user or requesting input from the user.
pub trait AjUi: Send + Sync {
    fn display_notice(&self, notice: &str);
    fn display_error(&self, error: &str);

    fn get_user_input(&self) -> Option<String>;

    fn agent_text_start(&self, text: &str);
    fn agent_text_update(&self, diff: &str);
    fn agent_text_stop(&self, text: &str);

    fn agent_thinking_start(&self, thinking: &str);
    fn agent_thinking_update(&self, diff: &str);
    fn agent_thinking_stop(&self);

    fn display_tool_result(&self, tool_name: &str, input: &str, output: &str);
    fn display_tool_result_diff(&self, tool_name: &str, input: &str, before: &str, after: &str);
    fn display_tool_error(&self, tool_name: &str, input: &str, error: &str);

    fn ask_permission(&self, message: &str) -> bool;

    fn display_token_usage(&self, usage: &TokenUsage);

    fn display_token_usage_summary(&self, summary: &UsageSummary);

    fn get_subagent_ui(&self, agent_number: usize) -> impl AjUi;
}

/// Token usage information for display
#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub accumulated_input: u64,
    pub turn_input: u64,
    pub accumulated_output: u64,
    pub turn_output: u64,
    pub accumulated_cache_creation: u64,
    pub turn_cache_creation: u64,
    pub accumulated_cache_read: u64,
    pub turn_cache_read: u64,
}

/// Usage summary for display including main agent and sub-agents
#[derive(Debug, Clone)]
pub struct UsageSummary {
    pub main_agent_usage: SubAgentUsage,
    pub sub_agent_usage: Vec<SubAgentUsage>,
    pub total_usage: SubAgentUsage,
}

/// Usage information for a single agent (main or sub-agent)
#[derive(Debug, Clone)]
pub struct SubAgentUsage {
    pub agent_id: Option<usize>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
}
