//! Sessionsation-hosted Claude sessions.
//!
//! A hosted session runs `claude -p --input-format stream-json --output-format
//! stream-json --replay-user-messages` under the account environment, owned by
//! one detached worker (`claude-host-worker <conversation>`). The worker is the
//! only writer of the provider's stdin. Its exclusive `worker.lock` is the
//! single-writer guarantee: while it is held (or the provider process it
//! started is still alive) no interactive resume, headless delivery or second
//! host may open the conversation.
//!
//! State lives in `DATA_ROOT/claude-hosted/<conversation>/`: `host.json` (the
//! journaled host record), `events.jsonl` (normalized events for the viewer)
//! and the worker lock. Requests reach the worker over a private control socket
//! under the runtime directory. A supervisor/client restart re-adopts a worker
//! that still holds its lock; a lost worker is marked stopped (its provider, if
//! still alive, is reported as draining: it has no stdin writer and exits at
//! end of input) and in-flight deliveries become uncertain, never replayed.
//!
//! Permission prompts arrive as `can_use_tool` control requests
//! (`--permission-prompt-tool stdio`) and stay pending until the operator
//! answers them (`api hosted respond`); nothing is auto-approved.
use super::{Enrolled, proc_start};
use crate::{
    catalog,
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::{ProviderId, claude},
};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, Read, Seek, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};

pub const SEMANTICS: &str = "hosted_stream_json";
/// Permission modes an operator may choose at launch. `account` passes no
/// flag (the account's normal mode) unless that mode bypasses permissions, in
/// which case `default` is used: bypass happens only when explicitly requested.
pub const PERMISSION_MODES: &[&str] = &[
    "account",
    "default",
    "acceptEdits",
    "plan",
    "bypassPermissions",
];
const MAX_EVENTS_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LINE: usize = 8 * 1024 * 1024;
const PREVIEW: usize = 4096;
const START_GRACE_SECONDS: i64 = 15;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Host {
    pub schema_version: u32,
    pub conversation_id: String,
    pub session_id: String,
    pub account: String,
    pub directory: String,
    /// new | resume
    pub origin: String,
    pub permission_mode: String,
    /// The `--permission-mode` actually passed; None = the account's normal mode.
    pub effective_permission_mode: Option<String>,
    pub epoch: u64,
    /// starting | running | stopping | stopped | failed
    pub state: String,
    pub reason: Option<String>,
    pub worker_pid: Option<u32>,
    pub worker_start: Option<u64>,
    pub child_pid: Option<u32>,
    pub child_start: Option<u64>,
    pub busy: bool,
    pub pending_permissions: Vec<Value>,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub stopped_at: Option<i64>,
}

fn base(paths: &Paths) -> PathBuf {
    paths.root.join("claude-hosted")
}
fn dir(paths: &Paths, conversation: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(conversation).context("INVALID_CONVERSATION_ID")?;
    Ok(base(paths).join(conversation))
}
fn host_file(paths: &Paths, c: &str) -> Result<PathBuf> {
    Ok(dir(paths, c)?.join("host.json"))
}
fn worker_lock(paths: &Paths, c: &str) -> Result<PathBuf> {
    Ok(dir(paths, c)?.join("worker.lock"))
}
pub fn events_file(paths: &Paths, c: &str) -> Result<PathBuf> {
    Ok(dir(paths, c)?.join("events.jsonl"))
}
pub fn socket(paths: &Paths, c: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(c).context("INVALID_CONVERSATION_ID")?;
    Ok(paths.runtime.join("hosted").join(format!("{c}.sock")))
}

pub fn read(paths: &Paths, c: &str) -> Result<Option<Host>> {
    let file = host_file(paths, c)?;
    match fs::read(&file) {
        Ok(bytes) => {
            let h: Host = serde_json::from_slice(&bytes)?;
            ensure!(h.schema_version == 1, "UNSUPPORTED_RECORD_VERSION");
            Ok(Some(h))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn save(paths: &Paths, h: &mut Host) -> Result<()> {
    h.updated_at = crate::daemon::now();
    atomic_write(
        &host_file(paths, &h.conversation_id)?,
        &serde_json::to_vec(h)?,
    )
}
/// Read-modify-write under the per-host mutation lock.
fn update<T>(paths: &Paths, c: &str, f: impl FnOnce(&mut Host) -> Result<T>) -> Result<T> {
    private_dir(&dir(paths, c)?)?;
    let lock = lock_file(&dir(paths, c)?.join("host.lock"))?;
    lock.lock_exclusive()?;
    let mut h = read(paths, c)?.context("HOST_NOT_FOUND")?;
    let out = f(&mut h)?;
    save(paths, &mut h)?;
    Ok(out)
}
pub fn worker_alive(paths: &Paths, c: &str) -> bool {
    worker_lock(paths, c)
        .ok()
        .is_some_and(|p| !crate::background::worker_stopped(&p).unwrap_or(false))
}
fn alive(pid: Option<u32>, start: Option<u64>) -> bool {
    match (pid, start) {
        (Some(pid), Some(start)) => {
            proc_start(pid).is_some_and(|(state, s)| s == start && !matches!(state, 'Z' | 'X'))
        }
        _ => false,
    }
}
fn terminal(state: &str) -> bool {
    matches!(state, "stopped" | "failed")
}

/// Positive reconciliation of a host record against its worker lock and its
/// provider process. Never starts or resends anything.
pub fn reconcile(paths: &Paths, c: &str) -> Result<Option<Host>> {
    let Some(h) = read(paths, c)? else {
        return Ok(None);
    };
    if terminal(&h.state) || worker_alive(paths, c) {
        return Ok(Some(h));
    }
    if h.state == "starting" && crate::daemon::now() - h.updated_at <= START_GRACE_SECONDS {
        return Ok(Some(h));
    }
    let epoch = h.epoch;
    let h = update(paths, c, |h| {
        if terminal(&h.state) || h.epoch != epoch {
            return Ok(h.clone());
        }
        h.pending_permissions.clear();
        h.busy = false;
        if alive(h.child_pid, h.child_start) {
            // No writer remains; the provider ends at end of input.
            h.state = "stopping".into();
            h.reason = Some("WORKER_LOST_PROVIDER_DRAINING".into());
        } else {
            h.state = "stopped".into();
            if h.reason.is_none() || h.reason.as_deref() == Some("WORKER_LOST_PROVIDER_DRAINING") {
                h.reason = Some("WORKER_LOST".into());
            }
            h.stopped_at = Some(crate::daemon::now());
        }
        Ok(h.clone())
    })?;
    crate::messaging::claude_steer::hosted_exited(paths, c, epoch, "HOST_WORKER_EXITED")?;
    Ok(Some(h))
}
/// A writer (worker or its provider process) may exist for this conversation.
pub fn live(paths: &Paths, c: &str) -> bool {
    reconcile(paths, c)
        .ok()
        .flatten()
        .is_some_and(|h| !terminal(&h.state))
}
/// The host accepts deliveries right now.
pub fn accepting(paths: &Paths, c: &str) -> Option<Host> {
    reconcile(paths, c)
        .ok()
        .flatten()
        .filter(|h| h.state == "running" && worker_alive(paths, c))
}
pub fn ensure_not_hosted(paths: &Paths, c: &str) -> Result<()> {
    if uuid::Uuid::parse_str(c).is_err() || !base(paths).join(c).is_dir() {
        return Ok(());
    }
    ensure!(
        !live(paths, c),
        "SESSION_ALREADY_RUNNING: a Sessionsation-hosted worker runs this conversation; take it over (`sessionsation take-over`) or stop it first"
    );
    Ok(())
}
/// Hosts of one account that may still write (block account removal/logout).
pub fn live_for_account(paths: &Paths, account: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(base(paths)) else {
        return vec![];
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|c| uuid::Uuid::parse_str(c).is_ok())
        .filter(|c| {
            read(paths, c)
                .ok()
                .flatten()
                .is_some_and(|h| h.account == account)
                && live(paths, c)
        })
        .collect()
}

pub fn public(paths: &Paths, h: &Host) -> Value {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "sessionsation".into());
    let root = paths.root.to_string_lossy().into_owned();
    json!({"session_id":h.session_id,"conversation_id":h.conversation_id,"account_id":format!("claude:{}",h.account),
        "delivery_semantics":SEMANTICS,"state":h.state,"reason":h.reason,"origin":h.origin,"directory":h.directory,
        "permission_mode":h.permission_mode,"effective_permission_mode":h.effective_permission_mode,
        "epoch":h.epoch,"busy":h.busy,"worker":{"pid":h.worker_pid,"alive":worker_alive(paths,&h.conversation_id)},
        "provider_process":{"pid":h.child_pid,"alive":alive(h.child_pid,h.child_start)},
        "pending_permissions":h.pending_permissions,
        "viewer_argv":[exe,"--data-dir",root,"view",h.conversation_id],
        "take_over_argv":[exe,"--data-dir",root,"take-over",h.session_id],
        "created_at":h.created_at,"started_at":h.started_at,"stopped_at":h.stopped_at,"updated_at":h.updated_at})
}

// ------------------------------------------------------------------- events

fn event(paths: &Paths, c: &str, mut v: Value) {
    let Ok(path) = events_file(paths, c) else {
        return;
    };
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_EVENTS_BYTES) {
        let _ = fs::rename(&path, path.with_extension("1.jsonl"));
    }
    v["at"] = json!(crate::daemon::now());
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{v}");
    }
}
fn preview(text: &str) -> String {
    let mut end = text.len().min(PREVIEW);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
pub(crate) fn text_blocks(content: &Value) -> Vec<String> {
    match content {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str().map(str::to_owned))
            .collect(),
        _ => vec![],
    }
}

/// One human-readable line per normalized event (viewer rendering).
pub fn render(v: &Value) -> Option<String> {
    let text = |k: &str| v[k].as_str().unwrap_or_default().to_owned();
    Some(match v["kind"].as_str()? {
        "host_started" => format!(
            "== hosted Claude session started (permission mode: {})",
            v["permission_mode"].as_str().unwrap_or("account")
        ),
        "host_stopped" => format!("== hosted worker stopped ({})", text("reason")),
        "user" => format!("> {}", text("text")),
        "assistant" => text("text"),
        "tool_use" => format!("[tool] {}", text("name")),
        "permission_request" => format!(
            "[permission pending] {} {} -- answer: sessionsation api hosted respond --session {} --request-id {} --allow|--deny",
            text("tool_name"),
            text("input_preview"),
            text("session_id"),
            text("request_id")
        ),
        "permission_answered" => {
            format!("[permission {}] {}", text("behavior"), text("request_id"))
        }
        "result" => format!(
            "-- turn {} ({})",
            if v["is_error"] == true {
                "failed"
            } else {
                "completed"
            },
            text("subtype")
        ),
        "interrupt" => "[interrupt requested]".into(),
        "lifecycle" => return None,
        "system" => format!("[system] {}", text("subtype")),
        _ => return None,
    })
}

// -------------------------------------------------------------------- start

fn resolve_mode(requested: Option<&str>, home: &Path) -> Result<(String, Option<String>)> {
    let requested = requested.unwrap_or("account");
    ensure!(
        PERMISSION_MODES.contains(&requested),
        "INVALID_PERMISSION_MODE: one of {}",
        PERMISSION_MODES.join(", ")
    );
    if requested != "account" {
        return Ok((requested.into(), Some(requested.into())));
    }
    let account_default = fs::read(home.join("settings.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["permissions"]["defaultMode"].as_str().map(str::to_owned));
    // Never bypass implicitly.
    let effective =
        (account_default.as_deref() == Some("bypassPermissions")).then(|| "default".to_owned());
    Ok(("account".into(), effective))
}

/// Provider argv after the binary.
pub fn worker_args(h: &Host) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--replay-user-messages",
        "--permission-prompt-tool",
        "stdio",
    ]
    .map(str::to_owned)
    .to_vec();
    if let Some(mode) = &h.effective_permission_mode {
        args.extend(["--permission-mode".into(), mode.clone()]);
    }
    let flag = if h.origin == "resume" {
        "--resume"
    } else {
        "--session-id"
    };
    args.extend([flag.into(), h.conversation_id.clone()]);
    args
}

pub struct Start {
    pub account: String,
    pub directory: Option<String>,
    pub name: Option<String>,
    /// Resume an existing stopped conversation (catalog id, generation).
    pub session: Option<(String, String)>,
    pub permission_mode: Option<String>,
    /// Admitted `api launch` request completed by this start.
    pub creation: Option<String>,
    pub attachment: Option<catalog::Attachment>,
}

fn account(paths: &Paths, name: &str) -> Result<Enrolled> {
    crate::messaging::headless::usable_account(paths, ProviderId::Claude, name)
}

/// Admit and start a hosted worker; waits briefly for the provider to run.
pub async fn start(paths: &Paths, s: Start) -> Result<Host> {
    let enrolled = account(paths, &s.account)?;
    let (mode, effective) = resolve_mode(s.permission_mode.as_deref(), &enrolled.home)?;
    let config = crate::store::load_config(paths)?;
    claude::prelaunch_gate(&config, &enrolled.home).await?;
    let (session, directory, origin, _lock) = match &s.session {
        Some((id, generation)) => {
            let session = catalog::resolve(paths, id, generation)?;
            ensure!(
                session.provider == ProviderId::Claude.as_str() && session.account == enrolled.name,
                "UNSUPPORTED_PROVIDER_OPERATION: hosting needs a Claude session of this account"
            );
            let lock = crate::external::conversation_lock(paths, &session.conversation_id)
                .context("SESSION_OPERATION_IN_PROGRESS")?;
            crate::lifecycle::ensure_available(paths, &session.conversation_id, None)?;
            crate::messaging::headless::ensure_idle(paths, &session.conversation_id)?;
            let observation = claude::observe(
                &enrolled.home,
                &session.conversation_id,
                session.attachment.as_ref(),
            );
            claude::launch::ensure_resumable(&session, &observation)?;
            let directory = claude::launch::resume_directory(&session, &observation)?;
            let session = if s.attachment.is_some() {
                catalog::move_attachment(paths, &session, s.attachment.clone())?
            } else {
                session
            };
            (session, directory, "resume", lock)
        }
        None => {
            let directory = s.directory.clone().context("INVALID_REQUEST: directory")?;
            crate::messaging::headless::usable_directory(&directory)?;
            let conversation = uuid::Uuid::new_v4().to_string();
            let lock = crate::external::conversation_lock(paths, &conversation)?;
            let session = match &s.creation {
                Some(request) => {
                    crate::launch::attached(paths, request, &conversation, s.attachment.clone())?
                }
                None => catalog::record_provider_launch(
                    paths,
                    ProviderId::Claude,
                    &enrolled.name,
                    &conversation,
                    s.name
                        .as_deref()
                        .or_else(|| Path::new(&directory).file_name().and_then(|n| n.to_str()))
                        .unwrap_or("claude"),
                    Some(&directory),
                    s.attachment.clone(),
                )?,
            };
            (session, directory, "new", lock)
        }
    };
    let c = session.conversation_id.clone();
    private_dir(&dir(paths, &c)?)?;
    ensure!(
        !live(paths, &c),
        "SESSION_ALREADY_RUNNING: a hosted worker already runs this conversation"
    );
    let now = crate::daemon::now();
    let epoch = read(paths, &c)?.map_or(0, |h| h.epoch) + 1;
    let mut host = Host {
        schema_version: 1,
        conversation_id: c.clone(),
        session_id: session.id.clone(),
        account: enrolled.name.clone(),
        directory,
        origin: origin.into(),
        permission_mode: mode,
        effective_permission_mode: effective,
        epoch,
        state: "starting".into(),
        reason: None,
        worker_pid: None,
        worker_start: None,
        child_pid: None,
        child_start: None,
        busy: false,
        pending_permissions: vec![],
        created_at: now,
        updated_at: now,
        started_at: None,
        stopped_at: None,
    };
    // Journal before any provider process can start.
    save(paths, &mut host)?;
    let spawned = crate::background::detach(&mut std::process::Command::new(
        crate::self_executable::path()?,
    ))
    .arg("--data-dir")
    .arg(&paths.root)
    .args(["claude-host-worker", &c])
    .spawn();
    if spawned.is_err() {
        update(paths, &c, |h| {
            h.state = "failed".into();
            h.reason = Some("WORKER_COULD_NOT_START_NO_PROVIDER_ACTION".into());
            Ok(())
        })?;
    }
    drop(_lock);
    for _ in 0..150 {
        let h = read(paths, &c)?.context("HOST_NOT_FOUND")?;
        if h.state != "starting" {
            return Ok(h);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    read(paths, &c)?.context("HOST_NOT_FOUND")
}

// ------------------------------------------------------------------- worker

enum Op {
    Deliver(String),
    Stop {
        timeout: u64,
        interrupt: bool,
    },
    Respond {
        request_id: String,
        allow: bool,
        message: Option<String>,
    },
    Interrupt,
}

fn kill_group(pid: u32, signal: i32) {
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

/// The detached owner of one hosted provider process.
pub async fn worker(paths: &Paths, c: &str) -> Result<()> {
    private_dir(&dir(paths, c)?)?;
    let lock = lock_file(&worker_lock(paths, c)?)?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    let Some(h) = read(paths, c)? else {
        return Ok(());
    };
    if h.state != "starting" {
        return Ok(());
    }
    let epoch = h.epoch;
    let me = std::process::id();
    let prepared = prepare(paths, &h).await;
    let home = match prepared {
        Ok(home) => home,
        Err(e) => {
            update(paths, c, |h| {
                h.state = "failed".into();
                h.reason = Some(format!("{e:#}").chars().take(300).collect());
                h.stopped_at = Some(crate::daemon::now());
                Ok(())
            })?;
            return Ok(());
        }
    };
    let sock = socket(paths, c)?;
    private_dir(sock.parent().context("socket parent")?)?;
    let _ = fs::remove_file(&sock);
    let listener = tokio::net::UnixListener::bind(&sock)?;
    let config = crate::store::load_config(paths)?;
    let mut std_command = claude::command(&config, &home);
    for name in [
        "HERDR_ENV",
        "HERDR_PANE_ID",
        "HERDR_SOCKET_PATH",
        "HERDR_SESSION",
        "TMUX",
        "TMUX_PANE",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
    ] {
        std_command.env_remove(name);
    }
    std::os::unix::process::CommandExt::process_group(&mut std_command, 0);
    let stderr = fs::File::create(dir(paths, c)?.join("provider-stderr.log"))
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut command = tokio::process::Command::from(std_command);
    command
        .args(worker_args(&h))
        .current_dir(&h.directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .kill_on_drop(false);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            update(paths, c, |h| {
                h.state = "failed".into();
                h.reason = Some("PROVIDER_COULD_NOT_START".into());
                h.stopped_at = Some(crate::daemon::now());
                Ok(())
            })?;
            let _ = fs::remove_file(&sock);
            return Ok(());
        }
    };
    let pid = child.id().context("provider pid")?;
    let start = proc_start(pid).map(|(_, s)| s);
    update(paths, c, |h| {
        h.state = "running".into();
        h.worker_pid = Some(me);
        h.worker_start = proc_start(me).map(|(_, s)| s);
        h.child_pid = Some(pid);
        h.child_start = start;
        h.started_at = Some(crate::daemon::now());
        Ok(())
    })?;
    event(
        paths,
        c,
        json!({"kind":"host_started","epoch":epoch,"permission_mode":h.permission_mode,"origin":h.origin}),
    );
    let (tx, mut rx) = mpsc::channel::<(Op, oneshot::Sender<Value>)>(16);
    tokio::spawn(accept(listener, tx));
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take().context("provider stdout")?;
    let mut lines = BufReader::new(stdout);
    let mut state = Turns::default();
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut killed = false;
    let mut buf = Vec::new();
    loop {
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            read = lines.read_until(b'\n', &mut buf) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if buf.len() <= MAX_LINE
                            && let Ok(v) = serde_json::from_slice::<Value>(&buf)
                            && let Some(reply) = handle(paths, c, &h, &v, &mut state)
                            && let Some(input) = stdin.as_mut()
                        {
                            let _ = write_frame(input, &reply).await;
                        }
                        buf.clear();
                    }
                }
            }
            Some((op, answer)) = rx.recv() => {
                let response = match op {
                    Op::Deliver(id) => deliver(paths, c, epoch, &id, stdin.as_mut(), &mut state).await,
                    Op::Interrupt => match stdin.as_mut() {
                        Some(input) => {
                            state.interrupted = true;
                            event(paths, c, json!({"kind":"interrupt"}));
                            let ok = write_frame(input, &interrupt_frame()).await.is_ok();
                            json!({"ok":ok})
                        }
                        None => json!({"ok":false,"error":"HOST_STOPPING"}),
                    },
                    Op::Respond { request_id, allow, message } => {
                        respond(paths, c, &h, stdin.as_mut(), &request_id, allow, message).await
                    }
                    Op::Stop { timeout, interrupt } => {
                        if interrupt && let Some(input) = stdin.as_mut() {
                            state.interrupted = true;
                            let _ = write_frame(input, &interrupt_frame()).await;
                        }
                        // Closing stdin ends the provider after its queued input.
                        if let Some(mut input) = stdin.take() {
                            let _ = input.shutdown().await;
                        }
                        let _ = update(paths, c, |h| { h.state = "stopping".into(); h.reason = Some("STOP_REQUESTED".into()); Ok(()) });
                        deadline = Some(tokio::time::Instant::now() + Duration::from_secs(timeout));
                        json!({"ok":true,"state":"stopping"})
                    }
                };
                let _ = answer.send(response);
            }
            _ = sleep => {
                if killed {
                    kill_group(pid, libc::SIGKILL);
                    deadline = None;
                } else {
                    kill_group(pid, libc::SIGTERM);
                    killed = true;
                    deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                }
            }
        }
    }
    drop(stdin);
    let status = match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(s) => s.ok(),
        Err(_) => {
            kill_group(pid, libc::SIGKILL);
            child.wait().await.ok()
        }
    };
    let reason = match status.and_then(|s| s.code()) {
        _ if killed => "PROVIDER_TERMINATED_AFTER_STOP_TIMEOUT".to_owned(),
        Some(0) => "PROVIDER_EXITED".to_owned(),
        Some(code) => format!("PROVIDER_EXITED_{code}"),
        None => "PROVIDER_KILLED".to_owned(),
    };
    crate::messaging::claude_steer::hosted_exited(paths, c, epoch, &reason)?;
    update(paths, c, |h| {
        if h.epoch == epoch {
            let requested = h.reason.as_deref() == Some("STOP_REQUESTED");
            h.state = "stopped".into();
            h.reason = Some(if requested && !killed {
                "STOPPED".into()
            } else {
                reason.clone()
            });
            h.busy = false;
            h.pending_permissions.clear();
            h.stopped_at = Some(crate::daemon::now());
        }
        Ok(())
    })?;
    event(paths, c, json!({"kind":"host_stopped","reason":reason}));
    let _ = fs::remove_file(&sock);
    drop(lock);
    Ok(())
}

/// Rechecks inside the worker before any provider process starts.
async fn prepare(paths: &Paths, h: &Host) -> Result<PathBuf> {
    let enrolled = account(paths, &h.account)?;
    let session = catalog::find(paths, &h.session_id)?;
    ensure!(
        session.conversation_id == h.conversation_id && session.account == h.account,
        "STALE_TARGET"
    );
    ensure!(
        !claude::live_sessions(&enrolled.home)?
            .iter()
            .any(|l| l.session_id == h.conversation_id),
        "SESSION_ALREADY_RUNNING: an interactive Claude process runs this conversation"
    );
    let transcript = claude::find_transcript(&enrolled.home, &h.conversation_id);
    if h.origin == "new" {
        ensure!(
            transcript.is_none(),
            "STALE_TARGET: the pre-assigned conversation already exists"
        );
    } else {
        ensure!(transcript.is_some(), "SESSION_STALE");
    }
    crate::messaging::headless::usable_directory(&h.directory)?;
    let config = crate::store::load_config(paths)?;
    claude::prelaunch_gate(&config, &enrolled.home).await?;
    Ok(enrolled.home)
}

async fn accept(
    listener: tokio::net::UnixListener,
    tx: mpsc::Sender<(Op, oneshot::Sender<Value>)>,
) {
    use tokio::io::AsyncReadExt;
    while let Ok((stream, _)) = listener.accept().await {
        let tx = tx.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            let mut reader = BufReader::new(read.take(64 * 1024));
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                return;
            };
            let text = |k: &str| v[k].as_str().map(str::to_owned);
            let op = match v["op"].as_str() {
                Some("deliver") => text("message_id").map(Op::Deliver),
                Some("interrupt") => Some(Op::Interrupt),
                Some("stop") => Some(Op::Stop {
                    timeout: v["timeout_seconds"].as_u64().unwrap_or(120).clamp(1, 3600),
                    interrupt: v["interrupt"] == true,
                }),
                Some("respond") => text("request_id").map(|request_id| Op::Respond {
                    request_id,
                    allow: v["behavior"] == "allow",
                    message: text("message"),
                }),
                Some("ping") => {
                    let _ = write.write_all(b"{\"ok\":true}\n").await;
                    return;
                }
                _ => None,
            };
            let Some(op) = op else {
                let _ = write
                    .write_all(b"{\"ok\":false,\"error\":\"INVALID_REQUEST\"}\n")
                    .await;
                return;
            };
            let (answer, response) = oneshot::channel();
            if tx.send((op, answer)).await.is_err() {
                return;
            }
            if let Ok(v) = response.await {
                let _ = write.write_all(format!("{v}\n").as_bytes()).await;
            }
        });
    }
}

async fn write_frame(input: &mut tokio::process::ChildStdin, v: &Value) -> std::io::Result<()> {
    input.write_all(format!("{v}\n").as_bytes()).await?;
    input.flush().await
}
fn interrupt_frame() -> Value {
    json!({"type":"control_request","request_id":uuid::Uuid::new_v4().to_string(),"request":{"subtype":"interrupt"}})
}

/// Per-worker turn bookkeeping: inputs whose turn has started and not finished.
#[derive(Default)]
pub(crate) struct Turns {
    pub active: Vec<String>,
    pub interrupted: bool,
}

async fn deliver(
    paths: &Paths,
    c: &str,
    epoch: u64,
    id: &str,
    stdin: Option<&mut tokio::process::ChildStdin>,
    state: &mut Turns,
) -> Value {
    use crate::messaging::claude_steer as steer;
    let Some(input) = stdin else {
        let _ = steer::abandon_unsent(paths, id, "HOST_STOPPING_NOT_SENT");
        return json!({"ok":false,"error":"HOST_STOPPING"});
    };
    let frames = match steer::hosted_begin(paths, id, c, epoch) {
        Ok(Some(frames)) => frames,
        Ok(None) => return json!({"ok":true,"dispatched":false}),
        Err(e) => return json!({"ok":false,"error":e.to_string()}),
    };
    let mut ok = true;
    for frame in &frames {
        if frame["type"] == "control_request" {
            // Only a running turn can end as interrupted.
            state.interrupted = !state.active.is_empty();
            event(paths, c, json!({"kind":"interrupt"}));
        }
        if write_frame(input, frame).await.is_err() {
            ok = false;
            break;
        }
    }
    let _ = steer::hosted_written(paths, id, ok);
    json!({"ok":ok,"dispatched":true})
}

async fn respond(
    paths: &Paths,
    c: &str,
    h: &Host,
    stdin: Option<&mut tokio::process::ChildStdin>,
    request_id: &str,
    allow: bool,
    message: Option<String>,
) -> Value {
    let pending = read(paths, c).ok().flatten().and_then(|h| {
        h.pending_permissions
            .into_iter()
            .find(|p| p["request_id"] == request_id)
    });
    let Some(pending) = pending else {
        return json!({"ok":false,"error":"PERMISSION_REQUEST_NOT_FOUND"});
    };
    let Some(input) = stdin else {
        return json!({"ok":false,"error":"HOST_STOPPING"});
    };
    let decision = if allow {
        json!({"behavior":"allow","updatedInput":pending["input"]})
    } else {
        json!({"behavior":"deny","message":message.unwrap_or_else(|| "Denied by the operator".into())})
    };
    let frame = json!({"type":"control_response","response":{"subtype":"success","request_id":request_id,"response":decision}});
    if write_frame(input, &frame).await.is_err() {
        return json!({"ok":false,"error":"STDIN_WRITE_FAILED"});
    }
    let _ = update(paths, c, |h| {
        h.pending_permissions
            .retain(|p| p["request_id"] != request_id);
        Ok(())
    });
    let behavior = if allow { "allow" } else { "deny" };
    event(
        paths,
        c,
        json!({"kind":"permission_answered","request_id":request_id,"behavior":behavior,"session_id":h.session_id}),
    );
    json!({"ok":true,"request_id":request_id,"behavior":behavior})
}

/// Handle one provider output event. Returns a frame to write back (an
/// error answer to an unsupported control request: none is left unanswered).
fn handle(paths: &Paths, c: &str, h: &Host, v: &Value, state: &mut Turns) -> Option<Value> {
    use crate::messaging::claude_steer as steer;
    match v["type"].as_str()? {
        "system" => {
            let subtype = v["subtype"].as_str().unwrap_or_default();
            if subtype == "init" && v["session_id"].as_str().is_some_and(|s| s != c) {
                event(
                    paths,
                    c,
                    json!({"kind":"system","subtype":"session_id_mismatch"}),
                );
            } else if matches!(subtype, "init" | "compact_boundary") {
                event(paths, c, json!({"kind":"system","subtype":subtype}));
            }
        }
        "command_lifecycle" => {
            let id = v["command_uuid"].as_str()?;
            let st = v["state"].as_str()?;
            event(
                paths,
                c,
                json!({"kind":"lifecycle","command_uuid":id,"state":st}),
            );
            if st == "started" {
                let _ = update(paths, c, |h| {
                    h.busy = true;
                    Ok(())
                });
            }
            let _ = steer::hosted_lifecycle(paths, c, id, st, state);
        }
        "user" => {
            let blocks = text_blocks(&v["message"]["content"]);
            if let Some(id) = v["uuid"].as_str() {
                let _ = steer::hosted_echo(paths, c, id);
            }
            if !blocks.is_empty() {
                event(
                    paths,
                    c,
                    json!({"kind":"user","text":preview(&blocks.join("\n"))}),
                );
            }
        }
        "assistant" => {
            let content = &v["message"]["content"];
            let text = text_blocks(content).join("");
            if !text.is_empty() {
                event(paths, c, json!({"kind":"assistant","text":preview(&text)}));
                let _ = steer::hosted_assistant(paths, c, &text, state);
            }
            for tool in content
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_use")
            {
                event(paths, c, json!({"kind":"tool_use","name":tool["name"]}));
            }
        }
        "result" => {
            event(
                paths,
                c,
                json!({"kind":"result","is_error":v["is_error"],"subtype":v["subtype"],"user_message_uuids":v["user_message_uuids"]}),
            );
            let _ = steer::hosted_result(paths, c, v, state);
            let queued = v["queued_turn_count"].as_u64().unwrap_or(0);
            let _ = update(paths, c, |h| {
                h.busy = queued > 0;
                Ok(())
            });
        }
        "control_request" => {
            let request_id = v["request_id"].as_str()?.to_owned();
            let request = &v["request"];
            if request["subtype"] == "can_use_tool" {
                let input_preview = preview(&request["input"].to_string());
                let item = json!({"request_id":request_id,"tool_name":request["tool_name"],"input":request["input"],
                    "input_preview":input_preview,"requested_at":crate::daemon::now()});
                let _ = update(paths, c, |h| {
                    h.pending_permissions
                        .retain(|p| p["request_id"] != request_id.as_str());
                    h.pending_permissions.push(item);
                    Ok(())
                });
                event(
                    paths,
                    c,
                    json!({"kind":"permission_request","request_id":request_id,"tool_name":request["tool_name"],
                        "input_preview":input_preview,"session_id":h.session_id}),
                );
            } else {
                return Some(
                    json!({"type":"control_response","response":{"subtype":"error","request_id":request_id,
                    "error":"unsupported control request for a Sessionsation-hosted session"}}),
                );
            }
        }
        _ => {}
    }
    None
}

// ------------------------------------------------------------------ control

/// One request to the running worker over its private control socket.
pub async fn call(paths: &Paths, c: &str, request: Value, timeout: Duration) -> Result<Value> {
    use tokio::io::AsyncReadExt;
    let sock = socket(paths, c)?;
    let run = async {
        let mut stream = tokio::net::UnixStream::connect(&sock)
            .await
            .context("HOST_UNREACHABLE")?;
        stream.write_all(format!("{request}\n").as_bytes()).await?;
        let mut bytes = vec![];
        let mut reader = BufReader::new((&mut stream).take(1024 * 1024));
        reader.read_until(b'\n', &mut bytes).await?;
        let v: Value = serde_json::from_slice(&bytes).context("HOST_PROTOCOL")?;
        anyhow::Ok(v)
    };
    tokio::time::timeout(timeout, run)
        .await
        .context("HOST_RESPONSE_TIMEOUT")?
}

/// Stop a hosted worker cleanly (stdin closed after optional interrupt); waits
/// for it to exit. A lost worker's still-running provider is terminated.
pub async fn stop(paths: &Paths, c: &str, timeout: u64, interrupt: bool) -> Result<Host> {
    let h = reconcile(paths, c)?.context("HOST_NOT_FOUND: this session is not hosted")?;
    if terminal(&h.state) {
        return Ok(h);
    }
    if worker_alive(paths, c) {
        let v = call(
            paths,
            c,
            json!({"op":"stop","timeout_seconds":timeout,"interrupt":interrupt}),
            Duration::from_secs(10),
        )
        .await?;
        ensure!(v["ok"] == true, "HOST_STOP_REFUSED");
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout + 20);
        while worker_alive(paths, c) {
            ensure!(
                std::time::Instant::now() < deadline,
                "HOST_STOP_UNCONFIRMED: the worker has not exited yet; read status again"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    } else if let (Some(pid), true) = (h.child_pid, alive(h.child_pid, h.child_start)) {
        // Orphaned provider without a writer: terminate its group.
        kill_group(pid, libc::SIGTERM);
        for _ in 0..50 {
            if !alive(h.child_pid, h.child_start) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if alive(h.child_pid, h.child_start) {
            kill_group(pid, libc::SIGKILL);
        }
    }
    reconcile(paths, c)?.context("HOST_NOT_FOUND")
}

// --------------------------------------------------------------------- CLI

/// Resolve a catalog session id or a hosted conversation id to (session, conversation).
pub fn target(paths: &Paths, id: &str) -> Result<(String, String)> {
    if let Ok(s) = catalog::find(paths, id) {
        return Ok((s.id, s.conversation_id));
    }
    let h = read(paths, id)?.context("SESSION_NOT_FOUND")?;
    Ok((h.session_id, h.conversation_id))
}

#[derive(Subcommand)]
pub enum Command {
    /// Start a hosted worker: a new conversation (--account, --directory) or a
    /// stopped one (--session, --expected-generation).
    Start {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        directory: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, conflicts_with_all = ["account", "directory", "name"], requires = "expected_generation")]
        session: Option<String>,
        #[arg(long)]
        expected_generation: Option<String>,
        /// account (default) | default | acceptEdits | plan | bypassPermissions
        #[arg(long)]
        permission_mode: Option<String>,
    },
    /// Host record, liveness and pending permission prompts.
    Status {
        #[arg(long)]
        session: String,
    },
    /// Stop the worker cleanly (close input; optionally interrupt the turn first).
    Stop {
        #[arg(long)]
        session: String,
        #[arg(long)]
        expected_generation: String,
        #[arg(long)]
        interrupt: bool,
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout_seconds: u64,
    },
    /// Answer a pending permission prompt.
    Respond {
        #[arg(long)]
        session: String,
        #[arg(long)]
        request_id: String,
        #[arg(long, conflicts_with = "deny", required_unless_present = "deny")]
        allow: bool,
        #[arg(long)]
        deny: bool,
        #[arg(long)]
        message: Option<String>,
    },
    /// Normalized events (newest last), for clients without a viewer pane.
    Events {
        #[arg(long)]
        session: String,
        #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u64).range(1..=5000))]
        limit: u64,
    },
}

pub async fn api(paths: &Paths, command: Command) -> Result<Value> {
    match command {
        Command::Start {
            account,
            directory,
            name,
            session,
            expected_generation,
            permission_mode,
        } => {
            let request = match session {
                Some(id) => {
                    let s = catalog::find(paths, &id)?;
                    Start {
                        account: s.account.clone(),
                        directory: None,
                        name: None,
                        session: Some((id, expected_generation.unwrap_or_default())),
                        permission_mode,
                        creation: None,
                        attachment: None,
                    }
                }
                None => {
                    let account = crate::provider::AccountRef::parse(
                        &account.context("INVALID_REQUEST: --account or --session")?,
                    )?;
                    ensure!(
                        account.provider == ProviderId::Claude,
                        "UNSUPPORTED_PROVIDER_OPERATION: hosted sessions are Claude-only"
                    );
                    if let Some(name) = &name {
                        crate::launch::validate_session_name(name)?;
                    }
                    Start {
                        account: account.name,
                        directory,
                        name,
                        session: None,
                        permission_mode,
                        creation: None,
                        attachment: None,
                    }
                }
            };
            let h = start(paths, request).await?;
            Ok(public(paths, &h))
        }
        Command::Status { session } => {
            let (_, c) = target(paths, &session)?;
            let h = reconcile(paths, &c)?.context("HOST_NOT_FOUND: this session is not hosted")?;
            Ok(public(paths, &h))
        }
        Command::Stop {
            session,
            expected_generation,
            interrupt,
            timeout_seconds,
        } => {
            let s = catalog::resolve(paths, &session, &expected_generation)?;
            let h = stop(paths, &s.conversation_id, timeout_seconds, interrupt).await?;
            Ok(public(paths, &h))
        }
        Command::Respond {
            session,
            request_id,
            allow,
            deny: _,
            message,
        } => {
            let (_, c) = target(paths, &session)?;
            ensure!(worker_alive(paths, &c), "HOST_NOT_RUNNING");
            let v = call(
                paths,
                &c,
                json!({"op":"respond","request_id":request_id,"behavior":if allow {"allow"} else {"deny"},"message":message}),
                Duration::from_secs(10),
            )
            .await?;
            ensure!(
                v["ok"] == true,
                "{}",
                v["error"].as_str().unwrap_or("HOST_PROTOCOL")
            );
            Ok(v)
        }
        Command::Events { session, limit } => {
            let (session, c) = target(paths, &session)?;
            let events = tail(&events_file(paths, &c)?, limit as usize)?;
            Ok(json!({"session_id":session,"conversation_id":c,"events":events}))
        }
    }
}

fn tail(path: &Path, limit: usize) -> Result<Vec<Value>> {
    let Ok(file) = fs::File::open(path) else {
        return Ok(vec![]);
    };
    let mut out: std::collections::VecDeque<Value> = Default::default();
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if out.len() == limit {
                out.pop_front();
            }
            out.push_back(v);
        }
    }
    Ok(out.into())
}

// ------------------------------------------------------- terminal commands

/// `run --hosted`: start (or host a stopped conversation) and show the viewer
/// in this terminal.
pub async fn run_in_terminal(
    paths: &Paths,
    account: &str,
    thread: Option<&str>,
    permission_mode: Option<String>,
    creation: Option<&str>,
) -> Result<()> {
    let attachment = crate::external::current_attachment(paths)?;
    let session = match thread {
        Some(thread) => {
            let s = catalog::list(paths)?
                .into_iter()
                .find(|s| {
                    s.provider == "claude" && s.conversation_id == thread && s.account == account
                })
                .context(
                    "SESSION_NOT_FOUND: no cataloged Claude session with this id on the account",
                )?;
            Some((s.id.clone(), s.generation()))
        }
        None => None,
    };
    let directory = std::env::current_dir()?
        .to_str()
        .context("working directory is not UTF-8")?
        .to_owned();
    let h = start(
        paths,
        Start {
            account: account.into(),
            directory: Some(directory),
            name: None,
            session,
            permission_mode,
            creation: creation.map(str::to_owned),
            attachment,
        },
    )
    .await?;
    ensure!(
        h.state != "failed",
        "PROVIDER_LAUNCH_FAILED: {}",
        h.reason.as_deref().unwrap_or("hosted worker failed")
    );
    exec_viewer(paths, &h.conversation_id)
}

fn exec_viewer(paths: &Paths, c: &str) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(std::env::current_exe()?)
        .arg("--data-dir")
        .arg(&paths.root)
        .args(["view", c])
        .exec();
    Err(error).context("could not start the hosted viewer")
}

/// Render a hosted session's event log; follow it live unless `once`.
/// In a terminal, typing `t` + Enter takes the session over, `q` quits.
pub async fn view(paths: &Paths, id: &str, once: bool) -> Result<()> {
    let (session, c) = target(paths, id)?;
    let path = events_file(paths, &c)?;
    let mut offset = 0u64;
    let mut out = std::io::stdout();
    let mut print_new = |offset: &mut u64| -> Result<()> {
        let Ok(mut file) = fs::File::open(&path) else {
            return Ok(());
        };
        if file.metadata()?.len() < *offset {
            *offset = 0;
        }
        file.seek(std::io::SeekFrom::Start(*offset))?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        let complete = text.rfind('\n').map_or(0, |i| i + 1);
        for line in text[..complete].lines() {
            if let Some(line) = serde_json::from_str::<Value>(line)
                .ok()
                .as_ref()
                .and_then(render)
            {
                writeln!(out, "{line}")?;
            }
        }
        out.flush()?;
        *offset += complete as u64;
        Ok(())
    };
    print_new(&mut offset)?;
    if once {
        return Ok(());
    }
    let (keys, mut input) = mpsc::unbounded_channel::<String>();
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        println!(
            "-- viewer of hosted session {session}: type t + Enter to take over, q + Enter to quit"
        );
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                if keys.send(line.trim().to_owned()).is_err() {
                    break;
                }
            }
        });
    } else {
        drop(keys);
    }
    let mut stopped_seen = false;
    loop {
        tokio::select! {
            key = input.recv() => match key.as_deref() {
                Some("q") => return Ok(()),
                Some("t") => return take_over(paths, &session).await,
                _ => {}
            },
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                print_new(&mut offset)?;
                let stopped = reconcile(paths, &c)?.is_none_or(|h| terminal(&h.state));
                if stopped && !stopped_seen {
                    println!("-- hosted worker is not running; t + Enter resumes the conversation interactively here");
                    stopped_seen = true;
                }
            }
        }
    }
}

/// Stop the hosted worker cleanly, then resume the conversation interactively
/// in this terminal. Never runs while the worker or its provider is alive.
pub async fn take_over(paths: &Paths, id: &str) -> Result<()> {
    let (session, c) = target(paths, id)?;
    if read(paths, &c)?.is_some() {
        let h = stop(paths, &c, 300, false).await?;
        ensure!(
            terminal(&h.state),
            "HOST_STOP_UNCONFIRMED: the hosted provider is still running; try again"
        );
    }
    let s = catalog::find(paths, &session)?;
    // This viewer may be the recorded terminal; the resumed client records it again.
    let s = match &s.attachment {
        Some(a) if a.pid == std::process::id() => catalog::move_attachment(paths, &s, None)?,
        _ => s,
    };
    claude::launch::exec_resume(paths, &s.id, &s.generation()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn host(origin: &str, mode: Option<&str>) -> Host {
        Host {
            schema_version: 1,
            conversation_id: "11111111-1111-4111-8111-111111111111".into(),
            session_id: "s".into(),
            account: "a".into(),
            directory: "/".into(),
            origin: origin.into(),
            permission_mode: "account".into(),
            effective_permission_mode: mode.map(str::to_owned),
            epoch: 1,
            state: "starting".into(),
            reason: None,
            worker_pid: None,
            worker_start: None,
            child_pid: None,
            child_start: None,
            busy: false,
            pending_permissions: vec![],
            created_at: 0,
            updated_at: 0,
            started_at: None,
            stopped_at: None,
        }
    }
    #[test]
    fn worker_argv_is_stream_json_with_a_prompt_host_and_never_implicit_bypass() {
        let a = worker_args(&host("new", None));
        for flag in [
            "--input-format",
            "--output-format",
            "--replay-user-messages",
            "--verbose",
        ] {
            assert!(a.iter().any(|x| x == flag), "{a:?}");
        }
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--permission-prompt-tool" && w[1] == "stdio")
        );
        assert!(a.windows(2).any(|w| w[0] == "--session-id"));
        assert!(!a.iter().any(|x| x == "--permission-mode"
            || x.contains("bypass")
            || x.contains("dangerously")));
        let r = worker_args(&host("resume", Some("plan")));
        assert!(r.windows(2).any(|w| w[0] == "--resume"));
        assert!(
            r.windows(2)
                .any(|w| w[0] == "--permission-mode" && w[1] == "plan")
        );
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_mode(None, d.path()).unwrap(),
            ("account".into(), None)
        );
        fs::write(
            d.path().join("settings.json"),
            r#"{"permissions":{"defaultMode":"bypassPermissions"}}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_mode(None, d.path()).unwrap(),
            ("account".into(), Some("default".into())),
            "an account bypass default is never inherited silently"
        );
        assert_eq!(
            resolve_mode(Some("bypassPermissions"), d.path())
                .unwrap()
                .1
                .as_deref(),
            Some("bypassPermissions")
        );
        assert!(resolve_mode(Some("yolo"), d.path()).is_err());
    }
    #[test]
    fn rendering_shows_prompts_and_replies_but_not_lifecycle_noise() {
        assert_eq!(
            render(&json!({"kind":"assistant","text":"hi"})).unwrap(),
            "hi"
        );
        assert!(render(&json!({"kind":"lifecycle"})).is_none());
        let p = render(&json!({"kind":"permission_request","tool_name":"Bash","input_preview":"{}","session_id":"S","request_id":"R"})).unwrap();
        assert!(p.contains("--request-id R") && p.contains("pending"));
    }
}
