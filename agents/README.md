# Maintaining sessionsation

## Workflow

- Verify the working tree and remote before editing. Use the existing
  `anastaci1a/sessionsation` repository; never create a replacement.
- Issues own tasks and acceptance criteria; Git and releases own history.
  Push each small, completed, appropriately tested commit immediately.
- Use local Git identity Ana Jahnel
  <48846277+anastaci1a@users.noreply.github.com>.
- Preserve the original installation, account data and live tmux sessions.
  Ask Ana before deployment, live migration, credential/access changes or spending.
- Test with isolated homes and fake servers. Live tests need separately authorized
  accounts. Never inspect or use the invoking agent's credentials for tests.
- Never commit credentials, private databases, account state or raw protocol traces.

## Implementation and verification

- Separate provider behavior, persistent catalog identity and terminal hosting.
  Reuse account/authentication logic; keep one supervisor/credential writer per home.
- Keep operation locks, generation checks and uncertain-delivery protection.
  Timeouts are not quiescence. Never automatically replay uncertain sends.
- Keep tmux supported; never silently migrate live sessions. Restart must retain
  the selected account and exact conversation, without default-account fallback.
- Rust changes: `cargo fmt --check`,
  `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`.
  Add safety-boundary regression tests and commit lockfile changes.
- `scripts/check.sh` uses `target-review/` by default. Do not overwrite an artifact
  currently selected by the UI/another consumer. Artifact promotion is separate
  from building/testing and requires the applicable deployment approval.
- Documentation changes: `python3 scripts/check_docs.py`; validate examples
  against help. Documentation-only work does not require the Rust suite.
- Distinguish fixture evidence from live evidence. Launching alone is not complete
  HERDR integration. See [operational acceptance](../docs/operations.md).

## Documentation maintenance

- Keep README approachable: purpose, prerequisites, quick setup, commands and links.
  Clearly distinguish implemented behavior from plans.
- `docs/README.md` indexes behavior, architecture, setup/CLI and operations.
  Give each fact one authoritative home; consolidate superseded text.
- Update relevant documentation with each behavior change. Split files for distinct
  reader needs, not features or work sessions. Do not accumulate session logs.
