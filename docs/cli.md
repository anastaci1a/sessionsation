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
`codex_binary` (default `codex` on PATH), `claude_binary`, `gemini_binary` (default `gemini`) and monitoring thresholds. Use
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
sessionsation account add --platform gemini me
sessionsation account add --platform claude --config-dir /home/me/.claude existing
sessionsation account login claude:personal
sessionsation account managed-home existing      # adopted Claude home -> managed home
sessionsation account list                      # table with a PROVIDER column
sessionsation account list --platform claude --json
sessionsation account rename work Office         # display name only; --clear resets
sessionsation account logout claude:personal   # sign out; log in again with account login
sessionsation account reorder claude:personal --position 0   # shared with the UI
```

Account names are **unique across providers**: `work` can be `codex:work` or
`claude:work` or `gemini:work`, never two of them. Adding a taken name fails with `ACCOUNT_NAME_EXISTS`
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
sessionsation api account refresh claude:personal
sessionsation api account logout claude:personal
sessionsation api account rename codex:example --display-name Office   # or --clear
```

Codex add/login return the provider's verification URL and user code. Complete the login
as the user; never copy credentials between homes. Claude login is interactive:
`sessionsation account login claude:NAME` runs the official `claude auth login
--claudeai` subscription login in the current terminal with that account's
environment; `api account login claude:NAME` (and `api account add claude:NAME`)
return the same `exec` argv plan (`login`) for a terminal client instead of a URL.
`api account add claude:NAME` also returns `login_required` (true unless the home
already holds a first-party claude.ai login), so a client can exec the plan directly.
The human `account add --platform claude NAME` starts that login immediately when a
fresh managed home is not logged in and stdin is a terminal (`--no-login` skips it);
adopted or operator-populated homes are never logged in automatically. The plan runs
`claude auth login --claudeai` through the single Claude environment builder
(`CLAUDE_CONFIG_DIR`, `DISABLE_AUTOUPDATER=1`, API-key/backend variables removed).
After login the supervisor re-probes `claude auth status` every 3 seconds while an
account is `auth_required`; `api account refresh claude:NAME` probes immediately and
returns `{account_id,provider,auth_required,auth_detail,auth_mode,observed_at}`
(`auth_mode` holds `logged_in`, `auth_method`, `api_provider`, `subscription_type`;
never identity). Codex refresh is `UNSUPPORTED_PROVIDER_OPERATION`. Disabled accounts remain listed;
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
Account display names (`account rename ACCOUNT DISPLAY_NAME`, API `api account
rename PROVIDER:NAME --display-name X|--clear`) apply to every provider and change
only the `display_name` shown in account JSON and the human `NAME` column (`display
(name)`). The stable name, `<provider>:<name>` id and home directory (a live
`CODEX_HOME` or `CLAUDE_CONFIG_DIR`) never change. Display names use the account-name
characters, at most 30, and are unique across every account's name and display name
(`ACCOUNT_DISPLAY_NAME_EXISTS`; a new account name equal to a display name is
`ACCOUNT_NAME_EXISTS`). An empty value or `--clear` resets it. The change is written
by the supervisor (event `account.renamed`). Human commands accept an unambiguous
display name wherever they accept an account name.
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
sessionsation api account remove claude:personal   # same preview/confirm/restore flow
```

Removal refuses active/loaded conversations or pending lifecycle jobs. Stop or
move them explicitly first. It unregisters the account from sessionsation selection
without disabling the original provider enrollment, stopping agents or deleting
credentials/history. The original switcher remains independent. Restore reverses
the membership change. No upstream provider account is deleted. Claude removal has
the same API and semantics: its preview reports verified live sessions, Claude
processes using the home (`active_processes`), unfinished headless deliveries and
transcript counts; a removed Claude account is hidden from selection and refuses
launch, resume and headless delivery with `ACCOUNT_REMOVED` until restored.

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
account. Claude deletion (`api account delete claude:NAME`) uses the same
commands, journal and recovery. It refuses while a verified live Claude session, a
Claude process with that `CLAUDE_CONFIG_DIR` or an unfinished headless delivery uses
the account. For a managed home it moves `projects/`, `history.jsonl`,
`file-history/` and `todos/` to `preserved-conversations/REQUEST-ID/home`, then
deletes the managed home including credentials (`home_disposition:"deleted"`). For
an adopted `--config-dir` home it never modifies or deletes the directory: only the
account record is removed and the receipt reports `home_disposition:"left_in_place"`
with `external_home`. The deleted account's catalog sessions stay listed as
`stopped` with `resume_block_reason:"account_deleted"`; resume and headless delivery
fail with `ACCOUNT_DELETED`. Paginated history requires native verification before credentials can be
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
sessionsation api state --cached                 # last snapshot, no scans (see contract revision 14)
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

## Add a running session to HERDR

`sessionsation api session repair-identity` restores merged session ids and
continuation lineage that an older release dropped from the catalog. It is
idempotent and never deletes, recreates or renames an entry.

A running session without a HERDR terminal reports `capabilities.add_to_herdr`
and `capability_reasons.add_to_herdr` in `api state`. List the destination
server's existing Spaces, then open it in a new tab (the name defaults to the
current session name):

```sh
sessionsation api spaces --session SESSION-ID --expected-generation GENERATION --herdr-session HERDR-SERVER
sessionsation api session add-to-herdr --session SESSION-ID --expected-generation GENERATION --herdr-session HERDR-SERVER --space SPACE-ID [--name NAME]
```

Only Claude background-daemon sessions are supported: the new tab runs `claude
attach <job>` with the account environment, which views the already-running
session (no agent is started). Interactive Claude sessions in another terminal and
Codex terminals outside HERDR cannot be moved (`INTERACTIVE_ELSEWHERE`,
`UNSUPPORTED_PROVIDER_OPERATION`). Stopped sessions use Start Session instead,
which also accepts a placement:

```sh
sessionsation api resume --session SESSION-ID --expected-generation GENERATION --herdr-session HERDR-SERVER [--space SPACE-ID] [--name NAME]
```

See [the contract](contract.md#session-identity-continuations-and-add-to-herdr-revision-12).

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
returned turn/runtime preconditions. It never creates, restarts or interrupts a
session. `api receipt` then reports the reply attributed to that steer. See the
[steering contract](contract.md#deliberate-in-turn-steering-revision-5) and
[steer replies](contract.md#steer-reply-correlation-revision-15) for the request,
receipt, retry and capability-negotiation rules.

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
A Claude session with a verified HERDR attachment (exact PID and start time) uses the
same name path as Codex: tab label changes flow into the catalog name
(`name_source:"herdr_last_observed"`) and `api rename`/`rename --session ID` renames
the HERDR tab. Detached or stopped sessions keep their catalog label (`name_source:
"catalog"`). The native Claude title is never changed live; the label is applied as
`--name` on the next launch/resume.

Stop, restart, account switch, relocation, Space moves and archive/delete are
unsupported for Claude and rejected with `UNSUPPORTED_PROVIDER_OPERATION`;
capabilities give the reason. Submit/Queue/Steer work only for hosted sessions and
for live sessions of accounts with live steering enabled (below).

Headless delivery (Agent-DM) uses the existing `api send`/`api receipt` commands.
`mode:"resume"` continues a **stopped** session (`session_id` +
`expected_generation`) by running `claude -p --resume UUID --output-format json` in
its recorded directory under the account environment; `mode:"new"` takes
`account_id`, an absolute existing `directory` and optional `name`, records the
catalog entry with a pre-assigned id and runs `claude -p --session-id UUID
--output-format json`. The message is written to stdin, not argv. Verified live
sessions (`SESSION_ALREADY_RUNNING`), missing transcripts (`SESSION_STALE`) and auth
problems are refused before anything is journaled. The request is journaled under
`DATA_ROOT/claude-deliveries/` before a detached worker starts Claude once, bounded
by `timeout_seconds` (default 600, max 3600). The receipt's `reply` holds the parsed
result text, `is_error`, `session_id`, `num_turns` and `usage`. A different reported
session id, a timeout, unparseable output or a worker that exits mid-run is
`delivery_unknown` and is never replayed; repeating the same `message_id` and
payload returns the original receipt, and a changed payload is `MESSAGE_ID_CONFLICT`.
Interactive resume of the same session is refused while a delivery runs.

Usage comes only from Claude Code's statusline. Launches pass a statusline that
runs the hidden `sessionsation provider-hook claude-statusline`, which atomically
writes `CLAUDE_CONFIG_DIR/.sessionsation/usage.json` (0600) with the rate-limit
windows only, then runs the account's own `settings.json` `statusLine.command`
with the same input and passes its output through. Recorder errors never break the
statusline. Rate limits appear only for subscription accounts after the first API
response; missing windows stay `null`, and readings older than 30 minutes or past
their reset are stale. No model request is made to obtain usage.

### Hosted Claude sessions and steering (revision 16)

A **hosted** session is run by Sessionsation instead of a terminal: a detached
worker starts `claude -p --input-format stream-json --output-format stream-json
--verbose --replay-user-messages --permission-prompt-tool stdio` with
`--session-id UUID` (new) or `--resume UUID` (a stopped conversation) under the
account environment. The terminal (or HERDR pane) shows a viewer of the worker's
normalized event log instead of the TUI.

```sh
sessionsation run --account claude:personal --hosted [--permission-mode plan]
sessionsation run --account claude:personal --hosted --thread CONVERSATION-UUID
sessionsation view SESSION-ID [--once]        # live log; t + Enter takes over, q quits
sessionsation take-over SESSION-ID            # stop the worker, then claude --resume here
sessionsation api launcher new --account claude:personal --hosted
sessionsation api launcher resume --session ID --expected-generation G --hosted
sessionsation api hosted start --account claude:personal --directory /abs/dir [--name N]
sessionsation api hosted start --session ID --expected-generation G
sessionsation api hosted status|events --session ID
sessionsation api hosted stop --session ID --expected-generation G [--interrupt] [--timeout-seconds N]
sessionsation api hosted respond --session ID --request-id R --allow|--deny [--message TEXT]
```

`api launch start` accepts an optional `"hosted":{"permission_mode":...}` for
`claude:` accounts; the new pane then runs the viewer. The permission mode is fixed
at launch: `account` (default) passes no flag, so the account's normal mode applies,
except that an account default of `bypassPermissions` is replaced by `default`;
`default`, `acceptEdits`, `plan` and `bypassPermissions` are passed explicitly.
Tool permission prompts arrive as `can_use_tool` requests and stay pending (in
`api hosted status`, the session row's `hosted.pending_permissions` and the viewer)
until answered with `api hosted respond`; nothing is auto-approved.

The worker is the only writer: its lock (`DATA_ROOT/claude-hosted/UUID/worker.lock`)
blocks interactive resume, headless delivery and a second host. Detached from the
supervisor, it survives supervisor restarts and is re-adopted by the next read. A
worker that dies is marked `stopped` (`WORKER_LOST`); while its provider still
drains (no stdin writer, it exits at end of input) the host is `stopping` and still
blocks other writers; `api hosted stop` terminates such an orphan. Closing the
viewer does not stop the worker. **Take over** stops the worker cleanly (stdin
closed after queued work; terminated after the timeout) and only then execs the
interactive `claude --resume UUID` in the same terminal.

`api send` modes `submit`, `queue` and `steer` on a hosted session write one
stream-json user frame with `uuid` = `message_id` and priority `next`, `later` or
`next`. Claude Code (verified in 2.1.312) treats the priorities as follows:

- `next` sent during a running turn **does not interrupt it**: the message is folded
  into that turn at its next tool round (between tool calls, as when you type
  mid-turn in the TUI). A turn that ends without another tool round finishes
  unchanged, and the message then runs as the next turn.
- `later` is never folded; it runs as its own turn after the current one.
- `now` **cancels** the running turn and starts a new one with the message.
  Sessionsation never sends `now`, so a steer never interrupts by itself;
  session rows say so in `input_delivery.steer` (`{priority:"next",
  interrupts:false, delivery:"next_tool_boundary_or_next_turn", ...}`).

`"steering":{"version":1,"interrupt":true}` (steer only) is the explicit way to
cancel the running turn first; the steer then runs as a new turn. Receipts
(`delivery_semantics:"hosted_stream_json"`) follow the `command_lifecycle` events
of that uuid and the `result` whose `user_message_uuids` names it; the reply is the
assistant text streamed between `started` and that result. A message is
`completed_with_reply`/`completed_without_reply` only after both its result and
its `completed` lifecycle. A cut turn still emits a `success` result with the
partial text, followed by `cancelled` for its inputs; such a message ends
`interrupted` (`TURN_INTERRUPTED` after a requested interrupt,
`TURN_CANCELLED_BY_PROVIDER` otherwise) with the partial reply kept and
`reply.final:false`. It is never replayed. A steer folded into a running turn has
`facts.folded:true`; its reply starts at its own `started`, and like the turn's
other inputs it has `shared_with_turn:true`.

**Live steering** reaches a normal interactive session through Claude Code's
cross-session messaging. It is per-account opt-in:

```sh
sessionsation account live-steering claude:personal --enable   # crossSessionInbound: accept
sessionsation account live-steering claude:personal --disable  # removes it again
sessionsation api account live-steering claude:personal        # show
```

The toggle edits only that key of the account's `settings.json` (other keys kept,
atomic replace, file mode kept; a symlinked file is refused; an operator's own
`hold`/`refuse` is never removed) and records `account.live_steering_enabled|disabled`.
Then `api send` on a running, non-hosted session of that account uses the session's
messaging socket from its registry entry: a detached worker listens on
`RUNTIME/peer/MESSAGE-ID.sock`, sends the frame (`from` = that socket, `session_id`
= the verified registry session), requests an idle notice and records
`peer_message_status` frames. Claude shows the text as a message **from another
Claude session**, which the model may only partially follow; receipts say so
(`delivery_semantics:"peer_message"`, `peer_message.label`). The reply is read from
the transcript after the record carrying the message id, up to the next input;
without that anchor the receipt never claims an answer. `held` (waiting for the
session's approval), `refused`, `expired` and dropped messages are reported as such;
peer messages use the same priorities (a steer is `next`: read between tool
calls, or a new turn when idle) and cannot interrupt. Without the opt-in, submit/queue/steer are refused
with `LIVE_STEERING_DISABLED`; session rows carry `capabilities.submit|queue|steer`,
`capability_reasons` and `input_delivery.semantics`.

Both paths journal under `DATA_ROOT/claude-steer/` before writing, share the
message-id namespace with every other store and never replay an uncertain send.
Hosted and live steering are tested against fakes; the fake hosted provider models the priority and cut-turn behaviour observed live with Claude Code 2.1.312.

## Gemini CLI accounts and sessions

Gemini CLI is the third provider. Each account is an isolated `GEMINI_CLI_HOME`
at the managed home `DATA_ROOT/gemini-accounts/NAME` (mode 0700); everything
Gemini keeps for it lives in `HOME/.gemini/`. Adopting an existing directory is
not supported. An operator-placed home is registered as is under the same rules as
Claude (`UNSAFE_ACCOUNT_HOME` otherwise). The config key `gemini_binary` (default
`gemini`) selects the executable. The supported install is the pinned
`@google/gemini-cli` through the host launcher; updates only by explicit reinstall.

Every Gemini child is built by one function: `GEMINI_CLI_HOME` is the account
home, `GEMINI_CLI_NO_RELAUNCH=1` (the recorded PID is the real process),
`GEMINI_FORCE_FILE_STORAGE=true`, and ambient `GEMINI_API_KEY`, `GOOGLE_API_KEY`,
`GOOGLE_GENAI_USE_VERTEXAI`, `GOOGLE_GENAI_USE_GCA`, `GOOGLE_CLOUD_PROJECT`,
`GOOGLE_CLOUD_LOCATION`, `GOOGLE_CLOUD_ACCESS_TOKEN`,
`GOOGLE_APPLICATION_CREDENTIALS`, `GOOGLE_GEMINI_BASE_URL`,
`GOOGLE_VERTEX_BASE_URL`, `CLOUD_SHELL`, `GEMINI_CLI_USE_COMPUTE_ADC` and
`GEMINI_MODEL` are removed. `HOME/.gemini/settings.json` is seeded on enrollment
and before every launch without clobbering other keys: auto-update and its notice
off, usage statistics off, automatic session deletion off, and the Login-with-Google
auth type when none is selected.

Gemini has no `auth status` command. An account is logged in when
`.gemini/oauth_creds.json` is a non-empty file (only stat'ed, never read) and
`google_accounts.json` records an active account; validity is not checked without
a network call. Launch and headless delivery are refused with `AUTH_REQUIRED`
otherwise. `sessionsation account login gemini:NAME` (plan: `api account login`;
also returned by `api account add gemini:NAME`) runs interactive `gemini` with
`NO_BROWSER=true` in the account environment; the user chooses Login with Google and
pastes the code. Refresh (`api account refresh gemini:NAME`) re-checks the files.
Add, display-name rename, removal/restore and deletion follow the Claude semantics;
deletion preserves the chat files, `projects.json` and prompt history in the
preserved-conversations vault and deletes the home with its credentials, settings
and trust files.

```sh
sessionsation account add --platform gemini me
sessionsation account login gemini:me
sessionsation run --account gemini:me                 # gemini --session-id UUID
sessionsation resume SESSION-ID --expected-generation GENERATION   # gemini -r UUID
```

A new session generates its native id, records the catalog entry and working
directory, then execs `gemini --session-id UUID`. Resume runs `gemini -r UUID` in
the recorded directory (sessions are working-directory scoped), including through
`api resume --herdr-session ... [--space] [--name]`. Discovery reads only the first
line (`sessionId`, `projectHash`, `startTime`) of
`.gemini/tmp/SLUG/chats/session-*.jsonl` plus `projects.json` for the directory,
never message text. Gemini keeps no live registry: a session is `running` only when
a process's command line names its id (`--session-id`/`-r UUID` after `gemini.js`
or `gemini`), its environment has this `GEMINI_CLI_HOME` and its PID and start time
verify. A process maps to the one catalog entry with that native id in that
account. A conversation switched with `/resume` inside a running process is not
visible from outside (the command line is unchanged). Otherwise `stopped`, `stale`
(chat missing) or `unknown`. Labels are Sessionsation labels (`native_rename`
false); a verified HERDR attachment syncs tab names exactly as for Claude.

Usage is **unsupported**: Gemini exposes quota only in its interactive `/stats
model`. Accounts report `usage_state:"unsupported"`, `usage.short`/`weekly` null,
and nothing is shown as 0%. Account limits cannot be set. Stop, restart, account
switch, relocation, Space moves, archive/delete and Submit/Queue/Steer are
unsupported. Add to HERDR is unsupported (`NO_NATIVE_ATTACH`; Gemini has no attach
to a running process): stop it, then use Start Session.

Headless delivery uses `api send` as for Claude, for **stopped** sessions
(`mode:"resume"`) and `mode:"new"`: `gemini -p "" -o json --approval-mode default
(-r|--session-id) UUID` with the message on stdin, `GEMINI_CLI_TRUST_WORKSPACE=true`
and the recorded directory. The approval mode is never `yolo`, so tools that need
confirmation do not run unattended. The JSON result `{session_id,response,error?}`
becomes the receipt's `reply`; a different reported id, a timeout or unparseable
output is `delivery_unknown` and never replayed. Live sessions are refused
(`SESSION_ALREADY_RUNNING`). Login, the NO_BROWSER flow, headless `-r -p` JSON and
the process command-line shape are verified only against a fake and the installed
CLI's source until a real login exists.

## Account log out

`sessionsation account logout ACCOUNT` (API: `api account logout PROVIDER:NAME`)
signs the account out with the provider's own mechanism and leaves the account
enrolled with `auth_required: true`, so the login plan (`account login`) is
available again. History, enrollment and the home are untouched. Codex asks the
account's app-server (`account/logout`, so no process keeps stale auth), or runs
`codex logout` with the account's `CODEX_HOME` when no server runs. Claude runs
`claude auth logout` with the account environment and confirms with `claude auth
status`. Gemini CLI 0.66.0 has no logout command: its `/auth signout` calls
`clearCachedCredentialFile()`, which removes `.gemini/oauth_creds.json` and moves
the active entry of `.gemini/google_accounts.json` to `old` (active becomes null);
sessionsation applies exactly those two effects. The command is refused with
`ACCOUNT_IN_USE` while a live session or provider process uses the account, and
fails clearly for unknown (`ACCOUNT_NOT_FOUND`, including deleted), removed
(`ACCOUNT_REMOVED`) and mismatched-provider accounts. Repeating it on an
already-logged-out account succeeds.
