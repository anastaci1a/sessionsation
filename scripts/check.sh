#!/bin/sh
# Full check (default) or a quick iteration pass.
#
#   scripts/check.sh                 fmt, clippy (all targets), every test, docs, contract
#   scripts/check.sh --quick [NAME]  fmt, clippy (lib+bins), lib tests; with NAME(s), also
#                                    only the tests whose names contain NAME (any binary)
#
# Builders iterate with --quick and targeted tests, then run the full check once
# before committing.
#
# Environment:
#   CARGO_TARGET_DIR              separate build tree (absolute path)
#   RUST_TEST_THREADS             parallel tests (default: cores, at most 12)
#   TOKIO_WORKER_THREADS          per-process runtime threads (default 2)
#   SESSIONSATION_TEST_TIME_SCALE test-only time compression (default 10; 1 = real time)
#   SESSIONSATION_NO_NEXTEST=1    use plain `cargo test` even when cargo-nextest exists
set -eu
check_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$check_root"
quick=0
if [ "${1:-}" = "--quick" ]; then
    quick=1
    shift
fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$check_root/target-review}"
cores=$(nproc 2>/dev/null || echo 4)
[ "$cores" -le 12 ] || cores=12
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-$cores}"
export NEXTEST_TEST_THREADS="${NEXTEST_TEST_THREADS:-$RUST_TEST_THREADS}"
export TOKIO_WORKER_THREADS="${TOKIO_WORKER_THREADS:-2}"
export SESSIONSATION_TEST_TIME_SCALE="${SESSIONSATION_TEST_TIME_SCALE:-10}"
nextest=0
if [ "${SESSIONSATION_NO_NEXTEST:-0}" != 1 ] && cargo nextest --version >/dev/null 2>&1; then
    nextest=1
fi

cargo fmt --check
if [ "$quick" = 1 ]; then
    cargo clippy --locked --lib --bins -- -D warnings
    cargo test --locked --lib --bins
    if [ "$#" -gt 0 ]; then
        if [ "$nextest" = 1 ]; then
            cargo nextest run --locked --no-fail-fast --no-tests=pass "$@"
        else
            for name in "$@"; do cargo test --locked "$name"; done
        fi
    fi
    exit 0
fi
cargo clippy --locked --all-targets -- -D warnings
if [ "$nextest" = 1 ]; then
    cargo nextest run --locked --no-fail-fast
    cargo test --locked --doc
else
    cargo test --locked
fi
python3 scripts/check_docs.py
python3 scripts/check_contract.py
