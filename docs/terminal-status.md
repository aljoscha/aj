# Terminal program status

The interactive UI reports its selected session through
[OSC 7501](https://www.superlogical.com/rex/docs/build/program-status), the
Program Status Protocol supported by Rex. Terminal presentation is up to the
terminal. AJ sends changes without a capability query or heartbeat.

- `working`: an agent, compaction, or background task is running in the
  selected session, or an active goal is continuing between turns. An explicit
  yield without running work is idle even with an active goal. Changing the
  viewed sub-agent does not change the scope.
- `done`: Main completed a live turn successfully.
- `error`: Main's live turn failed. A recoverable tool error alone does not
  count as a failed turn.
- `idle`: no work is running, including after cancellation or explicit yield.
- `blocked`, `kind=auth`: the login flow requires browser authorization or
  input. Ordinary idle time and waiting for background work are not blocked.

Deliberate keyboard, paste, or mouse input acknowledges a completion or error.
Selecting a session and replaying history do not announce fresh completion.
Disconnected or catching-up sessions clear their status rather than reporting
stale work. Leaving the selected session's drive loop clears the record,
including normal exit and render errors.

Reports use one root record with `app=aj`. AJ does not send model text, titles,
percentages, child records, or OSC 9;4 fallback reports. It does not infer
permission requests or questions from model prose. Print mode and headless
hosts send no status escapes. Known VTE versions older than 0.54 are excluded
because they cannot safely ignore unknown OSC sequences.
