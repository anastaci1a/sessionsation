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
process never proves absence of deliberate stop/pause intent. Interrupted/error
turns require review in these conservative hints. Unknown queue depth remains null.
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
idempotency storage is bounded at 10,000 and never silently evicts replay protection.
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

Steering receipts use `delivery_semantics:"deliberate_in_turn_steer"` and are
**acceptance-only**. `acknowledged_turn_id` is present only after a matching native
acknowledgement. `facts.native_acceptance` distinguishes `accepted`, `unknown`
(possible dispatch), and `not_attempted`. A lost/malformed acknowledgement or
unclassified native error remains `delivery_unknown`; it is not safe to resend.
The inspected summary protocol cannot reliably correlate a steer with a distinct
reply. Therefore `reply_correlation:"unsupported"`, `input_recorded:false`,
`input_consumed:"unknown"`, `turn_completed:"unknown"`, `reply_observed:false`,
and null `reply`/`correlation` remain explicit. Receipt polling never borrows the
active turn's eventual reply or attempts delivery. The native queue ID is null.

Schema definitions `SteeringIntent`, `SteeringTarget`, and the conditional
`MessageRequest`/`Receipt` rules are in [v1.json](contract/v1.json); request
examples and accepted/uncertain receipts are in [fixtures.json](contract/fixtures.json).
Older requests/receipts remain valid. Clients must negotiate this capability,
retain their original UUID/request, and handle acceptance separately from delivery.

### Explicit Resume-and-submit

API v1 contract revision 3 adds `api send` with `mode:"resume"`. Negotiate
`message_recovery` from `api capabilities`; the older provider `resume` flag
refers to saved-session launching and does not imply this message mode. Codex's
verified native protocol profiles are **0.163.0 and 0.164.0**. Claude recovery is unsupported.
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
quiescence is unsupported. The 10,000-entry bound never evicts replay protection.

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

For **that invocation of `api send`**, `ADAPTER_BUSY` precedes request binding,
admission and native dispatch. It does not negate an earlier admission of the same ID.
Ordinary messages bind in SQLite; explicit Resume first binds in its recovery
journal before any launch. Submission uncertainty is saved before touching the
queue socket. There is no automatic receipt or
binding eviction. For `api receipt`, `ADAPTER_BUSY` means lookup could not run;
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
| expired | Submission deadline passed while provably unsent |
| cancelled | Local cancellation or positive supported native cancellation |
| failed | Correlated native turn failed/interrupted; no successful reply |

`facts` contains bounded positive booleans for local admission, native acceptance,
recorded input and observed reply. `input_consumed` remains unknown. Native
acceptance survives expiry; TTL is not native execution cancellation. The schema
fixtures include reconnect errors, acceptance after expiry and projection recovery.

## Account operations

`api account list|add|login|enable|disable|usage|limits|remove|restore|delete|deletions` are documented in
[the CLI guide](cli.md). Removal is local membership removal, not provider deletion,
credential purging or disabling the original switcher enrollment. The separately
confirmed `delete` operation deletes local credentials/data while preserving
conversations; it requires a capable sole owner. Arbitrary history purging remains
unsupported. Claude is explicitly `SETUP_REQUIRED`; no enrollment,
usage, account switching or cross-provider conversion is claimed for it.

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

Account removal previews scan metadata with four bounded readers and deterministic
ordering. Their revision and fresh-confirmation checks still include the same
stored-file metadata; no cached or partial inventory grants consent.

Session snapshots/details optionally include `conversation_created_at` (the native
recorded timestamp, otherwise null) and `history_file_bytes` (observed rollout
bytes, excluding shared account databases). Creation is never inferred from mtime.

## Conversation archives

Capability `conversation_archive.version:1` advertises list/archive/restore/delete,
`compression:"gzip"` and `storage:"sessionsation_data_root"`. `api archives`
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
