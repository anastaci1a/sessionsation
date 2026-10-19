#!/bin/sh
set -eu
check_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$check_root"
# Override with an absolute path when checks must use a separate build tree.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$check_root/target-review}"
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-2}"
export TOKIO_WORKER_THREADS="${TOKIO_WORKER_THREADS:-2}"
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 scripts/check_docs.py
python3 scripts/check_contract.py
