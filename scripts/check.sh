#!/bin/sh
set -eu
# Build once: docker build -f scripts/Dockerfile.check -t sessionsation-build-tools:local .
# Offline toolchain with no account homes, runtime sockets or live sessions mounted.
# The cache contains downloaded Rust packages only; review builds stay in target-review/ to preserve any launcher-pinned artifact.
docker run --rm --network none \
  -v "$PWD:/work" \
  -v /mnt/user/appdata/codex-switcher-build/cargo/registry:/usr/local/cargo/registry:ro \
  -e RUST_TEST_THREADS="${SESSIONSATION_TEST_THREADS:-2}" \
  -e CARGO_TARGET_DIR="/work/${SESSIONSATION_CHECK_TARGET:-target-review}" \
  -w /work sessionsation-build-tools:local \
  sh -c 'cargo fmt --check && cargo clippy --offline --locked --all-targets -- -D warnings && cargo test --offline --locked && python3 scripts/check_docs.py && python3 scripts/check_contract.py'
