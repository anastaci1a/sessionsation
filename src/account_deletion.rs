//! Durable CLI-owned local account deletion. Admission never scans account files.
use crate::background::SpawnReaped as _;
use crate::{
    accounts, catalog, daemon, external,
    paths::{Paths, atomic_write, lock_file, private_dir, validate_name},
    provider::{
        ProviderId, native,
        ops::{self, ArchiveLayout},
    },
    record::Journal,
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
    /// Claude adopted config directory: never modified or deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_home: Option<PathBuf>,
    /// `deleted` (managed home removed) or `left_in_place` (adopted directory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_disposition: Option<String>,
}
impl Journal for Record {
    const DIR: &'static str = crate::layout::ACCOUNT_DELETIONS;
    const NOT_FOUND: &'static str = "ACCOUNT_DELETION_NOT_FOUND";
    const UNREADABLE: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn key(&self) -> (i64, &str) {
        (self.created_at.unwrap_or_default(), &self.id)
    }
    fn touch(&mut self, now: i64) {
        self.updated_at = now;
    }
    fn check_id(id: &str) -> Result<()> {
        uuid::Uuid::parse_str(id).context("INVALID_REQUEST_ID")?;
        Ok(())
    }
}
/// One admission (start or recover) at a time.
fn admission(paths: &Paths) -> Result<fs::File> {
    crate::record::lock_exclusive(&Record::dir(paths).join("admission.lock"))
}
pub fn status(paths: &Paths, id: &str) -> Result<Record> {
    let mut r = Record::read(paths, id)?;
    let stopped = Record::worker_stopped(paths, id)?;
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
pub fn list(paths: &Paths) -> Result<Vec<Record>> {
    let mut out = vec![];
    if Record::dir(paths).is_dir() {
        for e in fs::read_dir(Record::dir(paths))? {
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
pub async fn preview(paths: &Paths, provider: ProviderId, name: &str) -> Result<Value> {
    validate_name(name)?;
    let state = daemon::call(paths, "status", json!({})).await?;
    let account = state["accounts"]
        .as_array()
        .and_then(|aa| {
            aa.iter().find(|a| {
                a["name"] == name && a["provider"].as_str().unwrap_or("codex") == provider.as_str()
            })
        })
        .context("ACCOUNT_NOT_FOUND")?;
    let affected: Vec<_> = catalog::list(paths)?
        .into_iter()
        .filter(|s| s.provider == provider.as_str() && s.account == name)
        .map(|s| json!({"id":s.id,"name":s.name,"running":s.attachment.is_some()}))
        .collect();
    let capable = state["capabilities"]["account_deletion"] == true;
    let mut value = json!({"account_id":format!("{provider}:{name}"),"can_delete":capable&&!affected.iter().any(|s|s["running"]==true),"owner_upgrade_required":!capable,"affected_sessions":affected,"preserves_history":true,"deletes_local_credentials":true,"upstream_account_unchanged":true,"file_scanning":false});
    if native::is_native(provider) {
        let adopted = account["config_home"].as_str();
        value["provider"] = json!(provider);
        value["adopted_config_home"] = json!(adopted);
        value["home_disposition"] = json!(if adopted.is_some() {
            "left_in_place"
        } else {
            "deleted"
        });
        // An adopted directory keeps its credentials and history where they are.
        value["deletes_local_credentials"] = json!(adopted.is_none());
    }
    Ok(value)
}
fn spawn(paths: &Paths, r: &Record) -> Result<()> {
    let log = Record::worker_lock(paths, &r.id)?.with_extension("log");
    let mut command =
        crate::background::self_worker(paths, &log, &["account-delete-worker", &r.id])?;
    command.env_remove("CLAUDE_CONFIG_DIR");
    for provider in native::NATIVE {
        for name in native::stripped_env(provider) {
            command.env_remove(name);
        }
    }
    command.env_remove("GEMINI_CLI_HOME").spawn_reaped()?;
    Ok(())
}
/// Refuse a symlinked or foreign directory between the data root and a
/// managed home (`accounts/<platform>`, or a deferred home's legacy parent).
fn safe_parent(paths: &Paths, home: &Path) -> Result<()> {
    let parent = home.parent().context("UNSAFE_ACCOUNT_PARENT")?;
    // raw-path: fails closed; a home spelled differently from the root is refused
    ensure!(parent.starts_with(&paths.root), "UNSAFE_ACCOUNT_PARENT");
    // raw-path: walks the spelling checked by starts_with just above
    for dir in parent.ancestors().take_while(|d| *d != paths.root) {
        let m = fs::symlink_metadata(dir)?;
        ensure!(
            m.is_dir() && !m.file_type().is_symlink() && m.uid() == unsafe { libc::geteuid() },
            "UNSAFE_ACCOUNT_PARENT"
        );
    }
    Ok(())
}
fn account_home(paths: &Paths, name: &str) -> Result<PathBuf> {
    let home = paths.account(name)?;
    safe_parent(paths, &home)?;
    Ok(home)
}
/// The managed native-provider home, refusing a symlinked or foreign parent.
fn native_managed_home(paths: &Paths, provider: ProviderId, name: &str) -> Result<PathBuf> {
    let home = native::default_home(paths, provider, name)?;
    safe_parent(paths, &home)?;
    Ok(home)
}
pub async fn start(
    paths: &Paths,
    provider: ProviderId,
    name: &str,
    id: &str,
    confirm: &str,
) -> Result<Value> {
    validate_name(name)?;
    ensure!(
        confirm == format!("{provider}:{name}"),
        "CONFIRMATION_REQUIRED"
    );
    Record::check_id(id)?;
    let _admission = admission(paths)?;
    if Record::exists(paths, id) {
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
    let account = owner["accounts"]
        .as_array()
        .and_then(|aa| {
            aa.iter().find(|a| {
                a["name"] == name && a["provider"].as_str().unwrap_or("codex") == provider.as_str()
            })
        })
        .context("ACCOUNT_NOT_FOUND")?;
    let facts = ops::facts(provider);
    let external_home = if facts.adopts_config_dir {
        account["config_home"].as_str().map(PathBuf::from)
    } else {
        None
    };
    let source_inode = if facts.supervised {
        let home = fs::symlink_metadata(account_home(paths, name)?)?;
        ensure!(
            home.is_dir() && !home.file_type().is_symlink(),
            "UNSAFE_ACCOUNT_DATA"
        );
        Some(home.ino())
    } else if external_home.is_none() {
        let home = fs::symlink_metadata(native_managed_home(paths, provider, name)?)?;
        ensure!(
            home.is_dir() && !home.file_type().is_symlink(),
            "UNSAFE_ACCOUNT_DATA"
        );
        Some(home.ino())
    } else {
        // The adopted directory is never modified, so its identity is not a precondition.
        None
    };
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
            .join(crate::layout::PRESERVED_CONVERSATIONS)
            .join(id)
            .join("home"),
        source_inode,
        can_cancel: false,
        can_recover: false,
        history_preserved: false,
        home_disposition: None,
        external_home,
    };
    r.save(paths)?;
    if let Err(e) = spawn(paths, &r) {
        r.phase = "failed".into();
        r.detail = format!("Worker did not start: {e}");
        r.save(paths)?;
    }
    Ok(json!(r))
}
pub fn recover(paths: &Paths, id: &str) -> Result<Value> {
    let _admission = admission(paths)?;
    let worker = lock_file(&Record::worker_lock(paths, id)?)?;
    worker
        .try_lock_exclusive()
        .context("ACCOUNT_DELETION_WORKER_RUNNING")?;
    let mut r = status(paths, id)?;
    ensure!(r.phase != "complete", "ACCOUNT_DELETION_NOT_RECOVERABLE");
    r.phase = "admitted".into();
    r.detail = "Explicit recovery requested; rechecking ownership before continuing.".into();
    r.save(paths)?;
    drop(worker);
    if let Err(e) = spawn(paths, &r) {
        r.phase = "failed".into();
        r.detail = format!("Recovery worker did not start: {e}");
        r.save(paths)?;
    }
    Ok(json!(r))
}
fn phase(paths: &Paths, r: &mut Record, p: &str, detail: &str) -> Result<()> {
    r.phase = p.into();
    r.detail = detail.into();
    r.save(paths)
}
pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    let guard = lock_file(&Record::worker_lock(paths, id)?)?;
    guard.try_lock_exclusive()?;
    let mut r = status(paths, id)?;
    ensure!(r.phase == "admitted", "ACCOUNT_DELETION_ALREADY_DISPATCHED");
    if let Err(e) = perform(paths, &mut r).await {
        r.phase = "failed".into();
        r.detail = format!(
            "{e:#}. Original or preserved conversation data is retained. Inspect this request; it was not automatically retried."
        );
        r.save(paths)?;
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
            .any(|s| crate::layout::same_home(&s.home, home)),
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
        !external::jobs(paths)?
            .iter()
            .any(|j| (crate::layout::same_home(&j.source.home, home)
                || paths
                    .account(&j.target)
                    .ok()
                    .is_some_and(|t| crate::layout::same_home(&t, home)))
                && j.pending()),
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
/// Move the entries of `source` that `keep` accepts into `target` (inside `vault`; every
/// directory down to it private), never through a symlink and never over an existing entry.
/// A missing `source` is an error only when `required`.
fn move_history(
    vault: &Path,
    source: &Path,
    target: &Path,
    keep: fn(&str) -> bool,
    required: bool,
) -> Result<()> {
    private_dir(vault)?;
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(_) if !required => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for e in entries {
        let e = e?;
        let name = e.file_name();
        if !keep(name.to_str().context("INVALID_HISTORY_NAME")?) {
            continue;
        }
        no_symlinks(&e.path())?;
        let mut dirs: Vec<&Path> = target.ancestors().take_while(|d| *d != vault).collect();
        dirs.reverse();
        dirs.into_iter().try_for_each(private_dir)?;
        let dest = target.join(&name);
        ensure!(!dest.exists(), "PRESERVED_HISTORY_COLLISION");
        fs::rename(e.path(), &dest).context("HISTORY_MOVE_FAILED_SOURCE_RETAINED")?;
        fs::File::open(source)?.sync_all()?;
        fs::File::open(target)?.sync_all()?;
    }
    Ok(())
}
async fn perform(paths: &Paths, r: &mut Record) -> Result<()> {
    for provider in native::NATIVE {
        if r.account_id.starts_with(&format!("{provider}:")) {
            return perform_native(paths, r, provider).await;
        }
    }
    perform_codex(paths, r).await
}
async fn perform_codex(paths: &Paths, r: &mut Record) -> Result<()> {
    ensure!(
        r.history_home
            == paths
                .root
                .join(crate::layout::PRESERVED_CONVERSATIONS)
                .join(&r.id)
                .join("home"),
        "INVALID_HISTORY_VAULT"
    );
    let name = ops::supervised_name(&r.account_id)
        .context(ops::UNSUPPORTED)?
        .to_owned();
    let _account = accounts::operation_lock(paths, ProviderId::Codex, &name)?;
    let jobs = paths.root.join(crate::layout::HANDOFF_JOBS);
    private_dir(&jobs)?;
    let queue = lock_file(&jobs.join("queue.lock"))?;
    crate::paths::try_lock_brief(&queue).context("OPERATION_BUSY")?;
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
        r.save(paths)?;
    }
    phase(
        paths,
        r,
        "checking_active",
        "Checking running sessions, loaded conversations and pending operations.",
    )?;
    idle(paths, &home).await?;
    let lease = lock_file(&home.join(".switcher-lease"))?;
    crate::paths::try_lock_brief(&lease).context("ACCOUNT_BUSY")?;
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
    private_dir(&paths.root.join(crate::layout::PRESERVED_CONVERSATIONS))?;
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
    move_history(&r.history_home, &home, &r.history_home, history_entry, true)?;
    external::verify_preserved_history(&r.history_home)?;
    r.history_preserved = true;
    r.save(paths)?;
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
    fs::File::open(home.parent().context("UNSAFE_ACCOUNT_PARENT")?)?.sync_all()?;
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
/// Claude conversation history inside a managed config home. Credentials
/// (`.credentials.json`), settings and the live registry (`sessions/`) stay behind.
fn claude_history_entry(name: &str) -> bool {
    matches!(
        name,
        "projects" | "history.jsonl" | "file-history" | "todos"
    )
}
fn preserve_claude(home: &Path, vault: &Path) -> Result<()> {
    private_dir(vault)?;
    // Shared-store links are removed (the link only, never what it points to):
    // the conversations stay in the shared store and remain visible to every
    // other account. Any other symlink still fails `no_symlinks` below.
    crate::shared_state::detach_links(home)?;
    move_history(vault, home, vault, claude_history_entry, true)
}
/// Gemini conversation history inside a managed home: chat files under
/// `.gemini/tmp`, prompt history under `.gemini/history` and the project
/// registry that maps working directories to them. Credentials, account
/// records, settings and trust files (`oauth_creds.json`, `google_accounts.json`,
/// `settings.json`, `trustedFolders.json`) stay behind and are deleted with the home.
fn gemini_history_entry(name: &str) -> bool {
    matches!(name, "tmp" | "history" | "projects.json")
}
/// Antigravity conversation history inside a managed home: the per-conversation
/// databases, transcripts and artifacts (`conversations`, `brain`, `annotations`,
/// `implicit`), the conversation index and prompt history, all under
/// `.gemini/antigravity-cli`. The OAuth token, settings, logs and caches stay
/// behind and are deleted with the home.
fn antigravity_history_entry(name: &str) -> bool {
    matches!(
        name,
        "conversations"
            | "brain"
            | "annotations"
            | "implicit"
            | "conversation_summaries.db"
            | "conversation_summaries.db-wal"
            | "conversation_summaries.db-shm"
            | "history.jsonl"
    )
}
fn preserve_native(provider: ProviderId, home: &Path, vault: &Path) -> Result<()> {
    match ops::facts(provider).archive {
        ArchiveLayout::GeminiChats => move_history(
            vault,
            &home.join(".gemini"),
            &vault.join(".gemini"),
            gemini_history_entry,
            false,
        ),
        ArchiveLayout::AgyIndex => {
            let source = crate::provider::antigravity::state_dir(home);
            let target = vault.join(".gemini/antigravity-cli");
            move_history(vault, &source, &target, antigravity_history_entry, false)
        }
        ArchiveLayout::ClaudeProjects | ArchiveLayout::Rollouts => preserve_claude(home, vault),
    }
}
/// Refuse while a verified live session, a provider process using this home
/// (CLAUDE_CONFIG_DIR or GEMINI_CLI_HOME), or an unfinished headless delivery uses
/// the account.
pub(crate) fn native_idle(
    paths: &Paths,
    provider: ProviderId,
    name: &str,
    home: &Path,
) -> Result<()> {
    let (live, processes, operations) = accounts::native_activity(paths, provider, name, home)?;
    ensure!(live.is_empty(), "ACCOUNT_HAS_RUNNING_SESSIONS");
    ensure!(!processes, "ACCOUNT_HAS_RUNNING_PROCESSES");
    ensure!(operations.is_empty(), "ACCOUNT_HAS_PENDING_OPERATIONS");
    Ok(())
}
async fn perform_native(paths: &Paths, r: &mut Record, provider: ProviderId) -> Result<()> {
    let title = native::title(provider);
    ensure!(
        r.history_home
            == paths
                .root
                .join(crate::layout::PRESERVED_CONVERSATIONS)
                .join(&r.id)
                .join("home"),
        "INVALID_HISTORY_VAULT"
    );
    let name = r
        .account_id
        .strip_prefix(&format!("{provider}:"))
        .context(ops::UNSUPPORTED)?
        .to_owned();
    let _account = accounts::operation_lock(paths, provider, &name)?;
    let finish = |paths: &Paths, name: &str, id: &str| {
        let (paths, name, id) = (paths.clone(), name.to_owned(), id.to_owned());
        async move {
            daemon::call(
                &paths,
                "account/delete/finish",
                json!({"name":name,"provider":provider,"request_id":id}),
            )
            .await
        }
    };
    let enrolled = daemon::call(paths, "status", json!({})).await?["accounts"]
        .as_array()
        .is_some_and(|aa| {
            aa.iter()
                .any(|a| a["name"] == name.as_str() && a["provider"] == provider.as_str())
        });
    if let Some(external) = r.external_home.clone() {
        // Adopted config directory: only the account record is removed. The
        // directory, its credentials and its conversations are never touched.
        if enrolled {
            phase(
                paths,
                r,
                "checking_active",
                &format!("Checking live {title} sessions, processes and headless deliveries."),
            )?;
            native_idle(paths, provider, &name, &external)?;
            phase(
                paths,
                r,
                "disabling_account",
                "Disabling future launches through the existing account owner.",
            )?;
            daemon::call(
                paths,
                "account/delete/prepare",
                json!({"name":name,"provider":provider,"request_id":r.id}),
            )
            .await?;
            native_idle(paths, provider, &name, &external)?;
            r.home_disposition = Some("left_in_place".into());
            phase(
                paths,
                r,
                "deleting_local_data",
                "Removing only the account record; the adopted config directory is left in place.",
            )?;
            finish(paths, &name, &r.id).await?;
        }
        r.home_disposition = Some("left_in_place".into());
        let detail = format!(
            "Account record removed. The adopted config directory {} was left in place unchanged, including its credentials and conversations.",
            external.display()
        );
        return phase(paths, r, "complete", &detail);
    }
    let home = native_managed_home(paths, provider, &name)?;
    if !home.exists() {
        ensure!(
            r.source_inode.is_some() && r.history_home.is_dir() && r.history_preserved,
            "ACCOUNT_DATA_UNAVAILABLE"
        );
        r.home_disposition = Some("deleted".into());
        phase(
            paths,
            r,
            "deleting_local_data",
            "Completing the original account deletion after verified history preservation.",
        )?;
        if enrolled {
            finish(paths, &name, &r.id).await?;
        }
        return phase(
            paths,
            r,
            "complete",
            &format!("Local {title} home and credentials deleted; conversation history retained."),
        );
    }
    let m = fs::symlink_metadata(&home)?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink(),
        "UNSAFE_ACCOUNT_DATA"
    );
    ensure!(r.source_inode == Some(m.ino()), "ACCOUNT_HOME_REPLACED");
    phase(
        paths,
        r,
        "checking_active",
        &format!("Checking live {title} sessions, processes and headless deliveries."),
    )?;
    native_idle(paths, provider, &name, &home)?;
    phase(
        paths,
        r,
        "disabling_account",
        "Disabling future launches through the existing account owner.",
    )?;
    daemon::call(
        paths,
        "account/delete/prepare",
        json!({"name":name,"provider":provider,"request_id":r.id}),
    )
    .await?;
    native_idle(paths, provider, &name, &home)?;
    private_dir(&paths.root.join(crate::layout::PRESERVED_CONVERSATIONS))?;
    private_dir(r.history_home.parent().context("INVALID_HISTORY_VAULT")?)?;
    phase(
        paths,
        r,
        "preserving_history",
        &format!("Moving {title} transcripts and history to private preserved storage."),
    )?;
    let manifest = r
        .history_home
        .parent()
        .context("INVALID_HISTORY_VAULT")?
        .join("account.json");
    atomic_write(
        &manifest,
        &serde_json::to_vec(
            &json!({"version":1,"provider":provider,"account":name,"request_id":r.id,"home":r.history_home}),
        )?,
    )?;
    preserve_native(provider, &home, &r.history_home)?;
    r.history_preserved = true;
    r.save(paths)?;
    native_idle(paths, provider, &name, &home)?;
    ensure!(
        fs::symlink_metadata(&home)?.ino() == r.source_inode.unwrap_or_default(),
        "ACCOUNT_HOME_REPLACED"
    );
    r.home_disposition = Some("deleted".into());
    phase(
        paths,
        r,
        "deleting_local_data",
        &format!(
            "Deleting this {title} account's remaining local files and credentials; preserved conversations are outside this directory."
        ),
    )?;
    fs::remove_dir_all(&home).context("ACCOUNT_LOCAL_DATA_CLEANUP_FAILED")?;
    fs::File::open(home.parent().context("UNSAFE_ACCOUNT_PARENT")?)?.sync_all()?;
    finish(paths, &name, &r.id).await?;
    phase(
        paths,
        r,
        "complete",
        &format!(
            "Local {title} home and credentials deleted. Conversation history preserved; upstream account unchanged."
        ),
    )
}
/// Histories stay in discovery under their original account identity after the
/// registration and credential home have been deleted by the sole owner.
pub fn preserved_homes(paths: &Paths) -> Result<Vec<(String, PathBuf)>> {
    Ok(list(paths)?
        .into_iter()
        .filter(|r| r.history_preserved && r.history_home.is_dir())
        .filter_map(|r| {
            ops::supervised_name(&r.account_id)
                .map(|name| (name.to_owned(), r.history_home.clone()))
        })
        .collect())
}
#[cfg(test)]
mod tests;
