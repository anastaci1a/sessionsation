# Contract changelog

One entry per contract version: what changed and where the current behaviour is
described in the [contract](contract.md). Revision N is contract `1.N.0`. Releases
and package history are in Git.

## Upgrade notes

- Negotiate by `contract_version` and capabilities, never by package version; log the
  executable SHA-256 from `api version`. A client upgrade is not an owner handoff:
  keep the sole supervisor and existing receipt databases.
- 1.64.0 turned some refusals into success: a live `api send` option the target's
  provider does not use is accepted, left out and named in `ignored_options` (before:
  `PROVIDER_DELIVERY_UNSUPPORTED`, `DELIVERY_UNAVAILABLE`,
  `UNSUPPORTED_PROVIDER_OPERATION` or `INVALID_REQUEST`). Detect providers with
  `capabilities.send_request.providers` instead.
- 1.54.0 activates layout version 3; binaries before 1.54.0 refuse it (`root rollback`
  is gone in 1.68.0). 1.49.0 adds journals older releases ignore; drain
  them before rolling back ([operations](operations.md#rolling-back-a-release)).

## Versions

- **1.69.1** Idle statusline replays no longer refill the meter after a redeem.
- **1.69.0** Removed: hosted Claude (`api hosted`, `--hosted`, `view`, `take-over`), `api turn-notify`,
  `turn-events`, `idle-notify` (use `api events`), `api account rename-backend` and their capability keys
  and the row `backend_rename`. Check `operations/account-renames` before the cutover
  (`scripts/preflight-account-renames.sh`).
- **1.68.3** Fresh OAuth usage outranks stale statusline.
- **1.68.2** `api stop --interrupt`. [Stop](contract.md#stop-and-restart).
- **1.68.1** A launch whose starter was killed before its worker ran is dispatched once
  more; a running recorded `worker` or a status probe never fails a launch; detached
  workers log stderr. [Launch](contract.md#new-session-launch).
- **1.68.0** Removed `root *` (capability `root_alias`), `layout migrate|rollback`, `shared migrate`,
  `account auth|show|history`, `doctor`, `simulate` and the hidden `message`, `ready`, `session`,
  `import`, `pause`, `thresholds`, `auto-switch`, `auto-start` commands; the daemon answers
  `automatic/*`, `auto-start/*`, `session/run|import|pause|resume` with unknown control method. `api auto-recover` and the
  external-job verbs (`move`, `restart`, `terminate`, ...) stay. Gemini CLI is dropped
  (`providers[gemini]` unsupported; accounts stay deletable). A dropped config key
  (`gemini_binary`, `native_sender_registration`) is ignored, not refused; `active_poll_seconds`
  is unused; `layout status` loses `in_use_now`. [Root alias](contract.md#root-alias).
- **1.67.1** Workers inherit no client lock fds; a repeated `session launch` re-admits;
  sends skip hourly retention.
- **1.67.0** A refused check before Enter withdraws the typed text. Launches use the
  stable `current/` executable; `/mnt/user` `PATH` entries map to `/mnt/disk1`; an
  errored Codex thread fails compaction after 30 s, not 180 s. Meter windows gain
  `fresh`, `age_seconds`; the meter `all_windows_fresh`.
- **1.66.3** The supervisor no longer waits for `codex --version` to
  serve. [Operations](operations.md#safe-service-maintenance).
- **1.66.2** `resume-launch` starts a Claude session without a transcript again under its
  id (`resumed_from:"no_history"`); such a session archives with `nothing_to_archive`;
  stop of an archived session is `already_stopped` with `archived:true`; a stop waits up
  to 3 s for the pane's idle shell before `PANE_BUSY`.
  [Resume](contract.md#saved-session-popup-resume), [stop](contract.md#stop-and-restart).
- **1.66.1** Receipt reads share the ledger lock and no longer wait for each other;
  writes stay exclusive; the receipts schema is stamped once (`user_version`).
  [Contention](contract.md#admission-and-contention).
- **1.66.0** Admitted launches stay listed (`availability:"launching"`) and rows carry
  `availability_reason`; stop and archive of an unknown session complete with
  `not_found:true`; `resume-launch` for Claude and Antigravity.
  [State](contract.md#identity-and-snapshots), [stop](contract.md#stop-and-restart),
  [resume](contract.md#saved-session-popup-resume).
- **1.65.0** (meter cycles) Usage windows gain `cycle`, `cycle_started_at`, `cycle_reason`,
  `starts_on_first_use`; meters `window_reasons` and (Claude) `cedar_ember`; `reset_credits`
  `fresh`, `age_seconds`; a redeem starts a cycle. [Usage](contract.md#limits-plan-and-usage).
- **1.64.0** One `api send` body for every provider (intents `deliver`, `when_idle`,
  `interrupt`; `ignored_options`; one `delivery` block), supervisor-owned delivery
  workers with one safe re-dispatch; `api events`, `api hooks [install]`, Claude
  `subagents_running` from hooks; compaction for Claude terminal sessions, Gemini and
  Antigravity refuse with `UNSUPPORTED_PROVIDER_OPERATION` (was `COMPACT_UNSUPPORTED`).
  [Send](contract.md#one-send-request-for-every-provider),
  [events](contract.md#session-events), [compaction](contract.md#compaction-at-the-next-idle-point).
- **1.63.0** `api observe` for every provider with a `conversation` block; one stop
  result shape; early provider exit fails a launch (`PROVIDER_EXITED`,
  `TERMINAL_PROVIDER_EXITED`, `TERMINAL_PANE_IS_SHELL`); archive of a session without
  history (`nothing_to_archive`). [Observe](contract.md#observe-for-every-provider),
  [stop](contract.md#stop-and-restart), [early exit](contract.md#early-provider-exit).
- **1.62.1** Codex meters say why they are stale and read on demand; the supervisor
  rebuilds the active snapshot every 20 s (`snapshot.fresh` up to 30 s, was 15).
  [Usage](contract.md#limits-plan-and-usage).
- **1.62.0** `usage.meter` for every account (Claude OAuth read without a session),
  `api usage` for every provider; session meters `context_pct`, `subagents_running`,
  `background_tasks`; `SESSIONSATION_SESSION_ID` in every provider child.
  [Usage](contract.md#limits-plan-and-usage), [meters](contract.md#session-meters-and-session-id).
- **1.61.0** `session launch --brief-delivery initial-prompt` accepted for Codex;
  `api stop --wait`. [Model](contract.md#session-model-and-one-pass-launch),
  [stop](contract.md#stop-and-restart).
- **1.60.0** Claude limit resets through `api account reset-credit` (OAuth owner,
  token renewal as Claude Code does). [Reset credits](contract.md#reset-credits).
- **1.59.0** Claude `api stop` (SIGTERM to the verified process); `api stop
  --disable-auto-start`. [Stop](contract.md#stop-and-restart).
- **1.58.0** Claude `set-model` admission waits for the first hook record
  (`hook_not_recorded`); Codex `facts.native_error` and `input_delivery.health`.
  [Model](contract.md#session-model-and-one-pass-launch), [receipts](contract.md#receipt-evidence).
- **1.57.x** Performance only.
- **1.56.0** `session resolve` fills cached facts; statusline replays never lower
  usage; detached workers are reaped.
  [Claude usage](contract.md#claude-usage-and-statusline).
- **1.55.0** Archive workers wait for the account lock; `--wait` on archive, restore,
  delete; idempotent `api stop` (`already_stopped`).
  [Archives](contract.md#conversation-archives), [stop](contract.md#stop-and-restart).
- **1.54.0** Root alias and re-key (layout version 3).
  [Root alias](contract.md#root-alias).
- **1.53.0** Codex rate-limit reset credits (`api account reset-credit`,
  `usage.reset_credits`). [Reset credits](contract.md#reset-credits).
- **1.52.0** Idle-tab reuse for starts and resumes (`reuse_idle_tabs`, `pane_reused`);
  delivered native journal records retire as terminal.
  [Pane reuse](contract.md#choosing-a-space-and-a-pane), [retention](contract.md#receipt-retention).
- **1.51.0** Outside-session senders shown by the bare component, prefixed only on a
  collision. **1.50.0** `from_component` and `SESSIONSATION_SENDER_COMPONENT`.
  [Component names](contract.md#component-sender-names).
- **1.49.1** Launches wait up to 120 s for account operations; pre-start refusals end
  `failed`.
- **1.49.0** Session `model`, `api session set-model`, one-pass `api session launch`;
  Claude launch models.
- **1.48.1** No steering while a compaction is pending (`target_compacting`).
- **1.48.0** Space-scoped session names (`space`, `canonical`, `--create-space`,
  `session set-space`, `session resolve`).
- **1.47.1** Compaction completion read from the full view of new turns; 180 s
  no-progress bound. **1.47.0** `api session compact --when-idle` for Codex with an
  input hold.
- **1.46.8** Auto Start `already_running` result. **1.46.7** `accounts[].plan`.
- **1.46.5** Codex steer and cancel wait for the ledger; `if_busy:"queue"` on submit.
  **1.46.4** Codex messaging waits for the ledger lease and releases it during
  app-server I/O.
- **1.46.3** Codex callers detected by `CODEX_THREAD_ID`; every native frame carries the
  `cross-session-message` envelope.
- **1.46.2** Launch acknowledgement wait 120 s (`LAUNCH_RUN_NEVER_STARTED`); abandon an
  uncertain new launch.
- **1.46.1** Statusline `refreshInterval`; `skills` no longer shared. **1.46.0** `agents`
  and `commands` shared with merge on migration.
- **1.45.2** Codex accepts Claude send options; Codex usage clock fixes.
  [Usage](contract.md#limits-plan-and-usage).
- **1.45.0** Auto Start prompt delivery read back from its receipt, native first for
  Claude, array-start trigger; Space names resolved by label; `api version.commit`;
  opt-in `await_reply`. [Auto Start](contract.md#auto-start),
  [Spaces](contract.md#choosing-a-space-and-a-pane).
- **1.44.0** Auto Start means host boot or classified crash; an explicit stop keeps
  the mode. [Auto Start](contract.md#auto-start).
- **1.43.0** Auto Start modes `disabled|start|start_prompt`; `api auto-recover`
  deprecated. [Auto Start](contract.md#auto-start).
- **1.42.0** `live_activity` for Claude, Gemini and Antigravity; `api usage`; uncertain
  operations named in `pending_operations`.
  [Live activity](contract.md#live-activity-and-pending-operations).
- **1.41.0** Caller origin detection; terminal typing withdraws orphans
  (`refusal_layout`); disk-space guard. [Delivery](contract.md#claude-input-delivery),
  [disk space](contract.md#disk-space-guard).
- **1.40.1** Cancelled stops release their gate; unsent `pending_local` receipts settle.
  [Stop](contract.md#stop-and-restart).
- **1.40.0** Native peer delivery as the agent path, `origin`.
  [Peer messages](contract.md#native-peer-messages).
- **1.39.0** Shared Claude conversation state (`shared` commands).
  [Shared state](contract.md#shared-claude-state).
- **1.38.0** Codex launch `model`/`reasoning_effort`.
- **1.37.0** `live_activity`, `pending_operations`, failed turns accept input, stop
  expiry, native send checks. [Messaging](contract.md#messaging).
- **1.36.0** `api stop` closes the HERDR pane (`--keep-pane`).
  [Stop](contract.md#stop-and-restart).
- **1.35.1** Observed inline operations end `complete`.
  [Inline](contract.md#inline-run-and-resume).
- **1.35.0** Session auto-recover (now Auto Start); switching a stopped session only
  re-homes it. [Account switch](contract.md#account-switch).
- **1.34.0** Claude account switch. [Account switch](contract.md#account-switch).
- **1.33.1** Inline journal settling. **1.33.0** Long terminal messages verified on a
  scrolling input box (Claude and agy 8192 bytes). [Terminal input](contract.md#terminal-input).
- **1.32.0** Named, journaled inline `run`/`resume`. [Inline](contract.md#inline-run-and-resume).
- **1.31.1** Deletion after a home move scans each home once. **1.31.0** Resolved-path
  home comparisons; `resume --confirm-unfinished`. [Layout](contract.md#layout-and-retention).
- **1.30.0** Per-provider account names.
  [Names](contract.md#names).
- **1.29.0** Data-root layout version 2 and retention.
  [Layout](contract.md#layout-and-retention).
- **1.28.0** `api abandon-operation`.
  [Abandon](contract.md#launch-acknowledgement-and-abandoning-uncertain-operations).
- **1.27.0** Claude default statusline. [Settings](contract.md#claude-account-settings).
- **1.26.0** Native `last_accessed`.
- **1.25.1** Codex 0.166.0 native profile. **1.25.0** Antigravity folder trust, stop,
  archive and delete. [Antigravity](contract.md#antigravity-cli).
- **1.24.0** Semantic `contract_version`. [Versioning](contract.md#contract-versioning-semver).
- **Revision 24** Antigravity provider. [Antigravity](contract.md#antigravity-cli).
- **Revision 23** Pending questions and `api answer`. [Questions](contract.md#pending-questions).
- **Revision 22** Claude bypass-permissions warning.
- **Revision 20** `--new-space LABEL`; Claude folder pre-trust.
  [Spaces](contract.md#choosing-a-space-and-a-pane), [trust](contract.md#folder-pre-trust).
- **Revision 19** Launch permissions. [Permissions](contract.md#launch-permissions).
- **Revision 18** Receipt retention; definite refusal for approval-blocked sends;
  capability reason codes; Gemini auth failures; resume in the saved directory;
  terminal draft re-check. [Retention](contract.md#receipt-retention).
- **Revision 17** Claude and Gemini archive/restore/delete; terminal input; native
  `active_since`; `PEER_SOCKET_UNLINKED`. [Archives](contract.md#conversation-archives),
  [terminal input](contract.md#terminal-input).
- **Revision 16** Claude live peer messages.
  [Delivery](contract.md#claude-input-delivery).
- **Revisions 13-15** Gemini provider; account log out, cached state, order; steer replies.
- **Revision 12** Session identity, continuations, Add to HERDR.
- **Revision 11** Claude removal/deletion, login plans, headless delivery, display names.
- **Revision 10** Claude provider and the per-operation capability table.
  [Claude](contract.md#claude-code).
- **Revisions 3-9** Resume-and-submit, exact-item idle start, in-turn steering, HERDR-owned
  names, turn checkpoints, `api turn-target`, batch observations.
  [Recovery](contract.md#explicit-resume-and-submit).
