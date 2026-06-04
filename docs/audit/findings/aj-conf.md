# Audit findings — aj-conf

- **Step:** C1
- **Date:** 2026-06-02
- **Audited commit:** 5a9eec6
- **Scope:** `src/aj-conf/src/lib.rs` (incl. its in-module `#[cfg(test)]`
  suite), `src/aj-conf/Cargo.toml`.

## Summary

`aj-conf` is a single 1816-line `lib.rs` that does three loosely-related
jobs: (1) the `config.toml` schema + lenient parser + comment-preserving
`toml_edit` writer, (2) path helpers under `~/.aj/` (config dir, sessions
dirs, dotenv *path*), and (3) `AgentEnv` — working-dir / git-root / OS /
date discovery plus user- and project-level `AGENTS.md`/`CLAUDE.md` context
loading. The config-schema half is genuinely excellent: a single
`Config::OPTIONS` source of truth that the parser, the unknown-key
suggester, and the settings writer all walk; a lenient per-field parser
that drops only the offending key and reports a typed `ConfigDiagnostic`;
and a round-trip-tested writer that preserves comments and drops
at-default keys. Error handling is clean `thiserror` with no `anyhow`.
Secret hygiene is clean — the crate only ever resolves the `.env` *path*
and never reads or persists API keys (those live in `.env`, loaded by the
binary). The notable issues are structural/contractual rather than
correctness bugs: the file is a three-responsibility grab-bag that wants
splitting; `Config::save` writes non-atomically (a crash mid-write can
truncate the user's config); the precedence chain advertised in the
`Config` doc (CLI > env > config) is *not* implemented here (the merge
lives in the `aj` binary) so the doc overstates the crate's contract; and
the `AgentEnv` half couples its tests to the real filesystem, cwd, `HOME`,
and wall clock.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 2 | 5 | 3 |

## Findings

### [Major][Boundaries] One 1816-line `lib.rs` carries three unrelated responsibilities (config schema, path helpers, agent-env/context loading) — `src/aj-conf/src/lib.rs:1`
**What:** The crate's single module mixes three cohesive-on-their-own but
mutually-unrelated concerns: the config schema/parse/save machinery
(`ConfigThinkingLevel`, `ConfigThinkingDisplay`, `ConfigSpeed`, `Config`,
`ConfigOption`, `ValueKind`, `ConfigDiagnostic`, `parse_config`,
`suggest_key`, `apply_into_document`); the `~/.aj/` path helpers
(`get_config_dir`, `config_file_path`, `get_dotenv_file_path`,
`get_sessions_dir_path`, `get_sessions_base_dir_path`, `path_to_dir_name`,
`find_git_root`, `display_path`); and `AgentEnv` + `ContextFile` +
`ContextFileKind` + the two `*_AGENTS_MD_PREFIX` consts, which discover the
runtime environment and load `AGENTS.md`/`CLAUDE.md`. These share almost no
state — `AgentEnv` doesn't touch `Config`, the schema doesn't touch
context files — yet they're interleaved in one file (note `display_path`
sits between `AgentEnv::load_project_instructions` and the `ConfigError`
enum, and the path helpers are split between `impl Config` and free
functions at the bottom).
**Why it matters:** The rubric calls out grab-bag modules and the audit
plan itself frames this step as "is a single 1800-line lib.rs cohesive or
should it be split into modules (config schema, path helpers,
loading/merging)?" The interleaving makes navigation hard, hides that
`AgentEnv`/context loading is arguably a different concern from "load
`config.toml`," and obscures the public surface.
**Suggested action:** Split into `schema.rs` (the `Config*` types,
`ConfigOption`, `ValueKind`, parser, suggester, writer + their tests),
`paths.rs` (the `~/.aj/` resolvers, `find_git_root`, `path_to_dir_name`,
`display_path`), and `env.rs` (`AgentEnv`, `ContextFile`,
`ContextFileKind`, the prefixes). Re-export from `lib.rs` to keep the
public path stable. Decide the exact cut with the user.
**Effort:** M

### [Major][Contracts] `Config::save` writes the file non-atomically — a crash or full disk mid-write can truncate the user's `config.toml` — `src/aj-conf/src/lib.rs:927`
**What:** `save` reads the existing file, applies edits into a
`toml_edit::DocumentMut`, then writes with `fs::write(&path,
doc.to_string())`. `fs::write` truncates-then-writes in place, so an
interruption (power loss, OOM kill, ENOSPC after truncate) leaves a
truncated or empty `config.toml`. The function goes to real lengths to
*not* clobber an invalid file (the `ConfigError::Update` guard) — that
care is undermined by the in-place overwrite of the valid case. The audit
already documented the correct pattern in this same workspace:
`auth.rs:547-574` writes `auth.json` 0600 and the auth layer uses careful
write discipline; the standard fix here is write-to-temp-then-`rename`,
which is atomic on the same filesystem.
**Why it matters:** This is the one persistence write in the crate and it
targets the user's hand-editable config (with their comments preserved —
the very thing the comment-round-trip is designed to protect). A truncated
write is data loss of user content. The rubric flags data-loss risk as a
top-severity concern; it's Major rather than Critical only because the
window is narrow and the file is small.
**Suggested action:** Write `doc.to_string()` to a sibling temp file
(`config.toml.tmp` or a `.NNN` suffix) in the same directory, then
`fs::rename` over the target. Optionally `fsync` before rename. Add a test
asserting an interrupted/failed write leaves the prior file intact (or at
least that the temp path is used).
**Effort:** S

### [Minor][Contracts] `Config` doc advertises the full CLI > env > config precedence chain, but this crate only implements the config-file layer — `src/aj-conf/src/lib.rs:561-565`
**What:** The `Config` doc says "The precedence order (highest to lowest)
is: CLI flags > env vars > config file." But `aj-conf` only loads and
writes the *file* layer; nothing in the crate consults CLI args or env
vars (it reads `HOME` for path resolution, never `MODEL_API`/`MODEL_URL`/
`MODEL_NAME`). The CLI/env merge that realizes that precedence lives in the
`aj` binary (`cli/args.rs`, `model.rs`). So the doc describes a contract
the crate doesn't own.
**Why it matters:** A reader of `aj-conf` looking for where precedence is
enforced will find nothing and may assume `Config::load` already applies
env overrides (it doesn't). Per the audit focus, precedence is "implemented
clearly" — but *not in this crate*; the doc should say so to keep the
boundary honest. This is a leaky-contract-by-documentation smell, not a
behavior bug.
**Suggested action:** Reword the `Config` doc to "This struct is the
config-file layer only; the binary overlays env vars and CLI flags on top
(CLI > env > config) — see `aj::model`." Keeps the precedence note as
orientation while making clear the merge isn't here.
**Effort:** S

### [Minor][Testing] `AgentEnv` tests read the real cwd, `HOME`, on-disk files, and wall clock — coupling the suite to the host environment — `src/aj-conf/src/lib.rs:1114,1585`
**What:** `test_agent_env_creation` and `test_agent_env_display` call
`AgentEnv::new()`, which reads `env::current_dir()`, walks for `.git`,
reads `env::consts::OS`, reads `chrono::Utc::now()`, *and* loads
`~/.agents/AGENTS.md` / `~/.claude/CLAUDE.md` plus `./AGENTS.md` from the
real filesystem. The assertions are deliberately weak (non-empty strings,
substring presence) to tolerate this, but the tests still depend on a real
`HOME`, a real cwd, and whatever instruction files happen to exist on the
runner — and `today_date` is wall-clock-derived. `AgentEnv::new()` is not
parameterized on its environment, so it can't be driven hermetically.
Contrast the *config* tests, which parse in-memory strings, and the
project-instruction tests, which correctly use per-test tempdirs
(`make_temp_dir`).
**Why it matters:** The recurring wall-clock / real-FS / real-env coupling
theme. The weak assertions mask the coupling rather than removing it; a
runner with an unusual `HOME` or a stray `AGENTS.md` could change what
`new()` loads, and `today_date` is untestable. The seam exists for project
instructions (`load_project_instructions(&dir)` takes a dir) but not for
the rest of `AgentEnv`.
**Suggested action:** Extract an `AgentEnv` constructor that takes
explicit `working_directory`/`home`/`today_date` (or a small
`EnvSources` struct) so the discovery logic can be tested hermetically;
keep `new()` as the real-env wrapper. At minimum, document that the two
`AgentEnv::new()` tests are smoke tests that touch the host environment.
**Effort:** M

### [Minor][Simplicity] `HOME` is read and `PathBuf`-wrapped in four places with three slightly different fallback behaviors — `src/aj-conf/src/lib.rs:230,277,875,1083`
**What:** `env::var("HOME")` appears in `load_user_instructions`
(`?`-on-absent → returns `None`), `display_path` (absent → returns the
path unchanged), `get_config_dir` (absent → `ConfigError::HomeNotFound`),
and `path_to_dir_name` (absent → falls back to the path's last component).
Each re-wraps into a `PathBuf` and each invents its own absent-`HOME`
policy. There's no single `home_dir()` helper, so the "what does a missing
`HOME` mean here" decision is scattered and inconsistent.
**Why it matters:** Sibling/within-file duplication of an environment read,
with three divergent fallbacks, is exactly the kind of convention-enforced
invariant the rubric flags. A future change to how `HOME` is resolved
(e.g. honoring `USERPROFILE` on Windows, or `directories`-style lookup)
has to be made in four spots with four different error shapes.
**Suggested action:** Add one private `fn home_dir() -> Option<PathBuf>`
(or `Result<PathBuf, ConfigError>`), and have the four call sites adapt its
result to their local policy. Documents the missing-`HOME` contract once.
**Effort:** S

### [Minor][Contracts] `path_to_dir_name` can collide distinct projects onto the same sessions directory — `src/aj-conf/src/lib.rs:1081`
**What:** `path_to_dir_name` joins the post-`HOME` path components with
`-`, so `~/Dev/project` → `Dev-project`. But `~/Dev/project` and
`~/Dev-project` (and `~/Dev/sub/project` vs `~/Dev-sub/project`) both
collapse to `Dev-project`, and any project *outside* `HOME` falls back to
just `file_name()` (the last component), so `/opt/foo` and `/srv/foo`
collide on `foo`. `get_sessions_dir_path` uses this name as the
per-project sessions subdir, so two different git roots can share one
sessions directory and interleave threads. The doc describes the happy
mapping but not the collision/ambiguity.
**Why it matters:** Sessions for distinct projects silently merging is a
correctness/contract gap for the thread-history feature, and the
dash-join is lossy (not reversible). Low probability in practice but
undocumented and unguarded.
**Suggested action:** Either document the known collision/ambiguity on
`path_to_dir_name` and `get_sessions_dir_path`, or make the name
collision-resistant (e.g. append a short hash of the full path, or keep
path separators encoded). Decide with the user since it touches the
on-disk sessions layout.
**Effort:** S

### [Minor][Testing] No test covers the `Config::save` "refuse to clobber invalid TOML" guard or the `ConfigError::Update` path — `src/aj-conf/src/lib.rs:921,1698`
**What:** The save round-trip is well-tested via the `rewrite` helper, but
`rewrite` calls `apply_into_document` on an already-parsed
`DocumentMut::unwrap()` — it bypasses `Config::save`'s
`existing.parse::<DocumentMut>().map_err(ConfigError::Update)` guard
entirely. So the documented contract "refuses to clobber a `config.toml`
that isn't valid TOML, returning `ConfigError::Update`" — the whole reason
that branch exists — has no test. Neither does the "missing file treated as
empty" branch (`ErrorKind::NotFound` → `String::new()`).
**Why it matters:** The clobber-guard is the load-bearing safety property
of `save` (it's why `save` is allowed to overwrite the user's file at all),
and it's the one branch the round-trip harness can't reach. Happy-path-only
coverage on the exact error leg most likely to bite.
**Suggested action:** Add a test that feeds malformed existing TOML through
a `save`-equivalent (parse step included) and asserts `ConfigError::Update`
with the file left untouched; and one that confirms a missing file is
treated as empty. If `save` is refactored to write atomically (Major
above), test the temp-file path there too.
**Effort:** S

### [Nit][Comments] `ConfigThinkingDisplay` doc has a duplicated, half-written opening paragraph — `src/aj-conf/src/lib.rs:56-82`
**What:** The doc block opens with "How the assistant's reasoning channel
is surfaced in `config.toml`. Mirrors `aj_models::types::ThinkingDisplay`
… vocabulary here." and then *restarts* with "How much of the assistant's
reasoning channel to surface to the user. A single knob…". Two competing
lead sentences, the first of which references matching "the Anthropic
SDK's `display` knob" — reading like a leftover from an edit rather than
one coherent doc.
**Why it matters:** Reads as accidental duplication; the rubric flags
fluff/garbled comments. Minor, but it's the type's primary doc.
**Suggested action:** Keep one opening sentence (the "single knob that
fans out to both provider-specific wire fields" framing plus the table is
the good part) and drop the redundant first paragraph.
**Effort:** S

### [Nit][Style] Unicode escape leaked into a doc comment (`\u2014` literal) — `src/aj-conf/src/lib.rs:988`
**What:** The `get_sessions_base_dir_path` doc contains the literal text
`\u2014` ("…resolve or descend into a per-project directory \u2014 it just
resolves…") instead of an em dash. It's the escape sequence as source
characters, not a rendered dash, so rustdoc shows `\u2014` verbatim.
**Why it matters:** Cosmetic doc defect; renders as gibberish in generated
docs.
**Suggested action:** Replace `\u2014` with a literal `—` (the rest of the
file uses real em dashes).
**Effort:** S

### [Nit][Simplicity] `ConfigThinkingLevel` / `ConfigThinkingDisplay` / `ConfigSpeed` each repeat a near-identical `Display` + `FromStr` + serde-derive triple — `src/aj-conf/src/lib.rs:14,83,655`
**What:** All three config enums derive `Deserialize` with
`rename_all`, then hand-write a `Display` matching each variant to its
lowercase name, then hand-write a case-insensitive `FromStr` with a
"expected a, b, c" error. The three are structurally identical (the
`Enum(&[...])` slices in `OPTIONS` even re-list the same variant strings a
fourth time). It's the within-file duplication theme at small scale, plus
the variant-name list now lives in four places per enum (derive, Display,
FromStr, OPTIONS slice) with only the `test_options_table_*` tests guarding
drift.
**Why it matters:** Minor, but adding a thinking level means editing four
spots; a macro or a small `strum`-style derive would collapse the triple
and keep the variant↔string mapping in one place.
**Suggested action:** Consider a declarative macro generating
`Display`/`FromStr`/the variant slice from one variant list, or adopt a
derive that does it. Low priority — the drift tests partly compensate.
**Effort:** M

## What's good

- **Single source of truth for the schema (`OPTIONS`, lines 693-826).**
  The parser (`parse_config`), the unknown-key suggester (`suggest_key`),
  the settings display (`ConfigOption::display`), and the writer
  (`apply_into_document`) all walk one table. The three `fn` pointers
  (`apply_toml_fn`/`display_fn`/`to_toml_fn`) are deliberately private with
  public `apply_toml`/`display`/`to_toml` wrappers, so the schema is the
  only place that names `Config` fields — a clean, well-documented seam.
  `test_options_table_matches_config_fields` and
  `test_options_table_has_no_duplicates` guard drift.
- **Lenient, typed diagnostics (`ConfigDiagnostic`, `parse_config`).**
  Per-field leniency is the right call: a bad `thinking` value drops only
  that key and reports `InvalidValue` while the rest of the file applies;
  wholesale fallback happens only on genuine TOML syntax errors
  (`ParseFailed`). Severities are sensibly split (read/parse failure =
  `Error`, ignored key/value = `Warning`), and `Display` passes the TOML
  crate's caret-pointed error through verbatim. The edit-distance
  unknown-key suggester ("did you mean `theme`?") is a nice touch with a
  documented, tested threshold.
- **Comment-preserving writer with default-elision (`save`,
  `apply_into_document`).** The `toml_edit` round-trip preserves user
  comments/ordering, updates owned keys in place, and *drops* keys reverted
  to default so a config never accumulates redundant at-default lines. The
  `to_toml` helpers (`opt_value_item`, `string_list_item`, `bool_item`)
  encode the "emit only when non-default" rule cleanly. The
  `apply_into_document` factoring lets the whole round-trip be tested
  without touching `~/.aj` — and it is, thoroughly (set/add/remove/full
  round-trip).
- **Error discipline (`ConfigError`).** Proper `thiserror` enum with
  `#[from] std::io::Error`, a dedicated `HomeNotFound`, and an `Update`
  variant carrying the verbatim parse error so the caller can show the
  user why a save was refused. No `anyhow` anywhere — the
  counterexample-to-`refresh.rs` theme holds here too.
- **Secret hygiene (clean).** The crate never reads, stores, logs, or
  prints API keys. `get_dotenv_file_path` resolves the `.env` *path* only;
  the binary owns loading it. This matches `CLAUDE.md`'s "secrets live in
  `.env`" split exactly — confirms, does not refute, the audit focus.
- **Graceful degradation contract.** `Config::load` is documented to always
  return a `Config`: missing `$HOME` or un-creatable `~/.aj` degrade to
  defaults + no diagnostics (other subsystems surface their own errors),
  and the doc says so. The `Default for Config` impl and field docs make
  the unset-means-provider-default behavior explicit.

## Boundary & architecture notes

`aj-conf` sits correctly at the bottom of the graph: its only dependencies
are `chrono`, `serde`, `strsim`, `thiserror`, `toml`, `toml_edit`,
`tracing` — no `aj_*` edges, all from `[workspace.dependencies]`, all used.
Dependency hygiene is clean (the `chrono` `serde` feature is enabled but
`chrono` is used only for `Utc::now().format(...)` in `AgentEnv`; the
`serde` feature may be unnecessary — worth a quick check, low priority).

Public surface is broad but mostly justified by the schema-as-data design
(`Config`, `ConfigOption`, `ValueKind`, `ConfigDiagnostic`, `Severity` are
all consumed by the `aj` settings command). Two things for synthesis: (1)
the precedence-merge contract advertised on `Config` is implemented in the
`aj` binary, not here (Minor finding) — verify in X1 that `aj::model`
overlays env/CLI correctly on top of `Config::load`; (2) `AgentEnv` /
`ContextFile` / context loading is arguably a separate concern from config
loading and may belong in its own module or even crate boundary — flagged
as the Major split finding.

## Test assessment

The **config-schema half is exemplary**: in-module `#[cfg(test)]` per
convention, exercising the public contract through `parse_config` /
`Config::option` / `ConfigOption::{display,to_toml}` rather than internals.
Coverage spans empty/valid/unknown-key(+suggestion)/invalid-value(+wrong-type)/
syntax-failure/multiple-diagnostics, the suggester thresholds, every
`Display` branch of `ConfigDiagnostic`, and a full save↔parse round trip
plus add/update/remove/default-elision via the `rewrite` helper. Fixtures
are clean (in-memory TOML strings; `make_temp_dir` with an atomic counter
to avoid PID collisions, mirroring the `aj-models` test helper).

**Gaps:** (1) `Config::save`'s invalid-TOML clobber guard and missing-file
branch are untested because `rewrite` bypasses the parse step (Minor);
(2) the `AgentEnv::new()` smoke tests couple to real cwd/`HOME`/FS/clock
with weak assertions and no hermetic seam (Minor); (3)
`get_sessions_dir_path` / `path_to_dir_name` collision behavior is
unfixtured; (4) `display_path` correctly pins `HOME` with restore (with a
clear `// SAFETY` note about process-wide env mutation), but it's the only
test that does — the other `HOME`-reading paths aren't driven under a
controlled `HOME`. No real-network coupling.

## Cross-cutting themes to bubble up

- **Non-atomic config write (NEW for this theme on the *config* file).**
  `Config::save` truncates-in-place; the workspace already has the
  atomic-write pattern in `auth.rs`. Synthesis should sweep every
  user-file writer (`config.toml`, themes, sessions) for write-to-temp +
  rename discipline.
- **Multi-responsibility module / file-as-crate (CONFIRMED, new shape).**
  Where prior steps flagged grab-bag *functions*, here a whole crate is one
  1816-line file spanning config + paths + agent-env. The schema half is a
  model of cohesion; the file boundary isn't.
- **Wall-clock / real-env test coupling (CONFIRMED, recurring).**
  `AgentEnv::new()` reads `Utc::now()`, cwd, `HOME`, and the real FS with no
  injection seam; matches the M4/M5 `now()`-non-determinism theme, here
  with the real-filesystem variant.
- **Within-file duplication (CONFIRMED, low scale).** Four `HOME` reads
  with three fallbacks, and three enum `Display`/`FromStr`/serde triples
  with the variant-string list repeated four times per enum. Smaller than
  the M5 OAuth locus but the same shape.
- **Contract advertised but not owned (NEW).** The `Config` doc claims the
  full CLI > env > config precedence; the merge lives in the binary. Feed
  into X1's check that the real precedence wiring matches the documented
  chain.
- **`anyhow`-free lib discipline (COUNTEREXAMPLE, holds).** Clean
  `thiserror` throughout — another data point that lib crates in this
  workspace can stay `anyhow`-free.
- **Secret hygiene (CONFIRMED clean).** No key reading/persisting/logging;
  `.env` is path-only. Reinforces the `CLAUDE.md` secrets split.
