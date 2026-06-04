# Audit synthesis — workspace-wide summary (X1)

- **Step:** X1 (main-agent synthesis; no sub-agent)
- **Date:** 2026-06-02
- **Audited source commit:** `7de08cc` (audit docs only on top of source)
- **Inputs:** the 23 per-unit findings reports in this directory and the
  workspace manifests.

## Headline

The workspace is in good structural health. The layering is real and
mostly enforced by the crate graph, the wire/agent/session/UI boundaries
hold, and several subsystems are exemplary (the `aj-tui` renderer and its
virtual-terminal test suite, the `Provider` trait, `AgentMessage` /
`ToolDetails`, the model registry, PKCE). There is **one** architecture
deviation and a small cluster of **real runtime/correctness risks**, but
the bulk of the 206 findings are structural cleanups that repeat across
crates — meaning a handful of targeted refactors retire most of them.

| Severity | Count |
|---|---|
| Critical | 1 |
| Major | 36 |
| Minor | 106 |
| Nit | 63 |
| **Total** | **206** |

## Dependency-graph verification

Checked every crate manifest's intra-workspace edges against the intended
graph in `CLAUDE.md`.

**Matches the intent**, leaf-first: `anthropic-sdk`/`openai-sdk` are
leaves under `aj-models`; `aj-models` depends only on the SDKs;
`aj-session`/`aj-tools` depend on `aj-agent` + `aj-models`; `aj-tui` is a
standalone framework with zero domain-crate deps; the `aj` binary sits on
top of everything. Persistence is genuinely a pure `AgentEvent`
subscriber.

**One deviation (the lone Critical):** `aj-agent` depends on `aj-conf`
(`AgentEnv`, `ConfigThinkingLevel`), but `CLAUDE.md` states the runtime
"depends only on `aj-models`." Either move the shared config types to a
layer `aj-agent` may use (or into `aj-agent`/`aj-models`), or update the
documented graph to admit the edge. (`aj-agent/Cargo.toml`, `lib.rs:21`)

**Manifest drift:** `aj-agent` is the only crate pinned to
`edition = "2021"`; every other crate inherits the 2024 workspace edition.
`aj-tui` pins a few direct dependency versions instead of using
`[workspace.dependencies]`.

## Top runtime / correctness risks (fix first)

These are the findings that can actually bite a user. None are
theoretical.

### P0 — truncated turns look like successful turns (M3, M4, AG1, A3, A4)

The single most impactful theme, independently found at five layers. When
a provider stream closes **without** a terminal frame (mid-stream
transport drop), all four adapters (Anthropic, OpenAI completions,
Responses, Codex) finalize the turn as a clean `Done`/`Stop`. The agent
runtime trusts that classification with no guard, and the UI renders the
truncated message as complete. A cut-off turn is therefore persisted and
displayed as if finished, and the retry layer never engages. Fix belongs
at the provider/runtime boundary (treat "stream ended before terminal" as
a retryable error), not the UI.

### P0 — interactive agent mutex freezes the loop and defeats Ctrl-C (A3)

`interactive.rs` holds the agent `TokioMutex` guard across the whole
`prompt().await` (`:1163`), and other paths lock the same mutex
(`:1895`). Opening `/thinking` or `/model` mid-turn calls
`agent.lock().await`, which suspends the entire `select!` loop —
**including the cancel arm** — until the turn finishes. A reachable
hang + lost cancellation.

### P1 — markdown parser can crash the process on model output (T4)

`render_list`/`parse_inline` recurse over nested `**`/`>`/`[[…]]` with
only a *width* cap, no recursion-depth guard, and assistant/user message
components feed untrusted model text straight in (A4). Deeply nested input
can overflow the stack and abort the process. Add a depth cap + a
regression test with adversarial nesting.

### P1 — OAuth token can leak into an error string/log (M5)

On a 2xx token response that fails to deserialize, the raw body (live
access/refresh tokens) is folded into `OAuthError::Parse`
(`oauth/anthropic.rs:652`, `oauth/openai.rs:689`), which can reach logs or
the terminal. The only secrets defect found — everything else (key
storage 0600, `--api-key` never persisted/logged, clipboard copies the
public OAuth URL only) is clean.

### P1 — durability of user-data writes (C1, SE1, TO2)

Three loci, two failure modes. Rewrite-in-place: `Config::save` truncates
`config.toml` (crash mid-write corrupts it); `write_file`/`edit_file`/
`edit_file_multi` all `fs::write` truncate-in-place — and
`edit_file_multi` advertises an "atomic" contract it doesn't honor.
Append-without-fsync: the session log never `sync_data`/`flush`s despite
documenting "durable when the call returns." Adopt write-temp+rename for
rewrites (the `aj-models::auth` writer is the in-tree model) and an
explicit flush/fsync policy for the append log. Related: the session log
has no single-writer guard, so two `aj continue <id>` processes interleave
lines and mint colliding entry ids (SE1).

## Cross-cutting structural themes (fix once, retire many)

Ordered by leverage. Each collapses several Major/Minor findings.

1. **No composition root in the binary (A1, A2, A3, TO1).** The
   session-setup pipeline (log resume/create, prompt freeze, sub-agent
   seed, repair + re-linearize, model/tool/precedence assembly) is
   duplicated ~120 lines between print and interactive mode, and **four
   times within** `interactive.rs` alone — already drifting (one path
   skips the re-linearize-after-repair step). The CLI>env>config
   precedence overlay (5 sites) and the disabled-tools filter (3 sites)
   are sub-cases. A single `SessionSetup`/startup-inputs seam retires all
   of these and fixes the latent drift bugs.

2. **Sibling duplication that wants a shared abstraction.** OAuth
   anthropic/openai modules are ~80% byte-identical (M5); OpenAI
   `classify_client_error` is copied 3× (M4); `SelectList` vs
   `SettingsList` are drifted re-implementations (T4) and the binary's
   selectors (model/session/command-palette/prompt-history) hand-copy a
   filterable-overlay-list skeleton plus a background-scan machinery while
   thinking/auth use the cleaner `SelectList` callbacks for the identical
   job (A4).

3. **`anyhow` in library crates + split error types.** `anyhow` reaches
   into `aj-models::refresh`, the `aj-agent` public tool trait (so every
   tool inherits it), `aj-session::repair`, and the non-streaming SDK
   methods — which also return `anyhow` while their streaming siblings
   return structured `thiserror` errors (S1, S2). Standardize on
   structured errors at every library boundary; `anyhow` stays only in the
   `aj` top level.

4. **Dead / half-wired surface.** The `emit_update` feature is wired
   across three layers but the runtime impl is a permanent no-op, leaving
   a dead event variant (AG1/AG2), bash self-throttling (TO2), and a fully
   implemented UI handler (A3/A4) for an event that never fires. Plus:
   never-emitted events `TurnEnd`/`QueueUpdate` and an always-empty
   `AgentEnd.messages` (AG1); six dead `Terminal` trait methods (T1);
   empty doc-only `persistence.rs` and `keys.rs` modules (A2/A3); a
   stubbed-but-documented `@file` expansion (A1/A2); dead config schema
   (A1). Either finish wiring or delete — each is small.

5. **Test doubles shipped in the production public API (M5, TO1).**
   `aj-models::scripted` (with a `panic!` arm) and `aj-tools::testing`
   (`DummyToolContext`) are ungated `pub mod`s. Combined with theme 7,
   this argues for extracting a dev-dependency test-support crate.

6. **Wall-clock / real-env coupling without an injection seam (M2, C1,
   M5, SE1, T5, A5).** `Utc::now()`/cwd/`HOME`/real-FS reads sit on pure
   transform, config, and persistence paths with no seam, so tests can't
   isolate them; some tests use real `thread::sleep` and assert emission
   counts (CI-flaky). A small clock/env abstraction improves determinism
   broadly.

7. **Untested complex seams + harness reuse (A3, A5, M3, M4, C1).** The
   binary's ~1150-line interactive `run` loop has **no** test (the one
   integration test drives the Agent directly and never enters `run`); the
   provider roundtrip suites are happy-path-only (no truncation/error
   frames — exactly where the P0 truncation bug hides); replay-parity
   structurally can't catch the dropped-timestamp bug because it compares
   two equally-lossy streams. The `aj-tui` `VirtualTerminal` harness is the
   best test infrastructure in the repo but is trapped in `tests/support`;
   extracting an `aj-tui-testkit` dev-dep crate would unblock a
   real-compositor parity test and the missing `run` test, and give themes
   5/6 a home.

8. **Comment chronology (every crate).** Pervasive "previously / used to /
   replaces the old / see `docs/aj-next-plan §N` / future PR" framing, and
   a few comments that are now actively wrong. Mechanical cleanup; comments
   should stand on their own.

9. **Lower-leverage consistency nits.** Three `truncate` impls with
   different (and individually correct) contracts but a misleadingly shared
   name (M2/TO1/T2); the "tab = 3 spaces" constant duplicated across three
   `aj-tui` files (T2/T4); `async-stream` declared-but-unused in
   `openai-sdk` and `aj-models`; a keybinding vocabulary mismatch where
   `cmd`/`meta` format but never parse (T3).

## Patterns worth preserving

Call these out so refactors don't regress them, and replicate them where
the gaps above live:

- `aj-tui` renderer: single reused frame buffer, byte-identical-row
  skipping, terminal restored on panic via atomics + panic hook + Drop
  (T1). The model for the perf/teardown themes.
- `aj-tui` test suite: a real VT100 virtual terminal asserting on rendered
  viewport, not internals, with strong edge/regression coverage (T5). The
  model for the happy-path-only gaps.
- The `Provider` trait (clean two-method seam, uniform terminal contract),
  `AgentMessage` (lossless untagged wire wrapper, legacy-tolerant), and
  `ToolDetails` (UI-neutral structured data the frontend renders by variant
  match, not string re-parsing) (M1, AG2, A4).
- The model registry/refresh (offline-first, atomic, well-tested) and the
  RFC-7636-correct PKCE implementation (M1, M5).

## Per-crate health snapshot

| Crate | C | Ma | Mi | N | Notes |
|---|---|---|---|---|---|
| anthropic-sdk | 0 | 1 | 4 | 3 | thin, clean; error-type split |
| openai-sdk | 0 | 1 | 4 | 2 | error-type split doubled; unused dep |
| aj-models | 0 | 6 | 22 | 16 | truncation classification; OAuth/err dup; secrets leak |
| aj-conf | 0 | 2 | 5 | 3 | one big file; non-atomic save |
| aj-agent | 1 | 4 | 8 | 5 | the `aj-conf` edge + edition; trusts terminal; dead events |
| aj-session | 0 | 3 | 5 | 3 | no fsync; no single-writer guard; timestamp loss |
| aj-tools | 0 | 2 | 11 | 4 | non-atomic writes; tool-desc/behavior drift |
| aj-tui | 0 | 5 | 24 | 18 | strongest crate overall; markdown recursion; list drift |
| aj | 0 | 12 | 23 | 9 | no composition root; mutex freeze; untested run loop |

## Suggested next steps

This audit is read-only; nothing above has been changed. To act on it:

1. Triage the **P0/P1 runtime risks** into fix tasks first — they are
   small, localized, and user-visible.
2. Decide the **`aj-agent → aj-conf`** question (move types vs. update the
   graph) before other `aj-agent` work builds on the current shape.
3. Schedule the **composition-root** and **shared-selector/list**
   refactors (themes 1–2) — highest finding-count leverage — and pair them
   with the **test-support crate** extraction (theme 7) so the new seams
   land with tests.
4. Sweep the mechanical themes (dead surface, chronology comments, unused
   deps, edition) opportunistically alongside the above.

Each finding in the per-unit reports carries a `path:line`, a suggested
action, and an effort estimate, so they can be promoted into tasks
directly.
