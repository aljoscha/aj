//! Terminal-capability facts shared across the vaxis frontend.
//!
//! Read once after `app.init(..)` from `app.vaxis().caps` and threaded into
//! [`TranscriptStyles`](crate::transcript::TranscriptStyles) alongside the
//! theme, so a capability change re-flows rendering the same way a theme swap
//! does. In practice caps are fixed for the session.

/// The runtime terminal capabilities the transcript styling reads.
#[derive(Clone, Copy)]
pub(crate) struct TerminalCaps {
    /// Whether inline kitty-graphics images render, from the real
    /// `caps.kitty_graphics` probe. False falls back to the text placeholder.
    pub(crate) images: bool,
    /// Whether we emit OSC 8 hyperlinks (markdown links, the login dialog's
    /// authorize URL).
    ///
    /// NOTE: vaxis surfaces no OSC 8 probe, so this stays optimistically true.
    /// vaxis writes the escape unconditionally and terminals that lack support
    /// ignore the bytes. The day vaxis grows a hyperlink probe this becomes a
    /// one-line change at the probe seam, since the runtime plumbing is already
    /// in place.
    pub(crate) hyperlinks: bool,
}

impl Default for TerminalCaps {
    fn default() -> TerminalCaps {
        TerminalCaps {
            images: false,
            hyperlinks: true,
        }
    }
}
