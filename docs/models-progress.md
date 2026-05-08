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
- [ ] 8b. Implement OpenAI Responses provider (`aj-models::openai`) — §7.3.
      The §12 plan didn't list this as a discrete step, but §3.4.3
      mandates `api: "openai-responses"` for native OpenAI models, and
      the bundled catalog reflects that — all 41 native OpenAI catalog
      entries are `openai-responses`. Today `provider_for("openai-responses")`
      returns `None`, so the new `Provider` trait can't drive any native
      OpenAI model; only `openai-completions` (third-party-shape, no
      catalog entries today) is wired. Implement against §7.3 — message
      conversion (input array of typed items, system/developer prompt,
      reasoning items round-tripped via `thinking_signature`, text
      signatures via `TextSignatureV1`, composite tool-call IDs
      `{call_id}|{item_id}`), request parameters (`reasoning.effort`,
      `include: ["reasoning.encrypted_content"]`, `prompt_cache_key`,
      `prompt_cache_retention`, `service_tier`, `store: false`),
      session-correlation headers (§7.3 preface), the SSE event
      mapping table (§7.3.6), usage parsing (§7.3.7), and the
      `response.status` → `StopReason` mapping (§7.3.8). The legacy
      `crate::openai::legacy::OpenAiModel` is already Responses-based
      and can serve as a wire-mapping reference; round-trip §7.3.3
      (encrypted reasoning), §7.3.4 (text signatures), §7.3.5
      (composite IDs) need fresh implementation since they're
      multi-turn-replay invariants the legacy code didn't have to care
      about. Ship 11b.iv round-trip fixtures alongside (Responses
      parse/serialize/semantic round-trip).

## Phase 4: Cross-Provider & Utilities

- [x] 9. Message transformation (`aj-models::transform`) — §8
- [x] 10. Partial JSON parser — §11.1
- [x] 11. Error classification & overflow detection (`aj-models::errors`) — §1.3, §10
- [ ] 11b. Round-trip test suite (`src/aj-models/tests/roundtrip/`) — §1.10, §12
   - [x] 11b.i. Scaffolding + Anthropic Messages: parse, serialize, semantic round-trip
   - [x] 11b.ii. OpenAI Chat Completions: parse, serialize, semantic round-trip
   - [x] 11b.iii. Cross-provider transform tests (one per direction)
   - [ ] 11b.iv. OpenAI Responses: parse, serialize, semantic round-trip
         (lands with step 8b)

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
