pub const SYSTEM_PROMPT: &str = include_str!("../SYSTEM_PROMPT.md");

pub mod cli;
pub mod cli_common;
pub mod cli_sub_agent;
pub mod event_bridge;
pub mod prompt_history;
