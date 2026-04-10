# aj-models Spec Implementation Progress

Tracking file for `docs/models-spec.md` implementation. Each item maps to a
step in §12 (Implementation Plan).

## Phase 1: Unified Types (aj-models)

- [x] 1. Define new type module (`aj-models::types`) — §1, §4
- [ ] 2. Define streaming event types (`aj-models::streaming`) — §2
- [ ] 3. Define model metadata and registry (`aj-models::registry`) — §3.1–§3.3
- [ ] 3b. Create model catalog generator (`scripts/generate-models.py`) — §3.4
- [ ] 4. Define provider trait (`aj-models::provider`) — §5

## Phase 2: Anthropic Provider

- [ ] 5. Update `anthropic-sdk` — §6.1
- [ ] 6. Implement Anthropic provider (`aj-models::anthropic`) — §6.2

## Phase 3: OpenAI Provider

- [ ] 7. Update `openai-sdk` — §7.1
- [ ] 8. Implement OpenAI Chat Completions provider (`aj-models::openai`) — §7.2

## Phase 4: Cross-Provider & Utilities

- [ ] 9. Message transformation (`aj-models::transform`) — §8
- [ ] 10. Partial JSON parser — §11.1
- [ ] 11. Context overflow detection — §10

## Phase 5: Authentication

- [ ] 12. OAuth infrastructure (`aj-models::oauth`) — §9.2
- [ ] 13. Anthropic OAuth (`aj-models::oauth::anthropic`) — §9.3
- [ ] 14. OpenAI OAuth (`aj-models::oauth::openai`) — §9.4
- [ ] 15. Auth storage (`aj-models::auth`) — §9.1

## Phase 6: Integration

- [ ] 16. Update `aj-agent` — migrate to new types and streaming
- [ ] 17. Update `aj` CLI — add provider flag, model registry
- [ ] 18. Remove old code — old messages, Model trait, StreamingEvent, etc.
