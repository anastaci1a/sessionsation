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
//!   the native outcome stays unknown and the message must never be resent.
//!   Since 1.52.0 a Claude/Gemini journal record that is written, whose worker has
//!   exited and whose state is `execution_started` (or a positively evidenced
//!   `native_accepted`) counts as delivered and retires with the terminal window,
//!   keeping its real state;
//! - keeps a compact tombstone (id, final state, reason, retirement time) for
//!   every retired id in `retired_messages`. A retired id is never accepted
//!   again: send and receipt both return `RECEIPT_RETIRED` with the final state.
//!   Tombstones are not counted against capacity and are not pruned.
use super::{Fault, Ledger, Reads, Result, ledger::Tombstone};
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
    /// The configured policy; built-in defaults when `config.json` does not load
    /// (`crate::retention::run` reports that error).
    pub(super) fn load(paths: &crate::paths::Paths) -> Self {
        Self::from(&crate::store::load_config(paths).unwrap_or_default())
    }
}
impl From<&crate::config::Config> for Policy {
    fn from(c: &crate::config::Config) -> Self {
        Self {
            terminal_seconds: i64::from(c.receipt_retention_days) * DAY,
            horizon_seconds: i64::from(c.receipt_unknown_horizon_days) * DAY,
        }
    }
}

/// Journal states that count as delivered once the record is written and its worker
/// has exited (contract 1.52.0): retired after the terminal window with their real state.
pub(crate) const DELIVERED_JOURNAL_STATES: [&str; 2] = ["execution_started", "native_accepted"];

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

/// The tombstone of a retired message id, if any.
pub(super) fn retired(ledger: &impl Reads, id: &str) -> Result<Option<Tombstone>> {
    ledger.retired(id)
}

/// `RECEIPT_RETIRED` refusal for a retired id (send or receipt).
pub(super) fn ensure_not_retired(ledger: &impl Reads, id: &str) -> anyhow::Result<()> {
    if let Some((state, reason, at)) = retired(ledger, id)? {
        anyhow::bail!(
            "RECEIPT_RETIRED: message {id} was retired at {at} by receipt retention (final state {state}{}); its id is never accepted again",
            reason.map(|r| format!(", reason {r}")).unwrap_or_default()
        );
    }
    Ok(())
}

/// Entries from which a send runs the pass itself: the supervisor's hourly sweep normally
/// keeps the stores far below it, and a pass scans cold files under the admission lock.
pub(super) const NEAR_CAPACITY: i64 = 9000;

/// Throttled retention pass over every receipt store. Failures never block a
/// send: retention is housekeeping and is retried on a later pass.
pub(super) fn maybe_run(ledger: &mut Ledger, now: i64) {
    let due = (|| -> Result<bool> {
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
    // Ids first (a covering index scan); a binding is read by key only for an orphan.
    let orphans: Vec<String> = {
        let mut s = ledger.db.prepare(
            "SELECT p.id FROM public_messages p WHERE NOT EXISTS (SELECT 1 FROM receipts r WHERE r.id=p.id)",
        )?;
        s.query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?
    };
    for id in orphans {
        let Some(text) = ledger.binding(&id)? else {
            continue;
        };
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
    /// Only `silent_accept` is read; any shape is accepted.
    #[serde(default)]
    facts: serde_json::Value,
}
impl Journal {
    /// Delivered for retention (contract 1.52.0): written (or complete), the message is
    /// in the transcript/turn (`execution_started`) or positively accepted natively
    /// (`native_accepted`, but not a Claude peer frame that only met a silent transport
    /// acceptance, which is not proof the receiver read it). The caller checks the worker.
    fn delivered(&self) -> bool {
        matches!(self.phase.as_str(), "written" | "complete")
            && match self.state.as_str() {
                "execution_started" => true,
                "native_accepted" => self.facts["silent_accept"] != true,
                _ => false,
            }
    }
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
        // A worker still holding its lock is never retired, and its record is not final.
        let lock = path.with_extension("worker.lock");
        let worker_live =
            || lock.exists() && !crate::background::worker_stopped(&lock).unwrap_or(false);
        let terminal_window = j.updated_at < now - policy.terminal_seconds;
        let (state, reason) = if certain {
            if !terminal_window {
                continue;
            }
            (j.state, j.reason)
        } else if terminal_window && j.delivered() && !worker_live() {
            // Delivered (1.52.0): its real state, never the horizon's `expired`.
            (j.state, j.reason)
        } else {
            if j.updated_at >= now - policy.horizon_seconds || worker_live() {
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
        let _ = fs::remove_file(path.with_extension("worker.log"));
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
mod tests;
