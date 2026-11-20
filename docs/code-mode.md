# Experimental Code Mode

Enable `code_mode = true` in `.aj/config.toml` or `~/.aj/config.toml`, or use
the `code_mode` setting. Changes take effect next turn. The default is off.

Eligibility comes from the pinned Codex catalog's `tool_mode`, not a copied
name list. The selected provider must also declare freeform tool support.
The Responses and Codex Responses adapters support it.
Other selections use ordinary tools and show a fallback notice.

## Model interface

The model receives raw-JavaScript `exec` and JSON `wait`. Ordinary tools are
called through the injected `tools` object, not advertised a second time.
Descriptions and TypeScript declarations are generated with Codex's vendored
helpers from AJ's actual tool schemas. `ALL_TOOLS` exposes the same catalog.

```js
const result = await tools.bash({command: "git status --short", description: "Inspect changes"});
text(result.stdout);
```

No imports, Node, filesystem, or networking are provided by the evaluator.
Use AJ's tools for those operations. Only explicitly emitted output reaches
the model. `content()` forwards MCP content blocks or an iterable of blocks.
Text and image output are supported. AJ does not support audio
tool output. `notify()` displays a notice and queues model context for the
next inference boundary rather than sending duplicate tool results.

Evaluator isolation is not AJ's security boundary. Sandbox the whole agent,
including its filesystem and process tools.

Goal controls remain direct-only. AJ's input-yield tool is named `yield`
while Code Mode is active, leaving `wait` with Codex's cell-wait semantics.
It does not collect cell output. Delegation and background-task tools remain
available inside JavaScript, subject to the agent's existing capability limits.

Each tool declares its `CodeModeExposure`: `Nested` by default, or `DirectOnly`.
The catalog and dispatch checks use that metadata, independently of execution
scheduling. Changes to exposure also apply to new calls from existing cells.

`apply_patch` accepts a patch string, matching Codex's freeform interface:
`await tools.apply_patch("*** Begin Patch\n...\n*** End Patch")`.
Outside Code Mode, providers declaring freeform support advertise the raw patch
grammar. Other providers advertise the JSON `{patchText: "..."}` interface.
Existing JSON calls remain executable. Hooks receive the actual argument shape,
string or object.
See [patch compatibility](../third-party/codex-apply-patch/README.md) for scope.

Tools with structured results expose those schemas. Bash returns the
command's combined output as `stdout`, an always-empty `stderr`, the exit
status, truncation and background-task metadata. Text reads
return unnumbered text plus line and continuation metadata. Other tools
return content blocks, including images that can be forwarded with `image()`.
Tool failures reject the JavaScript call and can be caught with `try/catch`.

The runtime bounds buffered output and applies each `exec` or `wait` output
budget, including `wait` with `terminate: true`. Oversized text retains its
beginning and end. Loss notices and execution status remain visible even with
a zero output budget. A truncated error still marks the script as failed.
`exit()` stops the whole cell immediately, including catch/finally handlers,
and commits only writes made before it was called.

## Lifetime and visibility

Cells can survive an answer and be collected in a later turn. Each subagent
has its own evaluator. Running cells are not restored after process restart
or session resume, and changing branches shuts down the live evaluator.

A cell that was running when its host ended is lost, and the agent that
started it is told once. A cell still inside its `exec` call has that call
answered as interrupted by the next open. A cell that had yielded gets a
task notification at the next open that says it was lost and its state is
gone. It is saved when the cell yields, and the cell's completion replaces it
([background tasks](background-tasks-spec.md), section 7). If it cannot be
saved, the cell is terminated and its `exec` call fails.

`store()` and `load()` keep JSON values on the agent's branch.
Committed values survive restart, resume, compaction, and disabling Code Mode.
Changing branches restores that branch's values. Subagent stores are separate
from Main and from each other. This does not make archived subagents runnable
after resume.

When a script finishes, its writes are recorded as one `code_mode_store` entry
in the session log, including if the script ends with an ordinary error.
The entry is committed before the new values or completed result become visible.
No `wait` collection is required to save a completed script's writes. No entry
is added for scripts that make no writes. Values are execution state, not model
messages, and remain outside the model context and the rendered transcript.

Cells read a snapshot of committed values plus their own pending writes.
Concurrent commits are serialized, with later commits replacing earlier values
for the same key. If cancellation wins before completion is accepted, pending
writes are discarded. A session that shuts down is closed for writing first, so a
script that completes after that commits nothing, as when the host is lost.
Neither cancellation nor a script error rolls back filesystem changes or other
tool side effects. A persistence failure stops execution rather than allowing
the agent to continue with uncertain store contents. Durability has the same
process-restart guarantee as other log entries, not a separate power-loss guarantee.

A yielded cell appears in the existing task controls as **open until
collected**. Evaluation may already be finished while the cell retains its
final result. Use `wait` to collect it or terminate it, or stop its task.
The task closes when the result is collected or the cell is terminated,
not when unobserved evaluation finishes. It does not independently wake the
agent at evaluation completion.

Nested calls use AJ's hooks, cancellation and scheduling. Their activity is
visible and persisted without becoming separate model tool results, including
on resume. Sequential resource operations remain exclusive across cells.
Runtime control tools can observe or cancel those operations without waiting
for their resource permits.

At the next turn, new nested dispatches use the effective tool catalog and
configuration. Calls already admitted retain their execution context.
Interrupting a turn cancels that agent's open cells. Disabling Code Mode or
switching to an ineligible model cancels them before ordinary execution resumes.
Session shutdown ends every cell and waits for its callbacks to settle. It
writes nothing for them: the next open reports each open cell as lost.

## Builds and vendoring

Run `python3 scripts/bootstrap-codex-code-mode-v8.py` before ordinary Cargo
commands in a fresh checkout. Native GNU Linux and macOS archive/binding pairs
are pinned for x86_64 and aarch64. Cross-compilation is not configured. No Node
installation is required at runtime. The extracted runtime excludes Codex's
independent UI/plugin runner. V8 is linked into AJ even when the setting
is off.

[Vendor documentation](../third-party/codex-code-mode/README.md) describes
source provenance, explicit extraction patches, checksum verification and
repeatable full-revision upgrades. Source licenses and notices are included.
Native binary redistribution is not cleared by this experiment. See the
[native licensing evidence](../third-party/codex-code-mode/NATIVE-LICENSES.md)
for the remaining notice and provenance work before distributing binaries.
