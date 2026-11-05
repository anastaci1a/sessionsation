//! Admission serialization across resume, handoff and conversation operations.
use crate::paths::{Paths, lock_file, private_dir};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::fs;

static INLINE_OWNER: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// This process runs one inline `resume`: its own journal entry never blocks it.
pub(crate) fn inline_owner() -> Option<&'static str> {
    INLINE_OWNER.get().map(String::as_str)
}
pub(crate) fn set_inline_owner(request: &str) {
    let _ = INLINE_OWNER.set(request.to_owned());
}
/// The unfinished operation that blocks a conversation: its id, journal kind and phase.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pending {
    pub id: String,
    pub kind: &'static str,
    pub phase: String,
}
impl Pending {
    /// `uncertain` and `stuck` operations need an explicit recover or abandon.
    pub fn unresolved(&self) -> bool {
        matches!(self.phase.as_str(), "uncertain" | "stuck")
    }
    pub fn code(&self) -> &'static str {
        if self.unresolved() {
            "RECOVERY_UNRESOLVED"
        } else {
            "SESSION_OPERATION_IN_PROGRESS"
        }
    }
    /// The refusal text: code, operation id, kind and phase, and how an unresolved one ends.
    pub fn message(&self) -> String {
        let how = if self.unresolved() {
            "; api recover it or api abandon-operation it (operator) to unblock the session"
        } else {
            "; retry when it finishes"
        };
        format!(
            "{}: operation {} ({}, phase {}){how}",
            self.code(),
            self.id,
            self.kind,
            self.phase
        )
    }
}
fn kind_of(directory: &str) -> &'static str {
    match directory {
        d if d == crate::layout::RESUMPTIONS => "resume",
        d if d == crate::layout::CONVERSATION_OPERATIONS => "conversation",
        d if d == crate::layout::HANDOFF_JOBS => "move",
        d if d == crate::layout::SPACE_MOVES => "space_move",
        d if d == crate::layout::CLAUDE_SWITCHES => "switch",
        d if d == crate::layout::AUTO_RECOVERIES => "auto_recover",
        _ => "message_recovery",
    }
}
pub(crate) fn pending(paths: &Paths, thread: &str, own: Option<&str>) -> Result<Option<String>> {
    Ok(pending_detail(paths, thread, own)?.map(|p| p.id))
}
pub(crate) fn pending_detail(
    paths: &Paths,
    thread: &str,
    own: Option<&str>,
) -> Result<Option<Pending>> {
    let own = own.or(INLINE_OWNER.get().map(String::as_str));
    crate::inline::settle_resumptions(paths);
    for directory in [
        crate::layout::RESUMPTIONS,
        crate::layout::CONVERSATION_OPERATIONS,
        crate::layout::HANDOFF_JOBS,
        crate::layout::SPACE_MOVES,
        crate::layout::CLAUDE_SWITCHES,
        crate::layout::AUTO_RECOVERIES,
        crate::layout::MESSAGE_RECOVERIES,
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
            if directory == crate::layout::HANDOFF_JOBS {
                if source == Some(thread)
                    && id != own
                    && matches!(
                        r["phase"].as_str(),
                        Some("complete" | "failed" | "cancelled" | "recovered")
                    )
                    && !crate::background::worker_stopped(&entry.with_extension("worker-lock"))?
                {
                    return Ok(Some(Pending {
                        id: id.context("INVALID_OPERATION_ID")?.into(),
                        kind: kind_of(directory),
                        phase: r["phase"].as_str().unwrap_or("unknown").into(),
                    }));
                }
                continue;
            }
            if source == Some(thread)
                && id != own
                && !matches!(
                    r["phase"].as_str(),
                    Some(
                        "running"
                            | "resumed"
                            | "prompting"
                            | "resuming"
                            | "complete"
                            | "failed"
                            | "cancelled"
                            | "recovered"
                            | "abandoned"
                    )
                )
            {
                return Ok(Some(Pending {
                    id: id.context("INVALID_OPERATION_ID")?.into(),
                    kind: kind_of(directory),
                    phase: r["phase"].as_str().unwrap_or("unknown").into(),
                }));
            }
        }
    }
    Ok(None)
}
pub(crate) fn ensure_available(paths: &Paths, thread: &str, own: Option<&str>) -> Result<()> {
    if let Some(p) = pending_detail(paths, thread, own)? {
        anyhow::bail!("{}", p.message());
    }
    Ok(())
}
/// Called after admission locking when the caller is not a handoff enqueue.
/// Enqueue keeps its existing identical-request deduplication rules instead.
pub(crate) fn ensure_no_handoff(paths: &Paths, thread: &str, own: Option<&str>) -> Result<()> {
    let root = paths.root.join(crate::layout::HANDOFF_JOBS);
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
    let root = paths.root.join(crate::layout::LIFECYCLE_ADMISSION);
    private_dir(&root)?;
    let lock = lock_file(&root.join(format!("{thread}.lock")))?;
    crate::paths::try_lock_brief(&lock).context("SESSION_OPERATION_IN_PROGRESS")?;
    ensure_available(paths, thread, own)?;
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;
    use serde_json::json;
    #[test]
    fn failed_handoff_keeps_reservation_until_recovery_worker_exits() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(d.path().join("state"))).unwrap();
        let thread = uuid::Uuid::new_v4().to_string();
        private_dir(&p.root.join(crate::layout::HANDOFF_JOBS)).unwrap();
        fs::write(
            p.root.join(crate::layout::HANDOFF_JOBS).join("move.json"),
            serde_json::to_vec(
                &json!({"id":"move","source":{"thread_id":thread},"phase":"failed"}),
            )
            .unwrap(),
        )
        .unwrap();
        let lock = lock_file(
            &p.root
                .join(crate::layout::HANDOFF_JOBS)
                .join("move.worker-lock"),
        )
        .unwrap();
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
        private_dir(&p.root.join(crate::layout::RESUMPTIONS)).unwrap();
        for phase in [
            "admitted",
            "preparing",
            "creating_terminal",
            "dispatching",
            "transferring",
            "uncertain",
        ] {
            fs::write(p.root.join(crate::layout::RESUMPTIONS).join("original.json"), serde_json::to_vec(&json!({"conversation_id":thread,"request":{"request_id":"original"},"phase":phase})).unwrap()).unwrap();
            assert!(admit(&p, &thread, Some("competing")).is_err(), "{phase}");
            let original = admit(&p, &thread, Some("original")).unwrap();
            assert!(
                admit(&p, &thread, Some("original")).is_err(),
                "simultaneous admission must lock"
            );
            drop(original);
        }
        fs::write(p.root.join(crate::layout::RESUMPTIONS).join("original.json"), serde_json::to_vec(&json!({"conversation_id":thread,"request":{"request_id":"original"},"phase":"running"})).unwrap()).unwrap();
        assert!(admit(&p, &thread, None).is_ok());
        private_dir(&p.root.join(crate::layout::CONVERSATION_OPERATIONS)).unwrap();
        fs::write(
            p.root
                .join(crate::layout::CONVERSATION_OPERATIONS)
                .join("archive.json"),
            serde_json::to_vec(
                &json!({"id":"archive","session":{"conversation_id":thread},"phase":"queued"}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(admit(&p, &thread, None).is_err());
        private_dir(&p.root.join(crate::layout::HANDOFF_JOBS)).unwrap();
        fs::write(
            p.root.join(crate::layout::HANDOFF_JOBS).join("move.json"),
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
