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

`scripts/check.sh` runs formatting, Clippy with warnings denied, locked Rust tests,
local Markdown links (`scripts/check_docs.py`) and schema/CLI-envelope checks
(`scripts/check_contract.py`). It resolves the checkout itself, defaults to
`target-review/`, and runs tests in parallel (one per core, at most 12; each test uses
its own temporary root, sockets and fake peers), under `cargo-nextest` when installed
(`SESSIONSATION_NO_NEXTEST=1` for plain `cargo test`). Override `CARGO_TARGET_DIR`
with an absolute path or `RUST_TEST_THREADS` explicitly. Check scripts default
`TOKIO_WORKER_THREADS` to 2 for owned subprocesses; production binaries are unchanged.

**Iterate fast, check once.** Iterate with `scripts/check.sh --quick [NAME]` (fmt,
Clippy on lib and bins, lib and bin unit tests, plus tests matching NAME) or targeted
tests such as `cargo test --test claude_terminal terminal_input`, then run the full
`scripts/check.sh` once before committing. `--quick` skips the docs and contract
checks: after docs edits run `python3 scripts/check_docs.py`, after contract/schema
edits `CARGO_TARGET_DIR="$PWD/target-review" python3 scripts/check_contract.py`
(after building the debug CLI there), and for doc size changes `cargo test --lib
guard`.

The suite compresses real-time waits with `SESSIONSATION_TEST_TIME_SCALE` (default 10
in `scripts/check.sh` and the nextest profile; `1` is real time). It is **test-only**:
unset in production, every wait keeps its documented value. It divides supervisor
tick intervals, native-send windows and terminal settle delays (`src/timing.rs`;
give-up budgets are never compressed below one second). Route a new production wait
that tests would otherwise sit through via `timing::scaled`, `timing::secs` or
`timing::budget`, and use condition polls rather than fixed sleeps in tests. On a
slow disk point `TMPDIR` at a tmpfs mounted with exec. For offline runs use
`CARGO_NET_OFFLINE=true scripts/check.sh` after `cargo fetch --locked`. Do not
overwrite an executable selected by another consumer.

With Docker and no local Rust/Python:

```sh
docker build -f scripts/Dockerfile.check -t sessionsation-public-check:local scripts
scripts/docker-check.sh                          # full check in the image
scripts/docker-check.sh scripts/check.sh --quick # fast pass
```

`scripts/docker-check.sh [command...]` uses a persistent build tree (`CHECK_TARGET`),
an optional registry cache (`CHECK_REGISTRY`) and `/tmp` on tmpfs; the image carries
a pinned `cargo-nextest`. It builds from a public Rust image and needs network access
only on the first run. No account homes, provider binary, live sockets, SSH agent or
credentials are mounted. Docker creates build files as its container user. The
default suite skips opt-in installed-binary fixtures; see
[operations](../docs/operations.md#optional-isolated-herdr-verification).

Rust changes need meaningful safety-boundary regressions and the full script. A
fixture starting another lifecycle action must wait for the previous worker's lock
release after a terminal phase, including cancellation; a journaled phase alone does
not prove that the worker has exited. Setup/check-script changes need
clean-environment execution. Test examples against actual help/envelopes. Keep
fixtures synthetic; do not copy an operator's root.

## Owned roles and the guard

Internal systems (locks, atomic writes, operation records, spawning, HERDR calls,
provider dispatch, the Claude OAuth client and others) each have one owner module,
listed in `src/guard/roles.rs`. The test-only guard (`src/guard.rs`, part of the lib
unit tests and so of every check) counts each role's sites outside its owner and
compares them with `guard/baseline.json`. Any change of a counter, up or down, needs
a ledger entry `{counter, by, reason, considered, step}`; the guard requires
`value == initial + Σ by`. `initial` is pinned in `src/guard/counters.rs`, and that
file and the roles table are pinned by `counters_digest` and `roles_digest`. A rise
needs a real `considered`, except for the size counters (`src_lines`, `api_rs_lines`,
`doc_bytes:*`, `doc_bytes_total`). The ledger also covers `doc_bytes:<path>` for
every Markdown file in the repository (outside `.git`, `target*` and vendored
registries; Git-tracked files only when Git is available, otherwise everything
except root-level and dot-directory scratch), their total, and the count of long
paragraphs repeated across files, so a new doc file needs an entry too. An
`Enforced` role rejects a new site outside its owner unless it carries
`// guard-allow(<role>): <why>` on the same line or the line above. The `oauth` role
matches Claude-OAuth specifics only (`.credentials.json`, `claudeAiOauth`, the
Anthropic API host, `/api/oauth`), so generic token code for other providers does
not trip it. Method-call roles become clippy `disallowed-methods` in `clippy.toml`
when they flip to `Enforced`; a justified site then carries
`#[allow(clippy::disallowed_methods, reason = "<role>: <why>")]`. Run
`cargo test --lib guard::measure -- --ignored --nocapture` to print current values
(`GUARD_SITES=<counter>` lists that counter's sites) and ready-to-paste baseline
values and ledger entries for any counter that drifted.

Known limits, to settle in the step that first flips a clippy role: an item-level
`#[allow(clippy::disallowed_methods)]` is not tied to the regex counter sites it
covers, so an Enforced clippy role still needs its counters' sites marked or at 0;
`build.rs` spawns `git` and needs a reasoned allow before `Command::new` enters
`clippy.toml`. Regex roles see one line at a time: aliased imports
(`use ProviderId::*`) or values bound earlier (`let c = ProviderId::Codex; p == c`)
are not matched, and counters count lines, not matches.

## Documentation

One owner per topic; never repeat a paragraph in another file (the guard counts
duplicates):

| Topic | Owner |
| --- | --- |
| Purpose, requirements, install, quick start | README.md |
| Command usage, flags, short examples | docs/cli.md |
| Behaviour, fields, error codes, capabilities (current only) | docs/contract.md |
| What changed per contract version | docs/changelog.md |
| Internal structure | docs/architecture.md |
| Verification scope, operator procedures | docs/operations.md |
| Contributor workflow, checks, guard, release | agents/README.md |

Update the owning section and link to it from elsewhere; remove superseded text
instead of adding "since" narratives (the changelog carries history). Split files only
for distinct reader needs. No session logs or deployment inventories in public
documentation. Keep machine paths, receipt databases and rollback copies outside Git.

## Release procedure

1. From a clean checkout of the commit being released (never a `git archive` copy),
   use a **fresh, empty target dir** every time:
   `SESSIONSATION_COMMIT=$(git rev-parse HEAD) CARGO_TARGET_DIR=$(mktemp -d) cargo build --release --locked`.
2. Run `scripts/check.sh` on that commit.
3. Install to `releases/<sha>` and check the embedded commit: `api version` must print
   `"commit":"<sha>"`.
4. Switch the `current` link, then run `scripts/smoke-installed.sh <installed binary>
   <sha>`. A failure means: switch the link back to the previous release immediately.
   `--help` is not a check: it never loads the real state.

This repository is public. Before each release, review tracked files and reachable
Git history. History still contains private deployment paths, endpoint/account/
session metadata and old operational records; removing current files does not
remove their earlier blobs. No history rewrite or visibility change is authorized by
routine cleanup; the owner decides separately. The inherited
`src/messaging/modes.rs` origin constant and matching legacy fixtures still encode an
operator deployment endpoint; changing that compatibility contract would affect
existing consumers, so it remains a known exposure. New consumers use public API v1.
Retain the MIT [upstream attribution](../NOTICE); review all new dependencies/assets
and do not infer permission for unlicensed source.
