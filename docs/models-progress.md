# aj-models Spec Implementation Progress

Tracking file for `docs/models-spec.md` implementation. Each item maps to a
step in §12 (Implementation Plan). Use `git log` for the authoritative state;
this file is the bridge between the spec and the git history.

## Phase 1: Unified Types (aj-models)

- [x] 1. Define new type module (`aj-models::types`) — §1, §4
- [x] 1b. Backfill `StreamOptions` per §4 and small §1 additions
       (`ThinkingLevel::XHigh`, `ToolResultMessage.details`).
       AssistantError / ErrorCategory deferred to step 11.
- [x] 2. Define streaming event types (`aj-models::streaming`) — §2
- [x] 3. Define model metadata and registry (`aj-models::registry`) — §3.1–§3.3
- [x] 3b. Build the catalog refresh command (`aj models update`) — §3.4
- [x] 4. Define provider trait (`aj-models::provider`) — §5

## Phase 2: Anthropic Provider

- [x] 5. Update `anthropic-sdk` — §6.1
- [x] 6. Implement Anthropic provider (`aj-models::anthropic`) — §6.2

## Phase 3: OpenAI Provider

- [x] 7. Update `openai-sdk` — §7.1
- [x] 8. Implement OpenAI Chat Completions provider (`aj-models::openai`) — §7.2
- [x] 8b. Implement OpenAI Responses provider (`aj-models::openai`) — §7.3.
      Lives in `src/aj-models/src/openai/responses.rs` and is wired
      into `provider_for("openai-responses")`. Implements §7.3.1
      message conversion (typed input array, system/developer
      prompt, reasoning items round-tripped via
      `thinking_signature`, text signatures via `TextSignatureV1`,
      composite tool-call IDs `{call_id}|{item_id}`), §7.3.2
      request parameters (`reasoning.effort`, `include`,
      `prompt_cache_key`, `prompt_cache_retention`, `service_tier`,
      `store: false`), the §7.3 session-correlation headers
      (`session_id`, `x-client-request-id` on `api.openai.com`
      only), the §7.3.6 SSE event mapping, §7.3.7 usage parsing
      with service-tier cost multiplier, and §7.3.8
      `response.status` → `StopReason` mapping. Public round-trip
      helpers (`assistant_message_to_input_items`,
      `parse_assistant_input_items`, `replay_sse_events`) plus
      `TextSignatureV1` are exposed for the round-trip suite.
      `openai-sdk::Client` got `with_extra_header` to plumb the
      session-correlation headers without forking the streaming
      path.
- [x] 8c. Implement OpenAI Codex Responses provider
      (`aj-models::openai::codex`) — §7.4. Lands as a new module
      sharing helpers with `aj-models::openai::responses`; wires
      `api: "openai-codex-responses"` into `provider_for`; renames
      the OpenAI OAuth provider id from `"openai"` to
      `"openai-codex"` (with `auth.json` migration); adds
      `OPENAI_CODEX_OAUTH_TOKEN` env var to §9.5 mapping; seeds the
      `provider: "openai-codex"` model catalog by hand and teaches
      `aj models update` to preserve those entries (§3.4.7); adds
      Codex parse / serialize / semantic round-trip fixtures.
      WebSocket transport is explicitly out of scope (§7.4.8) — SSE
      only.

      Sub-progress:
   - [x] 8c.i. Provider-id split + env-var mapping. Renamed
         `OpenAIOAuth::id()` from `"openai"` to `"openai-codex"` per
         spec §7.4.1; added `"openai-codex" => ["OPENAI_CODEX_OAUTH_TOKEN"]`
         to `find_env_keys` per §9.5 (deliberately *not* falling back
         to `OPENAI_API_KEY` — the two credential pools target
         different endpoints and crossing them surfaces as a 401
         mid-request). `read_auth_file` gained a silent, idempotent
         in-memory migration that moves OAuth-typed entries from
         `auth.json["openai"]` to `auth.json["openai-codex"]`; the
         migration only fires if the destination slot is empty and
         leaves plain `api_key` entries under `openai` untouched
         (those are legitimate `OPENAI_API_KEY` paste-ins for the
         public API). The migrated shape persists to disk the next
         time any mutating operation (`set` / `remove` /
         `refresh_oauth_with_lock`) round-trips through
         `write_auth_file`. Five new unit tests cover the migration:
         basic rewrite, `api_key` preservation, no-clobber when both
         ids are populated, persist-to-disk round-trip, and the
         §9.5 env-var split. Existing tests (`default_registry_has_*`,
         `openai_oauth_implements_provider_metadata`) updated to
         match the renamed id.
   - [x] 8c.ii. Catalog seed + refresh preservation. Added the
         hand-curated codex seed list as `src/aj-models/data/codex.json`
         (10 entries: `gpt-5.1`, `gpt-5.1-codex-max`, `gpt-5.1-codex-mini`,
         `gpt-5.2`, `gpt-5.2-codex`, `gpt-5.3-codex`, `gpt-5.3-codex-spark`,
         `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.5`, all with the fixed
         `api: "openai-codex-responses"` / `provider: "openai-codex"` /
         `base_url: "https://chatgpt.com/backend-api"` triple per §3.4.3,
         hand-curated per-model pricing, `context_window: 272000` /
         `max_tokens: 128000` per §3.4.7 — except `gpt-5.3-codex-spark`
         which carries `context_window: 128000` and text-only input).
         `aj_models::registry` ships `bundled_codex_seed()` (parses
         `codex.json`, drops any non-codex entries with a warning to
         keep the file from accidentally injecting foreign providers),
         `splice_codex_seed(models, seed)` (additive-only merge keyed
         by `(provider, id)` — existing entries always win so a
         refreshed user cache isn't silently rewritten by the
         in-process seed), and the public `CODEX_PROVIDER_ID` constant
         so refresh/load agree on the key. `ModelRegistry::from_catalog_with_overrides`
         calls the splice *before* overrides run so authored patches
         can target codex models too. `aj_models::refresh::build_catalog_from_json`
         filters any upstream entry whose mapped provider would be
         `openai-codex` (defensive — models.dev doesn't categorize
         anything that way today, but the guard keeps the seed
         authoritative if it ever changes) and then splices the codex
         seed onto the catalog before sort/overrides, so refresh
         writes the codex entries into the user cache and subsequent
         refreshes diff cleanly. Updated three pre-existing refresh
         tests to use `bundled_codex_seed().len()` for counts and to
         look up models by id rather than positional index (since the
         codex tail shifted indices). Added five new tests:
         `bundled_codex_seed_well_formed` pins provider/api/base_url/
         max_tokens invariants and the canonical id list;
         `splice_codex_seed_is_additive_only` proves the additive
         semantics and idempotency on re-splice;
         `splice_skips_non_codex_entries` pins the splice's contract
         (filtering of foreign providers happens in
         `bundled_codex_seed`, not in the splice);
         `load_surfaces_codex_models` proves `ModelRegistry::load()`
         exposes the codex set with `gpt-5.5` correctly tagged as
         xhigh-capable; and `refresh_preserves_codex_entries_across_runs`
         proves a second refresh against the same upstream feed
         produces an empty diff (no codex entries flagged as
         "removed"). Updated `registry_lookup_and_listing` to
         account for the codex provider being spliced into the
         registry's `providers()` listing.
   - [x] 8c.iii. SDK + provider implementation. New
         [`openai-sdk::Client::codex_responses_stream`] method POSTs to
         `{base_url}/codex/responses` while sharing the request body,
         header machinery, and SSE parser with `responses_stream` (the
         existing endpoint factors through a new private
         `responses_stream_at_path` helper so the two methods differ
         only in URL path). New `aj-models::openai::codex` module
         ships [`OpenAiCodexResponsesProvider`] alongside the
         §7.3 [`OpenAiResponsesProvider`]; it implements the §7.4 wire
         differences enumerated below and reuses every other §7.3
         behaviour through `pub(super)` helpers on
         [`super::responses`].

         **Authentication (§7.4.1).** The provider treats
         [`StreamOptions::api_key`] as the OAuth JWT access token and
         decodes the [`chatgpt_account_id`] claim at request time via
         the existing OAuth helper (`oauth::openai::extract_account_id`,
         promoted from `fn` to `pub(crate)`). Headers stamped via
         `openai-sdk::Client::with_extra_header` on every request:
         `Authorization: Bearer <jwt>` (handled by `bearer_auth`),
         `chatgpt-account-id: <jwt claim>`, `originator: aj`,
         `OpenAI-Beta: responses=experimental`,
         `User-Agent: aj/<version> (<os> <arch>)`, plus the §7.3
         session-correlation headers (`session_id`,
         `x-client-request-id`) when [`StreamOptions::session_id`] is
         set. Defaults: an empty `model.base_url` falls back to
         `https://chatgpt.com/backend-api` so callers don't have to
         restate the value the registry already encodes.

         **Message conversion (§7.4.2).** Reuses
         `responses::convert_messages` (which was made `pub(super)`
         and parameterized on `api_name` so the cross-model
         `tool_call.id` rewrite in `append_assistant_message` keys off
         the correct provider). The system prompt is *not* added as an
         input item; instead it rides on the top-level
         `instructions` field per §7.4.3, with the §7.4.3 default
         `"You are a helpful assistant."` when the caller leaves
         [`Context::system_prompt`] empty.

         **Request parameters (§7.4.3).** `store: false` and
         `parallel_tool_calls: true` are hardcoded; `tool_choice` is
         always `"auto"` regardless of [`StreamOptions::tool_choice`];
         `strict` is omitted from every tool definition (new
         `to_codex_tool` helper sets `strict: None`, which the
         openai-sdk's `#[serde(skip_serializing_if = "Option::is_none")]`
         drops on the wire); `text.verbosity`, `max_output_tokens`,
         and `prompt_cache_retention` are never sent; `prompt_cache_key`
         is sourced exclusively from `session_id`.

         **Service-tier pricing (§7.4.4).** New
         `codex_cost_multiplier` function pointer (exposed via a
         `pub(super) const CODEX_COST_MULTIPLIER` to avoid an
         `fn as CostMultiplierFn` cast at the call site) replaces the
         responses-default multiplier inside the streaming state
         machine. `flex → 0.5×`, `priority → 2×` for every model
         except `gpt-5.5` where `priority → 2.5×`; the resolver
         applies the request tier when the server echoes `default`
         (today the SDK doesn't model a `default` variant on
         [`OpenAIServiceTier`] so the branch is structural — when /
         if the SDK adds it, only the resolver changes). Reuses
         `responses::map_service_tier` for the [`ServiceTier`] →
         [`OpenAIServiceTier`] projection.

         **Stream event normalization (§7.4.5).** New
         `normalize_codex_event` runs before each SSE event reaches
         the shared `StreamState`:
         - `response.done` and `response.incomplete` (the latter is a
           legacy event name the Codex backend still emits in places)
           are rewritten to `response.completed` with the inner
           `response.status` normalized into the recognized set
           (`completed`, `incomplete`, `failed`, `cancelled`, `queued`,
           `in_progress`). For the `response.incomplete` rewrite the
           inner status defaults to `Incomplete` if the wire omits it,
           so the §7.3.8 length/content-filter branch picks the right
           [`StopReason`] downstream. The terminal-event semantics
           propagate via a new local `NormalizedEvent::Terminal`
           variant that stops the SSE drain after dispatching the
           rewritten event — enforces the "no more events after
           completion" contract for Codex.
         - Top-level `error` SSE events and `response.failed` events
           surface as `Err(AssistantError)` via
           `responses::error_from_code` (which delegates to the §10
           [`classify_openai_error`] classifier), short-circuiting the
           run before the state machine sees them.
         - Everything else flows through unchanged. The shared §7.3.6
           handlers in `StreamState::process` (text deltas, function
           call arguments, reasoning summaries, output item
           added/done) are bumped to `pub(super)` so the codex module
           can call them.

         **Error mapping (§7.4.6).** New `classify_codex_client_error`
         wraps `responses::classify_client_error` to overlay a
         friendly 429 message when the error code matches
         `usage_limit_reached` / `usage_not_included` /
         `rate_limit_exceeded` *or* the HTTP status is 429 without a
         recognizable code. The optional `plan_type` / `resets_at`
         fields are extracted from the raw error body (try the
         `{"error":{...}}` envelope shape first, then the bare
         `{"plan_type":...,"resets_at":...}` shape) and formatted as
         `"You have hit your ChatGPT usage limit (<plan> plan). Try
         again in ~<N> min."`. Minutes-until-reset is rounded to the
         nearest minute and floored at 0 so a past `resets_at`
         renders as `~0 min`. Category remains `RateLimit` for the
         rate-limit code path so the agent's retry layer sees the
         same semantics regardless of the friendly overlay.

         **Reused machinery (§7.4.7).** The `StreamState` from
         `responses.rs` was refactored to accept the API name and
         cost-multiplier as constructor parameters (new
         `StreamState::new_with(api_name, model, requested_tier,
         multiplier)` constructor; the existing `new(model,
         requested_tier)` is kept as a thin wrapper passing
         `"openai-responses"` and the default multiplier). The api
         name flows through the terminal error message templating and
         the cross-model `append_assistant_message` check; the
         multiplier function pointer flows through `StreamState::finalize`
         so the per-provider pricing curve applies on top of the base
         `calculate_cost` walk. Out-of-scope per §7.4.8: WebSocket
         transport — the provider is SSE-only.

         Twenty-five new unit tests in `openai::codex::tests` cover:
         the User-Agent prefix; `build_request`'s system-prompt
         routing into `instructions` (and the default when empty);
         the hardcoded `store: false` / `tool_choice: "auto"` /
         `parallel_tool_calls: true`; the omission of
         `text.verbosity`, `max_output_tokens`,
         `prompt_cache_retention`, and `strict` regardless of caller
         inputs; the per-tool no-`strict` wire shape;
         reasoning-only-on-reasoning-models;
         `codex_cost_multiplier`'s default curve, the `gpt-5.5`
         priority exception (2.5×), and the requested-tier fallback;
         friendly-message construction with the envelope shape (plan
         type + minutes), the bare 429 case, and the
         non-usage-limit error skip path; `classify_codex_client_error`'s
         429 overlay; event normalization for legacy `response.done` /
         `response.incomplete` (with status preservation) and
         top-level `error` events; the unknown-event passthrough; the
         auth-error path when `api_key` is missing or the JWT lacks
         the account-id claim; and the `to_codex_tool` strict-field
         omission.

         The `StreamState` API-name parameterization rippled into
         three pre-existing tests in `responses.rs::tests`
         (`classify_status_completed_with_tool_use`,
         `classify_status_incomplete_subcases`) that now pass
         `API_NAME` as the trailing argument. Existing
         `openai-responses` round-trip tests
         (`tests/roundtrip/openai_responses.rs`) compile and pass
         unchanged.

         `cargo build`, `cargo test --workspace`, `cargo fmt`, and
         `cargo clippy -p aj-models --all-targets` all pass clean
         (only the pre-existing `clone_on_ref_ptr` warnings in
         `oauth/anthropic.rs` remain — none in the new `codex.rs`
         module or the touched files).
   - [x] 8c.iv. Wire `"openai-codex-responses"` into `provider_for`.
         Added the match arm in `src/aj-models/src/provider.rs::provider_for`
         that dispatches `api: "openai-codex-responses"` to
         [`OpenAiCodexResponsesProvider`] instead of falling through to
         the synthetic `unsupported_api_stream` error. Doc comments on
         the [`Provider`] trait and `provider_for` were updated to list
         the new api string alongside the existing three.

         New unit test `openai_codex_responses_api_is_dispatched_to_codex_provider`
         in `provider::tests` verifies the dispatch path without a
         network call: it calls [`complete`] against a fake model with
         `api: "openai-codex-responses"` and no `api_key` set, then
         asserts the resulting `AssistantError` carries
         [`ErrorCategory::Auth`] (the Codex provider's auth check
         firing) rather than [`ErrorCategory::InvalidRequest`] (the
         unknown-API path). The error category is the discriminator —
         the unknown-API path emits a message starting with
         `"no provider registered for api"` and an `InvalidRequest`
         category, so a regression that drops the arm shows up
         immediately. `result.api == "openai-codex-responses"` is
         asserted alongside so attribution stays correct on log lines.

         `cargo build`, `cargo test -p aj-models`, `cargo fmt`, and
         `cargo clippy -p aj-models --all-targets` all pass clean
         (only the pre-existing `clone_on_ref_ptr` warnings remain in
         `oauth/anthropic.rs`).
   - [x] 8c.v. Round-trip parse / serialize / semantic fixtures.
         New [`aj_models::openai::codex::assistant_message_to_input_items`],
         [`parse_assistant_input_items`], and [`replay_sse_events`]
         helpers expose the Codex-tagged §1.10 round-trip surface so
         the test suite can replay request bodies through the same
         parse / serialize path the live provider uses. The helpers
         delegate to the shared `super::responses` machinery: a new
         `pub(super) fn append_assistant_message` and
         `parse_assistant_input_items_with_api(api_name, items)`
         factor out the api-name-parameterized parse so codex stamps
         `api = "openai-codex-responses"` on the returned message
         without duplicating logic; the replay helper wraps the
         `StreamState::new_with(...)` constructor in a loop that runs
         every event through `normalize_codex_event` first, surfacing
         terminal `error` / `response.failed` frames as
         message-level errors with `stop_reason = Error` and the §10
         classifier output on `error`.

         **Fixtures.** New `tests/roundtrip/fixtures/openai-codex-responses/`
         directory ships four scenarios: `text_only`, `thinking_text`,
         `tool_call` (mirrors the public Responses suite's shapes
         under the Codex `api` / `provider` identity), plus
         `legacy_done` — a Codex-specific scenario whose terminal SSE
         event is `response.done` (the older event name the Codex
         backend still emits in places) instead of
         `response.completed`. Each scenario has a hand-crafted `.sse`
         dump and a `.request.json` golden file. SSE event sequence
         numbers are dense and start at 0; `usage` carries
         non-zero `input_tokens` / `output_tokens` so the parse
         test's assertion on the unified [`Usage`] survives.

         **Test file.** `tests/roundtrip/openai_codex_responses.rs`
         registers via `roundtrip.rs` and runs the standard
         parse / serialize / semantic round-trip shape per scenario,
         plus a fifth `legacy_done_terminator_normalized` test that
         pins the post-normalization state (`stop_reason: Stop`,
         `response_id: Some("resp_codex_legacy_1")`, non-zero usage)
         so a future regression that drops the §7.4.5 rewrite shows
         up as an explicit fail rather than a subtle stop-reason
         drift to the state machine's unterminated-stream default.
         Thirteen new tests in total (4 × {parse, serialize,
         semantic_roundtrip} + 1 normalization assertion). `cargo
         build`, `cargo test -p aj-models`, `cargo fmt`, and `cargo
         clippy -p aj-models --all-targets` all pass clean (only
         pre-existing `clone_on_ref_ptr` warnings in
         `oauth/anthropic.rs` remain).

## Phase 4: Cross-Provider & Utilities

- [x] 9. Message transformation (`aj-models::transform`) — §8
- [x] 10. Partial JSON parser — §11.1
- [x] 11. Error classification & overflow detection (`aj-models::errors`) — §1.3, §10
- [x] 11b. Round-trip test suite (`src/aj-models/tests/roundtrip/`) — §1.10, §12
   - [x] 11b.i. Scaffolding + Anthropic Messages: parse, serialize, semantic round-trip
   - [x] 11b.ii. OpenAI Chat Completions: parse, serialize, semantic round-trip
   - [x] 11b.iii. Cross-provider transform tests (one per direction)
   - [x] 11b.iv. OpenAI Responses: parse, serialize, semantic round-trip

## Phase 5: Authentication

- [x] 12. OAuth infrastructure (`aj-models::oauth`) — §9.2
- [x] 13. Anthropic OAuth (`aj-models::oauth::anthropic`) — §9.3
- [x] 14. OpenAI OAuth (`aj-models::oauth::openai`) — §9.4
- [x] 15. Auth storage (`aj-models::auth`) — §9.1

## Phase 6: Integration

> **Note for future sessions:** steps 16–18 are being executed as
> the concrete commit-by-commit rollout in **`docs/aj-next-progress.md`
> Phase 6** (`Provider` trait + unified types migration). The
> Phase 0–2 work in `aj-next-progress.md` already landed and left
> the binary working but still riding on the legacy `Model` /
> `StreamingEvent` / `crate::messages::*` surface; Phase 6
> decomposes the remaining migration into 9 atomic commits
> (provider cleanup → scripted impl → legacy adapter → agent
> internals → binary call sites → final deletion). Pick the next
> unchecked item from `aj-next-progress.md` Phase 6; tick 16/17/18
> off here once step 6.9 lands.

- [ ] 16. Update `aj-agent` — migrate to new types and streaming
      (executed via aj-next §2.0–§2.5; see `aj-next-progress.md`)
- [ ] 17. Update `aj` CLI — add provider flag, model registry
      (executed via aj-next §2.5; see `aj-next-progress.md`)
- [ ] 18. Remove old code — replaced by the models-spec rewrite.
      Mandatory: the legacy `Model` trait, `create_model`, the legacy
      `StreamingEvent` enum, the `messages` module (replaced by
      `types`), the `aj-models::anthropic::legacy` and
      `aj-models::openai::legacy` modules (replaced by the new
      `Provider` impls — `legacy.rs` is unconditional dead code once
      `aj-agent` migrates off the `Model` trait), the `async-openai`
      dependency if it's still pulled in, and the `openai_ng` module
      if it survived earlier cleanups. Audit: grep for `Model::` /
      `StreamingEvent` / `crate::messages::` / `legacy::` / `aj_models::Model`
      after the §2.5 binary swap and remove every remaining reference.
      Constraint: this step lands only after step 8b *and* aj-next
      §2.4–§2.5 — both the new Provider trait must cover every model
      surface the catalog points at, and the binary must have moved
      off the legacy `Model` trait, before legacy.rs can be deleted.
      (executed via aj-next §2.6; see `aj-next-progress.md`)
