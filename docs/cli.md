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

The planned public contract provides versioned JSON, structured errors, explicit
provider capabilities, snapshots and watch operations. Unsupported operations fail
explicitly. Claude capabilities require verification before activation.
