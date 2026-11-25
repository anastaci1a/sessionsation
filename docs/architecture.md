# Architecture

## Backend

One stable session catalog and one provider-qualified account list. Backend session
IDs are independent of provider conversation IDs and runtime attachments; runtime
generations reject stale mutations. [The contract](contract.md) owns client-facing
behaviour. The durable session catalog is an atomic JSON store under
`registry/session-catalog/` (with `identity.json` and `spaces.json` sidecars that
heal a catalog rewritten by an older release); it never repurposes provider
conversation IDs or pane IDs. Account snapshots project the Codex owner's registry
into one provider-qualified list. Local removal markers keep recoverable membership
without modifying the original enrollment or adding a credential writer.

Long-running work runs in durable, detached workers (own process group) that journal
each phase under `operations/` before doing it, hold operation locks and recheck
generations; a requester that exits only detaches. Nothing is replayed automatically:
an outcome that cannot be proven is `uncertain` and settled by explicit recover or
abandon. Operation discovery reads journals through both their JSON entries and
stable worker-lock/log entries, because some file systems can briefly omit a JSON
entry while it is atomically replaced; lifecycle reservations and operation lists
share this helper. `record.rs` is the one record engine (load, save, list, locks, worker
lock) of the account-deletion, -rename, reset-credit, auto-start, idle-notice, compaction
and model-change journals. The supervisor (`daemon serve`) is the sole account and credential
writer per root; it polls usage, refreshes the cached state snapshot, runs Auto
Start, compaction and model-change releases, event ingest, retention and the
re-dispatch of provably unsent deliveries, and starts delivery workers as its own
children. Its periodic passes serve unchanged journals and the catalog from memory
keyed by file identity.

Account deletion uses a durable CLI worker and the existing owner for reservation and
final deregistration; account/queue/lease locks exclude new launches and transfers,
and active processes or loaded conversations block deletion. Preserved history
storage keeps rollouts, native databases and page fingerprints outside the
credential home; a later explicit-account resume uses the transfer worker without
creating a credential owner there. Conversation archives use managed gzip bundles
(Codex) or moved exact-id native files (Claude, Gemini, Antigravity) with verified
digests outside account homes; native removal happens only after the export
verifies.

## Terminal hosts

Provider adapters own enrollment, capabilities, usage, conversation history and
delivery receipts; terminal hosts own attachment and process placement. HERDR and tmux
launch the same account-specific command, and viewer detachment never implies
stopping an agent. HERDR bindings keep the agent's own socket locator; legacy live
bindings read it from the bound PID after checking its start identity, so discovery
and relaunch never borrow the observer's HERDR environment. Explicit sockets are sent
without HERDR's `--session` flag, which would otherwise override them with the
caller's XDG directory (HERDR 0.9.5). A pending launch/resume receipt supplies the
locator before its pane has a process binding. A partial host observation keeps a
still-live attachment and its generation; only positive exit/PID-reuse evidence
clears it, and no runtime mutation is enabled until host and process are observed
again.

HERDR Space placement is a presentation operation under the lifecycle and messaging
locks. A detached worker owns the move through receipt finalization and revalidates
the saved target before dispatch; durable `space-moves/` records keep the original
process, terminal and socket identity. Completed provenance updates canonical pane
bindings and catalog attachment revisions without changing the provider-runtime
generation or historical receipts, and new launch/resume requests delimit placement
provenance so reused pane IDs cannot apply an earlier terminal's moves. Read-only
messaging inventory includes managed tmux and HERDR bindings with their deletion and
owner gates.

## Providers

`src/provider/` is the provider boundary: `ProviderId` (`codex`, `claude`, `gemini`,
`antigravity`), provider-qualified `AccountRef` parsing and an explicit
`ProviderCapabilities` table (supported flag plus reason per operation). Callers
dispatch on `ProviderId`, never on strings; owned roles and their guard counters are
described in [contributor guidance](../agents/README.md#owned-roles-and-the-guard).

- Codex behaviour lives in the supervisor, discovery and lifecycle modules, talking to
  each account's app-server over its control socket.
- `provider/claude.rs` owns Claude enrollment, environment, the authentication probe,
  launch/resume plans, discovery, live identity and usage; `provider/claude/hosted.rs`
  the hosted (stream-json) worker, which holds `worker.lock`, owns the provider's stdin
  and serves a private control socket, so a supervisor restart re-adopts it;
  `provider/claude/peer.rs` and `native_peer.rs` the cross-session messaging endpoint
  and wire format; `messaging/claude_steer.rs` admission, the durable records in
  `messaging/claude-steer/`, the per-message workers and the receipts of every native
  live path; `claude_idle.rs` native agent delivery and idle notices.
- `provider/native.rs` dispatches the shared native-session model (account home,
  transcript discovery, live identity, launch, headless delivery) between Claude,
  Gemini and Antigravity; `provider/gemini.rs` and `provider/antigravity.rs` (with
  their directories) hold the provider specifics.
- `provider/activity.rs` and `native_activity.rs` derive `live_activity`;
  `provider/send.rs`, `stop_shape.rs`, `compact.rs`, `events.rs`, `hooks.rs`,
  `usage_meter.rs`, `session_meters.rs` and `conversation.rs` give one shape across
  providers for send, stop, compaction, events, hooks, usage and observe.

Account records carry `provider` and, for adopted Claude homes, `config_home`.
Registries written before providers existed get Codex once through
`store::migrate_legacy_codex_provider` (event `account.provider_migrated_legacy_codex`);
a provider is never inferred from a name. Account names are unique per provider: the
registry key is the bare name for Codex and `<provider>:<name>` otherwise (event
`account.key_migrated_per_provider`).

Claude, Gemini and Antigravity sessions are bound to their account home; an account
switch moves a conversation's exact files between homes (or nothing, with shared
Claude state under `<root>/shared/claude/`) and resumes it. There is no history
conversion or credential sharing. Gemini has no auth status, live registry, rename or
usage interface, so authentication is file presence, liveness is /proc command line
plus start time and labels are Sessionsation-only; it is verified against fakes only
(see `tests/gemini.rs`). Claude behaviour was checked against Claude's
[CLI reference](https://code.claude.com/docs/en/cli-reference),
[authentication guidance](https://code.claude.com/docs/en/authentication) and
[environment variables](https://code.claude.com/docs/en/env-vars).

## Reuse and separate UI

The MIT-licensed backend and regression fixtures are imported from
`anastaci1a/codex-switcher`; [NOTICE](../NOTICE) records the pinned source revision
and [LICENSE](../LICENSE) keeps attribution. Its UI is excluded. `sessionsation-ui`
is a separate repository consuming the public versioned contract: a HERDR popup for
account/usage management, session actions and new/resume launchers. Account,
credential and lifecycle logic stays in sessionsation.
