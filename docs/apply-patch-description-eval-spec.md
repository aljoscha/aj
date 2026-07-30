# Apply patch description evaluation

## Decision

Determine whether a shorter `apply_patch` tool description reduces AJ-recorded
catalog cost and interaction count without materially reducing task completion
or patch reliability.

The decision compares two frozen descriptions. The assigned description is the
only treatment. Pair order and unique trial identities vary only according to
the frozen schedule.

- `current`: the production description from `ApplyPatchTool`.
- `compact-v1`: the candidate below.

The compact description is eligible to ship only when task success is
non-inferior, patch-failure and edit-bypass guardrails pass, and at least one
efficiency endpoint improves by a worthwhile amount without degrading the
other. Tool-call errors, session interactions, latency, token usage, and
AJ-recorded catalog cost explain the result.

## Compact description candidate

````text
Apply a patch to one or more files using the Codex patch format.

You MUST read an existing file before patching it.

Wrap the patch in `*** Begin Patch` and `*** End Patch`. Each file operation
starts with one of these headers:

- `*** Add File: <path>` creates a file. Prefix every content line with `+`.
- `*** Delete File: <path>` deletes a file. Nothing follows the header.
- `*** Update File: <path>` updates a file. It may be followed by
  `*** Move to: <new path>` to rename it.

An update contains one or more hunks starting with `@@`, optionally followed by
a class, function, or other selector. Within a hunk, prefix unchanged context
with a space, removed lines with `-`, and added lines with `+`.

Include at least 3 unchanged lines before and after a change when available.
Use enough context or a selector to identify the location uniquely. Do not
repeat overlapping context between adjacent hunks. Use `*** End of File` after
a hunk that must match the end of the file.

Multiple file operations may appear in one patch. Paths may be relative or
absolute.

Example:
```
*** Begin Patch
*** Update File: src/utils.ts
@@ export function label
 export function label(value: string) {
-  return value
+  return value.trim()
 }
*** End Patch
```
````

The candidate ends with exactly one LF after the example's closing three
backticks. It has no extra blank line. The implementation must store these
bytes in a versioned UTF-8 source file and load that file without trimming or
newline conversion. That file, rather than Markdown rendering, is the
authoritative input to the recorded hash and byte length.

The tool input schema remains unchanged. The model still calls the JSON
function with `patchText`. The description only defines the string contained in
that field.

## Experimental unit

One trial is one fresh agent session operating on one isolated repository
fixture at an immutable revision. A trial has:

- one description variant
- one model and provider
- one reasoning effort
- one task instance and verifier
- one fresh checkout and conversation
- one fixed timeout and model-turn limit

Each generated task instance defines one pair and runs once under each variant.
The two checkouts are independent but byte-identical. The pair's trials run
adjacently. For each archetype, a seeded bit selects whether its first
repetition is `current, compact-v1` (`AB`) or `compact-v1, current` (`BA`).
Later repetitions alternate order. Counts differ by at most one when the fixed
repetition count is odd. The runner uses the run seed to shuffle complete pair
keys, never individual trials, before execution.

Providers do not offer deterministic generation for every model. Pairing and
repetition reduce that variance. They do not eliminate it.

## Controls

The default evaluation uses `openai-codex/gpt-5.6-sol` with low reasoning.
`freeze` may select another provider, model, and reasoning level from the local
catalog. It freezes the selected catalog entry, capability identity, normalized
tool catalog, and reasoning level before smoke. The runner verifies that the
entry supports the selected effort, exposes `apply_patch`, and sends the frozen
effort through a supported Responses payload. It refuses paid runs if any
assertion fails or the local catalog changes. It uses the production system
instructions, provider options, and builtin tool set, except that the `agent`
tool and background bash are disabled for both variants. Sub-agents add a second
model policy and make attribution to the patch description unclear.

Each provider, model, and reasoning selection is a separate estimand with its
own frozen plan, records, pilot, main sample, and decision. Trials from different
selections are never pooled. The model selection is a fixed control within one
run, not another treatment.

The Codex adapter does not serialize `StreamOptions.max_tokens`. The frozen
contract therefore uses the model catalog or server maximum as the enforceable
per-request output ceiling and records it by that name. It does not claim that a
smaller client cap is sent. The parent also bounds request count and aggregate
completed output usage, but completed-usage enforcement cannot stop tokens
before the provider reports them.

The runner obtains builtin tools normally, finds the erased definition named
`apply_patch`, and replaces only its `description` field for `compact-v1`. The
input schema, execution closure, execution mode, and all other tool descriptions
remain identical.

Every trial uses a new `Agent`, checkout, session log, session ID, and effective
provider prompt-cache key. This defines a fresh-session, fresh-key estimand. The
runner does not perform warm-up calls because a different session ID produces a
different cache key on this provider path. It records the effective key as a
hash and records reported cache-read and cache-write tokens. Reports stratify
sessions into reported zero cache read, reported positive cache read, and
unknown cache read. A fresh key does not guarantee a provider cache miss, so the
intent-to-treat result includes every stratum. Prompts must describe the desired
repository change and acceptance criteria without naming `apply_patch` or
explaining its syntax.

After the primary evaluation, a smaller exploratory run at `high` reasoning may
check direction. It is not confirmatory and cannot change the shipping decision.

## Task suite

The suite uses generated local fixtures rather than active project worktrees or
popular public repositories. Generation starts from committed templates and
substitutes a seeded identifier set. Paired variants receive the same generated
instance. Different repetitions receive different identifiers. This reduces
memorized solutions while preserving the same structural task.

Each task has an independent verifier and a changed-path allowlist. Verifiers
prefer behavior tests over exact diffs. Exact expected contents are appropriate
for narrow data or configuration edits. Every verifier also rejects unrelated
changes. Model-visible `check.py` files are coarse smoke checks only. The
authoritative verifier generates hidden checks from the frozen task parameters
inside the verifier guest. Those checks exercise broad deterministic domains,
boundary and breaking inputs, related files, indentation, rename behavior, and
ambiguous repeated structures. Hidden check sources and cases are never written
into the model-visible repository. `task_passed` requires the hidden check to
pass.

The initial suite contains 16 task archetypes with this weighting:

| Count | Class | Required coverage |
| ---: | --- | --- |
| 6 | Common single-file updates | unique replacement, multiline edit, insertion, removal, indentation-sensitive edit, nearby changes |
| 3 | Multi-file updates | two related source files, source plus test, three-file configuration change |
| 3 | File operations | add, delete, rename with content change |
| 2 | Ambiguous context | repeated blocks and repeated methods requiring a selector or wider context |
| 1 | File boundary | append or replace content anchored at end of file |
| 1 | Uncommon text | conflict markers or CRLF input, alternating by repetition |

At least half of the tasks must require running a cheap verifier command. At
least four must admit more than one valid diff so the suite does not reward
copying an expected patch.

### Target workload and estimand

The target workload is the generated suite profile above. Each of the 16
archetypes has weight `1/16`, so the class weights are the counts in the table.
Within an archetype, the target is the fixture generator's seeded identifier
distribution. The estimand for each endpoint is the weighted mean paired effect
of `compact-v1` versus `current` over that profile at the declared model,
reasoning effort, and fresh-key cache regime.

Claims apply only to this suite profile. They may be generalized to a deployment
workload only if task-class weights are estimated from production task data and
frozen before anyone sees any evaluation outcome. In that case the manifest
records the data window, classification procedure, counts, and resulting fixed
weights. The unweighted suite result is still reported.

## Outcomes and metrics

### Primary outcome

`task_passed` is true only when the independent verifier succeeds, changed paths
stay within the allowlist, and the agent completes within its limits. This is
the primary measure because a syntactically valid patch can still make the
wrong change.

The runner assigns exactly one terminal status using this ordered taxonomy:

1. `runner_internal`: the sandbox, event accounting, snapshots, or artifact
   contract failed.
2. `infrastructure_failed`: the attempt saw a retryable provider or transport
   failure, or credentials and model resolution failed.
3. `cancelled`: an operator or parent shutdown interrupted the trial.
4. `timed_out`: the predeclared trial timeout expired.
5. `turn_limit`: the predeclared model-turn limit was reached.
6. `model_failed`: the agent received a terminal model error not covered above.
7. `verifier_failed`: verification or the changed-path allowlist failed.
8. `passed`: verification and the allowlist passed.

The first applicable status wins. The first three statuses are invalid trials,
not task failures. `runner_internal` stops the run for investigation.
`infrastructure_failed` and `cancelled` preserve the attempt and rerun the entire
pair with the same seeds. Transparent provider retries are disabled in the
worker, so usage, latency, mutations, and cancellation from a failed request
cannot enter a valid replacement. A pair has at most 32 fresh isolated
attempts with bounded backoff. Statuses four through seven set `task_passed` to
false. Status eight sets it to true. This makes the valid-trial intent-to-treat
denominator ordered and exhaustive.

### Patch reliability

Count every model-requested `apply_patch` call, including calls rejected before
execution. The runner serializes potentially mutating tool calls and takes a
filesystem snapshot immediately before and after each call. It then assigns the
first applicable classification:

- `schema_error`: the call does not deserialize into `ApplyPatchInput`
- `partial_application`: an invoked call returns an error and its snapshots
  differ
- `success`: the invoked tool returns `is_error: false`
- `format_error`: the result starts with `apply_patch verification failed:`
- `rejected`: the result starts with `patch rejected:`
- `application_error`: any remaining invoked tool error

The ordering makes the categories mutually exclusive and exhaustive. Snapshot
differences, not result text, identify partial application.

Report both of these rates:

```text
failed_patch_call_rate = failed apply_patch calls / attempted apply_patch calls
sessions_with_patch_failure = sessions with at least one failed patch call / sessions
```

The session-level rate is the comparison metric. The call-level rate is
diagnostic because repeated failures within one session are correlated.

### Session length

Record:

- model responses, meaning completed provider inference rounds
- tool rounds, meaning model responses that requested at least one tool
- total tool calls and calls by tool name
- `apply_patch` attempts and successful calls
- recovery rounds after the first failed patch call
- wall-clock duration
- final assistant response text

There is no simulated human reply. Task completion is determined only by the
verifier and limits, not by classifying the final response's prose. "Back and
forth" therefore means agent-model inference and tool rounds, not synthetic user
messages.

### Cost and tokens

An evaluator event collector sums usage from every assistant `MessageEnd`
within an attempt, including terminal error and aborted messages. It does not use
`Agent::accumulated_usage`, which only accounts for successful turns. An attempt
that observes a provider failure remains in immutable artifacts but cannot enter
a completion marker, pilot reduction, or main inference. Record:

- input tokens
- output tokens
- cache-read tokens
- cache-write tokens
- total tokens
- AJ-recorded catalog cost by category and total

Token values are provider-reported. AJ computes `Usage.cost` from the frozen
model catalog, so every report and result field calls it
`aj_recorded_catalog_cost`, never billed or invoice cost. This endpoint is
defined as the value AJ records today. On the target provider, unreported
cache-write tokens enter `Usage` as zero, so this endpoint is not a complete
estimate of provider charges. The runner records field-presence metadata and
reports a separate sensitivity range that allows unknown cache-write tokens to
vary from zero through reported input tokens. The sensitivity range is
diagnostic because AJ exposes no authoritative cache-write count.

Report mean and median session AJ-recorded catalog cost, AJ-recorded cost per
successful task, and first-response AJ-recorded cost. Keep cache categories
separate and include the cache-write sensitivity range. Total tokens are not a
cost substitute.

The generated report includes per-variant distributions for every token
category, total tokens, duration, tool rounds, total calls, calls by tool, and
recovery rounds. It also includes patch-classification counts, absolute task
success, edit-bypass and terminal-failure counts, final-assistant-text counts
and byte lengths, and content-addressed text blob references. Cache-write
sensitivity reports the full possible relative decision range, not only variant
means.

### Edit bypass

The fixture manifest declares the filesystem paths and metadata that constitute
repository state, plus any ignored verifier or build-output paths. Around every
`apply_patch`, bash, or other potentially mutating call, the runner records an
ordered before-and-after snapshot of that state. A session is `edit_bypass` if
any non-`apply_patch` call changes repository state, even if a later successful
patch makes another change or restores it. Such a session still counts according
to its verifier for the intent-to-treat task outcome. Bypass is a guardrail
because it avoids the behavior under evaluation.

## Run protocol

1. Freeze the suite manifest, descriptions, endpoint definitions, margins,
   efficiency thresholds, statistical implementation, run seed, and pilot size.
2. Resolve the model and credentials once, then record the exact provider,
   model ID, model catalog hash, reasoning effort, and AJ revision. Complete the
   sandbox and captured-request preflight before any paid call.
3. Generate and freeze a maximum deterministic universe of paired task
   instances before model calls. Freeze the smoke and pilot schedules as
   prefixes of separate seeded permutations.
4. Run an excluded smoke pair for every archetype. Stop if the runner, verifier,
   accounting, isolation, or artifact capture is incorrect.
5. Run an excluded pilot with three paired repetitions per archetype. This is 48
   pairs and 96 sessions.
6. Give a sample planner only blinded pooled pilot summaries. The planner may
   estimate pooled event rates, paired discordance or within-pair correlation,
   between-archetype heterogeneity, and paired efficiency variance. It cannot
   see which label is `current` or `compact-v1`.
7. Choose one main-run repetition count. Select that many repetitions per
   archetype from a third predeclared permutation of the frozen universe, then
   freeze their complete-pair order. Run that fixed sample without inspecting
   interim outcomes.
8. If requested, run a separately labeled high-effort exploratory sample after
   the low-effort decision is final.

The sample planner targets at least 80% power for each binary guardrail under no
true degradation, using one-sided 5% alpha and the exact analysis margins below.
It uses a conservative analytic paired-score approximation over the one-sided
95% nuisance region for pooled event and discordance rates. It never substitutes
zero for an unobserved pilot event. It also requires enough pairs that the exact
score method's all-zero upper bound can clear each rare-event margin and its
all-pass lower bound can clear the task-success margin.

For efficiency, the planner uses paired relative observations and a one-sided
95% upper confidence bound for their variance. Model-response observations are
integers. A response-improvement alternative retains the mandatory first
response when one was observed and treats only follow-up responses as removable.
A zero-response timeout remains zero. Follow-up removal uses exact independent
Bernoulli draws. Retention accounts for the observed fraction with any response,
so zero-response mixtures preserve a true 10% mean improvement. If retaining a
mandatory first response makes that reduction impossible, the response-benefit
alternative is infeasible. The two alternatives remain a true 10% improvement
in one endpoint and no true change in the other. Each Monte Carlo replicate draws
every planned paired cost and response observation jointly and independently
within its archetype. Both conditions in the corresponding shipping alternative
must pass in the same replicate. It does not multiply one sampled pilot pair by
the planned repetition count.
The planner evaluates achieved power with 512 deterministic Monte Carlo
replicates using common random numbers. It requires a one-sided 95% Wilson lower
bound of at least 80%, not a simulated point estimate. It evaluates the complete
deterministic efficiency power curve up to the practical frozen cap, so the
search does not assume that finite Monte Carlo results are monotone. Efficiency
planning has a frozen minimum of two pairs per archetype. The exact
algorithm version, fixed replicate count, seed, variance method, and confidence
controls are frozen in the planning record.
Pilot sessions are never included in main inference. If the required fixed
sample is impractical, the result is inconclusive. Margins and thresholds are
not weakened. Final confirmatory analysis still uses 100,000 bootstrap
replicates.
### Exploratory pilot report

After `plan-main` has frozen a valid planned main file, `analyze-pilot` may
restore treatment labels and produce a descriptive report over the 48 pilot
pairs. The command must first validate the complete smoke and pilot evidence
against the unplanned schedule hash committed by planning. Its report sample is
exactly the three marker-referenced pilot pairs from each of the 16 archetypes.
Smoke pairs and every unmarked abandoned attempt are excluded.

The pilot report is structurally separate from confirmatory analysis. It is not
eligible for a shipping decision and cannot emit a shipping disposition,
guardrail or harm result, inferential interval, significance result, statistical
power claim, threshold result, or sample recommendation. It reports only
observed variant counts, rates, paired discordance, distributions, and
compact-v1 minus current differences. The report includes deterministic evidence
identities and hashes so it can be reproduced from the frozen plan and JSONL
stream.

Every provider, model, and reasoning level uses independent plan, records,
artifact, pilot-report, and confirmatory-report files. Levels are not pooled. A
48-pair pilot has an operational target of 1 to 2 hours, but this is not
guaranteed. Provider latency, bounded pair retries, and available local capacity
can extend the run.

`--max-cost-usd` is a conservative catalog admission control, not an interrupt
or invoice control. The hard per-pair reserve covers both trials at the maximum
request count, full catalog input context, catalog or server output ceiling,
the highest applicable catalog rates, cache-read pricing, and cache-write
sensitivity. Smoke and pilot use the same reserve. The fixed main run starts
only if its budget covers this reserve for every remaining planned pair. During
the run, the runner starts a pair only if the remaining reserve covers both
trials, then lets both finish even if the estimate is exceeded. The blinded
pilot's one-sided 95% pair-cost reserve is frozen and reported as an additional
empirical diagnostic. It is not the hard budget guard.
`--max-trials` must admit an even number of trials. It is a cumulative per-phase
limit across resumed commands, and every durable trial record from that phase
consumes it. Both controls are checked only before starting a fresh adjacent
pair attempt. A process interruption or invalid trial leaves an incomplete pair.
Its records are retained, the whole pair is rerun with the same seeds, and
neither attempt enters main inference. If budget or interruption
prevents the full frozen sample from completing, the partial result is
descriptive and cannot support shipping.

Smoke and pilot commands accept only the original unplanned plan. Pilot requires
every valid smoke completion marker in the same artifact stream. Main accepts
only a planned plan. Planning freezes the unplanned schedule hash and a
deterministic hash over sorted excluded completion-marker hashes, referenced
trial hashes, and their identities. The blinded summary and both reserves are
bound by the same planning hash. Main admission and analysis recompute this
commitment from the same stream. Immutable unreferenced failed or incomplete
attempts remain in the stream but do not enter the commitment or reduction.

The evaluation worker disables transparent provider retries. Any retryable
provider or transport failure immediately invalidates the trial and its pair.
The runner recreates the whole pair in fresh isolation with bounded backoff,
subject to the cumulative trial budget and the 32-attempt pair cap. Every
attempt remains in artifacts, but only a clean complete attempt can receive a
completion marker.

## Analysis and decision rule

Analyze trials by their assigned description even if the model bypasses the
patch tool. Estimate each binary effect as `compact-v1 - current` using the
fixed archetype weights. Compute its one-sided 95% bound by inverting a
predeclared stratified paired score test for the weighted risk difference over
the paired 2 by 2 counts in each archetype. The implementation, numerical
tolerance, and test vectors are frozen before the main run. Its tests must show
a nonzero-width bound when both variants observe zero rare events. Individual
patch calls are not independent observations.

The three guardrails form an intersection-union decision, so each must pass its
one-sided 95% bound:

- task success: `compact-v1 - current` must have a lower confidence bound above
  `-5` percentage points
- sessions with a patch failure: the upper confidence bound for
  `compact-v1 - current` must be below `+3` percentage points
- edit bypass: the upper confidence bound for `compact-v1 - current` must be
  below `+2` percentage points

The two efficiency endpoints are weighted mean AJ-recorded catalog cost and
weighted mean model responses. For each endpoint, define relative change as
`mean(compact-v1) / mean(current) - 1`. Use 100,000 seeded paired bootstrap
replicates, resampling repetitions within each fixed archetype and preserving
the declared weights. Because either endpoint may establish benefit, use a
one-sided 97.5% upper bound for each improvement claim. Shipping requires all
binary guardrails and one of these two cases:

- one endpoint's point estimate is at most `-5%` and its upper bound is below
  zero, while the other endpoint's one-sided 95% upper bound is below `+2%`
- the same rule with the endpoints exchanged

The cost endpoint intentionally follows AJ's current accounting and is compared
identically across variants. Report its limitations and cache-write sensitivity
range next to every cost decision. Report absolute effects, relative effects,
bounds, medians, and tail percentiles. Latency, token categories, call-level
errors, and cache strata are diagnostic and do not override the decision rule.

## Runner and artifacts

The runner is a trusted parent plus one disposable worker per paid trial. A
`TempDir` alone is not isolation because bash and absolute patch paths can reach
the host. The parent must enforce all of these conditions:

- The worker runs in a disposable container or VM with a read-only base image.
  Its generated fixture is the only persistent writable mount. A bounded guest
  tmpfs holds temporary `HOME` and `TMPDIR` data. It has no host worktree, home
  directory, credential files, container socket, or unrelated host mount.
- The image has a required OCI provenance label set from a build argument. Its
  value is the clean source `HEAD`, or the exact dirty-worktree provenance hash
  when a dirty build is explicitly supported. Preflight and every live run
  compare the label with host source provenance and fail closed on a mismatch.
  Every trial records Docker's exact immutable image ID.
- The worker receives a temporary `HOME` and a minimal allowlisted environment.
  Provider credentials remain in the parent. The worker's provider reaches a
  parent-owned IPC broker through a non-inheritable capability created for that
  trial. No filesystem socket or discoverable endpoint is mounted in the guest.
  The broker authenticates the trial and enforces its model, effort, request
  count, concurrency, token limits, and cancellation. Tool child processes
  receive neither the capability nor model credentials. General worker network
  access is denied.
- CPU, memory, process, disk, and wall-clock limits are fixed. Absolute and
  parent-relative paths can affect only the disposable guest filesystem.
- Every Docker command and one-shot helper has a fixed parent deadline followed
  by explicit container kill, wait, and removal. The whole-trial deadline covers
  volume creation, fixture generation, baseline capture, worker initialization,
  final capture, verification, and cleanup. Preflight has its own overall
  deadline and includes a helper that never writes a protocol frame. Expiry
  cancels provider work and waits for bounded containment cleanup. Expiry before
  the first provider request is retryable infrastructure failure. Expiry after a
  captured request preserves observed usage and is `timed_out` only when cleanup
  is confirmed. Otherwise it is `runner_internal`.
- The parent owns the artifact directory outside the worker's writable area.
  The model and its tools cannot alter events, snapshots, diffs, or result
  records after capture.

If these guarantees are unavailable, the runner refuses live-model runs. Local
scripted tests may use a weaker fixture only when they do not execute
model-generated shell commands.

Inside the worker, construct the production `Agent` loop directly with
`Agent::with_provider`, the fixture mounted at the canonical guest path
`/workspace`, and tool definitions bound to that path. Both members of every
pair have identical model-visible paths. Do not call `aj-app::build_agent` or
depend on process-global current directory. Obtain the production builtin tools,
disable `agent`, and replace only the selected `apply_patch` description. Both
variants use the same wrappers and restrictions.

The eval bash wrapper rejects `run_in_background: true` and executes every
foreground command in a dedicated child cgroup or equivalent process boundary.
Before it returns, it terminates and reaps every descendant, then takes the
post-call snapshot. The runner also snapshots repository state between model
responses and before each mutating call. Any state change outside a recorded
`apply_patch` interval is `edit_bypass`, including delayed shell writes. An
unattributed mutation that prevents reliable ordering is `runner_internal`.

After the trial completes, fails, or reaches a limit, the runner shuts down the
`TaskRegistry` and waits for quiescence. It then kills any remaining process in
the worker cgroup before taking the final agent-state snapshot. A failure to
quiesce or terminate processes is `runner_internal`. The parent captures the
final diff and changed paths from this snapshot before verification. It then
clones that state into a second disposable guest and runs the verifier there.
The verifier guest has the same filesystem, environment, network, process, and
resource containment, but no model-broker capability. Verifier commands and
acceptance logic are immutable parent-owned inputs. The parent snapshots the
verifier copy before and after the command, records verifier mutations, and
never derives the agent diff from verifier state. The candidate mount is
read-only. Authoritative checks inspect a separate copy of the pre-verifier
bytes. Any mutation or attempted source repair fails verification.

Write immutable JSONL trial records plus a generated JSON and Markdown summary.
Each trial record must include:

- run ID, pair ID, attempt ID, task ID, task seed, repetition, variant, and order
- hashes and byte lengths of both frozen descriptions
- AJ revision, suite revision, model catalog hash, provider, model, and effort
- effective cache-key hash, cache stratum, and usage-field presence flags
- prompt, fixture revision, verifier command, and verifier output
- validity, terminal status, outcome, and all metrics above
- ordered per-tool mutation deltas, final agent diff, and changed paths
- pre-verifier and post-verifier hashes and verifier mutation paths
- conversation-log path and provider error details
- exact image ID, source provenance, UTC date, runtime limits, catalog or server
  output ceiling, system-prompt hash, and normalized actual payload hashes

The parent flushes each trial record before continuing and writes a complete-pair
marker only after both valid records are durable. On resume, it first recovers
the earliest unmarked attempt whose two durable records exactly match the frozen
pair and contain no provider retry or error evidence. Other unmarked pairs are
rerun in full under a new attempt ID while old records remain immutable.
`--max-cost-usd`, `--max-trials`, and `--seed` are required controls.

Resume rejects any scheduled attempt or completion marker with a different
image ID, source provenance, UTC date, runtime limit, model catalog, model,
effort, system prompt, or relevant provider control. Analysis applies the same
single-context requirement. The first attempt freezes the UTC date and every
later phase reuses it. Credential or model resolution failure after the
first pair is known creates a durable zero-usage `infrastructure_failed` attempt
for that pair. It consumes one of the bounded pair attempts without fabricating
model usage.

## Harness verification

Before any paid run, test the runner with `ScriptedProvider` plus a capturing
provider wrapper. `ScriptedProvider` alone is insufficient because it ignores
the request `Context`. For a fixed synthetic trial identity, capture `Context`,
`StreamOptions`, or the serialized payload through `on_payload` and assert that
the two variants differ only in the exact `apply_patch` description bytes. The
paid-path equality hash is derived from each actual `on_payload` JSON value. It
normalizes only the opaque cache identity and `apply_patch` description. Model,
instructions, input, tools, and all stable provider fields remain covered. Also
test:

- successful patch on the first call
- malformed patch followed by successful recovery
- failed partial application detected from snapshots
- shell edit followed by a successful patch classified as edit bypass
- task completion with a verifier failure
- provider failure classified as infrastructure failure
- usage and AJ-recorded catalog cost accumulation across successful, error,
  aborted, and retried assistant `MessageEnd` events
- missing cache-write usage identified and included in the sensitivity range
- the reasoning level frozen by the plan present in the captured model request
- API background bash rejected, shell-level background descendants reaped, and
  the task registry quiescent before snapshot
- broker capability unavailable to tool child processes and request limits
  enforced by the parent
- paired workers exposing the same canonical path and identical model context
  outside the description and opaque trial identities
- verifier mutations detected without changing the captured agent diff
- a three-point behavior special case and a self-repairing verifier candidate
  rejected by hidden checks over pre-verifier bytes
- verifier guest unable to reach the model broker, host files, credentials, or
  unrestricted network
- resume rerunning an incomplete pair without duplicating a complete pair
- resume rejecting mixed image, source, date, limits, and provider controls
- budget and trial stops occurring only between complete pairs
- all-zero rare-event input producing a nonzero-width score bound

An isolation integration test attempts absolute and parent-relative patch paths,
host-file reads, credential-environment reads, background processes, and network
access. None may escape the disposable guest or expose credentials. The test
also proves that the parent artifact store is not model-writable.

The scripted tests validate accounting and classification. They do not count as
evidence about description quality.
