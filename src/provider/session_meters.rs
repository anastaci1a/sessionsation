//! Session meters on every session row (contract 1.62.0, SM3): `context_pct`,
//! `subagents_running` and `background_tasks`.
//!
//! Every row of every provider has the same keys. Each value comes with `<field>_source`,
//! `<field>_observed_at` (Unix seconds) and `<field>_reason`; a value nobody reported is
//! `null` with the reason, never a guess:
//!
//! - Claude `context_pct`: Claude Code's status-line payload (`context_window.used_percentage`,
//!   the share of the model's context window the last request used), recorded per
//!   conversation by Sessionsation's status-line hook ([`crate::session_model::HookRecord`]).
//!   Source `statusline`. One small file read per row, so `state --cached` shows it current.
//! - Codex `context_pct`: the newest `token_count` event of a running conversation's rollout
//!   (`info.last_token_usage.total_tokens` of `info.model_context_window`), read from a
//!   bounded tail by a full `api state` (cached rows keep that reading and its time). Source
//!   `rollout`. Codex's own footer subtracts a fixed 12,000-token baseline first; this is the
//!   raw share.
//! - Gemini `context_pct`: the footer is screen state only; reading it safely needs the
//!   typing engine's screen profiles, so it is `null` with the reason until then (it will be
//!   reported with source `screen`).
//! - Claude `subagents_running` (contract 1.64.0): the subagents started and not yet stopped in
//!   this process, counted from the `SubagentStart`/`SubagentStop` hook events Sessionsation
//!   installs ([`crate::provider::events::Tasks`]). Source `hook_events`. Counted only when the
//!   tracker saw this process start (`SessionStart` at or after the row's `active_since`), so a
//!   session launched before the hooks were installed reports `null` with the reason.
//! - Claude `background_tasks`: Claude Code reports a background task's start
//!   (`PostToolUse` with `backgroundTaskId`, journaled as `background_task_started`) but no hook
//!   when it finishes, so no running count is known: `null` with the reason.
//! - Codex: subagents and background terminals are reported only per loaded thread over its
//!   app-server, which state rows do not read; Gemini and Antigravity report none: `null`
//!   with the reason.
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub const FIELDS: [&str; 3] = ["context_pct", "subagents_running", "background_tasks"];

pub const CLAUDE_NO_CONTEXT_YET: &str =
    "no status-line payload with a context reading has been recorded for this conversation yet";
pub const CODEX_NOT_RUNNING: &str =
    "read from the rollout of a running conversation by a full api state";
pub const CODEX_NO_TOKEN_COUNT: &str =
    "the rollout tail has no token_count event with a context window yet";
pub const GEMINI_CONTEXT: &str = "the Gemini context share is shown only in its terminal footer; screen reading lands with the shared typing engine";
pub const ANTIGRAVITY_CONTEXT: &str = "the Antigravity CLI reports no context usage";
pub const CLAUDE_NOT_RUNNING: &str = "the session is not running";
pub const CLAUDE_NO_HOOK_EVENTS: &str = "no SessionStart hook event from this session's process has been recorded (the hooks are installed at launch: a session started before then reports none until it is resumed)";
pub const CLAUDE_BACKGROUND: &str = "Claude Code reports a background task's start (PostToolUse with backgroundTaskId, journaled as background_task_started) but no hook when it finishes, so the running count is not known";
pub const CODEX_TASKS: &str = "Codex reports subagents and background terminals only per loaded thread over its app-server; state rows do not read them yet";
pub const NATIVE_TASKS: &str = "the provider reports no subagent or background task counts";

/// One field: value, source, observation time, reason.
fn set(
    row: &mut Value,
    field: &str,
    value: Value,
    source: Option<&str>,
    at: Option<i64>,
    reason: Option<&str>,
) {
    let known = !value.is_null();
    row[field] = value;
    row[format!("{field}_source")] = json!(source.filter(|_| known));
    row[format!("{field}_observed_at")] = json!(at.filter(|_| known));
    row[format!("{field}_reason")] = json!(reason.filter(|_| !known));
}
fn unknown(row: &mut Value, field: &str, reason: &str) {
    set(row, field, Value::Null, None, None, Some(reason));
}

/// A Codex rollout's context reading: `(pct, tokens, window, observed_at)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Context {
    pub pct: f64,
    pub tokens: u64,
    pub window: u64,
    pub observed_at: Option<i64>,
}
const TAILS: [u64; 2] = [64 * 1024, 256 * 1024];
pub(crate) fn tail(path: &Path, limit: u64) -> Option<(String, bool)> {
    let meta = fs::symlink_metadata(path).ok().filter(|m| m.is_file())?;
    let mut file = fs::File::open(path).ok()?;
    let from = meta.len().saturating_sub(limit);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = vec![];
    file.take(limit).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let text = if from > 0 {
        text.split_once('\n').map(|(_, rest)| rest.to_owned())?
    } else {
        text
    };
    Some((text, from == 0))
}
/// The newest `token_count` event with usage and a context window in a bounded rollout tail.
pub fn codex_context(rollout: &Path) -> Option<Context> {
    for limit in TAILS {
        let (text, whole) = tail(rollout, limit)?;
        let found = text.lines().rev().find_map(|line| {
            if !line.contains("\"token_count\"") {
                return None;
            }
            let v: Value = serde_json::from_str(line).ok()?;
            if v["type"] != "event_msg" || v["payload"]["type"] != "token_count" {
                return None;
            }
            let info = &v["payload"]["info"];
            let window = info["model_context_window"].as_u64().filter(|w| *w > 0)?;
            let tokens = info["last_token_usage"]["total_tokens"].as_u64()?;
            let pct = (tokens as f64 * 1000. / window as f64).round() / 10.;
            Some(Context {
                pct: pct.min(100.),
                tokens,
                window,
                observed_at: v["timestamp"]
                    .as_str()
                    .and_then(crate::reset_credit::claude::parse_time),
            })
        });
        if found.is_some() || whole {
            return found;
        }
    }
    None
}

/// Codex fields of one row from its rollout (full `api state`; `running` conversations only).
pub fn apply_codex(row: &mut Value, rollout: Option<&Path>, running: bool) {
    match rollout.filter(|_| running).map(|r| (codex_context(r), r)) {
        Some((Some(c), _)) => {
            set(
                row,
                "context_pct",
                json!(c.pct),
                Some("rollout"),
                c.observed_at,
                None,
            );
            row["context_tokens"] = json!(c.tokens);
            row["context_window_size"] = json!(c.window);
        }
        Some((None, _)) => {
            unknown(row, "context_pct", CODEX_NO_TOKEN_COUNT);
            row["context_tokens"] = Value::Null;
            row["context_window_size"] = Value::Null;
        }
        None => {
            unknown(row, "context_pct", CODEX_NOT_RUNNING);
            row["context_tokens"] = Value::Null;
            row["context_window_size"] = Value::Null;
        }
    }
    unknown(row, "subagents_running", CODEX_TASKS);
    unknown(row, "background_tasks", CODEX_TASKS);
}

/// Claude `subagents_running` from the hook-event tracker of this process (contract 1.64.0).
fn apply_claude_tasks(row: &mut Value, tasks: Option<&crate::provider::events::Tasks>) {
    unknown(row, "background_tasks", CLAUDE_BACKGROUND);
    if row["availability"] != "running" {
        return unknown(row, "subagents_running", CLAUDE_NOT_RUNNING);
    }
    let since = row["active_since"].as_i64();
    let current = tasks.filter(|t| {
        !t.ended
            && t.process_started_at
                .is_some_and(|at| since.is_none_or(|s| at >= s - 2))
    });
    match current {
        Some(t) => set(
            row,
            "subagents_running",
            json!(t.subagents.len()),
            Some("hook_events"),
            Some(t.last_event_at),
            None,
        ),
        None => unknown(row, "subagents_running", CLAUDE_NO_HOOK_EVENTS),
    }
}

/// Fill the fields of one public session row. A Codex row keeps what a full `api state` read
/// (a cached snapshot carries it with its time); one without them gets nulls with reasons.
/// `hook` is the Claude conversation's status-line record.
pub fn apply(
    row: &mut Value,
    hook: Option<&crate::session_model::HookRecord>,
    tasks: Option<&crate::provider::events::Tasks>,
) {
    match row["provider"].as_str().unwrap_or_default() {
        "claude" => {
            match hook.and_then(|h| h.context_used_percentage.map(|p| (p, h))) {
                Some((pct, h)) => {
                    set(
                        row,
                        "context_pct",
                        json!(pct),
                        Some("statusline"),
                        Some(h.observed_at_ms / 1000),
                        None,
                    );
                    row["context_tokens"] = json!(h.context_tokens);
                    row["context_window_size"] = json!(h.context_window_size);
                }
                None => {
                    unknown(row, "context_pct", CLAUDE_NO_CONTEXT_YET);
                    row["context_tokens"] = Value::Null;
                    row["context_window_size"] = Value::Null;
                }
            }
            apply_claude_tasks(row, tasks);
        }
        "codex" => {
            if row.get("context_pct_source").is_none() {
                apply_codex(row, None, false);
            }
        }
        provider => {
            let reason = if provider == "gemini" {
                GEMINI_CONTEXT
            } else {
                ANTIGRAVITY_CONTEXT
            };
            unknown(row, "context_pct", reason);
            row["context_tokens"] = Value::Null;
            row["context_window_size"] = Value::Null;
            unknown(row, "subagents_running", NATIVE_TASKS);
            unknown(row, "background_tasks", NATIVE_TASKS);
        }
    }
}

#[cfg(test)]
mod tests;
