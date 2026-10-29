//! Claude and Gemini headless delivery through the public `api send`/`api receipt`
//! contract.
//!
//! `mode:"resume"` continues a STOPPED native session (Claude `claude -p --resume
//! ID --output-format json`; Gemini `gemini -r ID -p "" -o json`, message on stdin);
//! `mode:"new"` starts a conversation with a pre-assigned `--session-id` whose
//! catalog entry is recorded first. Both run in
//! a detached worker under the account environment. The request is journaled
//! before any provider process starts, the outcome is a durable receipt, and an
//! uncertain outcome (timeout, crash, unparseable output, session-id mismatch) is
//! never replayed. Live sessions are never addressed: Claude has no supported
//! input API for a running interactive session. Records of both providers share
//! `DATA_ROOT/claude-deliveries/` (the directory keeps its original name so
//! existing receipts stay readable); each record names its provider.
use crate::{
    catalog,
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::{AccountRef, ProviderId, claude, native},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};

pub const DEFAULT_TIMEOUT_SECONDS: u64 = 600;
pub const MAX_TIMEOUT_SECONDS: u64 = 3600;
const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REPLY_TEXT: usize = 64 * 1024;
pub const SEMANTICS: &str = "headless_print";
/// Gemini approval mode for headless turns: never `yolo`; tools that need
/// confirmation are not run unattended.
pub const GEMINI_APPROVAL_MODE: &str = "default";

fn claude_provider() -> ProviderId {
    ProviderId::Claude
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Resume,
    New,
}

/// One versioned headless request. `resume` names a catalog session and its
/// generation; `new` names an account and an absolute working directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub api_version: u32,
    pub message_id: String,
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub text: String,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// Codex recovery intents do not apply; accepted only as absent/null.
    #[serde(default, skip_serializing)]
    pub recovery: Option<Value>,
    #[serde(default, skip_serializing)]
    pub steering: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub schema_version: u32,
    pub request: Request,
    /// Absent in records written before Gemini existed: those are Claude.
    #[serde(default = "claude_provider")]
    pub provider: ProviderId,
    pub session_id: String,
    pub conversation_id: String,
    pub account: String,
    pub directory: String,
    pub accepted_generation: String,
    pub timeout_seconds: u64,
    /// admitted -> executing -> complete
    pub phase: String,
    pub state: String,
    pub reason: Option<String>,
    pub reply: Option<Value>,
    pub reported_session_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

fn dir(paths: &Paths) -> PathBuf {
    paths.root.join("claude-deliveries")
}
fn file(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_REQUEST")?;
    Ok(dir(paths).join(format!("{id}.json")))
}
fn worker_lock(paths: &Paths, id: &str) -> Result<PathBuf> {
    Ok(file(paths, id)?.with_extension("worker.lock"))
}
pub fn exists(paths: &Paths, id: &str) -> bool {
    file(paths, id).is_ok_and(|p| p.is_file())
}
fn read(paths: &Paths, id: &str) -> Result<Record> {
    let r: Record = serde_json::from_slice(&fs::read(file(paths, id)?)?)?;
    ensure!(r.schema_version == 1, "UNSUPPORTED_RECORD_VERSION");
    Ok(r)
}
fn save(paths: &Paths, r: &mut Record) -> Result<()> {
    r.updated_at = crate::daemon::now();
    atomic_write(
        &file(paths, &r.request.message_id)?,
        &serde_json::to_vec(r)?,
    )
}
fn all(paths: &Paths) -> Result<Vec<Record>> {
    let mut out = vec![];
    let Ok(entries) = fs::read_dir(dir(paths)) else {
        return Ok(out);
    };
    for e in entries {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "json") {
            out.push(serde_json::from_slice(&fs::read(&p)?)?);
        }
    }
    Ok(out)
}

/// The durable record as readers see it: an admitted request whose worker
/// stopped before execution was provably never sent; one whose worker stopped
/// while executing has an unknown outcome. Projection only; never replays.
fn project(paths: &Paths, mut r: Record) -> Result<Record> {
    if r.phase == "complete" {
        return Ok(r);
    }
    let stopped = crate::background::worker_stopped(&worker_lock(paths, &r.request.message_id)?)?;
    if !stopped {
        return Ok(r);
    }
    // The worker may have recorded its outcome and exited after the first read.
    if let Ok(current) = read(paths, &r.request.message_id)
        && current.phase != r.phase
    {
        return project(paths, current);
    }
    match r.phase.as_str() {
        "admitted" if crate::daemon::now() - r.updated_at > 10 => {
            r.state = "rejected".into();
            r.reason = Some("WORKER_EXITED_BEFORE_EXECUTION".into());
        }
        "executing" => {
            r.state = "delivery_unknown".into();
            r.reason = Some("WORKER_EXITED_DURING_EXECUTION".into());
        }
        _ => {}
    }
    Ok(r)
}
fn active(paths: &Paths, r: &Record) -> Result<bool> {
    Ok(matches!(
        project(paths, r.clone())?.state.as_str(),
        "pending_local" | "execution_started"
    ))
}
/// Unfinished headless deliveries of one account (blocks removal/deletion).
pub fn active_for_account(paths: &Paths, account: &str) -> Result<Vec<String>> {
    let mut ids = vec![];
    for r in all(paths)? {
        if r.account == account && active(paths, &r)? {
            ids.push(r.request.message_id.clone());
        }
    }
    Ok(ids)
}
/// Refuse interactive resume while a headless delivery may run this conversation.
pub fn ensure_idle(paths: &Paths, conversation: &str) -> Result<()> {
    crate::provider::claude::hosted::ensure_not_hosted(paths, conversation)?;
    for r in all(paths)? {
        ensure!(
            r.conversation_id != conversation || !active(paths, &r)?,
            "SESSION_OPERATION_IN_PROGRESS: a headless delivery is using this session"
        );
    }
    Ok(())
}

pub fn receipt_value(r: &Record) -> Value {
    let started = matches!(
        r.state.as_str(),
        "execution_started" | "completed_with_reply" | "failed" | "delivery_unknown"
    ) && r.started_at.is_some();
    let matched =
        r.reply.is_some() && r.reported_session_id.as_deref() == Some(r.conversation_id.as_str());
    let correlated = matched && matches!(r.state.as_str(), "completed_with_reply" | "failed");
    json!({"message_id":r.request.message_id,"session_id":r.session_id,"accepted_generation":r.accepted_generation,
        "provider":r.provider,"provider_conversation_id":r.conversation_id,"account_id":format!("{}:{}",r.provider,r.account),
        "mode":r.request.mode,"delivery_semantics":SEMANTICS,"state":r.state,"reason":r.reason,
        "facts":{"accepted_local":true,"accepted_native":started,"input_recorded":correlated,
            "input_consumed":if correlated {json!(true)} else {json!("unknown")},"reply_observed":correlated},
        "correlation":if r.reply.is_some() {json!({"session_id":r.conversation_id,"reported_session_id":r.reported_session_id,"matched":matched})} else {Value::Null},
        "reply":if correlated {r.reply.clone().unwrap_or(Value::Null)} else {Value::Null},
        "execution":{"phase":r.phase,"directory":r.directory,"timeout_seconds":r.timeout_seconds,
            "created_at":r.created_at,"started_at":r.started_at,"finished_at":r.finished_at,
            "new_conversation":r.request.mode == Mode::New,"automatic_replay":false},
        "observation":{"state":"cached","checked_at":crate::daemon::now(),"native_attempted_at":r.started_at,
            "evidence_observed_at":r.finished_at,"freshness":"unknown","error_code":null}})
}

pub fn receipt(paths: &Paths, id: &str, cancelling: bool) -> Result<Value> {
    let r = project(paths, read(paths, id)?)?;
    ensure!(
        !cancelling || !matches!(r.state.as_str(), "pending_local" | "execution_started"),
        "HEADLESS_CANCELLATION_UNSUPPORTED: a headless delivery cannot be cancelled once admitted; read its receipt"
    );
    Ok(receipt_value(&r))
}

/// The enrolled, selectable account behind a headless request.
pub(crate) fn usable_account(
    paths: &Paths,
    provider: ProviderId,
    name: &str,
) -> Result<claude::Enrolled> {
    let account = native::account(paths, provider, name)?;
    crate::accounts::ensure_available(paths, name)?;
    let state = crate::store::load_state_snapshot(paths)?;
    let record = state.accounts.get(name).context("ACCOUNT_NOT_FOUND")?;
    ensure!(record.deletion_id.is_none(), "ACCOUNT_DELETION_IN_PROGRESS");
    ensure!(account.enabled, "ACCOUNT_UNAVAILABLE: account is disabled");
    ensure!(
        account.home.is_dir(),
        "ACCOUNT_HOME_UNAVAILABLE: the {} home does not exist",
        native::title(provider)
    );
    Ok(account)
}
pub(crate) fn usable_directory(directory: &str) -> Result<()> {
    let p = Path::new(directory);
    ensure!(
        p.is_absolute() && p.is_dir(),
        "SAVED_DIRECTORY_UNAVAILABLE: the working directory must be an existing absolute directory"
    );
    Ok(())
}
/// Resume preconditions against the current catalog entry and native state.
fn resumable(
    paths: &Paths,
    provider: ProviderId,
    session: &catalog::Session,
    home: &Path,
) -> Result<String> {
    crate::lifecycle::ensure_available(paths, &session.conversation_id, None)?;
    ensure_idle(paths, &session.conversation_id)?;
    let observation = native::observe(
        provider,
        home,
        &session.conversation_id,
        session.attachment.as_ref(),
    );
    native::ensure_resumable(session, &observation)?;
    let directory = native::resume_directory(session, &observation)?;
    Ok(directory)
}

fn shape(request: &Request) -> Result<u64> {
    ensure!(
        request.api_version == crate::api::VERSION,
        "UNSUPPORTED_API_VERSION"
    );
    uuid::Uuid::parse_str(&request.message_id).context("INVALID_REQUEST")?;
    ensure!(
        !request.text.is_empty() && request.text.len() <= 16384,
        "INVALID_REQUEST"
    );
    ensure!(
        request.recovery.as_ref().is_none_or(Value::is_null),
        "UNSUPPORTED_PROVIDER_OPERATION: resume is headless for this provider; omit the Codex recovery intent"
    );
    ensure!(
        request.steering.as_ref().is_none_or(Value::is_null),
        "STEERING_ONLY_VALID_WITH_STEER"
    );
    let timeout = request.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    ensure!(
        (1..=MAX_TIMEOUT_SECONDS).contains(&timeout),
        "INVALID_TIMEOUT: timeout_seconds must be 1..=3600"
    );
    match request.mode {
        Mode::Resume => ensure!(
            request.session_id.is_some()
                && request.expected_generation.is_some()
                && request.account_id.is_none()
                && request.directory.is_none()
                && request.name.is_none(),
            "INVALID_REQUEST: mode resume takes session_id and expected_generation only"
        ),
        Mode::New => ensure!(
            request.session_id.is_none()
                && request.expected_generation.is_none()
                && request.account_id.is_some()
                && request.directory.is_some(),
            "INVALID_REQUEST: mode new takes account_id and directory (optional name)"
        ),
    }
    Ok(timeout)
}

/// Codex delivery stores share the message-id namespace.
pub(crate) fn ensure_unused_elsewhere(paths: &Paths, id: &str) -> Result<()> {
    ensure!(!super::recovery::exists(paths, id), "MESSAGE_ID_CONFLICT");
    ensure!(
        !super::claude_steer::exists(paths, id),
        "MESSAGE_ID_CONFLICT"
    );
    let ledger = super::Ledger::open(paths).map_err(anyhow::Error::from)?;
    super::public::initialize(&ledger).map_err(anyhow::Error::from)?;
    ensure!(
        super::public::get(&ledger, id)
            .map_err(anyhow::Error::from)?
            .is_none()
            && ledger.get(id).map_err(anyhow::Error::from)?.is_none(),
        "MESSAGE_ID_CONFLICT"
    );
    Ok(())
}

pub async fn send(paths: &Paths, request: Request) -> Result<Value> {
    let timeout = shape(&request)?;
    let id = request.message_id.clone();
    private_dir(&dir(paths))?;
    let admission = lock_file(&dir(paths).join("admission.lock"))?;
    admission.lock_exclusive()?;
    if exists(paths, &id) {
        let old = read(paths, &id)?;
        ensure!(old.request == request, "MESSAGE_ID_CONFLICT");
        drop(admission);
        return receipt(paths, &id, false);
    }
    ensure_unused_elsewhere(paths, &id)?;
    let now = crate::daemon::now();
    ensure!(
        request.expires_at > now && request.expires_at <= now + 86400,
        "INVALID_EXPIRY"
    );
    let count = all(paths)?.len();
    ensure!(count < 10000, "RECEIPT_CAPACITY_REACHED");
    let config = crate::store::load_config(paths)?;
    let (provider, session, account, directory) = match request.mode {
        Mode::Resume => {
            let session = catalog::resolve(
                paths,
                request.session_id.as_deref().unwrap_or_default(),
                request.expected_generation.as_deref().unwrap_or_default(),
            )?;
            let provider =
                native::of(&session.provider).context("UNSUPPORTED_PROVIDER_OPERATION")?;
            let account = usable_account(paths, provider, &session.account)?;
            let directory = resumable(paths, provider, &session, &account.home)?;
            native::prelaunch_gate(provider, &config, &account.home).await?;
            (provider, session, account, directory)
        }
        Mode::New => {
            let target = AccountRef::parse(request.account_id.as_deref().unwrap_or_default())?;
            let provider = native::require(target.provider)?;
            let name = target.name;
            let account = usable_account(paths, provider, &name)?;
            let directory = request.directory.clone().unwrap_or_default();
            usable_directory(&directory)?;
            native::prelaunch_gate(provider, &config, &account.home).await?;
            let label = request
                .name
                .clone()
                .or_else(|| {
                    Path::new(&directory)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| provider.as_str().into());
            // The native id is chosen here and recorded before any execution.
            let conversation = uuid::Uuid::new_v4().to_string();
            let session = catalog::record_provider_launch(
                paths,
                provider,
                &name,
                &conversation,
                &label,
                Some(&directory),
                None,
            )?;
            (provider, session, account, directory)
        }
    };
    let mut record = Record {
        schema_version: 1,
        provider,
        session_id: session.id.clone(),
        conversation_id: session.conversation_id.clone(),
        account: account.name.clone(),
        directory,
        accepted_generation: session.generation(),
        timeout_seconds: timeout,
        phase: "admitted".into(),
        state: "pending_local".into(),
        reason: None,
        reply: None,
        reported_session_id: None,
        created_at: now,
        updated_at: now,
        started_at: None,
        finished_at: None,
        request,
    };
    // Journal before any provider process can start.
    save(paths, &mut record)?;
    drop(admission);
    let spawned = crate::background::detach(&mut std::process::Command::new(
        crate::self_executable::path()?,
    ))
    .arg("--data-dir")
    .arg(&paths.root)
    .args(["claude-delivery-worker", &id])
    .spawn();
    if spawned.is_err() {
        record.phase = "complete".into();
        record.state = "rejected".into();
        record.reason = Some("WORKER_COULD_NOT_START_NO_PROVIDER_ACTION".into());
        save(paths, &mut record)?;
    }
    receipt(paths, &id, false)
}

fn finish(paths: &Paths, r: &mut Record, state: &str, reason: Option<String>) -> Result<()> {
    r.phase = "complete".into();
    r.state = state.into();
    r.reason = reason;
    r.finished_at = Some(crate::daemon::now());
    save(paths, r)
}
fn code(e: &anyhow::Error) -> String {
    e.to_string()
        .split(':')
        .next()
        .unwrap_or("REJECTED")
        .to_owned()
}

/// Detached worker: recheck, journal `executing`, run once, record the outcome.
pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    let lock = lock_file(&worker_lock(paths, id)?)?;
    lock.try_lock_exclusive()
        .context("HEADLESS_WORKER_RUNNING")?;
    let mut r = read(paths, id)?;
    if r.phase != "admitted" {
        return Ok(());
    }
    if r.request.expires_at <= crate::daemon::now() {
        return finish(
            paths,
            &mut r,
            "expired",
            Some("EXPIRED_BEFORE_EXECUTION".into()),
        );
    }
    let prepared = prepare(paths, &r).await;
    let (home, _conversation_lock) = match prepared {
        Ok(v) => v,
        Err(e) => return finish(paths, &mut r, "rejected", Some(code(&e))),
    };
    r.phase = "executing".into();
    r.state = "execution_started".into();
    r.started_at = Some(crate::daemon::now());
    save(paths, &mut r)?;
    let config = crate::store::load_config(paths)?;
    let outcome = execute(&config, &home, &r).await;
    match outcome {
        Outcome::NotStarted(reason) => finish(paths, &mut r, "rejected", Some(reason)),
        Outcome::Unknown(reason) => finish(paths, &mut r, "delivery_unknown", Some(reason)),
        Outcome::Reply { reply, session_id } => {
            r.reported_session_id = session_id.clone();
            let is_error = reply["is_error"] == true;
            r.reply = Some(reply);
            if session_id.as_deref() != Some(r.conversation_id.as_str()) {
                // The CLI continued (or forked) another conversation: never success.
                finish(
                    paths,
                    &mut r,
                    "delivery_unknown",
                    Some("SESSION_ID_MISMATCH".into()),
                )
            } else if is_error {
                finish(
                    paths,
                    &mut r,
                    "failed",
                    Some("PROVIDER_REPORTED_ERROR".into()),
                )
            } else {
                finish(paths, &mut r, "completed_with_reply", None)
            }
        }
    }
}

async fn prepare(paths: &Paths, r: &Record) -> Result<(PathBuf, fs::File)> {
    let account = usable_account(paths, r.provider, &r.account)?;
    let session = catalog::find(paths, &r.session_id)?;
    ensure!(
        session.provider == r.provider.as_str()
            && session.account == r.account
            && session.conversation_id == r.conversation_id,
        "STALE_TARGET"
    );
    let lock = crate::external::conversation_lock(paths, &r.conversation_id)
        .context("SESSION_OPERATION_IN_PROGRESS")?;
    crate::lifecycle::ensure_available(paths, &r.conversation_id, None)?;
    match r.request.mode {
        Mode::Resume => {
            let observation = native::observe(
                r.provider,
                &account.home,
                &r.conversation_id,
                session.attachment.as_ref(),
            );
            native::ensure_resumable(&session, &observation)?;
        }
        Mode::New => {
            ensure!(
                native::find_transcript(r.provider, &account.home, &r.conversation_id).is_none(),
                "STALE_TARGET: the pre-assigned conversation already exists"
            );
            ensure!(
                !native::live_sessions(r.provider, &account.home)?
                    .iter()
                    .any(|l| l.session_id == r.conversation_id),
                "SESSION_ALREADY_RUNNING"
            );
        }
    }
    usable_directory(&r.directory)?;
    let config = crate::store::load_config(paths)?;
    native::prelaunch_gate(r.provider, &config, &account.home).await?;
    Ok((account.home, lock))
}

enum Outcome {
    NotStarted(String),
    Unknown(String),
    Reply {
        reply: Value,
        session_id: Option<String>,
    },
}

/// Argument vector after the binary for one headless turn; the message is
/// written to stdin (never argv: no option injection or process-list exposure).
pub fn print_args(provider: ProviderId, mode: Mode, conversation: &str) -> Vec<String> {
    let args: Vec<&str> = match (provider, mode) {
        (ProviderId::Gemini, _) => {
            // `-p` takes a prompt that is appended to stdin; the real message is
            // on stdin, so the prompt is empty. Tools needing approval are not
            // run unattended (never yolo).
            let flag = if mode == Mode::Resume {
                "-r"
            } else {
                "--session-id"
            };
            vec![
                "-p",
                "",
                "-o",
                "json",
                "--approval-mode",
                GEMINI_APPROVAL_MODE,
                flag,
                conversation,
            ]
        }
        (_, Mode::Resume) => vec!["-p", "--resume", conversation, "--output-format", "json"],
        (_, Mode::New) => vec![
            "-p",
            "--session-id",
            conversation,
            "--output-format",
            "json",
        ],
    };
    args.into_iter().map(str::to_owned).collect()
}

async fn execute(config: &crate::config::Config, home: &Path, r: &Record) -> Outcome {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut std_command = native::command(r.provider, config, home);
    if r.provider == ProviderId::Gemini {
        // Headless runs cannot answer the folder-trust prompt; the directory is
        // the session's recorded one, chosen by the operator.
        std_command.env("GEMINI_CLI_TRUST_WORKSPACE", "true");
    }
    std::os::unix::process::CommandExt::process_group(&mut std_command, 0);
    let mut command = tokio::process::Command::from(std_command);
    command
        .args(print_args(r.provider, r.request.mode, &r.conversation_id))
        .current_dir(&r.directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(_) => return Outcome::NotStarted("PROVIDER_COULD_NOT_START".into()),
    };
    let pid = child.id();
    let text = r.request.text.clone();
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let run = async move {
        if let Some(mut input) = stdin.take() {
            let _ = input.write_all(text.as_bytes()).await;
            let _ = input.shutdown().await;
        }
        let mut bytes = vec![];
        if let Some(out) = stdout.as_mut() {
            let _ = out.take(MAX_OUTPUT_BYTES + 1).read_to_end(&mut bytes).await;
        }
        let status = child.wait().await;
        (bytes, status)
    };
    let limit = std::time::Duration::from_secs(r.timeout_seconds);
    let (bytes, status) = match tokio::time::timeout(limit, run).await {
        Ok(v) => v,
        Err(_) => {
            // Timeouts are not quiescence: stop the whole process group and
            // record an uncertain outcome that is never replayed.
            if let Some(pid) = pid {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            return Outcome::Unknown("EXECUTION_TIMEOUT".into());
        }
    };
    if bytes.len() as u64 > MAX_OUTPUT_BYTES {
        return Outcome::Unknown("OUTPUT_TOO_LARGE".into());
    }
    let exit = status.ok().and_then(|s| s.code());
    let Some(result) = parse(r.provider, &bytes, exit) else {
        return Outcome::Unknown(format!(
            "UNPARSEABLE_OUTPUT (exit {})",
            exit.map_or("signal".into(), |c| c.to_string())
        ));
    };
    let session_id = result["session_id"].as_str().map(str::to_owned);
    let mut text = result["result"].as_str().unwrap_or_default().to_owned();
    let truncated = text.len() > MAX_REPLY_TEXT;
    if truncated {
        let mut end = MAX_REPLY_TEXT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    let reply = json!({"text":text,"text_truncated":truncated,"is_error":result["is_error"].as_bool().unwrap_or(false),
        "subtype":result["subtype"],"session_id":session_id,"num_turns":result["num_turns"],"usage":result["usage"],
        "duration_ms":result["duration_ms"]});
    Outcome::Reply { reply, session_id }
}

/// Gemini `-o json`: `{session_id, response, stats, error?}`, normalised to the
/// shape the Claude result uses. A reported error, or a non-zero exit with a
/// parseable result, is an error reply.
fn parse_gemini(bytes: &[u8], exit: Option<i32>) -> Option<Value> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    if !v.is_object()
        || !(v.get("session_id").is_some()
            || v.get("response").is_some()
            || v.get("error").is_some())
    {
        return None;
    }
    let error = v.get("error").filter(|e| !e.is_null());
    let subtype = error
        .and_then(|e| e["type"].as_str().map(str::to_owned))
        .or_else(|| exit.filter(|c| *c != 0).map(|c| format!("exit_{c}")));
    let stats = v.get("stats").filter(|s| s.to_string().len() <= 16 * 1024);
    Some(
        json!({"type":"result","result":v["response"],"session_id":v["session_id"],
        "is_error":error.is_some() || exit != Some(0),"subtype":subtype,"num_turns":null,
        "usage":stats,"duration_ms":null}),
    )
}

/// The `--output-format json` result object (a single object, or the final
/// `type:"result"` element of an event array).
fn parse(provider: ProviderId, bytes: &[u8], exit: Option<i32>) -> Option<Value> {
    if provider == ProviderId::Gemini {
        return parse_gemini(bytes, exit);
    }
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let result = match v {
        Value::Array(items) => items.into_iter().rev().find(|i| i["type"] == "result")?,
        Value::Object(_) => v,
        _ => return None,
    };
    (result["type"] == "result" || result.get("result").is_some()).then_some(result)
}

/// Explicit refusal for modes a native provider cannot perform on a live session.
pub fn unsupported(provider: ProviderId, mode: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "UNSUPPORTED_PROVIDER_OPERATION: {} sessions do not support {mode}: {}; use mode resume for a stopped session or mode new",
        native::title(provider),
        native::no_input_api(provider)
    )
}

/// Route one raw `api send` request: Claude and Gemini headless modes, or the existing
/// Codex contract unchanged.
pub async fn dispatch(paths: &Paths, bytes: &[u8]) -> Result<Value> {
    let v: Value = serde_json::from_slice(bytes).context("INVALID_REQUEST")?;
    let id = v["message_id"].as_str().unwrap_or_default();
    let headless =
        |v: Value| -> Result<Request> { serde_json::from_value(v).context("INVALID_REQUEST") };
    if v["mode"] == "new" {
        return send(paths, headless(v)?).await;
    }
    if super::claude_steer::exists(paths, id) {
        return super::claude_steer::send(paths, v).await;
    }
    if exists(paths, id) {
        let request = headless(v).map_err(|_| anyhow::anyhow!("MESSAGE_ID_CONFLICT"))?;
        return send(paths, request).await;
    }
    let native_target = v["session_id"]
        .as_str()
        .and_then(|sid| catalog_provider(paths, sid))
        .and_then(|p| native::of(&p));
    if let Some(provider) = native_target {
        match v["mode"].as_str() {
            Some("resume") => return send(paths, headless(v)?).await,
            Some("submit" | "queue" | "steer") if provider == ProviderId::Claude => {
                return super::claude_steer::send(paths, v).await;
            }
            Some(mode @ ("submit" | "queue" | "steer")) => return Err(unsupported(provider, mode)),
            _ => bail!("INVALID_REQUEST"),
        }
    }
    let request: super::PublicRequest = serde_json::from_value(v).context("INVALID_REQUEST")?;
    super::send_public(paths, request).await
}
fn catalog_provider(paths: &Paths, id: &str) -> Option<String> {
    uuid::Uuid::parse_str(id).ok()?;
    let file = paths.root.join("session-catalog/catalog.json");
    let v: Value = serde_json::from_slice(&fs::read(file).ok()?).ok()?;
    v["sessions"][id]["provider"].as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn print_args_never_carry_the_message_and_select_the_session_flag() {
        let id = "11111111-1111-4111-8111-111111111111";
        assert_eq!(
            print_args(ProviderId::Claude, Mode::Resume, id),
            ["-p", "--resume", id, "--output-format", "json"]
        );
        assert_eq!(
            print_args(ProviderId::Claude, Mode::New, id)[1],
            "--session-id"
        );
        let g = print_args(ProviderId::Gemini, Mode::Resume, id);
        assert_eq!(
            g,
            [
                "-p",
                "",
                "-o",
                "json",
                "--approval-mode",
                "default",
                "-r",
                id
            ]
        );
        assert!(!g.iter().any(|a| a == "yolo" || a == "--yolo" || a == "-y"));
        assert_eq!(
            print_args(ProviderId::Gemini, Mode::New, id)[6],
            "--session-id"
        );
    }
    #[test]
    fn result_parsing_accepts_object_or_event_array_only() {
        let c = ProviderId::Claude;
        let o = parse(
            c,
            br#"{"type":"result","result":"hi","session_id":"s","is_error":false}"#,
            Some(0),
        )
        .unwrap();
        assert_eq!(o["result"], "hi");
        let a = parse(
            c,
            br#"[{"type":"system"},{"type":"result","result":"x","session_id":"s"}]"#,
            Some(0),
        )
        .unwrap();
        assert_eq!(a["result"], "x");
        assert!(parse(c, b"not json", Some(0)).is_none());
        assert!(parse(c, br#"{"type":"system"}"#, Some(0)).is_none());
        assert!(parse(c, b"[]", Some(0)).is_none());
        let g = ProviderId::Gemini;
        let ok = parse(
            g,
            br#"{"session_id":"s","response":"hi","stats":{"models":{}}}"#,
            Some(0),
        )
        .unwrap();
        assert_eq!(ok["result"], "hi");
        assert_eq!(ok["is_error"], false);
        let failed = parse(
            g,
            br#"{"session_id":"s","error":{"type":"FatalError","message":"x"}}"#,
            Some(1),
        )
        .unwrap();
        assert_eq!(failed["is_error"], true);
        assert_eq!(failed["subtype"], "FatalError");
        let exit = parse(g, br#"{"session_id":"s","response":"cut"}"#, Some(53)).unwrap();
        assert_eq!(exit["is_error"], true);
        assert_eq!(exit["subtype"], "exit_53");
        assert!(parse(g, b"[]", Some(0)).is_none());
        assert!(parse(g, br#"{"other":1}"#, Some(0)).is_none());
        assert!(parse(g, b"plain text", Some(0)).is_none());
    }
    #[test]
    fn stale_worker_projection_is_never_success_and_never_replays() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(d.path().join("root"))).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let mut r = Record {
            schema_version: 1,
            provider: ProviderId::Claude,
            request: Request {
                api_version: 1,
                message_id: id.clone(),
                mode: Mode::Resume,
                session_id: Some(uuid::Uuid::new_v4().to_string()),
                expected_generation: Some("a".repeat(64)),
                account_id: None,
                directory: None,
                name: None,
                text: "private".into(),
                expires_at: 1,
                timeout_seconds: None,
                recovery: None,
                steering: None,
            },
            session_id: "s".into(),
            conversation_id: "c".into(),
            account: "a".into(),
            directory: "/".into(),
            accepted_generation: "g".into(),
            timeout_seconds: 1,
            phase: "executing".into(),
            state: "execution_started".into(),
            reason: None,
            reply: None,
            reported_session_id: None,
            created_at: 0,
            updated_at: 0,
            started_at: Some(0),
            finished_at: None,
        };
        save(&paths, &mut r).unwrap();
        let p = project(&paths, read(&paths, &id).unwrap()).unwrap();
        assert_eq!(p.state, "delivery_unknown");
        assert_eq!(
            read(&paths, &id).unwrap().phase,
            "executing",
            "projection only"
        );
        let v = receipt_value(&p);
        assert!(!v.to_string().contains("private"));
        assert_eq!(v["facts"]["reply_observed"], false);
        assert!(active_for_account(&paths, "a").unwrap().is_empty());
        r.phase = "admitted".into();
        r.state = "pending_local".into();
        r.updated_at = 0;
        atomic_write(
            &file(&paths, &id).unwrap(),
            &serde_json::to_vec(&r).unwrap(),
        )
        .unwrap();
        assert_eq!(
            project(&paths, read(&paths, &id).unwrap()).unwrap().state,
            "rejected"
        );
    }
}
