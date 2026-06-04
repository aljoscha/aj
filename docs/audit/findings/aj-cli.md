# Audit findings — aj-cli

- **Step:** A1
- **Date:** 2026-06-02
- **Audited commit:** aaefc43
- **Scope:** `src/aj/src/cli.rs`, `src/aj/src/cli/args.rs`,
  `src/aj/src/cli/file_args.rs`, `src/aj/src/config.rs`,
  `src/aj/src/config/keybindings.rs`,
  `src/aj/src/config/slash_commands.rs`, `src/aj/src/config/theme.rs`,
  `src/aj/src/model.rs`, `src/aj/src/auth.rs`, `src/aj/src/clipboard.rs`,
  `src/aj/Cargo.toml` (incl. the in-module `#[cfg(test)]` suites).
  Cross-referenced `src/aj/src/modes/print.rs` and
  `src/aj/src/modes/interactive.rs` for the precedence-merge and
  disabled-tools call sites (those files belong to A2/A3).

## Summary

The CLI/config/model/auth/clipboard surface is in good shape and
secret hygiene is clean: the `--api-key` override is documented as
never-persisted and is never logged; `auth.rs` only ever *describes*
credentials (method + source) and never prints a token; `clipboard.rs`
copies image bytes and the OAuth *authorization* URL (public, carries a
PKCE challenge, not a secret); and the `.env` loader logs only the path.
`model.rs`, `theme.rs`, and `keybindings.rs` are cohesive, well-documented,
and well-tested — `theme.rs` in particular is a model of a self-contained
loader-plus-hot-reload module with edge-case-first tests. The standout
issues are two contract gaps. First, `cli/file_args.rs::expand` is a
no-op passthrough, yet `CLAUDE.md`, the `cli/args.rs` flag docs, the
module doc, and `print.rs` all advertise `@file` expansion as a working
feature — a documented contract with no implementation, and it isn't even
called from interactive mode. Second, the documented CLI > env > config
precedence merge has no single owner: `model.rs` only does registry
lookup, and the `args.X.or(config.X)` overlay is hand-copied across the
two mode entry points (print + interactive ×2), exactly mirroring the
disabled-tools duplication TO1 flagged (also confirmed here at three
sites). Both are localized and fixable.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 2 | 4 | 2 |

## Findings

### [Major][Contracts] `cli/file_args.rs::expand` is a no-op passthrough while `@file` expansion is advertised as a working feature across the CLI surface — `src/aj/src/cli/file_args.rs:23`
**What:** `expand` returns its input unchanged
(`pub fn expand(prompt: String) -> Result<String> { Ok(prompt) }`,
`file_args.rs:23-25`), and its own doc says so ("Today this is a
passthrough — the scaffold step doesn't ship expansion behaviour … 
Subsequent Phase 1 steps wire the real resolver"). Yet `@path` expansion
is documented as a live contract in four places: `CLAUDE.md`'s `aj`
description, the `Args::prompt` doc ("any `@path` token is expanded by
[`crate::cli::file_args`]", `cli/args.rs:58`), the `Command::Continue`
prompt doc (`cli/args.rs:121`), and the `cli` module doc (`cli.rs:6`).
The stub is only wired into print mode (`print.rs:421`); the interactive
entry point never calls `expand` at all, so even when implemented the two
modes would diverge.
**Why it matters:** A user typing `aj "summarize @src/main.rs"` gets the
literal token `@src/main.rs` sent to the model, not the file content — a
silent feature absence the docs promise is present. This is the
"contract advertised but not owned/implemented" smell at its sharpest:
the type, the call site, and the docs exist, but the behaviour is a
return-the-input stub. The single-call-site-in-print-only wiring also
means the eventual implementation has a built-in mode asymmetry.
**Suggested action:** Either implement the resolver the docs describe
(cwd-relative path handling, directory listings, binary-file rejection)
and call it from both mode entry points, or — if it's genuinely deferred —
strip the `@file` claims from `CLAUDE.md`, the `Args`/`Command` flag docs,
and the module docs so they stop advertising an absent feature. Decide the
cut with the user; the guidance is explicit about not silently shipping a
downgraded/aspirational contract.
**Effort:** M

### [Major][Simplicity] The CLI > env > config precedence merge has no single owner and is hand-duplicated across the mode entry points — `src/aj/src/model.rs:88`, `src/aj/src/modes/print.rs:201-203,185-190`, `src/aj/src/modes/interactive.rs:204-215,166-171,259-264`
**What:** `model.rs::resolve` takes a pre-merged `(provider, model, url)`
triple — it does registry lookup only, not precedence. The actual
overlay (`args.model_api.as_deref().or(config.model_api.as_deref())`,
and the same for `model_name`/`model_url`) is copy-pasted at the two
`resolve` call sites: `print.rs:201-203` and `interactive.rs:204-215`.
The *provider id* half of the same merge is then hand-rolled a *third*
and *fourth* time to decide where the `--api-key` runtime override lands
(`print.rs:185-190`, `interactive.rs:259-264`) and a *fifth* on the
scripted path (`interactive.rs:166-171`), each with a slightly different
shape (`.clone().or_else(...)` vs `.as_deref().or(...)`,
`.unwrap_or(DEFAULT_PROVIDER_ID)` vs `.unwrap_or_else(|| DEFAULT…to_string())`).
The env layer is folded in implicitly by clap's `env = "MODEL_API"`
attrs (`cli/args.rs:17,21,25`), so `args.model_*` is "post-env" by the
time these run — correct, but undocumented at the merge sites except for
one inline note (`print.rs:126-129`). C1 already flagged that the
`Config` doc advertises this precedence chain it doesn't own; this is
where it actually lives, scattered.
**Why it matters:** This is a *documented contract* (`CLAUDE.md`'s
"Model selection precedence (highest to lowest): CLI flags → env vars →
config.toml → built-in defaults"). Five hand-written copies of the
overlay drift easily — e.g. the provider-id picks already use two
different fallback idioms, and a future field (a sixth model knob, or a
per-provider default) must be threaded through every copy. The audit
focus asks whether precedence is "implemented in ONE clear place"; it is
not.
**Suggested action:** Add one helper (e.g. `model::ModelSelection { api,
name, url }` with a `fn merge(args: &Args, config: &Config) ->
ModelSelection`, plus a `provider_id()` accessor that applies the
`DEFAULT_PROVIDER_ID` fallback once) and call it from both modes. Keep
the env layer in clap but document the "args are post-env" assumption on
the helper. Collapses five sites to one and gives the documented
precedence a single home.
**Effort:** M

### [Minor][Simplicity] Disabled-tools filter copy-pasted across three binary call sites — `src/aj/src/modes/print.rs:146-149`, `src/aj/src/modes/interactive.rs:178-180,227-229`
**What:** `tools.retain(|tool| !config.disabled_tools.contains(&tool.name))`
(guarded by `if !config.disabled_tools.is_empty()`) appears verbatim at
`print.rs:146`, `interactive.rs:178` (scripted path), and
`interactive.rs:227` (registry path). `aj-tools::get_builtin_tools`
explicitly defers this filtering to the binary (TO1, `lib.rs:54-56`), and
TO1 already flagged the three sites; this audit confirms them at the
current commit. One site (`print.rs:148`) additionally logs the filtered
set; the other two don't, so the copies have already begun to diverge.
**Why it matters:** Three independent copies of a filter that must agree.
The natural seam — "build the catalog minus disabled names" — is the
predicate TO1 proposed living in `aj-tools` (or a thin binary helper).
The divergent logging is exactly the drift the duplication invites.
**Suggested action:** As TO1 recommends: add
`get_builtin_tools(options, disabled: &[String])` (or a
`filter_disabled` helper) in `aj-tools` and call it from all three sites;
move the `tracing::info!` into the helper so logging is uniform.
**Effort:** S

### [Minor][Boundaries] `ThemeColor::ThinkingMinimal` is a dead schema token — declared, JSON-keyed, parsed from every theme, but never mapped to any thinking level — `src/aj/src/config/theme.rs:176,234,286`
**What:** `ThemeColor::ThinkingMinimal` ("thinkingMinimal") is in the
enum (`theme.rs:176`), in `json_key` (`:234`), in `all()` (`:286`), and
is therefore a *required* key in every palette file (both bundled themes
and every user theme must define `"thinkingMinimal"` or
`from_json` errors with `MissingColor`). But `thinking_color_token`
(`theme.rs:1267-1275`) never returns it — `ThinkingConfig` has no
`Minimal` variant, so the token maps to nothing. The
`thinking_color_token_maps_each_level_to_its_token` test confirms every
real level routes elsewhere.
**Why it matters:** A schema token that the loader forces every user
theme author to supply but that the renderer never consults is vestigial
config (the rubric's "dead code / vestigial config" item). It also makes
the 51-key schema that every test fixture and user theme must spell out
one key larger than necessary. Either `ThinkingConfig::Minimal` was
removed and this is a leftover, or a minimal tint was planned and never
wired.
**Suggested action:** Drop `ThinkingMinimal` from the enum, `json_key`,
`all()`, both bundled JSONs, and the test fixtures; or, if a minimal
thinking level is coming, document the token as reserved. Decide with the
user since removing it changes the required user-theme schema.
**Effort:** S

### [Minor][Comments] `SlashAction::NotYetImplemented` is documented as reserved-for-the-future and has no producer — dead variant + chronology comment — `src/aj/src/config/slash_commands.rs:205-212`
**What:** The doc says "No builtin command maps here today; the variant
is preserved so future deferred commands can land without re-introducing
the type." `dispatch` (`:232-253`) never returns it, and no
`BUILTIN_COMMANDS` entry references it. So the variant is unreachable
dead code, and its doc is precisely the forward-looking "today / future"
chronology framing the guidance and TO1's `BuiltinToolOptions` finding
call out.
**Why it matters:** An enum variant kept solely against a hypothetical
future command is over-engineering; the comment narrates roadmap rather
than describing steady-state behaviour. Adding it back later is a
one-line change, so "preserved so future commands can land" buys nothing.
**Suggested action:** Remove the variant (and its host-side match arm, if
any) until a command actually needs it; or, if it's genuinely about to be
used, drop the "today / future" framing and state what produces it.
**Effort:** S

### [Minor][Contracts] `Theme::available()` and the `/theme` command are half-wired: a discovery API and a watcher exist, but there is no `theme` slash command — `src/aj/src/config/theme.rs:802-823`, `src/aj/src/config/slash_commands.rs:68`
**What:** `Theme::available()` (`theme.rs:806`) is documented "Used by
future `/theme` selector autocomplete," and `editor_border_color_for_bash_mode`
(`theme.rs:1300`) is "Reserved for a future bash-mode toggle." But
`BUILTIN_COMMANDS` (`slash_commands.rs:68-146`) has no `theme` entry and
`dispatch` has no `theme` arm, so `available()` has no in-tree caller and
the bash-mode border builder is unused. The theme hot-reload watcher is
fully wired and tested, but the user-facing *selection* path the doc
points at doesn't exist.
**Why it matters:** Public API justified solely by a "future" caller is
the speculative-generality / dead-public-surface smell, and the doc
chronology ("future `/theme`") will go stale. `available()` is `pub` and
untested for the user-dir-enumeration branch precisely because nothing
consumes it.
**Suggested action:** Either land the `/theme` command that consumes
`available()` (and a `bash mode` toggle for the border builder), or mark
these as internal/remove the "future" framing and the dead `pub` until a
consumer exists. Decide scope with the user.
**Effort:** S

### [Nit][Comments] Module docs narrate scaffold/plan chronology that has gone stale — `src/aj/src/config.rs:6-7`, `src/aj/src/cli/file_args.rs:8-12`
**What:** `config.rs` says "The scaffold only declares the modules; the
'Selectors and theming' step in Phase 1 fills them in" — but the modules
are fully filled in (theme.rs is 1849 lines). `file_args.rs:8-12`
narrates "The scaffold reserves the module; the print-mode and
interactive-mode steps fill in the actual expansion logic." Both are the
chronology-in-comments the guidance forbids: they describe a past plan
state, not the current code.
**Why it matters:** Comments must stand on their own from the current
code; "the scaffold step fills this in later" is false for `config.rs`
(already filled) and is doing double duty for `file_args.rs` (where it
also disguises the unimplemented-feature gap flagged above).
**Suggested action:** Replace with steady-state descriptions of what each
module *is*. Fold the `file_args.rs` doc into whatever decision is made on
the Major `expand` finding.
**Effort:** S

### [Nit][Style] `aj` keybinding constants and `BUILTIN_COMMANDS` action-id pairing are guarded only by hand — no test asserts every `action_id` resolves — `src/aj/src/config/slash_commands.rs:123,137`
**What:** Two `BUILTIN_COMMANDS` entries carry an `action_id`
(`ACTION_HISTORY_OPEN`, `ACTION_PALETTE_OPEN`) that the palette/help UI
resolves at render time against the keybindings manager. The
keybindings tests assert each *constant* resolves to its default key, and
the slash-command tests assert each *command* dispatches — but nothing
asserts that a command's `action_id` actually exists in `all_keybindings()`.
A typo'd or removed action id would surface as a blank shortcut column at
runtime, not a test failure.
**Why it matters:** The two halves (catalog `action_id` ↔ keybinding
definition) are coupled by convention across two files; the module doc
itself stresses keeping the catalog/dispatch pairing "honest," but the
catalog↔keybinding pairing has no such guard.
**Suggested action:** Add a small test iterating `BUILTIN_COMMANDS`,
asserting every `Some(action_id)` is present in a
`KeybindingsManager::new(all_keybindings(), …)`. Cheap drift guard.
**Effort:** S

## What's good

- **Secret hygiene is clean (confirms the `CLAUDE.md` split, refutes any
  leak).** `--api-key` is documented as a runtime-only override that is
  "never written to disk" and intentionally has no `env =` binding
  (`cli/args.rs:28-35`); it is applied via `AuthStorage::set_runtime_api_key`
  and never logged. `auth.rs` only ever renders *method + source* labels
  (`"env: ANTHROPIC_API_KEY"`, `"subscription"`, `"expires in 1h 47m"`) —
  it never formats a token, and a corrupt/locked `auth.json` degrades to a
  status row rather than crashing the overlay (`auth.rs:106-113`). The
  `.env` loader logs the path only (`main.rs:41`). The M5 token-leak theme
  does **not** reproduce in the binary's CLI layer.
- **Clipboard copies a public URL, not a secret.** `auth::copy_to_clipboard`
  is invoked only on the OAuth *authorization* URL in the login dialog
  (`login_dialog.rs:187,237`) — a public, PKCE-protected URL, not the
  resulting token. `clipboard.rs` itself only ever moves *image* bytes to a
  tempfile; it reads no credentials. The dual arboard + OSC 52 path and the
  tmux passthrough wrapper are carefully documented and unit-tested
  (`osc52_payload`).
- **`theme.rs` is a model loader module.** One cohesive responsibility
  (JSON palette → resolved ANSI escapes), a closed-enumeration schema with
  `json_key`/`all()` kept in lockstep, transitive var resolution with
  cycle detection, environment-driven `ColorMode` downsampling with an
  injectable `from_json_with_mode` test seam, a fail-open `load` vs
  fail-loud `load_strict` split, and a directory-targeted hot-reload
  watcher that survives editor tempfile+rename and swallows mid-edit parse
  errors. The test suite covers var cycles, unknown vars, missing keys,
  256-color downsampling, the hot-swap invariant, and the watcher
  end-to-end including the ignore-unrelated-files and swallow-parse-error
  paths. Strong, replicable.
- **`model.rs` is well-factored and well-tested for what it owns.** The
  `(api, model, url, speed) → ResolvedModel` lookup, the lazy
  `ApiKeyResolver` (documented as walking override→env→stored→OAuth on
  every inference so a session can start uncredentialed), the
  one-knob→two-wire-field `apply_thinking_display` fan-out, and the
  `anthropic-beta` header coalescing are each documented with their
  rationale and covered by focused unit tests (including the
  detailed→summarized degrade and the duplicate-beta skip).
- **`anyhow` is used well and only where appropriate.** Per `CLAUDE.md`,
  the binary may use `anyhow`; it adds `.context(...)` at the I/O and
  resolution boundaries (`print.rs:184,206`, `interactive.rs:157,218`) and
  `model.rs` produces structured `anyhow!` messages the callers wrap
  further. The genuinely library-shaped modules in scope
  (`theme.rs`, `keybindings.rs`) correctly use `thiserror`/typed returns,
  not `anyhow`.
- **Slash-command catalog/dispatch pairing is deliberately honest.** Both
  halves live in one file with a doc that explains why; commands are
  uniformly zero-argument with graceful trailing-token handling, and the
  dispatch tests cover quit/unknown/whitespace/trailing-arg for every
  command.

## Boundary & architecture notes

Dependency direction matches `CLAUDE.md`: the `aj` binary depends on all
five `aj-*` crates plus `aj-tui`, and these modules consume those crates'
public surfaces (`aj_conf::Config`, `aj_models::{auth,registry,provider,types}`,
`aj_tui::{keybindings,components}`, `aj_tools::get_builtin_tools`) without
reaching back the wrong way. All deps come from `[workspace.dependencies]`;
nothing in scope is obviously unused (`similar` is for diff rendering in
A4 components, `notify` powers the theme watcher, `arboard`/`image`/`rand`
power clipboard, `base64` powers OSC 52). Two boundary observations for
synthesis: (1) the precedence merge that C1 noted lives in the binary is
confirmed here, scattered across the two mode entry points with no single
owner (Major finding); (2) `aj/config/*` and `aj-conf` have a *clean*
split — `aj-conf` owns the file schema/parse/save and path helpers, while
`aj/config/*` owns the runtime-resolved artifacts (keybindings registry,
theme palette + watcher, slash-command catalog) that consume the parsed
`Config`. That boundary is not muddy; the only cross-layer rough edge is
the precedence merge, which is genuinely a binary-orchestration concern
and arguably belongs in `aj::model` (where this report suggests it land).

## Test assessment

In-module `#[cfg(test)]` per convention throughout, exercising public
contracts. `theme.rs` is the high point (see What's good): edge-case-first
coverage of the loader, the hot-swap invariant, and the fs-watcher
end-to-end, with an injectable color mode and tempdir-based watcher tests
(no `$HOME` coupling). `model.rs` builds tiny in-memory registries
(avoiding the bundled catalog's nondeterminism) and covers the
provider/model lookup error paths, the speed-header matrix, and the
thinking-display fan-out including the degrade cases. `auth.rs` covers the
`format_remaining` buckets and the OSC 52 bare/tmux shapes deterministically.
`clipboard.rs` covers the pure helpers (filename generation, MIME→ext,
type negotiation) and pins the `image`-crate BMP/TIFF feature dependency
with a hand-rolled BMP fixture — a nice guard against a silent feature
strip. `slash_commands.rs` and `keybindings.rs` cover dispatch and default
bindings.

Gaps: (1) the subprocess clipboard backends (`wl-paste`, PowerShell) and
arboard OS access are untestable without platform mocking and are
acknowledged as hand-tested — acceptable. (2) `Theme::available()`'s
user-dir enumeration branch is untested (no consumer; tied to the
half-wired `/theme` finding). (3) No test asserts `BUILTIN_COMMANDS`
`action_id`s resolve against the keybindings manager (Nit). (4)
`auth.rs::now_unix_ms` and `clipboard` `temp_dir`/`generate_filename`
touch wall-clock/RNG/`TMPDIR`, but the time-dependent `format_remaining`
is correctly tested with an injected `now`, so no flakiness. No
real-network coupling.

## Cross-cutting themes to bubble up

- **Contract advertised but not implemented (NEW, sharper than C1).** C1
  found a *doc* overstating a crate's contract; here `@file` expansion is
  documented as live across `CLAUDE.md` + four code docs while
  `file_args::expand` is a literal passthrough stub, wired into only one
  of two modes. Synthesis should sweep for other "scaffold reserves the
  module / fills in later" stubs that docs treat as done.
- **Precedence merge has no single owner (CONFIRMED, this is where it
  lives).** C1 predicted the CLI > env > config merge lives in the binary;
  it does, hand-copied across five sites with divergent idioms. Feed into
  X1's check that the documented precedence is actually realized — it is,
  but not in one place.
- **Duplication across binary call sites (CONFIRMED, generalizes TO1).**
  The disabled-tools filter (three sites, already diverging on logging)
  and the precedence overlay (five sites) are the same shape TO1 flagged.
  The binary lacks a "build the agent's startup inputs" seam, so each mode
  re-derives toolset and model selection inline.
- **Dead/vestigial config kept against a "future" caller (CONFIRMED,
  recurring chronology smell).** `ThemeColor::ThinkingMinimal` (required
  in every user theme, mapped by nothing), `SlashAction::NotYetImplemented`
  (no producer), `Theme::available()` / `editor_border_color_for_bash_mode`
  (no `/theme` or bash-mode consumer) — all carry "future" framing the
  guidance forbids. Same pattern as TO1's `BuiltinToolOptions` "today/will"
  doc and the `aj-models` `scripted` surface.
- **Secret hygiene (CONFIRMED clean, third data point).** After C1 (config
  never touches keys) and the M5 theme, the binary's CLI layer also keeps
  keys/tokens out of logs, status text, and the clipboard. The
  `--api-key`-never-persisted contract is explicit and honored.
- **anyhow used well at the top level (COUNTEREXAMPLE holds).** The binary
  uses `anyhow` with `.context` at boundaries; the library-shaped modules
  in scope stay on `thiserror`. Reinforces the workspace split.
