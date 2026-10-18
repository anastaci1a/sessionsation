# Setup and CLI

The imported Codex CLI is available through `cargo build --locked` and
`target/debug/sessionsation --help`. Do not install over Codex Switcher.

```sh
target/debug/sessionsation --data-dir /tmp/sessionsation-example status --json
target/debug/sessionsation --data-dir /tmp/sessionsation-example doctor --json
```

State defaults to `SESSIONSATION_HOME`, then `/mnt/user/appdata/sessionsation` on
Unraid or `~/.local/share/sessionsation` elsewhere. `--data-dir` takes precedence.
The imported CLI is provisional; the public sessionsation contract is not stable.

## Terminal hosting

Keep the foreground terminal with `sessionsation run --account NAME` (including
inside tmux). Select HERDR with an explicit named server:

```sh
herdr --session coding server
```

Keep that headless server running. From another terminal:

```sh
sessionsation run --account personal --host herdr --herdr-session coding
herdr session attach coding
```

The JSON launch receipt reports `running` only after matching the account home,
exact conversation and live process. `launch_unconfirmed` means inspect the named
server before retrying; the command may already have started. A pane-admission
acknowledgement alone does not establish that Codex started.

Use `instances` for discovered targets. HERDR targets include the server name and
opaque pane ID. Existing tmux targets continue to work. No live session is migrated.

```sh
sessionsation move --pane herdr/coding/w1:p1 --to other-account
sessionsation restart --pane herdr/coding/w1:p1
sessionsation terminate --pane herdr/coding/w1:p1
sessionsation run --account personal --thread EXACT-UUID --host herdr --herdr-session coding
sessionsation moves
```

Copy targets from discovery; the examples' pane IDs are placeholders. Lifecycle
commands return journaled jobs. Inspect `moves`, answer any explicit risk/background
confirmation with `move-response`, or use `recover` after a failed operation.
HERDR moves wait for positive idle evidence without steering. Restart replaces the
terminal client while retaining the account server and recorded permissions.

Detaching the HERDR viewer (default **Ctrl+B, Q**) leaves agents running. `terminate`
stops the selected agent and preserves resumable history. `herdr server stop` stops
the named host and its panes; it is not a viewer-detach command.

The planned public contract provides versioned JSON, structured errors, explicit
provider capabilities, snapshots and watch operations. Unsupported operations fail
explicitly. Claude capabilities require verification before activation.
