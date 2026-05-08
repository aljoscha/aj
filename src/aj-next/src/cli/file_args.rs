//! Expansion of `@path` tokens in free-form prompt arguments.
//!
//! The CLI accepts `@somefile.rs` tokens in the user's initial
//! message: each token is replaced with the file's content (or a
//! directory listing) so the model gets a self-contained prompt
//! without the user having to paste manually.
//!
//! `docs/aj-next-plan.md` §4 calls out `cli/file_args.rs` as a
//! dedicated module for this expansion. The scaffold reserves the
//! module; the print-mode and interactive-mode steps fill in the
//! actual expansion logic alongside the editor's autocomplete
//! integration in `aj-tui`.

use anyhow::Result;

/// Expand any `@path` tokens in `prompt` into inline file content.
///
/// Today this is a passthrough — the scaffold step doesn't ship
/// expansion behaviour. Subsequent Phase 1 steps wire the real
/// resolver (with cwd-relative path handling, directory listings
/// for folders, and binary-file rejection) once the print mode
/// and interactive editor surfaces are in place.
pub fn expand(prompt: String) -> Result<String> {
    Ok(prompt)
}
