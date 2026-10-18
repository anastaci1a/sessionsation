# Architecture

## Intended backend contract

One uniquely named session catalog and one provider-qualified account list.
Backend session IDs are independent of provider conversation IDs and runtime
attachments. Runtime generations reject stale mutations. Atomic rename preserves
backend identity.

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
