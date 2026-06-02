<!--
Copy this file to docs/audit/findings/<unit>.md and fill it in.
<unit> matches the step's findings filename in audit-progress.md
(e.g. aj-models-core). Keep the section headings; delete this comment.
-->
# Audit findings — <unit>

- **Step:** <step id, e.g. M1>
- **Date:** <YYYY-MM-DD>
- **Audited commit:** <git short sha>
- **Scope:** <files reviewed>

## Summary

Two to four sentences on overall health, the standout themes, and whether
this unit's boundaries hold up.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 0 | 0 | 0 |

## Findings

<!-- One block per finding, ordered by severity. Use file:line anchors. -->

### [SEVERITY][Category] Short title — `path/to/file.rs:line`
**What:** the issue, concretely.
**Why it matters:** the consequence.
**Suggested action:** what a fix would do.
**Effort:** S | M | L

## What's good

Patterns worth preserving or replicating elsewhere. Dimensions that came
back clean.

## Boundary & architecture notes

How this unit's dependency direction and public surface compare to the
intended graph in `CLAUDE.md`. Anything to verify in the synthesis step.

## Test assessment

Do the tests exercise the boundary/contract? Notable gaps, edge cases
missed, fixture/helper quality, flakiness risks.

## Cross-cutting themes to bubble up

Observations that likely recur in other crates — collected by the X1
synthesis step.
