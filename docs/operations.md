# Verification, compatibility and operations

## What is verified

The offline suite uses disposable homes, fake provider servers and owned tmux
sockets. It covers stable IDs, stale targets, locks, concurrent operations, partial
failure/recovery, uncertain delivery, account deletion preserving saved history,
and archive/restore/delete. Error-state regressions also exercise public switch/restart/stop with bounded
turn summaries, preserving identity even when full-history reads are rejected.
The optional HERDR retained-writer fixture verifies explicit recovery onto the
same or another account using synthetic provider servers and inert terminals.
These tests do not establish support for an arbitrary
installed provider binary. Run the commands in [contributor guidance](../agents/README.md).

The public contract is [v1](contract.md), with its [schema](contract/v1.json) and
[synthetic fixtures](contract/fixtures.json). Old `docs/messaging/` fixtures belong
to inherited compatibility commands; new integrations use `api --api-version 1`.
The inherited UI is excluded. Account enrollment, credential ownership and lifecycle
remain in this backend, not a terminal plugin or consumer adapter.

The [portable verification record](verification.json) owns exact passing counts,
commands, artifact hashes and the initial cold-run timing limitation. A successful
source/fixture check is distinct from provider compatibility acceptance.

## Version scope

| Component | Evidence | Limit |
| --- | --- | --- |
| Linux x86-64, Rust 1.95.0 | Locked source build and fixture checks | No Windows/macOS backend support claimed |
| Codex terminal 0.162.1 / account app-server 0.163.0 | Earlier authorized disposable native acceptance | Not a promise for all stock builds, newer versions or other account setups |
| HERDR 0.9.5, terminal protocol 22 | Isolated headless host and PTY tests | Other versions must be checked explicitly |
| tmux | Isolated discovery, exact process ownership and lifecycle regressions | Existing unrelated live sessions are never silently migrated |
| Claude Code 2.1.312 | Fake-provider fixtures for enrollment, auth gating, launch/resume, live identity and statusline usage | No stop/restart/switch/messaging; usage only while Claude Code runs |

Earlier native acceptance established account-specific launch, reported usage,
correlated replies, account transfer with history comparison, restart/resume,
rollback and viewer detach. A later empty disposable conversation exercised fresh
switching, a changed directory/Space, startup waiting and archive/delete without
model turns. The isolated post-account-deletion HERDR fixture preserves 70 synthetic
turns and 270 items on a destination account. These are distinct evidence scopes;
synthetic history or empty native history is not a substantive model recall test.
No such native tests are repeated automatically by the check scripts.

## Optional isolated HERDR verification

Install the release from [HERDR's upstream releases](https://github.com/herdrdev/herdr/releases/tag/v0.9.5),
verify its checksum, and ensure `herdr` is on PATH. Run only with a disposable
HOME/configuration; the ignored fixtures create and stop their own named host:

```sh
SESSIONSATION_ISOLATED_HOST_TEST=1 cargo test --locked --test external_move installed_herdr_ -- --ignored --nocapture
```

The fixtures verify account/home/UUID/socket/permissions, origin shell retention,
foreign observer environments, startup waiting and saved-history resume after local
account deletion. The Space-move fixture also verifies a busy pane move, stable
provider-runtime generation, unaffected split siblings, lost-reply reconciliation
without replay, unchanged launch receipts, and subsequent account handoff in the
moved terminal. Explicit message recovery tests stopped, interrupted and errored
agents in both hosts, preserving history and identity and preventing replay.
These use fake providers, no credentials and no model calls. A separate ignored fixture requires a separately supplied original Switcher executable;
it is not part of portable default checks. Never mount live homes or sockets into
a fixture container. [Machine-readable scope](verification.json) records current
portable verification; timing and host-specific operational receipts belong outside Git.

The same record separately identifies opt-in real-provider acceptance: disposable
tmux and headless HERDR conversations, `/exit`, explicit exited-process Resume,
saved-context recall and repeated original message IDs. That run used an existing
non-invoking account's credential owner through a conversation-scoped proxy and
an isolated metadata-supervisor harness. It validates actual provider replies,
not a production owner upgrade. Interrupted/error and lost-ack cases remain
synthetic coverage. Never treat portable checks as authorization for model usage
or mount an operator's credentials into their container.

## Safe service maintenance

Back up the entire private data root consistently, including databases, catalogs,
request/receipt journals, account homes and archives. Protect backups like credentials.
Keep project files separate. An archive is recoverable history, not a backup of

At startup the daemon removes leftover `.tmp` files (exactly `.tmp` plus six
characters, regular files, owned by the daemon user, unmodified for an hour) from
its own state directories; account homes and archives are never swept. Deleted
sessions leave only ids in their manifest.
credentials or a running process; external backups are outside session deletion.

Use one supervisor per root. Updating a client executable does not update the owner.
Capability checks gate operations requiring a newer owner. A viewer detach does not
stop agents; a host-server stop can terminate all its panes. Migrations, owner
handoffs and provider upgrades require their own reviewed maintenance procedure.
Do not rename a live root, overwrite a selected executable, remove locks, rewind
generations, replay uncertain requests, or infer safety from a timeout.

All readers sharing a root must understand its terminal namespaces and lifecycle
reservations, including `space-moves/` and `message-recoveries/`, before enabling their actions. Coordinate
compatible owner/client pins first: older consumers do not understand this
reservation. An older
reader can invalidate a custom HERDR binding even when a newer client handles its
socket correctly. Review and update consumer pins together; a new client cannot
repair another process's old discovery behavior. Preserve old receipt domains and
modes. Legacy busy Submit/Steer and messaging Resume are not automatically mapped
to non-steering v1 Submit/Queue.

## Data-root layout (contract 1.29.0)

Layout version 2 groups the data root by purpose; `layout.json` records the
version, the migration state and every deferred account home:

```text
DATA_ROOT/
  layout.json  config.json  daemon.lock
  registry/    registry.db, account-catalog/, session-catalog/, conversations/,
               thread-locations/, managed-panes/, herdr-panes/, provider-state/,
               session-access.json
  sessions/    archived/ (was archived-sessions/), preserved/ (was
               preserved-conversations/), hosted/ (was claude-hosted/)
  operations/  launches/, resumptions/, deletions/, account-deletions/,
               conversation-operations/, handoff-jobs/, space-moves/,
               lifecycle-admission/, message-recoveries/, move-backups/
  messaging/   receipts.db, checkpoints/, observations/, claude-steer/,
               claude-deliveries/, questions/
  cache/       state-snapshot-*.json, observations/, compatibility-report.json
  logs/        daemon.log, handoff.log
  accounts/<platform>/<name>   codex, claude, gemini, antigravity homes
```

A new root is created as version 2. A legacy (version 1) root is refused by
every command with `LAYOUT_MIGRATION_REQUIRED`, except `api version` and
`layout`; an interrupted migration or rollback is refused with
`LAYOUT_MIGRATION_IN_PROGRESS` until `layout migrate` (or `layout rollback`)
is run again.

### Migrate

```sh
sessionsation daemon stop
sessionsation layout migrate --dry-run            # the plan, nothing changes
sessionsation layout migrate --backup /path/new-empty-dir
sessionsation daemon start
sessionsation layout status                       # deferred homes and why
```

- It holds `daemon.lock` (so no supervisor of any build runs) and
  `layout.lock`, and refuses a destination that already exists
  (`LAYOUT_CONFLICT`) before changing anything.
- The backup holds every top-level entry except account homes, with
  `registry.db` and `messaging/receipts.db` copied consistently
  (`VACUUM INTO`) and `layout-backup.json` listing the homes (path, inode).
  Homes are only renamed, never copied, so credentials stay in the root.
- The registry WAL is checkpointed, then `registry.db` moves with its
  companions. A non-database file is left at `registry.db` so an older build
  pointed at the root fails ("file is not a database") instead of creating an
  empty registry.
- `archived-sessions`, `preserved-conversations` and `move-backups` leave
  relative compatibility symlinks: archive manifests, account-deletion records
  and move backups store absolute paths, which keep resolving. Journals are
  never rewritten.
- An account home that any process uses (an environment value such as
  `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `GEMINI_CLI_HOME` or `HOME`, a
  command-line argument such as a Codex `--remote unix://.../app-server-control.sock`,
  a working directory, an executable or an open file under the home) is not
  moved. It is *deferred*: it stays at its legacy path, the reason (pids and
  how they use it) is recorded in `layout.json`, and the account keeps
  working there (`api state` reports `home` and `home_deferred: true`).
  Running Codex app-servers, Claude, Gemini and Antigravity processes are
  therefore never disturbed.
- An idle home moves to `accounts/<platform>/<name>` and leaves a relative
  symlink at its legacy path, so absolute paths inside provider state (Codex
  `state_5.sqlite` rollout paths, control socket paths) keep resolving. The
  tests bind a Codex control socket, move the home and connect through the old
  path, and read a recorded SQLite path through it.
- Run `layout migrate --backup NEW-DIR` again at any time (for example after
  a reboot, before the supervisor starts) to move deferred homes that are now
  idle; a run with nothing to do needs no backup and changes nothing.

### Roll back

```sh
sessionsation daemon stop
sessionsation layout rollback --dry-run
sessionsation layout rollback        # with this (layout-aware) build
# then switch the installed release back and start the older supervisor
```

Rollback moves every store and every home at `accounts/<platform>/<name>`
back to its legacy name (removing compatibility links and the registry
tripwire) and removes `layout.json`. It refuses, before any change, a home in
use at its new path or a legacy path that is occupied. Settings added in
1.29.0 (the retention keys below) must not be present in `config.json` for an
older build. As a last resort, the `--backup` directory restores all metadata.

### Retention

The supervisor runs one retention pass a few minutes after start and then
hourly; `sessionsation retention run [--dry-run]` runs one now. Settings in
`config.json` (days are 1..=3650):

| Class | Setting (default) | Rule |
| --- | --- | --- |
| Logs | `log_max_bytes` (8 MiB), `log_retention_days` (30) | A larger `logs/*.log` is copied to `.log.1` and truncated in place; an older `.log.1` is removed. |
| Cache | `cache_retention_days` (14) | Untouched files under `cache/observations/` are removed (regenerated on demand). Snapshots and reports are kept. |
| Operations history | `operation_retention_days` (90) | Journals in a final phase (launches `running`/`failed`; resumptions also `abandoned`; conversation operations, space moves and message recoveries `complete`/`failed`; handoff jobs also `cancelled`/`recovered`) are removed with their sidecars unless a lock is held. Unfinished or `uncertain` journals, `deletions/`, `account-deletions/`, lifecycle admission locks and move backups are kept. |
| Receipts | `receipt_retention_days` (14), `receipt_unknown_horizon_days` (90) | Unchanged receipt retention (revision 18) runs in the same pass; retired ids keep tombstones. |

## Remaining limits

- Busy Queue/FIFO and unplanned native-loss acceptance in downstream adapters need
  separate end-to-end verification; idle Queue/reply evidence does not prove them.
- Native input consumption can remain unknown even after admission or a reply;
  use the receipt's separate facts, never an exactly-once/completion assumption.
- Live subagent coordination and unsent terminal drafts are not guaranteed across
  transfers. Clear drafts first and review subtree warnings.
- Provider-login cancellation requires owner-contract work; closing a UI does not
  cancel the provider login.
- Uncertain handoffs and interrupted archives need explicit reconciliation; no
  blind retry/recovery. Cross-provider conversation conversion is outside this version.
- The repository is public, but inherited messaging compatibility still encodes an
  installation-specific endpoint. It is not a portable default for new integrations;
  use public API v1. Repository history also retains deployment metadata. Current
  documentation cleanup does not sanitize those Git objects; follow the
  [contributor review rules](../agents/README.md) for any separate remediation.

## Native messaging compatibility

The 0.164.0 queue service and the thread-read/turn-summary handler bodies were
compared with upstream 0.163.0 and are unchanged. Queue request/response types
remain compatible; unrelated new image item fields are not delivery evidence.
Both exact profiles run through the isolated busy/idle queue regression.
Unknown versions remain blocked. The public contract owns version discovery
and mismatch diagnostics; compare the app-server handshake, not just the terminal
CLI version. Source compatibility and synthetic tests do not prove a model send.

### Profile 0.165.0

0.165.0 was added through the same gate, without provider calls except one
operator-approved live steer:

- Generated `app-server generate-json-schema --experimental` output of the exact
  0.164.0 and 0.165.0 executables was diffed structurally. Every type Sessionsation
  uses is byte-identical: `turn/start`, `turn/steer` (params and `{turnId}`
  response), `turn/interrupt`, `thread/read`, `thread/turns/list` params
  (`itemsView` notLoaded/summary/full), `thread/items/list`, `thread/queue/*`,
  `thread/resume`, `item/started`/`item/completed`, the account rate-limit response and
  update notification, and login/logout/account-read params; no request method was
  removed or renamed. Differences are
  additive only: a `modelNotFound` `CodexErrorInfo` value (carried inside turn
  errors), a `gcp` account variant, `thread/memory/read`, optional
  `ThreadListParams.attachedToThreadId`, `ThreadForkParams.attachToSource`,
  `ThreadSampleParams.threadSource` and `NetworkRequirements.reviewDomains`.
  `activeTurnNotSteerable` is unchanged.
- `doctor --probe` against a temporary data root: isolated stdio handshake, graceful
  idle exit, required schemas and the Unix WebSocket passed;
  `external_token_contract` reports the same unchanged login schema as 0.164.0.
- `scripts/check_native_compaction.py` (both modes) and the native checkpoint
  fixture below passed against 0.165.0 in a network-disabled container.
- A live steer on a disposable thread recorded the item order that steer reply
  correlation relies on (see the contract's revision 15 section).

The queue service and handler source bodies were not re-read for 0.165.0; the
queue request/response schemas are identical.

### Profile 0.166.0

The managed daemons auto-updated to 0.166.0 while the terminal CLI stayed on
0.165.0, so every delivery mode reported `unsupported_native_version`. 0.166.0 was
added through the same exact-version gate (contract 1.25.1), without provider calls:

- `app-server generate-json-schema --experimental` of the exact 0.165.0 and 0.166.0
  daemon executables was diffed per definition and per method. No client request,
  server request or notification method was removed or renamed. Every type
  Sessionsation reads keeps its shape apart from additive optional fields:
  `ThreadItem` gains flat `startedAtMs`/`completedAtMs` (so turns, items and item
  notifications carry them), `imageGeneration` gains `generationId`,
  `AdditionalContextEntry` (in `turn/start`/`turn/steer` params, unused here) gains
  `name`, `ThreadUsage` gains `nativeUsageUsdMicros`, and `CodexErrorInfo` gains
  `policyCapacityUnavailable`. `thread/start`, `thread/resume`, `thread/turns/list`
  params, `thread/queue/*`, `turn/steer` (`{turnId}` response),
  `thread/settings/update`, `item/tool/requestUserInput`, `serverRequest/resolved`
  and the permission-profile types are unchanged. New and unused:
  `accountSession/list`, `accountSession/switch`, `account/gcp/setup`.
  `ReviewDelivery` dropped `detached` (Sessionsation never starts reviews).
- Upstream `rust-v0.165.0...rust-v0.166.0`: the queue handler only swaps its
  provider check (`check_thread_model_provider_for_input`); `TurnItemsView` summary
  selection is unchanged; `initialize` no longer replays config warnings (unused).
  Thread-history projection was refactored to carry item timings. Item ids and
  order are not schema-visible, so check the first live steer after deploy with
  `receipt` (steer reply correlation) before relying on it.
- `scripts/check_native_compaction.py` (both modes) passed against 0.166.0 in a
  network-disabled container, with results identical to 0.165.0.

### Codex daemon release pinning

Codex's managed daemon runs a `codex app-server daemon pid-update-loop` per
CODEX_HOME. Unless `<CODEX_HOME>/app-server-daemon/settings.json` has
`{"updater":{"autoUpdateEnabled":false}}`, it fetches the standalone installer
hourly, installs the latest release under `packages/app-server-daemon/releases`,
repoints `current` and moves the daemon to it. That can leave the native protocol
outside the verified profiles above.

Sessionsation now writes that flag (preserving every other key, mode 0600) for
each enrolled Codex account when its supervisor starts and before every
`account prepare`/`restart-server`. Codex re-reads the flag before each scheduled
check, so a running `pid-update-loop` exits by itself within one interval
(default 60 minutes), and `daemon start`/`restart` also stop it. The running
app-server is not restarted and keeps its selected release. An unreadable
settings file is left untouched and logged (Codex also skips scheduled updates
while it cannot parse it).

To update deliberately: generate and diff the schemas of the new release as
above, add and verify its profile, deploy that Sessionsation, then run
`CODEX_HOME=<account home> codex app-server daemon update` for each account while
it is idle (the update may interrupt running work).

The checkpoint native-boundary fixture uses the exact supported native executable
against synthetic legacy history in a disposable home, with no credentials or
model calls. It proves the real -32601 item-list gate, then checkpoint/continuation
projection through the actual Unix control server. Run in a network-disabled check
container with only the binary mounted read-only, or supply an absolute local path:

```sh
SESSIONSATION_NATIVE_CHECKPOINT_BINARY=/path/to/verified/codex \
  cargo test --locked installed_native_control_legacy_checkpoint_projection -- --ignored
```

This fixture is distinct from the synthetic active-turn HERDR/tmux cases. It does
not send input to, resume, or interrupt an installed working session.

Compaction/Steer can be verified without provider access using Python 3 and an
explicit Codex 0.164.0, 0.165.0 or 0.166.0 executable:

```sh
python3 scripts/check_native_compaction.py /absolute/path/to/codex
python3 scripts/check_native_compaction.py /absolute/path/to/codex --no-instant-interrupt
```

Run in a network-disabled container with only that executable and script mounted.
The fixture uses an empty temporary home, environment without credentials, and a
loopback fake Responses provider. It gates automatic compaction, sends an exact-turn
Steer, verifies the connection remains open and input appears only afterward, then
checks dedicated compact rejection without interruption. It does not contact an
installed socket, use account homes or call a real model. The tested native server
is independent of whichever terminal frontend is on PATH. Source references are
`core/src/session/turn_input.rs` and `core/tests/suite/pending_input.rs` in
[Codex rust-v0.164.0](https://github.com/openai/codex/tree/rust-v0.164.0/codex-rs).
