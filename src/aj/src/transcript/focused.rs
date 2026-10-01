//! A presentation-only projection. Original entry indices stay intact so
//! selection, message navigation and branch anchors keep their identities.
//! Hidden activity has zero height. Assistant blocks are projected separately
//! because one message can contain both thinking and user-facing prose.

use std::ops::Range;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct GroupId(EntryId, usize);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Part {
    Header {
        id: GroupId,
        label: String,
        failed: bool,
        selected: bool,
    },
    Body,
    Assistant(Range<usize>),
}

#[derive(Default)]
pub(super) struct FocusedTranscript {
    parts: HashMap<EntryId, Vec<Part>>,
    overrides: HashMap<(AgentId, GroupId), bool>,
    groups: Vec<(GroupId, usize, bool)>,
    headers: HashMap<EntryId, Vec<(Range<usize>, GroupId)>>,
    pub selected: Option<GroupId>,
    expand_all: bool,
}

struct Group {
    id: GroupId,
    index: usize,
    open: bool,
    counts: Vec<(String, usize)>,
    running: Vec<String>,
    failures: Vec<String>,
    task_failures: Vec<String>,
    stopped_tasks: Vec<String>,
}

impl Group {
    fn count(&mut self, name: &str) {
        if let Some((_, count)) = self.counts.iter_mut().find(|(n, _)| n == name) {
            *count += 1;
        } else {
            self.counts.push((name.to_owned(), 1));
        }
    }

    fn label(&self) -> String {
        let mut fields = self
            .counts
            .iter()
            .map(|(name, count)| format!("{name} ×{count}"))
            .collect::<Vec<_>>();
        if fields.is_empty() {
            fields.push("Activity".into());
        }
        if !self.running.is_empty() {
            fields.push(format!("running: {}", self.running.join(", ")));
        }
        if let Some(first) = self.failures.first() {
            fields.push(format!("{} failed: {first}", self.failures.len()));
        }
        for (outcome, tasks) in [
            ("failed", &self.task_failures),
            ("stopped", &self.stopped_tasks),
        ] {
            if let Some(first) = tasks.first() {
                let plural = if tasks.len() == 1 { "" } else { "s" };
                fields.push(format!("{} task{plural} {outcome}: {first}", tasks.len()));
            }
        }
        format!(
            "{} {}",
            if self.open { "▾" } else { "▸" },
            fields.join(" · ")
        )
    }
}

/// Keep status labels to a short, terminal-safe line, without parsing or
/// generating a semantic summary of the tool's output.
fn short(text: &str) -> String {
    let clean = sanitize_terminal_output(text);
    let line = clean
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let mut chars = line.chars();
    let mut result: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

impl FocusedTranscript {
    pub fn rebuild(&mut self, chat: &ChatState, display: TranscriptDisplay) {
        self.parts.clear();
        self.groups.clear();
        if self.expand_all != display.tools_expanded {
            self.overrides.clear();
            self.expand_all = display.tools_expanded;
        }
        if display.transcript_mode != aj_conf::TranscriptMode::Focused {
            self.headers.clear();
            return;
        }
        let Some(transcript) = chat.transcript(chat.active_view()) else {
            return;
        };
        let mut group = None;
        for (index, entry) in transcript.entries().iter().enumerate() {
            self.parts.insert(entry.id, Vec::new());
            match &entry.kind {
                EntryKind::Assistant(a) => {
                    for (block, content) in a.message.content.iter().enumerate() {
                        match content {
                            AssistantContent::Thinking(_) => {
                                let g = self.activity(&mut group, chat, entry.id, index, block);
                                g.count("thinking");
                                if !a.finalized && block + 1 == a.message.content.len() {
                                    g.running.push("thinking".into());
                                }
                                if g.open {
                                    self.push(entry.id, Part::Assistant(block..block + 1));
                                }
                            }
                            AssistantContent::Text(t) if !t.text.is_empty() => {
                                self.finish(&mut group);
                                self.push(entry.id, Part::Assistant(block..block + 1));
                            }
                            _ => {}
                        }
                    }
                }
                EntryKind::Tool(tool) => {
                    let g = self.activity(&mut group, chat, entry.id, index, 0);
                    g.count(&tool.tool);
                    let task_id = crate::tool_cell::badge_task_id(tool);
                    let task = task_id.and_then(|id| chat.tasks().get(&id));
                    let running = task.map_or(tool.status == ToolStatus::Running, |t| {
                        t.status == TaskStatus::Running
                    });
                    if running {
                        let label = task
                            .map(|t| t.label.as_str())
                            .or_else(|| tool.args.get("command").and_then(Value::as_str))
                            .unwrap_or(&tool.tool);
                        g.running.push(short(label));
                    }
                    if crate::tool_cell::derive_status(tool, chat.tasks())
                        == crate::tool_cell::VisualStatus::Failed
                    {
                        let detail = match task.map(|t| t.status) {
                            Some(TaskStatus::Exited(Some(code))) => format!("exit {code}"),
                            Some(TaskStatus::Exited(None)) => "terminated by signal".into(),
                            Some(TaskStatus::CaptureFailed(_)) => "output capture failed".into(),
                            Some(TaskStatus::Killed) => "stopped".into(),
                            _ => match &tool.details {
                                Some(ToolDetails::Text { summary, .. }) => short(summary),
                                Some(ToolDetails::Bash {
                                    exit_code: Some(code),
                                    ..
                                }) => format!("exit {code}"),
                                _ => "failed".into(),
                            },
                        };
                        g.failures.push(format!("{}: {detail}", short(&tool.tool)));
                    }
                    if g.open {
                        self.push(entry.id, Part::Body);
                    }
                }
                EntryKind::SubAgent(sub) => {
                    let g = self.activity(&mut group, chat, entry.id, index, 0);
                    // Delegation calls are represented by boxes instead of
                    // tool entries in the transcript model.
                    g.count(if sub.tool_name.is_empty() {
                        "agent"
                    } else {
                        &sub.tool_name
                    });
                    if sub.status == SubAgentStatus::Running {
                        g.running.push(short(&sub.task));
                    } else if matches!(
                        sub.status,
                        SubAgentStatus::Failed | SubAgentStatus::Truncated
                    ) {
                        g.failures.push(short(&sub.task));
                    }
                    if g.open {
                        self.push(entry.id, Part::Body);
                    }
                }
                EntryKind::TaskNotification(notification) => {
                    let g = self.activity(&mut group, chat, entry.id, index, 0);
                    // A completion is a result, not another invocation of the
                    // launching tool. Keep its outcome distinct from tool failures.
                    g.count("task results");
                    match notification.outcome {
                        TaskOutcome::Succeeded => {}
                        TaskOutcome::Failed { code } => {
                            let label = short(&notification.label);
                            g.task_failures.push(match code {
                                Some(code) => format!("{label} (exit {code})"),
                                None => label,
                            });
                        }
                        TaskOutcome::Killed => g.stopped_tasks.push(short(&notification.label)),
                    }
                    if g.open {
                        self.push(entry.id, Part::Body);
                    }
                }
                EntryKind::TurnUsage(_) if group.is_some() => {
                    if group.as_ref().is_some_and(|g| g.open) {
                        self.push(entry.id, Part::Body);
                    }
                }
                _ => {
                    self.finish(&mut group);
                    self.push(entry.id, Part::Body);
                }
            }
        }
        self.finish(&mut group);
    }

    fn push(&mut self, id: EntryId, part: Part) {
        self.parts.entry(id).or_default().push(part);
    }

    fn activity<'a>(
        &mut self,
        group: &'a mut Option<Group>,
        chat: &ChatState,
        entry: EntryId,
        index: usize,
        block: usize,
    ) -> &'a mut Group {
        group.get_or_insert_with(|| {
            let id = GroupId(entry, block);
            let open = self
                .overrides
                .get(&(chat.active_view(), id))
                .copied()
                .unwrap_or(self.expand_all);
            self.push(
                entry,
                Part::Header {
                    id,
                    label: String::new(),
                    failed: false,
                    selected: false,
                },
            );
            Group {
                id,
                index,
                open,
                counts: Vec::new(),
                running: Vec::new(),
                failures: Vec::new(),
                task_failures: Vec::new(),
                stopped_tasks: Vec::new(),
            }
        })
    }

    fn finish(&mut self, group: &mut Option<Group>) {
        let Some(g) = group.take() else { return };
        let label = g.label();
        if let Some(parts) = self.parts.get_mut(&g.id.0) {
            for part in parts {
                if let Part::Header {
                    id,
                    label: text,
                    failed,
                    selected,
                } = part
                    && *id == g.id
                {
                    *text = label;
                    *failed = !g.failures.is_empty()
                        || !g.task_failures.is_empty()
                        || !g.stopped_tasks.is_empty();
                    *selected = self.selected == Some(g.id);
                    break;
                }
            }
        }
        self.groups.push((g.id, g.index, g.open));
    }

    pub fn toggle(&mut self, agent: AgentId, id: GroupId) {
        if let Some((_, _, open)) = self.groups.iter().find(|(key, _, _)| *key == id) {
            self.overrides.insert((agent, id), !open);
        }
    }

    pub fn navigate(&mut self, cursor: usize, forward: bool) -> Option<(usize, EntryId)> {
        let current = self
            .selected
            .and_then(|id| self.groups.iter().position(|(key, _, _)| *key == id));
        let next = if let Some(current) = current {
            if forward {
                current.checked_add(1)
            } else {
                current.checked_sub(1)
            }
        } else if forward {
            self.groups
                .iter()
                .position(|(_, index, _)| *index >= cursor)
        } else {
            self.groups
                .iter()
                .rposition(|(_, index, _)| *index <= cursor)
        }?;
        let (id, index, _) = *self.groups.get(next)?;
        self.selected = Some(id);
        Some((index, id.0))
    }

    pub fn selected_line(&self) -> Option<usize> {
        let selected = self.selected?;
        self.headers
            .get(&selected.0)?
            .iter()
            .find(|(_, id)| *id == selected)
            .map(|(range, _)| range.start)
    }

    pub fn hash(&self, entry: EntryId, hasher: &mut DefaultHasher) {
        self.parts.get(&entry).hash(hasher);
    }

    pub fn shows_body(&self, entry: EntryId) -> bool {
        self.parts
            .get(&entry)
            .is_none_or(|parts| parts.contains(&Part::Body))
    }

    pub fn header_at(&self, entry: EntryId, line: usize) -> Option<GroupId> {
        self.headers
            .get(&entry)?
            .iter()
            .find(|(range, _)| range.contains(&line))
            .map(|(_, id)| *id)
    }

    pub fn body_at(&self, entry: EntryId, line: usize) -> bool {
        self.shows_body(entry)
            && self
                .headers
                .get(&entry)
                .and_then(|headers| headers.last())
                .is_none_or(|(range, _)| line > range.end)
    }

    pub fn draw(
        &mut self,
        entry: &Entry,
        chat: &ChatState,
        display: TranscriptDisplay,
        styles: &TranscriptStyles,
        focus: Option<&[TextSpan]>,
        image: ImageRender,
        ctx: &DrawContext,
    ) -> Surface {
        let Some(parts) = self.parts.get(&entry.id) else {
            return build_entry_widget(entry, chat, display, styles, false, focus, image)
                .into_indented_boxed()
                .draw(ctx);
        };
        let mut result = Surface::empty();
        let mut headers = Vec::new();
        for part in parts {
            let mut widget: Box<dyn Widget> = match part {
                Part::Header {
                    label,
                    failed,
                    selected,
                    ..
                } => {
                    let style = if *selected {
                        styles.accent
                    } else if *failed {
                        styles.error
                    } else {
                        styles.dim
                    };
                    let text = if *selected {
                        format!("{label} · Enter to toggle\n\n")
                    } else {
                        format!("{label}\n\n")
                    };
                    Box::new(indent_entry(RichText::new(vec![TextSpan {
                        text,
                        style,
                        ..TextSpan::default()
                    }])))
                }
                Part::Body => build_entry_widget(entry, chat, display, styles, false, focus, image)
                    .into_indented_boxed(),
                Part::Assistant(range) => {
                    let EntryKind::Assistant(a) = &entry.kind else {
                        continue;
                    };
                    Box::new(indent_entry(build_assistant_blocks(
                        &a.message.content[range.clone()],
                        display.show_thinking_block,
                        display.syntax_highlight,
                        styles,
                    )))
                }
            };
            let surface = widget.draw(ctx);
            let start = result.size.height;
            result.size.width = result.size.width.max(surface.size.width);
            result.size.height = start.saturating_add(surface.size.height);
            if let Part::Header { id, .. } = part {
                headers.push((
                    usize::from(start)..usize::from(result.size.height.saturating_sub(1)),
                    *id,
                ));
            }
            result.children.push(SubSurface {
                origin: RelativePoint {
                    row: i32::from(start),
                    col: 0,
                },
                surface,
                z_index: 0,
            });
        }
        self.headers.insert(entry.id, headers);
        result
    }
}
