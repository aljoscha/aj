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
set -euo pipefail

# State that legitimately outlives a test lives under one named per-process
# root, which nothing is left to remove: the process is gone. Each entry here
# needs that justification, and the pattern must be specific enough that a new
# leak cannot hide behind it.
#
#   aj-usage-*  the usage overlay spawns its fetch onto a deliberately leaked
#               runtime, so a task holding a clone of its credential store
#               outlives the test that built it and writes afterwards. The
#               stores sit in per-test subdirectories under this one root.
allowed=(
    'aj-usage-[A-Za-z0-9]*'
)

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

cargo test --workspace --no-run --quiet
# Gateway integration tests each own a multi-thread Tokio runtime. Letting
# libtest run several of them together can starve their real loopback requests
# on a small runner. Run every other target normally, then this module alone.
# The guard still covers the whole suite, and the ordinary Test job retains the
# parallel execution coverage.
TMPDIR="$scratch" cargo test --workspace --exclude aj --quiet
TMPDIR="$scratch" cargo test -p aj --quiet -- --skip gateway::tests
TMPDIR="$scratch" cargo test -p aj --quiet gateway::tests -- --test-threads=1

residue=()
while IFS= read -r -d '' entry; do
    name="$(basename "$entry")"
    for pattern in "${allowed[@]}"; do
        # shellcheck disable=SC2053 # the pattern is meant to glob
        if [[ $name == $pattern ]]; then
            continue 2
        fi
    done
    residue+=("$name")
done < <(find "$scratch" -mindepth 1 -maxdepth 1 -print0)

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
