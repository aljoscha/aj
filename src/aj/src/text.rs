//! Text hygiene for values the frontend draws.
//!
//! A session's id, a user's tag and a peer's host name all reach the screen
//! from places this process does not control: a filesystem that accepts any
//! byte in a name, a hand-edited sidecar, a host that may not normalize what
//! it publishes. The widgets that draw them promise one line per row, and the
//! text runtime cannot keep that promise on its own.

/// Drop control characters, so `text` occupies exactly one drawn line.
///
/// Two reasons, and both are why this is applied at every sink rather than
/// trusted to the producer:
///
/// - A newline splits the row and misattributes every line below it.
/// - A carriage return that is not part of a `\r\n` pair panics `RichText`'s
///   hard-break walk, which happens inside `draw` and so takes the whole
///   frontend down.
pub(crate) fn one_line(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_are_dropped() {
        assert_eq!(one_line("two\nlines"), "twolines");
        assert_eq!(one_line("ab\rcd"), "abcd");
        assert_eq!(one_line("bell\u{7}"), "bell");
        assert_eq!(one_line("fix-auth"), "fix-auth");
    }

    /// The one input the text runtime cannot survive, drawn through the widget
    /// that draws every folded value: a lone carriage return underflows
    /// `RichText`'s hard-break walk, which is a panic in the draw path.
    #[test]
    fn a_lone_carriage_return_is_safe_to_draw() {
        use vaxis::vxfw::{RichText, TextSpan, Widget};

        let mut text = RichText {
            softwrap: false,
            ..RichText::new(vec![TextSpan {
                text: one_line("ab\rcd"),
                ..TextSpan::default()
            }])
        };
        let surface = text.draw(&crate::test_support::draw_ctx(20, Some(1)));
        assert_eq!(surface.size.height, 1, "one line, and no panic getting it");
    }
}
