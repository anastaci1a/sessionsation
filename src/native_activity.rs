//! Content-free `activity` / `live_activity` of running Claude, Gemini and
//! Antigravity sessions (contract 1.42.0), the native counterpart of the Codex
//! rollout/app-server facts in [`crate::live_activity`].
//!
//! Signals are local and read-only, and cheap enough for the cached snapshot:
//! a bounded tail of the transcript (record types and stop reasons only, never
//! message text) and, for Claude, the native peer registry entry
//! `<home>/sessions/<pid>.json` (`status` busy|idle|waiting, `statusUpdatedAt`).
//! The newest evidence wins, because the registry is not rewritten on every
//! transition. When neither signal is conclusive the state is `unknown`.
use crate::{
    live_activity::{self, TurnFacts},
    provider::{ProviderId, activity::parse_rfc3339, claude::Observation},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// First and second (fallback) tail sizes: a tool result can be one huge line.
const TAILS: [u64; 2] = [128 * 1024, 1024 * 1024];
/// A user message nobody has answered counts as work only while it is this recent (ms).
const UNANSWERED_MS: i64 = 10 * 60 * 1000;
const REGISTRY_MAX: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tail {
    /// Mid-turn (tool in flight, tool result or prompt awaiting the model).
    Working,
    Ended,
    Interrupted,
    Error,
}

/// One decisive transcript record: what it implies and when it was written (ms).
#[derive(Clone, Debug, PartialEq)]
struct Verdict {
    tail: Tail,
    at_ms: Option<i64>,
    error: Option<String>,
}

fn read_tail(path: &Path, limit: u64) -> Option<(String, bool)> {
    let meta = fs::symlink_metadata(path).ok().filter(|m| m.is_file())?;
    let mut file = fs::File::open(path).ok()?;
    let from = meta.len().saturating_sub(limit);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut bytes = vec![];
    file.take(limit).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // A tail that starts mid-record drops its first (partial) line.
    let text = if from > 0 {
        text.split_once('\n').map(|(_, rest)| rest.to_owned())?
    } else {
        text
    };
    Some((text, from == 0))
}

/// Scan complete records newest first with `decide`; widen the tail once.
fn scan(path: &Path, decide: impl Fn(&Value) -> Option<Verdict>) -> Option<Verdict> {
    for limit in TAILS {
        let (text, whole) = read_tail(path, limit)?;
        let found = text
            .lines()
            .rev()
            .filter(|l| l.starts_with('{'))
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find_map(|v| decide(&v));
        if found.is_some() || whole {
            return found;
        }
    }
    None
}

fn short_token(v: &Value) -> Option<String> {
    v.as_str()
        .filter(|s| !s.is_empty() && s.len() <= 40)
        .filter(|s| {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .map(str::to_owned)
}

/// The first text of a user record's content, only to recognise Claude's own markers.
fn user_marker(content: &Value) -> Option<&str> {
    match content {
        Value::String(s) => Some(s.as_str()),
        Value::Array(items) => items
            .iter()
            .find(|i| i["type"] == "text")
            .and_then(|i| i["text"].as_str()),
        _ => None,
    }
}

fn claude_record(v: &Value) -> Option<Verdict> {
    if v["isSidechain"] == true {
        return None;
    }
    let at_ms = v["timestamp"].as_str().and_then(parse_rfc3339);
    let verdict = |tail, error| Some(Verdict { tail, at_ms, error });
    match v["type"].as_str()? {
        "assistant" => {
            if v["isApiErrorMessage"] == true {
                return verdict(
                    Tail::Error,
                    Some(short_token(&v["error"]).unwrap_or_else(|| "api_error".into())),
                );
            }
            match v["message"]["stop_reason"].as_str() {
                Some("end_turn" | "stop_sequence" | "max_tokens" | "refusal") => {
                    verdict(Tail::Ended, None)
                }
                // tool_use, or a record still streaming (no stop reason yet)
                _ => verdict(Tail::Working, None),
            }
        }
        "user" => {
            if v["isMeta"] == true {
                return None;
            }
            let content = &v["message"]["content"];
            if content
                .as_array()
                .is_some_and(|items| items.iter().any(|i| i["type"] == "tool_result"))
            {
                return verdict(Tail::Working, None);
            }
            let marker = user_marker(content).unwrap_or_default();
            if marker.starts_with("[Request interrupted by user") {
                verdict(Tail::Interrupted, None)
            } else if marker.starts_with("<local-command") || marker.starts_with("<command-name>") {
                None // slash-command bookkeeping, not a turn
            } else {
                verdict(Tail::Working, None)
            }
        }
        "system" => (v["subtype"] == "turn_duration").then_some(Verdict {
            tail: Tail::Ended,
            at_ms,
            error: None,
        }),
        _ => None,
    }
}

fn gemini_record(v: &Value) -> Option<Verdict> {
    let at_ms = v["timestamp"].as_str().and_then(parse_rfc3339);
    let tail = match v["type"].as_str()? {
        "user" => Tail::Working,
        "gemini" => {
            if v["toolCalls"].as_array().is_some_and(|c| !c.is_empty()) {
                Tail::Working
            } else {
                Tail::Ended
            }
        }
        "error" => Tail::Error,
        _ => return None,
    };
    Some(Verdict {
        tail,
        at_ms,
        error: None,
    })
}

fn antigravity_record(v: &Value) -> Option<Verdict> {
    let kind = v["type"].as_str()?;
    let at_ms = v["created_at"].as_str().and_then(parse_rfc3339);
    if v["status"].as_str().is_some_and(|s| s != "DONE") {
        return None; // a step not yet settled: do not guess from it
    }
    let tail = match kind {
        "PLANNER_RESPONSE" => {
            if v["tool_calls"].as_array().is_some_and(|c| !c.is_empty()) {
                Tail::Working
            } else {
                Tail::Ended
            }
        }
        _ => Tail::Working, // USER_INPUT or a tool step: the model has not answered yet
    };
    Some(Verdict {
        tail,
        at_ms,
        error: None,
    })
}

#[derive(Clone, Debug, PartialEq)]
struct Registry {
    status: String,
    updated_ms: i64,
}

/// `<home>/sessions/<pid>.json` of the observed process (pid and start time
/// must match), `status` busy|idle|waiting. The `.key` files are never opened.
fn registry(home: &Path, o: &Observation) -> Option<Registry> {
    let pid = o.pid?;
    let path = home.join("sessions").join(format!("{pid}.json"));
    let meta = fs::symlink_metadata(&path).ok().filter(|m| m.is_file())?;
    if meta.len() > REGISTRY_MAX {
        return None;
    }
    let v: Value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
    let start = v["procStart"]
        .as_str()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| v["procStart"].as_u64());
    if v["pid"].as_u64() != Some(pid as u64) || start != o.process_start {
        return None;
    }
    let status = v["status"]
        .as_str()
        .filter(|s| matches!(*s, "busy" | "idle" | "waiting"))?;
    let updated_ms = v["statusUpdatedAt"]
        .as_i64()
        .or_else(|| v["updatedAt"].as_i64())?;
    Some(Registry {
        status: status.to_owned(),
        updated_ms,
    })
}

fn facts_of(tail: Tail, source: &str, error: Option<String>) -> TurnFacts {
    TurnFacts {
        observed: true,
        source: Some(source.into()),
        started: true,
        in_progress: tail == Tail::Working,
        blocked: false,
        last_status: match tail {
            Tail::Working => None,
            Tail::Ended => Some("completed".into()),
            Tail::Interrupted => Some("interrupted".into()),
            Tail::Error => Some("failed".into()),
        },
        error,
        ended_at: None,
    }
}

/// Combine the transcript verdict and the registry status into facts plus the
/// time (Unix ms) of the evidence used. `now_ms` bounds how long an unanswered
/// prompt counts as work.
fn combine(
    verdict: Option<Verdict>,
    registry: Option<Registry>,
    now_ms: i64,
    expire_work: bool,
) -> Option<(TurnFacts, i64)> {
    let transcript_at = verdict.as_ref().and_then(|v| v.at_ms);
    if let Some(r) = &registry
        && transcript_at.is_none_or(|t| r.updated_ms > t)
    {
        let mut f = facts_of(
            if r.status == "idle" {
                Tail::Ended
            } else {
                Tail::Working
            },
            "native_registry",
            None,
        );
        if r.status == "idle" {
            f.started = false;
            f.last_status = None;
        }
        f.blocked = r.status == "waiting";
        return Some((f, r.updated_ms));
    }
    let v = verdict?;
    let at = v.at_ms?;
    if v.tail == Tail::Working && now_ms - at > UNANSWERED_MS && expire_work {
        return None;
    }
    Some((facts_of(v.tail, "transcript_tail", v.error), at))
}

fn antigravity_log(db: &Path) -> Option<PathBuf> {
    let id = db.file_stem()?;
    let state = db.parent()?.parent()?;
    Some(
        state
            .join("brain")
            .join(id)
            .join(".system_generated/logs/transcript.jsonl"),
    )
}

/// Facts of one running session, with the Unix-seconds time of their evidence.
pub fn facts(
    provider: ProviderId,
    home: &Path,
    o: &Observation,
    now_ms: i64,
) -> Option<(TurnFacts, i64)> {
    let transcript = o.transcript.as_deref();
    match provider {
        ProviderId::Claude => {
            let verdict = transcript.and_then(|t| scan(t, claude_record));
            // A tool_use or prompt older than the unanswered window is still work for
            // Claude (tools may run for hours); only registry waiting upgrades it.
            combine(verdict, registry(home, o), now_ms, false)
        }
        ProviderId::Gemini => {
            let v = transcript.and_then(|t| scan(t, gemini_record))?;
            combine(Some(v), None, now_ms, true)
        }
        ProviderId::Antigravity => {
            let v = transcript
                .and_then(antigravity_log)
                .and_then(|log| scan(&log, antigravity_record))?;
            combine(Some(v), None, now_ms, true)
        }
        ProviderId::Codex => None,
    }
}

/// `activity` string, the same vocabulary as Codex rows.
pub fn activity_label(f: &TurnFacts) -> &'static str {
    match live_activity::classify(f) {
        "working" | "blocked" => "busy",
        "idle" | "turn_ended" | "error" => "idle",
        _ => "unknown",
    }
}

/// Fill `activity` and `live_activity` of a native session row. Only running
/// sessions with an observation get a value; unknown evidence is an explicit
/// `unknown` state, never a guess.
pub fn apply(
    provider: ProviderId,
    home: &Path,
    value: &mut Value,
    observed: Option<&Observation>,
    now: i64,
) {
    value["activity"] = Value::Null;
    value["live_activity"] = Value::Null;
    let Some(o) = observed.filter(|o| o.availability == "running") else {
        return;
    };
    match facts(provider, home, o, now * 1000) {
        Some((mut f, at_ms)) => {
            // Pending questions of the session itself mean it waits on the user.
            if value["pending_questions"]
                .as_array()
                .is_some_and(|q| !q.is_empty())
                && f.in_progress
            {
                f.blocked = true;
            }
            value["activity"] = json!(activity_label(&f));
            value["live_activity"] = live_activity::public(Some(&f), at_ms / 1000);
        }
        None => {
            value["activity"] = json!("unknown");
            value["live_activity"] = live_activity::public(Some(&TurnFacts::default()), now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn rec(kind: &str, stop: Option<&str>, content: Value, ts: &str) -> Value {
        json!({"type":kind,"timestamp":ts,"message":{"role":kind,"stop_reason":stop,"content":content}})
    }
    fn state(records: &[Value], now_ms: i64) -> (String, i64) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        for r in records {
            writeln!(f, "{r}").unwrap();
        }
        let v = scan(&path, claude_record);
        match combine(v, None, now_ms, false) {
            Some((f, at)) => (live_activity::classify(&f).into(), at),
            None => ("unknown".into(), 0),
        }
    }
    const T0: &str = "2026-11-05T03:00:00.000Z";

    #[test]
    fn claude_tail_states() {
        let now = parse_rfc3339("2026-11-05T03:00:30Z").unwrap();
        let tool_use = rec(
            "assistant",
            Some("tool_use"),
            json!([{"type":"tool_use"}]),
            T0,
        );
        let result = rec("user", None, json!([{"type":"tool_result"}]), T0);
        let end = rec("assistant", Some("end_turn"), json!([{"type":"text"}]), T0);
        let prompt = rec("user", None, json!([{"type":"text","text":"hi"}]), T0);
        assert_eq!(state(&[prompt.clone(), tool_use.clone()], now).0, "working");
        assert_eq!(state(&[tool_use.clone(), result], now).0, "working");
        assert_eq!(state(&[tool_use.clone(), end.clone()], now).0, "turn_ended");
        assert_eq!(state(&[end.clone(), prompt], now).0, "working");
        // Noise after the decisive record is skipped.
        let noise = json!({"type":"api-request","timestamp":"2026-11-05T03:00:05Z"});
        let duration = json!({"type":"system","subtype":"turn_duration","timestamp":T0});
        assert_eq!(state(&[tool_use, duration, noise], now).0, "turn_ended");
        let interrupted = rec(
            "user",
            None,
            json!([{"type":"text","text":"[Request interrupted by user]"}]),
            T0,
        );
        assert_eq!(state(&[interrupted], now).0, "turn_ended");
        let api_error = json!({"type":"assistant","isApiErrorMessage":true,"error":"rate_limit",
            "timestamp":T0,"message":{"stop_reason":"stop_sequence","content":[]}});
        assert_eq!(state(&[api_error], now).0, "error");
        assert_eq!(state(&[], now).0, "unknown");
        assert_eq!(
            state(&[json!({"type":"file-history-snapshot"})], now).0,
            "unknown"
        );
        let (_, at) = state(&[end], now);
        assert_eq!(at, parse_rfc3339(T0).unwrap());
    }

    #[test]
    fn registry_wins_only_when_newer() {
        let t = parse_rfc3339(T0).unwrap();
        let ended = Some(Verdict {
            tail: Tail::Ended,
            at_ms: Some(t),
            error: None,
        });
        let reg = |s: &str, at| {
            Some(Registry {
                status: s.into(),
                updated_ms: at,
            })
        };
        let class = |v, r| {
            let (f, at) = combine(v, r, t + 1000, false).unwrap();
            (live_activity::classify(&f), f.source.unwrap(), at)
        };
        // Stale registry busy (older than the finished turn): the transcript wins.
        assert_eq!(
            class(ended.clone(), reg("busy", t - 1000)),
            ("turn_ended", "transcript_tail".into(), t)
        );
        assert_eq!(
            class(ended.clone(), reg("waiting", t + 500)),
            ("blocked", "native_registry".into(), t + 500)
        );
        assert_eq!(class(ended.clone(), reg("busy", t + 500)).0, "working");
        assert_eq!(class(ended, reg("idle", t + 500)).0, "idle");
        assert_eq!(class(None, reg("idle", t)).0, "idle");
        assert!(combine(None, None, t, false).is_none());
    }

    #[test]
    fn gemini_and_antigravity_inferred_work_expires() {
        let t = parse_rfc3339(T0).unwrap();
        let working = Verdict {
            tail: Tail::Working,
            at_ms: Some(t),
            error: None,
        };
        let fresh = combine(Some(working.clone()), None, t + 1000, true);
        assert_eq!(live_activity::classify(&fresh.unwrap().0), "working");
        assert!(combine(Some(working), None, t + UNANSWERED_MS + 1, true).is_none());
        assert_eq!(
            gemini_record(&json!({"type":"gemini","timestamp":T0,"toolCalls":[{}]}))
                .unwrap()
                .tail,
            Tail::Working
        );
        assert_eq!(
            gemini_record(&json!({"type":"gemini","timestamp":T0}))
                .unwrap()
                .tail,
            Tail::Ended
        );
        assert_eq!(
            antigravity_record(
                &json!({"type":"PLANNER_RESPONSE","status":"DONE","created_at":T0,"tool_calls":[]})
            )
            .unwrap()
            .tail,
            Tail::Ended
        );
        assert!(
            antigravity_record(&json!({"type":"PLANNER_RESPONSE","status":"RUNNING"})).is_none()
        );
    }

    #[test]
    fn oversized_last_line_falls_back_to_wider_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        let end = rec("assistant", Some("end_turn"), json!([]), T0);
        writeln!(f, "{end}").unwrap();
        let big = rec(
            "assistant",
            Some("tool_use"),
            json!([{"x":"a".repeat(300_000)}]),
            "2026-11-05T03:00:10Z",
        );
        writeln!(f, "{big}").unwrap();
        let v = scan(&path, claude_record).unwrap();
        assert_eq!(v.tail, Tail::Working);
    }
}

#[cfg(test)]
mod live_probe {
    use super::*;
    use crate::provider::claude;

    /// Read-only probe of one real Claude home: `SESSIONSATION_PROBE_HOME=<home> cargo test
    /// native_activity::live_probe -- --ignored --nocapture`. Prints state, source and
    /// evidence time per running session; never message text.
    #[test]
    #[ignore]
    fn print_live_claude_activity() {
        let Some(home) = std::env::var_os("SESSIONSATION_PROBE_HOME").map(PathBuf::from) else {
            return;
        };
        let now = crate::daemon::now();
        let live = claude::probe(&home).unwrap_or_default();
        for l in &live {
            let o = claude::observe_in(&home, &l.session_id, None, &Ok(live.clone()));
            let out = facts(ProviderId::Claude, &home, &o, now * 1000);
            let shown = out.map(|(f, at)| {
                (
                    live_activity::public(Some(&f), at / 1000),
                    activity_label(&f),
                )
            });
            println!("pid {} -> {:?}", l.pid, shown);
        }
    }
}
