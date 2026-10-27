# Compile-time audit

## Scope and method

Baseline: `5e3819c4`, Rust/Cargo 1.98.1 on eight logical Linux CPUs with 18 GiB
RAM. Measurements use the existing development profile, a separate initially
empty target directory, and Cargo `--timings`. The registry/download cache and
the verified local V8 archive were already present. "Clean" therefore means
clean compilation artifacts, not downloading dependencies or building V8 from
C++. No competing Cargo jobs ran.

Compiler diagnostics used `RUSTC_BOOTSTRAP=1` with the installed compiler's
`-Ztime-passes`, JSON timing output and `-Zdump-mono-stats`. No toolchain or
project compiler setting was changed. Compiler-phase measurements use fresh
per-crate incremental directories so they do not mistake reused queries for
cheap cold compilation. Those diagnostic timings are separate from ordinary
edit/rebuild measurements.

The audit covers development and test compilation. Release compilation and
the separate vendor-workspace CI job are not benchmarked.

## Ordinary builds

| Workload | Wall time |
| --- | ---: |
| Clean `cargo build -p aj --bin aj` | 59.990 s |
| Repeat unchanged binary build | 0.195 s |
| First `cargo test --workspace --no-run` after that build | 65.442 s |
| Repeat unchanged test build | 0.244 s |
| Switch back to the already-built production feature set | 0.198 s |
| Small private sidebar calculation edit, binary rebuild | 4.257 / 2.768 s |
| Same kind of edit, workspace test artifact rebuild | 6.388 s |
| Private registry staleness-threshold edit, binary rebuild | 8.049 s |
| Same shared-library edit, workspace test artifact rebuild repeats | 18.815 / 15.237 s |

The edit measurements changed a constant in a private sidebar calculation
and a private registry threshold, then restored the source. They measure real
incremental invalidation rather than an unchanged command. No altered
executable or test was run. These are
specific edit cases, not guarantees for arbitrary changes.

The shared-library edit rebuilds **38 units**, versus two for the sidebar edit.
Its first test rebuild took **58.711 s**, followed by 20.823 s restoring the
source. The explicit repeats above were substantially faster. The timing
reports show several downstream test executables delayed together in the
outlier, but do not establish the cause. Treat it as observed variability,
not a typical 59-second compiler cost or proof of an async problem.

### Clean-build critical path

AWS-LC's native build script takes **26.77 s**, delaying the TLS/HTTP chain.
The final `aj` binary compilation takes **11.34 s**. Other large units include
`aj-models` (5.77 s), `moxcms` (4.88 s), `image` (4.27 s), `aj-app` (4.10 s)
and Tokio (3.95 s). Units overlap, so these durations are not additive.

V8's build-script run takes **1.26 s**, its Rust library **2.02 s**, and the
code-mode runtime **2.59 s**. The local archive avoids a full native V8 build.
Normal source edits do not rebuild these native dependencies.

Test compilation adds real work, not an accidental second build of identical
inputs. Tokio gains `test-util`, and workspace crates gain their test-support
features. Unit-test harnesses and integration executables also need compilation.
Only 84 of the test report's 503 units do work. The largest unit is the `aj`
test executable (**36.70 s**), followed by the `aj-models` test executable
(19.00 s) and the `aj-app` test executable (14.81 s). The normal `aj` executable
also builds with the test graph's unified dependency features.

"Fast" selects execution, not necessarily compilation. Tests marked ignored
without `slow-tests` still compile into the test harness. Likewise, a libtest
name filter does not avoid compiling the rest of that target. Selecting a
package/target can avoid unrelated test executables, but excluding slow test
bodies at compile time would be a separate coverage/discovery policy change.

## Compiler phases and async

With dependencies warm but the final crate's incremental queries cold:

| Compiler measurement | Binary | Test harness |
| --- | ---: | ---: |
| Total rustc time | 10.407 s | 31.825 s |
| Type checking | 1.690 s | 6.222 s |
| MIR borrow checking | 1.225 s | 6.741 s |
| Generic instance collection | 2.991 s | 6.668 s |
| Code generation, including nested stages | 5.619 s | 14.494 s |
| Linker invocation | 0.973 s | 1.208 s |

Compiler phases overlap or nest. In particular, generic collection and LLVM
work are included within broader code-generation measurements. Do not sum
the table to obtain total time.

Monomorphization statistics record roughly **39,000 instances** in the binary
and **98,000** in its test harness. Tokio task machinery, panic handling,
Serde and ordinary collection operations contribute repeated instances.
Async therefore contributes code-generation work, but these measurements do
not identify a pathological async abstraction or justify blanket boxing.
`interactive::drive` is the largest individual MIR-size estimate, but has one
instance. MIR size estimates are neither compiler timings nor machine-code
sizes. Much of the test expansion is distributed across the composed tests.

## Configuration assessment

The setup is broadly reasonable:

- Development application code is unoptimized, with line-table debug info and
  incremental compilation. The one optimized certificate-decoding dependency
  takes 0.71 s in this clean build and is reused for tests.
- No-op builds are about 0.2 s, with no evidence of always-dirty generation.
  Unicode generation watches specific data files. The vendor protocol build
  script's non-gRPC path has package-wide change detection, but it does not
  rerun on arbitrary AJ edits and is not a measured bottleneck.
- Syn 2 and 3 both compile because multiple dependencies require them.
  Removing the workspace's direct usage alone would not eliminate a version.
- `--all-targets` includes examples and integration targets, not just the
  executable. Use the target appropriate to the edit during iteration and
  retain broad checks for the final range.
- The existing 137 GiB target cache contains many accumulated artifact and
  incremental variants. A fresh binary-plus-test build occupied about 11 GiB.
  Cache size alone is not evidence of repeated recompilation. The audit does
  not delete or rewrite the pre-existing cache.

There is no measured reason here to change TLS backends, remove code mode,
switch linkers, box futures broadly or split architectural crates merely to
improve compilation. The main costs are native cold compilation and the
size/dependency reach of the test build. Reconsider test executable layout
only if cold CI compilation, rather than ordinary incremental edits, becomes
the limiting workflow.

## Verification and reproduction

All temporary source changes were restored. The default scratch-guarded
workspace suite then passed **4,083 tests**, with 191 ignored and no residue.
Formatting and diff checks passed. Only this report is retained in the
repository. No production source, manifest or compiler configuration changed.

Cargo's ordinary build reports can be reproduced with:

```sh
cargo build -p aj --bin aj --timings
cargo test --workspace --no-run --timings
```

For a clean-artifact comparison, set `CARGO_TARGET_DIR` to a new directory
rather than cleaning the working build cache. Reports are written beneath
`CARGO_TARGET_DIR/cargo-timings`. Compiler phase diagnostics can be restricted
to one final crate with `cargo rustc -p aj --bin aj --profile test -- ...`.
They need unstable compiler options, not a permanently enabled nightly
toolchain or changes to the project's normal flags.
