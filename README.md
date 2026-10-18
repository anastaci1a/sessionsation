# sessionsation

A CLI-first account and session manager for Codex and Claude, with provider behavior
separate from terminal hosting. The backend will reuse
[Codex Switcher](https://github.com/anastaci1a/codex-switcher).

**Status:** repository foundations only; no sessionsation runtime is implemented yet.
The original switcher installation and live sessions remain independent.

The initial target is Linux (including Unraid), Codex CLI, Rust, and tmux or HERDR.
Claude and a separate `sessionsation-ui` consumer follow verified Codex integration.
These are plans, not current capabilities.

For now, validate documentation with `python3 scripts/check_docs.py`.

See [documentation](docs/README.md) and [contributor workflow](agents/README.md).
