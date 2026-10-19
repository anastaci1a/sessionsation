//! Durable saved-session intent. Reuses the account launcher and transfer worker.
use crate::{
    api, catalog, daemon, external,
    launch::{self, Host, Space},
    paths::{Paths, atomic_write, lock_file, private_dir},
};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, io::Read, path::PathBuf, process::Stdio};
#[derive(Subcommand)]
pub enum Command {
    Validate,
    Start,
    Status {
        #[arg(long)]
        request_id: String,
    },
    List,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub request_id: String,
    pub session_id: String,
    pub expected_generation: String,
    pub account_id: String,
    pub name: String,
    pub host: Host,
    pub space: Space,
    /// Omitted preserves the saved location. Planned directories are created only after Start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<launch::Directory>,
    /// Explicit consent for an interrupted saved turn, never other risks.
    #[serde(default)]
    pub confirm_unfinished: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub version: u32,
    pub request: Request,
    pub directory: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_directory: Option<PathBuf>,
    #[serde(default)]
    pub created_directories: Vec<PathBuf>,
    pub previous_name: String,
    pub conversation_id: String,
    pub generation: String,
    pub phase: String,
    pub detail: Option<String>,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub pane_id: Option<String>,
    pub operation_id: Option<String>,
    /// Immutable admission time; absent for legacy receipts whose creation is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    pub updated_at: i64,
}
fn file(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_REQUEST_ID")?;
    Ok(paths.root.join("resumptions").join(format!("{id}.json")))
}
fn lock(paths: &Paths) -> Result<fs::File> {
    let dir = paths.root.join("resumptions");
    private_dir(&dir)?;
    let f = lock_file(&dir.join("admission.lock"))?;
    f.lock_exclusive()?;
    Ok(f)
}
fn read(paths: &Paths, id: &str) -> Result<Receipt> {
    let path = file(paths, id)?;
    ensure!(path.is_file(), "RESUME_NOT_FOUND");
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn save(paths: &Paths, r: &mut Receipt) -> Result<()> {
    r.updated_at = daemon::now();
    atomic_write(
        &file(paths, &r.request.request_id)?,
        &serde_json::to_vec_pretty(r)?,
    )
}
fn change(
    paths: &Paths,
    id: &str,
    edit: impl FnOnce(&mut Receipt) -> Result<()>,
) -> Result<Receipt> {
    let _lock = lock(paths)?;
    let mut r = read(paths, id)?;
    edit(&mut r)?;
    save(paths, &mut r)?;
    Ok(r)
}
fn records(paths: &Paths) -> Result<Vec<Receipt>> {
    let dir = paths.root.join("resumptions");
    let mut records = vec![];
    if dir.is_dir() {
        for e in fs::read_dir(dir)? {
            let e = e?;
            if e.file_type()?.is_file() && e.path().extension().is_some_and(|x| x == "json") {
                records.push(serde_json::from_slice::<Receipt>(&fs::read(e.path())?)?);
            }
        }
    }
    records.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    Ok(records)
}
fn shape(r: &Request) -> Result<()> {
    ensure!(r.version == 1, "UNSUPPORTED_RESUME_VERSION");
    uuid::Uuid::parse_str(&r.request_id).context("INVALID_REQUEST_ID")?;
    uuid::Uuid::parse_str(&r.session_id).context("INVALID_SESSION_ID")?;
    ensure!(
        !r.name.is_empty()
            && r.name.chars().count() <= 24
            && r.name.trim() == r.name
            && !r.name.chars().any(char::is_control),
        "INVALID_SESSION_NAME"
    );
    crate::paths::validate_name(
        r.account_id
            .strip_prefix("codex:")
            .context("UNSUPPORTED_PROVIDER_OPERATION")?,
    )?;
    crate::paths::validate_name(&r.host.session)?;
    ensure!(r.host.socket.is_absolute(), "INVALID_HOST_SOCKET");
    if let Some(d) = &r.directory {
        launch::directory_shape(d)?;
    }
    Ok(())
}
fn validate_saved_history(source: &external::Instance, confirmed: bool) -> Result<()> {
    ensure!(
        source.inactive && source.move_id.is_none(),
        "SESSION_STATE_UNVERIFIED"
    );
    if source.block_reason.as_deref() == Some("unfinished") {
        ensure!(confirmed, "UNFINISHED_HISTORY_CONFIRMATION_REQUIRED");
    } else {
        ensure!(source.block_reason.is_none(), "SESSION_STATE_UNVERIFIED");
    }
    Ok(())
}
async fn validate(paths: &Paths, r: &Request) -> Result<(catalog::Session, external::Instance)> {
    shape(r)?;
    let (s, source) = api::selected(paths, &r.session_id, &r.expected_generation).await?;
    crate::lifecycle::ensure_available(paths, &s.conversation_id, Some(&r.request_id))?;
    ensure!(
        source.inactive && s.attachment.is_none(),
        "SESSION_ALREADY_RUNNING"
    );
    if r.directory.is_none() && !std::path::Path::new(&source.cwd).is_dir() {
        anyhow::bail!("SAVED_DIRECTORY_UNAVAILABLE");
    }
    validate_resume_history(&source, r)?;
    if let Some(d) = &r.directory {
        launch::directory_check(d)?;
    } else {
        let cwd = PathBuf::from(&source.cwd);
        ensure!(
            cwd.is_absolute() && cwd.is_dir(),
            "SAVED_DIRECTORY_UNAVAILABLE"
        );
    }
    ensure!(
        !catalog::list(paths)?
            .iter()
            .any(|other| other.id != s.id && other.name == r.name),
        "SESSION_NAME_EXISTS"
    );
    let target = r.account_id.strip_prefix("codex:").unwrap();
    crate::accounts::ensure_available(paths, target)?;
    let state = daemon::call(paths, "status", json!({})).await?;
    ensure!(
        state["accounts"].as_array().is_some_and(|aa| aa
            .iter()
            .any(|a| a["name"] == target && a["enabled"] == true && a["auth_required"] == false)),
        "ACCOUNT_UNAVAILABLE"
    );
    let spaces = launch::host_call(&r.host, &["workspace", "list"]).await?;
    let spaces = spaces["workspaces"].as_array().context("HOST_PROTOCOL")?;
    match &r.space {
        Space::Existing { id } => ensure!(
            spaces.iter().any(|s| s["workspace_id"] == *id),
            "WORKSPACE_NOT_FOUND"
        ),
        Space::New { label } => {
            launch::validate_name(label)?;
            ensure!(
                !spaces.iter().any(|s| s["label"] == *label),
                "WORKSPACE_NAME_EXISTS"
            );
        }
    }
    Ok((s, source))
}
pub async fn api(paths: &Paths, command: Command) -> Result<Value> {
    match command {
        Command::Status { request_id } => return Ok(json!(status(paths, &request_id).await?)),
        Command::List => {
            let mut values = vec![];
            for r in records(paths)? {
                values.push(status(paths, &r.request.request_id).await?);
            }
            return Ok(json!(values));
        }
        _ => {}
    }
    let mut bytes = vec![];
    std::io::stdin().take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "REQUEST_TOO_LARGE");
    let r: Request = serde_json::from_slice(&bytes).context("INVALID_RESUME_REQUEST")?;
    shape(&r)?;
    if matches!(command, Command::Validate) {
        let (_, source) = validate(paths, &r).await?;
        return Ok(
            json!({"valid":true,"directory":r.directory.as_ref().map(|d|d.path.clone()).unwrap_or_else(||source.cwd.into()),"request_id":r.request_id}),
        );
    }
    let _lock = lock(paths)?;
    if file(paths, &r.request_id)?.exists() {
        let old = read(paths, &r.request_id)?;
        ensure!(old.request == r, "REQUEST_ID_CONFLICT");
        return Ok(json!(old));
    }
    ensure!(
        !records(paths)?
            .iter()
            .any(|prior| prior.request.session_id == r.session_id
                && !matches!(prior.phase.as_str(), "running" | "failed" | "recovered")),
        "RESUME_IN_PROGRESS"
    );
    let selected = catalog::resolve(paths, &r.session_id, &r.expected_generation)?;
    let _admission =
        crate::lifecycle::admit(paths, &selected.conversation_id, Some(&r.request_id))?;
    crate::lifecycle::ensure_no_handoff(paths, &selected.conversation_id, Some(&r.request_id))?;
    let (s, source) = validate(paths, &r).await?;
    let mut receipt = Receipt {
        version: 1,
        directory: r
            .directory
            .as_ref()
            .map(|d| d.path.clone())
            .unwrap_or_else(|| source.cwd.clone().into()),
        saved_directory: Some(source.cwd.into()),
        created_directories: vec![],
        request: r,
        previous_name: s.name.clone(),
        conversation_id: s.conversation_id.clone(),
        generation: s.generation(),
        phase: "admitted".into(),
        detail: None,
        workspace_id: None,
        tab_id: None,
        pane_id: None,
        operation_id: None,
        created_at: Some(daemon::now()),
        updated_at: daemon::now(),
    };
    save(paths, &mut receipt)?;
    let child = crate::background::detach(&mut std::process::Command::new(
        crate::self_executable::path()?,
    ))
    .arg("--data-dir")
    .arg(&paths.root)
    .args(["resume-worker", &receipt.request.request_id])
    .env("HERDR_SOCKET_PATH", &receipt.request.host.socket)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn();
    if child.is_err() {
        receipt.phase = "failed".into();
        receipt.detail = Some("Worker could not start; no resources created".into());
        save(paths, &mut receipt)?;
    }
    Ok(json!(receipt))
}
pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    let worker = lock_file(&file(paths, id)?.with_extension("worker.lock"))?;
    worker.try_lock_exclusive()?;
    let r = change(paths, id, |r| {
        ensure!(r.phase == "admitted", "RESUME_ALREADY_DISPATCHED");
        r.phase = "preparing".into();
        Ok(())
    })?;
    if let Err(e) = prepare(paths, &r).await {
        change(paths, id, |r| {
            r.phase = if r.phase == "preparing" {
                "failed"
            } else {
                "uncertain"
            }
            .into();
            r.detail = Some(format!("{e:#}"));
            Ok(())
        })?;
    }
    Ok(())
}
fn saved_directory(receipt: &Receipt) -> &std::path::Path {
    receipt
        .saved_directory
        .as_deref()
        .unwrap_or(&receipt.directory)
}
fn validate_resume_history(source: &external::Instance, r: &Request) -> Result<()> {
    let mut history = source.clone();
    if r.directory.is_some()
        && history.block_reason.as_deref() == Some("unknown")
        && !PathBuf::from(&source.cwd).is_dir()
    {
        ensure!(
            !external::rollout_still_open(&source.rollout)?,
            "SESSION_STATE_UNVERIFIED"
        );
        // Supersede only a missing old directory, never a history/ownership gate.
        history.block_reason = if external::complete_rollout(&source.rollout)? {
            None
        } else {
            Some("unfinished".into())
        };
    }
    validate_saved_history(&history, r.confirm_unfinished)
}
async fn prepare(paths: &Paths, receipt: &Receipt) -> Result<()> {
    let r = &receipt.request;
    let id = &r.request_id;
    let (_, source) = validate(paths, r).await?;
    ensure!(
        std::path::Path::new(&source.cwd) == saved_directory(receipt),
        "SAVED_DIRECTORY_CHANGED"
    );
    if let Some(d) = &r.directory {
        launch::create_directories(d, |p| {
            change(paths, id, |r| {
                r.created_directories.push(p.to_path_buf());
                Ok(())
            })?;
            Ok(())
        })?;
    }
    change(paths, id, |r| {
        r.phase = "creating_terminal".into();
        Ok(())
    })?;
    let created = launch::create_terminal(&r.host, &r.space, &receipt.directory, &r.name).await?;
    let pane = created["root_pane"]["pane_id"]
        .as_str()
        .context("HOST_PANE_ID_MISSING")?;
    let tab = created["tab"]["tab_id"]
        .as_str()
        .context("HOST_TAB_ID_MISSING")?;
    let space = created["tab"]["workspace_id"]
        .as_str()
        .or_else(|| created["workspace"]["workspace_id"].as_str())
        .context("HOST_WORKSPACE_ID_MISSING")?;
    if let Space::Existing { id } = &r.space {
        ensure!(id == space, "HOST_WRONG_WORKSPACE");
    }
    change(paths, id, |r| {
        r.pane_id = Some(pane.into());
        r.tab_id = Some(tab.into());
        r.workspace_id = Some(space.into());
        Ok(())
    })?;
    if matches!(r.space, Space::New { .. }) {
        launch::host_call(&r.host, &["tab", "rename", tab, &r.name]).await?;
    }
    let target = crate::terminal::herdr_target(&r.host.session, pane)?;
    let mut ready = false;
    for _ in 0..50 {
        if crate::terminal::state_at(&target, Some(&r.host.socket)).is_ok_and(|s| s.0) {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    ensure!(ready, "NEW_TERMINAL_NOT_READY");
    let (current, source) = api::selected(paths, &r.session_id, &r.expected_generation).await?;
    ensure!(
        source.inactive
            && current.attachment.is_none()
            && std::path::Path::new(&source.cwd) == saved_directory(receipt),
        "STALE_TARGET"
    );
    change(paths, id, |r| {
        r.phase = "renaming".into();
        Ok(())
    })?;
    let renamed = catalog::rename(paths, &r.session_id, &r.expected_generation, &r.name)?;
    change(paths, id, |r| {
        r.generation = renamed.generation();
        r.phase = "dispatching".into();
        Ok(())
    })?;
    let account = r.account_id.strip_prefix("codex:").unwrap();
    if account != source.account || paths.account(account)? != source.home {
        // Bind the transfer's durable job ID before enqueue; a lost acknowledgment
        // is reconciled by identity, never by starting another transfer.
        change(paths, id, |r| {
            r.operation_id = Some(id.into());
            r.phase = "transferring".into();
            Ok(())
        })?;
        external::queue_saved_resume(
            paths,
            source,
            account,
            external::ResumeDestination {
                pane: &target,
                directory: r.directory.as_ref().map(|d| d.path.as_path()),
            },
            (&r.session_id, &renamed.generation()),
            id,
            r.confirm_unfinished,
        )
        .await?;
    } else {
        let q = crate::terminal::shell_quote;
        let command = format!(
            "cd -- {} && SESSIONSATION_HERDR_SESSION={} SESSIONSATION_LAUNCHER_BYPASS=1 {} --data-dir {} resume-run {}",
            q(receipt.directory.to_str().context("INVALID_DIRECTORY")?),
            q(&r.host.session),
            q(std::env::current_exe()?
                .to_str()
                .context("INVALID_EXECUTABLE")?),
            q(paths.root.to_str().context("INVALID_DATA_ROOT")?),
            q(id)
        );
        launch::host_call(&r.host, &["pane", "run", pane, &command]).await?;
        for _ in 0..300 {
            let receipt = status(paths, id).await?;
            if receipt.phase == "running" {
                return Ok(());
            }
            ensure!(receipt.phase != "uncertain", "RESUME_ACK_UNCERTAIN");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        anyhow::bail!("RESUME_ACK_UNCERTAIN");
    }
    Ok(())
}
pub async fn run(paths: &Paths, id: &str) -> Result<()> {
    let r = change(paths, id, |r| {
        ensure!(r.phase == "dispatching", "RESUME_ALREADY_DISPATCHED");
        ensure!(
            std::env::var("HERDR_PANE_ID").ok().as_ref() == r.pane_id.as_ref(),
            "HOST_WRONG_PANE"
        );
        ensure!(
            std::env::var("SESSIONSATION_HERDR_SESSION").ok().as_deref()
                == Some(&r.request.host.session),
            "HOST_WRONG_SESSION"
        );
        r.phase = "starting_provider".into();
        Ok(())
    })?;
    let result = external::run_account_resume_requested(
        paths,
        r.request.account_id.strip_prefix("codex:").unwrap(),
        &r.conversation_id,
        (&r.request.session_id, &r.generation),
        id,
        r.request.directory.as_ref().map(|d| d.path.as_path()),
    )
    .await;
    if let Err(e) = result {
        change(paths, id, |r| {
            r.phase = "uncertain".into();
            r.detail = Some(format!("{e:#}"));
            Ok(())
        })?;
    }
    Ok(())
}
pub(crate) fn attached(paths: &Paths, id: &str, session: &catalog::Session) -> Result<()> {
    change(paths, id, |r| {
        ensure!(
            session.id == r.request.session_id
                && session.conversation_id == r.conversation_id
                && format!("codex:{}", session.account) == r.request.account_id,
            "RESUME_IDENTITY_MISMATCH"
        );
        r.generation = session.generation();
        r.phase = "attached".into();
        Ok(())
    })?;
    Ok(())
}
pub async fn status(paths: &Paths, id: &str) -> Result<Receipt> {
    let r = read(paths, id)?;
    if r.operation_id.is_none()
        && daemon::now() - r.updated_at > 10
        && let Some((phase, detail)) = crate::background::interrupted_launch(&r.phase)
        && crate::background::worker_stopped(&file(paths, id)?.with_extension("worker.lock"))?
    {
        return change(paths, id, |current| {
            if current.phase == r.phase && current.updated_at == r.updated_at {
                current.phase = phase.into();
                current.detail = Some(detail.into());
            }
            Ok(())
        });
    }
    // A completed launch receipt is historical admission evidence. A later stop
    // or generation must not rewrite it as uncertainty or change Space ordering.
    if matches!(r.phase.as_str(), "running" | "failed" | "recovered") {
        return Ok(r);
    }
    if let Some(job_id) = &r.operation_id {
        if let Some(job) = external::jobs(paths)?.into_iter().find(|j| j.id == *job_id) {
            if matches!(
                job.phase.as_str(),
                "complete" | "failed" | "cancelled" | "recovered"
            ) {
                let items = external::discover(paths).await?;
                catalog::reconcile(paths, &items)?;
            }
            let session = catalog::list(paths)?
                .into_iter()
                .find(|s| s.id == r.request.session_id);
            return change(paths, id, |r| {
                r.detail = Some(job.detail.clone());
                r.phase = match job.phase.as_str() {
                    "complete" => {
                        if session.as_ref().is_some_and(|s| {
                            s.conversation_id == r.conversation_id
                                && format!("codex:{}", s.account) == r.request.account_id
                                && external::new_launch_observed(paths, s).unwrap_or(false)
                        }) {
                            if r.request.directory.is_some() {
                                external::save_resume_directory(
                                    paths,
                                    &r.conversation_id,
                                    &r.directory,
                                )?;
                            }
                            "running"
                        } else {
                            "uncertain"
                        }
                    }
                    "recovered" => "recovered",
                    "failed" | "cancelled" => "failed",
                    _ => "transferring",
                }
                .into();
                if let Some(s) = session {
                    r.generation = s.generation();
                }
                Ok(())
            });
        }
    }
    if r.phase == "attached" || r.phase == "uncertain" {
        if let Some(s) = catalog::list(paths)?.into_iter().find(|s| {
            s.id == r.request.session_id
                && s.generation() == r.generation
                && s.conversation_id == r.conversation_id
                && format!("codex:{}", s.account) == r.request.account_id
        }) {
            if external::new_launch_observed(paths, &s).unwrap_or(false) {
                return change(paths, id, |r| {
                    if r.request.directory.is_some() {
                        external::save_resume_directory(paths, &r.conversation_id, &r.directory)?;
                    }
                    r.phase = "running".into();
                    Ok(())
                });
            }
        }
    }
    Ok(r)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unfinished_consent_never_bypasses_other_history_gates() {
        let mut source: external::Instance = serde_json::from_value(json!({
            "inactive":true,"pane":"","tmux_session":"","pid":0,"cwd":"/tmp",
            "thread_id":"fixture","rollout":"/tmp/history","home":"/tmp/home",
            "binary":"codex","resume_options":[],"block_reason":"unfinished"
        }))
        .unwrap();
        assert!(
            validate_saved_history(&source, false)
                .unwrap_err()
                .to_string()
                .contains("UNFINISHED_HISTORY_CONFIRMATION_REQUIRED")
        );
        assert!(validate_saved_history(&source, true).is_ok());
        for reason in ["busy", "unknown", "diverged", "moving", "future"] {
            source.block_reason = Some(reason.into());
            assert!(validate_saved_history(&source, true).is_err());
        }
        source.block_reason = Some("unfinished".into());
        source.move_id = Some("operation".into());
        assert!(validate_saved_history(&source, true).is_err());
        source.move_id = None;
        source.inactive = false;
        assert!(validate_saved_history(&source, true).is_err());
    }
    #[test]
    fn directory_override_preserves_writer_and_unfinished_history_guards() {
        let tmp = tempfile::tempdir().unwrap();
        let rollout = tmp.path().join("history.jsonl");
        fs::write(
            &rollout,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n",
        )
        .unwrap();
        let source: external::Instance = serde_json::from_value(json!({"inactive":true,"pane":"","tmux_session":"","pid":0,"cwd":tmp.path().join("missing"),"thread_id":"fixture","rollout":rollout,"home":tmp.path(),"binary":"codex","resume_options":[],"block_reason":"unknown"})).unwrap();
        let mut r: Request = serde_json::from_value(json!({"version":1,"request_id":uuid::Uuid::new_v4(),"session_id":uuid::Uuid::new_v4(),"expected_generation":"g","account_id":"codex:fixture","name":"Name","host":{"session":"fixture","socket":"/tmp/socket"},"space":{"kind":"existing","id":"w1"},"directory":{"path":tmp.path(),"create":[],"confirmed_existing":true}})).unwrap();
        assert!(validate_resume_history(&source, &r).is_ok());
        let writer = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
        assert!(validate_resume_history(&source, &r).is_err());
        drop(writer);
        fs::write(
            &rollout,
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
        )
        .unwrap();
        assert!(
            validate_resume_history(&source, &r)
                .unwrap_err()
                .to_string()
                .contains("UNFINISHED_HISTORY_CONFIRMATION_REQUIRED")
        );
        r.confirm_unfinished = true;
        assert!(validate_resume_history(&source, &r).is_ok());
        r.directory = None;
        assert!(validate_resume_history(&source, &r).is_err());
    }
    #[test]
    fn legacy_resume_receipt_preserves_saved_directory_and_request_identity() {
        let request = json!({"version":1,"request_id":uuid::Uuid::new_v4(),"session_id":uuid::Uuid::new_v4(),"expected_generation":"g","account_id":"codex:fixture","name":"Name","host":{"session":"fixture","socket":"/tmp/socket"},"space":{"kind":"existing","id":"w1"}});
        let receipt: Receipt = serde_json::from_value(json!({"version":1,"request":request,"directory":"/original","previous_name":"Old","conversation_id":"thread","generation":"g","phase":"running","updated_at":1})).unwrap();
        assert_eq!(saved_directory(&receipt), std::path::Path::new("/original"));
        assert!(receipt.created_directories.is_empty());
        assert_eq!(
            serde_json::to_value(&receipt.request).unwrap()["directory"],
            Value::Null
        );
    }
    #[test]
    fn request_directory_is_explicit_and_unknown_provider_is_rejected() {
        let mut value = json!({"version":1,"request_id":uuid::Uuid::new_v4(),"session_id":uuid::Uuid::new_v4(),"expected_generation":"g","account_id":"codex:fixture","name":"Name","host":{"session":"fixture","socket":"/tmp/socket"},"space":{"kind":"existing","id":"w1"}});
        assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_ok());
        value["directory"] = json!("/different");
        assert!(serde_json::from_value::<Request>(value.clone()).is_err());
        value["directory"] = json!({"path":"/different","create":[],"confirmed_existing":true});
        assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_ok());
        value.as_object_mut().unwrap().remove("directory");
        value["account_id"] = json!("claude:fixture");
        assert!(shape(&serde_json::from_value(value).unwrap()).is_err());
    }
}
