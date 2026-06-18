# OpenRouter Provider Integration

Status: accepted, v1 in progress.

This spec describes adding OpenRouter as a model provider in `aj`. It
records the investigation that led to the design, the decision to route
OpenRouter through the OpenAI Responses API, and the concrete changes
that make up the v1 implementation. It complements `docs/models-spec.md`
(the wire-layer spec) and does not restate the catalog or provider
machinery defined there.

## Motivation

OpenRouter is an aggregator that exposes hundreds of models from many
upstream providers behind a single API and a single credential. Adding
it gives `aj` users access to that long tail (and to models we do not
integrate natively) without a per-provider adapter for each one.

## Background: OpenRouter's API surface

OpenRouter exposes three request shapes, all authenticated with a single
`OPENROUTER_API_KEY` bearer token:

1. **Chat Completions** at `https://openrouter.ai/api/v1/chat/completions`.
   The original OpenAI-compatible shape. One-way reasoning only.
2. **Responses API (Beta)** at `https://openrouter.ai/api/v1/responses`.
   A drop-in for OpenAI's Responses API. Stateless, streams reasoning,
   and round-trips encrypted reasoning items across turns. Bearer auth.
3. **Anthropic Messages** at base `https://openrouter.ai/api` (so
   `POST /v1/messages`). Anthropic-compatible, used by Claude Code and
   the Anthropic Agent SDK. Bearer auth via `ANTHROPIC_AUTH_TOKEN`, not
   the `x-api-key` header.

We map every OpenRouter model onto exactly one wire shape, per the
"one api per `(provider, id)`" rule in `docs/models-spec.md` §3.4.3.

## Decision: route OpenRouter through the OpenAI Responses API

v1 maps OpenRouter models to `api = "openai-responses"` with
`base_url = "https://openrouter.ai/api/v1"` and provider id
`"openrouter"`. We reuse the existing `OpenAiResponsesProvider` and the
`openai-sdk` client unchanged. No new `Provider` implementation and no
new wire `api` string.

Rationale:

- The Responses shape is the newer API and preserves reasoning across
  turns via encrypted reasoning items, which our responses provider
  already handles. Chat Completions drops prior-turn reasoning.
- Our `openai-sdk` already POSTs to `{base_url}/responses` with bearer
  auth, so OpenRouter's endpoint works with only a catalog `base_url`.
- It satisfies the project preference to integrate only via the newer
  Responses API or the Anthropic API rather than Chat Completions.

The Anthropic Messages path is viable too but needs SDK work (see
Future work) and is out of scope for v1.

### Evidence: live probe

A live run of our `OpenAiResponsesProvider` against
`https://openrouter.ai/api/v1/responses` (free model
`openai/gpt-oss-20b:free`) confirmed:

- OpenRouter emits the canonical OpenAI Responses SSE event names our
  SDK already models (`response.output_item.added`,
  `response.reasoning_text.delta`, `response.output_text.delta`,
  `response.output_item.done`, `response.completed`, ...). The differing
  names in OpenRouter's docs (`response.content_part.delta`,
  `response.reasoning.delta`) do not match the wire.
- A reasoning prompt returns a clean `Stop` with correct text and usage
  (`input`, `output`, `cache_read` all populated).
- A tool prompt returns `ToolUse` with arguments parsed correctly.

Our SDK's stream event enum has an `Other(Value)` catch-all and the
provider rebuilds the final message from `output_item.done`, so any
future event-name drift degrades gracefully rather than failing.

## Design

### Catalog generation (authoritative, refresh-driven)

OpenRouter models are added to the catalog by the `aj update-models`
refresh flow (`aj-models/src/refresh.rs`), fetched live from
`https://openrouter.ai/api/v1/models`. We do not ship a static snapshot
of OpenRouter models in the bundled seed (`data/models.json`). The seed
stays the Anthropic and OpenAI snapshot it is today, and OpenRouter
becomes available once the user runs `aj update-models`. This keeps the
binary lean (OpenRouter lists hundreds of models that change often) and
keeps the live API as the single source of truth.

The refresh fetches both upstreams. models.dev is the baseline source
and its failure is fatal (the existing cache is left untouched, matching
the "a broken fetch must never brick the registry" contract). An
OpenRouter fetch failure is not fatal: we log a warning and carry
forward the OpenRouter rows already in the cache, so a third-party
outage or a network that blocks `openrouter.ai` never blocks a
first-party refresh.

Mapping from an OpenRouter `/models` entry to `ModelInfo`:

- `id`: the full slash-namespaced id verbatim (e.g.
  `anthropic/claude-sonnet-4`, `openai/gpt-oss-20b:free`).
- `api`: `"openai-responses"`. `base_url`:
  `"https://openrouter.ai/api/v1"`. `provider`: `"openrouter"`.
- Eligibility filter: keep a model only if `supported_parameters`
  includes `"tools"` (agent use requires tool calling) and the model
  can output text (`architecture.output_modalities` absent or contains
  `"text"`). This drops pure image-generation models.
- `reasoning`: `supported_parameters` includes `"reasoning"`.
- `input`: text always, plus image when
  `architecture.input_modalities` includes `"image"`.
- `cost`: OpenRouter prices are per-token strings in USD. Multiply by
  1e6 to get the per-million-token figures our `ModelCost` uses
  (`pricing.prompt`, `.completion`, `.input_cache_read`,
  `.input_cache_write`). Missing prices default to zero so we never
  silently bill against an unknown rate.
- `context_window`: `context_length`. `max_tokens`:
  `top_provider.max_completion_tokens`, falling back to 4096.
- `supports_adaptive_thinking`: always `false` (Anthropic-only concept).
- `headers`: `None` (see attribution headers under Future work).

The `Catalog::source` is `"models.dev+openrouter"` when the live
OpenRouter list is used, `"models.dev+openrouter (cached)"` when carried
forward after a failed fetch, and `"models.dev"` otherwise. The whole
catalog is sorted by `(provider, id)`, so OpenRouter sorts after
`openai-codex`. The hand-curated Codex seed is spliced in as before.

### Wire shape and provider reuse

No change to `provider_for` dispatch (`provider.rs`). OpenRouter's
`api = "openai-responses"` selects the existing
`OpenAiResponsesProvider`, and the per-call `base_url` from `ModelInfo`
points the `openai-sdk` client at OpenRouter.

### Authentication

`find_env_keys("openrouter")` returns `["OPENROUTER_API_KEY"]`
(`aj-models/src/auth.rs`). OpenRouter uses a plain bearer API key, so the
generic credential chain (runtime `--api-key` override, env var, stored
`auth.json` key) works with no OAuth provider. There is no in-TUI flow
to store a plain API key today (the `/login` picker is OAuth-only), so
OpenRouter credentials come from the env var or `--api-key`.

### Reasoning fidelity fix (`openai-responses` provider)

OpenAI's first-party reasoning models surface visible reasoning as
*summary* text (`response.reasoning_summary_text.*`), with the raw chain
delivered only as encrypted content. Some OpenRouter models instead
stream the raw chain as plain text via `response.reasoning_text.*` and
send no summary.

Our responses provider previously routed only summary deltas into the
thinking block and, on `output_item.done`, overwrote the thinking text
with the joined summary. For a model whose reasoning item carries an
empty `summary` and a populated `content`, that produced an empty
thinking block. The fix:

- Route `response.reasoning_text.delta` into the thinking block, the
  same as summary text deltas, so raw reasoning renders live.
- On `output_item.done` for a reasoning item, derive the final thinking
  text from the `summary` when present, otherwise from the `content`
  reasoning-text parts, and fall back to whatever was accumulated from
  live deltas when the item carries neither.

This benefits any OpenAI-compatible Responses endpoint that streams raw
reasoning text, not just OpenRouter. A model streams either a summary or
a raw chain, not both, so there is no double-counting.

### UI surfaces

- Add `"openrouter"` to `KNOWN_PROVIDERS` in `aj/src/auth.rs` and
  `aj/src/usage.rs` so it appears in the `/auth` and `/usage` overlays
  even before a credential is stored. Update the `/usage` test that pins
  the known set.
- Extend the `--model-api` help text (`aj/src/cli/args.rs`) to mention
  `openrouter`.

The model selector enumerates `registry.providers()` dynamically, so it
needs no change once the catalog carries OpenRouter rows. OpenRouter has
no OAuth provider, so it renders with its bare id as the label.

## Known limitations and risks

- **Beta API.** OpenRouter's Responses API is beta and may change. The
  catch-all event handling limits the blast radius of wire drift.
- **Per-model coverage.** The Responses transformation layer is broad
  but we do not verify every model supports it. A model that rejects the
  Responses shape surfaces a normal provider error.
- **No stored key in the TUI.** Credentialing relies on
  `OPENROUTER_API_KEY` or `--api-key`.
- **Catalog requires a refresh.** A fresh install has no OpenRouter
  models until `aj update-models` runs.

## Future work

- **Anthropic Messages path.** OpenRouter's `/v1/messages` endpoint
  could back an additional provider entry mapped to
  `anthropic-messages`. This needs the `anthropic-sdk` to support bearer
  auth without forcing the Anthropic OAuth beta headers, which are
  currently coupled to bearer mode.
- **Attribution headers.** OpenRouter accepts optional `HTTP-Referer`
  and `X-Title` headers for traffic attribution. Supporting them means
  wiring the currently-unused `ModelInfo::headers` (and/or
  `StreamOptions::headers`) into the OpenAI providers' SDK client.
- **Preferred default model.** Optionally add an `("openrouter", <id>)`
  entry to `PREFERRED_DEFAULT_MODELS`. Omitted in v1 because OpenRouter
  ids churn and selection falls back to the first listed model.

## Testing plan

- Unit tests for the OpenRouter `/models` parsing and mapping (filter,
  pricing conversion, modality, reasoning flag) using a fixture.
- Unit test for the reasoning-text rendering fix in the responses
  provider state machine.
- Unit test for `find_env_keys("openrouter")`.
- Update the `/usage` known-providers test.
- A gated, `#[ignore]`d live integration test that streams a free
  OpenRouter model through the responses provider, run manually with
  `OPENROUTER_API_KEY` set.
