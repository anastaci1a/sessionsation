# sessionsation

A Linux CLI for managing coding-agent accounts and named sessions. It keeps account
selection, conversation history and lifecycle operations in one backend; terminal
hosting is handled separately by tmux or [HERDR](https://herdr.dev).

**Early development.** Codex, Claude Code, Gemini CLI and Antigravity CLI (`agy`) are
implemented providers, each with isolated account homes. Codex has the full lifecycle:
usage, stop, restart, idle-bound account switching, running HERDR Space moves,
non-steering Submit/Queue and explicit steering. Claude Code supports subscription
accounts, new/resume, live tracking, statusline usage, account switching, archives,
hosted (stream-json) sessions and live delivery (native cross-session messaging for
agent senders, with terminal-input and headless fallbacks); stop, restart and Space
moves are still unsupported. Gemini CLI and Antigravity CLI support new/resume, live
tracking, terminal-input and headless delivery and archives; their usage is
unsupported by the providers. What each provider can do is declared per operation by
`api capabilities` and is the source of truth. There is no cross-provider conversation
conversion.

Today the CLI provides account enrollment, usage/reset reporting, enable/disable,
local account deletion with preserved conversations, session discovery, new/resume,
rename, idle-bound account switching, running HERDR Space moves, archives and explicit
conversation deletion, per-session Auto Start (resume only, or resume and prompt) and turn-end notifications, shared
Claude conversation state across accounts (`sessionsation shared`), data-root layout
migration and retention, and a disk-space guard.
Integrations use a versioned JSON API with stable session IDs, generation checks,
non-steering Submit/Queue, explicit in-turn steering, durable receipts and live
activity (`live_activity`, `pending_operations`) per session. Availability depends on the installed
provider's verified native capabilities; building successfully is not provider acceptance.

## Install

Requires Linux, Git, a C toolchain and the Rust toolchain pinned by
[rust-toolchain.toml](rust-toolchain.toml). Running agents also needs a compatible
installation of the provider CLI you enroll (Codex, Claude Code, Gemini CLI or
Antigravity CLI), gzip, rsync, and tmux or HERDR. See
[setup and compatibility](docs/cli.md) before enrolling accounts.

```sh
git clone https://github.com/anastaci1a/sessionsation.git
cd sessionsation
cargo build --release --locked
mkdir -p "$HOME/.local/bin"
install -m 755 target/release/sessionsation "$HOME/.local/bin/sessionsation"
export PATH="$HOME/.local/bin:$PATH"
```

No released binary package or universal provider-version compatibility is claimed.
The repository is public; cloning and building do not require GitHub authentication.

## Quick start

These commands inspect an empty, explicit local data root without enrolling an
account, starting an agent or making provider calls:

```sh
export SESSIONSATION_HOME="$HOME/.local/share/sessionsation"
sessionsation --help
sessionsation api --api-version 1 capabilities
sessionsation api --api-version 1 state
```

Then follow [account setup and terminal launch](docs/cli.md). The main commands are
`setup`, `daemon`, `account`, `run`, `resume`, `status`, `doctor`, `shared`,
`layout`, `retention` and the machine interface `api` (`sessionsation --help` lists
them; `sessionsation api --help` lists the API commands). Keep one supervisor
per data root. Reusing an existing installation is an explicit choice, not an
automatic import or migration; never rename its live account homes.

The separate [sessionsation-ui](https://github.com/anastaci1a/sessionsation-ui)
provides a HERDR popup. Closing a viewer leaves agents running; stopping an agent
is a separate backend action.

## Development

`scripts/check.sh` runs the full suite (formatting, Clippy with warnings denied, all
tests, documentation links and the contract schema). For quick iteration use
`scripts/check.sh --quick [NAME]` (formatting, Clippy and unit tests, plus the tests
matching NAME), and `scripts/docker-check.sh [command...]` to run either in the
public check image. Tests run in parallel under `cargo-nextest` when it is installed,
with real-time waits compressed by the test-only `SESSIONSATION_TEST_TIME_SCALE`
(default 10 in the scripts; `1` is real time). `--quick` skips the docs and contract
checks, so for docs-only edits run `python3 scripts/check_docs.py`. Details are in
[contributor/build checks](agents/README.md).

## Documentation

See [documentation](docs/README.md), [public API v1](docs/contract.md),
[verification and limits](docs/operations.md), and
[contributor/build checks](agents/README.md).

MIT licensed; backend reuse and the retained upstream revision are recorded in
[NOTICE](NOTICE) and [LICENSE](LICENSE).

## Release procedure

1. From a clean checkout of the commit being released (never a `git archive` copy), use a **fresh,
   empty target dir** every time: `SESSIONSATION_COMMIT=$(git rev-parse HEAD) CARGO_TARGET_DIR=$(mktemp -d) cargo build --release --locked`.
2. Run `scripts/check.sh` on that commit.
3. Install to `releases/<sha>` and check the embedded commit: `api version` must print `"commit":"<sha>"`.
4. Switch the `current` link, then run `scripts/smoke-installed.sh <installed binary> <sha>`. A failure
   means: switch the link back to the previous release immediately.
`--help` is not a check: it never loads the real state.
