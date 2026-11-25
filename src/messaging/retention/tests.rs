use super::*;
use crate::messaging::{Enqueue, Receipt, tests::fixture};
const POLICY: Policy = Policy {
    terminal_seconds: 14 * DAY,
    horizon_seconds: 90 * DAY,
};
fn plan(db: &Ledger, sql: &str) -> String {
    let mut s = db.db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    s.query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .flatten()
        .collect::<Vec<_>>()
        .join(" | ")
}
/// Contract 1.67.1: a send never scans the large `public_messages` bindings, and runs no
/// retention pass while far from capacity.
#[test]
fn send_admission_reads_public_messages_by_key_and_runs_no_pass() {
    let (_dir, paths, input, now) = fixture();
    let mut db = Ledger::open(&paths).unwrap();
    let big = format!("{{\"cancelled\":false,\"pad\":\"{}\"}}", "x".repeat(5600));
    db.db
        .execute(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<5000) INSERT INTO public_messages SELECT 'id'||x, ? FROM n",
            [&big],
        )
        .unwrap();
    for sql in [
        "SELECT binding FROM public_messages WHERE id='id7'",
        "SELECT count(*) FROM public_messages",
        "SELECT p.id FROM public_messages p WHERE NOT EXISTS (SELECT 1 FROM receipts r WHERE r.id=p.id)",
    ] {
        let p = plan(&db, sql);
        assert!(
            !p.split(" | ")
                .any(|l| l.contains("SCAN public_messages") && !l.contains("COVERING INDEX")),
            "{sql}: {p}"
        );
    }
    db.accept(&input, now).unwrap();
    let runs: i64 = db
        .db
        .query_row("SELECT count(*) FROM retention_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(runs, 0, "a send far from capacity ran a retention pass");
    // The pass itself reads each orphan's binding by key.
    assert_eq!(run(&mut db, now, POLICY).unwrap(), 0);
}
fn admit(db: &mut Ledger, input: &Enqueue, now: i64, state: &str, age_days: i64) -> String {
    let mut i = input.clone();
    i.message_id = uuid::Uuid::new_v4().to_string();
    let mut r = db.accept(&i, now).unwrap();
    r.state = state.into();
    db.save(&r).unwrap();
    db.db
        .execute(
            "UPDATE receipts SET expiry=? WHERE id=?",
            params![now - age_days * DAY, i.message_id],
        )
        .unwrap();
    i.message_id
}
fn state(db: &Ledger, id: &str) -> Option<Receipt> {
    db.get(id).unwrap()
}
#[test]
fn retires_only_old_certain_terminal_receipts_and_tombstones_them() {
    let (_dir, paths, input, now) = fixture();
    let mut db = Ledger::open(&paths).unwrap();
    let old_done = admit(&mut db, &input, now, "completed_with_reply", 15);
    let young_done = admit(&mut db, &input, now, "failed", 13);
    let old_uncertain = admit(&mut db, &input, now, "unknown", 60);
    let old_accepted = admit(&mut db, &input, now, "native_accepted", 89);
    let ancient_accepted = admit(&mut db, &input, now, "execution_started", 91);
    let ancient_local = admit(&mut db, &input, now, "pending_local", 91);
    db.db
        .execute(
            "INSERT INTO receipts VALUES (?,?,?,?)",
            params![uuid::Uuid::new_v4().to_string(), "h", now - 400 * DAY, "{"],
        )
        .unwrap();
    assert_eq!(run(&mut db, now, POLICY).unwrap(), 3);
    // Kept: young terminal, uncertain/unfinished within the horizon, unreadable.
    assert!(state(&db, &young_done).is_some());
    assert_eq!(state(&db, &old_uncertain).unwrap().state, "unknown");
    assert_eq!(state(&db, &old_accepted).unwrap().state, "native_accepted");
    let left: i64 = db
        .db
        .query_row("SELECT count(*) FROM receipts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 4);
    // Retired with the honest final state.
    assert!(state(&db, &old_done).is_none());
    assert_eq!(
        retired(&db, &old_done).unwrap().unwrap().0,
        "completed_with_reply"
    );
    let (s, reason, _) = retired(&db, &ancient_accepted).unwrap().unwrap();
    assert_eq!(
        (s.as_str(), reason.as_deref()),
        ("expired", Some(HORIZON_REASON))
    );
    let (s, reason, _) = retired(&db, &ancient_local).unwrap().unwrap();
    assert_eq!((s.as_str(), reason), ("expired", None));
    // The horizon transition was published before retirement.
    let events = db.events(None).unwrap().to_string();
    assert!(events.contains(HORIZON_REASON));
    // A retired id is never accepted again, with any content.
    let mut again = input.clone();
    again.message_id = old_done.clone();
    assert_eq!(db.accept(&again, now).unwrap_err().0, "receipt_retired");
    again.text = "different".into();
    assert_eq!(db.accept(&again, now).unwrap_err().0, "receipt_retired");
    assert_eq!(db.lookup(&old_done).unwrap_err().0, "receipt_retired");
    assert_eq!(
        db.lookup(&uuid::Uuid::new_v4().to_string()).unwrap_err().0,
        "receipt_not_found"
    );
    // Idempotent: a second pass retires nothing more.
    assert_eq!(run(&mut db, now, POLICY).unwrap(), 0);
}
#[test]
fn retirement_is_throttled_and_frees_capacity() {
    let (_dir, paths, input, now) = fixture();
    let mut db = Ledger::open(&paths).unwrap();
    db.db.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO receipts SELECT x,'hash',0,'{\"message_id\":\"m\",\"session_id\":\"s\",\"generation\":\"g\",\"state\":\"rejected\",\"native_queue_id\":null,\"reason\":null}' FROM n;").unwrap();
    // Throttled: a recent pass means no retirement yet.
    db.db
        .execute("INSERT INTO retention_runs VALUES('all',?)", [now - 60])
        .unwrap();
    assert_eq!(db.accept(&input, now).unwrap_err().0, "receipt_capacity");
    db.db.execute("DELETE FROM retention_runs", []).unwrap();
    assert_eq!(db.accept(&input, now).unwrap().state, "pending_local");
    let tombs: i64 = db
        .db
        .query_row("SELECT count(*) FROM retired_messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tombs, 10000);
}
#[test]
fn journals_retire_certain_outcomes_and_keep_uncertain_until_the_horizon() {
    let (_dir, paths, _input, now) = fixture();
    let dir = paths.root.join(crate::layout::CLAUDE_STEER);
    fs::create_dir_all(&dir).unwrap();
    let write = |phase: &str, state: &str, age: i64| {
        let id = uuid::Uuid::new_v4().to_string();
        fs::write(
            dir.join(format!("{id}.json")),
            serde_json::json!({"phase":phase,"state":state,"reason":null,
                "updated_at":now - age * DAY,"request":{"message_id":id}})
            .to_string(),
        )
        .unwrap();
        id
    };
    let done = write("complete", "completed_with_reply", 20);
    let fresh = write("complete", "rejected", 2);
    let unknown = write("complete", "delivery_unknown", 30);
    let stuck = write("dispatching", "pending_local", 30);
    let ancient = write("complete", "delivery_unknown", 100);
    let mut db = Ledger::open(&paths).unwrap();
    assert_eq!(run(&mut db, now, POLICY).unwrap(), 2);
    assert_eq!(count_journal(&dir), 3);
    for id in [&fresh, &unknown, &stuck] {
        assert!(dir.join(format!("{id}.json")).exists());
    }
    assert_eq!(
        retired(&db, &done).unwrap().unwrap().0,
        "completed_with_reply"
    );
    let (s, r, _) = retired(&db, &ancient).unwrap().unwrap();
    assert_eq!(
        (s.as_str(), r.as_deref()),
        ("expired", Some(HORIZON_REASON))
    );
    drop(db);
    // A retired journal id cannot be reused by any delivery path.
    let e = super::super::headless::ensure_unused_elsewhere(&paths, &done).unwrap_err();
    assert!(e.to_string().starts_with("RECEIPT_RETIRED"), "{e}");
}
#[test]
fn delivered_journal_records_retire_after_the_terminal_window_with_their_state() {
    use fs2::FileExt;
    let (_dir, paths, _input, now) = fixture();
    let dir = paths.root.join(crate::layout::CLAUDE_DELIVERIES);
    fs::create_dir_all(&dir).unwrap();
    let write = |phase: &str, state: &str, age: i64, facts: serde_json::Value| {
        let id = uuid::Uuid::new_v4().to_string();
        fs::write(
            dir.join(format!("{id}.json")),
            serde_json::json!({"phase":phase,"state":state,"reason":null,"facts":facts,
                "updated_at":now - age * DAY,"request":{"message_id":id}})
            .to_string(),
        )
        .unwrap();
        id
    };
    let none = serde_json::json!({});
    let started = write("written", "execution_started", 20, none.clone());
    let accepted = write("written", "native_accepted", 20, none.clone());
    let young = write("written", "execution_started", 5, none.clone());
    let silent = write(
        "written",
        "native_accepted",
        20,
        serde_json::json!({"silent_accept":true}),
    );
    let unknown = write("written", "unknown", 20, none.clone());
    let delivery_unknown = write("complete", "delivery_unknown", 20, none.clone());
    let local = write("dispatching", "pending_local", 20, none.clone());
    let busy = write("written", "execution_started", 20, none.clone());
    let ancient_unknown = write("written", "unknown", 100, none.clone());
    // A worker still runs for `busy`: it holds its lock.
    let lock = crate::paths::lock_file(&dir.join(format!("{busy}.worker.lock"))).unwrap();
    lock.lock_exclusive().unwrap();
    // A stopped worker left its (free) lock behind for `started`.
    fs::write(dir.join(format!("{started}.worker.lock")), "").unwrap();
    let mut db = Ledger::open(&paths).unwrap();
    assert_eq!(run(&mut db, now, POLICY).unwrap(), 3);
    for id in [&young, &silent, &unknown, &delivery_unknown, &local, &busy] {
        assert!(dir.join(format!("{id}.json")).exists(), "{id} kept");
        assert!(retired(&db, id).unwrap().is_none());
    }
    // Retired with the real state, no horizon reason, lock removed.
    for (id, state) in [
        (&started, "execution_started"),
        (&accepted, "native_accepted"),
    ] {
        assert!(!dir.join(format!("{id}.json")).exists());
        let (s, r, _) = retired(&db, id).unwrap().unwrap();
        assert_eq!((s.as_str(), r), (state, None));
    }
    assert!(!dir.join(format!("{started}.worker.lock")).exists());
    // Unknown still follows the horizon.
    let (s, r, _) = retired(&db, &ancient_unknown).unwrap().unwrap();
    assert_eq!(
        (s.as_str(), r.as_deref()),
        ("expired", Some(HORIZON_REASON))
    );
    // The live worker's record goes once its worker has exited.
    drop(lock);
    assert_eq!(run(&mut db, now, POLICY).unwrap(), 1);
    assert_eq!(retired(&db, &busy).unwrap().unwrap().0, "execution_started");
}
