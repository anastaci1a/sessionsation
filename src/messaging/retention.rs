//! Receipt retention (contract revision 18). Receipts are bounded stores
//! (10,000 entries each); without retention every send eventually fails with
//! `RECEIPT_CAPACITY_REACHED`. A pass, at most once per hour and only under the
//! exclusive adapter lock:
//!
//! - retires certain-terminal receipts older than `receipt_retention_days`
//!   (default 14), measured from the submission deadline (`expires_at`) or,
//!   for Claude/Gemini journals, from the last record update;
//! - never retires a non-terminal or uncertain receipt (`unknown`,
//!   `native_accepted`, `execution_started`, `pending_local`,
//!   `delivery_unknown`) before `receipt_unknown_horizon_days` (default 90).
//!   Past that horizon it is first recorded as terminal `expired` with reason
//!   `outcome_unknown_at_retention_horizon`: Sessionsation stopped observing,
//!   the native outcome stays unknown and the message must never be resent;
//! - keeps a compact tombstone (id, final state, reason, retirement time) for
//!   every retired id in `retired_messages`. A retired id is never accepted
//!   again: send and receipt both return `RECEIPT_RETIRED` with the final state.
//!   Tombstones are not counted against capacity and are not pruned.
use super::{Fault, Ledger, Result};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use std::{fs, path::Path};

pub(super) const HORIZON_REASON: &str = "outcome_unknown_at_retention_horizon";
const INTERVAL_SECONDS: i64 = 3600;
const DAY: i64 = 86400;

#[derive(Clone, Copy, Debug)]
pub(super) struct Policy {
    pub terminal_seconds: i64,
    pub horizon_seconds: i64,
}
impl Policy {
    pub(super) fn load(paths: &crate::paths::Paths) -> Self {
        let c = crate::store::load_config(paths).unwrap_or_default();
        Self {
            terminal_seconds: i64::from(c.receipt_retention_days) * DAY,
            horizon_seconds: i64::from(c.receipt_unknown_horizon_days) * DAY,
        }
    }
}

/// Codex ledger states that are final and certain.
pub(super) fn certain_terminal(state: &str) -> bool {
    matches!(
        state,
        "completed_with_reply"
            | "completed_without_reply"
            | "failed"
            | "rejected"
            | "cancelled"
            | "expired"
    )
}

pub(super) fn initialize(ledger: &Ledger) -> Result<()> {
    ledger.db.execute_batch(
        "CREATE TABLE IF NOT EXISTS retired_messages(id TEXT PRIMARY KEY, state TEXT NOT NULL, reason TEXT, retired_at INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS retention_runs(store TEXT PRIMARY KEY, last_run INTEGER NOT NULL);",
    )?;
    Ok(())
}

/// The tombstone of a retired message id, if any.
pub(super) fn retired(ledger: &Ledger, id: &str) -> Result<Option<(String, Option<String>, i64)>> {
    initialize(ledger)?;
    Ok(ledger
        .db
        .query_row(
            "SELECT state,reason,retired_at FROM retired_messages WHERE id=?",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
}

/// `RECEIPT_RETIRED` refusal for a retired id (send or receipt).
pub(super) fn ensure_not_retired(ledger: &Ledger, id: &str) -> anyhow::Result<()> {
    if let Some((state, reason, at)) = retired(ledger, id)? {
        anyhow::bail!(
            "RECEIPT_RETIRED: message {id} was retired at {at} by receipt retention (final state {state}{}); its id is never accepted again",
            reason.map(|r| format!(", reason {r}")).unwrap_or_default()
        );
    }
    Ok(())
}

/// Throttled retention pass over every receipt store. Failures never block a
/// send: retention is housekeeping and is retried on a later pass.
pub(super) fn maybe_run(ledger: &mut Ledger, now: i64) {
    let due = (|| -> Result<bool> {
        initialize(ledger)?;
        let last: Option<i64> = ledger
            .db
            .query_row(
                "SELECT last_run FROM retention_runs WHERE store='all'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(last.is_none_or(|t| now - t >= INTERVAL_SECONDS || t > now))
    })();
    if !due.unwrap_or(false) {
        return;
    }
    let policy = Policy::load(&ledger.paths.clone());
    let _ = run(ledger, now, policy);
}

/// One unthrottled pass; returns the number of retired ids.
pub(super) fn run(ledger: &mut Ledger, now: i64, policy: Policy) -> Result<usize> {
    initialize(ledger)?;
    super::public::initialize(ledger)?;
    let mut retired = codex(ledger, now, policy)?;
    let root = ledger.paths.root.clone();
    for store in [
        crate::layout::CLAUDE_STEER,
        crate::layout::CLAUDE_DELIVERIES,
    ] {
        retired += files(ledger, &root.join(store), now, policy)?;
    }
    ledger.db.execute(
        "INSERT INTO retention_runs(store,last_run) VALUES('all',?) ON CONFLICT(store) DO UPDATE SET last_run=excluded.last_run",
        [now],
    )?;
    Ok(retired)
}

fn tombstone(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    state: &str,
    reason: Option<&str>,
    now: i64,
) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO retired_messages(id,state,reason,retired_at) VALUES(?,?,?,?)",
        params![id, state, reason, now],
    )?;
    Ok(())
}

fn codex(ledger: &mut Ledger, now: i64, policy: Policy) -> Result<usize> {
    let old: Vec<(String, String, i64)> = {
        let mut s = ledger
            .db
            .prepare("SELECT id,receipt,expiry FROM receipts WHERE expiry < ? ORDER BY expiry")?;
        s.query_map(
            [now - policy.terminal_seconds.min(policy.horizon_seconds)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
        .collect::<std::result::Result<_, _>>()?
    };
    let mut retire = Vec::new();
    for (id, text, expiry) in old {
        let Some(mut receipt) = serde_json::from_str::<super::Receipt>(&text).ok() else {
            continue; // Unreadable evidence is kept, never discarded.
        };
        if !certain_terminal(&receipt.state) {
            if expiry >= now - policy.horizon_seconds {
                continue;
            }
            // Honest terminal state first, durably and as an event.
            if receipt.state == "pending_local" {
                receipt.reason = None; // Provably never dispatched.
            } else {
                receipt.reason = Some(HORIZON_REASON.into());
            }
            receipt.state = "expired".into();
            ledger.save(&receipt)?;
        } else if expiry >= now - policy.terminal_seconds {
            continue;
        }
        retire.push((id, receipt.state, receipt.reason));
    }
    // One durable transaction: tombstones and deletions land together.
    let tx = ledger.db.transaction()?;
    for (id, state, reason) in &retire {
        tombstone(&tx, id, state, reason.as_deref(), now)?;
        tx.execute("DELETE FROM receipts WHERE id=?", [id])?;
        tx.execute("DELETE FROM public_messages WHERE id=?", [id])?;
        tx.execute("DELETE FROM mode_requests WHERE id=?", [id])?;
    }
    tx.commit()?;
    let mut retired = retire.len();
    // Public bindings that never produced a receipt: provably undispatched.
    let orphans: Vec<(String, String)> = {
        let mut s = ledger.db.prepare(
            "SELECT id,binding FROM public_messages WHERE id NOT IN (SELECT id FROM receipts)",
        )?;
        s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?
    };
    for (id, text) in orphans {
        #[derive(Deserialize)]
        struct Orphan {
            cancelled: bool,
            request: OrphanRequest,
        }
        #[derive(Deserialize)]
        struct OrphanRequest {
            expires_at: i64,
        }
        let Ok(b) = serde_json::from_str::<Orphan>(&text) else {
            continue;
        };
        let limit = if b.cancelled {
            policy.terminal_seconds
        } else {
            policy.horizon_seconds
        };
        // A resume binding is owned by its recovery journal until it binds.
        if b.request.expires_at >= now - limit || super::recovery::exists(&ledger.paths, &id) {
            continue;
        }
        let tx = ledger.db.transaction()?;
        tombstone(
            &tx,
            &id,
            if b.cancelled { "cancelled" } else { "expired" },
            None,
            now,
        )?;
        tx.execute("DELETE FROM public_messages WHERE id=?", [&id])?;
        tx.commit()?;
        retired += 1;
    }
    Ok(retired)
}

/// Fields shared by the Claude/Gemini receipt journals.
#[derive(Deserialize)]
struct Journal {
    phase: String,
    state: String,
    reason: Option<String>,
    updated_at: i64,
    request: JournalRequest,
}
#[derive(Deserialize)]
struct JournalRequest {
    message_id: String,
}

fn files(ledger: &mut Ledger, dir: &Path, now: i64, policy: Policy) -> Result<usize> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(0);
    };
    let mut retired = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if uuid::Uuid::parse_str(stem).is_err() || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Some(j) = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Journal>(&b).ok())
        else {
            continue;
        };
        if j.request.message_id != stem {
            continue;
        }
        let certain =
            j.phase == "complete" && !matches!(j.state.as_str(), "delivery_unknown" | "unknown");
        let (state, reason) = if certain {
            if j.updated_at >= now - policy.terminal_seconds {
                continue;
            }
            (j.state, j.reason)
        } else {
            if j.updated_at >= now - policy.horizon_seconds {
                continue;
            }
            // A worker still holding its lock is never retired.
            let lock = path.with_extension("worker.lock");
            if lock.exists() && !crate::background::worker_stopped(&lock).unwrap_or(false) {
                continue;
            }
            ("expired".to_owned(), Some(HORIZON_REASON.to_owned()))
        };
        let tx = ledger.db.transaction()?;
        tombstone(&tx, stem, &state, reason.as_deref(), now)?;
        tx.commit()?;
        // Tombstone first: an interrupted pass leaves the id blocked either way.
        fs::remove_file(&path).map_err(|_| Fault("local_io_error"))?;
        let _ = fs::remove_file(path.with_extension("worker.lock"));
        retired += 1;
    }
    Ok(retired)
}

/// Capacity of a file journal without reading any record.
pub(super) fn count_journal(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .count()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::{Enqueue, Receipt, tests::fixture};
    const POLICY: Policy = Policy {
        terminal_seconds: 14 * DAY,
        horizon_seconds: 90 * DAY,
    };
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
        super::super::public::initialize(&db).unwrap();
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
        initialize(&db).unwrap();
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
}
