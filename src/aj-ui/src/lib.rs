/// Ui abstraction for AJ. The agent and all tools must use this when displaying
/// information to the user or requesting input from the user.
pub trait AjUi {
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
}
