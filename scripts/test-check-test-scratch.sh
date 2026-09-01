#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT

mkdir "$fixture/cargo-bin" "$fixture/scan-failure-bin"
cat >"$fixture/cargo-bin/cargo" <<'EOF'
#!/usr/bin/env bash
if [[ " $* " == *" --no-run "* ]]; then
    exit 0
fi
case "${SCRATCH_FIXTURE:-clean}" in
    clean) ;;
    allowed)
        mkdir -p \
            "$TMPDIR/aj-usage-A1b2C3" \
            "$TMPDIR/aj-task-lifetime-D4e5F6"
        ;;
    lexical)
        mkdir -p \
            "$TMPDIR/aj-usage-collect-leak" \
            "$TMPDIR/aj-usage-test-A-leak"
        ;;
    punctuation)
        mkdir -p "$TMPDIR/aj-usage-A1b2!3"
        ;;
    newline)
        mkdir -p "$TMPDIR/"$'aj-usage-A1b2C3\n'
        ;;
    *)
        echo "error: unknown scratch fixture ${SCRATCH_FIXTURE}" >&2
        exit 2
        ;;
esac
exit 0
EOF
# Emit no entries, so only the scan status can make the guard fail.
ln -s ../cargo-bin/cargo "$fixture/scan-failure-bin/cargo"
cat >"$fixture/scan-failure-bin/find" <<'EOF'
#!/usr/bin/env bash
echo "injected residue scan failure" >&2
exit 71
EOF
chmod +x "$fixture/cargo-bin/cargo" "$fixture/scan-failure-bin/find"

scan_stdout="$fixture/scan-stdout"
scan_stderr="$fixture/scan-stderr"
if PATH="$fixture/scan-failure-bin:$PATH" \
    "$repo/scripts/check-test-scratch.sh" >"$scan_stdout" 2>"$scan_stderr"; then
    echo "error: scratch guard accepted a failed residue scan" >&2
    exit 1
fi

if ! grep -Fqx "injected residue scan failure" "$scan_stderr"; then
    echo "error: scratch guard test did not reach the injected scan failure" >&2
    exit 1
fi
if ! grep -Fqx "error: could not scan test scratch space for residue" "$scan_stderr"; then
    echo "error: scratch guard did not name the residue scan failure" >&2
    exit 1
fi
if grep -Fq "the test suite left scratch space behind" "$scan_stderr"; then
    echo "error: scratch guard misreported a scan failure as residue" >&2
    exit 1
fi
if grep -Fq "ok: the suite left no scratch residue" "$scan_stdout" "$scan_stderr"; then
    echo "error: scratch guard reported success after a scan failure" >&2
    exit 1
fi

allowed_stdout="$fixture/allowed-stdout"
allowed_stderr="$fixture/allowed-stderr"
if ! SCRATCH_FIXTURE=allowed PATH="$fixture/cargo-bin:$PATH" \
    "$repo/scripts/check-test-scratch.sh" >"$allowed_stdout" 2>"$allowed_stderr"; then
    echo "error: scratch guard rejected a process-lifetime root" >&2
    exit 1
fi
if ! grep -Fqx "ok: the suite left no scratch residue" "$allowed_stdout"; then
    echo "error: allowed-root case did not report a clean scratch directory" >&2
    exit 1
fi

reject_residue_case() {
    local case_name=$1
    local stdout="$fixture/$case_name-stdout"
    local stderr="$fixture/$case_name-stderr"
    if SCRATCH_FIXTURE="$case_name" PATH="$fixture/cargo-bin:$PATH" \
        "$repo/scripts/check-test-scratch.sh" >"$stdout" 2>"$stderr"; then
        echo "error: scratch guard accepted $case_name usage residue" >&2
        exit 1
    fi
    if ! grep -Fqx "error: the test suite left scratch space behind:" "$stderr"; then
        echo "error: $case_name usage root was not classified as residue" >&2
        exit 1
    fi
    if grep -Fq "could not scan test scratch space" "$stderr"; then
        echo "error: $case_name usage residue was misreported as a scan failure" >&2
        exit 1
    fi
    if grep -Fq "ok: the suite left no scratch residue" "$stdout" "$stderr"; then
        echo "error: scratch guard reported clean after $case_name usage residue" >&2
        exit 1
    fi
}

reject_residue_case lexical
for name in aj-usage-collect-leak aj-usage-test-A-leak; do
    if ! grep -Fqx "  $name" "$fixture/lexical-stderr"; then
        echo "error: scratch guard did not name lexical residue $name" >&2
        exit 1
    fi
done
reject_residue_case punctuation
if ! grep -Fqx "  aj-usage-A1b2!3" "$fixture/punctuation-stderr"; then
    echo "error: scratch guard did not name same-length punctuation residue" >&2
    exit 1
fi
reject_residue_case newline
if ! grep -Fqx "  aj-usage-A1b2C3" "$fixture/newline-stderr"; then
    echo "error: scratch guard did not preserve trailing-newline residue" >&2
    exit 1
fi

echo "ok: scratch guard rejects residue scan failures"
echo "ok: scratch guard allows only named process-lifetime roots"
