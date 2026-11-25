use super::*;
use std::io::Write;

#[test]
fn rfc3339() {
    assert_eq!(parse_rfc3339("1970-01-02T00:00:00Z"), Some(86_400_000));
    assert_eq!(
        parse_rfc3339("2026-10-29T16:45:47.123Z"),
        Some(1_793_292_347_123)
    );
    assert_eq!(
        parse_rfc3339("2026-10-29T18:45:47+02:00"),
        Some(1_793_292_347_000)
    );
    assert_eq!(parse_rfc3339("garbage"), None);
}
#[test]
fn claude_and_gemini_use_last_record_and_cache_per_mtime() {
    let dir = tempfile::tempdir_in(std::env::var("TMPDIR").unwrap_or("/dev/shm".into())).unwrap();
    let p = dir.path().join("s.jsonl");
    let mut f = fs::File::create(&p).unwrap();
    writeln!(f, r#"{{"type":"user","timestamp":"2026-10-29T10:00:00Z"}}"#).unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","timestamp":"2026-10-29T10:05:00Z"}}"#
    )
    .unwrap();
    writeln!(f, r#"{{"type":"file-history-snapshot"}}"#).unwrap();
    drop(f);
    let want = parse_rfc3339("2026-10-29T10:05:00Z");
    assert_eq!(last_accessed(ProviderId::Claude, &p), want);
    assert_eq!(last_accessed(ProviderId::Gemini, &p), want);
    // Same stamp: served from cache even if the content were unreadable.
    assert_eq!(last_accessed(ProviderId::Claude, &p), want);
    let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
    writeln!(f, r#"{{"type":"user","timestamp":"2026-10-29T11:00:00Z"}}"#).unwrap();
    drop(f);
    assert_eq!(
        last_accessed(ProviderId::Claude, &p),
        parse_rfc3339("2026-10-29T11:00:00Z")
    );
    // No timestamps anywhere: mtime fallback.
    let q = dir.path().join("t.jsonl");
    fs::write(&q, "{\"a\":1}\n").unwrap();
    let m = millis(fs::metadata(&q).unwrap().modified().unwrap());
    assert_eq!(last_accessed(ProviderId::Claude, &q), m);
}
#[test]
fn antigravity_uses_brain_log_then_db_mtime_and_missing_is_none() {
    let dir = tempfile::tempdir_in(std::env::var("TMPDIR").unwrap_or("/dev/shm".into())).unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    let db = dir.path().join("conversations").join(format!("{id}.db"));
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    fs::write(&db, "x").unwrap();
    let m = |p: &Path| millis(fs::metadata(p).unwrap().modified().unwrap());
    assert_eq!(last_accessed(ProviderId::Antigravity, &db), m(&db));
    let log = dir
        .path()
        .join("brain")
        .join(id)
        .join(".system_generated/logs/transcript.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(&log, "{}\n").unwrap();
    assert_eq!(last_accessed(ProviderId::Antigravity, &db), m(&log));
    let gone = dir.path().join("conversations/none.db");
    assert_eq!(last_accessed(ProviderId::Antigravity, &gone), None);
    assert_eq!(
        last_accessed(ProviderId::Claude, &dir.path().join("nope.jsonl")),
        None
    );
}
