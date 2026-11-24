# Verification, compatibility and operations

## What is verified

The offline suite uses disposable homes, fake provider servers and owned tmux
sockets. It covers stable IDs, stale targets, locks, concurrent operations, partial
failure/recovery, uncertain delivery, account deletion preserving saved history,
archive/restore/delete, and switch/restart/stop on errored sessions. These tests do
not establish support for an arbitrary installed provider binary. Run the commands in
[contributor guidance](../agents/README.md).

The public contract is [v1](contract.md), with its [schema](contract/v1.json) and
[synthetic fixtures](contract/fixtures.json). `docs/messaging/` fixtures belong to
inherited compatibility commands; new integrations use `api --api-version 1`. The
[portable verification record](verification.json) owns exact passing counts,
commands, artifact hashes and the initial cold-run timing limitation. A successful
source/fixture check is distinct from provider compatibility acceptance.

## Version scope

| Component | Evidence | Limit |
| --- | --- | --- |
| Linux x86-64, Rust 1.95.0 | Locked source build and fixture checks | No Windows/macOS backend support claimed |
| Codex terminal 0.162.1 / account app-server 0.163.0-0.166.0 | Earlier authorized disposable native acceptance; schema diffs below | Not a promise for all stock builds, newer versions or other account setups |
| HERDR 0.9.5, terminal protocol 22 | Isolated headless host and PTY tests | Other versions must be checked explicitly |
| tmux | Isolated discovery, exact process ownership and lifecycle regressions | Existing unrelated live sessions are never silently migrated |
| Claude Code 2.1.312 (native messaging aligned with 2.1.314) | Fake-provider fixtures for enrollment, auth gating, launch/resume, live identity, statusline usage, account switch, shared state, hosted sessions and live delivery; redacted real screens for terminal input | No restart or Space move; native cross-session delivery is fixture-verified |
| Gemini CLI 0.66.0, Antigravity CLI 1.4.10 | Fake-provider fixtures and installed-CLI source review | No usage; Gemini individual login retired upstream; login and delivery verified only against fakes |

Earlier native acceptance established account-specific launch, reported usage,
correlated replies, account transfer with history comparison, restart/resume,
rollback and viewer detach. A later empty disposable conversation exercised fresh
switching, a changed directory/Space, startup waiting and archive/delete without
model turns. The isolated post-account-deletion HERDR fixture preserves 70 synthetic
turns and 270 items on a destination account. Synthetic or empty native history is
not a substantive model recall test, and no native test is repeated automatically.

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
without replay, unchanged launch receipts and a later account handoff in the moved
terminal. Explicit message recovery tests stopped, interrupted and errored agents in
both hosts. They use fake providers, no credentials and no model calls. A separate
ignored fixture needs a separately supplied original Switcher executable. Never
mount live homes or sockets into a fixture container.

The verification record also identifies opt-in real-provider acceptance: disposable
tmux and headless HERDR conversations, `/exit`, explicit exited-process Resume,
saved-context recall and repeated original message IDs, run through a
conversation-scoped proxy and an isolated metadata-supervisor harness. It validates
provider replies, not a production owner upgrade. Never treat portable checks as
authorization for model usage or mount an operator's credentials into a container.

## Safe service maintenance

Back up the entire private data root consistently, including databases, catalogs,
request/receipt journals, account homes and archives, and protect backups like
credentials. An archive is recoverable history, not a backup of credentials or a
running process; external backups are outside session deletion.

Use one supervisor per root. Updating a client executable does not update the owner.
A viewer detach does not stop agents; a host-server stop can terminate all its panes.
Migrations, owner handoffs and provider upgrades need their own reviewed procedure.
Do not rename a live root, overwrite a selected executable, remove locks, rewind
generations, replay uncertain requests, or infer safety from a timeout. At startup
the daemon removes leftover `.tmp` files (exactly `.tmp` plus six characters, regular
files owned by the daemon user, unmodified for an hour) from its own state
directories; account homes and archives are never swept.

All readers sharing a root must understand its terminal namespaces and lifecycle
reservations, including `space-moves/` and `message-recoveries/`, before enabling
their actions. Coordinate owner and client pins first: an older reader can invalidate
a custom HERDR binding even when a newer client handles its socket correctly, and a
new client cannot repair another process's discovery. Preserve old receipt domains
and modes. Legacy busy Submit/Steer and messaging Resume are not mapped to
non-steering v1 Submit/Queue.

### Rolling back a release

Check the [changelog](changelog.md) for every version you roll back over. From 1.66.1
`messaging/receipts.db` carries `PRAGMA user_version` 1, stamped when the supervisor
starts; older releases ignore it. A release older than 1.54.0 refuses a layout
version 3 root: run `root rollback` first.
Before rolling back below 1.49.0, drain its operations, because older releases do not run
them and a pending Codex change keeps holding input:

1. `sessionsation api operations`, rows with `kind:"model_change"` and
   `terminal:false`: cancel `waiting` ones (`api cancel-operation ID`); wait for
   `applying` ones to end (Claude about a minute, Codex at most 180 s without
   progress).
2. Wait until every `model_change` row shows `hold.released:true`; `released:false`
   means held messages are still undelivered (see `held_messages` in `api session
   set-model-status --operation ID`).
3. Rows with `kind:"session_launch"` and `terminal:false`: wait for them, or cancel
   those whose brief is still `held`/`pending`; let a brief in `sending` finish.
4. Then install the older release. Claude sessions call the status-line and event
   hooks by path; keep the release directory they name (or the package's `current`
   link) until they are restarted.

## Data-root layout

`layout.json` records the layout version, the migration state and every deferred
account home. Version 2 groups the root by purpose; an active
[root alias](contract.md#root-alias-and-re-key) stamps version 3 with the same tree:

```text
DATA_ROOT/
  layout.json  config.json  daemon.lock
  registry/    registry.db, account-catalog/, session-catalog/ (catalog.json,
               identity.json, spaces.json, models.json), conversations/,
               thread-locations/, managed-panes/, herdr-panes/, provider-state/,
               session-access.json, auto-recover.json, turn-notify.json,
               events.jsonl, pane-claims.json
  sessions/    archived/, preserved/, hosted/
  operations/  launches/, resumptions/, deletions/, account-deletions/,
               account-renames/, account-reset-credits/, conversation-operations/,
               handoff-jobs/, space-moves/, lifecycle-admission/,
               message-recoveries/, compactions/, model-changes/, session-launches/,
               claude-switches/, claude-idle-notify/, auto-recoveries/,
               shared-migrations/, root-migrations/, move-backups/
  messaging/   receipts.db, ledger-stats.jsonl, checkpoints/, observations/,
               claude-steer/, claude-deliveries/, questions/, input-health/
  cache/       state-snapshot-*.json, observations/, compatibility-report.json,
               claude-transcript-facts.json
  logs/        daemon.log, handoff.log
  shared/      claude/ (when shared Claude state is enabled)
  accounts/<platform>/<name>   codex, claude, gemini, antigravity homes
```

A new root is created as version 2. A legacy (version 1) root is refused by every
command except `api version` and `layout` with `LAYOUT_MIGRATION_REQUIRED`; an
interrupted migration or rollback is refused with `LAYOUT_MIGRATION_IN_PROGRESS` until
`layout migrate` (or `layout rollback`) is run again.

### Migrate

```sh
sessionsation daemon stop
sessionsation layout migrate --dry-run            # the plan, nothing changes
sessionsation layout migrate --backup /path/new-empty-dir
sessionsation daemon start
sessionsation layout status                       # deferred homes and why
```

- It holds `daemon.lock` (no supervisor of any build runs) and `layout.lock`, and
  refuses a destination that already exists (`LAYOUT_CONFLICT`) before changing
  anything.
- The backup holds every top-level entry except account homes, with `registry.db`
  and `messaging/receipts.db` copied consistently (`VACUUM INTO`) and
  `layout-backup.json` listing the homes (path, inode). Homes are only renamed, never
  copied, so credentials stay in the root.
- The registry WAL is checkpointed, then `registry.db` moves with its companions. A
  non-database file is left at `registry.db` so an older build pointed at the root
  fails instead of creating an empty registry.
- `archived-sessions`, `preserved-conversations` and `move-backups` leave relative
  compatibility symlinks, because archive manifests, account-deletion records and move
  backups store absolute paths. Journals are never rewritten.
- An account home that any process uses (an environment value such as `CODEX_HOME`,
  `CLAUDE_CONFIG_DIR`, `GEMINI_CLI_HOME` or `HOME`, an argument such as a Codex
  `--remote unix://.../app-server-control.sock`, a working directory, an executable or
  an open file) is *deferred*: it stays at its legacy path, the reason is recorded in
  `layout.json`, and the account keeps working there (`home_deferred: true`). Running
  provider processes are never disturbed.
- An idle home moves to `accounts/<platform>/<name>` and leaves a relative symlink at
  its legacy path, so absolute paths inside provider state (Codex `state_5.sqlite`
  rollout paths, control socket paths) keep resolving. Stored owner and pane-binding
  home paths are rewritten (`home_refs_rewritten`); a later run repairs records an
  earlier run left behind.
- Run `layout migrate --backup NEW-DIR` again at any time (for example after a reboot,
  before the supervisor starts) to move deferred homes that are now idle; a run with
  nothing to do needs no backup.

### Roll back

```sh
sessionsation daemon stop
sessionsation layout rollback --dry-run
sessionsation layout rollback        # with this (layout-aware) build
# then switch the installed release back and start the older supervisor
```

Rollback moves every store and every home at `accounts/<platform>/<name>` back to its
legacy name (removing compatibility links and the registry tripwire) and removes
`layout.json`. It refuses, before any change, a home in use at its new path or an
occupied legacy path. The retention keys below must not be present in `config.json`
for an older build. As a last resort, the `--backup` directory restores all metadata.

### Retention

The supervisor runs one retention pass a few minutes after start and then hourly;
`sessionsation retention run [--dry-run]` runs one now. Settings in `config.json`
(days are 1..=3650):

| Class | Setting (default) | Rule |
| --- | --- | --- |
| Logs | `log_max_bytes` (8 MiB), `log_retention_days` (30) | A larger `logs/*.log` is copied to `.log.1` and truncated in place; an older `.log.1` is removed. |
| Cache | `cache_retention_days` (14) | Untouched files under `cache/observations/` are removed (regenerated on demand). Snapshots and reports are kept. |
| Operations history | `operation_retention_days` (90) | Journals in a final phase (launches `running`/`failed`; resumptions also `abandoned`; conversation operations, space moves and message recoveries `complete`/`failed`; handoff jobs also `cancelled`/`recovered`) are removed with their sidecars unless a lock is held. Unfinished or `uncertain` journals, `deletions/`, `account-deletions/`, lifecycle admission locks and move backups are kept. |
| Receipts | `receipt_retention_days` (14), `receipt_unknown_horizon_days` (90) | [Receipt retention](contract.md#receipt-retention) runs in the same pass; retired ids keep tombstones. `messaging/ledger-stats.jsonl` (one JSON line per ledger open: lock wait, open, schema and hold milliseconds, caller) is cut to its last 1 MiB above 10 MiB. |

`operations/root-migrations/` and `operations/account-reset-credits/` are never
pruned.

## Root re-key

`sessionsation root migrate` moves a data root and its sessions from one spelling of
a bind-mounted tree to the other, for example from `/mnt/user/appdata/sessionsation`
to `/mnt/disk1/appdata/sessionsation`. The old spelling keeps working throughout (the
binds stay mounted); the alias only changes how paths are compared and which spelling
new records use. Reference: [root alias and re-key](contract.md#root-alias-and-re-key).

Order for a window:

1. Before the window: install the release (the alias stays inert until `root
   migrate`), run `root migrate --from F --to T --backup DIR --dry-run` and review
   `left_behind`, `collisions`, `memory_conflicts`, `memory_split`,
   `existing_target_keys` and `unmapped`. Resolve memory conflicts by making the two
   files identical. Create throwaway canary sessions on the old path (one Claude, one
   Codex) in a scratch directory. Stop every hosted (stream-json) Claude worker:
   `root migrate` refuses while one is live.
2. Wait until `api operations` shows nothing in flight, then `sessionsation daemon
   stop`.
3. `root migrate --from F --to T --backup DIR` (a new or empty directory outside the
   root; add `--allow-full-copy` only where the file system cannot reflink). Running
   sessions are not stopped (`deferred_running`); stopped Claude sessions are re-keyed.
4. Start the supervisor, run `root status` and read `gate`. `fail` means a session
   that ran before the activation is no longer recognised as running: stop and run
   `root rollback` (supervisor stopped).
5. Canaries: `root migrate --session <canary>` for the Claude and the Codex canary;
   both must reach `verified` before any real session.
6. Each running session in turn (sessions of one directory back to back): `root
   migrate --session S [--accept-risks] [--prompt-file F]`, then `root status` until
   `verified`, `left_behind` or `uncertain`.
7. `root status`: report `left_behind`, `memory_split` and `unmapped`.

Agent-DM keeps working through the window. It derives the supervisor's control socket
from its configured `dataRoot`; while the alias is active the old runtime path is a
directory symlink to the canonical runtime directory, so its owner check (`lstat` of
the socket, owner and mode, then a connect) reaches the running supervisor. A stale
old runtime directory without a live socket is moved aside (`.pre-alias-<time>`,
never removed); one with a live socket or another symlink is left alone and reported.
Agent-DM can be repointed to the canonical root later; `root rollback` removes the
symlink.

The dry run reads journals directly and settles nothing, but may still write what
every command writes when it opens the root (catalog identity healing, a
hosted-worker record whose worker has exited). Memory fast-forwards use an atomic
exchange; a write landing in the same file within milliseconds can end in the saved
previous version (a narrow, accepted race).

Release gate: the real bind-mount test is `#[ignore]` in the default suite because it
needs a user and mount namespace. Run it in the check image with:

```sh
docker run --rm --security-opt seccomp=unconfined -v "$PWD:/work" -w /work \
  sessionsation-public-check:local \
  cargo test --locked --test root_migrate bind_namespace -- --ignored
```

Rollback: `root rollback --session S` for one session, `root rollback` (supervisor
stopped) for the whole root; sessions already `verified` stay on their new directory.
The whole-root rollback refuses while any process runs with the new spelling of an
account home (`ROOT_ROLLBACK_BLOCKED` lists them): stop those sessions first and
revert the boot files, launcher and `/usr/bin/sessionsation` link together with it. A
rollback that cannot finish ends `rollback_incomplete` and can be run again. Every
moved transcript has a verified copy under `DIR/sessions/<session>/<attempt>/`; no
command deletes these copies.

## Remaining limits

- Busy Queue/FIFO and unplanned native-loss acceptance in downstream adapters need
  separate end-to-end verification; idle Queue/reply evidence does not prove them.
- Native input consumption can remain unknown even after admission or a reply; use
  the receipt's separate facts, never an exactly-once/completion assumption.
- Live subagent coordination and unsent terminal drafts are not guaranteed across
  transfers. Clear drafts first and review subtree warnings.
- Provider-login cancellation requires owner-contract work; closing a UI does not
  cancel the provider login.
- Uncertain handoffs and interrupted archives need explicit reconciliation; no blind
  retry/recovery. Cross-provider conversation conversion is outside this version.
- Inherited messaging compatibility still encodes an installation-specific endpoint
  and repository history retains deployment metadata; see the
  [contributor review rules](../agents/README.md).

## Native messaging compatibility

Every native protocol profile is added through an exact-version gate; unknown versions
stay blocked. The contract owns version discovery and mismatch diagnostics; compare
the app-server handshake, not just the terminal CLI version. Source compatibility and
synthetic tests do not prove a model send.

- **0.164.0.** The queue service and the thread-read/turn-summary handler bodies were
  compared with upstream 0.163.0 and are unchanged; queue request/response types stay
  compatible. Both exact profiles run through the isolated busy/idle queue regression.
- **0.165.0.** `app-server generate-json-schema --experimental` of the exact 0.164.0
  and 0.165.0 executables was diffed structurally. Every type Sessionsation uses is
  byte-identical (`turn/start`, `turn/steer`, `turn/interrupt`, `thread/read`,
  `thread/turns/list`, `thread/items/list`, `thread/queue/*`, `thread/resume`, item
  notifications, rate limits, login/logout/account-read); differences are additive
  (`modelNotFound`, a `gcp` account variant, `thread/memory/read` and optional
  parameters). `doctor --probe` against a temporary root, `check_native_compaction.py`
  (both modes) and the checkpoint fixture passed in a network-disabled container. One
  operator-approved live steer recorded the item order that
  [steer reply correlation](contract.md#steer-reply-correlation) relies on.
- **0.166.0.** The managed daemons auto-updated while the terminal CLI stayed on
  0.165.0, so every delivery mode reported `unsupported_native_version`. The schema
  diff per definition and method found no removed or renamed method and only additive
  optional fields (`startedAtMs`/`completedAtMs` on items, `generationId`,
  `AdditionalContextEntry.name`, `nativeUsageUsdMicros`,
  `policyCapacityUnavailable`); new unused methods `accountSession/list`,
  `accountSession/switch`, `account/gcp/setup`; `ReviewDelivery` dropped `detached`
  (never used). Upstream `rust-v0.165.0...rust-v0.166.0`: the queue handler only swaps
  its provider check, `TurnItemsView` summary selection is unchanged. Item ids and
  order are not schema-visible, so check the first live steer after deploy with
  `receipt`. `check_native_compaction.py` passed with results identical to 0.165.0.

### Codex daemon release pinning

Codex's managed daemon runs a `codex app-server daemon pid-update-loop` per
CODEX_HOME that, unless `<CODEX_HOME>/app-server-daemon/settings.json` has
`{"updater":{"autoUpdateEnabled":false}}`, installs the latest release hourly and
moves the daemon to it, possibly outside the verified profiles. Sessionsation writes
that flag (other keys kept, 0600) for each enrolled Codex account at supervisor start
and before every `account prepare`/`restart-server`. A running loop exits at its next
check (default 60 minutes), and Codex's own `daemon start`/`restart` stop it; the
running app-server keeps its release. An unreadable settings file is left untouched
and logged. To update deliberately: diff the new release's schemas as above, add and
verify its profile, deploy that Sessionsation, then run `CODEX_HOME=<account home>
codex app-server daemon update` for each account while it is idle.

### Native fixtures

The checkpoint native-boundary fixture runs the exact supported native executable
against synthetic legacy history in a disposable home, with no credentials or model
calls. It proves the real -32601 item-list gate, then checkpoint/continuation
projection through the actual Unix control server. Run it in a network-disabled
container with only the binary mounted read-only:

```sh
SESSIONSATION_NATIVE_CHECKPOINT_BINARY=/path/to/verified/codex \
  cargo test --locked installed_native_control_legacy_checkpoint_projection -- --ignored
```

Compaction/Steer can be verified without provider access using Python 3 and an
explicit Codex 0.164.0, 0.165.0 or 0.166.0 executable:

```sh
python3 scripts/check_native_compaction.py /absolute/path/to/codex
python3 scripts/check_native_compaction.py /absolute/path/to/codex --no-instant-interrupt
```

Run it in a network-disabled container with only that executable and script mounted.
It uses an empty temporary home, an environment without credentials and a loopback
fake Responses provider; it gates automatic compaction, sends an exact-turn Steer,
verifies the connection stays open and input appears only afterward, then checks
dedicated compact rejection without interruption. It contacts no installed socket and
no real model. Source references: `core/src/session/turn_input.rs` and
`core/tests/suite/pending_input.rs` in
[Codex rust-v0.164.0](https://github.com/openai/codex/tree/rust-v0.164.0/codex-rs).
Neither fixture sends input to, resumes or interrupts an installed working session.

`NATIVE_HANDOFF_UNVERIFIED` (from `doctor`) means automatic switching stays
unavailable until the cross-account, terminal draft, subagent restoration and
tool-quiescence gates pass; run `sessionsation doctor --probe`.
