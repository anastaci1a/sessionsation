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

thread_local! {
    /// Test-only time spent per receipt a batch settles (`pace`).
    static PACE: std::cell::Cell<std::time::Duration> = const { std::cell::Cell::new(std::time::Duration::ZERO) };
}
pub(super) fn pace() {
    std::thread::sleep(PACE.with(std::cell::Cell::get));
}
/// A fixed mix of every retention case, with fixed ids: receipts (old and young, certain
/// and uncertain, unreadable), orphan bindings (cancelled, expired, young) and journals.
fn mixed(paths: &Paths, input: &Enqueue, now: i64, receipts: usize) {
    let mut db = Ledger::open(paths).unwrap();
    let id = |n: usize| format!("00000000-0000-4000-8000-{n:012}");
    let states = [
        ("completed_with_reply", 15),
        ("failed", 13),
        ("unknown", 60),
        ("native_accepted", 89),
        ("execution_started", 91),
        ("pending_local", 91),
        ("rejected", 400),
    ];
    for n in 0..receipts {
        let (state, age) = states[n % states.len()];
        let mut i = input.clone();
        i.message_id = id(n);
        let mut r = db.accept(&i, now).unwrap();
        r.state = state.into();
        db.save(&r).unwrap();
        db.db
            .execute(
                "UPDATE receipts SET expiry=? WHERE id=?",
                params![now - age * DAY - n as i64, i.message_id],
            )
            .unwrap();
        db.db
            .execute(
                "INSERT INTO public_messages VALUES (?,'{}')",
                [&i.message_id],
            )
            .unwrap();
    }
    db.db
        .execute(
            "INSERT INTO receipts VALUES (?,?,?,?)",
            params![id(900_000), "h", now - 400 * DAY, "{"],
        )
        .unwrap();
    for n in 0..30 {
        let (cancelled, age) = [(true, 20), (false, 20), (false, 100)][n % 3];
        let binding =
            serde_json::json!({"cancelled":cancelled,"request":{"expires_at":now - age * DAY}});
        db.db
            .execute(
                "INSERT INTO public_messages VALUES (?,?)",
                params![id(500_000 + n), binding.to_string()],
            )
            .unwrap();
    }
    let dir = paths.root.join(crate::layout::CLAUDE_STEER);
    fs::create_dir_all(&dir).unwrap();
    for n in 0..30 {
        let (phase, state, age) = [
            ("complete", "completed_with_reply", 20),
            ("complete", "rejected", 2),
            ("complete", "delivery_unknown", 30),
            ("written", "execution_started", 20),
            ("complete", "delivery_unknown", 100),
        ][n % 5];
        let id = id(700_000 + n);
        fs::write(
            dir.join(format!("{id}.json")),
            serde_json::json!({"phase":phase,"state":state,"reason":null,
                "updated_at":now - age * DAY,"request":{"message_id":id}})
            .to_string(),
        )
        .unwrap();
    }
}
/// Every table (rows in key order, events in sequence order) and the journal file names.
fn dump(paths: &Paths) -> Vec<String> {
    let db = Ledger::open(paths).unwrap();
    let mut out = Vec::new();
    for sql in [
        "SELECT id||hash||expiry||receipt FROM receipts ORDER BY id",
        "SELECT receipt FROM events ORDER BY seq",
        "SELECT id||binding FROM public_messages ORDER BY id",
        "SELECT id||mode FROM mode_requests ORDER BY id",
        "SELECT id||state||coalesce(reason,'-')||retired_at FROM retired_messages ORDER BY id",
        "SELECT store||last_run FROM retention_runs ORDER BY store",
    ] {
        let mut s = db.db.prepare(sql).unwrap();
        out.extend(
            s.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .flatten(),
        );
        out.push(String::new());
    }
    let mut files: Vec<String> = fs::read_dir(paths.root.join(crate::layout::CLAUDE_STEER))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    out.extend(files);
    out
}
/// Hold times (ms) of the writer opens a pass made from this module (`ledger-stats.jsonl`).
fn holds(paths: &Paths) -> Vec<f64> {
    fs::read_to_string(paths.root.join(crate::layout::LEDGER_STATS))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| {
            v["caller"]
                .as_str()
                .is_some_and(|c| c.contains("retention.rs"))
        })
        .filter_map(|v| v["hold_ms"].as_f64())
        .collect()
}
/// 1.71.1: the batched hourly pass leaves exactly what one pass under a single hold leaves.
#[test]
fn batched_pass_leaves_the_same_ledger_and_journals_as_a_single_hold() {
    let (_a, single, input, now) = fixture();
    let (_b, batched, _, _) = fixture();
    mixed(&single, &input, now, 140);
    mixed(&batched, &input, now, 140);
    let mut db = Ledger::open(&single).unwrap();
    let single_hold = std::time::Instant::now();
    let one = run(&mut db, now, POLICY).unwrap();
    let single_hold = single_hold.elapsed();
    drop(db);
    let many = run_batched(&batched, now, POLICY, std::time::Duration::from_secs(5), 7).unwrap();
    assert_eq!(one, many);
    assert!(one > 100, "{one}");
    assert_eq!(dump(&single), dump(&batched));
    let h = holds(&batched);
    assert!(h.len() > 20, "{h:?}");
    eprintln!(
        "retention: single hold {single_hold:?}; batched {} holds of 7, max {:.1} ms",
        h.len(),
        h.iter().copied().fold(0.0, f64::max)
    );
    // Idempotent in batches too.
    assert_eq!(
        run_batched(&batched, now, POLICY, std::time::Duration::from_secs(5), 7).unwrap(),
        0
    );
    assert_eq!(dump(&batched).len(), dump(&single).len());
}
/// 1.71.1: an admission during the hourly pass waits for one batch at most, not the pass.
#[test]
fn an_admission_during_a_batched_pass_waits_for_one_batch_at_most() {
    let (_dir, paths, input, now) = fixture();
    mixed(&paths, &input, now, 280);
    let batch = 8;
    let per_receipt = std::time::Duration::from_millis(10);
    let pass = {
        let paths = paths.clone();
        std::thread::spawn(move || {
            PACE.with(|p| p.set(per_receipt));
            let started = std::time::Instant::now();
            let n = run_batched(
                &paths,
                now,
                POLICY,
                std::time::Duration::from_secs(5),
                batch,
            );
            (n, started.elapsed())
        })
    };
    // Admissions while the pass settles receipts (well over one batch of pace each).
    std::thread::sleep(per_receipt * 30);
    let mut waits = Vec::new();
    for _ in 0..5 {
        let asked = std::time::Instant::now();
        let mut db =
            super::super::ledger::open_write_blocking(&paths, std::time::Duration::from_secs(30))
                .unwrap();
        waits.push(asked.elapsed());
        let mut i = input.clone();
        i.message_id = uuid::Uuid::new_v4().to_string();
        db.accept(&i, now).unwrap();
        drop(db);
        std::thread::sleep(per_receipt * 5);
    }
    let (n, took) = pass.join().unwrap();
    assert!(n.unwrap() > 100);
    let one_batch = per_receipt * u32::try_from(batch).unwrap();
    let worst = waits.iter().max().copied().unwrap();
    eprintln!("pass {took:?}; admission waits {waits:?}; one batch {one_batch:?}");
    assert!(took > one_batch * 10, "{took:?}");
    assert!(worst < one_batch * 3, "{waits:?}");
    let h = holds(&paths);
    assert!(
        h.iter().all(|&ms| ms < (one_batch * 3).as_secs_f64() * 1e3),
        "{h:?}"
    );
}
/// The pass pages both tables by key: no full read of `public_messages` bindings.
#[test]
fn batched_scans_read_public_messages_through_the_key_index() {
    let (_dir, paths, _input, _now) = fixture();
    let db = Ledger::open(&paths).unwrap();
    let p = plan(
        &db,
        "SELECT p.id,EXISTS(SELECT 1 FROM receipts r WHERE r.id=p.id) FROM public_messages p WHERE p.id>'' ORDER BY p.id LIMIT 9",
    );
    assert!(
        !p.split(" | ")
            .any(|l| l.contains("SCAN p") && !l.contains("COVERING INDEX")),
        "{p}"
    );
}
