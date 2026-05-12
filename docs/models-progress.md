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
- [ ] 8c. Implement OpenAI Codex Responses provider
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
   - [ ] 8c.iii. SDK + provider implementation.
   - [ ] 8c.iv. Wire `"openai-codex-responses"` into `provider_for`.
   - [ ] 8c.v. Round-trip parse / serialize / semantic fixtures.

## Phase 4: Cross-Provider & Utilities

- [x] 9. Message transformation (`aj-models::transform`) — §8
- [x] 10. Partial JSON parser — §11.1
- [x] 11. Error classification & overflow detection (`aj-models::errors`) — §1.3, §10
- [ ] 11b. Round-trip test suite (`src/aj-models/tests/roundtrip/`) — §1.10, §12
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

> **Note for future sessions:** steps 16–18 are being executed as the
> concrete commit-by-commit rollout in `docs/aj-next-plan.md` §2
> (Phase 0 — refactor the core), tracked in
> `docs/aj-next-progress.md`. The aj-next plan decomposes step 16
> (`Update aj-agent`) into ~6 atomic commits — contract types →
> tool migrations → bus → flip → split loop → cleanup — each of
> which keeps the `aj` binary byte-identical along the way.
> Models-spec step 16 itself acknowledges this dependency: "if [the
> aj-session extraction] lands first, `aj-agent` no longer touches
> `ConversationLog` directly and this step has nothing to update on
> the persistence path." Pick the next item from
> `aj-next-progress.md`; check 16/17/18 off here once §2.4–§2.6 of
> the aj-next plan land.

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
