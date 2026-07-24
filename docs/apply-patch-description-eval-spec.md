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

The primary evaluation uses `openai-codex/gpt-5.6-sol` with
`ThinkingConfig::Low` set explicitly. The runner must verify that the resolved
catalog entry supports this effort and that a captured serialized request sends
`low`. It refuses paid runs if either assertion fails. It uses the production
system instructions, provider options, and builtin tool set, except that the
`agent` tool and background bash are disabled for both variants. Sub-agents add
a second model policy and make attribution to the patch description unclear.

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
changes.

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
2. `infrastructure_failed`: provider or transport retries were exhausted, or
   credentials and model resolution failed.
3. `cancelled`: an operator or parent shutdown interrupted the trial.
4. `timed_out`: the predeclared trial timeout expired.
5. `turn_limit`: the predeclared model-turn limit was reached.
6. `model_failed`: the agent received a terminal model error not covered above.
7. `verifier_failed`: verification or the changed-path allowlist failed.
8. `passed`: verification and the allowlist passed.

The first applicable status wins. The first three statuses are invalid trials,
not task failures. `runner_internal` stops the run for investigation.
`infrastructure_failed` and `cancelled` preserve the attempt and rerun the entire
pair with the same seeds at most once. Statuses four through seven set
`task_passed` to false. Status eight sets it to true. This makes the valid-trial
intent-to-treat denominator ordered and exhaustive.

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

An evaluator event collector sums usage from every assistant `MessageEnd`,
including terminal error and aborted messages and attempts that the provider or
agent later retries. It does not use `Agent::accumulated_usage`, which only
accounts for successful turns. Record:

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
It uses paired event-count simulation or enumeration with blinded pooled event
counts and paired correlations. For each nuisance quantity, the planner uses the
point in its one-sided 95% interval that produces the largest required sample.
It does not use a normal approximation or plug in an event probability of zero
when no pilot event was observed. It also requires enough pairs that the score
method's all-zero upper bound can clear each rare-event margin and its all-pass
lower bound can clear the task-success margin. For efficiency, it targets 80%
power to pass the full rule below under each of two alternatives. In each
alternative, one endpoint has a true 10% improvement and the other has no true
change. Pilot sessions are never included in main inference. If the required
fixed sample is impractical, the result is inconclusive. Margins and thresholds
are not weakened.

`--max-cost-usd` is an admission control based on AJ-recorded catalog cost, not
an interrupt. Before the main run, freeze a per-pair reserve from the blinded
pilot's one-sided 95% upper bound for pair cost. The fixed main run starts only
if its budget covers that reserve for every planned pair. During the run, the
runner starts a pair only if the remaining reserve covers both trials, then lets
both finish even if the estimate is exceeded.
`--max-trials` must admit an even number of trials. Both controls are checked
only between adjacent pairs. A process interruption or invalid trial leaves an
incomplete pair. Its records are retained, the whole pair is rerun with the same
seeds, and neither attempt enters main inference. If budget or interruption
prevents the full frozen sample from completing, the partial result is
descriptive and cannot support shipping.

Provider failures are retried according to the normal provider policy and
recorded separately. Exhausted retries make the trial and its pair invalid under
the taxonomy above. If its one pair rerun also fails, the fixed sample is
incomplete and cannot support shipping. Every attempt remains in artifacts.

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
never derives the agent diff from verifier state.

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

The parent flushes each trial record before continuing and writes a complete-pair
marker only after both valid records are durable. Resume skips only marked
complete pairs. An unmarked pair is rerun in full under a new attempt ID while
old records remain immutable. `--max-cost-usd`, `--max-trials`, and `--seed` are
required controls.

## Harness verification

Before any paid run, test the runner with `ScriptedProvider` plus a capturing
provider wrapper. `ScriptedProvider` alone is insufficient because it ignores
the request `Context`. For a fixed synthetic trial identity, capture `Context`,
`StreamOptions`, or the serialized payload through `on_payload` and assert that
the two variants differ only in the exact `apply_patch` description bytes. Also
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
- `ThinkingConfig::Low` present in the captured model request
- API background bash rejected, shell-level background descendants reaped, and
  the task registry quiescent before snapshot
- broker capability unavailable to tool child processes and request limits
  enforced by the parent
- paired workers exposing the same canonical path and identical model context
  outside the description and opaque trial identities
- verifier mutations detected without changing the captured agent diff
- verifier guest unable to reach the model broker, host files, credentials, or
  unrestricted network
- resume rerunning an incomplete pair without duplicating a complete pair
- budget and trial stops occurring only between complete pairs
- all-zero rare-event input producing a nonzero-width score bound

An isolation integration test attempts absolute and parent-relative patch paths,
host-file reads, credential-environment reads, background processes, and network
access. None may escape the disposable guest or expose credentials. The test
also proves that the parent artifact store is not model-writable.

The scripted tests validate accounting and classification. They do not count as
evidence about description quality.
