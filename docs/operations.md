# Verification, compatibility and operations

## What is verified

The offline suite uses disposable homes, fake provider servers and owned tmux
sockets. It covers stable IDs, stale targets, locks, concurrent operations, partial
failure/recovery, uncertain delivery, account deletion preserving saved history,
and archive/restore/delete. These tests do not establish support for an arbitrary
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
| Claude | Explicit setup-required capabilities | Enrollment, usage, messaging and lifecycle are not implemented |

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

## Safe service maintenance

Back up the entire private data root consistently, including databases, catalogs,
request/receipt journals, account homes and archives. Protect backups like credentials.
Keep project files separate. An archive is recoverable history, not a backup of
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
- Review the contributor release gates before public distribution. Repository
  history and inherited deployment-specific compatibility fields need review;
  documentation cleanup does not sanitize Git history.
