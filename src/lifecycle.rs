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
        d if d == crate::layout::ROOT_MIGRATIONS => "root_rekey",
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
    let mut found = None;
    walk(paths, |directory, entry, r| {
        if source_of(r) != Some(thread) {
            return false;
        }
        match judge(directory, entry, r, own) {
            Some(verdict) => {
                found = Some(verdict);
                true
            }
            None => false,
        }
    })?;
    found.transpose()
}

/// The fields the admission checks read from a journal entry (see [`source_of`] and
/// [`judge`]), parsed once per file version: the journals hold thousands of finished
/// operations and every pass visits all of them. Anything else in the entry is dropped.
static ENTRIES: crate::file_cache::FileCache<Value> = crate::file_cache::FileCache::new();
fn reduced(full: Value) -> Value {
    use serde_json::json;
    json!({
        "id": full["id"],
        "phase": full["phase"],
        "conversation_id": full["conversation_id"],
        "session": {"conversation_id": full["session"]["conversation_id"]},
        "source": {"thread_id": full["source"]["thread_id"]},
        "request": {"request_id": full["request"]["request_id"], "message_id": full["request"]["message_id"]},
    })
}
/// The reduced entry of one journal file; the I/O and parse errors are the caller's to word.
fn entry_value(
    entry: &std::path::Path,
) -> std::result::Result<std::sync::Arc<Value>, crate::file_cache::LoadError<serde_json::Error>> {
    ENTRIES.load(entry, |b| serde_json::from_slice::<Value>(b).map(reduced))
}

/// The conversation an operation journal entry names.
fn source_of(r: &Value) -> Option<&str> {
    r["conversation_id"]
        .as_str()
        .or_else(|| r["session"]["conversation_id"].as_str())
        .or_else(|| r["source"]["thread_id"].as_str())
}

/// Visits every journal entry in admission order (directory order, then entry
/// order) until `visit` returns true. Only an absent directory means "no
/// journals"; any other stat or read error blocks, naming the file.
fn walk(
    paths: &Paths,
    mut visit: impl FnMut(&'static str, &std::path::Path, &Value) -> bool,
) -> Result<()> {
    for directory in [
        crate::layout::RESUMPTIONS,
        crate::layout::CONVERSATION_OPERATIONS,
        crate::layout::HANDOFF_JOBS,
        crate::layout::SPACE_MOVES,
        crate::layout::CLAUDE_SWITCHES,
        crate::layout::AUTO_RECOVERIES,
        crate::layout::MESSAGE_RECOVERIES,
        crate::layout::ROOT_MIGRATIONS,
    ] {
        let root = paths.root.join(directory);
        // Only an absent directory means "no journals"; any other stat error
        // (EIO, EACCES) blocks admission, naming the directory.
        match fs::metadata(&root) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "SESSION_OPERATION_IN_PROGRESS: operation journal directory {} is unreadable: {e}",
                    root.display()
                ));
            }
        }
        let entries = crate::paths::journal_paths(&root)?;
        ENTRIES.retain_dir(&root, &entries.iter().map(|e| e.as_path()).collect());
        for entry in entries {
            if entry.extension().is_none_or(|e| e != "json") {
                continue;
            }
            // A read (I/O) error always blocks, naming the file: it may be a re-key
            // record of a session that must not be resumed or launched meanwhile.
            let r = match entry_value(&entry) {
                Ok(v) => v,
                Err(crate::file_cache::LoadError::Io(e)) => {
                    return Err(anyhow::Error::new(e).context(format!(
                        "SESSION_OPERATION_IN_PROGRESS: operation journal {} is unreadable",
                        entry.display()
                    )));
                }
                Err(crate::file_cache::LoadError::Parse(e)) => {
                    // Root re-key records are written atomically, so a fully read file
                    // there that does not parse is not one of them: log, do not block.
                    // raw-path: a journal directory constant, not a filesystem path
                    if directory == crate::layout::ROOT_MIGRATIONS {
                        eprintln!(
                            "lifecycle: ignoring unparseable file {} in {}: {e}",
                            entry.display(),
                            crate::layout::ROOT_MIGRATIONS
                        );
                        continue;
                    }
                    return Err(e.into());
                }
            };
            if visit(directory, &entry, &r) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Whether the entry (naming `thread`) blocks it: the blocking operation, or
/// an error reading its worker lock / id. `None`: the entry does not block.
fn judge(
    directory: &str,
    entry: &std::path::Path,
    r: &Value,
    own: Option<&str>,
) -> Option<Result<Pending>> {
    let id = r["id"]
        .as_str()
        .or_else(|| r["request"]["request_id"].as_str())
        .or_else(|| r["request"]["message_id"].as_str());
    let pending = |id: Option<&str>| {
        Ok(Pending {
            id: id.context("INVALID_OPERATION_ID")?.into(),
            kind: kind_of(directory),
            phase: r["phase"].as_str().unwrap_or("unknown").into(),
        })
    };
    // raw-path: a journal directory constant, not a filesystem path
    if directory == crate::layout::HANDOFF_JOBS {
        if id != own
            && matches!(
                r["phase"].as_str(),
                Some("complete" | "failed" | "cancelled" | "recovered")
            )
        {
            return match crate::background::worker_stopped(&entry.with_extension("worker-lock")) {
                Ok(true) => None,
                Ok(false) => Some(pending(id)),
                Err(e) => Some(Err(e)),
            };
        }
        return None;
    }
    (id != own
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
        ))
    .then(|| pending(id))
}

/// Every conversation's blocking operation from one pass over the journals,
/// for batch readers such as `state`. `get` answers exactly what
/// [`pending_detail`] would for that conversation, including which error it
/// would have hit: an entry decides only the conversation it names, and a
/// journal that cannot be read blocks only conversations whose answer was not
/// settled by an earlier entry.
pub(crate) struct PendingIndex {
    found: std::collections::HashMap<String, std::result::Result<Pending, String>>,
    failed: Option<String>,
}
impl PendingIndex {
    pub(crate) fn scan(paths: &Paths) -> Self {
        let own = INLINE_OWNER.get().map(String::as_str);
        crate::inline::settle_resumptions(paths);
        let mut found: std::collections::HashMap<String, std::result::Result<Pending, String>> =
            Default::default();
        let failed = walk(paths, |directory, entry, r| {
            let Some(thread) = source_of(r) else {
                return false;
            };
            if found.contains_key(thread) {
                return false;
            }
            if let Some(verdict) = judge(directory, entry, r, own) {
                found.insert(thread.to_owned(), verdict.map_err(|e| format!("{e:#}")));
            }
            false
        })
        .err()
        .map(|e| format!("{e:#}"));
        Self { found, failed }
    }
    pub(crate) fn get(&self, thread: &str) -> Result<Option<Pending>> {
        match self.found.get(thread) {
            Some(Ok(p)) => Ok(Some(p.clone())),
            Some(Err(e)) => Err(anyhow::anyhow!("{e}")),
            None => match &self.failed {
                Some(e) => Err(anyhow::anyhow!("{e}")),
                None => Ok(None),
            },
        }
    }
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
        let r = match entry_value(&entry) {
            Ok(v) => v,
            Err(crate::file_cache::LoadError::Io(e)) => return Err(e.into()),
            Err(crate::file_cache::LoadError::Parse(e)) => return Err(e.into()),
        };
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
    /// A journal entry parsed once and served from memory answers like a fresh read, and a
    /// rewrite, removal or corruption of the file is seen on the next pass.
    #[test]
    fn cached_journal_entries_follow_the_files() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(d.path().join("state"))).unwrap();
        let (a, b) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        let dir = p.root.join(crate::layout::CONVERSATION_OPERATIONS);
        private_dir(&dir).unwrap();
        let put = |name: &str, id: &str, thread: &str, phase: &str| {
            fs::write(
                dir.join(name),
                serde_json::to_vec(
                    &json!({"id":id,"session":{"conversation_id":thread},"phase":phase,"noise":[1,2,3]}),
                )
                .unwrap(),
            )
            .unwrap();
        };
        let answer = |thread: &str| {
            let cached = (
                pending_detail(&p, thread, None),
                PendingIndex::scan(&p).get(thread),
            );
            crate::file_cache::trust_fresh_stamps(false);
            let fresh = (
                pending_detail(&p, thread, None),
                PendingIndex::scan(&p).get(thread),
            );
            crate::file_cache::trust_fresh_stamps(true);
            let text = |r: Result<Option<Pending>>| r.map_err(|e| format!("{e:#}"));
            assert_eq!(text(cached.0), text(fresh.0), "{thread}");
            assert_eq!(text(cached.1), text(fresh.1), "{thread}");
            pending_detail(&p, thread, None).map_err(|e| format!("{e:#}"))
        };
        crate::file_cache::trust_fresh_stamps(true);
        put("x.json", "x", &a, "queued");
        put("y.json", "y", &b, "complete");
        for _ in 0..2 {
            assert_eq!(answer(&a).unwrap().unwrap().id, "x");
            assert!(answer(&b).unwrap().is_none());
        }
        // Rewritten in place (different size): no longer blocks.
        put("x.json", "x", &a, "cancelled");
        assert!(answer(&a).unwrap().is_none());
        // Another operation appears for the same conversation.
        put("z.json", "z", &a, "queued");
        assert_eq!(answer(&a).unwrap().unwrap().id, "z");
        // Removed.
        fs::remove_file(dir.join("z.json")).unwrap();
        assert!(answer(&a).unwrap().is_none());
        // Corrupted: blocks with the parse error, like a fresh read.
        fs::write(dir.join("y.json"), b"{").unwrap();
        assert!(answer(&b).is_err());
        crate::file_cache::trust_fresh_stamps(false);
    }
    /// One pass over the journals answers every conversation exactly as a scan per
    /// conversation does, including which ones an unreadable journal blocks.
    #[test]
    fn the_batch_index_answers_like_a_scan_per_conversation() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(d.path().join("state"))).unwrap();
        let ids: Vec<String> = (0..6).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        let write = |dir: &str, name: &str, v: Value| {
            private_dir(&p.root.join(dir)).unwrap();
            fs::write(p.root.join(dir).join(name), serde_json::to_vec(&v).unwrap()).unwrap();
        };
        let compare = |p: &Paths| {
            let index = PendingIndex::scan(p);
            for id in ids.iter().map(String::as_str).chain(["unknown-thread"]) {
                let one = pending_detail(p, id, None).map_err(|e| format!("{e:#}"));
                let batch = index.get(id).map_err(|e| format!("{e:#}"));
                assert_eq!(one, batch, "{id}");
            }
        };
        compare(&p);
        write(
            crate::layout::RESUMPTIONS,
            "a.json",
            json!({"conversation_id":ids[0],"request":{"request_id":"a"},"phase":"uncertain"}),
        );
        write(
            crate::layout::CONVERSATION_OPERATIONS,
            "b.json",
            json!({"id":"b","session":{"conversation_id":ids[1]},"phase":"queued"}),
        );
        write(
            crate::layout::CONVERSATION_OPERATIONS,
            "c.json",
            json!({"id":"c","session":{"conversation_id":ids[1]},"phase":"queued"}),
        );
        write(
            crate::layout::CONVERSATION_OPERATIONS,
            "d.json",
            json!({"id":"d","session":{"conversation_id":ids[2]},"phase":"complete"}),
        );
        write(
            crate::layout::HANDOFF_JOBS,
            "e.json",
            json!({"id":"e","source":{"thread_id":ids[3]},"phase":"failed"}),
        );
        write(
            crate::layout::HANDOFF_JOBS,
            "f.json",
            json!({"source":{"thread_id":ids[4]},"phase":"failed"}),
        );
        write(
            crate::layout::SPACE_MOVES,
            "g.json",
            json!({"conversation_id":ids[5],"phase":"moving"}),
        );
        compare(&p);
        assert_eq!(
            PendingIndex::scan(&p).get(&ids[1]).unwrap().unwrap().id,
            "b"
        );
        // An unreadable journal blocks only what no earlier entry settled.
        fs::write(
            p.root
                .join(crate::layout::CONVERSATION_OPERATIONS)
                .join("bad.json"),
            b"{",
        )
        .unwrap();
        compare(&p);
        let index = PendingIndex::scan(&p);
        assert!(index.get(&ids[0]).unwrap().is_some());
        assert!(index.get(&ids[2]).is_err());
        assert!(index.get("unknown-thread").is_err());
    }
}
