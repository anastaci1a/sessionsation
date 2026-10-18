//! Observational history only: never submits work or controls a native process.
use crate::{
    external::{Activity, Instance},
    model::{Account, Usage},
    store::Store,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const HALF_HOUR: i64 = 1800;
pub const RETENTION: i64 = 90 * 86400;

#[cfg(test)]
mod tests;

pub(crate) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_history(
        account TEXT NOT NULL, bucket INTEGER NOT NULL, observed INTEGER,
        short REAL, weekly REAL, short_absent INTEGER NOT NULL, weekly_absent INTEGER NOT NULL,
        PRIMARY KEY(account,bucket));
        CREATE TABLE IF NOT EXISTS activity_history(
        account TEXT NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL,
        active INTEGER NOT NULL, unknown INTEGER NOT NULL, PRIMARY KEY(account,start));
        CREATE INDEX IF NOT EXISTS activity_history_end ON activity_history(account,end);",
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub active: u32,
    pub unknown: u32,
}

pub(crate) fn counts(instances: &[Instance]) -> BTreeMap<String, Counts> {
    let mut threads = BTreeMap::<String, (BTreeSet<String>, BTreeSet<String>)>::new();
    for s in instances
        .iter()
        .filter(|s| !s.inactive && !s.tmux_dead && !s.tmux_only)
    {
        let account = s.display_account.as_ref().unwrap_or(&s.account).clone();
        let key = if s.thread_id.is_empty() {
            s.key()
        } else {
            s.thread_id.clone()
        };
        let (busy, unknown) = threads.entry(account).or_default();
        match s.activity {
            Activity::Busy => {
                busy.insert(key);
            }
            Activity::Unknown => {
                unknown.insert(key);
            }
            _ => {}
        }
    }
    threads
        .into_iter()
        .map(|(a, (busy, unknown))| {
            let count = Counts {
                active: busy.len() as u32,
                unknown: unknown.difference(&busy).count() as u32,
            };
            (a, count)
        })
        .collect()
}

#[derive(Default)]
pub(crate) struct Recorder {
    previous: Option<(i64, BTreeMap<String, Counts>)>,
    pruned_at: i64,
}

impl Recorder {
    pub(crate) fn observe(
        &mut self,
        store: &mut Store,
        accounts: &BTreeMap<String, Account>,
        at: i64,
        stale: u64,
        activity: Option<BTreeMap<String, Counts>>,
    ) -> Result<()> {
        let current = activity.map(|mut counts| {
            accounts
                .keys()
                .map(|name| (name.clone(), counts.remove(name).unwrap_or_default()))
                .collect::<BTreeMap<_, _>>()
        });
        let tx = store.db.transaction()?;
        let bucket = at.div_euclid(HALF_HOUR) * HALF_HOUR;
        for a in accounts.values() {
            let u = &a.usage;
            let fresh = !a.auth_required && u.observed_at >= bucket && u.observed_fresh(at, stale);
            let remaining = |window: &Option<crate::model::Window>| {
                window
                    .as_ref()
                    .filter(|w| {
                        fresh
                            && w.resets_at > at
                            && w.used_percent.is_finite()
                            && (0.0..=100.0).contains(&w.used_percent)
                    })
                    .map(|w| 100.0 - w.used_percent)
            };
            let short = remaining(&u.short);
            let weekly = remaining(&u.weekly);
            let observed =
                (fresh && (short.is_some() || weekly.is_some())).then_some(u.observed_at);
            // First usable reading in each half hour. Retry missing readings, never invent a backfill.
            tx.execute(
                "INSERT INTO usage_history VALUES(?1,?2,?3,?4,?5,?6,?7)
                ON CONFLICT(account,bucket) DO UPDATE SET observed=excluded.observed,
                short=excluded.short,weekly=excluded.weekly,short_absent=excluded.short_absent,
                weekly_absent=excluded.weekly_absent WHERE usage_history.observed IS NULL",
                params![
                    a.name,
                    bucket,
                    observed,
                    short,
                    weekly,
                    fresh && u.window_absent(300),
                    fresh && u.window_absent(10080)
                ],
            )?;
        }
        if let (Some((previous, before)), Some(after)) = (
            self.previous
                .as_ref()
                .filter(|(previous, _)| at > *previous && at - previous <= 90),
            &current,
        ) {
            for (name, count) in before {
                if !after.contains_key(name) {
                    continue;
                }
                // A wall-clock correction or restart must never overlap stored durations.
                let latest: Option<i64> = tx.query_row(
                    "SELECT MAX(end) FROM activity_history WHERE account=?1",
                    [name],
                    |r| r.get(0),
                )?;
                let from = (*previous).max(latest.unwrap_or(*previous));
                if from >= at {
                    continue;
                }
                let updated = tx.execute("UPDATE activity_history SET end=?1 WHERE account=?2 AND end=?3 AND active=?4 AND unknown=?5 AND start>=?6",
                    params![at,name,from,count.active,count.unknown,bucket-HALF_HOUR])?;
                if updated == 0 {
                    tx.execute(
                        "INSERT OR IGNORE INTO activity_history VALUES(?1,?2,?3,?4,?5)",
                        params![name, from, at, count.active, count.unknown],
                    )?;
                }
            }
        }
        if at - self.pruned_at >= 86400 {
            tx.execute(
                "DELETE FROM usage_history WHERE bucket<?1",
                [at - RETENTION],
            )?;
            tx.execute(
                "DELETE FROM activity_history WHERE end<?1",
                [at - RETENTION],
            )?;
        }
        tx.commit()?;
        if at - self.pruned_at >= 86400 {
            self.pruned_at = at;
        }
        self.previous = current.map(|c| (at, c));
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Bin {
    pub start: i64,
    pub end: i64,
    pub short: Option<f64>,
    pub weekly: Option<f64>,
    pub observed_at: Option<i64>,
    pub samples: u32,
    pub active_seconds: i64,
    pub session_seconds: i64,
    pub observed_seconds: i64,
    pub unknown_seconds: i64,
    pub peak: u32,
    pub periods: Vec<Period>,
    pub omitted_periods: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Period {
    pub start: i64,
    pub end: i64,
    pub active: u32,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct History {
    pub account: String,
    pub short_absent: bool,
    pub weekly_absent: bool,
    pub bins: Vec<Bin>,
}

pub(crate) fn query(
    db: &Connection,
    account: &str,
    usage: &Usage,
    start: i64,
    end: i64,
    step: i64,
) -> Result<History> {
    ensure!(
        start >= 0
            && end > start
            && end <= i64::MAX - RETENTION
            && end - start <= RETENTION
            && (HALF_HOUR..=RETENTION).contains(&step)
            && step % HALF_HOUR == 0
            && (end - start + step - 1) / step <= 168,
        "invalid history range (maximum 90 days / 168 bars)"
    );
    let mut bins: Vec<_> = (start..end)
        .step_by(step as usize)
        .map(|start| Bin {
            start,
            end: (start + step).min(end),
            ..Default::default()
        })
        .collect();
    let mut stmt = db.prepare("SELECT bucket,observed,short,weekly FROM usage_history WHERE account=?1 AND bucket>=?2 AND bucket<?3 ORDER BY bucket")?;
    let rows = stmt.query_map(params![account, start, end], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<i64>>(1)?,
            r.get::<_, Option<f64>>(2)?,
            r.get::<_, Option<f64>>(3)?,
        ))
    })?;
    for r in rows {
        let (bucket, observed, short, weekly) = r?;
        let bin = &mut bins[((bucket - start) / step) as usize];
        if observed.is_some() {
            bin.samples += 1;
        }
        // The last scheduled sample represents the bar; missing tails stay missing.
        bin.short = short;
        bin.weekly = weekly;
        bin.observed_at = observed;
    }
    for bin in &mut bins {
        if bin
            .observed_at
            .is_some_and(|at| at / HALF_HOUR < (bin.end - 1) / HALF_HOUR)
        {
            bin.short = None;
            bin.weekly = None;
            bin.observed_at = None;
        }
    }
    let mut stmt = db.prepare("SELECT start,end,active,unknown FROM activity_history WHERE account=?1 AND start<?3 AND end>?2 ORDER BY start")?;
    let rows = stmt.query_map(params![account, start, end], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, u32>(2)?,
            r.get::<_, u32>(3)?,
        ))
    })?;
    for r in rows {
        let (from, to, active, unknown) = r?;
        let first = ((from.max(start) - start) / step) as usize;
        let last = ((to.min(end) - 1 - start) / step) as usize;
        for bin in &mut bins[first..=last] {
            let from = from.max(bin.start);
            let to = to.min(bin.end);
            let seconds = to - from;
            bin.observed_seconds += seconds;
            if unknown > 0 {
                bin.unknown_seconds += seconds;
            }
            bin.peak = bin.peak.max(active);
            bin.session_seconds += seconds * i64::from(active);
            if active > 0 {
                bin.active_seconds += seconds;
                if let Some(last) = bin
                    .periods
                    .last_mut()
                    .filter(|p| p.end == from && p.active == active)
                {
                    last.end = to;
                } else if bin.periods.len() < 64 {
                    bin.periods.push(Period {
                        start: from,
                        end: to,
                        active,
                    });
                } else {
                    bin.omitted_periods += 1;
                }
            }
        }
    }
    Ok(History {
        account: account.into(),
        short_absent: usage.window_absent(300),
        weekly_absent: usage.window_absent(10080),
        bins,
    })
}

pub(crate) async fn monitor(
    paths: crate::paths::Paths,
    tx: tokio::sync::mpsc::Sender<(i64, Option<BTreeMap<String, Counts>>)>,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            crate::external::live_sessions(&paths),
        )
        .await;
        if tx
            .send((
                crate::daemon::now(),
                result.ok().and_then(Result::ok).map(|s| counts(&s)),
            ))
            .await
            .is_err()
        {
            break;
        }
    }
}
