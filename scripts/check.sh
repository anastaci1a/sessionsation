#!/bin/sh
set -eu
# Build once: docker build -f scripts/Dockerfile.check -t sessionsation-build-tools:local .
# Offline toolchain with no account homes, runtime sockets or live sessions mounted.
# The cache contains downloaded Rust packages only; build outputs stay in target/.
docker run --rm --network none \
  -v "$PWD:/work" \
  -v /mnt/user/appdata/codex-switcher-build/cargo/registry:/usr/local/cargo/registry:ro \
  -w /work sessionsation-build-tools:local \
  sh -c 'cargo fmt --check && cargo clippy --offline --locked --all-targets -- -D warnings && cargo test --offline --locked && python3 scripts/check_docs.py && python3 scripts/check_contract.py'
