# Audit findings — aj-models-auth

- **Step:** M5
- **Date:** 2026-06-02
- **Audited commit:** cf14db6
- **Scope:** `src/aj-models/src/auth.rs`, `src/aj-models/src/oauth.rs`,
  `src/aj-models/src/oauth/anthropic.rs`, `src/aj-models/src/oauth/openai.rs`,
  `src/aj-models/src/oauth/page.rs`, `src/aj-models/src/oauth/pkce.rs`,
  `src/aj-models/src/scripted.rs`, `src/aj-models/src/scripted/demos.rs`
  (incl. their in-module `#[cfg(test)]` suites).

## Summary

This is a careful, well-documented auth layer with genuinely good
secret hygiene at rest: `auth.json` is written 0600 under a 0700 parent,
runtime overrides never touch disk, refresh is serialized across
processes with a sidecar mkdir-lock plus a re-read-after-lock guard
against double-spending the refresh token, and the OAuth client ids are
correctly treated as public (PKCE carries the security). The PKCE
implementation is RFC-7636-correct (32-byte CSPRNG verifier, S256
challenge, pinned to the Appendix-B test vector) and the two flows make
a deliberate, documented choice on `state` (Anthropic reuses the
verifier per upstream contract; OpenAI mints 16 random bytes). The one
real blemish is a **secrets-in-error-message** bug: both token-exchange
paths fold the raw token-endpoint response body — which on a 2xx carries
`access_token` + `refresh_token` — into `OAuthError::Parse`, so a
deserialize failure produces an error string containing live tokens that
the binary will log/print (Major). Beyond that the two OAuth provider
modules are ~80% byte-identical (callback server, request-head reader,
`write_response`, `parse_callback_request`, `parse_authorization_input`,
`ParsedAuth`, the `MockTokenServer` test harness) with no shared seam —
the largest sibling-duplication locus the audit has seen. `ScriptedProvider`
is a clean, faithful `Provider` test double, but it ships in the crate's
non-gated public surface alongside production code. Boundaries otherwise
hold.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 2 | 5 | 4 |

## Findings

### [Major][Misc] Token-endpoint response body (containing live `access_token`/`refresh_token`) is embedded in `OAuthError::Parse` — `src/aj-models/src/oauth/anthropic.rs:652`, `src/aj-models/src/oauth/openai.rs:689`
**What:** `post_token_request` reads the response body into `text`, and
on a 2xx-but-undeserializable response builds
`OAuthError::Parse(format!("token response: {e}; body={text}"))`. The
2xx body of a token exchange/refresh *is* the credential payload — it
contains `access_token` and `refresh_token` (the OpenAI access token is
a JWT, the refresh token is long-lived). So any schema drift that makes
`TokenResponse` fail to deserialize a *successful* response yields an
`OAuthError` whose `Display` string carries live secrets. `OAuthError`
flows up through `AuthError::OAuth` to the binary, where login/refresh
failures are surfaced to the user and (per the workspace's `tracing`
usage) logged. The non-2xx arm (`OAuthError::Server { body }`,
`anthropic.rs:645` / `openai.rs:682`) has the same shape but is lower
risk because an error body normally carries `{"error": ...}`, not
tokens; the `Parse` arm is the dangerous one because it only fires on a
*successful* token-bearing body.
**Why it matters:** The rubric treats a secret that can reach a log or
stdout as Critical and one that could be persisted/printed as at least
Major. This is a token that lands in an error `Display` the host renders
and logs — a plaintext credential leak into logs/terminal on a plausible
(if uncommon) upstream-change path. It also defeats the otherwise-careful
at-rest hygiene (0600 file, no-disk overrides).
**Suggested action:** Drop the raw body from the `Parse` message. Either
log only the serde error and a redacted/length summary
(`"token response failed to parse: {e} ({n} bytes)"`), or keep the body
behind a redaction helper that strips `access_token`/`refresh_token`/
`id_token` values. Apply the same treatment to `Server { body }` if the
upstream can ever echo credentials there. Add a unit test asserting the
error string for a token-bearing body does not contain the token.
**Effort:** S

### [Major][Simplicity] The Anthropic and OpenAI OAuth modules are ~80% duplicated with no shared seam — `src/aj-models/src/oauth/anthropic.rs:226-476`, `src/aj-models/src/oauth/openai.rs:267-502`
**What:** `await_callback`, `handle_callback_connection`,
`read_http_request_head`, `find_subsequence`, `write_response`,
`CallbackParams`, `parse_callback_request`, `parse_authorization_input`,
`ParsedAuth`, `post_token_request`'s structure, `now_unix_ms`, the
endpoint-constant block, the `with_token_url` client builder, and the
entire `MockTokenServer` test harness are near-identical across the two
provider modules. The genuinely provider-specific deltas are small and
enumerable: client id / URLs / scopes / callback path-and-port, JSON vs.
form-encoded token bodies, Anthropic's verifier-as-state vs. OpenAI's
random state, and OpenAI's JWT `account_id` extraction. Everything else
is copy-paste. The module docs already frame the OpenAI flow as "the
Anthropic flow, with a few quirks," which is exactly the shape that wants
a shared core. `oauth.rs` and `page.rs` show the seam is achievable — the
trait, credentials, and callback-page chrome are already shared; the
callback *server* and input parsing are not.
**Why it matters:** This is the widest sibling-duplication locus in the
audit so far (it dwarfs the OpenAI `classify_client_error` triple flagged
in M4). Two copies of the localhost callback server, the request-head
reader, the state-mismatch handling, and the four-shape paste parser
drift independently — a security-relevant fix (e.g. tightening the head
size cap, or hardening `parse_authorization_input`) has to be made twice
and can be missed in one. The `MockTokenServer` is duplicated a third
time across the two test modules.
**Suggested action:** Extract a shared `oauth::callback` module (the
listener loop, `read_http_request_head`, `write_response`,
`parse_callback_request`, `CallbackParams`) and a shared
`oauth::paste` module (`ParsedAuth` + `parse_authorization_input`),
parameterized by callback path. Let each provider own only its endpoint
constants, request-body encoding, state policy, and the
`token_to_credentials` step. Share `MockTokenServer` under a test-only
module. Decide the exact split with the user since it touches both
security-relevant flows.
**Effort:** M

### [Minor][Boundaries] `ScriptedProvider` and the demo catalog are in the crate's ungated public surface, shipping test scaffolding into production builds — `src/aj-models/src/lib.rs:23`, `src/aj-models/src/scripted.rs:148`
**What:** `pub mod scripted;` (and `scripted::demos`) is a first-class,
always-compiled public module of `aj-models`, exporting `ScriptedProvider`,
`ScriptBuilder`, `ProviderScript`, `ExhaustedBehavior`, and the
`demos::catalog/lookup/names` table. It's a faithful test double — but it
is not behind a `#[cfg(feature = "...")]` or `#[cfg(test)]` gate, so it
links into every release build of the binary. The module doc itself
states its audiences are "tests, demos, and TUI eyeballing." The
`--scripted` CLI flag is a real product feature, so `demos` arguably
belongs in production; `ScriptedProvider`'s `ExhaustedBehavior::Panic`
arm (`scripted.rs:220`, a `panic!` on the inference task) is purely a
test affordance that has no business in a shipped library's public API.
This is the same over-broad-test-only-surface theme M1–M4 raised, here as
a whole module rather than a handful of functions.
**Why it matters:** A `panic!`-on-misuse path and a step-by-step event
builder are advertised as supported public API of the wire crate, and
the panic arm violates the "no panic on reachable paths in lib code"
rubric line for any embedder who constructs `ScriptedProvider::new(...)
.on_exhausted(Panic)` outside a test. It also blurs which of the crate's
`Provider` impls are real.
**Suggested action:** Decide one policy with the user: feature-gate the
test-only parts (`ScriptedProvider` + `ScriptBuilder` behind
`feature = "scripted"`/`"test-support"`, enabled by the binary's
`--scripted` path and by dev-deps) while keeping the panic arm
`#[cfg(test)]`-only or documenting it as test-only; or, if `--scripted`
needs the full builder in production, keep the module but move the
`Panic` behavior behind a test gate. Track with the workspace
public-surface decision in synthesis.
**Effort:** M

### [Minor][Contracts] `get_api_key` returns a stale token for a non-expired-but-unrefreshable OAuth credential, and the env-var "OAuth token" path can't refresh — undocumented dead-ends — `src/aj-models/src/auth.rs:320-327`, `src/aj-models/src/auth.rs:467-474`
**What:** Two related contract gaps. (1) In the stored-OAuth arm, if the
credential is *not* expired the code returns `provider.get_api_key(&creds)`
without consulting expiry margin beyond the bare `is_expired_at` check —
fine — but if `lookup_oauth_provider` fails (`UnknownProvider`) the whole
call errors even when a perfectly valid, non-expired `access` token is
sitting in `creds`; a hand-edited `auth.json` with a typo'd provider id
turns a usable token into a hard error. (2) `find_env_keys("anthropic")`
prefers `ANTHROPIC_OAUTH_TOKEN`, and `openai-codex` resolves
`OPENAI_CODEX_OAUTH_TOKEN`; these are short-lived bearer tokens with *no*
refresh path, but `get_api_key` returns them verbatim (step 2) with no
expiry awareness, so an expired env OAuth token surfaces as a confusing
401 mid-request rather than a "log in again" prompt. The doc comment on
`find_env_keys` notes the codex var "on its own cannot be refreshed" but
`get_api_key`'s contract doesn't surface what happens when it's stale.
**Why it matters:** The §9.1 resolution chain is the auth boundary's core
contract; two of its branches have edge behavior (valid-token-but-bad-provider-id
hard-errors; expired-env-OAuth-token silently passed through) that isn't
documented at the API and could be enforced more helpfully.
**Suggested action:** For (1), in the OAuth arm, fall back to returning
the stored non-expired `access` directly if provider lookup fails (the
token is still usable until expiry), or document that an unknown provider
id is treated as fatal even for a fresh token. For (2), add a sentence to
`get_api_key` noting env-supplied OAuth tokens are returned without
refresh and will surface upstream 401s when stale.
**Effort:** S

### [Minor][Testing] No test covers the OAuth-refresh-failure path through `AuthStorage`, nor the manual-paste `select!` race in either flow — `src/aj-models/src/auth.rs:394-422`, `anthropic.rs:226`, `openai.rs:267`
**What:** The `auth.rs` suite covers the refresh *success* path
(`get_api_key_refreshes_expired_oauth`) and the fresh-cache path, but
there is no test where `provider.refresh_token` returns `Err` — i.e. the
`refresh_oauth_with_lock` error propagation that the doc says callers use
to "surface a re-login prompt" (`auth.rs:294`). Likewise
`obtain_code`/`obtain_code_and_state` — the `biased` `select!` racing the
callback server against `on_manual_code_input`, the state-mismatch reject
in the manual path, and the empty-manual-input drop-through to the prompt
fallback — has no test in either provider module; the callback-server
tests drive `await_callback` directly, never the racing wrapper. The M4
audit's "happy-path-only" theme recurs: the refresh/login *error and race*
legs are the ones most likely to bite and are unfixtured.
**Why it matters:** The refresh-failure contract is load-bearing for the
"log in again" UX, and the manual-code `select!` is the trickiest control
flow in the flows (state validation, listener teardown on drop, fallback
ordering). Both are untested at the boundary.
**Suggested action:** Add a stub `OAuthProvider` whose `refresh_token`
returns `Err` and assert `get_api_key` surfaces `AuthError::OAuth` while
leaving the stored (stale) credential intact. Add a `OAuthCallbacks`
double that supports manual input to drive `obtain_code` through the
state-mismatch and empty-input-then-prompt paths.
**Effort:** M

### [Minor][Contracts] The 30 s HTTP timeout means a slow token exchange can outlive the refresh margin; `REFRESH_SAFETY_MARGIN_MS` and `REQUEST_TIMEOUT_SECS` aren't related in either direction — `src/aj-models/src/oauth/anthropic.rs:84,90`, `src/aj-models/src/oauth/openai.rs:95,101`
**What:** `REFRESH_SAFETY_MARGIN_MS = 5 min` is the headroom subtracted
from `expires_in` so refresh fires before lapse, and `REQUEST_TIMEOUT_SECS
= 30` bounds each token call. These are independent constants in two
files; nothing documents that the margin must exceed the worst-case
refresh round-trip (timeout + lock-wait, where `LOCK_TIMEOUT = 30 s` in
`auth.rs:583`). In the pathological case (token expires in <5 min,
sibling holds the lock for ~30 s, then a 30 s upstream call), the refresh
can still race the actual expiry. It's a soft assumption holding the
whole proactive-refresh design together, stated nowhere.
**Why it matters:** The relationship between the three time budgets is an
invariant enforced by convention across two files, not by types or a
shared definition. A future tweak to one constant could silently break
the "always refresh before expiry" guarantee.
**Suggested action:** Add a one-line note where `REFRESH_SAFETY_MARGIN_MS`
is defined that it must comfortably exceed `LOCK_TIMEOUT +
REQUEST_TIMEOUT_SECS`, or centralize the three constants in one place
with a comment relating them. If the duplication finding above is taken,
these become shared and the relationship is documented once.
**Effort:** S

### [Minor][Errors] `OAuthCredentials::extra` `serde(flatten)` silently swallows any future top-level token field into the untyped map — `src/aj-models/src/oauth.rs:54`
**What:** `extra: HashMap<String, serde_json::Value>` is
`#[serde(flatten)]`, so on deserialize *every* unrecognized top-level key
in an `auth.json` OAuth entry lands in `extra`. That's the intended
mechanism for OpenAI's `accountId`, but it also means a typo'd or
renamed core field (e.g. a future upstream that returns `access_token`
instead of `access`) is silently captured into `extra` rather than
erroring, and `OAuthProvider::get_api_key` would then return an empty
`access` string with no diagnostic. Combined with the env/stored
resolution returning that empty string, the failure mode is an opaque
upstream 401.
**Why it matters:** The flatten-catch-all trades a loud parse error for a
silent empty-credential, which is exactly the kind of swallowed error the
rubric flags. It's a low-probability path but the contract ("extra holds
*provider-specific* fields") isn't enforced — anything unknown qualifies.
**Suggested action:** Document on the `extra` field that it absorbs all
unrecognized top-level keys (so readers know a misnamed core field won't
error), and consider an `OAuthProvider::get_api_key`/login-time assertion
that `access` is non-empty before persisting, returning
`OAuthError::Parse` otherwise.
**Effort:** S

### [Nit][Simplicity] OpenAI `generate_state` builds its hex string with a per-byte `format!` allocation — `src/aj-models/src/oauth/openai.rs:535-538`
**What:** `for byte in bytes { hex.push_str(&format!("{byte:02x}")); }`
allocates a `String` per byte. It's 16 bytes so the cost is trivial, but
`write!(&mut hex, "{byte:02x}")` (with `use std::fmt::Write`) or a
hex-encoding of the existing `base64` path avoids 16 throwaway
allocations and reads no worse.
**Why it matters:** Tiny, but it's a needless-alloc pattern in code that
already pulls in encoding crates.
**Suggested action:** Use `write!` into the pre-sized buffer, or encode
via a hex helper.
**Effort:** S

### [Nit][Comments] `find_env_keys` doc says "three providers" but the spec/code split distinguishes `openai` from `openai-codex`, which is four mappings across three ids — `src/aj-models/src/auth.rs:458-466`
**What:** The doc enumerates `anthropic` (two vars), `openai` (one var),
`openai-codex` (one var) — three provider ids, four env vars. The prose
"we cover three providers today" is accurate on id count but reads as
glossing the OAuth-token-vs-API-key distinction that the rest of the
module (and the §7.4.1 migration) works hard to keep separate. Minor
wording.
**Why it matters:** A reader skimming the doc might miss that `anthropic`
has both an OAuth-token and an API-key env var with a preference order.
**Suggested action:** Reword to "three provider ids / four variables" and
note the anthropic preference order inline (it's already in the match,
just not the prose).
**Effort:** S

### [Nit][Contracts] `read_http_request_head` truncates at `MAX_REQUEST_HEAD_BYTES` and then parses whatever it has, rather than rejecting — silently accepts a partial head — `src/aj-models/src/oauth/anthropic.rs:402-405`, `src/aj-models/src/oauth/openai.rs:431-434`
**What:** On hitting the 16 KiB cap the loop `break`s and returns the
buffer as-is; the caller then parses the (possibly truncated) request
line. For the OAuth callback this is harmless — the real callback is a
short GET and the request line comes first — but the contract "anything
larger is malformed" (the constant's doc) isn't enforced: an
over-cap request is parsed, not rejected. Defensive-totality is fine; the
comment just overstates the guarantee.
**Why it matters:** Comment/behavior mismatch on a network-facing reader;
no security impact because only the first line (already received) is
used, but a reader expecting rejection on oversize won't get it.
**Suggested action:** Either return an error / `Ok(None)` on hitting the
cap, or soften the constant's doc to "we stop reading after this; only
the request line is consulted, which always arrives first."
**Effort:** S

### [Nit][Comments] `current_unix_ms` doc claims it's "pulled out so tests can stub it in via `OAuthCredentials::is_expired_at`" but it's never stubbed and the two providers have their own `now_unix_ms` — `src/aj-models/src/auth.rs:693-698`
**What:** Three separate wall-clock readers exist: `auth::current_unix_ms`,
`anthropic::now_unix_ms`, `openai::now_unix_ms`, each
`chrono::Utc::now().timestamp_millis()`. The `auth.rs` one's doc justifies
its existence by a stubbing story, but nothing stubs it; `is_expired_at`
takes `now` as a parameter precisely so the *credential* check is pure,
which is the real testability seam. The clock readers themselves are
untestable and triplicated — the recurring wall-clock-non-determinism
theme (M4 flagged Codex's `minutes_until`), here as three undeduplicated
`now()` helpers.
**Why it matters:** Minor, but the comment's rationale doesn't match
reality and the triplication is part of the broader duplication finding.
**Suggested action:** Drop the misleading "so tests can stub it" clause
(the real seam is `is_expired_at(now)`), and fold the three `now_unix_ms`
into one shared helper when the duplication finding is addressed.
**Effort:** S

## What's good

- **At-rest secret hygiene (`auth.rs:547-574`).** `auth.json` is written
  0600 with a 0700 parent on Unix, runtime `--api-key` overrides live
  only in memory and are documented as never-persisted, and
  `has_runtime_override` deliberately exposes presence-not-value. The
  `migrate_legacy_openai_oauth` log (`auth.rs:542`) prints only a
  provider-id move, never token material — the sole `tracing` call in the
  whole scope and it's clean.
- **Cross-process refresh safety (`auth.rs:385-422`).** `refresh_oauth_with_lock`
  holds the sidecar mkdir-lock across the entire read-modify-write *and*
  re-reads under the lock so a sibling that already refreshed wins,
  avoiding double-spending the refresh token — a real concurrency hazard
  handled correctly and explained in the doc. The `FileLock` (atomic
  `mkdir`, stale-steal by mtime, exponential backoff, best-effort
  `Drop` rmdir) is a tidy, well-tested primitive with the steal threshold
  pulled out as a parameter so the test drives it without sleeping.
- **PKCE correctness (`pkce.rs`).** 32-byte OS-CSPRNG verifier, S256
  challenge, base64url-no-pad, CSPRNG failure propagated (not panicked),
  and pinned to the RFC 7636 Appendix-B test vector plus distinctness and
  helper-agreement tests. Exactly the right shape.
- **Public-client-id framing.** Both flows document the OAuth client id
  as intentionally public ("security comes from PKCE, not from hiding the
  id," `anthropic.rs:46`, `openai.rs:47`) — correct and forestalls a
  future reader treating it as a leaked secret.
- **State / CSRF handling is deliberate and tested.** Anthropic's
  verifier-as-state quirk and OpenAI's 16-byte random state are each
  documented with the upstream rationale, the state mismatch in the
  callback is non-fatal (keeps listening — handles stale browser tabs)
  while the manual-paste path treats a mismatch as fatal `StateMismatch`,
  and both behaviors have callback-server tests.
- **`page.rs` is a clean shared seam.** Pure string builders, HTML-escaped
  (with the `&#39;` legacy-browser note), empty-details collapsed, and an
  explicit XSS-escaping test. This is the model the duplicated callback
  *server* code should follow.
- **`OAuthProvider`/`OAuthCallbacks` trait design (`oauth.rs`).**
  Object-safe (compile-time-asserted), `&self` callbacks with a documented
  `Send + Sync` rationale, capability-flag (`supports_manual_code_input`)
  consulted before the race with a default-error safety net, and
  `is_expired_at(now)` taking the clock as a parameter so expiry logic is
  pure and testable.
- **`ScriptedProvider` faithfully mirrors the `Provider` contract.** It
  honors the cancellation token mid-delay and emits an `aborted` terminal
  with the captured partial, falls back to `producer.end()` if a script
  lacks a terminal event (the same transient-on-drop contract the real
  providers uphold), stamps the requested model identity onto synthesized
  messages, and `script_from_message` round-trips identity/usage/timestamp
  — a thorough, well-tested double whose only issue is where it lives
  (Minor finding), not what it does.
- **Token→credentials conversion is centralized and tested.** Both flows
  compute the absolute expiry (with margin) at parse time so callers never
  juggle relative lifetimes, and OpenAI rejects a JWT lacking
  `chatgpt_account_id` rather than persisting partial credentials — with
  a test pinning that rejection.

## Boundary & architecture notes

Dependency direction is correct: the auth/oauth/scripted modules depend
only on `reqwest`/`tokio`/`serde`/`chrono`/`base64`/`rand`/`sha2`/
`async-trait`/`thiserror`/`tracing` and on in-crate modules
(`provider`, `registry`, `streaming`, `types`) — no `aj_*` edges, so the
unit stays below `aj-agent`/`aj-session` as `CLAUDE.md` intends. The
auth layer is cleanly decoupled from any specific provider's HTTP API:
`AuthStorage` only ever sees the `OAuthProvider` trait, and the
default-registry id split (`anthropic` / `openai-codex`) plus the §7.4.1
in-memory migration keep the JWT pool and the API-key pool from
colliding — well executed.

Public-surface notes for synthesis: (1) `pub mod scripted` ships a test
double + its `panic!` arm in the production public API (Minor finding);
(2) `oauth::openai::extract_account_id` is correctly `pub(crate)` for the
Codex provider — the right visibility, noted as a positive seam. The two
OAuth provider modules are otherwise self-contained behind the shared
trait; the duplication (Major finding) is *within* the `oauth` module
tree, not a cross-layer leak.

`errors.rs`/`refresh.rs`'s `anyhow` theme does **not** recur here: both
`AuthError` and `OAuthError` are proper `thiserror` enums with
`#[from]` conversions, coarse-but-documented variants, and no `anyhow` in
the scope. This is the correct lib-crate error discipline and a useful
counterexample for the synthesis `anyhow` decision — the auth layer shows
the crate can stay `anyhow`-free where it has real `Result` boundaries.

## Test assessment

Tests are in-module under `#[cfg(test)]` per convention and largely
exercise the right boundaries:

- **auth.rs** is strong: CRUD round-trips through the public storage API,
  the §9.1 resolution-chain priority (override > env > stored), the
  OAuth refresh-and-persist path via a stub provider, a `PanickyProvider`
  proving fresh tokens don't trigger refresh, the §7.4.1 legacy-id
  migration in all four cases (migrate / preserve-api-key / skip-on-collision
  / persist-on-next-write), a real 10-way concurrent-write test through
  the file lock, and direct `try_steal_stale_lock` coverage with an
  injected age. The serde-shape tests pin the flattened on-disk layout.
- **pkce.rs** pins format, distinctness, helper-agreement, and the RFC
  test vector — exemplary.
- **page.rs** covers happy path, optional-details, empty-details collapse,
  and XSS escaping.
- **anthropic.rs / openai.rs** drive token exchange + refresh end-to-end
  against a hand-rolled `MockTokenServer` (capturing the wire body to
  assert grant_type/client_id/encoding), authorize-URL parameter pinning,
  the callback server's accept/wrong-path/state-mismatch/upstream-error
  paths, JWT account-id extraction across every malformed shape, and
  trait-dispatch smoke tests.
- **scripted.rs / demos.rs** cover chunking edge cases (zero/empty/multi-byte),
  per-inference dispatch, exhausted-EndTurn vs. exhausted-Panic, the
  cancellation-mid-delay aborted terminal, both error/aborted stop-reason
  mappings, and a catalog well-formedness test asserting every demo ends
  on a terminal event.

Gaps (see findings): no OAuth refresh-*failure* path through `AuthStorage`;
no test of the manual-paste `select!` race / state-mismatch / empty-input
fallback in either flow (the callback-server tests bypass the racing
wrapper); no test that the `Parse` error string for a token-bearing body
is redacted (it currently isn't — Major). The `MockTokenServer` busy-waits
via `captured_body` polling (50 × 10 ms) and the callback tests use a
fixed `sleep(20ms)` before connecting to let the server reach `accept` —
mild timing assumptions that could flake on a heavily loaded runner,
though the bounds are generous. No real-network or HOME-dependent coupling
in this scope (scratch paths are per-test tempdirs with an atomic counter
to avoid PID collisions).

## Cross-cutting themes to bubble up

- **Secrets in error/diagnostic strings (NEW, primary concern).** The
  token-endpoint `Parse` error embeds the raw 2xx body (live tokens). M4
  found secret handling *clean* in the Codex JWT path; this is the first
  place the workspace folds a credential into a surfaced string. Synthesis
  should sweep every `format!`/`Display` that interpolates an HTTP body or
  header on an auth path, and consider a shared redaction helper.
- **Sibling-module duplication (CONFIRMED, widest locus).** The two OAuth
  flows duplicate ~250 lines of callback server + paste parsing + test
  harness with no shared seam — larger than the M4 `classify_client_error`
  triple and the M1 `tools::Tool`/`ToolDefinition` split. The
  `shared trait + shared page, duplicated server` shape suggests a missing
  `oauth::callback`/`oauth::paste` module. One refactor decision.
- **Over-broad / test-only public surface (CONFIRMED, new shape).** Where
  M1–M4 flagged individual `pub` functions, here it's an entire ungated
  `pub mod scripted` (with a `panic!` arm) shipped into production. Same
  workspace policy question — gate test scaffolding behind a feature or
  `#[cfg(test)]`.
- **Wall-clock non-determinism (CONFIRMED, new locus).** Three
  undeduplicated `Utc::now().timestamp_millis()` helpers
  (`auth.rs`, `anthropic.rs`, `openai.rs`) plus a misleading "so tests can
  stub it" comment; the real testable seam (`is_expired_at(now)`) is good
  and worth replicating as the pattern.
- **Happy-path-only error coverage (CONFIRMED, recurring).** The
  refresh-failure and manual-paste-race legs are unfixtured, mirroring the
  M3/M4 "error legs under-tested" finding on the streaming side, now on
  the auth side.
- **`anyhow`-free lib discipline (COUNTEREXAMPLE).** Unlike `refresh.rs`
  (M1), this scope uses `thiserror` end-to-end with no `anyhow` — evidence
  the crate can shed `anyhow` if `refresh.rs` is converted. Feed into the
  synthesis `anyhow` decision.
