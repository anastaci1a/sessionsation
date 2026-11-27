# Public CLI contract v1

This is the authoritative interface for clients, including `sessionsation-ui` and
Agent DM. Use `sessionsation api --api-version 1 ...`. API v1 is independent of the
package version. Additive result fields may appear; ignore unknown result fields.
Request objects reject unknown fields. Do not depend on inherited commands or
native-provider compatibility interfaces.

The [JSON Schema](contract/v1.json) describes envelopes, snapshots, sessions,
accounts and message requests/receipts. [Examples](contract/fixtures.json) are
checked alongside the CLI regression tests. Command usage is in [CLI](cli.md),
verification scope in [operations](operations.md), and the per-version history in
the [changelog](changelog.md). Each section here describes current behaviour.

## Contract versioning (semver)

The client contract has a semantic version `MAJOR.MINOR.PATCH`, reported as
`contract_version` (for example `"1.65.0"`) by `api version` and `api capabilities`.
The integer `contract_revision` always equals MINOR (revision N is contract `1.N.0`).
A build that predates `contract_version` is read as `1.<contract_revision>.0`.
`api_version` stays the integer MAJOR (`1`); the package version is independent.
Negotiate with `contract_version` (same MAJOR, MINOR at least what you need) and
prefer `api capabilities` fields over version numbers for individual features.
Capability objects carry `since_revision`/`since_contract` markers where a client
needs them.

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
3. MINOR: add the `since_revision`/`since_contract` marker, update the reference
   section here, add one [changelog](changelog.md) entry, and update
   `docs/contract/v1.json` and `fixtures.json` for new shapes, keeping them
   additive. MAJOR: add a new `contract/vN.json` and migration notes.
4. Update tests (the `contract_version`/`contract_revision` assertions in
   `tests/*.rs`) and `scripts/check_contract.py`.
5. Run `scripts/check.sh` once and review.
6. Release; the `current` symlink moves it live. Clients that follow `current` and
   gate on semver need no change for MINOR or PATCH releases. A MAJOR release needs
   client releases first or in the same cutover.

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

`api version` identifies the executable path, SHA-256, embedded commit,
package/API versions and contract version, without contacting a provider or a
supervisor. Clients negotiate by semver and capabilities and log the SHA-256 for
audit; exact hash pins remain an option. `api capabilities` publishes
provider-specific operations and unsupported reasons: every `providers[]` entry
has flat booleans and an `operations` object (`{supported, reason}` per operation).

`attachment_lookup.version:1` advertises `api attachment --session ID
--expected-generation GENERATION`: a read-only, constant-target catalog/process-epoch
lookup for terminal viewers. It returns the catalog session and attachment with
`observed_at`, `scope:catalog_generation_and_process_identity`,
`starts_process:false` and `sends_input:false`. It does no provider/history
discovery and is not message readiness evidence. A stale generation, missing
attachment or unverifiable PID is refused. Clients still verify the intended
host/socket and pane membership before focusing, then repeat this lookup to reject
replacements.

`api state` publishes accounts, sessions, capabilities and `observed_at` (Unix
seconds). `account_source` is `supervisor`, `offline_snapshot` or `uninitialized`;
offline metadata does not prove an account can presently launch.

- `session.id` is a stable backend UUID. `conversation_id` is a separate opaque
  provider identifier (`conversation_id_source:"native"` for Claude, Gemini and
  Antigravity). Neither is a pane ID.
- `attachment` is null or identifies the current host/process generation. Treat its
  `target` as opaque and discover it again after replacement. A missing attachment
  does not authorize starting another client; inspect `availability` and capabilities.
- `session.generation` changes on rename, account transfer, replacement or loss of
  an attachment. Mutations require the current `--expected-generation` and fail
  with `STALE_TARGET` rather than selecting a replacement.
- Names are display labels, never identities (see
  [session names](#herdr-owned-session-names) and
  [Space-scoped names](#space-scoped-session-names)). Use UUIDs for targets.
- Account IDs are provider-qualified, such as `codex:example`. A removed membership
  remains represented for recovery with `membership:"removed"` and `selectable:false`.
  Disabled memberships remain enrolled. Use `selectable`, not name or plan, for selection.

Session capabilities are fresh snapshot hints; the backend revalidates them at the
mutation boundary. Every false capability carries a `capability_reasons` code
(for example `SESSION_NOT_RUNNING`, `AMBIGUOUS_SESSION`, `ACCOUNT_UNKNOWN`,
`NOT_IN_HERDR`, `SESSION_ALREADY_RUNNING`, `SESSION_RUNNING_STOP_FIRST`,
`HISTORY_UNAVAILABLE`, `SESSION_ARCHIVED`, `UNSUPPORTED_PROVIDER_OPERATION`,
`SESSION_OPERATION_IN_PROGRESS`). Unmanaged external sessions are not lifecycle
targets through v1.

Native sessions (Claude, Gemini, Antigravity) add `native_status`, `native_name`,
`native` (`kind`, `job_id`; `kind:"bg"` for Claude background-daemon sessions),
`pid`, `process_start`, `host_known` and `capability_reasons`, and use availability
`running|stopped|stale|unknown`. `name`/`name_source` are Sessionsation metadata;
`native_*` are the provider's. A launched Claude process that has not yet
registered or written a transcript (for example on its trust prompt) reports
`availability:"running"` with the discovery warning `NO_CONVERSATION_YET`.

**Caller binding** (1.71.3, `capabilities.caller_binding`). Running Codex rows also carry top-level
`pid`/`process_start` (their attachment). Every row has `caller_binding`
`{process:{pid, process_start}, env:{name, value}|null}` and `caller_binding_reason`:
the process under which the session's tool commands run (`process_start` is `/proc/<pid>/stat`
field 22). Codex: the shared app-server serving the conversation (its socket peer; the TUI when
it serves itself) with `env` `CODEX_THREAD_ID` = `conversation_id`. Claude, Gemini and
Antigravity: the native process, `env:null`. A row that is not running, or whose process is
unknown, has `caller_binding:null` with reason `NOT_RUNNING`, `APP_SERVER_UNKNOWN` or
`PROCESS_UNKNOWN`. A caller proves it is the session by finding `process` among its own ancestors
(pid and start time) and, when `env` is set, carrying that variable with that value; `env` alone
is never trusted.

A session admitted by `api launch start` never leaves `state` before its process
verifiably ends (`capabilities.session_lifecycle.launch_listing`). Until the launch has a
catalog entry (phases `admitted` to `starting_provider`, or `uncertain`) the Active and
All views list a row with the request id as `id`, `availability:"launching"`,
`generation` and usually `conversation_id` null, a `launch` block (`request_id`,
`phase`, `detail`, `pane`, `created_at`, `updated_at`) and every capability false with
`SESSION_LAUNCHING`. Every row has `availability_reason`: `LAUNCHING: <phase>`,
`NO_CONVERSATION_YET` (running, no conversation yet), `PROVIDER_EXITED` (a session
Sessionsation launched into a pane whose process is no longer running) or null.

Session snapshots optionally expose `activity`, `last_accessed`, `active_since`,
`conversation_created_at` (the native recorded timestamp, never inferred from
mtime) and `history_file_bytes` (observed history bytes, excluding shared account
databases); see [timestamps](#session-timestamps). Normal provider exit (including
Codex `/exit`) clears the runtime attachment on discovery while retaining stable
identity, conversation files, names and resume availability.

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
their existing checks and confirmations.

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
`availability:archived`, independent of runtime liveness, and remain accessible
through `api archives` and explicit Restore/Delete. Provider archives do not
acquire automatic restore or messaging capability.

Recovered histories from deleted-account vaults remain saved resume sources, even
if their retained provider file is under `archived_sessions`; the optional
`provider_storage_archived` describes storage rather than a current managed archive.
Labels (including prefix variants) are not identity: never merge sessions by
display name or infer an archive from a missing attachment. Discovery/filtering
sends no message, deletes no history and never unarchives or resumes a thread.

### Cached state

`api state --cached [--archive-filter F]` serves the last full result of the same
filter with the identical JSON shape plus `snapshot: {observed_at, fresh, cached}`;
`fresh` is true while the snapshot is at most 30 seconds old. It performs no
transcript scan, process probe or catalog write (`capabilities` is current), but
re-reads the local Gemini authentication-failure record. Every full `api state`
stores its result atomically as `DATA_ROOT/cache/state-snapshot-<filter>.json`
(`snapshot.cached:false`); with no snapshot yet, `--cached` computes a full state.
While it runs, the supervisor rebuilds the `active` snapshot every 20 s, so
`--cached` is at most about 20 s old without anyone running a full `state`; a
stopped supervisor leaves the file as it was and `snapshot.fresh` says so. A
lifecycle job that ends updates the cached rows it changed. `state_cached:true`
advertises it. A cached read never reflects a session started after the snapshot.

A full `state` reuses unchanged per-file transcript facts
(`cache/claude-transcript-facts.json`, keyed by path, size and mtime), runs
inventory and the supervisor status concurrently, takes one `/proc` pass for all
native accounts and resolves deleted-account checks once per account.

### Session identity and continuations

`api capabilities.session_identity` describes the sources. A live process is attached
to exactly one catalog entry, the one whose conversation it currently runs: Claude the
registry `sessions/<pid>.json` `sessionId` (PID and start time verified); Codex the
thread of its single open rollout (tmux TUIs), else its `resume <thread>` argv; Gemini
a process whose command line names the id (`gemini_process_source:
"cmdline_session_id_argument"`); Antigravity the open `presence/UUID.lock` or the
`--conversation` argument (`antigravity_process_source:
"open_presence_lock_or_conversation_argument"`). When a process switches conversation in its terminal, the
existing entry of the new conversation takes over the attachment (name, account,
history kept) and the entry it left is released to `stopped`. A Codex TUI connected
to an app-server (`--remote`) and a Gemini `/resume` do not expose a switch, so it is
not observed.

Claude continuations: when Claude Code sends a conversation to its background daemon
under a new session id, it appends `{"type":"continued-in","sessionId":OLD,
"continuedInSessionId":NEW}` to the old transcript. Only that record is read
(bounded tail; it counts only while no `user`/`assistant` record follows it and NEW
is a transcript of the same account home). The entry of OLD then follows NEW: stable
id, name and account are kept, `previous_conversation_ids` lists OLD, and an entry
already registered for NEW is merged into it. Generation-checked lookups of a merged
id fail with `SESSION_MERGED: ...` naming the surviving id. The survivor is the entry
that already records the lineage, else the pre-existing entry of the earliest
conversation. Aliases and lineage are mirrored in `session-catalog/identity.json` and
restored from it on every catalog access. `api session repair-identity` runs that
restore plus continuation linking and reports `{restored_aliases,restored_lineage,
restored_spaces,merged,aliases,lineages,unresolved_ids,unresolved_reason}`; it never
deletes, recreates or renames an entry. Branches (`forkedFrom.sessionId`) stay
separate conversations, exposed as `forked_from_conversation_id`. No other lineage is
inferred.

### HERDR-owned session names

Negotiate `session_names.version:1`. Inventory/state/watch reads one bounded host
snapshot per server/socket and maps each verified attachment's pane ID to its tab ID
and label, for every provider; it never matches by display name. The catalog
caches the last observed label for host outages and retains it after the process
exits; it is not an independent naming authority for live HERDR sessions.
`name_source` is `herdr_last_observed` for HERDR attachments and `catalog`
otherwise; it is provenance, not freshness. Unavailable/ambiguous host observations
leave the cached name unchanged. Sessions sharing a tab share its label, and HERDR
can contain duplicate labels. New launches and catalog-only renames still reject
name collisions within a Space.

`api rename` validates generation, lifecycle locks and exact host process, renames
the containing HERDR tab, then reads it back before reporting success. A lost
acknowledgement is reconciled by that read, never replayed. Failed verification
returns `HOST_RENAME_UNCONFIRMED` or `HOST_RENAME_NOT_APPLIED`; refresh state before
retrying. Concurrent host edits win on subsequent observation. Inactive and
non-HERDR sessions keep catalog naming. Host labels are neither truncated nor given
synthetic suffixes. Native Claude/Gemini/Antigravity titles are never changed live
(`native_rename:false`); the label is applied as `--name` on the next launch/resume.
Renames do not restart agents, alter conversations or change accounts. Catalog
generations change when a new label is observed; runtime generations do not.

### Space-scoped session names

A session belongs to a HERDR Space and its name is unique only within that Space.
Negotiate `capabilities.session_spaces.version == 1`.

- **Row fields.** Every session row carries `space` (the Space label, or `null` for an
  unscoped entry), `name` (bare) and `canonical` (`<space>:<name>`, `null` when
  unscoped or when the label contains `:`). Display `canonical`; never split it.
- **Inactive sessions keep their Space** whether or not the HERDR Space still exists or
  was renamed. Only `move-space`, `session set-space` (including `--clear`), a
  resume/add-to-herdr into another Space, or a host observation of a live session in
  a different Space changes it. Recording a Space where none was known changes no
  generation; every change between Spaces does.
- **Grammar.** Bare name: non-empty, no surrounding spaces, no control characters, no
  `/`, not `.`/`..`, at most 64 characters, no `:`. Canonical: exactly one `:`, a valid
  Space label before it and a valid bare name after it; anything else (`a:b:c`, `:x`,
  `x:`, `x: y`) is `INVALID_SESSION_NAME`. Case is preserved; uniqueness is
  case-insensitive within one Space (unscoped entries form their own bucket). Host
  observations may still create duplicates.
- **Targets.** Every `api … --session` (and `--notify-session`) accepts the id,
  `<space>:<name>` or a bare name: id, then merged id (`SESSION_MERGED`), then
  canonical, then bare; an exact match wins over a case-insensitive one; several
  matches are `AMBIGUOUS_SESSION`. A target matching nothing is passed on unchanged.
  JSON request `session_id` fields stay ids.
- **Rename** takes a bare name or a canonical name of the session's own Space and
  never moves (`SPACE_MISMATCH`, also for an unscoped session: use `session
  set-space`). HERDR always gets the bare name; the read-back accepts a tab label
  `<own space>:<name>`. A pane label equal to the old name is renamed too
  (`pane_label`: `confirmed`, `unconfirmed` or `null`).
- **Host observation.** The tab label becomes `name` and the pane's workspace label
  `space`. A label `<space>:<name>` whose prefix is the pane's own Space is read as
  `<name>`; any other label with `:` is `INVALID_HOST_NAME` and leaves the cached name.
- **`api session set-space --session T --expected-generation G (--space LABEL |
  --clear)`** is catalog only, refused with `ALREADY_IN_HERDR` for a verified-live
  HERDR terminal (use `move-space`). The Space need not exist; the name must be unique
  there.
- **Placement names.** `api launch`/`resume-launch` accept a canonical `name` that must
  agree with `space` (`SPACE_MISMATCH`); receipts store the bare name and the launch
  receipt records `space_label`. `create_space` is a CLI flag only. A `resume --name` is
  checked for uniqueness only in the destination Space. Automatic names avoid the same
  folded name in every Space.
- **Storage.** `space` is stored in `catalog.json` and mirrored in
  `registry/session-catalog/spaces.json`; a catalog rewritten by a release without
  Space support is healed from it (`restored_spaces`). Name reservations are keyed by
  `space/name`. New refusal code `SPACE_MISMATCH`; reused `AMBIGUOUS_SESSION`,
  `SESSION_NAME_EXISTS`, `INVALID_SESSION_NAME`, `WORKSPACE_NAME_EXISTS`,
  `SPACE_NOT_FOUND`, `ALREADY_IN_HERDR`.

### Session timestamps

`active_since` (Unix seconds) measures the current verified attachment's process
lifetime, not conversation age; it is null when no honest source exists (stopped,
archived, unverified or unobserved sessions). Codex: the verified process start.
Claude: the registry `sessions/<pid>.json` `startedAt` when the registry process is
verified against /proc and `startedAt` is neither before that process start nor in
the future, else the verified /proc start; a session whose process is a pre-started
`claude bg-spare` uses only the verified HERDR `claude attach` client start; a hosted
session without a registry time uses the worker's `started_at`. Gemini and
Antigravity: the verified /proc start.

`last_accessed` is in Unix milliseconds. Codex reports the open/attach time. Claude,
Gemini and Antigravity report the conversation's most recent activity: Claude and
Gemini the `timestamp` (else `lastUpdated`) of the last transcript record from a
bounded, cached tail read (else the file mtime), following continued-in and parked-job
lineage; Antigravity the mtime of the brain `transcript.jsonl`, else of the
conversation db. A missing source, a deleted account or an archived entry gives null.

## Passive readiness

Aggregate state/watch reports attachment availability, not native activity. Its
optional native enrichment has a shared five-second budget and reads only matching
loaded identities when all discovered terminals have conversation IDs. A slow
account supervisor falls back to the stored snapshot after one second;
`account_source` identifies that fallback. `timing` reports inventory, account
status and total milliseconds; these bounds exclude filesystem/host discovery.
`discovery_warnings` marks incomplete native enrichment without discarding
positively discovered identities.

`directory` is the discovered working directory (process/launch or native thread
metadata), not a directory inferred from a display name or HERDR Space. Renaming
or moving a tab does not change it.

For a selected session use:

```sh
sessionsation api observe --session BACKEND-ID --expected-generation GENERATION [--history [N]]
```

It answers for every provider and adds a `conversation` block (see
[observe for every provider](#observe-for-every-provider)); the readiness rules below
are Codex's. It performs read-only native status/public-summary requests on the
existing owner-managed socket. It never sends input, resumes/loads a thread, starts
a worker or probes with a message. Native observation is bounded to five seconds;
known launcher bindings avoid terminal-host discovery and rollout scans. The result
is typed by `Readiness` in the schema: idle/working/interrupted/errored/
disconnected/unknown, diagnostic reason, `observed_at`, `valid_until`, freshness and
a separate `runtime_generation`. `process_state` and `process_observed_at` report a
kernel-validated running, exited or unattached process; an idle native thread is
not a process-stop claim. `observation.status` distinguishes fresh, stale and
unobserved evidence. Freshness expires five seconds after the native evidence was
obtained; cache hits never extend it. It is not a reservation.

For local legacy histories, last-turn summaries are indexed by runtime generation,
canonical history path, device/inode, size and nanosecond modification/change
times. Every reuse still makes fresh native status reads before and after it and
rechecks the file identity; changed files, processes or endpoints invalidate the
index, and concurrent readers share one reconstruction. No history text is stored.
`observation.turn_summary` identifies `native_summary` versus
`unchanged_history_cache` and keeps the original summary's `observed_at` and expiry:
unchanged disk contents cannot exclude a newly started in-memory turn. Cold
reconstruction shares the five-second budget and fails closed when exceeded.
Nonlocal/unverifiable history paths use uncached native observation.

RPC timeouts identify `native_initialize_timeout`, `native_thread_read_timeout` or
`native_turn_summary_timeout`; the overall bound is `observation_timeout`.
`observation.failure` supplies the RPC method (null for the overall budget), budget
in milliseconds, a readable explanation and retryability. Missing evidence keeps
both timestamps null and every action unavailable. Slow provider reads are not
proof of idle readiness; back off instead of forcing refresh. A changed catalog or
runtime rejects the result as `STALE_TARGET`.

`explicit_intent` reports a positively recorded stop token or unknown; an idle
process never proves absence of deliberate stop intent. Fresh interrupted turns
(not failed turns) expose `actions.resume.available:true` when otherwise eligible;
Submit and Queue then report `explicit_resume_required`, matching dispatch. Offer
explicit [Resume-and-submit](#explicit-resume-and-submit) using the observed runtime,
previous turn and stop token; never silently substitute Resume for an ordinary
send. Unknown queue depth remains null.

Stop intent is a permission gate, separate from observed execution: it may coexist
with working state while a stop drains or after continuation outside Sessionsation.
Polling never clears it merely because the agent is working. Newly recorded stop
tokens identify their durable stop-operation journal. Explicit saved-session Open
clears its prior token only after confirming the reopened process, using a
compare-and-clear that preserves any newer stop request; focusing an already-live
terminal does not clear it. Submit is available only for positively idle eligible
observations; Queue may be available while working. Sending-disabled,
removed-account, stale, explicit-stop and unknown observations withhold readiness
with reasons. These are hints: sending rechecks its own gates. Poll a selected
session no faster than every five seconds; back off after errors.

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
than guessing. The private cache is disposable; it stores typed status, not
conversation bodies, and cache hits never renew `observed_at` or `valid_until`.
The compatibility `state` enum is unchanged; additive `runtime_state`
distinguishes a recorded deliberate stop. Stale evidence withholds sending hints.

Clients should batch their visible targets, keep stable IDs as keys, preserve old
receipts, and avoid per-target sequential discovery loops. Timeouts and unknown
states are pending evidence, not proof of a stopped or unavailable agent. A late
provider cannot extend this command's native budget.

### Passive current-turn binding

`api turn-target --session ID --expected-generation GENERATION` reads the latest
native turn identity/status without an old receipt or sending permission.
Negotiate `turn_observation.version:1`. It uses the same owned endpoint and verified
native profiles as checkpoints, validates catalog/runtime before and after, and
compares two bounded summaries. Active, completed, failed and interrupted turns are
supported; empty history returns `available:false` with `no_observed_turn`. It never
launches, steers, clears stop tokens or reads private items. Failures/races return
errors, never guessed targets or renewed stale timestamps. Use its `turn_id` and
`runtime_generation` to bind a new checkpoint stream; existing receipt streams still
reject `checkpoint_stale_turn`. This is turn-associated observation, not delivery or
causal reply evidence, and does not authorize sending. Schema: `TurnTarget`;
fixture: `turn_target`.

### Observe for every provider

`api observe --session ID --expected-generation G [--history [N]]` returns the
`Readiness` keys plus a `conversation` block (`capabilities.observe_conversation`):

```json
{"supported":true,"reason":null,"source":"transcript","history_turns":10,
 "last_reply":{"text":"done","at":1800000000,"truncated":false},"last_reply_source":"transcript","last_reply_reason":null,
 "last_instruction":{"text":"fix it","at":1799999990,"truncated":false},"last_instruction_source":"transcript","last_instruction_reason":null,
 "compacted_at":1799990000,"compacted_at_source":"transcript","compacted_at_reason":null,
 "recent_turns":[{"instruction":{"text":"fix it","at":1799999990,"truncated":false},"reply":{"text":"done","at":1800000000,"truncated":false},"started_at":1799999990,"ended_at":1800000000}],
 "recent_turns_source":"transcript","recent_turns_reason":null}
```

An unknown value is null with `<field>_reason` (`no_history_file`,
`history_unreadable`, `not_recorded`, `not_compacted`, `history_not_requested`, or the
provider reason). `--history` alone means 10 turns, at most 50. Texts are cut at 8192
bytes (`last_*`) and 2048 bytes (turn texts) on a character boundary with
`truncated:true`. Every read is a bounded tail (256 KiB, then 1 MiB, then 4 MiB) of one
provider file; nothing is cached and no provider process is contacted.

| Field | Codex (`rollout`) | Claude (`transcript`) | Antigravity (`transcript_log`) |
|---|---|---|---|
| `last_instruction` | `event_msg` `user_message` | human `user` record (not meta, tool results or the compaction summary) | `USER_INPUT` from `USER_EXPLICIT` (inside `<USER_REQUEST>`) |
| `last_reply` | `event_msg` `agent_message` | text blocks of `assistant` records | `PLANNER_RESPONSE` |
| `compacted_at` | `compacted` / `context_compacted` | `system` `compact_boundary` | null (not recorded) |
| `recent_turns` | instruction to next instruction; `task_complete` / `turn_aborted` end it | same | same |

Gemini is `conversation.supported:false` with the reason "Gemini CLI no longer
supports individual accounts (migrate to Antigravity)", `state:"unknown"`,
`reason:"provider_unsupported"`. Claude and Antigravity readiness: `state`
`idle`/`working`/`errored` from the live activity facts while a verified process runs,
`disconnected` otherwise; `reason` `native_input_via_send` while running (every
`actions.*` unavailable with that reason), `no_live_process` or `session_archived`;
`runtime_generation` null, `delivery_semantics:"api_send"`. `api observations` stays
Codex-only.

`api session resolve --session TARGET [--expected-generation G]` returns the row
without side effects and fills `availability`, `activity`, `pid` and
`live_activity` from the last cached snapshot (`active`, then `all`), adding
`snapshot: {"observed_at","age_seconds","fresh","cached":true,"source":"state_snapshot"}`
(`fresh`: at most 30 s old); a session in no snapshot has those fields null and
`snapshot: null`.

### Live activity and pending operations

Capabilities `live_activity` (with `providers`) and `pending_operations`.
`sessions[].live_activity` is `{state, error, last_turn_status, source, observed_at}`
for running sessions of every provider (null otherwise; also in `state --cached`), and
`activity` is `busy`, `idle` or `unknown`. `state` is `working` (a turn is in
progress), `idle` (ready, no turn seen), `turn_ended` (the latest turn completed or
was interrupted and nothing has started since), `blocked` (waiting on an approval or
user input), `error` (the latest turn failed; `error` holds the provider's text, at
most 300 characters, one line) or `unknown` (no usable evidence; never a guess).
`observed_at` is the time of the evidence, not of the snapshot. Signals are local and
read-only, bounded tail reads (128 KiB, one retry at 1 MiB), never message text:

- Codex: `app_server` (thread status read during the state scan) or `rollout`
  (bounded tail of the history file). `api observe` adds `last_turn:{status,error}`.
- Claude `transcript_tail`: an assistant record with `stop_reason` `tool_use` (or
  still streaming), a `tool_result` or an unanswered user prompt is `working`; an
  `end_turn`/`stop_sequence`/`max_tokens` assistant record or a `turn_duration` system
  record is `turn_ended`; `[Request interrupted by user]` is `turn_ended` with
  `last_turn_status:"interrupted"`; an API-error record is `error`. Claude
  `native_registry`: `<CLAUDE_CONFIG_DIR>/sessions/<pid>.json` `status` (`waiting` is
  `blocked`), used only when newer than the newest transcript record. A session with
  `pending_questions` is `blocked` while a turn is in progress.
- Gemini and Antigravity `transcript_tail`: the last chat/step record (no tool calls:
  `turn_ended`; tool calls, or an unanswered input or tool step: `working`; an
  unanswered input older than 10 minutes, or a step that is not `DONE`: `unknown`).

`sessions[].pending_operations` lists unfinished operations of that conversation as
`[{id, kind, phase, created_at, expires_at, unresolved, resolve}]` (`kind` `stop`,
`pause`, `switch`, `restart`, `resume`, `auto_recover`, `space_move`, `conversation`,
`move` or `message_recovery`; `expires_at` only for a stop). A session blocked by an
`uncertain` or stuck operation reports `RECOVERY_UNRESOLVED` (with
`resume_block_reason:"recovery_unresolved"`), otherwise
`SESSION_OPERATION_IN_PROGRESS`; refusals name the operation id, kind and phase.

### Session meters and session id

Every session row (`api state`, `state --cached`, `session resolve`) has the same
keys for every provider: `context_pct`, `subagents_running` and `background_tasks`,
each with `<field>_source`, `<field>_observed_at` and `<field>_reason`, plus
`context_tokens` and `context_window_size`. A value nobody reported is null with the
reason. `capabilities.session_meters` lists the fields and sources.

| Field | Codex | Claude | Gemini | Antigravity |
|---|---|---|---|---|
| `context_pct` | `rollout`: the newest `token_count` event of a running conversation (`last_token_usage.total_tokens` of `model_context_window`, the raw share; Codex's footer subtracts a 12,000-token baseline first), read by a full `api state` | `statusline`: `context_window.used_percentage`, recorded per conversation by the status-line hook; read on every row | null | null |
| `subagents_running` | null | `hook_events`: subagents started and not stopped in this process, counted only when its `SessionStart` is at or after the row's `active_since` | null | null |
| `background_tasks` | null | null (no hook reports a finish) | null | null |

Every provider process Sessionsation starts for a session carries
`SESSIONSATION_SESSION_ID=<session id>` (the catalog id, not the conversation id), on
every start path: launch and `session launch`, resume (also by Auto Start and account
switch), Codex restart and move, hosted Claude, headless runs and Claude
`add-to-herdr`. For Codex it is also set as the thread's shell environment
(`shell_environment_policy.set` on `thread/start` and `thread/resume`; a thread the
app-server already has loaded keeps its value). An inherited value is removed from
every child and from the supervisor. `capabilities.session_env` describes it.

## Session and operation commands

`api session --session ID --expected-generation GENERATION` returns the current
public session with `directory`, `availability`, `placement` and `original_host`.
Missing information is null/unavailable. Original host provenance comes only from a
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

Lifecycle mutations return journaled operation objects, whose `id`,
`source_session_id`, `kind`, `phase`, `detail` and `confirmation_seq` identify
progress. A returned job is not completed work. Terminal phases include `complete`,
`cancelled`, `failed`, `recovered` and `abandoned`. A failed or uncertain operation
must be inspected before retrying. v1 operations expose `can_cancel`, `can_recover`
and `can_respond` where applicable; consumers must not infer recovery support from a
failed phase alone. Archive operations do not support `api recover`. A second
operation on a session that already has a queued or running one fails with
`SESSION_OPERATION_IN_PROGRESS` (retryable; the message names the queued job id).

Resume, handoff and conversation-archive admissions share a per-conversation lock.
An admitted or uncertain resume reserves its conversation until a settled receipt;
only that same request may continue its transfer. Public session capabilities
disable competing lifecycle actions and expose `pending_operation_id` while
reserved. A terminal handoff remains reserved while its worker holds the worker
lock; a completed/failed label alone does not establish that continuation or
recovery has stopped. Submit/Queue admission also retains this reservation until
the handoff worker exits, including its continuation and rollback tail. This blocks
new native writes; it does not suspend messages already accepted into Codex's own
queue or its terminal composer's local queue (the native queue API has no pause
primitive). Such pending input is not deleted or replayed to simulate a hold; the
handoff retains its source if queued work prevents a safe unload.

Durable launch, resume, deletion and archive workers use separate process groups,
so closing a requesting viewer does not stop their work. v1 operations wait for
positive idle state, send no steer and do not automatically continue work.
Risk/background-stop consent is separate. `api respond OPERATION-ID yes|no
--sequence CURRENT-SEQUENCE` rejects stale confirmations. `api cancel-operation
OPERATION-ID` requests cancellation before an unsafe transition.

`api operations` lists v1 lifecycle, launch, resume, compaction, model-change,
account-rename, reset-credit, auto-start and local account deletion operations; it
does not adopt legacy switcher jobs. `source_session_id` is the stable backend ID,
or null if the source was removed from the catalog; `provider_conversation_id` is
separate. New launch, resume and account-deletion records expose immutable
`created_at` admission time; legacy records omit it when unknown (never substitute
`updated_at`). Public recovery rejects jobs outside this contract. Codex switch,
restart, relocate, resume and launcher resume, and Auto Start, refuse with
`SESSION_UNAVAILABLE` when discovery finds no history source for the session (homes
are compared by resolved path, so a legacy compatibility link does not cause it).

Account deletion records have `kind:"account_delete"`, `account_id`, `id`, `phase`,
`detail`, `updated_at`, `history_home`, `history_preserved` and `can_cancel:false`;
they have no source session. Terminal phases are `complete` and `failed`. See
[account deletion](#account-removal-and-deletion).

`api launcher new --account codex:example`, `api launcher resume --session ID
--expected-generation GENERATION` and `api launcher shell` return an opaque
`{action:"exec",argv:[...],cwd:...}` plan (`--hosted` returns a `run --hosted` plan
for Claude). A terminal client executes the exact argv without a shell, applying cwd
only when non-null. The backend revalidates the session when the resume plan
executes. Do not reconstruct provider launch commands or handle credentials in a
client. Plain-shell launch and every command typed into a pane set
`SESSIONSATION_LAUNCHER_BYPASS=1`, which Sessionsation never reads: it tells a
shell-profile launcher not to start another session.

## New-session launch

`api launch validate|start` read one JSON request from stdin; `api launch list`
lists retained receipts, newest first, without replaying requests, and `api launch
status --request-id UUID` reconciles a durable record. Request v1 has `version`,
UUID `request_id`, provider-qualified `account_id`, `name`, `host: {session,
socket}`, `space: {kind: "existing", id}` or `{kind: "new", label}`, and
`directory: {path, create, confirmed_existing}`, plus optional `permissions`
([launch permissions](#launch-permissions)), `model`/`reasoning_effort` (Codex) and
`hosted` (Claude). Paths are absolute. `create` is an ordered contiguous list of
missing directories ending at `path`; an existing directory requires an empty list
and `confirmed_existing: true`. Validation creates no directories or HERDR
resources. Start revalidates, reserves the name and dispatches once. Reusing a UUID
with different input fails; identical input returns its original record.

Phases are admitted, preparing, creating_terminal, dispatching, starting_provider,
attached, running, failed, uncertain and abandoned. `attached` means catalog/native
identity was recorded; `running` requires post-exec process evidence. Records expose
created directory paths, workspace/tab/pane IDs, stable session ID, generation and
native conversation ID when known. A failure or timeout must not trigger an
automatic new Start; reconcile the original request (uncertain operations keep their
name reservation). Status can reconcile a late attachment after a worker timeout
using the same catalog generation, account, native UUID, PID/start time and remote
socket argv. It never replays commands or changes focus. The receipt's `worker`
(`{pid, start}`) is the spawned `launch-worker`, whose stderr goes to
`<request_id>.worker.log` next to the receipt (restarted above 64 KiB); while that
exact process runs the launch is never judged dead. An `admitted` receipt without
`worker`, more than 10 s old and under 60 s since admission, had its starter killed
before any step: status dispatches it once more. No credentials are in
requests or receipts. Launch timing, waits and abandonment:
[launch acknowledgement](#launch-acknowledgement-and-abandoning-uncertain-operations).

`api account list` also reports `prefill: {eligible, reason, observed_at,
valid_until}`, a conservative preference respecting usage freshness, account
thresholds and allowance (purchased credits do not justify prefill). `selectable`
remains separate.

### Saved-session popup Resume

`capabilities.resume.version == 1` exposes `api resume-launch validate|start` with
JSON stdin, plus `status --request-id UUID` and `list` (schema `ResumeRequest`,
`ResumeReceipt`). The request has version, request UUID, stable `session_id`,
`expected_generation`, account ID, editable name, host and Space. A saved Codex
conversation does not require its original account to remain registered;
original-account metadata is provenance, not a credential. Optional
`confirm_unfinished: true` acknowledges only the unfinished-history warning
(`resume.unfinished_confirmation`); live-writer, directory, conflict and generation
checks remain. Optional `directory` uses New Session's path, creation plan and
confirmation (`resume.directory_selection`); omitted, the saved location is used.
Start journals created directories and keeps `saved_directory` for source
verification and rollback; no project files move. The chosen location is retained
after a verified running handoff. Session details report the last verified
launch/resume Space.

Start journals the intent before creating a tab. A second unfinished Resume for the
same session is rejected. Generation, saved state, account availability and
operation ownership are checked again before provider execution. Same-account Resume
uses the exact-conversation launcher; a changed account delegates to the
non-steering transfer worker (history, permissions and stable ID kept),
`operation_id` naming that job. It creates no model turn.

Receipts distinguish admission, preparation, terminal creation, rename, dispatch,
transfer, provider start, attachment and observed running. `failed` before host
effects is safe failure; `uncertain` after effects requires inspection, never
automatic replay; `recovered` describes the transfer worker's rollback. The name may
already be committed if a later transfer fails (`previous_name`). Closing the popup
only detaches its observer. A receipt that reached `running` is not regressed by a
later Stop; read the catalog for current liveness. A lost acknowledgement ends
`uncertain` with `RESUME_ACK_UNCERTAIN`, keeps the session reserved
(`RESUME_IN_PROGRESS`) and can be released with `api abandon-operation`.

The same request, receipt and answers apply to Codex, Claude and Antigravity
(`capabilities.session_lifecycle.resume_launch`). Claude and Antigravity resume on the
session's own account (`account_id` must match) in its saved directory (a different
`directory` is `UNSUPPORTED_PROVIDER_OPERATION`); the worker types `sessionsation
resume` into the destination pane as `api resume --herdr-session` does, with phases
`admitted`, `preparing`, `resuming`, then `running`, `failed` or `uncertain`. Only
Codex moves accounts. A session that already runs is `phase:"running"` with
`already_running:true`, nothing opened or typed (validate: `valid:true,
already_running:true`). Gemini is refused with `UNSUPPORTED_PROVIDER_OPERATION`,
`error.supported:false` and the reason that Gemini CLI no longer supports individual
accounts. Suspend is `api stop --wait`, resume is `api resume-launch start`. A Claude
session with no transcript (stopped before its first turn) is started again under the
same conversation id on its account and directory, with `resumed_from:"no_history"` in
the validate answer and the receipt and a detail saying so; `api resume` and the
launcher still refuse it with `SESSION_STALE`.

`api resume` (Codex and native) starts the provider in the session's saved
directory, refusing with `SAVED_DIRECTORY_UNAVAILABLE` when it is not an absolute
existing directory. No observed running session within the poll fails with
`RESUME_LAUNCH_UNCONFIRMED`, and a pane command that returns to an idle shell fails
promptly with `RESUME_LAUNCH_FAILED`. The new tab is labelled with the session's
current name. A provider that exits right after starting is detected
([early provider exit](#early-provider-exit)).

### Resume availability reasons

Session state and details include nullable `resume_block_reason`:
`unsupported_provider`, `host_unavailable`, `history_unavailable`,
`already_running`, `moving`, `missing_directory`, `history_busy`,
`unfinished_history`, `conflicting_history`, `unverified_history`, `archived` or
`account_deleted`. `unfinished_history` is a confirmation warning, not a disabled
resume. Capability flags remain authoritative and launch rechecks current state;
tolerate future codes. Read-only history inspection does not constitute
`history_busy`; an open writer does, and uninspectable descriptor flags keep the
busy gate. A recorded terminal blocks resume only while its exact process could
still hold the conversation; killed processes, reused PIDs, leftover registry files
of dead processes and terminals that switched conversation do not. Claude
`capabilities.resume` is false with `SAVED_DIRECTORY_UNAVAILABLE` when no existing
absolute saved directory is known.

A live terminal can be waiting at Codex's workspace-trust/startup prompt before its
conversation is loaded. Lifecycle workers report `waiting_for_provider_ready` until
native state is confirmed; they never approve that prompt. Background-terminal
checks for unloaded saved/child histories require consistent native read/list/read
evidence rather than treating a rejected RPC as empty.

### Launch acknowledgement and abandoning uncertain operations

`api launch` waits up to 120 s for `launch-run` to acknowledge. When the wait expires
while the receipt is still `dispatching`, `launch-run` can no longer start (it
requires `dispatching`), so the receipt becomes `failed`
(`LAUNCH_RUN_NEVER_STARTED: provider was not started`) and the name is released.
Only a launch that `launch-run` claimed stays `uncertain` (`LAUNCH_ACK_UNCERTAIN`).
A late `launch-run` prints the phase and `LAUNCH_ALREADY_DISPATCHED` in the pane and
starts nothing. Codex `launch-run` and inline `run` wait up to 120 s (backoff 50 ms
doubling to 1 s) for the shared account lock while an account operation such as an
archive holds it, staying `starting_provider` with an `ACCOUNT_BUSY wait` detail (the
acknowledgement wait does not count that time). A `launch-run` error before the first
provider mutation (Codex: the app-server thread start), including `ACCOUNT_BUSY: an
account operation held the lock for 120s`, ends the launch `failed` with the name
released; `uncertain` is kept for failures after a provider start may have happened
(and for Claude, Gemini and Antigravity terminal launches, which do not mark that
point). On any error `launch-run` prints `sessionsation launch failed: <CODE>:
<reason>` to the pane's stderr and exits 1.

Contract 1.73.1: an `uncertain` new launch that is provably dead settles on its own as
`failed` (detail `settled: launch worker gone, no session was bound ...`; a provider
conversation may have been allocated with no turn) and its name is released. Provably
dead: the recorded `launch-worker` (pid and start time) is gone and its lock free, no
conversation or catalog entry was bound, and no other live process carries the request
id as an argument or as `SESSIONSATION_SESSION_ID`; an unreadable process table or
process of the same user means not dead. Liveness is judged in the caller's pid
namespace, which assumes every caller runs on the host. The check runs in the launch
reconcile (`api launch status` and `list`, and the supervisor following a `session
launch`) and, before the admission lock, when `api launch start` checks a name such a
launch holds, so a retry right after succeeds; the settle itself requires the phase
still `uncertain` and an unchanged `updated_at`. Anything less certain stays `uncertain` and
keeps the name; its `SESSION_NAME_EXISTS` names the holding launch and the
`abandon-operation` to run. A retry with the same request id is replayed unchanged.

`api abandon-operation --operation ID --confirm ID` (`capabilities.abandon_operation`,
`applies_to:["resume_launch","launch","conversation"]`, `launch_phases:["uncertain"]`) releases an
operation that is deliberately never replayed. It moves the receipt to the terminal
phase `abandoned` (`abandoned_from`, `abandoned_at`, a `detail` stating that nothing
was sent), sends, resumes and replays nothing, and only releases the reservation.
For a resume-launch it accepts `uncertain`, or a non-terminal receipt older than 10 s
whose worker has exited; for a new launch only `uncertain` with a stopped worker. The
same command ends uncertain or stalled account switches, account renames, Auto Start
operations and inline resumptions. Refusals: `CONFIRMATION_MISMATCH`,
`OPERATION_ALREADY_TERMINAL`, `OPERATION_WORKER_ACTIVE`, `OPERATION_STILL_ACTIVE` (a
transfer job is still running; use `cancel-operation`), `OPERATION_NOT_STUCK`,
`SESSION_PROCESS_LIVE` (the session's process, or the launch pane running something
other than an idle shell), `SESSION_EXISTS` (a catalog session with the launch's id)
and `SESSION_STATE_UNVERIFIED`.

### Inline `run` and `resume`

The commands a terminal client types into an existing pane (`sessionsation run
--account A`, `sessionsation resume SESSION --expected-generation G`) are named and
journaled (`capabilities.inline_launch`, `{name:true, journaled:true,
operations:true, ...}`).

- `--name NAME` follows the `api launch` / `api resume --name` rules
  (`INVALID_SESSION_NAME`, `SESSION_NAME_EXISTS`; inside a HERDR pane a canonical name
  must name the pane's own Space, outside HERDR it is `INVALID_ARGUMENT`). `run` gives
  the new entry that name; `resume` renames first (the generation changes). With
  `HERDR_PANE_ID` set the pane's tab is relabelled (best effort). `run --name` is
  refused with `--thread`, `--hosted` or `--host herdr`. Without `--name`, `run` uses
  the directory name (`-2` suffix on a clash).
- `resume --confirm-unfinished` acknowledges only the `unfinished_history` warning of
  a saved Codex session (otherwise `UNFINISHED_HISTORY_CONFIRMATION_REQUIRED`; any other
  block reason fails with `SESSION_STATE_UNVERIFIED`, or names the blocking operation:
  `RECOVERY_UNRESOLVED`, `SESSION_OPERATION_IN_PROGRESS`).
- Every inline `run` writes a launch receipt under `operations/launches` and every
  inline `resume` a resumption receipt under `operations/resumptions`, with
  `inline:{pid,process_start,pane_id}` and `host.session:"inline"`; `api launch
  list|status` and `api resume-launch list|status` show them. Phases:
  `starting_provider`, `attached` (bound to the terminal process, which becomes the
  provider), `complete` (the exact provider process was observed, or ended after
  attaching; terminal, holds no lock), `failed` (refused before binding; the name is
  released; also a resume whose process ended before any provider attached) and
  `uncertain` (never replayed; holds its reservation; release an inline resumption with
  `api abandon-operation`). Phases settle lazily when read; an ended process gets the
  detail "The inline provider process has ended.". Hosted `api launch` /
  `api resume-launch` receipts keep `running` as their terminal success phase; treat
  inline `complete` like `running`.
- `api operations` lists them as `{id, kind:"launch"|"resume", inline:true,
  session_id, name, account_id, phase, detail, created_at, updated_at, pane_id,
  can_cancel:false, can_respond:false, can_recover:false}`; finished entries drop out
  24 hours after they ended or when their session is deleted.
- Locks: launches take the shared account launch lock and the name reservation;
  resumes take the resumption admission lock, the per-conversation admission lock,
  the handoff check and the conversation lock, exactly like `api resume-launch`.
  `run --thread`, `run --hosted` and `run --host herdr` are not journaled this way.

### Early provider exit

A launch whose exact recorded provider process exits before it is observed running,
or within 15 s after (`SESSIONSATION_LAUNCH_EXIT_WINDOW_SECONDS`, `0` disables),
ends `failed` with `detail` `PROVIDER_EXITED: …`; the catalog entry stays as a stopped
session. A `session launch` whose provider exited before the brief was sent ends
`failed` / `provider_exited`. Terminal input re-checks right before the first key and
before Enter that the exact provider process still runs and that HERDR does not
report a bare shell in front of the pane (`TERMINAL_PROVIDER_EXITED`,
`TERMINAL_PANE_IS_SHELL`).

## Launch configuration

### Launch permissions

`api capabilities.launch_permissions` (`values` `["yolo","default"]`,
`built_in_default:"yolo"`, `setting:"launch_permissions"`,
`request_field:"permissions"`, `requests:["launch","resume-launch"]`,
`cli_option:"--permissions"`). `LaunchRequest` and `ResumeRequest` accept an optional
`permissions` (`yolo` | `default`); omitted, the `launch_permissions` setting applies.
Every provider process started for a session uses one mode: new launches, resume
launches, inline `run`/`resume`, Codex restart and account switch, saved-session
open, move recovery and hosted Claude without a `permission_mode`.

| Mode | Codex | Claude Code | Gemini CLI | Antigravity CLI |
|---|---|---|---|---|
| `yolo` | `thread/start`/`thread/resume` with approval `never` and the `:danger-full-access` profile, verified on the result | `--dangerously-skip-permissions`, `IS_SANDBOX=1` and the flag setting `skipDangerousModePermissionPrompt` | `--approval-mode yolo` | `--dangerously-skip-permissions` |
| `default` | no override; resumes, restarts and switches keep the recorded approval/sandbox | no flag | no flag | no flag |

Explicit Codex options after `--` (`-a`, `-s`,
`--dangerously-bypass-approvals-and-sandbox`, `--full-auto`; `--yolo` is an alias of
the bypass flag) and a hosted Claude `permission_mode` win. Claude refuses permission
bypass as root unless `IS_SANDBOX=1`, set only for bypassing starts. Codex rejects
permission flags on a remote resume, so they are never passed to the remote terminal;
instead every verified `thread/resume` is followed by `thread/settings/update` with
the same approval policy and profile, recorded in the conversation, so an unloaded
and reloaded conversation keeps them instead of falling back to `config.toml` or
folder-trust defaults. Codex message recovery restores the recorded permissions
exactly. Headless delivery never bypasses: Claude prints without bypass and Gemini
uses `--approval-mode default`. A running session keeps its mode until restarted.

### Folder pre-trust

In yolo mode, Sessionsation pre-accepts the provider's folder-trust dialog for the
launch directory so a new session does not stop at it. Only the trust entry
changes: every other member keeps its bytes and order, the file is replaced
atomically with its mode and owner (compare-and-swap against concurrent writes), a
symlinked file is refused, a missing or malformed file is left untouched, and
credentials are never read. Failures are reported on stderr and never block the
start. `default` launch permissions and headless delivery change nothing.

- Claude (`capabilities.claude_folder_trust`: `setting:"claude_pretrust_folders"`,
  `built_in_default:true`, `applies_when:"launch_permissions_yolo"`,
  `applies_to:["interactive","hosted"]`): before an interactive bypass start or a
  hosted worker with effective `bypassPermissions`, it sets
  `projects["<directory>"].hasTrustDialogAccepted = true` in the account's
  `CLAUDE_CONFIG_DIR/.claude.json`, under the canonical directory and, when
  different, the recorded spelling. A directory Claude does not know yet gets
  Claude's own per-project defaults.
- Antigravity (`capabilities.antigravity_folder_trust`:
  `setting:"antigravity_pretrust_folders"`, `built_in_default:true`,
  `config_file:"AGY_ACCOUNT_HOME/.gemini/antigravity-cli/settings.json"`,
  `key:"trustedWorkspaces"`, `match:"exact_directory"`, `credentials_read:false`):
  before a yolo `new` or `resume` it appends the launch directory (canonical and,
  when different, as recorded) to `trustedWorkspaces`, which is how agy 1.4.10
  records an accepted dialog (a subdirectory of a trusted directory asks again).

### Claude account settings

Both commands edit one key of the account's `CLAUDE_CONFIG_DIR/settings.json`
(other keys kept, atomic replace, mode kept, a symlinked file refused with
`SETTINGS_IS_SYMLINK`, `.credentials.json` never read) and apply to sessions started
afterwards. Codex, Gemini and Antigravity get `UNSUPPORTED_PROVIDER_OPERATION`.

- **Bypass warning** (`capabilities.claude_bypass_warning`:
  `setting:"skipDangerousModePermissionPrompt"`, `values:["skip","show"]`,
  `new_accounts_default:"skip"`). `api account bypass-warning claude:NAME
  [--skip|--show]` returns `{account_id,provider,bypass_warning,setting,value,
  previous,changed,settings_file,applies_to:"sessions_started_after_the_change"}`;
  without a flag it only reads. `--skip` writes `true`, `--show` an explicit
  `false`; events `account.bypass_warning_skipped|shown` are recorded on change.
  Accounts Sessionsation creates (`account add` into a new managed home, or
  `account login` of a managed home with no `settings.json`) default to skip;
  adopted and existing homes are untouched. Claude account rows add
  `bypass_warning` (`skip` when the stored value is `true`, else `show`),
  `bypass_warning_value` and `bypass_warning_setting`.
- **Default statusline** (`capabilities.claude_default_statusline`, config key
  `claude_default_statusline_command`; an empty value disables it). A new managed
  Claude home (the same two cases) gets
  `{"statusLine":{"type":"command","command":"<the configured command>"}}` seeded;
  an existing `statusLine`, adopted and existing homes are never touched.
  Sessionsation's own statusline hook forwards to this command
  ([Claude usage](#claude-usage-and-statusline)).

## Session model and one-pass launch

Capability `session_model` (version 1), plus `launch_model` for the JSON `api launch`
fields `model` and `reasoning_effort`.

- **Exact ids only.** Codex: letters, digits and `. _ - : /`, 1-100 characters, not
  starting with `-` (and for `set-model` an id from the account's `model/list`);
  efforts `none|minimal|low|medium|high|xhigh`. Claude: `claude-` followed by
  lower-case letters, digits, dots and dashes, optional `[1m]`; efforts
  `low|medium|high|xhigh|max`; aliases (`opus`, `sonnet`, `default`, ...) are
  `INVALID_MODEL`. Hosted Claude with a model is `UNSUPPORTED_PROVIDER_OPERATION`, as
  are Gemini and Antigravity. A launch model is not checked against what the account
  can use. Applied values are always the ones the session reports; a substituted
  model or capped effort is a failure carrying the observed value (ASCII
  case-insensitive equality; Claude ids compare ignoring a trailing `[1m]`).
- **Always pass a model.** A Claude `/model` or `/effort` (including the ones
  `set-model` types) saves the value as the account default for later starts without
  a model; Sessionsation does not restore it. Resumes (`api resume`, inline `resume`,
  Auto Start, account switch, hosted resume) pass the conversation's last recorded
  model and effort; headless sends pass none. A Codex launch model starts the thread
  with that model and `model_reasoning_effort` and opens the terminal with `-m` and
  `-c model_reasoning_effort="..."`; Claude terminal launches use `--model`/`--effort`.
- **Row field `model`** (`state`, `state --cached`, `session`, `session resolve`):
  `{"id","effort","observed_at","source"}` or null. Sources: `claude_statusline`
  (Sessionsation's status-line hook writes the payload's `model.id` and
  `effort.level` to `<account home>/.sessionsation/model/<session_id>.json`),
  `codex_thread` (`thread/read` during inventory, kept in
  `registry/session-catalog/models.json`) and `set_model`. Not part of the
  generation. Claude sessions started by a release without the hook report null
  until stopped and resumed.
- **`api session set-model`** journals `operations/model-changes/<id>.json`: `waiting
  → applying → complete | failed | cancelled | expired`, applied at the next idle
  point (`--when-idle`: up to `--expires-at`, default one hour; otherwise within
  60 s). One pending change per session (same values: `existing:true`; other values:
  `SESSION_OPERATION_IN_PROGRESS`; same `--operation` with other values:
  `OPERATION_ID_CONFLICT`). Admission refusals: `SESSION_NOT_FOUND`,
  `AMBIGUOUS_SESSION`, `STALE_TARGET`, `UNSUPPORTED_PROVIDER_OPERATION`,
  `INVALID_MODEL`, `INVALID_REASONING_EFFORT`, `INVALID_OPERATION_ID`,
  `INVALID_EXPIRY`, `INVALID_ARGUMENT`, `SESSION_NOT_RUNNING`, `TERMINAL_NOT_ATTACHED`,
  `TERMINAL_INPUT_DISABLED`, `SESSION_STATE_UNVERIFIED` (Claude: no status-line record
  from the running process) and the lifecycle codes. Already running the requested
  values: `complete`, `already_applied`, nothing sent. Cancel while `waiting`
  (`applying`: `OPERATION_WORKER_ACTIVE`; ended: `OPERATION_ALREADY_TERMINAL`). Listed
  as `kind:"model_change"`. `--wait N` (default 90, or 0 with `--when-idle`, max 600)
  blocks until the record ends. Claude admission needs the status-line record of the
  session's own process; with `--wait` it waits for the first one (250 ms backoff
  doubling to 2 s), otherwise or after the wait it fails `SESSION_STATE_UNVERIFIED`
  with `retryable:true` and `detail:"hook_not_recorded"`
  (`session_model.set.admission_wait`). A record of this process without a model
  (status line disabled, folder trust not accepted) stays non-retryable.
  - Codex: at an idle point (no active turn, empty native queue, under session
    ownership) `thread/settings/update {model}`, then `{effort}`, verified with
    `thread/read` (`model_not_available`, `effort_unsupported`, `provider_rejected`,
    `model_not_confirmed`/`effort_not_confirmed` after three attempts). New public
    `submit`/`queue` input is held (`pending_local`, `model_change_hold`) and released
    through the compaction release path. Compactions and model changes of one session
    run in request order (`compaction_pending`, `model_change_pending`). A change stuck
    in `applying` without progress for 180 s fails `apply_not_observed`. Steering is not
    held.
  - Claude (HERDR terminal sessions): a detached worker types `/model <id>` and
    `/effort <level>` only after two idle observations at least 1.5 s apart (HERDR
    agent `idle`/`done` and an **empty** main prompt; a draft is never cleared:
    `draft_present`) and while no terminal delivery is in flight or touched the pane in
    the last 5 s (`terminal_delivery_active`). Waiting changes back off (Claude 4 s to
    30 s, Codex 2 s to 30 s). It removes only its own command (by backspaces after
    verifying a prefix; otherwise `prompt_not_owned`). The `Switch model?` / `Change
    effort level?` dialog is answered by up/down to `Yes` and Enter (a
    `PreModelSwitch` hook prompt is declined: `blocked_by_hook`); never Esc, Ctrl+C,
    Ctrl+D or digit keys. On failure the worker chooses `No` and verifies the main
    prompt, else `pane_left_in_dialog`. Effort is applied after the model; without
    `--effort` the effort in force before is re-applied. Confirmation comes only from
    the status-line record; no change for 8 s is `model_not_confirmed`; an unidentified
    screen ends with `TERMINAL_OVERLAY_UNIDENTIFIED` and `refusal_layout`. Claude input
    is not held; terminal deliveries wait for the pane lock.
- **`api session launch`** admits an ordinary launch request (with `model` and
  `reasoning_effort`) and journals `operations/session-launches/<request_id>.json`
  plus the brief (`.brief`, 0600, removed when the record ends): `admitted →
  launching → verifying_model → delivering_brief → complete | failed | cancelled`.
  The brief is **held until the model (and requested effort) is verified** from the
  session itself (`brief.delivery`: Claude `held`, Codex `first_turn`), then sent as an
  ordinary message (Claude `terminal_input` submit; Codex submit with
  `if_busy:"queue"`), with a new message id journaled before each attempt; pre-submit
  refusals are retried every 5 s up to 5 times only when nothing was written, and
  `delivery_unknown` is never retried. `--brief-delivery initial-prompt` hands the
  brief to Claude as its start-up prompt (`handed_to_provider`, `delivered` once the
  transcript holds it; not gated on the model); Codex accepts the flag and still
  delivers as `first_turn` (`brief.requested_delivery` records it). Mismatch: `failed`
  `model_not_confirmed` / `effort_not_confirmed` / `effort_unsupported`, brief
  `not_sent`, session left running. `--deadline` (default 300 s) bounds
  launch-to-delivered-brief (`ready_timeout`; a send in flight then is
  `delivery_unknown`, never `not_sent`); no launch receipt 120 s after admission is
  `launch_failed`. Brief limits: Claude held 8192 bytes and the terminal-input rules,
  initial prompt 32768 bytes and not starting with `/ ! #`, Codex 16384 bytes (a brief
  file over 65536 bytes, an initial prompt or Codex brief over its limit is
  `INPUT_TOO_LARGE`; the held Claude brief `TERMINAL_TEXT_TOO_LONG`).
  `--wait N` (default 0, max 1800) ends without `complete` as `MODEL_NOT_CONFIRMED`,
  `PROMPT_NOT_DELIVERED`, `PROVIDER_LAUNCH_FAILED` or `LAUNCH_ACK_UNCERTAIN` with
  `error.operation`; without it `ok:true` is only the admission. The same
  `--request-id` with the same request and brief returns the record (`existing:true`);
  different input is `REQUEST_ID_CONFLICT`. Cancel with `api cancel-operation`.
- Before rolling back to a release without these records, drain them
  ([operations](operations.md#rolling-back-a-release)).

## Stop and restart

`api stop --session ID --expected-generation G [--keep-pane] [--wait] [--interrupt]
[--discard-queued] [--disable-auto-start]` ends the selected agent and preserves resumable history. `--interrupt` first interrupts a Codex
turn (`turn/interrupt`); Claude and Antigravity stops always do. A Codex `--wait` that ends while the job still waits
for idle is retryable `OPERATION_TIMEOUT` (`operation.id`, `operation.phase`); a repeat stop returns that job.
Queued Codex input refuses `--interrupt` before `turn/interrupt` (`QUEUED_INPUT`, not retryable, naming the client
message ids; no job is added), as it refuses a plain stop whose last turn is already interrupted: Codex never starts
queued input after an interrupted turn. A plain stop otherwise queues its job (Codex drains the queue at idle).
`--discard-queued` (1.69.3, every provider) first cancels the session's own unsent public input through `api cancel`,
then deletes Codex native items no receipt names (an item with a receipt is cancelled through `api cancel`); the result,
or the error of a stop that failed afterwards, adds `discarded_messages`, `not_discarded` and `discarded_native_items`.
A message still queued behind an interrupted Codex turn reports `reason:"queued_after_interrupted_turn"`. While a stop
job's worker runs, `api cancel` is retryable `OPERATION_BUSY`; a stop whose worker died before archiving anything is
settled after 10 s, one that died later is `MIGRATION_STATE_UNKNOWN` until `recover`.
Since 1.71.2 the cancels of a `--discard-queued` stop pass a pending stop job of the same session that
has not begun stopping (`queued`, `waiting_for_idle` or `waiting_for_pause`: no `stopping_source`, nothing
archived; it may already have paused the goal or ended background terminals); the stop then joins that job.
Every result has the same keys (`capabilities.stop_result`): `id`, `kind`,
`session_id`, `source_session_id` (both name the catalog session), `provider`,
`phase`, `detail`, `already_stopped`, `pid`, `process_start`, `signal`, `pane_closed`,
`pane_close_reason`, `interrupted_turn`, `auto_start`, `provider_conversation_id`,
`source_account_id`; a value the provider path did not report is null.

- **Codex** queues a `terminate` job that waits for positive idle (`interrupted_turn`
  null; `signal:"SIGTERM"` once complete). A stop still waiting for idle
  (`waiting_for_*` phases) gives up after 1800 s (`TERMINATE_WAIT_SECONDS`, from
  `created_at`): it fails "the stop request expired while waiting for the session to
  become idle; nothing was stopped" and releases its stop gate. A cancelled or failed
  stop job releases its own gate while the exact source process still runs (reads heal
  an already-stale gate), so a session that kept running is not left
  `explicit_resume_required`; a completed stop keeps its gate, another job's gate is
  never released. With `--wait` the call polls the job (300 s bound;
  `SESSIONSATION_OPERATION_WAIT_MS` in tests): `complete` returns the job;
  `failed`/`cancelled` is `OPERATION_FAILED` with `error.operation`; no end is
  `OPERATION_TIMEOUT` (retryable; the stop stays queued). `capabilities.stop_wait`.
- **Claude** (`capabilities.claude_lifecycle`): the target is the process Claude
  Code's registry names as the holder of the conversation (exact pid, /proc start time,
  config home), re-read under the conversation lock and signalled through a pidfd
  whose start time is checked after opening (otherwise `SESSION_STATE_UNVERIFIED`,
  nothing signalled). SIGTERM only, then up to 10 s
  (`claude_lifecycle.stop.wait_seconds`). A turn in progress is interrupted
  (`interrupted_turn:true`; its unfinished reply is not saved); the transcript stays
  resumable. Background (`bg`) sessions and pre-started spares are
  `UNSUPPORTED_PROVIDER_OPERATION` (they belong to Claude's daemon); hosted sessions
  are `HOSTED_SESSION` (use `api hosted stop`). Rows report `capabilities.stop:true`
  exactly while the registry-verified interactive process runs.
- **Antigravity** (`capabilities.antigravity_lifecycle`): SIGTERM to the exact
  verified agy process (pidfd; pid, start time, `agy` command line, account home, and
  that it holds the conversation), then up to 20 s. SIGTERM runs agy's normal exit
  (it prints `Resume with ... agy --conversation=ID`, checkpoints and removes the
  `-wal`/`-shm` files); a typed `/exit` is not used because Enter during a turn queues
  text.
- **Gemini** has no stop: `UNSUPPORTED_PROVIDER_OPERATION` with `error.supported:false`
  and `error.reason`, also the row's `capability_reasons.stop`.
- A session with no running process (not archived; for Codex, a session whose source
  discovery reports `SESSION_UNAVAILABLE`) returns `already_stopped:true`,
  `phase:"complete"`, `interrupted_turn:false`; the generation is still checked, and
  the pane of the session's newest resume-launch or of its launch (else a recorded pane
  whose process is gone) is closed as below. An archived session gives the same answer
  with `archived:true` whatever generation the caller holds (the archive changed it).
- **Unknown session** (`capabilities.session_lifecycle.unknown_session`): stop or
  archive of an id (or name) that names no catalog session and no merged id is one
  terminal answer, so a retire always finishes: `ok` with `phase:"complete"`,
  `not_found:true`, `session_id` the target; stop adds `already_stopped:true` and the
  shared stop keys, archive `kind:"archive"`, `nothing_to_archive:true` (`--confirm`
  must still equal `--session`). The generation cannot be checked. A pane
  Sessionsation's own launch of that id ran in is closed as below. A launch of that id
  that has not settled is `SESSION_LAUNCHING` (retryable) instead.
- Errors: `STOP_TIMEOUT` (not exited in time; never killed, SIGKILL is never sent;
  retryable), `SESSION_STATE_UNVERIFIED`, `AMBIGUOUS_SESSION`, `STOP_UNAVAILABLE`,
  `STOP_FAILED`, `SESSION_NOT_RUNNING`.
- **Pane close** (`stop_closes_pane`, `stop_keep_pane`, `stop_pane_close`): after the
  exact agent process is gone (and, for Codex, the history is back in the resume list)
  the recorded HERDR pane is closed with `herdr pane close`, but only if it still
  exists and only its idle shell runs there (`pane process-info`); a busy pane is
  re-checked for up to 3 s (`session_lifecycle.pane_settle_seconds`) while the stopped
  provider's wrapper or shutdown children leave it. `--keep-pane` keeps it. `pane_close_reason` is null when closed, else `KEEP_PANE`, `NO_PANE`,
  `PANE_BUSY`, `PANE_MISSING`, `PANE_STATE_UNVERIFIED`, `PANE_CLOSE_FAILED`,
  `PANE_CLOSE_UNSUPPORTED` (tmux attachments are never closed) or
  `PANE_TARGET_INVALID`. A pane that cannot be closed does not fail the stop. Restart,
  switch, move and Auto Start never close panes.
- **`--disable-auto-start`** sets the session's Auto Start to `disabled` after a stop
  without error (including `already_stopped`) and returns the row's `auto_start`
  projection; for a Codex stop without `--wait` it is applied at admission, with
  `--wait` after completion. Without the flag an explicit stop only marks the session
  ([Auto Start](#auto-start)).

`api restart` (Codex only) replaces the terminal client on its selected account,
keeping the recorded permissions and the account server. Claude and Antigravity
restart is stop then resume. Before typing a command into a reused HERDR shell
Sessionsation clears the line with `pane send-keys <pane> ctrl+e ctrl+u`; tmux relaunch
uses `respawn-pane -k`. After a lifecycle command or job the cached snapshots are
patched with the new generation, and the `resume`/`stop`/`restart`/`switch` result
carries it.

### Account switch

`api switch --session ID --expected-generation G --to PROVIDER:NAME [--accept-risks]`
moves a conversation to another account of the same provider (another prefix or a
bare name is `ACCOUNT_PROVIDER_REQUIRED`). Codex and Claude support it; Gemini and
Antigravity do not. `capabilities.switch_account` is true for a stopped session and
for a running one that can be stopped and resumed in place, otherwise false with
`capability_reasons.switch_account`.

Switching a **stopped/saved** session only re-homes it, never starts it. Codex copies
and byte-verifies the rollout and child histories in the target home, retires the old
copy and lets discovery list the session as `saved` on the target account (new
catalog account and generation), without loading the thread on the target
app-server, opening a pane or starting Codex (`complete` detail says the session
stays saved). A later resume starts it there with the recorded permissions. A
resume launch that chooses an account (`resume_destination`) and switching a live
Codex session use the non-steering transfer worker, which waits for positive idle and
never forces interruption.

A Claude switch returns the Codex-switch operation keys (`id`, `kind:"account"`,
`phase`, `detail`, `source_session_id`, `source_account_id`, `target_account_id`,
`provider_conversation_id`, `can_cancel:false`, `can_recover`, `can_respond:false`)
plus `provider:"claude"`, `uncertain`, `terminal`, `stalled`, `replays:false`,
`generation`, `resume {state, pane, permissions}` and `receipt` `{files_moved,
paths, account_memory:{moved:false, stays_with, detail}, shared_state}`.

- **Phases** (journal `operations/claude-switches/ID.json`, driven by a detached
  `claude-switch-worker`, each journaled before its work): `admitted`, `stopping`,
  `stopped`, `copying`, `copied`, `switching`, `switched`, `removing`, `removed`,
  `resume_pending`, `resuming`, then `complete`, `failed` or `abandoned`;
  `uncertain` waits for `api recover` or `api abandon-operation`.
- **Admission** (nothing changes on refusal): `STALE_TARGET`, `SWITCH_SAME_ACCOUNT`,
  `ACCOUNT_UNAVAILABLE`, `ACCOUNT_REMOVED`, `ACCOUNT_RENAME_UNRESOLVED`,
  `OPERATION_BUSY`, `SESSION_OPERATION_IN_PROGRESS`, a headless delivery in flight,
  `HISTORY_NOT_FOUND`, `SESSION_STALE`, `SESSION_STATE_UNVERIFIED`, `TARGET_CONFLICT`
  (a copy in the target home), `SESSION_HOSTED` (stop it first),
  `SESSION_NOT_SWITCHABLE` (background or parked), `AMBIGUOUS_SESSION`,
  `SESSION_PANE_UNKNOWN` (a running process outside a recorded pane cannot be resumed
  afterwards), `SAVED_DIRECTORY_UNAVAILABLE`, `SHARED_STATE_MIXED`, and
  `RISK_CONFIRMATION_REQUIRED` for a `busy` running session (repeat with
  `--accept-risks`; stopping interrupts the turn).
- **Stop**: the live process is identified as discovery does (registry pid, start
  time, session id and config home), signalled with `SIGTERM` through a pidfd opened
  before the identity is re-checked, and waited for 30 s. A process that does not exit
  is `STOP_TIMEOUT`, never killed; a different process holding the conversation is
  `SESSION_CHANGED`.
- **What moves**: for every lineage conversation, `projects/<slug>/<id>.jsonl`,
  `projects/<slug>/<id>/`, `file-history/<id>/`, `session-env/<id>/` and
  `todos/<id>-agent-*.json` (the archive/delete set; symlinks refused). Per-project
  `memory/`, `settings.json`, credentials and other account-level files stay with
  the source. With [shared state](#shared-claude-state) on both accounts the copy and
  removal phases are journaled but move nothing (`receipt.shared_state:true`,
  `files_moved:0`).
- **Crash safety**: digests are journaled once the source is quiescent; each file is
  copied to staging in the target home, verified, renamed into place; only then is
  the catalog switched (new revision and generation, attachment cleared) and the
  source copies removed after a second digest check. A failure before the catalog
  change ends `failed` with the source intact; after it, `uncertain`. `api recover ID`
  continues from the journaled phase (copy, switch and removal are idempotent);
  `api abandon-operation ID --confirm ID` ends an `uncertain` or stalled (no worker,
  over 10 s old) operation and moves nothing.
- **Resume**: a session that was running is relaunched in its pane as
  `sessionsation resume` with the recorded permissions (from the stopped process's
  command line), the target `CLAUDE_CONFIG_DIR` and the recorded directory.
  `resume.state` is `confirmed` only after the exact new process is observed on the
  target account, otherwise the operation is `uncertain` with `unconfirmed`; the
  dispatch is never replayed. A stopped session gets `resume.state:"not_needed"`.

### Auto Start

Auto Start means "start this session again after a host reboot, an array restart or
a crash, and not much else". Every provider with a resume path is supported (Codex,
Claude, Gemini, Antigravity). `api capabilities.auto_start` describes it (clients show
the control only when present; it carries `default_prompt`, `default_prompt_crash`,
`max_prompt_bytes`, `enable_requires:"none"`, the classification table under
`triggers` and `last_result_states`).

- **Command**: `api auto-start --session ID --expected-generation G --mode
  disabled|start|start_prompt [--prompt TEXT]`. `disabled` is the default for every
  session; `start` resumes and sends nothing; `start_prompt` resumes, then sends the
  prompt once (the custom `--prompt`, 1-4000 bytes and only valid with
  `start_prompt`, else the default). `--mode start_prompt` without `--prompt` clears a
  custom prompt. The mode can be set on live, stopped and saved sessions; archived,
  hosted, background and unsupported sessions get `AUTO_RECOVER_UNSUPPORTED`; a stale
  generation is `STALE_TARGET`, an unfinished operation
  `SESSION_OPERATION_IN_PROGRESS`, a bad prompt `INVALID_ARGUMENT`. Disabling is always
  allowed and the toggle never changes the generation. The receipt is
  `{"session_id","generation","changed","mode","auto_start":{...},"auto_recover":{...}}`.
  `api auto-start --all --mode disabled` (operator bulk; only `disabled`) returns
  `{"all":true,"mode":"disabled","changed","disabled_count","disabled":[ids]}`.
- **State**: every session row has `auto_start:{mode, enabled, prompt, last_pane,
  last_started_at, last_result, workspace_label, suppressed, starts_last_hour,
  max_per_hour}` and `capabilities.auto_start` / `capability_reasons.auto_start`.
  `last_pane` is a `herdr/<session>/<pane>` target kept current while the session is
  live. `last_result` is `null` or `{state, code, detail, operation_id, at}`.
- **Triggers.** The supervisor checks shortly after start and then every 15 s
  (`SESSIONSATION_AUTO_RECOVER_SECONDS`, `0` disables; skipped when no session has a
  mode). A session is due only after 3 consecutive not-running checks with the
  `api state` identity rules, when it is not archived, deleted, hosted or suppressed,
  has no other operation (lifecycle admission) or unfinished recovery, and HERDR
  answers (`HERDR_UNAVAILABLE` is surfaced and retried).
  - *Host boot*: `/proc/sys/kernel/random/boot_id` differs from the `boot_id` in
    `registry/auto-recover.json`; every session with a mode that is not running is
    started once per boot (`boot_started`), including ones stopped before the reboot.
  - *Array start*: on the same boot, a changed mount id of `/mnt/user` makes every such
    session due once (`array_id`, `array_due`/`array_started`). A new boot uses the
    boot trigger only.
  - *Crash*: on the same boot, a session seen running whose recorded process (pid and
    /proc start time) vanished is started only when the kernel log (`dmesg`) has a line
    naming that pid and not older than the process: `Out of memory: Killed process
    <pid>`, `<comm>[<pid>]: segfault at`, `traps: <comm>[<pid>] trap ...` or `general
    protection`. For Claude a removed `sessions/<pid>.json` without a crash line is a
    clean exit (`skipped_clean_exit`); a left-behind entry without one is
    `exit_unclassified`. Every other exit (`/exit`, a closed pane, SIGABRT/runtime
    aborts, a manual `kill -9`, a line that left the ring buffer) is not started.
  - A supervisor or daemon restart on the same boot starts nothing, and the first
    pass after an upgrade only records the boot and array ids.
- **Explicit stop** keeps the mode and only marks the stop
  (`last_result` `skipped_explicit_stop` / `EXPLICIT_STOP`; the exit skips are
  `skipped_clean_exit` / `CLEAN_EXIT` and `exit_unclassified` / `EXIT_UNCLASSIFIED`, a
  failed start `failed` / `RECOVERY_FAILED`), so the next boot starts
  the session; `api stop --disable-auto-start` also sets it to `disabled`. Archive
  and delete set `disabled` (`ARCHIVED_OR_DELETED`). `switch` and `restart` only
  suppress it until the session is observed live again.
- **Pane choice**: the recorded pane is reused only while it exists and only an idle
  shell runs in it (`herdr pane process-info`: the foreground group is the shell, a
  single shell process); the input line is cleared with `ctrl+e ctrl+u` first.
  Otherwise a new tab opens (`--no-focus`, labelled with the session name) in the same
  workspace (the recorded id while it has the recorded label, else the workspace with
  that label), else in a new workspace. Pane and tab are renamed after the session.
- **Resume and prompt**: `sessionsation resume ID --expected-generation G
  --confirm-unfinished` runs in the pane through the normal path (same flags,
  permissions and environment as a manual resume). The prompt goes through `api send`
  (`mode:"submit"`; for Claude it waits up to 60 s,
  `SESSIONSATION_AUTO_RECOVER_NATIVE_SECONDS`, for the registry entry and socket, then
  uses `delivery:"native"`). It is `delivered` only when its receipt shows `written`,
  `input_consumed` or native acceptance. A `rejected` receipt with nothing written and
  pre-admission refusals are retried with a fresh message id and backoff for up to
  180 s (`SESSIONSATION_AUTO_RECOVER_PROMPT_SECONDS`); anything that may have been
  written ends `uncertain` and is never re-sent. `prompt.attempts` and `prompt.detail`
  list every attempt. Default prompts name the trigger and its UTC time (boot, array
  restart, or the crash and what was seen).
- **`last_result.state`**: `started_after_boot`, `started_after_array_start`,
  `started_after_crash` (each optionally `_with_prompt`, only for a delivered prompt,
  else code `PROMPT_NOT_DELIVERED`), `skipped_clean_exit`, `exit_unclassified`,
  `skipped_explicit_stop`, `already_running` (started some other way; not counted
  toward the rate limit and not suppressing), `failed`, `uncertain`, `rate_limited`,
  `waiting`, `disabled`, `abandoned`. Older records may read `started` /
  `started_with_prompt`.
- **Rate limit**: at most 3 attempts per session per hour; beyond that
  `rate_limited` until the window passes.
- **Operations**: each start is journaled in `operations/auto-recoveries/ID.json` and
  listed as `kind:"auto_recover"` with `mode`, `trigger` (`boot`, `array_start`,
  `crash`), the account-switch operation keys, `resume {state, pane, pane_reused,
  recorded_pane}` and `prompt {state, attempts, detail, message_id}`. Phases:
  `admitted`, `pane_ready`, `resuming`, `resumed`, `prompting` (skipped for `start`;
  `prompt.state:"skipped"`), then `complete`, `failed` (nothing started) or
  `abandoned`; `uncertain` (dispatched but not confirmed) and stalled operations
  (no worker, over 10 s old) wait for `api recover ID` (re-checks, never replays) or
  `api abandon-operation ID --confirm ID`, and block further automatic starts.
- **Storage**: `registry/auto-recover.json` (atomic, fsynced, under a lock; schema 2
  with `mode`, `enabled` mirroring it). It is outside the catalog, so pane bookkeeping
  never changes a generation.
- **Deprecated, still served**: `api auto-recover --session ID --expected-generation
  G (--enable [--prompt TEXT] | --disable)` (`--enable` = `start_prompt`),
  `sessions[].auto_recover` (`enabled` = mode is not `disabled`; `last_result.state`
  maps `*_with_prompt` to `recovered`, other starts to `recovered_without_prompt`,
  skips and `already_running` to `waiting`) and `capabilities.auto_recover`
  (`deprecated:true`).
- **Tests only** override the sources: `SESSIONSATION_BOOT_ID_FILE`,
  `SESSIONSATION_KERNEL_LOG_FILE`, `SESSIONSATION_MOUNTINFO_FILE`,
  `SESSIONSATION_ARRAY_MOUNT`.

## Compaction at the next idle point

```sh
api session compact --session ID --expected-generation G --when-idle [--expires-at UNIX] [--operation UUID]
api session compact-status (--operation UUID | --session ID)
api session compact-cancel --operation UUID        # or: api cancel-operation UUID
```

`capabilities.session_compact` lists providers, methods, phases, reasons, expiry
bounds and hold semantics. `--when-idle` is required. `--expires-at` defaults to one
hour, at most 24 hours (`INVALID_EXPIRY`); `--operation` is a caller UUID (generated
when omitted). The supervisor must be running (`SESSIONSATION_COMPACTION_SECONDS`,
default 2, 0 disables the loop). Records are `operations/compactions/ID.json`, listed
by `api operations` (`kind:"compaction"`) while pending and for an hour after.

```json
{"id":"UUID","kind":"compaction","provider":"codex","when":"idle","session_id":"...","accepted_generation":"...",
 "provider_conversation_id":"...","phase":"waiting","terminal":false,"reason":"turn_active",
 "detail":"A turn is active; the compaction starts when it ends.",
 "hold":{"active":true,"released":false,"scope":"new public submit and queue sends to this session"},
 "created_at":0,"updated_at":0,"expires_at":0,"started_at":null,"finished_at":null,"attempts":0,
 "compaction_turn_id":null,"can_cancel":true,"can_respond":false,"can_recover":false,
 "non_steering":true,"replays":false,"existing":false,"held_messages":["message ids"],
 "supported":true,"method":"app_server","native_method":"thread/compact/start",
 "already_compacted":false,"nothing_to_compact":false}
```

- **Phases.** `waiting`, `starting` (journaled just before the start), `running`,
  then `complete`, `failed`, `cancelled` or `expired`. Waiting reasons: `turn_active`,
  `native_queue_not_empty`, `approval_or_user_input_required`, `session_not_running`,
  `session_busy`, `native_sending_disabled`, `explicit_resume_required`,
  `provider_unavailable`, `state_unverified`, `provider_rejected`,
  `compaction_pending`, `model_change_pending`, `draft_present`. End reasons:
  `completed`, `cancelled`, `expired`, `generation_changed`, `provider_rejected`,
  `unsupported_native_version`, `compaction_failed`, `compaction_interrupted`,
  `compaction_not_observed`, `compaction_not_started`, `compaction_timeout`.
- **Nothing to do.** When the history shows no instruction or reply yet the operation
  completes at once with `nothing_to_compact:true`; when its newest compaction is
  newer than the last instruction and reply, with `already_compacted:true`.
- **One per session.** A second `compact` (or the same `--operation`) returns the
  pending operation with `existing:true`. Cancel only while `waiting`; afterwards
  `COMPACTION_ALREADY_STARTED` or `COMPACTION_ALREADY_FINISHED`.
- **Codex** (`method:"app_server"`): `thread/compact/start` at the first true idle point:
  no active turn (`idle`, or a settled `errored`/`interrupted` last turn), no pending
  approval or question, an empty native queue, all checked under the session ownership
  lock. Input already in the native queue runs first. An error answer returns to
  `waiting` (`provider_rejected`), at most 3 attempts. An unanswered start stays
  `starting` and is resolved only from the thread's own turns, never issued again.
  Completion: the first turn not in the pre-start baseline whose summary shows no user
  message, read once in the full view (one turn, positioned with the server's
  `nextCursor`), carrying a `contextCompaction` item (`completed` → `complete`;
  `failed`/`interrupted` → `failed`). Bounds: 30 minutes running
  (`compaction_timeout`); idle again with no compaction turn after 30 s, or no
  observable progress for 180 s, is `compaction_not_observed`. Undecided passes are
  logged once a minute per step. A process replaced before the start fails
  `generation_changed`. A pending operation does not block `stop`, `restart` or
  `switch`.
- **Hold** (Codex and Claude). While an operation is `waiting` (until its `expires_at`),
  `starting` or `running`, and until its held input is released, public `submit` and
  `queue` sends stay `pending_local` (`reason:"compaction_hold"`). A Codex `steer` is refused
  (`api steer-target` returns `available:false`, `reason:"target_compacting"`; a steer
  send becomes a terminal `rejected` receipt with `target_compacting`); send `submit`
  with `if_busy:"queue"` or `queue` instead. `resume` and the legacy `messaging`
  commands are not held. On release the supervisor re-sends held messages through the
  idempotent send path in arrival order (at most 20 per tick) with **queue semantics**
  (`hold_release:{from:"compaction",delivered_as:"queue"}`); a failed step stops the
  pass at that message (`compaction_release_retry`) until its own `expires_at`.
  `hold.released` turns true when nothing is left. Compactions typed into the TUI are
  recognised only by the app-server's structured `compact_turn_not_steerable` refusal.
- **Claude** (`method:"terminal"`): at the next idle point with an empty prompt (two
  observations 1.5 s apart, no terminal delivery in flight) it types `/compact` into
  the verified HERDR pane, reads it back (the highlighted menu entry must be
  `/compact`), presses Enter once, and completes on the `PostCompact` hook event or a
  transcript `compact_boundary` at or after the start. A draft is never cleared; a
  command Enter did not run is removed and the operation fails
  `compaction_not_started`; an idle pane with no compaction after 2 minutes fails
  `compaction_not_observed`. A held Claude send has no worker until released (priority
  `later`); a Claude `steer` is not held.
- Gemini and Antigravity refuse with `UNSUPPORTED_PROVIDER_OPERATION`
  (`error.supported:false`, `error.reason`). Other refusals: `STALE_TARGET`,
  `SESSION_NOT_RUNNING` (no live app-server attachment), `INVALID_OPERATION_ID`,
  `OPERATION_ID_CONFLICT`, `OPERATION_NOT_FOUND`.

## Messaging

`api send` reads a single request from stdin; maximum request size is 32 KiB and
text is limited to 16 KiB. Supply a new UUID `message_id`, stable backend
`session_id`, current `expected_generation`, a `mode`, text and an `expires_at` Unix
timestamp within the next 24 hours. `api_version` must be 1. Modes are `submit`,
`queue`, `steer`, `resume` (explicit recovery or headless continuation) and `new`
(headless new conversation). One live request shape serves every provider; see
[one send request](#one-send-request-for-every-provider) for the per-provider
mapping and `ignored_options`.

For Codex, Submit and Queue use the verified native **non-steering queue**. Submit
requires idle preflight and rejects a busy target; a concurrent turn can defer a
submitted message but never turn it into a steer. Queue accepts idle or busy
eligible sessions. An optional `"if_busy":"queue"` on a Codex `submit` queues it
natively (`native_accepted`) instead of `rejected`/`target_busy` when a turn started
meanwhile (any other value, or another mode, is `INVALID_REQUEST`; the receipt echoes
it). A Submit or Queue to a session that waits on an approval or user
input is a definite refusal, `rejected` with reason
`approval_or_user_input_required`, before any native write; a resend returns that
refusal. Ordinary Submit/Queue recheck terminal-turn state before native insertion:
an `interrupted` turn or a recorded stop intent requires explicit Resume even when
native metadata says idle, while a failed (`errored`) turn, for example a model at
capacity, leaves a live idle session that accepts input (`actions.resume` stays
available too).

Native send checks (Codex submit/queue; capability `native_send_checks`): acceptance
is not consumption; `SESSIONSATION_NATIVE_SEND_CHECKS=off` turns (1) and (3) off
(test fixtures only). (1) Before the send, up to 20 s are spent (without holding the
ledger) waiting for input readiness: a process older than 4 s whose thread answers
native reads with a settled turn state; busy, blocked and interrupted states end the
wait at once. (2) A late or lost add acknowledgement is reconciled against the native
queue. (3) After an input accepted by an idle session, up to 8 s are spent confirming
consumption (the queue entry is gone and a turn records the message id): consumed is
`execution_started`/`completed_with_reply` with `facts.input_consumed:true`; still
queued at the deadline, the entry is removed and the receipt is `failed` with
`input_not_consumed` (nothing ran; safe to resend with a new id); neither seen is
`delivery_unknown` with `consumption_unverified`. A queue send to a busy target skips
(3). An unsent `pending_local` submission settles when its receipt is read: past its
expiry it is `expired`; if its session was replaced (generation changed) it is
`rejected` with `generation_mismatch`.

`ok:true` from `api send` means the request was processed and a receipt exists, not
that the input was delivered: read `result.state` and `facts`. `rejected`, `failed`,
`expired` and `delivery_unknown` were not (provably) delivered.

`api receipt MESSAGE-ID` observes progress; `api cancel MESSAGE-ID` cancels only
when supported and positively confirmed. Receipt facts distinguish local admission,
native acceptance, recorded input and an observed public reply. Native acceptance
is not completion or proof of input consumption. Only a correlated terminal reply
establishes `completed_with_reply`.

A repeated identical ID/request returns or reconciles the original receipt. Changed
payload, mode, target, generation or expiry gives `MESSAGE_ID_CONFLICT`. Message ids
share one namespace across every store and provider. Requests remain bound to the
original conversation/runtime even after rename or transfer. An uncertain write is
journaled before socket I/O and is never resent. A request still `admitted` with
nothing written or typed is not uncertain: the supervisor dispatches it once more
when its worker died ([delivery workers](#one-send-request-for-every-provider)).
Receipt and idempotency storage is bounded and never silently evicts replay
protection ([receipt retention](#receipt-retention)). Expiry is an unsent submission
deadline, not cancellation of accepted native work.

### One send request for every provider

A live `api send` takes the same body for every provider, and the caller never
branches on the provider (`capabilities.send_request` lists the intents, the mapping
and the options each provider ignores):

```json
{"api_version":1,"message_id":"<uuid>","session_id":"<uuid>","expected_generation":"<hex>",
 "mode":"deliver","text":"...","expires_at":1800000600,"origin":"agent","from_component":"guard"}
```

`mode` is an intent, or one of the provider modes `submit`, `queue`, `steer`:

| Intent | Codex | Claude | Antigravity, Gemini |
|---|---|---|---|
| `deliver` | `submit` with `if_busy:"queue"`: idle starts a turn, busy waits in the native queue | `submit` (priority `next`): idle starts a turn, mid-turn the running turn reads it at its next tool boundary | `submit`: idle Enter starts a turn, busy Enter follows the provider's queue setting |
| `when_idle` | `queue` (native queue) | `queue` (priority `later`, peer message) | `queue` (the key that queues) |
| `interrupt` | `steer` of the active turn, its target read by Sessionsation (as `steer-target`); with no turn it can steer, delivered as `deliver` with `delivery.fallback_reason` | `steer` | `steer` (the key that sends immediately) |

`interrupt` hands the text to the running turn as soon as the provider allows; it
never cancels the turn (only `steering.interrupt:true` on a hosted Claude session
does). A Codex `interrupt` that names its own `steering` target is sent with it.
`resume` and `new` (headless and recovery sends) are not intents.

`delivery`, `await_reply`, `timeout_seconds`, `if_busy`, `recovery` and the `steering`
keys may be sent to any provider. One the target's provider does not use is left out
and listed in `ignored_options` (a list, empty when nothing was left out) with
`ignored_reason` (one sentence, or null) on every live result and receipt. Unknown
option values and a path a provider has but cannot use now
(`TERMINAL_QUEUE_UNSUPPORTED`, `PEER_INTERRUPT_UNSUPPORTED`) are still refused.

| Option | Used by | Ignored for |
|---|---|---|
| `if_busy` | Codex `submit` (an intent sets it) | Claude, Antigravity, Gemini; Codex `when_idle` / `interrupt` |
| `recovery` | Codex `resume` | Claude, Antigravity, Gemini |
| `steering.expected_turn_id`, `steering.expected_runtime_generation` | Codex `steer` | Claude, Antigravity, Gemini |
| `steering.interrupt` | hosted Claude | Codex, Antigravity, Gemini |
| `timeout_seconds` | Claude, Antigravity, Gemini workers | Codex |
| `await_reply` | Claude peer messages | Codex, Antigravity, Gemini |
| `delivery` (`peer_message`, `native`, `hosted_stream_json`, `terminal_input`) | Claude; `terminal_input` also Antigravity and Gemini | every value but `auto` on Codex; `peer_message`, `native`, `hosted_stream_json` on Antigravity and Gemini |

Every live result and receipt has one delivery block:

```json
"delivery":{"mode_requested":"deliver","intent":"deliver","mode_used":"queue","framing":"native_queue",
 "queued":true,"held":false,"fallback_reason":null}
```

`mode_used` is the provider mode that ran; `framing` is `native_queue`,
`native_steer`, `peer_message`, `terminal_input` or `hosted_stream_json`. `queued` is
true while the provider keeps the message in its own queue behind a running turn;
`held` while it is kept back before the conversation (a Codex compaction or
model-change hold, a Claude peer message reported `held`). Both become false once the
turn takes the message. Older messages report their own mode as `mode_requested`,
`intent:null` and empty `ignored_options`.

**Delivery workers belong to the supervisor.** A Claude peer or terminal send and an
Antigravity or Gemini terminal send are journaled by the sending CLI, which then asks
the supervisor (control method `message/worker`) to start the delivery worker as the
supervisor's own child, with the sender's environment, so the sender may exit right
after `api send` returns. Without a reachable supervisor the CLI starts the worker in
a new session (`setsid`). A worker's stderr goes to `<message_id>.worker.log` next to
its journal record (restarted above 64 KiB), retired with the record. A request
whose worker is gone while still `admitted`, with nothing written, no terminal step
and no dispatch time, provably never reached the provider: more than 10 s after its
last change the supervisor (every 5 s, and any receipt read) dispatches it once more
(`execution.redispatches`, at most 1; `execution.redispatched_at`). Only if that
worker dies the same way does it end `rejected` / `WORKER_EXITED_BEFORE_DELIVERY`.
Nothing at or past `dispatching`, written or typed is dispatched again.

### Deliberate in-turn steering

Negotiate `steering.version == 1`. `mode:"steer"` is never a fallback for Submit,
Queue or Resume. For Codex it sends `turn/steer` to an existing busy turn without
interrupting, cancelling, restarting, launching or queuing a later turn. Native
acceptance does **not** promise immediate consumption or a response. Claude, Gemini
and Antigravity steering are described per provider.

First call `api steer-target --session ID --expected-generation GENERATION`. This
bounded read-only check returns `active_turn_id`, `runtime_generation`, catalog
`generation`, availability/reason and five-second evidence timestamps. Normal
observations advertise `actions.steer`; callers still need the explicit target
lookup. Set `mode:"steer"`, omit `recovery`, and add:

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
gates under the lifecycle locks. Idle, approval-blocked, stale, moved or
changed-turn targets are rejected without substitution, and the native operation
also enforces the expected active turn. The journal enters uncertainty **before**
native I/O. Retries after an attempted steer, process restart or lost
acknowledgement return the original receipt and never resend. Before dispatch, a
failed preflight produces a terminal rejected receipt. Only provably unsent local
intent can be cancelled; accepted/uncertain steering cannot
(`STEERING_CANCELLATION_UNSUPPORTED`). A steer to a session with a pending
compaction is refused ([compaction](#compaction-at-the-next-idle-point)).

The verified native profiles distinguish automatic compaction within a regular turn
from a dedicated compact turn. Automatic compaction retains accepted Steer input
until model continuation. Dedicated compact/review turns reject injection before
acceptance; the exact structured native rejection becomes a terminal `rejected`
receipt with `compact_turn_not_steerable` or `review_turn_not_steerable`. Generic
rejections and lost acknowledgements remain uncertain. Native metadata has no
compact-task flag, so a preflight can still advertise eligibility before this final
rejection. The [native fixture](operations.md#native-messaging-compatibility)
describes verification.

Steering receipts use `delivery_semantics:"deliberate_in_turn_steer"`.
`acknowledged_turn_id` is present only after a matching native acknowledgement.
`facts.native_acceptance` distinguishes `accepted`, `input_observed` (no
acknowledgement, but the steer's own input item was found), `unknown` (possible
dispatch) and `not_attempted`. A lost/malformed acknowledgement or unclassified
native error remains `delivery_unknown`. Receipt polling never attempts delivery.
The native queue ID is null. Reply attribution:
[steer reply correlation](#steer-reply-correlation). Schema: `SteeringIntent`,
`SteeringTarget` and the conditional `MessageRequest`/`Receipt` rules.

### Steer reply correlation

Negotiate `capabilities.steering.reply_correlation == "after_steer_item"`
(`steering.reply_observation` publishes the bounds). Steering receipts,
`api steer-target` and readiness `actions.steer` report `acceptance_only:false`.
`turn/steer` carries `clientUserMessageId` = the request `message_id`; the steered
turn's full item history then contains a `userMessage` whose `clientId` is that ID,
after the input that started the turn. An assistant item still streaming when the
steer arrived can be discarded natively. The summary items view omits steer inputs,
so only the full view of the single steered turn is read.

- **Anchor.** Immediately before `turn/steer`, Sessionsation reads that turn
  (`thread/turns/list`, `itemsView:full`, `limit:1`) and records item identifiers
  only: the count of earlier user inputs and items, and the last assistant item ID.
  The turn must still be latest and in progress and must not already contain the
  ID, otherwise the steer is rejected unsent (`active_turn_changed`,
  `ambiguous_native_correlation`).
- **Read bound.** Anchor and observation reads accept one native frame of up to
  64 MiB; only item identifiers, client IDs and assistant text are retained. A larger
  frame fails closed with `native_frame_limit` before `turn/steer`; more than 8192
  items is `steer_turn_too_large`. Full-turn reads scale their time budget with the
  frame bound (base plus about one second per 16 MiB).
- **Observation.** `api receipt` on a `native_accepted`, `delivery_unknown` or
  `execution_started` steer reads the same turn newest-first (at most 8 one-turn
  pages, 8192 items) and locates the steer input by `clientId`. It never resends and
  does not hold the writer lock during the read.
- **Attribution.** The reply window starts after the steer input and ends at the
  next user input or the end of the turn. `reply` is the last `final_answer`
  assistant item in that window, otherwise the last non-empty one. When the window
  runs to the end of the turn the same text also answers the turn's earlier input:
  `facts.shared_with_turn_tail:true`. A later steer bounds the window
  (`correlation.next_input_item_id`).
- **States.** `execution_started` (input recorded, turn in progress, `reply` may be
  partial with `final:false`); `completed_with_reply` (`reply.final:true`);
  `completed_without_reply`; `failed` with `steer_turn_interrupted` (partial reply
  kept) or `input_not_consumed` (acknowledged, but the turn ended without the steer
  input). An unacknowledged steer only gains positive evidence; it never becomes
  `input_not_consumed`. Terminal states are cached and never regressed.
- **Re-verification.** Every read re-checks the anchor: the steer input must keep
  its item ID, appear after at least as many user inputs as at steer time, and the
  recorded pre-steer assistant item must not appear after it. A lost input reports
  `observation.error_code:"steer_anchor_lost"` and keeps the stored evidence; the ID
  in a different turn reports `correlation_changed`. Ambiguous or oversized evidence
  (`ambiguous_native_correlation`, `reply_too_large`) is never attributed. Codex
  renumbers legacy `item-N` ids when it rebuilds a finished turn: an input found
  under another legacy id (same `clientId`, exactly once) is reconciled, keeping
  `steer_input_item_id` and naming the new id in
  `correlation.completion_input_item_id`; the legacy `last_agent_item_id` is then
  not compared.

Receipt fields: `facts.turn_status`, `facts.reply_final`,
`facts.shared_with_turn_tail`, `facts.input_recorded`, `input_consumed` (`true`,
`false` or `"unknown"`) and `turn_completed`; `correlation` is `{native_turn_id,
steer_input_item_id, next_input_item_id, reply_item_id, observed_runtime_generation,
anchor}`; `reply` is `{item_id, text, phase, final, shared_with_turn_tail}`.
`observation.state` is `acceptance_only`, `observed`, `uncorrelated`, `unavailable`
(with `error_code`) or `cached`. Receipts without an anchor are correlated by
`clientId` alone. Fixtures: `receipt_steer_progress`, `receipt_steer_reply`,
`receipt_steer_interrupted`, `receipt_steer_not_consumed`.

### Explicit Resume-and-submit

`api send` with `mode:"resume"` on a Codex session is caller-requested continuation
of the same conversation, never conversion, new-conversation creation, steering,
forced interruption or automatic restart. Negotiate `message_recovery` (and
`message_recovery.exact_item_idle_start` for interrupted-thread recovery); the
provider `resume` flag refers to saved-session launching and does not imply this
mode. Submit, Queue, observation, discovery and receipt polling never launch an
exited agent. For Claude, Gemini and Antigravity, `mode:"resume"` is
[headless delivery](#headless-delivery) instead.

The request retains the ordinary message fields and adds `recovery`:

```json
{
  "expected_state": "interrupted",
  "expected_previous_turn_id": "turn-from-observation",
  "expected_stop_id": null,
  "expected_runtime_generation": "64-character-runtime-generation"
}
```

Copy current observation values; previous-turn and stop keys must be present, null
only when none was observed. Expected states are `interrupted`, `errored` and
`stopped`. A retained live attachment requires its exact native runtime generation.
A live idle session requires a recorded stop token to use `stopped`; idle alone is
not a stop. Unknown, unobserved, stale or nonterminal history is not proof that
recovery is supported. A live process with an unloaded conversation must reconnect
or be explicitly stopped first.

For an **exited agent**, first generate an explicit launch plan:

```sh
sessionsation api recovery-plan < recovery-target.json > recovery-plan-envelope.json
```

`recovery-target.json` contains `session_id`, `expected_generation` and one host,
`{"kind":"herdr","session":"HOST-NAME","socket":"/absolute/owned/herdr.sock","workspace_id":"SPACE-ID"}`
or `{"kind":"tmux","socket":"/absolute/owned/tmux.sock","session":"unused-session-name"}`.
A new normal tab/pane is created without moving any live agent. Supply the returned
opaque `RecoveryPlan` unchanged as `recovery.launch`; its `previous_turn_id`,
`previous_turn_status` and `stop_intent_id` describe the saved target. Null runtime
generation is acceptable only for exited-process recovery with a plan. The saved
account and directory cannot be overridden in this mode. Missing recorded
permissions require an explicitly configured launch; this mode never falls back to
permissive/default-account settings.

Planning and admission validate conversation identity/history, account eligibility,
recorded approval/sandbox policy, native owner/profile, empty queued work, no active
background work, directory identity, provider/backend executable hashes,
configuration fingerprint and destination socket/Space identity. The worker and
launcher recheck these before their transitions; changed material requires a new
plan. No credential owner is started. All mutators sharing the root must understand
`message-recoveries` reservations; an older supervisor is refused with
`RECOVERY_SUPERVISOR_UPGRADE_REQUIRED_COORDINATE_ALL_ROOT_WRITERS`.

The message UUID is journaled **before launch**. One explicit invocation can start
one worker; retries and status queries only inspect its original journal and
receipt. Changed payload/preconditions/plan under the same UUID are
`MESSAGE_ID_CONFLICT`. Launch dispatch uncertainty is `launch_unknown`, separate
from `delivery_unknown` for an uncertain native queue write. Once native write
intent exists, the queue request is never replayed. Queue addition uses the
provider's atomic non-steering dispatch path, checked against the
[native queue implementation](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/ext/queue/src/service.rs).

`api capabilities.native_protocol.supported_versions` lists the exact verified
app-server versions (0.163.0 through 0.166.0). `api observe` reports the actual
handshake version under `observation.native_protocol`, including on a rejected
version. Unknown versions fail closed with `unsupported_native_version`; check the
account app-server version, which can differ from the terminal executable. No
automatic downgrade, restart or replay occurs after a provider upgrade.

Resume receipts add `launch: {required, phase, confirmed, detail, generation,
target, created_at, updated_at, reconcile_command}` and `recovery_start: {state,
turn_id}`. `launch.generation` is the candidate post-launch **catalog** generation,
usable only once confirmed; `accepted_runtime_generation` is the native attachment
generation, null until delivery is bound. `launch.confirmed` requires a positive
process/attachment check, not message acceptance; always inspect `facts`,
correlation and reply. `recovery_start.state` `confirmed` means the native start was
acknowledged; `unknown` means it may have executed; `rejected` means it was refused,
possibly because automatic dispatch already consumed the item. Neither permits
replay. The stop token is cleared after a positive, idle-checked
`thread/queue/start` acknowledgement for the exact new queue item, or durable
correlation proving that the Resume input executed, by compare-and-clear under the
conversation lock (a newer stop request survives). Queue insertion alone does not
wake an interrupted thread. Recovery preflight reads the native summary freshly
within one five-second budget. Only the original Resume dispatch invokes the native
start; retries, reconciliation and polling never do. Before binding delivery,
recovery waits up to ten seconds for the local receipt ledger lock. Public receipt
polling releases the root-wide receipt writer lock during native observation, then
commits only if the durable receipt still matches its starting snapshot.

```sh
sessionsation api receipt MESSAGE-ID
sessionsation api recovery-reconcile MESSAGE-ID
```

Reconciliation acquires the original worker lease and never launches, resumes,
sends, cancels or retries input. Before recorded dispatch it can mark a stopped
worker failed (a created plain shell may remain). After dispatch it requires
positive evidence of the original attachment; it can settle launch facts and release
the local reservation, preserving the delivery ledger. If launch remains ambiguous it
refuses and keeps the reservation. Routine receipt polling does not take over a
worker. Pending recovery cancellation without verified worker quiescence is
unsupported. Actionable refusals distinguish missing plans, changed
executable/configuration/history/runtime/stop/turn, missing permissions or
directories, unavailable accounts, queued/background work, busy or unverified native
state, and incompatible owners.

### Admission and contention

The messaging ledger has one `adapter.lock` (lease) across legacy and v1
send/status/receipt/cancel operations. A read that changes nothing (`api receipt`,
legacy `status`/`observe` or `modes` status of a settled, unknown (not found) or
retired id, and every `events` page) holds it shared, so readers never wait for each
other; a write holds it exclusively and excludes readers too. A read that must settle,
observe or cancel then waits for the exclusive lease within the same budget. Codex
`api send`, `api receipt`, `api cancel`, `steer-target`, recovery and the legacy
`messaging` commands wait for the lease (up to 20 s for sends, 10 s for reads) and for
the per-session ownership lock (otherwise `OPERATION_BUSY`); `ADAPTER_BUSY` is returned
only after that budget. The lease is not held across Codex app-server I/O (status
read, `thread/queue/add`, steer, anchor read, reconciliation, reply observation,
native cancel): the submission is journalled `unknown` (`steer_dispatch_intent`,
`cancellation_uncertain`) under the lease before the I/O, so exactly one process
delivers a message id, and the result is saved only if the receipt is unchanged, so
a stale result or error never regresses a state recorded meanwhile. Claude, Gemini
and Antigravity sends check the shared message-id namespace under this lock and wait
up to three seconds. Do not remove locks, stop legacy observers or introduce another
owner to bypass it.

`ADAPTER_BUSY` on `api send` precedes request binding, admission and native
dispatch for that invocation; it does not negate an earlier admission of the same ID.
Retry receipt lookup or the **identical request**, never a replacement message. For
`api receipt`, it means lookup could not run and says nothing about the send. After
contention clears, `RECEIPT_NOT_FOUND` means no v1 binding or recovery journal exists
for that ID in the selected root; verify the exact root and original request before
drawing a conclusion. A wrong root, unbound legacy ID or lost original error must not
trigger a replacement send.

### Receipt evidence

`accepted_generation` is the original backend generation; the independent
`accepted_runtime_generation` is the original native attachment generation (or null
while Resume has not yet bound delivery). `correlation` is null until positively
matched by request ID, or contains `native_turn_id`, `original_input_item_id`,
optional `completion_input_item_id`, `observed_runtime_generation` and optional
`reply_item_id`. Completion projection IDs never overwrite original input IDs;
runtime replacement may change the observed runtime generation without rebinding.
`native_queue_id` is separate. Only public final reply item/text/phase are projected.

`observation` reports not_checked, observed, uncorrelated, unavailable, cached or
not_applicable. `checked_at` is the receipt-query time; `native_attempted_at` is
null when no native read occurred. `evidence_observed_at` is durably recorded when
correlation was positively observed; cached queries do not refresh it. Freshness is
fresh for less than five seconds, otherwise stale. `error_code` is a bounded observer
diagnostic, not proof that delivery failed. Correlation, acceptance and completed
replies survive observation errors and restarts. Poll receipts no faster than every
five seconds; for push-style waiting use [session events](#session-events).

For verified local legacy histories, receipt reads preserve bounded scan progress
between polls: each poll reads at most one 50-turn summary page (or rechecks the
newest page) within a five-second budget. Progress is private, keyed to the receipt,
runtime generation and file identity, and expires after five minutes. All pages must
be checked for duplicate client IDs and the newest page must agree before a candidate
is published. `reply_scan_pending` means keep polling the same receipt;
`reply_scan_changed` and `history_changed_during_observation` mean the snapshot
changed; `reply_scan_limit_exceeded` is the 1,000-turn bound; `reply_observation_busy`
means another observer owns that receipt's scan.

| State | Evidence |
| --- | --- |
| pending_local | Durable local admission; no confirmed native acceptance |
| native_accepted | Positive queue acceptance; consumption/completion unproven |
| execution_started | Correlated native input/turn; no terminal reply yet |
| completed_with_reply | Correlated public terminal reply observed |
| delivery_unknown | Submission may have occurred; never automatically replay |
| rejected | Preflight refused this request |
| expired | Submission deadline passed while provably unsent; or, with reason `outcome_unknown_at_retention_horizon`, retired unfinished |
| cancelled | Local cancellation or positive supported native cancellation |
| failed | Correlated native turn failed/interrupted; no successful reply |

Native-provider receipts add `held`, `completed_without_reply`, `interrupted` and
`refused`. `facts` contains bounded positive booleans for local admission, native
acceptance, recorded input and observed reply; `input_consumed` remains unknown
unless observed. Native acceptance survives expiry.

**Codex submit diagnostics** (`capabilities.codex_submit_diagnostics`). After a
transport error the receipt carries `facts.native_error` `{kind, message, at}` with
`kind` `endpoint_unavailable` (socket missing, connection refused, handshake failed,
connect timeout), `disconnected`, `timeout`, `native_rejected`, `malformed_response`,
`native_frame_limit` or `unsafe_endpoint`, and a fixed sentence per kind (never message
text, provider output or paths); the first error of an uncertain receipt is kept. An
error after the add was written keeps `delivery_unknown` (`submission_uncertain`,
`facts.retryable:false`). A connection that could not be made before anything was
written is `rejected` with `endpoint_unavailable` and `facts.retryable:true` (send again
with a new id); a held-input release keeps its retry loop. Codex rows carry
`input_delivery.health` `{last_outcome: accepted|not_delivered|uncertain|null,
last_attempt_at, last_success_at, consecutive_failures, last_native_error}` from
`messaging/input-health/<conversation>.json`, written by submit attempts only.

**Send timings** (1.71.2). Every submit/queue/steer receipt (Codex, Claude, Gemini,
Antigravity) adds `facts.readiness_ms`, `ledger_wait_ms`, `ledger_hold_ms` and
`provider_ms`, integer milliseconds recorded once by the admitting send (a resend never
changes them): the readiness wait before admission (Codex `await_ready`, excluding its
busy-ledger pre-check open; Claude the peer endpoint wait; 0 when none ran), the admission's
ledger wait and its hold until just before the receipt is created (`accept_timed`; Claude: the
message-id check, excluding the `admission.lock` wait), and the provider call (Codex
`thread/queue/add`, Claude peer frame write). Each is `null` when not recorded: older
receipts, and `provider_ms` until a provider call was recorded (steer, typed and
hosted input never record it).

### Receipt retention

`api capabilities.receipt_retention` describes it. Each receipt store (Codex
messaging ledger, native input journal, headless deliveries) holds at most 10,000
receipts; the supervisor's hourly [retention](operations.md#retention) pass keeps them
far below it (since 1.67.1 a send runs a pass itself, under the exclusive messaging
lock, only from 9,000 receipts), so sends never hit
`RECEIPT_CAPACITY_REACHED` in normal use:

- A certainly terminal receipt (`completed_with_reply`, `completed_without_reply`,
  `failed`, `rejected`, `cancelled`, `expired`) is retired once it is older than
  `receipt_retention_days` (default 14). Age is counted from `expires_at` for Codex
  receipts and from the last record update for journals.
- A native journal record whose `phase` is `written` (or `complete`), whose worker
  has exited and whose state is `execution_started`, or `native_accepted` backed by
  positive evidence (the receiver's `delivered` status, a native queue or screen
  evidence), retires like a terminal receipt and keeps its real state in the
  tombstone (`delivered_journal_states_retire_as_terminal`, `delivered_requires`).
- Any other non-terminal or uncertain receipt (`pending_local`, `native_accepted`
  with only a silent transport acceptance, `execution_started`, `delivery_unknown`,
  or a record with a live worker) is never retired before
  `receipt_unknown_horizon_days` (default 90). Past that horizon it is saved as
  terminal `expired` with reason `outcome_unknown_at_retention_horizon` (a provably
  unsent `pending_local` becomes `expired` without that reason), then retired. That
  reason means Sessionsation stopped observing; never resend the message.
- A Codex binding that never produced a receipt is retired as `cancelled` (when
  cancelled) or, past the horizon, `expired`. Recovery journals and unreadable
  records are kept.

Every retired id keeps a compact tombstone (id, final state, reason, retirement
time; no text, target or reply), not counted toward capacity and never pruned. A
retired id is never accepted again by any path: `api send` and `api receipt` return
`RECEIPT_RETIRED` with the final state (legacy messaging: `receipt_retired`). Keep
your own copy of a reply you need beyond the window. Both settings are config keys
(1..=3650 days; the horizon must not be shorter than the retention).
`retention run` reports `policy` (the values used) and `config_error` (null, or why
`config.json` failed to validate and built-in defaults were used, also warned on
stderr).

### Component sender names

A native Claude message from a process that is not inside a managed session is
labelled by the component behind it: optional `from_component` (string or null) on
every `api send` request shape, else `SESSIONSATION_SENDER_COMPONENT` in the
environment of the `api send` process (the field wins; messages Sessionsation sends
itself never read the variable). A component must match
`^[A-Za-z0-9][A-Za-z0-9._-]{0,39}$`: an invalid field is `INVALID_ARGUMENT` before
anything is admitted, an invalid variable is ignored with a stderr warning. The
envelope shows the bare component (`from-name="resource-guard"`,
`from-mode="prompting"`). When it equals, case-insensitively, the name of any catalog
session (any Space, running or not) or `sessionsation`, it is shown as
`from-name="sessionsation:<component>"`; nothing is refused, a field that fell back is
reported as `facts.from_component_prefixed:"collides_with_session"`, a variable that
fell back prints a warning (an unreadable catalog also falls back). A caller inside a
managed session keeps its own name (its canonical name when it fits 64 characters)
and a supplied field is reported as `facts.from_component_ignored:"caller_is_session"`.
Codex targets and headless sends validate and ignore it. The component is not part of
the stored request; the sender of the first admission stands. Capability
`session_spaces.from_component` (`since_contract` 1.50.0, `request_field`, `env`,
`pattern`, `from_name:"<component>"`, `collision_fallback`, `collision_rule`,
`changed_in`, `prefixed_fact`, `applies_when:"caller_not_a_managed_session"`).

### Headless delivery

For a **stopped** Claude, Gemini or Antigravity session, `mode:"resume"` continues
the conversation in a one-shot provider run in its recorded directory; `mode:"new"`
creates a conversation (`ClaudeHeadlessRequest`; the provider entry's
`headless_delivery`: `version:1`, `modes:["resume","new"]`,
`semantics:"headless_print"`, `requires_stopped_session:true`,
`timeout_seconds:{default:600,max:3600}`, `timeout_is_uncertain:true`,
`automatic_replay:false`, `cancellation:false`):

```json
{"api_version":1,"message_id":"UUID","mode":"resume","session_id":"UUID",
 "expected_generation":"HEX64","text":"...","expires_at":UNIX,"timeout_seconds":600}
{"api_version":1,"message_id":"UUID","mode":"new","account_id":"claude:NAME",
 "directory":"/abs/dir","name":"optional label","text":"...","expires_at":UNIX}
```

The message goes on stdin, never argv. Commands: Claude `claude -p (--resume|--session-id)
UUID --output-format json` (no permission bypass); Gemini `gemini -p "" -o json
--approval-mode default (-r|--session-id) UUID` with `GEMINI_CLI_TRUST_WORKSPACE=true`;
Antigravity `agy --conversation UUID --input-format stream-json --output-format
stream-json --print-timeout Ns --disable-slash-commands -p=` with one stream-json user
event (`N` is the timeout minus 5 s; `mode:"new"` first creates the conversation as a
launch does), never `--dangerously-skip-permissions`. Admission refuses before
journaling on `STALE_TARGET`, `SESSION_ALREADY_RUNNING`, `SESSION_STALE`,
`SESSION_OPERATION_IN_PROGRESS`, `SAVED_DIRECTORY_UNAVAILABLE`, `ACCOUNT_REMOVED`,
`ACCOUNT_DELETED`, `ACCOUNT_UNAVAILABLE`, `AUTH_REQUIRED`, `WRONG_AUTH_MODE`,
`INVALID_EXPIRY`, `INVALID_TIMEOUT`. The request is journaled under
`DATA_ROOT/messaging/claude-deliveries/` (all providers; records without `provider`
are Claude) before a detached worker starts the provider once. Interactive resume is
refused while a delivery runs.

Receipt (`ClaudeHeadlessReceipt`, also `api receipt`):

```json
{"message_id","session_id","accepted_generation","provider","provider_conversation_id","account_id",
 "mode","delivery_semantics":"headless_print","state","reason",
 "facts":{"accepted_local","accepted_native","input_recorded","input_consumed","reply_observed"},
 "correlation":{"session_id","reported_session_id","matched"}|null,
 "reply":{"text","text_truncated","is_error","subtype","session_id","num_turns","usage","duration_ms"}|null,
 "execution":{"phase","directory","timeout_seconds","created_at","started_at","finished_at",
 "new_conversation","automatic_replay":false},"observation":{...}}
```

`state`: `pending_local`, `execution_started` (journaled before the process starts),
`completed_with_reply` (session id matched, no error), `failed` (matched with an
error; Antigravity: a status other than `SUCCESS`), `rejected` (provably not executed,
for example `WORKER_EXITED_BEFORE_EXECUTION`; an Antigravity sign-in prompt with no
turn is `AUTH_REQUIRED`; a Gemini run whose output is unparseable after the CLI failed
while establishing its sign-in, with the conversation unchanged, is
`<CODE>: <text>; nothing was sent to the model`), `expired`, or `delivery_unknown`
(`EXECUTION_TIMEOUT`, `SESSION_ID_MISMATCH`, `UNPARSEABLE_OUTPUT`, `OUTPUT_TOO_LARGE`,
`WORKER_EXITED_DURING_EXECUTION`). `reply` is non-null only for a matched session id;
uncertain outcomes are never replayed. `api cancel` returns
`HEADLESS_CANCELLATION_UNSUPPORTED` while pending or executing.

### Public turn checkpoints

Negotiate `capabilities.checkpoints.version == 1`. `api checkpoints < request.json`
is an explicit read-only observation, independent of sending and receipt polling
(Codex only). The request has `version:1`, stable `session_id`,
`expected_generation`, `expected_runtime_generation`, `expected_turn_id` and
optional `cursor` (initially null). Obtain exact turn/runtime identity from a
tracked receipt, `api steer-target` or `api turn-target`; never select a turn by
name. Schema: `CheckpointRequest`, `Checkpoints`, `CheckpointEvent`; fixtures
`checkpoint_request`/`checkpoints`.

The response exposes only native `agentMessage` text marked `commentary`,
`partial_answer` or `final_answer`. No reasoning, user input, tool output, terminal
capture or raw transcript is returned or journaled. History mode is read from the
attached server with metadata-only `thread/read`. `thread/items/list` is used only
for **paginated** histories (legacy histories reject it with -32601); for **legacy**
histories the observer uses `thread/turns/list` with `itemsView:full`, `limit:1`,
descending, verifies the exact latest turn and discards every non-public item before
journaling. It never uses whole-thread `thread/read(includeTurns:true)` or reads
transcript files. Unknown profiles fail closed. No observer, terminal supervisor,
credential writer or subscription is started.

- Every event has a stable content revision `event_id`, native `item_id`, monotonic
  journal `sequence` and `observed_at`. Text is a full snapshot, not a delta; a
  changed item emits another revision. Deduplicate by `event_id`.
- Association is **turn-only**: text does not prove Steer consumption, causation,
  delivery or completion; even `phase:final_answer` is not completion evidence.
  `turn_status` is separately observed native state.
- The requested turn must still be the latest visible turn before and after the
  read. A different turn returns `CHECKPOINT_STALE_TURN`; a transition during the
  read returns `CHECKPOINT_TURN_CHANGED`. Catalog/runtime changes return
  `STALE_TARGET`; never carry a cursor into another runtime/turn. Endpoint failure,
  unsupported profile or timeout yields an error and no advanced cursor. Reads have
  an eight-second overall budget and never send input or launch.
- Paginated reads scan at most 50 native items. Legacy reads inspect one exact
  latest turn, bounded to **8 MiB native wire input and 8192 items**, and project at
  most 50 unseen public snapshots per scan. Both routes return at most 50 retained
  events. `history_mode` reports the route; `checkpoints.history_modes` advertises
  `legacy` and `paginated`. Ordinary messaging connections keep their 1 MiB frame
  limit. `NATIVE_FRAME_LIMIT` or `CHECKPOINT_LEGACY_LIMIT` refuses oversized input
  without partial text.
- The compact success envelope plus trailing newline is capped at **786432 bytes**
  (`checkpoints.max_response_bytes`). `byte_limited:true` marks a page shortened by
  bytes; whole events are preserved and the cursor advances only past returned
  events. A single public text item up to 65536 UTF-8 bytes is supported.
  `scan_pending` means native pages or unseen legacy snapshots remain; `has_more`
  means journal events remain. Save the cursor only after processing the response.
  Unread journal events drain before more native items are scanned
  (`native_page_read:false`, original timestamps kept). This is polling, not
  lossless streaming; empty events do not mean the agent is idle.
- Up to 256 events and 4096 deduplication revisions are retained per exact
  session/runtime/turn. `CHECKPOINT_CURSOR_GAP` means the cursor is expired, from a
  different journal or beyond its tail; start again with a null cursor
  (`history_gap:true` marks truncated older events). The revision ceiling returns
  `CHECKPOINT_RETENTION_LIMIT`.
- Journal writes are atomic and serialized by an observer-only lock
  (`CHECKPOINT_OBSERVER_BUSY`: retry the read). Native rejections expose only
  `error.native:{method,code}`. `observed_at` timestamps the item-page read,
  `checked_at` the response, and `valid_until` is five seconds after observation.

Use checkpoint events for an intermediate activity feed and the original receipt for
delivery/correlation. Do not promote a checkpoint to a completed mailbox reply.

## Pending questions

Negotiate `capabilities.questions.version == 1`. Every `api state` session row has
`pending_questions` (empty when none or the session is not running): questions an
agent asked the user and is waiting on. Question text and option labels are
included.

```json
{"request_id":"call_…","asked_at":1793285639,"kind":"codex_request_user_input_async",
 "blocking":false,"turn_id":"…","answerable":true,"unanswerable_reason":null,
 "questions":[{"id":"q1","text":"Which inputs…?","options":[{"id":"o1","label":"Synthetic"}],
   "multi":false,"allow_free_text":true}]}
```

Detection is passive (incremental history scan; no provider contact) and survives
`state --cached`:

| kind | provider tool | ends when | answer path |
|---|---|---|---|
| `codex_request_user_input` | Plan-mode `request_user_input` (blocking) | its tool output, or the turn ends | `item/tool/requestUserInput` response |
| `codex_request_user_input_async` | Default-mode `request_user_input_async` (the turn keeps running) | a new turn starts, or Sessionsation answers | follow-up message on the public non-steering queue, delivered when the turn ends |
| `claude_hosted_ask_user_question` | `AskUserQuestion` in a hosted session | its permission request is answered | `can_use_tool` allow with `updatedInput.answers` |
| `claude_ask_user_question` | `AskUserQuestion` in a Claude terminal | its `tool_result` | none: `answerable:false`, `unanswerable_reason` `PROVIDER_TERMINAL_ONLY` |

Gemini records `ask_user` calls only after they complete and Antigravity questions
are not detected, so their rows have `pending_questions:[]` and
`pending_questions_unsupported_reason`. An async Codex question answered in the TUI
is not visible until the next turn starts; answer each question in one place.

`api answer --session ID --expected-generation G --request-id R [--answers JSON]`
(stdin when `--answers` is absent, at most 16 KiB) answers once. `QuestionAnswers`
maps every question id to `{"option_ids":[…],"text":"…"}`: at least one of them,
one option unless `multi`, text only when `allow_free_text`. Codex encodes text as
`user_note: …` after the selected labels; Claude joins labels and text with `, `.
The `QuestionAnswerReceipt` state is `answered` (Codex `serverRequest/resolved`, or
the hosted worker wrote its control response), `queued` (an async Codex answer
accepted by `api send` mode `queue`; `message_receipt` is that receipt),
`delivery_unknown` (never resent) or `rejected` (nothing sent; `reason`).

The catalog generation is checked (`STALE_TARGET`) and one answer per session runs
at a time (`OPERATION_BUSY`). The receipt is journaled before the native write;
repeating the identical answer returns it with no native I/O. A different answer
after an attempt is `QUESTION_ALREADY_ANSWERED`; a question that is gone, answered
elsewhere or superseded is `QUESTION_NOT_PENDING`. For a blocking Codex question
Sessionsation reads the thread first and never loads it, subscribes with
`thread/resume` (`excludeTurns`) so the app-server replays the pending request, and
answers only the replayed request whose `itemId` equals `request_id`; if it is not
replayed it sends nothing. Codex accepts the first response from any client.
Answers are recorded in `messaging/questions/events.jsonl`.

## Session events and notifications

### Session events

`api events [--after SEQ] [--wait SECONDS] [--session ID] [--kind KIND] [--limit N]`
returns `{events, cursor, gap, oldest_seq, latest_seq, more, waited}`
(`capabilities.session_events`). Every event is `{seq, session_id, provider, kind, at,
source, provider_conversation_id, detail}`; `kind` is `turn_started`, `turn_completed`
(`detail.status` `completed`/`interrupted`/`failed`), `subagent_started` /
`subagent_stopped` (`detail.agent_id`, `agent_type`), `background_task_started`
(`detail.task_id`), `background_task_finished`, `compacted`, `idle`, `session_started`
or `session_ended`. No event carries prompt, reply or tool text. Pass `cursor` as the
next `--after`; `--wait` (at most 300) blocks for the first new one; `more:true` means
`--limit` (default 500, at most 2000) cut the page. The journal
(`registry/events.jsonl`) is trimmed to 5000 past 10000; `gap:true` says events after
`--after` were dropped. The first look at a running process records it without an
event.

| Provider | Source | Kinds |
|---|---|---|
| Claude | `hook`: Claude Code hooks (below), ingested every 2 s by the supervisor (`SESSIONSATION_EVENTS_SECONDS`, 0 disables) and on every `api events` / full `api state` | all but `background_task_finished` |
| Codex | `supervisor`: each full state observation of a running session's turn facts (a turn end is identified by its rollout timestamp; `task_complete` is `completed` or
`failed`, `turn_aborted` `interrupted`) | `turn_started`, `turn_completed`; `compacted` (`source:"operation"`) when a Sessionsation compaction completes |
| Antigravity | `supervisor`: the agy step log, as transitions between observations (a very short turn can be missed) | `turn_started`, `turn_completed` |
| Gemini | none (`supported:false`) | |

**Hooks** (`capabilities.hooks`). Every Claude launch (and `api hooks install
[--account ID]`) puts one async command hook per needed event (`SessionStart`,
`SessionEnd`, `UserPromptSubmit`, `Stop`, `StopFailure`, `SubagentStart`,
`SubagentStop`, `PostToolUse` matcher `Bash`, `PostCompact`, `Notification` matcher
`idle_prompt`) into the account's `settings.json` as its own entry
`{"hooks":[{"type":"command","command":"<sessionsation> provider-hook claude-event 2>/dev/null || true","async":true,"timeout":10}]}`.
The path is the package's stable `<pkg>/current/sessionsation` when the running
binary is the release it points at (otherwise the running path); a missing binary
makes the hook exit 0 silently, and an entry naming another path is rewritten in
place. An entry is Sessionsation's only when its command contains `provider-hook
claude-event`; user hooks and other keys are kept; an installed file is not rewritten
(`changed:false`, `already_installed:true`). A symlinked or invalid settings file is
refused (`SETTINGS_IS_SYMLINK`, `SETTINGS_INVALID`) and reported; a launch never fails
for it, and `SESSIONSATION_CLAUDE_HOOKS=0` skips the launch-time install. The hook
appends a content-free line to `<home>/.sessionsation/hook-events.jsonl` and always
exits 0. Hooks apply to Claude processes started after the install. Codex and
Antigravity need no hook (`method:"supervisor"`; Codex's one `notify` slot belongs to
the user). `api hooks` returns one row per account (`account_id, provider, supported,
method, required, events, installed, missing_events, stale_events, user_hooks,
last_event_at, reason`, plus `changed`/`already_installed` for `install`) and one per
Claude conversation the hooks reported (`session_id, provider_conversation_id,
process_started_at, last_event_at, ended, subagents_running`).

### Turn-end notifications

`api turn-notify --session ID --expected-generation G (--enable [--notify-session ID2]
| --disable)` (Codex sessions; capability `turn_notify`; rows carry
`turn_notify:{enabled,notify_session,enabled_at}`). While enabled the supervisor (poll
`SESSIONSATION_TURN_NOTIFY_SECONDS`, default 5, 0 disables) records an event each time
a turn ends: `kind:"turn_ended"` (completed or interrupted) or `kind:"error"` (the
provider text in `error`). A turn is identified by the rollout timestamp of its end,
so it is reported exactly once; the first observation after enabling is a baseline.
Settings and the last 500 events live in `registry/turn-notify.json` and never change
a generation. `api turn-events [--after SEQ] [--wait SECONDS]` returns
`{events:[{seq,at,session_id,name,kind,error,delivery}],cursor,gap}` (also the
`idle`/`exited` events of `api idle-notify`); `--wait` is at most 300. With
`--notify-session` the supervisor also queues one short message (`mode:"queue"`,
never a steer, never retried; the session's name and id and, for an error, the
provider text) to that session; its outcome is the event's `delivery`. The journal is
the source of truth. For every provider use [session events](#session-events).

## HERDR placement

### Batch placement observations

Negotiate `capabilities.placement.version == 1`. `api state` includes each session's
`placement` and a top-level `hosts` catalog; `api placements` returns all catalog
memberships keyed by stable session ID plus the host/Space catalog; `api session`
includes the same placement. `api spaces` and `original_host` remain launch
provenance and move-admission helpers, not membership discovery. One bounded HERDR
snapshot per bound host is shared with name discovery; pane → tab → Space must agree.
Host and Space IDs are opaque UUIDs persisted under the data root (scoped to the
explicit HERDR endpoint and server name, and to that host's native Space ID); labels
are not keys. Each membership includes `catalog_generation`. Public projections contain
no socket, filesystem locator or raw pane.

`state` is `observed`, `last_known` or `unobserved`. `observed_at` and `valid_until`
(30 seconds later) describe the evidence; `checked_at` the last attempt. Failed
snapshots, changed attachments, expired evidence and stopped sessions keep the prior
placement with a reason, never freshened by a failed read. Sessions never observed
remain `unobserved`. The host catalog retains last-observed Spaces with per-Space
times; absence does not prove deletion. `api placements` includes catalog history;
join it to your state/archive filter by session ID. Placement is not message readiness
or move authorization. Schemas: `Placement`, `PlacementHost`, `Placements`.

### Choosing a Space and a pane

A requested Space (`--space` on `resume`, `launch`, `move-space`, `add-to-herdr`) is
resolved as the HERDR workspace id, then the visible label (exact, then
case-insensitive). The Space-label rule (`INVALID_SPACE_LABEL`): 1-128 bytes, no
surrounding whitespace, control characters or `/`, not `.`/`..`.
`capabilities.new_space` (`{version, resume, add_to_herdr, move_space}`) advertises
`--new-space LABEL`, exclusive with `--space`: it reuses an existing Space of that
label, else creates one. `--create-space` requires that no Space of that label exists
(`WORKSPACE_NAME_EXISTS`); for `resume`/`add-to-herdr` its label is the Space part of a
canonical `--name`. A new Space uses its own initial shell pane for the session, so
nothing extra is created or closed (`add-to-herdr` names that tab and returns
`new_space: bool`).

`api resume` places the session, in order: `--space`, `--new-space`, `--create-space`,
else the stored Space (an existing Space with that label, or a new Space with exactly
that label), else the Space of a canonical `--name` (`SPACE_NOT_FOUND`), else a new
Space labelled `sessionsation`. A canonical `--name` must agree with the destination
(`SPACE_MISMATCH`) and be free there (`SESSION_NAME_EXISTS`); the destination label is
stored on a verified start (best effort).

**Idle-tab reuse** (`capabilities.pane_reuse`, setting `reuse_idle_tabs`, default
`true`). A start or resume into an existing Space (`api launch start`, `session
launch`, `api resume-launch start`, `api resume --space/--new-space`, Auto Start) first
picks, in order: (1) for resumes, the session's recorded pane (the attachment, else
`auto_start.last_pane`) if it is in the destination Space and reusable: the foreground
process group is the shell and a single idle shell (`bash`, `sh`, `zsh`, `fish`) runs
there; (2) a tab whose label equals the label a new tab would get, with exactly one
reusable pane that is no other session's attachment (first in tab order); (3) a new
tab. A reused pane gets exactly the command a new tab would get; no other keys are
sent and the screen is not cleared. The pane is re-tested right before typing (else a
new tab opens) and claimed for two minutes (`registry/pane-claims.json`). Any
uncertainty means a new tab. Receipts and the `api resume` result gain
`pane_reused:true` and a `detail`. `session add-to-herdr` does not reuse panes.

**Session temp directory** (`capabilities.session_tmp`, setting `session_tmp_root`). Every
session process Sessionsation starts (launch, resume, restart, account switch; all
providers) gets `TMPDIR`, `TMP` and `TEMP`, and Claude also `CLAUDE_CODE_TMPDIR`, set to
`<root>/<provider>/<account>` (default root `<data_root>/tmp`; mode 0700, created if
missing). A symlink or a directory of another owner is refused (`SESSION_TMP_UNSAFE`).
Setting `session_tmp_root` to `"none"` or `""` keeps the inherited environment. Running
sessions keep their old temp directory until relaunched; the daemon and workers are unchanged.
From 1.72.2 the same variables are set on `codex app-server daemon start|restart`, the command
that starts an account's shared Codex app-server (the parent of Codex tool commands); the managed
daemon inherits it. `capabilities.session_tmp` carries `codex_app_server: true` and `applies_at:
"next app-server start or restart"`: a running server keeps its old environment, and it is
replaced only when the supervisor finds no server (`account/prepare`, the first Codex launch
after the server exited or was stopped), on `account/restart-server`, or after the server exits.
A supervisor restart adopts a running server and does not restart it. An unsafe directory
fails that start with `SESSION_TMP_UNSAFE`; the supervisor keeps running. `"none"` passes nothing.
From 1.73.2 they are also set on headless runs (every provider), the hosted Claude process, `claude
attach` and the private stdio `codex app-server` the supervisor starts for an enrolled account when no
shared server runs; each takes its session's (or that account's) own provider and account. An unsafe
directory refuses that start (`SESSION_TMP_UNSAFE`: a headless receipt is `rejected`, a hosted start
`failed`). `capabilities.session_tmp.covers` lists the covered starts. Not covered: auth and version
probes, login and logout (including the Codex re-login server), the Gemini API-key validation run,
the `agy` run that creates an empty conversation before a launch or headless new, the
`doctor --probe` handshake and the stdio server for a Codex `account add` (no enrolled account yet).

### Move a running session between Spaces

`capabilities.space_move.version == 1` advertises it; `session.capabilities.move_space`
gates the selected attachment. `api spaces --session ID --expected-generation G`
returns `SpaceChoices`: stable session ID, captured generation, current Space ID,
existing Spaces with `id/name/current`, and positive pane/terminal identity, read
from the session's own bound host socket even in an unrelated caller environment.

`api move-space --session ID --expected-generation G (--space SPACE | --new-space
LABEL | --create-space LABEL) --request-id UUID` returns `SpaceMoveOperation`, also in
`api operations`. Phases: `admitted → dispatching → reconciling → complete`, or
`failed`/`uncertain`. Admission returns before host dispatch, checks the session
name's uniqueness in the destination Space and a fresh `workspace list` for a new
label; a detached, per-request locked worker revalidates the saved target and
completes placement independently of the caller. Observe the operation until a
terminal phase. No cancel/respond action is offered. The same UUID and target observes
the saved result; changed input is `REQUEST_ID_CONFLICT`. Selecting the current Space
completes without mutation. A new Space is created in the same host request (`pane
move PANE --new-workspace --label LABEL --no-focus`), so it holds only the moved tab;
its id is journaled (`created_space`, reported as `space_id` and `created_space_id`;
`new_space` echoes the label), so Recover or a retry never creates a second one. The
verified destination label becomes the session's `space`; the session label stays on
the new tab.

The pane moves into a new normal tab without focus stealing; the agent process,
account, conversation, working directory and permissions survive, and busy agents
move without interruption. Split siblings stay in the source tab; HERDR may close an
empty source tab/Space; a zoomed tab may need unzooming. tmux sessions, inactive
conversations and cross-instance moves are out of scope (use New/Resume). HERDR can
change the pane ID: the stable terminal ID and kernel process identity tie the
returned pane to the original runtime. Only attachment placement/revision changes;
backend ID, conversation ID, PID/start identity, runtime generation and old receipts
stay. Discovery, switch/restart and `original_host` follow verified placement
provenance within the launch/resume request's epoch; historical receipts are never
rewritten, and reused pane IDs in a later launch do not import earlier history.

Admission shares lifecycle and messaging locks; in-flight or uncertain placement
blocks competing lifecycle mutations. A lost reply or incomplete ownership update is
`uncertain`, never permission to retry: `api recover UUID` is read-only toward HERDR,
finds the original terminal by its stable id in any Space and repairs binding/catalog
metadata. A crash before dispatch can release its unissued intent. Durable
`space-moves/` records keep the original process, terminal and socket identity.

### Add a running session to HERDR

A running session without a HERDR terminal reports `capabilities.add_to_herdr` and
`capability_reasons.add_to_herdr` (`SESSION_NOT_RUNNING`, `ALREADY_IN_HERDR`,
`INTERACTIVE_ELSEWHERE`, `ATTACHED_ELSEWHERE`, `CODEX_TERMINAL_NOT_MOVABLE`,
`NO_NATIVE_ATTACH` (Gemini, Antigravity), `HOSTED_SESSION`, each followed by `: text`).
`api spaces --session ID --expected-generation G --herdr-session SERVER
[--herdr-socket PATH]` lists that server's Spaces (`HostSpaceChoices`,
`current_space_id:null`, `pane:null`).

`api session add-to-herdr --session ID --expected-generation G --herdr-session
SERVER [--herdr-socket PATH] (--space SPACE | --new-space LABEL | --create-space)
[--name NAME]` (`capabilities.add_to_herdr.requires_one_of:["space","new_space"]`)
opens a running Claude background session in a new tab (label NAME, default the
session name; a different NAME also renames the catalog entry) by running `claude
attach <job>` with the account environment; no agent is started. The pane records
itself before exec and success requires its foreground PID to be that `claude attach`
client:

```json
{"state":"attached","session_id","generation","host":"herdr","herdr_session","space_id",
 "tab_id","pane_id","target","name","job_id","attachment":{...},"starts_agent":false,
 "sends_input":false}
```

`state:"attach_unconfirmed"` (generation/attachment null) is never retried; refresh.
Refusals: the capability reasons, `SPACE_NOT_FOUND`, `STALE_TARGET`,
`UNSUPPORTED_PROVIDER_OPERATION`. Stopped sessions use `api resume` with a placement
instead. Schema: `AddToHerdrResult`.

## Conversation archives

Capability `conversation_archive.version:1` advertises list/archive/restore/delete
(`providers:["codex","claude","gemini","antigravity"]`; Codex `compression:"gzip"`,
`storage:"sessionsation_data_root"`; `wait`, `account_wait`) and `native`
(`NativeConversationArchive`: `storage:"moved_native_files"`, `compression:"none"`,
`restore_target:"original_account_home"`,
`scope:"exact_conversation_ids_including_previous_conversation_ids"`,
`delete_removes_catalog_entry:true`, `delete_removes_merged_aliases:true`,
`delete_staging:"deletions"`, `dead_registry_entries_deleted:true`,
`credentials_touched:false`, `session_capabilities`, `refusal_reasons`). `api archives`
returns archived/prepared entries with stable `id`, fresh `generation`, provider
conversation identity, original creation time, original history bytes,
`compressed_bytes` (null for native entries or when unmeasurable) and
`previous_conversation_ids`.

`api archive`, `api restore-session` and `api delete-session` require `--session`,
`--expected-generation`, `--request-id` (UUID) and `--confirm` equal to the stable
session ID, and admit a durable background job (`kind` `archive_session`,
`restore_session` or `delete_session`, with immutable `created_at`); only phase
`complete` confirms it, and the terminal phases are `complete` and `failed`. Jobs are
not cancellable and never replayed; the same request/target/generation returns the
original job, a reused ID with different input is rejected. The worker waits up to
120 s (`SESSIONSATION_ACCOUNT_WAIT_MS` shortens it in tests) for the exclusive
account lock (backoff 50 ms to 1 s; phase
`waiting_for_account`; then `ACCOUNT_BUSY: an account operation held the lock for
120s`). `--wait` polls until `complete` (`ok:true` with the operation) or `failed`
(`OPERATION_FAILED`, `error.operation`, retryable when the detail starts with
`ACCOUNT_BUSY`), at most 300 s (`OPERATION_TIMEOUT`). Every record carries
`nothing_to_archive`. From 1.73.3 a worker that cannot be built or spawned leaves the job `failed` ("Worker did not start: …"), and a `queued` job whose worker never recorded itself, over 10 s old with its `.lock` free, is started once more by the supervisor (about every 20 s, and on an archive, restore or delete refused because of it); the record keeps `respawned_at`. `api operations` and `--wait` report such a job `queued` until the sweep itself writes `failed` under its lock, which it does on a later pass if that worker never recorded itself either ("Worker did not start after one automatic retry; nothing was changed."). A job whose recorded worker died is never restarted: `api abandon-operation --operation ID --confirm ID` ends it `failed` ("Abandoned by the operator: worker gone; original history or the prepared archive is retained for review") once that exact worker is gone and its `.lock` is free, which frees the session (otherwise `OPERATION_WORKER_ACTIVE`; a never-started job `OPERATION_NOT_STUCK`, since the sweep is its only exit; a settled one `OPERATION_ALREADY_TERMINAL`), and its `SESSION_OPERATION_IN_PROGRESS` names that command. No new error code or phase.

Archive/Restore/Delete invalidate the captured generation. Archive hides the session
from normal state (`availability:"archived"`, `archived:true`,
`archive.kind:"managed"`, `resume_block_reason:"archived"`, every capability false
except `restore` and `delete`); Restore returns the same identities as saved history
without starting an agent; Delete is permanent, including known managed archives,
and removes the catalog entry and every merged alias that resolved to it (the id then
fails `SESSION_NOT_FOUND`). Project files, account credentials and upstream accounts
are never touched; external backups are outside its scope. Archive and Delete need
inactive sessions; Archive also disables Auto Start. Prepared archives are listed for
explicit recovery. Do not edit archive manifests or move managed history manually.
Deleted sessions leave only ids in their manifest.

**Codex.** Archive exports verified gzip history to
`DATA_ROOT/sessions/archived/SESSION-ID`, outside credential homes, and removes native
copies only after the export verifies. Restore keeps backend and provider IDs and
rejects live/loaded conversations, stale generations and changed compressed files.
Delete uses the native deletion path for the conversation and its subagents and
removes its catalog entry and recovery journal. Availability depends on verified
inactive history and operation ownership, not enrollment; external homes remain
eligible with final native-server, live-writer and generation checks; every candidate
home is resolved to one path before scanning. Admission refuses with `SESSION_ACTIVE`.

**Claude, Gemini and Antigravity.** The provider owns its home, so jobs move only the
exact-id files of the selected entry's lineage (its `conversation_id` and every
`previous_conversation_ids` member; forks are separate entries):

- Claude: `projects/DIR/ID.jsonl`, the `projects/DIR/ID/` sidecar (subagent
  transcripts, tool results), `file-history/ID/`, `session-env/ID/` and
  `todos/ID-agent-*.json`. Delete also stages `sessions/PID.json` entries naming a
  lineage conversation whose process is gone, and removes the now-empty project
  directory of its own conversation.
- Gemini: every `.gemini/tmp/SLUG/chats/session-*.jsonl` whose header names the id.
- Antigravity: `conversations/ID.db` (plus `-wal`/`-shm`), `brain/ID/`,
  `annotations/ID.pbtxt`, the empty `presence/ID.lock`, the row of
  `conversation_summaries.db` and the entry in `jetbox_summaries_proto.pb` (the set
  agy's own `/resume` delete removes). The index is edited only through SQLite, one
  `BEGIN IMMEDIATE` transaction per row, after a `VACUUM INTO` backup kept in the
  archive bundle or delete staging; Archive saves the whole row for Restore, Delete
  also drops the cache entry. Not touched: `history.jsonl`, `implicit/*.pb` and the
  OAuth token.

Archive writes `DATA_ROOT/sessions/archived/SESSION-ID/native.json` (original path and
SHA-256 tree digest of each item) first, then moves the files into
`native/OPERATION-ID/` (rename, or copy, verify and remove across file systems;
uncompressed); the provider then neither lists nor resumes the conversation. Restore
moves the identical files back to the same paths of the same home. Delete stages the
files under `DATA_ROOT/operations/deletions/native-OPERATION-ID/` with a `plan.json`,
removes the catalog entry and aliases (in `catalog.json` and `identity.json`), then
the staging directory; an interrupted delete leaves both for review. A session with no
history file and no other files archives and restores cleanly with
`nothing_to_archive:true` (such a `stale` row has `capabilities.archive:true`). For
Claude the transcript decides: a session stopped before its first turn archives with
`nothing_to_archive:true` and its side files (`session-env`) move with it; for other
providers files without the transcript still fail `HISTORY_NOT_FOUND`. Credentials are never read.

Row reasons (`capability_reasons.archive|delete`) and refusals: `SESSION_RUNNING` (a
verified registry entry or process, interactive or headless, holds a lineage
conversation), `SESSION_HOSTED`, `SESSION_STATE_UNKNOWN` (the live probe failed),
`ACCOUNT_DELETED`, `HISTORY_UNAVAILABLE` (archive only), `SESSION_ALREADY_ARCHIVED`,
`SESSION_NOT_ARCHIVED` (`restore` on a live row), `SESSION_MERGED` (act on the
survivor); the worker also refuses `HISTORY_IN_USE` (open writer), `HISTORY_SYMLINK`,
`HISTORY_CHANGED`, `RESTORE_CONFLICT` (an original path exists again),
`ACCOUNT_HOME_CHANGED` and `ACCOUNT_DELETED`. Fixtures: `archived_session_claude`,
`native_conversation_archive`.

Repeats (since 1.70.1, every provider): `api archive` of an already archived session returns
ok with `already_archived:true`, and `api restore-session` of an active one `already_active:true`,
both `phase:"complete"` with the usual job keys (`kind` `archive_session` or `restore_session`, no
job is journaled). The generation is not checked on a repeat, as for `stop` of an archived
session; the row reasons `SESSION_ALREADY_ARCHIVED` and `SESSION_NOT_ARCHIVED` are unchanged.

## Accounts

Command usage is in the [CLI guide](cli.md#accounts). Every account belongs to one
provider; IDs are `<provider>:<name>`. The machine API takes qualified IDs only (a bare
name is `ACCOUNT_PROVIDER_REQUIRED`; a provider that differs from the enrolled one is
`ACCOUNT_PROVIDER_MISMATCH`). Human commands also accept an unambiguous bare name or
display name (several providers: `ACCOUNT_AMBIGUOUS`).

Account rows carry `provider`, `provider_capabilities`, `membership`, `selectable`,
`order`, `display_name`, `folder_name`, `name_mismatch`, `running_sessions`,
`generation`, `backend_rename`, `home`, `home_deferred`, `config_home` (adopted Claude
homes), `auth_required`, `auth_detail` (`AUTH_REQUIRED`, `WRONG_AUTH_MODE`,
`AUTH_FAILED`; never identity), `usage_state`
(`fresh|partial|stale|missing|no_windows|error|unsupported`), `usage`, `plan`,
`prefill` and provider fields (`bypass_warning*`, `auth_mode`, `api_key_present`,
`auth_failed*`). Disabled accounts remain listed; disabling prevents future selection
and does not stop agents.

### Names

Account names are **unique per provider** (`codex:work` and `claude:work` coexist;
`capabilities.account_names`, `unique_across_providers:false`). New names allow 1-30
ASCII letters, digits, hyphens or underscores (`new_account_name_max:30`, also the
folder name); existing longer names keep working. A taken name is
`ACCOUNT_NAME_EXISTS` naming the same-provider owner. Under a deep data root a Codex
control socket (`<root>/accounts/codex/<folder>/app-server-control/app-server-control.sock`)
can exceed the 107-byte `sun_path` limit for folder names longer than about 17
characters. Session names allow at most 64 Unicode scalar values
(`session_name_max:64`) without controls or surrounding whitespace; automatic labels
get collision-safe suffixes.

**Display names** (`api account rename PROVIDER:NAME --display-name X|--clear
[--expected-generation G]`) return `{account_id,name,display_name,changed}` and change
only `display_name` (`null` when it equals the name) for every provider, also while
sessions run. They use the account-name characters, at most 64
(`account_display_name_max`), and are unique among the names and display names of
the provider's accounts (`ACCOUNT_DISPLAY_NAME_EXISTS`; a new account name equal to a
display name is `ACCOUNT_NAME_EXISTS`). Errors also `INVALID_DISPLAY_NAME`,
`ACCOUNT_DELETION_IN_PROGRESS`. Event `account.renamed`.

**Backend rename** (`api account rename-backend ID [NEW_NAME] --expected-generation G
[--request-id UUID]`; without `NEW_NAME` the folder and id are renamed to match the
display name) returns `{account_id, previous_account_id, name, folder_name,
display_name, name_mismatch, generation, home, operation, changed}`; clients must
switch to the new id and re-read sessions (their generations change). The home moves
to `accounts/<platform>/<new>` even from a deferred legacy path; the registry, catalog
entries, switching policies, Gemini auth-failure state and usage history move with
it; old receipts, digests and message journals keep the old id. Refusals:
`ACCOUNT_SESSIONS_RUNNING` (any session or provider process runs or launches),
`ACCOUNT_NAME_EXISTS`, `ACCOUNT_NAME_UNCHANGED`, `ACCOUNT_HOME_EXISTS`,
`ACCOUNT_HOME_ADOPTED`, `ACCOUNT_REMOVED`, `ACCOUNT_DELETION_IN_PROGRESS`,
`ACCOUNT_POLL_IN_PROGRESS`, `ACCOUNT_RENAME_UNRESOLVED`, `ACCOUNT_HOME_MOVE_FAILED`,
`STALE_TARGET`. It is journaled in `operations/account-renames/` (`kind:"account_rename"`),
never replayed, reconciled from evidence at supervisor start or by `api recover`;
what cannot be decided stays `uncertain` until `api abandon-operation`.
`backend_rename` on the row is `{available, reason, update_backend,
update_backend_target}` with `reason` the code a rename would return now.

**Order.** `order` is 0-based and dense; `account list` and `state` are sorted by it,
and a new account is appended. `api account reorder ID --position N` returns
`{"account_id","provider","position","changed","order":[ids]}` (event
`account.reordered` on change; `N` beyond the last position is `INVALID_POSITION`;
`account_reorder:true`).

### Enrollment, login and log out

`api account add ID` enrolls the account. Codex add/login return the provider's
verification URL and user code. For Claude, Gemini and Antigravity, `api account add`
and `api account login` return an exec plan `{login:{action:"exec",interactive:true,
credentials_in_client:false,argv}}` plus `login_required`; Claude add also returns
`existing_home` (an operator-populated managed home registered unchanged) and `auth`
(`logged_in`, `auth_method`, `api_provider`, `subscription_type`, `problem`). `api
account refresh ID` (Claude, Gemini, Antigravity; Codex
`UNSUPPORTED_PROVIDER_OPERATION`) probes now and returns
`{account_id,provider,auth_required,auth_detail,auth_mode,observed_at}` with
`auth_mode` `{logged_in,auth_method,api_provider,subscription_type}` (plus
`rate_limit_tier` when Claude reports one) or null when the probe failed. The
supervisor re-probes every 3 seconds while a Claude account is `auth_required`.
Provider homes and authentication: [Claude](#claude-code), [Gemini](#gemini-cli),
[Antigravity](#antigravity-cli).

`api account managed-home claude:NAME` switches a Claude account adopted with
`--config-dir` to `<root>/accounts/claude/NAME` (which the operator copied there
first: 0700, owned by the user, not a symlink; nothing is created, copied or moved)
and returns `{account_id,provider,home,previous_home,relocated,already_managed}`
(`note:"ALREADY_MANAGED"` when idempotent). Errors:
`UNSUPPORTED_PROVIDER_OPERATION`, `ACCOUNT_HOME_UNAVAILABLE`, `UNSAFE_ACCOUNT_HOME`,
`ACCOUNT_IN_USE` (a verified live session or any readable process with
`CLAUDE_CONFIG_DIR` set to either home), `AUTH_REQUIRED`, `WRONG_AUTH_MODE`,
`AUTH_IDENTITY_MISMATCH` (a hash of the `auth status` identity differs; identity
values are never printed). Event `account.claude_home_relocated`.

`api account restart-server ID` (contract 1.72.3, capability
`operations.account_restart_server`, Codex) restarts the account's shared app-server with
`codex app-server daemon restart` (the same supervisor method relocation uses, retried while an
account poll runs; the 1.72.2 temp variables apply) and returns `{account_id, restarted:true, pid,
process_start}` of the new server, read from its control socket's peer credentials. Refusals,
nothing restarted: `SESSION_OPERATION_IN_PROGRESS` while the server has loaded conversations,
`ACCOUNT_BUSY` while another Codex terminal uses it or a direct launcher holds the account lease,
`ACCOUNT_POLL_IN_PROGRESS` if a poll outlasts the retries, and for a Claude, Gemini or Antigravity
account (no shared server) `UNSUPPORTED_PROVIDER_OPERATION` with `supported:false`.

`api account logout ID` signs out with the provider's own mechanism and leaves the
account enrolled with `auth_required:true`, `selectable:false`:

```json
{"account_id":"claude:work","provider":"claude","logged_out":true,"auth_required":true,"method":"claude auth logout"}
```

Codex uses the account app-server's `account/logout` (or `codex logout` with the
account's `CODEX_HOME` when none runs); Claude `claude auth logout`, confirmed with
`claude auth status`; Gemini applies `clearCachedCredentialFile`'s two effects
(removes `.gemini/oauth_creds.json`, moves the active `google_accounts.json` entry to
`old`), and in `api_key` mode deletes the key; Antigravity is unsupported
(`account_logout:false`; use `/logout` inside agy). Errors: `ACCOUNT_IN_USE` (live
session, provider process or held Codex lease), `ACCOUNT_NOT_FOUND` (unknown or
deleted), `ACCOUNT_REMOVED`, `ACCOUNT_PROVIDER_MISMATCH`,
`ACCOUNT_DELETION_IN_PROGRESS`, `LOGOUT_INCOMPLETE`. Idempotent; event
`account.logged_out` when the stored state changed.

### Account removal and deletion

`api account remove ID` previews affected catalog sessions, active/loaded
conversations, pending operations and stored-data counts/paths (four bounded readers,
deterministic order; Claude adds `active_processes`, hosted sessions and
`stored_data.adopted`). `--confirm ID --revision PREVIEW-REVISION` removes the local
membership; `api account restore ID` reverses it. Removal refuses active/loaded
conversations or pending lifecycle jobs, never disables the original enrollment,
stops agents or deletes credentials/history; a removed account refuses launch,
resume and headless delivery with `ACCOUNT_REMOVED`.

`api account delete ID` previews intent without scanning account files;
`--confirm ID --request-id UUID` admits a durable background operation (the request
id binds one account permanently; reuse for another is `REQUEST_ID_CONFLICT`; repeat
it only to inspect). The worker checks active/loaded sessions and pending work,
reserves the account through its sole owner, preserves saved/archived history in
`sessions/preserved/REQUEST-ID/home`, then deletes the remaining account files and
credentials; active processes block it, and it never stops agents. Preserved
histories stay discoverable with their stable IDs; resume needs an explicit enrolled
destination account. Per provider it preserves: Codex rollouts and native history
metadata (paginated history needs native verification first); Claude `projects/`,
`history.jsonl`, `file-history/` and `todos/` of a managed home (an adopted
`--config-dir` home is never modified: `home_disposition:"left_in_place"`,
`external_home`); Gemini chat files, `projects.json` and prompt history;
Antigravity `conversations`, `brain`, `annotations`, `implicit`, the index and
`history.jsonl`. With [shared Claude state](#shared-claude-state) only the links are
removed. Previews add `home_disposition` (`deleted|left_in_place`),
`adopted_config_home` and `deletes_local_credentials`. Sessions of a deleted account
report `availability:"stopped"`, `resume_block_reason:"account_deleted"`,
`history_preserved:true` and `ACCOUNT_DELETED` for resume and headless delivery.

`api account deletions [--request-id UUID]` reads receipts; `api recover UUID`
explicitly resumes a failed/interrupted worker after acquiring its operation lock (a
running worker rejects it). Deletion cannot be cancelled once admitted; do not call
`cancel-operation` or `respond` on it. An orphaned worker is marked failed after a
ten-second admission grace. Accounts reserved for deletion expose `deletion` status
and stay unselectable; a partial credential deletion must not be re-enabled. The
owner must advertise `account_deletion`; an older owner returns
`owner_upgrade_required:true` and refuses with `OWNER_UPGRADE_REQUIRED`.

### Limits, plan and usage

`api account limits ID` (capability `account_limits`, Codex and Claude) returns
`{account_id,limits,saved:false}`; `--short-used P --weekly-used P` (both, finite
0-100, maximum **used** percentages) saves through the supervisor and returns
`saved:true`. `limits` is the snapshot's `switch_thresholds`; display markers as
`100 - *_percent` and mark custom limits only when `customized:true`. Concurrent edits
are last-write; after an uncertain response read the limits first. Gemini and
Antigravity are `UNSUPPORTED_PROVIDER_OPERATION`.

`accounts[].plan` (`account_plan`) is the subscription plan the provider last
reported, read-only: `{"provider","raw","name","rate_limit_tier","source","verified_at"}`.
`raw` is the value as reported; `name` is a label only where certain (Claude `pro`;
Codex `free`, `plus`, `pro`, `team`, `business`, `enterprise`), else null (Claude
`max` covers both Max and Ultra). `rate_limit_tier` is Claude's `rateLimitTier` when
reported. `source` is `auth_status` (Claude probe, kept in the registry and refreshed
at most hourly while unchanged) or `usage_snapshot` (Codex `planType`). Gemini and
Antigravity report null.

**Usage meter** (`capabilities.usage_meter`). Every account row and `api usage
[--account ID] [--cached]` (one row per account of every provider; `--cached` never
contacts a provider) carry `usage.meter`:

```json
{"supported":true,"requires_session":false,"source":"oauth","fresh":true,"observed_at":1800000000,
 "age_seconds":12,"windows":{"five_hour":{"used_percent":31.0,"resets_at":1800003600,
 "observed_at":1800000000,"source":"oauth","cycle":"1800003600:1799985600:natural",
 "cycle_started_at":1799985600,"cycle_reason":"natural"},"seven_day":null},"reason":null}
```

- Codex `app_server`: the supervisor's `account/rateLimits/read` of every enabled
  account (up to 20 s, also through a shared app-server, never a second one). The
  supervisor skips an account while a possible destination terminal or a held
  `.switcher-lease` could contend for the login; the meter then says so in `reason`.
  `api usage` (not `--cached`) and `api account limits` ask the supervisor to read when
  the reading is older than 60 s and a read is safe
  (`SESSIONSATION_CODEX_METER_CACHE_SECONDS`).
- Claude: `statusline` (a running session's status-line payload, below) and `oauth`
  (the account's `GET https://api.anthropic.com/api/oauth/usage?cedar_ember=1&skip_spend=1`, `{utilization,
  resets_at}` per window, clamped to 100) share one owner (`usage.lock`) and
  `usage.json`. `api usage` and `api account limits` read a meter older than 60 s, at
  most once per 60 s per account; the supervisor reads idle accounts older than 600 s
  once a minute at most, and those whose reset credits are older
  (`SESSIONSATION_CLAUDE_METER_SECONDS`, 0 off), with the stored
  token only, so a background read never rotates a refresh token (`AUTH_STALE` in
  `<home>/.sessionsation/usage-read.json`).
- Per-window freshness (`usage_meter.window_fields`, 1.67.0): `meter.fresh` and
  `age_seconds` describe the newest reading. Every present window also has its own `fresh`
  and `age_seconds` from its own `observed_at`, against the same bound, and a window past
  its own reset is not fresh (`window_reasons`); `meter.all_windows_fresh` is true when
  every present window is fresh. Claude's `seven_day` (OAuth) can lag the statusline
  `five_hour` by minutes. A null window stays null.
- Gemini and Antigravity: `supported:false` with `reason`.
- Reset cycles (`usage_meter.cycles`): every window here and in `usage.short` /
  `usage.weekly` has `cycle` (opaque id), `cycle_started_at` and `cycle_reason`, which
  change on any reset: `natural` (a `resets_at` more than 300 s later,
  `same_cycle_seconds`; started at `resets_at` minus the window length), `redeem` (this
  account's `reset-credit` answered `reset` for a credit that clears the window),
  `external_redeem` (a grant's `resets_left`, or Codex `available` net of expired
  credits, fell between two reads) or Codex `provider_lowered` (less usage within one
  `resets_at`). Within one cycle `used_percent` never goes down, whichever source
  reported it: a lower Claude reading is a stale replay and is kept out, also when an
  older release's statusline recorder (still run by a long-lived session) rewrote
  `usage.json` (`usage-cycles.json` keeps the rule). A redeem starts a new cycle even
  with the same `resets_at`, held open on the old value: the next OAuth answer closes
  it even when lower; a statusline reading closes it only when not lower. A reading
  taken (an OAuth read: sent) before the cycle started, or before the recorded one, is
  kept out. After a reset, a statusline reading within 0.5 of the pre-reset value is
  kept out while the cycle is open or below that value (an idle session's replay),
  until an OAuth answer shows usage back at it. A lower OAuth answer in a closed cycle
  is accepted (`cycle_reason` `provider_lowered`) once a second one at least 60 s later
  reports the same or less, so a missed reset heals within two meter reads and one
  stale answer never lowers the meter. A redeem elsewhere is also a grant that had uses
  left and now is gone from `grants` or lost `resets_left`; nothing is inferred without
  a stored status, for a window shorter than that status's age, or for the grant this
  account just claimed (the claim updates the stored status under `usage.lock`; a read
  sent before the claim's answer is not stored). Codex infers a redeem only from a
  listed credit set. A cleared window the answer then
  reports without a time is null. Claude windows a grant clears: `five_hour`;
  `seven_day` for any `seven_day*` entry or a `fable`, `weekly` or `full` grant; both
  for an unknown grant. The Claude `reset-credit` result adds `cycles_started`.
- After a window's own reset (`usage_read.windows_after_own_reset`): until a provider
  read after the reset shows the new cycle, the meter is `fresh:false` with
  `window reset at <resets_at>; awaiting a post-reset read`; a past `resets_at` is never
  fresh. The supervisor reads a Claude account once at its next tick (at most 60 s) after
  each recorded reset passes, with the stored token only; Codex polls once after a reset.
  The read shows the new future `resets_at`, or, for a window that has not started
  (Claude: the answer names it with `resets_at` null and no usage), `used_percent` 0,
  `resets_at` null, `starts_on_first_use:true` (every window has the key),
  `cycle_reason` `natural` (`usage_meter.starts_on_first_use`; Codex windows always carry
  `resetsAt`). `meter.window_reasons` says why each window is null or past its reset.
- `fresh`: some window is present, none is past its reset and the newest reading is
  younger than the provider's limit (Codex `stale_seconds`, Claude 1800 s). A meter that
  is not fresh always has a `reason`. A missing window is null, never 0%.
  `usage.observed_at` is null without a real observation.
- The row's `read` (`api usage`) or `meter_read` (`api account limits`) is
  `{attempted, ok, code|reason, message, attempted_at, retryable}` or null when no
  read was needed; codes `AUTH_REQUIRED`, `AUTH_STALE`, `USAGE_READ_FAILED`. A failed
  read keeps the last value.
- `api usage` rows keep `account`, `state`, `fresh`, `source`, `semantics`,
  `observed_at`, `age_seconds`, `claude_version`, `short`, `weekly` (each with its own
  `observed_at` and `age_seconds`), `exhausted`, and add `account_id`, `provider`,
  `supported`, `requires_session`, `meter` and `read`. `api account usage ID --start
  UNIX --end UNIX --step 1800` returns history.
- `state --cached` overlays the supervisor's current Codex usage (a local read, 2 s
  bound); when it does not answer, a frozen value is marked `stale` by its age. A
  stored `observed_at` ahead of the clock never blocks new polls and is clamped
  (`usage.observed_at_clamped_from`).

### Reset credits

`api account reset-credit ID [--credit-id ID] [--kind 5-hour|weekly|fable|full]
[--request-id UUID]` spends a stored rate-limit reset (`capabilities.account_reset_credit`,
`providers:["codex","claude"]`, `idempotent_by:"request_id"`, `auto_retry:false`,
`reversible:false`). **It is not reversible** and is sent only by this command.

- **Result** `{account_id, request_id, outcome, rate_limits_after, attempts,
  operation}` (Claude adds `credit_id` and `claude {result, reason, resets_left,
  cleared, weekly_resets_at, cooldown_until}`). `outcome` (`ok:true`): `reset`,
  `nothingToReset` (nothing spent), `noCredit`, `alreadyRedeemed` (this request id
  already succeeded), and for Claude `cooldown` (nothing spent; try after
  `cooldown_until`). After `reset`/`alreadyRedeemed` usage is read again at once
  (`rate_limits_after`, or `rate_limits_after_error`).
- **Idempotency.** The request id (a lowercase UUID, generated when omitted) is
  journaled in `operations/account-reset-credits/<request_id>.json` (`requesting`)
  **before** sending, then `complete`, `failed` or `uncertain`. Retry with the same
  request id (it can never spend a second credit); a new id is a new attempt. The same
  id for another account or credit is `REQUEST_ID_CONFLICT`. Listed as
  `kind:"account_reset_credit"`, never pruned. The account's operation lock is held
  throughout (`ACCOUNT_BUSY`, nothing sent; also while a direct Codex terminal holds
  the account).
- **Errors.** `RESET_CREDIT_OUTCOME_UNKNOWN` (no answer, 5xx, unreadable or unknown
  result: `uncertain`) and `RESET_CREDIT_FAILED` (an error answer, 4xx/429, or never
  connected: `failed`) both name the request id to retry with. Also
  `UNSUPPORTED_PROVIDER_OPERATION` (other providers, or a Codex without the method,
  naming its version), `INVALID_REQUEST_ID`, `AUTH_REQUIRED`, `AUTH_STALE`
  (retryable), `ACCOUNT_DELETION_IN_PROGRESS`, `ACCOUNT_REMOVED`,
  `RESET_CREDITS_READ_FAILED`, `RESET_CREDITS_UNREADABLE`. Provider error text is
  never echoed.
- **Stored credits** appear as `usage.reset_credits` `{available, observed_at,
  credits}` (or null): Codex from the app-server's `rateLimitResetCredits`
  (`credits` `[{id, status, expires_at}]`, possibly shorter than `available`); Claude
  from the `cedar_ember` block of `GET .../api/oauth/usage?cedar_ember=1&skip_spend=1`
  (`available` sums `resets_left` of grants that are not paused; one row per grant
  `{id, status, expires_at, kind: session|weekly|fable|full, label, resets_left,
  resets_total, paused, usable_now, starts_at, clears, unavailable_reason}`, plus
  `eligible`, `ineligible_reason`, `at_limit`, `next_grant_id`, `cooldown_until`,
  `weekly_resets_at`). `api account limits claude:NAME` reads it when older than 60 s
  (`reset_credits_read`); `account list` and `state` show the stored read
  (`<home>/.sessionsation/reset-credits.json`) and never call the API. Every Claude meter
  read is this same request, so the supervisor refreshes it with the meter (also while a
  session keeps the meter fresh). Both providers add `fresh` and `age_seconds` (the
  meter's staleness limit).
- **Codex** sends `account/rateLimitResetCredit/consume` `{creditId, idempotencyKey}`
  through the supervisor's worker or the shared app-server (Codex 0.165.0 and later).
- **Claude** posts `{program:"cedar_ember", grant_id, request_id}` to
  `https://api.anthropic.com/api/organizations/<organizationUuid>/reset_rate_limits`.
  The grant is `--credit-id`, else a usable grant of `--kind`, else `next_grant_id`,
  else the first usable grant (none: `noCredit`, `sent:false`, nothing journaled). The
  access token comes from `CLAUDE_CONFIG_DIR/.credentials.json` (`claudeAiOauth`) and
  is only on curl's stdin (`curl -q -K -`, config `curl_binary`), with Claude Code's
  headers. Renewal matches Claude Code so the two never race a refresh-token rotation:
  only when the token expires within 300 s or after a 401 (then sent once more), under
  Claude Code's `.oauth_refresh.lock` (and the legacy `<realpath>.lock`), re-reading
  the file under the lock, posting to `https://platform.claude.com/v1/oauth/token` and
  writing back atomically (0600) under `.storage-write.lock` with compare-and-swap.

## Providers

`capabilities.providers[]` reports `codex`, `claude`, `gemini` and `antigravity`
(`implemented:true`, `operations` with `{supported, reason}`, and flat booleans and
fields such as `adopt_config_dir`, `account_switch` (with `operations.account_switch`),
`account_removal`, `account_deletion`, `account_rename`, `account_refresh`,
`account_limits`, `account_logout`, `account_restart_server`, `usage`, `usage_state` and `usage_source`, for
example `codex-app-server-rate-limits`, `claude-code-statusline`,
`gemini-cli-unsupported`, `antigravity-cli-unsupported`). The
`operations` table is the source of truth; this section lists provider-specific
behaviour. There is no cross-provider conversation conversion. Every provider child
is built by one environment function per provider that sets the account home, removes
ambient credentials and backend overrides so billing cannot switch silently, removes
`XDG_RUNTIME_DIR` (Claude, Antigravity) and sets `SESSIONSATION_SESSION_ID`.

### Codex

The full lifecycle: usage, stop, restart, account switch, relocation
(`change_directory`, `move_project`), Space moves, checkpoints, steering,
Resume-and-submit and compaction through the account app-server. Verified native
profiles are listed in `native_protocol.supported_versions` and
`steering.compaction_verified_profiles` ([operations](operations.md#native-messaging-compatibility));
any other `initialize.userAgent` version fails closed with `unsupported_native_version`.
Sessionsation writes `{"updater":{"autoUpdateEnabled":false}}` into each account's
`app-server-daemon/settings.json` (other keys kept, 0600) at supervisor start and
before every `account prepare`/`restart-server`, so the managed daemon is not moved to
an unverified release ([release pinning](operations.md#codex-daemon-release-pinning)).

`api relocate --session ID --expected-generation G --to /absolute/destination
[--move-files] [--dry-run]` changes a live managed session's directory (`--move-files`
also moves the project folder; otherwise the destination must exist). `--dry-run`
returns source, destination, affected sessions and confirmation detail without
changes. Admission rechecks the generation; lifecycle reservations, idle checks,
background-work consent and recovery apply; account, conversation and session
identity stay and a live agent is restarted in the new directory. Folder moves reject
existing destinations, symlinks and protected roots, wait for other sessions using
the source, and keep the original backup across file systems. For inactive sessions
choose the directory in the Resume plan.

### Claude Code

Each account is an isolated `CLAUDE_CONFIG_DIR`: the managed home
`DATA_ROOT/accounts/claude/NAME` (0700), or with `--config-dir DIR` an adopted directory
used **in place** (absolute, existing, outside the data root, not overlapping another
account's home; never copied or moved). An operator-placed managed home is registered
as is if it is a 0700 directory owned by the user and not a symlink (otherwise
`UNSAFE_ACCOUNT_HOME`); it is never wiped or re-permissioned. Config keys:
`claude_binary` (default `claude`), `claude_session_home` (optional absolute `HOME` for
Claude children), `claude_statusline_refresh_seconds`, `claude_pretrust_folders`,
`claude_default_statusline_command`. Every Claude child gets `CLAUDE_CONFIG_DIR`,
`DISABLE_AUTOUPDATER=1`, and has `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
`ANTHROPIC_BASE_URL`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_USE_BEDROCK|VERTEX|FOUNDRY`,
`ANTHROPIC_BEDROCK_BASE_URL`, `ANTHROPIC_VERTEX_PROJECT_ID` and `XDG_RUNTIME_DIR`
removed. Before any launch `claude auth status --json` (bounded) must report a
`claude.ai` login on `firstParty`, else `AUTH_REQUIRED` or `WRONG_AUTH_MODE`; email and
organization fields are never stored or printed. Login runs `claude auth login
--claudeai` through the same environment; the human `account add` starts it when a
fresh managed home is not logged in and stdin is a terminal (`--no-login` skips it).

A new session generates its native id, records the catalog entry (provider, account,
`conversation_id`, directory) and execs `claude --session-id UUID --name LABEL
--settings JSON` (the settings carry the statusline hook and
`crossSessionInbound:"accept"`); resume runs `claude --resume UUID --name LABEL` in the
recorded directory and refuses a verified live session or a missing transcript
(`SESSION_STALE`). Discovery reads only `sessionId`/`cwd`/`timestamp` from transcript
headers in `projects/*/*.jsonl`. A session is `running` only when a `sessions/PID.json`
registry entry names it, the PID exists with the recorded `procStart` and, when
readable, the process's config dir is the account home; `native_status` and
`native_name` come from that entry. Otherwise `stopped`, `stale` (transcript missing)
or `unknown` (probe failed). Supported: new, resume, rename (HERDR tab), stop, account
switch, archive/restore/delete, compaction, set-model, Add to HERDR for background
sessions, live delivery ([input delivery](#claude-input-delivery)), headless delivery,
pending questions (hosted answers), events and hooks. Unsupported: restart (stop,
then resume), relocation and Space moves.

#### Claude usage and statusline

Launches pass a statusline that runs the hidden `sessionsation provider-hook
claude-statusline`, which atomically writes `CLAUDE_CONFIG_DIR/.sessionsation/usage.json`
(0600; rate-limit windows only) and the per-conversation model and context records,
then runs the account's own `settings.json` `statusLine.command` with the same input
and passes its output through; recorder errors never break the statusline.
`refreshInterval` (`claude_statusline_refresh_seconds`, default 300, `0` omits it)
makes Claude Code re-run it while idle. Rate limits appear only for subscription
accounts after the first API response. Recording merges per window (`five_hour`,
`seven_day`): the same `resets_at` with a lower value keeps the stored window (an idle
session replays its last response); a window missing from a payload is carried until
its own `resets_at`; `usage.json` is rewritten only when a window changed, so an
account with only replaying sessions turns `stale`. Readings older than 30 minutes or
past their reset are stale. `.tmp*` files older than 10 minutes are swept from
`.sessionsation/`. Claude usage keeps the shared `usage.short`/`usage.weekly` shape
(`duration_minutes` 300/10080). No model request is made to obtain usage; the OAuth
read is described under [usage meter](#limits-plan-and-usage).

### Shared Claude state

Capability `shared_claude_state`. Claude's non-credential per-conversation state
lives in one store, `<root>/shared/claude/<name>`, for `name` in `projects`
(transcripts and per-project `memory/`), `file-history`, `todos`, `session-env`,
`agents` (custom subagents) and `commands`. Every managed Claude account home has a
directory symlink `<account home>/<name>` whose stored target is exactly that
absolute path (as the root is configured). Credentials, `settings.json`,
`sessions/`, `skills/` (it holds `skills/synced/<org-bucket-id>`, claude.ai
cloud-synced skills of one account) and everything else stay per account. Adopted
(`--config-dir`) homes are never linked (`applicable:false`). Nothing changes on a
root until `shared enable` or the first `shared migrate` writes
`shared/claude/.sessionsation-shared`.

- **Trusted links.** Sessionsation refuses symlinks everywhere (digest, copy and
  remove fail with `HISTORY_SYMLINK`). Only `shared_state::trusted_link` accepts a
  symlink at exactly `<managed Claude home>/<name>` for a listed name, owned by this
  user, whose stored target is exactly `<root>/shared/claude/<name>` and resolves
  (no other symlink on the way) to a real private directory owned by this user
  inside private `shared/` and `shared/claude/`. Anything else is refused as before.
  The helper is consulted only for launch-time repair, the parent check when
  restoring archived files, account deletion and the switch no-op.
- **Ensure-links.** On `account add` and before every Claude launch, resume, hosted
  start, headless delivery, attach and switch, a missing link is created, an empty
  real directory is replaced by a link and a trusted link is kept. Errors:
  `SHARED_STATE_UNMIGRATED` (a real directory with data where a link belongs; run
  `shared migrate`), `SHARED_STATE_LINK_UNTRUSTED`, `SHARED_STATE_STORE_UNSAFE`.
  `account add` reports `shared_state:{state:"linked"|"disabled"|"not_applicable"|"error"}`.
  Once enabled, an unmigrated account cannot be launched or resumed; running
  sessions are not affected.
- **Delete, log out, rename.** Account deletion removes the links only; shared data
  survives the account. Log out touches no history. Rename keeps the links valid.
- **`shared migrate --account claude:NAME [--dry-run] [--allow-full-copy]
  [--fs-path DIR]`** (CLI only; prints JSON) is journaled in
  `operations/shared-migrations/<id>.json`, idempotent (`already_migrated:true`) and
  resumable. (1) Preflight: account home and store on one file system (`st_dev`;
  otherwise `SHARED_STATE_CROSS_DEVICE`), the alias proven, `renameat2(RENAME_EXCHANGE)`
  and (unless `--allow-full-copy`) reflink probed; `--dry-run` stops here and prints
  the plan. (2) Backup: a reflink copy of each directory to
  `<root>/backups/shared-migrate-<UTC time>/<account>/<name>`
  (`BACKUP_REFLINK_UNSUPPORTED` without reflink unless `--allow-full-copy`).
  (3) Link per name with no moment in which the account path is missing and no data
  copied (inodes unchanged, so open files keep working): with no `<name>` in the
  store yet, a placeholder link is exchanged with the account's real directory; with
  one, non-conflicting entries move by `RENAME_NOREPLACE`, shared directories merge
  recursively, and the emptied account directory is exchanged with a link. A file
  present on both sides is a conflict, never overwritten: the name stays a real
  directory, paths are printed under `results.<name>.conflicts`, and the command
  exits with `SHARED_STATE_MIGRATION_INCOMPLETE` until resolved. For `agents` and
  `commands`, identical files are kept once and of two different files the newer
  stays in the store and the other is set aside in `<backup_dir>/set-aside/`
  (`--dry-run` prints one `report` line per such file).
- **`--fs-path`.** On a FUSE union such as the Unraid user share (no
  `RENAME_EXCHANGE`, no reflink) `--data-dir` stays the logical root, so link targets
  keep that spelling, while `--fs-path /mnt/disk1/...` runs all file operations on
  the real disk. The alias is proven with a probe file.
- **`shared status`** lists the store state and each Claude account's per-name state
  (`linked`, `missing`, `empty_directory`, `unmigrated`, `untrusted`);
  **`shared enable`** turns the feature on without migrating.

### Claude input delivery

`capabilities.claude_input_delivery` (`version:1`; the provider entry repeats it as
`input_delivery`) describes three live paths plus [headless delivery](#headless-delivery)
for stopped sessions. Session rows carry `capabilities.submit|queue|steer`,
`capability_reasons` (`SESSION_NOT_RUNNING`, `LIVE_STEERING_DISABLED`,
`PEER_ENDPOINT_UNAVAILABLE`, `PEER_SOCKET_UNLINKED`, `TERMINAL_NOT_ATTACHED`,
`TERMINAL_QUEUE_UNSUPPORTED`, `SESSION_OPERATION_IN_PROGRESS`) and `input_delivery`
(`semantics`, `interrupt`, `label`, `steer`, `modes` `{submit,queue,steer}` → path or
null, `available` in preference order, `terminal_input:{available,reason}`). A hosted
session reports `availability:"running"`, `resume:false`, `stop`/`restart` reason
`HOSTED_SESSION` (use `api hosted stop`) and `add_to_herdr` reason `HOSTED_SESSION`.

Request (`ClaudeSteerRequest`): the ordinary `api send` fields with
`mode:"submit"|"queue"|"steer"`, optional `timeout_seconds` (reply wait, default 600,
max 3600), `delivery` (`claude_input_delivery.delivery_request`: field `delivery`,
values `auto` (default), `hosted_stream_json`, `terminal_input`, `peer_message`; `native`
is also accepted), `origin` (`operator`|`agent`), `await_reply` (boolean,
default false) and, for steer, `"steering":{"version":1,"interrupt":bool}`.
Priorities: submit `next`, queue `later`, steer `next`. There is no idle preflight:
the provider orders inputs. Claude Code folds a `next` input into a running turn at
its next tool round (or runs it as the next turn) and never cuts the turn for it;
`later` runs as its own later turn; `now` would **cancel** the running turn, so
Sessionsation never sends it. `input_delivery.steer` (`ClaudeSteerSemantics`) reports
`{priority:"next",interrupts:false,delivery:"next_tool_boundary_or_next_turn",
interrupt_option:"steering.interrupt",interrupted_inputs_state:"interrupted"}`; only
`steering.interrupt:true` on a hosted session cancels the running turn first.

**Path choice** (`claude_input_delivery.preference`
`["hosted_stream_json","terminal_input","peer_message"]`). A running hosted worker
always wins. Otherwise, with `delivery:"auto"`, a `submit` or `steer` from an `agent`
origin (or `delivery:"native"`) tries the native peer message first, waiting up to
5 s (`SESSIONSATION_NATIVE_WAIT_SECONDS`) for the endpoint, and types into the HERDR pane only when nothing native was
possible: no verified registry entry, `peerProtocol` other than 1, inbound messages
not accepted, `PEER_SOCKET_UNLINKED` or `PEER_SOCKET_UNREACHABLE` (receipt
`native_fallback:{from:"peer_message",to:"terminal_input",reason,stage}`, `stage`
`admission` or `delivery`). An `operator` send prefers terminal input. `queue` and
interrupts cannot be typed, so they use the peer path only. Once a byte was written
the message is never typed or resent. A named path that does not apply is refused
(`DELIVERY_UNAVAILABLE`, `TERMINAL_NOT_ATTACHED`, `LIVE_STEERING_DISABLED`, ...),
never replaced; when neither path is usable, the `LIVE_STEERING_DISABLED` message
also names why terminal input is ineligible.

**Origin.** Without `origin`, Sessionsation walks the caller's process tree
(`/proc/<pid>/stat` parent chain, at most 64 hops) and treats the send as `agent` when
an ancestor is the verified process of a live catalog attachment. For Codex callers,
whose tool commands run under the daemon-owned app-server, it then maps the caller's
or an ancestor's `CODEX_THREAD_ID` (only that variable is read from
`/proc/<pid>/environ`) to the Codex catalog session. Otherwise it is `operator`. An
explicit `origin` always wins. Receipts (Claude and Codex) carry `origin` and
`origin_source` (`explicit`, `detected`, `default`). Only the catalog is consulted; an
unreadable catalog means `default`.

Admission refuses without journaling on `STALE_TARGET`, `SESSION_NOT_RUNNING` (use
mode `resume`), `LIVE_STEERING_DISABLED`, `PEER_ENDPOINT_UNAVAILABLE`,
`PEER_SOCKET_UNLINKED`, `PEER_INTERRUPT_UNSUPPORTED`, `SESSION_OPERATION_IN_PROGRESS`,
account/auth errors, `INVALID_EXPIRY` and `INVALID_REQUEST` (an unknown `origin`).
Every path journals under `DATA_ROOT/messaging/claude-steer/` before writing.

Receipt (`ClaudeSteerReceipt`, also from `api receipt`): `delivery_semantics`,
`priority`, `origin`, `origin_source`, `native_fallback`, `state`, `reason`, `facts`
(`written`, `accepted_native`, `input_recorded`, `input_consumed`, `reply_observed`,
`reply_final`, `folded`, `shared_with_turn`, `interrupt_requested`, `lifecycle[]`,
`held`, `peer_status[]`, `idle_notice`), `correlation`, `reply`
(`{text,text_truncated,final,shared_with_turn}` plus the hosted `result` fields
`is_error,subtype,num_turns,duration_ms,usage`), `execution` (`phase`
admitted/dispatching/written/complete, `host_epoch`, `automatic_replay:false`) and
`observation`. States: `pending_local`, `held`, `native_accepted`,
`execution_started`, `completed_with_reply`, `completed_without_reply`, `failed`,
`interrupted`, `cancelled`, `refused`, `rejected`, `expired`, `delivery_unknown`.
Terminal states are never regressed; only `completed_*` carries `reply.final:true`.
`api cancel` cancels only a request still `admitted`, otherwise
`STEERING_CANCELLATION_UNSUPPORTED` (interrupt a hosted turn with `api hosted stop
--interrupt` or a steer with `interrupt:true`). Fixtures: `claude_steer_request`,
`claude_steer_receipt_*`, `claude_steer_semantics`, `hosted_session`, `live_steering`,
`launch_request_hosted`.

#### Hosted sessions

A hosted session is run by a detached Sessionsation worker instead of a terminal: it
owns the stdin of `claude -p --input-format stream-json --output-format stream-json
--verbose --replay-user-messages --permission-prompt-tool stdio` with `--session-id
UUID` (new) or `--resume UUID` (a stopped conversation), appends a normalized
`events.jsonl` for the viewer and serves a private control socket. `api hosted start`
(`--account claude:N --directory DIR [--name]` or `--session ID
--expected-generation G`, `--permission-mode account|default|acceptEdits|plan|bypassPermissions`),
`status`, `events`, `stop [--interrupt] [--timeout-seconds N]` and `respond
--request-id R --allow|--deny [--message TEXT]`; `api launch start` accepts
`"hosted":{"permission_mode"}`. `account` passes no flag (an account default of
`bypassPermissions` becomes `default`); without a `permission_mode` the
[launch permissions](#launch-permissions) decide (`yolo` → `bypassPermissions`). Tool
permission prompts arrive as `can_use_tool` requests and stay pending until answered
with `api hosted respond`; nothing is auto-approved.

`HostedSession` fields: `state` (`starting|running|stopping|stopped|failed`), `reason`
(`STOPPED`, `WORKER_LOST`, `WORKER_LOST_PROVIDER_DRAINING`, `PROVIDER_EXITED[_CODE]`,
…), `permission_mode`, `effective_permission_mode`, `epoch`, `busy`, `worker`,
`provider_process`, `pending_permissions[]` (`request_id`, `tool_name`, `input`,
`input_preview`, `requested_at`), `viewer_argv` and `take_over_argv`. The worker lock
(`DATA_ROOT/sessions/hosted/UUID/worker.lock`) makes it the only writer: a worker, or
a provider it started that still drains, blocks interactive/headless resume and a
second host (`SESSION_ALREADY_RUNNING`). The worker survives supervisor restarts and
is re-adopted by the next read; a lost worker is `stopped` (`WORKER_LOST`) and its
in-flight deliveries settle as below. Closing the viewer does not stop the worker;
`take-over` stops it cleanly, then execs the interactive `claude --resume UUID` in
the same terminal.

Hosted delivery writes one stream-json user frame with `uuid` = `message_id`. The
worker marks the record `dispatching` before writing; a request it never claimed is
`rejected` (`HOST_UNREACHABLE_NOT_SENT`, `HOST_STOPPED_BEFORE_DELIVERY`).
`command_lifecycle` `queued` → `native_accepted`, `started` → `execution_started`;
`cancelled`/`discarded` before `started` → `cancelled`
(`NATIVE_CANCELLED`/`NATIVE_DISCARDED`), `refused` → `refused`. The terminal state needs
both the `result` whose `user_message_uuids` contains the id and the id's terminal
lifecycle, in either order: `completed` → `completed_with_reply` /
`completed_without_reply`, or `failed` (`PROVIDER_REPORTED_ERROR`). The reply is the
assistant text streamed between `started` and that result. `cancelled`/`discarded`
after `started` means the consuming turn was cut: the input ends `interrupted`
(`TURN_INTERRUPTED` after a requested interrupt, else `TURN_CANCELLED_BY_PROVIDER`)
with the partial reply and `final:false`. A steer folded into a running turn has
`facts.folded:true` and `shared_with_turn:true`. Another reported `session_id` is
`delivery_unknown` (`SESSION_ID_MISMATCH`). A provider or worker exit makes
in-flight messages `failed` (`*_DURING_TURN`) or `delivery_unknown`
(`PROVIDER_EXITED…`, `HOST_WORKER_EXITED`, `STDIN_WRITE_FAILED`). `correlation` is
`{kind:"command_uuid",command_uuid,lifecycle,user_message_uuids}`.

#### Native peer messages

The peer path (`peer_message`) delivers through Claude Code's cross-session
messaging; the receiver sees a message from another session (Agent-DM labels it a
weak steer). A session accepts inbound messages when the account sets
`crossSessionInbound: accept` (`api account live-steering claude:NAME
[--enable|--disable]`, returning `LiveSteering` `{account_id,provider,live_steering,
setting:"crossSessionInbound",value,previous,changed,settings_file}`, events
`account.live_steering_enabled|disabled` on change; an operator's own `hold`/`refuse`
is never removed) or the process was started with `--settings` saying so.
Sessionsation launches and resumes Claude with `crossSessionInbound:"accept"` merged
into its `--settings` JSON unless the account's own value is `hold` or `refuse`.

Wire protocol (`provider/claude/native_peer.rs`, with the version pins): frame
`{"type":"user","message":{"role":"user","content":TEXT},"priority":"next"|"later","from":"uds:<our socket>","msgV":1,"msg_id":ID,...}`,
newline terminated; control frames are `{"type":"control","action":...}`. The reply
socket is bound in the receiver's socket directory as `<our pid>-<8 hex>.sock` (the
receiver checks it against SO_PEERCRED; `peer_message.reply_socket` says `namespaced`
or `private`). The `*.key` token files are never read. Frames over 1 MiB are refused
(`PEER_MESSAGE_TOO_LARGE`). The receiver's socket path comes from its registry entry
(`messagingSocketPath`); Claude children run without `XDG_RUNTIME_DIR`, so sockets
live under `/tmp/cc-socks/<pid>.sock` where elogind cannot unlink them. A registry
naming a socket file that is gone is `PEER_SOCKET_UNLINKED` (the text adds whether
the process still listens); resume or restart the session to rebind.

`message.content` is always the envelope Claude Code renders as the collapsed
`@<name> ❯` row:

```
<cross-session-message from="<from>" from-name="<name>" from-mode="bypass|prompting">\n<body>\n</cross-session-message>
```

`from` is the frame's reply address, percent-encoded except `[A-Za-z0-9:_/.-]`.
`from-name` is the sender's session name, resolved when the message is admitted, or
the [component name](#component-sender-names) for an outside-session sender, with
`"`, `<`, `>`, controls and newlines removed, capped at 64 characters, `sessionsation`
when empty. `from-mode` is the SENDER's permission class: a Claude sender launched with
`--dangerously-skip-permissions` or `--permission-mode bypassPermissions`, or a Codex
sender whose latest `turn_context` records approval `never` with sandbox
`danger-full-access`, is `bypass`; everything else is `prompting`. A mismatch with the
receiver makes Claude hold the message for approval; that is intended. Inside the
body every `<cross-session-message` and `</cross-session-message` gets a zero-width
space after the `<`. The config key `native_sender_registration` is a no-op: no sender
registry entry is written, and receipts written while it worked may still carry
`sender_registration` with `SENDER_NAME_COLLIDES_WITH_LIVE_SESSION`,
`SENDER_REGISTRY_UNAVAILABLE`, `SENDER_PID_FILE_FOREIGN` or `SENDER_NAME_INVALID`. The
supervisor still sweeps such entries (both ownership markers, dead pid) every 60 s.

Status frames: `delivered` → `native_accepted`, `held` → `held` (a bypass-permission
receiver), `denied`/`refused`/`dropped` → `refused` (`PEER_MESSAGE_DENIED|REFUSED|DROPPED`),
`expired` → `expired`. With no status inside 3 seconds the state is `native_accepted`
with `peer_message.evidence:"transport_accepted_silent"` (`facts.silent_accept`):
the transport took the frame, which is not proof that Claude read it. The transcript
anchor (the record carrying the message id) upgrades any of these to
`input_recorded` and the reply (`evidence:"transcript_anchor"`): the assistant text
after it until the next input, final after that input or an idle notice. Without the
anchor, `reply` stays null (`observation.error_code:"peer_anchor_not_found"`). A
connection that never opened is `rejected` (`PEER_SOCKET_UNREACHABLE`); a failed
write is `delivery_unknown`. `correlation` is `{kind:"transcript_anchor",message_id,
anchor_found,anchor_offset,turn_ended,notify_msg_id}`. Only `await_reply:true`
subscribes to Claude's idle notice (which Claude shows the receiving user) so the
receipt can reach `completed_with_reply`; otherwise the worker exits after the
3-second status window. Errors also include `PEER_REPLY_SOCKET_UNAVAILABLE`.

`api idle-notify --session ID --expected-generation G [--notify-session ID2]` (a
running Claude session that accepts inbound messages) subscribes with
`notify_when_idle` for one `peer_idle_notice`, without a model request in the watched
session. Journaled in `operations/claude-idle-notify/`; a detached worker keeps the
subscription for 12 hours. The notice becomes a `kind:"idle"` or `"exited"` event in
`api turn-events` with `detail:{state,finished_at,status}` and, with
`--notify-session`, one queued message. A repeat call returns the active
subscription; a lost worker shows `stalled:true`; nothing is replayed.

Parked sessions: moving an interactive session to the background with Claude Code's
left-arrow gesture records `parkedJobId` in the interactive process's registry entry
and forks the conversation into that `bg` job. The interactive process remains the
job's frontend, so it stays the entry's terminal: `terminal_input` uses it, peer
messages go to the job's own socket, and a generated job title never replaces the
session name. When Claude clears `parkedJobId` the process is no longer a terminal of
the job.

### Terminal input

`delivery_semantics:"terminal_input"` types the text into the session's HERDR pane as
the user's own keystrokes, so the provider records a real human turn. It is the
strongest steer for a session Sessionsation does not host. Claude Code records a
`user` record when idle, or a `queued_command` attachment with `origin.kind:"human"`
mid-turn, read at the next tool round. Capabilities: `claude_input_delivery.terminal_input`
(`ClaudeTerminalInputDelivery`, with `max_text_bytes`), `gemini_input_delivery`
(`GeminiTerminalInputDelivery`) and `antigravity_input_delivery` (with
`herdr_agent:"agy"`, `keys_by_queued_messages`, `max_text_bytes`, `max_lines`).

**Eligibility.** A running, non-hosted catalog session whose `attachment` is a
verified HERDR pane (`attachment.host:"herdr"` with the exact PID and start time of
the provider process). A Claude background session (`native.kind:"bg"`) qualifies
through its `claude attach <job>` client: the attachment must be that exact client
(PID, start time and `attach <native.job_id>` argv) and, at admission and before the
first key, HERDR must report it as the pane's only foreground process. Otherwise
`TERMINAL_NOT_ATTACHED`. The config switch `terminal_input_enabled` (default `true`)
turns it off for every provider: rows report `TERMINAL_INPUT_DISABLED`, requests
naming `terminal_input` are refused with it, and an admitted request still unsent
when it goes off ends `rejected`. Claude types only `submit` and `steer`
(`TERMINAL_QUEUE_UNSUPPORTED`); `steering.interrupt` is refused
(`TERMINAL_INTERRUPT_UNSUPPORTED`): typed input never sends Esc or Ctrl+C.

**Text.** The text must be typeable verbatim, or admission refuses it without
journaling: tabs and control characters, a trailing backslash (Enter would become a
newline) and a last token starting with `@` or `/` (Enter would accept a completion)
are `TERMINAL_PAYLOAD_UNSUPPORTED`; the limit after trimming is
`TERMINAL_TEXT_TOO_LONG` (Claude 8192 bytes; Gemini 1000 bytes and 8 lines;
Antigravity 8192 bytes and 200 lines). CRLF becomes LF and surrounding whitespace is
trimmed (`text_normalized`). A leading `/`, `!`, `#` or `?` gets a backslash
(`escaped_prefix`; Gemini escapes `/`, `!`, `?`; Antigravity refuses text starting
with `/`, `?` or `!`). The receipt correlates on the delivered text.

**Procedure** (journaled under `DATA_ROOT/messaging/claude-steer/` before every write,
one request per pane at a time; each code ends the request `rejected` before anything
is submitted):

1. `herdr agent list` must show the provider's agent on the pane
   (`TERMINAL_AGENT_UNKNOWN`) and not `blocked` (`TERMINAL_AGENT_BLOCKED`), checked
   again before the first destructive key and before Enter.
2. `pane read --format ansi` must show the main prompt with no overlay. Claude: the
   transcript view is left with ctrl+o and the shortcut help with `?` on the empty
   prompt (`view_switched`); the input box is the last `❯` in column 0 directly under a
   rule (a plain rule, or a labelled top rule starting with 3 or more dashes and a
   space), closed by the next rule; the pointer colour is not a marker. A viewed agent
   (its name as a pill in the top rule, a `Message @name…` placeholder, or `◯ main` in
   the agent list) is `TERMINAL_VIEW_NOT_MAIN`: no key returns safely, because Esc
   stops a running subagent. Other refusals: `TERMINAL_VIM_MODE`,
   `TERMINAL_SHELL_MODE`, `TERMINAL_OVERLAY_UNIDENTIFIED` (completion menus, focused
   footer items, pickers, dialogs) and `TERMINAL_VIEW_UNDETERMINED`.
3. The draft in the input box must be reconstructable exactly: paste or image
   placeholders, a row reaching the right edge (possible soft wrap), more than the
   box's rows (`max_draft_rows`; Claude 12) and dim text mixed with typed text are
   `TERMINAL_DRAFT_UNRECONSTRUCTABLE`. A draft starting with `?`, `!`, `#` or `/`, or
   ending in an `@` token, is `TERMINAL_DRAFT_UNRESTORABLE`. Trailing spaces of a draft
   line are not visible and not preserved. The draft is journaled privately and never
   appears in receipts. It is read once more immediately before clearing; if the box
   changed, the request is `TERMINAL_DRAFT_CHANGED` and nothing is sent.
4. The input is cleared and verified empty (`TERMINAL_CLEAR_UNVERIFIED`, draft kept
   in the journal). Claude uses ctrl+k/ctrl+u pairs (up to 600 rounds; ctrl+l only
   redraws); Gemini ctrl+e, ctrl+u, ctrl+k and backspace; Antigravity alt+> then
   ctrl+u/backspace (up to 400 rounds).
5. The text is typed with `pane send-text` (no bracketed paste, so it is never
   collapsed into a paste placeholder) in chunks of 32 characters, ctrl+j between
   lines. The screen is read every 80 non-blank characters; each read must show a tail
   of everything typed so far at least as long as the text typed since the previous
   read, so consecutive reads tile the message even when the box scrolls (whitespace
   and soft wraps are ignored; agy's `> ↑ N more lines` row is skipped). A mismatch or
   a block withdraws the text and restores the draft (`TERMINAL_TYPED_TEXT_MISMATCH`).
   Enter is journaled (`steps` gains `submit`) and sent once (Gemini: at least 30 ms
   after the last key, since a faster Enter becomes a newline). A failed Enter is
   `delivery_unknown` (`TERMINAL_SUBMIT_UNCERTAIN`).
6. Enter empties the input box whether it starts a turn or queues the text. The draft
   is then typed back unsubmitted and verified (`draft.restored`, `restore_error`).
   Every exit after the clear restores the draft or reports what the box holds:
   `draft.left_in_input` (`draft`, `empty`, `message`, `other`, `unknown`) and
   `left_chars`, with the draft kept in the journal. A failed restore is reported,
   never retried. A worker that fails after typing and before Enter withdraws its text.
   A worker that died past the clear leaves an orphan: a receipt read after a
   10-second grace, or the next delivery to the same pane, withdraws the stranded text
   if the box still holds it, types the journaled draft back and ends the request
   `rejected` with `TERMINAL_WORKER_EXITED_BEFORE_SUBMIT`.
7. The transcript after the submit is searched for the human record carrying exactly
   the delivered text with a timestamp at or after the submit. Its uuid is
   `correlation.input_uuid` (`{kind:"transcript_record",input_uuid,record_type,
   anchor_offset,turn_ended}`) and anchors `reply`. Without it within 60 seconds (or
   `timeout_seconds` if shorter; a mid-turn Gemini/agy Enter that was visibly queued
   waits up to `timeout_seconds`) the receipt is `delivery_unknown`
   (`TERMINAL_INPUT_UNCONFIRMED`). Nothing is re-sent.

Other codes: `TERMINAL_HOST_UNAVAILABLE`, `TERMINAL_HOST_FAILED_BEFORE_SUBMIT`. Keys
never sent: Esc, Ctrl+C, Ctrl+D, Ctrl+L (and for agy Ctrl+K and up/down). Receipts add
`terminal_input` (`ClaudeTerminalInput`): `label`, `target`, `escaped_prefix`,
`text_normalized`, `view_switched`, `draft:{present,chars,restored,restore_error,
left_in_input,left_chars}`, `steps`, and on a refused screen `refusal_layout`
(`screen_rows`, `rules`, `box_rows`, `max_draft_rows`, footer rows; never draft text).
A key the user presses during the few hundred milliseconds of a delivery can land
between steps. Screen grammars and key bindings come from the Claude Code 2.1.312,
Gemini CLI 0.66.0 and agy 1.4.10 bundles and HERDR 0.9.5; the Claude markers are
checked against redacted real screens in `tests/fixtures/claude-screens/`, which the
fake HERDR replays. Fixtures: `claude_steer_request_terminal`,
`claude_steer_receipt_terminal*`.

**Gemini.** Gemini CLI 0.66.0 has no usable live input API (no stream-json input; ACP
cancels the running turn on a new prompt), so `submit`, `queue` and `steer` are typed
(`delivery` `auto` or `terminal_input`, otherwise `DELIVERY_UNAVAILABLE`). Enter means a
user turn when idle; mid-turn, Gemini's own queue with `experimental.modelSteering`
off, or a "user steering update" with it on. Hence `queue` needs the setting off
(`GEMINI_QUEUE_WOULD_STEER`), `steer` needs it on (`GEMINI_STEERING_DISABLED`), and a
mid-turn `submit` is refused when it is on (`GEMINI_SUBMIT_WOULD_STEER`). The CLI reads
the setting at startup only: a settings file changed after the process started is
`GEMINI_STEERING_RESTART_REQUIRED`, an unreadable or trust-dependent value
`GEMINI_STEERING_UNKNOWN`. `input_delivery.model_steering` is `{enabled,configured,
source,restart_required,reason}` from the user, workspace and system layers.
`api account live-steering gemini:NAME [--enable|--disable]` (`GeminiLiveSteering`)
sets or removes only `experimental.modelSteering` in `.gemini/settings.json` (atomic,
mode kept, symlinked or commented files refused), with
`applies_to:"sessions_started_after_the_change"`, `restart_required` and
`running_sessions`. Refused before any key: shell mode, vim mode, reverse search,
completions, dialogs, `[Pasted Text: N lines]`, ghost text and possibly soft-wrapped
drafts; the input box is recognised only with its half-block or line borders. Receipts
(`GeminiSteerReceipt`) add `gemini_input`: `model_steering`, `steering_source`,
`busy_at_submit`, `expected` (`user_turn`, `native_queue`, `steering_update`),
`screen_evidence` (`queued` or `steering_hint` seen after a mid-turn Enter, then
`native_accepted`), `outcome` and `steering:{applied,fate}`. The anchor is the first
`user` record at or after the submit containing the text:
`submitted`/`queued` (queued messages are joined into one prompt when the turn ends:
`batched` and `facts.shared_with_turn` true); `steering_applied` (added to the next
tool response: `record_type:"steering_update"`, `fate:"applied_at_next_tool_step"`,
`facts.folded`); `steering_after_turn` (the turn ended without a tool call and Gemini
submitted the update alone once idle: `steering.applied:false`,
`fate:"submitted_after_turn_as_steering_prompt"`; Gemini collapses its whitespace,
shown as `input_delivery.steer.caveat`). Fixtures: `gemini_steer_*`,
`gemini_live_steering`.

**Antigravity.** The screen must show agy's main prompt: the input box between two
full-width rules and one footer row (`? for shortcuts`, `esc to cancel` or `Press up
to edit queued messages`); menus, panels, pickers, the trust or permission dialog and
an armed exit (`TERMINAL_EXIT_ARMED`) are refused. Idle, Enter starts a turn. Busy,
agy's `queuedMessages` setting decides: `queue` (default) Enter queues and alt+m sends
to the running turn; `send-immediately` the reverse. `queue` and `steer` press the
matching key and are refused (`ANTIGRAVITY_QUEUE_MODE_UNKNOWN`) when the setting cannot
be read (an inline `--settings` JSON wins over `settings.json`); rows carry
`input_delivery.queued_messages {value,source}`. The receipt is anchored on the first
explicit `USER_INPUT` step in `brain/UUID/.system_generated/logs/transcript.jsonl`
whose `<USER_REQUEST>` holds the text and adds `antigravity_input` `{label,queue_mode,
queue_mode_source,busy_at_submit,key,expected,screen_evidence (queued|sending),
outcome (submitted|queued|sent_immediately),caveat}`.

### Gemini CLI

Google ended Gemini CLI login for individual accounts, so Gemini sessions cannot be
started with a personal login; the provider is kept for Workspace and API-key
accounts and reports `supported:false` with that reason for observe, stop and events.
Each account is an isolated `GEMINI_CLI_HOME` at `DATA_ROOT/accounts/gemini/NAME`
(0700; state in `HOME/.gemini/`); adoption is not supported. `gemini_binary` (default
`gemini`) selects the executable (the pinned `@google/gemini-cli` through the host
launcher). Every child gets `GEMINI_CLI_HOME`, `GEMINI_CLI_NO_RELAUNCH=1`,
`GEMINI_FORCE_FILE_STORAGE=true`, and has `GEMINI_API_KEY`, `GOOGLE_API_KEY`,
`GOOGLE_GENAI_USE_VERTEXAI`, `GOOGLE_GENAI_USE_GCA`, `GOOGLE_CLOUD_PROJECT`,
`GOOGLE_CLOUD_LOCATION`, `GOOGLE_CLOUD_ACCESS_TOKEN`, `GOOGLE_APPLICATION_CREDENTIALS`,
`GOOGLE_GEMINI_BASE_URL`, `GOOGLE_VERTEX_BASE_URL`, `CLOUD_SHELL`,
`GEMINI_CLI_USE_COMPUTE_ADC` and `GEMINI_MODEL` removed. `.gemini/settings.json` is
seeded on enrollment and before every launch without clobbering other keys:
auto-update and its notice off, usage statistics off, automatic session deletion off,
and the Login-with-Google auth type when none is selected.

**Authentication modes** (provider entry `auth_modes` and `auth_command`). The mode
is the account's `security.auth.selectedType` in `.gemini/settings.json`: `google`
(`oauth-personal`; `google_personal` is `available:false` with its reason,
`google_workspace` available) or `api_key` (`gemini-api-key`; `input:"stdin_json"`,
`field:"api_key"`); Vertex is `UNSUPPORTED_AUTH_MODE`. Clients show unavailable
entries disabled with their `reason`. `api account auth gemini:NAME [--mode google |
--mode api-key --key-stdin]` returns `{account_id,provider,auth_mode,api_key_present,
auth_required,auth_failed,auth_failure_reason,auth_failure_observed_at,changed,
running_sessions,applies_to}` (plus `login_required` and `login` in `google` mode).
`--key-stdin` reads exactly `{"api_key":"..."}` (at most 32 KiB; 1-512 visible ASCII
characters after trimming, else `INVALID_API_KEY_FORMAT`), stores it atomically in
`HOME/secrets/gemini-api-key` (0600 in a 0700 directory; symlinks refused with
`UNSAFE_SECRET_PATH`; a key file that is not owner-only is ignored) and adds
`validation:{state,code,reason,timeout_seconds}` from one headless run (at most 15 s)
in a throwaway home under `registry/provider-state/gemini-validate/`: `valid`,
`auth_failed` or `inconclusive` (the key stays stored). The key reaches only that
account's children as `GEMINI_API_KEY` and `SESSIONSATION_GEMINI_API_KEY` and never
appears in argv, state, receipts, logs, errors or output. Other errors:
`INVALID_REQUEST`, `API_KEY_REQUIRED`. In `api_key` mode the account is logged in when
the key file exists, `account login` is refused (`AUTH_MODE_API_KEY`) and logout
deletes the key. Running sessions keep the mode they started with.

In `google` mode an account is logged in when `.gemini/oauth_creds.json` is a non-empty
file (stat only) and `google_accounts.json` records an active account; login runs
interactive `gemini` with `NO_BROWSER=true`. Account rows add `auth_mode`,
`api_key_present`, `auth_failed`, `auth_failure_reason` (`AUTH_CLIENT_UNSUPPORTED`,
`AUTH_TIER_INELIGIBLE`, `AUTH_GRANT_INVALID`, `AUTH_UNAUTHENTICATED`,
`AUTH_PERMISSION_DENIED`, `AUTH_REAUTH_REQUIRED`, `AUTH_API_KEY_INVALID`,
`AUTH_API_KEY_MISSING`, `AUTH_FAILED`) and `auth_failure_observed_at`, from the last
recognizable failure of a headless run or the login terminal (fatal-authentication
exit 41 or known messages; a network error never counts), stored outside the account
home as a code, time and credential-file stat identity. While set the row is
`auth_required:true`, `selectable:false`, `auth_detail:"AUTH_FAILED"`; a later
successful run, login, newer credential file or newly stored key clears it.

Sessions: `gemini --session-id UUID` (new) and `gemini -r UUID` in the recorded
directory. Discovery reads only the first line (`sessionId`, `projectHash`,
`startTime`) of `.gemini/tmp/SLUG/chats/session-*.jsonl` plus `projects.json`. A
session is `running` only when a process's command line names its id after
`gemini.js` or `gemini`, its environment has this `GEMINI_CLI_HOME` and its PID and
start time verify. Usage is unsupported (`usage_state:"unsupported"`, null windows,
never 0%). Supported: new, resume, rename, terminal input, headless delivery, archive.
Unsupported: stop, restart, account switch, relocation, Space moves, Add to HERDR
(`NO_NATIVE_ATTACH`), compaction, set-model, observe, events and pending questions.
Login, headless JSON and the command-line shape are verified against fakes and the
installed CLI's source only.

### Antigravity CLI

Google Antigravity CLI (`agy`, verified with 1.4.10). Each account is an isolated
`HOME` at `DATA_ROOT/accounts/antigravity/NAME` (0700; state in
`HOME/.gemini/antigravity-cli/`, `.gemini/config`, `.config/antigravity`).
`antigravity_binary` (default `agy`) selects the executable. Every child gets
`AGY_ACCOUNT_HOME` and `HOME` set to the account home, `XDG_CACHE_HOME=HOME/.cache` and
`AGY_CLI_DISABLE_AUTO_UPDATE=1`; ambient Google/Gemini keys, Vertex/Cloud project and
endpoint overrides, `GEMINI_MODEL`, `ANTIGRAVITY_API_KEY`, `AGY_LLM_GATEWAY_*`,
`AGY_ADC_AUTH` and `XDG_RUNTIME_DIR` are removed. An account is logged in when
`.gemini/antigravity-cli/antigravity-oauth-token` is a non-empty file (stat only);
launch and headless delivery are refused with `AUTH_REQUIRED` otherwise. Login runs
`agy` (Google OAuth: open the URL, paste the code) and ends when the token file
appears; `refresh` reports `auth_mode.auth_method:"google-oauth"`. Usage is
unsupported.

agy cannot pre-assign a conversation id, so a new session first creates an empty
conversation with a bounded headless run that sends no prompt (`agy --input-format
stream-json --output-format stream-json --print-timeout 20s
--disable-slash-commands -p=` with empty stdin, at most 30 s), records the id from its
`init` event, then execs `agy --conversation UUID`; resume runs the same command in
the recorded directory. Discovery reads agy's index `conversation_summaries.db`
read-only (id, workspace URI, step count, modification time); conversations with at
least one step are registered, and a conversation exists while `conversations/UUID.db`
does. A session is `running` when an interactive `agy` process whose `HOME` is this
home holds `presence/UUID.lock` (or the db) open, or names `--conversation UUID`;
because the open lock is read, `/resume` or `/clear` inside agy is followed.
Supported: new, resume, rename, stop, terminal input, headless delivery, archive,
observe and events. Unsupported: restart, account switch, relocation, Space moves, Add
to HERDR (`NO_NATIVE_ATTACH`), log out, compaction, set-model and pending questions.

## Data root

### Layout and retention

`api capabilities.data_layout` reports the layout version, directories, account
fields and the `layout` commands; `retention` reports classes, settings and built-in
defaults. The layout, migration, rollback and retention procedures are in
[operations](operations.md#data-root-layout). Account rows carry `home` (the home in
use: an adopted Claude config home, the managed `accounts/<platform>/<name>`, or a
deferred legacy path) and `home_deferred`. A legacy root is refused by every command
except `api version` and `layout` with `LAYOUT_MIGRATION_REQUIRED`; other codes are
`LAYOUT_MIGRATION_IN_PROGRESS`, `LAYOUT_CONFLICT`, `LAYOUT_UNSUPPORTED`,
`BACKUP_REQUIRED` and `DAEMON_RUNNING`. Owner and home paths recorded before a home
moved are compared by resolved path, and `layout migrate` and backend rename rewrite
stored conversation `owner` and pane-binding `home` values (`home_refs_rewritten`);
journals and receipts are never rewritten. Only `sessionsation setup` creates a
missing root; every other command fails closed with `DATA_ROOT_MISSING`.

### Disk-space guard

Capability `disk_space`; config `min_free_disk_mb` (default 512, 0 disables). Archive,
restore, delete, switch, restart, resume, relocate, move-space, launch start,
resume-launch start, account delete, `shared migrate` and `layout migrate` (not dry
runs) check the free space of the file system holding the data root first and refuse
with `INSUFFICIENT_DISK_SPACE` (`free_bytes`, `needed_bytes`, `operation`). `api
disk-space` reports the same figures read-only. Any ENOSPC/EDQUOT is reported as
`DISK_FULL`. A non-cached `api state` whose refresh fails that way serves the last
snapshot with `snapshot.refresh_failed:"DISK_FULL"`, `snapshot.fresh:false` and a
`warnings[]` entry `STATE_REFRESH_FAILED_DISK_FULL`; without a snapshot it fails
`DISK_FULL`.

### Root alias and re-key

On Unraid `/mnt/user/<share>` and `/mnt/disk1/<share>` can be one tree with two
spellings (a bind, not a symlink). `sessionsation root migrate` moves a data root and
its sessions from one spelling to the other; the operator procedure is in
[operations](operations.md#root-re-key). Capability `root_alias` (`since_contract`
1.54.0, `version`, `layout_version: 3`, `state`, `entries`, `commands`,
`session_states`, `normalize_requests`, `gate`, `retained_copies`, `memory_merge`,
`errors`).

```sh
sessionsation root status
sessionsation root migrate --from /mnt/user --to /mnt/disk1 --backup DIR [--dry-run] [--allow-full-copy]
sessionsation root migrate --session S [--expected-generation G] [--accept-risks] [--prompt TEXT | --prompt-file F]
sessionsation root rollback [--session S] [--dry-run]
sessionsation root finalize [--dry-run] [--accept-unresumed]
```

- **Alias.** `--from F --to T` becomes one entry per first path component found in
  stored paths (for example `/mnt/user/appdata` -> `/mnt/disk1/appdata`), accepted only
  when proven: both are real directories, no other array disk or pool mount beside the
  two spellings holds content under the share (network/FUSE mounts are never
  consulted; each check bounded to 2 s, `stat_timeout`), and `st_dev:st_ino` match or a
  probe file written under `T` is read under `F`. Unproven shares are reported
  (`unproven`, `unmapped`). The block is stored in `layout.json` as `root_alias`
  (`version`, `state` `pending|active|inactive`, `root_id`, `canonical_root`,
  `previous_root`, `entries`, `normalize_requests`, `run`, `backup`, `activated_at`).
  Activation stamps layout version 3; earlier builds refuse it (`LAYOUT_UNSUPPORTED`)
  and `root rollback` returns the root to version 2.
- **Opening and comparing.** A root opened by either spelling resolves to
  `canonical_root` when both are the same directory or show the same `root_id`,
  otherwise `ROOT_ALIAS_UNVERIFIED`. `api version` adds `requested_root` and `root`.
  Every path equality or prefix test (homes, working directories, transcripts,
  process environments such as `CLAUDE_CONFIG_DIR` and `CODEX_HOME`) compares the alias
  forms, so a process started with the old spelling stays verified. Stored values keep
  their spelling; new request directories take the new spelling once the alias is
  active. Path-valued fields change spelling for migrated rows; treat them as opaque
  or compare after mapping. While active, each old spelling's runtime path
  (`/tmp/sessionsation-<uid>-<hash>`) is a directory symlink to the canonical runtime
  directory, so clients that derive the control socket from the old root keep working.
- **Root step** (supervisor stopped, else `DAEMON_RUNNING`). Preconditions: layout 2
  complete or an unfinished run of the same `--from/--to` (`LAYOUT_MIGRATION_REQUIRED`,
  `ROOT_MIGRATION_MISMATCH`), the root's entry proven (`ROOT_ALIAS_UNPROVEN`), no
  non-terminal operation and no live hosted Claude worker (`ROOT_MIGRATION_BLOCKED`
  with the list; an unreadable journal also blocks), a rollback-gate snapshot for every
  attached or running session, disk space, `--backup` new or empty and outside the
  root (`BACKUP_REQUIRED`, `BACKUP_NOT_EMPTY`), and reflink unless `--allow-full-copy`
  (`BACKUP_REFLINK_UNSUPPORTED`). Steps, journaled and idempotent in
  `operations/root-migrations/<run>.json` (values in `<run>.changes.jsonl`): `backup`
  (metadata, `VACUUM INTO` database copies, a reflink copy of `shared/claude/projects`,
  every managed `.claude.json`; manifest `root-backup.json` with sha256 per file;
  `BACKUP_VERIFY_FAILED`), `mark`, `links` (the shared-store links re-pointed),
  `registry` (conversation owners, pane homes, thread locations, `codex_binary`),
  `claude` (`.claude.json` project entries cloned to the new key, memory merged,
  stopped sessions re-keyed), `codex` (thread locations of threads not running),
  `activate`, `verify` (`ROOT_VERIFY_FAILED`). Only a session positively known to be
  stopped is re-keyed; an undecided probe leaves it (`left_behind`, `probe_failed`).
  Journals, receipts, archives and logs are never rewritten.
- **Claude re-key of one session.** All lineage transcripts are located (two for one
  id: `ROOT_REKEY_CONFLICT`). Every name to move (`<id>.jsonl`, `<id>/`) is copied to
  `<backup>/sessions/<session>/<attempt>/<old key>/` and verified by sha256 **before**
  any rename, which happens only while the session is stopped, with
  `RENAME_NOREPLACE` (a name already in the new key is a collision). Memory
  (`projects/<key>/memory/`) is merged per file, never moved: a missing file is copied,
  a file changed on one side fast-forwards by an atomic exchange (undone if a writer
  changed the target meanwhile), replaced versions are kept under
  `<backup>/memory-versions/`, and a file changed on both sides is a conflict (copied to
  `<backup>/memory-conflicts/`; the session is not switched). Then the catalog
  `directory` changes once. The quiet check fails closed (unreadable registry or
  account list, a process naming the conversation, a transcript writer). Any error
  after the move moves the names back before `left_behind`.
- **Session step** (`--session`, alias active). A running Claude session in a HERDR
  pane is stopped (pidfd SIGTERM; a busy one needs `--accept-risks`; Claude deliveries
  in flight are waited for up to 30 s, `SESSION_DELIVERY_IN_PROGRESS`), re-keyed,
  resumed once in the same pane and verified (same conversation in the recorded pane;
  registry and `/proc` cwd equal the new directory; the pre-move bytes are a prefix of
  the live transcript; memory). A positive failure moves it back and resumes it on the
  old path (`left_behind`); an unconfirmed resume is `uncertain`; memory problems end
  `verify_failed`. `--prompt` is delivered once after verification. A stopped Claude
  session is re-keyed in place (`rekeyed`); a Codex thread that is not running gets a
  thread location, a running one goes through the directory job (`relocating`, then
  `verified`); archived sessions are `archived_rekey_at_restore`; Gemini and
  Antigravity are `ROOT_REKEY_UNSUPPORTED_PROVIDER`; other refusals
  `ROOT_REKEY_UNSUPPORTED`. Records `operations/root-migrations/session-<session>.json`
  (`kind:"root_rekey"` while in flight; never pruned).
- **`root status`** returns `{layout_version, requested_root, root, root_alias, run,
  counts, sessions, deferred_running, left_behind, memory_split, memory_conflicts,
  collisions, unmapped, retained, gate, gate_detail}`. Session states: `verified`,
  `rekeyed`, `archived_rekey_at_restore`, `deferred_running`, `left_behind`,
  `unsupported_provider`, `not_applicable`, `verify_failed`, `failed`, `uncertain`,
  `relocating`; rows carry their retained copies (`copy`, `sha256`, `verified`). `gate`
  is `pass` when every session attached or running before activation is still
  recognised as running and every probe succeeded (`gate_detail.sessions[]`), `fail`
  otherwise (run `root rollback`), `not_applicable` without an activated run.
- **Rollback** (supervisor stopped): every session this run changed that is not
  `verified` moves back, journaled values are restored (original bytes when unchanged
  since), links re-pointed, unchanged cloned `.claude.json` entries removed, thread
  locations restored when still the re-key's record, `layout.json` back to version 2.
  Refused while a re-key worker runs or a process uses the new spelling of a home in
  `CLAUDE_CONFIG_DIR`, `CODEX_HOME` or `GEMINI_CLI_HOME` (`ROOT_ROLLBACK_BLOCKED` with
  the pids). `root rollback --session S` undoes one session (a running one is stopped,
  moved back and resumed by a detached worker). Rollback also moves back lineage names
  found only in the new key and maps records written after activation back. Problems
  end `rollback_incomplete` (alias still active; rerun).
- **Finalize** removes empty old project-key directories (`rmdir` only) and reports old
  `memory/` directories (`memory_removable`). No command deletes the retained copies.
