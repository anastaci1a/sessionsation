use super::*;
use std::fs;

fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}
fn write(dir: &Path, name: &str, lines: &[Value]) -> PathBuf {
    let p = dir.join(name);
    let text: String = lines.iter().map(|l| l.to_string() + "\n").collect();
    fs::write(&p, text).unwrap();
    p
}

#[test]
fn every_provider_gets_the_same_keys_with_sources_or_reasons() {
    let t = tempfile::tempdir().unwrap();
    let ev = |at: &str, kind: &str, payload: Value| json!({"timestamp":at,"type":kind,"payload":payload});
    let rollout = write(
        t.path(),
        "rollout.jsonl",
        &[
            ev("2026-11-23T10:00:00Z", "session_meta", json!({"id":"x"})),
            ev(
                "2026-11-23T10:00:01Z",
                "event_msg",
                json!({"type":"user_message","message":"first ask"}),
            ),
            ev(
                "2026-11-23T10:00:02Z",
                "event_msg",
                json!({"type":"agent_message","message":"first answer"}),
            ),
            ev(
                "2026-11-23T10:00:03Z",
                "event_msg",
                json!({"type":"task_complete"}),
            ),
            ev(
                "2026-11-23T10:00:04Z",
                "compacted",
                json!({"message":"summary"}),
            ),
            ev(
                "2026-11-23T10:00:05Z",
                "event_msg",
                json!({"type":"user_message","message":"second ask"}),
            ),
            ev(
                "2026-11-23T10:00:06Z",
                "event_msg",
                json!({"type":"agent_message","message":"working"}),
            ),
            ev(
                "2026-11-23T10:00:07Z",
                "event_msg",
                json!({"type":"agent_message","message":"second answer"}),
            ),
        ],
    );
    let t0 = crate::reset_credit::claude::parse_time("2026-11-23T10:00:00Z").unwrap();
    let codex = observe(ProviderId::Codex, Some(&rollout), Some(5));
    assert_eq!(codex["last_reply"]["text"], "second answer", "{codex}");
    assert_eq!(codex["last_reply"]["at"], t0 + 7);
    assert_eq!(codex["last_reply_source"], "rollout");
    assert!(codex["last_reply_reason"].is_null());
    assert_eq!(codex["last_instruction"]["text"], "second ask");
    assert_eq!(codex["compacted_at"], t0 + 4);
    let turns = codex["recent_turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0]["instruction"]["text"], "first ask");
    assert_eq!(turns[0]["reply"]["text"], "first answer");
    assert_eq!(turns[0]["ended_at"], t0 + 3);
    assert_eq!(turns[1]["started_at"], t0 + 5);
    // Without --history the turns are not read.
    let plain = observe(ProviderId::Codex, Some(&rollout), None);
    assert!(plain["recent_turns"].is_null());
    assert_eq!(plain["recent_turns_reason"], NOT_REQUESTED);

    let rec = |kind: &str, extra: Value| {
        let mut v = json!({"type":kind,"timestamp":"2026-11-23T11:00:00Z"});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    };
    let transcript = write(
        t.path(),
        "claude.jsonl",
        &[
            rec(
                "user",
                json!({"message":{"role":"user","content":"please fix"}}),
            ),
            rec(
                "assistant",
                json!({"message":{"content":[{"type":"text","text":"fixed"},{"type":"tool_use","name":"Bash"}]}}),
            ),
            rec(
                "user",
                json!({"message":{"content":[{"type":"tool_result","content":"ok"}]}}),
            ),
            rec("system", json!({"subtype":"compact_boundary"})),
            rec(
                "user",
                json!({"isCompactSummary":true,"message":{"content":"SUMMARY"}}),
            ),
            rec("user", json!({"isMeta":true,"message":{"content":"META"}})),
        ],
    );
    let claude = observe(ProviderId::Claude, Some(&transcript), Some(5));
    assert_eq!(claude["last_instruction"]["text"], "please fix", "{claude}");
    assert_eq!(claude["last_reply"]["text"], "fixed");
    assert_eq!(claude["last_reply_source"], "transcript");
    assert!(claude["compacted_at"].is_i64());
    assert_eq!(claude["recent_turns"].as_array().unwrap().len(), 1);

    let log = write(
        t.path(),
        "transcript.jsonl",
        &[
            json!({"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-11-23T12:00:00Z","content":"<USER_REQUEST>\nhello agy\n</USER_REQUEST>"}),
            json!({"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-11-23T12:00:01Z","content":"hi there"}),
        ],
    );
    let agy = observe(ProviderId::Antigravity, Some(&log), Some(5));
    assert_eq!(agy["last_instruction"]["text"], "hello agy", "{agy}");
    assert_eq!(agy["last_reply"]["text"], "hi there");
    assert_eq!(agy["last_reply_source"], "transcript_log");
    assert!(agy["compacted_at"].is_null());
    assert_eq!(agy["compacted_at_reason"], AGY_NO_COMPACTION);

    let gemini = observe(ProviderId::Gemini, None, None);
    assert_eq!(gemini["supported"], false);
    assert_eq!(gemini["reason"], GEMINI_UNSUPPORTED);
    assert_eq!(gemini["last_reply_reason"], GEMINI_UNSUPPORTED);
    let missing = observe(ProviderId::Claude, Some(&t.path().join("none")), None);
    assert_eq!(missing["last_reply_reason"], NO_FILE);
    for v in [&claude, &agy, &gemini, &missing, &plain] {
        assert_eq!(keys(v), keys(&codex), "{v}");
    }
    // Long texts are cut at a character boundary and say so.
    let long = write(
        t.path(),
        "long.jsonl",
        &[ev(
            "2026-11-23T10:00:00Z",
            "event_msg",
            json!({"type":"agent_message","message":"é".repeat(MAX_TEXT)}),
        )],
    );
    let cut = observe(ProviderId::Codex, Some(&long), None);
    assert_eq!(cut["last_reply"]["truncated"], true);
    assert_eq!(cut["last_reply"]["text"].as_str().unwrap().len(), MAX_TEXT);
    assert!(cut["last_instruction"]["text"].is_null());
    assert!(
        cut["last_instruction_reason"]
            .as_str()
            .unwrap()
            .starts_with("not_recorded")
    );
}
