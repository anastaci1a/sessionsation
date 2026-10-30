//! Data-root retention (layout version 2). One pass covers four classes, each
//! configurable in `config.json` (days are 1..=3650):
//!
//! - logs (`logs/*.log`): a log larger than `log_max_bytes` (default 8 MiB) is
//!   copied to `<name>.1` and truncated in place (writers append, so they keep
//!   their descriptor); a rotated `.1` older than `log_retention_days`
//!   (default 30) is removed.
//! - cache (`cache/observations/`): entries untouched for `cache_retention_days`
//!   (default 14) are removed; they are regenerated on demand. State snapshots
//!   and the compatibility report are current files and are never removed.
//! - operations history: a journal in a certain final phase, untouched for
//!   `operation_retention_days` (default 90), is removed with its sidecar files
//!   (locks, logs, responses), unless a lock sidecar is held. Never removed:
//!   unfinished or `uncertain` journals (never replayed, so they stay as
//!   evidence), session deletion tombstones (`deletions/`), account deletion
//!   records (`account-deletions/`), lifecycle admission locks and move backups.
//! - receipts: the existing receipt retention (`receipt_retention_days`,
//!   `receipt_unknown_horizon_days`, tombstones in `retired_messages`) runs as
//!   part of the same pass.
//!
//! The supervisor runs the pass at most hourly; `sessionsation retention run`
//! runs it now (`--dry-run` lists what would be removed and skips receipts).
use crate::{layout, paths::Paths};
use anyhow::Result;
use fs2::FileExt;
use serde_json::{Value, json};
use std::{fs, path::Path, time::SystemTime};

const DAY: u64 = 86400;

/// Final phases per operation directory; anything else is kept.
pub const PRUNABLE: &[(&str, &[&str])] = &[
    (layout::LAUNCHES, &["running", "failed"]),
    (layout::RESUMPTIONS, &["running", "failed", "abandoned"]),
    (layout::CONVERSATION_OPERATIONS, &["complete", "failed"]),
    (
        layout::HANDOFF_JOBS,
        &["complete", "failed", "cancelled", "recovered"],
    ),
    (layout::SPACE_MOVES, &["complete", "failed"]),
    (layout::MESSAGE_RECOVERIES, &["complete", "failed"]),
];

#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub log_days: u32,
    pub log_max_bytes: u64,
    pub cache_days: u32,
    pub operation_days: u32,
}
impl Policy {
    pub fn load(paths: &Paths) -> Self {
        Self::from(&crate::store::load_config(paths).unwrap_or_default())
    }
}
impl From<&crate::config::Config> for Policy {
    fn from(c: &crate::config::Config) -> Self {
        Self {
            log_days: c.log_retention_days,
            log_max_bytes: c.log_max_bytes,
            cache_days: c.cache_retention_days,
            operation_days: c.operation_retention_days,
        }
    }
}

/// Public description for `api capabilities` (built-in defaults; the
/// configured values are in `config.json`).
pub fn describe() -> Value {
    let c = crate::config::Config::default();
    json!({
        "since_contract": "1.29.0",
        "interval_seconds": 3600,
        "command": "retention run [--dry-run]",
        "logs": {"setting_days":"log_retention_days","default_days":c.log_retention_days,"setting_max_bytes":"log_max_bytes","default_max_bytes":c.log_max_bytes,"rotated_suffix":".1","paths":[layout::LOGS]},
        "cache": {"setting":"cache_retention_days","default_days":c.cache_retention_days,"paths":[layout::OBSERVATIONS]},
        "operations": {"setting":"operation_retention_days","default_days":c.operation_retention_days,"final_phases":PRUNABLE.iter().map(|(d, p)| (d.to_string(), json!(p))).collect::<serde_json::Map<_, _>>(),"never":[layout::DELETIONS,layout::ACCOUNT_DELETIONS,layout::LIFECYCLE_ADMISSION,layout::MOVE_BACKUPS],"unfinished_or_uncertain":"kept"},
        "receipts": {"setting":"receipt_retention_days","default_days":c.receipt_retention_days,"unknown_horizon_setting":"receipt_unknown_horizon_days","default_unknown_horizon_days":c.receipt_unknown_horizon_days,"tombstones":"kept"},
    })
}

fn age_days(meta: &fs::Metadata, now: SystemTime) -> u64 {
    meta.modified()
        .ok()
        .and_then(|m| now.duration_since(m).ok())
        .map_or(0, |d| d.as_secs() / DAY)
}

/// One pass over logs, cache and operations history (not receipts).
pub fn sweep(root: &Path, policy: Policy, now: SystemTime, dry_run: bool) -> Result<Value> {
    let mut logs = vec![];
    if let Ok(entries) = fs::read_dir(root.join(layout::LOGS)) {
        for e in entries.flatten() {
            let path = e.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".log") && meta.len() > policy.log_max_bytes {
                if !dry_run {
                    let rotated = path.with_file_name(format!("{name}.1"));
                    fs::copy(&path, &rotated)?;
                    fs::OpenOptions::new().write(true).open(&path)?.set_len(0)?;
                }
                logs.push(json!({"rotated":name,"bytes":meta.len()}));
            } else if name.ends_with(".log.1") && age_days(&meta, now) >= policy.log_days.into() {
                if !dry_run {
                    fs::remove_file(&path)?;
                }
                logs.push(json!({"removed":name}));
            }
        }
    }
    let mut cache = 0usize;
    let mut dirs = vec![(root.join(layout::OBSERVATIONS), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = fs::symlink_metadata(e.path()) else {
                continue;
            };
            if meta.file_type().is_dir() && depth < 3 {
                dirs.push((e.path(), depth + 1));
            } else if meta.file_type().is_file() && age_days(&meta, now) >= policy.cache_days.into()
            {
                if !dry_run {
                    fs::remove_file(e.path())?;
                }
                cache += 1;
            }
        }
    }
    let mut operations = serde_json::Map::new();
    for (dir, phases) in PRUNABLE {
        let n = prune_operations(&root.join(dir), phases, policy.operation_days, now, dry_run)?;
        if n > 0 {
            operations.insert((*dir).into(), json!(n));
        }
    }
    Ok(json!({"dry_run":dry_run,"logs":logs,"cache_removed":cache,"operations_removed":operations}))
}

fn prune_operations(
    dir: &Path,
    phases: &[&str],
    days: u32,
    now: SystemTime,
    dry_run: bool,
) -> Result<usize> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(0);
    };
    let entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    let mut removed = 0;
    for journal in entries
        .iter()
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
    {
        let Ok(meta) = fs::symlink_metadata(journal) else {
            continue;
        };
        if !meta.file_type().is_file() || age_days(&meta, now) < days.into() {
            continue;
        }
        let Some(phase) = fs::read(journal)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["phase"].as_str().map(str::to_owned))
        else {
            continue;
        };
        if !phases.contains(&phase.as_str()) {
            continue;
        }
        let Some(stem) = journal.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let prefix = format!("{stem}.");
        let sidecars: Vec<_> = entries
            .iter()
            .filter(|p| {
                p != &journal
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect();
        // A held lock means a worker still owns this operation.
        let held = sidecars.iter().any(|p| {
            p.to_string_lossy().contains("lock")
                && fs::File::open(p).is_ok_and(|f| f.try_lock_exclusive().is_err())
        });
        if held {
            continue;
        }
        if !dry_run {
            fs::remove_file(journal)?;
            for p in sidecars {
                let _ = fs::remove_file(p);
            }
        }
        removed += 1;
    }
    Ok(removed)
}

/// Full pass, receipts included; `last_run` throttling is the caller's.
pub fn run(paths: &Paths, dry_run: bool) -> Result<Value> {
    let policy = Policy::load(paths);
    let mut result = sweep(&paths.root, policy, SystemTime::now(), dry_run)?;
    result["receipts_retired"] = if dry_run {
        Value::Null
    } else {
        match crate::messaging::run_receipt_retention(paths, crate::daemon::now()) {
            Ok(n) => json!(n),
            Err(e) => json!({"error": e.to_string()}),
        }
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn aged(path: &Path, days: u64) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(days * DAY + 60))
            .unwrap();
    }

    #[test]
    fn retention_removes_only_old_final_history_and_rotates_logs() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        layout::ensure_dirs(root).unwrap();
        let policy = Policy {
            log_days: 30,
            log_max_bytes: 10,
            cache_days: 14,
            operation_days: 90,
        };
        // logs
        fs::write(root.join(layout::DAEMON_LOG), "0123456789abcdef").unwrap();
        fs::write(root.join("logs/handoff.log.1"), "old").unwrap();
        aged(&root.join("logs/handoff.log.1"), 31);
        // cache
        let obs = root.join(layout::OBSERVATIONS);
        fs::create_dir_all(&obs).unwrap();
        for (name, days) in [("old.json", 15), ("new.json", 1)] {
            fs::write(obs.join(name), "{}").unwrap();
            aged(&obs.join(name), days);
        }
        let snapshot = layout::state_snapshot(root, "active");
        fs::write(&snapshot, "{}").unwrap();
        aged(&snapshot, 400);
        // operations
        let handoff = root.join(layout::HANDOFF_JOBS);
        fs::create_dir_all(&handoff).unwrap();
        for (id, phase, days) in [
            ("done", "complete", 91),
            ("recent", "complete", 10),
            ("unsure", "uncertain", 400),
            ("busy", "failed", 400),
        ] {
            fs::write(
                handoff.join(format!("{id}.json")),
                json!({"phase":phase}).to_string(),
            )
            .unwrap();
            aged(&handoff.join(format!("{id}.json")), days);
            fs::write(handoff.join(format!("{id}.worker-lock")), "").unwrap();
        }
        let busy = fs::File::open(handoff.join("busy.worker-lock")).unwrap();
        busy.lock_exclusive().unwrap();
        let deletions = root.join(layout::DELETIONS);
        fs::create_dir_all(&deletions).unwrap();
        fs::write(
            deletions.join("x.json"),
            json!({"phase":"complete"}).to_string(),
        )
        .unwrap();
        aged(&deletions.join("x.json"), 1000);

        let dry = sweep(root, policy, SystemTime::now(), true).unwrap();
        assert_eq!(dry["operations_removed"][layout::HANDOFF_JOBS], 1);
        assert!(
            handoff.join("done.json").exists(),
            "dry run changes nothing"
        );

        let r = sweep(root, policy, SystemTime::now(), false).unwrap();
        assert_eq!(r["cache_removed"], 1);
        assert_eq!(r["operations_removed"][layout::HANDOFF_JOBS], 1);
        assert!(!handoff.join("done.json").exists() && !handoff.join("done.worker-lock").exists());
        for kept in [
            "recent.json",
            "unsure.json",
            "busy.json",
            "busy.worker-lock",
        ] {
            assert!(handoff.join(kept).exists(), "{kept}");
        }
        assert!(
            deletions.join("x.json").exists(),
            "tombstones are never removed"
        );
        assert!(
            snapshot.exists() && obs.join("new.json").exists() && !obs.join("old.json").exists()
        );
        assert_eq!(fs::read(root.join(layout::DAEMON_LOG)).unwrap().len(), 0);
        assert_eq!(
            fs::read_to_string(root.join("logs/daemon.log.1")).unwrap(),
            "0123456789abcdef"
        );
        assert!(!root.join("logs/handoff.log.1").exists());
        drop(busy);
    }
}
