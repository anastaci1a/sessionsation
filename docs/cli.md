# Setup and CLI

## Build and state

Use Linux/Unraid, Rust 1.95, a C compiler, Codex CLI and tmux or HERDR.
The tested build on this server is described in [operations](operations.md).

```sh
cargo build --locked
export PATH="$PWD/target/debug:$PATH"
sessionsation --help
sessionsation api capabilities
```

State defaults to `SESSIONSATION_HOME`, then `/mnt/user/appdata/sessionsation` on
Unraid or `~/.local/share/sessionsation` elsewhere. `--data-dir /absolute/path`
takes precedence. Keep one data root for all clients. The stable session catalog
lives under `session-catalog/`; retain it to retain backend IDs and names.

The original switcher installation is independent. To use its already-enrolled
accounts, explicitly select its data root; sessionsation talks to the existing
supervisor and account servers and retains their ownership locks. Do not stop or
replace that owner or copy its credentials. New development/tests use disposable
roots. Installing or migrating a live service is a separate owner decision.

## Accounts

For a fresh private data root, `sessionsation setup` initializes its metadata
supervisor. Enrollment is interactive through a device/browser login:

```sh
sessionsation api account add codex:personal
sessionsation api account list
sessionsation api account login codex:personal
sessionsation api account disable codex:personal
sessionsation api account enable codex:personal
```

Add/login return the provider's verification URL and user code. Complete the login
as the user; never copy credentials between homes. Disabled accounts remain listed;
disabling prevents future selection and does not stop existing agents. Usage and
reported reset timestamps are in account snapshots; do not invent missing windows.
Historical usage uses `api account usage codex:personal --start UNIX --end UNIX
--step 1800`.

Removal previews affected catalog sessions, active/loaded conversations, pending
operations and stored-data counts/paths. It requires exact ID and fresh revision:

```sh
sessionsation api account remove codex:personal
sessionsation api account remove codex:personal --confirm codex:personal --revision PREVIEW-REVISION
sessionsation api account restore codex:personal
```

Removal refuses active/loaded conversations or pending lifecycle jobs. Stop or
move them explicitly first. It unregisters the account from sessionsation selection
without disabling the original provider enrollment, stopping agents or deleting
credentials/history. The original switcher remains independent. Restore reverses
the membership change. Permanent credential/local-data purging is unsupported;
it must never be inferred from removal. No upstream provider account is deleted.

## Terminal hosting

Keep the current terminal with `sessionsation run --account personal`, including
inside tmux or HERDR. HERDR's session/pane context is registered automatically.
Outside a managed host, foreground launch still works, but v1 managed lifecycle
requires a discovered tmux/HERDR attachment.

For headless hosting, keep this command running in its own terminal/service:

```sh
herdr --session coding server
```

Then launch and optionally attach a viewer:

```sh
sessionsation run --account personal --host herdr --herdr-session coding
herdr session attach coding
```

The launch JSON reports `running` only after matching the exact account home,
conversation and process. `launch_unconfirmed` means inspect before retrying; the
command may already have started. Admission alone is not successful startup.
No existing live session is migrated.

Detach a HERDR viewer with default **Ctrl+B, Q**. This leaves agents running.
Backend `api stop` ends the selected agent and preserves resumable history.
`herdr server stop` stops the named host and panes; it is not viewer detachment.
Restart replaces the terminal client on its selected account, retaining recorded
permissions and the existing account server. Clear unsent drafts before lifecycle
operations. Switching waits for positive idle and never forces interruption.

## Public machine interface

Use [contract v1](contract.md) for state/watch, unique names, generation-checked
session operations, receipts and launcher plans. `api --api-version 1` accepts only
supported contract versions. All API output is JSON; watch uses NDJSON.

```sh
sessionsation api state
sessionsation api state --watch --interval 2
sessionsation api launcher new --account codex:personal
sessionsation api operations
```

Use backend session IDs and generations from state, not example pane IDs. Operation
responses require a fresh confirmation sequence. Use the public operation commands:

```sh
sessionsation api respond OPERATION-ID yes --sequence CURRENT-SEQUENCE
sessionsation api cancel-operation OPERATION-ID
```

No automatic switching, forced-steer messaging, cross-provider conversion or Claude
runtime is enabled. Claude is prepared as a separate capability boundary pending
installation/account verification. See [architecture](architecture.md).
