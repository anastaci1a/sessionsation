//! Conversation labels and authoritative homes survive pane/account changes.
use super::*;

#[derive(Default, Serialize, Deserialize)]
struct Record {
    name: Option<String>,
    #[serde(default)]
    stop_intent: Option<String>,
    owner: Option<PathBuf>,
    #[serde(default)]
    deleting: bool,
    #[serde(default)]
    deleted: bool,
}
fn record_path(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id)?;
    let dir = paths.root.join("conversations");
    private_dir(&dir)?;
    Ok(dir.join(format!("{id}.json")))
}
fn read(paths: &Paths, id: &str) -> Result<Record> {
    let p = record_path(paths, id)?;
    if p.exists() {
        Ok(serde_json::from_slice(&fs::read(p)?)?)
    } else {
        Ok(Record::default())
    }
}
fn update(paths: &Paths, id: &str, change: impl FnOnce(&mut Record)) -> Result<()> {
    let p = record_path(paths, id)?;
    let lock = crate::paths::lock_file(&p.with_extension("lock"))?;
    lock.lock_exclusive()?;
    let mut record = read(paths, id)?;
    change(&mut record);
    atomic_write(&p, &serde_json::to_vec(&record)?)
}
pub(super) fn operation_lock(paths: &Paths, id: &str) -> Result<File> {
    uuid::Uuid::parse_str(id)?;
    let lock = crate::paths::lock_file(&jobs_dir(paths)?.join(format!("{id}.open-lock")))?;
    lock.try_lock_exclusive()
        .context("another operation is changing this conversation; retry shortly")?;
    Ok(lock)
}
pub(super) fn ensure_available(paths: &Paths, id: &str) -> Result<()> {
    let record = read(paths, id)?;
    ensure!(!record.deleted, "this conversation was permanently deleted");
    ensure!(
        !record.deleting,
        "conversation deletion is incomplete; retry Delete conversation to finish it"
    );
    Ok(())
}
pub(super) fn deletion_state(paths: &Paths, id: &str, complete: bool) -> Result<()> {
    update(paths, id, |r| {
        r.deleting = !complete;
        r.deleted = complete;
    })
}
pub(super) fn deleting(paths: &Paths, id: &str) -> Result<bool> {
    Ok(read(paths, id)?.deleting)
}
pub(super) fn label(source: &Instance) -> String {
    if source.tmux_only || source.needs_prefix() {
        return source.tmux_session.clone();
    }
    if !source.display_name.is_empty() {
        return opening::tmux_label(&source.display_name);
    }
    if !source.tmux_session.is_empty() {
        let name = &source.tmux_session;
        let suffix = format!("-{}", source.thread_id.chars().take(8).collect::<String>());
        return opening::tmux_label(if source.thread_id.is_empty() {
            name
        } else {
            name.strip_suffix(&suffix).unwrap_or(name)
        });
    }
    opening::tmux_label(
        Path::new(&source.cwd)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("saved"),
    )
}
pub(super) fn saved_name(paths: &Paths, id: &str) -> Result<Option<String>> {
    Ok(read(paths, id)?.name)
}
pub(super) fn set_name(paths: &Paths, id: &str, name: &str) -> Result<()> {
    update(paths, id, |r| r.name = Some(opening::tmux_label(name)))
}
pub(super) fn remember(paths: &Paths, source: &Instance) -> Result<()> {
    remember_name(paths, &source.thread_id, &label(source))
}
pub(super) fn remember_name(paths: &Paths, id: &str, name: &str) -> Result<()> {
    if read(paths, id)?.name.is_some() {
        return Ok(());
    }
    update(paths, id, |r| {
        if r.name.is_none() && !r.deleted && !r.deleting {
            r.name = Some(opening::tmux_label(name));
        }
    })
}
pub(super) fn decorate(paths: &Paths, items: &mut Vec<Instance>) -> Result<()> {
    for source in items.iter_mut().filter(|s| !s.thread_id.is_empty()) {
        // Capture the tmux label while its thread identity is still known. External
        // /exit or tmux termination may remove the pane before another operation.
        if !source.inactive && !source.tmux_session.is_empty() {
            remember(paths, source)?;
        }
        let mut record = read(paths, &source.thread_id)?;
        // Older discovery removed a prefix before passing the name to an
        // idempotent prefix helper, collapsing codex-codex-foo to codex-foo.
        // Repair only an exact match backed by this conversation's live pane.
        if !source.inactive
            && source.tmux_session.starts_with("codex-codex-")
            && record.name.as_deref() == source.tmux_session.strip_prefix("codex-")
        {
            update(paths, &source.thread_id, |r| {
                if !r.deleted && !r.deleting && r.name == record.name {
                    r.name = Some(source.tmux_session.clone());
                }
            })?;
            record = read(paths, &source.thread_id)?;
        }
        source.display_name = record
            .name
            .map(|name| opening::tmux_label(&name))
            .unwrap_or_else(|| label(source));
        if source.needs_prefix() {
            source.display_name = source.tmux_session.clone();
            source.warnings.push("Codex detected without its codex- prefix. Select this session to approve adding it before using Codex actions.".into());
        }
        if record.deleting {
            source.block_reason = Some("deleting".into());
            source.detail = "Deletion was interrupted. Choose Delete conversation again to finish; project files are untouched.".into();
        }
    }
    items.retain(|source| {
        let Ok(record) = read(paths, &source.thread_id) else {
            return true;
        };
        !record.deleted
            && (!source.inactive
                || source.archived
                || record
                    .owner
                    .as_ref()
                    .is_none_or(|home| home == &source.home))
    });
    Ok(())
}
pub(super) fn current_copy(paths: &Paths, source: &Instance) -> Result<bool> {
    let record = read(paths, &source.thread_id)?;
    Ok(!record.deleted
        && (source.archived
            || record
                .owner
                .as_ref()
                .is_none_or(|home| home == &source.home)))
}
pub async fn selected(paths: &Paths, key: &str) -> Result<Instance> {
    let items = if key.starts_with("archive:") {
        opening::archives(paths)?
    } else {
        discover(paths).await?
    };
    items
        .into_iter()
        .find(|s| {
            s.key() == key
                || key
                    .strip_prefix("thread:")
                    .is_some_and(|id| id == s.thread_id)
        })
        .context("session is no longer available; refresh Sessions")
}
pub(super) fn validate_label(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty()
            && name.len() <= 128
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || " _+-=~()[]".contains(c)),
        "use 1–128 characters: letters, numbers, spaces, hyphens, underscores or brackets"
    );
    Ok(())
}
pub async fn rename(paths: &Paths, key: &str, name: &str) -> Result<Value> {
    let name = name.trim();
    validate_label(name)?;
    let name = opening::tmux_label(name);
    let source = selected(paths, key).await?;
    let _lock = if source.thread_id.is_empty() {
        None
    } else {
        ensure_available(paths, &source.thread_id)?;
        Some(operation_lock(paths, &source.thread_id)?)
    };
    ensure!(
        source.move_id.is_none(),
        "finish the pending session operation before renaming"
    );
    let mut tmux_name = source.tmux_session.clone();
    if !source.inactive {
        let current = live_sessions(paths)
            .await?
            .into_iter()
            .find(|s| s.pane == source.pane && s.thread_id == source.thread_id)
            .context("session changed; refresh and rename it again")?;
        tmux_name = name.clone();
        if tmux_name != current.tmux_session && !source.pane.starts_with("herdr/") {
            tmux_inventory::release_ended_name(&tmux_name)?;
            tmux(&["rename-session", "-t", &source.pane, &tmux_name])?;
        }
    }
    if !source.thread_id.is_empty() {
        set_name(paths, &source.thread_id, &name)?;
    }
    Ok(
        json!({"schema_version":1,"thread_id":source.thread_id,"name":name,"tmux_session":tmux_name}),
    )
}
pub(super) async fn moved(paths: &Paths, job: &Job, target_home: &Path) -> Result<Option<String>> {
    update(paths, &job.source.thread_id, |r| {
        r.owner = Some(target_home.to_path_buf())
    })?;
    if job.source_archived {
        return Ok(None);
    }
    // Saved/direct sources are archived only after destination verification.
    let result = async {
        let mut control = crate::control::Control::connect(&job.source.home).await?;
        let thread = control.thread(&job.source.thread_id, false).await?;
        ensure!(
            thread["status"]["type"] == "notLoaded",
            "source was reopened"
        );
        control
            .request("thread/archive", json!({"threadId":job.source.thread_id}))
            .await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    Ok(result.err().map(|_| {
        "The old copy is retained for recovery and hidden from the regular session list.".into()
    }))
}

pub(super) fn messaging_owner(paths: &Paths, id: &str, home: &Path) -> Result<bool> {
    let record = read(paths, id)?;
    Ok(!record.deleted && !record.deleting && record.owner.is_none_or(|owner| owner == home))
}

/// Inventory only switcher's own naming/ownership metadata; no history scan.
pub(super) fn messaging_inventory(paths: &Paths) -> Result<Vec<String>> {
    let dir = paths.root.join("conversations");
    let mut ids = Vec::new();
    if !dir.exists() {
        return Ok(ids);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || entry.path().extension().is_none_or(|e| e != "json") {
            continue;
        }
        let path = entry.path();
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if uuid::Uuid::parse_str(id).is_err() {
            continue;
        }
        let r = read(paths, id)?;
        if !r.deleted && !r.deleting && r.owner.is_some() {
            ids.push(id.into());
        }
        ensure!(ids.len() <= 256, "discovery limit");
    }
    Ok(ids)
}

pub(super) fn stop_intent(paths: &Paths, id: &str) -> Result<Option<String>> {
    Ok(read(paths, id)?.stop_intent)
}
pub(super) fn set_stop_intent(paths: &Paths, id: &str, token: Option<String>) -> Result<()> {
    update(paths, id, |r| r.stop_intent = token)
}

pub(super) fn clear_stop_intent(paths: &Paths, id: &str, expected: Option<&str>) -> Result<()> {
    ensure!(
        clear_stop_intent_if_matches(paths, id, expected)?,
        "RECOVERY_STOP_INTENT_CHANGED_OBSERVE_AGAIN"
    );
    Ok(())
}
pub(super) fn clear_stop_intent_if_matches(
    paths: &Paths,
    id: &str,
    expected: Option<&str>,
) -> Result<bool> {
    let p = record_path(paths, id)?;
    let lock = crate::paths::lock_file(&p.with_extension("lock"))?;
    lock.lock_exclusive()?;
    let mut record = read(paths, id)?;
    if record.stop_intent.as_deref() != expected {
        return Ok(false);
    }
    if record.stop_intent.is_some() {
        record.stop_intent = None;
        atomic_write(&p, &serde_json::to_vec(&record)?)?;
    }
    Ok(true)
}

pub(super) fn restore_archived(paths: &Paths, id: &str, home: &Path) -> Result<()> {
    update(paths, id, |r| {
        r.deleted = false;
        r.deleting = false;
        r.owner = Some(home.into());
    })
}

#[cfg(test)]
mod stop_tests {
    use super::*;
    #[test]
    fn recovery_cannot_clear_a_newer_stop_intent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: dir.path().join("state"),
            runtime: dir.path().join("runtime"),
        };
        let id = uuid::Uuid::new_v4().to_string();
        set_stop_intent(&paths, &id, Some("new-stop".into())).unwrap();
        assert!(clear_stop_intent(&paths, &id, Some("old-stop")).is_err());
        assert_eq!(
            stop_intent(&paths, &id).unwrap().as_deref(),
            Some("new-stop")
        );
        assert!(clear_stop_intent(&paths, &id, None).is_err());
        clear_stop_intent(&paths, &id, Some("new-stop")).unwrap();
        assert!(stop_intent(&paths, &id).unwrap().is_none());
    }
}
