# Test timing

Tests that need real elapsed time are gated behind the `slow-tests` feature
with `ignore = "slow: <what it waits for>"`. The fast suite waits on events,
not on timers. `rg 'ignore = "slow' src` lists the current slow inventory
with the reason for each.

## Deliberately retained real time

These tests keep wall-clock waits because the wait is the behavior under
test, or because nothing observable marks the moment the test needs.

- **Process teardown.** `SIGTERM` to `SIGKILL` escalation, command timeouts
  and the survival of a process a command left behind are OS integration
  tests in `aj-tools`. They run the production grace periods.
- **The RTK hook deadline.** The test runs a real hook process under an
  isolated `PATH` and waits out the 500 ms timeout.
- **`std::time::Instant` clocks.** Host idle release and goal elapsed
  accounting read `Instant`, which Tokio's paused clock does not move.
  `start_paused` alone cannot replace their real-time tests.
- **Filesystem watcher delivery and cross-process lock exclusion.** Both
  cross a process or kernel boundary that no virtual clock controls.
- **Condvar waiters in `vaxis::queue`.** Short sleeps let a waiter block
  before the test wakes it. Waiter registration is not observable without
  extra coordination, which these small delays do not justify.
- **Remote quiet windows.** Catch-up, backpressure, reconnect and heartbeat
  tests observe that nothing more arrives, or drive real socket timeouts.
- **Vendored Code Mode tests** keep their own virtual-clock and V8-thread
  coordination and are not part of the workspace suite.

A conditional poll with a generous failure deadline is not a fixed wait. It
stays where no notification gives the same evidence.

## Patterns to reuse

- **Paused Tokio time through a composed path.** Print-mode tests in
  `src/aj-app/src/print.rs` drive whole turns with `start_paused`, including
  the scripted provider's chunk delays. The host shutdown and lock-release
  tests in `src/aj-app/tests/session_host.rs` and the agent cancellation
  tests in `src/aj-agent/src/lib.rs` do the same.
- **A frame as an ordering fence.** The goal wait tests in
  `src/aj-app/tests/session_host/goals/wait.rs` read up to the idle state
  frame that follows the turn join, then assert.
- **Real I/O first, then advance the clock.** The gateway preview tests in
  `src/aj/src/gateway/tests.rs` let real socket work settle, then call
  `tokio::time::pause` and `advance` past the budget.

A general clock abstraction that saves one second usually costs more than
the small real-time test it replaces.
