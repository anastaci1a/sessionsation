# Architecture

## Backend contract

One uniquely named session catalog and one provider-qualified account list.
Backend session IDs are independent of provider conversation IDs and runtime
attachments. Runtime generations reject stale mutations. Atomic rename preserves
backend identity.

[The versioned CLI contract](contract.md) owns client-facing behavior. The durable
session catalog is a separate atomic JSON store; it does not repurpose provider
conversation IDs or pane IDs. Account snapshots project the existing Codex owner's
registry into one provider-qualified list. Local removal markers retain recoverable
membership without modifying the original enrollment or adding a credential writer.

Provider adapters own enrollment, capabilities, usage, conversation history and
delivery receipts. Terminal hosts own attachment and process placement. HERDR and
tmux must launch the same account-specific Codex command. Viewer detachment must
never imply stopping an agent.

## Reuse

The MIT-licensed backend and regression fixtures are imported from
`anastaci1a/codex-switcher`; [NOTICE](../NOTICE) records the pinned source revision
and [LICENSE](../LICENSE) retains attribution. Its UI is excluded. Keep one
supervisor and credential writer per account home.

## Separate UI

`sessionsation-ui` is a separate repository consuming the public versioned backend
contract. It may offer a thin HERDR plugin for account/usage management,
selected-session switching, and new/resume/plain-shell launchers. Account,
credential and lifecycle logic stays in sessionsation. Verify the backend first.
Cross-provider conversation conversion is outside this version.

## Claude preparation

Ana has requested preparation only because no Claude installation/account setup is
present. The v1 capability list reserves `claude` and reports `SETUP_REQUIRED` for
runtime behavior. Codex-only operations reject other provider IDs explicitly.

Future implementation must verify the installed binary and isolated config homes
against Claude's [CLI reference](https://code.claude.com/docs/en/cli-reference),
[authentication guidance](https://code.claude.com/docs/en/authentication) and
[environment variables](https://code.claude.com/docs/en/env-vars). Documented flags
are not evidence for safe account transfer, usage/reset reporting or live queues.
Those capabilities remain false until verified; no Codex-to-Claude history
conversion or credential sharing is implemented.
