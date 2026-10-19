//! Account-independent compressed conversation archives and explicit public jobs.
//! Native deletion is used only after a verified, recoverable export exists.
use super::*;
use crate::catalog::Session;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Archive,
    Restore,
    Delete,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Archive => "archive_session",
            Self::Restore => "restore_session",
            Self::Delete => "delete_session",
        }
    }
}
#[derive(clap::Args)]
pub struct Target {
    #[arg(long)]
    pub session: String,
    #[arg(long)]
    pub expected_generation: String,
    #[arg(long)]
    pub request_id: String,
    #[arg(long)]
    pub confirm: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Operation {
    id: String,
    kind: Kind,
    session: Session,
    expected_generation: String,
    phase: String,
    detail: String,
    created_at: i64,
    updated_at: i64,
    source: Option<Instance>,
    #[serde(default)]
    worker_pid: Option<u32>,
    #[serde(default)]
    worker_start: Option<u64>,
}
impl Operation {
    fn public(&self) -> Value {
        json!({"id":self.id,"kind":self.kind.label(),"session_id":self.session.id,"phase":self.phase,"detail":self.detail,"created_at":self.created_at,"updated_at":self.updated_at,"can_cancel":false,"can_recover":false,"can_respond":false})
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Blob {
    home: Option<PathBuf>,
    original: PathBuf,
    relative: Option<PathBuf>,
    blob: String,
    bytes: u64,
    digest: String,
    compressed_digest: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Archive {
    version: u32,
    session: Session,
    source: Instance,
    state: String,
    archived_at: i64,
    bundle: String,
    blobs: Vec<Blob>,
    thread_ids: std::collections::BTreeSet<String>,
    facts: Value,
}
fn root(paths: &Paths) -> Result<PathBuf> {
    let p = paths.root.join("archived-sessions");
    private_dir(&p)?;
    Ok(p)
}
fn slot(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id)?;
    let p = root(paths)?.join(id);
    private_dir(&p)?;
    Ok(p)
}
fn manifest(paths: &Paths, id: &str) -> Result<PathBuf> {
    Ok(slot(paths, id)?.join("manifest.json"))
}
fn load(paths: &Paths, id: &str) -> Result<Archive> {
    let a: Archive = serde_json::from_slice(&fs::read(manifest(paths, id)?)?)?;
    ensure!(a.version == 1 && a.session.id == id, "INVALID_ARCHIVE");
    uuid::Uuid::parse_str(&a.bundle)?;
    Ok(a)
}
fn save(paths: &Paths, a: &Archive) -> Result<()> {
    atomic_write(&manifest(paths, &a.session.id)?, &serde_json::to_vec(a)?)
}
fn ops_root(paths: &Paths) -> Result<PathBuf> {
    let p = paths.root.join("conversation-operations");
    private_dir(&p)?;
    Ok(p)
}
fn op_path(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id)?;
    Ok(ops_root(paths)?.join(format!("{id}.json")))
}
fn op_load(paths: &Paths, id: &str) -> Result<Operation> {
    Ok(serde_json::from_slice(&fs::read(op_path(paths, id)?)?)?)
}
fn op_save(paths: &Paths, o: &Operation) -> Result<()> {
    atomic_write(&op_path(paths, &o.id)?, &serde_json::to_vec(o)?)
}
fn phase(paths: &Paths, o: &mut Operation, p: &str, text: &str) -> Result<()> {
    o.phase = p.into();
    o.detail = text.into();
    o.updated_at = daemon::now();
    op_save(paths, o)
}
fn optional(paths: &Paths, id: &str) -> Result<Option<Archive>> {
    let p = manifest(paths, id)?;
    if p.exists() {
        Ok(Some(load(paths, id)?))
    } else {
        Ok(None)
    }
}
fn effective(o: &Operation) -> Value {
    let mut v = o.public();
    if !matches!(o.phase.as_str(), "complete" | "failed")
        && daemon::now() - o.updated_at > 10
        && o.worker_pid.zip(o.worker_start).is_none_or(|(pid, ticks)| {
            process_info(pid).map_or(true, |p| {
                p.start_time != ticks || matches!(p.state, 'Z' | 'X')
            })
        })
    {
        v["phase"] = json!("failed");
        v["detail"] = json!(
            "Worker exited without a final receipt. Do not replay automatically; inspect original history and any prepared archive."
        );
    }
    v
}
pub fn operations(paths: &Paths) -> Result<Vec<Value>> {
    let mut out = vec![];
    for e in fs::read_dir(ops_root(paths)?)? {
        let e = e?;
        if e.file_type()?.is_file() && e.path().extension().is_some_and(|x| x == "json") {
            let o: Operation = serde_json::from_slice(&fs::read(e.path())?)?;
            out.push(effective(&o));
        }
    }
    Ok(out)
}
/// A confirmed job retains discovery provenance even without an enrolled account
/// or the initiating viewer's CODEX_HOME in the background worker environment.
pub(super) fn source_homes(paths: &Paths) -> Result<Vec<PathBuf>> {
    let mut homes = Vec::new();
    for e in fs::read_dir(ops_root(paths)?)? {
        let e = e?;
        if e.file_type()?.is_file() && e.path().extension().is_some_and(|x| x == "json") {
            let o: Operation = serde_json::from_slice(&fs::read(e.path())?)?;
            if let Some(source) = o.source {
                homes.push(source.home);
            }
        }
    }
    Ok(homes)
}
fn all(paths: &Paths) -> Result<Vec<Archive>> {
    let mut out = vec![];
    for e in fs::read_dir(root(paths)?)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            let Some(id) = e.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if uuid::Uuid::parse_str(&id).is_ok() && e.path().join("manifest.json").is_file() {
                out.push(load(paths, &id)?);
            }
        }
    }
    Ok(out)
}
pub fn hidden(paths: &Paths) -> Result<std::collections::BTreeSet<String>> {
    Ok(all(paths)?
        .into_iter()
        .filter(|a| a.state != "restored")
        .map(|a| a.session.id)
        .collect())
}
pub fn restored_homes(paths: &Paths) -> Result<Vec<(String, PathBuf)>> {
    let storage = root(paths)?;
    Ok(all(paths)?
        .into_iter()
        .filter(|a| {
            matches!(a.state.as_str(), "restored" | "prepared")
                && storage.join(&a.session.id).join("home").is_dir()
        })
        .map(|a| {
            (
                a.session.account,
                storage.clone().join(a.session.id).join("home"),
            )
        })
        .collect())
}
pub(super) fn file_only_home(paths: &Paths, home: &Path) -> Result<bool> {
    let storage = root(paths)?;
    if !all(paths)?
        .iter()
        .any(|a| a.state != "deleted" && storage.join(&a.session.id).join("home") == home)
    {
        return Ok(false);
    }
    if crate::control::available(home) {
        return Ok(false);
    }
    ensure!(!home_has_processes(home, false)?, "HISTORY_HOME_IN_USE");
    ensure!(
        !fs::read_dir(home)?.filter_map(Result::ok).any(|e| e
            .file_name()
            .to_string_lossy()
            .starts_with("state")
            && e.path().extension().is_some_and(|e| e == "sqlite")),
        "HISTORY_SERVER_REQUIRED"
    );
    Ok(true)
}
pub fn list(paths: &Paths) -> Result<Value> {
    let catalog = crate::catalog::list(paths)?;
    let storage = root(paths)?;
    Ok(json!(all(paths)?.into_iter().filter(|a|a.state!="restored"&&a.state!="deleted").map(|a| {
        let session=catalog.iter().find(|s|s.id==a.session.id).unwrap_or(&a.session);
        let compressed_bytes = a.blobs.iter().try_fold(0_u64, |sum, b| {
            let metadata = fs::symlink_metadata(storage.join(&session.id).join("bundles").join(&a.bundle).join(&b.blob)).ok()?;
            (metadata.is_file() && !metadata.file_type().is_symlink()).then_some(())?;
            sum.checked_add(metadata.len())
        });
        json!({"id":session.id,"name":session.name,"provider":session.provider,"conversation_id":session.conversation_id,"account_id":format!("codex:{}",session.account),"generation":session.generation(),"availability":"archived","state":a.state,"archived_at":a.archived_at,"conversation_created_at":a.facts["conversation_created_at"],"history_file_bytes":a.blobs.iter().map(|b|b.bytes).sum::<u64>(),"compressed_bytes":compressed_bytes,"capabilities":{"restore":true,"delete":true}})
    }).collect::<Vec<_>>()))
}
pub async fn start(paths: &Paths, kind: Kind, t: Target) -> Result<Value> {
    ensure!(t.confirm == t.session, "CONFIRMATION_REQUIRED");
    op_path(paths, &t.request_id)?;
    let admission = crate::paths::lock_file(&ops_root(paths)?.join("admission.lock"))?;
    admission.lock_exclusive()?;
    if op_path(paths, &t.request_id)?.exists() {
        let o = op_load(paths, &t.request_id)?;
        ensure!(
            o.session.id == t.session
                && o.kind == kind
                && o.expected_generation == t.expected_generation,
            "REQUEST_ID_CONFLICT"
        );
        return Ok(o.public());
    }
    let archived =
        optional(paths, &t.session)?.is_some_and(|a| a.state != "restored" && a.state != "deleted");
    let (session, source) = if archived {
        ensure!(kind != Kind::Archive, "SESSION_ALREADY_ARCHIVED");
        (
            crate::catalog::resolve(paths, &t.session, &t.expected_generation)?,
            None,
        )
    } else {
        ensure!(kind != Kind::Restore, "SESSION_NOT_ARCHIVED");
        let (s, i) = crate::api::selected(paths, &t.session, &t.expected_generation).await?;
        ensure!(i.inactive && i.move_id.is_none(), "SESSION_ACTIVE");
        (s, Some(i))
    };
    ensure!(session.provider == "codex", "PROVIDER_UNSUPPORTED");
    let _admission = crate::lifecycle::admit(paths, &session.conversation_id, Some(&t.request_id))?;
    crate::lifecycle::ensure_no_handoff(paths, &session.conversation_id, Some(&t.request_id))?;
    ensure!(
        !operations(paths)?
            .iter()
            .any(|o| o["session_id"] == t.session
                && !matches!(o["phase"].as_str(), Some("complete" | "failed"))),
        "OPERATION_BUSY"
    );
    let o = Operation {
        id: t.request_id,
        kind,
        session,
        source,
        expected_generation: t.expected_generation,
        phase: "queued".into(),
        detail: "Confirmed; waiting for conversation checks.".into(),
        created_at: daemon::now(),
        updated_at: daemon::now(),
        worker_pid: None,
        worker_start: None,
    };
    op_save(paths, &o)?;
    let spawned = crate::background::detach(&mut Command::new(crate::self_executable::path()?))
        .arg("--data-dir")
        .arg(&paths.root)
        .args(["conversation-worker", &o.id])
        .env_remove("CODEX_HOME")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        let mut failed = o.clone();
        phase(
            paths,
            &mut failed,
            "failed",
            &format!("Worker did not start: {e}"),
        )?;
        return Err(e.into());
    }
    Ok(o.public())
}
fn digest(p: &Path) -> Result<String> {
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p)?;
    ensure!(f.metadata()?.is_file(), "ARCHIVE_NOT_REGULAR");
    let mut hash = Sha256::new();
    let mut buf = [0; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn gzip(input: &Path, output: &Path, decode: bool) -> Result<()> {
    ensure!(
        !fs::symlink_metadata(input)?.file_type().is_symlink(),
        "ARCHIVE_SYMLINK"
    );
    let input = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(input)?;
    let out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(output)?;
    let mut cmd = Command::new("gzip");
    cmd.args(if decode {
        vec!["-d", "-c"]
    } else {
        vec!["-n", "-c"]
    });
    let result = cmd
        .stdin(input)
        .stdout(out.try_clone()?)
        .stderr(Stdio::piped())
        .output()?;
    ensure!(
        result.status.success(),
        "GZIP_FAILED: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    out.sync_all()?;
    Ok(())
}
fn safe_relative(p: &Path) -> bool {
    !p.as_os_str().is_empty()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        && (p.starts_with("sessions") || p.starts_with("archived_sessions"))
}
fn bundle(paths: &Paths, a: &Archive) -> Result<PathBuf> {
    let base = slot(paths, &a.session.id)?.join("bundles");
    private_dir(&base)?;
    let dir = base.join(&a.bundle);
    private_dir(&dir)?;
    Ok(dir)
}
async fn archive(paths: &Paths, o: &mut Operation) -> Result<()> {
    let source = o.source.clone().context("MISSING_SOURCE")?;
    let _account = crate::accounts::operation_lock(paths, &source.account)?;
    crate::catalog::resolve(paths, &o.session.id, &o.expected_generation)?;
    phase(
        paths,
        o,
        "checking_history",
        "Checking inactive copies and preserving full history proofs.",
    )?;
    let plan = deletion::preview(paths, &source.key()).await?;
    ensure!(!plan.copies.is_empty(), "HISTORY_NOT_FOUND");
    let primary = plan
        .copies
        .iter()
        .filter(|c| c.home.as_ref() == Some(&source.home))
        .map(|c| c.path.clone())
        .collect::<Vec<_>>();
    ensure!(primary.contains(&source.rollout), "SOURCE_HISTORY_CHANGED");
    let mut a = Archive {
        version: 1,
        session: o.session.clone(),
        source: source.clone(),
        state: "preparing".into(),
        archived_at: daemon::now(),
        bundle: o.id.clone(),
        blobs: vec![],
        thread_ids: plan.thread_ids.clone(),
        facts: history_file_details(&source.rollout),
    };
    ensure!(
        optional(paths, &o.session.id)?.is_none_or(|a| a.state == "restored"),
        "ARCHIVE_NEEDS_REVIEW"
    );
    let dir = bundle(paths, &a)?;
    let home = dir.join("home");
    private_dir(&home)?;
    history::preserve_selected_history(&source.home, &home, &primary).await?;
    phase(
        paths,
        o,
        "compressing_history",
        "Compressing verified history into sessionsation-managed Archived Sessions.",
    )?;
    for (index, copy) in plan.copies.iter().enumerate() {
        ensure!(
            digest(&copy.path)? == copy.digest && !rollout_still_open(&copy.path)?,
            "HISTORY_CHANGED"
        );
        let name = format!("{index}.jsonl.gz");
        let dest = dir.join(&name);
        gzip(&copy.path, &dest, false)?;
        ensure!(digest(&copy.path)? == copy.digest, "HISTORY_CHANGED");
        let relative = copy
            .home
            .as_ref()
            .and_then(|h| copy.path.strip_prefix(h).ok())
            .map(Path::to_path_buf);
        ensure!(
            relative.as_ref().is_none_or(|p| safe_relative(p)),
            "INVALID_HISTORY_PATH"
        );
        a.blobs.push(Blob {
            home: copy.home.clone(),
            original: copy.path.clone(),
            relative,
            blob: name,
            bytes: fs::metadata(&copy.path)?.len(),
            digest: copy.digest.clone(),
            compressed_digest: digest(&dest)?,
        });
    }
    a.state = "prepared".into();
    save(paths, &a)?;
    phase(
        paths,
        o,
        "moving_history",
        "Verified compressed archive saved; clearing original native history without touching project files.",
    )?;
    crate::catalog::resolve(paths, &o.session.id, &o.expected_generation)?;
    deletion::delete(paths, &source.key(), &source.thread_id, &plan.revision).await?;
    a.state = "archived".into();
    a.session = crate::catalog::invalidate(paths, &o.session.id, &o.expected_generation)?;
    save(paths, &a)?;
    phase(
        paths,
        o,
        "complete",
        "Conversation compressed and moved to Archived Sessions. Restore preserves its identity and history.",
    )
}
async fn restore_quiescent(paths: &Paths, a: &Archive) -> Result<()> {
    deletion::inactive(paths, &a.source, &a.thread_ids).await?;
    let mut homes: std::collections::BTreeSet<_> =
        a.blobs.iter().filter_map(|b| b.home.clone()).collect();
    homes.insert(a.source.home.clone());
    homes.insert(slot(paths, &a.session.id)?.join("home"));
    for home in homes {
        if crate::control::available(&home) {
            let mut control = crate::control::Control::connect(&home).await?;
            ensure!(
                !control
                    .loaded()
                    .await?
                    .iter()
                    .any(|t| t["id"].as_str().is_some_and(|id| a.thread_ids.contains(id))),
                "HISTORY_THREAD_LOADED"
            );
        } else {
            ensure!(!home_has_processes(&home, false)?, "HISTORY_HOME_IN_USE");
        }
    }
    Ok(())
}
async fn restore(paths: &Paths, o: &mut Operation) -> Result<()> {
    let mut a = load(paths, &o.session.id)?;
    ensure!(
        a.state != "deleted" && a.state != "restored",
        "SESSION_NOT_ARCHIVED"
    );
    let _locks = a
        .thread_ids
        .iter()
        .map(|id| catalog::operation_lock(paths, id))
        .collect::<Result<Vec<_>>>()?;
    crate::catalog::resolve(paths, &o.session.id, &o.expected_generation)?;
    restore_quiescent(paths, &a).await?;
    for b in &a.blobs {
        if b.original.exists() {
            ensure!(
                digest(&b.original)? == b.digest && !rollout_still_open(&b.original)?,
                "RESTORE_CONFLICT"
            );
        }
    }
    phase(
        paths,
        o,
        "restoring_history",
        "Verifying compressed data and restoring history into sessionsation-managed storage.",
    )?;
    let home = slot(paths, &o.session.id)?.join("home");
    private_dir(&home)?;
    let dir = bundle(paths, &a)?;
    for b in a
        .blobs
        .iter()
        .filter(|b| b.home.as_ref() == Some(&a.source.home))
    {
        ensure!(
            Path::new(&b.blob).components().count() == 1,
            "INVALID_ARCHIVE_BLOB"
        );
        let relative = b
            .relative
            .as_ref()
            .filter(|p| safe_relative(p))
            .context("INVALID_HISTORY_PATH")?;
        ensure!(
            digest(&dir.join(&b.blob))? == b.compressed_digest,
            "ARCHIVE_CHANGED"
        );
        let dest = home.join(relative);
        let parent = dest.parent().context("INVALID_HISTORY_PATH")?;
        let mut cursor = home.clone();
        for component in relative.parent().unwrap().components() {
            cursor.push(component);
            private_dir(&cursor)?;
        }
        private_dir(parent)?;
        if dest.exists() {
            ensure!(digest(&dest)? == b.digest, "RESTORE_CONFLICT");
            continue;
        }
        let temp = dest.with_extension(format!("restore-{}", o.id));
        gzip(&dir.join(&b.blob), &temp, true)?;
        ensure!(
            fs::metadata(&temp)?.len() == b.bytes && digest(&temp)? == b.digest,
            "ARCHIVE_CORRUPT"
        );
        fs::rename(&temp, &dest)?;
    }
    atomic_write(
        &home.parent().unwrap().join("history-checks.json"),
        &fs::read(dir.join("history-checks.json"))?,
    )?;
    history::verify_preserved_history(&home)?;
    restore_quiescent(paths, &a).await?;
    crate::catalog::resolve(paths, &o.session.id, &o.expected_generation)?;
    for thread in &a.thread_ids {
        catalog::restore_archived(paths, thread, &home)?;
    }
    a.state = "restored".into();
    a.session = crate::catalog::invalidate(paths, &o.session.id, &o.expected_generation)?;
    save(paths, &a)?;
    phase(
        paths,
        o,
        "complete",
        "Conversation restored as saved history. Start Session chooses an account and terminal; Restore never starts an agent.",
    )
}
async fn delete(paths: &Paths, o: &mut Operation) -> Result<()> {
    let prior = optional(paths, &o.session.id)?;
    let source = o
        .source
        .clone()
        .or_else(|| prior.as_ref().map(|a| a.source.clone()))
        .context("MISSING_SOURCE")?;
    let _account = crate::accounts::operation_lock(paths, &source.account)?;
    crate::catalog::resolve(paths, &o.session.id, &o.expected_generation)?;
    phase(
        paths,
        o,
        "deleting_history",
        "Deleting this conversation's local native history and archive copies; project files and credentials are unchanged.",
    )?;
    let plan = deletion::preview(paths, &source.key()).await?;
    crate::catalog::resolve(paths, &o.session.id, &o.expected_generation)?;
    deletion::delete(paths, &source.key(), &source.thread_id, &plan.revision).await?;
    let session = crate::catalog::invalidate(paths, &o.session.id, &o.expected_generation)?;
    let dir = slot(paths, &session.id)?;
    // The directory belongs solely to this backend session. Refuse symlinks.
    fn validate(p: &Path) -> Result<()> {
        let m = fs::symlink_metadata(p)?;
        ensure!(!m.file_type().is_symlink(), "ARCHIVE_SYMLINK");
        if m.is_dir() {
            for e in fs::read_dir(p)? {
                validate(&e?.path())?;
            }
        }
        Ok(())
    }
    validate(&dir)?;
    fs::remove_dir_all(&dir)?;
    private_dir(&dir)?;
    save(
        paths,
        &Archive {
            version: 1,
            session,
            source,
            state: "deleted".into(),
            archived_at: daemon::now(),
            bundle: o.id.clone(),
            blobs: vec![],
            thread_ids: plan.thread_ids,
            facts: Value::Null,
        },
    )?;
    phase(
        paths,
        o,
        "complete",
        "Conversation and its local saved/archive history deleted. Project files, account credentials and upstream accounts are unchanged.",
    )
}
pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    let lock = crate::paths::lock_file(&op_path(paths, id)?.with_extension("lock"))?;
    lock.try_lock_exclusive()?;
    let mut o = op_load(paths, id)?;
    if o.phase != "queued" {
        return Ok(());
    } // Never automatically replay an uncertain native mutation.
    o.worker_pid = Some(std::process::id());
    o.worker_start = Some(process_info(std::process::id())?.start_time);
    op_save(paths, &o)?;
    let result = match o.kind {
        Kind::Archive => archive(paths, &mut o).await,
        Kind::Restore => restore(paths, &mut o).await,
        Kind::Delete => delete(paths, &mut o).await,
    };
    if let Err(e) = result {
        phase(
            paths,
            &mut o,
            "failed",
            &format!(
                "{e:#}. No automatic retry; original history or the prepared archive is retained for review."
            ),
        )?;
    }
    Ok(())
}
