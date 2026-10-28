# Setup and CLI

## Build and prerequisites

The source build needs Linux, Git, a C compiler/linker, and rustup with the
repository-pinned toolchain (currently Rust 1.95.0). On Debian/Ubuntu:

```sh
sudo apt-get update
sudo apt-get install -y build-essential git curl ca-certificates gzip rsync tmux
# Install rustup using https://rustup.rs/ if it is not already installed.
rustup show
cargo build --release --locked
```

SQLite is bundled by the Rust dependency; no system SQLite development package is
required. [Contributor instructions](../agents/README.md) cover Python/schema
checks and a container built entirely from public images, without a private cache.

For real accounts, obtain Codex from [OpenAI's CLI installation guidance](https://learn.chatgpt.com/docs/codex/cli).
For example, with Node.js/npm already installed, `npm install -g @openai/codex`
installs the CLI; pin and record the version you actually verify. The
[app-server documentation](https://learn.chatgpt.com/docs/app-server) describes the
native interface. Neither installation nor a successful `--version` check proves
this project's queue, background-terminal, permission or history-export support.
The tested version matrix and remaining gates are in [operations](operations.md).
No provider CLI, credentials or model access is needed for the offline fixtures.

## Configuration and ownership

State defaults to `SESSIONSATION_HOME`, then the platform default:
`$HOME/.local/share/sessionsation` on Linux, or
`/mnt/user/appdata/sessionsation` on Unraid (the array must be mounted).
`--data-dir /absolute/path` overrides both. Only `sessionsation setup` creates a missing
root; every other command fails closed with `DATA_ROOT_MISSING` so a stale
`SESSIONSATION_HOME` or an unmounted array never creates an empty shadow root. The
supervisor's private runtime directory (control socket) is
`/tmp/sessionsation-EUID-HASH`, where HASH is the first 16 hex digits of the
SHA-256 of the canonical data-root path (previously `/tmp/codex-switcher-…`). Use an explicit root consistently in
services and clients; an empty default root is not an import of another root.

For a new installation, select a private, user-owned directory before setup:

```sh
export SESSIONSATION_HOME="$HOME/.local/share/sessionsation"
sessionsation setup
sessionsation status --json
```

Setup starts a metadata supervisor. The config at `DATA_ROOT/config.json` selects
`codex_binary` (default `codex` on PATH) and monitoring thresholds. Use
`sessionsation config --help` for inspection/editing. Enrolling accounts starts
account-specific provider workers and authentication; it is separate from setup.
Stop your new supervisor with `sessionsation daemon stop`. Systemd user installation
is optional through `setup --systemd`; review its service before enabling it.

An existing Codex Switcher root may be selected explicitly with `--data-dir`.
Keep its current sole supervisor and credential writer; do not run setup, copy
credentials, move account homes, or start another owner against live data. Runtime
socket names retain legacy terminology for compatibility. Renaming/migrating a
live installation needs a separate, backed-up maintenance procedure; it is not
part of installing this CLI. Stable IDs live under `session-catalog/`; retain the
catalog and original receipts with the root. An unrelated default Codex home or
unmanaged terminal is not automatically adopted.

## Accounts

For a fresh private data root, `sessionsation setup` initializes its metadata
supervisor. Every account belongs to one provider, chosen explicitly; there is no
default provider:

```sh
sessionsation account add --platform codex example
sessionsation account add --platform claude personal
sessionsation account add --platform claude --config-dir /home/me/.claude existing
sessionsation account login claude:personal
sessionsation account managed-home existing      # adopted Claude home -> managed home
sessionsation account list                      # table with a PROVIDER column
sessionsation account list --platform claude --json
```

Account names are **unique across providers**: `work` can be `codex:work` or
`claude:work`, never both. Adding a taken name fails with `ACCOUNT_NAME_EXISTS`
naming the existing owner. Public IDs are always `<provider>:<name>`. Human
commands also accept a bare name for an existing account (unambiguous because names
are global); a qualified ID whose provider differs from the enrolled one fails with
`ACCOUNT_PROVIDER_MISMATCH`. Missing or unknown `--platform` is an argument error.

The machine API takes qualified IDs only (a bare name is `ACCOUNT_PROVIDER_REQUIRED`):

```sh
sessionsation api account add codex:example
sessionsation api account add claude:personal [--config-dir DIR]
sessionsation api account list [--platform claude]
sessionsation api account list
sessionsation api account login codex:example
sessionsation api account disable codex:example
sessionsation api account enable codex:example
```

Codex add/login return the provider's verification URL and user code. Complete the login
as the user; never copy credentials between homes. Claude login is interactive:
`sessionsation account login claude:NAME` runs the official `claude auth login
--claudeai` subscription login in the current terminal with that account's
environment; `api account login claude:NAME` (and `api account add claude:NAME`)
return an `exec` argv plan for a terminal client instead of a URL. Disabled accounts remain listed;
disabling prevents future selection and does not stop existing agents. Usage and
reported reset timestamps are in account snapshots; do not invent missing windows.

`sessionsation account managed-home NAME` (API: `api account managed-home claude:NAME`)
switches a Claude account adopted with `--config-dir` to the managed home
`<root>/claude-accounts/NAME` without changing its id, name or catalog sessions. The
operator must first copy the config directory there (0700, owned by you, not a
symlink); sessionsation never creates, copies or moves files. It fails closed with
`UNSUPPORTED_PROVIDER_OPERATION` (Codex), `ACCOUNT_HOME_UNAVAILABLE` or
`UNSAFE_ACCOUNT_HOME` (managed directory), `ACCOUNT_IN_USE` (a verified live Claude
session or any readable process with `CLAUDE_CONFIG_DIR` set to either home),
`AUTH_REQUIRED`/`WRONG_AUTH_MODE` (managed home not a first-party claude.ai login) or
`AUTH_IDENTITY_MISMATCH` (a hash of the `auth status` identity differs between the
homes; identity values are never printed). An already managed account succeeds with
`already_managed:true` and `note:"ALREADY_MANAGED"`. A change is recorded as event
`account.claude_home_relocated` through the supervisor.
New account names allow 1–30 ASCII letters, digits, hyphens or underscores
(excluding `codex:`), enforced by public API admission and the account owner.
Existing longer names remain valid for other operations.
User-assigned public session names (new, resume and rename) allow at most 30 Unicode
scalar values, excluding controls and surrounding whitespace. Existing longer
catalog names remain readable and keep their identity; new automatic discovery
labels are shortened with collision-safe suffixes. Space labels and directory
names retain their separate limits.
Historical usage uses `api account usage codex:example --start UNIX --end UNIX
--step 1800`.

Read or save account thresholds through the existing supervisor:

```sh
sessionsation api account limits codex:example
sessionsation api account limits codex:example --short-used 80 --weekly-used 90
```

Both values must be finite percentages from 0 through 100 and supplied together.
These are maximum **used** percentages: 80 used corresponds to 20 remaining in
the UI. Saving delegates to the existing account writer; it does not start a
supervisor, alter credentials, enable automation or resolve an unknown plan tier.
Concurrent edits use the supervisor's serialized last-write behavior. After an
uncertain response, read the limits before deciding whether another save is needed.

Deregistration (`account remove`) previews affected catalog sessions, active/loaded conversations, pending
operations and stored-data counts/paths. It requires exact ID and fresh revision:

```sh
sessionsation api account remove codex:example
sessionsation api account remove codex:example --confirm codex:example --revision PREVIEW-REVISION
sessionsation api account restore codex:example
```

Removal refuses active/loaded conversations or pending lifecycle jobs. Stop or
move them explicitly first. It unregisters the account from sessionsation selection
without disabling the original provider enrollment, stopping agents or deleting
credentials/history. The original switcher remains independent. Restore reverses
the membership change. No upstream provider account is deleted.

Actual local account deletion is separate and never deletes saved conversations:

```sh
sessionsation api account delete codex:example
sessionsation api account delete codex:example --confirm codex:example --request-id NEW-UUID
sessionsation api account deletions --request-id ORIGINAL-UUID
sessionsation api operations
sessionsation api recover ORIGINAL-UUID
```

The first command is a fast intent preview: no account file scanning. Confirmation
admits a durable background operation. Only then does the CLI check active/loaded
sessions and pending work, reserve the account through its sole owner, preserve
saved/archived rollouts and native history metadata in private
`preserved-conversations/REQUEST-ID/home`, and delete the remaining account files
and credentials. Active processes block deletion; it never stops agents to proceed.
The upstream provider account is unchanged. Preserved histories remain discoverable
with their stable session IDs; resume requires an explicit enrolled destination
account. Paginated history requires native verification before credentials can be
deleted; unavailable verification blocks deletion while retaining data.

The account owner must advertise `account_deletion`; an older running owner returns
`owner_upgrade_required:true` in the preview and refuses admission with
`OWNER_UPGRADE_REQUIRED`. Updating a CLI client alone does not replace that owner.
Repeat the same request ID to inspect its original admission, never to resend.
Inspect a failed/interrupted operation before explicitly recovering its original
ID. A running worker rejects recovery. Deletion cannot be cancelled once admitted;
closing a progress viewer only detaches it. Failed operations retain original or
preserved histories and are never automatically retried.

## Terminal hosting

Keep the current terminal with `sessionsation run --account example`, including
inside tmux or HERDR. HERDR's session/pane context is registered automatically.
Outside a managed host, foreground launch still works, but v1 managed lifecycle
requires a discovered tmux/HERDR attachment.

For headless hosting, keep this command running in its own terminal/service:

```sh
herdr --session coding server
```

Then launch and optionally attach a viewer:

```sh
sessionsation run --account example --host herdr --herdr-session coding
herdr session attach coding
```

The launch JSON reports `running` only after matching the exact account home,
conversation and process. `launch_unconfirmed` means inspect before retrying; the
command may already have started. Admission alone is not successful startup.
No existing live session is migrated.

New Codex sessions default to YOLO: approval policy `never` and the native
`:danger-full-access` permission profile. The backend selects and verifies these
permissions at conversation creation, before saving history or attaching a terminal.
Explicit `--sandbox`/`--ask-for-approval` or `--full-auto` options still take precedence.
`run -- --yolo` is accepted as the short alias for
`--dangerously-bypass-approvals-and-sandbox`. Remote Codex terminals inherit the
verified server-side permissions; those flags are deliberately not passed to the
remote terminal command. Resume and account transfer retain recorded permissions
unless an explicit launch override is supplied.

Detach a HERDR viewer with default **Ctrl+B, Q**. This leaves agents running.
Backend `api stop` ends the selected agent and preserves resumable history.
`herdr server stop` stops the named host and panes; it is not viewer detachment.
Restart replaces the terminal client on its selected account, retaining recorded
permissions and the existing account server. Clear unsent drafts before lifecycle
operations. Switching waits for positive idle and never forces interruption.

## Public machine interface

Use [contract v1](contract.md) for state/watch, host-owned display names, generation-checked
session operations, receipts and launcher plans. `api --api-version 1` accepts only
supported contract versions. All API output is JSON; watch uses NDJSON.

```sh
sessionsation api state
sessionsation api state --watch --interval 2
sessionsation api launcher new --account codex:example
sessionsation api operations
```

Use backend session IDs and generations from state, not example pane IDs. Operation
responses require a fresh confirmation sequence. Use the public operation commands:

```sh
sessionsation api respond OPERATION-ID yes --sequence CURRENT-SEQUENCE
sessionsation api cancel-operation OPERATION-ID
```

No automatic switching, forced-steer messaging or cross-provider conversion is
enabled. See [Claude Code accounts and sessions](#claude-code-accounts-and-sessions)
and [architecture](architecture.md).

Passive native readiness is available through `sessionsation api observe --session
BACKEND-ID --expected-generation GENERATION`; see the authoritative
[readiness and receipt contract](contract.md#passive-readiness). Aggregate state/watch
remains attachment discovery, not proof of idle activity.

Popup clients resume a saved session into a new HERDR tab using `api resume-launch
validate` and `api resume-launch start` with a versioned JSON request on stdin.
An optional `directory` plan chooses a different working directory without moving project files; omitted plans retain the saved location.
Use `api resume-launch list` or `api resume-launch status --request-id UUID` to
inspect the original request after detaching. The backend uses the saved directory when the request omits a directory plan;
account, name and destination Space are explicit choices.
See [the request and failure contract](contract.md#saved-session-popup-resume).

## Move a running session between HERDR Spaces

Read a fresh session ID/generation from `api state`, then list destinations on
that session's own HERDR instance (not the invoking terminal's current instance):

```sh
sessionsation api spaces --session SESSION-ID --expected-generation GENERATION
sessionsation api move-space --session SESSION-ID --expected-generation GENERATION --space SPACE-ID --request-id REQUEST-UUID
sessionsation api operations
sessionsation api recover REQUEST-UUID
```

Choose an existing Space ID from that response and a fresh request UUID. The
selected pane moves into a new normal tab without focus stealing; the existing
agent process, account, conversation, working directory and permissions survive.
Busy agents can move without interruption or a provider command. Split siblings
stay in the source tab; HERDR may close an empty source tab/Space. A zoomed tab may
need to be unzoomed first. tmux sessions, inactive conversations, cross-instance
moves and creating a destination Space are outside this action. Use New/Resume
for a saved session's launch destination.

The request returns an `admitted` receipt; a detached worker completes the move
even if the requesting CLI or UI closes. Observe its operation ID with
`api operations` (or repeat the identical request).
Only phase `complete` confirms the move and ownership update. For `uncertain`,
never issue a replacement request: Recover inspects the original stable terminal
and reconciles metadata without sending another move. Until it can prove the
requested destination, the reservation blocks competing lifecycle actions;
manual host inspection may be needed. Repeating the original request observes its
record, including failure, rather than replaying it. See the
[placement contract](contract.md#herdr-space-placement).
## Executable and archive discovery

```sh
sessionsation --version
sessionsation api version
sessionsation api capabilities
sessionsation api state --archive-filter all
sessionsation api state --archive-filter archived
```

Read the exact binary hash/capabilities before selecting a reviewed client. The
[identity and archive contract](contract.md#archive-metadata-and-filtering) defines
positive archive evidence versus unobserved catalog records. These queries never
resume, unarchive or delete conversations.

## Saved and archived conversations

Inactive Codex conversations have separate Archive and Delete actions. Archive
exports verified gzip history to `DATA_ROOT/archived-sessions/SESSION-ID`, outside
all credential homes, before removing native copies. Restore retains backend and
provider conversation IDs and exposes saved history without starting an agent.
It rejects live/loaded conversations, stale generations and changed compressed
files. Project files and account credentials are outside these operations.

Read a fresh ID and generation from `api state`, or `api archives` for archived
items. Each mutation needs a new request UUID and explicit confirmation of that
exact stable backend ID:

```sh
sessionsation api archives
sessionsation api archive --session ID --expected-generation GENERATION --request-id REQUEST-UUID --confirm ID
sessionsation api restore-session --session ID --expected-generation GENERATION --request-id REQUEST-UUID --confirm ID
sessionsation api delete-session --session ID --expected-generation GENERATION --request-id REQUEST-UUID --confirm ID
sessionsation api operations
```

These commands admit durable background jobs; only phase `complete` confirms
completion. Delete permanently removes the selected conversation, its subagents
and known local history/archive copies. It does not remove projects, account
credentials or upstream provider accounts; external backups are outside its scope.
Archive and Delete both require inactive sessions. Failure retains recoverable
archive data where available. Prepared archives are listed for explicit recovery;
repeat request IDs observe the original job and never replay a failed mutation.
Do not edit archive manifests or move managed history between accounts manually.

Explicit Resume-and-submit is separate from ordinary saved-session launching.
Discover `message_recovery` with `api capabilities`; use `api recovery-plan` for an
exited agent, then `api send` with explicit recovery preconditions. Receipt queries
and `api recovery-reconcile` never send input or restart a process. See the
[public recovery contract](contract.md#explicit-resume-and-submit) for payloads,
owner compatibility, refusal reasons and launch-versus-delivery facts.

## Directory changes and project moves

Active managed Codex sessions advertise `change_directory` and `move_project`
capabilities. `api relocate --session ID --expected-generation GENERATION --to
/absolute/destination --dry-run` validates a destination and returns the source,
destination, affected sessions and confirmation detail without changing files.
Add `--move-files` to move the project folder; otherwise the destination must
already exist and files remain in place. After reviewing the plan, invoke the
same command without `--dry-run` to explicitly authorize the operation.

The generation is checked again at admission. Existing lifecycle reservations,
non-steering idle checks, background-work consent and recovery apply. Both
actions retain account, conversation and logical session identity; a live agent
is restarted in the new directory. Folder moves reject existing destinations,
symlinks and protected roots and wait for other sessions using the source.
Cross-filesystem moves retain the original backup. Poll the returned operation
through `api operations`; never automatically retry an uncertain mutation.
For inactive sessions, choose the directory in the Resume launch plan instead.

## Explicit in-turn steering

For a deliberate message to a busy turn, use `api steer-target --session ID --expected-generation GENERATION`, then `api send` with `mode:"steer"` and the
returned turn/runtime preconditions. This is acceptance-only and never creates,
restarts or interrupts a session. See the [steering contract](contract.md#deliberate-in-turn-steering-revision-5)
for the request, receipt, retry and capability-negotiation rules.

For read-only intermediate assistant updates from an exact tracked turn, use
`api checkpoints < request.json`; see [public turn checkpoints](contract.md#public-turn-checkpoints-revision-7)
for identity preconditions, cursors and the distinction from delivery receipts.

### Batch HERDR Space membership

`sessionsation api placements` reads all known session placements in one batch.
For a graph already reading `api state`, use `sessions[].placement` and `hosts`
in that response instead. Match opaque IDs, not Space labels or `original_host`.
See [placement freshness and identity](contract.md#batch-placement-observations-revision-9)
for last-known/stopped behavior and capability negotiation.

## Claude Code accounts and sessions

Each Claude account is an isolated `CLAUDE_CONFIG_DIR`. The normal layout is the
managed home `DATA_ROOT/claude-accounts/NAME` (mode 0700), created by `account
add --platform claude NAME`. If an operator has already placed a home there (for
example a copied existing config), `account add` registers it as is: it must be a
0700 directory owned by the user and not a symlink, and it is never wiped,
re-initialised or re-permissioned (otherwise `UNSAFE_ACCOUNT_HOME`). Enrollment
reports the home's current authentication state (`auth.problem`, never identity).
As an advanced option, `--config-dir DIR` adopts a config directory **in place**: it must be absolute, exist, be a
directory, lie outside the data root, and not overlap another account's home. It
is never copied or moved. Config keys `claude_binary` (default `claude`) and
`claude_session_home` (optional absolute `HOME` for Claude children; unset
inherits) select the runtime.

Every Claude child is built by one function: `CLAUDE_CONFIG_DIR` is the account
home, `DISABLE_AUTOUPDATER=1`, and ambient `ANTHROPIC_API_KEY`,
`ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_OAUTH_TOKEN`,
`CLAUDE_CODE_USE_BEDROCK|VERTEX|FOUNDRY`, `ANTHROPIC_BEDROCK_BASE_URL` and
`ANTHROPIC_VERTEX_PROJECT_ID` are removed so billing/backend cannot switch
silently. Before any launch, `claude auth status --json` (bounded) must report a
`claude.ai` login on `firstParty`; otherwise the launch is refused with
`AUTH_REQUIRED` or `WRONG_AUTH_MODE`. Email and organization fields are never
stored or printed. The supervisor repeats this probe to report `auth_required`.

```sh
sessionsation run --account claude:personal          # new session here
sessionsation resume SESSION-ID --expected-generation GENERATION
sessionsation api launcher new --account claude:personal
sessionsation api launcher resume --session SESSION-ID --expected-generation GENERATION
```

A new session generates its native id, records the catalog entry (provider,
account, `conversation_id`, directory) and then execs `claude --session-id UUID
--name LABEL --settings JSON`. Resume runs `claude --resume UUID --name LABEL`
in the recorded directory; it refuses a verified live session ("already running;
focus it") and a missing transcript (`SESSION_STALE`). `api launch` and `api
resume --herdr-session` accept `claude:` accounts and sessions the same way.

Discovery reads only `sessionId`/`cwd`/`timestamp` from transcript headers in
`projects/*/*.jsonl` (never message text). A session is `running` only when a
`sessions/PID.json` registry entry names it, the PID exists with the recorded
`procStart` (PID-reuse guard) and, when readable, the process's config dir is the
account home; `native_status` (`idle`/`busy`) and `native_name` come from that
entry. Otherwise it is `stopped` (transcript present), `stale` (transcript
missing) or `unknown` (probe failed). Names/titles never establish liveness.
Rename changes only the catalog label (applied as `--name` next launch/resume).

Stop, restart, account switch, relocation, Space moves, archive/delete and
Submit/Queue/Steer are unsupported for Claude and rejected with
`UNSUPPORTED_PROVIDER_OPERATION`; capabilities give the reason.

Usage comes only from Claude Code's statusline. Launches pass a statusline that
runs the hidden `sessionsation provider-hook claude-statusline`, which atomically
writes `CLAUDE_CONFIG_DIR/.sessionsation/usage.json` (0600) with the rate-limit
windows only, then runs the account's own `settings.json` `statusLine.command`
with the same input and passes its output through. Recorder errors never break the
statusline. Rate limits appear only for subscription accounts after the first API
response; missing windows stay `null`, and readings older than 30 minutes or past
their reset are stale. No model request is made to obtain usage.
