//! Archive, restore and delete for native-provider (Claude, Gemini) sessions.
//!
//! The provider owns its config home, so these jobs move only exact-id files of
//! one catalog entry's conversations (its current conversation and every
//! `previous_conversation_ids` lineage member): Claude's
//! `projects/<dir>/<id>.jsonl` transcript, its `projects/<dir>/<id>/` subagent and
//! tool-result sidecar directory, `file-history/<id>/`, `session-env/<id>/` and
//! `todos/<id>-agent-*.json`; Gemini's `.gemini/tmp/<slug>/chats/session-*.jsonl`
//! files whose header names the id. Credentials (`.credentials.json`,
//! `sessions/*.key`, OAuth files) are never read or touched.
//!
//! Archive moves those files into `<root>/archived-sessions/<session>/native/<op>/`
//! (rename, or copy-verify-remove across file systems) and records a `native.json`
//! manifest with each original path and content digest; the provider no longer
//! lists or resumes the conversation. Restore moves them back to the same paths
//! of the same account home. Delete stages the files (and dead
//! `sessions/<pid>.json` registry entries naming the conversation) under
//! `<root>/deletions/native-<op>/` with a `plan.json` journal, removes the
//! catalog entry and its merged aliases, then removes the staged files.
use super::*;
use crate::provider::{ProviderId, claude, gemini, native};
use std::collections::BTreeSet;
use std::os::unix::fs::DirBuilderExt;

/// One moved path: a regular file or a directory tree without symlinks.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Entry {
    original: PathBuf,
    /// Relative to the operation's bundle directory.
    stored: String,
    dir: bool,
    bytes: u64,
    digest: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Manifest {
    version: u32,
    session: Session,
    provider: String,
    home: PathBuf,
    /// archiving | archived | restoring | restored | deleted
    state: String,
    archived_at: i64,
    bundle: String,
    conversation_ids: Vec<String>,
    entries: Vec<Entry>,
}
const MANIFEST: &str = "native.json";

fn manifest_path(paths: &Paths, id: &str) -> Result<PathBuf> {
    Ok(slot(paths, id)?.join(MANIFEST))
}
pub(super) fn load(paths: &Paths, id: &str) -> Result<Option<Manifest>> {
    let p = manifest_path(paths, id)?;
    match fs::read(&p) {
        Ok(bytes) => {
            let m: Manifest = serde_json::from_slice(&bytes)?;
            ensure!(m.version == 1 && m.session.id == id, "INVALID_ARCHIVE");
            uuid::Uuid::parse_str(&m.bundle)?;
            Ok(Some(m))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn save(paths: &Paths, m: &Manifest) -> Result<()> {
    atomic_write(
        &manifest_path(paths, &m.session.id)?,
        &serde_json::to_vec(m)?,
    )
}
fn archived_state(state: &str) -> bool {
    matches!(state, "archiving" | "archived" | "restoring")
}
fn all(paths: &Paths) -> Result<Vec<Manifest>> {
    let mut out = vec![];
    for e in fs::read_dir(root(paths)?)? {
        let e = e?;
        let Some(id) = e.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if uuid::Uuid::parse_str(&id).is_ok() && e.path().join(MANIFEST).is_file() {
            out.extend(load(paths, &id)?);
        }
    }
    Ok(out)
}
pub(super) fn metadata(paths: &Paths) -> Result<std::collections::BTreeMap<String, Value>> {
    Ok(all(paths)?
        .into_iter()
        .map(|m| {
            (
                m.session.id.clone(),
                json!({"kind":"managed","state":m.state,"archived":archived_state(&m.state),"archived_at":m.archived_at,"source":"managed_manifest","storage":"moved_native_files"}),
            )
        })
        .collect())
}
pub(super) fn list(paths: &Paths) -> Result<Vec<Value>> {
    let catalog = crate::catalog::list(paths)?;
    Ok(all(paths)?
        .into_iter()
        .filter(|m| archived_state(&m.state))
        .map(|m| {
            let session = catalog.iter().find(|s| s.id == m.session.id).unwrap_or(&m.session);
            json!({"id":session.id,"name":session.name,"provider":session.provider,"conversation_id":session.conversation_id,
                "previous_conversation_ids":session.previous_conversation_ids,
                "account_id":format!("{}:{}",session.provider,session.account),"generation":session.generation(),"availability":"archived",
                "state":if m.state == "archived" {"archived"} else {"prepared"},"archived_at":m.archived_at,"conversation_created_at":null,
                "history_file_bytes":m.entries.iter().map(|e|e.bytes).sum::<u64>(),"compressed_bytes":null,"compression":"none",
                "storage":"moved_native_files","capabilities":{"restore":true,"delete":true}})
        })
        .collect())
}

/// Every conversation of one entry, oldest first.
fn conversations(s: &Session) -> Vec<String> {
    let mut ids = s.previous_conversation_ids.clone();
    ids.push(s.conversation_id.clone());
    ids
}
fn home_of(paths: &Paths, provider: ProviderId, account: &str) -> Result<PathBuf> {
    Ok(native::account(paths, provider, account)?.home)
}

/// Why this conversation may not be archived, restored or deleted now: a live
/// provider process names it, or a Sessionsation-hosted worker owns it.
fn activity(
    paths: &Paths,
    provider: ProviderId,
    home: Option<&Path>,
    ids: &[String],
) -> Result<Option<String>> {
    if provider == ProviderId::Claude {
        for id in ids {
            if claude::hosted::live(paths, id) {
                return Ok(Some(format!(
                    "SESSION_HOSTED: a Sessionsation-hosted worker runs conversation {id}; stop it first"
                )));
            }
        }
    }
    if let Some(home) = home {
        let live = native::live_sessions(provider, home).map_err(|e| {
            anyhow::anyhow!("SESSION_STATE_UNKNOWN: live process probe failed: {e}")
        })?;
        if let Some(l) = live.iter().find(|l| ids.contains(&l.session_id)) {
            return Ok(Some(format!(
                "SESSION_RUNNING: {} process {} runs conversation {}; end it first",
                native::title(provider),
                l.pid,
                l.session_id
            )));
        }
    }
    Ok(None)
}
fn ensure_quiet(
    paths: &Paths,
    provider: ProviderId,
    home: Option<&Path>,
    ids: &[String],
) -> Result<()> {
    if let Some(reason) = activity(paths, provider, home, ids)? {
        bail!(reason);
    }
    Ok(())
}

/// Sessions-row capability projection (state and details).
pub fn apply_session(
    paths: &Paths,
    provider: ProviderId,
    value: &mut Value,
    home: Option<&Path>,
    archive: Option<&Value>,
) {
    if provider == ProviderId::Antigravity {
        // agy deletes conversations only from its own picker: never moved here.
        let reason = format!(
            "UNSUPPORTED_PROVIDER_OPERATION: {}",
            crate::provider::antigravity::NO_ARCHIVE
        );
        for k in ["archive", "delete", "restore"] {
            value["capabilities"][k] = json!(false);
            value["capability_reasons"][k] = json!(reason);
        }
        return;
    }
    if let Some(a) = archive.filter(|a| a["archived"] == true) {
        value["archive"] = a.clone();
        value["archived"] = json!(true);
        value["availability"] = json!("archived");
        value["resume_block_reason"] = json!("archived");
        if let Some(caps) = value["capabilities"].as_object_mut() {
            for v in caps.values_mut() {
                *v = json!(false);
            }
        }
        value["capabilities"]["restore"] = json!(true);
        value["capabilities"]["delete"] = json!(true);
        value["capability_reasons"]["archive"] = json!("SESSION_ALREADY_ARCHIVED");
        value["capability_reasons"]["resume"] = json!("SESSION_ARCHIVED: restore it first");
        value["capability_reasons"]["restore"] = Value::Null;
        value["capability_reasons"]["delete"] = Value::Null;
        for k in ["submit", "queue", "steer"] {
            value["capability_reasons"][k] = json!("SESSION_ARCHIVED: restore it first");
        }
        if value["input_delivery"].is_object() {
            value["input_delivery"]["semantics"] = Value::Null;
        }
        return;
    }
    let ids: Vec<String> = value["previous_conversation_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(std::iter::once(&value["conversation_id"]))
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let availability = value["availability"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    let hosted = value["hosted"].is_object()
        && !matches!(
            value["hosted"]["state"].as_str(),
            Some("stopped" | "failed")
        );
    let (archive_reason, delete_reason): (Option<String>, Option<String>) = if value["resume_block_reason"]
        == "account_deleted"
        || home.is_none()
    {
        let r = "ACCOUNT_DELETED: the account home is gone; its history is not managed here";
        (Some(r.into()), Some(r.into()))
    } else if hosted {
        let r =
            "SESSION_HOSTED: a Sessionsation-hosted worker runs this conversation; stop it first";
        (Some(r.into()), Some(r.into()))
    } else if availability == "running" {
        let r = "SESSION_RUNNING: the provider process runs this conversation; end it first";
        (Some(r.into()), Some(r.into()))
    } else if availability == "unknown" {
        let r = "SESSION_STATE_UNKNOWN: the live process probe failed";
        (Some(r.into()), Some(r.into()))
    } else {
        match activity(paths, provider, None, &ids[..ids.len().saturating_sub(1)]) {
            Ok(Some(r)) => (Some(r.clone()), Some(r)),
            Err(e) => (Some(e.to_string()), Some(e.to_string())),
            Ok(None) if availability == "stale" => (
                Some(
                    "HISTORY_UNAVAILABLE: no transcript to archive; delete removes the entry"
                        .into(),
                ),
                None,
            ),
            Ok(None) => (None, None),
        }
    };
    value["capabilities"]["archive"] = json!(archive_reason.is_none());
    value["capabilities"]["delete"] = json!(delete_reason.is_none());
    value["capabilities"]["restore"] = json!(false);
    value["capability_reasons"]["archive"] = json!(archive_reason);
    value["capability_reasons"]["delete"] = json!(delete_reason);
    value["capability_reasons"]["restore"] = json!("SESSION_NOT_ARCHIVED");
}

/// Admission checks before a job is journaled (rechecked by the worker).
pub(super) fn admit(paths: &Paths, kind: Kind, s: &Session) -> Result<()> {
    let provider = native::of(&s.provider).context("UNSUPPORTED_PROVIDER_OPERATION")?;
    ensure!(
        provider != ProviderId::Antigravity,
        "UNSUPPORTED_PROVIDER_OPERATION: {}",
        crate::provider::antigravity::NO_ARCHIVE
    );
    let archived = load(paths, &s.id)?.is_some_and(|m| archived_state(&m.state));
    match kind {
        Kind::Archive => ensure!(!archived, "SESSION_ALREADY_ARCHIVED"),
        Kind::Restore => ensure!(archived, "SESSION_NOT_ARCHIVED"),
        Kind::Delete => {}
    }
    let home = match home_of(paths, provider, &s.account) {
        Ok(home) => Some(home),
        Err(_) if archived && kind == Kind::Delete => None,
        Err(e) => return Err(e),
    };
    ensure_quiet(paths, provider, home.as_deref(), &conversations(s))
}

// ------------------------------------------------------------ file handling

fn reject_symlink(p: &Path) -> Result<fs::Metadata> {
    let m = fs::symlink_metadata(p)?;
    ensure!(
        !m.file_type().is_symlink(),
        "HISTORY_SYMLINK: {} is a symlink; nothing was moved",
        p.display()
    );
    ensure!(
        m.is_file() || m.is_dir(),
        "HISTORY_NOT_REGULAR: {}",
        p.display()
    );
    Ok(m)
}
/// (bytes, digest) of a regular file or a symlink-free directory tree.
fn tree_digest(p: &Path) -> Result<(u64, String)> {
    let m = reject_symlink(p)?;
    if m.is_file() {
        return Ok((m.len(), digest(p)?));
    }
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut children: Vec<_> = fs::read_dir(p)?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|e| e.file_name());
    for c in children {
        let (n, d) = tree_digest(&c.path())?;
        bytes += n;
        hash.update(c.file_name().as_bytes());
        hash.update([0]);
        hash.update(d.as_bytes());
        hash.update([u8::from(c.file_type()?.is_dir())]);
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let m = reject_symlink(src)?;
    if m.is_dir() {
        std::fs::DirBuilder::new().mode(0o700).create(dst)?;
        for c in fs::read_dir(src)? {
            let c = c?;
            copy_tree(&c.path(), &dst.join(c.file_name()))?;
        }
        return Ok(());
    }
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(src)?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dst)?;
    std::io::copy(&mut input, &mut out)?;
    out.sync_all()?;
    Ok(())
}
fn remove_tree(p: &Path) -> Result<()> {
    if reject_symlink(p)?.is_dir() {
        tree_digest(p)?; // refuses any nested symlink before removing
        fs::remove_dir_all(p)?;
    } else {
        fs::remove_file(p)?;
    }
    Ok(())
}
/// Create missing ancestors (0700) without re-permissioning existing ones.
fn ensure_parent(p: &Path) -> Result<()> {
    let parent = p.parent().context("INVALID_HISTORY_PATH")?;
    if !parent.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    ensure!(
        !fs::symlink_metadata(parent)?.file_type().is_symlink(),
        "HISTORY_SYMLINK: {}",
        parent.display()
    );
    Ok(())
}
/// Move one verified entry; the destination must not exist. Across file systems
/// the copy is verified before the source is removed.
fn move_entry(src: &Path, dst: &Path, expected: &str) -> Result<()> {
    ensure!(
        fs::symlink_metadata(dst).is_err(),
        "RESTORE_CONFLICT: {} already exists",
        dst.display()
    );
    ensure!(tree_digest(src)?.1 == expected, "HISTORY_CHANGED");
    ensure_parent(dst)?;
    match fs::rename(src, dst) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_tree(src, dst)?;
            ensure!(tree_digest(dst)?.1 == expected, "HISTORY_COPY_MISMATCH");
            remove_tree(src)?;
        }
        Err(e) => return Err(e.into()),
    }
    ensure!(tree_digest(dst)?.1 == expected, "HISTORY_CHANGED");
    Ok(())
}

/// Exact-id native files of these conversations in one account home.
fn collect(provider: ProviderId, home: &Path, ids: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = BTreeSet::new();
    let mut add = |p: PathBuf| -> Result<()> {
        if fs::symlink_metadata(&p).is_ok() {
            reject_symlink(&p)?;
            out.insert(p);
        }
        Ok(())
    };
    for id in ids {
        uuid::Uuid::parse_str(id)?;
        match provider {
            ProviderId::Gemini => {
                for p in gemini::chat_paths(home, id) {
                    add(p)?;
                }
            }
            _ => {
                if let Ok(dirs) = fs::read_dir(home.join("projects")) {
                    for d in dirs.flatten() {
                        if !d.file_type().is_ok_and(|t| t.is_dir()) {
                            continue;
                        }
                        add(d.path().join(format!("{id}.jsonl")))?;
                        add(d.path().join(id))?;
                    }
                }
                add(home.join("file-history").join(id))?;
                add(home.join("session-env").join(id))?;
                if let Ok(todos) = fs::read_dir(home.join("todos")) {
                    let prefix = format!("{id}-agent-");
                    for t in todos.flatten() {
                        let name = t.file_name();
                        let name = name.to_string_lossy();
                        if name.starts_with(&prefix) && name.ends_with(".json") {
                            add(t.path())?;
                        }
                    }
                }
            }
        }
    }
    // Every path must stay inside the account home.
    for p in &out {
        ensure!(p.starts_with(home), "INVALID_HISTORY_PATH");
    }
    Ok(out.into_iter().collect())
}
fn not_open(p: &Path) -> Result<()> {
    let open = if fs::symlink_metadata(p)?.is_dir() {
        false
    } else {
        rollout_still_open(p)?
    };
    ensure!(
        !open,
        "HISTORY_IN_USE: {} has a writer; end it first",
        p.display()
    );
    Ok(())
}
fn bundle_dir(paths: &Paths, m: &Manifest) -> Result<PathBuf> {
    let dir = slot(paths, &m.session.id)?.join("native").join(&m.bundle);
    private_dir(&dir)?;
    Ok(dir)
}

// ------------------------------------------------------------------- jobs

pub(super) fn run(paths: &Paths, o: &mut Operation) -> Result<()> {
    let provider = native::of(&o.session.provider).context("UNSUPPORTED_PROVIDER_OPERATION")?;
    ensure!(
        provider != ProviderId::Antigravity,
        "UNSUPPORTED_PROVIDER_OPERATION: {}",
        crate::provider::antigravity::NO_ARCHIVE
    );
    let _account = crate::accounts::operation_lock(paths, &o.session.account)?;
    match o.kind {
        Kind::Archive => archive(paths, provider, o),
        Kind::Restore => restore(paths, provider, o),
        Kind::Delete => delete(paths, provider, o),
    }
}

fn archive(paths: &Paths, provider: ProviderId, o: &mut Operation) -> Result<()> {
    let s = crate::catalog::resolve(paths, &o.session.id, &o.expected_generation)?;
    ensure!(
        load(paths, &s.id)?.is_none_or(|m| !archived_state(&m.state)),
        "SESSION_ALREADY_ARCHIVED"
    );
    let home = home_of(paths, provider, &s.account)?;
    let ids = conversations(&s);
    phase(
        paths,
        o,
        "checking_history",
        "Checking that no process or hosted worker runs this conversation.",
    )?;
    ensure_quiet(paths, provider, Some(&home), &ids)?;
    let files = collect(provider, &home, &ids)?;
    ensure!(
        native::find_transcript(provider, &home, &s.conversation_id).is_some(),
        "HISTORY_NOT_FOUND"
    );
    let mut m = Manifest {
        version: 1,
        session: s.clone(),
        provider: provider.as_str().into(),
        home: home.clone(),
        state: "archiving".into(),
        archived_at: daemon::now(),
        bundle: o.id.clone(),
        conversation_ids: ids.clone(),
        entries: vec![],
    };
    for (i, p) in files.iter().enumerate() {
        not_open(p)?;
        let (bytes, digest) = tree_digest(p)?;
        m.entries.push(Entry {
            original: p.clone(),
            stored: i.to_string(),
            dir: fs::symlink_metadata(p)?.is_dir(),
            bytes,
            digest,
        });
    }
    let dir = bundle_dir(paths, &m)?;
    // The manifest names both locations before anything moves.
    save(paths, &m)?;
    phase(
        paths,
        o,
        "moving_history",
        "Moving the conversation's files into sessionsation-managed Archived Sessions.",
    )?;
    ensure_quiet(paths, provider, Some(&home), &ids)?;
    for e in &m.entries {
        not_open(&e.original)?;
        move_entry(&e.original, &dir.join(&e.stored), &e.digest)?;
    }
    m.state = "archived".into();
    m.session = crate::catalog::invalidate(paths, &s.id, &o.expected_generation)?;
    save(paths, &m)?;
    phase(
        paths,
        o,
        "complete",
        "Conversation files moved to Archived Sessions; the provider no longer lists or resumes it. Restore moves them back unchanged.",
    )
}

fn restore(paths: &Paths, provider: ProviderId, o: &mut Operation) -> Result<()> {
    let mut m = load(paths, &o.session.id)?.context("SESSION_NOT_ARCHIVED")?;
    ensure!(archived_state(&m.state), "SESSION_NOT_ARCHIVED");
    let s = crate::catalog::resolve(paths, &o.session.id, &o.expected_generation)?;
    let home = home_of(paths, provider, &s.account)?;
    ensure!(
        fs::canonicalize(&home).ok() == fs::canonicalize(&m.home).ok(),
        "ACCOUNT_HOME_CHANGED: the account home differs from the archived original; nothing was restored"
    );
    phase(
        paths,
        o,
        "checking_history",
        "Checking original locations and that no process runs this conversation.",
    )?;
    ensure_quiet(paths, provider, Some(&home), &m.conversation_ids)?;
    let dir = bundle_dir(paths, &m)?;
    for e in &m.entries {
        ensure!(e.original.starts_with(&m.home), "INVALID_HISTORY_PATH");
        ensure!(
            Path::new(&e.stored).components().count() == 1,
            "INVALID_ARCHIVE_BLOB"
        );
        let stored = dir.join(&e.stored);
        let original = fs::symlink_metadata(&e.original).is_ok();
        let kept = fs::symlink_metadata(&stored).is_ok();
        match (original, kept) {
            // Interrupted archive/restore: this entry is already in place.
            (true, false) => ensure!(tree_digest(&e.original)?.1 == e.digest, "RESTORE_CONFLICT"),
            (true, true) => bail!("RESTORE_CONFLICT: {} exists again", e.original.display()),
            (false, true) => ensure!(tree_digest(&stored)?.1 == e.digest, "ARCHIVE_CHANGED"),
            (false, false) => bail!("ARCHIVE_CORRUPT: {} is missing", e.stored),
        }
    }
    m.state = "restoring".into();
    save(paths, &m)?;
    phase(
        paths,
        o,
        "restoring_history",
        "Moving the conversation's files back to their original account-home paths.",
    )?;
    for e in &m.entries {
        let stored = dir.join(&e.stored);
        if fs::symlink_metadata(&stored).is_ok() {
            move_entry(&stored, &e.original, &e.digest)?;
        }
    }
    let _ = fs::remove_dir(&dir);
    m.state = "restored".into();
    m.session = crate::catalog::invalidate(paths, &s.id, &o.expected_generation)?;
    save(paths, &m)?;
    phase(
        paths,
        o,
        "complete",
        "Conversation restored to its account home as saved history; Restore never starts an agent.",
    )
}

#[derive(Serialize, Deserialize)]
struct Staged {
    original: PathBuf,
    staged: String,
}

fn delete(paths: &Paths, provider: ProviderId, o: &mut Operation) -> Result<()> {
    let prior = load(paths, &o.session.id)?;
    let archived = prior.as_ref().is_some_and(|m| archived_state(&m.state));
    let s = crate::catalog::resolve(paths, &o.session.id, &o.expected_generation)?;
    let home = match home_of(paths, provider, &s.account) {
        Ok(home) => Some(home),
        Err(_) if archived => None,
        Err(e) => return Err(e),
    };
    let ids = conversations(&s);
    phase(
        paths,
        o,
        "checking_history",
        "Checking that no process or hosted worker runs this conversation.",
    )?;
    ensure_quiet(paths, provider, home.as_deref(), &ids)?;
    // (current location, original path) of every file of this entry.
    let mut sources: Vec<(PathBuf, PathBuf)> = vec![];
    if let Some(m) = prior.as_ref().filter(|_| archived) {
        let dir = slot(paths, &m.session.id)?.join("native").join(&m.bundle);
        for e in &m.entries {
            let stored = dir.join(&e.stored);
            if fs::symlink_metadata(&stored).is_ok() {
                sources.push((stored, e.original.clone()));
            }
        }
    }
    if let Some(home) = &home {
        for p in collect(provider, home, &ids)? {
            not_open(&p)?;
            sources.push((p.clone(), p));
        }
        if provider == ProviderId::Claude {
            let set: BTreeSet<String> = ids.iter().cloned().collect();
            for p in claude::dead_registry_entries(home, &set)? {
                sources.push((p.clone(), p));
            }
        }
    }
    let staging = paths
        .root
        .join("deletions")
        .join(format!("native-{}", o.id));
    private_dir(&staging.join("files"))?;
    let staged: Vec<Staged> = sources
        .iter()
        .enumerate()
        .map(|(i, (_, original))| Staged {
            original: original.clone(),
            staged: i.to_string(),
        })
        .collect();
    atomic_write(
        &staging.join("plan.json"),
        &serde_json::to_vec(
            &json!({"version":1,"operation":o.id,"session":s,"provider":provider.as_str(),"conversation_ids":ids,"files":staged}),
        )?,
    )?;
    phase(
        paths,
        o,
        "deleting_history",
        "Staging this conversation's files for deletion; project files and credentials are unchanged.",
    )?;
    for ((current, _), st) in sources.iter().zip(&staged) {
        let digest = tree_digest(current)?.1;
        move_entry(current, &staging.join("files").join(&st.staged), &digest)?;
    }
    if let Some(home) = home.as_deref().filter(|_| provider == ProviderId::Claude) {
        for (_, original) in &sources {
            remove_empty_project_dir(home, original, s.directory.as_deref());
        }
    }
    let removed = redacted(crate::catalog::remove(paths, &s)?);
    save(
        paths,
        &Manifest {
            version: 1,
            session: removed,
            provider: provider.as_str().into(),
            // A deleted manifest keeps ids only: no name, directory or home path.
            home: PathBuf::new(),
            state: "deleted".into(),
            archived_at: daemon::now(),
            bundle: o.id.clone(),
            conversation_ids: ids,
            entries: vec![],
        },
    )?;
    let native_dir = slot(paths, &o.session.id)?.join("native");
    if native_dir.exists() {
        remove_tree(&native_dir)?;
    }
    remove_tree(&staging)?;
    phase(
        paths,
        o,
        "complete",
        "Conversation, its sidecar files and its catalog entry deleted. Project files, account credentials and upstream accounts are unchanged.",
    )
}

/// What a deleted manifest keeps: the ids that stop a replayed or stale
/// request (`id`, provider, account, conversation ids, revision) and nothing
/// that names the user's work (name, directory, pane attachment).
fn redacted(mut s: Session) -> Session {
    s.name = String::new();
    s.directory = None;
    s.attachment = None;
    s
}

/// Claude keeps a conversation in `<home>/projects/<encoded cwd>/`. After the
/// conversation's files are staged away, remove that directory only if it is
/// the direct project directory of the deleted file (and, when the entry
/// recorded a directory, the one that directory encodes to) and it is empty.
/// `remove_dir` (rmdir) fails on a non-empty directory, so no other file is
/// ever touched; symlinks are never followed or removed.
fn remove_empty_project_dir(home: &Path, original: &Path, directory: Option<&str>) {
    let Some(dir) = original.parent() else {
        return;
    };
    if dir.parent() != Some(home.join("projects").as_path()) {
        return;
    }
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    if let Some(cwd) = directory {
        let encoded: String = cwd
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        if encoded != name && cwd.replace('/', "-") != name {
            return;
        }
    }
    if fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_dir()) {
        let _ = fs::remove_dir(dir);
    }
}
