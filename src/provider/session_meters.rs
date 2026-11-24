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
mod tests {
    use super::*;

    fn keys(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    }

    #[test]
    fn every_provider_row_gets_the_same_keys_with_sources_or_reasons() {
        let t = tempfile::tempdir().unwrap();
        let rollout = t.path().join("rollout.jsonl");
        let event = |tokens: u64, at: &str| {
            json!({"timestamp":at,"type":"event_msg","payload":{"type":"token_count","info":{
                "total_token_usage":{"total_tokens":tokens * 3},
                "last_token_usage":{"input_tokens":tokens - 100,"output_tokens":100,"total_tokens":tokens},
                "model_context_window":258_400},"rate_limits":null}})
            .to_string()
        };
        let mut text = String::new();
        text.push_str(&event(10_000, "2026-11-23T10:00:00.000Z"));
        text.push('\n');
        // Codex emits token_count with info null before the first response.
        text.push_str(r#"{"timestamp":"2026-11-23T10:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#);
        text.push('\n');
        text.push_str(&event(64_600, "2026-11-23T10:05:00.500Z"));
        text.push('\n');
        // A large tool output after the newest count still leaves it in the bounded tail.
        text.push_str(&format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"output\":\"{}\"}}}}\n",
            "x".repeat(100_000)
        ));
        fs::write(&rollout, text).unwrap();
        let c = codex_context(&rollout).unwrap();
        assert_eq!((c.pct, c.tokens, c.window), (25.0, 64_600, 258_400));
        assert_eq!(
            c.observed_at,
            crate::reset_credit::claude::parse_time("2026-11-23T10:05:00Z")
        );

        let mut codex = json!({"provider":"codex"});
        apply_codex(&mut codex, Some(&rollout), true);
        assert_eq!(codex["context_pct"], 25.0);
        assert_eq!(codex["context_pct_source"], "rollout");
        assert!(codex["context_pct_reason"].is_null());
        assert!(codex["subagents_running"].is_null());
        assert_eq!(codex["subagents_running_reason"], CODEX_TASKS);
        assert!(codex["subagents_running_source"].is_null());
        // A cached row keeps its reading.
        let before = codex.clone();
        apply(&mut codex, None, None);
        assert_eq!(codex, before);
        let mut stopped = json!({"provider":"codex"});
        apply_codex(&mut stopped, Some(&rollout), false);
        assert!(stopped["context_pct"].is_null());
        assert_eq!(stopped["context_pct_reason"], CODEX_NOT_RUNNING);
        let empty = t.path().join("empty.jsonl");
        fs::write(&empty, "{\"type\":\"session_meta\"}\n").unwrap();
        let mut fresh = json!({"provider":"codex"});
        apply_codex(&mut fresh, Some(&empty), true);
        assert_eq!(fresh["context_pct_reason"], CODEX_NO_TOKEN_COUNT);

        let hook = crate::session_model::HookRecord {
            schema_version: 1,
            session_id: "s".into(),
            model_id: Some("claude-fable-5-2".into()),
            model_display_name: None,
            effort_level: None,
            thinking_enabled: None,
            fast_mode: None,
            claude_version: None,
            observed_at_ms: 1_800_000_000_123,
            changed_at_ms: 1,
            context_used_percentage: Some(42.0),
            context_tokens: Some(84_000),
            context_window_size: Some(200_000),
        };
        let mut claude = json!({"provider":"claude"});
        apply(&mut claude, Some(&hook), None);
        assert_eq!(claude["context_pct"], 42.0);
        assert_eq!(claude["context_pct_source"], "statusline");
        assert_eq!(claude["context_pct_observed_at"], 1_800_000_000);
        assert_eq!(claude["context_window_size"], 200_000);
        assert_eq!(claude["background_tasks_reason"], CLAUDE_BACKGROUND);
        assert_eq!(claude["subagents_running_reason"], CLAUDE_NOT_RUNNING);
        let mut unread = json!({"provider":"claude"});
        apply(
            &mut unread,
            Some(&crate::session_model::HookRecord {
                context_used_percentage: None,
                ..hook.clone()
            }),
            None,
        );
        assert!(unread["context_pct"].is_null());
        assert_eq!(unread["context_pct_reason"], CLAUDE_NO_CONTEXT_YET);
        let mut gemini = json!({"provider":"gemini"});
        apply(&mut gemini, None, None);
        assert_eq!(gemini["context_pct_reason"], GEMINI_CONTEXT);
        let mut agy = json!({"provider":"antigravity"});
        apply(&mut agy, None, None);
        for row in [&claude, &unread, &gemini, &agy, &stopped, &fresh] {
            assert_eq!(keys(row), keys(&codex), "{row}");
        }
    }

    #[test]
    fn claude_subagents_running_is_counted_from_the_hook_events_of_this_process() {
        use crate::provider::events::Tasks;
        let mut t = Tasks {
            process_started_at: Some(1_000),
            last_event_at: 1_050,
            ..Default::default()
        };
        t.subagents.insert("a1".into(), "Explore".into());
        t.subagents.insert("a2".into(), "general-purpose".into());
        let row = |active_since: i64| json!({"provider":"claude","availability":"running","active_since":active_since});
        let mut running = row(999);
        apply(&mut running, None, Some(&t));
        assert_eq!(running["subagents_running"], 2);
        assert_eq!(running["subagents_running_source"], "hook_events");
        assert_eq!(running["subagents_running_observed_at"], 1_050);
        assert!(running["subagents_running_reason"].is_null());
        assert!(running["background_tasks"].is_null());
        assert_eq!(running["background_tasks_reason"], CLAUDE_BACKGROUND);
        // None running is a real 0.
        let idle = Tasks {
            subagents: Default::default(),
            ..t.clone()
        };
        let mut zero = row(999);
        apply(&mut zero, None, Some(&idle));
        assert_eq!(zero["subagents_running"], 0);
        // A tracker from an earlier process (started before this one), an ended one, none at
        // all, or a stopped session: null with the reason.
        for (r, tasks, reason) in [
            (row(2_000), Some(&t), CLAUDE_NO_HOOK_EVENTS),
            (row(999), None, CLAUDE_NO_HOOK_EVENTS),
            (
                json!({"provider":"claude","availability":"saved"}),
                Some(&t),
                CLAUDE_NOT_RUNNING,
            ),
        ] {
            let mut r = r;
            apply(&mut r, None, tasks);
            assert!(r["subagents_running"].is_null(), "{r}");
            assert_eq!(r["subagents_running_reason"], reason);
        }
        let ended = Tasks {
            ended: true,
            ..t.clone()
        };
        let mut r = row(999);
        apply(&mut r, None, Some(&ended));
        assert_eq!(r["subagents_running_reason"], CLAUDE_NO_HOOK_EVENTS);
    }
}
