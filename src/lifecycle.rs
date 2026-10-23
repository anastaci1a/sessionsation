//! Admission serialization across resume, handoff and conversation operations.
use crate::paths::{Paths, lock_file, private_dir};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde_json::Value;
use std::fs;

pub(crate) fn pending(paths: &Paths, thread: &str, own: Option<&str>) -> Result<Option<String>> {
    for directory in [
        "resumptions",
        "conversation-operations",
        "handoff-jobs",
        "space-moves",
        "message-recoveries",
    ] {
        let root = paths.root.join(directory);
        if !root.is_dir() {
            continue;
        }
        for entry in crate::paths::journal_paths(&root)? {
            if entry.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let r: Value = serde_json::from_slice(&fs::read(&entry)?)?;
            let id = r["id"]
                .as_str()
                .or_else(|| r["request"]["request_id"].as_str())
                .or_else(|| r["request"]["message_id"].as_str());
            let source = r["conversation_id"]
                .as_str()
                .or_else(|| r["session"]["conversation_id"].as_str())
                .or_else(|| r["source"]["thread_id"].as_str());
            if directory == "handoff-jobs" {
                if source == Some(thread)
                    && id != own
                    && matches!(
                        r["phase"].as_str(),
                        Some("complete" | "failed" | "cancelled" | "recovered")
                    )
                    && !crate::background::worker_stopped(&entry.with_extension("worker-lock"))?
                {
                    return Ok(Some(id.context("INVALID_OPERATION_ID")?.into()));
                }
                continue;
            }
            if source == Some(thread)
                && id != own
                && !matches!(
                    r["phase"].as_str(),
                    Some("running" | "complete" | "failed" | "cancelled" | "recovered")
                )
            {
                return Ok(Some(id.context("INVALID_OPERATION_ID")?.into()));
            }
        }
    }
    Ok(None)
}
pub(crate) fn ensure_available(paths: &Paths, thread: &str, own: Option<&str>) -> Result<()> {
    ensure!(
        pending(paths, thread, own)?.is_none(),
        "SESSION_OPERATION_IN_PROGRESS"
    );
    Ok(())
}
/// Called after admission locking when the caller is not a handoff enqueue.
/// Enqueue keeps its existing identical-request deduplication rules instead.
pub(crate) fn ensure_no_handoff(paths: &Paths, thread: &str, own: Option<&str>) -> Result<()> {
    let root = paths.root.join("handoff-jobs");
    if !root.is_dir() {
        return Ok(());
    }
    for entry in crate::paths::journal_paths(&root)? {
        if entry.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let r: Value = serde_json::from_slice(&fs::read(&entry)?)?;
        if r["source"]["thread_id"] == thread && r["id"].as_str() != own {
            ensure!(
                matches!(
                    r["phase"].as_str(),
                    Some("complete" | "failed" | "cancelled" | "recovered")
                ),
                "SESSION_OPERATION_IN_PROGRESS"
            );
        }
    }
    Ok(())
}
pub(crate) fn admit(paths: &Paths, thread: &str, own: Option<&str>) -> Result<fs::File> {
    uuid::Uuid::parse_str(thread).context("INVALID_CONVERSATION_ID")?;
    let root = paths.root.join("lifecycle-admission");
    private_dir(&root)?;
    let lock = lock_file(&root.join(format!("{thread}.lock")))?;
    lock.try_lock_exclusive()
        .context("SESSION_OPERATION_IN_PROGRESS")?;
    ensure_available(paths, thread, own)?;
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn failed_handoff_keeps_reservation_until_recovery_worker_exits() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(d.path().join("state"))).unwrap();
        let thread = uuid::Uuid::new_v4().to_string();
        private_dir(&p.root.join("handoff-jobs")).unwrap();
        fs::write(
            p.root.join("handoff-jobs/move.json"),
            serde_json::to_vec(
                &json!({"id":"move","source":{"thread_id":thread},"phase":"failed"}),
            )
            .unwrap(),
        )
        .unwrap();
        let lock = lock_file(&p.root.join("handoff-jobs/move.worker-lock")).unwrap();
        lock.lock_exclusive().unwrap();
        assert_eq!(pending(&p, &thread, None).unwrap().as_deref(), Some("move"));
        assert!(admit(&p, &thread, None).is_err());
        assert!(admit(&p, &thread, Some("move")).is_ok());
        drop(lock);
        assert!(admit(&p, &thread, None).is_ok());
    }
    #[test]
    fn competing_operations_reject_pending_and_uncertain_resumes_but_allow_original_handoff() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(d.path().join("state"))).unwrap();
        let thread = uuid::Uuid::new_v4().to_string();
        private_dir(&p.root.join("resumptions")).unwrap();
        for phase in [
            "admitted",
            "preparing",
            "creating_terminal",
            "dispatching",
            "transferring",
            "uncertain",
        ] {
            fs::write(p.root.join("resumptions/original.json"), serde_json::to_vec(&json!({"conversation_id":thread,"request":{"request_id":"original"},"phase":phase})).unwrap()).unwrap();
            assert!(admit(&p, &thread, Some("competing")).is_err(), "{phase}");
            let original = admit(&p, &thread, Some("original")).unwrap();
            assert!(
                admit(&p, &thread, Some("original")).is_err(),
                "simultaneous admission must lock"
            );
            drop(original);
        }
        fs::write(p.root.join("resumptions/original.json"), serde_json::to_vec(&json!({"conversation_id":thread,"request":{"request_id":"original"},"phase":"running"})).unwrap()).unwrap();
        assert!(admit(&p, &thread, None).is_ok());
        private_dir(&p.root.join("conversation-operations")).unwrap();
        fs::write(
            p.root.join("conversation-operations/archive.json"),
            serde_json::to_vec(
                &json!({"id":"archive","session":{"conversation_id":thread},"phase":"queued"}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(admit(&p, &thread, None).is_err());
        private_dir(&p.root.join("handoff-jobs")).unwrap();
        fs::write(
            p.root.join("handoff-jobs/move.json"),
            serde_json::to_vec(
                &json!({"id":"move","source":{"thread_id":thread},"phase":"launching_target"}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(ensure_no_handoff(&p, &thread, None).is_err());
        assert!(ensure_no_handoff(&p, &thread, Some("move")).is_ok());
    }
}
