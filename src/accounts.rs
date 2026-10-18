//! sessionsation account membership. Codex's existing supervisor remains the sole
//! authentication owner. Removal unregisters locally without disabling or deleting
//! that provider enrollment, credentials, or conversation history.
use crate::{
    daemon, external,
    paths::{Paths, atomic_write, lock_file, private_dir, validate_name},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, os::unix::fs::MetadataExt, path::Path};

fn marker(paths: &Paths, name: &str) -> Result<std::path::PathBuf> {
    validate_name(name)?;
    Ok(paths
        .root
        .join("account-catalog/codex")
        .join(format!("{name}.removed.json")))
}
pub fn removed(paths: &Paths, name: &str) -> Result<bool> {
    let path = marker(paths, name)?;
    match fs::symlink_metadata(path) {
        Ok(m) => {
            ensure!(
                m.is_file() && !m.file_type().is_symlink(),
                "UNSAFE_ACCOUNT_MEMBERSHIP"
            );
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub fn ensure_available(paths: &Paths, name: &str) -> Result<()> {
    ensure!(
        !removed(paths, name)?,
        "ACCOUNT_REMOVED: restore the local membership before using this account"
    );
    Ok(())
}
pub fn operation_lock(paths: &Paths, name: &str) -> Result<fs::File> {
    validate_name(name)?;
    let dir = paths.root.join("account-catalog/locks");
    private_dir(&dir)?;
    let lock = lock_file(&dir.join(format!("codex-{name}.lock")))?;
    lock.try_lock_exclusive().context("ACCOUNT_BUSY")?;
    Ok(lock)
}
pub fn launch_lock(paths: &Paths, name: &str) -> Result<fs::File> {
    validate_name(name)?;
    let dir = paths.root.join("account-catalog/locks");
    private_dir(&dir)?;
    let lock = lock_file(&dir.join(format!("codex-{name}.lock")))?;
    FileExt::try_lock_shared(&lock).context("ACCOUNT_BUSY")?;
    Ok(lock)
}

fn files(root: &Path, path: &Path, output: &mut Vec<Value>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let m = fs::symlink_metadata(e.path())?;
        if m.is_dir() && !m.file_type().is_symlink() {
            files(root, &e.path(), output)?;
        } else {
            output.push(json!({"path":e.path().strip_prefix(root)?,"bytes":m.len(),"inode":m.ino(),"modified":m.mtime(),"modified_ns":m.mtime_nsec()}));
        }
    }
    Ok(())
}

pub async fn preview(paths: &Paths, name: &str) -> Result<Value> {
    validate_name(name)?;
    let status = daemon::call(paths, "status", json!({})).await?;
    let account = status["accounts"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["name"] == name))
        .context("ACCOUNT_NOT_FOUND")?;
    let home = paths.account(name)?;
    if let Ok(metadata) = fs::symlink_metadata(&home) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "UNSAFE_ACCOUNT_DATA"
        );
    }
    let mut stored = vec![];
    files(&home, &home, &mut stored)?;
    let active:Vec<_>=external::host_instances(paths)?.into_iter().filter(|i|i.home==home)
        .map(|i|json!({"target":i.pane,"conversation_id":i.thread_id,"pid":i.pid,"process_start":i.start_time})).collect();
    let loaded = if crate::control::available(&home) {
        crate::control::Control::connect(&home)
            .await
            .context("ACCOUNT_OWNERSHIP_UNVERIFIED")?
            .loaded()
            .await?
            .into_iter()
            .map(|t| json!({"conversation_id":t["id"],"status":t["status"]}))
            .collect::<Vec<_>>()
    } else {
        vec![]
    };
    let operations: Vec<_> = external::jobs(paths)?
        .into_iter()
        .filter(|j| j.target == name || j.source.home == home)
        .filter(|j| {
            !matches!(
                j.phase.as_str(),
                "complete" | "cancelled" | "failed" | "recovered"
            )
        })
        .map(|j| json!({"id":j.id,"phase":j.phase}))
        .collect();
    let history_ids = stored
        .iter()
        .filter_map(|f| f["path"].as_str())
        .filter_map(|p| p.strip_suffix(".jsonl"))
        .filter_map(|p| p.get(p.len().checked_sub(36)?..))
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .collect::<std::collections::BTreeSet<_>>();
    let affected = crate::catalog::list(paths)?
        .into_iter()
        .filter(|s| s.provider == "codex" && s.account == name)
        .map(|s| json!({"id":s.id,"name":s.name,"conversation_id":s.conversation_id}))
        .collect::<Vec<_>>();
    // Usage polling does not invalidate consent, but identity, enabled state,
    // affected sessions, ownership and changes to stored data do.
    let fingerprint = json!({"name":name,"enabled":account["enabled"],"auth_required":account["auth_required"],
        "stored":stored.iter().filter(|f| f["path"].as_str().is_some_and(|p| !p.starts_with("app-server-control/") && !p.starts_with("logs/") && p!=".switcher-lease")).collect::<Vec<_>>(),"active":active,"loaded":loaded,"operations":operations,"affected":affected});
    let revision = format!("{:x}", Sha256::digest(serde_json::to_vec(&fingerprint)?));
    Ok(
        json!({"account_id":format!("codex:{name}"),"removed":removed(paths,name)?,"revision":revision,
        "affected_sessions":affected,"active_sessions":active,"loaded_conversations":loaded,"pending_operations":operations,
        "can_remove":active.is_empty() && loaded.is_empty() && operations.is_empty(),
        "stored_data":{"home":home,"files":stored.len(),"bytes":stored.iter().filter_map(|f|f["bytes"].as_u64()).sum::<u64>(),
            "history_ids":history_ids,"history_files":stored.iter().filter(|f|f["path"].as_str().is_some_and(|s|s.ends_with(".jsonl"))).count()},
        "effects":{"unregister_local_membership":true,"disable_provider_enrollment":false,
            "stop_agents":false,"delete_credentials":false,"delete_history":false,"delete_upstream_account":false},
        "purge_supported":false}),
    )
}

pub async fn remove(paths: &Paths, name: &str, revision: &str, confirm: &str) -> Result<Value> {
    ensure!(confirm == format!("codex:{name}"), "CONFIRMATION_REQUIRED");
    let _account = operation_lock(paths, name)?;
    let jobs = paths.root.join("handoff-jobs");
    private_dir(&jobs)?;
    let queue = lock_file(&jobs.join("queue.lock"))?;
    queue.try_lock_exclusive().context("OPERATION_BUSY")?;
    let current = preview(paths, name).await?;
    ensure!(current["revision"] == revision, "STALE_ACCOUNT_PREVIEW");
    ensure!(current["can_remove"] == true, "ACCOUNT_IN_USE");
    let file = marker(paths, name)?;
    atomic_write(
        &file,
        &serde_json::to_vec(
            &json!({"schema_version":1,"removed_at":daemon::now(),"preview":current}),
        )?,
    )?;
    Ok(
        json!({"account_id":format!("codex:{name}"),"removed":true,"history_preserved":true,"credentials_preserved":true,"upstream_account_unchanged":true}),
    )
}

pub fn restore(paths: &Paths, name: &str) -> Result<Value> {
    let _account = operation_lock(paths, name)?;
    let file = marker(paths, name)?;
    ensure!(removed(paths, name)?, "ACCOUNT_NOT_REMOVED");
    fs::remove_file(&file)?;
    fs::File::open(file.parent().context("missing membership directory")?)?.sync_all()?;
    Ok(
        json!({"account_id":format!("codex:{name}"),"restored":true,"provider_credentials_unchanged":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_launches_share_membership_but_exclude_removal() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(d.path().join("state"))).unwrap();
        let a = launch_lock(&p, "test").unwrap();
        let b = launch_lock(&p, "test").unwrap();
        assert!(operation_lock(&p, "test").is_err());
        FileExt::unlock(&a).unwrap();
        FileExt::unlock(&b).unwrap();
        drop(a);
        drop(b);
        let removal = operation_lock(&p, "test").unwrap();
        assert!(launch_lock(&p, "test").is_err());
        drop(removal);
        assert!(launch_lock(&p, "test").is_ok());
    }
    #[test]
    fn removal_membership_never_erases_or_disables_provider_data() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(d.path().join("state"))).unwrap();
        let home = p.account("test").unwrap();
        private_dir(&home).unwrap();
        fs::write(home.join("history.jsonl"), b"recoverable fixture").unwrap();
        let file = marker(&p, "test").unwrap();
        atomic_write(&file, b"{}").unwrap();
        assert!(ensure_available(&p, "test").is_err());
        restore(&p, "test").unwrap();
        assert!(ensure_available(&p, "test").is_ok());
        assert_eq!(
            fs::read(home.join("history.jsonl")).unwrap(),
            b"recoverable fixture"
        );
    }
}
