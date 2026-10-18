# sessionsation

A CLI-first account and session manager for Codex and Claude, with provider behavior
separate from terminal hosting. The backend reuses
[Codex Switcher](https://github.com/anastaci1a/codex-switcher).

**Status:** the Codex backend and its CLI have been imported without the UI.
Account enrollment/monitoring, exact-conversation launch, tmux discovery and manual
history-preserving moves are inherited. HERDR now hosts the same account-specific
Codex launcher, including discovery, restart and recovery. A versioned JSON API
provides stable session IDs, unique names, usage, non-steering messaging and safe
local account removal. The original installation remains independent.

Use Linux (including Unraid), Codex CLI, Rust 1.95, and tmux or HERDR.
Claude runtime is not configured; its capabilities explicitly report setup required.
The separate [sessionsation-ui](https://github.com/anastaci1a/sessionsation-ui)
provides a thin HERDR management viewer and future-terminal launcher.

Build with `cargo build --locked`, then `export PATH="$PWD/target/debug:$PATH"`.
Inspect `sessionsation --help`.
Use `--data-dir /absolute/disposable/path` for experiments; the default on Unraid
is `/mnt/user/appdata/sessionsation`. Do not point an experimental supervisor at
live account data. Run the isolated checks with `scripts/check.sh` on this server.

```sh
sessionsation account list
sessionsation run --account personal
sessionsation run --account personal --host herdr --herdr-session coding
sessionsation api state
```

The HERDR command requires an existing named server. See [setup and CLI](docs/cli.md).
Integrations use [CLI contract v1](docs/contract.md); see
[verification evidence and limitations](docs/operations.md).

See [documentation](docs/README.md) and [contributor workflow](agents/README.md).
