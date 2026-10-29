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
and contract revision, without contacting a provider or a supervisor. Version text
alone is insufficient for a reviewed pin; compare artifact hash and capabilities.
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
process never proves absence of deliberate stop/pause intent. Fresh interrupted/error
turns expose `actions.resume.available:true` when otherwise eligible; Submit and
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

Native 0.164.0 and 0.165.0 distinguish automatic compaction within a regular turn from a
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
verified native protocol profiles are **0.163.0, 0.164.0 and 0.165.0**. Claude recovery is unsupported.
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

`api account list|add|login|managed-home|enable|disable|usage|limits|remove|restore|delete|deletions` are documented in
[the CLI guide](cli.md). Removal is local membership removal, not provider deletion,
credential purging or disabling the original switcher enrollment. The separately
confirmed `delete` operation deletes local credentials/data while preserving
conversations; it requires a capable sole owner. Arbitrary history purging remains
unsupported. Since revision 11 `remove`, `restore` and `delete` also accept
`claude:` IDs (see revision 11 below; revision 10 rejected them).
`api account managed-home claude:NAME` (additive) switches an adopted Claude account to
`<root>/claude-accounts/NAME` and returns `{account_id,provider,home,previous_home,relocated,already_managed}`
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

`new_account_name_max:30` limits new enrollments in both the public API and sole account owner; longer existing IDs
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

Revision 17 (unreleased) populates `active_since` for running Claude and Gemini
sessions from the first honest source, never a guess, and leaves it null otherwise
(`last_accessed` stays null for them). Claude: the live registry
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
names are unique across providers. Account snapshots add `provider`,
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
where `auth_mode` is `{logged_in,auth_method,api_provider,subscription_type}` or
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
`DATA_ROOT/claude-deliveries/`; older records without a `provider` field are Claude.
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
`DATA_ROOT/state-snapshot-<filter>.json` (and reports `snapshot.cached:false`);
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

Revision 17 (unreleased) also adds the capability/admission reason
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
`auth_failure_reason` (`AUTH_CLIENT_UNSUPPORTED`, `AUTH_GRANT_INVALID`,
`AUTH_UNAUTHENTICATED` or `AUTH_REAUTH_REQUIRED`, with fixed text) and
`auth_failure_observed_at`. They come from the last recognizable failure in a headless
Gemini run's stderr/JSON error or the `account login` terminal, stored in a
sessionsation-owned file outside the account home (reason code, timestamp and the
credential file's stat identity only; never raw output or credential content). While
set, the row also reports `auth_required:true`, `selectable:false` (a login is needed
although credential files exist) and `auth_detail:"AUTH_FAILED"`. A generic crash never
sets it; a later successful headless run or successful login clears it, and a newer
credential file supersedes it. Interactive Gemini launches are not observed.

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
(Enter would accept a completion). The limit is 4096 bytes after trimming
(`TERMINAL_TEXT_TOO_LONG`). CRLF becomes LF and surrounding whitespace is trimmed
(`text_normalized`). A leading `/`, `!`, `#` or `?` gets a backslash
(`escaped_prefix`), so command, shell, memory and help triggers stay plain text.
The receipt correlates on that delivered text.

Delivery is journaled under `DATA_ROOT/claude-steer/` before every write, one
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
5. The text is typed with `pane send-text` (no bracketed paste) in small chunks,
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
