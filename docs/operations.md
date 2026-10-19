# Operations and acceptance

Development uses isolated state. Preserve the original installation, account data
and live tmux sessions. Never silently migrate them or start competing supervisors
against their homes. Restart must retain account, exact conversation and permissions.

The ignored `installed_herdr_*` tests in `tests/external_move/launch.rs` use a fresh
HERDR config/socket, synthetic
accounts/native server and inert compiled provider. Run only in the offline check
container with the installed HERDR binary mounted at `/usr/local/bin/herdr` and
`SESSIONSATION_ISOLATED_HOST_TEST=1`, using `cargo test --offline --locked --test
external_move installed_herdr_ --
--ignored --nocapture`. It verifies exact destination, new tab, account/UUID/socket,
duplicate admission, unchanged origin pane and retained shell; no model turn occurs.
This backend handoff test is separate from UI popup/visual acceptance.

Account removal is distinct from disabling: preview affected sessions and stored
data, require explicit confirmation, handle active sessions safely, and retain
recoverable history by default. Permanent local-data/credential purging requires a
separate explicit operation. Never delete the upstream provider account.

Normal submission and queuing retain truthful receipts and recovery. Uncertain
delivery must not be automatically replayed. No forced interruption or steer mode
is requested.

Live acceptance uses disposable conversations and authorized test accounts. Verify
launch, HERDR recognition, usage, account transfer, retained history, restart/resume
and rollback. Launch and mocks alone do not establish complete integration.
Ask Ana before spending or changing access.

## Codex/HERDR evidence and limits

The terminal Codex client reports `0.162.1`; the separately owned account app-server
reports `0.163.0`. Their paths/hashes are recorded in the verification manifest.
The native transport verifies the app-server initialize profile, not the terminal
client version. HERDR reports `0.9.5` (API protocol 22).
Version strings alone do not prove another binary has the same native methods.
One disposable conversation has verified:

- Account-specific launch and HERDR recognition on a headless named server.
- Native queue acceptance followed by a correlated completed reply.
- Fresh account usage with reported windows and reset timestamps.
- Account transfer with complete persisted-history comparison and successful recall
  of an earlier marker on the destination account.
- Same-account restart, explicit stop and exact-UUID resume; read-only sandbox and
  `never` approval settings retained.
- A host rejection before destination admission, followed by automatic recovery
  to the source account and renewed process ownership/messaging discovery.
- Attaching and detaching a HERDR viewer while the agent remains alive.
- Public v1 rename with unchanged backend identity, rejection of a stale restart
  without changing the process, and stop/resume retaining both IDs and account.

Offline regressions additionally exercise lost replies, stale process ownership,
history divergence, active tools, cancellation, and recovery. This does not verify
automatic switching, arbitrary native versions, live subagent coordination, or
preservation of unsent terminal drafts. Clear drafts before lifecycle operations.

## Public contract verification

`scripts/check.sh` runs formatting, Clippy, the Rust suite, documentation links,
and JSON Schema/fixture checks against real CLI envelopes. Tests cover concurrent
unique renames and launches, generation changes, complete watch snapshots,
non-steering busy operations, durable Submit/Queue idempotency, removal previews,
active-account refusal, reversible membership and original-service preservation.

Account removal/restore and local deletion have isolated fixture evidence. The deletion
regression verifies asynchronous admission, refusal of an unhosted account-home
process, explicit interrupted-worker recovery, request-ID deduplication, credential
removal and stable saved/archived discovery. The additional installed-HERDR fixture
`installed_herdr_resumes_preserved_paginated_archive_after_account_deletion`
passed alongside the normal-exit/handoff fixture in 12.16 seconds. It rejects a
conversation becoming loaded during history verification before stopping its
metadata server or removing credentials. Explicit recovery then resumes the
synthetic archived conversation on an explicit destination after local credential
deletion, with all 70 turns/270 items matching and zero model turns. These are synthetic native servers, not live Codex evidence.

A read-only preview against the existing personal account took 38.57/38.94/39.28 ms;
`file_scanning:false` in every result. The old deregistration preview measured
5639.23 ms for its file/revision inventory; deletion's intent preview deliberately
does not perform that work. The approved existing owner advertises deletion support;
older owners still report `owner_upgrade_required:true` and reject admission.
A client artifact update does not replace the root's sole supervisor. Claude remains preparation-only by Ana's
direction. Live model evidence is limited to the disposable Codex conversation
above. No new credential owner or deployment of the original switcher is implied.

## Existing-owner activation gate

Updating the popup client does not replace the running root supervisor. Local
account deletion requires that sole owner to advertise its capability.
Activation is a separate, explicitly approved live owner handoff: stop the old
supervisor through its public daemon command, wait for its socket disappearance
**and** registry lock release (`flock -w 15 /explicit/root/daemon.lock true`), then
start the selected immutable backend against the **same** explicit
root. Preserve the original executable/startup configuration and a private rollback
record. Recheck account/session identity and native PID/start ticks before/after;
never stop provider agents or introduce a second owner to obtain the capability.
A managed-session reconciliation requirement blocks the stop.

The ignored `installed_owner_handoff_preserves_native_agent_and_catalog` fixture
mounts the hash-pinned original Switcher executable read-only at
`/usr/local/bin/original-codex-switcher`, with
`SESSIONSATION_ISOLATED_HOST_TEST=1`. In the network-disabled check container,
run `cargo test --offline --locked --test external_move installed_owner_handoff
-- --ignored --nocapture`. Use isolated state and synthetic native servers only;
never mount production homes or sockets. The original on-disk binary passed in
1.13 seconds; the exact replaced live-owner
image (`ccb423d09545ebd1fd2b7e776ad18d62764df9689ea69be341364f38346ef728`)
passed in 1.07 seconds: the active
native thread, control socket, client PID/start ticks, account/conversation,
backend ID and tmux pane survived the original/new supervisor handoff, with zero
model turns. These are isolated procedure checks; live activation requires operator approval.
The approved installed activation and current immutable owner are recorded in the
[sibling UI deployment evidence](https://github.com/anastaci1a/sessionsation-ui/blob/main/docs/deployment.json).

## Artifact provenance and adapter handoff

[Verification manifest](verification.json) records the exact source commit, build
artifact path/hash, toolchain/image and passing test counts. The original e4d7833
debug binary remains a development artifact; verify its hash
before acceptance because rebuilding can replace it. The UI selects a separate
immutable popup backend, documented with its source, path/hash, tests and rollback in the
[UI deployment manifest](https://github.com/anastaci1a/sessionsation-ui/blob/main/docs/deployment.json). This does not
promote or alter the staged Agent DM review candidate.
The [popup evidence](https://github.com/anastaci1a/sessionsation-ui/blob/main/docs/popup-evidence.json)
distinguishes UI/PTY/installed-HERDR fixtures, exact test sources and zero-model
acceptance. Its additive account limits and saved-session Resume contract do not
change Submit/Queue semantics or adopt legacy receipts.
The original installed `codex-switcher` binary has not been replaced.

Adapters must receive an explicit data root from the operator. For existing Ana
accounts this is `/mnt/user/appdata/codex-switcher`; its existing supervisor remains
the sole credential/enrollment writer. Use that owner's existing socket namespace
and locks through the backend. An adapter must not start its own provider server,
copy credentials, or run another backend owner against the same homes. All clients
of those accounts share this root's session catalog, operation journals and receipt
ledger. UI configuration is separate and owns no account data. Run as the existing
filesystem owner; changing ownership/access is outside this handoff.

The new default `/mnt/user/appdata/sessionsation` is an independent, initially empty
root. It does not discover/import the old root automatically. Selecting a different
root creates a different catalog/receipt domain, not an alias for existing IDs.
Never merge these domains in an adapter.

The [v1 messaging contract](contract.md#messaging) is the coordination boundary for
Agent DM: Submit performs idle preflight then uses the non-steering native queue;
Queue admits eligible busy or idle targets. Legacy receipts are not automatically
imported or mapped. A public receipt requires its durable v1 request binding, and a
legacy message-ID collision is refused. Legacy status names must not be equated to
v1 delivery facts or completion. Submit/Queue semantic changes and any legacy
receipt mapping require explicit coordination before implementation; compatibility
adapters remain separate projects. Clients should consume capabilities and the
published schema rather than inherited commands.


## Legacy coexistence and review gates

Agent DM production continues to observe old jobs/receipts through its original
installed Switcher interface and existing application database. Preserve their
original request IDs, native identifiers, modes, generations and observer cursors.
The sessionsation v1 adapter uses a separate application-tagged database and new
requests only. It must not fall back between interfaces, infer a backend ID from a
provider conversation, replay an old request, or reuse a legacy cursor. Existing
receipt records remain intact; v1 stores its request binding/observation metadata
in its own table in the existing ledger and refuses unbound legacy receipt IDs.
No migration or compatibility mapping is implemented by these additions.

[Review response](contract/review-response.json) tracks the pinned Agent DM request,
exact identity evidence and unresolved acceptance gates. Added readiness and public
observer fields require adapter review, not automatic production activation.
An accepted immutable installed artifact, explicit semantic adoption, separately
approved disposable adapter/runtime tests and a reviewed cutover remain distinct
gates. No review build replaces the launcher-selected artifact or its supervisor.

Agent DM owns the separately authorized isolated runtime acceptance. Its
[numbered results](https://github.com/anastaci1a/agent-dm/blob/77cea576da9075558ec73b0cc55308a117edb5e1/contracts/sessionsation/NUMBERED_RESULTS.json)
prove busy Submit rejection; the original Queue attempt was uncertain locally.
[Backend read-only reconciliation](contract/queue-reconciliation.json) records the
exact root/instance, retained records and admission finding. No Queue replay or
native resume was performed for reconciliation. The initial send error and exact
lock contender remain unknown; a receipt-time busy diagnostic cannot identify
either. The [contract](contract.md#admission-and-contention) specifies the shared
ledger concurrency boundary. This investigation does not change dispatch semantics,
old receipts, the installed candidate or production selection.

HERDR discovery rejects dead/replaced process bindings before socket queries;
bindings/history remain recoverable and live candidates still require full current
host/PID/start/home/UUID confirmation. Menu activity hints read a bounded 64 KiB
rollout tail. A partial first record is skipped; damaged tails or no retained
activity event remain unknown. Native idle checks, operation locks and full history
verification at lifecycle boundaries are unchanged. No cached idle state authorizes
a mutation. Current bindings retain their original socket namespace; a partial host
response preserves a still-live attachment without claiming verified readiness.
See the UI deployment record and [verification manifest](verification.json) for
exact selected artifacts.

Mixed-version readers are a separate rollout gate. The existing Agent DM helper
still selects its older immutable CLI against this same root. That reader can
misinterpret a custom HERDR config/socket namespace and invalidate an otherwise
live binding. The current client fixes its own observer environment and explicit
socket routing; it cannot change another process's old discovery behavior. Agent DM
must review and adopt a compatible pin before custom-namespace acceptance on the
shared root is complete. Do not rewind catalog generations, replay mutations, or
change its pin/data/receipts to conceal this incompatibility.
