# sessionsation

A CLI-first account and session manager for Codex and Claude, with provider behavior
separate from terminal hosting. The backend reuses
[Codex Switcher](https://github.com/anastaci1a/codex-switcher).

**Status:** the Codex backend and its CLI have been imported without the UI.
Account enrollment/monitoring, exact-conversation launch, tmux discovery and manual
history-preserving moves are inherited. HERDR now hosts the same account-specific
Codex launcher, including discovery, restart and recovery. The unified public
contract is under development. The original installation remains independent.

The initial target is Linux (including Unraid), Codex CLI, Rust, and tmux or HERDR.
Claude and a separate `sessionsation-ui` consumer follow verified Codex integration.
These are plans, not current capabilities.

Build with `cargo build --locked`. Inspect `target/debug/sessionsation --help`.
Use `--data-dir /absolute/disposable/path` for experiments; the default on Unraid
is `/mnt/user/appdata/sessionsation`. Do not point an experimental supervisor at
live account data. Run the isolated checks with `scripts/check.sh` on this server.

```sh
sessionsation account list
sessionsation run --account personal
sessionsation run --account personal --host herdr --herdr-session coding
```

The HERDR command requires an existing named server. See [setup and CLI](docs/cli.md).

See [documentation](docs/README.md) and [contributor workflow](agents/README.md).
