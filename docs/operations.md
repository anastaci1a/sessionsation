# Operations and acceptance

Development uses isolated state. Preserve the original installation, account data
and live tmux sessions. Never silently migrate them or start competing supervisors
against their homes. Restart must retain account, exact conversation and permissions.

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

The server's Codex build reports `0.162.1`; HERDR reports `0.9.5` (API protocol 22).
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

Account removal/restore has fixture evidence; no real account membership or
credentials were changed to test it. Claude remains preparation-only by Ana's
direction. Live model evidence is limited to the disposable Codex conversation
above. No new credential owner or deployment of the original switcher is implied.
