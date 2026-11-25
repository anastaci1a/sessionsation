# Setup and CLI

Usage only: commands, flags and short examples. Behaviour, fields and error codes
are in the [contract](contract.md); `sessionsation --help` and `sessionsation api
--help` list every command.

## Build and prerequisites

Linux, Git, a C compiler/linker and rustup with the repository-pinned toolchain.
SQLite is bundled. On Debian/Ubuntu:

```sh
sudo apt-get install -y build-essential git curl ca-certificates gzip rsync tmux
rustup show
cargo build --release --locked
```

Running agents needs the provider CLI you enroll: Codex (for example `npm install -g
@openai/codex`; see [OpenAI's CLI guidance](https://learn.chatgpt.com/docs/codex/cli)
and the [app-server documentation](https://learn.chatgpt.com/docs/app-server)), Claude
Code, Gemini CLI or Antigravity CLI (`agy`), plus tmux or HERDR. A successful
`--version` does not prove compatibility; the verified versions are in
[operations](operations.md#version-scope). The offline fixtures need no provider,
credentials or model access.

## Data root and configuration

The data root is `--data-dir /absolute/path`, else `SESSIONSATION_HOME`, else
`$HOME/.local/share/sessionsation` (Linux) or `/mnt/user/appdata/sessionsation`
(Unraid). Only `setup` creates a missing root; every other command fails with
`DATA_ROOT_MISSING`. The supervisor's control socket is under
`/tmp/sessionsation-EUID-HASH` (HASH: first 16 hex digits of the SHA-256 of the
canonical root path).

```sh
export SESSIONSATION_HOME="$HOME/.local/share/sessionsation"
sessionsation setup                 # creates the root, starts the metadata supervisor
sessionsation setup --systemd       # optional user service; review it first
sessionsation status --json
sessionsation daemon start|stop
sessionsation config --help         # inspect and edit DATA_ROOT/config.json
sessionsation config set launch_permissions default
```

Keep one supervisor per root. An existing Codex Switcher root may be selected with
`--data-dir`; keep its sole supervisor and credential writer, and do not run setup,
copy credentials or move account homes against live data. An unrelated default Codex
home or unmanaged terminal is not adopted.

Config keys (`config.json`): `codex_binary`, `claude_binary`, `gemini_binary`,
`antigravity_binary`, `claude_session_home`, `curl_binary`, monitoring thresholds,
[`launch_permissions`](contract.md#launch-permissions),
[`claude_pretrust_folders`, `antigravity_pretrust_folders`](contract.md#folder-pre-trust),
[`claude_default_statusline_command`](contract.md#claude-account-settings),
[`claude_statusline_refresh_seconds`](contract.md#claude-usage-and-statusline),
[`terminal_input_enabled`](contract.md#terminal-input),
[`reuse_idle_tabs`](contract.md#choosing-a-space-and-a-pane),
[`min_free_disk_mb`](contract.md#disk-space-guard),
[`receipt_retention_days`, `receipt_unknown_horizon_days`](contract.md#receipt-retention)
and the [retention](operations.md#retention) keys. `native_sender_registration` is
accepted and ignored.

## Accounts

```sh
sessionsation account add --platform codex example
sessionsation account add --platform claude personal [--no-login]
sessionsation account add --platform claude --config-dir /home/me/.claude existing
sessionsation account add --platform gemini me
sessionsation account add --platform antigravity me
sessionsation account login claude:personal       # runs the provider login here
sessionsation account managed-home existing       # adopted Claude home -> managed home
sessionsation account list [--platform claude] [--json]
sessionsation account rename work Office          # display name; --clear resets
sessionsation account reorder claude:personal --position 0
sessionsation account logout claude:personal
sessionsation account live-steering claude:personal --enable|--disable
sessionsation account bypass-warning claude:personal --skip|--show
sessionsation account auth gemini:me [--mode google | --mode api-key --key-stdin < key.json]
```

Human commands accept `PROVIDER:NAME`, an unambiguous bare name or a display name.
The machine API takes qualified IDs only:

```sh
sessionsation api account add claude:personal [--config-dir DIR]
sessionsation api account list [--platform claude]
sessionsation api account login|refresh|logout|enable|disable codex:example
sessionsation api account rename codex:example --display-name Office [--expected-generation G]   # or --clear
sessionsation api account rename-backend codex:example [NEW] --expected-generation G [--request-id UUID]
sessionsation api account reorder codex:example --position N
sessionsation api account managed-home claude:NAME
sessionsation api account live-steering claude:NAME [--enable|--disable]
sessionsation api account bypass-warning claude:NAME [--skip|--show]
sessionsation api account auth gemini:NAME [--mode google|api-key [--key-stdin]]
sessionsation api account limits codex:example [--short-used 80 --weekly-used 90]
sessionsation api account usage codex:example --start UNIX --end UNIX --step 1800
sessionsation api usage [--account ID] [--cached]
```

Complete a Codex login with the returned URL and code as the user; never copy
credentials between homes. Limits are maximum **used** percentages (80 used is 20
remaining). Semantics: [accounts](contract.md#accounts), provider homes and login:
[Claude](contract.md#claude-code), [Gemini](contract.md#gemini-cli),
[Antigravity](contract.md#antigravity-cli).

Removal (local membership) and deletion (local data, conversations preserved):

```sh
sessionsation api account remove codex:example                     # preview
sessionsation api account remove codex:example --confirm codex:example --revision PREVIEW-REVISION
sessionsation api account restore codex:example
sessionsation api account delete codex:example                     # intent preview
sessionsation api account delete codex:example --confirm codex:example --request-id NEW-UUID
sessionsation api account deletions --request-id ORIGINAL-UUID
sessionsation api recover ORIGINAL-UUID
```

Repeat a request ID only to inspect it. See
[removal and deletion](contract.md#account-removal-and-deletion).

Reset credits and limit resets (**irreversible**; retry only with the same
`--request-id`):

```sh
sessionsation api account limits codex:work                 # usage.reset_credits
sessionsation api account reset-credit codex:work [--credit-id ID] [--request-id UUID]
sessionsation api account reset-credit claude:personal [--kind 5-hour|weekly|fable|full] [--credit-id GRANT] [--request-id UUID]
```

See [reset credits](contract.md#reset-credits).

## Terminal hosting

Run in the current terminal (also inside tmux or HERDR; a HERDR pane is registered
automatically), or headless in HERDR:

```sh
sessionsation run --account example [--name NAME] [--permissions yolo|default] [-- CODEX-OPTIONS]
herdr --session coding server                       # keep running in its own terminal
sessionsation run --account example --host herdr --herdr-session coding
herdr session attach coding
```

The launch JSON reports `running` only after matching the exact account home,
conversation and process; `launch_unconfirmed` means inspect before retrying.
Outside a managed host foreground launch works, but v1 lifecycle needs a discovered
tmux/HERDR attachment. `run -- --yolo` is the short alias for
`--dangerously-bypass-approvals-and-sandbox`. Detach a HERDR viewer with
**Ctrl+B, Q**; agents keep running. `herdr server stop` stops the host and its panes.
Clear unsent drafts before lifecycle operations.

## Sessions

```sh
sessionsation --version
sessionsation api version
sessionsation api capabilities
sessionsation api state [--archive-filter active|archived|all]
sessionsation api state --cached
sessionsation api state --watch --interval 2
sessionsation api session --session ID --expected-generation G
sessionsation api session resolve --session localica:LEAD [--expected-generation G]
sessionsation api session repair-identity
sessionsation api attachment --session ID --expected-generation G
sessionsation api observe --session ID --expected-generation G [--history [N]]
sessionsation api observations < batch.json
sessionsation api turn-target --session ID --expected-generation G
sessionsation api rename --session ID --expected-generation G --name NAME
sessionsation api launcher new --account codex:example [--hosted]
sessionsation api launcher resume --session ID --expected-generation G [--hosted]
sessionsation api launcher shell
```

Use backend session IDs and generations from state, never pane IDs or names. Any
`--session` accepts an id, `<space>:<name>` or a bare name. See
[identity and snapshots](contract.md#identity-and-snapshots) and
[passive readiness](contract.md#passive-readiness).

New and resumed sessions:

```sh
sessionsation api launch validate|start < launch-request.json
sessionsation api launch status --request-id UUID
sessionsation api launch list
sessionsation api resume-launch validate|start < resume-request.json
sessionsation api resume-launch status --request-id UUID
sessionsation api resume-launch list
sessionsation api resume --session ID --expected-generation G --herdr-session S [--space ID|LABEL | --new-space LABEL | --create-space] [--name NAME]
sessionsation run --account ACCOUNT [--name NAME] [--permissions yolo|default]
sessionsation resume SESSION --expected-generation G [--confirm-unfinished] [--name NAME] [--permissions yolo|default]
```

Request and receipt fields: [launch](contract.md#new-session-launch),
[resume](contract.md#saved-session-popup-resume), [inline](contract.md#inline-run-and-resume).
`--permissions` is accepted by every command and overrides the setting for the
processes that command starts.

Session model and one-pass launch (always pass an exact model id; Claude aliases are
refused):

```sh
sessionsation api session launch --space SPACE --name NAME --account PROVIDER:ACCOUNT \
    --model EXACT-MODEL-ID [--effort EFFORT] --brief-file FILE|- [--when-ready] [--wait SECONDS] \
    [--brief-delivery held|initial-prompt] [--create-space] [--directory DIR [--create-directory]] \
    [--permissions yolo|default] [--herdr-session H] [--herdr-socket PATH] [--request-id UUID] \
    [--deadline SECONDS] [--dry-run]
sessionsation api session launch-status --request-id UUID
sessionsation api session set-model --session S --expected-generation G --model EXACT-MODEL-ID \
    [--effort EFFORT] [--when-idle [--expires-at UNIX]] [--operation UUID] [--wait SECONDS]
sessionsation api session set-model-status --operation UUID      # or --session S
sessionsation api session set-model-cancel --operation UUID      # or api cancel-operation UUID
```

See [session model](contract.md#session-model-and-one-pass-launch).

## Lifecycle

```sh
sessionsation api stop --session ID --expected-generation G [--keep-pane] [--wait] [--disable-auto-start]
sessionsation api restart --session ID --expected-generation G
sessionsation api switch --session ID --expected-generation G --to PROVIDER:OTHER [--accept-risks]
sessionsation api relocate --session ID --expected-generation G --to /abs/dest [--move-files] [--dry-run]
sessionsation api auto-start --session ID --expected-generation G --mode disabled|start|start_prompt [--prompt TEXT]
sessionsation api auto-start --all --mode disabled
sessionsation api session compact --session ID --expected-generation G --when-idle [--expires-at UNIX] [--operation UUID]
sessionsation api session compact-status --operation UUID        # or --session ID
sessionsation api session compact-cancel --operation UUID
sessionsation api operations
sessionsation api recover OPERATION-ID
sessionsation api respond OPERATION-ID yes|no --sequence CURRENT-SEQUENCE
sessionsation api cancel-operation OPERATION-ID
sessionsation api abandon-operation --operation ID --confirm ID
```

Mutations return journaled operations; only a terminal phase confirms the result,
and an uncertain operation is inspected and recovered or abandoned, never re-sent.
`api auto-recover --enable|--disable` is the deprecated form of `auto-start`. See
[stop and restart](contract.md#stop-and-restart), [account switch](contract.md#account-switch),
[Auto Start](contract.md#auto-start), [compaction](contract.md#compaction-at-the-next-idle-point),
[relocation](contract.md#codex) and
[abandoning](contract.md#launch-acknowledgement-and-abandoning-uncertain-operations).

## Messaging

```sh
echo '{"api_version":1,"message_id":"UUID","session_id":"ID","expected_generation":"G",
       "mode":"deliver","text":"…","expires_at":UNIX}' | sessionsation api send
sessionsation api receipt MESSAGE-ID
sessionsation api cancel MESSAGE-ID
sessionsation api steer-target --session ID --expected-generation G
sessionsation api recovery-plan < recovery-target.json
sessionsation api recovery-reconcile MESSAGE-ID
sessionsation api checkpoints < request.json
sessionsation api answer --session ID --expected-generation G --request-id call_... [--answers JSON]
sessionsation api events [--after SEQ] [--wait SECONDS] [--session ID] [--kind KIND] [--limit N]
sessionsation api hooks [install [--account ID]]
sessionsation api turn-notify --session ID --expected-generation G --enable [--notify-session ID2]   # or --disable
sessionsation api turn-events [--after SEQ] [--wait SECONDS]
sessionsation api idle-notify --session ID --expected-generation G [--notify-session ID2]
```

Modes are the intents `deliver`, `when_idle`, `interrupt` (every provider), the
provider modes `submit`, `queue`, `steer`, and `resume`/`new` for recovery and
headless delivery. Supply a new UUID per message and repeat the identical request
only to read its receipt. `ok:true` means a receipt exists; read `result.state`. A
script or monitor can name itself with `"from_component":"resource-guard"` or
`SESSIONSATION_SENDER_COMPONENT=resource-guard`. See [messaging](contract.md#messaging),
[one send request](contract.md#one-send-request-for-every-provider),
[steering](contract.md#deliberate-in-turn-steering),
[Resume-and-submit](contract.md#explicit-resume-and-submit),
[checkpoints](contract.md#public-turn-checkpoints),
[questions](contract.md#pending-questions) and [events](contract.md#session-events).

Pending questions in state:

```sh
sessionsation api state | jq ".result.sessions[] | select(.pending_questions != []) | {id, generation, pending_questions}"
echo '{"q1":{"option_ids":["o2"]},"name":{"text":"zebra"}}' | \
  sessionsation api answer --session ID --expected-generation G --request-id call_...
```

### Hosted Claude sessions

```sh
sessionsation run --account claude:personal --hosted [--permission-mode plan] [--thread CONVERSATION-UUID]
sessionsation view SESSION-ID [--once]        # live log; t + Enter takes over, q quits
sessionsation take-over SESSION-ID            # stop the worker, then claude --resume here
sessionsation api hosted start --account claude:personal --directory /abs/dir [--name N]
sessionsation api hosted start --session ID --expected-generation G
sessionsation api hosted status|events --session ID
sessionsation api hosted stop --session ID --expected-generation G [--interrupt] [--timeout-seconds N]
sessionsation api hosted respond --session ID --request-id R --allow|--deny [--message TEXT]
```

Closing the viewer does not stop the worker. Priorities, receipts and permission
requests: [hosted sessions](contract.md#hosted-sessions).

## HERDR Spaces

```sh
sessionsation api spaces --session ID --expected-generation G [--herdr-session SERVER [--herdr-socket PATH]]
sessionsation api placements
sessionsation api move-space --session ID --expected-generation G (--space ID | --new-space LABEL | --create-space LABEL) --request-id UUID
sessionsation api session add-to-herdr --session ID --expected-generation G --herdr-session SERVER (--space ID | --new-space LABEL | --create-space) [--name NAME]
sessionsation api session set-space --session ID --expected-generation G (--space LABEL | --clear)
sessionsation api rename --session localica:LEAD --expected-generation G --name localica:opus-#1067
sessionsation run --account A --name localica:opus-#1067      # inside a pane of that Space
```

A move returns an `admitted` operation; poll `api operations` until `complete`, and
use `api recover` (never a new request) for `uncertain`. Name forms:

| `--name` | Typed Space | Result |
|---|---|---|
| `opus-#1067` | `--space wR` (localica) | `localica:opus-#1067` |
| `localica:opus-#1067` | none | Space resolved by label `localica` |
| `Localica:LEAD` | none | case-insensitive label match, stored as `localica:LEAD` |
| `localica:opus-#1067` | `--space w21` (sessionstuffs) | `SPACE_MISMATCH` |
| `newspace:LEAD` | none | `SPACE_NOT_FOUND` |
| `newspace:LEAD --create-space` | none | Space `newspace` created |
| `localica:LEAD --create-space` | none | `WORKSPACE_NAME_EXISTS` |
| `a:b:c`, `:lead`, `localica:`, `localica: LEAD` | | `INVALID_SESSION_NAME` |
| `LEAD` (rename, `lead` exists in the same Space) | | `SESSION_NAME_EXISTS` |
| `LEAD` (rename, `LEAD` exists only in another Space) | | ok |
| `localica:x` (rename of an unscoped session) | | `SPACE_MISMATCH` |

See [placement](contract.md#herdr-placement) and
[Space-scoped names](contract.md#space-scoped-session-names). Idle tabs are reused
for starts and resumes; `sessionsation config set reuse_idle_tabs false` always opens
a new tab.

## Archives

```sh
sessionsation api archives
sessionsation api archive --session ID --expected-generation G --request-id UUID --confirm ID [--wait]
sessionsation api restore-session --session ID --expected-generation G --request-id UUID --confirm ID [--wait]
sessionsation api delete-session --session ID --expected-generation G --request-id UUID --confirm ID [--wait]
```

Delete is permanent. Do not edit archive manifests or move managed history between
accounts by hand. See [conversation archives](contract.md#conversation-archives).

## Shared Claude state

```sh
sessionsation shared status
sessionsation shared enable
```

See [shared Claude state](contract.md#shared-claude-state).

## Data-root maintenance

```sh
sessionsation api disk-space
sessionsation layout status
sessionsation retention run [--dry-run]
```

Procedures: [layout](operations.md#data-root-layout), [retention](operations.md#retention),
[root alias](operations.md#root-alias). Reference:
[data root](contract.md#data-root).
