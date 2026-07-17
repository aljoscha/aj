//! The branch banner: one accent row above the editor while a branch
//! anchor is armed.
//!
//! Reads the shared branch-indicator cell (written by the Shell in
//! lockstep with the branch anchor) at draw time. Zero height when no
//! anchor is armed, so the slot collapses and the editor sits flush
//! under the transcript between branches.

use std::cell::RefCell;
use std::rc::Rc;

use vaxis::vxfw::{DrawContext, Event, EventContext, RichText, Size, Surface, TextSpan, Widget};

use crate::transcript::TranscriptStyles;

/// The banner row shown directly above the editor while a branch is armed.
pub(crate) struct BranchBanner {
    /// The branch indicator, shared with the Shell (its single writer).
    /// `Some` holds the "branching from message" preview while a branch
    /// anchor is armed, `None` otherwise.
    indicator: Rc<RefCell<Option<String>>>,
    styles: Rc<TranscriptStyles>,
}

impl BranchBanner {
    pub(crate) fn new(
        indicator: Rc<RefCell<Option<String>>>,
        styles: Rc<TranscriptStyles>,
    ) -> BranchBanner {
        BranchBanner { indicator, styles }
    }

    /// Replace the palette styles, for a runtime theme swap.
    pub(crate) fn set_styles(&mut self, styles: Rc<TranscriptStyles>) {
        self.styles = styles;
    }
}

impl Widget for BranchBanner {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let Some(indicator) = self.indicator.borrow().clone() else {
            // No armed anchor: collapse to zero height so the editor sits
            // flush under the transcript.
            return Surface::with_size(Size {
                width: ctx.max.width.unwrap_or(0),
                height: 0,
            });
        };
        let span = |text: String, style| TextSpan {
            text,
            style,
            ..TextSpan::default()
        };
        // A leading blank row separates the banner from the transcript
        // above, then ` {indicator}` in accent so the pending branch is
        // visible but unobtrusive.
        let spans = vec![
            span("\n ".to_string(), self.styles.text),
            span(indicator, self.styles.accent),
        ];
        let mut rich = RichText::new(spans);
        // Softwrap off: a long preview on a narrow terminal truncates with
        // an ellipsis instead of wrapping, which would grow the slot and
        // push the editor down a row.
        rich.softwrap = false;
        rich.draw(ctx)
    }

    fn handle_event(&mut self, _ctx: &mut EventContext, _event: &Event) {}
}

#[cfg(test)]
mod tests {
    use aj_app::theme::Theme;

    use super::*;

    fn styles() -> Rc<TranscriptStyles> {
        Rc::new(TranscriptStyles::from_theme(
            &Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor),
        ))
    }

    fn draw_rows(indicator: Rc<RefCell<Option<String>>>, width: u16) -> Vec<String> {
        let mut banner = BranchBanner::new(indicator, styles());
        let surface = banner.draw(&crate::test_support::draw_ctx(width, None));
        crate::test_support::rows(&surface)
    }

    #[test]
    fn collapses_to_zero_height_when_disarmed() {
        let banner = Rc::new(RefCell::new(None));
        let mut b = BranchBanner::new(banner, styles());
        let surface = b.draw(&crate::test_support::draw_ctx(80, None));
        assert_eq!(surface.size.height, 0);
    }

    #[test]
    fn shows_the_indicator_when_armed() {
        let indicator = Rc::new(RefCell::new(Some(
            "branching from message: fix the parser".to_string(),
        )));
        let rows = draw_rows(indicator, 80);
        assert!(
            rows.iter()
                .any(|r| r.contains("branching from message: fix the parser")),
            "banner shows the indicator: {rows:?}"
        );
    }

    /// A long preview truncates instead of wrapping, so the banner never
    /// grows past its leading blank row plus one content row.
    #[test]
    fn truncates_instead_of_wrapping() {
        let indicator = Rc::new(RefCell::new(Some(format!(
            "branching from message: {}",
            "x".repeat(200)
        ))));
        let mut b = BranchBanner::new(indicator, styles());
        let surface = b.draw(&crate::test_support::draw_ctx(40, None));
        assert_eq!(surface.size.height, 2, "blank row plus one content row");
    }
}
