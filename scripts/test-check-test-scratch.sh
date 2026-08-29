#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT

mkdir "$fixture/bin"
cat >"$fixture/bin/cargo" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
# Emit no entries, so only the scan status can make the guard fail.
cat >"$fixture/bin/find" <<'EOF'
#!/usr/bin/env bash
echo "injected residue scan failure" >&2
exit 71
EOF
chmod +x "$fixture/bin/cargo" "$fixture/bin/find"

stdout="$fixture/stdout"
stderr="$fixture/stderr"
if PATH="$fixture/bin:$PATH" "$repo/scripts/check-test-scratch.sh" >"$stdout" 2>"$stderr"; then
    echo "error: scratch guard accepted a failed residue scan" >&2
    exit 1
fi

if ! grep -Fqx "injected residue scan failure" "$stderr"; then
    echo "error: scratch guard test did not reach the injected scan failure" >&2
    exit 1
fi
if ! grep -Fqx "error: could not scan test scratch space for residue" "$stderr"; then
    echo "error: scratch guard did not name the residue scan failure" >&2
    exit 1
fi
if grep -Fq "the test suite left scratch space behind" "$stderr"; then
    echo "error: scratch guard misreported a scan failure as residue" >&2
    exit 1
fi
if grep -Fq "ok: the suite left no scratch residue" "$stdout" "$stderr"; then
    echo "error: scratch guard reported success after a scan failure" >&2
    exit 1
fi

echo "ok: scratch guard rejects residue scan failures"
