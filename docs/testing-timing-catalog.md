# Test timing catalog

## Provider-owned HTTP transports

The OpenAI verbosity request matrix runs all 36 combinations over real local
HTTP. Temporary instrumentation at `b23f9a8f` attributed 967–982 ms of its
1.00–1.01 s runtime to constructing 36 HTTP clients, about 97% of the total.
There are no deliberate sleeps in that test.

All four inference adapters retain a lazily initialized transport per provider
handle. Initialization follows early cancellation, credential and model checks.
SDK wrappers stay request-local, so credentials, endpoints, headers, betas and
session identifiers are resolved afresh. In-flight requests retain the
transport without retaining the provider itself. There is no global cache.

AJ already retains provider handles in its model configuration. The verbosity
matrix does the same, once per API, while retaining every request and assertion.
This additionally exercises settings changes on the same provider. Its three
isolated warm runs took **0.106, 0.110 and 0.104 s**. Instrumentation confirmed
**three transport builds**, totalling 80 ms, rather than 36.

The free `stream`/`complete` convenience helpers still create a fresh provider
per call. Callers seeking reuse retain a `provider_for` handle. Direct provider
construction uses `::default()`. SDK `Client::new` retains its independent
transport behavior, with an explicit constructor for supplying a transport.

TLS, proxy and timeout defaults are unchanged, but their initialization and
retention follow provider lifetime rather than each inference. Idle pooled
connections and TLS configuration can outlive a turn. API credentials are not
transport defaults, and transport ownership ends with its provider and active
requests. This reduces construction work without promising lower idle memory.

Two real-socket boundary tests exercise all four APIs. They observe same-socket
reuse after fully consumed 401 responses, credential/header rotation and
clearing, endpoint changes, and independent pools for separate providers.
Concurrent requests establish distinct sockets before cancellation, then
require EOF on the cancelled request and a valid response on the other.
These do not assert that every successful SSE inference reuses its connection.
Fresh-per-request transport mutations in each SDK fail the socket-reuse check.

Independent review approved the ownership and request isolation. The default
workspace passes **4,082 tests**, with 191 ignored. Slow-inclusive passes
**4,264**, with nine ignored. The transport tests passed three additional
repeats. Formatting, all-target Clippy/build, dependency guards and scratch
checks pass. No profiling instrumentation or mutations remain.

With compilation separate, default workspace runs took **18.437 s and
16.925 s**, versus 19.893 s and 18.172 s at `b23f9a8f`. Slow-inclusive wall
times varied from **54.411 to 58.299 s**, versus a 57.034 s baseline. Summed
suite execution was 50.948–51.477 s versus 52.334 s. The focused matrix gain
is much clearer than a precise full-suite speedup claim.

Run the focused checks with:

```sh
RUSTC_BOOTSTRAP=1 ./scripts/check-test-scratch.sh -p aj-models \
  --test verbosity --test provider_transport
```

## Queue-aware stream pacing

The host HTTP writer keeps its 50 ms coalescing window, but does not start
another wait while a reliable frame is queued behind snapshots. It drains
that prefix in FIFO order. Closed queues also drain without starting another
wait. Eligibility is a bounded queue inspection, with no additional buffers,
tasks, timers, counters or cancellation machinery.

### Where teardown time goes

Temporary instrumentation of all 1,342 slow-inclusive `aj` binary tests at
`40be38fa` recorded real and Tokio elapsed time separately. All passed.
These are sums of overlapping calls, not workspace wall-time savings:

| Shutdown boundary | Calls | Summed wall time | p95 | Maximum |
| --- | ---: | ---: | ---: | ---: |
| Gateway HTTP server | 146 | 66 ms | 0.53 ms | 21 ms |
| Gateway control links | 147 | 24 ms | 0.42 ms | 1.16 ms |
| Session host | 458 | 152 ms | 0.57 ms | 1.73 ms |
| Host HTTP server | 289 | 2,750 ms | 50.53 ms | 172 ms |

The gateway's 21 ms maximum was its deliberate stalled-client test, whose
five-second grace ran on virtual time. Ordinary gateway teardown is not a
worthwhile optimization target in this sample. The host HTTP tail instead
shows the cost of pacing queued snapshots even after producer shutdown.
No enrollment, withdrawal or history-cancellation hardening is included.

A repeat with queue-aware pacing ran 1,343 tests successfully. The same 289
host HTTP shutdowns totalled 1,831 ms, with a 51.5 ms maximum instead of
172 ms. The remaining single-window tail is intentionally left alone.

### Completion latency versus resource use

The Linux-only opt-in profile uses real hosts, direct HTTP and gateway HTTP,
generated replies, and one SSE connection carrying one or four sessions.
It verifies complete replies and ordering, and waits for the process's socket
count to return to at most its pre-fixture baseline after cleanup.

Three isolated runs per variant, debug builds, compilation excluded:

| Gateway workload | Maximum completion gap, baseline → queue-aware | Process CPU, baseline → queue-aware | Client SSE body bytes, baseline → queue-aware |
| --- | ---: | ---: | ---: |
| 1 session, 512-byte reply, 8 characters / 20 ms | 78 → 33 ms | 60 → 60 ms | 31,186 → 31,210 |
| 4 sessions, same moderate rate | 383 → 35 ms | 120 → 120 ms | 58,765 → 59,549 |
| 4 sessions, 16 KiB each, 64 characters / 2 ms | 373 → 42 ms | 270 → 250 ms | 418,128 → 425,257 |
| 4 sessions, 16 KiB each, unpaced provider | 402 → 67 ms | 130 → 120 ms | 290,209 → 290,209 |

Cells are medians. Completion gaps compare each HTTP-observed `AgentEnd` with
an independently drained local attachment, then take the largest gap across
sessions. These include scheduling and transport overhead, not just deliberate
pacing, and are not publication-to-display latency guarantees. The moderate
case is synthetic too, not a live-provider benchmark.

CPU includes provider, persistence, observers and both sides of the transport,
but excludes fixture setup/teardown. Counters have 10 ms resolution on this
machine, so the result supports comparable CPU cost, not a CPU improvement
claim. Bytes include attach setup and possible chunk read-ahead, exclude HTTP
headers, and measure only the gateway's client-facing leg. Frame counts refer
to parsed events rather than byte chunks.

A diagnostic that removed pacing entirely cost 1,980 ms CPU and 9,301,563 body
bytes for the four-session fast stream, roughly seven times the CPU and
22 times the traffic of baseline. That diagnostic is not shipped. Direct-host
measurements show the same latency improvement without the gateway hop.

Run the profile alone, never alongside other Cargo or benchmark processes:

```sh
RUSTC_BOOTSTRAP=1 ./scripts/check-test-scratch.sh -p aj --bin aj \
  remote::tests::stream_profile::profile_stream_pacing -- \
  --ignored --exact --nocapture --test-threads=1
getconf CLK_TCK # CPU ticks per second for the printed cpu_ticks counters
```

It stays ignored even with `slow-tests`. Reproducing the baseline requires
removing only the `attachment.can_coalesce()` condition in `frame_stream`.
The no-hold diagnostic bypasses that wait entirely. Restore production code
before running correctness checks.

The default-tier regression uses the real host and HTTP SSE body encoder with
generated completions already queued, then freezes time only for inspection.
It verifies distinct session snapshots and every reliable frame's payload and
order. Removing the eligibility condition fails at the first unwanted pacing
wait. The existing real-socket streaming-burst test checks retained coalescing.

### Verification and iteration time

Independent review approved the implementation and profiling method. Default
workspace: **4,080 passed, 191 ignored**. Slow-inclusive: **4,262 passed,
nine ignored**. The extra ignore is the opt-in profile, not a regression moved
out of normal testing. The deterministic regression also passed ten repeats.
Formatting, all-target Clippy/build, dependency guards and scratch-runner
checks pass. All temporary instrumentation and counterfactual mutations are
removed.

Warm unfiltered default runs took **19.893 s and 18.172 s**, compared with
20.312 s and 18.574 s before this change. Slow-inclusive wall time was
**57.034 s**, compared with 57.173 s, effectively unchanged at this scale.
Compilation was separate and no Cargo jobs competed during measurement.
The important gain is remote completion latency, with a modest default-suite
improvement rather than a large whole-workspace speedup.

## Host fixtures and gateway transport reuse

The session-host harness uses the shared stateless OAuth providers with
independent auth stores. Its live-sub-agent reattach case holds a generated
child stream until the opening bracket has been inspected, then forwards every
generated event. Parent and child scripts no longer depend on scheduling order.
The case takes 77–81 ms instead of about 3 s and joins the default tier.

The two reasoning-selector failures use an explicit reasoning-capable host
catalog rather than expecting one in the ambient catalog. Real model
confirmation, selector input, footer/log checks and session scoping remain.

Each gateway owns a reusable typed HTTP transport, distinct from its bounded
proxy transport. Enrollment, control-link reconnects, attached streams,
previews and history reuse it. TLS/proxy settings and per-request, stream-open
and silence deadlines retain their existing policies. Cancellation still
belongs to each response. Idle pooled sockets may outlive withdrawal without
keeping a subscription or reconnecting the withdrawn host.

A real HTTP/SSE regression observes a stalled proxy request timing out, then
requires fresh control and attached-stream updates and independent response
cancellation. Substituting the bounded proxy transport for the typed transport
fails that check at the control-stream lifetime assertion.

### Verification and measurements

All checks are unfiltered. Default: **4,078 passed, 190 ignored**.
Slow-inclusive: **4,260 passed, eight ignored**. Both scratch guards pass.
The pool/selector cases and promoted gate passed three additional repeats.
Independent review, formatting, all-target Clippy/build, dependency boundaries
and scratch-runner checks pass.

With compilation separate and ordinary parallelism, warm default runs took
20.312 s and **18.574 s**, versus 21.233 s and 20.274 s for `51012090`.
The slow-inclusive run took **57.173 s**, versus 59.994 s. Its summed suite
execution time was 52.564 s, versus 55.069 s. Coverage includes the repaired
selectors and a new transport-lifetime regression rather than skipping them.

### Gateway lifecycle questions

These remain unchanged. The teardown measurements above do not justify
additional lifecycle machinery:

- A real HTTP diagnostic reproduced an enrollment completing and establishing
  a control link after server and gateway shutdown returned. Shutdown does not
  currently fence a pending enrollment handshake from committing.
- History response bodies lack the independent shutdown cancellation used by
  event splices. Source inspection indicates a backpressured history read can
  retain upstream work after shutdown. This needs a dedicated runtime check.
- Withdrawal cancels subscriptions, but an admitted request can resolve its
  target, wait for its upload, then forward after withdrawal. Whether withdrawal
  should revoke such pending work needs an explicit decision.

## Stream fences and fixture setup

Profiling the default binary separated fixture cost from waiting. A fresh
HTTP client took about 27 ms, and each auth fixture constructed two OAuth
clients. `AuthStorage::for_test` reuses the stateless default provider objects
through the existing injection seam. Each store still owns its credential
file, runtime overrides, provider registration map and read counter. An auth
boundary test checks isolation and parity with the default provider registry.
No TLS checks, provider capabilities or production constructors are bypassed.

The generated remote cut sweeps use ordered progress instead of a quiet tail:

- On a quiescent attachment, the first directory frame follows the complete
  block producer, including lifecycle repairs after `CaughtUp`.
- An isolated scripted turn ends at its live idle state. A generated cut past
  that point stops there rather than waiting for 300 ms of silence.
- Calibration still counts actual frames, cuts remain seeded and generated,
  and interruption and missed-transient preconditions remain asserted.

This is not a universal settlement fence. Attachments during running work,
independent background tasks and settings notices need their own completion
conditions. Their generic quiet-tail helper is unchanged.

The task-output viewer advances one polling interval after appending bytes and
after the real adapter confirms the kill. Real time resumes before waiting for
file reads, HTTP responses and terminal input. All three adapters still run.
The stalled-client shutdown test proves actual socket backpressure and upstream
release in real time, then lets the unchanged five-second grace elapse
virtually with the unread client still held. Five isolated repeats went from
6.88–6.93 s to 1.80–2.06 s. These composed cases remain in the slow tier.

### Combined measurements and verification

Compared with `fab69d01`, using normal parallelism and excluding compilation:

| Measurement | Before | After |
| --- | ---: | ---: |
| Warm default run, filtering the two known failures | 23.590 s | 20.110 s |
| Default `aj` binary execution | 12.691 s | 9.227 s |
| Slow-inclusive run, same two filters | 68.032 s | 59.994 s |
| Summed slow-inclusive suite execution | 61.930 s | 55.069 s |
| Task-output scrollback, full run | 4.878 s | 2.168 s |
| Generated tool-turn cuts, full run | 3.780 s | 1.956 s |
| Generated sub-agent cuts, full run | 3.428 s | 2.182 s |
| Generated missed-transient cuts, full run | 3.682 s | 2.450 s |

Two unfiltered warm default runs took 21.233 s and 20.274 s and reproduced
only the two known selector failures: 4,074 passed, two failed, 191 ignored.
With exactly those two filtered, default and slow-inclusive scratch guards
passed. The full run had 4,257 passed and eight ignored. The five changed
timing-sensitive integration cases also passed three further combined repeats.

Independent review, workspace formatting, all-target Clippy with warnings
denied, all-target build, dependency boundaries and scratch-runner contracts
passed. No additional slow tests were promoted. Remaining HTTP-client
construction and transport pacing are real costs, not incidental sleeps.
Production transport reuse would be a separate change, not a reason to bypass
TLS/proxy initialization in tests.

### Coverage limit found by mutation

Suppressing the post-`CaughtUp` conclusion sweep does **not** fail generated
sub-agent convergence: block bracketing already closes the box and the
convergent comparison does not prove delivery of each transient. The dedicated
`a_reattach_with_no_durable_suffix_still_concludes_a_sub_agent` test does fail
at its explicit sweep observation. It remains unchanged. Generated convergence
is not a substitute for that wire-boundary assertion.

## Follow-up sweep

The follow-up to `6e65e588` removes incidental waits across runtime, providers,
persistence, terminal widgets, host publication, Bash, and local/remote UI
fixtures. It promotes **59 more tests** into the default suite:

- 40 interactive tests, including locked-refusal stability and completed-history
  fixtures that keep their generated events without presentation pacing.
- 15 host tests covering publication, coalescing, cancellation equivalence,
  persistence failure containment and wire-codec round trips.
- The process-I/O budget test, real PTY resize test, and two simulated
  unavailable-reap tests.

| Representative test | Audit baseline | Targeted follow-up measurement |
| --- | ---: | ---: |
| Locked refusal across directory updates | 15.252 s | 0.313 s |
| Detached-child cancel/kill equivalence, four scenarios | 9.251 s | 0.328 s |
| Detached-writer persistence failure containment | 5.006 s | 0.114 s |
| Directory process-I/O budget | 5.186 s | 0.328 s |
| Simulated unavailable reap, direct/background | 4.024 s each | About 0.023 s each |
| PTY resize | 0.536 s isolated | 0.069–0.078 s |

The refusal test waits for the final directory transition to reach the UI and
checks that `drive` returns neither a recovery continuation nor a directory
reattach obligation. It no longer relies on a long quiet window to infer that
no retry is scheduled. A mutation that reintroduced refusal retries failed.

Publisher tests retain concurrent receivers and the generated relabel burst.
A mutation that shortened the publisher's coalescing timer by 100 times failed
the frame-count assertion. Real I/O counters, actual PTYs, Bash processes,
signal handling, persisted bytes and generated message streams remain real.
Only established timer/script phases use virtual time.

Instantaneous remote turns exposed a fixture race: host-idle and client-idle
flags could both be true before reply frames arrived. Settlement now also
requires the client's applied durable position to reach the host's position,
plus the lifecycle/task endings. A read-only `test-support` accessor exposes
that applied position, explicitly not a reconnect cursor.

Runtime status changes use pending-wait evidence. Fanout additionally checks
that the registered waker fires. OAuth callbacks use already-bound listeners,
reset rejection checks the real kernel accept queue after the owned request
finishes, and debounce tests check the virtual boundary before and at expiry.
Persistence tests use explicit file/message timestamps rather than ageing
fixtures. A reviewed unbounded late-listener wait is guarded by a timeout.

### Combined verification

Warm default-suite runs took **23.639–24.304 s**, versus 22.524 s for the
first-pass range. Default coverage grew by 59 tests rather than getting a
shorter whole-workspace runtime. The main incremental-development gains are
the affected tests and the expanded fast tier.

The slow-inclusive run took **68.032 s** excluding compilation. Its summed
test-suite execution time fell from **79.140 s to 61.930 s**, about **22%**.
These are normal-parallelism runs, not sums of individual test durations.

- Two unfiltered default runs reproduced only the known reasoning-model
  selector failures documented below: 4,073 passed, two failed, 191 ignored.
- Filtering exactly those two failures: default 4,073 passed / 191 ignored,
  slow-inclusive 4,256 passed / eight ignored. Both scratch guards passed.
- Workspace formatting, all-target Clippy with warnings denied, all-target
  build, dependency-boundary guards and scratch-runner contract checks passed.
- Independent reviews approved the combined changes and the final scripted
  host phases. The late-listener watchdog finding was repaired and re-reviewed.

### Deliberately retained timing

- Real TERM/KILL escalation, capture-pipe drain and escaped-process survival
  windows remain OS integration tests. Their production budgets are unchanged.
- The RTK deadline retains process/PATH isolation. Spill-failure and mid-drain
  tests retain observation/staging where child readiness alone would not prove
  the required failure phase.
- Host idle release and goal elapsed accounting use `std::time::Instant`.
  Filesystem watcher delivery and cross-process lock exclusion are also real.
  These need a separate boundary/design change, not just Tokio clock ticks.
- Eight short sleeps in the synchronous queue tests arrange condvar waiters.
  Waiter registration is not observable without extra coordination or a
  concurrency-model harness. That machinery is not justified solely by these
  small delays.
- Remote quiet tails cover reliable transients beyond the durable cursor.
  Remaining live-stream/backpressure/reconnect cases require their own phase
  fences. Completed-history fixtures are not a substitute for those cases.
- Vendored Code Mode's separate tests are not edited by this workspace sweep.
  Their import boundary and existing virtual-clock/V8-thread coordination need
  to remain intact.

Conditional OS/readiness polls and generous failure deadlines are not fixed
waits. They remain where no existing notification provides equivalent evidence.
The remaining slow inventory is therefore not a claim that every timer literal
still costs its full duration.

## First-pass reductions (`6e65e588`)

The inventory below records the audit baseline at `1bb64ae3`. The optimization
pass uses three narrow mechanisms, without a general simulation framework:

- Completed-history UI fixtures retain every generated demo event but omit
  presentation delays. Live-stream scenarios keep their own staging.
- Autocomplete tests drive the actual host delivery channel until the expected
  rows are visible, instead of sleeping after every byte.
- Task-output tests use the real registry with controlled output/status
  transitions. They prove the read is pending before completion or cancellation.
  The timeout case advances Tokio time to just before and exactly at expiry.

Print's legacy-session setup uses virtual time only for its scripted first
turn. Time resumes before the continuation executes a real Bash child. The
Bash progress test needs no staging delay because it observes a leading-edge
update.

Two remote catch-up tests use a real Bash process blocked on a test-owned
socket instead of 3/5-second sleeps. A held generated child stream establishes
the required live/completed child state before the parent is released. Closing
the socket on failure releases the command without scratch files.

### Tier reassessment

Nineteen tests move into the default fast suite: the fourteen lifecycle/selector
tests installing an unpaced demo, `real_input_new_session_keeps_the_launch_env_armed`
through unpaced disk-session setup, the three autocomplete popup tests, and
`block_timeout_is_not_an_error`.

| Changed group | Baseline | Targeted measurement |
| --- | --- | --- |
| Fourteen lifecycle/selector cases | About 1–2 s each | 100–330 ms each |
| Three autocomplete popup cases | 836–847 ms each | 10–15 ms each |
| Task-output overlap, terminal completion, cancellation | 200/200/100 ms staging plus scheduling | Under 1 ms each |
| Task-output timeout | Full 1 s wall time | Under 1 ms, still verifies the full virtual deadline |
| Legacy print continuation | 1.030 s isolated | 0.141 s isolated |
| Bash leading-edge update | 300 ms staging plus process overhead | About 3 ms |
| Catch-up with tool and child live | 4.357 s | 0.91–0.92 s across five repetitions |
| Catch-up with no durable suffix | 6.372 s | 1.11–1.12 s across five repetitions |

The fourteen lifecycle cases plus three autocomplete cases totalled 19.886 s
of individual test time in the audit run and 2.346 s in the targeted run.
These sums are not elapsed suite savings, since tests run concurrently.

The two remote cases stay in the slow tier because their shared 300 ms quiet
tails remain. Those cover reliable terminal events beyond the durable cursor.
Real process escalation, pipe drain, socket-backpressure and survival windows
also remain slow. Promotion follows removal of the actual delay, not a relaxed
assertion or a shorter failure timeout.

### Iteration cost and verification

The default workspace suite was measured with `--no-fail-fast`, normal test
parallelism and per-test JSON timings. Build time was recorded separately.
The comparable warm runs were **22.419 s before and 22.524 s after**, including
the runner overhead. Summed test-suite times were 18.903 s and 18.865 s.
This is effectively unchanged whole-suite iteration time with nineteen more
tests enabled, not a claimed fast-suite speedup. Targeted reruns of the changed
tests are much faster.

The initial baseline artifact build took 44.328 s. Warm `cargo test --no-run`
checks took about 0.23 s. A run overlapping other Cargo work was excluded from
the timing comparison. Compilation and unrelated fixture setup remain separate
iteration costs.

The unfiltered fast suite passed 4,014 tests, ignored 250, and reproduced only
the two reasoning-model fixture failures documented below. Independent review
approved the test-boundary changes and the real-process gates. The gate's
`/dev/tcp` requires Bash network redirections, available on the Ubuntu CI test
runner. The print review's incidental context-window change was removed by
using phase-local virtual time instead of a provider override.

A temporary mutation delayed the production task-output timeout by an extra
second. The virtual-time test failed at the promised expiry. The mutation was
then removed. Workspace Clippy, all-target build, formatting, dependency guards
and scratch-runner contract checks passed.

With only those two reproduced baseline failures explicitly filtered, the
scratch-guard runs passed **4,014 fast tests** and **4,256 fast-plus-slow tests**,
both without residue. The full run ignored eight opt-in/helper/doctest cases.
Its summed suite time was 79.140 s, compared with 83.966 s across the audit's
baseline targets. This remains a single-run comparison, not a benchmark claim.
The unfiltered workspace is still not green because of the two existing
reasoning-model fixture failures.

## Scope and interpretation

Audit of the workspace at `1bb64ae3`, on 2026-10-26. This catalogs tests whose
cost or reliability depends on time, rather than proposing changes to their
contracts. Source locations refer to that revision. Tests are grouped when
they share the same wait mechanism.

**Best first targets:** unpaced setup for lifecycle/UI tests, explicit gates
instead of 3/5-second shell staging, and the unconditional autocomplete helper.
The shared 300/600 ms settling windows have broad impact but need stronger
completion observations. Real process-grace tests are a separate, higher-risk
category.

Sections: [measurements](#measured-hotspots),
[application](#application-and-composed-host-tests),
[interactive](#interactive-ui-fixtures),
[transport](#remote-transport-and-gateway),
[tools](#shell-tools-and-task-output),
[other crates](#runtime-models-persistence-and-terminal-ui),
[vendor](#separate-vendor-tests-and-exclusions).

- **Fast** means included by the default workspace test command, not necessarily
  quick in practice. **Slow** means enabled by `--features slow-tests`.
- **Paid wait** is time a passing test deliberately consumes. **Poll** can end
  early, but adds scheduling latency. **Guard** is only an upper bound on failure.
- A negative observation such as “no event arrives for 100 ms” pays the full
  timeout. A positive observation wrapped in a 10-second timeout normally does
  not. Long-lived mock tasks and shell `sleep 30` commands may be cancelled,
  so their nominal duration is not automatically the test's duration.
- Virtual Tokio time is not wall time. It does not advance `std::time::Instant`,
  OS processes, filesystem notifications, or another runtime's clock.

## Measured hotspots

One Linux debug-build sample per target, with normal test parallelism and the
slow tier enabled.
These are individual elapsed times, not additive suite costs or estimated
savings. Shared serial-test locks and machine load can inflate individual
results. Compilation is excluded.

| Test | Seconds | Dominant timing mechanism |
| --- | ---: | --- |
| `interactive::tests::a_refusal_is_folded_once_however_long_the_client_waits` | 15.252 | Fixed 15 s negative-observation window |
| `a_cancelled_detached_sub_ends_exactly_as_a_killed_task_does` | 9.251 | Paced scripted streams across four scenarios |
| `gateway::tests::a_shutdown_releases_a_stalled_clients_upstreams` | 7.021 | Real socket stall detection and 5 s server shutdown grace |
| `remote::tests::a_reattach_with_no_durable_suffix_still_concludes_a_sub_agent` | 6.372 | 5 s shell staging plus repeated 300 ms quiet gaps |
| `tools::bash::tests::a_holder_outside_the_group_is_bounded_by_dropping_the_pipes` | 5.207 | Command timeout, capture drain, TERM grace and pipe close window |
| `the_directory_opens_no_logs_at_startup_or_during_refresh` | 5.186 | 4 ms per scripted chunk, real I/O measurement and 20 ms polling |
| `a_detached_writers_persistence_failure_ends_only_its_session` | 5.006 | Paced detached-child fixture |
| `interactive::tests::task_output_scrollback_local_direct_and_gateway` | 4.870 | Repeated 500 ms output-viewer polling across three adapters, plus transport/rendering work |
| `remote::tests::a_cut_with_a_tool_and_a_sub_agent_running_converges` | 4.357 | Paced fixture and repeated quiet gaps |
| `tools::bash::tests::a_cancel_with_a_holder_outside_the_group_is_still_bounded` | 4.225 | Capture drain and process teardown grace windows |
| `cancelling_a_turn_leaves_its_background_task_running` | 4.025 | Fixed 4 s survival observation |
| `tools::bash::tests::direct_command_reap_bound_releases_capture_and_cleanup_ownership` | 4.024 | TERM grace and unavailable-reap timeout |
| `tools::bash::tests::cancelled_background_command_settles_after_an_unavailable_reap` | 4.024 | TERM grace and unavailable-reap timeout |
| `tools::bash::tests::the_drain_escalates_from_sigterm_to_sigkill` | 4.019 | 2 s drain plus 2 s TERM grace |
| `an_unchanged_directory_is_not_published_again` | 3.955 | Paced stream and 600 ms list-settling windows |

The binary suite took 45.650 s. The other crates' test suites totalled
36.243 s, and the binary crate's integration suites totalled 2.073 s.
There were 181 passing tests at or above one second across those runs.
Individual durations are not additive estimates of removable suite time.

### Reproduction and failures

The stable toolchain's libtest JSON/timing switches require opting into unstable
options. No source or test behavior was changed for measurement:

```sh
RUSTC_BOOTSTRAP=1 ./scripts/check-test-scratch.sh \
  --workspace --features slow-tests -- \
  -Z unstable-options --format json --report-time

RUSTC_BOOTSTRAP=1 ./scripts/check-test-scratch.sh \
  --workspace --exclude aj --features slow-tests -- \
  -Z unstable-options --format json --report-time

RUSTC_BOOTSTRAP=1 ./scripts/check-test-scratch.sh \
  -p aj --features slow-tests \
  --test label_rule --test serve_shutdown --test session_env -- \
  -Z unstable-options --format json --report-time
```

The first command stopped at the binary suite with 1,339 passed and two failed.
An exact rerun reproduced both failures, with a 45.872 s suite duration:

- `interactive::tests::palette_selector_cancel_returns_to_palette_confirm_returns_to_editor`
- `interactive::tests::thinking_selector_updates_footer_and_is_session_scoped`

Both fail at `src/aj/src/interactive.rs:18497` with “host catalog offers a
reasoning model”. They fail in about 0.1 s, not on a timing assertion. Their root
cause is outside this timing audit. The second command passed 2,907 tests with
eight ignored. The third covers the ten integration tests skipped after the
binary unit-suite failure. All ten passed. Both successful runs left no scratch
residue. Together: **4,256 passed, two failed, eight ignored**. The workspace is
therefore **not green**. Ignored live-provider tests were not enabled. The
catalog does not include a cold-build performance analysis.

Isolated scratch-guard reruns confirmed the legacy-print test at 1.030 s and
the PTY resize test at 0.536 s. Running the PTY target with `--test-threads=1`
reduced each other PTY test to about 0.030 s, exposing serial-lock waiting in
their parallel-run measurements. Formatting and catalog-reference checks passed.

## Application and composed host tests

In the next three sections, **H** means `src/aj-app/tests/session_host.rs`,
**I** means `src/aj/src/interactive.rs`, **R** means
`src/aj/src/remote/tests.rs`, and **G** means `src/aj/src/gateway/tests.rs`.
Ranges identify shared-fixture families rather than claiming every test in the
range waits. Source-derived durations distinguish configured pacing from
unconditional waiting.

| Tests or family | Location | Tier | Timing and replacement direction |
| --- | --- | --- | --- |
| `print_continue_of_a_legacy_log_ignores_armed_env` | `src/aj-app/src/print.rs:1026` | Fast | Unpaused `streaming-text` setup costs about 1 s, measured 1.030 s alone. Unlike nearby paused print tests, this also runs a real Bash child. Use unpaced scripted setup rather than blindly pausing OS work. |
| `watch_user_theme_delivers_reparsed_theme_on_file_edit` | `src/aj-app/src/theme.rs:1311` | Fast | Real filesystem delivery plus 100 ms debounce, 5 s guard. Retain real watcher smoke coverage, isolate debounce policy with controlled events/time. |
| `watch_user_theme_ignores_unrelated_files_in_same_dir`, `watch_user_theme_swallows_parse_errors_silently` | `src/aj-app/src/theme.rs:1346`, `:1376` | Fast | 500 ms negative receive, 300 ms invalid-save staging followed by debounce. Acknowledge processed unrelated/invalid events before asserting absence or issuing the valid save. |
| `collect_fetches_every_account_and_times_out_one_without_hiding_its_sibling` | `src/aj-app/src/usage.rs:608` | Fast | Full 100 ms timeout. Pause after account setup and drive the actual timeout. |
| `continuation_rerun_restarts_the_runtime_clock`, `canonical_form_ignores_instants_and_active_view` | `src/aj-app/src/chat/reducer.rs:2788`, `:5017` | Fast | 2 ms thread sleeps. Give states explicitly different instants. |
| `a_parked_stream_is_woken_by_a_frame_and_ended_by_a_close` | `src/aj-app/src/host/fanout.rs:1113` | Fast | Two 20 ms sleeps. Observe receiver pending/waker registration before publish/close, preserving lost-wakeup coverage. |
| Turn-compaction tests using `recording_config` | `src/aj-app/src/turn/compaction_tests.rs:57` | Fast | 1 ms per generated delta provides cancellation checkpoints. Gate a cancellable stream boundary rather than substituting an uncancellable immediate replay. |
| `concurrent_config_edits_preserve_disables_and_keep_readers_available` | H:304 | Slow | 250 ms blocked-write observation, reader response bound 2 s. Acknowledge pending writes and prove readers finish while lock remains held. |
| `a_crashed_rivals_hold_falls_away_on_its_own` | H:2224 | Slow | Next 2 s lock-probe tick, 100 ms polling. Confirm real child death, then explicitly drive the probe clock. |
| Idle-release/retention/takeover group | H:3689–4886, H:10296–10988 | Slow | 200 ms grace, roughly 100 ms sweeps, 10 ms release polls. Negative retention windows include 400/600/800/1,000 ms. Control sweeps and their completion, retaining real lock/log ownership. Not all idle clocks are Tokio clocks. |
| `a_release_does_not_stamp_a_session_with_its_own_flush` | H:3997 | Slow | Full 1.2 s to separate activity time from flush mtime. Explicit activity time plus observed flush must preserve proof that mtime is later. |
| `work_between_two_ticks_restarts_the_grace`, `a_command_racing_a_release_is_neither_lost_nor_doubled` | H:4226, H:4465 | Slow | Two 150 ms sleeps, and twenty 5 ms sleeps respectively. Coordinate actual work/release boundaries. Preserve proof commands crossed real cold-session transitions. |
| Directory publication/refresh/tag/archive/codec group | H:2689, H:9226–10241, H:10641–10700, H:13131–13172 | Slow | Shared `LIST_SETTLE = 600 ms`, frequently twice, against a 200 ms publisher. Positive cases can await relevant publication. Negative cases need acknowledged publisher windows. |
| `an_unchanged_directory_is_not_published_again` | H:9774 | Slow | Paced answer plus 600 ms settle. Drive multiple publisher ticks with genuinely unchanged rows, then a real change. |
| `streaming_deltas_alone_do_not_wake_the_list_publisher` | H:9298 | Slow | 600 ms settling plus 400 ms measurement overlaps 15 ms/character streaming. Preserve at least five updates and no durable event during measurement. |
| `distinct_directories_inside_one_window_reach_a_client_as_one_frame` | H:9409 | Slow | Two 600 ms waits plus twenty-four 5 ms mutation sleeps. Keep concurrently reading client and distinct labels so queue replacement cannot masquerade as publisher coalescing. |
| Paced busy/head/queue/steer/cancellation/attach/persistence families | H:2917–2955, H:4893–6531, H:7895, H:8196–8994, H:11666–11777 | Slow | Usually 5–30 ms per character/argument chunk. `a_cancelled_detached_sub_ends_exactly_as_a_killed_task_does` at H:6227 runs four scenarios. `a_detached_writers_persistence_failure_ends_only_its_session` at H:7897 consumes paced detached work. Gate the required live phase, act, then release. |
| `every_queue_mutation_publishes_an_update`, `a_cancel_stops_the_turn_and_publishes_its_notice`, `shutdown_cancels_gracefully_and_flushes`, `a_cancelled_turn_publishes_its_terminal_message`, `a_driven_foreground_turn_is_part_of_session_cleanup_ownership` | H:5120, H:5716, H:11376, H:11578, H:12194 | Fast | 20/30/40/40/20 ms configured chunks. Only the consumed prefix before cancellation is paid. Use explicit provider-phase gates. |
| `waiting_counts_toward_goal_time_without_running_work` | `src/aj-app/tests/session_host/goals/wait.rs:128` | Slow | Full 1.1 s elapsed-time observation. Goal accounting uses `std::time::Instant`, so Tokio pause alone cannot fix this. Keep composed no-extra-work assertions. |
| `goal_main_cancel_pauses_but_attaching_to_live_work_does_not` | `src/aj-app/tests/session_host/goals.rs:1332` | Fast | Configured 1 s/character, cancelled after turn start and attachment. Full response duration is not paid. Hold inference explicitly. |
| `the_directory_opens_no_logs_at_startup_or_during_refresh` | `src/aj-app/tests/list_refresh_io.rs:169` | Slow | 1,000 chunks at 4 ms each, nominal 4 s, measured 5.186 s. 20 ms polls under 30 s guard. Keep real process I/O counters and repeated live refreshes. Drive gated bursts and publisher windows, not a trivial immediate turn. |
| `host_drop_reaps_foreground_bash_before_releasing_the_lock` | H:12244 | Slow | Real 2 s TERM grace and 5/10 ms polls. Preserve real process death/reaping before session lock release. |
| `concurrent_shutdown_does_not_close_the_owners_fanout_early`, `shutdown_closes_an_attach_block_waiting_on_the_log` | H:12558, H:12692 | Fast | 20 ms negative observations. Acknowledge parked receiver/producer state while owner flush is held. |

The host's common lifecycle/task/persistence waits poll every 5–10 ms under a
20 s guard (H:732–814). Goal-driver waits similarly poll every 5 ms under a
10 s guard. These are cumulative polling costs, not 10–20 s fixed waits.

## Interactive UI fixtures

| Tests or family | Location | Tier | Timing and replacement direction |
| --- | --- | --- | --- |
| `autocomplete_popup_is_an_overlay_above_the_fixed_editor`, `at_file_autocomplete_end_to_end_through_the_shell`, `the_completion_popup_clears_the_strip` | I:9217–9492, I:26642 | Slow | Helper always sleeps eighty times for 1 ms per typed byte, with no early success exit. Await matcher/walker quiescence or expected results with a failure guard. |
| `quit_arm_times_out_and_disarms` | I:12511 | Fast | 2 ms thread sleep despite zero timeout. Use an explicitly due timer. |
| `a_selected_local_target_does_not_mask_host_loss` | I:13160 | Fast | 100 ms stop future, paid even if drive exits earlier because helper joins both futures. Stop on observed drive completion. |
| Tests marked “paced streaming-text demo” | I:10503–28998, I:29645, I:32035–34217 | Slow | Demo uses 25 ms per eight-character chunk, roughly 0.9 s per complete reply. Multiple seed turns multiply this. Use the same generated content without pacing for persisted-history/UI-only cases. Gate live phases when busy state matters. |
| Parallel-agent replay/resume/view-switch group | I:9981–10313, I:25742, I:32795 | Slow | 25 ms chunks, 200 ms section gaps, real short Bash calls. Preserve generated subagent structure and child-consumption ordering, not presentation speed. |
| `background_task_completion_wakes_the_agent` | I:13464 | Slow | Demo pacing plus real shell `sleep 1`, partly overlapping parent work. Release real child after parent idle instead. |
| `attaching_over_a_running_background_sub_counts_it_in_the_footer` | I:28784 | Slow | 1 s shell staging, paced text, configured 10 s child delay interrupted by shutdown. Acknowledge child start and hold it explicitly. |
| Busy-script refusal/steering family | I:9716, I:32739 | Mostly slow | 50 ms/character, about 1.2 s if entire reply is consumed. Gate inference. |
| Directory mirror/tag/host-picker families | I:26088, I:27532–28098, I:34492–35088 | Slow | Real 1 s host publisher window, sometimes repeated. Use configurable coalescing or publication acknowledgment for UI-only cases, retain dedicated cadence coverage. |
| `enter_alone_on_the_picker_creates_nowhere`, `cancelling_the_picker_mints_nothing` | I:34750, I:34854 | Slow | 500 ms negative drive timeout each. Acknowledge input and completion of admitted create work before checking both hosts. |
| Background-session recovery and retention family | I:22714–23166, I:24085 | Slow | 150 ms quiet helper, 200 ms grace. `the_loop_discharges_a_background_session_s_re_attach` adds 2 s, `a_switch_keeps_the_outgoing_session_attached_and_live` adds 800 ms. Observe recovery and explicitly drive sweeps. |
| WarmPeer attach/history/rejoin fixtures | I:29220–30199, I:30462–30628, I:30719–31402 | Mixed | Sleep after each frame: often 500 ms for slow attach, 150 ms recovery, 10–60 ms for fast cases. Gate frames until intended input/state observations complete. Large silence caps are not necessarily paid. |
| `the_loop_reads_input_while_a_block_arrives` | I:30786 | Slow | 200 ms/frame, more than 1 s incomplete block. Hold block while input is acknowledged, then finish it. Keep silence-deadline policy covered separately. |
| `a_stream_open_failure_keeps_the_target_selected_and_reconnecting`, `the_loop_paces_a_stream_that_dies_inside_the_block` | I:30259, I:30909 | Slow | Full 2 s observation windows. Drive retry deadlines and count acknowledged attempts rather than wall time. |
| `an_outage_is_one_warning_row_rewritten_by_each_retry` | I:30330 | Slow | Reconnect backoff starts at 200 ms and doubles toward 5 s. Three attempts plus another outage. Advance only after each rendered attempt is observed. |
| `a_refusal_is_folded_once_however_long_the_client_waits` | I:31285 | Slow | Full 15 s after refusal. Highest single fixed wait. Exercise directory transitions and several maximum backoffs under controlled time, asserting one stream open and one refusal row. |
| `task_output_scrollback_local_direct_and_gateway` | I:32869 | Slow | Real 500 ms output-viewer polling across three adapters and live/completed views. Drive polling after acknowledged output/status mutations, retaining transports and scrollback rendering. |
| `selector_parity_cancel_and_reopen_isolates_delayed_host_fill` | `src/aj/src/interactive/tests/selector_parity.rs:337` | Slow | 50 ms while request is held. Await peer request acceptance before closing/reopening. |
| `settings_parity_stalled_open_keeps_drive_responsive_and_ignores_late_reply` | `src/aj/src/interactive/tests/settings_parity.rs:945` | Slow | Full 1 s negative observation with 1 ms polls. Acknowledge late-reply processing before checking overlay identity/content. |

Other UI settling helpers use 1/5/10/20 ms polling with mostly 20 s guards.
Autocomplete's `drive_to_quiescent` (`src/aj/src/autocomplete/tests.rs:985`)
polls at 2 ms, at most 200 iterations plus matcher work, and exits early.
Usage-overlay `wait_for` (`src/aj/src/usage_overlay.rs:1386`) polls the thread
every 5 ms up to 200 iterations. Adapter parity tests retain real local/direct/
gateway behavior and mostly poll for state, rather than consuming whole caps.

## Remote transport and gateway

| Tests or family | Location | Tier | Timing and replacement direction |
| --- | --- | --- | --- |
| Catch-up/creation/head/settings/env/convergence using `Attached::settle` | R:1077–1137 and callers | Slow | Full 300 ms quiet tail per settle, restarted by each frame. Durable-cursor equality is insufficient because terminal reliable transients follow it. Replacement must fence those too. |
| `an_authorized_peer_reaches_every_route` | R:4411 | Fast | Two inference probes use settle, hence at least two 300 ms quiet tails despite being fast-tier. Explicitly observe prompt/steer completion. |
| `defaultable_unknown_fields_leave_session_state_untouched` | R:2138 | Slow | 300 ms sleep. A subsequent ordered operation may provide a processing fence if it really orders all relevant work. |
| `tool_turn_stream_cuts_at_seeded_boundaries_converge`, `sub_agent_stream_cuts_at_seeded_boundaries_converge`, `stream_cuts_converge_without_replaying_notices_raised_while_away` | R:4665–4988 | Slow | 2 ms chunks, calibration and generated cuts, 300 ms quiet tails, extra tail if cut exceeds available frames. Preserve generated coverage and actual missed-transient preconditions. Gate at generated boundaries, do not replace with fixed examples. |
| `a_cut_with_a_tool_and_a_sub_agent_running_converges`, `a_reattach_with_no_durable_suffix_still_concludes_a_sub_agent` | R:5032, R:5181 | Slow | Real shell 3 s/5 s staging plus quiet tails. Hold parent process on explicit release until cut/empty-suffix assertions finish. |
| `an_idle_stream_heartbeats_with_a_real_frame`, `an_idle_gateway_stream_heartbeats` | R:3309, G:3749 | Slow | Two 80 ms/two 50 ms intervals. Establish real stream, then advance and acknowledge heartbeat frames. |
| `a_streaming_burst_reaches_the_client_coalesced` | R:3350 | Slow | 300 chunks at 1 ms and 50 ms production lossy pace. Controlled bursts must keep the receiver reading, preserving actual coalescing evidence. |
| `a_stalled_block_withholds_the_heartbeat` | R:3438 | Slow | Full 600 ms silence timeout against 80 ms heartbeat. Hold real log lock and advance established stream's clock. |
| `a_silent_stream_is_reported_dead`, `a_silent_stream_is_reported_dead_to_a_polling_client`, `opening_a_stream_against_a_mute_host_is_abandoned` | R:3508, R:3560, R:3615 | Slow | 200 ms silence, 900 ms observation with 25 ms polls, 200 ms open timeout. Preserve proof that repeated cancelled receives do not reset the stream-owned deadline. |
| Gateway duplicate-directory/reset/reconnect suppression families | G:3327, G:3826, G:5337, G:5380, G:5914 | Slow | 400 ms per `frames_within(QUIET)`, sometimes twice. Observe processing, then exercise the relevant quiet/retry windows. |
| `a_spliced_upstream_carries_no_list_frames` | G:4733 | Slow | Actual 200 ms host publication. Already uses a later turn as a transport ordering fence. Keep it while controlling publisher time. |
| `prompt_history_all_retains_failed_and_timed_out_host_snapshots`, `a_hung_host_costs_only_its_own_sessions` | G:2992, G:6826 | Slow | Full 3 s upstream timeout. Hung-session test adds 400 ms quiet tail. Observe healthy/partial results before advancing stalled request timeout. |
| `a_host_that_answers_nothing_becomes_a_503` | G:7841 | Slow | Full 300 ms request timeout. Acknowledge accepted request, advance timeout, retain actual response mapping. |
| `a_host_that_hangs_up_at_once_is_not_redialed_at_the_floor_rate` | G:7947 | Slow | Full 2 s window, 5 ms retry floor and 200 ms ceiling. Drive completed handshake attempts across retry deadlines. |
| Real socket-backpressure tests: withdrawal, oversized block, cross-host live frame, two sessions, stalled-client shutdown | G:6171, G:6498, G:6559, G:6659, G:6953 | Slow | `until_stalled` samples every 250 ms and requires at least two samples. Some add 400 ms quiet windows. Stalled shutdown additionally pays server's 5 s grace. Preserve real sockets/deep generated blocks, investigate explicit pump-park/write-progress observation. |
| `a_slow_attach_does_not_lose_another_hosts_block` | G:6729 | Slow | Large real backfill and 10 ms progress polls, no fixed stall-sampling helper. Preserve held peer and generated backfill. |
| `a_preview_batch_outwaits_the_gateways_stalled_upstream` | G:1012 | Slow, hybrid | The 30 s deadline is advanced virtually in one-second steps. Each iteration pays 10 ms real sleep, nominally at most about 310 ms. Do not count this as 30 s wall time. |

`src/aj/tests/serve_shutdown.rs` keeps real process/socket/signal coverage.
The two second-signal tests at `:189` and `:195` pay 20 ms negative reads while
holding incomplete requests. `one_sigterm_closes_an_attached_stream_without_spending_the_server_grace`
at `:215` can wait for the initial 1 s list publication and a 20 ms negative read.
Exit budgets of 2–5 s are guards, not mandatory waits. Session-env subprocess
helpers (`src/aj/tests/session_env.rs:28`) poll exit every 10 ms under 5 s guards.
The slow `print_continue_reports_that_env_is_create_only_without_backfilling_the_log`
at `:158` consumes two paced demo turns in separate processes.

## Shell tools and task output

Locations in this section are relative to `src/aj-tools/`. Times are source-derived
successful-path waits, not measured totals. This crate has 20 slow-tier tests.

### Artificial staging and observation windows

| Tests | Location | Tier | Time paid and next investigation |
| --- | --- | --- | --- |
| `emit_update_fires_during_execution` | `src/tools/bash.rs:4015` | Fast | 300 ms shell sleep. Only requires at least one update, and the implementation emits a leading-edge update. The sleep appears unnecessary for the asserted contract. |
| `cancellation_kills_command_and_marks_error` | `src/tools/bash.rs:2665` | Fast | 150 ms before cancellation. Wait for child readiness instead. The 30 s command is killed, not awaited. |
| `background_started_result_carries_id_and_spill_path` | `src/tools/bash.rs:4243` | Fast | 200 ms before command completion. Gate completion until after started-result assertions. |
| `background_ignores_timeout` | `src/tools/bash.rs:4396` | Fast | 300 ms shell sleep. Explicitly hold and release the process, preserving proof that the foreground timeout policy does not apply. |
| `stateless_reads_return_overlapping_tails` | `src/tools/task.rs:490` | Fast | 200 ms between output lines plus 25 ms polling. Release second output after the first read, giving a stronger ordering guarantee. |
| `block_resolves_on_terminal_status` | `src/tools/task.rs:517` | Fast | 200 ms shell sleep. Establish a pending read, then explicitly finish the task. |
| `turn_cancel_cancels_the_wait_not_the_task` | `src/tools/task.rs:574` | Fast | 100 ms before cancellation. Poll the output wait to pending, then cancel it. |
| `block_timeout_is_not_an_error` | `src/tools/task.rs:550` | Slow | Full 1 s blocking-read timeout. Controlled registry task plus virtual time can test this without shell timing. |
| `the_grace_outlives_a_leader_that_exits_at_once` | `src/tools/bash.rs:3623` | Fast | 500 ms in a descendant's TERM handler. Release the handler after observing leader exit instead. Retain real signals and descendants. |
| `a_drop_during_the_drain_still_kills_the_group` | `src/tools/bash.rs:3254` | Slow | 300 ms timeout intentionally drops the tool call. Observe leader exit and drain readiness instead. Holder readiness alone is insufficient. |
| `a_straggler_that_let_go_of_the_pipes_is_left_alone` | `src/tools/bash.rs:2967` | Slow | Full 300 ms survival window. Requires a cleanup-completion fence and subsequent process acknowledgment to replace safely. |
| `a_spill_failure_does_not_leak_the_command` | `src/tools/bash.rs:3402` | Slow | 1 s positive-control command plus 1.5 s leak-observation window. Observe the actual failed-path process exiting rather than only delayed marker absence. Preserve the positive control. |
| `cancelling_a_turn_leaves_its_background_task_running` | `tests/cancel_teardown.rs:352` | Slow | Full 4 s survival window. This covers the possible teardown horizon, so immediate liveness is not equivalent. |
| `rtk_passthrough_runs_the_hook_answer_or_original_command` | `src/tools/bash.rs:1977` | Slow | 500 ms hook timeout and nine isolated test-binary launches. `rtk_passthrough_child` at `:2043` is permanently ignored and invoked by the parent. Keep PATH isolation, consider separating timer expiry from response cases. |

### Real teardown windows

These pin different observable boundaries, not interchangeable copies of one
test. The production constants are `KILL_GRACE = 2 s`,
`CAPTURE_DRAIN_GRACE = 2 s`, and `CAPTURE_CLOSE_GRACE = 200 ms`
(`src/tools/bash.rs:161–176`). Direct termination can return early when the
leader exits. Detached guard teardown deliberately sleeps through the 2 s grace.
Capture drain can consume two 2 s rounds plus a 200 ms close window.

| Tests | Location | Tier | Time paid and constraint |
| --- | --- | --- | --- |
| `a_holder_outside_the_group_is_bounded_by_dropping_the_pipes` | `src/tools/bash.rs:3044` | Slow | About 5.2 s: 1 s command timeout plus 4.2 s capture-drain sequence. Escaped holder cannot be killed via the command's group. |
| `a_cancel_with_a_holder_outside_the_group_is_still_bounded` | `src/tools/bash.rs:3319` | Slow | About 4.2 s after cancellation. Same escaped-descriptor problem through the cancellation path. |
| `direct_command_reap_bound_releases_capture_and_cleanup_ownership`, `cancelled_background_command_settles_after_an_unavailable_reap` | `src/tools/bash.rs:2097`, `:2175` | Slow | 4 s each, two full 2 s termination/reap windows. Unavailable reap is simulated with a live child and nonexistent process-group ID. Controlled clock advancement looks more feasible here than in real signal-escalation cases. |
| `the_drain_escalates_from_sigterm_to_sigkill` | `src/tools/bash.rs:3189` | Slow | About 4 s. Preserve real TERM acknowledgment, KILL and pipe EOF. |
| `timeout_kills_command_and_marks_error` | `src/tools/bash.rs:2789` | Slow | At least 3 s: 1 s command timeout plus 2 s TERM grace. Real TERM-immune process and final error report. |
| `a_reaped_leader_still_gets_its_group_killed` | `src/tools/bash.rs:3684` | Slow | About 2.3 s: 300 ms staging followed by 2 s detached grace. Replace staging, retain descendant escalation. |
| `cancellation_escalates_to_sigkill_when_sigterm_is_ignored`, `a_dropped_command_that_ignores_sigterm_is_killed` | `src/tools/bash.rs:2715`, `:3462` | Slow | About 2 s each. Direct cancellation and dropped-future ownership are distinct paths. |
| `a_dropped_command_releases_its_capture_readers` | `src/tools/bash.rs:3506` | Slow | About 2 s. Readers are synthetic pending tasks, making controlled clock advancement plausible once the live-child fixture is established. |
| `a_dropped_call_releases_the_pipes_the_tool_opened` | `src/tools/bash.rs:3571` | Slow | About 2 s plus writer polling. Real escaped writer observes that the tool released its read ends. |
| `descendant_holding_a_pipe_does_not_hold_the_turn_open`, `a_straggler_holding_one_stream_is_drained_like_any_other`, `background_task_with_a_pipe_holder_still_finishes` | `src/tools/bash.rs:2859`, `:3116`, `:4329` | Slow | About 2 s each. Foreground, asymmetric stdout/stderr and background completion. Later drain rounds normally return early at EOF. |

Automatic virtual-clock advancement is unsafe around real child processes:
Tokio can see its own runtime as idle while the OS child has not yet run.
Any clock-driven replacement needs readiness and signal/EOF acknowledgments,
not just `start_paused` on these tests.

### Polls and guards that are not fixed delays

| Tests or helper | Location | Behavior |
| --- | --- | --- |
| `wait_until`, `drop_when_ready`, `wait_for` | `src/tools/bash.rs:3970`, `:3989`, `:4229` | 20 ms condition polling up to nominal 8 s, 10 ms readiness polling up to nominal 4 s, and 25 ms polling up to nominal 5 s. Prefer existing event/terminal notifications where they observe the right boundary. |
| `cancelling_a_turn_kills_the_commands_process_group`, `a_cancelled_command_gets_its_sigterm_handler` | `tests/cancel_teardown.rs:232`, `:293` | Fast. 20 ms readiness/death polls with 10 s/8 s guards. Do not necessarily pay the full detached grace. Keep real composed cancellation coverage. |
| `a_guard_dropped_without_a_runtime_still_kills_the_group`, `a_guard_dropped_by_a_dying_runtime_still_signals` | `src/tools/bash.rs:3752`, `:3824` | Fast. Blocking-thread readiness/death polls. Runtime absence is the contract, so an async-only substitute would miss it. |
| `task_stop_kills_the_process_group` | `src/tools/task.rs:661` | Fast. Spawns `pgrep` every 25 ms, bounded by 5 s per phase. Track actual child PID and exit instead. Long shell sleep is killed early. |
| `background_driver_emits_task_start_before_task_end`, `background_spill_file_readable_while_running` | `src/tools/bash.rs:4361`, `:4414` | Fast. 25 ms event/output polling. Notifications can replace cadence. |
| `direct_bash_calls_overlap_with_isolated_output`, `promise_all_bash_calls_overlap_with_isolated_output` | `tests/bash_concurrency.rs:127`, `:132` | Fast. 10 ms shell marker polling, 5 s command guard. Already proves structural overlap rather than relying on elapsed speed. Low priority. |
| Task ownership/listing tests, session-env tests, apply-patch interface tests | `src/tools/task.rs`, `tests/session_env.rs`, `tests/apply_patch_interface.rs` | Long shell fixtures are cancelled, or finite commands run under early-return guards. Do not count nominal 20–30 s caps as test cost. |

## Runtime, models, persistence and terminal UI

Paths in this section are relative to `src/`. Unless marked otherwise, these
tests run in the default fast tier.

| Tests | Location | Time paid or timing dependence | Next investigation |
| --- | --- | --- | --- |
| `wait_terminal_resolves_on_status_flip`, `quiesce_resolves_when_all_entries_are_terminal` | `aj-agent/src/lib.rs:3340`, `:3371` | 50 ms staging each | Poll waiter to pending before flipping status, or pause time. |
| `terminal_status_does_not_quiesce_before_the_detached_driver_returns` | `aj-agent/src/lib.rs:3397` | 10 ms negative observation | Existing start/release latches make virtual time straightforward. |
| `truncated_turn_is_retried_then_succeeds` | `aj-agent/src/lib.rs:6372` | Production jittered retry, first backoff based on 100 ms | Pause Tokio time, retain real retry logic. |
| `parallel_results_keep_call_order_despite_skew`, `sequential_tools_never_overlap`, `concurrency_cap_bounds_in_flight_calls` | `aj-agent/src/lib.rs:8384`, `:8421`, `:8455` | 60 ms skew, two 20 ms sequential delays, two waves of 40 ms probes | Admission/release gates can prove ordering and cap without scheduler-speed assumptions. |
| `background_spawn_delivers_report_notice`, `background_spawn_failure_delivers_failed_notice` | `aj-agent/src/lib.rs:7296`, `:7423` | 10 ms polls up to nominal 5 s | Await registry completion or notice notifications. |
| Four `callback_server_*` tests | `aj-models/src/oauth/callback.rs:277–342` | 20 ms staging each | Listener is already bound. Remove staging and rely on socket connection semantics. |
| OAuth `exchange_authorization_code_round_trips`, `refresh_with_round_trips`, both providers | `aj-models/src/oauth/anthropic.rs:615`, `:655`, `aj-models/src/oauth/openai.rs:643`, `:701` | Request-capture helper polls at 10 ms, nominal 500 ms cap | Request is captured before responding, so a completed round trip should already have it. |
| `reset_post_keeps_roundtripped_offer_binding_across_account_changes`, `stale_reset_target_sends_no_post` | `aj-models/src/usage.rs:1499`, `:1631` | Two 250 ms negative accepts, one 250 ms negative accept | After operation completion, determine whether any detached dispatch is possible before replacing with immediate socket observation. |
| `refresh_excludes_other_process_writes_until_completion_or_death` | `aj-models/src/auth.rs:2530` | Two 100 ms negative windows, 10 ms child polls, conditional production lock backoff | Preserve real cross-process exclusion. Child IPC can replace readiness polling. |
| `concurrent_writes_serialize_via_lock` | `aj-models/src/auth.rs:2451` | Conditional 20–500 ms production lock backoff, not a guaranteed delay | Measure contention before changing real locking coverage. |
| `cancellation_during_inter_event_delay_emits_aborted_terminal`, `explicit_call_kind_survives_streaming_and_cancellation` | `aj-models/src/scripted.rs:1069`, `:1425` | 20 ms cancellation staging, 10 ms/chunk in cancelled variants | Virtual time or controlled event release. The 60 s terminal delay is cancelled. |
| `an_archived_bit_round_trips_and_clears` | `aj-session/src/persistence.rs:970` | 10 ms for mtime distinction | Set an explicitly old mtime before testing that no rewrite occurs. |
| `previews_carry_the_labels_the_sidecars_hold`, `previews_carry_the_archived_bit` | `aj-session/src/persistence.rs:1641`, `:1667` | Three/two 2 ms filename-spacing sleeps | Creation already handles filename collisions. Remove sleeps, keep distinct-ID assertions. |
| `session_preview_uses_largest_message_timestamp` | `aj-session/src/persistence.rs:1903` | 20 ms elapsed timestamp staging | Persist explicitly chosen timestamps in deliberately different order. |
| `closing_a_persistence_fence_drains_admitted_writers_and_rejects_late_snapshots` | `aj-session/src/listener.rs:509` | Scheduler yields and full 20 ms negative timeout | Explicit admission acknowledgment and pending-close observation. |
| `fill_block_fill_block`, `drain_block_drain_block`, `two_readers`, `drain_wakes_blocked_producer` | `vaxis/src/queue.rs:367`, `:433`, `:451`, `:531` | Three 10 ms sleeps in each first pair, one 10 ms sleep in each latter test. First asserts blocking longer than 5 ms. Unbounded yield loops also exist. | Synchronize waiter registration/rechecks. Preserve spurious-wake coverage, not just eventual output. |
| `timeout_tick_disarms_the_sequence`, `ctrl_c_ladder` | `vaxis/src/vxfw/keymap.rs:849`, `:918` | 1 ms each despite zero timeout | Use explicit due time or verify zero-deadline semantics need no sleep. |
| `debounces_at_autocomplete_while_typing`, `debounces_hash_autocomplete_while_typing` and at-token popup tests | `vaxis/src/vxfw/text_area.rs:5847`, `:5916`, `:6126–6274` | Production 20 ms debounce, repeated when `type_settle` waits after each character | Pause time and advance across the debounce boundary. |
| `spawn_prints_hello_and_exits`, `osc_sets_window_title` | `vaxis/tests/terminal_emulator.rs:87`, `:123` | 20 ms polls with 3 s guards, serial-test lock | About 30 ms each in serial rerun, not the 600/567 ms shown during parallel measurement. Most apparent cost was waiting behind the resize test. |
| `resize_updates_child_reported_size` | `vaxis/tests/terminal_emulator.rs:159` | **Slow**, shell `sleep 0.5` before `stty size`, measured 536 ms serially | Gate child input until the parent has resized the PTY. |
| `recover_writes_reset_and_restores_termios` | `vaxis/src/tty/posix.rs:623` | 5 ms `EAGAIN` polling under 1 s guard | Readiness notification could replace cadence. Keep real PTY behavior. |

`recent_history_paces_snapshots_and_ends_complete`
(`aj-session/src/prompt_history.rs:577`) has no sleep but depends on a 100 ms
elapsed-time publication gate while scanning 64 files. It is a determinism
candidate, not a fixed-wait reduction. Likewise token-expiry, Retry-After,
render-duration and diff-deadline assertions use time without deliberately
waiting. Diff's 100 ms budget is a computational ceiling, not a fixed delay.

## Separate vendor tests and exclusions

`vendor/codex-code-mode` is excluded from the parent workspace. Its unit tests
are enabled in its own workspace, but were not run here. Doctests are disabled.

| Test | Location under `vendor/codex-code-mode/` | Timing and option |
| --- | --- | --- |
| `execute_to_pending_excludes_delayed_timeout_tool_calls_until_wait` | `code-mode-runtime/src/service_tests.rs:854` | Full 2 s sleep around a 1 s JavaScript timer. Establish timer registration, then drive controlled time while retaining runtime pause/resume behavior. |
| `pending_mode_freezes_runtime_commands_until_resume` | `code-mode-runtime/src/runtime/mod.rs:452` | Full 1 s negative event timeout. Controlled time needs a processing barrier, not an immediate empty-channel check. |
| Yield-grace, timer-release and session-limit tests | `code-mode-runtime/src/service_tests.rs:62–425`, `code-mode-runtime/src/runtime/mod.rs:518` | Already virtual. `hold_virtual_time` coordinates with V8 threads and prevents premature automatic advancement. Do not count their 10 s/hour timer values as wall time. |
| Actor observation and completion helpers | `code-mode-runtime/src/cell_actor/tests.rs:218`, `code-mode-runtime/src/service_contract_tests.rs:367` | Yield polling under 1 s guards, not fixed 1 s waits. Notifications may improve clarity more than speed. |

Local HTTP/SSE adapter tests, Code Mode integration guards, finite shell
commands, and terminal handshake guards generally return early. Opt-in live
provider calls are a different cost category and remain excluded. No additional
explicit test sleeps were found in `aj-wire`, `aj-conf`, the SDKs,
`vaxis-derive`, or `vaxis-ucd`.

The scratch runner builds first and runs tests once. Its shell contract tests
use fake Cargo and do not rerun the Rust workspace. The import/bootstrap and
dependency-check scripts have no deliberate test sleeps. Their filesystem,
subprocess and build costs are outside this wait-focused inventory.

## Where to start

1. **Small fast-tier wins.** Unpace the legacy-print setup, remove the progress
   test's unnecessary sleep, and replace callback/task staging with readiness
   gates. These do not require a new shared clock abstraction.
2. **High-leverage fixture work.** Keep generated demo/script content but remove
   pacing where only persisted history or final UI state matters. Gate live
   phases where interleaving is the contract. Replace the unconditional
   autocomplete loop with observable quiescence and a failure deadline.
3. **Target the largest individual windows.** Explicitly release the parent
   shell after the 3/5-second catch-up scenarios' assertions. Exercise the
   15-second refusal invariant by advancing acknowledged retry transitions.
4. **Investigate shared settling boundaries.** The 300 ms remote quiet tails
   and 600 ms directory settles multiply across many scenarios. Do not replace
   them with cursor equality or an immediate empty-queue check if reliable
   transients or background producers remain outstanding. Avoid adding wire
   machinery solely to make tests faster.
5. **Keep OS guarantees intact.** Real process groups, pipe EOF, TERM handlers,
   socket backpressure and KILL escalation need OS-level coverage. Start with
   simulated unavailable-reap timers and staging waits, not shorter production
   grace periods or mocks that erase the tested behavior.

These are investigation directions, not approved redesigns. In particular,
introducing a general clock abstraction just to save one second may cost more
than keeping a small real-time test.

## Existing patterns worth reusing

`src/aj-app/src/print.rs:715` drives full print turns with paused Tokio time,
including the scripted provider's chunk delays. Host shutdown and idle-release
tests in `src/aj-app/tests/session_host.rs:11972` and `:12804` also exercise
composed behavior with virtual time. Goal wait tests use the joined-idle frame
as an ordering fence in `src/aj-app/tests/session_host/goals/wait.rs:16`.
Agent cancellation/quiescence tests at `src/aj-agent/src/lib.rs:3430` and
`:6171`, history-fill coalescing at `src/aj/src/interactive.rs:20583`, and the
gateway preview-timeout test at `src/aj/src/gateway/tests.rs:1069` also use
virtual time. The latter coordinates real socket work before advancing timers.

Do not count those virtual durations as wall-clock savings still available.
Conversely, goal elapsed accounting uses `std::time::Instant` in
`src/aj-app/src/host/driver/goal.rs:67`, so adding `start_paused` alone would not
replace its real-time test.
