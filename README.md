# sessionsation

A Linux CLI for managing coding-agent accounts and named sessions. It keeps account
selection, conversation history and lifecycle operations in one backend; terminal
hosting is handled separately by tmux or [HERDR](https://herdr.dev).

**Early development.** Codex, Claude Code, Gemini CLI and Antigravity CLI (`agy`)
are implemented providers, each with isolated account homes. What each provider can
do is declared per operation by `api capabilities`, which is the source of truth; the
[contract](docs/contract.md#providers) summarises it. There is no cross-provider
conversation conversion. Integrations use a versioned JSON API with stable session
IDs, generation checks, durable receipts and explicit, never-replayed operations.
Availability depends on the installed provider's verified native capabilities;
building successfully is not provider acceptance.

## Install

Requires Linux, Git, a C toolchain and the Rust toolchain pinned by
[rust-toolchain.toml](rust-toolchain.toml); running agents also needs a compatible
provider CLI, gzip, rsync, and tmux or HERDR (see [setup](docs/cli.md)).

```sh
git clone https://github.com/anastaci1a/sessionsation.git
cd sessionsation
cargo build --release --locked
mkdir -p "$HOME/.local/bin"
install -m 755 target/release/sessionsation "$HOME/.local/bin/sessionsation"
```

No released binary package or universal provider-version compatibility is claimed.

## Quick start

These commands inspect an empty, explicit data root without enrolling an account,
starting an agent or calling a provider:

```sh
export SESSIONSATION_HOME="$HOME/.local/share/sessionsation"
sessionsation --help
sessionsation api --api-version 1 capabilities
sessionsation api --api-version 1 state
```

Then follow [account setup and terminal launch](docs/cli.md). Keep one supervisor per
data root. Reusing an existing installation is an explicit choice, not an automatic
import; never rename its live account homes. The separate
[sessionsation-ui](https://github.com/anastaci1a/sessionsation-ui) provides a HERDR
popup; closing a viewer leaves agents running.

## Documentation and development

Start at [documentation](docs/README.md). Contributor workflow, checks and the release
procedure are in [agents/README.md](agents/README.md); `scripts/check.sh` runs the full
suite.

MIT licensed; backend reuse and the retained upstream revision are recorded in
[NOTICE](NOTICE) and [LICENSE](LICENSE).
