#!/usr/bin/env bash
# Guard: the test suite must not leave scratch files behind.
#
# Run the whole suite with `TMPDIR` pointed at an empty directory and fail if
# anything is still in it afterwards. Scratch space belongs to an owning guard
# (`TempDir`), so residue means some test handed out a bare path, or dropped its
# guard while something was still writing. Both have bitten this project, and
# both are invisible until /tmp fills up.
#
# The suite is built first, under the ambient temp directory, so that rustc's
# own scratch files are not mistaken for a test's.
# Arguments select a cargo test target or scheduling mode. With no arguments,
# the guard runs the whole workspace with ordinary test parallelism.
set -euo pipefail

# State that legitimately outlives a test lives under one named per-process
# root, which nothing is left to remove: the process is gone. Each entry here
# needs that justification, and the pattern must be specific enough that a new
# leak cannot hide behind it.
#
#   aj-usage-XXXXXX  the usage overlay spawns its fetch onto a deliberately
#                    leaked runtime, so a task holding a clone of its credential
#                    store outlives the test that built it and writes afterwards.
#                    TempDir adds exactly six random alphanumeric characters to
#                    the prefix; stores use per-test subdirectories below it.
#   aj-task-lifetime-XXXXXX
#                    fixtures whose owner tasks can survive a panic use this
#                    process-lifetime root. This includes the OAuth cancellation
#                    race's non-yielding poll and composed host tests whose
#                    detached tasks can still reach persisted session state.
#                    Each fixture uses a distinct subdirectory below this root.
allowed=(
    'aj-usage-[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]'
    'aj-task-lifetime-[A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9][A-Za-z0-9]'
)

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

scan="$(mktemp)"
trap 'rm -rf "$scratch" "$scan"' EXIT

if (( $# == 0 )); then
    set -- --workspace
fi
cargo test --no-run --quiet "$@"
TMPDIR="$scratch" cargo test --quiet "$@"

# Bash does not propagate a process substitution's status through its reader,
# so materialize the NUL stream before accepting an empty scan as clean.
if ! find "$scratch" -mindepth 1 -maxdepth 1 -print0 >"$scan"; then
    echo "error: could not scan test scratch space for residue" >&2
    exit 1
fi

residue=()
while IFS= read -r -d '' entry; do
    # Command substitution strips trailing newlines from filenames.
    name="${entry##*/}"
    for pattern in "${allowed[@]}"; do
        # shellcheck disable=SC2053 # the pattern is meant to glob
        if [[ $name == $pattern ]]; then
            continue 2
        fi
    done
    residue+=("$name")
done <"$scan"

if [ ${#residue[@]} -ne 0 ]; then
    echo "error: the test suite left scratch space behind:" >&2
    printf '  %s\n' "${residue[@]}" >&2
    echo >&2
    echo "A test helper must return an owning guard (tempfile::TempDir), not a" >&2
    echo "bare path, and the guard has to outlive every use of the directory." >&2
    echo "Manual teardown is not cleanup: a failing assertion skips it." >&2
    exit 1
fi

echo "ok: the suite left no scratch residue"
