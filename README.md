# sessionsation

A Linux CLI for managing coding-agent accounts and named sessions. It keeps account
selection, conversation history and lifecycle operations in one backend; terminal
hosting is handled separately by tmux or [HERDR](https://herdr.dev).

**Early development.** Codex is implemented. Claude is a capability placeholder,
not a working provider. There is no cross-provider conversation conversion.

Today the CLI provides account enrollment, usage/reset reporting, enable/disable,
local account deletion with preserved conversations, session discovery, new/resume,
rename, idle-bound account switching, running HERDR Space moves, archives and explicit conversation deletion.
Integrations use a versioned JSON API with stable session IDs, generation checks,
non-steering Submit/Queue and durable receipts. Availability depends on the installed
provider's verified native capabilities; building successfully is not provider acceptance.

## Install

Requires Linux, Git, a C toolchain and the Rust toolchain pinned by
[rust-toolchain.toml](rust-toolchain.toml). Running agents also needs a compatible
Codex installation, gzip, rsync, and tmux or HERDR. See
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
The repository must be accessible to your Git identity while it remains private.

## Quick start

These commands inspect an empty, explicit local data root without enrolling an
account, starting an agent or making provider calls:

```sh
export SESSIONSATION_HOME="$HOME/.local/share/sessionsation"
sessionsation --help
sessionsation api --api-version 1 capabilities
sessionsation api --api-version 1 state
```

Then follow [account setup and terminal launch](docs/cli.md). Keep one supervisor
per data root. Reusing an existing installation is an explicit choice, not an
automatic import or migration; never rename its live account homes.

The separate [sessionsation-ui](https://github.com/anastaci1a/sessionsation-ui)
provides a HERDR popup. Closing a viewer leaves agents running; stopping an agent
is a separate backend action.

See [documentation](docs/README.md), [public API v1](docs/contract.md),
[verification and limits](docs/operations.md), and
[contributor/build checks](agents/README.md).

MIT licensed; backend reuse and the retained upstream revision are recorded in
[NOTICE](NOTICE) and [LICENSE](LICENSE).
