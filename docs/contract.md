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

`api capabilities` publishes provider-specific operations and unsupported reasons.
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
- Session names are unique across the catalog; rename is atomic and retains both
  identities. `SESSION_NAME_EXISTS` rejects a collision.
- Account IDs are provider-qualified, such as `codex:example`. A removed membership
  remains represented for recovery with `membership:"removed"` and `selectable:false`.
  Disabled memberships remain enrolled. Use `selectable`, not name or plan, for selection.

Session capabilities are fresh snapshot hints; the backend revalidates them at the
mutation boundary. Unmanaged external sessions are not lifecycle targets through v1.

## Passive readiness

Aggregate state/watch reports attachment availability, not native activity. For a
selected session use:

```sh
sessionsation api observe --session BACKEND-ID --expected-generation GENERATION
```

This performs read-only native status/public-summary requests on the existing
owner-managed socket. It never sends input, resumes/loads a thread, starts a worker
or probes with a message. Native observation is bounded to five seconds, in
addition to normal host/catalog discovery. The result is typed by `Readiness` in
the schema: idle/working/interrupted/errored/disconnected/unknown, diagnostic reason,
`observed_at`, `valid_until`, freshness and a separate `runtime_generation`.
Freshness expires five seconds after observation starts; it is not a reservation.
A changed catalog or runtime rejects the result as `STALE_TARGET`.

`explicit_intent` reports a positively recorded stop token or unknown; an idle
process never proves absence of deliberate stop/pause intent. Interrupted/error
turns require review in these conservative hints. Unknown queue depth remains null.
Submit is available only for positively idle eligible observations; Queue may be
available while working. Sending-disabled, removed-account, stale, explicit-stop
and unknown observations withhold readiness with reasons. These are conservative
hints, not a new dispatch policy: actual sending still rechecks its existing gates.
No legacy busy-Steer capability or messaging Resume is adopted. Poll a selected
session no faster than every five seconds; use backoff after errors.

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

### Admission and contention

The messaging ledger has one exclusive `adapter.lock` across legacy and v1
send/status/receipt/cancel operations. It is held through native dispatch or
observation. Contention returns `ADAPTER_BUSY` without waiting; an adapter-local
serialized lane reduces self-contention but cannot coordinate other processes.
Do not remove locks, stop legacy observers or introduce another owner to bypass it.

For **that invocation of `api send`**, `ADAPTER_BUSY` precedes request binding,
admission and native dispatch. It does not negate an earlier admission of the same ID.
The public binding and receipt are durable before any native write; submission
uncertainty is saved before touching the socket. There is no automatic receipt or
binding eviction. For `api receipt`, `ADAPTER_BUSY` means lookup could not run;
it says nothing about whether the send succeeded. Preserve operation context and
the original error instead of converting a receipt error into a send rejection.

After contention clears, `RECEIPT_NOT_FOUND` means no v1 binding exists for that ID
in the selected root. Verify the exact root/instance and original immutable request
before drawing an admission conclusion. A wrong root, unbound legacy ID or lost
original error must not trigger a replacement send. This is not a Queue retry or
new model-use authorization. Receipt polling may resume with backoff; uncertain
delivery is never automatically replayed. Historical backend reconciliation and
its limits are linked from [operations](operations.md#legacy-coexistence-and-review-gates).

### Receipt evidence

`accepted_generation` is the original backend generation; the independent
`accepted_runtime_generation` is the original native attachment generation.
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

`new_account_name_max:24` limits new enrollments in both the public API and sole account owner; longer existing IDs
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
with JSON stdin, plus `status --request-id UUID` and `list`. See `ResumeRequest`
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
Session state and details include nullable `resume_block_reason`: `unknown_account`, `host_unavailable`,
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
