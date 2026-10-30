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
//! `DATA_ROOT/messaging/claude-deliveries/` (the directory keeps its original name so
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
    paths.root.join(crate::layout::CLAUDE_DELIVERIES)
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
pub fn active_for_account(
    paths: &Paths,
    provider: ProviderId,
    account: &str,
) -> Result<Vec<String>> {
    let mut ids = vec![];
    for r in all(paths)? {
        if r.provider == provider && r.account == account && active(paths, &r)? {
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
    crate::accounts::ensure_available(paths, provider, name)?;
    let state = crate::store::load_state_snapshot(paths)?;
    let record = state.account(provider, name).context("ACCOUNT_NOT_FOUND")?;
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
    // Only an id check: wait briefly for a Codex observer instead of failing
    // a Claude/Gemini send with ADAPTER_BUSY.
    let mut ledger = super::Ledger::open_waiting(paths, std::time::Duration::from_secs(3))
        .map_err(anyhow::Error::from)?;
    super::public::initialize(&ledger).map_err(anyhow::Error::from)?;
    super::retention::ensure_not_retired(&ledger, id)?;
    super::retention::maybe_run(&mut ledger, crate::daemon::now());
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
    let count = super::retention::count_journal(&dir(paths));
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
            let conversation = if provider == ProviderId::Antigravity {
                // agy cannot pre-assign an id: create the empty conversation now
                // (no prompt is sent), then record it before any execution.
                let (config, home, dir) = (
                    config.clone(),
                    account.home.clone(),
                    PathBuf::from(&directory),
                );
                tokio::task::spawn_blocking(move || {
                    crate::provider::antigravity::allocate(&config, &home, &dir)
                })
                .await??
            } else {
                uuid::Uuid::new_v4().to_string()
            };
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
    let outcome = execute(paths, &config, &home, &r).await;
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
        Mode::New if r.provider == ProviderId::Antigravity => {
            // agy cannot pre-assign an id: admission created the (empty) conversation.
            ensure!(
                native::find_transcript(r.provider, &account.home, &r.conversation_id).is_some(),
                "STALE_TARGET: the created conversation is missing"
            );
            ensure!(
                !native::live_sessions(r.provider, &account.home)?
                    .iter()
                    .any(|l| l.session_id == r.conversation_id),
                "SESSION_ALREADY_RUNNING"
            );
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

async fn execute(
    paths: &Paths,
    config: &crate::config::Config,
    home: &Path,
    r: &Record,
) -> Outcome {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut std_command = native::command(r.provider, config, home);
    if r.provider == ProviderId::Gemini {
        // Headless runs cannot answer the folder-trust prompt; the directory is
        // the session's recorded one, chosen by the operator.
        std_command.env("GEMINI_CLI_TRUST_WORKSPACE", "true");
    }
    std::os::unix::process::CommandExt::process_group(&mut std_command, 0);
    // The conversation file before the run: unchanged afterwards (with a sign-in
    // failure) proves the prompt was never recorded as sent.
    let before = transcript_stamp(r.provider, home, &r.conversation_id);
    let agy = r.provider == ProviderId::Antigravity;
    let mut command = tokio::process::Command::from(std_command);
    command
        .args(if agy {
            antigravity_args(&r.conversation_id, r.timeout_seconds)
        } else {
            print_args(r.provider, r.request.mode, &r.conversation_id)
        })
        .current_dir(&r.directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(
            if matches!(r.provider, ProviderId::Gemini | ProviderId::Antigravity) {
                Stdio::piped()
            } else {
                Stdio::null()
            },
        )
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(_) => return Outcome::NotStarted("PROVIDER_COULD_NOT_START".into()),
    };
    let pid = child.id();
    let text = if agy {
        antigravity_input(&r.request.text)
    } else {
        r.request.text.clone()
    };
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let run = async move {
        if let Some(mut input) = stdin.take() {
            let _ = input.write_all(text.as_bytes()).await;
            let _ = input.shutdown().await;
        }
        let mut bytes = vec![];
        let mut errors = vec![];
        // Read both pipes together so a chatty stderr cannot stall stdout. Stderr (Gemini
        // only) is bounded and used solely to recognize an authentication failure.
        let out_read = async {
            if let Some(out) = stdout.as_mut() {
                let _ = out.take(MAX_OUTPUT_BYTES + 1).read_to_end(&mut bytes).await;
            }
        };
        let err_read = async {
            if let Some(err) = stderr.as_mut() {
                let _ = err
                    .take(crate::provider::gemini::auth_failure::SCAN_BYTES as u64)
                    .read_to_end(&mut errors)
                    .await;
                // Keep draining so a chatty child is never blocked on a full pipe.
                let _ = tokio::io::copy(err, &mut tokio::io::sink()).await;
            }
        };
        tokio::join!(out_read, err_read);
        let status = child.wait().await;
        (bytes, errors, status)
    };
    let limit = std::time::Duration::from_secs(r.timeout_seconds);
    let (bytes, errors, status) = match tokio::time::timeout(limit, run).await {
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
    let auth = if r.provider == ProviderId::Gemini {
        observe_gemini_auth(paths, &r.account, home, &bytes, &errors, exit)
    } else {
        None
    };
    if agy && let Some(reason) = antigravity_sign_in_refusal(&bytes, &errors) {
        return Outcome::NotStarted(reason);
    }
    let Some(result) = parse(r.provider, &bytes, exit) else {
        // Unparseable output after the CLI failed to establish its sign-in, with the
        // conversation untouched: the prompt never reached the model, so this is a
        // definite refusal rather than an uncertain delivery.
        if let Some(reason) = sign_in_refusal(
            auth.as_ref(),
            &before,
            &transcript_stamp(r.provider, home, &r.conversation_id),
        ) {
            return Outcome::NotStarted(reason);
        }
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

/// Record or clear the account's last observed Gemini authentication failure from one
/// finished headless run. Only stderr of a failed run and a reported JSON `error` are
/// inspected (never the reply text), so a model answer mentioning an auth term cannot
/// mark the account failed; a clean successful run clears a recorded failure.
fn observe_gemini_auth(
    paths: &Paths,
    account: &str,
    home: &Path,
    stdout: &[u8],
    stderr: &[u8],
    exit: Option<i32>,
) -> Option<crate::provider::gemini::auth_failure::Classified> {
    use crate::provider::gemini::auth_failure as failure;
    let reported = serde_json::from_slice::<Value>(stdout)
        .ok()
        .and_then(|v| v.get("error").filter(|e| !e.is_null()).cloned());
    if reported.is_none() && exit == Some(0) {
        failure::clear(paths, account);
        return None;
    }
    let mut text = String::new();
    // A JSON error's own exit code (`error.code`) counts like the process status.
    let mut status = exit;
    if let Some(error) = &reported {
        text.push_str(&error.to_string());
        text.push('\n');
        if error["code"].as_i64() == Some(failure::EXIT_AUTHENTICATION as i64) {
            status = Some(failure::EXIT_AUTHENTICATION);
        }
    }
    if exit != Some(0) {
        text.push_str(&String::from_utf8_lossy(stderr));
    }
    failure::record_output(paths, account, home, &text, status)
        .ok()
        .flatten()
        .map(|(_, classified)| classified)
}

/// Bound for one API-key validation run (clients wait about 20 s in total).
pub const GEMINI_VALIDATION_SECONDS: u64 = 15;
const GEMINI_VALIDATION_PROMPT: &str = "Reply with the single word OK.";

/// One tiny headless Gemini run (`gemini -p "" -o json`, prompt on stdin) that
/// checks an `api_key` account's stored key. It runs in a throwaway Gemini home
/// under the data root, so no chat is recorded in (or discovered from) the
/// account; the key reaches the child only through the environment. The result
/// is classified and recorded against the account exactly like a headless
/// delivery: `valid` (clean run; clears a recorded failure), `auth_failed` (a
/// recognizable authentication failure, recorded) or `inconclusive` (timeout,
/// crash, quota or network). Raw output is never returned or stored.
pub async fn validate_gemini_key(
    paths: &Paths,
    config: &crate::config::Config,
    account: &str,
    home: &Path,
) -> Value {
    use crate::provider::gemini::{self, auth_mode};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let verdict = |state: &str, code: Option<&str>, reason: Option<&str>| json!({"state":state,"code":code,"reason":reason,"timeout_seconds":GEMINI_VALIDATION_SECONDS});
    let Some(key) = auth_mode::read_key(home) else {
        return verdict(
            "inconclusive",
            Some("API_KEY_MISSING"),
            Some("no API key is stored"),
        );
    };
    let run_home = paths
        .root
        .join(crate::layout::PROVIDER_STATE)
        .join("gemini-validate")
        .join(uuid::Uuid::new_v4().to_string());
    let prepared = private_dir(&run_home)
        .and_then(|_| gemini::seed_settings(&run_home))
        .and_then(|_| auth_mode::set_mode(&run_home, auth_mode::Mode::ApiKey));
    if prepared.is_err() {
        let _ = fs::remove_dir_all(&run_home);
        return verdict(
            "inconclusive",
            Some("VALIDATION_NOT_STARTED"),
            Some("the validation home could not be prepared"),
        );
    }
    let mut std_command = gemini::command(config, &run_home);
    std_command
        .env("GEMINI_API_KEY", key.expose())
        .env(auth_mode::WRAPPER_ENV, key.expose())
        .env("GEMINI_CLI_TRUST_WORKSPACE", "true")
        .args([
            "-p",
            "",
            "-o",
            "json",
            "--approval-mode",
            GEMINI_APPROVAL_MODE,
        ])
        .current_dir(&run_home);
    drop(key);
    std::os::unix::process::CommandExt::process_group(&mut std_command, 0);
    let mut command = tokio::process::Command::from(std_command);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let outcome = match command.spawn() {
        Err(_) => Err("PROVIDER_COULD_NOT_START"),
        Ok(mut child) => {
            let pid = child.id();
            let mut stdin = child.stdin.take();
            let mut stdout = child.stdout.take();
            let mut stderr = child.stderr.take();
            let run = async move {
                if let Some(mut input) = stdin.take() {
                    let _ = input.write_all(GEMINI_VALIDATION_PROMPT.as_bytes()).await;
                    let _ = input.shutdown().await;
                }
                let (mut out, mut err) = (vec![], vec![]);
                let out_read = async {
                    if let Some(o) = stdout.as_mut() {
                        let _ = o.take(MAX_OUTPUT_BYTES).read_to_end(&mut out).await;
                        let _ = tokio::io::copy(o, &mut tokio::io::sink()).await;
                    }
                };
                let err_read = async {
                    if let Some(e) = stderr.as_mut() {
                        let _ = e
                            .take(gemini::auth_failure::SCAN_BYTES as u64)
                            .read_to_end(&mut err)
                            .await;
                        let _ = tokio::io::copy(e, &mut tokio::io::sink()).await;
                    }
                };
                tokio::join!(out_read, err_read);
                (out, err, child.wait().await)
            };
            match tokio::time::timeout(
                std::time::Duration::from_secs(GEMINI_VALIDATION_SECONDS),
                run,
            )
            .await
            {
                Ok(v) => Ok(v),
                Err(_) => {
                    if let Some(pid) = pid {
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGKILL);
                        }
                    }
                    Err("VALIDATION_TIMEOUT")
                }
            }
        }
    };
    let _ = fs::remove_dir_all(&run_home);
    let (stdout, stderr, status) = match outcome {
        Ok(v) => v,
        Err(code) => {
            return verdict(
                "inconclusive",
                Some(code),
                Some("the validation run did not finish; the key is stored but unverified"),
            );
        }
    };
    let exit = status.ok().and_then(|s| s.code());
    if let Some(c) = observe_gemini_auth(paths, account, home, &stdout, &stderr, exit) {
        return verdict("auth_failed", Some(c.code), Some(c.reason));
    }
    let clean =
        exit == Some(0) && parse_gemini(&stdout, exit).is_some_and(|r| r["is_error"] == false);
    if clean {
        verdict("valid", None, None)
    } else {
        verdict(
            "inconclusive",
            Some("VALIDATION_INCONCLUSIVE"),
            Some(
                "Gemini did not answer and reported no recognizable authentication failure (for example quota, network or a crash); the key is stored but unverified",
            ),
        )
    }
}

/// (size, modification time) of the conversation's native file, if any.
fn transcript_stamp(
    provider: ProviderId,
    home: &Path,
    conversation: &str,
) -> Option<(u64, std::time::SystemTime)> {
    let path = native::find_transcript(provider, home, conversation)?;
    let m = fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// The definite-refusal reason for a run whose output was unparseable: only when
/// the CLI failed while establishing its sign-in and the conversation file is
/// unchanged (nothing was recorded as sent).
fn sign_in_refusal<T: PartialEq>(
    auth: Option<&crate::provider::gemini::auth_failure::Classified>,
    before: &T,
    after: &T,
) -> Option<String> {
    let auth = auth.filter(|a| a.at_sign_in)?;
    (before == after).then(|| format!("{}; nothing was sent to the model", auth.reason))
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

/// agy headless turn on an existing conversation: one stream-json user message
/// on stdin (never argv), agy's own `--print-timeout` a little inside ours,
/// slash commands off so the text is never run as a command. Never
/// `--dangerously-skip-permissions`.
pub fn antigravity_args(conversation: &str, timeout_seconds: u64) -> Vec<String> {
    let print_timeout = format!("{}s", timeout_seconds.saturating_sub(5).max(1));
    [
        "--conversation",
        conversation,
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--print-timeout",
        &print_timeout,
        "--disable-slash-commands",
        "-p=",
    ]
    .map(str::to_owned)
    .to_vec()
}
/// The single NDJSON stream-json user message agy 1.4.10 reads.
pub fn antigravity_input(text: &str) -> String {
    let mut line =
        json!({"event":"user","message":{"role":"user","content":[{"type":"text","text":text}]}})
            .to_string();
    line.push('\n');
    line
}
/// A definite refusal: agy stopped at its sign-in prompt (stderr) and its
/// result reports an error with no turn, so the message never reached a model.
fn antigravity_sign_in_refusal(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    let err = String::from_utf8_lossy(stderr);
    if !err.contains("Authentication required") {
        return None;
    }
    let result = parse_antigravity(stdout, None);
    let untouched = result.as_ref().is_none_or(|r| {
        r["num_turns"].as_i64().unwrap_or(0) == 0 && r["result"].as_str().unwrap_or("").is_empty()
    });
    untouched.then(|| {
        "AUTH_REQUIRED: Antigravity asked for a browser sign-in; run `sessionsation account login antigravity:<name>`; nothing was sent to the model".to_owned()
    })
}
/// agy stream-json output: the final `{"event":"result","result":{conversation_id,
/// status,response,error?,num_turns,usage,duration_seconds}}`, normalised to the
/// shape the Claude result uses. A status other than SUCCESS, an error or a
/// non-zero exit is an error reply.
fn parse_antigravity(bytes: &[u8], exit: Option<i32>) -> Option<Value> {
    let result = bytes.split(|b| *b == b'\n').rev().find_map(|line| {
        let v: Value = serde_json::from_slice(line).ok()?;
        (v["event"] == "result" && v["result"].is_object()).then(|| v["result"].clone())
    })?;
    let error = result
        .get("error")
        .filter(|e| !e.is_null() && e.as_str() != Some(""));
    let ok = result["status"] == "SUCCESS" && error.is_none() && exit.is_none_or(|c| c == 0);
    let subtype = if ok {
        None
    } else {
        Some(
            result["status"]
                .as_str()
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_else(|| "error".into()),
        )
    };
    let usage = result
        .get("usage")
        .filter(|s| s.to_string().len() <= 16 * 1024);
    let session = result["conversation_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some(
        json!({"type":"result","result":result["response"],"session_id":session,
        "is_error":!ok,"subtype":subtype,"error":error,"num_turns":result["num_turns"],
        "usage":usage,"duration_ms":result["duration_seconds"].as_f64().map(|s| (s * 1000.0) as u64)}),
    )
}

/// The `--output-format json` result object (a single object, or the final
/// `type:"result"` element of an event array).
fn parse(provider: ProviderId, bytes: &[u8], exit: Option<i32>) -> Option<Value> {
    if provider == ProviderId::Gemini {
        return parse_gemini(bytes, exit);
    }
    if provider == ProviderId::Antigravity {
        return parse_antigravity(bytes, exit);
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
    if native_target.is_some() {
        match v["mode"].as_str() {
            Some("resume") => return send(paths, headless(v)?).await,
            Some("submit" | "queue" | "steer") => {
                return super::claude_steer::send(paths, v).await;
            }
            _ => bail!("INVALID_REQUEST"),
        }
    }
    let request: super::PublicRequest = serde_json::from_value(v).context("INVALID_REQUEST")?;
    super::send_public(paths, request).await
}
fn catalog_provider(paths: &Paths, id: &str) -> Option<String> {
    uuid::Uuid::parse_str(id).ok()?;
    let file = paths
        .root
        .join(crate::layout::SESSION_CATALOG)
        .join("catalog.json");
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
        assert!(
            active_for_account(&paths, ProviderId::Claude, "a")
                .unwrap()
                .is_empty()
        );
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

#[cfg(test)]
mod gemini_auth_tests {
    use super::*;
    use crate::provider::gemini::auth_failure as failure;
    #[test]
    fn headless_gemini_runs_record_only_recognizable_auth_failures_and_clear_on_success() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(t.path().join("root"))).unwrap();
        let home = paths.root.join("accounts/gemini/g1");
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
        let cur = || failure::current(&paths, "g1", &home);
        // Generic crash: nothing recorded.
        observe_gemini_auth(&paths, "g1", &home, b"", b"Segmentation fault", Some(139));
        assert!(cur().is_none());
        // Auth rejection on stderr of a failed run.
        observe_gemini_auth(&paths, "g1", &home, b"", b"Error: invalid_grant", Some(41));
        assert_eq!(cur().unwrap().code, "AUTH_GRANT_INVALID");
        // Another crash neither clears nor replaces it.
        observe_gemini_auth(&paths, "g1", &home, b"", b"ECONNRESET", Some(1));
        assert!(cur().is_some());
        // A reply that merely mentions an auth term does not count as a failure and,
        // being a clean success, clears the record.
        let ok = br#"{"session_id":"s","response":"UNAUTHENTICATED means 401","stats":{}}"#;
        observe_gemini_auth(&paths, "g1", &home, ok, b"", Some(0));
        assert!(cur().is_none());
        // A JSON-reported error is classified from the error object.
        let bad = br#"{"session_id":"s","error":{"type":"Error","message":"client no longer supported"}}"#;
        observe_gemini_auth(&paths, "g1", &home, bad, b"", Some(1));
        assert_eq!(cur().unwrap().code, "AUTH_CLIENT_UNSUPPORTED");
        // stderr of a successful run is never inspected.
        observe_gemini_auth(&paths, "g1", &home, ok, b"invalid_grant noise", Some(0));
        assert!(cur().is_none());
    }
}

#[cfg(test)]
mod gemini_sign_in_refusal_tests {
    use super::*;
    use crate::provider::gemini::auth_failure as failure;
    #[test]
    fn real_unsupported_client_run_is_a_definite_refusal_and_marks_the_account() {
        let t = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(t.path().join("root"))).unwrap();
        let home = paths.root.join("accounts/gemini/g1");
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
        let stderr = failure::tests::UNSUPPORTED_CLIENT_STDERR.as_bytes();
        // The real run: exit 1, empty stdout, the tier rejection on stderr.
        assert!(parse(ProviderId::Gemini, b"", Some(1)).is_none());
        let auth = observe_gemini_auth(&paths, "g1", &home, b"", stderr, Some(1)).unwrap();
        assert!(auth.at_sign_in);
        let recorded = failure::current(&paths, "g1", &home).unwrap();
        assert_eq!(recorded.code, "AUTH_CLIENT_UNSUPPORTED");
        // Untouched conversation: a definite refusal naming the reason.
        let reason = sign_in_refusal(Some(&auth), &None::<u8>, &None).unwrap();
        assert!(reason.starts_with("AUTH_CLIENT_UNSUPPORTED:"), "{reason}");
        assert!(reason.ends_with("nothing was sent to the model"));
        // A conversation file that changed during the run keeps it uncertain.
        assert!(sign_in_refusal(Some(&auth), &None, &Some(1u8)).is_none());
        // A failure that is not a sign-in failure (a 401 mid-run) stays uncertain,
        // and so does a run with no recognized authentication failure.
        let mid = observe_gemini_auth(&paths, "g1", &home, b"", b"status 401", Some(1)).unwrap();
        assert!(!mid.at_sign_in);
        assert!(sign_in_refusal(Some(&mid), &None::<u8>, &None).is_none());
        assert!(sign_in_refusal(None, &None::<u8>, &None).is_none());
        // A JSON-reported fatal authentication exit code counts like the status.
        let json = br#"{"session_id":"s","error":{"type":"Error","message":"x","code":41}}"#;
        assert!(
            observe_gemini_auth(&paths, "g1", &home, json, b"", Some(1))
                .unwrap()
                .at_sign_in
        );
        // A later clean run clears the mark.
        let ok = br#"{"session_id":"s","response":"hi","stats":{}}"#;
        assert!(observe_gemini_auth(&paths, "g1", &home, ok, b"", Some(0)).is_none());
        assert!(failure::current(&paths, "g1", &home).is_none());
    }
}
