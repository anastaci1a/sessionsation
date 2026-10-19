//! Durable CLI-owned local account deletion. Admission never scans account files.
use crate::{
    accounts, catalog, daemon, external,
    paths::{Paths, atomic_write, lock_file, private_dir, validate_name},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
};
#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub account_id: String,
    pub kind: String,
    pub phase: String,
    pub detail: String,
    /// Immutable admission time; absent for legacy receipts whose creation is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    pub updated_at: i64,
    pub history_home: PathBuf,
    pub source_inode: Option<u64>,
    pub can_cancel: bool,
    #[serde(default)]
    pub can_recover: bool,
    pub history_preserved: bool,
}
fn record_file(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_REQUEST_ID")?;
    Ok(paths
        .root
        .join("account-deletions")
        .join(format!("{id}.json")))
}
fn save(paths: &Paths, r: &mut Record) -> Result<()> {
    r.updated_at = daemon::now();
    atomic_write(&record_file(paths, &r.id)?, &serde_json::to_vec_pretty(r)?)
}
pub fn status(paths: &Paths, id: &str) -> Result<Record> {
    let mut r: Record = serde_json::from_slice(
        &fs::read(record_file(paths, id)?).context("ACCOUNT_DELETION_NOT_FOUND")?,
    )?;
    let stopped = worker_stopped(paths, id)?;
    if !matches!(r.phase.as_str(), "failed" | "complete")
        && daemon::now() - r.updated_at > 10
        && stopped
    {
        r.phase = "failed".into();
        r.detail = "Deletion worker exited without a final receipt. History remains retained; Recover explicitly rechecks and continues this original request.".into();
    }
    r.can_recover = r.phase == "failed" && stopped;
    Ok(r)
}
fn worker_stopped(paths: &Paths, id: &str) -> Result<bool> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(record_file(paths, id)?.with_extension("worker.lock"))
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e.into()),
    };
    match file.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub fn list(paths: &Paths) -> Result<Vec<Record>> {
    let dir = paths.root.join("account-deletions");
    let mut out = vec![];
    if dir.is_dir() {
        for e in fs::read_dir(dir)? {
            let e = e?;
            if e.file_type()?.is_file() && e.path().extension().is_some_and(|x| x == "json") {
                let r: Record = serde_json::from_slice(&fs::read(e.path())?)?;
                out.push(status(paths, &r.id)?);
            }
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    Ok(out)
}
/// Fast intent preview: catalog and owner metadata only, no account tree access.
pub async fn preview(paths: &Paths, name: &str) -> Result<Value> {
    validate_name(name)?;
    let state = daemon::call(paths, "status", json!({})).await?;
    ensure!(
        state["accounts"]
            .as_array()
            .is_some_and(|aa| aa.iter().any(|a| a["name"] == name)),
        "ACCOUNT_NOT_FOUND"
    );
    let affected: Vec<_> = catalog::list(paths)?
        .into_iter()
        .filter(|s| s.provider == "codex" && s.account == name)
        .map(|s| json!({"id":s.id,"name":s.name,"running":s.attachment.is_some()}))
        .collect();
    Ok(
        json!({"account_id":format!("codex:{name}"),"can_delete":state["capabilities"]["account_deletion"]==true&&!affected.iter().any(|s|s["running"]==true),"owner_upgrade_required":state["capabilities"]["account_deletion"]!=true,"affected_sessions":affected,"preserves_history":true,"deletes_local_credentials":true,"upstream_account_unchanged":true,"file_scanning":false}),
    )
}
fn spawn(paths: &Paths, r: &Record) -> Result<()> {
    crate::background::detach(&mut std::process::Command::new(
        crate::self_executable::path()?,
    ))
    .arg("--data-dir")
    .arg(&paths.root)
    .args(["account-delete-worker", &r.id])
    .env_remove("CODEX_HOME")
    .env_remove("OPENAI_API_KEY")
    .env_remove("CODEX_API_KEY")
    .env_remove("CODEX_ACCESS_TOKEN")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()?;
    Ok(())
}
fn account_home(paths: &Paths, name: &str) -> Result<PathBuf> {
    let parent = fs::symlink_metadata(paths.root.join("accounts"))?;
    ensure!(
        parent.is_dir()
            && !parent.file_type().is_symlink()
            && parent.uid() == unsafe { libc::geteuid() },
        "UNSAFE_ACCOUNT_PARENT"
    );
    paths.account(name)
}
pub async fn start(paths: &Paths, name: &str, id: &str, confirm: &str) -> Result<Value> {
    validate_name(name)?;
    ensure!(confirm == format!("codex:{name}"), "CONFIRMATION_REQUIRED");
    record_file(paths, id)?;
    let root = paths.root.join("account-deletions");
    private_dir(&root)?;
    let lock = lock_file(&root.join("admission.lock"))?;
    lock.lock_exclusive()?;
    if record_file(paths, id)?.exists() {
        let r = status(paths, id)?;
        ensure!(r.account_id == confirm, "REQUEST_ID_CONFLICT");
        return Ok(json!(r));
    }
    ensure!(
        !list(paths)?
            .iter()
            .any(|r| r.account_id == confirm && r.phase != "complete"),
        "ACCOUNT_DELETION_IN_PROGRESS"
    );
    let owner = daemon::call(paths, "status", json!({})).await?;
    ensure!(
        owner["capabilities"]["account_deletion"] == true,
        "OWNER_UPGRADE_REQUIRED"
    );
    ensure!(
        owner["accounts"]
            .as_array()
            .is_some_and(|aa| aa.iter().any(|a| a["name"] == name)),
        "ACCOUNT_NOT_FOUND"
    );
    let home = fs::symlink_metadata(account_home(paths, name)?)?;
    ensure!(
        home.is_dir() && !home.file_type().is_symlink(),
        "UNSAFE_ACCOUNT_DATA"
    );
    let mut r = Record {
        id: id.into(),
        account_id: confirm.into(),
        kind: "account_delete".into(),
        phase: "admitted".into(),
        detail: "Checking that the account has no active or loaded conversations.".into(),
        created_at: Some(daemon::now()),
        updated_at: daemon::now(),
        history_home: paths
            .root
            .join("preserved-conversations")
            .join(id)
            .join("home"),
        source_inode: Some(home.ino()),
        can_cancel: false,
        can_recover: false,
        history_preserved: false,
    };
    save(paths, &mut r)?;
    if let Err(e) = spawn(paths, &r) {
        r.phase = "failed".into();
        r.detail = format!("Worker did not start: {e}");
        save(paths, &mut r)?;
    }
    Ok(json!(r))
}
pub fn recover(paths: &Paths, id: &str) -> Result<Value> {
    let admission = lock_file(&paths.root.join("account-deletions/admission.lock"))?;
    admission.lock_exclusive()?;
    let worker = lock_file(&record_file(paths, id)?.with_extension("worker.lock"))?;
    worker
        .try_lock_exclusive()
        .context("ACCOUNT_DELETION_WORKER_RUNNING")?;
    let mut r = status(paths, id)?;
    ensure!(r.phase != "complete", "ACCOUNT_DELETION_NOT_RECOVERABLE");
    r.phase = "admitted".into();
    r.detail = "Explicit recovery requested; rechecking ownership before continuing.".into();
    save(paths, &mut r)?;
    drop(worker);
    if let Err(e) = spawn(paths, &r) {
        r.phase = "failed".into();
        r.detail = format!("Recovery worker did not start: {e}");
        save(paths, &mut r)?;
    }
    Ok(json!(r))
}
fn phase(paths: &Paths, r: &mut Record, p: &str, detail: &str) -> Result<()> {
    r.phase = p.into();
    r.detail = detail.into();
    save(paths, r)
}
pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    let guard = lock_file(&record_file(paths, id)?.with_extension("worker.lock"))?;
    guard.try_lock_exclusive()?;
    let mut r = status(paths, id)?;
    ensure!(r.phase == "admitted", "ACCOUNT_DELETION_ALREADY_DISPATCHED");
    if let Err(e) = perform(paths, &mut r).await {
        r.phase = "failed".into();
        r.detail = format!(
            "{e:#}. Original or preserved conversation data is retained. Inspect this request; it was not automatically retried."
        );
        save(paths, &mut r)?;
    }
    Ok(())
}
pub(crate) async fn idle(paths: &Paths, home: &Path) -> Result<()> {
    ensure!(
        !external::home_has_processes(home, true)?,
        "ACCOUNT_HAS_RUNNING_PROCESSES"
    );
    ensure!(
        !external::host_instances(paths)?
            .iter()
            .any(|s| s.home == home),
        "ACCOUNT_IN_USE"
    );
    if crate::control::available(home) {
        ensure!(
            crate::control::Control::connect(home)
                .await?
                .loaded()
                .await?
                .is_empty(),
            "ACCOUNT_HAS_LOADED_CONVERSATIONS"
        );
    }
    ensure!(
        !external::jobs(paths)?.iter().any(|j| (j.source.home == home
            || paths.account(&j.target).ok().as_deref() == Some(home))
            && !matches!(
                j.phase.as_str(),
                "complete" | "failed" | "cancelled" | "recovered"
            )),
        "ACCOUNT_HAS_PENDING_OPERATIONS"
    );
    Ok(())
}
fn history_entry(name: &str) -> bool {
    matches!(
        name,
        "sessions" | "archived_sessions" | "history.jsonl" | "session_index.jsonl"
    ) || [".sqlite", ".sqlite-wal", ".sqlite-shm"]
        .iter()
        .any(|suffix| {
            name.strip_suffix(suffix).is_some_and(|stem| {
                stem == "state"
                    || stem
                        .strip_prefix("state_")
                        .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
            })
        })
}
fn no_symlinks(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    ensure!(!m.file_type().is_symlink(), "HISTORY_SYMLINK_UNVERIFIED");
    if m.is_dir() {
        for e in fs::read_dir(path)? {
            no_symlinks(&e?.path())?;
        }
    }
    Ok(())
}
fn preserve(home: &Path, vault: &Path) -> Result<()> {
    private_dir(vault)?;
    for e in fs::read_dir(home)? {
        let e = e?;
        let name = e.file_name();
        if !history_entry(name.to_str().context("INVALID_HISTORY_NAME")?) {
            continue;
        }
        no_symlinks(&e.path())?;
        let dest = vault.join(name);
        ensure!(!dest.exists(), "PRESERVED_HISTORY_COLLISION");
        fs::rename(e.path(), &dest).context("HISTORY_MOVE_FAILED_SOURCE_RETAINED")?;
        fs::File::open(home)?.sync_all()?;
        fs::File::open(vault)?.sync_all()?;
    }
    Ok(())
}
async fn perform(paths: &Paths, r: &mut Record) -> Result<()> {
    ensure!(
        r.history_home
            == paths
                .root
                .join("preserved-conversations")
                .join(&r.id)
                .join("home"),
        "INVALID_HISTORY_VAULT"
    );
    let name = r
        .account_id
        .strip_prefix("codex:")
        .context("UNSUPPORTED_PROVIDER_OPERATION")?
        .to_owned();
    let _account = accounts::operation_lock(paths, &name)?;
    let jobs = paths.root.join("handoff-jobs");
    private_dir(&jobs)?;
    let queue = lock_file(&jobs.join("queue.lock"))?;
    queue.try_lock_exclusive().context("OPERATION_BUSY")?;
    let home = account_home(paths, &name)?;
    if !home.exists() {
        ensure!(
            r.source_inode.is_some() && r.history_home.is_dir() && r.history_preserved,
            "ACCOUNT_DATA_UNAVAILABLE"
        );
        external::verify_preserved_history(&r.history_home)?;
        phase(
            paths,
            r,
            "deleting_local_data",
            "Completing the original account deletion after verified history preservation.",
        )?;
        daemon::call(
            paths,
            "account/delete/finish",
            json!({"name":name,"request_id":r.id}),
        )
        .await?;
        return phase(
            paths,
            r,
            "complete",
            "Local account files and credentials deleted; conversation history retained.",
        );
    }
    let m = fs::symlink_metadata(&home)?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink(),
        "UNSAFE_ACCOUNT_DATA"
    );
    if let Some(inode) = r.source_inode {
        ensure!(inode == m.ino(), "ACCOUNT_HOME_REPLACED");
    } else {
        r.source_inode = Some(m.ino());
        save(paths, r)?;
    }
    phase(
        paths,
        r,
        "checking_active",
        "Checking running sessions, loaded conversations and pending operations.",
    )?;
    idle(paths, &home).await?;
    let lease = lock_file(&home.join(".switcher-lease"))?;
    lease.try_lock_exclusive().context("ACCOUNT_BUSY")?;
    phase(
        paths,
        r,
        "disabling_account",
        "Disabling future launches through the existing account owner.",
    )?;
    daemon::call(
        paths,
        "account/delete/prepare",
        json!({"name":name,"request_id":r.id}),
    )
    .await?;
    idle(paths, &home).await?;
    // Validate each parent before creating the vault or moving private data;
    // checking only the leaf would permit traversal through a parent symlink.
    private_dir(&paths.root.join("preserved-conversations"))?;
    private_dir(r.history_home.parent().context("INVALID_HISTORY_VAULT")?)?;
    external::preserve_history_checks(&home, &r.history_home).await?;
    // Native page verification can take time. Recheck immediately before the
    // metadata stop boundary, rather than relying on the pre-scan observation.
    idle(paths, &home).await?;
    phase(
        paths,
        r,
        "stopping_metadata",
        "Stopping only this idle account's metadata server.",
    )?;
    if crate::control::available(&home) {
        let config = crate::store::load_config(paths)?;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio::process::Command::new(config.codex_binary)
                .args(["app-server", "daemon", "stop"])
                .env("CODEX_HOME", &home)
                .env_remove("OPENAI_API_KEY")
                .env_remove("CODEX_API_KEY")
                .env_remove("CODEX_ACCESS_TOKEN")
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("ACCOUNT_SERVER_STOP_UNCERTAIN")??;
        ensure!(out.status.success(), "ACCOUNT_SERVER_STOP_FAILED");
        ensure!(
            !crate::control::available(&home),
            "ACCOUNT_SERVER_STILL_RUNNING"
        );
    }
    ensure!(
        !external::home_has_processes(&home, false)?,
        "ACCOUNT_HAS_RUNNING_PROCESSES"
    );
    phase(
        paths,
        r,
        "preserving_history",
        "Moving saved/archived conversations and native history metadata to private preserved storage.",
    )?;
    let manifest = r
        .history_home
        .parent()
        .context("INVALID_HISTORY_VAULT")?
        .join("account.json");
    atomic_write(
        &manifest,
        &serde_json::to_vec(
            &json!({"version":1,"account":name,"request_id":r.id,"home":r.history_home}),
        )?,
    )?;
    preserve(&home, &r.history_home)?;
    external::verify_preserved_history(&r.history_home)?;
    r.history_preserved = true;
    save(paths, r)?;
    idle(paths, &home).await?;
    ensure!(
        !external::home_has_processes(&home, false)?,
        "ACCOUNT_HAS_RUNNING_PROCESSES"
    );
    ensure!(
        fs::symlink_metadata(&home)?.ino() == r.source_inode.unwrap(),
        "ACCOUNT_HOME_REPLACED"
    );
    phase(
        paths,
        r,
        "deleting_local_data",
        "Deleting this account's remaining local files and credentials; preserved conversations are outside this directory.",
    )?;
    // The account operation lock and owner reservation remain held. Close the
    // in-home lease before unlinking it: FUSE retains open unlinked files as
    // .fuse_hidden entries, making the final rmdir fail with ENOTEMPTY.
    drop(lease);
    fs::remove_dir_all(&home).context("ACCOUNT_LOCAL_DATA_CLEANUP_FAILED")?;
    fs::File::open(paths.root.join("accounts"))?.sync_all()?;
    daemon::call(
        paths,
        "account/delete/finish",
        json!({"name":name,"request_id":r.id}),
    )
    .await?;
    phase(
        paths,
        r,
        "complete",
        "Local account files and credentials deleted. Saved conversations remain preserved; upstream account unchanged.",
    )
}
/// Histories stay in discovery under their original account identity after the
/// registration and credential home have been deleted by the sole owner.
pub fn preserved_homes(paths: &Paths) -> Result<Vec<(String, PathBuf)>> {
    Ok(list(paths)?
        .into_iter()
        .filter(|r| r.history_preserved && r.history_home.is_dir())
        .filter_map(|r| {
            r.account_id
                .strip_prefix("codex:")
                .map(|name| (name.to_owned(), r.history_home.clone()))
        })
        .collect())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn orphaned_deletion_is_recoverable_but_live_worker_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        private_dir(&paths.root.join("account-deletions")).unwrap();
        let r = json!({"id":id,"account_id":"codex:fixture","kind":"account_delete","phase":"checking_active","detail":"checking","updated_at":0,"history_home":"/fixture","source_inode":null,"can_cancel":false,"history_preserved":true});
        atomic_write(
            &record_file(&paths, &id).unwrap(),
            &serde_json::to_vec(&r).unwrap(),
        )
        .unwrap();
        let orphan = status(&paths, &id).unwrap();
        assert_eq!(orphan.phase, "failed");
        assert!(orphan.can_recover);
        let lock = lock_file(
            &record_file(&paths, &id)
                .unwrap()
                .with_extension("worker.lock"),
        )
        .unwrap();
        lock.lock_exclusive().unwrap();
        let live = status(&paths, &id).unwrap();
        assert_eq!(live.phase, "checking_active");
        assert!(!live.can_recover);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(record_file(&paths, &id).unwrap()).unwrap())
                .unwrap(),
            r,
            "read projection never rewrites a receipt"
        );
    }
    #[tokio::test]
    async fn replaced_account_home_and_symlink_parent_are_rejected_before_data_changes() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(d.path().join("root"))).unwrap();
        let home = paths.account("disposable").unwrap();
        private_dir(&home).unwrap();
        fs::write(home.join("auth.json"), "synthetic credential").unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let mut r = Record {
            id: id.clone(),
            account_id: "codex:disposable".into(),
            kind: "account_delete".into(),
            phase: "admitted".into(),
            detail: String::new(),
            created_at: None,
            updated_at: 0,
            history_home: paths
                .root
                .join("preserved-conversations")
                .join(id)
                .join("home"),
            source_inode: Some(fs::metadata(&home).unwrap().ino().wrapping_add(1)),
            can_cancel: false,
            can_recover: false,
            history_preserved: false,
        };
        assert!(
            perform(&paths, &mut r)
                .await
                .unwrap_err()
                .to_string()
                .contains("ACCOUNT_HOME_REPLACED")
        );
        assert_eq!(
            fs::read(home.join("auth.json")).unwrap(),
            b"synthetic credential"
        );
        assert!(!r.history_home.exists());
        assert!(!paths.root.join("registry.db").exists());
        fs::rename(
            paths.root.join("accounts"),
            paths.root.join("retained_accounts"),
        )
        .unwrap();
        let outside = d.path().join("outside");
        fs::create_dir_all(outside.join("disposable")).unwrap();
        fs::write(outside.join("disposable/auth.json"), "outside credential").unwrap();
        std::os::unix::fs::symlink(&outside, paths.root.join("accounts")).unwrap();
        assert!(
            perform(&paths, &mut r)
                .await
                .unwrap_err()
                .to_string()
                .contains("UNSAFE_ACCOUNT_PARENT")
        );
        assert_eq!(
            fs::read(outside.join("disposable/auth.json")).unwrap(),
            b"outside credential"
        );
        assert!(
            paths
                .root
                .join("retained_accounts/disposable/auth.json")
                .is_file()
        );
    }
    #[test]
    fn interrupted_history_move_retains_both_halves_and_recovers_without_credentials() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("account");
        let vault = d.path().join("vault/home");
        fs::create_dir_all(home.join("sessions")).unwrap();
        fs::create_dir_all(home.join("archived_sessions")).unwrap();
        private_dir(&vault).unwrap();
        fs::write(home.join("sessions/a.jsonl"), "saved").unwrap();
        fs::write(home.join("archived_sessions/b.jsonl"), "archived").unwrap();
        fs::write(home.join("auth.json"), "secret").unwrap();
        fs::rename(home.join("sessions"), vault.join("sessions")).unwrap();
        preserve(&home, &vault).unwrap();
        assert_eq!(fs::read(vault.join("sessions/a.jsonl")).unwrap(), b"saved");
        assert_eq!(
            fs::read(vault.join("archived_sessions/b.jsonl")).unwrap(),
            b"archived"
        );
        assert!(home.join("auth.json").is_file());
        assert!(!vault.join("auth.json").exists());
        assert!(!history_entry("state_secret.sqlite"));
        assert!(history_entry("state_5.sqlite-wal"));
    }
    #[test]
    fn history_move_preserves_native_data_and_excludes_credentials() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("account");
        fs::create_dir_all(home.join("sessions")).unwrap();
        fs::create_dir_all(home.join("archived_sessions")).unwrap();
        for (p, b) in [
            ("auth.json", "secret"),
            ("state_5.sqlite", "native metadata"),
            ("sessions/conversation.jsonl", "conversation"),
            ("archived_sessions/old.jsonl", "archive"),
        ] {
            fs::write(home.join(p), b).unwrap();
        }
        let vault = d.path().join("vault/home");
        preserve(&home, &vault).unwrap();
        assert!(!vault.join("auth.json").exists());
        assert_eq!(
            fs::read(vault.join("sessions/conversation.jsonl")).unwrap(),
            b"conversation"
        );
        assert_eq!(
            fs::read(vault.join("state_5.sqlite")).unwrap(),
            b"native metadata"
        );
        fs::remove_dir_all(home).unwrap();
        assert!(vault.join("archived_sessions/old.jsonl").exists());
    }
}
