# Experimental Code Mode

Enable `code_mode = true` in `.aj/config.toml` or `~/.aj/config.toml`, or use
the `code_mode` setting. Changes take effect next turn. The default is off.

Eligibility comes from the pinned Codex catalog's `tool_mode`, not a copied
name list. AJ also requires the OpenAI Responses or Codex Responses API.
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
the model. Text and image output are supported. AJ does not support audio
tool output. `notify()` displays a notice and queues model context for the
next inference boundary rather than sending duplicate tool results.

Evaluator isolation is not AJ's security boundary. Sandbox the whole agent,
including its filesystem and process tools.

Goal controls remain direct-only. AJ's input-yield tool is named `yield`
while Code Mode is active, leaving `wait` with Codex's cell-wait semantics.
It does not collect cell output. Delegation and background-task tools remain
available inside JavaScript, subject to the agent's existing capability limits.

Tools with structured results expose those schemas. Bash returns stdout,
stderr, exit status, truncation and background-task metadata. Text reads
return unnumbered text plus line and continuation metadata. Other tools
return content blocks, including images that can be forwarded with `image()`.
Tool failures reject the JavaScript call and can be caught with `try/catch`.

## Lifetime and visibility

Cells can survive an answer and be collected in a later turn. `store()` and
`load()` share JSON values within that agent's live session. Each subagent
has its own evaluator. Nothing is restored after process restart or session
resume, and changing branches clears this ephemeral state.

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

At the next run, new nested dispatches use the effective tool catalog and
configuration. Calls already admitted retain their execution context.
Interrupting a run cancels that agent's open cells. Disabling Code Mode or
switching to an ineligible model cancels them before ordinary execution resumes.
Session shutdown owns their cancellation and waits for callbacks to settle.

## Builds and vendoring

Run `python3 scripts/bootstrap-codex-code-mode-v8.py` before ordinary Cargo
commands in a fresh checkout. Native GNU Linux and macOS archive/binding pairs
are pinned for x86_64 and aarch64. Cross-compilation is not configured. No Node
installation is required at runtime. V8 is linked into AJ even when the setting
is off.

[Vendor documentation](../third-party/codex-code-mode/README.md) describes
source provenance, explicit extraction patches, checksum verification and
repeatable full-revision upgrades. Source licenses and notices are included.
Native binary redistribution is not cleared by this experiment. See the
[native licensing evidence](../third-party/codex-code-mode/NATIVE-LICENSES.md)
for the remaining notice and provenance work before distributing binaries.
