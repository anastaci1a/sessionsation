use super::*;
use crate::messaging::tests::fixture;
use serde_json::Value;
use std::process::Command;

fn user_version(paths: &Paths) -> i64 {
    let db = Connection::open_with_flags(
        paths.root.join(layout::RECEIPTS_DB),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

fn stats(paths: &Paths) -> Vec<Value> {
    let text = fs::read_to_string(paths.root.join(layout::LEDGER_STATS)).unwrap_or_default();
    text.lines()
        .map(|l| {
            assert!(l.len() < 512, "stats line too long: {l}");
            serde_json::from_str(l).unwrap()
        })
        .collect()
}

fn ddl(statements: &[String]) -> Vec<&String> {
    statements
        .iter()
        .filter(|s| {
            let s = s.to_ascii_uppercase();
            s.contains("CREATE ")
                || s.contains("INSERT INTO METADATA")
                || s.contains("USER_VERSION =")
        })
        .collect()
}

#[test]
fn second_open_runs_no_ddl() {
    let (_dir, paths, input, now) = fixture();
    let first = trace::statements(|| {
        let mut db = open_write_blocking(&paths, Duration::ZERO).unwrap();
        db.accept(&input, now).unwrap();
    });
    // The first open creates the schema (so the recorder sees DDL at all).
    assert!(
        ddl(&first).iter().any(|s| s.contains("CREATE TABLE")),
        "{first:#?}"
    );
    assert_eq!(user_version(&paths), SCHEMA_VERSION);
    let later = trace::statements(|| {
        let reader = open_read_blocking(&paths, Duration::ZERO).unwrap();
        assert!(reader.lookup(&input.message_id).is_ok());
        drop(reader);
        let mut writer = open_write_blocking(&paths, Duration::ZERO).unwrap();
        let receipt = writer.get(&input.message_id).unwrap().unwrap();
        writer.save(&receipt).unwrap();
        assert!(writer.retired(&input.message_id).unwrap().is_none());
        assert!(writer.binding(&input.message_id).unwrap().is_none());
    });
    assert!(!later.is_empty());
    assert!(ddl(&later).is_empty(), "DDL on a later open: {later:#?}");
}

#[test]
fn readers_do_not_block_each_other() {
    let (_dir, paths, input, now) = fixture();
    let stored = open_write_blocking(&paths, Duration::ZERO)
        .unwrap()
        .accept(&input, now)
        .unwrap();
    let a = open_read_blocking(&paths, Duration::ZERO).unwrap();
    let b = open_read_blocking(&paths, Duration::ZERO).unwrap();
    let id = input.message_id.clone();
    let p = paths.clone();
    // A reader on another thread (its own lock description) is not refused either.
    let c = std::thread::spawn(move || {
        let c = open_read_blocking(&p, Duration::ZERO).unwrap();
        c.lookup(&id).unwrap()
    })
    .join()
    .unwrap();
    assert_eq!(a.lookup(&input.message_id).unwrap(), stored);
    assert_eq!(b.lookup(&input.message_id).unwrap(), stored);
    assert_eq!(c, stored);
}

#[tokio::test]
async fn writer_excludes_readers() {
    let (_dir, paths, input, now) = fixture();
    open_write_blocking(&paths, Duration::ZERO)
        .unwrap()
        .accept(&input, now)
        .unwrap();
    // Two readers at once; a writer waits for both.
    let a = open_read(&paths, Duration::ZERO).await.unwrap();
    let b = open_read(&paths, Duration::ZERO).await.unwrap();
    let busy = open_write(&paths, Duration::from_millis(60)).await;
    assert_eq!(busy.err().unwrap().0, "adapter_busy");
    drop(a);
    assert_eq!(
        open_write_blocking(&paths, Duration::ZERO).err().unwrap().0,
        "adapter_busy"
    );
    drop(b);
    // The writer excludes every reader until it is dropped.
    let mut writer = open_write(&paths, Duration::ZERO).await.unwrap();
    assert_eq!(
        open_read(&paths, Duration::from_millis(60))
            .await
            .err()
            .unwrap()
            .0,
        "adapter_busy"
    );
    let mut receipt = writer.get(&input.message_id).unwrap().unwrap();
    receipt.state = "cancelled".into();
    writer.save(&receipt).unwrap();
    drop(writer);
    let reader = open_read(&paths, Duration::ZERO).await.unwrap();
    assert_eq!(reader.lookup(&input.message_id).unwrap().state, "cancelled");
}

#[test]
fn reader_connection_is_query_only() {
    let (_dir, paths, input, now) = fixture();
    open_write_blocking(&paths, Duration::ZERO)
        .unwrap()
        .accept(&input, now)
        .unwrap();
    let reader = open_read_blocking(&paths, Duration::ZERO).unwrap();
    // Every read method works on the query_only connection ...
    assert!(reader.get(&input.message_id).unwrap().is_some());
    assert!(reader.retired(&input.message_id).unwrap().is_none());
    assert!(reader.binding(&input.message_id).unwrap().is_none());
    assert_eq!(reader.expiry(&input.message_id).unwrap(), input.expires_at);
    assert_eq!(
        reader.events(None).unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(reader.held_ids(&input.session_id).unwrap().is_empty());
    // ... and a write through it fails instead of racing other readers.
    assert!(reader.db.execute("DELETE FROM events", []).is_err());
    assert!(reader.db.execute_batch("CREATE TABLE x(y)").is_err());
}

#[test]
fn stats_line_is_appended_without_the_adapter_lock() {
    let (_dir, paths, input, now) = fixture();
    let mut writer = open_write_blocking(&paths, Duration::ZERO).unwrap();
    writer.accept(&input, now).unwrap();
    // The refused open appends its line while the writer still holds the lock.
    let call_line = line!() + 1;
    let busy = open_read_blocking(&paths, Duration::from_millis(80));
    assert_eq!(busy.err().unwrap().0, "adapter_busy");
    let lines = stats(&paths);
    let refused = lines.last().unwrap();
    assert_eq!(refused["outcome"], "adapter_busy");
    assert_eq!(refused["op"], "read");
    assert!(
        refused["lock_wait_ms"].as_f64().unwrap() >= 80.0,
        "{refused}"
    );
    assert!(refused["attempts"].as_u64().unwrap() >= 2, "{refused}");
    assert!(refused["hold_ms"].is_null());
    assert!(
        refused["caller"]
            .as_str()
            .unwrap()
            .ends_with(&format!("ledger/tests.rs:{call_line}")),
        "{refused}"
    );
    // The holder's own line is written once it has released the lock.
    let before = lines.len();
    drop(writer);
    let lines = stats(&paths);
    assert_eq!(lines.len(), before + 1);
    let held = lines.last().unwrap();
    assert_eq!(held["op"], "write");
    assert_eq!(held["outcome"], "ok");
    assert_eq!(held["migrated"], true);
    assert!(held["hold_ms"].as_f64().unwrap() >= 80.0, "{held}");
    for key in ["at", "pid", "lock_wait_ms", "open_ms", "schema_ms"] {
        assert!(held[key].is_number(), "{key}: {held}");
    }
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(paths.root.join(layout::LEDGER_STATS))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn stats_file_is_trimmed_to_its_last_megabyte() {
    let (_dir, paths, _, _) = fixture();
    private_dir(&paths.root.join("messaging")).unwrap();
    let path = paths.root.join(layout::LEDGER_STATS);
    assert!(!trim_stats(&paths).unwrap());
    let line = format!("{{\"pad\":\"{}\"}}\n", "x".repeat(200));
    let mut body = line.repeat((STATS_MAX / line.len() as u64) as usize + 2);
    body.push_str("{\"last\":true}\n");
    fs::write(&path, &body).unwrap();
    assert!(trim_stats(&paths).unwrap());
    let kept = fs::read_to_string(&path).unwrap();
    assert!(kept.len() as u64 <= STATS_KEEP && kept.len() as u64 > STATS_KEEP - 512);
    assert!(
        kept.lines()
            .all(|l| serde_json::from_str::<Value>(l).is_ok())
    );
    assert!(kept.ends_with("{\"last\":true}\n"));
    assert!(!trim_stats(&paths).unwrap());
}

/// Create a 1.65.0-shaped ledger (old table set, `user_version` 0) with one receipt.
fn old_ledger(paths: &Paths, input: &Enqueue, now: i64) -> Receipt {
    let receipt = open_write_blocking(paths, Duration::ZERO)
        .unwrap()
        .accept(input, now)
        .unwrap();
    let db = Connection::open(paths.root.join(layout::RECEIPTS_DB)).unwrap();
    db.execute_batch(
        "DROP TABLE public_messages; DROP TABLE retired_messages; DROP TABLE retention_runs; \
         UPDATE metadata SET epoch='old-epoch'; PRAGMA user_version=0;",
    )
    .unwrap();
    drop(db);
    fs::remove_file(paths.root.join(layout::LEDGER_STATS)).unwrap();
    receipt
}

const RACE_ROOT: &str = "SESSIONSATION_TEST_LEDGER_RACE_ROOT";

/// Child of `old_version_db_migrates_once_under_the_exclusive_lock`: waits for the go
/// file, then opens the ledger for reading and prints the receipt it finds.
#[test]
#[ignore = "child process of old_version_db_migrates_once_under_the_exclusive_lock"]
fn race_child() {
    let Ok(root) = std::env::var(RACE_ROOT) else {
        return;
    };
    let paths = Paths {
        root: root.clone().into(),
        runtime: std::path::PathBuf::from(&root).join("run"),
    };
    let go = paths.root.join("go");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !go.exists() {
        assert!(Instant::now() < deadline, "no go file");
        std::thread::sleep(Duration::from_millis(1));
    }
    let id = fs::read_to_string(&go).unwrap();
    let reader = open_read_blocking(&paths, Duration::from_secs(20)).unwrap();
    println!("RECEIPT {}", json!(reader.lookup(id.trim()).unwrap()));
}

#[test]
fn old_version_db_migrates_once_under_the_exclusive_lock() {
    let (_dir, paths, input, now) = fixture();
    let receipt = old_ledger(&paths, &input, now);
    assert_eq!(user_version(&paths), 0);
    // A shared holder: neither child may migrate while it holds the lock.
    let holder = Lock::open(&paths, LockKind::Adapter).unwrap();
    assert!(holder.try_shared().unwrap());
    let children: Vec<_> = (0..2)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "messaging::ledger::tests::race_child",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(RACE_ROOT, &paths.root)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    fs::write(paths.root.join("go"), &input.message_id).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(user_version(&paths), 0, "migrated under a shared holder");
    drop(holder);
    for child in children {
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let line = stdout
            .lines()
            .find_map(|l| l.split_once("RECEIPT ").map(|(_, r)| r))
            .unwrap_or_else(|| panic!("child printed no receipt: {stdout}"));
        assert_eq!(serde_json::from_str::<Value>(line).unwrap(), json!(receipt));
    }
    assert_eq!(user_version(&paths), SCHEMA_VERSION);
    let lines = stats(&paths);
    let reads: Vec<_> = lines.iter().filter(|l| l["op"] == "read").collect();
    assert_eq!(reads.len(), 2, "{lines:#?}");
    assert_eq!(
        reads.iter().filter(|l| l["migrated"] == true).count(),
        1,
        "{lines:#?}"
    );
    let db = open_read_blocking(&paths, Duration::ZERO).unwrap();
    assert_eq!(db.events(None).unwrap()["cursor"], "old-epoch:1");
    assert!(db.binding(&input.message_id).unwrap().is_none());
    assert!(db.retired(&input.message_id).unwrap().is_none());
}

/// Regenerates `tests/fixtures/ledger/receipts-v1.{db,json}` into
/// `$SESSIONSATION_TEST_LEDGER_FIXTURE_OUT` (a ledger stamped by this build).
/// Run only when `SCHEMA_VERSION` changes, as a new `receipts-vN` fixture.
#[test]
#[ignore = "compatibility fixture writer; run by hand"]
fn write_stamped_fixture() {
    let Ok(out) = std::env::var("SESSIONSATION_TEST_LEDGER_FIXTURE_OUT") else {
        return;
    };
    let (_dir, _, input, now) = fixture();
    let out = std::path::PathBuf::from(out);
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths {
        root: tmp.path().join("state"),
        runtime: tmp.path().join("run"),
    };
    fs::create_dir_all(&paths.root).unwrap();
    let receipt = open_write_blocking(&paths, Duration::ZERO)
        .unwrap()
        .accept(&input, now)
        .unwrap();
    assert_eq!(user_version(&paths), SCHEMA_VERSION);
    fs::create_dir_all(&out).unwrap();
    fs::copy(
        paths.root.join(layout::RECEIPTS_DB),
        out.join("receipts-v1.db"),
    )
    .unwrap();
    fs::write(
        out.join("receipts-v1.json"),
        serde_json::to_string_pretty(&json!({"input": input, "now": now, "receipt": receipt}))
            .unwrap()
            + "\n",
    )
    .unwrap();
}

/// Open plus one receipt read with 8 concurrent readers and 1 writer (the S2
/// measurement; `--ignored --nocapture` prints the latencies).
#[test]
#[ignore = "measurement; run by hand"]
fn measure_open_read_under_contention() {
    use std::sync::{Arc, Barrier};
    let env = |k: &str, d: u64| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let readers_n = env("MEASURE_READERS", 8) as usize;
    let think = Duration::from_micros(env("MEASURE_THINK_US", 0));
    let with_writer = env("MEASURE_WRITER", 1) == 1;
    const READS: usize = 200;
    let (_dir, paths, input, now) = fixture();
    open_write_blocking(&paths, Duration::ZERO)
        .unwrap()
        .accept(&input, now)
        .unwrap();
    let start = Arc::new(Barrier::new(readers_n + 2));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (paths, id, start, done) = (
            paths.clone(),
            input.message_id.clone(),
            start.clone(),
            done.clone(),
        );
        std::thread::spawn(move || {
            start.wait();
            let mut writes = 0;
            while with_writer && !done.load(std::sync::atomic::Ordering::Relaxed) {
                let mut db = open_write_blocking(&paths, Duration::from_secs(30)).unwrap();
                let r = db.get(&id).unwrap().unwrap();
                db.save(&r).unwrap();
                drop(db);
                writes += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            writes
        })
    };
    let readers: Vec<_> = (0..readers_n)
        .map(|_| {
            let (paths, id, start) = (paths.clone(), input.message_id.clone(), start.clone());
            std::thread::spawn(move || {
                start.wait();
                (0..READS)
                    .map(|_| {
                        let t = Instant::now();
                        let db = open_read_blocking(&paths, Duration::from_secs(30)).unwrap();
                        db.get(&id).unwrap().unwrap();
                        drop(db);
                        let e = t.elapsed();
                        std::thread::sleep(think);
                        e
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let wall = Instant::now();
    start.wait();
    let mut all: Vec<Duration> = readers
        .into_iter()
        .flat_map(|r| r.join().unwrap())
        .collect();
    let wall = wall.elapsed();
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let writes = writer.join().unwrap();
    all.sort();
    let at = |q: f64| all[((all.len() as f64 * q) as usize).min(all.len() - 1)].as_secs_f64() * 1e3;
    let mean = all.iter().map(Duration::as_secs_f64).sum::<f64>() / all.len() as f64 * 1e3;
    println!(
        "MEASURE readers={readers_n} think_us={} writer={with_writer} reads={} writes={writes} slow5ms={} wall_s={:.2} reads_per_s={:.0} mean_ms={mean:.2} p50_ms={:.2} p95_ms={:.2} p99_ms={:.2} max_ms={:.2}",
        think.as_micros(),
        all.len(),
        all.iter().filter(|d| d.as_millis() >= 5).count(),
        wall.as_secs_f64(),
        all.len() as f64 / wall.as_secs_f64(),
        at(0.5),
        at(0.95),
        at(0.99),
        at(1.0)
    );
    let waits: Vec<f64> = stats(&paths)
        .iter()
        .filter(|l| l["op"] == "read")
        .map(|l| l["lock_wait_ms"].as_f64().unwrap())
        .collect();
    let mean_wait = waits.iter().sum::<f64>() / waits.len() as f64;
    println!(
        "MEASURE stats_read_lines={} mean_lock_wait_ms={mean_wait:.2}",
        waits.len()
    );
}

/// A ledger written and stamped (`user_version` 1) by 1.66.0, checked in so every
/// later build proves it still opens it (regenerate with `write_stamped_fixture`).
const V1_DB: &[u8] = include_bytes!("../../../tests/fixtures/ledger/receipts-v1.db");
const V1_JSON: &str = include_str!("../../../tests/fixtures/ledger/receipts-v1.json");
/// The 1.65.0 open path, verbatim: its per-open batch and epoch insert, and the
/// public and retention tables it created on use.
const OPEN_1_65: &str = "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS metadata(epoch TEXT NOT NULL); CREATE TABLE IF NOT EXISTS receipts(id TEXT PRIMARY KEY, hash TEXT NOT NULL, expiry INTEGER NOT NULL, receipt TEXT NOT NULL); CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY AUTOINCREMENT, receipt TEXT NOT NULL); CREATE TABLE IF NOT EXISTS mode_requests(id TEXT PRIMARY KEY, mode TEXT NOT NULL); CREATE TABLE IF NOT EXISTS public_messages(id TEXT PRIMARY KEY, binding TEXT NOT NULL); CREATE TABLE IF NOT EXISTS retired_messages(id TEXT PRIMARY KEY, state TEXT NOT NULL, reason TEXT, retired_at INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS retention_runs(store TEXT PRIMARY KEY, last_run INTEGER NOT NULL);";

fn v1_root() -> (tempfile::TempDir, Paths, Value) {
    let (dir, paths, _, _) = fixture();
    private_dir(&paths.root.join("messaging")).unwrap();
    let db = paths.root.join(layout::RECEIPTS_DB);
    fs::write(&db, V1_DB).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
    (dir, paths, serde_json::from_str(V1_JSON).unwrap())
}

#[test]
fn checked_in_v1_ledger_opens_without_migration() {
    let (_dir, paths, fixture) = v1_root();
    let id = fixture["input"]["message_id"].as_str().unwrap();
    assert_eq!(user_version(&paths), 1);
    let ran = trace::statements(|| {
        let reader = open_read_blocking(&paths, Duration::ZERO).unwrap();
        assert_eq!(json!(reader.lookup(id).unwrap()), fixture["receipt"]);
        assert_eq!(
            reader.events(None).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        drop(reader);
        let mut writer = open_write_blocking(&paths, Duration::ZERO).unwrap();
        let input: Enqueue = serde_json::from_value(fixture["input"].clone()).unwrap();
        // Idempotent re-accept of the stored message.
        assert_eq!(
            json!(
                writer
                    .accept(&input, fixture["now"].as_i64().unwrap())
                    .unwrap()
            ),
            fixture["receipt"]
        );
    });
    assert!(ddl(&ran).is_empty(), "{ran:#?}");
    assert!(stats(&paths).iter().all(|l| l["migrated"] == false));
}

#[test]
fn the_1_65_open_path_still_opens_a_v1_ledger_and_keeps_its_stamp() {
    let (_dir, paths, fixture) = v1_root();
    let id = fixture["input"]["message_id"].as_str().unwrap();
    {
        // What a 1.65.0 build runs on every open, then a lookup and a write.
        let db = Connection::open(paths.root.join(layout::RECEIPTS_DB)).unwrap();
        db.execute_batch(OPEN_1_65).unwrap();
        if db
            .query_row("SELECT count(*) FROM metadata", [], |r| r.get::<_, i64>(0))
            .unwrap()
            == 0
        {
            db.execute("INSERT INTO metadata VALUES (?)", ["new-epoch"])
                .unwrap();
        }
        let text: String = db
            .query_row("SELECT receipt FROM receipts WHERE id=?", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            fixture["receipt"]
        );
        db.execute("INSERT INTO events(receipt) VALUES (?)", [&text])
            .unwrap();
    }
    assert_eq!(user_version(&paths), 1, "a 1.65.0 open must keep the stamp");
    let reader = open_read_blocking(&paths, Duration::ZERO).unwrap();
    assert_eq!(json!(reader.lookup(id).unwrap()), fixture["receipt"]);
    assert_eq!(
        reader.events(None).unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(stats(&paths).iter().all(|l| l["migrated"] == false));
}

const HOT_ROOT: &str = "SESSIONSATION_TEST_LEDGER_HOT_ROOT";

/// Child of `concurrent_readers_roll_back_a_hot_journal`: changes the ledger in a
/// transaction large enough to spill to the database file, then dies by SIGKILL.
#[test]
#[ignore = "child process of concurrent_readers_roll_back_a_hot_journal"]
fn hot_journal_child() {
    let Ok(root) = std::env::var(HOT_ROOT) else {
        return;
    };
    let paths = Paths {
        root: root.clone().into(),
        runtime: std::path::PathBuf::from(&root).join("run"),
    };
    let writer = open_write_blocking(&paths, Duration::from_secs(10)).unwrap();
    writer
        .db
        .execute_batch(
            "PRAGMA cache_size=1; BEGIN IMMEDIATE; UPDATE receipts SET receipt='crashed';",
        )
        .unwrap();
    let pad = "x".repeat(4000);
    for i in 0..400 {
        writer
            .db
            .execute(
                "INSERT INTO receipts VALUES (?,?,?,?)",
                params![format!("pad-{i}"), "h", 0, pad],
            )
            .unwrap();
    }
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
}

#[test]
fn concurrent_readers_roll_back_a_hot_journal() {
    use std::sync::{Arc, Barrier};
    let (_dir, paths, input, now) = fixture();
    let stored = open_write_blocking(&paths, Duration::ZERO)
        .unwrap()
        .accept(&input, now)
        .unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "messaging::ledger::tests::hot_journal_child",
            "--ignored",
            "--test-threads=1",
        ])
        .env(HOT_ROOT, &paths.root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
    let journal = paths.root.join("messaging/receipts.db-journal");
    assert!(
        fs::metadata(&journal).unwrap().len() > 0,
        "no hot journal left"
    );
    let start = Arc::new(Barrier::new(8));
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let (paths, id, start) = (paths.clone(), input.message_id.clone(), start.clone());
            std::thread::spawn(move || {
                start.wait();
                let reader = open_read_blocking(&paths, Duration::from_secs(10))?;
                reader.lookup(&id)
            })
        })
        .collect();
    for r in readers {
        assert_eq!(r.join().unwrap().unwrap(), stored);
    }
    assert!(!journal.exists(), "the hot journal was not rolled back");
    let db = open_read_blocking(&paths, Duration::ZERO).unwrap();
    assert!(db.get("pad-0").unwrap().is_none());
}

impl Ledger {
    /// One exclusive attempt, no waiting.
    #[track_caller]
    pub(in crate::messaging) fn open(paths: &Paths) -> Result<Self> {
        open_write_blocking(paths, Duration::ZERO)
    }
}

/// A receipt by id; a retired id is `receipt_retired`, never `not_found` (the public readers
/// build their own answers; this is the plain lookup the tests compare against).
pub(in crate::messaging) trait Lookup: Reads {
    fn lookup(&self, id: &str) -> Result<Receipt> {
        match self.get(id)? {
            Some(r) => Ok(r),
            None if self.retired(id)?.is_some() => Err(Fault("receipt_retired")),
            None => Err(Fault("receipt_not_found")),
        }
    }
}
impl<T: Reads> Lookup for T {}

/// Blocking [`open_read`] for the synchronous tests.
#[track_caller]
pub(in crate::messaging) fn open_read_blocking(
    paths: &Paths,
    budget: Duration,
) -> Result<LedgerRead> {
    let (db, lease) = acquire_blocking(paths, Access::Read, budget, Location::caller())?;
    Ok(LedgerRead { db, _lease: lease })
}
fn events_of(db: &Connection, cursor: Option<&str>) -> Result<Value> {
    let epoch: String = db.query_row("SELECT epoch FROM metadata", [], |r| r.get(0))?;
    let (min, max): (i64, i64) = db.query_row(
        "SELECT coalesce(min(seq),1),coalesce(max(seq),0) FROM events",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let after = if let Some(cursor) = cursor {
        let (given, seq) = cursor.split_once(':').ok_or(Fault("invalid_cursor"))?;
        let seq: i64 = seq.parse().map_err(|_| Fault("invalid_cursor"))?;
        if given != epoch || seq < min - 1 || seq > max {
            return Err(Fault("cursor_gap"));
        }
        seq
    } else {
        min - 1
    };
    let mut statement =
        db.prepare("SELECT seq,receipt FROM events WHERE seq>? ORDER BY seq LIMIT 100")?;
    let rows = statement.query_map([after], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut events = Vec::new();
    let mut last = after;
    for row in rows {
        let (seq, text) = row?;
        last = seq;
        let receipt: Receipt = serde_json::from_str(&text).map_err(|_| Fault("storage_error"))?;
        events.push(json!({"cursor":format!("{epoch}:{seq}"),"receipt":receipt}));
    }
    Ok(
        json!({"events":events,"cursor":format!("{epoch}:{last}"),"has_more":last<max,"public_replies":"correlated_terminal_summary"}),
    )
}

impl Ledger {
    pub(in crate::messaging) fn events(&self, cursor: Option<&str>) -> Result<Value> {
        events_of(&self.db, cursor)
    }
}
impl LedgerRead {
    pub(in crate::messaging) fn events(&self, cursor: Option<&str>) -> Result<Value> {
        events_of(&self.db, cursor)
    }
}
