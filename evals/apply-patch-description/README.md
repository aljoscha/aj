# Apply patch description evaluation

This crate freezes the generated suite, runs live paired trials in isolated
Docker guests, and analyzes only durable complete-pair markers.

## Build and run

Run these commands from the repository root. Live commands accept only an
immutable image ID or digest reference. The documented build requires a clean
source tree so the image label binds exactly to `HEAD`.

```bash
test -z "$(git status --porcelain=v1 --untracked-files=all)"
SOURCE_PROVENANCE=$(git rev-parse HEAD)
docker build \
  --build-arg AJ_EVAL_SOURCE_PROVENANCE="$SOURCE_PROVENANCE" \
  -f evals/apply-patch-description/Containerfile \
  -t aj-apply-patch-eval:local .
IMAGE=$(docker image inspect --format '{{.Id}}' aj-apply-patch-eval:local)

cargo run -p aj-apply-patch-eval -- \
  preflight --image "$IMAGE"

cargo run -p aj-apply-patch-eval -- freeze \
  --seed apply-patch-description-v1 \
  --universe-per-archetype 512 \
  --provider openai-codex \
  --model gpt-5.6-sol \
  --reasoning low \
  --output eval-artifacts/unplanned-plan.json

cargo run -p aj-apply-patch-eval -- run \
  --phase smoke \
  --plan eval-artifacts/unplanned-plan.json \
  --records eval-artifacts/records.jsonl \
  --artifact-dir eval-artifacts \
  --image "$IMAGE" \
  --max-cost-usd 6000 \
  --max-trials 1024 \
  --timeout-seconds 600 \
  --max-model-responses 12

cargo run -p aj-apply-patch-eval -- run \
  --phase pilot \
  --plan eval-artifacts/unplanned-plan.json \
  --records eval-artifacts/records.jsonl \
  --artifact-dir eval-artifacts \
  --image "$IMAGE" \
  --max-cost-usd 18000 \
  --max-trials 3072 \
  --timeout-seconds 600 \
  --max-model-responses 12

cargo run -p aj-apply-patch-eval -- plan-main \
  --plan eval-artifacts/unplanned-plan.json \
  --records eval-artifacts/records.jsonl \
  --output-plan eval-artifacts/planned-main.json \
  --output-report eval-artifacts/planning-report.json
cargo run -p aj-apply-patch-eval -- analyze-pilot \
  --plan eval-artifacts/unplanned-plan.json \
  --planning-report eval-artifacts/planning-report.json \
  --records eval-artifacts/records.jsonl \
  --output-json eval-artifacts/pilot-summary.json \
  --output-markdown eval-artifacts/pilot-summary.md

MAIN_TRIALS=$(python3 -c '
import json
print(64 * len(json.load(open("eval-artifacts/planning-report.json"))["selected_main_pair_ids"]))
')
MAIN_BUDGET=$(python3 -c '
import json
r = json.load(open("eval-artifacts/planning-report.json"))
print(1.01 * r["conservative_catalog_pair_reserve"] * len(r["selected_main_pair_ids"]))
')

cargo run -p aj-apply-patch-eval -- run \
  --phase main \
  --plan eval-artifacts/planned-main.json \
  --records eval-artifacts/records.jsonl \
  --artifact-dir eval-artifacts \
  --image "$IMAGE" \
  --max-cost-usd "$MAIN_BUDGET" \
  --max-trials "$MAIN_TRIALS" \
  --timeout-seconds 600 \
  --max-model-responses 12

cargo run -p aj-apply-patch-eval -- analyze \
  --plan eval-artifacts/planned-main.json \
  --records eval-artifacts/records.jsonl \
  --output-json eval-artifacts/summary.json \
  --output-markdown eval-artifacts/summary.md
```

`--max-trials` must be even and is cumulative for the selected phase across
resumes. Every durable trial record in that phase consumes the limit. Smoke and
pilot accept only the original unplanned plan. Pilot also requires every smoke
pair to be valid and complete in the same JSONL stream. Main accepts only the
planned file. It recomputes the exact pilot
completion-stream digest before any provider call. Set main `--max-trials` to at
least sixteen times the selected pair count in `planning-report.json`.

Provider, model, and reasoning are selected only by `freeze`. The defaults are
`openai-codex`, `gpt-5.6-sol`, and `low`. Freeze resolves the local catalog
entry, checks that the reasoning level is supported, checks that the model's
tool family exposes `apply_patch`, and checks that its provider adapter uses a
supported Responses payload. It binds the catalog entry and normalized tool
catalog into the plan and schedule hashes. Later commands have no model override
and fail if the local catalog no longer matches.

`analyze-pilot` accepts the original unplanned plan and the durable planning
report. It deterministically reruns `plan-main` with the frozen default controls
and requires the complete recomputed planning report, including its hash,
blinded summary, evidence, configuration, sample result, selected IDs, and
reserve, to equal the supplied report before it restores treatment labels. This
works whether planning recommended a main sample or was inconclusive. Exactly
the 48 marked pilot pairs enter the report. Smoke pairs, unmarked abandoned
attempts, and any partial main run do not enter it. The JSON and Markdown are
descriptive exploratory artifacts. They cannot support or alter a shipping
decision and do not contain inferential results, thresholds, or a sample
recommendation.

Use a separate plan, records file, artifact directory, pilot report, main sample,
and analysis for every provider, model, or reasoning level. Results from
different levels are not pooled into one shipping decision. The runner
executes adjacent pairs serially to preserve provider, cache, and pair-order
controls. It emits no pilot result until all 48 pairs are complete. The
operational target for completing one pilot is 1 to 2 hours. This is a planning
target, not a guarantee. Endpoint latency, bounded retries, and local capacity
can extend the run beyond that target. For example, a second model or reasoning
level starts with a distinct freeze command and output directory:

```bash
cargo run -p aj-apply-patch-eval -- freeze \
  --seed apply-patch-description-v1-other-model \
  --universe-per-archetype 512 \
  --provider <provider> \
  --model <model-id> \
  --reasoning <level> \
  --output eval-artifacts-other-model/unplanned-plan.json
```

The planner uses conservative paired score and variance approximations. It uses
exactly 512 deterministic Monte Carlo replicates with joint cost and response
draws, and a one-sided Wilson lower confidence bound on achieved power. It scans
the complete efficiency power curve up to the practical cap. Its algorithm
version, simulation count, confidence controls, seed, and nuisance inputs are
frozen in the planning record. Final inference still uses 100,000 paired
bootstrap replicates. A zero current mean makes that relative endpoint explicit
and undefined, which forces an inconclusive shipping decision. A defined point
estimate whose bootstrap includes a zero-current resample is reported with
unbounded confidence bounds and is also inconclusive.

Every phase reserves a conservative catalog estimate for the maximum request
count, full catalog input context, catalog output ceiling, cache-read pricing,
and cache-write sensitivity. Main refuses to start unless the budget covers
this reserve for every remaining planned pair. The empirical pilot reserve is
reported separately. Both are AJ catalog estimates, not invoice controls.

## Safety model

The host process is the only credential holder and the only process that can
reach the provider or artifact directory. Each trial uses a fresh, size-bounded
named tmpfs volume mounted at `/workspace`. The agent worker, every tool worker,
snapshot helpers, and the verifier run with no network, a read-only root,
dropped capabilities, `no-new-privileges`, bounded CPU, memory, processes,
files, and temporary filesystems. Docker attach stdin and stdout carry a
bounded framed protocol. No socket, host worktree, home directory, Docker
socket, credential file, or artifact mount enters a guest.

Effectful tools run in separate one-shot containers. The parent snapshots the
fixture immediately before and after each call. Exiting the tool container
kills shell descendants before the post-call snapshot. Verification uses a
read-only candidate mount and evaluates a copy of the pre-verifier bytes.
Model-visible `check.py` files are coarse smoke checks. Broad
parameter-generated behavior checks remain hidden in the evaluator binary. Any
verifier mutation or attempted self-repair fails verification. `preflight` runs
mandatory path, host-read, environment, network, API-background, and
shell-descendant probes. It also proves that a helper which never returns a
frame is killed, waited for, and removed. A live run repeats preflight and fails
closed.

Every container, including detached volume keepers and one-shot helpers, gets a
parent-generated opaque name before spawn. The parent explicitly stops or kills
it by that validated name, waits for Docker to confirm exit, and removes it
before any dependent snapshot or verifier runs. Volume cleanup is explicit on
every trial path. Cleanup failure produces a durable `runner_internal` record
without a confirmatory pair marker. The mandatory preflight cancels an attached
guest during a delayed write, checks that its name is absent, and proves that no
later fixture mutation occurs. Docker commands and one-shot helpers have fixed
deadlines. The whole-trial deadline includes volume creation, fixture setup,
snapshots, worker execution, verification, and cleanup. A deadline before the
first provider request is a retryable infrastructure failure. A deadline after
a captured request preserves observed usage and becomes a valid `timed_out`
outcome only after cleanup is confirmed.

Generated fixtures are Git repositories rooted at `/workspace`, with a fixed
local author and deterministic initial commit. Records retain the baseline
commit, snapshot hashes, changed paths, binary Git diff, NUL-delimited porcelain
status, and one-message-per-line conversation JSONL as content-addressed blobs.
The diff is always computed against the parent-recorded baseline commit, so an
agent-created commit cannot hide file changes by moving `HEAD`.
They also retain the exact immutable image ID and source provenance as `HEAD`
plus a dirty-worktree content hash when applicable. The required OCI image
label must equal this provenance. Resume and analysis reject mixed image IDs,
source state, UTC dates, runtime limits, model controls, system prompts, or
catalog identities. The first attempt freezes the run's UTC date, which later
phases reuse even when execution crosses a wall-clock date boundary. A dirty
tree is never reported as clean `HEAD`.

There is no local live mode. Scripted unit tests do not execute generated shell
commands.

## Public API limitations

The runtime records these limitations on every trial.

`ToolDetails` is serialized by the one-shot production tool worker and
deserialized back into the exact public enum variant by the agent worker. A
round-trip failure is `runner_internal`. It is never silently replaced with a
generic detail shape.

The public Codex provider currently omits `StreamOptions.max_tokens` from the
wire request. The evaluator therefore records the catalog or server maximum as
the enforceable per-request output ceiling. The parent enforces request count
and an aggregate observed completed-output ceiling. If normalized completed
usage first reports an aggregate above that ceiling, the parent cancels and
fails closed. This cannot interrupt tokens before the provider reports
completed usage. Model turns, wall time, conservative budget admission, and all
container resources are also enforced by the parent.

Before credentials are resolved, the parent constructs both typed first-request
contexts using the same code and frozen UTC date as the worker. It verifies the
canonical path, system prompt, date, model, frozen reasoning effort, messages,
tool order, schemas, and that only `apply_patch.description` differs. Production
`on_payload` validation necessarily occurs after the provider constructs its
serialized request. An invalid payload cancels and drains that request before a
`runner_internal` result. Every request records a hash of the actual serialized
payload after normalizing only the opaque cache key and the `apply_patch`
description. Model, instructions, input, tools, and every stable provider option
remain in the hash. Paired first hashes must match before a completion marker is
written. The reasoning effort in these checks is the value frozen in the plan.

The frozen catalog disables only `agent` and rejects background bash. The
`task_output` and `task_stop` tools remain present symmetrically because they can
only inspect or stop an existing task and cannot start work themselves. No
sub-agent or background task producer is available.

Normalized `Usage` does not retain raw field presence. Records therefore place
zero cache-read usage in the unknown stratum, mark all usage presence fields
unknown, and include the declared cache-write sensitivity range from zero
through reported input tokens.
The recorded catalog cost is AJ's normalized estimate, not invoice cost.
Credential or model resolution failures create a durable zero-usage
`infrastructure_failed` attempt for the first scheduled incomplete pair.
Retryable provider failures also invalidate the attempt immediately. The worker
does not retry them in place because their usage, latency, and partial effects
must not enter a valid observation. The entire pair is recreated in fresh
isolation, with bounded backoff and at most 32 attempts. Resume derives the
next backoff from the durable attempt count and recovers an unmarked clean
complete attempt before starting replacement work.
