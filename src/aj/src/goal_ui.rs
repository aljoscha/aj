//! Goal controls over live session state, with drafts independent of chat input.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use aj_agent::goal::{Goal, GoalAction, GoalRequest, GoalStatus};
use aj_app::chat::ChatState;
use aj_app::host::{Command, CommandOutcome};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use vaxis::key::{Key, Modifiers};
use vaxis::vxfw::{
    DrawContext, Event, EventContext, MaxSize, RelativePoint, Size, SubSurface, Surface, Text,
    TextArea, Widget, WidgetRef, draw_widget, to_widget_ref,
};

use crate::control::Control;
use crate::footer::format_goal_runtime;
use crate::interactive::OverlayHandles;
use crate::overlay::{
    OverlayChrome, OverlayPlacement, OverlayStack, close_top, subtitle_edit_close,
};
use crate::settings_ui::{RowKind, SettingList, SettingRow, TextEditOverlay, push_window};
use crate::toasts::{ToastStack, show_toast};

/// Requests stay addressed to the host and session selected when this opens.
pub(crate) fn open_goal(
    handles: OverlayHandles,
    control: Control,
    session: String,
    chat: Rc<RefCell<ChatState>>,
    redraw: UnboundedSender<()>,
    entry: bool,
) -> futures::future::LocalBoxFuture<'static, ()> {
    let (requests, mut receiver) = unbounded_channel::<GoalRequest>();
    let toasts = Rc::clone(&handles.toasts);
    let ui = GoalUi::open(&handles, chat, requests, entry);
    let weak = Rc::downgrade(&ui);
    // The shell owns completion, not the overlay's draw lifecycle. Keep only a
    // weak UI handle so closing the window cannot suppress a refusal or leak it.
    Box::pin(async move {
        while let Some(request) = receiver.recv().await {
            let action = request.action.clone();
            let result = match control.command(&session, Command::Goal(request)).await {
                // Only the event stream writes the live goal projection.
                Ok(CommandOutcome::Goal(_)) => Ok(()),
                Ok(_) => Err("Unexpected response while saving the goal.".into()),
                Err(error) if error.unknown_endpoint() => {
                    Err("Goals are not supported by this version of AJ.".into())
                }
                Err(error) => Err(error.to_string()),
            };
            if let Err(error) = &result {
                show_toast(&toasts, error.clone());
            }
            if let Some(ui) = weak.upgrade() {
                let mut state = ui.borrow_mut();
                state.pending = false;
                state.notice = result.err();
                if state.notice.is_none() {
                    state.pages.retain(|page| {
                        if let Some(page) = page.upgrade() {
                            page.borrow().clear_filter();
                            true
                        } else {
                            false
                        }
                    });
                    match action {
                        GoalAction::Edit { .. } | GoalAction::Clear => state.edit_draft = None,
                        GoalAction::SetBudget { .. } => state.budget_draft = None,
                        GoalAction::Replace { .. } => {
                            state.replacement = None;
                            state.objective.clear();
                            state.budget.clear();
                        }
                        _ => {}
                    }
                }
            }
            let _ = redraw.send(());
        }
    })
}

struct GoalUi {
    // The stack owns the widgets. Holding it weakly prevents an ownership cycle.
    stack: Weak<RefCell<OverlayStack>>,
    editor: WidgetRef,
    chrome: OverlayChrome,
    toasts: ToastStack,
    chat: Rc<RefCell<ChatState>>,
    requests: UnboundedSender<GoalRequest>,
    entry: bool,
    /// The replacement target is captured before drafting, never inferred at save.
    replacement: Option<String>,
    budget_draft: Option<(String, String)>,
    objective: String,
    budget: String,
    /// A refused edit belongs to the goal it was written for, not a replacement.
    edit_draft: Option<(String, String)>,
    pending: bool,
    notice: Option<String>,
    pages: Vec<Weak<RefCell<SettingList>>>,
}

impl GoalUi {
    fn open(
        handles: &OverlayHandles,
        chat: Rc<RefCell<ChatState>>,
        requests: UnboundedSender<GoalRequest>,
        entry: bool,
    ) -> Rc<RefCell<Self>> {
        let ui = Rc::new(RefCell::new(Self {
            stack: Rc::downgrade(&handles.stack),
            editor: Rc::clone(&handles.editor),
            chrome: handles.chrome.clone(),
            toasts: Rc::clone(&handles.toasts),
            chat,
            requests,
            entry,
            replacement: None,
            budget_draft: None,
            objective: String::new(),
            budget: String::new(),
            edit_draft: None,
            pending: false,
            notice: None,
            pages: Vec::new(),
        }));
        push_goal_page(&ui, false);
        ui
    }

    fn close(&self, ctx: &mut EventContext) {
        if let Some(stack) = self.stack.upgrade() {
            close_top(&stack, ctx, &self.editor);
        }
    }

    fn error(&self, text: &str) {
        show_toast(&self.toasts, text.to_string());
    }

    fn request(&mut self, request: GoalRequest) {
        if self.pending {
            return;
        }
        self.notice = Some("Saving… You can close this window.".into());
        self.pending = self.requests.send(request).is_ok();
    }

    fn target(&mut self, action: GoalAction) {
        let id = self.chat.borrow().goal.as_ref().map(|goal| goal.id.clone());
        if let Some(id) = id {
            self.request(GoalRequest::for_goal(id, action));
        }
    }

    fn submit_new(&mut self) {
        match create_action(&self.objective, &self.budget) {
            Ok(GoalAction::Create {
                objective,
                token_budget,
            }) => {
                let request = if let Some(id) = &self.replacement {
                    GoalRequest::for_goal(
                        id.clone(),
                        GoalAction::Replace {
                            objective,
                            token_budget,
                        },
                    )
                } else {
                    GoalAction::Create {
                        objective,
                        token_budget,
                    }
                    .into()
                };
                self.request(request);
            }
            Err(error) => self.error(error),
            _ => unreachable!(),
        }
    }

    fn activate(ui: &Rc<RefCell<Self>>, ctx: &mut EventContext, id: &str, drafting: bool) {
        let mut state = ui.borrow_mut();
        if state.pending {
            return;
        }
        let has_goal = state.chat.borrow().goal.is_some();
        match id {
            "objective" | "draft" => {
                drop(state);
                Self::open_objective(ui, ctx, drafting, id == "draft");
            }
            "budget" | "budget_draft" => {
                drop(state);
                Self::open_budget(ui, ctx, drafting, id == "budget_draft");
            }
            "start" if !has_goal || state.replacement.is_some() => {
                if let Err(error) = create_action(&state.objective, &state.budget) {
                    state.error(error);
                } else {
                    let confirm = state.replacement.is_some()
                        && state
                            .chat
                            .borrow()
                            .goal
                            .as_ref()
                            .is_some_and(|goal| goal.status != GoalStatus::Complete);
                    if confirm {
                        drop(state);
                        open_replace_confirmation(ui, ctx);
                    } else {
                        state.submit_new();
                    }
                }
            }
            "new" => {
                let id = state
                    .chat
                    .borrow()
                    .goal
                    .as_ref()
                    .map(|goal| goal.id.clone());
                if state.replacement != id {
                    state.objective.clear();
                    state.budget.clear();
                }
                state.replacement = id;
                drop(state);
                ctx.request_focus(push_goal_page(ui, true));
                ctx.redraw = true;
            }
            "back" => state.close(ctx),
            "leave" => state.close(ctx),
            "manage" => {
                state.entry = false;
                state.close(ctx);
                drop(state);
                ctx.request_focus(push_goal_page(ui, false));
                ctx.redraw = true;
            }
            "pause" => state.target(GoalAction::Pause),
            "resume" => {
                state.target(GoalAction::Resume);
                if state.entry {
                    state.close(ctx);
                }
                state.entry = false;
            }
            "clear" => state.target(GoalAction::Clear),
            _ => {}
        }
    }

    fn open_objective(
        ui: &Rc<RefCell<Self>>,
        ctx: &mut EventContext,
        drafting: bool,
        stale_draft: bool,
    ) {
        let state = ui.borrow();
        let Some(stack) = state.stack.upgrade() else {
            return;
        };
        let chat = state.chat.borrow();
        let goal = chat.goal.as_ref().filter(|_| !drafting);
        let (goal_id, current) = if stale_draft {
            let Some((id, text)) = &state.edit_draft else {
                return;
            };
            (Some(id.clone()), text.as_str())
        } else {
            (
                goal.map(|goal| goal.id.clone()),
                goal.map_or(state.objective.as_str(), |goal| {
                    state.edit_draft_for(goal).unwrap_or(&goal.objective)
                }),
            )
        };
        let area = TextArea::new();
        area.borrow_mut().set_text(current);
        area.borrow_mut().set_border_color(state.chrome.border.fg);
        // All submission is handled by the wrapper, before TextArea can clear
        // its buffer. No conversation history or palette trigger is installed.
        area.borrow_mut().set_submit_enabled(false);
        let focus = to_widget_ref(Rc::clone(&area));
        let overlay = Rc::new(RefCell::new(ObjectiveEditor {
            area,
            ui: Rc::downgrade(ui),
            goal_id,
        }));
        push_window(
            &stack,
            &state.chrome,
            "Goal objective",
            format!(
                "Enter: {}  •  Ctrl+J for newline  •  Esc to cancel",
                match goal {
                    Some(goal)
                        if matches!(
                            goal.status,
                            GoalStatus::Complete | GoalStatus::BudgetLimited
                        ) && goal
                            .token_budget
                            .is_none_or(|budget| goal.tokens_used < budget) =>
                        "Save and continue",
                    Some(_) => "Save changes",
                    None => "Keep draft",
                }
            ),
            to_widget_ref(overlay),
            Rc::clone(&focus),
            OverlayPlacement::Large,
        );
        ctx.request_focus(focus);
        ctx.redraw = true;
    }

    fn open_budget(
        ui: &Rc<RefCell<Self>>,
        ctx: &mut EventContext,
        drafting: bool,
        stale_draft: bool,
    ) {
        let state = ui.borrow();
        let Some(stack) = state.stack.upgrade() else {
            return;
        };
        let goal = state.chat.borrow().goal.clone().filter(|_| !drafting);
        let mut goal_id = goal.as_ref().map(|goal| goal.id.clone());
        let mut current = goal.as_ref().map_or_else(
            || state.budget.clone(),
            |goal| {
                state
                    .budget_draft
                    .as_ref()
                    .filter(|(id, _)| *id == goal.id)
                    .map(|(_, text)| text.clone())
                    .unwrap_or_else(|| {
                        goal.token_budget
                            .map(|value| value.to_string())
                            .unwrap_or_default()
                    })
            },
        );
        if stale_draft && let Some((id, text)) = &state.budget_draft {
            goal_id = Some(id.clone());
            current = text.clone();
        }
        let overlay = Rc::new(RefCell::new(TextEditOverlay::new(&current)));
        let weak = Rc::downgrade(ui);
        overlay
            .borrow()
            .set_on_submit_raw(Box::new(move |ctx, text| {
                let Some(ui) = weak.upgrade() else { return };
                let mut state = ui.borrow_mut();
                match parse_budget(text) {
                    Ok(token_budget) => {
                        if let Some(id) = &goal_id {
                            state.budget_draft = Some((id.clone(), text.to_string()));
                            state.request(GoalRequest::for_goal(
                                id.clone(),
                                GoalAction::SetBudget { token_budget },
                            ));
                        } else {
                            state.budget = text.to_string();
                        }
                        state.close(ctx);
                    }
                    Err(error) => state.error(error),
                }
                ctx.redraw = true;
            }));
        let weak = Rc::downgrade(ui);
        overlay.borrow_mut().on_cancel = Some(Box::new(move |ctx| {
            if let Some(ui) = weak.upgrade() {
                ui.borrow().close(ctx);
            }
        }));
        let focus = overlay.borrow().focus_target();
        push_window(
            &stack,
            &state.chrome,
            "Token budget (blank for unlimited)",
            subtitle_edit_close("confirm"),
            to_widget_ref(overlay),
            Rc::clone(&focus),
            OverlayPlacement::Small,
        );
        ctx.request_focus(focus);
        ctx.redraw = true;
    }

    fn rows(&self, drafting: bool) -> Vec<SettingRow> {
        let mut rows = Vec::new();
        let chat = self.chat.borrow();
        if !drafting
            && let Some((id, text)) = &self.edit_draft
            && !chat.goal.as_ref().is_some_and(|goal| goal.id == *id)
        {
            rows.push(row(
                "draft",
                "Unsent objective",
                &one_line(text),
                "The target goal changed. Open to copy your retained draft.",
                true,
            ));
        }
        if !drafting
            && let Some((id, text)) = &self.budget_draft
            && !chat.goal.as_ref().is_some_and(|goal| goal.id == *id)
        {
            rows.push(row(
                "budget_draft",
                "Unsent budget",
                text,
                "The target goal changed. Open to copy your retained draft.",
                true,
            ));
        }
        if let Some(goal) = &chat.goal
            && self.entry
            && !drafting
        {
            rows.push(row(
                "leave",
                if matches!(goal.status, GoalStatus::Active | GoalStatus::Complete) {
                    "Close"
                } else {
                    "Leave stopped"
                },
                goal.status.label(),
                "Leave the goal unchanged. Manage it from the command palette.",
                true,
            ));
            if can_resume(goal) {
                rows.push(row(
                    "resume",
                    "Resume",
                    "",
                    &one_line(&goal.objective),
                    true,
                ));
            }
            rows.push(row(
                "manage",
                "Manage goal",
                "",
                "Edit the objective or budget without starting pursuit.",
                true,
            ));
            return rows;
        }
        if let Some(goal) = &chat.goal
            && !drafting
        {
            rows.push(row(
                "objective",
                "Objective",
                &one_line(&goal.objective),
                if self.edit_draft_for(goal).is_some() {
                    "Edit draft retained. Enter to review and retry."
                } else {
                    "Edit the objective, including during an active turn."
                },
                true,
            ));
            rows.push(row(
                "status",
                "Status",
                goal.status.label(),
                "Current goal status.",
                false,
            ));
            rows.push(row(
                "tokens",
                "Tokens used",
                &goal.tokens_used.to_string(),
                "Usage for this goal, not the entire session.",
                false,
            ));
            rows.push(row(
                "time",
                "Time used",
                &format_goal_runtime(goal.time_used_seconds),
                "Time spent on this goal.",
                false,
            ));
            rows.push(row(
                "budget",
                "Token budget",
                &budget_label(goal.token_budget),
                "Blank for unlimited. Saving does not resume a stopped goal.",
                true,
            ));
            if goal.status == GoalStatus::Active {
                rows.push(row(
                    "pause",
                    "Pause",
                    "",
                    "Pause pursuit without cancelling the current turn.",
                    true,
                ));
            } else if can_resume(goal) {
                rows.push(row(
                    "resume",
                    "Resume",
                    "",
                    "Continue working toward this goal.",
                    true,
                ));
            }
            rows.push(row(
                "new",
                "New goal",
                "",
                "Draft a replacement with its own budget and zero usage.",
                true,
            ));
            rows.push(row(
                "clear",
                "Clear",
                "",
                "Remove the goal without cancelling the current turn.",
                true,
            ));
        } else {
            rows.push(row(
                "objective",
                "Objective",
                &one_line(&self.objective),
                "Enter to edit a multiline objective.",
                true,
            ));
            rows.push(row(
                "budget",
                "Token budget",
                &if self.budget.trim().is_empty() {
                    "Unlimited".into()
                } else {
                    self.budget.clone()
                },
                "Optional positive integer. Set when creating the goal.",
                true,
            ));
            rows.push(row(
                "start",
                if self.replacement.is_some() {
                    "Replace goal"
                } else {
                    "Start goal"
                },
                "",
                "Create this goal and start pursuit.",
                true,
            ));
            if self.replacement.is_some() {
                rows.push(row(
                    "back",
                    "Back to current goal",
                    "",
                    "Return without replacing the goal.",
                    true,
                ));
            }
        }
        rows
    }

    fn edit_draft_for(&self, goal: &Goal) -> Option<&str> {
        self.edit_draft
            .as_ref()
            .filter(|(id, _)| *id == goal.id)
            .map(|(_, text)| text.as_str())
    }
}

/// A separate page gives each navigation step its own filter and focus target.
fn push_goal_page(ui: &Rc<RefCell<GoalUi>>, drafting: bool) -> WidgetRef {
    let mut state = ui.borrow_mut();
    let stack = state.stack.upgrade().expect("open goal stack");
    let list = Rc::new(RefCell::new(SettingList::new(
        Vec::new(),
        state.chrome.select.clone(),
        false,
    )));
    state.pages.push(Rc::downgrade(&list));
    let weak = Rc::downgrade(ui);
    list.borrow_mut().on_open = Some(Box::new(move |ctx, id, _| {
        if let Some(ui) = weak.upgrade() {
            let drafting = drafting && ui.borrow().replacement.is_some();
            GoalUi::activate(&ui, ctx, id, drafting);
        }
    }));
    let weak = Rc::downgrade(ui);
    list.borrow_mut().on_close = Some(Box::new(move |ctx| {
        if let Some(ui) = weak.upgrade() {
            ui.borrow().close(ctx);
        }
    }));
    let focus = list.borrow().focus_target();
    let widget = Rc::new(RefCell::new(GoalOverlay {
        ui: Rc::clone(ui),
        list,
        drafting,
    }));
    push_window(
        &stack,
        &state.chrome,
        if drafting {
            "New goal"
        } else if state.entry {
            "Continue saved goal?"
        } else {
            "Goal"
        },
        subtitle_edit_close("select"),
        to_widget_ref(widget),
        Rc::clone(&focus),
        OverlayPlacement::Large,
    );
    focus
}

fn open_replace_confirmation(ui: &Rc<RefCell<GoalUi>>, ctx: &mut EventContext) {
    let state = ui.borrow();
    let Some(stack) = state.stack.upgrade() else {
        return;
    };
    let list = Rc::new(RefCell::new(SettingList::new(
        vec![
            row(
                "cancel",
                "Keep editing",
                "",
                "Keep the current goal and replacement draft.",
                true,
            ),
            row(
                "replace",
                "Replace unfinished goal",
                "",
                "Start the replacement with zero usage. The current goal stays in branch history.",
                true,
            ),
        ],
        state.chrome.select.clone(),
        false,
    )));
    let weak = Rc::downgrade(ui);
    list.borrow_mut().on_open = Some(Box::new(move |ctx, id, _| {
        if let Some(ui) = weak.upgrade() {
            let mut state = ui.borrow_mut();
            state.close(ctx);
            if id == "replace" {
                state.submit_new();
            }
        }
    }));
    let weak = Rc::downgrade(ui);
    list.borrow_mut().on_close = Some(Box::new(move |ctx| {
        if let Some(ui) = weak.upgrade() {
            ui.borrow().close(ctx);
        }
    }));
    let focus = list.borrow().focus_target();
    push_window(
        &stack,
        &state.chrome,
        "Replace unfinished goal?",
        subtitle_edit_close("select"),
        to_widget_ref(list),
        Rc::clone(&focus),
        OverlayPlacement::Small,
    );
    ctx.request_focus(focus);
    ctx.redraw = true;
}

struct GoalOverlay {
    drafting: bool,
    ui: Rc<RefCell<GoalUi>>,
    list: Rc<RefCell<SettingList>>,
}

impl Widget for GoalOverlay {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let ui = self.ui.borrow();
        let drafting = self.drafting && ui.replacement.is_some();
        self.list.borrow().replace_rows(ui.rows(drafting));
        self.list.borrow().load_notice(ui.notice.clone());
        let size = ctx.max.size();
        let mut surface = Surface::with_size(size);
        let mut offset = 0;
        if !drafting
            && !ui.entry
            && let Some(goal) = &ui.chat.borrow().goal
        {
            // Bound the preview so long objectives leave room for the controls.
            // The objective editor exposes the full text.
            let preview_ctx = ctx.with_constraints(
                Size::default(),
                MaxSize {
                    width: Some(size.width),
                    height: Some(size.height.saturating_sub(8).min(6)),
                },
            );
            let preview = Text::new(format!("Objective:\n{}", goal.objective)).draw(&preview_ctx);
            if preview.size.height > 0 {
                offset = preview.size.height + 1;
                surface.children.push(SubSurface {
                    origin: RelativePoint { row: 0, col: 0 },
                    surface: preview,
                    z_index: 0,
                });
            }
        }
        drop(ui);
        let list_ctx = ctx.with_constraints(
            Size::default(),
            MaxSize {
                width: Some(size.width),
                height: Some(size.height.saturating_sub(offset)),
            },
        );
        surface.children.push(SubSurface {
            origin: RelativePoint {
                row: i32::from(offset),
                col: 0,
            },
            surface: draw_widget(&to_widget_ref(Rc::clone(&self.list)), &list_ctx),
            z_index: 0,
        });
        surface
    }
}

struct ObjectiveEditor {
    area: Rc<RefCell<TextArea>>,
    ui: Weak<RefCell<GoalUi>>,
    /// Fix the editor's purpose at open. Live updates must not turn an edit into
    /// creation or redirect its draft to another goal.
    goal_id: Option<String>,
}

impl Widget for ObjectiveEditor {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        child_surface(&to_widget_ref(Rc::clone(&self.area)), ctx)
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else { return };
        let Some(ui) = self.ui.upgrade() else { return };
        if key.matches(Key::ESCAPE, Modifiers::empty()) {
            ui.borrow().close(ctx);
            ctx.consume_and_redraw();
        } else if key.matches(Key::ENTER, Modifiers::empty()) {
            let text = self.area.borrow().expanded_text();
            let mut state = ui.borrow_mut();
            if text.trim().is_empty() {
                state.error("Goal objective must not be empty.");
            } else if !state.pending {
                if let Some(id) = &self.goal_id {
                    state.edit_draft = Some((id.clone(), text.clone()));
                    state.request(GoalRequest::for_goal(
                        id.clone(),
                        GoalAction::Edit { objective: text },
                    ));
                } else {
                    state.objective = text;
                }
                state.close(ctx);
            }
            ctx.consume_and_redraw();
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

pub(crate) fn can_resume(goal: &Goal) -> bool {
    !matches!(goal.status, GoalStatus::Active | GoalStatus::Complete)
        && goal
            .token_budget
            .is_none_or(|budget| goal.tokens_used < budget)
}

fn parse_budget(text: &str) -> Result<Option<u64>, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or("Token budget must be a positive integer, or blank for unlimited.")
}

/// Preserve the child's identity in the focus path. Returning its surface
/// directly would replace that identity with the wrapping widget's identity.
fn child_surface(child: &WidgetRef, ctx: &DrawContext) -> Surface {
    let child = draw_widget(child, ctx);
    let mut surface = Surface::with_size(child.size);
    surface.children.push(SubSurface {
        origin: RelativePoint { row: 0, col: 0 },
        surface: child,
        z_index: 0,
    });
    surface
}

fn create_action(objective: &str, budget: &str) -> Result<GoalAction, &'static str> {
    if objective.trim().is_empty() {
        return Err("Goal objective must not be empty.");
    }
    Ok(GoalAction::Create {
        objective: objective.to_string(),
        token_budget: parse_budget(budget)?,
    })
}

fn budget_label(budget: Option<u64>) -> String {
    budget.map_or_else(|| "Unlimited".into(), |value| value.to_string())
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn row(id: &str, label: &str, value: &str, description: &str, editable: bool) -> SettingRow {
    SettingRow {
        id: id.into(),
        label: label.into(),
        value: value.into(),
        description: description.into(),
        kind: if editable {
            RowKind::Submenu
        } else {
            RowKind::Cycle(Vec::new())
        },
        inherited: false,
        clear_to: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{draw_ctx, rows, widget_app};

    struct GoalWidget {
        handles: OverlayHandles,
        chat: Rc<RefCell<ChatState>>,
        requests: tokio::sync::mpsc::UnboundedReceiver<GoalRequest>,
    }

    impl GoalWidget {
        fn new(goal: Option<Goal>) -> Self {
            let handles = OverlayHandles::for_tests();
            let chat = Rc::new(RefCell::new(ChatState::new(
                aj_agent::events::AgentSettings {
                    context_window: 0,
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "off".into(),
                    thinking_display: "default".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
            )));
            chat.borrow_mut().goal = goal;
            let (requests, receiver) = unbounded_channel();
            GoalUi::open(&handles, Rc::clone(&chat), requests, false);
            Self {
                handles,
                chat,
                requests: receiver,
            }
        }

        fn rows(&self) -> Vec<String> {
            let widget = Rc::clone(&self.handles.stack.borrow().top().unwrap().widget);
            rows(&draw_widget(&widget, &draw_ctx(80, Some(24))))
        }

        async fn send(&self, events: impl IntoIterator<Item = Event>) {
            let (widget, focus) = {
                let stack = self.handles.stack.borrow();
                let top = stack.top().unwrap();
                (Rc::clone(&top.widget), Rc::clone(&top.focus))
            };
            let (mut app, root, _input) = widget_app(
                widget,
                focus,
                Size {
                    width: 80,
                    height: 24,
                },
            )
            .await;
            app.render(&root).unwrap();
            for event in events {
                app.handle_input(event);
            }
        }

        async fn choose(&self, query: &str) {
            self.send([
                key(u32::from('a'), Modifiers::CTRL),
                key(u32::from('k'), Modifiers::CTRL),
                Event::Paste(query.into()),
                key(Key::ENTER, Modifiers::empty()),
            ])
            .await;
        }

        async fn submit(&self, text: &str) {
            self.send([
                Event::Paste(text.into()),
                key(Key::ENTER, Modifiers::empty()),
            ])
            .await;
        }
    }

    fn key(codepoint: u32, mods: Modifiers) -> Event {
        Event::KeyPress(Key {
            codepoint,
            mods,
            ..Key::default()
        })
    }

    #[test]
    fn creation_validates_without_altering_the_multiline_draft() {
        let objective = "First line\n  second line  ";
        assert!(matches!(create_action(objective, "  ").unwrap(),
            GoalAction::Create { objective: value, token_budget: None } if value == objective));
        assert!(matches!(
            create_action(objective, "42").unwrap(),
            GoalAction::Create {
                token_budget: Some(42),
                ..
            }
        ));
        assert!(create_action(" \n ", "").is_err());
        for invalid in ["0", "-1", "1.5", "abc", "18446744073709551616"] {
            assert!(create_action(objective, invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn invalid_budget_keeps_the_editor_and_cancelled_objective_is_not_submitted() {
        let mut ui = GoalWidget::new(None);
        ui.choose("objective").await;
        ui.send([
            Event::Paste("discard this".into()),
            key(Key::ESCAPE, Modifiers::empty()),
        ])
        .await;
        assert!(ui.requests.try_recv().is_err());
        ui.choose("objective").await;
        assert!(!ui.rows().join("\n").contains("discard this"));
        let objective = "First line\n  second line  ";
        ui.submit(objective).await;
        ui.choose("token budget").await;
        ui.submit("0").await;
        assert_eq!(ui.handles.stack.borrow().depth(), 2);
        assert!(ui.rows().join("\n").contains('0'));
        assert!(
            ui.handles
                .toasts
                .borrow()
                .iter()
                .any(|toast| toast.text().contains("positive integer"))
        );
        assert!(ui.requests.try_recv().is_err());
        ui.send([
            key(u32::from('a'), Modifiers::CTRL),
            key(u32::from('k'), Modifiers::CTRL),
        ])
        .await;
        ui.submit("100").await;
        assert_eq!(ui.handles.stack.borrow().depth(), 1);
        assert!(
            ui.requests.try_recv().is_err(),
            "drafting must not start pursuit"
        );
        ui.choose("start goal").await;
        let request = ui.requests.try_recv().unwrap();
        assert!(request.expected_goal_id.is_none());
        assert!(matches!(request.action,
            GoalAction::Create { objective: saved, token_budget: Some(100) }
            if saved == objective));
    }

    #[tokio::test]
    async fn management_bounds_preview_but_keeps_the_full_objective_editable() {
        let objective = format!(
            "first line\n{}\nsecond line",
            "A detailed requirement. ".repeat(200)
        );
        let mut ui = GoalWidget::new(Some(Goal {
            id: "goal".into(),
            objective: objective.clone(),
            status: GoalStatus::Active,
            token_budget: Some(100),
            tokens_used: 0,
            time_used_seconds: 0,
        }));
        let preview = ui.rows();
        assert!(preview.iter().any(|line| line.contains("first line")));
        assert!(
            preview
                .iter()
                .filter(|line| line.contains("A detailed requirement."))
                .count()
                >= 2,
            "objective wraps across preview rows"
        );
        assert!(!preview.join("\n").contains("second line"));
        assert!(
            preview.join("\n").contains("Token budget"),
            "preview leaves room for controls"
        );
        ui.choose("objective").await;
        assert!(ui.rows().join("\n").contains("second line"));
        ui.send([key(Key::ENTER, Modifiers::empty())]).await;
        let request = ui.requests.try_recv().unwrap();
        assert_eq!(request.expected_goal_id.as_deref(), Some("goal"));
        assert!(
            matches!(request.action, GoalAction::Edit { objective: saved } if saved == objective)
        );
    }

    #[test]
    fn management_renders_live_goal_durations() {
        let ui = GoalWidget::new(Some(Goal {
            id: "goal".into(),
            objective: "objective".into(),
            status: GoalStatus::Active,
            token_budget: None,
            tokens_used: 0,
            time_used_seconds: 0,
        }));
        for (seconds, expected) in [
            (59, "59s"),
            (90, "1m"),
            (7200, "2h"),
            (5400, "1h 30m"),
            (93780, "1d 2h 3m"),
        ] {
            ui.chat
                .borrow_mut()
                .goal
                .as_mut()
                .unwrap()
                .time_used_seconds = seconds;
            let rows = ui.rows();
            let time = rows.iter().find(|row| row.contains("Time used")).unwrap();
            assert_eq!(
                time.trim_matches('│')
                    .trim()
                    .strip_prefix("Time used")
                    .unwrap()
                    .trim(),
                expected
            );
        }
    }
}
