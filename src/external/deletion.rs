//! Explicit, irreversible deletion of inactive conversation history, never project files.
use super::*;
use crate::control::Control;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Copy {
    pub(super) home: Option<PathBuf>,
    pub(super) id: String,
    pub(super) path: PathBuf,
    pub(super) digest: String,
    #[serde(default)]
    parents: BTreeSet<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Plan {
    pub source: Instance,
    pub revision: String,
    pub confirmation: String,
    pub detail: String,
    pub thread_ids: BTreeSet<String>,
    pub(super) copies: Vec<Copy>,
}
impl Plan {
    pub fn history_copies(&self) -> usize {
        self.copies.len()
    }
}
pub(super) fn pending(paths: &Paths) -> Result<Vec<Instance>> {
    let mut items = Vec::new();
    let Ok(entries) = fs::read_dir(paths.root.join(crate::layout::DELETIONS)) else {
        return Ok(items);
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let plan: Plan = serde_json::from_slice(&fs::read(entry.path())?)?;
        if catalog::deleting(paths, &plan.source.thread_id)? {
            let mut source = plan.source;
            source.archived = false;
            source.inactive = true;
            source.block_reason = Some("deleting".into());
            source.detail = "Deletion was interrupted. Choose Delete conversation to review and finish it. Project files are intact.".into();
            items.push(source);
        }
    }
    Ok(items)
}
fn purge_input_history(home: &Path, ids: &BTreeSet<String>) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let path = home.join("history.jsonl");
    if !path.exists() {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    ensure!(
        file.metadata()?.is_file(),
        "input history is not a regular file"
    );
    // Codex's message-history writer takes the same exclusive flock. Keep the
    // inode, so another TUI's already-open append handle remains valid.
    file.try_lock_exclusive()
        .context("input history is busy; retry deletion to finish clearing its cached prompts")?;
    let mut before = Vec::new();
    file.read_to_end(&mut before)?;
    let mut after = Vec::new();
    for line in before.split_inclusive(|b| *b == b'\n') {
        let selected = serde_json::from_slice::<Value>(line)
            .ok()
            .is_some_and(|v| v["session_id"].as_str().is_some_and(|id| ids.contains(id)));
        if !selected {
            after.extend_from_slice(line);
        }
    }
    if after != before {
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&after)?;
        file.set_len(after.len() as u64)?;
        file.sync_all()?;
    }
    Ok(())
}
fn journal(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id)?;
    let dir = paths.root.join(crate::layout::DELETIONS);
    private_dir(&dir)?;
    Ok(dir.join(format!("{id}.json")))
}
fn homes(paths: &Paths, source: &Instance) -> Result<BTreeSet<PathBuf>> {
    let mut homes = BTreeSet::from([source.home.clone()]);
    homes.extend(host_instances(paths)?.into_iter().map(|s| s.home));
    if let Some(home) = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
    {
        if let Ok(home) = fs::canonicalize(home) {
            homes.insert(home);
        }
    }
    for job in jobs(paths)? {
        homes.insert(job.source.home);
    }
    for (_, home) in crate::layout::codex_homes(&paths.root) {
        homes.insert(fs::canonicalize(home)?);
    }
    homes.extend(super::preserved_homes(paths)?.into_iter().map(|(_, h)| h));
    Ok(homes)
}
fn references(value: &Value, ids: &BTreeSet<String>) -> bool {
    match value {
        Value::String(s) => ids.contains(s),
        Value::Array(items) => items.iter().any(|v| references(v, ids)),
        Value::Object(items) => items.values().any(|v| references(v, ids)),
        _ => false,
    }
}
fn scan(paths: &Paths, source: &Instance) -> Result<(BTreeSet<String>, Vec<Copy>)> {
    let mut candidates = Vec::new();
    for home in homes(paths, source)? {
        for dir in ["sessions", "archived_sessions"] {
            let mut files = Vec::new();
            collect_rollouts(&home.join(dir), &mut files)?;
            candidates.extend(files.into_iter().map(|p| (Some(home.clone()), p)));
        }
    }
    let mut backups = Vec::new();
    collect_rollouts(&paths.root.join(crate::layout::MOVE_BACKUPS), &mut backups)?;
    candidates.extend(backups.into_iter().map(|p| (None, p)));
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter_map(|(h, p)| session_meta(&p).ok().map(|m| (h, p, m)))
        .collect();
    let mut ids = BTreeSet::from([source.thread_id.clone()]);
    loop {
        let before = ids.len();
        for (_, _, meta) in &candidates {
            if references(&meta["source"], &ids) {
                if let Some(id) = meta["id"]
                    .as_str()
                    .filter(|id| uuid::Uuid::parse_str(id).is_ok())
                {
                    ids.insert(id.into());
                }
            }
        }
        if before == ids.len() {
            break;
        }
    }
    let mut copies = Vec::new();
    for (home, path, meta) in candidates {
        if let Some(id) = meta["id"].as_str().filter(|id| ids.contains(*id)) {
            let digest = format!("{:x}", Sha256::digest(fs::read(&path)?));
            copies.push(Copy {
                home,
                id: id.into(),
                path,
                digest,
                parents: ids
                    .iter()
                    .filter(|parent| {
                        *parent != id
                            && references(&meta["source"], &BTreeSet::from([(*parent).clone()]))
                    })
                    .cloned()
                    .collect(),
            });
        }
    }
    copies.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((ids, copies))
}
pub(super) async fn inactive(
    paths: &Paths,
    source: &Instance,
    ids: &BTreeSet<String>,
) -> Result<()> {
    ensure!(
        source.inactive,
        "end this live tmux session first; deletion is only for inactive conversations"
    );
    ensure!(
        !live_sessions(paths)
            .await?
            .iter()
            .any(|s| ids.contains(&s.thread_id)),
        "a copy of this conversation or its subagent is live in tmux; end it before deleting"
    );
    for job in jobs(paths)? {
        ensure!(
            !ids.contains(&job.source.thread_id)
                || matches!(
                    job.phase.as_str(),
                    "complete" | "failed" | "cancelled" | "recovered"
                ),
            "a session operation is still pending; finish or cancel it before deleting"
        );
    }
    Ok(())
}
pub async fn preview(paths: &Paths, key: &str) -> Result<Plan> {
    let source = match catalog::selected(paths, key).await {
        Ok(source) => source,
        Err(error) => {
            let Some(id) = key
                .strip_prefix("thread:")
                .or_else(|| key.rsplit(':').next())
            else {
                return Err(error);
            };
            let Ok(p) = journal(paths, id) else {
                return Err(error);
            };
            if !p.exists() {
                return Err(error);
            }
            serde_json::from_slice::<Plan>(&fs::read(p)?)?.source
        }
    };
    ensure!(
        source.inactive,
        "end this live tmux session first; deletion is only for inactive conversations"
    );
    let (mut thread_ids, copies) = scan(paths, &source)?;
    if catalog::deleting(paths, &source.thread_id)? {
        let prior: Plan = serde_json::from_slice(&fs::read(journal(paths, &source.thread_id)?)?)?;
        thread_ids.extend(prior.thread_ids);
    }
    inactive(paths, &source, &thread_ids).await?;
    let revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&copies)?));
    let confirmation = format!("DELETE {}", catalog::label(&source));
    Ok(Plan { source, revision, confirmation, thread_ids, copies, detail: "Permanently delete this conversation and its subagent history from every known local account, including archives and switcher history backups. /resume and switcher recovery cannot undo this. Project folders and their files are NOT deleted. Copies on other machines or external backups are outside this operation.".into() })
}
/// Permanent user-facing deletion: the history, then every leftover that names
/// the conversation (session-catalog entry, recovery journal, label, home path).
pub async fn delete(paths: &Paths, key: &str, confirm: &str, revision: &str) -> Result<Value> {
    let plan = preview(paths, key).await?;
    let result = delete_history(paths, key, confirm, revision).await?;
    for session in catalog_entries(paths, &plan.thread_ids)? {
        crate::catalog::remove(paths, &session)?;
    }
    forget(paths, &plan.thread_ids, confirm)?;
    Ok(result)
}

/// Deletes the history only; archiving reuses it and keeps its catalog entry.
pub(super) async fn delete_history(
    paths: &Paths,
    key: &str,
    confirm: &str,
    revision: &str,
) -> Result<Value> {
    let initial = preview(paths, key).await?;
    ensure!(
        confirm == initial.source.thread_id,
        "confirmation must match the exact conversation UUID from the preview"
    );
    let mut locks = Vec::new();
    for id in &initial.thread_ids {
        locks.push(catalog::operation_lock(paths, id)?);
    }
    let plan = preview(paths, key).await?;
    ensure!(
        plan.thread_ids == initial.thread_ids && plan.revision == revision,
        "conversation history changed; review a fresh deletion preview before confirming"
    );
    let mut controls = std::collections::BTreeMap::new();
    let mut pairs = BTreeSet::new();
    for copy in &plan.copies {
        if let Some(home) = &copy.home
            && !archiving::file_only_home(paths, home)?
        {
            pairs.insert((home.clone(), copy.id.clone()));
        }
    }
    let mut remaining = pairs.clone();
    let mut order = Vec::new();
    while !remaining.is_empty() {
        let leaf = remaining
            .iter()
            .find(|(home, id)| {
                !plan.copies.iter().any(|c| {
                    c.home.as_ref() == Some(home)
                        && c.parents.contains(id)
                        && remaining.contains(&(home.clone(), c.id.clone()))
                })
            })
            .cloned()
            .context("conversation history contains a dependency cycle; nothing was deleted")?;
        remaining.remove(&leaf);
        order.push(leaf);
    }
    // All accounts must be reachable and all copies unloaded before deleting any.
    for (home, id) in &pairs {
        if !controls.contains_key(home) {
            let account = account_for_home(paths, home);
            let control = if crate::control::available(home) {
                Control::connect(home).await?
            } else {
                ensure!(
                    !matches!(account.as_str(), "external" | "unknown"),
                    "the original Codex server is unavailable; restart Codex for that home, then retry deletion"
                );
                daemon::ensure_started(paths).await?;
                account_control(paths, &account).await?
            };
            controls.insert(home.clone(), control);
        }
        let thread = controls.get_mut(home).unwrap().thread(id, false).await?;
        ensure!(
            thread["status"]["type"] == "notLoaded",
            "conversation {id} is still loaded in Codex; close it before deleting"
        );
    }
    for copy in &plan.copies {
        ensure!(
            !rollout_still_open(&copy.path)?,
            "conversation history still has a writer; wait before deleting"
        );
    }
    atomic_write(&journal(paths, confirm)?, &serde_json::to_vec(&plan)?)?;
    for id in &plan.thread_ids {
        catalog::deletion_state(paths, id, false)?;
    }
    for (home, id) in &order {
        let control = controls.get_mut(home).unwrap();
        ensure!(
            control.thread(id, false).await?["status"]["type"] == "notLoaded",
            "conversation was reopened; retry deletion when closed"
        );
        control
            .request_timeout(
                "thread/delete",
                json!({"threadId":id}),
                Duration::from_secs(120),
            )
            .await?;
    }
    // Native deletion removes indexed history; remove any additional exact-ID
    // rollout copies and switcher backups too. Never remove a directory tree.
    for copy in &plan.copies {
        if copy.path.exists() {
            ensure!(
                session_meta(&copy.path)?["id"] == copy.id && !rollout_still_open(&copy.path)?,
                "history copy changed during deletion; retry after review"
            );
            ensure!(
                !fs::symlink_metadata(&copy.path)?.file_type().is_symlink(),
                "history became a symlink; file retained"
            );
            fs::remove_file(&copy.path)?;
        }
    }
    for home in homes(paths, &plan.source)? {
        purge_input_history(&home, &plan.thread_ids)?;
    }
    for id in &plan.thread_ids {
        catalog::deletion_state(paths, id, true)?;
    }
    Ok(
        json!({"schema_version":1,"deleted":true,"thread_id":confirm,"history_copies":plan.copies.len(),"project_files_deleted":false}),
    )
}

/// Session-catalog entries of these exact Codex conversations.
fn catalog_entries(paths: &Paths, ids: &BTreeSet<String>) -> Result<Vec<crate::catalog::Session>> {
    Ok(crate::catalog::list(paths)?
        .into_iter()
        .filter(|s| {
            s.provider == "codex"
                && (ids.contains(&s.conversation_id)
                    || s.previous_conversation_ids.iter().any(|p| ids.contains(p)))
        })
        .collect())
}

/// After a completed permanent deletion: the conversation's label and home path
/// go from its record, and its recovery journal (name, directory) is removed.
/// Recovery needs both only while the record says `deleting`.
pub(super) fn forget(paths: &Paths, ids: &BTreeSet<String>, thread_id: &str) -> Result<()> {
    for id in ids {
        catalog::redact_deleted(paths, id)?;
    }
    let _ = fs::remove_file(journal(paths, thread_id)?);
    Ok(())
}

/// A deleted archive record keeps ids and state, not what names the work.
pub(super) fn redact_source(source: &mut Instance) {
    source.display_name.clear();
    source.tmux_session.clear();
    source.cwd.clear();
    source.rollout = PathBuf::new();
    source.home = PathBuf::new();
    source.detail.clear();
    source.warnings.clear();
}
