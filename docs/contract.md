# Public CLI contract v1

This is the authoritative interface for clients, including `sessionsation-ui` and
Agent DM. Use `sessionsation api --api-version 1 ...`. API v1 is independent of the
package version. Additive result fields may appear; ignore unknown result fields.
Request objects reject unknown fields. Do not depend on inherited commands or
native-provider compatibility interfaces.

The [JSON Schema](contract/v1.json) describes envelopes, snapshots, sessions,
accounts and message requests/receipts. [Examples](contract/fixtures.json) are
checked alongside the CLI regression tests. See [CLI setup](cli.md) and
[verification evidence and limits](operations.md).

## Contract versioning (semver)

The client contract has a semantic version `MAJOR.MINOR.PATCH`, reported as
`contract_version` (a string such as `"1.25.0"`) by `api version` and
`api capabilities`. The integer `contract_revision` is retained for backward
compatibility and always equals the MINOR part: revisions 1-29 were all additive,
so revision N maps to contract `1.N.0` and the current contract is `1.49.1`
(the 1.25.1 PATCH is described under [Codex 0.166.0 native profile](#codex-01660-native-profile-1251) and the 1.31.1 PATCH under [Deletion after a home move](#deletion-after-a-home-move-1311) and the 1.35.1 PATCH under [Observed inline operations are complete](#observed-inline-operations-are-complete-1351) and the 1.40.1 PATCH under [Stale stop gates and unsent receipts](#stale-stop-gates-and-unsent-receipts-1401) and the 1.47.1 PATCH under [Compaction follow-up observes real compaction turns](#compaction-follow-up-observes-real-compaction-turns-1471) and the 1.48.1 PATCH under [No steering while a compaction is pending](#no-steering-while-a-compaction-is-pending-1481) and the 1.49.1 PATCH under [Launches wait for account operations](#launches-wait-for-account-operations-1491)).
A build that predates `contract_version` is read as `1.<contract_revision>.0`.
`api_version` stays the integer MAJOR (`1`), and the package version is independent.
Negotiate with `contract_version`: same MAJOR, MINOR at least what you need, and
prefer `api capabilities` fields over version numbers for individual features.

`api --api-version X` accepts `MAJOR`, `MAJOR.MINOR` or `MAJOR.MINOR.PATCH` and
serves any client that requests the same MAJOR (currently 1); another MAJOR is
refused with exit 2 and an `INVALID_ARGUMENT` envelope.

### What each part means

- MAJOR: breaking changes only. Removing or renaming a command, field or
  enum value; changing a field's type, meaning or units; tightening request
  validation; changing error codes, exit codes or receipt semantics; changing
  safety behavior (replay, delivery, ownership). A MAJOR bump needs a new schema
  file and a reviewed client migration.
- MINOR: additive. New fields, commands, capabilities, enum values, error codes
  and providers. Existing clients keep working unchanged. Reset PATCH to 0.
- PATCH: fixes with no contract change (no schema, field, command or enum change,
  only corrected behavior within the documented contract). Docs-only edits do not bump.

### Client rules for additive data

Clients must treat unknown data as forward-compatible and never misuse it:

- Ignore unknown result fields. (Request objects still reject unknown fields.)
- Unknown providers: show them generically (the provider's own `id`/platform text
  and a neutral icon); offer only operations and delivery modes the provider's
  `api capabilities` entry declares. Never assume Codex/Claude behavior for them.
- Unknown enum values (states, reasons, error codes, availability): display
  generically and take the conservative path (no mutation, no replay, no
  readiness inferred). They are never mapped to a known value.
- Gate on capabilities or on `contract_version` (same MAJOR, MINOR at least the
  minimum needed), not on exact equality, and not on package version.
- A different MAJOR must be refused with a clear reason, not partially used.

### Release checklist

1. Classify the change as MAJOR, MINOR or PATCH using the rules above.
2. Update `CONTRACT_MAJOR`/`CONTRACT_MINOR`/`CONTRACT_PATCH`/`CONTRACT_VERSION` in
   `src/api.rs` together (`contract_revision` follows MINOR; a unit test enforces this).
3. MINOR: add a `since_revision` marker and a section in this document;
   update `docs/contract/v1.json` and `fixtures.json` for new shapes, keeping
   them additive. MAJOR: add a new `contract/vN.json` and migration notes.
4. Update tests (the `contract_version`/`contract_revision` assertions in
   `tests/*.rs`) and `scripts/check_contract.py`.
5. Run `scripts/check.sh` once (fmt, clippy, tests, docs, contract; iterate with
   `--quick` and targeted tests first) and review.
6. Release and publish the new package; the `current` symlink moves it live.
   Clients that follow `current` and gate on semver need no change for MINOR or
   PATCH releases. A MAJOR release needs client releases first or in the same
   cutover.

## Transport and errors

- Success: exit 0, one compact UTF-8 JSON object on stdout with
  `{"api_version":1,"ok":true,"result":...}`.
- Runtime failure: exit 1, JSON on stderr with `api_version`, `ok:false`, and
  `error:{code,message,retryable}`. Argument/version errors use exit 2.
- `api state --watch --interval 2` emits complete NDJSON snapshots, flushing each
  object. Stop the watcher independently; this does not stop any agent.
- Treat `error.code` as machine-readable. Messages are diagnostic, not identifiers.
  A delivery receipt with uncertain state is a successful query result, not proof
  that sending failed. Never infer safe replay from a disconnected client.

## Identity and snapshots

`api version` identifies the actual executable path, SHA-256, package/API versions
and contract version (`contract_version`, with `contract_revision` = its MINOR),
without contacting a provider or a supervisor. Clients that follow the `current`
release negotiate by semver and capabilities (see Contract versioning) and log the
SHA-256 for audit; exact hash pins remain an option for clients that want one.
`api capabilities` publishes provider-specific operations and unsupported reasons.
`attachment_lookup.version:1` advertises
`api attachment --session ID --expected-generation GENERATION`: a read-only,
constant-target catalog/process-epoch lookup for terminal viewers. It returns
the catalog session and attachment with `observed_at`,
`scope:catalog_generation_and_process_identity`, `starts_process:false` and
`sends_input:false`. It does no provider/history discovery and is not message
readiness evidence. A stale generation, missing attachment or unverifiable PID
is refused. Clients still verify the intended host/socket and pane membership
before focusing, then repeat this cheap lookup to reject replacements.
`api state` publishes accounts, sessions, capabilities and `observed_at` (Unix
seconds). `account_source` is `supervisor`, `offline_snapshot` or `uninitialized`;
offline metadata does not prove an account can presently launch.

- `session.id` is a stable backend UUID. `conversation_id` is a separate opaque
  provider identifier. Neither is a pane ID.
- `attachment` is null or identifies the current host/process generation. Treat its
  `target` as opaque and discover it again after replacement. A missing attachment
  does not authorize starting another client; inspect `availability` and capabilities.
- `session.generation` changes on rename, account transfer, replacement or loss of
  an attachment. Mutations require the current `--expected-generation` and fail
  with `STALE_TARGET` rather than selecting a replacement.
- Names are display labels, never identities. Revision 6 uses HERDR’s tab label
  for every verified live HERDR session (working or idle). Sessions sharing a tab
  share its label, and HERDR can contain duplicate labels. Use UUIDs for targets.
  New launches and catalog-only renames still reject name collisions.
- Account IDs are provider-qualified, such as `codex:example`. A removed membership
  remains represented for recovery with `membership:"removed"` and `selectable:false`.
  Disabled memberships remain enrolled. Use `selectable`, not name or plan, for selection.

Session capabilities are fresh snapshot hints; the backend revalidates them at the
mutation boundary. Unmanaged external sessions are not lifecycle targets through v1.

### Lifecycle actions after a provider error

A provider failure (quota, capacity, or an interrupted turn) does not revoke
Switch, Restart, Stop, Rename or Move-to-Space capabilities. These actions remain
subject to account/host support, unique ownership, operation locks and generation
checks. Clients should observe the selected session before opening a mutation
flow and warn on `errored`/`interrupted`/`stopped`. An unavailable observation is
uncertainty, not proof of active work or permanent unavailability. A warning
confirmation never substitutes for the worker's final safety checks.

For native `systemError`, lifecycle checks read only the latest turn summary
and recheck the thread identity/status. Completed, failed and interrupted terminal
turns with no active flags are quiescent; missing evidence and active work still
block destructive steps. Queued input, active goals and background terminals keep
their existing checks and confirmations. Submit/Queue semantics do not change.

An inactive conversation can retain a writable history descriptor in its native
app-server. If that server positively verifies the same history and idle state,
discovery exposes `unfinished_history` with explicit-confirmation resume rather
than trapping it behind `history_busy`. Same-account resume reattaches the saved
thread; transfer safely unloads it through the existing worker. Unknown or active
writers remain busy; polling never unloads or resumes them.

### Archive metadata and filtering

`api state --archive-filter active|archived|all` defaults to `active`.
Every session carries nullable `archived` and an `archive` object with
`kind/state/archived/archived_at/source`. Managed manifests are authoritative;
provider archive status comes from positively discovered history placement.
`kind:unknown, archived:null` means a catalog-only/unobserved conversation, not
proof that it is active or stopped. Unknown entries remain in the default list.
`--archive-filter archived` selects only positive archive evidence; `all` retains
both, excluding permanently deleted managed records. Archived entries use
`availability:archived`; this is independent of runtime liveness. Managed archives
remain accessible through `api archives` and explicit Restore/Delete commands.
Provider archives do not acquire automatic restore or messaging capability.

Recovered histories from deleted-account vaults remain saved resume sources, even
if their retained provider file is under `archived_sessions`; the optional
`provider_storage_archived` describes storage rather than a current managed archive.
Labels (including prefix variants) are not identity. Two similar labels may belong
to different backend/provider UUIDs; never merge them by display name or infer an
archive from a missing attachment. Discovery/filtering sends no message, deletes no
history and never unarchives or resumes a thread.

## Passive readiness

Aggregate state/watch reports attachment availability, not native activity. Its
optional native enrichment has a shared five-second budget and reads only matching
loaded identities when all discovered terminals have conversation IDs. A slow
account supervisor falls back to the stored snapshot after one second;
`account_source` identifies that fallback. `timing` reports inventory, account
status and total milliseconds. These bounds exclude filesystem/host discovery;
they are not an end-to-end latency guarantee. `discovery_warnings` marks incomplete
native enrichment without discarding positively discovered identities.

`directory` is the discovered working directory (process/launch or native thread
metadata), not a directory inferred from a display name or HERDR Space. Renaming
or moving a tab does not change the agent working directory. A desired root can
differ from both process and provider metadata; reporting the desired root would
be false until an explicit supported directory/lifecycle operation applies it.

For a
selected session use:

```sh
sessionsation api observe --session BACKEND-ID --expected-generation GENERATION
```

This performs read-only native status/public-summary requests on the existing
owner-managed socket. It never sends input, resumes/loads a thread, starts a worker
or probes with a message. Native observation is bounded to five seconds; known launcher bindings avoid
terminal-host discovery and rollout scans. The result is typed by `Readiness` in
the schema: idle/working/interrupted/errored/disconnected/unknown, diagnostic reason,
`observed_at`, `valid_until`, freshness and a separate `runtime_generation`.
`process_state` and `process_observed_at` separately report a kernel-validated
running, exited or unattached process; an idle native thread is not a process-stop
claim. `observation.status` distinguishes absent evidence from expired evidence.
Freshness expires five seconds after the native evidence was obtained; neither
batch-cache nor summary-cache hits extend it. It is not a reservation.
For local legacy histories, last-turn summaries are indexed by runtime generation,
canonical history path, device/inode, size and nanosecond modification/change
times. Every reuse still makes fresh native status reads before and after it and
rechecks the file identity. Changed files, processes or endpoints invalidate the
index; concurrent readers share one reconstruction. No history text is stored in
this index. `observation.turn_summary` identifies `native_summary` versus
`unchanged_history_cache` and retains the original summary's `observed_at`.
The public observation keeps that original timestamp and expiry too: unchanged
disk contents cannot exclude a newly started in-memory turn. Summary entries
expire after five seconds even when the file is unchanged. Cold reconstruction
shares the existing five-second overall observation budget; it no longer has an
earlier three-second cutoff when this indexed path is available. A cold provider
read can still exceed that budget and must fail closed. Nonlocal/unverifiable
history paths retain uncached native observation.
RPC timeouts identify `native_initialize_timeout`, `native_thread_read_timeout`,
or `native_turn_summary_timeout`; the overall bound is `observation_timeout`.
`observation.failure` supplies the RPC method (null for the overall budget),
budget in milliseconds, a readable explanation and retryability. Missing evidence
keeps both timestamps null and every action unavailable. Slow provider reads are
not proof of idle readiness; back off instead of repeatedly forcing refresh.
Cached-only failed observations remain `observation.status: unobserved`, not stale
evidence. The compatibility `freshness` field remains fresh/stale.
A changed catalog or runtime rejects the result as `STALE_TARGET`.

`explicit_intent` reports a positively recorded stop token or unknown; an idle
process never proves absence of deliberate stop/pause intent. Fresh interrupted
turns (1.37.0: not failed turns, see below) expose `actions.resume.available:true` when otherwise eligible; Submit and
Queue report `explicit_resume_required`, matching dispatch. They are recoverable
targets, not permanently unavailable sessions. Clients should offer explicit
Resume-and-submit using the observed runtime, previous turn and stop token; do not
turn this reason into an unsupported-delivery result or silently substitute Resume
for an ordinary send. The obsolete `terminal_turn_requires_review` reason is no
longer emitted. Unknown queue depth remains null.
Stop intent is a permission gate, separate from observed execution: it may coexist
with working state while a stop drains or after continuation outside Sessionsation.
Polling never clears it merely because the agent is working. Newly recorded stop
tokens identify their durable stop-operation journal; older tokens have no proven
operation attribution. Explicit saved-session Open clears its prior token only
after confirming the reopened process, using a compare-and-clear that preserves
any newer stop request. Focusing an already-live terminal does not clear it.
Submit is available only for positively idle eligible observations; Queue may be
available while working. Sending-disabled, removed-account, stale, explicit-stop
and unknown observations withhold readiness with reasons. These are conservative
hints, not a new dispatch policy: actual sending still rechecks its existing gates.
No legacy busy-Steer capability is adopted. Explicit Resume is described below. Poll a selected
session no faster than every five seconds; use backoff after errors.

### Batch readiness and client polling

`api observations` reads `{api_version:1, sessions:[{session_id,
expected_generation}], policy:"if_stale"}` from stdin. Policies are `refresh`,
`if_stale` (default), and `cached_only`. At most 64 unique targets are accepted;
eight observations run concurrently under a shared five-second native budget.
Results preserve input order and carry individual `{session_id,ok,data}` or
`{session_id,ok:false,error}` entries. One bad target does not discard other results.

Launcher records are checked against kernel process identities and catalog
ownership. Passive observation performs no terminal-host discovery and takes no
mutation lock. Unrecorded live attachments return `ATTACHMENT_UNOBSERVED` rather
than guessing. Dispatch still independently verifies terminal/runtime ownership.
The private cache is disposable; it stores typed status, not conversation bodies.
Cache hits never renew `observed_at` or `valid_until`. `observation.status`
distinguishes fresh, stale and unobserved; null timestamps mean no native evidence.
The compatibility `state` enum remains unchanged; additive `runtime_state`
distinguishes a recorded deliberate stop. Stale evidence withholds sending hints.

Clients should batch their visible targets, keep stable IDs as keys, preserve old
receipts, and avoid independent per-target sequential discovery loops. Timeouts
and unknown states are pending evidence, not proof of a stopped or unavailable
agent. A late provider cannot extend this command's native budget. Host scheduling
and local filesystem overhead remain outside that budget. This change removes
backend discovery/lock overhead; it does not establish the cause of a downstream
36–44-second refresh cycle, which can also include adapter concurrency/backoff.


## Session and operation commands

`api session --session ID --expected-generation GENERATION` returns the current
public session with `directory`, `availability`, and `original_host`. Missing
information is null/unavailable. Original host provenance comes only from a
matching recorded launch; it includes the host session/socket and workspace/tab/pane
IDs. Clients must verify those host resources still exist. Capability
`session_details:true` advertises this read-only command.

```sh
sessionsation api rename --session BACKEND-ID --expected-generation GENERATION --name work
sessionsation api switch --session BACKEND-ID --expected-generation GENERATION --to codex:other
sessionsation api restart --session BACKEND-ID --expected-generation GENERATION
sessionsation api stop --session BACKEND-ID --expected-generation GENERATION
sessionsation api resume --session BACKEND-ID --expected-generation GENERATION --herdr-session coding
sessionsation api operations
sessionsation api recover OPERATION-ID
```

Lifecycle mutations return journaled operation objects, whose `id`, `source_session_id`, `kind`, `phase`,
`detail` and `confirmation_seq` identify progress. A returned job is not completed
work. Terminal phases include `complete`, `cancelled`, `failed` and `recovered`.
A failed or uncertain operation must be inspected before retrying. v1 operations
expose `can_cancel`, `can_recover` and `can_respond` where applicable; consumers
must not infer recovery support from a failed phase alone. Archive operations do
not support replay/recovery through `api recover`. Account-deletion status marks
an orphaned worker failed after a ten-second admission grace period, without
rewriting its receipt. Recovery explicitly continues the same confirmed request.
Accounts reserved for deletion expose `deletion` status and remain unselectable
until cleanup finishes; a partial credential deletion must not be re-enabled.
Resume, handoff and conversation-archive admissions share a per-conversation lock.
An admitted or uncertain resume reserves its conversation until a settled receipt;
only that same request may continue its transfer. Public session capabilities
disable competing lifecycle actions and expose `pending_operation_id` while reserved.
A terminal handoff remains reserved while its worker holds the worker lock;
a completed/failed label alone does not establish that continuation or recovery
has stopped.
Submit/Queue admission also retains this reservation until the handoff worker
exits, including its continuation and rollback tail. This blocks new native
writes; it does not suspend messages already accepted into Codex's own queue or
its terminal composer's local queue. The inspected native queue API has no pause
primitive. Such pending input is not deleted or replayed to simulate a hold;
the handoff retains its source if queued work prevents a safe unload.
Durable launch, resume, deletion and archive workers use separate process groups
so closing a requesting viewer does not stop their work. v1 operations
wait for positive idle state, send no steer, and do not automatically continue work.
Risk/background-stop consent is separate. `api respond OPERATION-ID yes|no
--sequence CURRENT-SEQUENCE` rejects stale confirmations. `api cancel-operation
OPERATION-ID` requests cancellation before an unsafe transition.

`api operations` exposes v1 lifecycle and local account deletion operations; it
does not adopt legacy switcher jobs. Account deletion records have
`kind:"account_delete"`, `account_id`, `id`, `phase`, `detail`, `updated_at`,
`history_home`, `history_preserved` and `can_cancel:false`; they have no source
session or conversation ID. Their terminal phases are `complete` and `failed`.
`api account delete ID` previews intent without scanning account files;
`--confirm ID --request-id UUID` admits the asynchronous operation. The ID binds
one account permanently; reuse for another account rejects `REQUEST_ID_CONFLICT`.
`api account deletions [--request-id UUID]` reads receipts; `api recover UUID`
explicitly resumes a failed/interrupted worker after acquiring its operation lock.
Do not call `cancel-operation` or `respond` on account deletion records.
See [local deletion semantics](cli.md) for preservation and sole-owner gates.
`source_session_id` is the stable backend ID, or null if the source was removed
from the catalog; historical operations remain inspectable. `provider_conversation_id` remains
explicitly separate. Public recovery rejects jobs outside this contract.

`api launcher new --account codex:example`, `api launcher resume --session ID
--expected-generation GENERATION`, and `api launcher shell` return an opaque
`{action:"exec",argv:[...],cwd:...}` plan. A terminal client executes the exact argv
without a shell, applying cwd only when non-null. The backend revalidates the
session when the resume plan executes. Do not reconstruct provider launch commands
or handle credentials in a client. Plain-shell launch sets the recursion bypass.

## Messaging

`api send` reads a single request from stdin; maximum request size is 32 KiB and
text is limited to 16 KiB. Supply a new UUID `message_id`, stable backend `session_id`,
current `expected_generation`, `mode:"submit"|"queue"`, text and an `expires_at`
Unix timestamp within the next 24 hours. `api_version` must be 1.

Both modes use the verified native **non-steering queue**. Submit requires idle
preflight and rejects a busy target. A concurrent turn can defer a submitted
message; it can never turn this request into a steer. Queue accepts idle or busy
eligible sessions. Existing sending/access gates and ownership locks remain enforced.

`api receipt MESSAGE-ID` observes progress; `api cancel MESSAGE-ID` cancels only
when supported and positively confirmed. Receipt facts distinguish local admission,
native acceptance, recorded input and an observed public reply. Native acceptance
is not completion or proof of input consumption. Only a correlated terminal reply
establishes `completed_with_reply`.

A repeated identical ID/request returns or reconciles the original receipt. Changed
payload, mode, target, generation or expiry gives `MESSAGE_ID_CONFLICT`. Requests
remain bound to the original conversation/runtime even after rename or transfer.
An uncertain write is journaled before socket I/O and is never resent. Receipt and
idempotency storage is bounded at 10,000 and never silently evicts replay protection
(retired ids keep a tombstone; see [receipt retention](#receipt-retention-revision-18)).
Revision 18: a Submit or Queue to a session that waits on an approval or user input
is a definite refusal, `rejected` with reason `approval_or_user_input_required`,
before any native write; a resend returns that refusal and never dispatches.
Expiry is an unsent submission deadline, not cancellation of accepted native work.

### HERDR-owned session names (revision 6)

Negotiate `session_names.version:1`. Inventory/state/watch reads one bounded host
snapshot per server/socket and maps the verified attachment's pane ID to its tab
ID and label. It never matches by display name. The catalog caches the last
observed label for host outages and retains it after the process exits; it is not
an independent naming authority for live HERDR sessions. `name_source` is
`herdr_last_observed` for HERDR attachments and `catalog` otherwise. This field
is provenance, not a freshness guarantee. Unavailable/ambiguous host observations
leave the cached name unchanged. No second background service is needed; existing
UI/client state polling performs reconciliation in the backend.

`api rename` validates generation, lifecycle locks and exact host process, renames
the containing HERDR tab, then reads it back before reporting success. A lost
acknowledgement is reconciled by that read, never automatically replayed. Failed
verification returns `HOST_RENAME_UNCONFIRMED` or `HOST_RENAME_NOT_APPLIED`;
refresh state before retrying. Concurrent host edits win on subsequent observation.
Since 1.48.0 names are scoped to a HERDR Space; see [Space-scoped session names](#space-scoped-session-names-1480).
The UI contains no naming/synchronization operations of its own. Inactive and
non-HERDR sessions retain catalog naming behavior. Host labels are neither
truncated nor given synthetic suffixes; explicit CLI input keeps the 30-character
limit. Renames do not restart agents, alter conversations or change accounts.

Logical session/conversation IDs and runtime generations survive label changes;
catalog generations change when a new label is observed. Clients must refresh
catalog generations before mutations and must not assume names are unique.
Submit/Queue/Resume/Steer semantics and existing receipts are unchanged.

### Deliberate in-turn steering (revision 5)

Negotiate `contract_revision >= 5` and `steering.version == 1` from `api capabilities`.
This is a separate explicit `mode:"steer"` on `api send`, never a fallback for Submit,
Queue or Resume. It sends `turn/steer` to an existing busy Codex turn without
interrupting, cancelling, restarting, launching, or queuing a later turn. Native
acceptance does **not** promise immediate consumption or a response.

First call `api steer-target --session ID --expected-generation GENERATION`.
This bounded read-only check returns `active_turn_id`, `runtime_generation`, catalog
`generation`, availability/reason and five-second evidence timestamps. Normal
observations advertise `actions.steer`; callers still need the explicit target
lookup because ordinary busy polling avoids reconstructing turn history.

The ordinary request fields remain required. Set `mode:"steer"`, omit `recovery`,
and add:

```json
{
  "steering": {
    "version": 1,
    "expected_turn_id": "active-turn-from-steer-target",
    "expected_runtime_generation": "64-character-runtime-generation-from-steer-target"
  }
}
```

Admission rechecks catalog/runtime identity, active turn, explicit-stop and sending
gates under the existing lifecycle locks. Idle, approval-blocked, stale, moved or
changed-turn targets are rejected without substitution. The native operation
also enforces the expected active turn, covering a turn transition after preflight.
The immutable request reserves its UUID across public and legacy transports.
The journal enters uncertainty **before** native I/O. Retries after an attempted
steer, process restart or lost acknowledgement return the original receipt and
never resend; changed payload, mode or preconditions conflict. Before dispatch,
a failed preflight produces a terminal rejected receipt. Cancellation can cancel
only provably unsent local intent; accepted/uncertain steering cannot be cancelled
through this API (`STEERING_CANCELLATION_UNSUPPORTED`).

Native 0.164.0, 0.165.0 and 0.166.0 distinguish automatic compaction within a regular turn from a
dedicated compact turn. Automatic compaction retains accepted Steer input until
model continuation; it does not interrupt the compaction request. No extra local
queue or automatic retry is added. Dedicated compact/review turns reject injection
before acceptance; the exact structured native rejection becomes a terminal
`rejected` receipt with `compact_turn_not_steerable` or
`review_turn_not_steerable`. Generic rejections and lost acknowledgements remain
uncertain. Native metadata has no compact-task flag, so a preflight can still
advertise eligibility before this final native rejection. After rejection, observe
again and explicitly choose a new action; never reuse the ID with a different turn
or payload. Capabilities publish this behavior and the verified profile. The
[native fixture](operations.md#native-messaging-compatibility) describes verification.

Steering receipts use `delivery_semantics:"deliberate_in_turn_steer"`.
`acknowledged_turn_id` is present only after a matching native acknowledgement.
`facts.native_acceptance` distinguishes `accepted`, `input_observed` (no
acknowledgement, but the steer's own input item was found), `unknown` (possible
dispatch), and `not_attempted`. A lost/malformed acknowledgement or unclassified
native error remains `delivery_unknown`; it is not safe to resend. Before revision
15 receipts were acceptance-only; reply attribution is described in
[steer reply correlation](#steer-reply-correlation-revision-15). Receipt polling
never attempts delivery. The native queue ID is null.

Schema definitions `SteeringIntent`, `SteeringTarget`, and the conditional
`MessageRequest`/`Receipt` rules are in [v1.json](contract/v1.json); request
examples and accepted/uncertain receipts are in [fixtures.json](contract/fixtures.json).
Older requests/receipts remain valid. Clients must negotiate this capability,
retain their original UUID/request, and handle acceptance separately from delivery.

### Passive current-turn binding (revision 8)

`api turn-target --session ID --expected-generation GENERATION` reads the latest
native turn identity/status without an old receipt or sending permission.
Negotiate `turn_observation.version:1`. It uses the same owned endpoint and verified
native profiles as checkpoints, validates catalog/runtime before and after, and
compares two bounded summaries. Active, completed, failed and interrupted turns are
supported; empty history returns `available:false` with `no_observed_turn`. It never
launches, steers, clears stop tokens or reads private items. Failures/races return
errors, never guessed targets or renewed stale timestamps.

Use its `turn_id` and `runtime_generation` to explicitly bind a new checkpoint
stream. Existing receipt streams still reject `checkpoint_stale_turn`: do not
retarget them or attribute the latest turn's text to an old send/Steer. This is
turn-associated observation, not delivery or causal reply evidence. Schema:
`TurnTarget`; fixture: `turn_target`. Steering eligibility is separate; this lookup
does not authorize sending.

### Explicit Resume-and-submit

API v1 contract revision 3 adds `api send` with `mode:"resume"`. Negotiate
`message_recovery` from `api capabilities`; the older provider `resume` flag
refers to saved-session launching and does not imply this message mode. Codex's
verified native protocol profiles are **0.163.0, 0.164.0, 0.165.0 and 0.166.0**. Claude recovery is unsupported.
This is caller-requested continuation of the same conversation, never conversion,
new-conversation creation, steering, forced interruption or automatic restart.
Revision 4 adds `message_recovery.exact_item_idle_start` and separate native start
evidence. Clients requiring interrupted-thread recovery should require that
capability, rather than relying on the package version alone. Existing requests
and receipts keep their identity; upgrading never replays an older request.
Ordinary Submit/Queue behavior and historical receipts are preserved. Submit,
Queue, observation, discovery and receipt polling never launch an exited agent.

The request retains the ordinary message fields and adds `recovery`:

```json
{
  "expected_state": "interrupted",
  "expected_previous_turn_id": "turn-from-observation",
  "expected_stop_id": null,
  "expected_runtime_generation": "64-character-runtime-generation"
}
```

These are placeholders, not runnable identifiers. Copy current observation values;
previous-turn and stop keys must be explicitly present, using null only when none
was observed. Supported expected states are `interrupted`, `errored` and `stopped`.
A retained live attachment requires its exact native runtime generation. A live
idle session requires a recorded stop token to use `stopped`; idle alone is not a
stop. Ordinary readiness and process existence are separate facts. Unknown,
unobserved, stale or nonterminal history is not proof that recovery is supported.
A live process with an unloaded conversation must reconnect or be explicitly
stopped first; this mode does not silently load a retained, unverified attachment.

For an **exited agent**, first generate an explicit launch plan:

```sh
sessionsation api recovery-plan < recovery-target.json > recovery-plan-envelope.json
```

`recovery-target.json` contains `session_id`, `expected_generation` and one host:

```json
{"kind":"herdr","session":"HOST-NAME","socket":"/absolute/owned/herdr.sock","workspace_id":"SPACE-ID"}
```

or:

```json
{"kind":"tmux","socket":"/absolute/owned/tmux.sock","session":"unused-session-name"}
```

Use an existing HERDR Space or a new, unused tmux session name on an existing
owned server. A new normal tab/pane is created without moving any live agent.
The returned `result` is an opaque `RecoveryPlan`: supply it unchanged as
`recovery.launch`. Its `previous_turn_id`, `previous_turn_status` and
`stop_intent_id` describe the saved target for the explicit preconditions. Null
runtime generation is acceptable only for exited-process recovery with a plan.
The saved account and directory cannot be overridden in this mode. Account transfer,
working-directory changes and ordinary saved-session launching remain separate
lifecycle actions. Missing recorded permissions require an explicitly configured
launch; this mode never falls back to permissive/default-account settings.

Planning and admission validate conversation identity/history, account eligibility,
recorded approval/sandbox policy, existing native owner/profile, empty queued work,
no active background work, directory identity, provider/backend executable hashes,
configuration fingerprint, and destination socket/Space identity. The worker and
account-aware launcher recheck these before their respective transitions. Changed
material requires a new reviewed plan. No credential owner is started. All mutators
sharing the root must understand `message-recoveries` reservations; an older
supervisor is refused with `RECOVERY_SUPERVISOR_UPGRADE_REQUIRED_COORDINATE_ALL_ROOT_WRITERS`.
Upgrade coordination is separate from installing a candidate CLI.

The immutable message UUID is journaled **before launch**. One explicit invocation
can start one worker; retries and status queries only inspect its original journal
and delivery receipt. Changed payload/preconditions/plan under the same UUID are
`MESSAGE_ID_CONFLICT`. Launch dispatch uncertainty is `launch_unknown`; it is
separate from `delivery_unknown` for an uncertain native queue write. Once native
write intent exists, the original queue request is never automatically replayed.
Native queue addition uses the provider's atomic non-steering dispatch path; a
concurrent turn may defer input, but cannot turn it into a steer. This was checked
against the [inspected native queue implementation](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/ext/queue/src/service.rs).

`api capabilities.native_protocol.supported_versions` lists exact verified
app-server versions. `api observe` reports the actual handshake version under
`observation.native_protocol`, including on a rejected version. Unknown versions
fail closed with `unsupported_native_version`; running processes alone never
prove messaging support. Check the account app-server version, which can differ
from the terminal executable. No automatic downgrade, restart or message replay
occurs after a provider upgrade.
Client IDs are correlation values, not an assumed native uniqueness guarantee.

Resume receipts add `launch: {required, phase, confirmed, detail, generation,
target, created_at, updated_at, reconcile_command}`. `launch.generation` is the
candidate post-launch **catalog** generation, usable as launch evidence only once confirmed; `accepted_runtime_generation` is the distinct
native attachment generation and is null until delivery is bound. `confirmed`
requires a positive process/attachment check. It does not mean the message was
accepted. Always inspect the ordinary receipt `facts`, correlation and reply.
A successful launch can still have expired, rejected or uncertain input. The stop
token is cleared after a positive, idle-checked `thread/queue/start`
acknowledgement for the exact newly accepted queue item, or later durable
correlation proving that the original explicit Resume input executed. Queue insertion alone
does not wake an interrupted native thread. Resume receipts additionally expose
`recovery_start: {state, turn_id}` (null for older receipts and other modes).
`confirmed` means the native start was acknowledged, not that input was recorded
or a reply exists. `unknown` means the start may have executed; `rejected` means
it was refused, possibly because automatic dispatch already consumed the item.
Neither state permits replay. Until start or input execution is proven, the stop
token remains for review. Clearing compares the original expected token under
the conversation lock; a newer stop request always survives. Receipt observation
may reconcile this metadata but never invokes a native start.
Ordinary public Submit/Queue recheck terminal-turn state before native insertion.
An interrupted/failed turn requires explicit Resume even when native thread
metadata says idle; queue insertion is not a substitute for recovery. The legacy
compatibility send interface is unchanged.
Recovery preflight freshly reads the native summary within one five-second
observation budget; it does not authorize a start from cached readiness.
Only the original explicit Resume dispatch invokes the native start. Ordinary
Submit/Queue, retries, reconciliation and polling never invoke it.
Before binding delivery, recovery waits up to ten seconds for the local receipt
ledger lock, so a concurrent status reader need not fail the operation. This waits
only for local admission; it never retries a native write. Exhausted lock waiting
still reports a confirmed launch separately from input that was never accepted.
Public receipt polling releases the root-wide receipt writer lock during native
observation, then commits only if the durable receipt still matches its starting
snapshot. A concurrent cancellation or delivery change takes precedence.

```sh
sessionsation api receipt MESSAGE-ID
sessionsation api recovery-reconcile MESSAGE-ID
```

Reconciliation acquires the original worker lease and never launches, resumes,
sends, cancels or retries input. Before recorded dispatch it can mark a stopped
worker failed (a created plain shell may remain). After dispatch it requires
positive evidence of the original attachment; it can settle launch facts and
release the local reservation, preserving the original delivery ledger. If launch
remains ambiguous, it refuses and retains the reservation: inspect the destination
and original IDs instead of starting a replacement. Routine receipt polling does
not take over a worker. Pending recovery cancellation without verified worker
quiescence is unsupported. The 10,000-entry bound never evicts replay protection:
retired ids keep a tombstone ([receipt retention](#receipt-retention-revision-18)).

Actionable refusals distinguish missing plans, changed executable/configuration/
history/runtime/stop/turn, missing permissions or directories, unavailable accounts,
queued/background work, busy or unverified native state, and incompatible owners.
`ADAPTER_BUSY` on send or receipt is transient contention, not failed delivery;
retry receipt lookup or the **identical request ID**, never a replacement message.

### Admission and contention

The messaging ledger has one exclusive `adapter.lock` across legacy and v1
send/status/receipt/cancel operations. It is held through native dispatch or
observation. Contention returns `ADAPTER_BUSY` without waiting; an adapter-local
serialized lane reduces self-contention but cannot coordinate other processes.
Do not remove locks, stop legacy observers or introduce another owner to bypass it.
Revision 18: Claude and Gemini sends check the shared message-id namespace under
this lock; that check waits up to three seconds for a concurrent Codex observer
instead of returning `ADAPTER_BUSY` at once (observations hold the lock for up
to their native-read budget, which was the cause of intermittent `ADAPTER_BUSY`
on Claude/Gemini sends). Codex sends and receipts still refuse at once.

For **that invocation of `api send`**, `ADAPTER_BUSY` precedes request binding,
admission and native dispatch. It does not negate an earlier admission of the same ID.
Ordinary messages bind in SQLite; explicit Resume first binds in its recovery
journal before any launch. Submission uncertainty is saved before touching the
queue socket. Receipts and bindings are retired only by
[receipt retention](#receipt-retention-revision-18), which keeps a tombstone. For `api receipt`, `ADAPTER_BUSY` means lookup could not run;
it says nothing about whether the send succeeded. Preserve operation context and
the original error instead of converting a receipt error into a send rejection.

After contention clears, `RECEIPT_NOT_FOUND` means no v1 binding or recovery journal exists for that ID
in the selected root. Verify the exact root/instance and original immutable request
before drawing an admission conclusion. A wrong root, unbound legacy ID or lost
original error must not trigger a replacement send. This is not a Queue retry or
new model-use authorization. Receipt polling may resume with backoff; uncertain
delivery is never automatically replayed. Historical backend reconciliation and
its limits are linked from [operations](operations.md#legacy-coexistence-and-review-gates).

### Receipt evidence

`accepted_generation` is the original backend generation; the independent
`accepted_runtime_generation` is the original native attachment generation
(or null while Resume has not yet bound delivery).
`correlation` is null until positively matched by request ID, or contains
`native_turn_id`, `original_input_item_id`, optional `completion_input_item_id`,
`observed_runtime_generation` and optional `reply_item_id`. Completion projection
IDs never overwrite original input IDs. Runtime replacement may change the observed
runtime generation without rebinding the request. `native_queue_id` is separate.
Only public final reply item/text/phase are projected; histories, tools and reasoning
are excluded.

`observation` reports not_checked, observed, uncorrelated, unavailable, cached or
not_applicable. `checked_at` is the receipt-query time; `native_attempted_at` is
null when no native read occurred. `evidence_observed_at` is durably recorded for
v1 when correlation was positively observed; old bindings may have unknown age.
Cached queries do not refresh it. Freshness is fresh for less than five seconds,
otherwise stale; missing evidence time is unknown. `error_code` is a bounded
observer diagnostic, not proof that delivery failed. Correlation, acceptance and
completed replies survive observation errors and restarts. Poll receipts no faster
than every five seconds, backing off after errors; there is no public event cursor.

For verified local legacy histories, public receipt reads preserve bounded scan
progress between polls instead of restarting pagination after every timeout.
Each poll reads at most one 50-turn summary page (or rechecks the newest page),
within a five-second native-read budget. Progress is private and keyed to the
immutable receipt, runtime generation and file identity/revision. It expires
after five minutes; changed history or runtime discards it. All pages must be
checked for duplicate client IDs and the newest page must agree before any
candidate is published. No partial match grants input/reply evidence.

`reply_scan_pending` means continue polling the same receipt; it is not a send
failure and never warrants another send. `reply_scan_changed` and
`history_changed_during_observation` mean the snapshot changed; existing durable
facts remain intact. `reply_scan_limit_exceeded` identifies the existing
1,000-turn bound, and `reply_observation_busy` means another observer owns that
receipt's scan. Missing/unverifiable local history retains the bounded uncached
scan. Stock legacy storage still reconstructs history for each native page;
persisting progress avoids lost work, not that upstream reconstruction cost.

| State | Evidence |
| --- | --- |
| pending_local | Durable local admission; no confirmed native acceptance |
| native_accepted | Positive queue acceptance; consumption/completion unproven |
| execution_started | Correlated native input/turn; no terminal reply yet |
| completed_with_reply | Correlated public terminal reply observed |
| delivery_unknown | Submission may have occurred; never automatically replay |
| rejected | Preflight refused this request |
| expired | Submission deadline passed while provably unsent; or, with reason `outcome_unknown_at_retention_horizon`, retired unfinished (see [receipt retention](#receipt-retention-revision-18)) |
| cancelled | Local cancellation or positive supported native cancellation |
| failed | Correlated native turn failed/interrupted; no successful reply |

`facts` contains bounded positive booleans for local admission, native acceptance,
recorded input and observed reply. `input_consumed` remains unknown. Native
acceptance survives expiry; TTL is not native execution cancellation. The schema
fixtures include reconnect errors, acceptance after expiry and projection recovery.

## Account operations

`api account list|add|login|auth|managed-home|enable|disable|usage|limits|remove|restore|delete|deletions` are documented in
[the CLI guide](cli.md). Removal is local membership removal, not provider deletion,
credential purging or disabling the original switcher enrollment. The separately
confirmed `delete` operation deletes local credentials/data while preserving
conversations; it requires a capable sole owner. Arbitrary history purging remains
unsupported. Since revision 11 `remove`, `restore` and `delete` also accept
`claude:` IDs (see revision 11 below; revision 10 rejected them).
`api account managed-home claude:NAME` (additive) switches an adopted Claude account to
`<root>/accounts/claude/NAME` and returns `{account_id,provider,home,previous_home,relocated,already_managed}`
(`note:"ALREADY_MANAGED"` when idempotent). Error codes: `UNSUPPORTED_PROVIDER_OPERATION`,
`ACCOUNT_HOME_UNAVAILABLE`, `UNSAFE_ACCOUNT_HOME`, `ACCOUNT_IN_USE`, `AUTH_REQUIRED`,
`WRONG_AUTH_MODE`, `AUTH_IDENTITY_MISMATCH`. Claude session `capabilities.resume` is false with
`capability_reasons.resume:"SAVED_DIRECTORY_UNAVAILABLE"` when no existing absolute saved directory is known.

Codex capability `account_limits` enables `account limits ID`, returning
`{account_id,limits,saved:false}`. Supplying both `--short-used` and `--weekly-used`
saves through the existing supervisor and returns its limits with `saved:true`.
The `limits` object is the existing account snapshot's `switch_thresholds` object;
clients display remaining-position markers as `100 - *_percent` and only mark
custom limits when `customized:true`. Saving does not acknowledge plan changes.
New launch, resume and account-deletion records expose immutable `created_at` Unix
admission time. Legacy records omit it when their creation time is unknown; never
substitute `updated_at`, which changes with progress. Legacy records are not
rewritten to invent dates. Adapters can sort known creation times descending and
place unknown dates last with stable ID ties.

`new_account_name_max:30` (also the folder name; display names allow `account_display_name_max:64` and session names `session_name_max:64`, both new in 1.35.0) limits new enrollments in both the public API and sole account owner; longer existing IDs
continue to work. Clients must gate new commands on published capabilities.

## New-session popup admission

The additive `api launch validate|start` commands read one JSON request from stdin;
`api launch list` lists retained receipts, newest first, without replaying requests.
`api launch status --request-id UUID` reconciles its durable record. Request v1 has
`version`, UUID `request_id`, provider-qualified `account_id`, unique `name`,
`host: {session, socket}`, `space: {kind: "existing", id}` or
`{kind: "new", label}`, and `directory: {path, create, confirmed_existing}`.
Paths are absolute. `create` is an ordered contiguous list of explicitly staged
missing directories ending at `path`; an existing directory requires an empty list
and `confirmed_existing: true`. Validation creates no project directories or HERDR
resources. Start revalidates, reserves the name and dispatches once. Reusing a UUID
with different input fails; repeating identical input returns its original record.

Launch records are separate from message receipts. Phases are admitted, preparing,
creating_terminal, dispatching, starting_provider, attached, running, failed and
uncertain. `attached` means catalog/native identity was recorded; `running` requires
post-exec process evidence. Records expose created directory paths, selected/new
workspace/tab/pane IDs, stable session ID, generation and native conversation ID
when known. A failure or timeout must not trigger an automatic new Start. Preserve
created resources and reconcile the original request; uncertain operations retain
their name reservation. No credentials are in requests or receipts.

Status can reconcile a late attachment after a worker timeout using the same
catalog generation, account, native UUID, PID/start time and remote socket argv.
It never replays provider/terminal commands. Terminal focus is a client presentation
action after `running`; status itself does not change the user's focused tab.

`api account list` additionally reports `prefill: {eligible, reason, observed_at,
valid_until}`. This conservative preference respects usage freshness, existing
account thresholds and ordinary allowance. Purchased credits do not justify
prefill. Explicit `selectable` remains separate; the popup can explain and permit
manual selection without rewriting thresholds. The installed supervisor remains
the sole account-data/credential owner.

Installed client provenance and isolated acceptance are recorded in
[verification scope](operations.md). Adding commands does not itself change a
consumer’s selected artifact or data-root owner. The existing `launcher` exec-plan
interface is unchanged.

### Saved-session popup Resume

`capabilities.resume.version == 1` exposes `api resume-launch validate|start`
with JSON stdin, plus `status --request-id UUID` and `list`. A saved Codex
conversation does not require its original account to remain registered. The
caller selects an available target account; preserved history, stable identity
and launch validation still apply. Original-account metadata is provenance, not
a resume credential. See `ResumeRequest`
and `ResumeReceipt` in the schema. The request has version, request UUID, stable
`session_id`, `expected_generation`, account ID, editable name, host and Space.
Optional `confirm_unfinished: true` acknowledges only the unfinished-history warning;
it defaults to false and is advertised by `resume.unfinished_confirmation`.
Unfinished histories remain resumable after this explicit confirmation; no user
message is replayed and live-writer, directory, conflict and generation checks remain.
Optional `directory` uses New Session's canonical path, deferred creation plan
and existing-directory confirmation; `resume.directory_selection` advertises it.
Omission preserves the saved location, including for old receipts. Validation
creates nothing. Start rechecks and journals created directories, retaining the
original `saved_directory` for source verification and rollback. Same-account and
account-transfer resume both use the chosen directory; no project files move.

A missing old directory can be replaced. Writable history, unfinished-history
consent, conflicts and lifecycle locks remain enforced. The chosen location is
retained after a verified running handoff; partial failure retains created
directories for review. Session details report the last verified launch/resume
Space; clients may prefer it only when the host and Space match their inventory.

Start journals the intent before creating a new normal tab. A repeated UUID with
identical input returns its original receipt; different input is a conflict. A
second unfinished Resume for the same stable session is rejected. Generation,
saved state, account availability and operation ownership are checked again before
provider execution. Same-account Resume uses the existing exact-conversation
launcher. A changed account delegates to the existing non-steering transfer worker,
retaining history, permissions and stable ID; `operation_id` identifies that job
for its normal confirmation/recovery APIs. It creates no model turn.

Receipts distinguish admission, preparation, terminal creation, rename, dispatch,
transfer, provider start, attachment and positively observed running. `failed`
before host effects is safe failure; `uncertain` after effects requires inspection,
never automatic replay. `recovered` describes the transfer worker's rollback.
The selected name may already be committed if a later transfer fails; the receipt
retains `previous_name`. Tabs and history are retained on partial failure. There
is no implicit cancellation, cleanup, or credential change. Closing the popup
only detaches its observer. List/status reconcile records without resubmitting.
Existing `api resume`, launcher plans and message receipts retain their semantics.
Once a Resume receipt reaches `running`, it records the completed handoff. A later
Stop does not regress that receipt; read the session catalog for current liveness.

Session snapshots optionally expose `activity`, `last_accessed` and `active_since`
(Unix seconds, null when unavailable). `active_since` measures the current verified
attachment's process lifetime, not conversation age or cumulative model work. Saved
conversations have no live attachment time. Normal provider exit (including Codex
`/exit`) clears runtime attachment on discovery while retaining stable identity,
conversation files, names and resume availability.

Revision 17 populates `active_since` for running Claude and Gemini
sessions from the first honest source, never a guess, and leaves it null otherwise
(`last_accessed` stays null for them until 1.26.0). Claude: the live registry
`sessions/<pid>.json` `startedAt`, accepted only when the registry process is
verified against /proc (PID plus start ticks) and `startedAt` is neither before that
process start nor in the future, else the verified /proc process start. A session
whose process is a pre-started `claude bg-spare` (the row `pid`/`process_start` may be
the spare) never uses the spare's start or its `startedAt`: only the exact, verified
HERDR `claude attach` client start counts, otherwise null. A Sessionsation-hosted
session without a registry time uses the hosted worker's recorded `started_at`.
Gemini: the verified /proc start of the session process. Stopped, archived, unverified
or unobserved sessions are null.

Account removal previews scan metadata with four bounded readers and deterministic
ordering. Their revision and fresh-confirmation checks still include the same
stored-file metadata; no cached or partial inventory grants consent.

Session snapshots/details optionally include `conversation_created_at` (the native
recorded timestamp, otherwise null) and `history_file_bytes` (observed rollout
bytes, excluding shared account databases). Creation is never inferred from mtime.

## Conversation archives

Capability `conversation_archive.version:1` advertises list/archive/restore/delete,
`compression:"gzip"` and `storage:"sessionsation_data_root"` for Codex. Claude and
Gemini sessions use the same commands since
[revision 17](#claude-and-gemini-archive-restore-and-delete-revision-17). `api archives`
returns archived/prepared entries with stable `id`, fresh `generation`, provider
conversation identity, original creation timestamp, original history bytes and
`compressed_bytes` (null if stored files cannot be measured safely).
Archive/Delete availability depends on verified inactive history and operation ownership, not enrollment. External homes remain eligible with final native-server, live-writer and generation checks. Confirmed conversation jobs retain their source-home provenance for background discovery without borrowing the viewer’s account environment.

`api archive`, `api restore-session` and `api delete-session` require `--session`,
`--expected-generation`, `--request-id` (UUID) and `--confirm` equal to the stable
session ID. A successful command returns a queued conversation operation, also
reported by `api operations`. Its immutable `created_at` and `id` correlate polling;
`kind` is `archive_session`, `restore_session` or `delete_session`. Intermediate
phases describe history verification/compression/restoration/removal; terminal
phases are `complete` and `failed`. These jobs are not cancellable. Admission is
not completion. The same request/target/generation returns its original job;
reusing an ID with different input is rejected. No automatic replay occurs.

Archive/Restore/Delete invalidate the captured generation. Archive hides the
session from normal state; Restore returns the same backend/provider identities
as saved history, without starting a model. Loaded threads and live copies block
unsafe changes. Delete is permanent, including known managed archives, and
preserves project files and credentials. Storage and recovery rules belong to
[the CLI reference](cli.md#saved-and-archived-conversations). Existing send receipts
and New/Resume receipt phases are unchanged.

### Resume availability reasons

Session state also publishes nullable `directory` from the observed source, so clients can warn about a shared project directory and show cached details without generation-bound reads.
Session state and details include nullable `resume_block_reason`: `unsupported_provider`, `host_unavailable`,
`history_unavailable`, `already_running`, `moving`, `missing_directory`,
`history_busy`, `unfinished_history`, `conflicting_history`, or `unverified_history`.
`unfinished_history` is a confirmation warning, not a disabled resume capability.
Other values explain observed resume gates; capability flags remain authoritative and
launch rechecks current state. Consumers must tolerate future reason codes.
Read-only history inspection does not constitute `history_busy`; an open writer
does. If descriptor access flags cannot be inspected, discovery conservatively
retains the busy gate. Live attachment and operation ownership checks still apply.

A live terminal can be waiting at Codex’s workspace-trust/startup prompt before its
conversation is loaded. Lifecycle workers report `waiting_for_provider_ready` and
retain it until native state is confirmed; they never approve that prompt or treat
the unloaded root as a ready agent. Background-terminal checks for unloaded
saved/child histories require consistent native read/list/read evidence rather
than treating a rejected RPC as empty.

## HERDR Space placement

`capabilities.space_move.version == 1` advertises the additive placement API;
`session.capabilities.move_space` gates the selected current attachment.
`api spaces --session ID --expected-generation GENERATION` returns
`SpaceChoices`: stable session ID, captured generation, current Space ID,
existing Spaces with `id/name/current`, and positive pane/terminal identity.
The bound host socket is authoritative even in an unrelated caller environment.
Since 1.48.0 `move-space` also takes `--create-space LABEL` (a label that must not exist), checks the
session name's uniqueness in the destination Space at admission, and writes the verified destination
label to the session's `space`; see [Space-scoped session names](#space-scoped-session-names-1480).

`api move-space --session ID --expected-generation GENERATION --space SPACE-ID
--request-id UUID` returns `SpaceMoveOperation`, also listed by `api operations`.
Phases: `admitted → dispatching → reconciling → complete`, or `failed/uncertain`.
Admission returns before host dispatch. A detached, per-request locked worker
revalidates the saved target and completes placement independently of the caller.
Clients must observe the returned operation ID until a terminal phase; an
`admitted` response does not confirm that the terminal moved. This adds no
supervisor and does not restart the agent.
No cancel/respond action is offered. Reuse of the same UUID and exact target
observes the saved result before generation resolution; changed input is
`REQUEST_ID_CONFLICT`. Stale generation or missing destination is rejected before
dispatch. Selecting the current Space completes without mutation.

New Space (revision 20): `api move-space ... --new-space LABEL --request-id UUID`
replaces `--space` (the two are mutually exclusive; one is required). The
label passes the Space-label rule (`INVALID_SPACE_LABEL`: 1-128 bytes, no surrounding
whitespace, control characters or `/`, not `.`/`..`; spaces inside are fine) and must not match any Space label in a fresh
`workspace list` of the session's own HERDR instance (`WORKSPACE_NAME_EXISTS`,
before any journal or host mutation). The worker issues one host request,
`pane move PANE --new-workspace --label LABEL --no-focus`: HERDR creates the
Space around the moved pane, so no initial shell pane exists and nothing is
closed. `workspace create` is not used. The created Space id is journaled
(`created_space`) only after it is verified as absent before admission, carrying
the label and holding the exact terminal; it is reported as `space_id` and
`created_space_id` (both null until then; `new_space` echoes the label).
A retry with the same UUID observes the saved result, a changed payload is
`REQUEST_ID_CONFLICT`, a lost reply is `uncertain` and `api recover` finds
the terminal by its stable terminal id in any Space (read-only), so no second
Space is ever created. `capabilities.new_space.move_space` gates this form.

HERDR can change the pane ID during a move. The stable terminal ID and verified
kernel process identity tie the returned pane to the original runtime. Only
attachment placement/revision changes; the backend ID, provider conversation ID,
PID/start identity, native runtime generation and old message receipts remain
intact. Discovery, account-switch/restart and `original_host` follow verified
placement provenance within the launch/resume request’s epoch; historical
launch/resume receipts are never rewritten. Reused host pane IDs in a subsequent
launch do not import earlier placement history.

Admission shares lifecycle and messaging locks. In-flight or uncertain placement
blocks competing lifecycle mutations. A lost reply or incomplete ownership update
is `uncertain`, never permission to retry. `api recover UUID` is read-only toward
HERDR: it finds the original terminal in the requested destination and repairs
binding/catalog metadata. A crash before dispatch can release its unissued intent.
An unconfirmed destination remains reserved; disappearance, timeout or observing
only the source does not prove that a move is safe to replay. Fresh snapshots
are required after completion. Existing Submit/Queue semantics are unchanged.

The [CLI reference](cli.md#move-a-running-session-between-herdr-spaces) owns host
scope, UI-visible effects and operational limitations.

## Client upgrade notes

Source package 0.2.0, contract revision 3, retains API v1 with additive capability
negotiation. New clients may opt into batch observations and explicit message
recovery; accept Resume-only null runtime generations and `launch_unknown` receipts.
Existing Submit/Queue requests and receipt domains are unchanged. Package/API
numbers alone do not identify a build: inspect the executable path and SHA-256
from `api version` alongside `api capabilities`. Older builds without `api version`
need `--version` and an external SHA-256. Readiness reason codes are extensible;
prefer optional `observation.failure.message` for timeout explanations and inspect
`observation.status` to distinguish unobserved from stale. A timeout never grants
permission to resend, resume or infer readiness.
Do not substitute PATH, another data root or an older receipt domain when pinning.
Keep the sole supervisor and existing receipt databases; a client upgrade is not
an owner handoff. Current installation selection and consumer pins stay unchanged
until their separate compatibility review. New archive views require accepting
`availability:archived`; keep default filtering for existing active-session clients.

## Public turn checkpoints (revision 7)

Negotiate `capabilities.checkpoints.version == 1`; package 0.2.0 remains API v1.
`api checkpoints < request.json` is an explicit read-only observation, independent
of sending and receipt polling. The request includes `version:1`, stable
`session_id`, `expected_generation`, `expected_runtime_generation`,
`expected_turn_id`, and optional `cursor` (initially null). Obtain exact turn/runtime
identity from a tracked receipt or `api steer-target`; never select a turn by name.
See `CheckpointRequest`, `Checkpoints`, and `CheckpointEvent` in the schema and
`checkpoint_request`/`checkpoints` in the fixtures.

The response exposes only native `agentMessage` text explicitly marked
`commentary`, `partial_answer`, or `final_answer`. Other payloads are structurally
skipped: no reasoning, user input, tool output, terminal capture or raw transcript
is returned or journaled. History mode is read from the attached server using metadata-only `thread/read`,
not inferred from the terminal/PATH executable. On native profiles 0.163.0 and
0.164.0, `thread/items/list` is supported only for **paginated** histories. Legacy
histories deliberately reject it with -32601. For **legacy** histories the observer
uses `thread/turns/list` with `itemsView:full`, `limit:1`, descending, verifies the
exact latest turn, and discards every non-public item before journaling. It never
uses whole-thread `thread/read(includeTurns:true)` or reads transcript files.
Both routes retain native-version and attachment security checks. Unknown profiles fail closed; Claude is unsupported.
No observer, terminal supervisor, credential writer or subscription is started.

- Every event has a stable content revision `event_id`, native `item_id`, monotonic
  journal `sequence`, and `observed_at`. Text is a full snapshot, not an append delta;
  a changed item emits another revision. Order is first observation in native page
  order, not proof of causal ordering between messages. Deduplicate by `event_id`.
- Association is **turn-only**. Text does not prove Steer consumption, causation,
  a correlated reply, message delivery or completion; steer replies come only
  from steering receipts (revision 15). Even `phase:final_answer` is not completion evidence by itself;
  `turn_status` is separately observed native state.
- The exact requested turn must still be the latest visible native turn, before and
  after the item read. Its status may be in progress, completed, failed or interrupted.
  A different turn returns `CHECKPOINT_STALE_TURN`; a transition during the read
  returns `CHECKPOINT_TURN_CHANGED`. Retain previous evidence and retry only the read,
  or resolve a newly tracked target explicitly. Never silently attribute another turn.
- Catalog/runtime changes return `STALE_TARGET`. Resolve fresh identity, but do not
  carry a cursor into another runtime/turn. Endpoint failure, unsupported native
  profile or timeout yields an error, no fresh events, and no advanced cursor.
  Reads have an eight-second overall budget and never send/replay input or launch.
- Paginated reads scan at most 50 native items. Legacy reads inspect one exact
  latest turn, bounded to **8 MiB native wire input and 8192 items**, and project
  at most 50 unseen public snapshots per scan. `scan_pending` remains true while
  additional unseen legacy snapshots remain; subsequent scans deduplicate by the
  existing event IDs. Both routes return at most 50 retained events.
  `history_mode` reports the selected native route; capability
  `checkpoints.history_modes` advertises this compatibility correction. The 8 MiB
  input allowance is exclusive to checkpoint connections; steer anchor and reply
  observation reads use a 64 MiB bound (see Steer reply correlation); other ordinary
  messaging connections retain their 1 MiB frame limit. `NATIVE_FRAME_LIMIT` or
  `CHECKPOINT_LEGACY_LIMIT` refuses oversized input without returning partial text.
  The complete compact success JSON envelope plus trailing newline is capped at
  **786432 bytes (768 KiB)**, below a 1 MiB client transport ceiling. JSON escaping
  and UTF-8 bytes count toward this limit. `byte_limited:true` marks a page shortened
  by bytes; whole events are preserved and the cursor advances only past returned
  events. A single public text item up to 65536 UTF-8 bytes remains supported.
  Capability `checkpoints.max_response_bytes` advertises the envelope limit.
  `scan_pending` means native pages or unseen legacy snapshots remain; `has_more` means journal events remain.
  Save the returned opaque cursor only after processing the response. Poll again
  while the exact turn is tracked; reaching the native page tail starts another
  scan from the beginning next time, so edits to an existing item are observed.
  While unread journal events remain, reads drain that backlog before scanning more
  native items; byte pagination therefore cannot evict its own unread events.
  `native_page_read:false` identifies this replay: exact-turn/runtime checks still
  run, but observation/event timestamps retain their original age.
  This is polling, not lossless token streaming; short-lived intermediate revisions
  between polls may be missed. Empty events do not mean the agent is idle.
- Up to 256 events and 4096 deduplication revisions are retained per exact
  session/runtime/turn in the Sessionsation root. `CHECKPOINT_CURSOR_GAP` means
  the cursor is expired, from a different journal, or beyond its tail. Start a new
  read with null cursor to consume the retained window; `history_gap:true` explicitly
  marks truncated older events. Reaching the revision ceiling returns
  `CHECKPOINT_RETENTION_LIMIT`; do not pretend the stream is complete.
- Journal writes are atomic and serialized by an observer-only lock. Retries and
  CLI restarts replay stable IDs/order without touching message ledgers. A provider
  restart changing runtime identity requires a new explicitly resolved stream.
  `CHECKPOINT_OBSERVER_BUSY` means another read is updating that same journal;
  retry the read. Native pagination errors leave the prior journal intact.
  Native rejections expose only `error.native:{method,code}`; provider error text,
  error data, raw frames and non-public payloads are never diagnostic output.
  Missing or changed history mode fails closed instead of guessing another route.
- `observed_at` timestamps the item-page read; `checked_at` is response preparation,
  and `valid_until` is five seconds after observation. `freshness` can already be
  stale if verification was slow. Replayed events retain their original timestamps.

Clients should use checkpoint events for an intermediate activity feed, while
continuing to use the original receipt for delivery/correlation. Do not promote a
checkpoint to a completed mailbox reply. Compatible additive API revisions should
be negotiated by capability rather than exact build equality.

### Claude provider (revision 10)

Additive. `capabilities.providers[]` reports `claude` with `implemented:true`, the
existing flat booleans and an `operations` object (`{supported, reason}` per
operation; Codex has the same object). `capabilities.account_names` states that
names are unique per provider since 1.30.0 (see [Account names](#per-provider-account-names-and-backend-rename-1300)). Account snapshots add `provider`,
`provider_capabilities`, `config_home` (adopted Claude homes), `auth_detail`
(`AUTH_REQUIRED`/`WRONG_AUTH_MODE`, never identity), `usage_state`
(`fresh|partial|stale|missing|no_windows|error`) and `usage.source`/`usage.semantics`
for every provider. Claude usage keeps the shared `usage.short`/`usage.weekly`
shape (`duration_minutes` 300/10080); a missing window is `null`, never 0.

`account add claude:NAME` returns `existing_home` (an operator-populated managed
home was registered unchanged) and `auth` (`logged_in`, `auth_method`,
`api_provider`, `subscription_type`, `problem`). `account add` accepts `claude:NAME` and `--config-dir`; `account login claude:NAME`
returns `{login:{action:"exec",argv,...}}`; `account list --platform` filters.
Bare names fail with `ACCOUNT_PROVIDER_REQUIRED`; a provider that differs from the
enrolled one fails with `ACCOUNT_PROVIDER_MISMATCH`; a taken name fails with
`ACCOUNT_NAME_EXISTS`.

Claude sessions add `conversation_id_source:"native"`, `native_status`,
`native_name`, `native`, `pid`, `process_start`, `host_known`,
`capability_reasons`, and use availability `running|stopped|stale|unknown`.
`name`/`name_source` (`catalog`, or `herdr_last_observed` for HERDR-attached sessions since revision 11) are Sessionsation metadata; `native_*` are Claude's.
`launcher new|resume`, `launch` and `resume` accept Claude accounts/sessions;
launch receipts keep `account_id: "claude:NAME"`.

### Claude lifecycle, login, headless delivery and display names (revision 11)

Additive; negotiate `contract_revision >= 11`. Provider entries add flat
`account_rename:true` (both) and, for Claude, `account_removal:true`,
`account_deletion:true`, `account_refresh:true` and a `headless_delivery` object
(`version:1`, `modes:["resume","new"]`, `semantics:"headless_print"`,
`requires_stopped_session:true`, `timeout_seconds:{default:600,max:3600}`,
`timeout_is_uncertain:true`, `automatic_replay:false`, `cancellation:false`).
`operations` adds `account_rename` and `headless_delivery` (`supported:false` for
Codex); Claude `account_deletion` carries a reason describing the adopted-home case.

Accounts: `remove|restore|delete|deletions|recover` accept `claude:NAME` with the
Codex request/response shapes. Removal previews add `active_processes` and
`stored_data.adopted`. Deletion previews add `home_disposition`
(`deleted|left_in_place`) and `adopted_config_home`; `deletes_local_credentials` is
false for an adopted directory. Deletion receipts add `external_home` and
`home_disposition`; an adopted directory is never modified. Sessions of a deleted
Claude account report `availability:"stopped"`, `resume_block_reason:"account_deleted"`,
`history_preserved:true` and `capability_reasons.resume:"ACCOUNT_DELETED"`.

Login: `api account add claude:NAME` returns `login` equal to
`api account login claude:NAME`'s `login` (`{action:"exec",interactive:true,
credentials_in_client:false,argv}`) plus `login_required` (boolean). `api account
refresh claude:NAME` probes `claude auth status` now and returns
`{account_id,provider:"claude",auth_required,auth_detail,auth_mode,observed_at}`
where `auth_mode` is `{logged_in,auth_method,api_provider,subscription_type}` (plus `rate_limit_tier` only when `claude auth status` reports one) or
null when the probe failed. The supervisor also re-probes every 3 seconds while a
Claude account is `auth_required`.

Session names: a Claude session with a verified HERDR attachment now follows the
Codex HERDR name contract (revision 6): `name_source:"herdr_last_observed"`, tab
label changes observed into `name`, and `api rename` renames the tab. Detached and
stopped Claude sessions keep `name_source:"catalog"`; `native_rename` stays
unsupported (the label is applied as `--name` on the next launch/resume).

Display names: account snapshots add optional `display_name`. `api account rename
PROVIDER:NAME --display-name X|--clear` returns `{account_id,name,display_name,changed}`;
`id`, `name` and the home never change. Errors: `INVALID_DISPLAY_NAME`,
`ACCOUNT_DISPLAY_NAME_EXISTS`, `ACCOUNT_DELETION_IN_PROGRESS`.

Headless delivery (`api send`, Claude sessions/accounts only):

```json
{"api_version":1,"message_id":"UUID","mode":"resume","session_id":"UUID",
 "expected_generation":"HEX64","text":"...","expires_at":UNIX,"timeout_seconds":600}
{"api_version":1,"message_id":"UUID","mode":"new","account_id":"claude:NAME",
 "directory":"/abs/dir","name":"optional label","text":"...","expires_at":UNIX}
```

`recovery`/`steering` must be absent or null; `timeout_seconds` is optional. Submit,
Queue and Steer addressed to a Claude session failed with
`UNSUPPORTED_PROVIDER_OPERATION` until revision 16 (see
[Claude steering](#claude-steering-hosted-sessions-and-live-peer-messages-revision-16)). Admission refuses (nothing is
journaled) on `STALE_TARGET`, `SESSION_ALREADY_RUNNING`, `SESSION_STALE`,
`SESSION_OPERATION_IN_PROGRESS`, `SAVED_DIRECTORY_UNAVAILABLE`, `ACCOUNT_REMOVED`,
`ACCOUNT_DELETED`, `ACCOUNT_UNAVAILABLE`, `AUTH_REQUIRED`, `WRONG_AUTH_MODE`,
`INVALID_EXPIRY`, `INVALID_TIMEOUT`. The response and `api receipt MESSAGE-ID` return:

```json
{"message_id","session_id","accepted_generation","provider":"claude",
 "provider_conversation_id","account_id","mode","delivery_semantics":"headless_print",
 "state","reason","facts":{"accepted_local","accepted_native","input_recorded",
 "input_consumed","reply_observed"},"correlation":{"session_id","reported_session_id","matched"}|null,
 "reply":{"text","text_truncated","is_error","subtype","session_id","num_turns","usage","duration_ms"}|null,
 "execution":{"phase","directory","timeout_seconds","created_at","started_at","finished_at",
 "new_conversation","automatic_replay":false},"observation":{...}}
```

`state`: `pending_local` (journaled), `execution_started` (journaled before the
process starts), `completed_with_reply` (session id matched, `is_error:false`),
`failed` (matched, `is_error:true`), `rejected` (provably not executed, e.g.
rechecks failed or `WORKER_EXITED_BEFORE_EXECUTION`), `expired`, or
`delivery_unknown` (`EXECUTION_TIMEOUT`, `SESSION_ID_MISMATCH`,
`UNPARSEABLE_OUTPUT`, `OUTPUT_TOO_LARGE`, `WORKER_EXITED_DURING_EXECUTION`). `reply`
is non-null only for a matched session id. Uncertain outcomes are never replayed;
the same `message_id` with an identical payload returns the original receipt and a
changed payload is `MESSAGE_ID_CONFLICT` (message ids are shared with the Codex
stores). `api cancel` returns `HEADLESS_CANCELLATION_UNSUPPORTED` while a delivery is
pending or executing. Schemas: `ClaudeHeadlessRequest`, `ClaudeHeadlessReceipt`,
`AccountLoginPlan`, `AccountRefresh`, `AccountRename` in [API v1](contract/v1.json).

### Session identity, continuations and Add to HERDR (revision 12)

Additive; negotiate `contract_revision >= 12`. `api capabilities` adds
`session_identity`, `add_to_herdr` and `resume_placement` objects.

Native-id re-attachment: a live process is attached to exactly one catalog entry,
the one whose conversation the process currently runs: for Claude the registry
`sessions/<pid>.json` `sessionId` (PID and start time verified); for Codex the
thread of its single open rollout (tmux TUIs), else its `resume <thread>` argv.
When the same process switches conversation in its terminal, the existing entry
of the new conversation takes over the exact attachment (name, account, history
kept) and the entry it left is released to `stopped`; no entry is created for a
conversation that already has one. A Codex TUI connected to an app-server
(`--remote`) does not expose a thread switch in argv or open files, so such a
switch is not observed.

Claude continuations: when Claude Code sends a conversation to its background
daemon under a new session id, it appends
`{"type":"continued-in","sessionId":OLD,"continuedInSessionId":NEW}` to the old
transcript. Only that record type and those two ids are read (bounded tail; it
counts only while no `user`/`assistant` record follows it and NEW is a transcript
of the same account home). The entry of OLD then follows NEW: stable id, name and
account are kept, `previous_conversation_ids` lists OLD, and an entry already
registered for NEW is merged into it. Generation-checked lookups of a merged id
fail with `SESSION_MERGED: ...` naming the surviving id; refresh the list. Merging
never deletes and recreates identity: the survivor is the entry that already
records the lineage, else the pre-existing entry of the earliest conversation; a
later entry for a conversation already in a lineage (for example one re-registered
by an older release) is merged into it, not the reverse. Aliases and lineage are
mirrored in `session-catalog/identity.json`, which older releases do not rewrite,
and are restored from it on every catalog access. `api session repair-identity`
runs that restore plus continuation linking explicitly and reports
`{restored_aliases,restored_lineage,merged,aliases,lineages,unresolved_ids,unresolved_reason}`;
`unresolved_ids` are ids that placement evidence names but no entry, alias or
identity record resolves. They are reported, never recreated. Branches (copied
records stamped `forkedFrom.sessionId`) stay separate conversations and are
exposed as `forked_from_conversation_id`. No other lineage is inferred, so no
manual link operation exists.

Parked Claude sessions: moving an interactive session to the background with
Claude Code's left-arrow gesture records `parkedJobId` in the interactive
process's registry entry and forks the conversation into that `bg` job
(`continued-in` on the old transcript). The interactive process stays in its
terminal as the job's frontend (the agents view with the job open), and input
typed there lands in the job's transcript. That process is therefore the
entry's terminal: the attachment is kept (or verified anew from its HERDR pane),
`terminal_input` uses it, peer messages go to the job's own messaging socket,
and a generated job title (`nameSource:"auto"`) never replaces the name given to
the parked session. A parked record no longer holds its old session id. When
Claude clears `parkedJobId`, the process is no longer a terminal of the job.

Claude sessions add `native.kind` (registry kind, `bg` for background-daemon
sessions) and `native.job_id`; all sessions add `previous_conversation_ids`,
`capabilities.add_to_herdr` and `capability_reasons.add_to_herdr`
(`SESSION_NOT_RUNNING`, `ALREADY_IN_HERDR`, `INTERACTIVE_ELSEWHERE`,
`ATTACHED_ELSEWHERE`, `CODEX_TERMINAL_NOT_MOVABLE`, each followed by `: text`).

Resume: a recorded terminal blocks resume only while its exact process could
still hold the conversation. Killed processes, reused PIDs, leftover or unreadable
registry files of dead processes and terminals that switched conversation do not.

`api spaces --session ID --expected-generation G --herdr-session SERVER
[--herdr-socket PATH]` lists that server's Spaces for a session not in HERDR:
`{session_id,generation,herdr_session,current_space_id:null,spaces:[{id,name,current:false}],pane:null}`.

`api session add-to-herdr --session ID --expected-generation G --herdr-session
SERVER [--herdr-socket PATH] --space SPACE-ID [--name NAME]` opens a running Claude
background session in a new tab (label NAME, default the session name; a
different NAME also renames the catalog entry) by running `claude attach <job>`
with the account environment. The pane records itself before exec and success
requires the pane's foreground PID to be that recorded `claude attach` client:

```json
{"state":"attached","session_id","generation","host":"herdr","herdr_session","space_id",
 "tab_id","pane_id","target","name","job_id","attachment":{...},"starts_agent":false,
 "sends_input":false}
```

`state:"attach_unconfirmed"` (generation/attachment null) is never retried;
refresh. Refusals: the capability reasons above, `SPACE_NOT_FOUND`, `STALE_TARGET`,
`UNSUPPORTED_PROVIDER_OPERATION` (Codex). `api resume` adds optional `--space
SPACE-ID` (new tab in that existing Space instead of a new Space) and `--name`
(tab label and catalog name). Schemas: `HostSpaceChoices`, `AddToHerdrResult`.

Revision 18: `api resume` (Codex and native) starts the provider in the session's saved
directory, refusing with `SAVED_DIRECTORY_UNAVAILABLE` when it is not an absolute
existing directory. It no longer reports `ok` for an unconfirmed launch: no observed
running session within the poll fails with `RESUME_LAUNCH_UNCONFIRMED`, and a pane
command that runs and then returns to an idle shell fails promptly with
`RESUME_LAUNCH_FAILED`. `move-space` keeps the session label on the new tab, and
restoring an archive keeps a relocated directory. New-session launches are unchanged.

### Batch placement observations (revision 9)

Negotiate `capabilities.placement.version == 1`. `api state` includes each
session's `placement` and a top-level `hosts` catalog. `api placements` returns
all catalog memberships keyed by stable session ID plus the host/Space catalog;
`api session` includes the same placement. No per-session `api spaces` or
`original_host` join is needed. Those older fields/commands remain compatible
launch provenance and move-admission helpers, not exhaustive membership discovery.

One bounded HERDR snapshot per bound host is shared with name discovery. Verified
live catalog attachments, including legacy owner-managed sessions without launch
receipts, supply membership. Pane → tab → Space must agree. Host and Space IDs
are opaque UUIDs persisted under the Sessionsation data root; labels are not keys.
Multiple hosts can have identical labels/native Space IDs without colliding.
Each membership includes `catalog_generation` for joining against the captured
catalog. Placement polling does not itself change lifecycle generations.
Public placement projections contain no socket, filesystem locator or raw pane.

`state` is `observed`, `last_known`, or `unobserved`. `observed_at` and `valid_until`
(30 seconds later) describe the actual evidence, not response time; `checked_at`
records the last attempt. Failed snapshots, changed attachments, expired evidence
and stopped sessions retain the prior placement with a reason, never freshened
by a failed read. Concurrent readers may reuse the last bounded observation
while name synchronization is locked. Placement is not message readiness or move
authorization. Revalidate lifecycle generations through existing APIs.

Host identity is scoped to the persisted explicit HERDR endpoint and server name;
Space identity to that host's native Space ID. Endpoint relocation or replacing a
host while reusing native IDs cannot be inferred as continuity. No network host
probing or caller-focused-Space inference occurs. Previously stopped sessions
never observed by this contract remain `unobserved`; no historical label is guessed.
The host catalog retains last-observed Spaces (including now absent ones), with
per-Space observation times; absence does not prove deletion. Archive filtering in
`api state` is unchanged; `api placements` includes catalog history, so consumers
should join it to their desired state/archive filter by session ID.

Schemas: `Placement`, `PlacementHost`, `Placements` in
[API v1](contract/v1.json); synthetic observed/stopped/outage/unknown examples in
[fixtures](contract/fixtures.json). Clients should accept additive revision changes
within API 1 and negotiate the placement capability rather than pin a package
version equality. Existing observation, delivery and owner protocols are unchanged.

### Gemini provider (revision 13)

Additive; negotiate `contract_revision >= 13`. `api capabilities` `providers`
gains `{"id":"gemini",...}` (same shape as Claude): `enrollment:true`,
`adopt_config_dir:false`, `login:"interactive_exec"`, `usage:false`,
`usage_state:"unsupported"`, `usage_source:"gemini-cli-unsupported"`,
`native_rename:false`, `account_refresh:true`, `account_limits:false`, and
`headless_delivery` (modes `resume`, `new`; `requires_stopped_session`;
`approval_mode:"default"`, never `yolo`). `operations` carries the per-operation
`{supported,reason}` table. `add_to_herdr` gains
`unsupported:{"gemini":"NO_NATIVE_ATTACH","codex":"CODEX_TERMINAL_NOT_MOVABLE"}`;
`session_identity` gains `gemini_process_source:"cmdline_session_id_argument"`.

Accounts use `gemini:NAME` ids. `api account add|login|refresh|rename|remove|restore|
delete` behave as for Claude (`auth_mode.auth_method` is `"oauth-personal"`;
`auth_detail` is `AUTH_REQUIRED` or null; there is no `WRONG_AUTH_MODE`). Account
rows report `usage_state:"unsupported"`, `usage_fresh:false` and `usage` with
`short`/`weekly` null and `state:"unsupported"`: absence of usage is never 0%.
`account limits ... --short-used` is `UNSUPPORTED_PROVIDER_OPERATION`.

Sessions have `provider:"gemini"`, `conversation_id_source:"native"`, `native`
(`kind`/`job_id` null) and the same lifecycle fields as Claude sessions.
`capabilities.add_to_herdr` is false with `capability_reasons.add_to_herdr`
`SESSION_NOT_RUNNING`, `ALREADY_IN_HERDR` or `NO_NATIVE_ATTACH`; `api session
add-to-herdr` for Gemini fails with `UNSUPPORTED_PROVIDER_OPERATION: NO_NATIVE_ATTACH...`.
`api resume --space/--name` and `launcher` work. `api send` mode `resume`/`new`
accepts `gemini:` sessions/accounts; receipts carry `provider:"gemini"` and
`account_id:"gemini:NAME"` (schemas `ClaudeHeadlessRequest`/`ClaudeHeadlessReceipt`
admit both providers). Records of both providers share
`DATA_ROOT/messaging/claude-deliveries/`; older records without a `provider` field are Claude.
Login and headless delivery are verified only against fakes until a real login exists.

### Account log out (revision 14)

Additive; negotiate `contract_revision >= 14`. Every `providers[]` entry of `api
capabilities` gains `account_logout:true`, and `operations.account_logout` is
`{supported:true,reason:<method text>}`. `api account logout <provider:name>`
returns

```json
{"account_id":"claude:work","provider":"claude","logged_out":true,"auth_required":true,"method":"claude auth logout"}
```

`method` names the mechanism: Codex `account/logout` of the account's app-server
(or `codex logout` when none runs), Claude `claude auth logout`, Gemini the
credential clearing of gemini-cli's `clearCachedCredentialFile` (no logout
command exists in 0.66.0). Errors: `ACCOUNT_IN_USE` (live session or provider
process, or held Codex lease), `ACCOUNT_NOT_FOUND` (unknown or deleted),
`ACCOUNT_REMOVED`, `ACCOUNT_PROVIDER_MISMATCH`, `ACCOUNT_DELETION_IN_PROGRESS`,
`LOGOUT_INCOMPLETE` (the provider still reports a login). The call is idempotent.
Afterwards the account row has `auth_required:true`, `selectable:false`, and the
supervisor's probe agrees; `api account login` returns the plan. The supervisor
records event `account.logged_out` when the stored state changed.

Also in revision 14: `usage.observed_at` is `null` (never `0`) whenever the
account has no real usage observation (Gemini always; Codex and Claude until
first observed).

### Cached state (revision 14)

`api state --cached [--archive-filter F]` serves the last full result of the same
filter with the identical JSON shape plus `snapshot: {observed_at, fresh, cached}`;
`fresh` is true while the snapshot is at most 15 seconds old. It performs no
transcript scan, process probe or catalog write (`capabilities` is current).
Every full `api state` stores its result atomically as
`DATA_ROOT/cache/state-snapshot-<filter>.json` (and reports `snapshot.cached:false`);
there is no second writer. With no snapshot yet, `--cached` computes a full state.
`api capabilities` advertises `state_cached:true`. Clients should refresh with a full
`state` periodically and render `--cached` between refreshes; a cached read never
reflects a session started after the snapshot.

Full `state` also runs inventory and the supervisor status concurrently, takes one
`/proc` pass for all Gemini accounts, and resolves deleted-account checks once per
account. Identity and merge semantics are unchanged.

### Account order (revision 14)

Accounts have a persistent order shared by the CLI and UI: field `order` (0-based,
dense) on each account row; `account list` and `state` accounts are sorted by it.
Existing accounts get today's alphabetical order once (event
`account.order_migrated`, idempotent); a new account is appended last. `api account
reorder <provider:name> --position N` (human: `account reorder NAME --position N`)
moves it and renumbers: `{"account_id","provider","position","changed","order":[ids]}`,
event `account.reordered` only when the order changed; `N` beyond the last position
is `INVALID_POSITION`. `api capabilities` advertises top-level `account_reorder:true`.

### Steer reply correlation (revision 15)

Additive; negotiate `contract_revision >= 15` and
`capabilities.steering.reply_correlation == "after_steer_item"`
(`steering.reply_observation` publishes the bounds). Requests are unchanged.
Steering receipts, `api steer-target` and readiness `actions.steer` now report
`acceptance_only:false` and `reply_correlation:"after_steer_item"`.

Native behavior (verified live on Codex 0.165.0): `turn/steer` carries
`clientUserMessageId` = the request `message_id`, and the steered turn's full
item history then contains a `userMessage` whose `clientId` is that ID, placed
after the input that started the turn. Assistant items produced after it follow
it in the same turn. An assistant item that was still streaming when the steer
arrived can be discarded natively and never appear in history. The summary items
view omits steer inputs, so only the full view of the single steered turn is read.

- **Anchor.** Immediately before `turn/steer`, Sessionsation reads that turn
  (`thread/turns/list`, `itemsView:full`, `limit:1`) and records item identifiers
  only: the count of earlier user inputs and items, and the last assistant item ID.
  The turn must still be latest and in progress and must not already contain the
  ID, otherwise the steer is rejected unsent (`active_turn_changed`,
  `ambiguous_native_correlation`).
- **Read bound.** Anchor and observation reads accept one native frame of up to
  64 MiB (a long turn's full items include tool output; 1.86 MB was observed live).
  Only item identifiers, client IDs and assistant text are retained while parsing.
  A larger frame fails closed with `native_frame_limit` before `turn/steer`, so the
  steer stays provably unsent; more than 8192 items is `steer_turn_too_large`.
- **Observation.** `api receipt` on a `native_accepted`, `delivery_unknown` or
  `execution_started` steer reads the same turn newest-first (at most 8 one-turn
  pages, 8192 items) and locates the steer input by `clientId`. Only item IDs,
  client IDs and assistant text are read; user text, reasoning and tool output are
  skipped. It never resends and does not hold the writer lock during the read.
- **Attribution.** The reply window starts after the steer input and ends at the
  next user input (a later steer) or the end of the turn. `reply` is the last
  `final_answer` assistant item in that window, otherwise the last non-empty one.
  When the window runs to the end of the turn the same text also answers the turn's
  earlier input: `facts.shared_with_turn_tail:true`. A later steer bounds the window
  (`correlation.next_input_item_id`).
- **States.** `execution_started` (input recorded, turn in progress, `reply` may be
  a partial with `final:false`); `completed_with_reply` (turn completed;
  `reply.final:true`); `completed_without_reply`; `failed` with reason
  `steer_turn_interrupted` (turn interrupted or failed, partial reply kept) or
  `input_not_consumed` (acknowledged, but the turn ended without the steer input).
  An unacknowledged (`delivery_unknown`) steer only gains positive evidence; it never
  becomes `input_not_consumed`. Terminal states are cached and never regressed.
- **Re-verification.** Every read re-checks the anchor: the steer input must keep
  its item ID, appear after at least as many user inputs as at steer time, and the
  recorded pre-steer assistant item must not appear after it. A recorded input that
  disappears (for example after a history rewrite around compaction) reports
  `observation.error_code:"steer_anchor_lost"` and keeps the stored evidence. The
  ID appearing in a different turn reports `correlation_changed`. Ambiguous or
  oversized evidence (`ambiguous_native_correlation`, `reply_too_large`) is never
  attributed.

New receipt fields: `facts.turn_status`, `facts.reply_final`,
`facts.shared_with_turn_tail`; `facts.input_recorded`, `input_consumed`
(`true`, `false` or `"unknown"`) and `turn_completed` now carry observed values.
`correlation` is `{native_turn_id, steer_input_item_id, next_input_item_id,
reply_item_id, observed_runtime_generation, anchor}` once the input is recorded;
`reply` is `{item_id, text, phase, final, shared_with_turn_tail}`. `observation.state`
is `acceptance_only` (send result or non-observable), `observed`, `uncorrelated`,
`unavailable` (with `error_code`) or `cached`. Receipts made before revision 15 have
no anchor and are still correlated by `clientId` without the anchor checks. Fixtures:
`receipt_steer_progress`, `receipt_steer_reply`, `receipt_steer_interrupted`,
`receipt_steer_not_consumed`. The account switcher's pause steer now also sends a
`clientUserMessageId`.

Revision 18: Codex renumbers legacy `item-N` ids when it rebuilds a finished
turn. As for submit, a steer input first observed in progress as one legacy id
and found under another legacy id in the completed, interrupted or failed turn
(same `clientId`, still exactly once) is reconciled: `steer_input_item_id` keeps
the in-progress id and the optional `correlation.completion_input_item_id` names
the finished-turn id. The anchor's legacy `last_agent_item_id` is then not
compared (it names a different item after renumbering); the earlier-input count
still is. Any other id change remains `correlation_changed`. Receipts left at
`execution_started` by that error progress on their next poll. Full-turn reads
scale their time budget with the 64 MiB frame bound (base plus about one second
per 16 MiB).

### Claude steering, hosted sessions and live peer messages (revision 16)

Additive; negotiate `contract_revision >= 16` and `capabilities.claude_input_delivery`
(`version:1`; the Claude provider entry repeats it as `input_delivery` and now
reports `submit`, `queue` and `steer` true with `operations.*.reason` naming the
conditions). Gate each send on the session row: `capabilities.submit|queue|steer`
are true only for a **hosted** session whose worker is running, or a **live**
interactive session of an account with live steering enabled and a verified
messaging socket; otherwise false with `capability_reasons` (`SESSION_NOT_RUNNING`,
`LIVE_STEERING_DISABLED`, `PEER_ENDPOINT_UNAVAILABLE`,
`SESSION_OPERATION_IN_PROGRESS`). Rows add `input_delivery`
(`{semantics:"hosted_stream_json"|"peer_message"|null, interrupt, label}`) and
`hosted` (the `HostedSession` object or null); a hosted session reports
`availability:"running"` and `resume:false`. Gemini is unchanged.

Request (`ClaudeSteerRequest`): the ordinary `api send` fields with
`mode:"submit"|"queue"|"steer"`, optional `timeout_seconds` (peer reply wait,
default 600, max 3600) and, for steer only, `"steering":{"version":1,"interrupt":bool}`
(a Codex `expected_turn_id` target is `INVALID_REQUEST`). Priorities: submit `next`,
queue `later`, steer `next`. There is no idle preflight: the provider orders inputs.
Claude Code 2.1.312 folds a `next` input into a running turn at its next tool round
(or runs it as the next turn when the turn ends without one) and never cuts the
turn for it; `later` runs as its own later turn; `now` would **cancel** the running
turn, so Sessionsation never sends it. A steer therefore never interrupts:
`claude_input_delivery.steer` and each session row's `input_delivery.steer`
(`ClaudeSteerSemantics`, null when the row takes no input) report
`{priority:"next",interrupts:false,delivery:"next_tool_boundary_or_next_turn",
interrupt_option:"steering.interrupt",interrupted_inputs_state:"interrupted"}`.
Only `steering.interrupt:true` cancels the running turn (its inputs end
`interrupted`) before the steer runs as a new turn.
Admission refuses without journaling on `STALE_TARGET`, `SESSION_NOT_RUNNING` (use
mode `resume`), `LIVE_STEERING_DISABLED`, `PEER_ENDPOINT_UNAVAILABLE`,
`PEER_INTERRUPT_UNSUPPORTED`, `SESSION_OPERATION_IN_PROGRESS`, account/auth errors and
`INVALID_EXPIRY`. Message ids are shared with every other store; same id and
payload return the original receipt, a change is `MESSAGE_ID_CONFLICT`.

Revision 17 also adds the capability/admission reason
`PEER_SOCKET_UNLINKED`: the live registry names a `messagingSocketPath` but the
socket file is missing (typically elogind removed `/run/user/<uid>`); the reason text
adds that the process still listens when `/proc/net/unix` and its fds show it.
Resume or restart the session to rebind. `PEER_ENDPOINT_UNAVAILABLE` remains for
genuinely absent or unusable endpoints. Every Claude child sessionsation launches
now runs without `XDG_RUNTIME_DIR`, so its socket lives under `/tmp/cc-socks`.

Revision 18: reason honesty. A second operation on a session that already has a queued
or running one fails with `SESSION_OPERATION_IN_PROGRESS` (retryable; the message still
names the queued job id) instead of `OPERATION_FAILED`. Every false Codex capability
now has a `capability_reasons` code: `SESSION_NOT_RUNNING`, `AMBIGUOUS_SESSION`,
`ACCOUNT_UNKNOWN`, `NOT_IN_HERDR`, `SESSION_ALREADY_RUNNING`,
`SESSION_RUNNING_STOP_FIRST`, `HISTORY_UNAVAILABLE`, `SESSION_ARCHIVED`,
`UNSUPPORTED_PROVIDER_OPERATION`, `SESSION_OPERATION_IN_PROGRESS` (a pending move).
Hosted Claude rows give `stop` and `restart` the reason `HOSTED_SESSION` (stop with
`api hosted stop`) and `add_to_herdr` a hosted-specific `HOSTED_SESSION` reason, not the
interactive-terminal ones. A launched Claude process that has not yet registered or
written a transcript (for example on its trust prompt) reports `availability:"running"`
with the discovery warning `NO_CONVERSATION_YET`; no conversation is invented. Peer
endpoint failures keep their own code (`PEER_SOCKET_UNLINKED`, ...), falling back to
`PEER_ENDPOINT_UNAVAILABLE` with the message; when `api send` (delivery auto) finds neither path usable, the
`LIVE_STEERING_DISABLED` message also names why terminal input is ineligible.

Revision 18: Gemini authentication failures. Account rows add `auth_failed`,
`auth_failure_reason` (`AUTH_CLIENT_UNSUPPORTED`, `AUTH_TIER_INELIGIBLE`,
`AUTH_GRANT_INVALID`, `AUTH_UNAUTHENTICATED`, `AUTH_PERMISSION_DENIED`,
`AUTH_REAUTH_REQUIRED` or `AUTH_FAILED`, with fixed text) and
`auth_failure_observed_at`. They come from the last recognizable failure in a headless
Gemini run's stderr/JSON error (known messages such as Google's "client is no longer
supported" tier rejection, or the CLI's fatal-authentication exit status 41; a network
error never counts) or the `account login` terminal, stored in a
sessionsation-owned file outside the account home (reason code, timestamp and the
credential file's stat identity only; never raw output or credential content). While
set, the row also reports `auth_required:true`, `selectable:false` (a login is needed
although credential files exist) and `auth_detail:"AUTH_FAILED"`. A generic crash never
sets it; a later successful headless run or successful login clears it, and a newer
credential file supersedes it. Interactive Gemini launches are not observed.
`api state --cached` re-reads this local record for Gemini rows, so a failure recorded
or cleared after the snapshot shows at once. A headless Gemini delivery whose output
is unparseable after the CLI failed while establishing its sign-in, with the
conversation file unchanged, ends `rejected` (a definite refusal: nothing reached the
model) with reason `<CODE>: <fixed text>; nothing was sent to the model`; any other
unparseable outcome stays `delivery_unknown`. `api resume` (with or without
`--space`) labels the new tab with the session's current name, so the resumed session
keeps it.

Receipt (`ClaudeSteerReceipt`, also from `api receipt`): `delivery_semantics`,
`priority`, `state`, `reason`, `facts` (`written`, `accepted_native`,
`input_recorded`, `input_consumed`, `reply_observed`, `reply_final`, `folded`,
`shared_with_turn`, `interrupt_requested`, `lifecycle[]`, `held`, `peer_status[]`,
`idle_notice`), `correlation`, `reply` (`{text,text_truncated,final,shared_with_turn}`
plus the hosted `result` fields `is_error,subtype,num_turns,duration_ms,usage`),
`execution` (`phase` admitted/dispatching/written/complete, `host_epoch`,
`automatic_replay:false`) and `observation`. States: `pending_local`, `held`,
`native_accepted`, `execution_started`, `completed_with_reply`,
`completed_without_reply`, `failed`, `interrupted`, `cancelled`, `refused`, `rejected`,
`expired`, `delivery_unknown`. Terminal states are never regressed. Only
`completed_*` carries `reply.final:true`/`facts.reply_final:true`; every other state
keeps any partial text with `final:false`.

- **Hosted** (`hosted_stream_json`). The record is journaled, then the worker
  marks it `dispatching` before writing the stdin frame (`uuid` = `message_id`);
  a request the worker never claimed becomes `rejected` (`HOST_UNREACHABLE_NOT_SENT`,
  `HOST_STOPPED_BEFORE_DELIVERY`). `command_lifecycle` `queued` → `native_accepted`,
  `started` → `execution_started`; `cancelled`/`discarded` before `started` →
  `cancelled` (`NATIVE_CANCELLED`/`NATIVE_DISCARDED`), `refused` → `refused`. The
  terminal state needs both the `result` whose `user_message_uuids` contains the id
  (all started inputs when absent) and the id's terminal lifecycle, in either order
  (a fresh turn's `completed` follows its result, a folded input's precedes it):
  `completed` → `completed_with_reply` / `completed_without_reply`, or `failed`
  (`PROVIDER_REPORTED_ERROR`) for an error result. `cancelled`/`discarded` after
  `started` means the consuming turn was cut: Claude Code still emits a `success`
  result with the partial text, so the input ends `interrupted` (`TURN_INTERRUPTED`
  after a requested interrupt, else `TURN_CANCELLED_BY_PROVIDER`; `failed` with
  `PROVIDER_REPORTED_ERROR` when the result was an error), the partial reply kept with
  `final:false`, never replayed. Another reported `session_id` is
  `delivery_unknown` (`SESSION_ID_MISMATCH`). If a later turn starts first, a
  result without a terminal lifecycle is settled from the result; if the provider
  exits first it is `delivery_unknown` (`…_BEFORE_TURN_OUTCOME`). `correlation` is
  `{kind:"command_uuid",command_uuid,lifecycle,user_message_uuids}`. A provider or
  worker exit makes in-flight messages `failed` (`*_DURING_TURN`, turn started) or
  `delivery_unknown` (`PROVIDER_EXITED…`, `HOST_WORKER_EXITED`, `STDIN_WRITE_FAILED`).
- **Peer message** (`peer_message`, with `peer_message:{label,status}`). Status
  frames: `held` → `held`, `delivered` → `native_accepted`, `denied`/`refused`/`dropped`
  → `refused` (`PEER_MESSAGE_*`), `expired` → `expired`. `correlation` is
  `{kind:"transcript_anchor",message_id,anchor_found,anchor_offset,turn_ended,notify_msg_id}`.
  The reply is the assistant text after the transcript record that carries the
  message id, until the next input; it is final after that input or the idle
  notice. Without the anchor, `reply` stays null and `observation.error_code` is
  `peer_anchor_not_found`: delivered is never reported as answered. A connection
  that never opened is `rejected` (`PEER_SOCKET_UNREACHABLE`); a failed write is
  `delivery_unknown`.

`api cancel` cancels only a request still `admitted`; afterwards it returns
`STEERING_CANCELLATION_UNSUPPORTED` (interrupt a hosted turn with `api hosted stop
--interrupt` or a new steer with `interrupt:true`).

Hosted sessions (`HostedSession`): `api hosted start` (`--account claude:N
--directory DIR [--name]` or `--session ID --expected-generation G`,
`--permission-mode account|default|acceptEdits|plan|bypassPermissions`), `status`,
`events`, `stop` and `respond`; `api launch start` accepts `"hosted":{"permission_mode"}`
for `claude:` accounts and `api launcher new|resume ... --hosted` return a
`run --hosted` plan. Fields: `state` (`starting|running|stopping|stopped|failed`),
`reason` (`STOPPED`, `WORKER_LOST`, `WORKER_LOST_PROVIDER_DRAINING`,
`PROVIDER_EXITED[_CODE]`, …), `permission_mode`, `effective_permission_mode` (never
`bypassPermissions` for `account`), `epoch`, `busy`, `worker`, `provider_process`,
`pending_permissions[]` (`request_id`, `tool_name`, `input`, `input_preview`,
`requested_at`), `viewer_argv` and `take_over_argv`. A worker, or a provider it
started that is still running, blocks interactive/headless resume and a second
host (`SESSION_ALREADY_RUNNING`). Account removal lists live hosts as
`hosted_session` activity.

Live steering opt-in (`LiveSteering`): `api account live-steering claude:NAME
[--enable|--disable]` returns `{account_id,provider,live_steering,setting:
"crossSessionInbound",value,previous,changed,settings_file}`; events
`account.live_steering_enabled|disabled` are recorded only on change. Fixtures:
`claude_steer_request`, `claude_steer_receipt_hosted`,
`claude_steer_receipt_interrupted`, `claude_steer_receipt_cut`, `claude_steer_semantics`, `claude_steer_receipt_peer`,
`claude_steer_receipt_peer_unanchored`, `claude_steer_receipt_held`,
`hosted_session`, `live_steering`, `launch_request_hosted`. Both paths are verified
against fakes only; see [CLI](cli.md#hosted-claude-sessions-and-steering-revision-16).

### Claude and Gemini archive, restore and delete (revision 17)

Additive; negotiate `contract_revision >= 17`. Claude and Gemini provider entries
now report `operations.archive` and `operations.delete_session` as
`supported:true`, and their reasons name the per-session conditions.
`conversation_archive` adds `providers:["codex","claude","gemini"]` and `native`
(`NativeConversationArchive`): `version:1`, `since_revision:17`,
`storage:"moved_native_files"`, `compression:"none"`,
`restore_target:"original_account_home"`,
`scope:"exact_conversation_ids_including_previous_conversation_ids"`,
`delete_removes_catalog_entry:true`, `delete_removes_merged_aliases:true`,
`delete_staging:"deletions"`, `dead_registry_entries_deleted:true`,
`credentials_touched:false`, `session_capabilities` and `refusal_reasons`.

The commands, request fields, operation records (`api operations`, kinds
`archive_session|restore_session|delete_session`) and the no-replay rules are the
[Codex ones](#conversation-archives). A merged id fails with `SESSION_MERGED`
naming its survivor. Archive and delete act on the survivor's whole lineage, meaning its
`conversation_id` and every `previous_conversation_ids` member. Native session rows carry
`capabilities.archive|delete|restore`. Their `capability_reasons` are:

- `archive`/`delete`: `SESSION_RUNNING` (a verified registry entry or process
  names a lineage conversation), `SESSION_HOSTED` (a live Sessionsation-hosted
  worker), `SESSION_STATE_UNKNOWN` (the live probe failed), `ACCOUNT_DELETED`, or
  for `archive` only `HISTORY_UNAVAILABLE` (no transcript; delete still removes the
  entry). Otherwise null. `restore` is false with `SESSION_NOT_ARCHIVED`.
- Archived rows: `availability:"archived"`, `archived:true`, `archive.kind:"managed"`
  (`storage:"moved_native_files"`), `resume_block_reason:"archived"`, and every
  capability false except `restore` and `delete`.

Admission and the worker both refuse with `SESSION_RUNNING`, `SESSION_HOSTED`,
`SESSION_STATE_UNKNOWN`, `SESSION_ALREADY_ARCHIVED` or `SESSION_NOT_ARCHIVED` (Codex
keeps `SESSION_ACTIVE`). The worker also refuses with `HISTORY_IN_USE` (open
writer), `HISTORY_SYMLINK`, `HISTORY_CHANGED`, `RESTORE_CONFLICT` (an original path
exists again) and `ACCOUNT_HOME_CHANGED`. Archive moves the files into Sessionsation
storage, and Claude Code or Gemini CLI then neither lists nor resumes the conversation.
Restore moves the identical files back to the same account-home paths. Delete is
permanent: it removes the files, Claude `sessions/PID.json` entries of dead
processes that name the conversation, the catalog entry and the merged aliases that resolved to it, so
the id then fails with `SESSION_NOT_FOUND`. `api archives` lists native entries with
`compressed_bytes:null`, `compression:"none"` and `previous_conversation_ids`.
Fixtures: `archived_session_claude`, `native_conversation_archive`. File scope and
storage are in the [CLI reference](cli.md#claude-and-gemini-archive-and-delete-revision-17).

### Claude terminal input (revision 17)

Additive in revision 17. `capabilities.claude_input_delivery` adds `terminal_input`
(`ClaudeTerminalInputDelivery`), `preference:["hosted_stream_json","terminal_input","peer_message"]`
and `delivery_request`. A third delivery path, `delivery_semantics:"terminal_input"`,
types the text into the session's HERDR pane as the user's own keystrokes. Claude
Code records it as a human turn: a `user` record when idle, or a `queued_command`
attachment with `origin.kind:"human"` mid-turn, read at the next tool round like
a `next` input. The peer path is unchanged; its text arrives as a message from
another session, which Agent-DM labels a weak steer.

Eligibility: a catalog session (launched or tracked by Sessionsation) that is
running and not hosted, whose `attachment` is a verified HERDR pane
(`attachment.host:"herdr"` with the exact PID and start time of the running Claude
process). A background session (`native.kind:"bg"`) qualifies through its
`claude attach <job>` client instead: the attachment must be that exact client
process (PID, start time and `attach <native.job_id>` argv), and at admission
and before the first key HERDR must report it as the pane's only foreground
process. Keys typed there reach the background session and land in its
transcript. Otherwise terminal input is unavailable with `TERMINAL_NOT_ATTACHED`.
The operator switch `terminal_input_enabled` in `DATA_ROOT/config.json`
(default `true`) turns terminal input off for Claude and Gemini: rows report
`terminal_input:{available:false,reason:"TERMINAL_INPUT_DISABLED"}`, requests
naming `terminal_input` are refused with that code (an `auto` request uses
another path when one applies), and an admitted request still unsent when the
switch goes off ends `rejected` with it.
Only `submit` and `steer` are typed. `queue` (`later`) cannot be typed
(`TERMINAL_QUEUE_UNSUPPORTED`). `steering.interrupt` is refused
(`TERMINAL_INTERRUPT_UNSUPPORTED`): typed input never sends Esc or Ctrl+C.

Path choice. Requests take an optional `delivery`: `auto` (default),
`hosted_stream_json`, `terminal_input` or `peer_message`. With `auto`, a running
hosted worker wins. Otherwise submit and steer use `terminal_input` when the row is
attached and `peer_message` otherwise, and queue uses `peer_message`. A named path
that does not apply is refused (`DELIVERY_UNAVAILABLE`, `TERMINAL_NOT_ATTACHED`,
`LIVE_STEERING_DISABLED`, ...), never silently replaced. Session rows extend
`input_delivery` with `modes` (`{submit,queue,steer}` → path or null),
`available` (every applicable path, in preference order) and
`terminal_input:{available,reason}`. `semantics`, `label` and `steer` describe
the submit/steer path. A terminal row's `steer` has `interrupt_option:null`, and
`capability_reasons.queue` is `TERMINAL_QUEUE_UNSUPPORTED` without live steering.

The text must be typeable verbatim, or admission refuses it without journaling.
Tabs and control characters are `TERMINAL_PAYLOAD_UNSUPPORTED`. So are a trailing
backslash (Enter would become a newline) and a last token starting with `@` or `/`
(Enter would accept a completion). The limit is 8192 bytes after trimming
(`TERMINAL_TEXT_TOO_LONG`). CRLF becomes LF and surrounding whitespace is trimmed
(`text_normalized`). A leading `/`, `!`, `#` or `?` gets a backslash
(`escaped_prefix`), so command, shell, memory and help triggers stay plain text.
The receipt correlates on that delivered text.

Delivery is journaled under `DATA_ROOT/messaging/claude-steer/` before every write, one
request per pane at a time. Procedure, with codes ending the request `rejected`
before anything is submitted:

1. `herdr agent list` must show a Claude agent on the pane (`TERMINAL_AGENT_UNKNOWN`)
   that is not `blocked` (`TERMINAL_AGENT_BLOCKED`). The agent is checked again
   before the first destructive key and before Enter.
2. `pane read --format ansi` must show the main prompt with no overlay. The
   detailed transcript view is left with ctrl+o and the shortcut help with `?` on
   the empty prompt; the receipt then has `view_switched`. The input box is the
   last `❯` in column 0 directly under a plain rule, closed by the next rule. The
   pointer's colour is not a marker: Claude Code draws it grey while a turn runs,
   so idle and busy main prompts are read alike. A viewed agent (its name as a
   pill on a coloured background in the top rule, a `Message @name…` placeholder,
   or `◯ main` in the agent list; a background session's own name in that rule is
   plain text) is `TERMINAL_VIEW_NOT_MAIN`. No key returns to the main view
   safely: Esc there stops a running subagent. Other refusals:
   `TERMINAL_VIM_MODE`, `TERMINAL_SHELL_MODE`, `TERMINAL_OVERLAY_UNIDENTIFIED`
   (a command or `@` completion menu drawn above the box, a focused footer item
   or agent list, pickers, dialogs) and `TERMINAL_VIEW_UNDETERMINED`.
3. The draft in the input box must be reconstructable exactly. Paste or image
   placeholders, a row reaching the right edge (possible soft wrap), more than 12
   rows and dim text mixed with typed text are `TERMINAL_DRAFT_UNRECONSTRUCTABLE`.
   A draft starting with `?`, `!`, `#` or `/`, or ending in an `@` token, cannot
   be typed back as text (`TERMINAL_DRAFT_UNRESTORABLE`). The draft is then journaled privately; it
   never appears in receipts.
4. The input is cleared with ctrl+k/ctrl+u pairs and verified empty
   (`TERMINAL_CLEAR_UNVERIFIED`, draft kept in the journal).
5. The text is typed with `pane send-text` (no bracketed paste, so Claude never collapses it to a `[Pasted text]` placeholder) in small chunks,
   with ctrl+j between lines, and checked on screen. On a mismatch or a block it
   is withdrawn and the draft restored (`TERMINAL_TYPED_TEXT_MISMATCH`). Enter is
   journaled (`steps` gains `submit`) and sent once. A failed Enter is
   `delivery_unknown` (`TERMINAL_SUBMIT_UNCERTAIN`).
6. Enter empties the input box whether it starts a turn (idle) or queues the
   text (busy). Once the box is seen empty the draft is typed back unsubmitted,
   also while the turn runs (typed text stays in the box), and verified with
   `pane read` (`terminal_input.draft.restored`, `restore_error`). Every exit
   after the clear either restores the draft or reports what the box holds:
   `draft.left_in_input` (`draft`, `empty`, `message`, `other`, `unknown`) and
   `left_chars`; the draft then stays in the private journal. A HERDR failure
   after the clear gets one best-effort restore onto a box seen empty (or still
   holding the unsent message). A failed restore is reported, never retried.
7. The transcript after the submit is searched for the human record carrying
   exactly the delivered text with a timestamp at or after the submit. Its `uuid`
   is `correlation.input_uuid` (`{kind:"transcript_record",input_uuid,record_type,
   anchor_offset,turn_ended}`), and only it anchors `reply`. Without it within
   60 seconds (or `timeout_seconds` if shorter) the receipt is `delivery_unknown`
   (`TERMINAL_INPUT_UNCONFIRMED`). The turn ends at the next real input, or when
   HERDR reports the agent working and then idle. Nothing is re-sent automatically.

Revision 18: step 3's draft is read once more immediately before the first
ctrl+k/ctrl+u. If the input box no longer holds exactly that draft (the user typed
in between), the request is rejected with `TERMINAL_DRAFT_CHANGED` and no key or
text is sent. Deleting a session also removes the now-empty Claude project
directory of its own conversation (never a non-empty or unrelated one), and the
deleted-session manifest keeps ids only (no name, directory, pane or home path);
a completed Codex delete removes its session-catalog entry and recovery journal.

Other codes: `TERMINAL_HOST_UNAVAILABLE`, `TERMINAL_HOST_FAILED_BEFORE_SUBMIT`
and `TERMINAL_WORKER_EXITED_BEFORE_SUBMIT`. In those cases a draft whose restore
was not verified stays in the journal. Receipts add `terminal_input`
(`ClaudeTerminalInput`): `label`, `target`, `escaped_prefix`, `text_normalized`,
`view_switched`, `draft:{present,chars,restored,restore_error,left_in_input,left_chars}` and `steps`.
Fixtures: `claude_steer_request_terminal`, `claude_steer_receipt_terminal`,
`claude_steer_receipt_terminal_unconfirmed`,
`claude_steer_receipt_terminal_refused`. The screen grammar and keys come from
the Claude Code 2.1.312 bundle and HERDR 0.9.5; the markers are checked
against real 2.1.312 screens (`tests/fixtures/claude-screens/`, redacted), which
the fake HERDR used by the tests replays (see
[CLI](cli.md#claude-terminal-input-revision-17)).

### Gemini terminal input (revision 17)

Additive in revision 17. Gemini sessions take `api send` modes `submit`, `queue`
and `steer` (the `ClaudeSteerRequest` shape; `delivery` `auto` or
`terminal_input`, anything else `DELIVERY_UNAVAILABLE`) as typed input into the
session's verified HERDR pane, through the same journal, procedure and refusal
codes as [Claude terminal input](#claude-terminal-input-revision-17). The
provider entry reports `submit`, `queue` and `steer` true with
`input_delivery`, and `capabilities.gemini_input_delivery`
(`GeminiTerminalInputDelivery`) describes the path. Gemini rows fill
`capabilities.submit|queue|steer`, `capability_reasons` and `input_delivery`
(`semantics`, `label`, `steer` as `GeminiSteerSemantics`, `modes`, `available`,
`terminal_input:{available,reason}`, `model_steering:{enabled,configured,source,
restart_required,reason}`).

Enter in Gemini CLI 0.66.0 means: a user turn when idle; mid-turn, the native
message queue with `experimental.modelSteering` off, or a user steering update
with it on. Hence `queue` needs the setting off (`GEMINI_QUEUE_WOULD_STEER`),
`steer` needs it on (`GEMINI_STEERING_DISABLED`), and a mid-turn `submit` is
refused when it is on (`GEMINI_SUBMIT_WOULD_STEER`). The CLI reads the setting at
startup only; a settings file changed after the process started makes it
`GEMINI_STEERING_RESTART_REQUIRED`, an unreadable or trust-dependent value
`GEMINI_STEERING_UNKNOWN`. The admission and the worker (before the first key and
again before Enter) check these.

Receipts (`GeminiSteerReceipt`: a `ClaudeSteerReceipt` with `provider:"gemini"`,
`account_id` `gemini:NAME` and `delivery_semantics:"terminal_input"`) add
`gemini_input` (`GeminiTerminalInput`): `model_steering`, `steering_source`,
`busy_at_submit`, `expected` (`user_turn`, `native_queue`, `steering_update`),
`screen_evidence` (`queued` or `steering_hint` seen right after a mid-turn Enter;
then `facts.accepted_native` is true and the state `native_accepted`), `outcome`
and `steering:{applied,fate}`. The anchor is the first `user` chat record at or
after the submit whose text contains the typed text:

- `submitted` / `queued`: a prompt carrying the text (`record_type:"user"`). A
  queued message shares its record with every other message queued during that
  turn (joined with a blank line); `batched` is then true and
  `facts.shared_with_turn` true.
- `steering_applied`: a record that carries the steering update with the next
  tool response (`record_type:"steering_update"`, `steering.applied:true`,
  `fate:"applied_at_next_tool_step"`, `facts.folded` true).
- `steering_after_turn`: the turn ended without a tool call, and Gemini submitted
  the update alone as a new prompt once idle (`steering.applied:false`,
  `fate:"submitted_after_turn_as_steering_prompt"`). It is not dropped; it is
  lost only if the process exits before it is idle again, which leaves the
  receipt `delivery_unknown`.

The reply is Gemini's text after the anchor until the next user prompt (tool
responses continue the turn). A mid-turn Enter that Gemini visibly queued or took
as a hint may stay unrecorded until the turn reaches it, so its confirmation
window is `timeout_seconds`; otherwise 60 seconds, as for Claude. Nothing is
re-sent. Never sent: Esc and Ctrl+C (both cancel a running Gemini request),
Ctrl+D, Ctrl+L.

`api account live-steering gemini:NAME [--enable|--disable]` returns
`GeminiLiveSteering`: it sets or removes only `experimental.modelSteering` in
`GEMINI_CLI_HOME/.gemini/settings.json` (atomic replace, mode kept, symlinks and
commented files refused), with `applies_to:"sessions_started_after_the_change"`,
`restart_required` and `running_sessions`. Fixtures: `gemini_steer_request`,
`gemini_steer_receipt_queued`, `gemini_steer_receipt_steer_applied`,
`gemini_steer_receipt_steer_after_turn`, `gemini_steer_receipt_refused`,
`gemini_live_steering`. Grammar and keys come from the Gemini CLI 0.66.0 bundle;
tests use a fake HERDR pane (see [CLI](cli.md#gemini-terminal-input-revision-17)).

### Receipt retention (revision 18)

Additive; negotiate `contract_revision >= 18` (`api capabilities.receipt_retention`).
Each receipt store (Codex messaging ledger, Claude/Gemini input journal, headless
deliveries) holds at most 10,000 receipts. Without retention every send would
eventually fail with `RECEIPT_CAPACITY_REACHED`, so a retention pass runs at most
once per hour during send admission, under the exclusive messaging lock:

- A certainly terminal receipt (`completed_with_reply`, `completed_without_reply`,
  `failed`, `rejected`, `cancelled`, `expired`) is retired once it is older than
  `receipt_retention_days` (default 14). Age is counted from the submission deadline
  (`expires_at`) for Codex receipts and from the last record update for journals.
- A non-terminal or uncertain receipt (`pending_local`, `native_accepted`,
  `execution_started`, `delivery_unknown`) is never retired before
  `receipt_unknown_horizon_days` (default 90). Past that horizon it is first saved
  as terminal `expired` with reason `outcome_unknown_at_retention_horizon` (a
  provably unsent `pending_local` becomes `expired` without that reason), then
  retired. That reason means Sessionsation stopped observing: the native outcome
  is unknown, and the message must never be resent.
- A Codex binding that never produced a receipt is retired as `cancelled` (when
  cancelled) or, past the horizon, `expired`. Recovery journals are not retired.
- Unreadable records are kept.

Every retired id keeps a compact tombstone (id, final state, reason, retirement
time; no text, target or reply). Tombstones are not counted toward capacity and
are not pruned. A retired id is never accepted again by any delivery path, with
the same or different content: `api send` and `api receipt` return
`RECEIPT_RETIRED` with the final state in the message (legacy messaging:
`receipt_retired`). Clients must keep their own copy of a reply they need beyond
the retention window. Both settings are `sessionsation config set` keys
(1..=3650 days; the horizon must not be shorter than the retention).

### Launch permissions (revision 19)

Additive; negotiate `contract_revision >= 19` (`api capabilities.launch_permissions`:
`values` `["yolo","default"]`, `built_in_default:"yolo"`, `setting:"launch_permissions"`,
`request_field:"permissions"`, `requests:["launch","resume-launch"]`,
`cli_option:"--permissions"`). `LaunchRequest` and `ResumeRequest` accept an optional
`permissions` (`LaunchPermissions`: `yolo` | `default`); omitted, the operator's
`launch_permissions` setting applies (default `yolo`). Requests without the field are
unchanged in shape; older backends reject it as an unknown field, so send it only
after negotiating the revision.

`yolo` starts Codex threads with approval `never` and the `:danger-full-access`
profile (verified on the `thread/start`/`thread/resume` result), Claude Code with
`--dangerously-skip-permissions` (plus `IS_SANDBOX=1`, which Claude requires as root,
and the flag setting `skipDangerousModePermissionPrompt`) and Gemini CLI with
`--approval-mode yolo`. `default` passes no override; Codex resumes, restarts and
account switches then keep the conversation's recorded permissions. It applies to
new launches, resume launches, Codex restart and account switch, saved-session open,
move recovery and hosted Claude without a `permission_mode`. Explicit Codex options
and a hosted `permission_mode` win; Codex message recovery restores the recorded
permissions; headless delivery keeps `approval_mode:"default"`, never `yolo`.
See [CLI](cli.md#launch-permissions-revision-19).

### New Space by label (revision 20)

Additive; negotiate `contract_revision >= 20`. `capabilities.new_space` ({version, resume, add_to_herdr, move_space}, booleans)
advertises `--new-space LABEL` on `api resume`, `api session add-to-herdr` and
`api move-space`. Each is exclusive with `--space`, validated with
the Space-label rule (`label_validation:"space_label"`) and refused with `WORKSPACE_NAME_EXISTS` when a Space of that
label already exists on the session's HERDR server. Resume and add-to-herdr use
the new Space's own initial shell pane for the session (nothing extra is created
or closed); add-to-herdr names its first tab with the session name, and its result
adds `new_space: bool`. Without any Space flag, `api resume` still opens an
unnamed new Space. `capabilities.add_to_herdr.requires_one_of` is
`["space","new_space"]` (`space` is no longer in `requires`).
The `move-space` form is described under Space moves (`New Space (revision 20)`).
`capabilities.new_space` carries `since_revision: 20`; older backends lack it and
reject `--new-space`, so send it only when the capability advertises the operation.

### Claude folder pre-trust (revision 20)

Additive; negotiate `contract_revision >= 20` (`api capabilities.claude_folder_trust`:
`setting:"claude_pretrust_folders"`, `built_in_default:true`,
`applies_when:"launch_permissions_yolo"`, `applies_to:["interactive","hosted"]`).
Before Sessionsation starts or resumes a Claude process in yolo mode (an interactive
`--dangerously-skip-permissions` start, or a hosted worker whose effective
permission mode is `bypassPermissions`), it pre-accepts Claude Code's folder-trust
dialog for the launch directory in that account's `CLAUDE_CONFIG_DIR/.claude.json`:
`projects["<directory>"].hasTrustDialogAccepted = true`, under the canonical
directory and, when different, the recorded spelling. A directory Claude does not
know yet gets Claude's own per-project defaults. Only that key changes: every other
member keeps its bytes and order, the file is replaced atomically with its mode and
owner (compare-and-swap against concurrent writes), a symlinked `.claude.json` is
refused, and a missing or malformed file is left untouched. `.credentials.json` is
never read. Failures are reported on stderr and never block the start. `default`
launch permissions, headless delivery and `claude_pretrust_folders:false` change
nothing.

### Gemini authentication modes (revision 21)

Additive; negotiate `contract_revision >= 21`. The Gemini entry of
`api capabilities.providers` adds:

- `auth_modes`: the ordered client choices
  `[{id,mode,label,available,reason,...}]`: `google_personal` (`mode:"google"`,
  `available:false`, `reason` set: Google ended Gemini CLI login for personal
  accounts on 2026-06-18), `google_workspace` (`mode:"google"`, available) and
  `api_key` (`mode:"api_key"`, available, `input:"stdin_json"`, `field:"api_key"`).
  Clients show unavailable entries disabled with their `reason` and never hard-code
  availability; a later revision can re-enable an entry.
- `auth_command`: `{since_revision:21,command:"account auth",modes:["google","api_key"],
  key_input:"stdin_json",validation_timeout_seconds:15,unsupported_modes:["vertex"]}`.

`api account auth gemini:NAME` returns `{account_id,provider,auth_mode,api_key_present,
auth_required,auth_failed,auth_failure_reason,auth_failure_observed_at,changed,
running_sessions,applies_to}` (plus `login_required` in `google` mode).
`--mode google` selects Login with Google and adds `login` (the `api account login`
plan). `--mode api-key --key-stdin` reads exactly `{"api_key":"..."}` from stdin
(at most 32 KiB, unknown fields refused), stores the key in the account's
`secrets/gemini-api-key` (0600, atomic, no symlinks), selects `gemini-api-key` and
adds `validation:{state,code,reason,timeout_seconds}` with `state`
`valid|auth_failed|inconclusive` from one headless run in a throwaway home. The key
is never part of any result, error, receipt, state or log. Errors:
`INVALID_REQUEST`, `INVALID_API_KEY_FORMAT`, `API_KEY_REQUIRED` (api-key without a
stored or supplied key), `UNSUPPORTED_AUTH_MODE` (`vertex`), `UNSAFE_SECRET_PATH`,
`UNSUPPORTED_PROVIDER_OPERATION` (not Gemini). `api account login` keeps its plan;
the plan's `account login` refuses an `api_key` account with `AUTH_MODE_API_KEY`.

Account rows add `auth_mode` (`google|api_key`, null for other providers) and
`api_key_present` (stat only; null for other providers). `auth_failed` and
`auth_failure_reason` (revision 18) also cover API keys: `AUTH_API_KEY_INVALID`
(the Gemini API rejected the key) and `AUTH_API_KEY_MISSING` (API-key mode without a
key in the child environment). A failure recorded for a key is superseded by a newly
stored key; `AUTH_CLIENT_UNSUPPORTED` remains the classification for a retired
personal Google login.

### Claude bypass-permissions warning (revision 22)

Additive; negotiate `contract_revision >= 22` (`api capabilities.claude_bypass_warning`:
`setting:"skipDangerousModePermissionPrompt"`, `values:["skip","show"]`,
`new_accounts_default:"skip"`). `api account bypass-warning claude:NAME
[--skip|--show]` (CLI `account bypass-warning`) returns `{account_id,provider,
bypass_warning:"skip"|"show",setting,value,previous,changed,settings_file,
applies_to:"sessions_started_after_the_change"}`; without a flag it only reads.
`--skip` writes `skipDangerousModePermissionPrompt:true` and `--show` writes an
explicit `false` in the account's `CLAUDE_CONFIG_DIR/settings.json`. Only that key
changes: other keys are preserved, the file is replaced atomically with its mode
kept, a symlinked file is refused (`SETTINGS_IS_SYMLINK`) and `.credentials.json`
is never read. Events `account.bypass_warning_skipped|shown` are recorded only on
change. A new Claude account created by Sessionsation (`account add` into a new
managed home, or `account login` of a managed home with no `settings.json`)
defaults to skip when the key is absent; adopted and existing homes are not
touched. Claude account rows add `bypass_warning` (`skip` when the stored value is
`true`, else `show`), `bypass_warning_value` (the raw boolean or `null`) and
`bypass_warning_setting`. Codex and Gemini rows lack the fields; the command
fails with `UNSUPPORTED_PROVIDER_OPERATION` for them.

### Pending questions (revision 23)

Additive; negotiate `contract_revision >= 23` and `capabilities.questions.version == 1`.
Every `api state` session row has `pending_questions` (empty when none or the
session is not running): questions an agent asked the user and is waiting on.
Question text and option labels are included; they are not secret.

```json
{"request_id":"call_…","asked_at":1793285639,"kind":"codex_request_user_input_async",
 "blocking":false,"turn_id":"…","answerable":true,"unanswerable_reason":null,
 "questions":[{"id":"q1","text":"Which inputs…?","options":[{"id":"o1","label":"Synthetic"}],
   "multi":false,"allow_free_text":true}]}
```

Detection is passive (incremental history scan; no provider contact, no extra
process) and also survives `state --cached`:

| kind | provider tool | ends when | answer path |
|---|---|---|---|
| `codex_request_user_input` | Plan-mode `request_user_input` (blocking) | its tool output, or the turn ends | `item/tool/requestUserInput` response |
| `codex_request_user_input_async` | Default-mode `request_user_input_async` (the turn keeps running) | a new turn starts, or Sessionsation answers | follow-up message on the public non-steering queue |
| `claude_hosted_ask_user_question` | `AskUserQuestion` in a hosted session | its permission request is answered | `can_use_tool` allow with `updatedInput.answers` |
| `claude_ask_user_question` | `AskUserQuestion` in a Claude terminal | its `tool_result` | none: `answerable:false`, `unanswerable_reason` `PROVIDER_TERMINAL_ONLY` |

Gemini CLI records `ask_user` tool calls only after they complete, so its rows have
`pending_questions:[]` and `pending_questions_unsupported_reason`. An async Codex
question answered in the TUI is not visible to Sessionsation until the next turn
starts; answer each question in one place.

`api answer --session ID --expected-generation G --request-id R [--answers JSON]`
(stdin when `--answers` is absent, at most 16 KiB) answers once. `QuestionAnswers`
maps every question id to `{"option_ids":[…],"text":"…"}`: at least one of them,
one option unless `multi`, text only when `allow_free_text`. Codex encodes text as
`user_note: …` after the selected labels; Claude joins labels and text with `, `.
The result is a `QuestionAnswerReceipt`:

- `answered`: the provider confirmed it (Codex `serverRequest/resolved`; the hosted
  worker wrote its control response).
- `queued`: an async Codex answer accepted by `api send` mode `queue`;
  `message_receipt` is that public receipt.
- `delivery_unknown`: the write may have happened. It is never resent.
- `rejected`: nothing was sent (`reason`); the question stays pending.

Safety: the catalog generation is checked (`STALE_TARGET`) and one answer per
session runs at a time (`OPERATION_BUSY`). The receipt is journaled before the
native write. Repeating the identical answer returns the original receipt with no
native I/O. A different answer after an attempt is `QUESTION_ALREADY_ANSWERED`.
A question that is gone, answered elsewhere or superseded is `QUESTION_NOT_PENDING`.
For a blocking Codex question Sessionsation reads the thread first and never loads
it. It subscribes with `thread/resume` (`excludeTurns`), which makes the app-server
replay the pending request to the new client. It answers only the replayed request
whose `itemId` equals `request_id`; if the request is not replayed it sends
nothing. Codex accepts the first response from any client and ignores later ones.
Answers are recorded in `questions/events.jsonl` under the data root.
See [CLI](cli.md#pending-questions-revision-23).

### Antigravity provider (revision 24)

Additive; negotiate `contract_revision >= 24`. `api capabilities` `providers` gains
`{"id":"antigravity","since_revision":24,...}` shaped like the Gemini entry without
`auth_modes`/`auth_command`: `enrollment:true`, `login:"interactive_exec"`,
`usage:false`, `usage_state:"unsupported"`, `usage_source:"antigravity-cli-unsupported"`,
`account_limits:false`, `account_logout:false`, `submit`/`queue`/`steer` true,
`input_delivery` and `headless_delivery` (modes `resume`, `new`; `print_timeout:true`;
`new_conversation` created first without a prompt). `operations` reports `archive`,
`delete_session` and `account_logout` unsupported with reasons. New top-level
`antigravity_input_delivery` (version 1: `herdr_agent:"agy"`,
`keys_by_queued_messages`, refusal reasons including `ANTIGRAVITY_QUEUE_MODE_UNKNOWN`
and `TERMINAL_EXIT_ARMED`); `launch_permissions.antigravity.yolo` is
`--dangerously-skip-permissions`; `add_to_herdr.unsupported.antigravity` is
`NO_NATIVE_ATTACH`; `session_identity.antigravity_process_source` is
`open_presence_lock_or_conversation_argument`; `questions.providers.antigravity` is
`{detect:false,answer:false}`. `conversation_archive.providers` is unchanged.

Accounts use `antigravity:NAME` ids; `api account add` returns the login plan (as for
Gemini), `refresh` reports `auth_mode.auth_method:"google-oauth"` when the token file
exists, `logout` and `limits` fail with `UNSUPPORTED_PROVIDER_OPERATION`. Account rows
report `usage_state:"unsupported"` with null windows. Session rows have
`provider:"antigravity"`, the Gemini-style `input_delivery` plus
`input_delivery.queued_messages {value,source}`, `capabilities.archive|delete|restore`
false (`UNSUPPORTED_PROVIDER_OPERATION`) and `pending_questions_unsupported_reason`.
`api send` accepts `antigravity:` accounts and sessions: headless receipts and
terminal-input receipts carry `provider:"antigravity"`; terminal receipts add
`antigravity_input` `{label,queue_mode,queue_mode_source,busy_at_submit,key,expected,
screen_evidence,outcome,caveat}`. Behaviour and limits: [CLI](cli.md#antigravity-cli-accounts-and-sessions-revision-24).

### Semantic contract version (1.24.0)

Adds `contract_version: "1.24.0"` to `api version` and `api capabilities` (additive;
`contract_revision` is unchanged and equals the MINOR part) and lets
`--api-version` take `MAJOR[.MINOR[.PATCH]]`, accepted whenever MAJOR is 1. See
[Contract versioning](#contract-versioning-semver). No behavior changed.

### Antigravity folder trust, stop, archive and delete (1.25.0, revision 25)

Additive MINOR; negotiate `contract_version` `1.25` or later, or the capabilities
below. Clients that gate on semver need no change.

- `api capabilities.antigravity_folder_trust`: `setting:"antigravity_pretrust_folders"`
  (`built_in_default:true`), `applies_when:"launch_permissions_yolo"`,
  `config_file:"AGY_ACCOUNT_HOME/.gemini/antigravity-cli/settings.json"`,
  `key:"trustedWorkspaces"`, `match:"exact_directory"`, `credentials_read:false`.
  Before a yolo interactive start or resume Sessionsation appends the launch
  directory to that array (other content preserved, atomic replace, symlink
  refused); `launch_permissions:default` and `antigravity_pretrust_folders:false`
  change nothing.
- `api capabilities.antigravity_lifecycle` documents the three operations below.
  The Antigravity provider entry reports `stop:true`, and `operations.stop`,
  `operations.archive` and `operations.delete_session` are supported with reasons
  (`restart` stays unsupported). `conversation_archive.providers` and
  `conversation_archive.native.providers` gain `"antigravity"`.
- `api stop --session ID --expected-generation G` on a running Antigravity session
  (`capabilities.stop:true`, otherwise `capability_reasons.stop`, for example
  `SESSION_NOT_RUNNING`) sends SIGTERM to the exact verified agy process and returns
  after it exited: `{id,source_session_id,kind:"terminate",phase:"complete",provider,
  signal:"SIGTERM",pid,process_start,provider_conversation_id,detail}`. Errors:
  `SESSION_NOT_RUNNING`, `SESSION_STATE_UNVERIFIED`, `AMBIGUOUS_SESSION`,
  `STOP_TIMEOUT` (not killed), `STOP_UNAVAILABLE`, `STOP_FAILED`. SIGKILL is never used.
- Antigravity sessions now report `capabilities.archive|delete|restore` like Claude
  and Gemini; `api archive`, `restore-session` and `delete-session` accept them. The
  exact-id file set, the index-row handling and the refusals are in
  [CLI](cli.md#antigravity-stop-archive-restore-and-delete-revision-25).

The revision 24 statement that Antigravity archive, restore, delete and stop are
unsupported no longer applies.

### Codex 0.166.0 native profile (1.25.1)

PATCH: no schema, field, command or enum change. `native_protocol.supported_versions`
and `steering.compaction_verified_profiles` gain `"0.166.0"` after the generated-schema
diff and offline fixture recorded in [operations](operations.md#profile-01660).
Version gating is unchanged: any other `initialize.userAgent` version (including a
later 0.166.x or 0.167.0) still fails closed with `unsupported_native_version`, and
observations keep reporting `native_protocol.version` and `supported:false` for it.

### Native `last_accessed` (1.26.0)

MINOR, additive: Claude, Gemini and Antigravity session snapshots and details now
populate `last_accessed` (previously always null for them), in Unix milliseconds,
the same unit Codex rows use, so the popup can sort inactive sessions by last
opened. The value is the time of the conversation's most recent activity, never the
time of the call: Claude and Gemini use the timestamp (`timestamp`, else
`lastUpdated`) of the last record in the transcript jsonl, read from a bounded
tail and cached per file (path, mtime, length), falling back to the file mtime when
no record carries one. Claude follows the entry's current conversation (continued-in
and parked-job lineage is already resolved onto the entry). Antigravity uses the mtime
of the brain `transcript.jsonl`, else of the conversation db; no database is opened.
A missing or unreadable source, a deleted account or an archived entry gives null.
Unlike Codex it is not an open/attach time. Existing fields are unchanged.

### Claude default statusline (1.27.0)

MINOR, additive: new config key `claude_default_statusline_command` and
`api capabilities.claude_default_statusline`. A new managed Claude account home gets
`statusLine` seeded in its `settings.json` (see
[Claude default statusline](cli.md#claude-default-statusline-contract-1270)).
Existing fields, adopted homes and existing homes are unchanged.

### Abandon uncertain operations (1.28.0)

MINOR, additive: command `api abandon-operation --operation ID --confirm ID` and
`api capabilities.abandon_operation`. Applies to resume-launch receipts
(`api resume-launch status|list`). Phase `abandoned` is a new terminal value
alongside `running`, `failed` and `recovered`: it never reverts, never replays or
sends, and releases the session reservation. Receipts gain optional
`abandoned_from` and `abandoned_at`. Refused while the target session's process is
live (`SESSION_PROCESS_LIVE`); see [cli](cli.md#abandoning-an-uncertain-resume-launch-contract-1280)
for the error codes.

### Data-root layout and retention (1.29.0)

MINOR, additive. Account rows gain `home` (the home in use: an adopted Claude
config home, the managed `accounts/<platform>/<name>`, or a deferred legacy
path) and `home_deferred` (true while the layout migration left an in-use home
at its legacy path). `api capabilities` gains `data_layout` (layout version,
directories, the account fields and the `layout` commands) and `retention`
(classes, settings and built-in defaults). New config keys
`log_retention_days`, `log_max_bytes`, `cache_retention_days` and
`operation_retention_days`. New error codes `LAYOUT_MIGRATION_REQUIRED`
(a legacy root; every command except `api version` and `layout`),
`LAYOUT_MIGRATION_IN_PROGRESS`, `LAYOUT_CONFLICT`, `LAYOUT_UNSUPPORTED`,
`BACKUP_REQUIRED` and `DAEMON_RUNNING` (layout commands). Request and result
shapes of existing commands are unchanged; clients that read data-root files
directly must use the new paths (see
[Data-root layout](operations.md#data-root-layout-contract-1290)).

### Per-provider account names and backend rename (1.30.0)

MINOR, additive. Account names are unique **per provider** (`codex:work` and `claude:work`
coexist); `capabilities.account_names.unique_across_providers` becomes `false`. A bare name that
exists under several providers fails with `ACCOUNT_AMBIGUOUS` (unambiguous bare names keep
working); `ACCOUNT_NAME_EXISTS` now names the same-provider owner only. Account rows gain
`folder_name`, `name_mismatch`, `running_sessions`, `generation` and `backend_rename`
(`capabilities.account_names` lists them). New command `api account rename-backend ID [NEW_NAME]
--expected-generation G [--request-id UUID]` ("Rename & update backend"; without `NEW_NAME` it
renames the folder and id to match the display name); `api account rename` gains
`--expected-generation`. New operation kind `account_rename` in `api operations`, resolved with
`api recover` or `api abandon-operation`. New error codes: `ACCOUNT_AMBIGUOUS`,
`ACCOUNT_SESSIONS_RUNNING`, `ACCOUNT_NAME_UNCHANGED`, `ACCOUNT_HOME_EXISTS`, `ACCOUNT_HOME_ADOPTED`,
`ACCOUNT_HOME_MOVE_FAILED`, `ACCOUNT_RENAME_UNRESOLVED`. A backend rename changes the account id:
clients must re-read accounts and sessions (session generations change) and treat old
receipts and digests as historical. See [cli](cli.md#account-folder-name-versus-display-name-contract-1300).

### Home-path robustness and `resume --confirm-unfinished` (1.31.0)

MINOR, additive. `sessionsation resume SESSION --expected-generation G` gains
`--confirm-unfinished`, matching `api resume-launch`'s `confirm_unfinished`: it acknowledges only
the `unfinished_history` warning of a saved Codex session (without it the command fails with
`UNFINISHED_HISTORY_CONFIRMATION_REQUIRED`; every other block reason still fails with
`SESSION_STATE_UNVERIFIED`). Behavior fixes, no request or result shape changes: owner and
binding home paths recorded before a home moved (`layout migrate`, backend rename) are compared
by resolved path, so conversation owners, pane bindings and Codex history paths that use a
legacy compatibility symlink no longer hide live attachments (`ATTACHMENT_UNOBSERVED`,
`SESSION_UNAVAILABLE`) or fail a same-account resume. `layout migrate` and backend rename now
also rewrite the stored `owner` (conversations) and `home` (managed-panes, herdr-panes
bindings) of the moved home; `layout migrate` reports `home_refs_rewritten`. Journals and
receipts are never rewritten.

### Deletion after a home move (1.31.1)

PATCH, no request or result shape changes. Conversation archive and deletion resolve every
candidate Codex home (the session's home, hosted instances, jobs and preserved homes) to one path
before scanning, so a home reachable through both its legacy compatibility symlink and its
`accounts/codex/<name>` path is scanned once. Previously the same rollout was listed twice and
the second `thread/read` failed with `-32600` after the first `thread/delete`, leaving the
deletion `failed` with the history already removed.

### Named, journaled inline `run` and `resume` (1.32.0)

MINOR, additive. The commands a terminal client types into an existing pane
(`sessionsation run --account A`, `sessionsation resume SESSION --expected-generation G`)
gain two things, advertised as `capabilities.inline_launch`
(`{since_revision:32, name:true, journaled:true, operations:true, ...}`; absent before 1.32,
so gate on it):

- `--name NAME` on both (`name:true`). Validated like `api launch` / `api resume --name`
  (`INVALID_SESSION_NAME`, `SESSION_NAME_EXISTS`). `run` gives the new catalog entry that name;
  `resume` renames the session first, as `api resume --name` does (the generation changes).
  When `HERDR_PANE_ID` is set the pane's tab is relabelled with the name, as hosted launches
  label their tabs (best effort; the catalog name is authoritative). `run --name` is refused
  with `--thread`, `--hosted` or `--host herdr` (`INVALID_ARGUMENT`). Without `--name`, `run`
  names the session as before (directory name, `-2` suffix on a clash).
- Journaling (`journaled:true`). Every inline `run` (Codex, Claude, Gemini, Antigravity)
  writes an ordinary launch receipt under `operations/launches` and every inline `resume` a
  resumption receipt under `operations/resumptions`, so `api launch list|status` and
  `api resume-launch list|status` show them. The receipt has `inline:{pid,process_start,pane_id}`
  and a placeholder host (`host.session:"inline"`). Phases: `starting_provider` (nothing has
  started), `attached` (the session is bound to the terminal process, which then becomes the
  provider), `complete` (the exact provider process was observed, or has ended after attaching; 1.35.1, earlier receipts say `running`), `failed`
  (the command was refused before the session was bound; the reserved name is released),
  `uncertain` (the outcome after binding is unknown, or the process ended without attaching).
  `uncertain` is never replayed and holds its reservation; an inline resumption is released
  with `api abandon-operation`. A phase is settled lazily when the receipt is read.
- `api operations` lists them (`operations:true`) as `{id, kind:"launch"|"resume", inline:true,
  session_id, name, account_id, phase, detail, created_at, updated_at, pane_id, can_cancel:false,
  can_respond:false, can_recover:false}`. Existing operation kinds and shapes are unchanged;
  hosted `api launch` / `api resume-launch` receipts are still not listed there.
- Locks: new launches take the shared account launch lock and the name reservation; resumes
  take the resumption admission lock, the per-conversation admission lock, the handoff check
  and the conversation lock (`RESUME_IN_PROGRESS` / `SESSION_OPERATION_IN_PROGRESS`),
  exactly like `api resume-launch`. A non-terminal resumption of the session blocks an inline
  resume and vice versa.

Not covered: `run --thread`, `run --hosted` and `run --host herdr` are unchanged and not
journaled here (hosted HERDR launches already use `api launch`); inline operations cannot be
cancelled or recovered through `api operations`. See [cli](cli.md#inline-run-and-resume-names-and-journal-contract-1320).

### Long terminal messages verified on a scrolling input box (1.33.0)

MINOR, behavior and limits. Claude Code (and agy) draw a long draft in an input box of
limited height that shows only the last rows of the text; the rest scrolls out of view.
Until 1.32 the whole typed text had to be on screen at once, so a message of roughly
1.5 KB or more was withdrawn with `TERMINAL_TYPED_TEXT_MISMATCH` in a short or narrow pane
(the receipt then showed `written:false`, `input_recorded:false`) although it was typed correctly.
Typed input is not a paste, so Claude never collapsed it to a `[Pasted text #N +K lines]`
placeholder; keystroke typing is kept because that is what the transcript correlation relies on.

- The text is still typed in small chunks, but the screen is now read every 80 non-blank
  characters. Each read must show a tail of everything typed so far, at least as long as the text
  typed since the previous read. Consecutive reads then tile the message, so every character is
  seen on screen; the first failed read withdraws the text (`TERMINAL_TYPED_TEXT_MISMATCH`)
  before Enter, as before. A message that fits is compared whole. Blank lines and soft wraps do
  not matter (whitespace is ignored in the comparison). The transcript correlation after Enter is unchanged.
- Limits: Claude `max_text_bytes` 4096 -> 8192 (`TERMINAL_TEXT_TOO_LONG` above it).
  agy `max_text_bytes` 1000 -> 8192 and `max_lines` 8 -> 200. agy shows `> ↑ N more lines`
  over the tail; that row is skipped for typed text. A user draft read that way is never
  reconstructed (refused as before). Gemini is unchanged (1000 bytes, 8 lines).
- Input clearing (withdrawal, draft clear) now sends up to 600 (Claude) / 400 (agy) key rounds,
  so long text can be removed; a clear that cannot be verified is still `TERMINAL_CLEAR_UNVERIFIED`.
- No file-backed fallback: direct delivery is verified, so none is needed.
- Capabilities: `terminal_input.max_text_bytes` (Claude) and the agy `max_text_bytes`/`max_lines`
  report the new limits; there is no new field.

### Inline journal settling (1.33.1)

PATCH, no shape changes. An inline receipt whose provider process has ended after it was
observed `running` now gets the detail "The inline provider process has ended." (lazy, once; the
phase `running` is the documented terminal phase, so `api abandon-operation` answers
`OPERATION_ALREADY_TERMINAL` consistently with `api operations`). Finished inline entries drop out
of `api operations` when their session no longer exists (deleted), or 24 hours after they ended
(`failed`/`abandoned` after 24 hours). Receipt files are kept for history and listed by
`api launch list` / `api resume-launch list`.

### Claude account switch (1.34.0)

MINOR, additive. `api switch --session ID --expected-generation G --to claude:B [--accept-risks]` now
works for a Claude session (same provider only: another provider prefix or a bare name is
`ACCOUNT_PROVIDER_REQUIRED`). The Claude provider capability `account_switch` and the operation
capability `operations.account_switch` are `true`; each session's `capabilities.switch_account` is
`true` for a stopped session and for a running interactive one whose process sits in a pane
Sessionsation recorded, otherwise `false` with the reason in `capability_reasons.switch_account`.
Gemini and Antigravity are unchanged. Codex switching is unchanged.

The result and the `api operations` row have the Codex-switch keys (`id`, `kind:"account"`, `phase`,
`detail`, `source_session_id`, `source_account_id`, `target_account_id`, `provider_conversation_id`,
`can_cancel:false`, `can_recover`, `can_respond:false`) plus `provider:"claude"`, `uncertain`,
`terminal`, `stalled`, `replays:false`, `generation` (the session's generation after the catalog
switch), `resume {state, pane, permissions}` and a `receipt`:
`{"files_moved":N,"paths":[...],"account_memory":{"moved":false,"stays_with":"claude:A","detail":...}}`.

- **Phases** (journal `operations/claude-switches/ID.json`): `admitted`, `stopping`, `stopped`, `copying`,
  `copied`, `switching`, `switched`, `removing`, `removed`, `resume_pending`, `resuming`, then the
  terminal `complete`, `failed` or `abandoned`; `uncertain` waits for `api recover` or
  `api abandon-operation`. Each phase is journaled before its work, a detached worker
  (`claude-switch-worker`) drives them, and nothing is replayed automatically.
- **Admission** (synchronous, nothing changes on refusal): generation (`STALE_TARGET`), the same
  account (`SWITCH_SAME_ACCOUNT`), both accounts enrolled, enabled and authenticated
  (`ACCOUNT_UNAVAILABLE`, `ACCOUNT_REMOVED`, `ACCOUNT_RENAME_UNRESOLVED`), no other unfinished
  operation on the session (`OPERATION_BUSY`, `SESSION_OPERATION_IN_PROGRESS`), no headless delivery
  in flight, a verified transcript (`HISTORY_NOT_FOUND`, `SESSION_STALE`,
  `SESSION_STATE_UNVERIFIED`) and no copy of the conversation in the target home (`TARGET_CONFLICT`).
- **Refusals for sessions that cannot be switched now**: `SESSION_HOSTED` (a Sessionsation-hosted
  stream-json session; stop it first, hosted switching is not implemented), `SESSION_NOT_SWITCHABLE`
  (background or parked sessions), `AMBIGUOUS_SESSION`, `SESSION_PANE_UNKNOWN` (a running process that
  is not in a recorded pane cannot be resumed afterwards; end it first), `SAVED_DIRECTORY_UNAVAILABLE`,
  and `RISK_CONFIRMATION_REQUIRED` for a running session that is `busy`: stopping it interrupts the
  turn, so the request must repeat with `--accept-risks` (an idle running session needs no flag).
- **Stop**: the live process is identified exactly as discovery does (registry pid, start time,
  session id and config home), signalled with `SIGTERM` through a pidfd opened before the identity is
  re-checked, and waited for (30 s). A process that does not exit is reported (`STOP_TIMEOUT`), never
  killed; a different process holding the conversation is `SESSION_CHANGED` and nothing is signalled.
- **What moves**: for every conversation of the lineage, `projects/<slug>/<id>.jsonl`,
  `projects/<slug>/<id>/`, `file-history/<id>/`, `session-env/<id>/` and `todos/<id>-agent-*.json`, from
  A's home to B's home (the exact set archive and delete use; symlinks are refused). **What stays**:
  per-project `projects/<slug>/memory/`, `settings.json`, credentials and every other account-level
  file stay with A and B gains none of them; `account_memory` in the receipt says so.
- **Crash safety**: digests are journaled once the source is quiescent; each file is copied to a staging
  directory in B's home, verified, then renamed into place; only then is the catalog switched (account
  becomes B, a new revision so a new generation, attachment cleared) and A's copies removed after a
  second digest check (stale registry entries of the stopped process are removed too). A failure
  before the catalog change removes only what the operation placed and ends `failed` with A intact; a
  failure after it ends `uncertain`. `api recover ID` continues from the journaled phase (copy,
  switch and removal are idempotent; staging left by a crash is discarded and redone).
  `api abandon-operation ID --confirm ID` ends an `uncertain` or stalled (no worker, over 10 s old)
  operation and moves nothing.
- **Resume**: a session that was running is relaunched in its pane (`resume_pending`, then
  `resuming`) as `sessionsation resume` with the recorded permissions (`--permissions yolo|default`
  from the stopped process's command line), the target account's `CLAUDE_CONFIG_DIR` and the recorded
  directory; the new process's attachment re-establishes pane, messaging and steering bindings (they
  follow the live process). `resume.state` is `confirmed` only after the exact new process is
  observed on the target account; otherwise the operation is `uncertain` with
  `resume.state:"unconfirmed"`. The dispatch is never replayed: `api recover` only re-checks. A
  session that was not running is only moved (`resume.state:"not_needed"`).
- UI and Agent-DM integration are not part of this revision; the API is additive and clients ignore
  unknown fields.

### Session auto-recover (1.35.0)

MINOR, additive. `api auto-recover --session ID --expected-generation G (--enable [--prompt TEXT] |
--disable)` turns a per-session toggle on or off. When it is on and the session's process later
disappears without an explicit stop (a host restart, a crash), the supervisor resumes the same
conversation, in its old HERDR pane when that is safe, and sends it a prompt. It works for every
provider with a resume path (Codex, Claude, Gemini, Antigravity).

- **Only for live sessions.** `--enable` is refused unless the session is running (`active` or
  `idle`): a stopped, saved, stale, unavailable or archived session gets `AUTO_RECOVER_REQUIRES_LIVE`.
  Hosted (stream-json) Claude sessions, background Claude sessions, Codex sessions of an unknown
  account and any provider without a resume path get `AUTO_RECOVER_UNSUPPORTED`. A stale generation
  is `STALE_TARGET`; an unfinished operation on the session is `SESSION_OPERATION_IN_PROGRESS`; a
  bad `--prompt` (empty, over 4000 bytes) is `INVALID_ARGUMENT`. `--disable` is always allowed
  (idempotent; `changed:false` when it was already off). Enabling again with the same prompt is
  `changed:false`. The toggle does not change the session's generation.
- **Receipt**: `{"session_id","generation","changed","auto_recover":{...}}`; clients ignore unknown fields.
- **State**: every session row of `api state` adds `auto_recover:{enabled, prompt, last_pane,
  last_recovered_at, last_result, workspace_label, suppressed, recoveries_last_hour, max_per_hour}`
  and `capabilities.auto_recover` with `capability_reasons.auto_recover` (`AUTO_RECOVER_REQUIRES_LIVE`
  or `AUTO_RECOVER_UNSUPPORTED[: reason]`; `true` while already enabled so it can be turned off).
  `last_pane` is a `herdr/<session>/<pane>` target kept current while the session is live.
  `last_result` is `null` or `{state, code, detail, operation_id, at}` with `state` one of
  `recovered`, `recovered_without_prompt`, `failed`, `uncertain`, `rate_limited`, `waiting`,
  `disabled`, `abandoned`. `api capabilities` adds the top-level `auto_recover` descriptor.
- **Storage**: the setting lives in `registry/auto-recover.json` (atomic, fsynced writes under a
  lock), not in the session catalog entry, so it survives restarts and pane bookkeeping never changes
  a generation.
- **Trigger**: the supervisor checks enabled sessions shortly after it starts and then every 15 s
  (`SESSIONSATION_AUTO_RECOVER_SECONDS`, `0` turns the loop off). A session qualifies only when it is
  enabled and not live by the same identity checks as `api state` for 3 consecutive checks (so a
  session that is running again is never started twice), it is not archived, deleted, hosted or
  suppressed, no other operation is running on it (`lifecycle` admission), no earlier recovery is
  unfinished, and HERDR answers (`HERDR_UNAVAILABLE` is surfaced in `last_result` and retried). The
  check is skipped entirely when no session has the toggle on.
- **Explicit stop**: `api stop`, `api archive` and `api delete-session` on the session **disable**
  auto-recover (`last_result.code:"EXPLICIT_STOP"`): a deliberate stop is never undone. `api switch`
  and `api restart` only **suppress** it until the session is observed live again, so the process they
  stop and relaunch themselves is not recovered a second time.
- **Pane choice**: the recorded pane is reused only if it still exists and only an idle shell runs in
  it (`herdr pane process-info`: foreground process group is the shell, a single shell process).
  Otherwise a new tab is opened (`--no-focus`, labelled with the session name) in the same workspace
  (the recorded id while it still has the recorded label, else the workspace with that label), else
  in a new workspace. The pane and tab are renamed after the session.
- **Resume**: through the normal path, as `sessionsation resume ID --expected-generation G
  --confirm-unfinished` in the pane (same flags, permissions and environment as a manual resume; an
  interrupted turn is expected). The prompt (default `Auto-recovery: the host restarted at <UTC time>
  and sessionsation resumed this session. Re-check your state and continue your work.`) is then sent
  once through the normal `api send` path (`mode:"submit"`) with a fixed `message_id`. A send is
  retried only on refusals that precede any admission (`SESSION_NOT_RUNNING`, `STALE_TARGET`,
  `LIVE_STEERING_DISABLED`, `PEER_ENDPOINT_UNAVAILABLE`, `PEER_SOCKET_UNLINKED`,
  `SESSION_OPERATION_IN_PROGRESS`) for up to 90 s; a clean refusal ends the operation `complete` with
  `prompt.state:"failed"` (`recovered_without_prompt`); anything ambiguous ends `uncertain` and is
  never re-sent.
- **Rate limit**: at most 3 recovery attempts per session per hour; beyond that nothing is started
  and `last_result` is `rate_limited` until the window passes.
- **Operations**: each recovery is journaled in `operations/auto-recoveries/ID.json` and listed by
  `api operations` as `kind:"auto_recover"` with the operation keys of an account switch plus
  `resume {state, pane, pane_reused, recorded_pane}` and `prompt {state, attempts, detail,
  message_id}`. **Phases**: `admitted`, `pane_ready`, `resuming`, `resumed`, `prompting`, then the
  terminal `complete`, `failed` (nothing was started) or `abandoned`; `uncertain` (the resume or prompt
  was dispatched but not confirmed) waits for `api recover ID` (re-checks, never replays) or
  `api abandon-operation ID --confirm ID`. A stalled operation (supervisor died, no worker, over 10 s
  old) is recoverable the same way, and blocks further automatic recoveries of that session.

Known limitation: a new account's name is its folder name, and Codex's control socket lives at
`<root>/accounts/codex/<folder>/app-server-control/app-server-control.sock`. Under a deep data root,
folder names longer than about 17 characters can exceed the 107-byte `sun_path` limit. This is
pre-existing and not widened here: `new_account_name_max` stays 30.

### Switching a stopped session only re-homes it (1.35.0)

`api switch` (and `move`) of a **stopped/saved** session is a quick re-homing, never a start. Claude
already behaved this way (it resumes only if it was running). For Codex, `capabilities.switch_account`
is now `true` for a saved session (with the usual `capability_reasons` otherwise) and `api switch`
no longer refuses it with `SESSION_NOT_RUNNING`. The job copies and byte-verifies the rollout and
child histories in the target home, retires the old copy and lets discovery list the session as
`saved` on the target account (new catalog account and generation). It does **not** load the thread
on the target app-server, open any pane or terminal, or start Codex; the journal phases are the
usual ones minus `launching_target` (`... verifying_history`, `complete` with a detail that says the
session stays saved). A later normal resume (or opening it in the UI) starts it there with the
recorded permissions: native resume rebuilds the target's index from the copied rollout. A resume
launch that explicitly chooses an account (`resume_destination`) is unchanged, and so is switching a
live session. Gemini and Antigravity do not support account switch.

### Observed inline operations are complete (1.35.1)

PATCH, no shape changes. An inline `run` / `resume` receipt used to stay in phase `running`
("Exact provider process observed; no launch replay") for the whole life of the provider
process, so `api operations` listed it as an ongoing resume although the session was up.
Once the exact provider process is observed the operation is finished: the receipt moves to
the terminal success phase `complete` (same detail), is never an unfinished resumption of the
session (it does not block a later resume or hold a lock), and drops out of `api operations`
24 hours after it completed (or when its session is deleted). Receipts already in `running`
(written by 1.33.0 - 1.35.0) settle to `complete` on the next read or settle pass: a live
process keeps the detail, an ended one gets "The inline provider process has ended.".
A process that ends before the provider attached is still `uncertain` and never replayed.
Hosted `api launch` / `api resume-launch` receipts keep `running` as their terminal success
phase. Clients treat inline `complete` like `running` as final.

### Stop closes the HERDR pane (1.36.0)

MINOR, additive. `api stop` (Codex and Antigravity; the hidden CLI `terminate` likewise) now also
closes the session's HERDR pane by default, after the session has actually stopped. `--keep-pane`
keeps the pane open (the previous behaviour). Capability flags: `stop_closes_pane: true`,
`stop_keep_pane: true`, and `stop_pane_close` (details and reason codes). Restart, switch,
move and auto-recover never close panes; the explicit stop already disables auto-recover.

- **What is closed**: only the exact pane recorded for the session (its catalog attachment, for Codex
  the source pane of the job), via `herdr pane close`, and only once the exact agent process is gone
  (verified by identity: pid and start time) and, for Codex, the history is restored to the resume
  list. A stop that fails, is refused, times out or is uncertain never closes anything.
- **Safety**: just before closing, the pane must still exist and nothing but its shell may run in it
  (`pane process-info`: foreground group is the shell, a single shell process; the same test as
  auto-recover's reusable pane). A pane running anything else (the user started another program) is
  left open.
- **tmux**: a tmux attachment is never closed (documented choice: skipped with a reason).
- **Receipt** (additive, on the stop result and `api operations` entries): `pane_closed` (bool) and
  `pane_close_reason` (null when closed, else `KEEP_PANE`, `NO_PANE` (no recorded pane),
  `PANE_BUSY`, `PANE_MISSING`, `PANE_STATE_UNVERIFIED`, `PANE_CLOSE_FAILED`,
  `PANE_CLOSE_UNSUPPORTED` (tmux) or `PANE_TARGET_INVALID`). A pane that cannot be closed does not
  fail the stop: the phase is still `complete`. For a Codex stop the outcome is final when the
  operation reaches `complete`; before that `pane_closed` is `false` with a null reason.

Related (1.36.0, no shape change): before sessionsation types a command into a reused, retained
HERDR shell (auto-recover's reused pane, Codex relaunch into a shell pane) it clears the input line with
`pane send-keys <pane> ctrl+e ctrl+u` (best effort, only when the foreground is the bare shell), so
leftover unsubmitted text is not prepended. tmux relaunch uses `respawn-pane -k`, which replaces
the shell, so nothing is cleared there.

### Live activity, send checks, stop expiry (1.37.0)

MINOR, additive. Capability flags `live_activity`, `pending_operations`, `native_send_checks`.

- **Failed turns accept input.** A Codex turn that ends in failure (for example "Selected model is
  at capacity", quota) leaves a live, idle session. Before 1.37.0 `api send` (submit/queue) and the
  passive readiness `actions.submit|queue` treated the `errored` state like an interrupted turn and
  refused with `explicit_resume_required`; the cause was the latest-turn status `failed` being
  mapped to `errored` and `errored` being gated like `interrupted`. Now `errored` is
  submit/queue-eligible when nothing else blocks (`actions.resume` stays available as well). Only an
  `interrupted` turn, a recorded stop intent and an approval/input prompt still require explicit action.
  `api observe` additionally carries `last_turn:{status,error}` and `activity`.
- **`sessions[].live_activity`** (Codex, running sessions; null otherwise; also in `api state --cached`
  since it is part of the stored snapshot):
  `{state, error, last_turn_status, source, observed_at}`. `state` is `working` (a turn is in
  progress), `idle` (ready, no turn seen), `turn_ended` (the latest turn completed or was
  interrupted and nothing has started since), `blocked` (waiting on an approval or user input),
  `error` (the latest turn failed; `error` holds the provider's text, at most 300 characters, one line)
  or `unknown` (no evidence). `source` is `app_server` (thread status read during the state scan) or
  `rollout` (bounded tail of the history file). `observed_at` is the snapshot time.
  A cached snapshot shows the activity of its own `observed_at`.
- **`sessions[].pending_operations`**: unfinished session operations of that conversation as
  `[{id, kind, phase, created_at, expires_at}]`; `kind` is `stop`, `pause`, `switch` or `restart`;
  `expires_at` is null except for a stop. (`pending_operation_id` is unchanged.)
- **Stop expiry.** A queued stop that is still waiting for the session to become idle
  (`waiting_for_*` phases) gives up after 1800 seconds (`TERMINATE_WAIT_SECONDS`, from the
  operation's `created_at`; journals without `expires_at` use that default): the operation fails with
  "the stop request expired while waiting for the session to become idle; nothing was stopped",
  the session keeps running and the stop gate recorded for that request is released (a newer stop's
  gate never is). Cancel with `api cancel-operation` is unchanged.
- **Native send checks (submit/queue to Codex).** Acceptance by the app-server is not consumption.
  (1) Before the send, up to 20 seconds are spent waiting (without holding the receipt ledger) for
  real input readiness: a process older than 4 seconds whose thread answers native reads with a
  settled turn state; busy, blocked and interrupted states end the wait at once and are then
  refused as before. (2) A late or lost add acknowledgement is reconciled against the native queue
  instead of left unknown when the entry is there. (3) After an input accepted by an idle session,
  up to 8 seconds are spent confirming it was consumed (the queue entry is gone and a turn records
  the message id). Consumed: `execution_started`/`completed_with_reply`, `facts.input_consumed:true`.
  Still queued at the deadline: the entry is removed again and the receipt is `failed` with reason
  `input_not_consumed` (`facts.input_consumed:false`, nothing ran, safe to resend with a new
  message id). Neither seen: `delivery_unknown` with reason `consumption_unverified`; never replay.
  A busy-target queue send is not subject to (3).
  Environment `SESSIONSATION_NATIVE_SEND_CHECKS=off` disables (1) and (3) (test fixtures only).

### Launch model override and turn-end notifications (1.38.0)

MINOR, additive. Capability flags `launch_model` and `turn_notify`.

- **Launch model override.** `LaunchRequest` (`api launch` validate/start) accepts optional
  `model` (a Codex model name such as `gpt-6-astra`: letters, digits and `. _ - : /`, 1-100
  characters, not starting with `-`) and `reasoning_effort` (`none`, `minimal`, `low`, `medium`,
  `high` or `xhigh`), for Codex accounts only (`UNSUPPORTED_PROVIDER_OPERATION` otherwise;
  `INVALID_MODEL` / `INVALID_REASONING_EFFORT` for bad values, rejected by `validate` as well as
  `start`). They override the account default for that session only: the new thread is started with
  that model and `model_reasoning_effort`, and the terminal is opened with `-m` and
  `-c model_reasoning_effort="..."`. Omitted fields keep the account default. The model name is not
  checked against what the account can use; an unavailable model is reported by Codex when a turn runs.
- **`api turn-notify --session ID --expected-generation G (--enable [--notify-session ID2] | --disable)`**
  (Codex sessions; default disabled; `sessions[].turn_notify:{enabled,notify_session,enabled_at}`).
  While enabled, the supervisor (poll `SESSIONSATION_TURN_NOTIFY_SECONDS`, default 5, 0 disables)
  records an event each time a turn of that session ends: `kind:"turn_ended"` (completed or
  interrupted) or `kind:"error"` (the turn failed; `error` has the provider text). A turn is identified
  by the rollout timestamp of its end, so it is reported exactly once and a turn shorter than the poll
  interval is still reported; the first observation after enabling is only a baseline (a turn that
  already ended is not reported). Settings and a journal of the last 500 events live in
  `registry/turn-notify.json`; they never change a session generation.
- **Delivery.** (1) The event journal: `api turn-events [--after SEQ] [--wait SECONDS]` returns
  `{events:[{seq,at,session_id,name,kind,error,delivery}],cursor,gap}`; pass `cursor` as the next
  `--after`; `--wait` (at most 300) blocks until an event after the cursor exists; `gap:true` means
  events before the oldest kept one were dropped. (2) With `--notify-session`, the supervisor also
  queues one short message (`mode:"queue"`, never a steer, never retried; content: the session's name and
  id and, for an error, the provider's error text; no conversation content) to that other Codex session; its
  outcome is the event's `delivery` (`{message_id,to,state,reason}` or `{state:"failed",error}`).
  Delivery to the designated session is best effort: the journal is the source of truth.

### Shared Claude conversation state (1.39.0)

MINOR, additive. Capability `shared_claude_state` (`api capabilities`).

A session must behave the same, and see the same information, on any account, so Claude's
non-credential per-conversation state moves out of the account homes into one store in the data root:
`<root>/shared/claude/<name>` for `name` in `projects` (transcripts and per-project `memory/`),
`file-history`, `todos` and `session-env` (the real subdirectory names of the Claude config dir).
Every managed Claude account home gets a directory symlink `<account home>/<name>` whose stored
target is exactly the absolute path `<root>/shared/claude/<name>` (as the root is configured, for
example `/mnt/user/appdata/sessionsation/...`). Credentials, settings, `sessions/` and everything else
stay per account. Adopted (`--config-dir`) homes are never linked (`shared status` reports them as
`applicable:false`).

- **Dormant until enabled.** Nothing changes on a root until `shared enable` or the first
  `shared migrate` writes `shared/claude/.sessionsation-shared`. Deploying the build alone changes
  no behavior.
- **Trusted-link model.** Sessionsation still refuses symlinks everywhere (generic walkers such as
  digest, copy and remove fail with `HISTORY_SYMLINK` on a link, so nothing can copy, digest or delete
  *through* one). One helper, `shared_state::trusted_link`, accepts exactly these links: a symlink at
  exactly `<managed Claude home>/<name>` for a listed name, owned by this user, whose stored target is
  exactly `<root>/shared/claude/<name>`, which resolves (no other symlink on the way) to that
  directory, and which is a real private (no group/other access) directory owned by this user inside
  real private `shared/` and `shared/claude/`. A link to another place, a link replaced by another
  link, a relative spelling, a link owned by another user, a link at an unlisted name or outside an
  account home, or a store entry that is itself a symlink is not trusted and is refused as before.
  The helper is consulted only where code explicitly handles a link: launch-time repair, the parent
  check when restoring archived files (`file-history/<id>`, `todos/...`), account deletion and the
  switch no-op.
- **Ensure-links.** On `account add`, before every Claude launch, resume, hosted start, headless
  delivery and attach, and on a switch, a missing link is created, an empty real directory is
  replaced by a link and a trusted link is kept. Errors: `SHARED_STATE_UNMIGRATED` (an account home
  has a real directory with data where a link belongs: nothing is merged automatically; run
  `sessionsation shared migrate --account claude:NAME`), `SHARED_STATE_LINK_UNTRUSTED` (another
  symlink or a file is there; left unchanged), `SHARED_STATE_STORE_UNSAFE`. `account add` reports
  `shared_state:{state:"linked"|"disabled"|"not_applicable"|"error"}`. Once the store is enabled, an
  account that is not migrated cannot be launched or resumed until it is migrated; running sessions
  are not affected.
- **Account switch.** When both accounts are fully linked the `copying`/`copied` and
  `removing`/`removed` phases are still journaled but copy and delete nothing (`receipt.shared_state:true`,
  `files_moved:0`; only dead `sessions/<pid>.json` entries of the source account are cleared). Per-project
  `memory/` is the same on both accounts. Two plain accounts keep the old move. A half-linked pair is
  refused with `SHARED_STATE_MIXED`, because the old move
  would delete from the shared store.
- **Delete, log out, rename.** Account deletion removes the links only (`unlink`; shared data is
  never moved or deleted, so conversations survive the account). Log out runs the provider's own
  logout and touches no history. Rename is a directory rename and the links stay valid.
- **`shared migrate --account claude:NAME [--dry-run] [--allow-full-copy] [--fs-path DIR]`** (CLI,
  not an `api` command; prints JSON). Journaled in `operations/shared-migrations/<id>.json`,
  idempotent (`already_migrated:true`) and resumable (the same command continues an unfinished
  journal). Steps: (1) preflight: account home and store on one file system (`st_dev`), the alias
  proven, `renameat2(RENAME_EXCHANGE)` and (unless `--allow-full-copy`) reflink probed on the real
  disk; `--dry-run` runs only the read-only checks and prints the per-name plan. (2) Backup: a reflink
  copy (`FICLONE`) of each directory to `<root>/backups/shared-migrate-<UTC time>/<account>/<name>`;
  without reflink support it fails with `BACKUP_REFLINK_UNSUPPORTED` unless `--allow-full-copy`.
  (3) Link, per name, with no moment in which the account path is missing and no data copied (inodes
  are unchanged, so file descriptors of live Claude processes keep working): if the store has no
  `<name>` yet, a placeholder symlink to `<root>/shared/claude/<name>` is created at that store path
  and exchanged in one `RENAME_EXCHANGE` with the account's real directory (the real directory is now
  in the store, the link at the account path); if the store already has `<name>`, non-conflicting
  entries are moved by `RENAME_NOREPLACE`, directories present on both sides are merged recursively,
  a file present on both sides is a reported conflict (never overwritten), and a temporary link is then
  exchanged with the emptied account directory the same way. Conflicts leave that name a real
  directory, print the paths under `results.<name>.conflicts`, exit non-zero with
  `SHARED_STATE_MIGRATION_INCOMPLETE`, and the command is repeated after the operator resolves them.
  Leftover `-tmp-*` project slugs are moved like any other entry.
- **`--fs-path`.** The Unraid user share (`/mnt/user`) is a FUSE union with no `RENAME_EXCHANGE` and
  no reflink. `--data-dir` stays the logical root, so every link target is `/mnt/user/...`, while
  `--fs-path /mnt/disk1/appdata/sessionsation` makes all file operations run on the real disk. The alias
  is proven by creating a probe file there and finding it under the data root. If the account's
  directories sit on another disk than the store, the preflight fails with `SHARED_STATE_CROSS_DEVICE`.
- **`shared status`** lists the store state and each Claude account's per-name state
  (`linked`, `missing`, `empty_directory`, `unmigrated`, `untrusted`); **`shared enable`** turns the
  feature on without migrating.

### Native delivery to Claude sessions (1.40.0)

MINOR, additive. Capability flags `claude_native_delivery` and `claude_idle_notify`.

Sessionsation already delivered a Claude `peer_message` (revision 16); 1.40.0 makes it the
agent-to-agent path and aligns it with Claude Code 2.1.312/2.1.314 cross-session messaging.

- **Selection.** `api send` takes `origin:"operator"|"agent"` (default `operator`). With
  `delivery:"auto"`, an operator send keeps the old preference (terminal input before peer
  message, so human-input authority is unchanged) and an `agent` send tries the native peer
  message first. `delivery:"native"` asks for it explicitly. `delivery:"peer_message"` stays
  native-only. Receipts carry `origin` and `native_fallback`.
- **Fallback, never both.** For `agent`/`native` sends of mode `submit` or `steer`, typing into the
  HERDR pane is the fallback when nothing native was possible: no verified registry entry or
  `peerProtocol` other than 1, inbound messages not accepted, the socket file gone
  (`PEER_SOCKET_UNLINKED`), or the connection never opened (`PEER_SOCKET_UNREACHABLE`). The
  receipt then has `native_fallback:{from:"peer_message",to:"terminal_input",reason,stage}`
  (`stage` is `admission` or `delivery`). Once a byte was written (accepted, held, or a write error)
  the message is never typed or resent: a failed write is `delivery_unknown`. `queue` and interrupts
  cannot be typed, so they stay native-only and are refused with the native reason.
- **Acceptance.** A session accepts inbound messages when the account sets
  `crossSessionInbound: accept` (`account live-steering`) or the live process itself was started
  with `--settings` saying so. Sessionsation launches and resumes Claude with
  `crossSessionInbound:"accept"` merged into its existing `--settings` JSON (other keys are kept)
  unless the account's own setting is an explicit `hold` or `refuse`. Bypass-permission sessions
  hold external messages by default, which is why the launch sets it.
- **Wire protocol** (one module, `provider/claude/native_peer.rs`, with the version pins): frame
  `{"type":"user","message":{"role":"user","content":TEXT},"priority":"next"|"later","from":"uds:<our socket>",
  "msgV":1,"msg_id":ID,...}`, newline terminated; control frames are `{"type":"control","action":...}`.
  Our reply socket is bound in the receiver's own socket directory as `<our pid>-<8 hex>.sock`
  (the receiver checks it against SO_PEERCRED and otherwise drops its receipts silently; the
  receipt's `peer_message.reply_socket` says `namespaced` or `private`). The optional auth line is
  not needed on Linux and the `*.key` token files are never read. Frames over 1 MiB are refused
  (`PEER_MESSAGE_TOO_LARGE`).
- **Receipts.** Success is silent. `peer_message_status` `delivered` gives `native_accepted`;
  `held` gives `held` (a bypass-permission receiver); `denied`/`refused`/`dropped` end `refused`
  (`PEER_MESSAGE_DENIED|REFUSED|DROPPED`, the drop reason is recorded) and `expired` ends `expired`.
  With no receipt inside 3 seconds the state is `native_accepted` with
  `peer_message.evidence:"transport_accepted_silent"`: the transport took the frame, which is not
  proof that Claude read it (a receiver that dropped the connection looks the same). The existing
  transcript correlation upgrades any of these to `input_recorded` and the reply
  (`evidence:"transcript_anchor"`); delivery is never reported as answered without it.
- **`api idle-notify --session ID --expected-generation G [--notify-session ID2]`** (Claude,
  running, inbound accepted). Subscribes with `notify_when_idle` for one `peer_idle_notice`
  (idle or exit), without a model request in the watched session. Journaled in
  `operations/claude-idle-notify/`; a detached worker keeps the subscription for 12 hours (Claude's
  own expiry). The notice becomes a `kind:"idle"` (or `"exited"`) event in `api turn-events`
  with `detail:{state,finished_at,status}` and, with `--notify-session`, one queued message. A
  repeat call returns the active subscription; a lost worker shows `stalled:true`; nothing is replayed.
- Errors: existing `LIVE_STEERING_DISABLED`, `PEER_ENDPOINT_UNAVAILABLE`, `PEER_SOCKET_UNLINKED`,
  `PEER_SOCKET_UNREACHABLE`, `PEER_INTERRUPT_UNSUPPORTED`; new `PEER_MESSAGE_TOO_LARGE`,
  `PEER_REPLY_SOCKET_UNAVAILABLE`, `INVALID_REQUEST` for an unknown `origin`.

### Stale stop gates and unsent receipts (1.40.1)

PATCH, no new fields.

- **A cancelled or failed stop no longer blocks sends.** `terminate`/move jobs record a stop
  gate (`stop_intent`) when they are admitted; it makes `steer-target` and the delivery gate report
  `explicit_resume_required` until the session is explicitly resumed. A job that was cancelled (or
  failed) before it stopped anything left that gate behind, so a session that kept running
  (state `working`, app-server confirmed) had no steer target and every submit was rejected
  `target_busy`. The worker now releases the job's own gate when it ends `cancelled`/`failed` while
  the exact source process (pid and start time) is still alive, and reads of the gate heal an
  already-stale one the same way. A completed stop keeps its gate; a gate of another job is never
  released.
- **An unsent `pending_local` submission always ends.** A message admitted while the session was
  `migrating` (or otherwise blocked by a transient reason) stayed `pending_local` for ever because
  nothing re-examined it. Reading its receipt now settles it: past its expiry it is `expired`; if the
  session it was admitted for has been replaced (its generation changed, for example by a stop and
  relaunch) it is `rejected` with `generation_mismatch`. Nothing was sent in either case.

### Detected agent origin, safer terminal typing, disk-space guard (1.41.0)

MINOR, additive. Capability additions: `claude_native_delivery.origin_detection`, `disk_space`.

- **Origin detection.** `api send` with no `origin` used to mean `operator` (typing into the pane).
  Callers that are agents (and scripts they run) rarely set it, so their messages were typed into a busy
  Claude's input box. When `origin` is omitted, Sessionsation now walks the caller's process tree
  (`/proc/<pid>/stat` parent chain, at most 64 hops) and treats the send as `agent` when an ancestor is the
  verified process (pid and start time) of a live catalog attachment, that is, when the caller runs
  inside a Sessionsation-managed session. A plain shell or the HERDR UI has no such ancestor and stays
  `operator`. An explicit `origin` always wins and is never second-guessed. The receipt carries
  `origin` (the resolved value) and `origin_source`: `explicit`, `detected` or `default`. Idempotent
  repeats return the original receipt, whatever the caller looks like the second time. Only the
  catalog is consulted (no provider, no HERDR); an unreadable catalog means `default`.
- **Terminal typing after a worker failure.** A typed message is never reported as delivered unless Enter
  went out (`submit` is journaled first). New:
  - When the worker fails after typing and before Enter (a HERDR error, or a journal write that fails on a
    full disk) it withdraws the message it typed even when the user had no draft; before, only a
    non-empty draft triggered that pass and the message stayed in the input box.
  - A worker that dies past the clear (`steps` has `clear`, not `submit`) leaves an orphan. A receipt
    read after the 10 second grace starts a cleanup worker (step `orphan_cleanup`) that withdraws the
    text if the box still holds it (all of it, its tail, or the part typed so far), types the user's
    journaled draft back, and ends the request `rejected` with `TERMINAL_WORKER_EXITED_BEFORE_SUBMIT`.
    The next delivery to the same pane does the same for any orphan found there before it reads the pane,
    so a stranded message no longer makes every later send fail `TERMINAL_DRAFT_UNRECONSTRUCTABLE`.
    A box that holds anything else is left alone and reported (`left_in_input`).
  - A refused screen records `terminal_input.refusal_layout`: `screen_rows`, `rules`, `box_rows`,
    `max_draft_rows` and the footer rows under the input box (UI chrome; never draft text), so
    `TERMINAL_DRAFT_UNRECONSTRUCTABLE` (a draft taller than `max_draft_rows`) and
    `TERMINAL_OVERLAY_UNIDENTIFIED` (an unrecognised footer) can be told apart.
- **Disk space.** `config.json` `min_free_disk_mb` (default 512, 0 disables). Archive, restore, delete,
  switch, restart, resume, relocate (not `--dry-run`), move-space, launch start, resume-launch start,
  account delete, `shared migrate` and `layout migrate` (not dry runs) check the free space of the
  filesystem holding the data root before changing anything and refuse with
  `INSUFFICIENT_DISK_SPACE` (error fields `free_bytes`, `needed_bytes`, `operation`). `api disk-space`
  reports the same figures read-only, so a script can check before a multi-step sequence (for example
  before stopping a session it is going to archive). Any ENOSPC/EDQUOT anywhere in an `api` or CLI
  failure is reported as `DISK_FULL` instead of a raw I/O message. A non-cached `api state` whose
  refresh fails that way serves the last cached snapshot with `snapshot.refresh_failed:"DISK_FULL"`,
  `snapshot.fresh:false` and a `warnings[]` entry `STATE_REFRESH_FAILED_DISK_FULL`; without a snapshot it
  fails `DISK_FULL`.
- **`api idle-notify` exit event.** A notice reporting `state:"exited"` is journaled as kind `exited`
  (it was `idle` with `detail.state:"exited"`); `detail` is unchanged.

### Native live activity, usage read, uncertain operations (1.42.0)

MINOR, additive. Capability additions: `usage_read`; `live_activity.providers` now lists `claude`,
`gemini` and `antigravity`.

- **`activity` and `live_activity` for running Claude, Gemini and Antigravity sessions** (also in
  `api state --cached`). Same shape and states as Codex rows: `activity` is `busy`, `idle` or `unknown`;
  `live_activity` is `{state, error, last_turn_status, source, observed_at}`. Not-running native rows keep
  `null`. Signals are local and read-only, bounded tail reads only (128 KiB, one retry at 1 MiB), and
  never include message text:
  - Claude `source:"transcript_tail"`: the newest decisive transcript record. An assistant record with
    `stop_reason` `tool_use` (or still streaming), a `tool_result`, or a user prompt with no reply
    is `working`; an `end_turn`/`stop_sequence`/`max_tokens` assistant record or a `turn_duration` system
    record is `turn_ended`; a `[Request interrupted by user]` record is `turn_ended` with
    `last_turn_status:"interrupted"`; an API-error record is `error` (`error` is Claude's short error
    code, or `api_error`). Sidechain, meta and bookkeeping records are skipped.
  - Claude `source:"native_registry"`: `<CLAUDE_CONFIG_DIR>/sessions/<pid>.json` (`status` `busy`, `idle`
    or `waiting`, with `statusUpdatedAt`; pid and process start must match the observed process). It is
    used only when it is newer than the newest transcript record, because Claude does not rewrite it on
    every transition. `waiting` (a permission prompt or question) is `blocked`. A session that also has
    `pending_questions` is `blocked` while a turn is in progress.
  - Gemini and Antigravity `source:"transcript_tail"`: the last chat/step record (a `gemini` record with
    no tool calls, or a planner response with no tool calls, is `turn_ended`; with tool calls, or a
    user input or tool step with no answer yet, `working`). An unanswered user input older than 10
    minutes, or a step that is not `DONE`, is `unknown`.
  - No usable evidence is `unknown` (`activity:"unknown"`), never a guess. `observed_at` is the time of
    the evidence used (the record or registry update), not of the snapshot; `api idle-notify` remains the
    push signal for the next idle transition.
- **`api usage [--account claude:NAME]`**: the projected Claude usage per account with `state`, `fresh`,
  `source`, `observed_at`, `age_seconds`, and `short` / `weekly` windows each carrying their own
  `observed_at` and `age_seconds`. The statusline recorder carries each window (`five_hour`,
  `seven_day`) forward until its own `resets_at` when a payload lacks it (Claude Code drops a window
  after its reset until the next API response), records a per-window `observed_at`, and sweeps
  `.tmp*` files older than 10 minutes from `<home>/.sessionsation/`. `usage.<window>.observed_at` is
  also in the account rows of `api state`.
- **Uncertain operations are named.** A session blocked by an unfinished journaled operation reports it
  in `sessions[].pending_operations` (`{id, kind, phase, created_at:null, expires_at:null, unresolved,
  resolve}` for journals that are not queued jobs; `kind` also `resume`, `auto_recover`, `switch`,
  `space_move`, `conversation`, `move` or `message_recovery`). Capability reasons are
  `RECOVERY_UNRESOLVED` for an `uncertain` or `stuck` operation (with `resume_block_reason:
  "recovery_unresolved"`) and `SESSION_OPERATION_IN_PROGRESS` otherwise; refusals carry the operation
  id (`RECOVERY_UNRESOLVED: operation ID (kind, phase uncertain); ...`), and `api resume` no longer
  answers `SESSION_STATE_UNVERIFIED` for them. An inline `resume` whose terminal process ended before
  any provider attached (nothing resumed, no input sent) settles as `failed` instead of `uncertain`,
  so the session is not left blocked; such receipts an earlier release left `uncertain` settle the same
  way on the next read.
- **Codex receipts carry `origin` and `origin_source`** (resolved by caller detection as for Claude;
  `operator`/`default` for receipts recorded before 1.42.0).
- **Daemon start** writes a fresh `api state` snapshot two seconds after the socket serves requests, so
  `api state --cached` does not show the previous process's data after a cutover.

### Auto Start modes (1.43.0)

MINOR, additive. "Auto recover" is renamed **Auto Start** and gains three modes per session:
`disabled` (the default for every session, including new ones; a session with no record is
`disabled`), `start` (when the process disappears, resume the session and send **nothing**) and
`start_prompt` (resume, then send the prompt once: the custom `--prompt` or the default `Auto-start:
the host restarted at <UTC time> and sessionsation resumed this session. Re-check your state and
continue your work.`). Everything in [Session auto-recover (1.35.0)](#session-auto-recover-1350)
(liveness checks, pane choice, explicit-stop disable, suppression, the 3 per hour rate limit,
journaled never-replayed operations) applies to both start modes.

- **Command**: `api auto-start --session ID --expected-generation G --mode disabled|start|start_prompt
  [--prompt TEXT]`. `--prompt` is only valid with `start_prompt` (`INVALID_ARGUMENT` otherwise; 1-4000
  bytes). `--mode start_prompt` without `--prompt` clears a custom prompt (the default applies);
  the descriptor carries `default_prompt` and `max_prompt_bytes` for clients that show a hint. Until 1.44.0 enabling a mode (from `disabled`) needed a live session exactly like `--enable` before (no longer: see [1.44.0](#auto-start-semantics-host-boot-and-crash-1440))
  (`AUTO_RECOVER_REQUIRES_LIVE`, `AUTO_RECOVER_UNSUPPORTED`); switching between `start` and
  `start_prompt` on an already enabled session, and `disabled`, are always allowed. The receipt is
  `{"session_id","generation","changed","mode","auto_start":{...},"auto_recover":{...}}`.
- **Bulk**: `api auto-start --all --mode disabled` disables every session at once (operator-only,
  like the other local admin commands; no `--session` or generation; only `disabled` is accepted
  since it can only stop actions). Receipt `{"all":true,"mode":"disabled","changed","disabled_count","disabled":[ids]}`.
- **State**: every session row adds `auto_start:{mode, enabled, prompt, last_pane, last_started_at,
  last_result, workspace_label, suppressed, starts_last_hour, max_per_hour}` and
  `capabilities.auto_start` / `capability_reasons.auto_start` (same rules as `auto_recover`).
  `last_result.state` is `started` (resumed, nothing to send or the prompt was not delivered; see
  `code`), `started_with_prompt`, `failed`, `uncertain`, `rate_limited`, `waiting`, `disabled` or
  `abandoned`. `api capabilities` adds the top-level `auto_start` descriptor (clients show the control
  only when it is present).
- **Journal**: operations keep `kind:"auto_recover"` and add `mode`. With `start` the `prompting`
  phase is skipped (`resumed` -> `complete`) and `prompt.state` is `skipped`.
- **Deprecated, still served**: `api auto-recover` (`--enable` = `start_prompt`, `--disable` =
  `disabled`), `sessions[].auto_recover` (`enabled` = mode is not `disabled`; `last_result.state`
  keeps the 1.35.0 names `recovered` / `recovered_without_prompt`) and `capabilities.auto_recover`
  (marked `deprecated:true`). Older clients keep working; they cannot see or set `start`.
- **Storage migration**: `registry/auto-recover.json` records gain `mode` (schema_version 2). A record
  without `mode` that is enabled reads, and is rewritten, as `start_prompt` with its prompt kept; a
  disabled one as `disabled`. The rewrite is one atomic replace under the store lock (a crash leaves the
  old or the new file) and running it again changes nothing. `enabled` stays in the file, mirroring
  the mode.

### Auto Start semantics: host boot and crash (1.44.0)

MINOR, a behavioural change to Auto Start (1.43.0). Auto Start now means "start this session after a
host reboot or a crash, and not much else". It supersedes the live-only, disable-on-stop and
start-on-any-disappearance rules of [Session auto-recover (1.35.0)](#session-auto-recover-1350) and
[Auto Start modes (1.43.0)](#auto-start-modes-1430); everything else there (pane choice, journaled
never-replayed operations, the 3 per hour rate limit, `RECOVERY_UNRESOLVED`, suppression by
`switch`/`restart`, HERDR availability, the 3 consecutive checks as settle delay) is unchanged.

- **The setting is independent of running state.** `api auto-start --mode start|start_prompt` (and the
  deprecated `--enable`) works on live, stopped and saved sessions; `AUTO_RECOVER_REQUIRES_LIVE` is no
  longer returned. `capabilities.auto_start` / `auto_recover` are true for inactive sessions;
  only archived sessions (and hosted/background/unsupported ones: `AUTO_RECOVER_UNSUPPORTED`) are
  excluded. The descriptor reports `enable_requires:"none"`.
- **An explicit stop no longer changes the mode**; it only marks the stop (so it is not a crash) and
  sets `last_result` to `skipped_explicit_stop` / `EXPLICIT_STOP`. **Archive and delete still set the
  mode to `disabled`** (`last_result.code` `ARCHIVED_OR_DELETED`; it was `EXPLICIT_STOP`).
  `switch` and `restart` still only pause until the session is live again.
- **Trigger A, host boot.** The supervisor compares `/proc/sys/kernel/random/boot_id` with the boot id
  persisted in `registry/auto-recover.json` (`boot_id`). When it differs, every session whose mode is not
  `disabled` and that is not running is started, including ones stopped (explicitly or not) before the
  reboot, after 3 consecutive not-running checks and once HERDR answers. Each session is started once
  per boot (`boot_started` records the boot id). A supervisor or daemon restart on the same boot (a
  cutover) starts nothing. The first pass after upgrading only records the boot id: it starts nothing.
- **Trigger B, crash.** On the same boot, a session that was seen running and whose process then
  vanished is started again only when the exit is classified as a crash. A clean exit (`/exit`), a closed
  pane (SIGHUP/SIGTERM from the terminal) and an explicit stop start nothing, and neither does an exit
  that cannot be classified (unknown means no start).
- **Classification** (`sessions[].auto_start.last_result`), using the process recorded while it ran
  (pid and /proc start time):
  - Kernel log (`dmesg`), any provider: a line naming that exact pid and not older than the process:
    `Out of memory: Killed process <pid>` (OOM killer, SIGKILL), `<comm>[<pid>]: segfault at` (SIGSEGV),
    `traps: <comm>[<pid>] trap ...` / `general protection` (SIGILL, SIGSEGV, SIGFPE). This is a crash:
    `started_after_crash`.
  - Claude additionally: the registry entry `<home>/sessions/<pid>.json` is removed by an orderly
    shutdown. Gone and no crash line: `skipped_clean_exit`. Left behind and no crash line: a hard kill,
    but a closed pane may look the same, so `exit_unclassified` (no start).
  - Codex, Gemini and Antigravity: the kernel log only; any other exit is `exit_unclassified`.
  - HERDR exposes no pane exit status and no provider transcript carries a reliable exit marker, so
    those are not used. **Cannot be classified**: SIGABRT and runtime aborts (e.g. a JavaScript heap
    out-of-memory abort) without a kernel line, a manual `kill -9`, a non-zero exit that still shuts down
    cleanly, and a crash whose kernel line already left the kernel ring buffer or that `dmesg` may not
    read. A hard kill the kernel did not log is deliberately not started.
- **`last_result.state`** is `started_after_boot`, `started_after_boot_with_prompt`,
  `started_after_crash`, `started_after_crash_with_prompt`, `skipped_clean_exit`, `exit_unclassified`,
  `skipped_explicit_stop`, `failed`, `already_running`, `uncertain`, `rate_limited`, `waiting`, `disabled` or `abandoned`
  (codes `STARTED_AFTER_BOOT[_WITH_PROMPT]`, `STARTED_AFTER_CRASH[_WITH_PROMPT]`, `CLEAN_EXIT`,
  `EXIT_UNCLASSIFIED`, `EXPLICIT_STOP`, `PROMPT_NOT_DELIVERED`, ...). Records from 1.43.0 may still read
  `started` / `started_with_prompt`.
- **Default prompts**: after a boot `Auto-start: the host restarted at <UTC boot time> and sessionsation
  started this session again after the restart. Re-check your state and continue your work.`; after a
  crash `Auto-start: this session's process crashed (<what was seen>) and sessionsation started it again
  at <UTC time>. Re-check your state and continue your work.` The descriptor carries both
  (`default_prompt`, `default_prompt_crash`) and the classification table under `triggers`.
- **Deprecated projection**: `auto_recover.last_result.state` maps every `started_after_*_with_prompt` to
  `recovered`, every other `started*` to `recovered_without_prompt`, and the `skipped_*` /
  `exit_unclassified` states to `waiting`. `enabled` still mirrors the mode.
- **Data**: existing records stay valid. New optional fields in `registry/auto-recover.json`
  (`boot_id` at the top; `boot_due`, `boot_started`, `stopped_explicitly_at`, `live_pid`, `live_start`,
  `live_boot` per session) default to absent. Operations gain an optional `trigger` (`boot` | `crash`).
- **Tests only** override the sources: `SESSIONSATION_BOOT_ID_FILE` and `SESSIONSATION_KERNEL_LOG_FILE`.


### Auto Start delivery, array start and Space names (1.45.0)

MINOR (includes the 1.44.1 fixes). Auto Start and resume no longer fail on things that change after a
reboot or an array restart.

- **Delivered means delivered (1.44.1).** The Auto Start prompt is read back through its message receipt.
  `prompt.state` is `delivered` only when the receipt shows `written`, `input_consumed` or native
  acceptance (or later). A `rejected` receipt with nothing written (for example
  `TERMINAL_VIEW_UNDETERMINED`, `ADAPTER_BUSY`) and the pre-admission refusals
  (`PEER_SOCKET_UNLINKED`, `SESSION_NOT_RUNNING`, ...) are retried with backoff for up to 180 s
  (`SESSIONSATION_AUTO_RECOVER_PROMPT_SECONDS`) with a fresh message id, while the session keeps its
  generation. Anything that may have been written (`delivery_unknown`, uncertain) is never retried and ends
  `uncertain`. `prompt.attempts` and `prompt.detail` list every attempt with its final receipt state
  (`attempt 1 (<id>): rejected/TERMINAL_VIEW_UNDETERMINED; attempt 2 (<id>): native_accepted`);
  `last_result` is `started_after_*_with_prompt` only for a delivered prompt, else `PROMPT_NOT_DELIVERED`.
- **Native first.** For Claude the prompt waits up to 60 s (`SESSIONSATION_AUTO_RECOVER_NATIVE_SECONDS`) for
  `<CLAUDE_CONFIG_DIR>/sessions/<pid>.json` and its socket, then is sent with `delivery: native`.
  Every other Sessionsation-to-Claude `submit`/`steer` with `delivery: auto` now also prefers the native
  peer message (shown by Claude Code as a collapsed `@ sender` entry) and waits up to 5 s
  (`SESSIONSATION_NATIVE_WAIT_SECONDS`) for the endpoint; it types only when native is unavailable.
- **Classifier fix.** The input box of a resumed Claude session has a short labelled top rule
  (`──── <session name> ─`, 8 dashes in 58 cells). It was not read as a rule, so every typed send was refused
  `TERMINAL_VIEW_UNDETERMINED`. A rule that starts with 3 or more dashes followed by a space now counts
  (fixture `labelled-top-rule-resumed.ansi`).
- **Trigger C, array start.** On the same boot, a changed mount id of `/mnt/user` (the shfs array mount;
  every mount gets a new id) makes every session with a mode that is not running due one start
  (`array_due`/`array_started` per session, `array_id` at the top of `registry/auto-recover.json`). A
  supervisor restart or cutover keeps the id and starts nothing; the first pass after upgrading only records it;
  a new boot uses the boot trigger, not both. Results are `started_after_array_start[_with_prompt]`;
  operations carry `trigger: array_start`. Default prompt: `Auto-start: the array was restarted at <UTC time>
  and sessionsation started this session again after the restart. ...`. Tests override with
  `SESSIONSATION_MOUNTINFO_FILE` and `SESSIONSATION_ARRAY_MOUNT`.
- **Space names.** A requested Space (`--space`, `resume`, `launch`, `move-space`, `add-to-herdr`) is resolved
  as the HERDR workspace id, then the visible name (exact, then case-insensitive). `--new-space <label>`
  whose name already exists reuses that Space instead of `WORKSPACE_NAME_EXISTS`. Auto Start already falls
  back from the recorded id to the recorded Space label.
  Since 1.48.0 these reuse semantics are retained for `--new-space`; `--create-space` is strict
  (`WORKSPACE_NAME_EXISTS` when the label exists).
- **No stale generation.** After `resume`, `stop`, `restart`, `switch`, `move-space`, `launch` the cached
  snapshots are refreshed at once (the session's `generation` is patched immediately, then a full
  refresh), and the `resume`/`stop`/`restart`/`switch` result carries the new `generation`.
- **Build identity.** `api version` reports `commit`, the git commit embedded at build time
  (`SESSIONSATION_COMMIT`, else `git rev-parse HEAD`). `scripts/smoke-installed.sh BIN COMMIT` checks the
  installed binary (`api version`, `api capabilities`, `api state --cached`) and the commit.
- **Sender name (not changed).** Claude Code labels a native message with the registry `name` of the
  sending process (`nameSource` `auto` = an AI title, `user` = `/rename`). `--name` on resume does not
  override an existing AI title; `/rename` does. Sessionsation does not edit transcripts or registries.

- **Idle subscription is opt-in (`await_reply`).** A native Claude delivery used to also ask for Claude's idle
  notice, which Claude Code shows to the receiving user as "A process claiming the address
  uds:/tmp/cc-socks/<pid>-<hex>.sock asked to be told when this session is next idle". The send request now has
  an optional boolean `await_reply` (default false); only `true` sends that subscription so the receipt can
  reach `completed_with_reply`. Without it (sends, Auto Start prompts and notices) nothing is requested, the
  receipt settles `native_accepted` (upgraded to `input_recorded`/consumed from the transcript anchor) after at most the
  3 s status window, and the worker exits promptly instead of lingering up to the 600 s timeout.

### Cached snapshots follow asynchronous lifecycle jobs (1.45.1)

PATCH. 1.45.0 refreshed the cached snapshots when a lifecycle command returned, but `stop`, `restart`,
`switch`, move, archive/delete/restore, resume and launch finish in a background worker: the refresh ran
when the job was only queued, so `api state --cached` kept `availability: running` and the old generation
until a full `state`, and archive/delete were refused `STALE_TARGET`. Every job worker now rewrites the
`active`, `archived` and `all` snapshots when its job completes (or fails), before it releases its lock.
No field or command changed.

### Codex send options, opt-in native sender registration, Codex usage clock (1.45.2)

PATCH (additive).

- **Codex targets accept the Claude send options.** `origin` (`operator` or `agent`; auto-detected when
  omitted, recorded on the receipt as `origin`/`origin_source`), `delivery` and `await_reply` no longer fail
  the whole request with `ok:false`. `delivery` other than `auto` names a Claude path and returns
  `PROVIDER_DELIVERY_UNSUPPORTED` (an unknown value is `INVALID_REQUEST`); `await_reply` is ignored for Codex.
  A refused unknown field now answers `INVALID_REQUEST: unknown field \`x\`, expected one of ...`.
- **`native_sender_registration` (config.json, default `false`).** When on, the native-message worker writes
  one registry entry for its own pid into the receiving Claude account (`<home>/sessions/<pid>.json`) before
  the frame is written, so Claude shows a Codex-originated or agent-originated message as the collapsed
  `@<sender> ❯` entry instead of an unidentified block. `name` is the sending session's catalog name
  (`sessionsation` for system messages); `messagingSocketPath` is the worker's reply socket, which is also the
  frame's `from`. Fields written: `pid`, `procStart` (the `/proc/<pid>/stat` start tick, as Claude writes it),
  `pidDomain`, `startedAt`, `updatedAt`, `cwd`, `version` (`sessionsation-<version>`), `peerProtocol` 1,
  `name`, `nameSource` `peer`, `nameSince`, `messagingSocketPath`, and the ownership markers `entrypoint`
  `sessionsation-peer-worker` plus `registeredBy` `sessionsation`. Deliberately absent: `sessionId`, `kind`,
  `peerFeatures`, `status`. No Claude process runs and no API call is made.
  - Receipt `sender_registration`: `{"name":..,"registered":true}` or `{"registered":false,"reason":..}`
    (`SENDER_NAME_COLLIDES_WITH_LIVE_SESSION` when a live non-sessionsation record already has that name,
    `SENDER_REGISTRY_UNAVAILABLE`, `SENDER_PID_FILE_FOREIGN`, `SENDER_NAME_INVALID`); the message is then
    delivered unregistered.
  - Cleanup: removed by a drop guard on success, error and timeout, and by a SIGTERM/SIGINT handler (which
    also unlinks the reply socket). After SIGKILL the entry is swept: by the next worker for that account and by
    the supervisor every 60 s, for every Claude account, whether or not the setting is still on. A sweep removes
    only entries carrying both ownership markers whose pid is dead or whose `procStart` no longer matches; it
    never matches by name and never touches any other entry.
  - Caveats: the entry is visible to Claude (for example in `/list-agents`) for the seconds the worker lives.
    Replies Claude sends back go to the worker's reply socket: with `await_reply` the worker records them into
    the receipt as before, otherwise they are ignored. Claude Code's use of the registry is undocumented; the
    rendering was derived from Claude Code 2.1.314 and is not asserted for other versions.
- **Codex usage clock and staleness.** An `observed_at` ahead of the clock (a clock step back) used to make
  `Account::observe` drop every later reading as older, freezing usage until the clock caught up, and was
  reported as is. A stored reading ahead of the clock no longer blocks new polls, and status never reports
  `observed_at` after now. `api state --cached` replayed Codex usage frozen at the last full `state`
  (hours old); it now overlays the supervisor's current Codex usage (a local read, no provider contact,
  2 s bound) and, when the supervisor does not answer, marks the frozen value `stale` by its age and clamps a
  future `observed_at` (adding `usage.observed_at_clamped_from`).

### Shared Claude customisation: agents, skills, commands (1.46.0, skills withdrawn in 1.46.1)

MINOR, additive. Capability `shared_claude_state` (names extended; `core_names`, `custom_names`,
`custom_since_contract`, `custom_merge`, `per_account` added).

After `api switch` to another account, user-level custom subagents vanished because only the four
conversation directories were shared. The shared set now also contains `agents` (custom subagents),
`skills` and `commands` (slash commands), the user-level directories of the Claude config dir. They use
the same store (`<root>/shared/claude/<name>`), the same trusted-link rule, the same migration (reflink
backup, `RENAME_EXCHANGE`, journal) and the same `shared status` reporting (`names.<name>`, and `linked`
is true only when all six are links (seven in 1.46.0, before `skills` was withdrawn)).

- **Per account on purpose.** `settings.json` and credentials are not shared: values such as `statusLine`
  and `permissions` may legitimately differ between accounts, and credentials are the account.
- **Launch is never blocked by these three.** `ensure_links` links a missing or empty `agents`, `skills` or
  `commands`; a real directory that holds data is left untouched and listed under `unmigrated` in the
  `shared_state` of the launch result until `shared migrate` runs. (For the four conversation names such
  data still fails with `SHARED_STATE_UNMIGRATED`.) An account switch depends only on the four conversation
  names.
- **Migration merges instead of reporting conflicts, for these three names.** When the store already holds
  the directory (a second account), each file present on both sides is resolved: identical content is kept
  once; for different content the file with the newest modification time stays in the store (the shared copy
  on a tie) and the other is moved, never deleted, to `<backup_dir>/set-aside/<name>/<relative path>`
  (a numeric suffix if that exists). Files only on one side move into the store. Entries that cannot be
  resolved (a file against a directory, a symlink) are still listed under `conflicts` and nothing is
  overwritten. Every account file is also in the reflink backup `<backup_dir>/<name>`.
- **`--dry-run` shows exactly this.** Per name: `would_move`, `would_keep_once`, `would_replace_with_newer`,
  `would_set_aside`, `conflicts` and `report`, one line per resolved file:
  `identical agents/a.md: kept once`,
  `differs agents/b.md: account copy is newer, kept in the shared store; the previous shared copy is set aside at <backup_dir>/set-aside/agents/b.md`,
  `differs agents/c.md: shared copy is newer, kept in the shared store; the account copy is set aside at <backup_dir>/set-aside/agents/c.md`.
  A real run reports the same lines under `results.<name>.report` with `duplicates`, `replaced` and
  `set_aside` counts.
- Accounts get the links when they are added (and at every launch) once shared state is enabled, as for the
  other directories. A journal written before 1.46.0 is resumed with the three names added.

### Statusline refresh and `skills` no longer shared (1.46.1)

PATCH. No request or result shape changes except the lists noted below.

- **Statusline refresh.** The `--settings` that sessionsation injects on Claude launch and resume now
  carries `"refreshInterval": 300` in `statusLine`. Claude Code re-runs the statusline command every N
  seconds even while the session is idle, so `usage.json` no longer goes stale for sessions that are driven
  only by background messages. New config key `claude_statusline_refresh_seconds` (default 300; `0`
  omits `refreshInterval`, restoring the previous behaviour).
- **`skills` is no longer shared.** A live `skills/` holds `skills/synced/<org-bucket-id>`, claude.ai
  cloud-synced skills that are specific to each account, so linking it across accounts mixed them up. The
  shared set is `projects`, `file-history`, `todos`, `session-env`, `agents`, `commands`: `names` in
  `shared status` and `api capabilities` has six entries, `custom_names` is `["agents","commands"]`, and
  `linked` is true when those six are links. `skills` is never linked, migrated or reported as `unmigrated`.
  A `skills/` that an earlier 1.46.x run linked is left as it is. A migration journal from 1.46.0 that listed
  `skills` without having moved it is resumed with that entry dropped; nothing in `skills/` is touched.

### Launch ack timeout and abandoning an uncertain launch (1.46.2)

PATCH. No request or result shape changes except `api capabilities.abandon_operation.applies_to`,
which is now `["resume_launch","launch"]` (plus `launch_phases: ["uncertain"]`).

- **Ack timeout is decided atomically.** `api launch` waits up to 120 s (was 30 s) for `launch-run` to
  acknowledge. When the wait expires while the receipt is still `dispatching`, `launch-run` never claimed it
  and can no longer start (it requires `dispatching`), so the receipt becomes `failed` with detail
  `LAUNCH_RUN_NEVER_STARTED: provider was not started` and the name reservation is released. Only a launch
  that `launch-run` did claim stays `uncertain` (`LAUNCH_ACK_UNCERTAIN`).
- **`launch-run` is loud.** Started after the receipt left `dispatching`, it prints the phase and
  `LAUNCH_ALREADY_DISPATCHED` to stderr in the pane and starts nothing.
- **Recovery.** `api abandon-operation --operation REQUEST-ID --confirm REQUEST-ID` now also ends a new launch stuck in
  `uncertain` whose worker has stopped: terminal `abandoned` (`abandoned_from: "uncertain"`), name released,
  nothing sent. Refused with `SESSION_EXISTS` when a catalog session with that id exists,
  `SESSION_PROCESS_LIVE` when the launch pane still runs something other than an idle shell,
  `SESSION_STATE_UNVERIFIED` when the pane state cannot be read, `OPERATION_NOT_STUCK` for any phase other
  than `uncertain`.

### Codex sender and origin detection (1.46.3)

PATCH. No request or result shape changes.

Codex tool commands run under the daemon-owned app-server, not under the TUI attachment process, so the
process-ancestry match missed Codex sessions: native sender registration fell back to `sessionsation` and an
omitted `origin` was detected as `operator`. After the ancestry match fails, the caller's own `CODEX_THREAD_ID`,
then that of each ancestor (read from `/proc/<pid>/environ`, only that one variable, bounded hops, so it also
works under `env -i`), is mapped to the catalog session with provider `codex` and that `conversation_id`. The
ancestry match still wins when both apply. An unknown thread id resolves as before (`sessionsation` sender,
`operator` origin).

#### Native frame sender envelope (1.46.3)

Claude Code renders a native peer frame as the collapsed `@<name> ❯` row only when
`message.content` is a well-formed envelope (the sender registry entry is never used for rendering). Every
native (peer) frame now carries

```
<cross-session-message from="<from>" from-name="<name>" from-mode="bypass|prompting">\n<body>\n</cross-session-message>
```

- Fixed attribute order; `from-session`, `hop-chain` and `from-plugin` are omitted. `from` is the frame's
  own `from` value (the reply socket address), percent-encoded except `[A-Za-z0-9:_/.-]`.
- `from-name` is the sender's session name (`caller_origin::sender`, resolved in the calling process when the
  message is admitted, since the detached worker is not the caller's descendant), with `"`, `<`, `>`, control
  characters and newlines removed, capped at 64 characters, `sessionsation` when empty or when the caller is
  not a managed session.
- `from-mode` is the SENDER's permission class, never the receiver's: Claude sender, `bypass` when its
  attachment process was launched with `--dangerously-skip-permissions` or `--permission-mode
  bypassPermissions`; Codex sender, `bypass` when the latest `turn_context` in its rollout records approval
  `never` with sandbox `danger-full-access`; everything else (operator, system, unknown, any read failure, a
  mode changed mid-session) is `prompting`. A mismatch with the receiver's permission class makes Claude hold
  the message for approval; that is intended and is not worked around.
- The body is escaped so it cannot close or forge an envelope: every `</cross-session-message` and
  `<cross-session-message` inside it gets a zero-width space (U+200B) after the `<`.
- `native_sender_registration` is now a documented no-op (the config key still loads; nothing is registered
  and the worker writes no registry entry). Receipts no longer gain `sender_registration`; records written
  by 1.45.2 keep any value they have.

### Codex messaging waits for the ledger (1.46.4)

PATCH. No request or result shape changes.

Since 1.46.3 agent sends to Codex sessions go through the public messaging path, so several agents sending and
polling receipts at once collided on the single messaging ledger lease and failed at once with `ADAPTER_BUSY`
(`ok:false`, retryable) before acceptance. Now:

- `api send` (Codex), `api receipt`, `api cancel` and the legacy `messaging` enqueue/status/observe/events/cancel
  wait for the lease: up to 20 s for sends and 10 s for reads, and for the per-session ownership lock
  (otherwise `OPERATION_BUSY`). `ADAPTER_BUSY` is returned only after that budget runs out.
- The lease is no longer held across Codex app-server I/O (status read, `thread/queue/add`, queue reconciliation,
  reply observation). A submission is journalled `unknown` under the lease before the add is sent, so exactly one
  process delivers a message id; every other process only reconciles. After the I/O the lease is re-taken and
  the result is saved only if the receipt is unchanged, so a stale result never regresses a state recorded
  meanwhile (cancelled, observed, terminal).
- A send to a busy (mid-turn) target is unchanged: `queue` queues natively, `submit` is refused with
  `target_busy`, `steer` steers.

### Codex steer waits, cancel waits, `if_busy` (1.46.5)

PATCH, plus one optional request field. Defaults are unchanged.

- **Steer no longer fails fast.** A `steer` that overlapped another send was rejected with reason
  `operation_busy` (the session or jobs queue lock was held for a moment). It now waits for session ownership
  (up to 20 s) and for the messaging ledger, and releases the ledger lease during the Codex status read, the
  steer RPC and the anchor read. The `unknown` / `steer_dispatch_intent` journal is still saved under the lease
  before the steer is sent, and the result is saved only if the receipt is unchanged, so one message id is
  never steered twice and a state recorded meanwhile is never regressed. `api steer-target` waits up to 10 s for
  the session lock instead of reporting it unavailable.
- **Cancel waits.** `api cancel` of a `native_accepted` message waits for the ledger and session lock, and runs
  the native delete without the ledger lease (`cancellation_uncertain` is journalled first; a concurrent
  writer's record wins).
- **Recovery and legacy `messaging` commands** wait for the ledger too (`resume` send and recovery
  reconciliation, `modes sending`, `modes events`).
- **Stale errors cannot overwrite a settled receipt.** When a delivery error is recorded on a receipt that another
  sender of the same id already advanced, the stored record is returned and kept.
- **`if_busy` on `submit` (new, optional).** `submit` refuses with `target_busy` when a turn is active; a sender
  that checked `steer-target`, saw no turn and lost the race to a starting turn got that rejection. The mode
  `queue` already means "dispatch now when idle, queue behind the active turn when busy", so a new optional
  request field `"if_busy":"queue"` on a Codex `submit` applies exactly that policy: an idle target takes it at
  once as before, a target whose turn started meanwhile gets it queued natively (`native_accepted`,
  non-steering) instead of `rejected`/`target_busy`. Any other value, or `if_busy` with another mode, is
  `INVALID_REQUEST`. The receipt echoes `if_busy`. `"steer"` is deliberately not offered: a steer needs the
  exact `expected_turn_id` and runtime generation of the turn it targets, which a submit does not have. For
  Claude and Gemini targets the field is accepted and ignored. Omitted, behaviour is unchanged.

### Launch status and validate no longer rescan state (1.46.6)

PATCH, no schema change. `api launch status|list|validate` and `api resume-launch status|list|validate` only
read receipts, but each ended with a full refresh of the cached state snapshots (Active, Archived and All, three
scans), so a status poll or the "Checking choices" validate took tens of seconds to minutes on a large archive.
They no longer rescan anything and return as soon as the receipt is read. Mutating lifecycle calls (`resume`,
`stop`, `restart`, `switch`, `move-space`, `launch start`, `resume-launch start`) patch the session generation in
the snapshots at once and refresh only the Active snapshot (in line for a named session, in a detached background process for `launch start`, `resume-launch start` and `move-space`); the launch,
resume, switch and move workers likewise refresh only Active when they finish. Archive, restore and delete
workers still refresh all three. `state --cached` after a launch may show the new session in the Active view
before the All view, until the next full `state`.

### Account plan (1.46.7)

PATCH, additive field. Every account row of `api state` (live and `--cached`), `api account list` and the
supervisor status carries `accounts[].plan`, the subscription plan the provider last reported, so tools stop
hand-maintaining it. It is read-only and non-secret; capabilities advertises it as `account_plan`.

```json
"plan": {"provider":"codex","raw":"prolite","name":null,"rate_limit_tier":null,"source":"usage_snapshot","verified_at":1762790000}
```

- `raw` is the provider value exactly as reported (never normalised or guessed). `name` is a human label only
  where the meaning is certain (Claude `pro` "Claude Pro" only; Codex `free`, `plus`, `pro`,
  `team`, `business`, `enterprise` as "ChatGPT <Plan>"); any other value, such as `prolite`, has `name: null`
  with `raw` kept. A Max 5x/20x multiplier is never inferred: `rate_limit_tier` is the string `claude auth status`
  reports as `rateLimitTier` when it reports one, otherwise null. Codex has no tier (null).
  Claude `auth status` reports `subscriptionType` `max` for both the Claude Max and the Claude Ultra plans, so raw
  `max` covers both and `name` stays null for it (since 1.46.8; earlier 1.46.7 labelled it "Claude Max").
- `source` is `auth_status` (Claude: the supervisor's own `claude auth status --json` probe, run on its account
  poll, `account refresh` and enrollment) or `usage_snapshot` (Codex: `planType` of the last rate-limit
  snapshot). `verified_at` is the unix time of that observation (Codex: the usage `observed_at`), not of the read.
- The last Claude observation is kept in the account registry, so it survives restarts and shows in
  `state --cached` without a probe; the stored time is refreshed at most hourly while the value is unchanged.
  A failed or logged-out probe keeps the previous observation. Codex `plan` follows the supervisor's usage
  snapshot in cached state like `usage`. Gemini and Antigravity report `plan: null`. A not yet observed plan is null.
- `usage.json` (the Claude statusline recorder) is unchanged; it carries no plan.

### Auto Start already running (1.46.8)

PATCH. When an Auto Start or Auto Recover attempt finds the session already running (a boot or array turn arrives
after the session was started some other way; start refuses `SESSION_ALREADY_RUNNING`), `last_result` is now
`state: "already_running"`, `code: "ALREADY_RUNNING"` (detail keeps the refusal text) instead of `failed` /
`RECOVERY_FAILED`. Nothing was started, so the attempt is not counted in `recoveries_last_hour` / `starts_last_hour`
and never contributes to `max_per_hour` (`rate_limited`), and it does not suppress the session. The boot or array
epoch is still marked handled, so it is not retried within the same boot. The deprecated `auto_recover` projection
reads it as `waiting`. Capabilities `auto_start.last_result_states` lists `already_running`.

### Codex compaction at the next idle point (1.47.0)

MINOR, new action. Leads compact Codex agents at phase boundaries, but typing `/compact` into the Codex TUI
is refused while a task is in progress, and incoming native messages keep starting new turns. Sessionsation
speaks the app-server protocol the TUIs already run against, so it queues the compaction and fires the
protocol request (`thread/compact/start`, empty response; the work is an ordinary turn with a
`contextCompaction` item) at the first true idle point. Nothing is typed into a terminal.

```sh
api session compact --session ID --expected-generation G --when-idle [--expires-at UNIX] [--operation UUID]
api session compact-status (--operation UUID | --session ID)
api session compact-cancel --operation UUID        # or: api cancel-operation UUID
```

`--when-idle` is required (the only mode; it reserves room for an immediate mode later). `--expires-at` is a
unix time like `api send`, default one hour from now, at most 24 hours (`INVALID_EXPIRY`). `--operation` is a
caller-chosen UUID (generated when omitted).

**Result** (all three commands; `api operations` lists the same rows with `kind:"compaction"` while pending
and for an hour after they finish):

```json
{"id":"UUID","kind":"compaction","provider":"codex","when":"idle","session_id":"...","accepted_generation":"...",
 "provider_conversation_id":"...","phase":"waiting","terminal":false,"reason":"turn_active",
 "detail":"A turn is active; the compaction starts when it ends.",
 "hold":{"active":true,"released":false,"scope":"new native input for this session"},
 "created_at":0,"updated_at":0,"expires_at":0,"started_at":null,"finished_at":null,"attempts":0,
 "compaction_turn_id":null,"can_cancel":true,"can_respond":false,"can_recover":false,
 "non_steering":true,"replays":false,"existing":false,"held_messages":["message ids"]}
```

`existing` is returned by `compact` (true when the call returned an operation that was already pending);
`held_messages` (ids, arrival order, null when the ledger is busy) by `compact-status`.

- **Phases.** `waiting` (journaled, not started), `starting` (journaled just before the app-server call),
  `running` (the app-server accepted it), then `complete`, `failed`, `cancelled` or `expired`. `reason` says
  why a `waiting` operation has not started: `turn_active`, `native_queue_not_empty`,
  `approval_or_user_input_required`, `session_not_running`, `session_busy`, `native_sending_disabled`,
  `explicit_resume_required`, `provider_unavailable`, `state_unverified`, `provider_rejected`; and why a
  finished one ended: `completed`, `cancelled`, `expired`, or a failure code (`generation_changed`,
  `provider_rejected`, `unsupported_native_version`, `compaction_failed`, `compaction_interrupted`,
  `compaction_not_observed`, `compaction_timeout`).
- **True idle.** The supervisor starts it only when the thread has no active turn (the same fresh native
  status the Codex send path uses: `idle`, or a settled `errored`/`interrupted` last turn), is not waiting on an
  approval or question, and its native queue is empty, all checked under the session ownership lock every send
  takes, so no input can begin between the check and the start. Input that was already accepted into the native
  queue before the request cannot be paused (the native queue has no pause) and runs first; the compaction waits
  until it has drained, which new input no longer prevents because of the hold below. `--when-idle` does not wait
  forever: it expires at `expires_at`.
- **Hold.** While an operation is pending (a `waiting` one stops holding at its `expires_at`, even if the supervisor
  is down), `starting` or `running`, a public `submit` or `queue` send to that Codex session is not added to the
  native queue: the receipt stays `pending_local` with `reason:"compaction_hold"` (and a `reason_detail`) and the
  message is bound to its id as usual. Held paths: `api send` modes `submit` and `queue` (also used by questions
  and notices). Not held: `steer` (refused with `target_compacting` since 1.48.1, see below) and `resume`, the legacy `messaging enqueue` command and the `messaging modes`
  commands (they have no public message row to release). The hold lasts until the held input is released, not just
  until the operation ends, so a new send cannot overtake a held one (`hold.released` turns true when a release pass
  found nothing left to deliver or retry).
- **Release.** When the operation completes, fails, expires or is cancelled, the supervisor re-sends the held
  messages through the ordinary idempotent send path (same message id), in arrival order, at most 20 per tick
  and after all pending operations were advanced, so a backlog cannot stall another compaction. Every released
  message is sent with **queue semantics**, including one that was a `submit`: after the first one starts a turn a
  submit would be refused as `target_busy`, so the native queue runs them in order instead. The receipt stays honest:
  `mode` is still `submit` and carries `hold_release:{from:"compaction",delivered_as:"queue"}`. A failed or
  uncertain step stops the pass at that message (later ones never overtake it): the receipt keeps `pending_local`
  with `reason:"compaction_release_retry"` and the next tick re-queries and retries, until the message's own
  `expires_at` makes it `expired`. A held message's `expires_at` and `api cancel` work as for any unsent message,
  nothing is lost or sent twice (an uncertain add is never replayed), and the release survives a supervisor restart.
- **One per session.** A second `compact` for a session with a pending operation (or the same `--operation` id)
  returns that operation with `existing:true` and creates nothing, whatever its expiry.
- **Cancel.** Only while `waiting`: the operation becomes `cancelled`, held input is released, and a repeat is a
  no-op. Once the start was attempted it is `COMPACTION_ALREADY_STARTED`; after it finished,
  `COMPACTION_ALREADY_FINISHED`. `api cancel-operation` accepts the id too.
- **Background terminals and rejected starts.** A thread with only background terminals and no active turn is
  idle by the rule above. If the app-server nevertheless answers `thread/compact/start` with an error (for
  example because it counts background work as in progress), the operation returns to `waiting` with
  `reason:"provider_rejected"` and is retried at the next check, at most 3 attempts in all, then `failed`
  (`provider_rejected`) with the hold released. Nothing was started in that case, so no turn is repeated. An
  unanswered start (timeout, dropped connection) stays `starting`: it is resolved only from the thread's own
  turns (a compaction turn appeared: `running`/`complete`) and is never issued again, since a slow app-server could
  then compact twice. If none appears within 30 seconds of the thread being idle, or nothing is observable for
  3 minutes (1.47.1), it fails `compaction_not_observed` and releases the hold.
- **Completion** is observed from the thread's recent turns: the first turn that was not in the baseline taken
  just before the start and carries a positive `contextCompaction` item. A turn whose summary shows a user message
  is never it; any other new turn is read once in the full view, exactly that turn (`thread/turns/list`,
  `itemsView:"full"`, `limit:1`, positioned with the server cursor; since 1.47.1, see below). `completed` is
  `complete`; `failed` or `interrupted` is `failed`; a run that is still going after 30 minutes, that never shows
  as a turn once the thread is idle again, or that shows no observable progress for 3 minutes (below), is `failed`
  (`compaction_timeout`, `compaction_not_observed`) and releases the hold.
- **Restarts.** The operation is bound to the native runtime generation at request time. If the session process
  was replaced before the start, it fails `generation_changed` and releases the hold. A pending operation does
  not block `stop`, `restart` or `switch`.
- **Refusals.** `COMPACT_UNSUPPORTED` for Claude (Claude agents compact themselves with `/compact`), Gemini and
  Antigravity sessions; `STALE_TARGET` or the usual session errors for a bad generation; `SESSION_NOT_RUNNING`
  when the Codex session has no live app-server attachment; `INVALID_EXPIRY`; `INVALID_OPERATION_ID`;
  `OPERATION_ID_CONFLICT` (the id belongs to another session); `OPERATION_NOT_FOUND`.
- **Journal and capability.** Records are `operations/compactions/ID.json` (retained like other finished
  operations). `api capabilities` advertises `session_compact` (providers, phases, reasons, expiry bounds, hold
  semantics). The supervisor must be running for a queued compaction to start
  (`SESSIONSATION_COMPACTION_SECONDS`, default 2, 0 disables the loop; a panic in one pass is caught, logged to the daemon log and retried). The native app-server profile is the one
  messaging already requires (`unsupported_native_version` otherwise).

### Compaction follow-up observes real compaction turns (1.47.1)

PATCH. 1.47.0 identified the compaction turn by a `contextCompaction` item in the `thread/turns/list` summary, or
else by reading the turn with `thread/items/list`. Real app-servers (0.163.0-0.166.0) list a compaction turn in
the summary view with no items at all (the summary keeps only user and agent messages) and refuse
`thread/items/list` with -32601 ("not supported yet") for legacy-history threads. The follow-up then stopped at that
error on every pass without recording anything: a started compaction stayed `running` and kept holding input until
`compaction_timeout` (30 minutes), although the turn had completed minutes earlier.

- **Detection.** Every new turn whose summary shows no user message is read once in the full view (one turn,
  positioned with the server's own `nextCursor`, its id verified). The `contextCompaction` item is listed there
  once the compaction completed; a still-running new turn without a user message is treated as the compaction in
  progress. `thread/items/list` is no longer used.
- **Bounded, never silent.** A started compaction that shows no observable progress for 180 seconds fails
  `compaction_not_observed` and releases the hold (held input is then delivered with queue semantics, so it never
  overtakes a compaction that is still running). Progress is a pass that read the thread and found it idle, or found
  a running new turn without a user message; failing reads (connection, `thread/read`, `thread/turns/list`, the
  full-turn read) and a thread that stays non-idle with no such turn (for example background work) are not. A long
  compaction turn is therefore bounded by the 30-minute run limit only. The progress clock is process memory: after
  a supervisor restart it starts from the operation's start time. The idle rule is unchanged (idle again with no
  compaction turn after 30 seconds fails `compaction_not_observed`).
- **Diagnostics.** Every pass that cannot decide writes one line to the daemon log naming the operation, the step
  (`connect`, `thread_read`, `turns_list`, `turn_read`, `no_compaction_turn`, `idle_without_compaction_turn`) and
  the error code, at most once a minute per operation and step; the give-up decision is logged too.

No field, phase, reason or command changed.
### Space-scoped session names (1.48.0)

MINOR, additive. A session belongs to a HERDR Space (workspace) and its name is unique only within that
Space. Negotiate `capabilities.session_spaces.version == 1`; `session_names` is unchanged (`version:1`,
byte-identical to 1.47.0).

- **Row fields.** Every session row (`api state`, `api session`, `rename`, `session resolve`,
  `session set-space`) carries `space` (the Space label, or `null` for an unscoped legacy entry), `name`
  (the bare name, unchanged field) and `canonical` (`<space>:<name>`, `null` when unscoped or when the Space
  label itself contains `:`). Clients display `canonical` when present and never re-derive it by splitting.
- **Inactive sessions keep their Space.** A stopped, saved or detached session keeps its stored `space` and
  `canonical`, whether or not the HERDR Space still exists or was renamed. Nothing clears it as a side
  effect (refresh, an unobserved or missing workspace, detach, stop, a backfill rerun). Only `move-space`,
  `session set-space` (including `--clear`), a resume/add-to-herdr into another Space, or a host observation
  of a live attached session placed in a different Space changes it.
- **Generation.** `space` is not part of the generation hash. Recording a Space where none was known (the
  one-time backfill, healing after a rollback, the first host observation of a live session) changes no
  generation. Every explicit change (set-space, move-space, resume into another Space, a live session seen in
  a different Space) also increments `revision`, so it changes the generation like any other mutation.
  Deploying 1.48.0 changes no generation and renames nothing.
- **Grammar.** Bare name: the session-name rules (non-empty, no surrounding spaces, no control characters, no
  `/`, not `.`/`..`, at most 64 characters) and no `:`. Canonical: exactly one `:`, a valid Space label before
  it and a valid bare name after it; anything else (`a:b:c`, `:x`, `x:`, `x: y`) is `INVALID_SESSION_NAME`.
  `#` is allowed. Case is preserved; uniqueness is case-insensitive within one Space (unscoped entries form
  their own bucket). Host observations may still create duplicates (HERDR owns live labels).
- **Targets.** Every `api … --session` (and `--notify-session`) accepts the session id, `<space>:<name>` or a
  bare name: id, then merged id (`SESSION_MERGED`), then canonical, then bare; an exact (case-sensitive)
  match wins over a case-insensitive one; several matches are `AMBIGUOUS_SESSION` (`AMBIGUOUS_SESSION: N
  sessions match "…" (…); use the canonical <space>:<name> or the session id`). A target matching nothing is
  passed on unchanged, so every command keeps its own refusal. JSON request ids (`session_id`) stay ids.
  `api session resolve --session TARGET [--expected-generation G]` returns the row without side effects.
- **Rename.** `api rename --name` takes a bare name or a canonical name of the session's own Space; rename
  never moves (`SPACE_MISMATCH`, also for an unscoped session: use `session set-space`). HERDR always
  gets the bare name. The read-back accepts a tab label `<own space>:<name>` as `<name>`. When the pane has a
  pane label equal to the old name, it is renamed too; the result's `pane_label` is `confirmed`,
  `unconfirmed` (presentation only, not a failure) or `null` (nothing sent).
- **Host observation.** A live HERDR tab label becomes `name` (as before) and the pane's workspace label
  becomes `space`. A label `<space>:<name>` whose prefix is the pane's own Space (case-insensitive) is read as
  `<name>`; any other label with `:` is `INVALID_HOST_NAME` and leaves the cached name unchanged.
- **Placement.** `api resume` places the session, in order: `--space`, `--new-space` (1.45 reuse),
  `--create-space` (the Space of a canonical `--name`, which must not exist: `WORKSPACE_NAME_EXISTS`), else
  the session's stored Space (an existing Space with that label, or a new Space with exactly that label when
  it no longer exists), else the Space of a canonical `--name` (`SPACE_NOT_FOUND`), else a new Space labelled
  `sessionsation` (unchanged). A canonical `--name` must agree with the destination (`SPACE_MISMATCH`), and
  the name must be free there (`SESSION_NAME_EXISTS`); the destination label is stored on a verified start.
  `session add-to-herdr` takes `--space | --new-space | --create-space` the same way. `api launch` and
  `api resume-launch` accept a canonical `name` that must agree with `space` (`SPACE_MISMATCH`); the receipt
  stores the bare name, and the launch receipt records `space_label`. `create_space` is a CLI flag only:
  the JSON requests have no new field, so 1.47 can read every receipt. Inline `run`/`resume --name` take a
  canonical name of the pane's own Space; outside HERDR a canonical name is `INVALID_ARGUMENT`.
- **`api session set-space --session T --expected-generation G (--space LABEL | --clear)`.** Catalog only;
  refused with `ALREADY_IN_HERDR` for a verified-live HERDR terminal (use `move-space`). The Space need not
  exist. Unique in the new Space.
- **Generation and Space.** Storing a Space where none was known (backfill, host observation, the Space
  recorded after a verified `resume`/`add-to-herdr`/launch) never changes the generation. A change between two
  different Spaces (host move, recorded destination) and explicit `session set-space` / `move-space` do. The
  Space recording after a verified start is best effort: a failure is logged, the start still reports
  success, and the next live refresh records the Space. A `resume --name` is checked for uniqueness only in the
  destination Space (case-insensitive, excluding the session itself), never in the old Space. Automatic names
  (discovery, first launch record) avoid the same folded name in every Space.
- **Message sender.** `from-name` of native messages is the canonical name when it fits 64 characters, else
  the bare name.
- **Storage and rollback.** `space` is stored in `catalog.json` (omitted when null) and mirrored in
  `registry/session-catalog/spaces.json`, which older releases never touch. A 1.47 binary reads and rewrites
  `catalog.json` without `space`; the next 1.48 transaction restores it from the sidecar (reported as
  `restored_spaces` by `session repair-identity`). The first 1.48 transaction backfills `space` once from
  placement evidence, else the Auto Start workspace label, else null. Name reservations are keyed by
  `space/name`; a 1.47 binary cannot see them during a rollback (launch admission transients only).
- **New refusal code.** `SPACE_MISMATCH`. Reused: `AMBIGUOUS_SESSION`, `SESSION_NAME_EXISTS`,
  `INVALID_SESSION_NAME`, `WORKSPACE_NAME_EXISTS`, `SPACE_NOT_FOUND`, `ALREADY_IN_HERDR`.

### No steering while a compaction is pending (1.48.1)

PATCH. Up to 1.48.0 a queued `session compact --when-idle` held `submit` and `queue` sends but not `steer`, and
`api steer-target` kept advertising the running turn, including the dedicated compaction turn. Two losses
followed in practice: a note meant for after the compaction was steered into the turn before it, so it landed
ahead of the compaction and was summarized away; and a steer into the compaction turn itself was refused by the
app-server (`compact_turn_not_steerable`), leaving the agent idle after the compaction with nothing to run.

- **Rule.** While a compaction operation holds input for the session (the hold window above: `waiting` before its
  `expires_at`, `starting`, `running`, and a finished operation until its held input is released), steering is not
  used. `api steer-target` returns `available:false`, `active_turn_id:null` and `reason:"target_compacting"`; a
  `mode:"steer"` send is refused before any native call and becomes a terminal `rejected` receipt with
  `reason:"target_compacting"` (`facts.accepted_native:false`, nothing was sent, the id is not reusable).
- **What to do instead.** Send with `mode:"submit"` (Codex: `if_busy:"queue"`) or `mode:"queue"`. That input is held
  (`pending_local`, `compaction_hold`) and delivered as a queued turn after the compaction, in arrival order.
- **Compactions Sessionsation did not start** (a `/compact` typed in the TUI) are still recognised only by the
  app-server's structured refusal: a terminal `rejected` receipt with `compact_turn_not_steerable`, nothing
  delivered. Automatic compaction inside a regular turn keeps accepted steer input (unchanged).
- **Receipts.** A steer receipt is `native_accepted` only on the app-server's matching acknowledgement and
  `execution_started` only once the steer's own `userMessage` item is observed in the turn; a generic native
  rejection or a lost acknowledgement stays `delivery_unknown` (reason `native_rejected` or the transport code),
  which is not a delivery.
- **Envelope.** `ok:true` from `api send` means the request was processed and a receipt exists, not that the input
  was delivered. Read `result.state` (and `facts`): `rejected`, `failed`, `expired` and `delivery_unknown` were not
  delivered (or not provably so). A client that only checks `ok` reports dropped input as sent.

`target_compacting` is a new reason value in existing fields; no field, command or state changed.

### Session model, set-model and one-pass launch (1.49.0)

MINOR. Adds the observed model of a session, `api session set-model` and the one-pass `api session launch`.
Capability `session_model` (version 1); every existing capability object is unchanged (`launch_model` still
lists `codex` for the JSON `api launch`, which since 1.49.0 also accepts `model`/`reasoning_effort` for Claude
terminal launches: exact Claude id, efforts `low|medium|high|xhigh|max`; hosted + model is
`UNSUPPORTED_PROVIDER_OPERATION`; still not checked against the account).

- **Exact ids only.** Claude: `claude-` followed by lower-case letters, digits, dots and dashes, optional `[1m]`;
  aliases never match (`INVALID_MODEL`). Codex: the launch-request model grammar, and for `set-model` the
  account's `model/list`. Applied values are always the ones the session reports; a substituted model or a
  capped effort is a failure carrying the observed value, never "close enough" (ASCII case-insensitive equality).
- **Callers must always pass a model.** `session launch` and `set-model` require `--model`. A Claude `/model` or
  `/effort` (including the ones `set-model` types) saves the value as the account's default for later starts
  without a model; Sessionsation does not restore it. New starts without a model (plain `api launch`, `run`)
  get the last switched-to value. **Resumes** (`api resume`, inline `resume`, Auto Start/auto-recover, account
  switch, hosted resume) pass the conversation's last recorded model and effort as `--model`/`--effort`
  (the row's `model`); with nothing recorded they start as before. Headless `-p` sends do not pass one.
- **Row field `model`** (`state`, `state --cached`, `session`, `session resolve`): `{"id","effort","observed_at",
  "source"}` or null. Sources: `claude_statusline` (Sessionsation's status-line hook records the payload's
  `model.id` and `effort.level` per conversation in `<account home>/.sessionsation/model/<session_id>.json`),
  `codex_thread` (`thread/read` during inventory, kept in `registry/session-catalog/models.json`) and
  `set_model`. Not part of the generation: a model change never makes a target stale. Claude sessions started by
  an older release run that release's hook and report `model:null` until stopped and resumed.
- **`session set-model`** journals `operations/model-changes/<id>.json`: `waiting → applying → complete | failed |
  cancelled | expired`; one pending change per session (same values: `existing:true`; other values:
  `SESSION_OPERATION_IN_PROGRESS`; same `--operation` with other values: `OPERATION_ID_CONFLICT`). Admission
  refusals: `SESSION_NOT_FOUND`, `AMBIGUOUS_SESSION`, `STALE_TARGET`, `UNSUPPORTED_PROVIDER_OPERATION` (gemini,
  antigravity, hosted Claude), `INVALID_MODEL`, `INVALID_REASONING_EFFORT`, `INVALID_OPERATION_ID`,
  `INVALID_EXPIRY`, `INVALID_ARGUMENT`, `SESSION_NOT_RUNNING`, `TERMINAL_NOT_ATTACHED`, `TERMINAL_INPUT_DISABLED`,
  `SESSION_STATE_UNVERIFIED` (Claude: no status-line record from the running process), and the lifecycle codes.
  Already running the requested values: journaled `complete`, `already_applied`, nothing sent. Cancel while
  `waiting` (`applying`: `OPERATION_WORKER_ACTIVE`; ended: `OPERATION_ALREADY_TERMINAL`). Listed by
  `api operations` (`kind:"model_change"`).
- **Codex.** At an idle point (no active turn, empty native queue, under session ownership):
  `thread/settings/update {model}`, then `{effort}`, verified with `thread/read`. Model not offered:
  `model_not_available`; effort not offered: `effort_unsupported`; JSON-RPC error: `provider_rejected`; never
  visible after three attempts: `model_not_confirmed` / `effort_not_confirmed`. New public `submit`/`queue` input
  is held (`pending_local`, `model_change_hold`) and released in arrival order through the compaction release
  path. Compactions and model changes of one session run in request order (waiting reasons
  `compaction_pending`, `model_change_pending`), and held input is released only when neither remains. A change
  stuck in `applying` with no observable progress for 180 s fails `apply_not_observed` and releases its hold.
  Steering is not held by a model change.
- **Claude.** A detached worker types `/model <id>` and `/effort <level>` into the verified HERDR pane only after
  **two** idle observations at least 1.5 s apart (HERDR agent `idle`/`done` and an **empty** main prompt; a draft
  is never cleared: waiting reason `draft_present`) and while no terminal delivery to the conversation is in
  flight or touched the pane in the last 5 s (`terminal_delivery_active`). Waiting changes are retried with
  back-off (Claude 4 s doubling to 30 s, Codex 2 s to 30 s). Text it must remove is only its own command, by
  backspaces after verifying the prompt holds exactly a prefix of it (otherwise `prompt_not_owned`, nothing
  cleared); a command left typed by a dead worker of the same operation is recognized exactly and cleared. A
  dialog is never left open: on every failure the worker chooses `No` (or Esc when the options cannot be read)
  and verifies the main prompt; if that cannot be verified the operation fails `pane_left_in_dialog` and
  `detail` names the pane. Dialog options must match the captured format (`❯ 1. Yes, switch to …`,
  `2. No, go back`). Model ids compare ignoring a trailing `[1m]`. The typed command must read back exactly with no menu; then Enter. A `Switch model?` /
  `Change effort level?` dialog is answered by moving the pointer with up/down to `Yes` and Enter (a
  `PreModelSwitch` hook prompt is declined: `blocked_by_hook`); never Esc, Ctrl+C, Ctrl+D or digit keys.
  Confirmation comes only from the status-line record (the footer text is never parsed). An empty prompt with no
  change for 8 s is `model_not_confirmed` (for example `Model '<id>' not found`); an unidentified screen ends the
  operation with `TERMINAL_OVERLAY_UNIDENTIFIED` and `refusal_layout` instead of waiting. Claude input is not held;
  terminal deliveries wait for the pane lock the worker holds. Screens: `tests/fixtures/claude-screens/model-*`,
  `effort-*` (Claude Code 2.1.314).
- **`session launch`** admits an ordinary launch request (`model`, `reasoning_effort`; nothing new in the request
  or receipt, so 1.48 binaries read both) and journals `operations/session-launches/<request_id>.json` plus the
  brief (`.brief`, 0600, removed when the record ends): `admitted → launching → verifying_model →
  delivering_brief → complete | failed | cancelled`. The brief is **held until the model (and requested effort) is
  verified** (`brief.delivery`: Claude `held`, Codex `first_turn`), then sent as an ordinary message (Claude
  `terminal_input` submit; Codex submit with `if_busy:"queue"`), a new message id journaled before each attempt,
  pre-submit refusals retried every 5 s up to 5 times (only when the receipt shows nothing was written, or
  provably does not exist; any other read error is retried as a read, never as a send, so the brief goes out at
  most once), `delivery_unknown` never retried. A deadline that passes while a send is in flight ends the record
  `failed`/`ready_timeout` with `brief.state:"delivery_unknown"`, never `not_sent`. A record with no launch
  receipt 120 s after admission ends `launch_failed`. Opt-in
  `--brief-delivery initial-prompt` (Claude) passes the brief as Claude's start-up prompt (`handed_to_provider`,
  `delivered` once the transcript holds it) and cannot be gated on the model. Mismatch: `failed`
  `model_not_confirmed` / `effort_not_confirmed` / `effort_unsupported`, brief `not_sent`, session left running.
  Deadline: `ready_timeout`. Brief limits: Claude held 8192 bytes and terminal-input rules, initial prompt 32768
  bytes and not starting with `/ ! #`, Codex 16384 bytes. `--wait` ends without `complete` as
  `MODEL_NOT_CONFIRMED`, `PROMPT_NOT_DELIVERED`, `PROVIDER_LAUNCH_FAILED` or `LAUNCH_ACK_UNCERTAIN` with
  `error.operation`. New error code: `MODEL_NOT_CONFIRMED`.

Rollback: drain first (see [operations](operations.md#data-root-layout-contract-1290)). An older binary ignores `models.json`, the hook records and both journal directories; pending
one-pass launches freeze and end `ready_timeout` (brief unsent) after re-upgrade.

### Launches wait for account operations (1.49.1)

PATCH. Up to 1.49.0 a launch tried the shared account lock once. While an account operation held it exclusively
(an archive holds it for its whole run, often 15 s or more), a `launch-run` refused at once with `ACCOUNT_BUSY`,
the launch record ended `uncertain` although nothing had been started, and `launch-run` returned to the pane's
shell with no output.

- **Wait.** Codex `launch-run` and inline `run` launches now wait for the account lock (backoff 50 ms doubling to
  1 s) for up to **120 s**. While waiting, the launch stays `starting_provider` with `detail` "waiting for an
  account operation (such as an archive) to finish; nothing has been started (ACCOUNT_BUSY wait)"; the worker's
  acknowledgement wait does not count that time. An inline launch prints a waiting line to its terminal. The
  exclusive side (archive, rename, removal) is unchanged and still refuses at once while a launch holds the lock.
- **Pre-start refusals are certain failures.** A `launch-run` error before the first provider mutation (Codex:
  before the app-server thread start), including `ACCOUNT_BUSY: an account operation held the lock for 120s`,
  ends the launch `failed` with the error in `detail` and releases the reserved name. `uncertain` is kept for
  failures after a provider start may have happened (and for Claude, Gemini and Antigravity terminal launches,
  which do not mark that point).
- **Never silent.** On any error `launch-run` prints `sessionsation launch failed: <CODE>: <reason>` to the pane's
  stderr (the same text as the record's `detail`) and exits 1.

No field, phase, command or error code is new.
