//! Terminal-capability facts shared across the vaxis frontend.

/// Whether we emit OSC 8 hyperlinks (markdown links, the login dialog's
/// authorize URL).
///
/// vaxis's `Capabilities` surfaces no hyperlink probe, so there is nothing to
/// read from `app.vaxis().caps`: we optimistically enable OSC 8. vaxis writes
/// the escape unconditionally and terminals that lack support ignore the
/// bytes. TODO(aljoscha): thread a real capability once vaxis detects
/// hyperlink support, wiring it through the theme-derived style builders the
/// way `ColorMode` is.
pub(crate) const TERMINAL_HYPERLINKS: bool = true;
