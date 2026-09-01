//! Shared account-label inspection before identity-sensitive actions.

use std::cell::RefCell;
use std::rc::Rc;

use aj_models::auth::{AccountLabelDisplayMode, display_account_label, validate_account_label};
use vaxis::cell::Style;
use vaxis::key::{Key, Modifiers};
use vaxis::vxfw::{
    DrawContext, Event, EventContext, MaxSize, RelativePoint, ScrollView, ScrollableView, Size,
    Source, SubSurface, Surface, Text, Widget, WidthBasis, draw_widget, to_widget_ref,
};

pub(crate) const ACCOUNT_INSPECTION_CELL_LIMIT: usize = 65_535;
pub(crate) const OVER_LIMIT_PREFIX_CELLS: usize = 512;

/// Represent an exact raw account for a picker. Search retains the complete
/// canonical representation, while the row gets a disclosed bounded prefix.
pub(crate) fn account_picker_text(raw: &str) -> (String, String) {
    let represented = display_account_label(raw, AccountLabelDisplayMode::Ordinary);
    let shown = if represented.len() > ACCOUNT_INSPECTION_CELL_LIMIT {
        let prefix = represented.chars().take(96).collect::<String>();
        format!("[clipped; exceeds 65,535-cell inspection limit] {prefix}…")
    } else {
        represented.clone()
    };
    (shown, represented)
}

/// Represent an account inside softwrapped prose without losing identity.
/// Over-limit labels are named generically after their dedicated inspection.
pub(crate) fn account_notice_text(raw: &str) -> String {
    let ordinary = display_account_label(raw, AccountLabelDisplayMode::Ordinary);
    let represented = if ordinary.contains(' ') {
        display_account_label(raw, AccountLabelDisplayMode::Ascii)
    } else {
        ordinary
    };
    if represented.len() > ACCOUNT_INSPECTION_CELL_LIMIT {
        "the selected account (label exceeds the terminal inspection limit)".to_string()
    } else {
        represented
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InspectionOutcome {
    Pending,
    Back,
    Confirmed,
}

/// Horizontally scrollable, non-softwrapped presentation of one exact raw
/// account label.
///
/// Representations through 65,535 cells are completely inspectable. A longer
/// legacy identity puts only a disclosed bounded prefix into vaxis and
/// requires one acknowledgement before confirmation.
pub(crate) struct AccountLabelInspection {
    pub(crate) text: Rc<RefCell<Text>>,
    pub(crate) view: Rc<RefCell<ScrollView>>,
    pub(crate) represented_cells: usize,
    pub(crate) surface_representation_cells: usize,
    pub(crate) over_limit: bool,
    pub(crate) acknowledged: bool,
    last_width: u16,
    warning_style: Style,
}

impl AccountLabelInspection {
    pub(crate) fn new(raw_label: &str, warning_style: Style) -> Self {
        let creation_valid = validate_account_label(raw_label).is_ok();
        let represented = display_account_label(raw_label, AccountLabelDisplayMode::Ordinary);
        // A creation-valid ordinary label is bounded to 512 cells by contract.
        // Only the scalar-escaped legacy branch can approach the u16 limit, and
        // every one of its bytes is one terminal cell.
        let represented_cells = if creation_valid {
            usize::from(vaxis::gwidth::gwidth(
                &represented,
                vaxis::gwidth::Method::Unicode,
            ))
        } else {
            represented.len()
        };
        let over_limit = represented_cells > ACCOUNT_INSPECTION_CELL_LIMIT;
        let shown = if over_limit {
            represented
                .chars()
                .take(OVER_LIMIT_PREFIX_CELLS)
                .collect::<String>()
        } else {
            represented
        };
        let surface_representation_cells = shown.len();
        let mut text_widget = Text::new(shown);
        text_widget.softwrap = false;
        let text = Rc::new(RefCell::new(text_widget));
        let mut view = ScrollView::new(Source::Slice(vec![to_widget_ref(Rc::clone(&text))]));
        view.draw_cursor = false;
        Self {
            text,
            view: Rc::new(RefCell::new(view)),
            represented_cells,
            surface_representation_cells,
            over_limit,
            acknowledged: false,
            last_width: 1,
            warning_style,
        }
    }

    pub(crate) fn draw(&mut self, ctx: &DrawContext) -> Surface {
        debug_assert!(!self.text.borrow().softwrap);
        debug_assert!(self.surface_representation_cells <= ACCOUNT_INSPECTION_CELL_LIMIT);
        if !self.over_limit {
            let represented_cells = ctx.string_width(&self.text.borrow().text);
            self.represented_cells = represented_cells;
            self.surface_representation_cells = represented_cells;
        }
        let size = ctx.max.size();
        self.last_width = size.width.max(1);
        let warning_rows = if self.over_limit { 2 } else { 1 };
        let mut surface = Surface::with_size(size);
        if size.height > 0 {
            let view_ctx = ctx.with_constraints(
                Size {
                    width: 0,
                    height: 0,
                },
                MaxSize {
                    width: Some(size.width),
                    height: Some(1),
                },
            );
            surface.children.push(SubSurface {
                origin: RelativePoint { row: 0, col: 0 },
                surface: draw_widget(&to_widget_ref(Rc::clone(&self.view)), &view_ctx),
                z_index: 0,
            });
        }
        if size.height > 1 {
            let message = if self.over_limit && self.acknowledged {
                "Incomplete inspection acknowledged. Press Enter again to continue with the exact raw account."
                    .to_string()
            } else if self.over_limit {
                format!(
                    "Only a clipped prefix is shown. This legacy account exceeds the 65,535-cell \
                     terminal inspection limit ({} cells).",
                    self.represented_cells
                )
            } else {
                "Use Left/Right or Home/End to inspect the complete account label.".to_string()
            };
            let mut warning = Text::new(message);
            warning.style = self.warning_style;
            warning.softwrap = true;
            warning.width_basis = WidthBasis::Parent;
            let warning_ctx = ctx.with_constraints(
                Size {
                    width: 0,
                    height: 0,
                },
                MaxSize {
                    width: Some(size.width),
                    height: Some(size.height.saturating_sub(1).min(warning_rows)),
                },
            );
            surface.children.push(SubSurface {
                origin: RelativePoint { row: 1, col: 0 },
                surface: warning.draw(&warning_ctx),
                z_index: 0,
            });
        }
        surface
    }

    pub(crate) fn handle_event(
        &mut self,
        ctx: &mut EventContext,
        event: &Event,
    ) -> InspectionOutcome {
        let Event::KeyPress(key) = event else {
            self.view.borrow_mut().handle_event(ctx, event);
            return InspectionOutcome::Pending;
        };
        if key.matches(Key::ESCAPE, Modifiers::empty()) {
            InspectionOutcome::Back
        } else if key.matches(Key::HOME, Modifiers::empty()) {
            self.view.borrow_mut().set_scroll_left(0);
            ctx.consume_and_redraw();
            InspectionOutcome::Pending
        } else if key.matches(Key::END, Modifiers::empty()) {
            let max_left = self
                .surface_representation_cells
                .saturating_sub(usize::from(self.last_width));
            self.view
                .borrow_mut()
                .set_scroll_left(u32::try_from(max_left).expect("bounded surface offset fits u32"));
            ctx.consume_and_redraw();
            InspectionOutcome::Pending
        } else if key.matches(Key::ENTER, Modifiers::empty()) {
            if self.over_limit && !self.acknowledged {
                self.acknowledged = true;
                self.warning_style.bold = true;
                ctx.consume_and_redraw();
                InspectionOutcome::Pending
            } else {
                InspectionOutcome::Confirmed
            }
        } else {
            self.view.borrow_mut().handle_event(ctx, event);
            InspectionOutcome::Pending
        }
    }
}
