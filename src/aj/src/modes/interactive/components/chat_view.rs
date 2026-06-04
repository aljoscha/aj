//! Slot-1 chat view component.
//!
//! Owns the main agent's transcript [`Container`] and switches the
//! visible transcript between the main agent and any sub-agent. There
//! are two view modes:
//!
//! - **Main view** (the default): the whole main transcript renders,
//!   including any [`SubAgentBox`]es inserted at their spawn points.
//!   Each sub-box is in [`SubAgentBoxMode::Compact`], so it reads as an
//!   inline windowed block within the main scrollback.
//! - **Sub-agent view**: when the user switches to observe `Sub(n)`,
//!   only that sub-agent's box renders, in [`SubAgentBoxMode::Full`];
//!   everything else (the main transcript and other boxes) is hidden.
//!
//! Sub-agent boxes live *inside* `main` as ordinary children; this
//! component just remembers their child indices so it can address them
//! by agent index.

use std::any::Any;
use std::collections::BTreeMap;

use aj_agent::events::AgentId;
use aj_tui::component::Component;
use aj_tui::components::spacer::Spacer;
use aj_tui::container::Container;
use aj_tui::keys::InputEvent;

use crate::config::theme::ChatTheme;
use crate::modes::interactive::components::assistant_message::AssistantMessageComponent;
use crate::modes::interactive::components::subagent_box::{
    SubAgentBox, SubAgentBoxMode, SubAgentStatus,
};
use crate::modes::interactive::components::tool_execution::ToolExecutionComponent;

/// A description of a known agent, suitable for an agent picker row.
pub struct AgentEntry {
    pub id: AgentId,
    /// `None` for the main agent; the sub-agent's task otherwise.
    pub task: Option<String>,
    /// `None` for the main agent; the sub-agent's status otherwise.
    pub status: Option<SubAgentStatus>,
}

/// Slot-1 chat component. Owns the main transcript and switches the
/// visible transcript between the main agent and any sub-agent.
pub struct ChatView {
    main: Container,
    /// `Sub(n)` -> child index of its box inside `main`.
    sub_boxes: BTreeMap<usize, usize>,
    /// The currently observed agent; `Main` by default.
    active: AgentId,
    theme: ChatTheme,
}

impl ChatView {
    pub fn new(theme: ChatTheme) -> Self {
        Self {
            main: Container::new(),
            sub_boxes: BTreeMap::new(),
            active: AgentId::Main,
            theme,
        }
    }

    /// Mutable access to the main transcript for main-agent routing and
    /// appends.
    pub fn container_mut(&mut self) -> &mut Container {
        &mut self.main
    }

    /// Ensure a box for `Sub(n)` exists in `main`, creating it (and a
    /// leading spacer that matches the inter-element rhythm) on first
    /// sight.
    pub fn ensure_sub_box(&mut self, n: usize, task: &str) {
        if self.sub_boxes.contains_key(&n) {
            return;
        }
        if !self.main.is_empty() {
            self.main.add_child(Box::new(Spacer::new(1)));
        }
        let idx = self.main.len();
        self.main
            .add_child(Box::new(SubAgentBox::new(n, task, &self.theme)));
        self.sub_boxes.insert(n, idx);
    }

    /// Mutable access to the box for `Sub(n)`, if one exists.
    pub fn sub_box_mut(&mut self, n: usize) -> Option<&mut SubAgentBox> {
        self.sub_boxes
            .get(&n)
            .copied()
            .and_then(move |idx| self.main.get_mut_as::<SubAgentBox>(idx))
    }

    /// Mutable access to an agent's transcript container: `main` for
    /// the main agent, the box's inner container for a sub-agent.
    pub fn agent_container_mut(&mut self, id: AgentId) -> Option<&mut Container> {
        match id {
            AgentId::Main => Some(&mut self.main),
            AgentId::Sub(n) => self.sub_box_mut(n).map(|b| b.inner_mut()),
        }
    }

    pub fn active(&self) -> AgentId {
        self.active
    }

    /// Switch the observed agent. The matching sub-box becomes `Full`;
    /// every other box becomes `Compact`.
    pub fn set_active(&mut self, id: AgentId) {
        self.active = id;
        let entries: Vec<(usize, usize)> = self
            .sub_boxes
            .iter()
            .map(|(&n, &idx)| (n, idx))
            .collect();
        for (n, idx) in entries {
            let mode = if matches!(id, AgentId::Sub(active) if active == n) {
                SubAgentBoxMode::Full
            } else {
                SubAgentBoxMode::Compact
            };
            if let Some(b) = self.main.get_mut_as::<SubAgentBox>(idx) {
                b.set_mode(mode);
            }
        }
    }

    /// All known agents: the main agent first, then sub-agents in
    /// ascending index order.
    pub fn agents(&self) -> Vec<AgentEntry> {
        let mut out = vec![AgentEntry {
            id: AgentId::Main,
            task: None,
            status: None,
        }];
        for (&n, &idx) in &self.sub_boxes {
            if let Some(b) = self.main.get_as::<SubAgentBox>(idx) {
                out.push(AgentEntry {
                    id: AgentId::Sub(n),
                    task: Some(b.task().to_string()),
                    status: Some(b.status()),
                });
            }
        }
        out
    }

    /// Expand or collapse every tool execution, in the main transcript
    /// and inside each sub-agent box.
    pub fn set_tools_expanded(&mut self, expanded: bool) {
        for i in 0..self.main.len() {
            if let Some(t) = self.main.get_mut_as::<ToolExecutionComponent>(i) {
                t.set_expanded(expanded);
            } else if let Some(b) = self.main.get_mut_as::<SubAgentBox>(i) {
                let inner = b.inner_mut();
                for j in 0..inner.len() {
                    if let Some(t) = inner.get_mut_as::<ToolExecutionComponent>(j) {
                        t.set_expanded(expanded);
                    }
                }
            }
        }
    }

    /// Show or hide the thinking block on every assistant message, in
    /// the main transcript and inside each sub-agent box.
    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        for i in 0..self.main.len() {
            if let Some(a) = self.main.get_mut_as::<AssistantMessageComponent>(i) {
                a.set_hide_thinking_block(hide);
            } else if let Some(b) = self.main.get_mut_as::<SubAgentBox>(i) {
                let inner = b.inner_mut();
                for j in 0..inner.len() {
                    if let Some(a) = inner.get_mut_as::<AssistantMessageComponent>(j) {
                        a.set_hide_thinking_block(hide);
                    }
                }
            }
        }
    }
}

impl Component for ChatView {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        match self.active {
            AgentId::Main => self.main.render(width),
            AgentId::Sub(n) => {
                if let Some(&idx) = self.sub_boxes.get(&n) {
                    if let Some(b) = self.main.get_mut_as::<SubAgentBox>(idx) {
                        return b.render(width);
                    }
                }
                self.main.render(width)
            }
        }
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    fn invalidate(&mut self) {
        self.main.invalidate();
    }
}

impl AsRef<dyn Any> for ChatView {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_tui::components::text::Text;

    use crate::config::theme::{Theme, ThemeHandle, chat_theme};

    fn theme() -> ChatTheme {
        chat_theme(&ThemeHandle::new(Theme::bundled_dark()))
    }

    fn strip_ansi(s: &str) -> String {
        let mut out: Vec<u8> = Vec::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'm' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("strip_ansi: surviving bytes remain valid UTF-8")
    }

    fn joined(lines: &[String]) -> String {
        lines.iter().map(|l| strip_ansi(l)).collect()
    }

    #[test]
    fn main_routing_renders_main_content() {
        let mut view = ChatView::new(theme());
        view.container_mut()
            .add_child(Box::new(Text::new("main line", 0, 0)));
        assert_eq!(view.active(), AgentId::Main);
        let lines = view.render(60);
        assert!(joined(&lines).contains("main line"), "{:?}", joined(&lines));
    }

    #[test]
    fn agent_container_addresses_sub_box() {
        let mut view = ChatView::new(theme());
        view.ensure_sub_box(1, "subtask");
        assert!(view.agent_container_mut(AgentId::Sub(1)).is_some());
        view.agent_container_mut(AgentId::Sub(1))
            .expect("sub box exists")
            .add_child(Box::new(Text::new("sub line", 0, 0)));
        assert!(view.agent_container_mut(AgentId::Sub(2)).is_none());
    }

    #[test]
    fn main_view_shows_compact_box_and_main_content() {
        let mut view = ChatView::new(theme());
        view.container_mut()
            .add_child(Box::new(Text::new("main line", 0, 0)));
        view.ensure_sub_box(1, "subtask");
        let lines = view.render(60);
        let text = joined(&lines);
        assert!(text.contains("main line"), "{text:?}");
        assert!(text.contains("agent 1"), "{text:?}");
        assert!(text.contains("subtask"), "{text:?}");
    }

    #[test]
    fn switching_to_sub_hides_main_and_shows_full_box() {
        let mut view = ChatView::new(theme());
        view.container_mut()
            .add_child(Box::new(Text::new("main line", 0, 0)));
        view.ensure_sub_box(1, "subtask");
        view.agent_container_mut(AgentId::Sub(1))
            .expect("sub box exists")
            .add_child(Box::new(Text::new("sub line", 0, 0)));

        view.set_active(AgentId::Sub(1));
        let text = joined(&view.render(60));
        assert!(text.contains("sub line"), "{text:?}");
        assert!(text.contains("observing"), "{text:?}");
        assert!(!text.contains("main line"), "{text:?}");

        view.set_active(AgentId::Main);
        assert!(joined(&view.render(60)).contains("main line"));
    }

    #[test]
    fn agents_lists_main_then_sub_agents() {
        let mut view = ChatView::new(theme());
        view.ensure_sub_box(1, "subtask");
        let agents = view.agents();
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].id, AgentId::Main);
        assert!(agents[0].task.is_none());
        assert_eq!(agents[1].id, AgentId::Sub(1));
        assert_eq!(agents[1].task.as_deref(), Some("subtask"));
        assert_eq!(agents[1].status, Some(SubAgentStatus::Running));
    }
}
