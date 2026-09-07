# aj: oversized or over-built commits since remote control began

Working list for targeted review sessions. Scope: every commit on `origin/main`
from `b787396` (2026-08-03, "add remote control wire foundations") to `1e20fcf`
(2026-09-03), 491 commits. Candidates were the 88 commits with 500 or more
non-doc lines changed plus 13 empty-body commits in the 300 to 499 range,
101 in total. Each was read by an auditor against the engineering standard
(smallest coherent solution, fewer concepts, natural boundary, no parallel
abstractions, tests at the stable boundary). Reference case: `c4b977b`.

Verdicts at audit time: 27 LOOK, 43 MAYBE, 31 FINE. Follow-up work has
retired 6 LOOKs.
Prod/test splits are estimates. Verify a verdict against the diff before acting
on it: the auditors read samples of the large diffs, not every line.

Recurring shapes worth naming, since they repeat across the list:

- Review-fix bundles: "fix what two reviews found", "close the review findings",
  "resolve gate findings". Several unrelated rulings land in one commit, often
  with an empty body, sometimes an hour after the commit they fix.
- Same path reshaped several times in one or two days (usage accounting
  bf3a5eb, bdaefe6, e1139c6, 5fc079e; lock generation b1e4ba9, e6d94fc;
  shutdown 9fd4f0c, a0badb0, 434fd05; list refresh 94334b1, 529fd4e).
- Production code restructured to give tests hooks: read counters, fault
  injection points, cfg(test) checkpoints (fe5d661, 317c06c, 529fd4e, 34fcbd8).
- Parallel copies of an existing mechanism (gateway outbound queue vs host
  fanout in bbca1ad; Strict* request mirrors in be726be; hand-built task
  tracker in 434fd05).
- Empty bodies on large cross-crate changes. 26 of the 88 big commits have no
  body at all.

## LOOK (27 total, 21 open)

Plausibly over-engineered or over-scoped relative to the user problem.

- ~~`94334b1` 2026-08-04 aj-app,aj-session: serve a list refresh from caches, not from the store~~ Retired by `e2c349d`.
- ~~`529fd4e` 2026-08-05 aj-app: refresh the directory from memory, and only publish changes~~ Retired: list fan-out now retains one current payload rather than one full copy per subscriber.
- ~~`84db84e` 2026-08-06 aj-app,aj: bound the working set, and fix what two reviews found~~ Retired: focus now solely owns the bounded MRU set, and attach requests serialize it directly.
- ~~`34fcbd8` 2026-08-07 aj-app,aj: set a session's tag from the host and the control port~~ Retired: cold labels are read at enumeration points without fingerprint caching.
- ~~`b57241c` 2026-08-07 aj,aj-app: fix what two reviews found in the sidebar~~ Retired: overrides accept the full chord grammar without a terminal-typeability model. Built-in defaults retain real-parser portability coverage.
- ~~`c4a59be` 2026-08-07 aj: make the sidebar's working set and hosts legible~~ Retired without code changes: host grouping was explicitly requested, and the layout machinery serves current behavior. No worthwhile simplification identified.
- `bbca1ad` 2026-08-11 aj: splice a client's session streams through a gateway
- `67c3d58` 2026-08-19 aj,aj-app: fix a review pass over bounding the catch-up
- `09f7dac` 2026-08-19 aj: close the review findings on the loop-folded catch-up
- `39920f0` 2026-08-26 aj-session,aj-app,aj: break session usage down by provider and model
- `9fd4f0c` 2026-08-26 aj-app,aj: bound host shutdown and escalate stop signals
- `b1e4ba9` 2026-08-26 aj-wire,aj-app,aj,docs: recover locked refusals from latest rows
- `e6d94fc` 2026-08-26 aj-wire,aj-app,aj,docs: harden lock generation recovery
- `a0badb0` 2026-08-28 aj-app,aj: close remaining shutdown races
- `434fd05` 2026-08-28 aj-app: own complete session teardown
- `532aaab` 2026-08-28 aj-session,aj-app: resolve session environment gate findings
- `2c09755` 2026-08-30 models: mark issued handshake usage partial
- `317c06c` 2026-08-30 session: implement crash-safe environment publication
- `48c07a9` 2026-08-30 aj-models,aj-app,aj: manage OAuth credentials by account
- `854f4a7` 2026-08-30 usage: mark undisclosed provider totals partial
- `c4b977b` 2026-08-30 aj-app,aj: keep failed transitions on the selected session
- `bdaefe6` 2026-08-30 aj-app: preserve checkpoint usage dependencies
- `e1139c6` 2026-08-30 aj-app: preserve committed compaction usage atomically
- `70e7ee6` 2026-08-31 aj-tools: bound direct command reap after sigkill
- `be726be` 2026-08-31 aj-wire: reject unknown command fields
- `5fc079e` 2026-08-31 agent: serialize live usage accounting
- `fe5d661` 2026-09-01 aj-models: make auth hardening guarantees load-bearing

## MAYBE (43)

Large but plausibly proportionate, with one specific thing worth a second look.

- `004dc2e` 2026-08-03 aj-app: close the host's ordering and teardown edges
- `0234e5d` 2026-08-03 aj-app,aj: rest reducer catch-up on idempotent application
- `b787396` 2026-08-03 aj-agent,aj-wire,docs: add remote control wire foundations
- `bc94ef0` 2026-08-03 aj-app: lock a session before its log is read, and derive live subs from finished ones
- `cfabd6a` 2026-08-03 aj-session,aj-app: key durable tagging per run and per append
- `8c04079` 2026-08-04 aj-agent,aj-wire,aj-app,aj: prepare the phase 2 transport boundary
- `d39d48a` 2026-08-04 aj: run the TUI against a remote host
- `0542858` 2026-08-05 aj-app,aj-session: release a session the moment nothing needs it
- `a812f97` 2026-08-06 aj-app,aj-session,aj-wire: build a directory row from a stat, not from a log
- `c207347` 2026-08-06 aj-app,aj: switch sessions by swapping, not rebuilding
- `c45a206` 2026-08-06 aj-session,aj-app,aj: make the id-gate tests back their claims
- `180f147` 2026-08-07 aj: give the session sidebar pointer gestures
- `84fa171` 2026-08-11 aj,aj-app,aj-conf: aggregate hosts behind aj gateway
- `f73c6a3` 2026-08-11 aj-wire,aj: a create names the host it is for
- `4bdc6d6` 2026-08-12 aj-app,aj: give the canonical form a convergent tier
- `64bd1e6` 2026-08-12 aj: a configured host's id is provisional, a dynamic one's is the record
- `65473dc` 2026-08-12 aj: draw a host the peer holds no rows for as an empty group
- `7cead11` 2026-08-13 aj,aj-app,docs: refuse a --host that names nothing, and say when it went unused
- `dbdc5ad` 2026-08-13 aj,aj-app: ask which host a create is for
- `6d704ae` 2026-08-14 aj,aj-app: put a session away, and show it again
- `86d366f` 2026-08-14 aj-app,aj: archive a session from the host and the control port
- `b97dd9e` 2026-08-14 aj,aj-app,docs: bound a host's share of the sidebar, and hold it still
- `0765d8b` 2026-08-19 aj: a gateway learns a host's name, keeps it, and republishes it
- `57db2b1` 2026-08-19 aj,aj-app: bound the wait for an attach block that never comes
- `86c76ed` 2026-08-19 aj,aj-app: stop asking after a refused attach, rejoin when the row returns
- `b68b4ec` 2026-08-19 aj: fold a catch-up from the drive loop instead of parking it
- `9705a42` 2026-08-23 aj-wire,aj-app: publish the locked row bit
- `0d705c6` 2026-08-24 aj-tools: pin what a cancelled turn does to a command's processes
- `0c8b2a9` 2026-08-25 aj-models: labeled credentials per provider in the auth store
- `c94f2d9` 2026-08-25 aj-app,aj,docs: re-ask a locked refusal when the rival lets go
- `cb3730a` 2026-08-26 aj-app: pin detached sub-agent cancel routing
- `cd18620` 2026-08-26 aj-agent,aj-models,aj-app: preserve disclosed facts when cancelling a turn
- `34dd548` 2026-08-27 aj-agent,aj-tools: overlay session env on tool subshells
- `b2f9b51` 2026-08-28 aj-app,aj: show session environment in info
- `a28b751` 2026-08-29 aj-models,aj-app: pin openai terminal retry boundaries
- `bf3a5eb` 2026-08-30 aj-app: account committed compaction usage live
- `8bf9e0a` 2026-08-31 aj: close account login composition gaps
- `0bd1045` 2026-08-31 session, aj-app: fuse logs after persistence failures
- `763c87b` 2026-08-31 models: preserve cancellation priority before request issuance
- `26a2f6a` 2026-08-31 aj-app: close compaction accounting evidence gaps
- `7c71560` 2026-08-31 aj-models: write auth storage atomically
- `882ef4d` 2026-09-01 aj: follow the focused host working directory
- `c056f34` 2026-09-01 tools: use absolute gutters for read file output

## LOOK, details

### 94334b1 2026-08-04 aj-app,aj-session: serve a list refresh from caches, not from the store [RETIRED]
- Status: Retired by `e2c349d`. Session listing now uses valid names and file metadata only. The format probe, verdict cache, retry machinery, and their test scaffolding are gone. The remaining store abstraction owns sidecars, locks, deterministic race seams, and release fingerprints that still serve current behavior.
- Stats: 6 files, +1303 -199, ~451 prod / ~852 test lines (estimate: store.rs test module, tests/list_refresh_io.rs, session_host.rs)
- Body: the 200ms list refresh re-opened every log to sniff its format, 7.4 MB/44 ms per tick on a 421-log store
- Verdict: LOOK
- Why: Fixes a per-tick filesystem scan by making the scan cheap instead of asking why a tick scans at all: new `host/store.rs` (678 lines) with `SessionStore` trait, `ColdSessions<S>`, `Fingerprint`, `Derived<T>`, `Cache`, an (mtime,size) invalidation scheme, plus a bytes-not-lines rewrite of two readers in `persistence.rs` (+374) so a torn multi-byte tail stays countable. The trait exists, by its own doc comment, so tests can count reads. One day later 529fd4e concluded the refresh path should touch no filesystem at all, which is the smaller problem statement this commit skipped.
- Simpler shape: refresh from memory and enumerate the store only at the rare external points (startup, explicit list, attach), which is what 529fd4e then did; the fingerprint cache and trait would not have been needed for the tick.

### 529fd4e 2026-08-05 aj-app: refresh the directory from memory, and only publish changes [RETIRED]
- Status: Retired. List fan-out now compares the payload once, retains one current directory, and keeps only an admission bit per subscriber. A fresh subscriber or one whose full queue dropped the frame remains pending, so the next refresh still serves it. The in-memory cold directory and release handoff remain because current rows carry activity, tag, archive, and lock state. Removing them would discard current behavior rather than simplify this change. `e2c349d` already removed the format and log-content caches inherited from `94334b1`.
- Stats: 7 files, +1207 -218, ~381 prod / ~826 test lines (estimate: store.rs and fanout.rs test modules, session_host.rs +530)
- Body: per-tick list refresh cost a directory read and a stat per log for the length of every turn, and steady state resent a byte-identical 60 KB `list` frame per client
- Verdict: LOOK
- Why: Corrects 94334b1's premise but keeps its whole cache apparatus (`SessionStore`, `ColdSessions`, `Fingerprint`, `Derived`) and adds more: `note_released`, `ReleasedMark` flowing from driver to store, a rule that a session that cannot produce a mark is not released, and a test-only `directory_reads: AtomicU64` counter exposed as `ColdSessions::directory_reads` and `SessionHost::store_directory_reads` in production code. The `list` dedup is per subscriber (`sent_list: Option<Vec<SessionSummary>>` cloned into every `Subscriber`, `offer_list`) to cover a fresh subscriber and a dropped frame. The body runs nine paragraphs to justify invariants ("a session is either live or rowed, never neither") that exist because the directory is now a second source of truth held in memory beside the store.
- Simpler shape: publish the directory only when it changed (one comparison at the publisher against the last published payload) and serve the latest payload on subscriber registration, with the store enumerated only at the rare external points; drop the fingerprint cache once the tick no longer scans.

### 84db84e 2026-08-06 aj-app,aj: bound the working set, and fix what two reviews found [RETIRED]
- Status: Retired. `SessionDirectory::focus` now solely owns recency and eviction, while `attach_requests` serializes the resulting working set. The prospective admission and narrowed-attach paths are gone. Directory and composed host/client coverage pin eviction, release, retained rows, successful reattachment, and transcript restoration.
- Stats: 4 files, +734 -243, ~320 prod / ~415 test lines (estimate: added lines after `mod tests` in directory.rs/interactive.rs plus remote/tests.rs)
- Body: Long. Bounds background attachment to WORKING_SET=8 (LRU vector), plus three review-found bugs, four non-discriminating tests rewritten, a hang-only rule given a test, and an API cleanup.
- Verdict: LOOK
- Why: A "fix what two reviews found" bundle: one feature (the bound), three behavior fixes (set-wide `needs_reattach`, narrowed attach, restore-notice folding), four test rewrites and a refactor in one commit. `attach_admitting` in interactive.rs adds a retry special case (whole-set attach refused, fall back to the single session) that the host later makes unnecessary in 2f19fc2. `SessionDirectory::focus` and `attach_requests(admitting)` both compute the same truncation, with a NOTE saying they "have to agree", which is coordination the design leans on rather than one owner of the bound. About half the diff is prose rewrites of existing doc comments.
- Simpler shape: Land the bound alone (one owner: `focus` evicts, `attach_requests` reads the set as it stands), and let the host answer per-session refusals instead of a client-side narrowing retry. Review fixes as their own small commits.

### 34fcbd8 2026-08-07 aj-app,aj: set a session's tag from the host and the control port [RETIRED]
- Status: Retired. Cold labels are read from their sidecars at enumeration points, without a fingerprint cache or tag-read counter. Publication remains memory-only. The directory retains label values and publication identity so a stale scan cannot erase a release's label or clear. Tag commands still write immediately, and validation, persistence, wire, and UI behavior are unchanged.
- Stats: 12 files, +1159 -41, ~330 prod / ~830 test lines (estimate: store.rs `mod tests` at line 583, tests/ dirs)
- Body: `POST /v1/sessions/{id}/tag` and `tag` on create, both `Command::Tag`; live sessions answer from memory; cold rows get labels from a fingerprinted sidecar cache.
- Verdict: LOOK
- Why: A display label gets a second cache layer parallel to the format sniff: `Tagged { at: Option<Fingerprint>, tag }`, `Cache.tags`, `enumerate_tags`/`read_tag` on the `SessionStore` trait, `evict_tags` mirroring `evict`, a `tag_reads` test counter, and a release path that records a tag "without a fingerprint so the next scan re-reads once and pins it". Sidecars are tiny files, so the read-avoidance machinery a812f97 needed for gigabyte logs is not obviously needed here, and 905eb22 four days later simply reads the sidecars synchronously in the selector scan. Tests assert on `tag_reads` counts, pinning the cache's internal behavior.
- Simpler shape: Read cold sidecars at enumeration points, never on the coalescing tick. Keep the live-session in-memory answer and scan/release protection, but drop fingerprint-based read avoidance and its counter.

### b57241c 2026-08-07 aj,aj-app: fix what two reviews found in the sidebar [RETIRED]
- Status: Retired. User overrides accept the full chord grammar, with syntax, reserved-key, and conflict validation intact. Protocol-dependent chords are documented rather than rejected. The production typeability predicate and its agreement sweep are gone, while built-in defaults retain real-parser dispatch coverage. Narrowed-attach machinery is absent. Viewed-position tracking and the unseen latch remain because delayed directory rows and cold sessions must not misreport what the user has seen. Sidebar responsiveness and layout fixes remain scoped behavior, not replacement machinery.
- Stats: 7 files, +1822 -482, ~565 prod / ~1255 test lines (estimate: `mod tests` regions in five files)
- Body: Very long. Removes busy refusals on switch/create, changes row ordering, fixes a lock-induced freeze, drops narrowed-attach sessions, moves the sidebar mirror in the drive loop, four cosmetic fixes, rewrites the sidebar tests, and extends the chord typeability guard to every modifier class including user overrides.
- Verdict: LOOK
- Why: Eight or more unrelated changes under a "fix what reviews found" subject. The keymap work alone adds a terminal-encoding model in aj-app (`untypeable_reason`, `CTRL_ALIASES`, `ESCAPE_INTRODUCERS`, `CONTROL_CODE_KEYS`, `KeybindingProblem::Untypeable`, ~224 lines) plus a ~300-line sweep in keymap.rs re-encoding every chord to bytes to prove the model agrees with the real parser: a parallel restatement of the input parser kept in sync by test. directory.rs reverses a812f97's stamp-based unseen mark back to seq positions with a new `Attached.delivered` tracker and a "last write wins, not max" rule, one day after the stamp design landed. `drop_all_but` and a new `Attachment` struct extend the narrowing retry from 84db84e instead of removing it.
- Simpler shape: Separate commits per ruling. For typeability, either derive the predicate from the parser crate directly or validate overrides by round-tripping through the parser at load time rather than maintaining a hand-written mirror.

### c4a59be 2026-08-07 aj: make the sidebar's working set and hosts legible [RETIRED]
- Status: Retired without code changes. The original brief explicitly requested host grouping before the gateway, not just the working-set encoding. `Layout`, `Group`, and `StripLine` serve current multi-host display and navigation. `Presence` names a derived presentation concept without adding stored state, and removing it would not meaningfully simplify the system.
- Stats: 2 files, +1132 -144, ~485 prod / ~645 test lines (estimate: sidebar.rs `mod tests` at line 714)
- Body: Three visual encodings per row (glyph color, label brightness for working set, focus marker), 24-column strip with its own rule, layout moved into pure `strip_lines`, rows grouped under per-host headers with an unreachable marker.
- Verdict: Retired after investigation. No worthwhile simplification identified.
- Why flagged: The encoding redesign and grouped layout landed before the gateway supplied host values. The size claim was overstated: sidebar.rs's non-test portion grew from 325 to 713 lines including comments, or 202 to 437 nonblank, non-comment lines. Its test section grew from 264 to 763 lines. Producer timing and diff size do not establish unnecessary machinery.
- Disposition: Keep the implementation. Deferring grouping would change the implementation sequence, not remove functionality the user requested and now uses.

### bbca1ad 2026-08-11 aj: splice a client's session streams through a gateway
- Stats: 7 files, +2620 -145, ~895 prod / ~1725 test lines (estimate: gateway/tests.rs +1414, `mod tests` in outbound.rs/splice.rs)
- Body: Gateway `/v1/events` opens one upstream per host named, forwards frames namespaced, `reset` on upstream loss, no gateway-held cursors, bounded per-client queue with the host fan-out's policy.
- Verdict: LOOK
- Why: outbound.rs (451 lines) is a second implementation of aj-app's `host/fanout.rs` LiveQueue: same `Queue`/`State`/`Sender`/`Receiver`/`Offered::{Queued,Dropped,Evicted}` shape, same lossy-key coalescing, same evict-on-reliable-overflow, same paced attach blocks. Its own module doc says the policy is "deliberately not a second one", yet it is a second copy of the mechanism, with its own six tests re-proving the queue semantics already tested in fanout.rs. splice.rs adds `Splice`, `Outgoing`, `Woken`, `Upstream`, `HostReturn` on top.
- Simpler shape: Lift the fan-out queue out of aj-app into a shared module generic over the frame type (it already lives in a crate the gateway depends on) and have the gateway use it, leaving splice.rs as the only new mechanism.

### 67c3d58 2026-08-19 aj,aj-app: fix a review pass over bounding the catch-up
- Stats: 3 files, +710 -240, ~200 prod / ~510 test lines (estimate: interactive.rs +510 test / +138 prod by hunk classification; client.rs +59 and control.rs +29 prod)
- Body: two adversarial reviews; outcome back to a bool, three fold defects fixed, `focus_session` guard restored, `discharge_reattach` acts on the answer, six tests four of which exist because a mutation survived.
- Verdict: LOOK
- Why: A "fix what N reviews found" bundle of +710/-240 landed 59 minutes after the commit it fixes, reverting that commit's `CatchUp` enum to a bool and its give-up notice while adding `World::abandon_attach_block`, `Attach` phase enum on `SessionClient` (`attach_phase()` replacing `awaiting_attach()`), and a rebuilt `WarmPeer` fixture with `redirect_to`, `block_opening`, `poll_for`, `settled` helpers. 510 test lines, explicitly justified by mutation survival rather than by a distinct promise per test, and the headline test ("a block whose frames keep arriving is not cut off") pins the doc comment's argument. Two review findings that would change the recovery's shape are deferred to beads, so the commit is fixing a shape it already suspects is wrong.
- Simpler shape: Land the three fold defects as one small fix with one test each, leave the outcome type alone until the refusal policy (settled next commit) is decided, and skip the `Attach` phase enum until a caller needs to tell Requested from Applying.

### 09f7dac 2026-08-19 aj: close the review findings on the loop-folded catch-up
- Stats: 1 files, +405 -48, ~130 prod / ~275 test lines (estimate: hunk classification; four new async tests of ~60 lines each)
- Body: empty
- Verdict: LOOK
- Why: Empty body on a +405 review-fix bundle 95 minutes after b68b4ec. It adds a `Folded { redraw, opened }` return struct so the drive loop can `reset_to_tail()` on the block's opening frame, `Block::fold_ready()` to drain queued frames before a missed deadline, a `debug_assert_eq!` that the fold and focus agree, a settled-block early return in `fold()`, and an on-exit `resume.take()` cleanup. Each is a separate finding with no stated motivation in the commit. Tests `a_settled_block_does_not_re_decide_itself` and `a_block_reports_its_opening_and_its_repaints` pin `Block` internals (`settled`, the `opened` bit) rather than a user-visible promise; the epoch-cache repaint bug the `opened` bit fixes is the one user-facing item and is not named in the message.
- Simpler shape: Three commits with bodies (drain-before-abandon, epoch adoption repaint, leave-loop cleanup), and the repaint pinned by what the screen shows rather than by `Folded::opened`.

### 39920f0 2026-08-26 aj-session,aj-app,aj: break session usage down by provider and model
- Stats: 6 files, +516 -20, ~105 prod / ~410 test lines
- Body: empty
- Verdict: LOOK
- Why: `UsageBucket` is keyed on `(provider, model, account)` but the aggregation loop sets `let account = None;` unconditionally (stats.rs:166, still so at HEAD), so the account dimension is speculative generality carried in the key, sort comparator, and every test tuple. Same-day 26366f1 landed `AssistantMessage.account`, so the value was available and simply not used. ~410 test lines for a 100-line aggregation, including a 240-line test that builds a multi-thread log to check bucket counts. Empty body on a change that adds a public struct to `SessionStats`.
- Simpler shape: bucket on `(provider, model)` only, add the account dimension when something reads it; or wire `a.account` in the same commit.

### 9fd4f0c 2026-08-26 aj-app,aj: bound host shutdown and escalate stop signals
- Stats: 11 files, +1304 -80, ~370 prod / ~935 test lines
- Body: empty
- Verdict: LOOK
- Why: Two-phase deadline (`HOST_SHUTDOWN_GRACE` minus `HOST_ABORT_GRACE`) with two nested `timeout_at` ladders for the session-map lock and again for driver joins, plus `ShutdownAborts` and `ShutdownFinish` drop guards, a `map_aborted` set to dedupe warnings, and a new `driver_aborts: StdMutex<HashMap<String, AbortHandle>>` kept in parallel with the `sessions` map that already owns each `LiveEntry`'s driver handle. `ShutdownSignals` grows a per-platform struct with a degradation loop for individually failed signal streams. Tests pin timing internals: `elapsed >= 29 && < 30`, `>= 5 && < 6`, and `trace_capture` asserting specific `tracing::warn!` phase strings (`shutdown_bounds_and_names_a_locked_log_flush`). Followed by a0badb0 ("close remaining shutdown races") and 434fd05 two days later.
- Simpler shape: one deadline: `timeout(GRACE, join_all(drivers))`, then abort whatever is left, with the abort handles read from the existing session entries. Second signal exits the process.

### b1e4ba9 2026-08-26 aj-wire,aj-app,aj,docs: recover locked refusals from latest rows
- Stats: 16 files, +1016 -72, ~350 prod / ~670 test lines
- Body: empty
- Verdict: LOOK
- Why: Adds a wire-protocol field `lock_generation` to both `SessionSummary` and `Frame::Error`, a wall-clock-seeded per-session counter in the host store (`lock_seed`, `generations: HashMap`, `set_locked` minting rules), a `>=` comparison clause in the client making three re-ask edges, spec changes disclosing clock-regression as a new failure corner, and a rework of fanout dedup semantics (queue admission vs delivered). All of this to cover a `list` coalescing race that c94f2d9 introduced the day before. Gateway splice has to relay and preserve ordering for the new field. e6d94fc re-hardens the minting rules the same day.
- Simpler shape: the refusal itself is evidence the bit was true, so a locked refusal can treat any later row with `locked: false` for that session as the fall, no generation needed on the wire.

### e6d94fc 2026-08-26 aj-wire,aj-app,aj,docs: harden lock generation recovery
- Stats: 11 files, +636 -230, ~200 prod / ~440 test lines
- Body: empty
- Verdict: LOOK
- Why: Same-day fix-up of b1e4ba9: `set_locked` splits into `acquired` and `observed` with different minting rules, `note_locked` becomes `note_acquire`/`note_unlocked`, a `LockState` struct appears, the spec gains a paragraph on which of three sources may increment, plus a new gateway ordering constraint (merged list before spliced refusal). `the_lock_seed_comes_from_unix_milliseconds` pins the seed's clock source. Three commits in two days on one recovery edge is the signal.
- Simpler shape: see b1e4ba9; if generations stay, one writer function with one rule.

### a0badb0 2026-08-28 aj-app,aj: close remaining shutdown races
- Stats: 9 files, +469 -95, ~135 prod / ~335 test lines
- Body: empty
- Verdict: LOOK
- Why: Fix-up bundle on 9fd4f0c that adds three new flags with overlapping meaning: `block_stop: CancellationToken` on the fanout (beside the existing per-attach `cancelled`), `AttachBlockCompletion(Arc<AtomicBool>)` beside the existing `block_done`, and `draining: AtomicBool` on `LiveSession` beside the host's `shut_down`. `serve_block` renames `cancelled` to `stopped` throughout. Each patches one race rather than putting shutdown under one signal.
- Simpler shape: one host-owned shutdown token that attach serving, block delivery, and live sessions all observe.

### 434fd05 2026-08-28 aj-app: own complete session teardown
- Stats: 20 files, +1478 -179, ~415 prod / ~1065 test lines
- Body: empty
- Verdict: LOOK
- Why: Third shutdown commit in three days. `TaskRegistry` gains `TaskDriver { abort, abort_requested, force_abort }`, `TaskDriverRegistration` (with a `spawned` flag and Drop fallback), `TaskDriverGuard`, `TaskCleanupGuard`, a `cleanups` counter, a `drivers_aborted` latch, and `register_unowned_for_test` to keep old tests compiling. bash.rs grows `ProcessTeardown` and arming. This is a hand-built task tracker layered over tokio's `JoinHandle`s. Empty body on 1478 lines, 1065 of them tests, including a registry test that pins that display status is not the completion fence.
- Simpler shape: a `JoinSet` or `tokio_util::task::TaskTracker` owned by the registry gives spawn tracking, close, and wait-for-all without the guard/registration/flag trio.

### 532aaab 2026-08-28 aj-session,aj-app: resolve session environment gate findings
- Stats: 13 files, +293 -82, ~135 prod / ~155 test lines
- Body: empty
- Verdict: LOOK
- Why: A "review findings" bundle: renames settings to state across log.rs/replay.rs/template.js, adds env display to the export, and touches two spec docs. It leaves a `#[deprecated] project_settings_entry` shim in replay.rs (still at HEAD, used only by a test under `#[allow(deprecated)]`) inside a single workspace with no external callers. The JS `quoteDisplayText` escaper duplicates the Rust `quoted_env_text` added in b2f9b51 with a different grammar.
- Simpler shape: finish the rename and delete the shim; one escaping rule for env display shared by both renderers or none.

### 2c09755 2026-08-30 models: mark issued handshake usage partial
- Stats: 8 files, +390 -75, ~175 prod / ~215 test lines
- Body: empty
- Verdict: LOOK
- Why: Adds `Usage.incomplete` and then a precise issued/not-issued boundary to decide when it applies: a second select helper `select_cancel_after_poll` (biased toward the request future) beside the existing `select_cancel`, an explicit `is_cancelled()` pre-check in each adapter, `client_error_was_issued` in errors.rs, and a spec paragraph enumerating which local failures stay "complete-zero". The unit test `request_selection_polls_once_before_a_racing_cancel_wins` pins polling order. The user-visible difference between a pre-issue and post-issue cancel with zero tokens is a boolean in stats.
- Simpler shape: `incomplete` is true unless the protocol's final usage evidence arrived; drop the issuance distinction and the second select helper.

### 317c06c 2026-08-30 session: implement crash-safe environment publication
- Stats: 29 files, +3822 -443, ~760 prod / ~3060 test lines
- Body: empty
- Verdict: LOOK
- Why: One commit bundles at least three features: a global `--env` flag with a hand-rolled argv pre-scan (`global_env_arguments`, `CliParser` wrapper, `LaunchEnvError`), a transactional first-publication protocol in log.rs (stage file plus `hard_link`, `InitialPublicationFault` enum and `AJ_TEST_INITIAL_PUBLICATION_CHECKPOINT` env hooks compiled into production code under `#[cfg(test)]`, crash-child subprocess tests), a frozen `pre_env_codec_fixture.rs`, and ~990 lines in bash.rs reworking rtk hook resolution (`find_rtk_on_path` with absolute-path checks, `bind_rtk_rewrite`, `ProcessTeardown`, `RTK_HOOK_TIMEOUT`, descendant reaping) with 20 new tests. Empty body on 4265 lines. Tests like `resume_refuses_every_malformed_env_creation_layout_without_changing_bytes` pin log layout internals.
- Simpler shape: the env record already buffers with the other seeds (558e77e); first publication is write-to-temp, fsync, rename, no in-tree fault injection. `--env` is a clap global arg. rtk hardening is its own change.

### 48c07a9 2026-08-30 aj-models,aj-app,aj: manage OAuth credentials by account
- Stats: 17 files, +9350 -406, ~6030 prod / ~3275 test lines (4368 of prod is the vendored `DerivedGeneralCategory.txt`)
- Body: empty
- Verdict: LOOK
- Why: Labeled accounts per provider is a real feature, but the label is treated as an adversarial identifier: a vendored Unicode 17 General Category table plus a 239-line `build.rs` generator, `account_label.rs` (846 lines) with compile-time version asserts against three crates, NFC-exactness, grapheme-starter rules, a "reversible display" grammar with two modes and injectivity tests over generated adversarial labels, `ACCOUNT_INSPECTION_CELL_LIMIT = 65_535` with over-limit acknowledgement dialogs, and an `AccountConfirmation` widget. `AuthStorage` grows `credential_read_count`/`reset_credential_read_count` instrumentation for tests (`connected_auth_refusals_use_a_positively_calibrated_zero_read_oracle`). interactive.rs +1493 and login.rs +984 for pickers and prompts. Empty body on 9350 lines.
- Simpler shape: label rules of non-empty, no control characters, byte bound, stored as a map with a default key; display via the existing `text::one_line`. No UCD table, no build script, no reversible grammar.
### 854f4a7 2026-08-30 usage: mark undisclosed provider totals partial
- Stats: 30 files, +962 -144, ~250 prod / ~700 test lines (estimate: added lines past each file's `mod tests` plus tests/, roundtrip, smoke_test.mjs)
- Body: empty
- Verdict: LOOK
- Why: One fact ("the provider did not disclose final usage") is stored in five places: `Usage.incomplete` (aj-models/types.rs), `TokenUsage.turn_incomplete` + `TokenUsage.accumulated_incomplete` (aj-agent/types.rs), `UsageSummary.incomplete`, `ContextUsage.incomplete` and `AgentFooter.last_turn_incomplete` (aj-app/footer.rs), each with its own sticky-OR plumbing, and then rendered at five surfaces (footer `≥20k/200k`, transcript row title, shutdown summary line, session_info row, export template). It also bundles an unrelated reducer rule (`if entry.is_none() { record_turn_usage }` so tagged checkpoint usage does not replace footer occupancy) and a compaction.rs anchor rewrite (`assistant_usage_anchor`). A 30-file cross-crate semantic change with an empty body and ~700 lines of tests, several asserting legacy JSON re-encodes byte-identically.
- Simpler shape: Carry `incomplete` on `Usage` only, derive the rendered marker from the last usage at display time, and land the checkpoint-occupancy reducer rule separately with its own explanation.

### c4b977b 2026-08-30 aj-app,aj: keep failed transitions on the selected session
- Stats: 4 files, +2732 -869, ~1100 prod / ~1600 test lines (estimate: hunks past `mod tests` at interactive.rs:7746 and client.rs:1043)
- Body: empty
- Verdict: LOOK
- Why: Reference case. Modest outcome (failed switch keeps target selected and toasts) delivered by re-platforming every session switch onto the lost-stream recovery machine, with seven new types (`PendingTransition`, `PendingStartup`, `TransitionFailure`, `ResumeAdvance`, `ForwardReset`, `AttachStall`, `RecoveryRows`) and eight copies of the "until the selected session finishes attaching" gate across handlers. ~1400 test lines, many pinning internal sequencing.
- Simpler shape: Keep the sequential switch and, on failure, leave the selection pointer where it is and post the toast, instead of introducing a pending-transition state machine.

### bdaefe6 2026-08-30 aj-app: preserve checkpoint usage dependencies
- Stats: 13 files, +275 -71, ~90 prod / ~160 test lines (estimate: session_host.rs +91, lib.rs and events.rs test hunks)
- Body: empty
- Verdict: LOOK
- Why: Adds `prerequisite: Option<AgentEvent>` to `Agent::account_usage` so a usage-accounting function emits an arbitrary event first and suppresses its own update if that fails (lib.rs `account_usage`), plus a `has_usage: bool` on `CompactionEnd` so the reducer knows whether to expect a follow-up event, plus `let _ = handoff.take()` cleanup at the call site (compaction.rs). Three coordinating flags to protect an ordering dependency between two events that only exists because the usage row's identity lived in reducer state. Superseded the same evening by e1139c6.
- Simpler shape: Put the checkpoint id on the usage event itself (as e1139c6 later did) so there is no ordering dependency to defend.

### e1139c6 2026-08-30 aj-app: preserve committed compaction usage atomically
- Stats: 15 files, +837 -226, ~230 prod / ~560 test lines (estimate: bus.rs, compaction.rs, client.rs, listener.rs test hunks plus session_host.rs)
- Body: empty
- Verdict: LOOK
- Why: Third reshape of the same path in one day. Adds `EventBus::emit_sequence` (stable-cohort delivery of an event pair), `Agent::account_compaction_usage`, `CompactionUsageUpdate.checkpoint_id`, and turns `AppendHandoff` into a token-numbered slot with an `AppendHandoffGuard` Drop impl (listener.rs) to handle cancellation between two emits. Tests include a select!-based cancellation-mid-listener test and a `client.rs` attach-filtering test that replays a 4-frame sequence. This is the reference pattern: a second layer of coordination machinery (cohort semantics, guard tokens) to recover ordering that a single event, or a single durable append followed by one emit, would give for free.
- Simpler shape: Emit exactly one event per committed checkpoint (`CompactionEnd` carrying the usage and its entry id) so there is no pair to keep atomic and no handoff to guard.

### 70e7ee6 2026-08-31 aj-tools: bound direct command reap after sigkill
- Stats: 2 files, +423 -62, ~150 prod / ~280 test lines (estimate: bash.rs test module +230, session_host.rs +46)
- Body: empty
- Verdict: LOOK
- Why: For the edge case "leader stays unreapable after SIGKILL", the change adds `ProcessTermination { Reaped, OwnershipReleased }`, `CaptureEnd::ReapReleased`, `ProcessGuard::terminate_user_command`, and threads a `reader_cancellation_expected: bool` through `drain_capture` -> `drain_round` -> `await_reader` only to suppress a JoinError log, then overrides the drain result afterwards (`let capture_end = if capture_released { ReapReleased } else { capture_end }`) at both call sites (bash.rs execute and `drive_background_bash`). Two ~100-line tests drive a real D-state-like child.
- Simpler shape: When the bounded reap fails, release the guard and skip `drain_capture` entirely, returning `ReapReleased` directly, so no flag has to travel through three drain functions.

### be726be 2026-08-31 aj-wire: reject unknown command fields
- Stats: 9 files, +1495 -116, ~450 prod / ~950 test lines (estimate: wire.rs +157, gateway/tests.rs +366, remote/tests.rs +444 are test files)
- Body: empty
- Verdict: LOOK
- Why: To reject unknown fields in JSON command bodies it builds a parallel shadow hierarchy: a sealed `request::Sealed` trait, `RequestBody` marker trait, `request_body!` macro, `EmptyRequest`, `RequestDecodeError`, and ~15 private `Strict*` mirrors (`StrictModelSelection`, `StrictSessionSettings`, `StrictUserContent`, `StrictPromptInput`, `StrictCreateSessionRequest`, ... 74 mentions in lib.rs) each with a `From` impl back to the public type, plus a protocol version bump. Every future request field must now be added twice. ~950 test lines, empty body for a wire-contract change.
- Simpler shape: `#[serde(deny_unknown_fields)]` on the request types themselves, splitting only the one or two nested models that are genuinely shared with tolerant observation decoding.

### 5fc079e 2026-08-31 agent: serialize live usage accounting
- Stats: 9 files, +465 -182, ~190 prod / ~200 test lines (estimate: lib.rs event_protocol_tests hunk +200; docs excluded)
- Body: empty
- Verdict: LOOK
- Why: Fourth reshape of `account_usage` in 27 hours (bf3a5eb -> bdaefe6 -> e1139c6 -> this). Introduces `UsageAccounting { AssistantTerminal, CommittedCompaction { checkpoint_id, reason, tokens_before, tokens_after, summary } }` so the agent constructs `CompactionEnd` itself, makes `account_usage` take `&mut self` for serialization, adds an `EventSubscriptions` capability type so hosts can subscribe without being able to emit, and adds a runtime rejection inside `emit_event` (`"accounting events must be emitted through Agent::account_usage"`) matching on event shape. A runtime guard against misuse of one's own API plus a capability split are parallel abstractions layered on top of the emit_sequence/guard machinery from e1139c6.
- Simpler shape: One accounting method with one event per committed operation and a doc comment on `emit_event`; serialization comes from the caller already holding the agent lock.

### fe5d661 2026-09-01 aj-models: make auth hardening guarantees load-bearing
- Stats: 1 files, +501 -19, ~160 prod / ~350 test lines (estimate: `mod fault` ~90 and `PermissionSite` wrappers ~60 are production-file code behind cfg(test))
- Body: Three verification gaps let hardening regress silently; add named writer, per-site chmod wrapper, thread-local fault points and a /dev/full swap so tests cross the exact production path.
- Verdict: LOOK
- Why: Production code is restructured solely to give tests hooks: `PermissionSite { ReplacementFile, Parent, ExistingFile }`, `set_unix_permissions`/`set_unix_file_permissions` wrappers with `#[cfg(test)] fault::injected_permission_failure(site)?`, `write_credentials` with a `#[cfg(test)]` intercept, and a `mod fault` with thread-local `RefCell<Option<WriteHook>>` and `HookGuard`. The tests then prove that each `?` after a chmod propagates, i.e. they mirror the internal structure site by site (the body itself concedes the injection "cannot catch a swallow of the chmod syscall itself"). ~500 lines to defend three `?` operators that the type system already makes visible.
- Simpler shape: Keep the one black-box test that matters (a failed write leaves the prior store byte-identical, which `/dev/full` already gives) and drop the per-site injection scaffolding.


## MAYBE, details

### 004dc2e 2026-08-03 aj-app: close the host's ordering and teardown edges
- Stats: 5 files, +531 -76, ~105 prod / ~426 test lines (estimate: added lines after `#[cfg(test)] mod tests` in fanout.rs plus tests/session_host.rs)
- Body: four ordering/teardown fixes from a review pass over the host, plus a blank-prompt refusal
- Verdict: MAYBE
- Why: A "fixes from the review pass" bundle: settings-notice splice position, head-switch queue clearing, boundary reset vs in-flight attach, leaked cancel entry, plus an unrelated blank-prompt refusal. Production delta is small and each fix is local. The 225 lines of unit tests in `host/fanout.rs` (`an_attach_block_flushes_held_frames_against_its_boundary`, `resetting_boundaries_leaves_an_in_flight_attach_alone`) pin a `pub(crate)` module's delivery rules directly rather than the host's observable stream, worth checking they survive a fan-out redesign.

### 0234e5d 2026-08-03 aj-app,aj: rest reducer catch-up on idempotent application
- Stats: 12 files, +1732 -606, ~690 prod / ~1040 test lines (estimate: reducer.rs test module ~900 added lines plus test_support.rs)
- Body: re-attach backfill re-applies part of a log entry regardless of cursor, so every durable-derived reducer effect must be idempotent; keys effects on the log entry id
- Verdict: MAYBE
- Why: Second same-day pass over the problem c4cef46 opened (c4cef46 kept `CompactionEnd` and notices append-only, this reverses that; it also deletes the `pending_task_cells` linkage map and the `TaskInfo` cell snapshot). The `reduce(entry: Option<String>, ...)` signature and the origin helpers (`usage_origin`, `notice_origin`, `compaction_origin`, `indexed_row`) are proportionate to a spec-mandated invariant. The thing to look at is `test_support::CanonicalState` (+223 here, +265 in c4cef46): a hand-maintained parallel projection of `ChatState` that has to be extended every time the model grows a field, or the equivalence tests go blind.

### b787396 2026-08-03 aj-agent,aj-wire,docs: add remote control wire foundations
- Stats: 12 files, +1815 -11, ~853 prod / ~962 test lines (estimate: tests/wire.rs and JSON fixtures counted as test)
- Body: empty
- Verdict: MAYBE
- Why: Empty body on a +1815 change that introduces a new crate. Content is spec-driven protocol models plus a forwarding-tolerant codec (`DecodedAgentEvent`/`DecodedKnown` retaining raw JSON, `RawObject`, `FrameRef`, `MetadataField`, hand-written `Serialize for Frame`), which the later gateway does use. `Frame::Vms`, `VmSummary`, `VmList`, `VmStatus` still have no producer anywhere outside tests on today's main, so that part was speculative.

### bc94ef0 2026-08-03 aj-app: lock a session before its log is read, and derive live subs from finished ones
- Stats: 6 files, +486 -46, ~138 prod / ~348 test lines (estimate: tests/session_host.rs)
- Body: materialization fixes (lock before build, terminal shutdown, dirty on materialize, drop warning) and inverting the live-sub set to a finished-sub set
- Verdict: MAYBE
- Why: Five distinct fixes in one commit, four of them under a "Materialization fixes" bullet list. The finished-set inversion (`host.rs` tracking "seen finish" and deriving live) is a knowingly accepted heuristic that trades a fabricated conclusion for a bracket left open too long, worth a second look as the sub-agent projection gets more callers.

### cfabd6a 2026-08-03 aj-session,aj-app: key durable tagging per run and per append
- Stats: 8 files, +1471 -326, ~376 prod / ~1095 test lines (estimate: replay.rs and listener.rs test modules, compaction.rs race test)
- Body: two ordering bugs in the durable-event seam (single open bracket force-closed per transition, `CompactionEnd` tagged after the guard dropped), plus cursor-beyond-last fix and type unification
- Verdict: MAYBE
- Why: Per-run `OpenRun` bracketing and merging `ProjectedEvent`/`PersistedEvent` into `TaggedEvent` reduce concepts. `AppendHandoff` (a `file`/`take` cell that carries an `EntryRef` from the append site to the emit site so the event goes out under the guard) is a side channel between two call sites rather than a return value, and the ~1100 test lines for ~380 prod is heavy even at the `project_suffix` boundary.

### 8c04079 2026-08-04 aj-agent,aj-wire,aj-app,aj: prepare the phase 2 transport boundary
- Stats: 34 files, +1943 -380, ~1095 prod / ~848 test lines (estimate: session_host.rs, wire.rs, fanout/client test modules)
- Body: five spec-amendment items: wire command models, thinking display as a settings axis, validated creation, producer-paced attach blocks, client fold reconciliation
- Verdict: MAYBE
- Why: A five-bullet bundle across 34 files where each bullet could have landed alone. The producer-paced attach (`LiveQueue`, `LiveSender`, `LiveReceiver`, `LiveQueueState`, `LossyKey`, `live_channel`) is a hand-rolled bounded channel with per-key coalescing and eviction in `host/fanout.rs`; spec 6.9 asks for the behavior, but a custom channel primitive is the piece worth a second look for whether a bounded mpsc plus a coalescing map at the producer would do.

### d39d48a 2026-08-04 aj: run the TUI against a remote host
- Stats: 16 files, +2698 -527, ~1727 prod / ~971 test lines (estimate: interactive.rs test module plus remote/tests.rs)
- Body: `aj connect <url>` runs the shell as a client of a remote host via one `Control` boundary
- Verdict: MAYBE
- Why: Foundational connect mode with one transport enum (`Control`/`Stream`/`ControlFrame`/`ControlError`) reusing `host::Command`, which is the right shape. The thing to look at is the reconnect machinery inside `interactive.rs` (`Resume`, `advance_resume`, backoff constants, `Connection` status states): it is the first layer of a drive-loop recovery state machine that d546b21 then extends with `Retry`/`ResumeStep` and c4b977b re-platforms session switching onto, so the growth path starts here.

### 0542858 2026-08-05 aj-app,aj-session: release a session the moment nothing needs it
- Stats: 16 files, +1645 -92, ~617 prod / ~1028 test lines (estimate: tests/session_host.rs +798, interactive.rs and lock.rs test modules)
- Body: a host held every session and its lock forever, blocking other aj processes; quiescent unattached sessions are released after an idle grace
- Verdict: MAYBE
- Why: The release itself (sweeper asks, driver decides via `release_if_idle`/`wind_down`/`ReleaseOutcome`/`ReleasedMark`, two monotonic clocks) is argued carefully and proportionate to a real lock-contention bug. Folded in are two adjacent pieces: the lock file now records and reports its holder (`LockHolder`, `record_holder`, `held_by`, `SessionLock::holder`), and the frontend grows `refresh_local_handles`/`rebind_handles` so it never renders through a released core. The holder naming is a separate UX decision that could have been its own change.

### a812f97 2026-08-06 aj-app,aj-session,aj-wire: build a directory row from a stat, not from a log
- Stats: 15 files, +845 -550, ~210 prod / ~635 test lines (estimate: tests/ dirs plus `mod tests` regions)
- Body: Enumeration read and JSON-validated every log (992 ms, 1.95 GB per launch on a real store). Rows now carry mtime-derived stamps, `last_seq` only for live rows.
- Verdict: MAYBE
- Why: Measured problem, net deletion in persistence.rs (`stored_last_seq` gone), fewer moving parts overall. Worth a second look: `SessionSummary.last_seq` becomes `Option<u64>` "present iff live" with no production reader (only tests read it), so it is half-removed rather than removed. The stamp monotonicity logic (`ReleasedMark.last_activity` as max of two clocks, `opening_stamp` on materialize) exists only to keep the unseen glyph honest, and b57241c the next day moves that glyph back to seq positions, so the stamp machinery's justification lasted one day.

### c207347 2026-08-06 aj-app,aj: switch sessions by swapping, not rebuilding
- Stats: 2 files, +501 -242, ~180 prod / ~320 test lines (estimate: interactive.rs `mod tests` at line 6215)
- Body: World holds a SessionDirectory instead of session+client. focus_session splits into swap (already attached) vs reopen over the whole set. Switch no longer detaches the outgoing session.
- Verdict: MAYBE
- Why: Foundational and mostly mechanical (`world.session` -> `world.session()`, `world.client` -> `world.client()`). Production change is small. Second look: `rename_focused` is a `test-support` hook that fabricates a state the message admits "no honest gesture produces", to stage a permanently refused attach in frontend tests. That is a test seam into internals rather than a boundary test. The semantic change that a switch now retains the outgoing session's lock indefinitely is stated as spec-conformant but is a real UX consequence landed inside a refactor commit.

### c45a206 2026-08-06 aj-session,aj-app,aj: make the id-gate tests back their claims
- Stats: 9 files, +360 -171, ~130 prod / ~230 test lines (estimate)
- Body: Adversarial review found three assertions non-discriminating and one degenerate; also membership stops folding an unreadable store into 404.
- Verdict: MAYBE
- Why: Test hardening is legitimate and the `on_disk` -> `Result<bool>` fix is right. Second look: `SessionHost::store_membership_lookups` is a test-support counter added so a test can pin that the id grammar runs before the store lookup, i.e. a test pinning internal ordering via a counter seam (`ColdSessions::membership_lookups`). Also bundles an unrelated `usize` -> `NonZeroUsize` change through fanout.rs and HostSetup.

### 180f147 2026-08-07 aj: give the session sidebar pointer gestures
- Stats: 2 files, +1073 -66, ~365 prod / ~705 test lines (estimate: sidebar.rs and interactive.rs `mod tests` regions)
- Body: Click on a row or `+ new` parks the same request the chords do; hover band; wheel scroll with an anchor that lapses on focus change.
- Verdict: MAYBE
- Why: Click resolves to `StripGesture` and funnels into `park_session_request`, which is the right shape. Second look: the wheel introduces `Anchor { at, focused }` with a "lapses when the focused session changes" rule, `scroll_by`, `run_from`, `last_anchor`, and capture-phase hover handling, all in one commit with the click. Scroll and hover are adjacent scope to "click switches", and 700 test lines for three gestures is heavy.

### 84fa171 2026-08-11 aj,aj-app,aj-conf: aggregate hosts behind aj gateway
- Stats: 19 files, +4145 -50, ~1760 prod / ~2385 test lines (estimate: gateway/tests.rs 1192, remote/tests.rs, plus `mod tests` in each module)
- Body: New `aj gateway` binary: one control link per enrolled host, merged `<host>:<session>` directory, wildcard proxy, REST enrollment persisted to hosts.json, static hosts from gateway.toml; attach and create refused at this stage.
- Verdict: MAYBE
- Why: Foundational feature, seven modules with clear ownership, proxy kept as one unread-body route (good for version skew). Second look: the enrollment surface is large for a first stage: `GET/POST/DELETE /v1/hosts`, `EnrollmentFile`/`Persisted`/`HostSource`/`EnrollHostRequest`/`HostSummary`/`HostList`, dynamic-vs-static persistence rules, and `Directory::adopt` with "an adopted id is fixed for the life of the enrollment". Persisting dynamic enrollments to `~/.aj/gateway/hosts.json` is persistence plus a schema without a stated user need beyond restart recovery, when gateway.toml already exists.

### f73c6a3 2026-08-11 aj-wire,aj: a create names the host it is for
- Stats: 12 files, +1138 -84, ~355 prod / ~780 test lines (estimate)
- Body: `CreateSessionRequest.host`; a plain host refuses a foreign id with 409; the gateway resolves the target (named, or sole enrolled) and edits exactly `host` up and `id` back.
- Verdict: MAYBE
- Why: Reasonable resolution of the create-on-gateway gap. Second look: the raw-JSON body editing (`JsonObject`, `string_field`/`set_string_field`, preserving unknown fields and number literals) exists so the gateway does not decode `CreateSessionRequest`, and four new error codes (`ambiguous_host`, `no_host_enrolled`, `unknown_host`, `host_unreachable`) for one route. ~780 test lines for one route's resolution rules is heavy.

### 4bdc6d6 2026-08-12 aj-app,aj: give the canonical form a convergent tier
- Stats: 2 files, +563 -29, ~0 prod / ~565 test lines (estimate: test_support.rs is test-support, remote/tests.rs)
- Body: Spec 11.2's claim that the fault sweep masks transient-only artifacts was untrue; a `ConvergentState` tier masks them and a sweep run proves the mask carries weight.
- Verdict: MAYBE
- Why: Test-support only, and it makes an existing claim true. Second look: a second oracle type (`ConvergentState` wrapping `CanonicalState`, `is_transient_only`, `renumber`, `assert_convergent_eq`, `assert_tier_eq`) rather than filtering the transient rows out of the one form before comparison, and the sweep now depends on a specific scripted "compact finds nothing" turn to reach the masked state.
### 64bd1e6 2026-08-12 aj: a configured host's id is provisional, a dynamic one's is the record
- Stats: 4 files, +817 -122, ~240 prod / ~580 test lines (estimate: tests.rs +391 plus the `mod tests` hunks in directory.rs ~+180; rest is prod)
- Body: configured and dynamic enrollments must answer a host reporting a new id differently (spec 7.1); adoption becomes write-ahead like withdrawal.
- Verdict: MAYBE
- Why: The behavior is spec-driven and the prod change is modest, but the settlement now has three parallel shapes for one decision: `Adopted` (Learned/Unchanged/Replaced(Withdrawn)), a private `Settling` enum with the same three variants, and `settling()` computed twice (once in `record_adopting`, once in `adopt`) so the write-ahead record and the mutation agree under one lock. `record()` grows an `adopting: Option<(&HostAddress, &str)>` override parameter to simulate the post-adopt set. Test volume is 2.5x prod and several tests assert on the write-ahead ordering (`record_adopting` refused where `adopt` is refused), which is internal sequencing rather than a client-visible promise.

### 65473dc 2026-08-12 aj: draw a host the peer holds no rows for as an empty group
- Stats: 2 files, +635 -60, ~145 prod / ~490 test lines (estimate: sidebar.rs `mod tests` hunks ~+330 plus a 105-line composed-strip test in interactive.rs)
- Body: a host with no rows (down across a gateway restart) was drawn as nothing, reading as "no such host" instead of "unreachable" (spec 7.1).
- Verdict: MAYBE
- Why: Group identity moving from row-derived to directory-derived is the right fix and small. The second look is the new `Budget { empty, rows }` struct plus `Layout::split()` that hand-divides the strip height so empty-host headers outrank rows and the overflow count keeps a line; that priority policy is defended at length in comments and pinned by many tests for a case (strip too short for its headers) that is rare. The interactive.rs test re-pins what sidebar.rs unit tests already cover.

### 7cead11 2026-08-13 aj,aj-app,docs: refuse a --host that names nothing, and say when it went unused
- Stats: 8 files, +406 -141, ~170 prod / ~235 test lines (estimate: interactive.rs test hunks ~+110, host_picker.rs tests ~+70, rest prod incl. 30 doc lines)
- Body: review pass over the host picker: blank `--host` matched everything, empty published id hijacked the sentinel, `--host` silently dropped on attach, plus renames/docs/tests.
- Verdict: MAYBE
- Why: Four real fixes, each a few lines (`named()` helper in host_picker.rs, blank-query refusal in `resolve_host`, the unused-flag notice in connect.rs). It is a "fix what the reviews found" bundle that also carries renames (`park_new_session` -> `settle_create_host`), a visibility change on `mod server`, and a spec/docs update in one commit, so the behavioral fixes are hard to see or revert individually. Not over-engineered, just over-bundled.

### dbdc5ad 2026-08-13 aj,aj-app: ask which host a create is for
- Stats: 11 files, +1547 -90, ~550 prod / ~1000 test lines (estimate: interactive.rs is +74 prod / +941 test by hunk-header classification; host_picker.rs ~210 prod / ~125 test; remote/tests.rs +54)
- Body: multi-host gateway creates were impossible; host travels on `SessionRequest::New`, a picker opens when ambiguous, `--host <id>` for scripts; two honesty fixes ride along.
- Verdict: MAYBE
- Why: The feature is real and the prod side is proportionate (host_picker.rs is a ~200-line module, `park_new_session` settles the question in one place). The second look is the ~940 test lines in interactive.rs, roughly 300 of which are a `RemoteGateway` fixture that boots a real gateway over real hosts with bounded polling helpers (`until`, `until_sessions`, `host_ids`) inside a unit-test module, to exercise a picker whose logic is already unit-tested in host_picker.rs. Two adjacent fixes (`peer_refusal` wording, moving the cwd rule onto the host) are folded in rather than called out separately.

### 6d704ae 2026-08-14 aj,aj-app: put a session away, and show it again
- Stats: 12 files, +1586 -67, ~600 prod / ~980 test lines (estimate: per-file `mod tests` hunks: connect.rs +197, interactive.rs +346, session_selector.rs +170, sidebar.rs +153, directory.rs +112)
- Body: the archive bit reached the wire with nothing reading it; sidebar filters archived rows, two chords, selector reveal, bare connect skips them, list-sessions marks them.
- Verdict: MAYBE
- Why: One coherent feature across five surfaces, and each piece is small except session_selector.rs, where the overlay becomes a new `SessionSelector` widget wrapping `FilterableSelect` for one chord, sharing four `Rc` fields (`select`, `ids`, `seen`, `reveal`) with the existing `SessionScan` struct so that both a batch arriving and a toggle rebuild the same rows. Two structs mirroring each other's state for a boolean toggle is the thing to revisit. Test volume is 1.6x prod, with connect.rs growing its own `Peer` fixture (~100 lines) for three tests.

### 86d366f 2026-08-14 aj-app,aj: archive a session from the host and the control port
- Stats: 12 files, +969 -48, ~260 prod / ~710 test lines (estimate: session_host.rs +279, gateway/tests.rs +148, remote/tests.rs +76, store.rs `mod tests` ~+195)
- Body: archive command mirrors the tag at every layer; cold cache gains a second sidecar axis; gateway unchanged but its pass-through pinned by tests.
- Verdict: MAYBE
- Why: The archive path itself is a faithful copy of the tag path and proportionate. The second look is store.rs: `Archived { at, archived }` beside `Tagged`, `enumerate_archived` beside `enumerate_tags`, `record_archived` beside the tag recording, and a fake-store `during_archived_listing` interleave hook beside `during_tag_read`. Two hand-mirrored axes in the cold cache (a third arrives in 9705a42) suggest the sidecar-axis machinery wants one shape rather than N copies. 148 lines of gateway tests for a change the gateway does not have is a lot of pinning for a pass-through.

### b97dd9e 2026-08-14 aj,aj-app,docs: bound a host's share of the sidebar, and hold it still
- Stats: 6 files, +1517 -274, ~470 prod / ~1050 test lines (estimate: sidebar.rs +830 test / +394 prod by hunk classification; interactive.rs +221 test / +24 prod)
- Body: one busy host filled the strip and rows moved under the pointer; per-group cap of five boring rows behind a fold line, and ordering no longer follows activity.
- Verdict: MAYBE
- Why: The user problem is clearly stated and the mechanism (`GROUP_CAP`, `SidebarRow::boring()`, `Unfolded` newtype over `Vec<Option<String>>`, `held_back()`, `fold_label()`, one chord plus click) is about the smallest that gives cap and fold. Two things to look at: the ordering rewrite (activity to label/insertion order, with ~120 lines of existing tests rewritten) lands in the same commit as the cap, and the test body is 2.2x prod including three async drive-loop tests for the chord/click that duplicate the layout tests' coverage.

### 0765d8b 2026-08-19 aj: a gateway learns a host's name, keeps it, and republishes it
- Stats: 7 files, +614 -108, ~215 prod / ~400 test lines (estimate: gateway/tests.rs +212, directory.rs `mod tests` ~+180)
- Body: the gateway records the name beside the id so an unreachable host's header survives a restart; refusals read the name, create candidates lead with the id.
- Verdict: MAYBE
- Why: The name rides the same adopt/record path as the id, which is right, but the settlement layer grows again: `Reported { host_id, name }`, `Settling { identity: Identity, renames: bool }` with a `changes()` method, the old `Settling` enum renamed to `Identity`, and `Adopted` still separate. Three types now describe one handshake outcome, on top of `Enrollment::label()` vs `Enrollment::candidate()` for two prose contexts. Each step is small; the accumulated shape (see 64bd1e6) is what to revisit.

### 57db2b1 2026-08-19 aj,aj-app: bound the wait for an attach block that never comes
- Stats: 5 files, +467 -58, ~200 prod / ~270 test lines (estimate: interactive.rs test hunks +269 incl. a ~90-line `WarmPeer` fixture; client.rs/control.rs are prod)
- Body: the shell froze forever in "Catching up" when a peer served `error` or `reset` instead of a block; wait now ends on the client's own arm with a silence deadline, three-valued outcome.
- Verdict: MAYBE
- Why: Real hang, and the silence-not-total deadline argument is sound. The second look is `CatchUp { Caught, Unattached, Stalled }` and the give-up-on-refusal policy attached to it: an hour later 67c3d58 collapses it back to a bool because spec 6.5 permits re-asking, then 86c76ed brings the enum back. Policy decided inside a bug fix, then thrashed within one afternoon.

### 86c76ed 2026-08-19 aj,aj-app: stop asking after a refused attach, rejoin when the row returns
- Stats: 3 files, +572 -55, ~240 prod / ~335 test lines (estimate: interactive.rs test hunks +335 incl. `listed_row`/`list_of` helpers; client.rs +68 and directory.rs +91 prod)
- Body: a refused re-attach was retried with backoff, folding one refusal row per attempt (13/minute measured); refusal withdraws the obligation and a `list` row's absent-then-present edge re-owes it.
- Verdict: MAYBE
- Why: Measured problem, protocol-signal fix, and the body records a known gap (`locked` rows never leave). What to look at: `CatchUp` returns as a three-valued enum an hour after 67c3d58 removed it; `SessionClient` gains a `withheld: bool` beside the epoch and arm as a third piece of attach state, plus `holds_attachment()`/`withheld()`/`owe_reattach()`; and `SessionDirectory::rows_returned()` with a per-session `noticed` flag is a set-wide edge detector layered on the directory for one consumer. Two of three tests exist because a mutation survived.

### b68b4ec 2026-08-19 aj: fold a catch-up from the drive loop instead of parking it
- Stats: 1 files, +570 -227, ~250 prod / ~320 test lines (estimate: hunk classification, +318 -140 in `mod tests`)
- Body: the block was folded in an await off the loop body, freezing paint and input for the whole catch-up; the per-frame rule becomes a `Block` fed from the select's frame arm, with a deadline wake.
- Verdict: MAYBE
- Why: Foundational and motivated (frozen UI on any large backfill). The mechanism is the state machine the reference commit later re-platforms onto: `Block { session, silence, deadline, settled }` with `open/settle/settled/deadline/fold/fold_through`, `Resume::block_mut()`/`arriving()`, `ResumeStep::CatchingUp(Block)`, and two drivers of the same rule (loop-fed for recovery, awaited `fold_through` with timeout for swap/branch/discharge). Worth checking whether the awaited driver could have been retired instead of kept as a second path.

### 9705a42 2026-08-23 aj-wire,aj-app: publish the locked row bit
- Stats: 13 files, +610 -16, ~235 prod / ~375 test lines (estimate: store.rs +272 test / +159 prod, session_host.rs +62, list_refresh_io.rs +34)
- Body: a row now says when a rival writer holds the session's lock (spec 6.5/6.8): refused/won acquires set/clear it, enumeration sweeps the lock dir; the third writer is deliberately absent.
- Verdict: MAYBE
- Why: Spec-mandated field, body is honest about the open design question and the ten wire/struct-literal touches are mechanical. The second look is store.rs: a third hand-mirrored cold-cache axis (`enumerate_locks`/`probe_lock` on `SessionStore`, `probe()`/`record_locked()`/`note_locked()`, `FakeLock` with its own `during_lock_listing` interleave hook, `lock_directory_reads()`/`lock_probes()` counters exposed for tests). With tags (existing), archived (86d366f) and now locks, the sidecar-axis pattern is copied three times.

### 0d705c6 2026-08-24 aj-tools: pin what a cancelled turn does to a command's processes
- Stats: 2 files, +1128 -7, ~0 prod / ~1130 test lines (bash.rs changes are all inside `mod tests`, +744; new tests/cancel_teardown.rs +384)
- Body: three seam tests through a live agent and real bash tool for cancel teardown, plus ~13 unit tests covering drop windows, grace, pipe read ends, host exit; several exist to hold the teardown to one path.
- Verdict: MAYBE
- Why: Test-only, and the seam tests (cancel kills the group, returns fast, background survives) pin real contracts the suite lacked. The rest is the thing to weigh: sixteen new test fns, several targeting internal windows ("a drop landing mid-drain after the child was reaped", "a failure between the spawn and the first await", "a guard dropped after its runtime is gone") whose descriptions in the body run to nine paragraphs. By the standard that each kept test pins a distinct promise, some of these will only ever fail alongside the seam tests.

### 0c8b2a9 2026-08-25 aj-models: labeled credentials per provider in the auth store
- Stats: 5 files, +837 -66, ~400 prod / ~440 test lines (estimate: auth.rs `mod tests` hunks +437; callers are one-line `None` additions)
- Body: auth.json entries become either a bare credential (unchanged bytes) or a labeled set with a default; `get_api_key` gains an account dimension with strict no-fallback resolution; six account-aware methods.
- Verdict: MAYBE
- Why: Storage-format work with backward compatibility handled cleanly (`StoredEntry` shares the `type` tag space, `Slot` keeps OAuth refresh writing back to the slot it read). The second look is that six public methods (`get_account`, `accounts`, `set_account`, `set_default_account`, `remove_account`, `login_account`) land with no caller: every existing call site passes `None`. That is a surface designed ahead of its consumer, which is fine if the account UI lands next and speculative if not.
### c94f2d9 2026-08-25 aj-app,aj,docs: re-ask a locked refusal when the rival lets go
- Stats: 5 files, +662 -99, ~150 prod / ~510 test lines (added lines after `#[cfg(test)]` per file)
- Body: a `locked` refusal's row never leaves the peer's list, so the absence edge alone strands it; add the `locked` bit falling as a second re-ask edge, and word the notice per edge.
- Verdict: MAYBE
- Why: Production change is small and native: `Refusal { Locked, Other }` on `SessionClient.withheld`, `rows_returned` becomes `rejoin_edges_fired` with one extra `released` predicate, and a second notice constant. The 3.4x test ratio is what to look at: five new unit tests in directory.rs plus two full interactive-loop tests (`a_locked_session_rejoins_when_the_rival_lets_go`, `the_locked_edge_reads_across_a_lost_connection`) pin the same edge at two layers, and the 40-line commit body spends a paragraph justifying a test rewrite. Superseded the next day by b1e4ba9, which suggests the edge was known to be racy when it landed.

### cb3730a 2026-08-26 aj-app: pin detached sub-agent cancel routing
- Stats: 1 files, +614 -1, ~0 prod / ~615 test lines
- Body: empty
- Verdict: MAYBE
- Why: Test-only, pinning behaviour that already existed. The cost is a bespoke harness (`EndDetached`, `ParentTurn`, `DetachedEnding` with a `end_detached_sub` matrix runner) at 600 lines for four tests, one of which asserts two gestures produce identical `DetachedEnding` values. Worth asking whether the equivalence test adds a promise the two direct tests don't already pin.

### cd18620 2026-08-26 aj-agent,aj-models,aj-app: preserve disclosed facts when cancelling a turn
- Stats: 12 files, +955 -68, ~70 prod / ~885 test lines
- Body: empty
- Verdict: MAYBE
- Why: Production change is right-shaped: providers seal every partial, the agent stops re-pricing on cancel, and `drain_ready` flushes queued events before the aborted terminal. The 13:1 test ratio is the thing to look at: a new `HeldSseServer` fixture, unit tests per adapter, and a 400-line `cancelled_turns.rs` with three tests whose names overlap (`cancellation_drains_a_queued_priced_terminal_before_persisting`, `cancellation_keeps_the_queued_providers_exact_price`, `cancellation_persists_an_anthropic_shaped_priced_partial`); the first pins the drain mechanism rather than the persisted outcome.

### 34dd548 2026-08-27 aj-agent,aj-tools: overlay session env on tool subshells
- Stats: 4 files, +368 -5, ~15 prod / ~355 test lines
- Body: empty
- Verdict: MAYBE
- Why: Production is 15 lines (`session_env` on session state, `cmd.envs(ctx.session_env())`). Four end-to-end agent tests at 350 lines pin layering, per-agent isolation, sub-agent inheritance, and background spill; the last two follow from the first two given how `session_env()` is read, so two may suffice.

### b2f9b51 2026-08-28 aj-app,aj: show session environment in info
- Stats: 4 files, +416 -24, ~55 prod / ~360 test lines
- Body: empty
- Verdict: MAYBE
- Why: `InfoRow::Env` plus an Env section is the right shape. Tests at three layers (digest rows, overlay rendering, two interactive-loop tests) for one display section is heavy, and `quoted_env_text` introduces a bespoke `\x20`-style quoting grammar for values that `text::one_line` already sanitises for other rows.

### a28b751 2026-08-29 aj-models,aj-app: pin openai terminal retry boundaries
- Stats: 2 files, +605 -0, ~0 prod / ~605 test lines
- Body: empty
- Verdict: MAYBE
- Why: Test-only, laying down the contract a300297 then implements against. The new `openai_stream_terminals.rs` builds its own scripted HTTP server (`ResponseScript`, `read_request`) because `provider_test_support` is crate-private; two SSE fixture servers now exist in the workspace.

### bf3a5eb 2026-08-30 aj-app: account committed compaction usage live
- Stats: 17 files, +779 -213, ~170 prod / ~550 test lines (estimate: session_host.rs +374 and test modules; lib.rs event_protocol_tests +140)
- Body: empty
- Verdict: MAYBE
- Why: Feature-shaped and mostly at the right boundary: summarizer usage is persisted on the `Compaction` entry, `complete_oneshot` returns the priced message, `Agent::account_usage` is shared with the turn path. The thing to revisit is `UsageOrigin { Assistant, Compaction }` replacing `last_finalized_assistant` in `AgentRender` so the reducer infers a usage row's owner from ambient state; within three hours (bdaefe6, e1139c6) this was patched with `has_usage` and then replaced by a self-identifying `checkpoint_id` on the event, which is what the row key should have been from the start.

### 8bf9e0a 2026-08-31 aj: close account login composition gaps
- Stats: 5 files, +307 -61, ~50 prod / ~260 test lines (estimate: content_overlay.rs and interactive.rs test hunks)
- Body: empty
- Verdict: MAYBE
- Why: Mostly doc-comment rewording plus one real change: `Shell.width_method: Cell<Method>` snapshotted at draw and threaded via new `spawn_shell_overlay_fetch` into `auth_rows(.., width_method)` so emoji/CJK account labels align in the auth overlay. Reasonable, but the "gaps" bundle shape and ~250 lines of rendering tests (`👋🏿` vs `個` under two width methods) for a column-padding fix are worth a glance. The account-inspection flow this commit routes through was deleted two days later (a5e691b).

### 0bd1045 2026-08-31 session, aj-app: fuse logs after persistence failures
- Stats: 15 files, +1436 -225, ~300 prod / ~1100 test lines (estimate: log.rs `test_support` module ~180 and test hunks ~400, session_host.rs +341)
- Body: A partial append makes the descriptor unsafe; fuse the log one-way on first I/O failure, signal the driver via oneshot, drain the session, and let the client re-ask.
- Verdict: MAYBE
- Why: Good body, real durability problem, and the core is small: `WriteState { Writable, WriteFailed }`, `ensure_writable()` at each mutation, `fuse()`, one oneshot. Worth a second look: `AppendWriter` enum gains a `#[cfg(test)] Faulting(FaultingAppendWriter)` variant and a ~180-line `test_support` fault fixture inside the production module, and the test suite pins fine-grained pending-record ownership after each failure kind (`pending_write_failure_releases_only_completed_record_ownership`, `pending_flush_failure_keeps_every_record_owned`, ...), which mirrors the internal `pending_writes` bookkeeping rather than the contract.

### 763c87b 2026-08-31 models: preserve cancellation priority before request issuance
- Stats: 6 files, +229 -115, ~90 prod / ~130 test lines (estimate: cancel.rs and provider test hunks)
- Body: empty
- Verdict: MAYBE
- Why: Replaces `select_cancel_after_poll` with `select_request` returning `RequestSelectOutcome { Ready, CancelledBeforePoll, CancelledAfterPoll }`, tracking whether the request future was polled once so a cancel before the first poll stays "complete zero" while one after stays "partial". Proportionate in size, but this is fine-grained semantics for the `incomplete` marker from 854f4a7: whether an aborted, zero-token request is marked partial is unlikely to matter to a user, and four providers now carry the three-way match.

### 26a2f6a 2026-08-31 aj-app: close compaction accounting evidence gaps
- Stats: 1 files, +467 -40, ~0 prod / ~450 test lines (single integration test file)
- Body: empty
- Verdict: MAYBE
- Why: Test-only, but the shape is telling: a `CompactionAccountingSnapshot` struct collecting 12 fields (durable stats, per-row `source_entry` plus an 8-tuple of accumulated/turn counters and two incomplete flags, live and host totals) compared across live, shutdown, durable, replay and attach boundaries. "Evidence gaps" is review-driven language; the snapshot pins the `TokenUsage` split and row identity internals, so it will fail on any legitimate redesign of the accounting events it exercises.

### 7c71560 2026-08-31 aj-models: write auth storage atomically
- Stats: 1 files, +452 -27, ~140 prod / ~310 test lines (estimate: hunks past `mod tests`)
- Body: empty
- Verdict: MAYBE
- Why: Atomic tempfile-and-rename for credentials is the right fix and the prod part is compact (`replace_auth_file`, `prepare_auth_parent`, `make_existing_auth_file_private`, `create_lock_dir`). Worth a second look: `replace_auth_file` takes an injectable `write: impl FnOnce(&mut File, &[u8])` purely for test fault injection, and the commit widens scope beyond "atomic" into absolute-path requirement, symlink rejection, and permission repair on every locked read, with ten tests and an empty body. That injection seam then grew into fe5d661.

### 882ef4d 2026-09-01 aj: follow the focused host working directory
- Stats: 13 files, +481 -48, ~180 prod / ~300 test lines (estimate: gateway/tests.rs +65, interactive.rs and wire tests hunks)
- Body: empty
- Verdict: MAYBE
- Why: Feature across wire (`DirectoryHost.working_directory`), gateway directory (`Enrollment.working_directory`, `Settling.moves_directory`, `record_changes` vs `changes` split), and client (`World.sync_working_directory`, `Shell.rebind_working_directory`). Proportionate, but `World.working_directory_follows_focus` is derived from "the remote hello omitted its directory" as the gateway discriminator, an implicit protocol heuristic that should probably be an explicit fact from `Hello`.

### c056f34 2026-09-01 tools: use absolute gutters for read file output
- Stats: 4 files, +415 -45, ~30 prod / ~360 test lines (estimate: new tests/read_file_persistence.rs 278, read_file.rs test hunks)
- Body: empty
- Verdict: MAYBE
- Why: The prod change is a simplification (one `format_numbered_lines` for both model and display body). The second look is the new 278-line integration test `real_read_file_results_compact_and_preserve_post_hook_bodies` that walks persistence, projection, replay and HTML export for a gutter-numbering change, plus 100k-line fixture tests for gutter width; ~12x test-to-prod ratio for a display tweak.


## FINE (31)

Large for a good reason. Listed so the scan is complete.

- `5597711` 2026-08-03 aj-app: add the client-side frame fold
  4 files, +995 -0, ~396 prod / ~599 test lines (estimate: client.rs `mod tests` region)
  One new type implementing a spec'd consumer contract, tests drive it through frames at the wire boundary.
- `c1f0a77` 2026-08-03 aj-session: add log snapshots and durable suffix projection
  4 files, +1022 -282, ~588 prod / ~434 test lines (estimate: replay.rs test module)
  Moves the read side onto a snapshot and unifies replay and suffix projection on one walk, which reduces concepts rather than adding them.
- `c4cef46` 2026-08-03 aj-app,aj: key reducer application on durable identity
  5 files, +1113 -68, ~508 prod / ~605 test lines (estimate: reducer.rs test module and test_support.rs)
  Spec 6.5 idempotency, implemented as a per-agent `message_index` beside the existing tool index, with tests at the `reduce` boundary; the follow-up cost shows up in 0234e5d rather than here.
- `c66b959` 2026-08-03 aj-session: add the persisting forwarder and the session lock
  6 files, +518 -50, ~226 prod / ~292 test lines (estimate: listener.rs test module)
  Two small, named seams the host needs, each with tests at its own boundary.
- `e9604de` 2026-08-03 aj-app: add the session host
  6 files, +3940 -0, ~2004 prod / ~1936 test lines (estimate: tests/session_host.rs, 46 test fns)
  The foundational layer of the design, with a clear single-publisher ordering argument and tests at the host's public attach/command boundary.
- `77d0e51` 2026-08-04 aj: serve the remote-control protocol over HTTP
  9 files, +3892 -1, ~1674 prod / ~2218 test lines (estimate: remote/tests.rs, 94 fns)
  Transport layer that adds no protocol semantics of its own, one trait for the external whois dependency, tests are HTTP-level plus the spec's equivalence harness.
- `d178e33` 2026-08-04 aj: point the interactive tests at the host
  2 files, +1013 -903, ~20 prod / ~993 test lines (estimate: nearly all in interactive.rs test module)
  Test-only follow-up to e1f13a1, roughly a 1:1 rewrite.
- `d546b21` 2026-08-04 aj: pace the drive loop's retries and re-attach an evicted local stream
  3 files, +557 -119, ~275 prod / ~282 test lines (estimate: interactive.rs test module)
  One small `Retry` type (delay, due) replaces four ad hoc retry paths and the loop merges its due time into the wake deadline, which is fewer moving parts than before.
- `e1f13a1` 2026-08-04 aj: reroute the interactive shell through the session host
  3 files, +1034 -1232, ~1033 prod / ~0 test lines (estimate: no test module changes, "tests still to follow")
  Net deletion that removes the local-only ownership path and the startup replay, tests landed in d178e33.
- `110210c` 2026-08-06 aj,aj-app: the session sidebar
  7 files, +802 -7, ~388 prod / ~414 test lines (estimate: sidebar.rs and interactive.rs test modules)
  A widget plus four keybindings following the existing status-chrome mirroring pattern, with the precedence rule as a pure function.
- `6e373d1` 2026-08-06 aj-app,aj: give the client a session directory and a multi-session attach
  4 files, +732 -4, ~316 prod / ~416 test lines (estimate: directory.rs test module)
  One new type with a clear split of knowledge and the single-session attach collapsed into the multi one; the one wrinkle (the focused transcript lives outside the directory because widgets hold it behind an `Rc`) is stated and bounded.
- `f00bad5` 2026-08-06 aj-app,aj,docs: back the tree and branch claims with tests that bite
  10 files, +456 -63, ~90 prod / ~370 test lines (estimate)
  Tests are at the wire/gesture boundary (drive the real chord, HTTP round trips), production change is a small correctness fix (`HeadTarget::named`, torn-log vs bad-request split in `resolve_head_target`).
- `f215150` 2026-08-06 aj-wire,aj-app,aj: take the tree view and branching over the wire
  13 files, +518 -221, ~200 prod / ~320 test lines (estimate)
  Removes a special case (client-side log walk and the connect-mode refusal) by putting resolution at the host boundary. `HeadRequest` as two optional fields rather than an enum is a small wire wart but typical for the JSON shape used elsewhere.
- `0936cb1` 2026-08-07 workspace: let the tests clean up their own scratch directories
  9 files, +251 -274, ~5 prod / ~245 test lines (estimate: only bash.rs +5 touches production)
  Test-only, net deletion, compiler-checked lifetimes found three helpers handing out dropped dirs.
- `0dcaf3f` 2026-08-07 aj-app,aj: set a session's tag from the shell
  7 files, +535 -9, ~190 prod / ~345 test lines (estimate: interactive.rs `mod tests` at line 6534)
  Rides existing concepts (AjAction, palette Command, overlay module, parked request). session_tag.rs is 84 lines. Two end-to-end tests drive real key bytes.
- `442b9b5` 2026-08-07 aj: toast what a session change has to say instead of folding it
  1 files, +418 -91, ~65 prod / ~355 test lines (estimate)
  Bug fix with a small production change (`focus_session` loses `lead`), tests read painted toast lines through the composed frame.
- `905eb22` 2026-08-11 aj-session,aj: give a session's tag its own column and its own filter
  4 files, +548 -105, ~100 prod / ~450 test lines (estimate)
  Bug fix with a small production change; the `#` scope is a one-constant addition. Notably it just reads sidecars in the scan, which is the simpler shape 34fcbd8 did not take.
- `2f19fc2` 2026-08-12 aj-app,aj: a host refuses an attach per session, not per request
  5 files, +521 -123, ~120 prod / ~400 test lines (estimate)
  Puts the rule at the host boundary with a small `Serving::{Block,Refusal}` enum and `HostError::code` shared with the HTTP layer, and makes the client-side narrowing retry from 84db84e unnecessary (though that retry is not removed here).
- `832bf45` 2026-08-12 aj,aj-wire: keep a learned host id, and name a host with none by its address
  9 files, +585 -160, ~230 prod / ~355 test lines (estimate: tests.rs +179, wire tests +59, `mod tests` hunks in directory.rs/enrollment.rs ~+115)
  A persistence gap fixed at the natural boundary (`Recorded { hosts, configured_ids }` in enrollment.rs, `record()`/`record_without()` in directory.rs), with a small additive wire change and a spec sentence corrected; the split into two lists is justified in the body (restoring must not resurrect a host deleted from the config).
- `8e8ac61` 2026-08-12 aj: a gateway refuses an attach per session, not per stream
  4 files, +408 -97, ~120 prod / ~285 test lines (estimate: tests.rs +255 plus directory.rs test hunks ~+40)
  Straight bug fix at the right seam: `AttachPlan { groups, unresolvable }` and `Unresolvable` replace a `Result`, the splice writes one `refusal()` frame each, and the withdraw path's stale comment is corrected. Tests cover the client-visible frames.
- `43df7cf` 2026-08-14 aj-wire,aj-app,aj: let a host name itself for a reader
  22 files, +557 -24, ~310 prod / ~250 test lines (estimate: wire.rs +111, session_host.rs +53, args.rs/host.rs `mod tests` ~+90)
  Additive protocol field with one validation function (`normalize_host_name`, `MAX_HOST_NAME_BYTES`) and one derivation (`derive_host_name`/`keep_tail`); the 22 files are mostly one-line struct-literal updates. Body scopes it explicitly and defers the gateway half.
- `26366f1` 2026-08-26 aj-models,aj-app,aj-session,aj-wire: record the resolved account on each turn
  25 files, +468 -59, ~190 prod / ~275 test lines
  One new type (`ResolvedApiKey`) replacing a bare `String`, a field threaded through the message type and its constructors, tests at the wire and session-file boundaries. Wide because the field touches every constructor, not because of mechanism.
- `558e77e` 2026-08-27 aj-session,aj-app: persist and redact session environment entries
  7 files, +475 -11, ~70 prod / ~405 test lines
  `EnvChange` reuses the existing settings-entry path (`append_settings_entry`), export redaction is a targeted serialize override, tests sit at the on-disk and HTML boundaries.
- `a300297` 2026-08-29 aj-models: checkpoint state-owned openai terminals
  4 files, +592 -56, ~85 prod / ~505 test lines
  Moves terminal construction into `StreamState` (`client_failed`, `ready_after_trailing_usage`) so a body failure after `finish_reason` keeps the partial, deleting the standalone `error_message` path; tests are at the provider stream boundary.
- `0143cd4` 2026-08-30 vaxis: handle oversized grapheme widths
  3 files, +543 -70, ~70 prod / ~470 test lines
  Saturating arithmetic and row-bounded coverage in the renderer replace a silent out-of-bounds skip; tests are regression pins for concrete panics and aliasing cases.
- `0694eb2` 2026-08-30 aj-models: honor requested pricing for default tier
  3 files, +428 -130, ~30 prod / ~400 test lines
  Deletes the duplicate `resolve_codex_service_tier` in favour of one shared resolver and replaces scattered multiplier asserts with two table-driven pricing tests.
- `ea55c19` 2026-08-31 skills: remove tmux sub-agent skill
  10 files, +1 -1065, ~1 prod / ~0 test lines (pure deletion of skill scripts plus one schema line)
  Deletion of an unused skill directory, nothing to revisit.
- `20db96e` 2026-09-02 aj: open session selector over connections
  3 files, +754 -83, ~230 prod / ~520 test lines (estimate: session_selector.rs and interactive.rs test hunks)
  A connected-mode session picker that reuses the existing widget through a two-variant `SelectorRows { Local, Connected }` and shares confirm/cancel/reveal by construction; the size is the feature plus scripted-host tests.
- `a5e691b` 2026-09-02 aj: apply account actions directly from pickers
  3 files, +128 -839, ~10 prod / ~120 test lines added (estimate: nearly all removed lines are `AccountConfirmation` overlay and its tests)
  Deletes the `AccountConfirmation` inspection overlay (RTL grapheme handling, over-limit acknowledgement, width-method tests) so pickers apply directly; a net simplification, and evidence that the inspection layer it removes (from the 08-30/31 account series) was over-built.
- `69bbe52` 2026-09-02 aj-app: add independent export sidebar filters
  5 files, +224 -197, ~85 prod / ~140 test lines (estimate: smoke_test.mjs +~130, export.rs test)
  Replaces four preset filter buttons with five independent toggles in the export template JS/CSS/HTML; a contained frontend feature with a smoke test.
- `b3aaae9` 2026-09-03 session: repair an interrupted final write on reopen and say so
  7 files, +430 -124, ~110 prod / ~320 test lines (estimate: log.rs test hunks, session_host.rs +18)
  The body states the fault model and the one-line lookahead plus serde `is_eof()` distinction is the smallest mechanism for it; `TailRepair` and `take_tail_repair` are two small additions carried to exactly one consumer.

## Appendix: all 101 candidates by size

Non-doc lines is additions plus deletions excluding `docs/` and `*.md`.

| sha | date | files | added | deleted | code lines | subject |
|---|---|---|---|---|---|---|
| 48c07a9 | 2026-08-30 | 17 | +9350 | -406 | 9740 | aj-models,aj-app,aj: manage OAuth credentials by account |
| 317c06c | 2026-08-30 | 29 | +3822 | -443 | 4265 | session: implement crash-safe environment publication |
| 84fa171 | 2026-08-11 | 19 | +4145 | -50 | 4185 | aj,aj-app,aj-conf: aggregate hosts behind aj gateway |
| e9604de | 2026-08-03 | 6 | +3940 | -0 | 3940 | aj-app: add the session host |
| 77d0e51 | 2026-08-04 | 9 | +3892 | -1 | 3893 | aj: serve the remote-control protocol over HTTP |
| c4b977b | 2026-08-30 | 4 | +2732 | -869 | 3601 | aj-app,aj: keep failed transitions on the selected session |
| d39d48a | 2026-08-04 | 16 | +2698 | -527 | 3225 | aj: run the TUI against a remote host |
| bbca1ad | 2026-08-11 | 7 | +2620 | -145 | 2765 | aj: splice a client's session streams through a gateway |
| 0234e5d | 2026-08-03 | 12 | +1732 | -606 | 2338 | aj-app,aj: rest reducer catch-up on idempotent application |
| 8c04079 | 2026-08-04 | 34 | +1943 | -380 | 2323 | aj-agent,aj-wire,aj-app,aj: prepare the phase 2 transport boundary |
| b57241c | 2026-08-07 | 7 | +1822 | -482 | 2296 | aj,aj-app: fix what two reviews found in the sidebar |
| e1f13a1 | 2026-08-04 | 3 | +1034 | -1232 | 2266 | aj: reroute the interactive shell through the session host |
| d178e33 | 2026-08-04 | 2 | +1013 | -903 | 1916 | aj: point the interactive tests at the host |
| b787396 | 2026-08-03 | 12 | +1815 | -11 | 1821 | aj-agent,aj-wire,docs: add remote control wire foundations |
| cfabd6a | 2026-08-03 | 8 | +1471 | -326 | 1784 | aj-session,aj-app: key durable tagging per run and per append |
| b97dd9e | 2026-08-14 | 6 | +1517 | -274 | 1763 | aj,aj-app,docs: bound a host's share of the sidebar, and hold it still |
| 0542858 | 2026-08-05 | 16 | +1645 | -92 | 1737 | aj-app,aj-session: release a session the moment nothing needs it |
| 434fd05 | 2026-08-28 | 20 | +1478 | -179 | 1657 | aj-app: own complete session teardown |
| 6d704ae | 2026-08-14 | 12 | +1586 | -67 | 1653 | aj,aj-app: put a session away, and show it again |
| dbdc5ad | 2026-08-13 | 11 | +1547 | -90 | 1637 | aj,aj-app: ask which host a create is for |
| 0bd1045 | 2026-08-31 | 15 | +1436 | -225 | 1628 | session, aj-app: fuse logs after persistence failures |
| 94334b1 | 2026-08-04 | 6 | +1303 | -199 | 1502 | aj-app,aj-session: serve a list refresh from caches, not from the store |
| be726be | 2026-08-31 | 9 | +1495 | -116 | 1495 | aj-wire: reject unknown command fields |
| 529fd4e | 2026-08-05 | 7 | +1207 | -218 | 1425 | aj-app: refresh the directory from memory, and only publish changes |
| a812f97 | 2026-08-06 | 15 | +845 | -550 | 1387 | aj-app,aj-session,aj-wire: build a directory row from a stat, not from a log |
| 9fd4f0c | 2026-08-26 | 11 | +1304 | -80 | 1384 | aj-app,aj: bound host shutdown and escalate stop signals |
| c1f0a77 | 2026-08-03 | 4 | +1022 | -282 | 1304 | aj-session: add log snapshots and durable suffix projection |
| c4a59be | 2026-08-07 | 2 | +1132 | -144 | 1276 | aj: make the sidebar's working set and hosts legible |
| f73c6a3 | 2026-08-11 | 12 | +1138 | -84 | 1216 | aj-wire,aj: a create names the host it is for |
| 34fcbd8 | 2026-08-07 | 12 | +1159 | -41 | 1200 | aj-app,aj: set a session's tag from the host and the control port |
| c4cef46 | 2026-08-03 | 5 | +1113 | -68 | 1181 | aj-app,aj: key reducer application on durable identity |
| 180f147 | 2026-08-07 | 2 | +1073 | -66 | 1139 | aj: give the session sidebar pointer gestures |
| 0d705c6 | 2026-08-24 | 2 | +1128 | -7 | 1135 | aj-tools: pin what a cancelled turn does to a command's processes |
| 854f4a7 | 2026-08-30 | 30 | +962 | -144 | 1106 | usage: mark undisclosed provider totals partial |
| cd18620 | 2026-08-26 | 12 | +955 | -68 | 1023 | aj-agent,aj-models,aj-app: preserve disclosed facts when cancelling a turn |
| 86d366f | 2026-08-14 | 12 | +969 | -48 | 1017 | aj-app,aj: archive a session from the host and the control port |
| 5597711 | 2026-08-03 | 4 | +995 | -0 | 995 | aj-app: add the client-side frame fold |
| b1e4ba9 | 2026-08-26 | 16 | +1016 | -72 | 993 | aj-wire,aj-app,aj,docs: recover locked refusals from latest rows |
| e1139c6 | 2026-08-30 | 15 | +837 | -226 | 990 | aj-app: preserve committed compaction usage atomically |
| 84db84e | 2026-08-06 | 4 | +734 | -243 | 977 | aj-app,aj: bound the working set, and fix what two reviews found |
| a5e691b | 2026-09-02 | 3 | +128 | -839 | 967 | aj: apply account actions directly from pickers |
| 67c3d58 | 2026-08-19 | 3 | +710 | -240 | 950 | aj,aj-app: fix a review pass over bounding the catch-up |
| 64bd1e6 | 2026-08-12 | 4 | +817 | -122 | 939 | aj: a configured host's id is provisional, a dynamic one's is the record |
| ea55c19 | 2026-08-31 | 10 | +1 | -1065 | 917 | skills: remove tmux sub-agent skill |
| bf3a5eb | 2026-08-30 | 17 | +779 | -213 | 915 | aj-app: account committed compaction usage live |
| 0c8b2a9 | 2026-08-25 | 5 | +837 | -66 | 903 | aj-models: labeled credentials per provider in the auth store |
| 20db96e | 2026-09-02 | 3 | +754 | -83 | 837 | aj: open session selector over connections |
| 110210c | 2026-08-06 | 7 | +802 | -7 | 809 | aj,aj-app: the session sidebar |
| e6d94fc | 2026-08-26 | 11 | +636 | -230 | 803 | aj-wire,aj-app,aj,docs: harden lock generation recovery |
| b68b4ec | 2026-08-19 | 1 | +570 | -227 | 797 | aj: fold a catch-up from the drive loop instead of parking it |
| c207347 | 2026-08-06 | 2 | +501 | -242 | 743 | aj-app,aj: switch sessions by swapping, not rebuilding |
| 832bf45 | 2026-08-12 | 9 | +585 | -160 | 740 | aj,aj-wire: keep a learned host id, and name a host with none by its address |
| f215150 | 2026-08-06 | 13 | +518 | -221 | 739 | aj-wire,aj-app,aj: take the tree view and branching over the wire |
| c94f2d9 | 2026-08-25 | 5 | +662 | -99 | 736 | aj-app,aj,docs: re-ask a locked refusal when the rival lets go |
| 6e373d1 | 2026-08-06 | 4 | +732 | -4 | 736 | aj-app,aj: give the client a session directory and a multi-session attach |
| 0765d8b | 2026-08-19 | 7 | +614 | -108 | 722 | aj: a gateway learns a host's name, keeps it, and republishes it |
| 65473dc | 2026-08-12 | 2 | +635 | -60 | 695 | aj: draw a host the peer holds no rows for as an empty group |
| d546b21 | 2026-08-04 | 3 | +557 | -119 | 676 | aj: pace the drive loop's retries and re-attach an evicted local stream |
| 905eb22 | 2026-08-11 | 4 | +548 | -105 | 653 | aj-session,aj: give a session's tag its own column and its own filter |
| a300297 | 2026-08-29 | 4 | +592 | -56 | 648 | aj-models: checkpoint state-owned openai terminals |
| 2f19fc2 | 2026-08-12 | 5 | +521 | -123 | 644 | aj-app,aj: a host refuses an attach per session, not per request |
| 86c76ed | 2026-08-19 | 3 | +572 | -55 | 627 | aj,aj-app: stop asking after a refused attach, rejoin when the row returns |
| cb3730a | 2026-08-26 | 1 | +614 | -1 | 615 | aj-app: pin detached sub-agent cancel routing |
| 9705a42 | 2026-08-23 | 13 | +610 | -16 | 615 | aj-wire,aj-app: publish the locked row bit |
| 0143cd4 | 2026-08-30 | 3 | +543 | -70 | 613 | vaxis: handle oversized grapheme widths |
| 004dc2e | 2026-08-03 | 5 | +531 | -76 | 607 | aj-app: close the host's ordering and teardown edges |
| a28b751 | 2026-08-29 | 2 | +605 | -0 | 605 | aj-models,aj-app: pin openai terminal retry boundaries |
| 4bdc6d6 | 2026-08-12 | 2 | +563 | -29 | 592 | aj-app,aj: give the canonical form a convergent tier |
| c66b959 | 2026-08-03 | 6 | +518 | -50 | 568 | aj-session: add the persisting forwarder and the session lock |
| a0badb0 | 2026-08-27 | 9 | +469 | -95 | 564 | aj-app,aj: close remaining shutdown races |
| 43df7cf | 2026-08-14 | 22 | +557 | -24 | 564 | aj-wire,aj-app,aj: let a host name itself for a reader |
| b3aaae9 | 2026-09-03 | 7 | +430 | -124 | 554 | session: repair an interrupted final write on reopen and say so |
| 0dcaf3f | 2026-08-07 | 7 | +535 | -9 | 544 | aj-app,aj: set a session's tag from the shell |
| 0694eb2 | 2026-08-30 | 3 | +428 | -130 | 540 | aj-models: honor requested pricing for default tier |
| 5fc079e | 2026-08-31 | 9 | +465 | -182 | 539 | agent: serialize live usage accounting |
| 39920f0 | 2026-08-26 | 6 | +516 | -20 | 536 | aj-session,aj-app,aj: break session usage down by provider and model |
| bc94ef0 | 2026-08-03 | 6 | +486 | -46 | 532 | aj-app: lock a session before its log is read, and derive live subs from finished ones |
| 26366f1 | 2026-08-26 | 25 | +468 | -59 | 527 | aj-models,aj-app,aj-session,aj-wire: record the resolved account on each turn |
| c45a206 | 2026-08-06 | 9 | +360 | -171 | 525 | aj-session,aj-app,aj: make the id-gate tests back their claims |
| 57db2b1 | 2026-08-19 | 5 | +467 | -58 | 525 | aj,aj-app: bound the wait for an attach block that never comes |
| 0936cb1 | 2026-08-07 | 9 | +251 | -274 | 525 | workspace: let the tests clean up their own scratch directories |
| 882ef4d | 2026-09-01 | 13 | +481 | -48 | 521 | aj: follow the focused host working directory |
| fe5d661 | 2026-09-01 | 1 | +501 | -19 | 520 | aj-models: make auth hardening guarantees load-bearing |
| 7cead11 | 2026-08-13 | 8 | +406 | -141 | 517 | aj,aj-app,docs: refuse a --host that names nothing, and say when it went unused |
| f00bad5 | 2026-08-06 | 10 | +456 | -63 | 514 | aj-app,aj,docs: back the tree and branch claims with tests that bite |
| 442b9b5 | 2026-08-07 | 1 | +418 | -91 | 509 | aj: toast what a session change has to say instead of folding it |
| 26a2f6a | 2026-08-31 | 1 | +467 | -40 | 507 | aj-app: close compaction accounting evidence gaps |
| 8e8ac61 | 2026-08-12 | 4 | +408 | -97 | 505 | aj: a gateway refuses an attach per session, not per stream |
| 558e77e | 2026-08-27 | 7 | +475 | -11 | 486 | aj-session,aj-app: persist and redact session environment entries |
| 70e7ee6 | 2026-08-31 | 2 | +423 | -62 | 485 | aj-tools: bound direct command reap after sigkill |
| 7c71560 | 2026-08-31 | 1 | +452 | -27 | 479 | aj-models: write auth storage atomically |
| c056f34 | 2026-09-01 | 4 | +415 | -45 | 460 | tools: use absolute gutters for read file output |
| 09f7dac | 2026-08-19 | 1 | +405 | -48 | 453 | aj: close the review findings on the loop-folded catch-up |
| b2f9b51 | 2026-08-28 | 4 | +416 | -24 | 440 | aj-app,aj: show session environment in info |
| 2c09755 | 2026-08-30 | 8 | +390 | -75 | 428 | models: mark issued handshake usage partial |
| 69bbe52 | 2026-09-02 | 5 | +224 | -197 | 421 | aj-app: add independent export sidebar filters |
| 34dd548 | 2026-08-27 | 4 | +368 | -5 | 373 | aj-agent,aj-tools: overlay session env on tool subshells |
| 8bf9e0a | 2026-08-31 | 5 | +307 | -61 | 368 | aj: close account login composition gaps |
| 532aaab | 2026-08-28 | 13 | +293 | -82 | 361 | aj-session,aj-app: resolve session environment gate findings |
| 763c87b | 2026-08-31 | 6 | +229 | -115 | 320 | models: preserve cancellation priority before request issuance |
| bdaefe6 | 2026-08-30 | 13 | +275 | -71 | 311 | aj-app: preserve checkpoint usage dependencies |
