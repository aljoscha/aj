//! Per-event-shape rendering components for the interactive mode.
//!
//! Each component owns the on-screen representation of one
//! [`AgentEvent`]-shaped piece of state — an assistant message
//! with its streaming text/thinking, a tool execution with its
//! `ToolDetails::*` payload, the footer status line, the selector
//! overlays, and so on. The event pump
//! ([`crate::modes::interactive::event_pump`]) decides which
//! component to forward each incoming event to; components only
//! know about their own state.
//!
//! [`AgentEvent`]: aj_agent::events::AgentEvent

pub mod agent_picker;
pub mod assistant_message;
pub mod auth_picker;
pub mod auth_status;
pub mod bash_execution;
pub mod chat_view;
pub mod command_palette;
pub mod compaction_summary;
pub mod diff;
pub mod footer;
pub mod header;
pub mod help_overlay;
pub mod loader_status;
pub mod login_dialog;
pub mod model_selector;
pub mod outcome;
pub mod pending_message;
pub mod prompt_history;
pub mod read_only_list;
pub mod session_info;
pub mod session_selector;
pub mod settings_window;
pub mod skills_window;
pub mod streaming_scan;
pub mod subagent_box;
pub mod task_output;
pub mod thinking_selector;
pub mod tool_execution;
pub mod usage_status;
pub mod user_message;
