use super::*;
use serde_json::json;
use std::fs;

#[test]
fn methods_and_nothing_or_already_compacted() {
    assert_eq!(method(ProviderId::Codex).name(), Some("app_server"));
    assert_eq!(method(ProviderId::Claude).native(), Some("/compact"));
    assert!(matches!(
        method(ProviderId::Gemini),
        Method::Unsupported(r) if r == conversation::GEMINI_UNSUPPORTED
    ));
    assert!(matches!(
        method(ProviderId::Antigravity),
        Method::Unsupported(_)
    ));
    assert_eq!(method_of("no-such"), Method::AppServer);
    let d = tempfile::tempdir().unwrap();
    let write = |lines: &[Value]| {
        let p = d.path().join("t.jsonl");
        fs::write(
            &p,
            lines
                .iter()
                .map(|l| l.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
        conversation::observe(ProviderId::Claude, Some(&p), None)
    };
    let user =
        |t: &str| json!({"type":"user","timestamp":t,"message":{"role":"user","content":"hi"}});
    let reply = |t: &str| json!({"type":"assistant","timestamp":t,"message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}});
    let boundary = |t: &str| json!({"type":"system","subtype":"compact_boundary","timestamp":t});
    assert_eq!(
        already_from(&write(&[json!({"type":"summary"})])),
        Some(Already::NothingToCompact)
    );
    assert_eq!(
        already_from(&write(&[
            user("2026-11-23T10:00:00Z"),
            reply("2026-11-23T10:00:05Z")
        ])),
        None
    );
    assert!(matches!(
        already_from(&write(&[
            user("2026-11-23T10:00:00Z"),
            reply("2026-11-23T10:00:05Z"),
            boundary("2026-11-23T10:01:00Z")
        ])),
        Some(Already::AlreadyCompacted(_))
    ));
    assert_eq!(
        already_from(&write(&[
            boundary("2026-11-23T10:01:00Z"),
            user("2026-11-23T10:02:00Z")
        ])),
        None
    );
    // No history file: nothing is decided here.
    assert_eq!(
        already_from(&conversation::observe(ProviderId::Claude, None, None)),
        None
    );
}
