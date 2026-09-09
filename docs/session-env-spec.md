# Session environment

## User experience

A session environment is an overlay on the environment inherited by its tool
processes. It can be supplied at creation and deliberately edited afterwards.
The overlay belongs to the current conversation branch, like recorded inference
settings. It is not an installation-wide default or an immutable identity.

The `env` command in the command palette opens **Session environment**, using
the settings window's filterable key/value rows:

- Enter on a variable opens its value editor. Enter confirms the edit and
  returns to the list. Escape cancels the field edit.
- **Add variable** asks for a name and then a value. An existing name must be
  edited through its row rather than accidentally replaced by an add.
- The settings clear binding (Ctrl+X by default) removes the selected variable.
  Saving an empty value keeps the variable with an empty value.
- Confirmed edits apply individually. The list remains open and changes only
  after the host accepts an edit. A refusal is shown without claiming success.
- Escape closes the list. There is no whole-document save, configuration write,
  or implicit environment change from opening the window.

The one-line field editors use JSON string escapes without enclosing quotes.
Ordinary text and spaces are literal. A newline is `\n`, a tab is `\t`, a
backslash is `\\`, and a quote is `\"`. Existing non-ASCII text is displayed
as Unicode escapes. Encoding is lossless, including leading/trailing spaces,
carriage returns, multiline values, and supplementary Unicode characters.
Invalid escapes or invalid environment entries leave the field open to correct.

Local and connected sessions use the same editor through `Control`. A host that
does not serve the environment endpoint produces a notice rather than an empty
editable map. The environment and its values are available over the trusted
control connection. HTML export is a separate artifact boundary and redacts
values.

## Branch and execution semantics

Each `EnvChange` states the complete resulting map. The last record on the
selected user-thread ancestry wins, without merging earlier maps. An empty map
clears all session overrides. Absence of a record means no overlay.

A branch inherits the map at the point it forks. Changes made afterwards on one
branch do not change its siblings. Selecting an earlier head, including the
system-prompt root before the environment seed, restores the map at that point.
Compaction does not remove state records from the ancestry used for extraction.
Resuming or materializing the log restores the environment of the selected
branch, not the launching process's `--env` values. Head selection follows the
session log's existing persistence rules.

Edits are refused while agents or background commands are running, using the
same idle requirement as head switching. An accepted edit installs the map for
subsequent tool invocations by the main agent and its sub-agents, including
parked sub-agents used again. Separate sessions do not share the map. No edit
can change the environment of a process that has already been spawned.

Layering in a tool child is:

1. Inherited host-process environment, including loaded `.env` files.
2. The selected branch's session map.
3. The tool's fixed determinism overrides, such as `TERM=dumb`, `NO_COLOR=1`,
   and `AGENT=aj`.

Removing a session key exposes any inherited value. It is not an instruction to
unset a host-process variable. Collisions with fixed overrides are allowed but
those overrides win. The optional `rtk hook check` inherits the host environment,
and its answer runs unchanged in the tool shell. AJ does not adjust PATH or bind
executables to protect RTK from session or command-local environment changes.
The frozen system prompt's workspace `<env>` block is unrelated to this map.

## Validation and persistence

Keys are exact and case-sensitive on supported Unix execution hosts. A key must
be non-empty and contain neither `=` nor NUL. Values may be empty and must not
contain NUL. The same validation applies at create, edit, and log-resume
boundaries. Invalid recorded maps are refused before tail repair can modify the
file.

The log retains the `env_change` discriminator and full string-to-string map.
New environment records are user-thread state entries parented at the current
head. They are non-punctuation, like inference-setting records. A fresh session
with only seeds leaves no conversation file. A confirmed edit to an established
log flushes the state entry before reporting success. An unchanged value or
removal of an absent key appends nothing.

First publication retains the log's existing transactional initial-write
contract: the pending prefix and first punctuation are installed together at the
canonical path without replacing an existing log. This feature does not change
staging, error recovery, or power-loss durability.

### Existing logs

An existing single root-parented Meta `EnvChange` creation record remains valid.
It supplies that log's baseline when the selected ancestry has no user-thread
environment record. A user-thread record overrides the entire baseline map,
including when it is empty. Other branches without an override retain the
baseline. No log rewrite or migration is required.

Logs with no environment records remain valid. Readers that only understand
immutable Meta environment records cannot read the new user-thread records and
must be upgraded before using edited sessions.

## Creation defaults

`--env KEY=VALUE` remains a repeatable global launch flag. It splits at the
first equals sign and refuses duplicate keys, missing equals signs, or invalid
entries. It has no process-environment binding and no `config.toml` equivalent.

For local interactive, connected, and print runs, the map applies to sessions
the invocation creates, including later in-TUI new sessions. It is passed
explicitly to each create, not installed in the host's base configuration.
Other clients creating sessions on an embedded host therefore do not inherit
the launcher's map.

On continue or attach, the existing session keeps its recorded branch map. The
launch map stays available for later creates, and a notice explains that the
flag did not edit the resumed session. Use the environment window for a
conscious edit. `serve` and `gateway` refuse `--env` because those invocations do
not create a session themselves.

Remote creates carry an optional top-level `env` map in `CreateSessionRequest`.
The host validates the map before minting and records it as the initial branch
environment. An explicitly empty map is recorded as empty, while an absent map
records no environment. Gateways forward it without interpreting its contents.

A protocol-2 host predating this field refuses it under strict request decoding,
before minting a session. Protocol-1 peers are refused at hello. Clients do not
retry after stripping the map, and no environment echo, proof exchange, or
capability pre-gate is needed. Successful creation keeps the ordinary
`SessionCreated` response and existing partial-create behavior.

## Host and wire boundary

`SessionHost::environment` reads the selected map, materializing the session as
needed like the tree read. A field-edit command carries a key and either a
string value or removal. The host edits its current map, rather than accepting a
client's stale copy of unrelated keys, and records the full resulting map.

GET `/v1/sessions/{id}/env` returns the string-to-string map, including values.
POST to the same route accepts a `key` and a `value`. A string value sets the
key, including an empty string. A null or omitted value removes it. Unknown
request fields are refused before dispatch. Gateways forward both operations to
the owning host. Hosts advertise the `session_env` capability, which is not a
client-side precondition for trying the operation.

The read is requested when the editor opens, not during directory enumeration.
Environment values do not enter directory rows or `state` frames. Live notices
and replay name only the keys using terminal-safe quoting. Seeds before the
first message and legacy Meta records project no environment notice.

Session info reports the selected branch's map. Export redacts every
`EnvChange` value in the embedded payload before JavaScript receives it, keeping
keys and replacing values with `[redacted]`. Export never mutates the source
log. Its state rows remain navigable and keys-only.

## Verification boundaries

Coverage observes whole-map replacement, absent versus empty, sibling isolation,
root-head selection, compaction, persisted resume, and the legacy Meta baseline
at the log API. Runtime tests observe the actual bash child after edits and head
switches, plus inheritance by sub-agents and session isolation. Host and wire
tests cover set/remove, no-ops, invalid and busy refusals, and local/remote and
gateway behavior. Editor tests drive the actual palette, settings widgets, and
drive loop, including cancellation, invalid drafts, whitespace, and empty values.
Export and replay retain value-nondisclosure coverage at their output boundaries.
