# Contributing to sessionsation

## Workflow and boundaries

Read the root AGENTS.md and this file before editing. Verify the existing working
tree and remote; preserve other work. Use the configured Git identity. Keep commits
small and coherent, run appropriate checks, and push completed changes to the
existing repository. Issues own tasks/acceptance; Git and releases own history.

Preserve original installations and live agents. Deployments, live migration,
credential/access changes and spending require the operator's authorization.
Tests use disposable homes and fake providers. Never use the invoking agent's
credentials or commit private data, protocol traces or configuration backups.
Keep one supervisor/credential writer per home, tmux support, operation locks,
generation checks and uncertain-delivery safeguards. Timeouts are not quiescence;
never automatically replay an uncertain send or silently migrate a live session.

## Reproducible checks

Install the pinned Rust toolchain with rustup. Debian/Ubuntu dependencies:

```sh
sudo apt-get install -y build-essential git curl ca-certificates tmux rsync python3 python3-jsonschema
rustup show
scripts/check.sh
```

The script runs formatting, Clippy with warnings denied, locked Rust tests, local
Markdown links and schema/CLI-envelope checks. It resolves the checkout itself,
defaults to `target-review/`, and limits independent test cases to two. Override
`CARGO_TARGET_DIR` with an absolute path or `RUST_TEST_THREADS` explicitly.
The initial run downloads locked public Cargo dependencies. For later offline
runs use `CARGO_NET_OFFLINE=true scripts/check.sh` after `cargo fetch --locked`.
Do not overwrite an executable selected by another consumer.

Alternatively, with Docker and no local Rust/Python installation:

```sh
docker build -f scripts/Dockerfile.check -t sessionsation-check .
docker run --rm -v "$PWD:/work" -w /work sessionsation-check scripts/check.sh
```

This builds from a public Rust image and installs declared Debian dependencies.
It needs network access for image/packages/crates on the first run, not a host
Cargo cache or a custom image from another project. No account homes, provider
binary, live sockets, SSH agent or credentials are mounted. Docker creates build
files as its container user; use a dedicated checkout or adjust ownership afterward.
The default suite skips three opt-in installed-binary fixtures; optional HERDR
checks and their limits are in [operations](../docs/operations.md).

Rust changes need meaningful safety-boundary regressions and the full script.
For docs-only edits, run `python3 scripts/check_docs.py`; contract/schema edits
also need `CARGO_TARGET_DIR="$PWD/target-review" python3 scripts/check_contract.py`
after building the debug CLI in that target directory.
Setup/check-script changes need clean-environment execution. Test examples against
actual help/envelopes. Keep fixtures synthetic; do not copy an operator's root.

## Documentation and release preparation

README owns purpose, requirements and short setup. docs/ owns behavior, architecture,
CLI/contract and operations, indexed by docs/README.md. Contributor workflow belongs
here. Update the authoritative section, remove superseded text and duplicate facts,
and split files only for distinct reader needs. No session logs or deployment
inventories in public documentation. Keep machine paths, receipt databases and
rollback copies outside Git.

Before public release, review tracked files AND reachable Git history. A bounded
credential-pattern scan in this pass found no credential-like matches, but that
is not a security certification. History still contains private deployment paths,
endpoint/account/session metadata and old operational records. Removing current
files does not remove their earlier blobs. No history rewrite or visibility change
is authorized by routine cleanup; the owner must decide separately.

The inherited `src/messaging/modes.rs` origin constant and matching legacy fixtures
still encode an operator deployment endpoint. Changing that compatibility contract
would affect existing consumers, so it remains an explicit public-readiness blocker,
not a documentation substitution. New consumers use public API v1. Retain the
MIT [upstream attribution](../NOTICE); review all new dependencies/assets and do not
infer permission for unlicensed source.

Check scripts also default TOKIO_WORKER_THREADS to 2 for owned subprocesses,
preventing large host CPU counts from creating oversized test runtimes. Override
it explicitly when investigating concurrency; production binaries are unchanged.
