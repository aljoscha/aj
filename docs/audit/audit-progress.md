# Module audit progress

Tracking file for the audit specified in `docs/audit/audit-plan.md`.
This is the file to point a session at:

> follow `@docs/audit/audit-progress.md` and do the next step

## How to do the next step

1. Read `docs/audit/audit-plan.md` for the rubric, severity taxonomy, and
   report template. (You only need to internalize it once per session.)
2. In the tables below, find the first step with status `TODO` and start it.
3. Record the commit under audit: `git rev-parse --short HEAD`.
4. **Spawn one read-only sub-agent** for the step. Its task must:
   - tell it to read `docs/audit/audit-plan.md` for the rubric and the
     `_TEMPLATE.md` format;
   - list the exact files in the step's scope (see the plan), including
     their tests;
   - have it **write** its report to `docs/audit/findings/<unit>.md` from
     the template, and **return** a short summary plus severity counts;
   - remind it the audit is **read-only**: no edits to source or tests.
5. Read the written findings file; sanity-check that it's specific,
   classified, and actionable.
6. Update this file: set the step's status to `Done`, fill in the
   severity counts, the findings link, the audited commit, and the date.
   Add anything reusable to "Cross-cutting themes".
7. Commit: `docs(audit): <unit> findings`.
8. Stop after one step (incremental mode), unless explicitly driving
   multiple steps in one session.

The final step **X1** is done by the main agent directly (no sub-agent):
it reads every findings report and synthesizes a top-level summary.

## Status legend

- `TODO` — not started.
- `WIP` — sub-agent dispatched / report being finalized.
- `Done` — findings written, counts recorded, committed.

Severity columns: **C**ritical / **Ma**jor / **Mi**nor / **N**it.

## Phase S — provider SDK clients

| Step | Unit | Status | C | Ma | Mi | N | Findings | Commit |
|---|---|---|---|---|---|---|---|---|
| S1 | anthropic-sdk | Done | 0 | 1 | 4 | 3 | [anthropic-sdk](findings/anthropic-sdk.md) | adfcaca |
| S2 | openai-sdk | Done | 0 | 1 | 4 | 2 | [openai-sdk](findings/openai-sdk.md) | 5d43f02 |

## Phase M — `aj-models` (wire layer)

| Step | Unit | Status | C | Ma | Mi | N | Findings | Commit |
|---|---|---|---|---|---|---|---|---|
| M1 | models-core | Done | 0 | 1 | 4 | 3 | [aj-models-core](findings/aj-models-core.md) | b415d89 |
| M2 | models-streaming | Done | 0 | 0 | 4 | 3 | [aj-models-streaming](findings/aj-models-streaming.md) | 867a6df |
| M3 | models-anthropic | Done | 0 | 1 | 4 | 3 | [aj-models-anthropic](findings/aj-models-anthropic.md) | b440134 |
| M4 | models-openai | Done | 0 | 2 | 5 | 3 | [aj-models-openai](findings/aj-models-openai.md) | d93f242 |
| M5 | models-auth | Done | 0 | 2 | 5 | 4 | [aj-models-auth](findings/aj-models-auth.md) | cf14db6 |

## Phase C — `aj-conf`

| Step | Unit | Status | C | Ma | Mi | N | Findings | Commit |
|---|---|---|---|---|---|---|---|---|
| C1 | conf | TODO | – | – | – | – | – | – |

## Phase AG — `aj-agent` (runtime + contracts)

| Step | Unit | Status | C | Ma | Mi | N | Findings | Commit |
|---|---|---|---|---|---|---|---|---|
| AG1 | agent-runtime | TODO | – | – | – | – | – | – |
| AG2 | agent-contracts | TODO | – | – | – | – | – | – |

## Phase SE — `aj-session`

| Step | Unit | Status | C | Ma | Mi | N | Findings | Commit |
|---|---|---|---|---|---|---|---|---|
| SE1 | session | TODO | – | – | – | – | – | – |

## Phase TO — `aj-tools`

| Step | Unit | Status | C | Ma | Mi | N | Findings | Commit |
|---|---|---|---|---|---|---|---|---|
| TO1 | tools-framework | TODO | – | – | – | – | – | – |
| TO2 | tools-impls | TODO | – | – | – | – | – | – |

## Phase T — `aj-tui`

| Step | Unit | Status | C | Ma | Mi | N | Findings | Commit |
|---|---|---|---|---|---|---|---|---|
| T1 | tui-core | TODO | – | – | – | – | – | – |
| T2 | tui-text | TODO | – | – | – | – | – | – |
| T3 | tui-editor | TODO | – | – | – | – | – | – |
| T4 | tui-components | TODO | – | – | – | – | – | – |
| T5 | tui-tests | TODO | – | – | – | – | – | – |

## Phase A — `aj` (binary)

| Step | Unit | Status | C | Ma | Mi | N | Findings | Commit |
|---|---|---|---|---|---|---|---|---|
| A1 | aj-cli | TODO | – | – | – | – | – | – |
| A2 | aj-core | TODO | – | – | – | – | – | – |
| A3 | aj-interactive | TODO | – | – | – | – | – | – |
| A4 | aj-components | TODO | – | – | – | – | – | – |
| A5 | aj-tests | TODO | – | – | – | – | – | – |

## Phase X — synthesis

| Step | Unit | Status | C | Ma | Mi | N | Findings | Commit |
|---|---|---|---|---|---|---|---|---|
| X1 | cross-crate synthesis | TODO | – | – | – | – | – | – |

## Cross-cutting themes

Recurring observations collected as steps complete; consumed by X1.

- **Error-type consistency at SDK boundaries** (S1): non-streaming
  `messages()` returns `anyhow::Error` while the streaming path returns
  structured `ClientError`. Check both SDKs standardize on a structured
  error (status + retry-after) and avoid `anyhow` in lib crates.
- **Dead/unused public surface on "thin" clients** (S1): SDKs expose more
  than `aj-models` consumes (orphaned conversions, setters,
  never-constructed error variants). Set one surface policy and apply it.
- **`thiserror` vs. hand-written `Display`** (S1): `ApiError` hand-rolls
  `Display` while deriving `Error`; pick one convention across crates.
- **Where wire-correctness is asserted** (S1, S2): SDKs lean on
  `aj-models/tests/roundtrip/` rather than testing their own request/error
  mapping. Decide where the HTTP boundary is covered. Confirmed for both.
- **Within-crate duplication of error/SSE mapping** (S2): openai-sdk's two
  streaming methods duplicate non-2xx + `Retry-After` + SSE-parse logic
  verbatim. Check whether aj-models adapters (M3/M4) repeat this.
- **Unused declared dependency** (S2, M1): `async-stream` declared but
  unused in openai-sdk and aj-models. Sweep all crates for unused deps.
- **Duplicated type across the aj-models/aj-agent boundary** (M1):
  `tools::Tool` duplicates `types::ToolDefinition`; `aj-agent` round-trips
  `ErasedToolDefinition → Tool → ToolDefinition`. Revisit in AG2.
- **`anyhow` reaches into lib crates** (S1, S2, M1): confirmed again in
  `aj-models/src/refresh.rs`. Track all lib-crate `anyhow` use.
- **Streaming hot-path cost** (M2): partial-JSON tool-arg parsing is
  O(n²) (each delta reparses the cumulative buffer) plus per-event deep
  clones. Watch the adapters (M3/M4) for similar.
- **Non-determinism from wall-clock in lib code** (M2): `transform.rs`
  stamps synthetic orphan tool-results with `Utc::now()`. Sweep for
  `Utc::now()`/`Instant::now()` in pure transform/persistence paths.
- **Stream end without terminal event → false success** (M3, M4):
  **CONFIRMED across all four providers** (Anthropic, OpenAI completions,
  Responses, Codex). A stream closing without a terminal frame finalizes
  as `Done/Stop`, so a truncated turn looks complete and is never retried.
  This is the single most impactful cross-cutting finding so far.
- **Duplicated error mapping across endpoints** (S2, M4): `classify_client_error`
  duplicated byte-for-byte across OpenAI completions/responses and a third
  time in Codex. SSE framing is correctly delegated to the SDKs (refuted).
- **Divergent terminal-error handling between sibling providers** (M4):
  Codex turns terminal `response.failed`/`error` into a hard `Err`
  (dropping partial content+usage) while Responses preserves the partial.
- **Secrets: token body leaks into error strings** (M5): on a 2xx OAuth
  token response that fails to deserialize, the raw body (live access/
  refresh tokens) is folded into `OAuthError::Parse` → logs/terminal.
  Sweep all `format!`s over HTTP bodies on auth paths.
- **Sibling OAuth duplication** (M5): anthropic/openai OAuth modules are
  ~80% byte-identical (callback server, parsing, mock harness) with no
  shared seam — widest duplication locus so far. Pairs with M4 error-map
  duplication.
- **Happy-path-only roundtrip coverage** (M3, M4): confirmed across all
  provider roundtrip suites; the truncation Major lives in an untested path.
- **Test-only `pub` items widen the surface** (M3, M4): widest locus is
  M4 (10 items). Recurs with M1/M2 surface findings.

Note: all S1 themes were confirmed in S2 (error split doubled, broader
dead surface, identical `ApiError` Display pattern).

## Audit log

One line per completed step (most recent last).

- 2026-06-02 · S1 anthropic-sdk · 0C/1Ma/4Mi/3N · adfcaca
- 2026-06-02 · S2 openai-sdk · 0C/1Ma/4Mi/2N · 5d43f02
- 2026-06-02 · M1 models-core · 0C/1Ma/4Mi/3N · b415d89
- 2026-06-02 · M2 models-streaming · 0C/0Ma/4Mi/3N · 867a6df
- 2026-06-02 · M3 models-anthropic · 0C/1Ma/4Mi/3N · b440134
- 2026-06-02 · M4 models-openai · 0C/2Ma/5Mi/3N · d93f242
- 2026-06-02 · M5 models-auth · 0C/2Ma/5Mi/4N · cf14db6
