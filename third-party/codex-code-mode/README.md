# Codex Code Mode extraction

Source: <https://github.com/openai/codex>, commit
`44984d20817fc58026a8bbccf053e288c6897c8f`.

`vendor/codex-code-mode` is an independent Cargo workspace containing:

- `code-mode-protocol`, package `codex-code-mode-protocol`
- `code-mode-runtime`, package `codex-code-mode-runtime`
- The complete upstream `models-manager/models.json`, at `models.json`
- Unmodified upstream `LICENSE`, `NOTICE`, and `clippy.toml`
- `v8-artifacts.json`, derived from the pinned `MODULE.bazel`

Patch `0001-standalone-protocol.patch` decouples Codex's larger protocol crate by extracting
`ToolName`, model tool messages, and the audio size limit. Only the model-message
schema/TypeScript derives are removed. Protocol gRPC and its build dependencies
are behind the optional `grpc` feature, disabled by default. The V8 sandbox
feature and all upstream tests are retained. Changed files carry
modification notices. `upstream.json` records SHA-256 hashes of source inputs,
the patch, workspace template, and every imported file.

Patch `0002-durable-store.patch` is an AJ runtime extension. It adds initial
stored values and an asynchronous delegate callback for completed writes.
Completion is reserved against cancellation before calling the host. New cell
snapshots, other commits, and completion delivery wait for acknowledgment.
Termination during an accepted commit waits for it rather than rolling it back.
Failed acknowledgment leaves the runtime map unchanged and reports an error.
Writes preceding an ordinary script error still commit, matching Codex semantics.
The default callback does nothing, preserving memory-only embedding behavior.
AJ supplies the callback through its persistence event bus and stops the agent
if acknowledgment fails. The patch includes native contract tests.

## Parent integration

Exclude `vendor/codex-code-mode` from the parent workspace and add path
dependencies as needed:

```toml
[workspace]
exclude = ["vendor/codex-code-mode"]

[workspace.dependencies]
codex-code-mode-protocol = { path = "vendor/codex-code-mode/code-mode-protocol" }
codex-code-mode-runtime = { path = "vendor/codex-code-mode/code-mode-runtime" }
```

Consume `vendor/codex-code-mode/models.json` directly. Select catalog entries by
their `tool_mode` field (`code_mode` or `code_mode_only`), not by a hand-maintained
model-name list. `code_mode_only` also indicates the upstream model requires
Code Mode rather than direct tool calls. Missing, `direct`, or unknown values
do not establish eligibility. The catalog is data, not an AJ model-registry
override. Eligibility policy and provider gating belong to the parent.

The nested `Cargo.lock` pins standalone verification. Parent builds resolve
their own lockfile. Keep V8 at the workspace's exact `150.4.0` requirement and
retain its `v8_enable_sandbox` feature.

## Native prerequisite

From the AJ checkout, with Python 3.11+, Rust/Cargo, curl, and a native C++ linker:

```sh
python3 scripts/bootstrap-codex-code-mode-v8.py
cargo test --manifest-path vendor/codex-code-mode/Cargo.toml --workspace --locked
```

After bootstrap, ordinary Cargo commands work without shell exports or a Cargo
wrapper. The repository `.cargo/config.toml` selects the local archive and
bindings using Rusty V8's supported `RUSTY_V8_ARCHIVE` and
`RUSTY_V8_SRC_BINDING_PATH` settings. Run Cargo from within the checkout so it
discovers that configuration. Bootstrap supports native x86_64 and aarch64 on
Linux GNU and macOS. Other targets and cross-compilation are not supported by
this fixed native pair. Do not share its directory between host architectures.

Both downloads must match the pinned Bazel SHA-256 hashes before the pair is
published under the ignored `third-party/codex-code-mode/native/` directory.
Failed downloads leave no installed partial pair. Existing pairs are verified,
never overwritten. An offline source can be supplied with `--from-dir DIR`,
containing the upstream artifact filenames. Revalidate an installation with:

```sh
python3 scripts/bootstrap-codex-code-mode-v8.py --check
```

**Build tradeoff:** Rusty V8 150.4.0 has no content-checksum verification in its
downloader. Its `.sum` records the URL, not a digest. `RUSTY_V8_MIRROR` constructs
`BASE/v150.4.0/...` URLs, unlike Codex's `rusty-v8-v150.4.0` release tag, and writes
downloaded bindings into the dependency source directory. Fully automatic,
verified first-build downloads would require a Rusty V8 change or additional
build machinery. This extraction instead requires the explicit prerequisite.
No dependency cache is patched and sandboxing is not disabled.

Cargo does not rehash the installed pair on every build. Run bootstrap in CI
before Cargo, including when restoring cached native files. Explicit environment
overrides of Cargo's defaults are outside this verified configuration. The
archive is statically linked, so release binaries retain V8's size and native
distribution costs. Native binaries are not committed here. Binary distributors
must also carry the licenses/notices required by V8 and its bundled dependencies,
in addition to the extracted Codex notices.
See [native licensing evidence and unresolved release requirements](NATIVE-LICENSES.md).

## Reproduction and verification

```sh
python3 scripts/import-codex-code-mode.py --upstream ../codex --check
cargo fetch --manifest-path vendor/codex-code-mode/Cargo.toml --locked
python3 scripts/test-codex-code-mode.py --upstream ../codex
cargo fmt --manifest-path vendor/codex-code-mode/Cargo.toml --all -- --check
cargo clippy --manifest-path vendor/codex-code-mode/Cargo.toml --workspace --all-targets --all-features --locked -- -D warnings
cargo test --manifest-path vendor/codex-code-mode/Cargo.toml --workspace --all-features --locked
```

The parent-integration contract resolves a temporary parent workspace offline.
`cargo fetch` supplies its cross-platform metadata dependencies even on a native
Linux host. It checks that both path dependencies retain their nested workspace
definitions and omit gRPC dependencies by default, without building AJ.

The importer reads the recorded commit with `git show`, independent of the clone's
working tree or current branch. An initial import uses the pin above. Without
`--check`, it creates an absent vendor tree or verifies an existing one.
To deliberately upgrade, first make the exact commit available in a local clone,
review its licensing and dependencies, and reconcile any local vendor edits:

```sh
python3 scripts/import-codex-code-mode.py --upstream ../codex --upgrade FULL_40_CHARACTER_COMMIT_SHA
python3 scripts/import-codex-code-mode.py --upstream ../codex --check
```

The importer never fetches or accepts moving refs. Upgrade verifies the existing
tree against its recorded hashes before building the complete replacement and
checking/applying every patch in scratch space. Changed patches or workspace
inputs require explicit `--upgrade`, including when retaining the same revision.
Edits, deletions, extra files or empty directories, symlinks, special files, and
missing provenance in import-owned paths are rejected. The Cargo-owned target
directory is not inspected internally. Do not edit the provenance record to
bypass this check.
The new revision becomes the default for subsequent verification and reimports.
Cargo's root `target/` and `Cargo.lock` are moved intact, not regenerated. Review
and refresh lockfiles and native artifacts separately when dependencies change.
Licensing evidence is revision-specific and must also be reviewed on upgrade.

### AJ compatibility review

After an upstream refresh, review both extraction patches, not only whether
they apply. In particular, inspect the completion/cancellation state machine
against `code-mode-runtime/tests/stored_values.rs`. It must not expose values or
a completed response before AJ acknowledges the store update. Also run AJ's
`code_mode` agent tests and `code_mode_store` application tests, then the normal
workspace gate. The latter covers persistence, branch selection, compaction,
resume, and provider-context exclusion through the composed application.

The raw `exec` grammar in `src/aj-agent/src/code_mode.rs` is copied from Codex's
`core/src/tools/code_mode/execute_spec.rs`, outside the imported crates. Compare
it explicitly, along with direct-only tool policy and the small AJ description
overrides, when reviewing an upgrade. Do not assume a successful import checks
these host-level interfaces.

To change a local patch, work in a separate copy of the verified extraction and
generate a patch relative to that workspace. Add it under `patches/`, then run
`--upgrade` with the recorded revision if upstream itself is unchanged. Do not
edit the installed vendor tree or its provenance hashes to bypass verification.
Both unified and Git-format patches are applied outside the enclosing AJ Git
worktree's prefix rules, so they cannot silently skip paths inside staging.

**Single-writer contract:** stop Cargo, editors, bootstrap, and other importers
while importing. Publication uses same-filesystem staging and a backup rename,
with rollback on Python-visible failures. It is not a crash-atomic transaction
across the vendor directory and provenance record. If killed or interrupted by
power loss, preserve `.codemode-import-*` under `vendor/` and
`.upstream.json.pending` under this directory. Recover the old tree from `old/`
and any Cargo state moved into `new/` (or the installed tree), then reconcile
the record before retrying. Failed rollback retains its backup for recovery.

Verification on Linux x86_64 includes a fresh network bootstrap, standalone
Cargo builds without V8 shell exports, default and gRPC-feature upstream tests,
and importer/bootstrap contracts. The contracts exercise all four artifact
selections using distinct fixtures. macOS and Linux aarch64 native execution
still require their respective hosts. No whole-AJ build is part of this check.
