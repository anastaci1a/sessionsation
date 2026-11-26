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
            && r["phase"]
                .as_str()
                .is_some_and(crate::external::job_settled)
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
                r["phase"]
                    .as_str()
                    .is_some_and(crate::external::job_settled),
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
mod tests;
