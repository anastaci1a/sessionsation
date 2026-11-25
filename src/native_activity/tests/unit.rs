use super::super::*;
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
    assert!(antigravity_record(&json!({"type":"PLANNER_RESPONSE","status":"RUNNING"})).is_none());
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
