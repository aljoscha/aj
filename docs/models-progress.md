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
