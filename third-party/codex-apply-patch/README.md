# Codex apply_patch grammar

Source: https://github.com/openai/codex
Revision: `44984d20817fc58026a8bbccf053e288c6897c8f`
License: Apache-2.0, reproduced in `LICENSE`. Upstream `NOTICE` is included.

`src/aj-tools/assets/apply_patch.lark` is an unmodified copy of
`codex-rs/core/assets/tools/apply_patch.lark`. The freeform description in
`src/aj-tools/src/tools/apply_patch.rs` comes from
`codex-rs/core/src/tools/handlers/apply_patch_spec.rs` at the same revision.
The grammar does not include the optional environment ID extension.

To update, choose and record an exact upstream revision, copy the base grammar
and upstream license and notice verbatim, and compare the freeform description.
Review parser and application behavior separately against the bounded
compatibility tests in `apply_patch.rs`. The grammar is not a claim of complete
Codex runtime compatibility. Run the aj-tools tests (including
`apply_patch_interface`), agent Code Mode tests, and the normal workspace gate.

## Compatibility boundary

AJ retains its own parser, diagnostics, structured diffs, and filesystem
execution. Selectors and EOF markers must match. Empty files, trailing blank
lines, content-preserving moves, and pure insertion follow the pinned behavior.

AJ's fuzzy matching and replacement reindentation remain distinct, as does its
whole-file CRLF handling for mixed line endings. Multi-file operations apply
sequentially and can report partial success, rather than using Codex's
model-handler preflight. Overlap rejection, alias protection, and move rollback
remain AJ safety checks.

Raw calls and Code Mode use string input. JSON-only APIs keep the `patchText`
schema and its JSON-appropriate description. The decoder also accepts `patch`
and `input` aliases, including raw history projected onto JSON-only APIs. Hooks
receive the original argument shape and may rewrite it or deny execution.
