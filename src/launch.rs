//! Durable new-session admission. No message receipts or supervisor ownership here.
use crate::{
    catalog, daemon,
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::{AccountRef, ProviderId},
};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    os::unix::fs::FileTypeExt,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

#[derive(Subcommand)]
pub enum Command {
    /// List retained receipts without starting or retrying a launch.
    List,
    /// Validate a JSON creation request from stdin without creating resources.
    Validate,
    /// Admit a JSON creation request exactly once; reconcile using status.
    Start,
    Status {
        #[arg(long)]
        request_id: String,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub session: String,
    pub socket: PathBuf,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Space {
    Existing { id: String },
    New { label: String },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directory {
    pub path: PathBuf,
    pub create: Vec<PathBuf>,
    pub confirmed_existing: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub request_id: String,
    pub account_id: String,
    pub name: String,
    pub host: Host,
    pub space: Space,
    pub directory: Directory,
    /// Revision 16, Claude only: start a Sessionsation-hosted session whose pane shows the viewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted: Option<Hosted>,
    /// Revision 19: launch permissions for this start (`yolo` | `default`);
    /// omitted uses the `launch_permissions` setting. A hosted `permission_mode` wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<crate::config::LaunchPermissions>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hosted {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub version: u32,
    pub request: Request,
    pub phase: String,
    pub detail: Option<String>,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub pane_id: Option<String>,
    pub conversation_id: Option<String>,
    pub session_id: Option<String>,
    pub generation: Option<String>,
    pub created_directories: Vec<PathBuf>,
    /// Immutable admission time; absent for legacy receipts whose creation is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    pub updated_at: i64,
}
fn validate_id(id: &str) -> Result<()> {
    ensure!(uuid::Uuid::parse_str(id).is_ok(), "INVALID_REQUEST_ID");
    Ok(())
}
fn folder(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
}
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name.trim() == name
            && !name.chars().any(char::is_control)
            && !name.contains('/')
            && name != "."
            && name != "..",
        "INVALID_SESSION_NAME"
    );
    Ok(())
}
/// User-assigned session names count Unicode scalar values, not UTF-8 bytes.
pub fn validate_session_name(name: &str) -> Result<()> {
    validate_name(name)?;
    ensure!(
        name.chars().count() <= crate::model::SESSION_NAME_MAX,
        "INVALID_SESSION_NAME"
    );
    Ok(())
}
fn shape(r: &Request) -> Result<()> {
    ensure!(r.version == 1, "UNSUPPORTED_LAUNCH_VERSION");
    validate_id(&r.request_id)?;
    validate_session_name(&r.name)?;
    crate::paths::validate_name(&r.host.session)?;
    AccountRef::parse(&r.account_id)?;
    ensure!(r.host.socket.is_absolute(), "INVALID_HOST_SOCKET");
    if let Some(h) = &r.hosted {
        ensure!(
            AccountRef::parse(&r.account_id)?.provider == ProviderId::Claude,
            "UNSUPPORTED_PROVIDER_OPERATION: hosted sessions are Claude-only"
        );
        ensure!(
            h.permission_mode
                .as_deref()
                .is_none_or(|m| crate::provider::claude::hosted::PERMISSION_MODES.contains(&m)),
            "INVALID_PERMISSION_MODE"
        );
    }
    if let Space::New { label } = &r.space {
        validate_name(label)?;
    }
    directory_shape(&r.directory)
}
pub(crate) fn directory_shape(directory: &Directory) -> Result<()> {
    ensure!(
        folder(&directory.path) && directory.create.len() <= 32,
        "INVALID_DIRECTORY"
    );
    let mut previous: Option<&Path> = None;
    for p in &directory.create {
        ensure!(
            folder(p) && directory.path.starts_with(p),
            "INVALID_DIRECTORY_PLAN"
        );
        if let Some(parent) = previous {
            ensure!(p.parent() == Some(parent), "INVALID_DIRECTORY_PLAN");
        }
        previous = Some(p);
    }
    if let Some(last) = previous {
        ensure!(last == directory.path, "INVALID_DIRECTORY_PLAN");
    }
    Ok(())
}
pub(crate) fn directory_check(d: &Directory) -> Result<()> {
    if d.create.is_empty() {
        ensure!(
            d.confirmed_existing && d.path.is_dir(),
            "DIRECTORY_CONFIRMATION_REQUIRED"
        );
        ensure!(fs::canonicalize(&d.path)? == d.path, "DIRECTORY_CHANGED");
    } else {
        ensure!(!d.confirmed_existing, "INVALID_DIRECTORY_PLAN");
        let parent = d.create[0].parent().context("INVALID_DIRECTORY_PLAN")?;
        ensure!(
            parent.is_dir() && fs::canonicalize(parent)? == parent,
            "DIRECTORY_PARENT_CHANGED"
        );
        for p in &d.create {
            ensure!(
                fs::symlink_metadata(p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                "DIRECTORY_ALREADY_EXISTS"
            );
        }
    }
    Ok(())
}
pub(crate) fn create_directories(
    d: &Directory,
    mut record: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    for p in &d.create {
        let parent = p.parent().context("INVALID_DIRECTORY_PLAN")?;
        ensure!(
            fs::canonicalize(parent)? == parent,
            "DIRECTORY_PARENT_CHANGED"
        );
        fs::create_dir(p).context("DIRECTORY_CREATE_FAILED")?;
        record(p)?;
    }
    Ok(())
}
fn location(paths: &Paths, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(paths.root.join("launches").join(format!("{id}.json")))
}
fn lock(paths: &Paths) -> Result<fs::File> {
    private_dir(&paths.root.join("launches"))?;
    let f = lock_file(&paths.root.join("launches/admission.lock"))?;
    f.lock_exclusive()?;
    Ok(f)
}
fn save(paths: &Paths, r: &mut Receipt) -> Result<()> {
    r.updated_at = daemon::now();
    atomic_write(
        &location(paths, &r.request.request_id)?,
        &serde_json::to_vec_pretty(r)?,
    )
}
pub fn status(paths: &Paths, id: &str) -> Result<Receipt> {
    let file = location(paths, id)?;
    ensure!(file.is_file(), "LAUNCH_NOT_FOUND");
    Ok(serde_json::from_slice(&fs::read(file)?)?)
}
fn reconcile_status(paths: &Paths, id: &str) -> Result<Receipt> {
    let r = status(paths, id)?;
    if daemon::now() - r.updated_at > 10
        && let Some((phase, detail)) = crate::background::interrupted_launch(&r.phase)
        && crate::background::worker_stopped(&location(paths, id)?.with_extension("worker.lock"))?
    {
        return change(paths, id, |current| {
            if current.phase == r.phase && current.updated_at == r.updated_at {
                if phase == "failed" {
                    catalog::release_reservation(paths, id, &current.request.name)?;
                }
                current.phase = phase.into();
                current.detail = Some(detail.into());
            }
            Ok(())
        });
    }
    if !matches!(r.phase.as_str(), "attached" | "uncertain") {
        return Ok(r);
    }
    let session = catalog::list(paths)?.into_iter().find(|s| {
        s.id == id
            && r.session_id.as_deref() == Some(s.id.as_str())
            && r.conversation_id.as_deref() == Some(s.conversation_id.as_str())
            && r.generation.as_deref() == Some(s.generation().as_str())
            && r.request.account_id == format!("{}:{}", s.provider, s.account)
    });
    if !session.as_ref().is_some_and(|s| launch_observed(paths, s)) {
        return Ok(r);
    }
    // Positive observation only: never resend a host/provider command or refocus
    // a viewer. Generation is checked again while holding the admission lock.
    change(paths, id, |current| {
        if matches!(current.phase.as_str(), "attached" | "uncertain")
            && current.generation == r.generation
        {
            current.phase = "running".into();
            current.detail = Some("Exact provider process observed; no launch replay".into());
        }
        Ok(())
    })
}
fn change(paths: &Paths, id: &str, f: impl FnOnce(&mut Receipt) -> Result<()>) -> Result<Receipt> {
    let _lock = lock(paths)?;
    let mut r = status(paths, id)?;
    f(&mut r)?;
    save(paths, &mut r)?;
    Ok(r)
}
pub async fn host_call(host: &Host, args: &[&str]) -> Result<Value> {
    ensure!(
        fs::metadata(&host.socket)?.file_type().is_socket(),
        "HOST_SOCKET_UNAVAILABLE"
    );
    let mut command = tokio::process::Command::new("herdr");
    command
        .args(args)
        .env("HERDR_SOCKET_PATH", &host.socket)
        .env("HERDR_SESSION", &host.session)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .context("HOST_RESPONSE_UNCERTAIN")??;
    ensure!(
        output.status.success(),
        "HOST_REJECTED: {}",
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(300)
            .collect::<String>()
    );
    if output.stdout.is_empty() && args.starts_with(&["pane", "run"]) {
        return Ok(json!({"admitted":true}));
    }
    ensure!(
        output.stdout.len() <= 8 * 1024 * 1024,
        "HOST_RESPONSE_TOO_LARGE"
    );
    let body: Value = serde_json::from_slice(&output.stdout).context("HOST_PROTOCOL")?;
    ensure!(body.get("error").is_none(), "HOST_REJECTED");
    Ok(body.get("result").unwrap_or(&body).clone())
}
async fn validate(paths: &Paths, r: &Request) -> Result<()> {
    shape(r)?;
    directory_check(&r.directory)?;
    catalog::check_name(paths, &r.name, &r.request_id)?;
    let account = AccountRef::parse(&r.account_id)?;
    crate::accounts::ensure_available(paths, &account.name)?;
    let state = daemon::call(paths, "status", json!({})).await?;
    ensure!(
        state["accounts"]
            .as_array()
            .is_some_and(|a| a.iter().any(|a| a["name"] == account.name.as_str()
                && a["provider"].as_str().unwrap_or("codex") == account.provider.as_str()
                && a["enabled"] == true
                && a["auth_required"] == false)),
        "ACCOUNT_UNAVAILABLE"
    );
    let spaces = host_call(&r.host, &["workspace", "list"]).await?;
    let spaces = spaces["workspaces"].as_array().context("HOST_PROTOCOL")?;
    match &r.space {
        Space::Existing { id } => ensure!(
            spaces.iter().any(|w| w["workspace_id"] == *id),
            "WORKSPACE_NOT_FOUND"
        ),
        Space::New { label } => ensure!(
            !spaces.iter().any(|w| w["label"] == *label),
            "WORKSPACE_NAME_EXISTS"
        ),
    }
    Ok(())
}
pub async fn api(paths: &Paths, command: Command) -> Result<Value> {
    if matches!(command, Command::List) {
        let mut records = Vec::new();
        let dir = paths.root.join("launches");
        if dir.is_dir() {
            for entry in crate::paths::journal_paths(&dir)? {
                if !entry.is_file() {
                    continue;
                }
                let Some(id) = entry
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_owned)
                else {
                    continue;
                };
                if uuid::Uuid::parse_str(&id).is_ok() {
                    records.push(reconcile_status(paths, &id)?);
                }
            }
        }
        records.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
        return Ok(json!(records));
    }
    if let Command::Status { request_id } = command {
        return Ok(serde_json::to_value(reconcile_status(paths, &request_id)?)?);
    }
    let mut bytes = Vec::new();
    std::io::stdin().take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "REQUEST_TOO_LARGE");
    let r: Request = serde_json::from_slice(&bytes).context("INVALID_LAUNCH_REQUEST")?;
    shape(&r)?;
    if matches!(command, Command::Validate) {
        validate(paths, &r).await?;
        return Ok(json!({"valid":true,"request_id":r.request_id}));
    }
    // The same request is never dispatched twice, even after a timeout.
    let _lock = lock(paths)?;
    if location(paths, &r.request_id)?.exists() {
        let existing = status(paths, &r.request_id)?;
        ensure!(existing.request == r, "REQUEST_ID_CONFLICT");
        return Ok(serde_json::to_value(existing)?);
    }
    validate(paths, &r).await?;
    catalog::reserve_name(paths, &r.name, &r.request_id)?;
    let mut receipt = Receipt {
        version: 1,
        request: r,
        phase: "admitted".into(),
        detail: None,
        workspace_id: None,
        tab_id: None,
        pane_id: None,
        conversation_id: None,
        session_id: None,
        generation: None,
        created_directories: Vec::new(),
        created_at: Some(daemon::now()),
        updated_at: daemon::now(),
    };
    save(paths, &mut receipt)?;
    let spawned =
        crate::background::detach(&mut std::process::Command::new(std::env::current_exe()?))
            .arg("--data-dir")
            .arg(&paths.root)
            .args(["launch-worker", &receipt.request.request_id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    if spawned.is_err() {
        catalog::release_reservation(paths, &receipt.request.request_id, &receipt.request.name)?;
        receipt.phase = "failed".into();
        receipt.detail = Some("Worker could not start; no resources created".into());
        save(paths, &mut receipt)?;
    }
    Ok(serde_json::to_value(receipt)?)
}
pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    // This process owns only a new-launch request, never a provider supervisor.
    let _worker = {
        let f = lock_file(
            &paths
                .root
                .join("launches")
                .join(format!("{id}.worker.lock")),
        )?;
        f.try_lock_exclusive()?;
        f
    };
    let receipt = change(paths, id, |r| {
        ensure!(r.phase == "admitted", "LAUNCH_ALREADY_DISPATCHED");
        r.phase = "preparing".into();
        Ok(())
    })?;
    if let Err(error) = prepare(paths, &receipt).await {
        change(paths, id, |r| {
            if !matches!(r.phase.as_str(), "running" | "failed") {
                if r.phase == "preparing" {
                    catalog::release_reservation(paths, id, &r.request.name)?;
                    r.phase = "failed".into();
                } else {
                    r.phase = "uncertain".into();
                }
                r.detail = Some(format!("{error:#}"));
            }
            Ok(())
        })?;
    }
    Ok(())
}
async fn prepare(paths: &Paths, receipt: &Receipt) -> Result<()> {
    let r = &receipt.request;
    let id = &r.request_id;
    validate(paths, r).await?;
    create_directories(&r.directory, |p| {
        change(paths, id, |r| {
            r.created_directories.push(p.to_path_buf());
            Ok(())
        })?;
        Ok(())
    })?;
    // Record uncertainty before the first external mutation: a lost reply is not rejection.
    change(paths, id, |r| {
        r.phase = "creating_terminal".into();
        Ok(())
    })?;
    let created = create_terminal(&r.host, &r.space, &r.directory.path, &r.name).await?;
    let pane = created["root_pane"]["pane_id"]
        .as_str()
        .context("HOST_PANE_ID_MISSING")?
        .to_owned();
    let tab = created["tab"]["tab_id"]
        .as_str()
        .context("HOST_TAB_ID_MISSING")?
        .to_owned();
    let workspace = created["tab"]["workspace_id"]
        .as_str()
        .or_else(|| created["workspace"]["workspace_id"].as_str())
        .context("HOST_WORKSPACE_ID_MISSING")?
        .to_owned();
    if let Space::Existing { id } = &r.space {
        ensure!(*id == workspace, "HOST_WRONG_WORKSPACE");
    }
    change(paths, id, |r| {
        r.workspace_id = Some(workspace.clone());
        r.tab_id = Some(tab.clone());
        r.pane_id = Some(pane.clone());
        Ok(())
    })?;
    if matches!(r.space, Space::New { .. }) {
        host_call(&r.host, &["tab", "rename", &tab, &r.name]).await?;
    }
    // Wait for the newly created shell only, never inspect/control an existing pane.
    let mut ready = false;
    for _ in 0..50 {
        let info = host_call(&r.host, &["pane", "process-info", "--pane", &pane]).await?;
        let i = &info["process_info"];
        if i["foreground_processes"].as_array().is_some_and(|p| {
            p.len() == 1
                && p[0]["pid"] == i["shell_pid"]
                && matches!(p[0]["name"].as_str(), Some("bash" | "sh" | "zsh" | "fish"))
        }) {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ensure!(ready, "NEW_TERMINAL_NOT_READY");
    let quote = crate::terminal::shell_quote;
    let exe = std::env::current_exe()?;
    let command = format!(
        "cd -- {} && SESSIONSATION_HERDR_SESSION={} SESSIONSATION_LAUNCHER_BYPASS=1 {} --data-dir {} launch-run {}",
        quote(r.directory.path.to_str().unwrap()),
        quote(&r.host.session),
        quote(exe.to_str().context("INVALID_EXECUTABLE")?),
        quote(paths.root.to_str().context("INVALID_DATA_ROOT")?),
        quote(id)
    );
    change(paths, id, |r| {
        r.phase = "dispatching".into();
        Ok(())
    })?;
    host_call(&r.host, &["pane", "run", &pane, &command]).await?;
    // launch-run confirms exact pane identity before touching a provider.
    for _ in 0..300 {
        let status = status(paths, id)?;
        if status.phase == "running" {
            return Ok(());
        }
        if status.phase == "attached" {
            let session = catalog::list(paths)?.into_iter().find(|s| s.id == *id);
            if !session.as_ref().is_some_and(|s| launch_observed(paths, s)) {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            change(paths, id, |r| {
                r.phase = "running".into();
                Ok(())
            })?;
            return Ok(());
        }
        ensure!(status.phase != "failed", "PROVIDER_LAUNCH_FAILED");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("LAUNCH_ACK_UNCERTAIN")
}
pub async fn run(paths: &Paths, id: &str) -> Result<()> {
    let receipt = change(paths, id, |r| {
        ensure!(r.phase == "dispatching", "LAUNCH_ALREADY_DISPATCHED");
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
    if let Some(mode) = receipt.request.permissions {
        crate::config::override_launch_permissions(mode);
    }
    let result = match AccountRef::parse(&receipt.request.account_id) {
        Err(error) => Err(error),
        Ok(AccountRef {
            provider: ProviderId::Codex,
            name,
        }) => crate::external::run_account_new_named(paths, &name, id, &receipt.request.name).await,
        Ok(AccountRef {
            provider: ProviderId::Claude,
            name,
        }) if receipt.request.hosted.is_some() => {
            crate::provider::claude::hosted::run_in_terminal(
                paths,
                &name,
                None,
                receipt
                    .request
                    .hosted
                    .as_ref()
                    .and_then(|h| h.permission_mode.clone()),
                Some(id),
            )
            .await
        }
        Ok(AccountRef {
            provider: ProviderId::Claude,
            name,
        }) => crate::provider::claude::launch::exec_new(paths, &name, None, Some(id)).await,
        Ok(AccountRef {
            provider: ProviderId::Gemini,
            name,
        }) => crate::provider::gemini::launch::exec_new(paths, &name, None, Some(id)).await,
    };
    if let Err(error) = result {
        change(paths, id, |r| {
            r.phase = "uncertain".into();
            r.detail = Some(format!("{error:#}"));
            Ok(())
        })?;
    }
    Ok(())
}
pub fn attached(
    paths: &Paths,
    id: &str,
    conversation: &str,
    attachment: Option<catalog::Attachment>,
) -> Result<catalog::Session> {
    let receipt = status(paths, id)?;
    let account = AccountRef::parse(&receipt.request.account_id)?;
    // Native providers resume by directory; Codex keeps its directory out of the catalog.
    let directory = crate::provider::native::is_native(account.provider)
        .then(|| receipt.request.directory.path.to_str())
        .flatten();
    let session = catalog::complete_reserved(
        paths,
        id,
        &receipt.request.name,
        account.provider,
        &account.name,
        conversation,
        directory,
        attachment,
    )?;
    change(paths, id, |r| {
        r.phase = "attached".into();
        r.conversation_id = Some(conversation.into());
        r.session_id = Some(session.id.clone());
        r.generation = Some(session.generation());
        Ok(())
    })?;
    Ok(session)
}
/// Positive provider-specific evidence that the recorded process runs the session.
fn launch_observed(paths: &Paths, s: &catalog::Session) -> bool {
    match ProviderId::parse(&s.provider) {
        Ok(ProviderId::Codex) => crate::external::new_launch_observed(paths, s).unwrap_or(false),
        Ok(ProviderId::Claude | ProviderId::Gemini) => crate::provider::native::launch_observed(s),
        Err(_) => false,
    }
}

pub(crate) async fn create_terminal(
    host: &Host,
    space: &Space,
    directory: &Path,
    name: &str,
) -> Result<Value> {
    let env = "SESSIONSATION_LAUNCHER_BYPASS=1";
    let created = match space {
        Space::Existing { id: workspace } => {
            host_call(
                host,
                &[
                    "tab",
                    "create",
                    "--workspace",
                    workspace,
                    "--cwd",
                    directory.to_str().context("INVALID_DIRECTORY")?,
                    "--label",
                    name,
                    "--env",
                    env,
                    "--no-focus",
                ],
            )
            .await?
        }
        Space::New { label } => {
            host_call(
                host,
                &[
                    "workspace",
                    "create",
                    "--cwd",
                    directory.to_str().context("INVALID_DIRECTORY")?,
                    "--label",
                    label,
                    "--env",
                    env,
                    "--no-focus",
                ],
            )
            .await?
        }
    };
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_creation_time_stays_unknown_and_admission_time_is_immutable() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../docs/contract/fixtures.json")).unwrap();
        let mut receipt: Receipt =
            serde_json::from_value(fixtures["launch_receipt"].clone()).unwrap();
        assert_eq!(receipt.created_at, None);
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
        save(&paths, &mut receipt).unwrap();
        assert_eq!(
            status(&paths, &receipt.request.request_id)
                .unwrap()
                .created_at,
            None
        );
        receipt.created_at = Some(123);
        receipt.phase = "running".into();
        save(&paths, &mut receipt).unwrap();
        let loaded = status(&paths, &receipt.request.request_id).unwrap();
        assert_eq!(loaded.created_at, Some(123));
        assert!(loaded.updated_at > 123);
        let mut resume_value = fixtures["resume_receipt"].clone();
        resume_value["conversation_id"] = serde_json::json!("11111111-1111-4111-8111-111111111111");
        resume_value["generation"] = serde_json::json!("legacy-generation");
        let resume: crate::resume::Receipt = serde_json::from_value(resume_value).unwrap();
        assert_eq!(resume.created_at, None);
        let deletion: crate::account_deletion::Record =
            serde_json::from_value(fixtures["account_deletion"].clone()).unwrap();
        assert_eq!(deletion.created_at, None);
    }
    #[test]
    fn directory_preview_does_not_create_and_rejects_late_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("new");
        let d = Directory {
            path: p.clone(),
            create: vec![p.clone()],
            confirmed_existing: false,
        };
        directory_check(&d).unwrap();
        assert!(!p.exists());
        fs::create_dir(&p).unwrap();
        assert!(directory_check(&d).is_err());
        assert!(
            directory_check(&Directory {
                path: p.clone(),
                create: vec![],
                confirmed_existing: false
            })
            .is_err()
        );
        directory_check(&Directory {
            path: p,
            create: vec![],
            confirmed_existing: true,
        })
        .unwrap();
    }
    #[test]
    fn session_limit_is_separate_from_directory_and_space_names() {
        for name in ["a".repeat(30), "界".repeat(30)] {
            validate_session_name(&name).unwrap();
        }
        for name in ["a".repeat(31), "界".repeat(31)] {
            assert!(validate_session_name(&name).is_err());
            validate_name(&name).unwrap();
        }
    }
    #[test]
    fn hosted_launches_are_claude_only_with_a_known_permission_mode() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("../docs/contract/fixtures.json")).unwrap();
        let mut r: Request = serde_json::from_value(fixtures["launch_request"].clone()).unwrap();
        assert!(r.hosted.is_none(), "older requests stay valid");
        r.account_id = "claude:personal".into();
        r.hosted = Some(Hosted {
            permission_mode: Some("plan".into()),
        });
        shape(&r).unwrap();
        r.hosted = Some(Hosted {
            permission_mode: Some("yolo".into()),
        });
        assert!(shape(&r).is_err());
        r.hosted = Some(Hosted {
            permission_mode: None,
        });
        r.account_id = "codex:work".into();
        assert!(
            shape(&r)
                .unwrap_err()
                .to_string()
                .contains("UNSUPPORTED_PROVIDER_OPERATION")
        );
        let mut v = fixtures["launch_request"].clone();
        v["hosted"] = serde_json::json!({"permission_mode":"plan","extra":1});
        assert!(serde_json::from_value::<Request>(v).is_err());
    }
    #[test]
    fn request_id_and_name_cannot_be_paths() {
        assert!(validate_id("../x").is_err());
        for n in ["../x", ".", "..", " x", "x/y"] {
            assert!(validate_name(n).is_err());
        }
    }
}
