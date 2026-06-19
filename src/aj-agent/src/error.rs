//! Opaque error type for the runtime's render-only failure seams.

/// Boxed, type-erased error used at the runtime's failure seams that
/// the agent only ever renders: tool execution, the event bus, and the
/// cause carried by [`crate::TurnError::Recoverable`] /
/// [`crate::TurnError::Fatal`].
///
/// These boundaries don't branch on the cause, so we expose a named
/// opaque error rather than a rich enum or `anyhow`. `?` still works for
/// any `std::error::Error`, and `"msg".into()` builds an ad-hoc error,
/// without leaking a specific error library into the public tool-author
/// or bus surface.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
