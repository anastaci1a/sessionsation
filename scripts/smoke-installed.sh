#!/bin/sh
# Post-install smoke test of the INSTALLED binary (never --help alone):
#   scripts/smoke-installed.sh BIN [EXPECTED_COMMIT]
# Runs version, capabilities and a cached state against the real data root, and refuses
# an artifact whose embedded commit differs from EXPECTED_COMMIT. Exit non-zero means:
# switch the `current` link back to the previous release immediately.
set -eu
bin=${1:?usage: smoke-installed.sh BIN [EXPECTED_COMMIT]}
expected=${2:-}
run() { "$bin" api --api-version 1 "$@"; }
out=$(run version) || { echo "SMOKE FAIL: api version" >&2; exit 1; }
echo "$out"
if [ -n "$expected" ]; then
    case "$out" in
        *"\"commit\":\"$expected\""*) ;;
        *) echo "SMOKE FAIL: embedded commit differs from $expected (stale build)" >&2; exit 1 ;;
    esac
fi
run capabilities >/dev/null || { echo "SMOKE FAIL: api capabilities" >&2; exit 1; }
run state --cached >/dev/null || { echo "SMOKE FAIL: api state --cached" >&2; exit 1; }
echo "SMOKE OK: $bin"
