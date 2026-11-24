//! Durable saved-session intent. Codex reuses the account launcher and transfer worker;
//! Claude and Antigravity resume natively in a HERDR pane (contract 1.66.0).
use crate::background::SpawnReaped as _;
use crate::{
    api, catalog, daemon, external,
    launch::{self, Host, Space},
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::ops::{self, ResumeLaunch},
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
    /// Revision 19: launch permissions for this start (`yolo` | `default`);
    /// omitted uses the `launch_permissions` setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<crate::config::LaunchPermissions>,
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
    /// Set by `api abandon-operation`: the phase the operator abandoned it from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned_at: Option<i64>,
    /// Immutable admission time; absent for legacy receipts whose creation is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    pub updated_at: i64,
    /// Contract 1.32: set for a `sessionsation resume` typed into an existing terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<crate::inline::Info>,
    /// Contract 1.52.0: the resume runs in an existing idle pane (the session's recorded
    /// pane or a same-named tab) instead of a new tab; `detail` says which. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pane_reused: bool,
    /// Contract 1.66.0: the session was already running when admitted; nothing was started
    /// (`phase:"running"`). Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub already_running: bool,
    /// Contract 1.66.2: `no_history` when the session had no transcript (stopped before its
    /// first turn) and was started again under the same conversation id. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
}
/// `resumed_from` of a session started again because it had no history (contract 1.66.2).
pub const NO_HISTORY: &str = "no_history";
pub(crate) fn file(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_REQUEST_ID")?;
    Ok(paths
        .root
        .join(crate::layout::RESUMPTIONS)
        .join(format!("{id}.json")))
}
pub(crate) fn lock(paths: &Paths) -> Result<fs::File> {
    let dir = paths.root.join(crate::layout::RESUMPTIONS);
    private_dir(&dir)?;
    let f = lock_file(&dir.join("admission.lock"))?;
    f.lock_exclusive()?;
    Ok(f)
}
pub(crate) fn read(paths: &Paths, id: &str) -> Result<Receipt> {
    let path = file(paths, id)?;
    ensure!(path.is_file(), "RESUME_NOT_FOUND");
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub(crate) fn save(paths: &Paths, r: &mut Receipt) -> Result<()> {
    r.updated_at = daemon::now();
    atomic_write(
        &file(paths, &r.request.request_id)?,
        &serde_json::to_vec_pretty(r)?,
    )
}
pub(crate) fn change(
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
/// Contract 1.66.2: the HERDR pane (`herdr/<server>/<pane>`) of the newest resume-launch
/// that started session `id` (not an inline resume, not an `already_running` answer).
pub(crate) fn resumed_pane(paths: &Paths, id: &str) -> Option<String> {
    records(paths)
        .ok()?
        .into_iter()
        .find(|r| {
            r.request.session_id == id
                && r.inline.is_none()
                && !r.already_running
                && r.phase == "running"
        })
        .and_then(|r| {
            crate::terminal::herdr_target(&r.request.host.session, r.pane_id.as_deref()?).ok()
        })
}
pub(crate) fn records(paths: &Paths) -> Result<Vec<Receipt>> {
    let dir = paths.root.join(crate::layout::RESUMPTIONS);
    let mut records = vec![];
    if dir.is_dir() {
        for e in crate::paths::journal_paths(&dir)? {
            if e.is_file() {
                records.push(serde_json::from_slice::<Receipt>(&fs::read(e)?)?);
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
    catalog::parse_name(&r.name)?;
    kind(r)?;
    crate::paths::validate_name(&r.host.session)?;
    ensure!(r.host.socket.is_absolute(), "INVALID_HOST_SOCKET");
    if let Some(d) = &r.directory {
        launch::directory_shape(d)?;
    }
    Ok(())
}
/// The Codex account a transfer resume-launch runs on (the HERDR transfer worker).
fn codex_account(r: &Request) -> Result<String> {
    let account = crate::provider::AccountRef::parse(&r.account_id)?;
    Ok(account.require_supervised()?.to_owned())
}
/// How this request's provider resumes (contract 1.66.0): the Codex transfer worker or the
/// provider's own resume; a provider without one (Gemini) is refused with its reason.
fn kind(r: &Request) -> Result<ResumeLaunch> {
    let account = crate::provider::AccountRef::parse(&r.account_id)?;
    match ops::facts(account.provider).resume_launch {
        ResumeLaunch::Refused(reason) => {
            Err(crate::provider::stop_shape::Unsupported(reason).into())
        }
        kind => Ok(kind),
    }
}
/// Detail of the answer for a session that already runs (contract 1.66.0).
pub const ALREADY_RUNNING: &str =
    "The session is already running; nothing was started (already_running).";
/// The receipt for a session that already runs: `phase:"running"`, `already_running:true`.
fn already_running(r: Request, s: &catalog::Session, directory: PathBuf) -> Receipt {
    Receipt {
        version: 1,
        directory: directory.clone(),
        saved_directory: Some(directory),
        created_directories: vec![],
        request: r,
        previous_name: s.name.clone(),
        conversation_id: s.conversation_id.clone(),
        generation: s.generation(),
        phase: "running".into(),
        detail: Some(ALREADY_RUNNING.into()),
        workspace_id: None,
        tab_id: None,
        pane_id: None,
        operation_id: None,
        abandoned_from: None,
        abandoned_at: None,
        created_at: Some(daemon::now()),
        updated_at: daemon::now(),
        inline: None,
        pane_reused: false,
        already_running: true,
        resumed_from: None,
    }
}
/// The session already runs: its entry and directory (the generation is checked). `None`
/// when it does not run or cannot be observed (admission then reports why).
async fn running(
    paths: &Paths,
    r: &Request,
    native: bool,
) -> Result<Option<(catalog::Session, PathBuf)>> {
    if native {
        let (s, o) = api::native_selected(paths, &r.session_id, &r.expected_generation)?;
        let directory = s
            .directory
            .clone()
            .or(o.directory.clone())
            .unwrap_or_default();
        return Ok((o.availability == "running").then(|| (s, directory.into())));
    }
    catalog::resolve(paths, &r.session_id, &r.expected_generation)?;
    Ok(api::selected(paths, &r.session_id, &r.expected_generation)
        .await
        .ok()
        .filter(|(_, source)| !source.inactive)
        .map(|(s, source)| (s, source.cwd.into())))
}
/// Admission checks of a provider that resumes natively (Claude, Antigravity): the same
/// account, its saved directory, an idle stopped conversation. Returns the entry, the
/// directory, the destination Space label and whether the session has no history and is
/// started again under its conversation id (contract 1.66.2, `resumed_from:"no_history"`).
async fn validate_native(
    paths: &Paths,
    r: &Request,
) -> Result<(catalog::Session, PathBuf, Option<String>, bool)> {
    shape(r)?;
    let (s, o) = api::native_selected(paths, &r.session_id, &r.expected_generation)?;
    crate::lifecycle::ensure_available(paths, &s.conversation_id, Some(&r.request_id))?;
    let own = format!("{}:{}", s.provider, s.account);
    ops::require(
        r.account_id == own,
        format!(
            "resume-launch resumes a {} session on its own account ({own})",
            s.provider
        ),
    )?;
    let fresh = crate::provider::native::without_history(&s, &o);
    if fresh {
        crate::provider::native::ensure_fresh_start(&s, &o)?;
    } else {
        crate::provider::native::ensure_resumable(&s, &o)?;
    }
    crate::messaging::headless::ensure_idle(paths, &s.conversation_id)?;
    let directory = PathBuf::from(crate::provider::native::resume_directory(&s, &o)?);
    if let Some(d) = &r.directory {
        ops::require(
            crate::alias::same_path(&d.path, &directory),
            format!(
                "resume-launch keeps the saved directory of a {} session",
                s.provider
            ),
        )?;
    }
    let label = destination(r, &s.id, paths).await?;
    Ok((s, directory, label, fresh))
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
/// Admission checks; also returns the destination Space label (the resumed row's `space`).
async fn validate(
    paths: &Paths,
    r: &Request,
) -> Result<(catalog::Session, external::Instance, Option<String>)> {
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
    let target = codex_account(r)?;
    let target = target.as_str();
    crate::accounts::ensure_available(paths, crate::provider::ProviderId::Codex, target)?;
    let state = daemon::call(paths, "status", json!({})).await?;
    ensure!(
        state["accounts"]
            .as_array()
            .is_some_and(|aa| aa.iter().any(|a| a["name"] == target
                && crate::provider::ops::supervised(a["provider"].as_str().unwrap_or("codex"))
                && a["enabled"] == true
                && a["auth_required"] == false)),
        "ACCOUNT_UNAVAILABLE"
    );
    let label = destination(r, &s.id, paths).await?;
    Ok((s, source, label))
}
/// The destination Space exists (or a new one's label is free) and the name is free there;
/// returns the destination Space label.
async fn destination(r: &Request, session: &str, paths: &Paths) -> Result<Option<String>> {
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
    // Unique in the destination Space (contract 1.48.0).
    let label = launch::space_label(spaces, &r.space);
    ensure!(
        !catalog::space_taken(paths, label.as_deref(), &r.name, session)?,
        "SESSION_NAME_EXISTS: \"{}\" is already used in this Space",
        r.name
    );
    Ok(label)
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
    let mut r: Request = serde_json::from_slice(&bytes).context("INVALID_RESUME_REQUEST")?;
    if let Some(d) = r.directory.as_mut() {
        launch::normalize_directory(d);
    }
    shape(&r)?;
    let label = launch::resolve_space(&r.host, &mut r.space).await?;
    let known = label.is_some();
    launch::bare_request_name(&mut r.name, label.as_deref(), known)?;
    // Contract 1.66.0: one request, receipt and `already_running` answer for every provider.
    let native = kind(&r)? == ResumeLaunch::Native;
    if matches!(command, Command::Validate) {
        if running(paths, &r, native).await?.is_some() {
            return Ok(json!({"valid":true,"already_running":true,"request_id":r.request_id}));
        }
        let (saved, fresh): (PathBuf, bool) = if native {
            let (_, directory, _, fresh) = validate_native(paths, &r).await?;
            (directory, fresh)
        } else {
            (validate(paths, &r).await?.1.cwd.into(), false)
        };
        return Ok(
            json!({"valid":true,"directory":r.directory.as_ref().map(|d|d.path.clone()).unwrap_or(saved),"request_id":r.request_id,"resumed_from":fresh.then_some(NO_HISTORY)}),
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
                && !matches!(
                    prior.phase.as_str(),
                    "running" | "complete" | "failed" | "recovered" | "abandoned"
                )),
        "RESUME_IN_PROGRESS"
    );
    let selected = catalog::resolve(paths, &r.session_id, &r.expected_generation)?;
    if let Some((s, directory)) = running(paths, &r, native).await? {
        let mut receipt = already_running(r, &s, directory);
        save(paths, &mut receipt)?;
        return Ok(json!(receipt));
    }
    let _admission =
        crate::lifecycle::admit(paths, &selected.conversation_id, Some(&r.request_id))?;
    crate::lifecycle::ensure_no_handoff(paths, &selected.conversation_id, Some(&r.request_id))?;
    let (s, saved, fresh): (_, PathBuf, bool) = if native {
        let (s, directory, _, fresh) = validate_native(paths, &r).await?;
        (s, directory, fresh)
    } else {
        let (s, source, _) = validate(paths, &r).await?;
        (s, source.cwd.into(), false)
    };
    let mut receipt = Receipt {
        version: 1,
        directory: r
            .directory
            .as_ref()
            .map(|d| d.path.clone())
            .unwrap_or_else(|| saved.clone()),
        saved_directory: Some(saved),
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
        abandoned_from: None,
        abandoned_at: None,
        created_at: Some(daemon::now()),
        updated_at: daemon::now(),
        inline: None,
        pane_reused: false,
        already_running: false,
        resumed_from: fresh.then(|| NO_HISTORY.to_owned()),
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
    .spawn_reaped();
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
    let prepared = match kind(&r.request) {
        Ok(ResumeLaunch::Native) => prepare_native(paths, &r).await,
        _ => prepare(paths, &r).await,
    };
    if let Err(e) = prepared {
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
    crate::api::refresh_active_snapshot(paths).await;
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
/// Contract 1.66.0: a provider that resumes natively (Claude, Antigravity) runs the same path
/// as `api resume --herdr-session`: a pane in the destination Space (an idle reused one or a
/// new tab) runs `sessionsation resume`, confirmed by the exact process. Rechecked before
/// typing. Phase `resuming` (as Auto Start and the account switch) does not block that typed
/// resume; a failure before it is `failed`, after it `uncertain` (never replayed).
async fn prepare_native(paths: &Paths, receipt: &Receipt) -> Result<()> {
    let r = &receipt.request;
    if let Some(mode) = r.permissions {
        crate::config::override_launch_permissions(mode);
    }
    validate_native(paths, r).await?;
    change(paths, &r.request_id, |r| {
        r.phase = "resuming".into();
        Ok(())
    })?;
    let result = api::resume_launch_native(paths, r).await?;
    let running = result["state"] == "running";
    let pane = result["target"]
        .as_str()
        .and_then(|t| crate::terminal::target(t).ok())
        .map(|(_, pane)| pane.to_owned());
    let generation = catalog::find(paths, &r.session_id).map(|s| s.generation());
    change(paths, &r.request_id, |x| {
        x.pane_id = pane;
        x.pane_reused = result["pane_reused"] == true;
        if let Ok(generation) = generation {
            x.generation = generation;
        }
        x.phase = if running { "running" } else { "uncertain" }.into();
        x.detail = if !running {
            Some("The resume command was typed but the provider process was not confirmed; inspect the pane, do not retry automatically".into())
        } else if x.resumed_from.as_deref() == Some(NO_HISTORY) {
            Some("The session had no conversation history; a fresh conversation was started under the same conversation id.".into())
        } else {
            None
        };
        Ok(())
    })?;
    Ok(())
}
async fn prepare(paths: &Paths, receipt: &Receipt) -> Result<()> {
    let r = &receipt.request;
    if let Some(mode) = r.permissions {
        crate::config::override_launch_permissions(mode);
    }
    let id = &r.request_id;
    let (_, source, label) = validate(paths, r).await?;
    ensure!(
        crate::alias::same_path(std::path::Path::new(&source.cwd), saved_directory(receipt)),
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
    // Contract 1.52.0: the session's recorded idle pane, then a same-named idle tab in
    // the Space, before a new tab.
    let terminal = launch::open_terminal(
        paths,
        &r.host,
        &r.space,
        &receipt.directory,
        &r.name,
        Some(&r.session_id),
        id,
    )
    .await?;
    let record = |t: &launch::Terminal| {
        change(paths, id, |r| {
            r.pane_id = Some(t.pane.clone());
            r.tab_id = Some(t.tab.clone());
            r.workspace_id = Some(t.workspace.clone());
            r.pane_reused = t.reused.is_some();
            r.detail = t.reused.as_ref().map(|x| x.detail().to_owned());
            Ok(())
        })
    };
    record(&terminal)?;
    if matches!(r.space, Space::New { .. }) {
        launch::host_call(&r.host, &["tab", "rename", &terminal.tab, &r.name]).await?;
    }
    let ready = |pane: String| async move {
        let target = crate::terminal::herdr_target(&r.host.session, &pane)?;
        for _ in 0..50 {
            if crate::terminal::state_at(&target, Some(&r.host.socket)).is_ok_and(|s| s.0) {
                return Ok((pane, target));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        anyhow::bail!("NEW_TERMINAL_NOT_READY")
    };
    // A reused pane was just seen idle; the re-check before dispatch is right before typing.
    let (mut pane, mut target) = if terminal.reused.is_some() {
        let target = crate::terminal::herdr_target(&r.host.session, &terminal.pane)?;
        (terminal.pane.clone(), target)
    } else {
        ready(terminal.pane.clone()).await?
    };
    let (current, source) = api::selected(paths, &r.session_id, &r.expected_generation).await?;
    ensure!(
        source.inactive
            && current.attachment.is_none()
            && crate::alias::same_path(std::path::Path::new(&source.cwd), saved_directory(receipt)),
        "STALE_TARGET"
    );
    change(paths, id, |r| {
        r.phase = "renaming".into();
        Ok(())
    })?;
    // The tab exists in the destination Space: the row takes its name and Space.
    let renamed = catalog::rename_in_space(
        paths,
        &r.session_id,
        &r.expected_generation,
        &r.name,
        label.as_deref(),
        "launch",
    )?;
    change(paths, id, |r| {
        r.generation = renamed.generation();
        r.phase = "dispatching".into();
        Ok(())
    })?;
    // Re-checked right before the command goes in: a reused pane that stopped being an
    // idle shell is left alone and a new tab opened instead (resume-run checks the pane).
    if let Some(fresh) =
        launch::recheck(&r.host, &r.space, &receipt.directory, &r.name, &terminal).await?
    {
        record(&fresh)?;
        (pane, target) = ready(fresh.pane.clone()).await?;
    }
    let account = codex_account(r)?;
    let account = account.as_str();
    if account != source.account
        || !crate::layout::same_home(&paths.account(account)?, &source.home)
    {
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
            q(crate::provider::claude::hooks::executable()?
                .to_str()
                .context("INVALID_EXECUTABLE")?),
            q(paths.root.to_str().context("INVALID_DATA_ROOT")?),
            q(id)
        );
        launch::host_call(&r.host, &["pane", "run", &pane, &command]).await?;
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
    if let Some(mode) = r.request.permissions {
        crate::config::override_launch_permissions(mode);
    }
    let result = external::run_account_resume_requested(
        paths,
        &codex_account(&r.request)?,
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
/// Operator-only: end an uncertain or stuck resume-launch so its session
/// reservation is released. Terminal, sends nothing, never replays. Refused while
/// the target session's process is live (or cannot be shown to be gone).
pub async fn abandon(paths: &Paths, id: &str, confirm: &str) -> Result<Value> {
    ensure!(id == confirm, "CONFIRMATION_MISMATCH");
    let r = read(paths, id)?;
    ensure!(
        !matches!(
            r.phase.as_str(),
            "running" | "complete" | "failed" | "recovered" | "abandoned"
        ),
        "OPERATION_ALREADY_TERMINAL"
    );
    ensure!(
        crate::background::worker_stopped(&file(paths, id)?.with_extension("worker.lock"))?,
        "OPERATION_WORKER_ACTIVE"
    );
    if let Some(job_id) = &r.operation_id
        && external::jobs(paths)?.into_iter().any(|j| {
            j.id == *job_id
                && !matches!(
                    j.phase.as_str(),
                    "complete" | "failed" | "cancelled" | "recovered"
                )
        })
    {
        anyhow::bail!("OPERATION_STILL_ACTIVE");
    }
    ensure!(
        r.phase == "uncertain" || daemon::now() - r.updated_at > 10,
        "OPERATION_NOT_STUCK"
    );
    // The operation can affect only its own conversation. A session that is gone
    // or has since been bound to another conversation is unaffected.
    let session = catalog::list(paths)?
        .into_iter()
        .find(|s| s.id == r.request.session_id && s.conversation_id == r.conversation_id);
    if let Some(s) = session {
        if let Some(a) = &s.attachment {
            ensure!(
                external::attachment_definitely_gone(a.pid, a.process_start),
                "SESSION_PROCESS_LIVE"
            );
        }
        let live = external::live_sessions(paths)
            .await
            .context("SESSION_STATE_UNVERIFIED")?;
        ensure!(
            !live
                .iter()
                .any(|i| !i.inactive && i.thread_id == r.conversation_id),
            "SESSION_PROCESS_LIVE"
        );
    }
    let done = change(paths, id, |r| {
        ensure!(
            !matches!(
                r.phase.as_str(),
                "running" | "complete" | "failed" | "recovered" | "abandoned"
            ),
            "OPERATION_ALREADY_TERMINAL"
        );
        r.abandoned_from = Some(r.phase.clone());
        r.abandoned_at = Some(daemon::now());
        r.detail = Some(format!(
            "Abandoned by operator from phase {} ({}); nothing was sent or replayed and the session reservation was released.",
            r.phase,
            r.detail.as_deref().unwrap_or("no detail")
        ));
        r.phase = "abandoned".into();
        Ok(())
    })?;
    Ok(json!(done))
}
pub async fn status(paths: &Paths, id: &str) -> Result<Receipt> {
    let r = read(paths, id)?;
    if r.inline.is_some() {
        crate::inline::settle_resume(paths, id)?;
        return read(paths, id);
    }
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
    if matches!(
        r.phase.as_str(),
        "running" | "complete" | "failed" | "recovered" | "abandoned"
    ) {
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
    fn receipt_for(
        paths: &Paths,
        session: &str,
        conversation: &str,
        phase: &str,
        age: i64,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let now = daemon::now();
        let r: Receipt = serde_json::from_value(json!({
            "version":1,
            "request":{"version":1,"request_id":id,"session_id":session,"expected_generation":"g",
                "account_id":"codex:fixture","name":"Name","host":{"session":"fixture","socket":"/tmp/socket"},
                "space":{"kind":"existing","id":"w1"}},
            "directory":"/tmp","previous_name":"Name","conversation_id":conversation,"generation":"g",
            "phase":phase,"detail":"RESUME_ACK_UNCERTAIN","workspace_id":null,"tab_id":null,
            "pane_id":null,"operation_id":null,"created_at":now - age,"updated_at":now - age
        }))
        .unwrap();
        private_dir(&paths.root.join(crate::layout::RESUMPTIONS)).unwrap();
        fs::write(file(paths, &id).unwrap(), serde_json::to_vec(&r).unwrap()).unwrap();
        id
    }
    #[tokio::test]
    async fn abandon_releases_uncertain_reservation_without_sending_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("state"))).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = receipt_for(&p, &session, "conv-1", "uncertain", 0);
        assert!(
            crate::lifecycle::pending(&p, "conv-1", None)
                .unwrap()
                .is_some()
        );
        let e = |r: Result<Value>| r.unwrap_err().to_string();
        assert_eq!(e(abandon(&p, &id, "other").await), "CONFIRMATION_MISMATCH");
        let done = abandon(&p, &id, &id).await.unwrap();
        assert_eq!(done["phase"], "abandoned");
        assert_eq!(done["abandoned_from"], "uncertain");
        assert!(
            done["detail"]
                .as_str()
                .unwrap()
                .contains("nothing was sent")
        );
        assert!(
            crate::lifecycle::pending(&p, "conv-1", None)
                .unwrap()
                .is_none()
        );
        // Terminal and never revived by status().
        assert_eq!(status(&p, &id).await.unwrap().phase, "abandoned");
        assert_eq!(e(abandon(&p, &id, &id).await), "OPERATION_ALREADY_TERMINAL");
    }
    #[tokio::test]
    async fn abandon_refuses_fresh_in_flight_operations_and_active_workers() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("state"))).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let fresh = receipt_for(&p, &session, "conv-2", "dispatching", 0);
        assert_eq!(
            abandon(&p, &fresh, &fresh).await.unwrap_err().to_string(),
            "OPERATION_NOT_STUCK"
        );
        let stuck = receipt_for(&p, &session, "conv-2", "dispatching", 60);
        let lock = lock_file(&file(&p, &stuck).unwrap().with_extension("worker.lock")).unwrap();
        lock.try_lock_exclusive().unwrap();
        assert_eq!(
            abandon(&p, &stuck, &stuck).await.unwrap_err().to_string(),
            "OPERATION_WORKER_ACTIVE"
        );
        drop(lock);
        assert_eq!(
            abandon(&p, &stuck, &stuck).await.unwrap()["abandoned_from"],
            "dispatching"
        );
    }
    #[tokio::test]
    async fn abandon_refuses_while_the_session_process_is_live() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(Some(tmp.path().join("state"))).unwrap();
        let stat = fs::read_to_string("/proc/self/stat").unwrap();
        let start: u64 = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        let live = catalog::record_launch(
            &p,
            "fixture",
            "conv-3",
            "live",
            Some(catalog::Attachment {
                host: "h".into(),
                target: "t".into(),
                pid: std::process::id(),
                process_start: start,
            }),
        )
        .unwrap();
        let id = receipt_for(&p, &live.id, "conv-3", "uncertain", 0);
        assert_eq!(
            abandon(&p, &id, &id).await.unwrap_err().to_string(),
            "SESSION_PROCESS_LIVE"
        );
        assert_eq!(read(&p, &id).unwrap().phase, "uncertain");
        // A different recorded process start time means that process is gone.
        catalog::record_launch(
            &p,
            "fixture",
            "conv-3",
            "live",
            Some(catalog::Attachment {
                host: "h".into(),
                target: "t".into(),
                pid: std::process::id(),
                process_start: start + 1,
            }),
        )
        .unwrap();
        // With the attachment gone the live scan still runs and finds no provider.
        let r = abandon(&p, &id, &id).await;
        assert!(
            r.is_ok()
                || r.unwrap_err()
                    .to_string()
                    .contains("SESSION_STATE_UNVERIFIED")
        );
    }
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
        // Contract 1.52.0: pane_reused defaults to false and is omitted while false.
        assert!(!receipt.pane_reused);
        let mut stored = serde_json::to_value(&receipt).unwrap();
        assert!(stored.get("pane_reused").is_none());
        let mut reused = receipt.clone();
        reused.pane_reused = true;
        assert_eq!(serde_json::to_value(&reused).unwrap()["pane_reused"], true);
        stored["pane_reused"] = json!(true);
        assert!(
            serde_json::from_value::<Receipt>(stored)
                .unwrap()
                .pane_reused
        );
        assert_eq!(
            serde_json::to_value(&receipt.request).unwrap()["directory"],
            Value::Null
        );
    }
    #[test]
    fn request_directory_is_explicit_and_unknown_provider_is_rejected() {
        let mut value = json!({"version":1,"request_id":uuid::Uuid::new_v4(),"session_id":uuid::Uuid::new_v4(),"expected_generation":"g","account_id":"codex:fixture","name":"Name","host":{"session":"fixture","socket":"/tmp/socket"},"space":{"kind":"existing","id":"w1"}});
        assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_ok());
        for name in ["x".repeat(64), "界".repeat(64)] {
            value["name"] = json!(name);
            assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_ok());
        }
        for name in ["x".repeat(65), "界".repeat(65)] {
            value["name"] = json!(name);
            assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_err());
        }
        value["name"] = json!("Name");
        value["directory"] = json!("/different");
        assert!(serde_json::from_value::<Request>(value.clone()).is_err());
        value["directory"] = json!({"path":"/different","create":[],"confirmed_existing":true});
        assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_ok());
        value.as_object_mut().unwrap().remove("directory");
        // Contract 1.66.0: Claude resumes natively; Gemini is refused.
        value["account_id"] = json!("claude:fixture");
        assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_ok());
        value["account_id"] = json!("gemini:fixture");
        assert!(shape(&serde_json::from_value(value).unwrap()).is_err());
    }
}
