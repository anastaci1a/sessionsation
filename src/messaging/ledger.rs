//! The receipts ledger (owner of the `ledger` role): `messaging/receipts.db` and the
//! `messaging/adapter.lock` that guards it.
//!
//! - [`open_read`] takes the lock **shared** and returns a [`LedgerRead`], which has
//!   read methods only and a `query_only` connection; readers never wait for each other.
//! - [`open_write`] takes it **exclusive** and returns a [`Ledger`]; it excludes every
//!   reader and writer. A reader that needs to write drops its `LedgerRead` and opens a
//!   writer (never an in-place upgrade), then re-reads.
//! - The schema batch runs only while `PRAGMA user_version` is below [`SCHEMA_VERSION`]:
//!   a shared open that finds an older version drops its lock, takes it exclusive,
//!   re-reads the version, migrates and stamps it, releases, and reopens shared.
//!   Older builds ignore `user_version` and keep running their idempotent
//!   `CREATE TABLE IF NOT EXISTS` batch, so the file stays compatible both ways.
//! - `journal_mode=DELETE` and `synchronous=FULL` are applied on every connection.
//! - Every open appends one JSON line (< 512 bytes, `O_APPEND`, no lock) to
//!   `messaging/ledger-stats.jsonl` after its lock is released (or the wait gave up);
//!   [`trim_stats`] keeps that file bounded.
use super::{Enqueue, Fault, Receipt, Result, SubmitEvidence, retention, uuid};
use crate::{
    layout,
    paths::{Paths, private_dir},
    sync::{Lock, LockKind},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    future::Future,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    panic::Location,
    time::{Duration, Instant},
};

/// Schema of `receipts.db`, stamped in `PRAGMA user_version`. Raise it with every
/// change of [`SCHEMA`]; version 1 is the 1.65.0 table set (all `IF NOT EXISTS`).
pub(super) const SCHEMA_VERSION: i64 = 1;
/// Every table of the receipts database, including the public-send bindings and the
/// retention tombstones that used to be created on each use.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS metadata(epoch TEXT NOT NULL); CREATE TABLE IF NOT EXISTS receipts(id TEXT PRIMARY KEY, hash TEXT NOT NULL, expiry INTEGER NOT NULL, receipt TEXT NOT NULL); CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY AUTOINCREMENT, receipt TEXT NOT NULL); CREATE TABLE IF NOT EXISTS mode_requests(id TEXT PRIMARY KEY, mode TEXT NOT NULL); CREATE TABLE IF NOT EXISTS public_messages(id TEXT PRIMARY KEY, binding TEXT NOT NULL); CREATE TABLE IF NOT EXISTS retired_messages(id TEXT PRIMARY KEY, state TEXT NOT NULL, reason TEXT, retired_at INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS retention_runs(store TEXT PRIMARY KEY, last_run INTEGER NOT NULL);";
pub(super) const RETRY: Duration = Duration::from_millis(25);
/// `ledger-stats.jsonl` is trimmed above this size to its last [`STATS_KEEP`] bytes.
pub(super) const STATS_MAX: u64 = 10 * 1024 * 1024;
pub(super) const STATS_KEEP: u64 = 1024 * 1024;

/// A retention tombstone: final state, reason, retired_at.
pub(super) type Tombstone = (String, Option<String>, i64);

#[cfg(test)]
pub(super) mod trace;

/// Read access shared by [`Ledger`] and [`LedgerRead`].
pub(super) trait Reads {
    fn get(&self, id: &str) -> Result<Option<Receipt>>;
    fn retired(&self, id: &str) -> Result<Option<Tombstone>>;
    /// The stored public-send binding (JSON text) of a message id.
    fn binding(&self, id: &str) -> Result<Option<String>>;
    /// Whether `id` was admitted through `modes` (queue or submit).
    fn mode_record(&self, id: &str) -> Result<bool>;
    fn expiry(&self, id: &str) -> Result<i64>;
    fn events(&self, cursor: Option<&str>) -> Result<Value>;
    /// Held public messages of a conversation, in arrival order; with `queued`, every message
    /// still `pending_local` or `native_accepted` too (`stop --discard-queued`, 1.69.3).
    fn held_ids(&self, conversation: &str, queued: bool) -> Result<Vec<String>>;
    /// A receipt by id; a retired id is `receipt_retired`, never `not_found`.
    fn lookup(&self, id: &str) -> Result<Receipt> {
        match self.get(id)? {
            Some(r) => Ok(r),
            None if self.retired(id)?.is_some() => Err(Fault("receipt_retired")),
            None => Err(Fault("receipt_not_found")),
        }
    }
}

/// Read-only queries (no statement here writes; `guard::ledger_read_is_read_only`).
mod read {
    use super::*;
    pub(super) fn get(db: &Connection, id: &str) -> Result<Option<Receipt>> {
        let text: Option<String> = db
            .query_row("SELECT receipt FROM receipts WHERE id=?", [id], |r| {
                r.get(0)
            })
            .optional()?;
        text.map(|s| serde_json::from_str(&s).map_err(|_| Fault("storage_error")))
            .transpose()
    }
    pub(super) fn retired(db: &Connection, id: &str) -> Result<Option<Tombstone>> {
        Ok(db
            .query_row(
                "SELECT state,reason,retired_at FROM retired_messages WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }
    pub(super) fn binding(db: &Connection, id: &str) -> Result<Option<String>> {
        Ok(db
            .query_row(
                "SELECT binding FROM public_messages WHERE id=?",
                [id],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub(super) fn mode_record(db: &Connection, id: &str) -> Result<bool> {
        Ok(db.query_row(
            "SELECT count(*) FROM mode_requests WHERE id=? AND mode IN ('queue','submit')",
            [id],
            |r| r.get::<_, i64>(0),
        )? == 1)
    }
    pub(super) fn expiry(db: &Connection, id: &str) -> Result<i64> {
        Ok(db.query_row("SELECT expiry FROM receipts WHERE id=?", [id], |r| r.get(0))?)
    }
    pub(super) fn held_ids(
        db: &Connection,
        conversation: &str,
        queued: bool,
    ) -> Result<Vec<String>> {
        let mut statement = db.prepare(
            "SELECT r.id FROM receipts r JOIN public_messages p ON p.id=r.id \
             WHERE (?2 AND json_extract(r.receipt,'$.state') IN ('pending_local','native_accepted') \
             OR json_extract(r.receipt,'$.state')='pending_local' \
             AND json_extract(r.receipt,'$.reason') IN ('compaction_hold','compaction_release_retry','model_change_hold')) \
             AND json_extract(p.binding,'$.native.session_id')=?1 ORDER BY p.rowid",
        )?;
        let rows =
            statement.query_map(params![conversation, queued], |row| row.get::<_, String>(0))?;
        Ok(rows.flatten().collect())
    }
    pub(super) fn events(db: &Connection, cursor: Option<&str>) -> Result<Value> {
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
            let receipt: Receipt =
                serde_json::from_str(&text).map_err(|_| Fault("storage_error"))?;
            events.push(json!({"cursor":format!("{epoch}:{seq}"),"receipt":receipt}));
        }
        Ok(
            json!({"events":events,"cursor":format!("{epoch}:{last}"),"has_more":last<max,"public_replies":"correlated_terminal_summary"}),
        )
    }
}

/// Shared (read-only) access to the ledger. It has no write method, and its
/// connection is `query_only`: a write through it fails instead of racing a reader.
pub(super) struct LedgerRead {
    db: Connection,
    _lease: Lease,
}

impl Reads for LedgerRead {
    fn get(&self, id: &str) -> Result<Option<Receipt>> {
        read::get(&self.db, id)
    }
    fn retired(&self, id: &str) -> Result<Option<Tombstone>> {
        read::retired(&self.db, id)
    }
    fn binding(&self, id: &str) -> Result<Option<String>> {
        read::binding(&self.db, id)
    }
    fn mode_record(&self, id: &str) -> Result<bool> {
        read::mode_record(&self.db, id)
    }
    fn expiry(&self, id: &str) -> Result<i64> {
        read::expiry(&self.db, id)
    }
    fn events(&self, cursor: Option<&str>) -> Result<Value> {
        read::events(&self.db, cursor)
    }
    fn held_ids(&self, conversation: &str, queued: bool) -> Result<Vec<String>> {
        read::held_ids(&self.db, conversation, queued)
    }
}

/// Exclusive (writer) access to the ledger.
pub(super) struct Ledger {
    // Declared before `lease`: the connection closes before the lock is released.
    pub(super) db: Connection,
    pub(super) paths: Paths,
    caller: &'static Location<'static>,
    lease: Option<Lease>,
    /// True while the writer lease is released around provider I/O.
    suspended: bool,
}

/// Open the ledger for reading, waiting up to `budget` while a writer holds it.
#[track_caller]
pub(super) fn open_read(
    paths: &Paths,
    budget: Duration,
) -> impl Future<Output = Result<LedgerRead>> + '_ {
    let caller = Location::caller();
    async move {
        let (db, lease) = acquire(paths, Access::Read, budget, caller).await?;
        Ok(LedgerRead { db, _lease: lease })
    }
}
/// Blocking [`open_read`] for sync callers.
#[track_caller]
pub(super) fn open_read_blocking(paths: &Paths, budget: Duration) -> Result<LedgerRead> {
    let (db, lease) = acquire_blocking(paths, Access::Read, budget, Location::caller())?;
    Ok(LedgerRead { db, _lease: lease })
}
/// Open the ledger for writing, waiting up to `budget` for readers and writers.
#[track_caller]
pub(super) fn open_write(
    paths: &Paths,
    budget: Duration,
) -> impl Future<Output = Result<Ledger>> + '_ {
    let caller = Location::caller();
    async move { Ledger::wait(paths, budget, caller).await }
}
/// Blocking [`open_write`] for sync callers (async paths use `open_write`). Never
/// bypasses the lock.
#[track_caller]
pub(super) fn open_write_blocking(paths: &Paths, budget: Duration) -> Result<Ledger> {
    let caller = Location::caller();
    let (db, lease) = acquire_blocking(paths, Access::Write, budget, caller)?;
    Ok(Ledger::new(paths, db, lease, caller))
}

#[derive(Clone, Copy, PartialEq)]
enum Access {
    Read,
    Write,
}
impl Access {
    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// Timings of one open (one stats line).
struct Stats {
    root: std::path::PathBuf,
    access: Access,
    caller: &'static Location<'static>,
    started: Instant,
    acquired: Option<Instant>,
    attempts: u32,
    open: Duration,
    schema: Duration,
    migrated: bool,
}
impl Stats {
    fn new(paths: &Paths, access: Access, caller: &'static Location<'static>) -> Self {
        Self {
            root: paths.root.clone(),
            access,
            caller,
            started: Instant::now(),
            acquired: None,
            attempts: 0,
            open: Duration::ZERO,
            schema: Duration::ZERO,
            migrated: false,
        }
    }
    fn line(&self, outcome: &str, hold: Option<Duration>) -> String {
        let ms = |d: Duration| (d.as_secs_f64() * 1e6).round() / 1e3;
        let waited = self.acquired.unwrap_or_else(Instant::now) - self.started;
        let lock_wait = waited.saturating_sub(self.open + self.schema);
        let mut caller = format!("{}:{}", self.caller.file(), self.caller.line());
        if caller.len() > 160 {
            caller = caller.split_off(caller.len() - 160);
        }
        let mut line = json!({
            "at": crate::daemon::now(),
            "pid": std::process::id(),
            "op": self.access.name(),
            "caller": caller,
            "outcome": outcome,
            "attempts": self.attempts,
            "lock_wait_ms": ms(lock_wait),
            "open_ms": ms(self.open),
            "schema_ms": ms(self.schema),
            "migrated": self.migrated,
            "hold_ms": hold.map(ms),
        })
        .to_string();
        line.push('\n');
        line
    }
    /// One `write(2)` of a line under 512 bytes on an `O_APPEND` descriptor: lines
    /// from concurrent processes never interleave. Best effort; takes no lock.
    fn append(&self, outcome: &str, hold: Option<Duration>) {
        let line = self.line(outcome, hold);
        debug_assert!(line.len() < 512, "{line}");
        let file = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root.join(layout::LEDGER_STATS));
        if let Ok(mut file) = file {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// The held adapter lock. Dropping it releases the lock, then appends the stats line.
struct Lease {
    lock: Option<Lock>,
    stats: Stats,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let hold = self.stats.acquired.map(|at| at.elapsed());
        drop(self.lock.take());
        self.stats.append("ok", hold);
    }
}

/// How long a connection waits for SQLite's own file locks (a reader rolling back a
/// hot journal, a checkpoint of another build) before the open counts as busy.
const SQLITE_BUSY: Duration = Duration::from_millis(250);

/// A SQLite failure during open: lock contention (`SQLITE_BUSY`/`LOCKED`, for
/// example readers racing to roll back a hot journal) is `adapter_busy` and so
/// retried within the caller's budget; anything else is `storage_error`.
fn sql(e: rusqlite::Error) -> Fault {
    match e.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
            Fault("adapter_busy")
        }
        _ => Fault("storage_error"),
    }
}

/// Open `receipts.db` (a regular file we own, never through a symlink) with the
/// per-connection settings and return it with its `user_version`; `None` when it
/// does not exist and `create` is false. We never open a descriptor of our own on
/// the database: closing one would drop every POSIX lock this process holds on it,
/// including the SHARED locks of other reader connections. `create` is only passed
/// under the exclusive lock, when no ledger connection can be open.
fn connect(paths: &Paths, create: bool) -> Result<Option<(Connection, i64)>> {
    let path = paths.root.join(layout::RECEIPTS_DB);
    match fs::symlink_metadata(&path) {
        Ok(m) if !m.file_type().is_file() || m.uid() != unsafe { libc::geteuid() } => {
            return Err(Fault("storage_error"));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !create => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Mode 0600 from the start; SQLite would create it with the umask.
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
            {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(Fault("storage_error")),
            }
        }
        Err(_) => return Err(Fault("storage_error")),
    }
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
        | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let db = Connection::open_with_flags(&path, flags).map_err(sql)?;
    #[cfg(test)]
    trace::install(&db);
    db.busy_timeout(SQLITE_BUSY).map_err(sql)?;
    db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
        .map_err(sql)?;
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(sql)?;
    Ok(Some((db, version)))
}

/// The schema batch and the epoch row, stamped with `SCHEMA_VERSION`, in one
/// transaction. Caller holds the lock exclusively.
fn migrate(db: &mut Connection, stats: &mut Stats) -> Result<()> {
    let started = Instant::now();
    (|| {
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(SCHEMA)?;
        tx.execute(
            "INSERT INTO metadata SELECT ? WHERE NOT EXISTS (SELECT 1 FROM metadata)",
            [uuid::Uuid::new_v4().to_string()],
        )?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()
    })()
    .map_err(sql)?;
    stats.schema += started.elapsed();
    stats.migrated = true;
    Ok(())
}

fn take(paths: &Paths, exclusive: bool) -> Result<Lock> {
    let lock = Lock::open(paths, LockKind::Adapter).map_err(|_| Fault("storage_error"))?;
    let held = if exclusive {
        lock.try_exclusive()
    } else {
        lock.try_shared()
    };
    match held {
        Ok(true) => Ok(lock),
        // As before S2, any failure to take the lock (contention or a transient
        // flock error) is `adapter_busy` and retried within the budget.
        Ok(false) | Err(_) => Err(Fault("adapter_busy")),
    }
}

/// One attempt; never waits (`adapter_busy` when the lock is held against us).
fn attempt(paths: &Paths, access: Access, stats: &mut Stats) -> Result<(Connection, Lock)> {
    stats.attempts += 1;
    private_dir(&paths.root.join("messaging")).map_err(|_| Fault("storage_error"))?;
    // A shared open that finds an older (or no) schema migrates under an exclusive
    // lock and then opens shared again; the second pass sees the current version.
    for _ in 0..2 {
        let write = access == Access::Write;
        let lock = take(paths, write)?;
        let opened = Instant::now();
        let found = connect(paths, write)?;
        stats.open += opened.elapsed();
        if let Some((mut db, version)) = found
            && (version >= SCHEMA_VERSION || write)
        {
            if version < SCHEMA_VERSION {
                migrate(&mut db, stats)?;
            }
            if !write {
                db.pragma_update(None, "query_only", true).map_err(sql)?;
            }
            stats.acquired = Some(Instant::now());
            return Ok((db, lock));
        }
        // Never upgrade in place: drop the shared lock, take it exclusive, re-check.
        drop(lock);
        let lock = take(paths, true)?;
        if let Some((mut db, version)) = connect(paths, true)?
            && version < SCHEMA_VERSION
        {
            migrate(&mut db, stats)?;
        }
        drop(lock);
    }
    Err(Fault("storage_error"))
}

fn lease(stats: Stats, lock: Lock) -> Lease {
    Lease {
        lock: Some(lock),
        stats,
    }
}

async fn acquire(
    paths: &Paths,
    access: Access,
    budget: Duration,
    caller: &'static Location<'static>,
) -> Result<(Connection, Lease)> {
    let mut stats = Stats::new(paths, access, caller);
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match attempt(paths, access, &mut stats) {
            Err(Fault("adapter_busy")) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(RETRY).await;
            }
            Ok((db, lock)) => return Ok((db, lease(stats, lock))),
            Err(error) => {
                stats.append(error.0, None);
                return Err(error);
            }
        }
    }
}

fn acquire_blocking(
    paths: &Paths,
    access: Access,
    budget: Duration,
    caller: &'static Location<'static>,
) -> Result<(Connection, Lease)> {
    let mut stats = Stats::new(paths, access, caller);
    let deadline = Instant::now() + budget;
    loop {
        match attempt(paths, access, &mut stats) {
            Err(Fault("adapter_busy")) if Instant::now() < deadline => {
                std::thread::sleep(RETRY);
            }
            Ok((db, lock)) => return Ok((db, lease(stats, lock))),
            Err(error) => {
                stats.append(error.0, None);
                return Err(error);
            }
        }
    }
}

/// Keep `ledger-stats.jsonl` bounded: above [`STATS_MAX`] it is replaced by its last
/// [`STATS_KEEP`] bytes (from the first whole line). Lines appended during the
/// replacement may be lost; they are statistics only.
pub(super) fn trim_stats(paths: &Paths) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let path = paths.root.join(layout::LEDGER_STATS);
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    if len <= STATS_MAX {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(len - STATS_KEEP))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    let start = tail
        .iter()
        .position(|&b| b == b'\n')
        .map_or(tail.len(), |i| i + 1);
    crate::paths::atomic_write(&path, &tail[start..]).map_err(std::io::Error::other)?;
    Ok(true)
}

impl Ledger {
    fn new(
        paths: &Paths,
        db: Connection,
        lease: Lease,
        caller: &'static Location<'static>,
    ) -> Self {
        Self {
            db,
            paths: paths.clone(),
            caller,
            lease: Some(lease),
            suspended: false,
        }
    }
    async fn wait(
        paths: &Paths,
        budget: Duration,
        caller: &'static Location<'static>,
    ) -> Result<Self> {
        let (db, lease) = acquire(paths, Access::Write, budget, caller).await?;
        Ok(Self::new(paths, db, lease, caller))
    }
    /// Run provider I/O without the root-wide lease. The lease is always taken
    /// again (waiting up to `budget`) before returning, so the caller must treat
    /// every ledger read made before the call as stale and re-verify it.
    pub(super) async fn during<T>(
        &mut self,
        budget: Duration,
        io: impl Future<Output = T>,
    ) -> Result<T> {
        // Close the connection before releasing the lock.
        self.db = Connection::open_in_memory()?;
        self.lease = None;
        self.suspended = true;
        let out = io.await;
        let paths = self.paths.clone();
        *self = Self::wait(&paths, budget, self.caller).await?;
        Ok(out)
    }
    /// After `during`, tell whether another writer changed this receipt. When it
    /// did, adopt the stored record and report `false`: our stale result must not
    /// regress it.
    pub(super) fn amend(
        &mut self,
        receipt: &mut Receipt,
        edit: impl FnOnce(&mut Receipt),
    ) -> Result<()> {
        if self.unchanged(receipt)? {
            edit(receipt);
            self.save(receipt)?;
        }
        Ok(())
    }
    pub(super) fn unchanged(&self, receipt: &mut Receipt) -> Result<bool> {
        match self.get(&receipt.message_id)? {
            Some(current) if current == *receipt => Ok(true),
            Some(current) => {
                *receipt = current;
                Ok(false)
            }
            None => Ok(false),
        }
    }
    pub(super) fn save(&mut self, receipt: &Receipt) -> Result<()> {
        if self.suspended {
            return Err(Fault("adapter_busy"));
        }
        let data = serde_json::to_string(receipt).map_err(|_| Fault("storage_error"))?;
        let tx = self.db.transaction()?;
        tx.execute(
            "UPDATE receipts SET receipt=? WHERE id=?",
            params![data, receipt.message_id],
        )?;
        tx.execute("INSERT INTO events(receipt) VALUES (?)", [data])?;
        tx.execute(
            "DELETE FROM events WHERE seq <= (SELECT coalesce(max(seq),0)-1000 FROM events)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(super) fn accept(&mut self, input: &Enqueue, now: i64) -> Result<Receipt> {
        self.accept_with_mode(input, now, None)
    }
    pub(super) fn accept_with_mode(
        &mut self,
        input: &Enqueue,
        now: i64,
        mode_hash: Option<&str>,
    ) -> Result<Receipt> {
        self.accept_bound(input, now, mode_hash.map(|hash| (hash, "queue", None)))
    }
    pub(super) fn accept_bound(
        &mut self,
        input: &Enqueue,
        now: i64,
        binding: Option<(&str, &str, Option<SubmitEvidence>)>,
    ) -> Result<Receipt> {
        // A public steering intent reserves its UUID even before receipt creation.
        let has_public: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='public_messages')", [], |r|r.get(0))?;
        if has_public {
            let public_mode: Option<String> = self
                .db
                .query_row(
                    "SELECT json_extract(binding,'$.request.mode') FROM public_messages WHERE id=?",
                    [&input.message_id],
                    |r| r.get(0),
                )
                .optional()?;
            if public_mode.as_deref() == Some("steer")
                && binding.as_ref().is_none_or(|b| b.1 != "public_steer")
            {
                return Err(Fault("message_id_conflict"));
            }
        }
        let steer = binding.as_ref().is_some_and(|b| b.1 == "public_steer");
        let mode_hash = binding.as_ref().map(|b| b.0);
        uuid(&input.message_id)?;
        uuid(&input.session_id)?;
        if input.text.is_empty()
            || input.text.len() > 16384
            || input.expected_generation.len() != 64
            || !input
                .expected_generation
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Fault("invalid_request"));
        }
        let hash = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(input).map_err(|_| Fault("invalid_request"))?)
        );
        let hash = mode_hash.unwrap_or(&hash).to_owned();
        if let Some(receipt) = self.get(&input.message_id)? {
            let old: String = self.db.query_row(
                "SELECT hash FROM receipts WHERE id=?",
                [&input.message_id],
                |r| r.get(0),
            )?;
            if old != hash {
                return Err(Fault("message_id_conflict"));
            }
            return Ok(receipt);
        }
        if self.retired(&input.message_id)?.is_some() {
            return Err(Fault("receipt_retired"));
        }
        if input.expires_at <= now || input.expires_at > now + 86400 {
            return Err(Fault("invalid_expiry"));
        }
        let count = |l: &Self| {
            l.db.query_row("SELECT count(*) FROM receipts", [], |r| r.get::<_, i64>(0))
        };
        if count(self)? >= retention::NEAR_CAPACITY {
            retention::maybe_run(self, now);
        }
        if count(self)? >= 10000 {
            return Err(Fault("receipt_capacity"));
        }
        let receipt = Receipt {
            recovery_start: None,
            message_id: input.message_id.clone(),
            session_id: input.session_id.clone(),
            generation: input.expected_generation.clone(),
            state: if mode_hash.is_some() && !steer {
                "unknown"
            } else {
                "pending_local"
            }
            .into(),
            native_queue_id: None,
            reason: mode_hash
                .filter(|_| !steer)
                .map(|_| "dispatch_intent".into()),
            correlation: None,
            reply: None,
            submit: binding.as_ref().and_then(|b| b.2.clone()),
            steer: None,
            native_error: None,
        };
        let data = serde_json::to_string(&receipt).map_err(|_| Fault("storage_error"))?;
        let tx = self.db.transaction()?;
        tx.execute(
            "INSERT INTO receipts VALUES (?,?,?,?)",
            params![input.message_id, hash, input.expires_at, data],
        )?;
        if let Some((_, mode, _)) = binding {
            tx.execute(
                "INSERT INTO mode_requests(id,mode) VALUES (?,?)",
                params![input.message_id, mode],
            )?;
        }
        tx.execute("INSERT INTO events(receipt) VALUES (?)", [data])?;
        tx.execute(
            "DELETE FROM events WHERE seq <= (SELECT coalesce(max(seq),0)-1000 FROM events)",
            [],
        )?;
        tx.commit()?;
        Ok(receipt)
    }
    pub(super) fn expire(&mut self, receipt: &mut Receipt, now: i64) -> Result<()> {
        let expiry = self.expiry(&receipt.message_id)?;
        if receipt.state == "pending_local" && expiry <= now {
            receipt.state = "expired".into();
            receipt.reason = None;
            self.save(receipt)?;
        }
        Ok(())
    }
}

impl Reads for Ledger {
    fn get(&self, id: &str) -> Result<Option<Receipt>> {
        if self.suspended {
            return Err(Fault("adapter_busy"));
        }
        read::get(&self.db, id)
    }
    fn retired(&self, id: &str) -> Result<Option<Tombstone>> {
        read::retired(&self.db, id)
    }
    fn binding(&self, id: &str) -> Result<Option<String>> {
        read::binding(&self.db, id)
    }
    fn mode_record(&self, id: &str) -> Result<bool> {
        read::mode_record(&self.db, id)
    }
    fn expiry(&self, id: &str) -> Result<i64> {
        read::expiry(&self.db, id)
    }
    fn events(&self, cursor: Option<&str>) -> Result<Value> {
        read::events(&self.db, cursor)
    }
    fn held_ids(&self, conversation: &str, queued: bool) -> Result<Vec<String>> {
        read::held_ids(&self.db, conversation, queued)
    }
}

#[cfg(test)]
mod tests;
