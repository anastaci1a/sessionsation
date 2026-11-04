#!/bin/sh
# Run any command (default: scripts/check.sh) in the public check image.
#   docker build -f scripts/Dockerfile.check -t sessionsation-public-check:local scripts
#   scripts/docker-check.sh [scripts/check.sh --quick]
# /tmp is a tmpfs: the tests create thousands of fsynced files, which on a spinning
# or network-backed disk dominate the run time. Override CHECK_TARGET (build tree)
# and CHECK_REGISTRY (cargo registry cache) to reuse caches between runs.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target="${CHECK_TARGET:-$root/target-docker}"
mkdir -p "$target"
[ "$#" -gt 0 ] || set -- scripts/check.sh
registry=
[ -z "${CHECK_REGISTRY:-}" ] || registry="-v $CHECK_REGISTRY:/usr/local/cargo/registry"
# shellcheck disable=SC2086
exec docker run --rm -v "$root:/work" -w /work -e CARGO_TARGET_DIR=/target \
    -e RUST_TEST_THREADS -e TOKIO_WORKER_THREADS -e SESSIONSATION_TEST_TIME_SCALE \
    -e SESSIONSATION_NO_NEXTEST -e NEXTEST_TEST_THREADS \
    -v "$target:/target" $registry --tmpfs /tmp:rw,exec,size="${CHECK_TMPFS:-6g}" \
    "${CHECK_IMAGE:-sessionsation-public-check:local}" "$@"
