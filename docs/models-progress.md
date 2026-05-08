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

## Phase 4: Cross-Provider & Utilities

- [x] 9. Message transformation (`aj-models::transform`) — §8
- [x] 10. Partial JSON parser — §11.1
- [x] 11. Error classification & overflow detection (`aj-models::errors`) — §1.3, §10
- [x] 11b. Round-trip test suite (`src/aj-models/tests/roundtrip/`) — §1.10, §12
   - [x] 11b.i. Scaffolding + Anthropic Messages: parse, serialize, semantic round-trip
   - [x] 11b.ii. OpenAI Chat Completions: parse, serialize, semantic round-trip
   - [x] 11b.iii. Cross-provider transform tests (one per direction)

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
- [ ] 18. Remove old code — old messages, Model trait, StreamingEvent, etc.
      (executed via aj-next §2.6; see `aj-next-progress.md`)
