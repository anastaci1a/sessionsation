# Architecture

## Backend contract

One stable session catalog and one provider-qualified account list.
Backend session IDs are independent of provider conversation IDs and runtime
attachments. Runtime generations reject stale mutations. Renames preserve backend
identity; live HERDR labels come from the host, with a retained catalog cache for
unavailable/stopped sessions.

[The versioned CLI contract](contract.md) owns client-facing behavior. The durable
session catalog is a separate atomic JSON store; it does not repurpose provider
conversation IDs or pane IDs. Account snapshots project the existing Codex owner's
registry into one provider-qualified list. Local removal markers retain recoverable
membership without modifying the original enrollment or adding a credential writer.
Actual local account deletion uses a durable CLI worker and the existing owner
for reservation and final deregistration. Account/queue/lease locks exclude new
launches and transfers; active processes or loaded conversations block deletion.
After confirmation, private history storage retains rollouts, native databases
and page fingerprints outside the credential home. Later explicit-account resume
uses the existing transfer worker and verifies preserved history, without creating
a credential owner in that storage.

Conversation archives use sessionsation-managed gzip bundles and verified native
history proofs outside account homes. Durable workers retain operation locks and
stale-generation checks; Restore installs a credential-free saved-history home.
Native removal occurs only after the archive export verifies. Permanent session
deletion uses the existing native deletion path and purges only conversation data.

Operation discovery reads journals through both their JSON entries and existing,
stable worker-lock/log entries. Some filesystems can omit a JSON directory entry
while its contents are atomically replaced; the stable entry supplies the path
for a direct read. Lifecycle reservations and operation lists share this discovery
helper, so a transient directory omission does not erase an admitted operation.
No journal migration or replacement supervisor is required.

Provider adapters own enrollment, capabilities, usage, conversation history and
delivery receipts. Terminal hosts own attachment and process placement. HERDR and
tmux must launch the same account-specific Codex command. Viewer detachment must
never imply stopping an agent. HERDR bindings retain the agent’s own socket locator;
legacy live bindings read that locator from the bound PID after checking its start
identity. Discovery and relaunch therefore do not borrow the observer’s HERDR
environment. Explicit sockets are sent without HERDR’s `--session` flag, which
otherwise overrides them using the caller’s XDG directory in version 0.9.5.
A pending launch/resume receipt supplies the locator before its pane has a process
binding. A partial host observation retains a still-live attachment and its
generation. Only positive exit/PID-reuse evidence clears it; no runtime mutation
is enabled until its host and process are positively observed again.

HERDR Space placement is a presentation operation under the existing lifecycle
and messaging locks. After validation and durable admission, a detached worker owns
the move through receipt finalization; UI/CLI requester exit does not terminate it.
The worker revalidates the saved target before dispatch and never replays a
previous dispatch intent. Durable `space-moves/` intent records retain original process,
terminal and socket identity. Completed provenance updates canonical pane bindings
and catalog attachment revisions without changing provider-runtime generation or
historical receipts. New launch/resume requests delimit placement provenance, so
reused pane IDs cannot apply an earlier terminal’s moves. Read-only messaging
inventory includes both managed tmux and HERDR bindings, retaining deletion/owner
gates; a newly created HERDR conversation needs no prior transfer to be observed. Lost replies require read-only host reconciliation; no
competing supervisor, credential writer or automatic host replay is introduced.

## Reuse

The MIT-licensed backend and regression fixtures are imported from
`anastaci1a/codex-switcher`; [NOTICE](../NOTICE) records the pinned source revision
and [LICENSE](../LICENSE) retains attribution. Its UI is excluded. Keep one
supervisor and credential writer per account home.

## Separate UI

`sessionsation-ui` is a separate repository consuming the public versioned backend
contract. It provides a HERDR popup for account/usage management, selected-session actions
and new/resume launchers; its older shell tools are separate compatibility entrypoints. Account,
credential and lifecycle logic stays in sessionsation. Verify the backend first.
Cross-provider conversation conversion is outside this version.

## Claude preparation

Claude runtime support is not implemented. The v1 capability list reserves `claude` and reports `SETUP_REQUIRED` for
runtime behavior. Codex-only operations reject other provider IDs explicitly.

Future implementation must verify the installed binary and isolated config homes
against Claude's [CLI reference](https://code.claude.com/docs/en/cli-reference),
[authentication guidance](https://code.claude.com/docs/en/authentication) and
[environment variables](https://code.claude.com/docs/en/env-vars). Documented flags
are not evidence for safe account transfer, usage/reset reporting or live queues.
Those capabilities remain false until verified; no Codex-to-Claude history
conversion or credential sharing is implemented.
