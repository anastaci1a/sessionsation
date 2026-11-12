//! One-pass `api session launch` (contract 1.49.0): start a Codex or Claude session with an
//! exact model and effort, verify both from the session itself, then deliver the brief, and
//! report all of it in ONE record keyed by the request id.
//!
//! The terminal, provider start, name reservation and uncertainty handling are exactly
//! `api launch`'s ([`crate::launch::admit`]); the launch request carries only fields older
//! binaries know (`model`, `reasoning_effort`), so receipts stay readable after a rollback.
//! The brief, its delivery and the verification live only in this journal
//! (`operations/session-launches/<request_id>.json`, the brief bytes in `<request_id>.brief`,
//! mode 0600, removed once the record is terminal).
//!
//! The brief is held by Sessionsation until the model (and effort, when requested) is
//! verified, so a session on the wrong model never receives it: Claude through an ordinary
//! terminal-input send, Codex as an ordinary public send. Claude's initial-prompt delivery
//! (`--brief-delivery initial-prompt`) is an explicit opt-in: it is faster, but the brief is
//! handed to Claude before the model can be verified.
use crate::{
    launch,
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::{AccountRef, ProviderId},
    session_model::{self, Observed},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub const DEFAULT_DEADLINE: i64 = 300;
pub const MIN_DEADLINE: i64 = 30;
pub const MAX_DEADLINE: i64 = 1800;
pub const MAX_WAIT: u64 = 1800;
const MAX_BRIEF_ATTEMPTS: u32 = 5;
const RETRY_SECONDS: i64 = 5;
/// A record still `admitted` with no launch receipt after this long was never admitted.
const ADMIT_GRACE_SECONDS: i64 = 120;
pub const CLAUDE_INITIAL_MAX: usize = 32768;
pub const CLAUDE_HELD_MAX: usize = 8192;
pub const CODEX_MAX: usize = 16384;

const TERMINAL: &[&str] = &["complete", "failed", "cancelled"];

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Brief {
    /// `held` (Claude, default) | `initial_prompt` (Claude, opt-in) | `first_turn` (Codex)
    delivery: String,
    /// pending | held | handed_to_provider | sending | delivered | not_sent | delivery_unknown
    state: String,
    sha256: String,
    bytes: usize,
    attempts: u32,
    message_id: Option<String>,
    #[serde(default)]
    message_ids: Vec<String>,
    receipt_state: Option<String>,
    reason: Option<String>,
    #[serde(default)]
    next_attempt_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    schema_version: u32,
    id: String,
    /// The request as given (before Space and name normalization), for idempotency.
    request: launch::Request,
    provider: String,
    brief: Brief,
    model_requested: String,
    effort_requested: Option<String>,
    observed: Option<Observed>,
    model_confirmed: bool,
    effort_confirmed: bool,
    /// admitted | launching | verifying_model | delivering_brief | complete | failed | cancelled
    phase: String,
    reason: Option<String>,
    detail: Option<String>,
    session_id: Option<String>,
    conversation_id: Option<String>,
    created_at: i64,
    updated_at: i64,
    deadline_at: i64,
}
impl Record {
    fn terminal(&self) -> bool {
        TERMINAL.contains(&self.phase.as_str())
    }
}

fn dir(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::SESSION_LAUNCHES)
}
fn file(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_REQUEST_ID")?;
    Ok(dir(paths).join(format!("{id}.json")))
}
fn brief_file(paths: &Paths, id: &str) -> Result<PathBuf> {
    Ok(file(paths, id)?.with_extension("brief"))
}
pub fn exists(paths: &Paths, id: &str) -> bool {
    file(paths, id).is_ok_and(|f| f.is_file())
}
fn read(paths: &Paths, id: &str) -> Result<Record> {
    let bytes = fs::read(file(paths, id)?).context("OPERATION_NOT_FOUND")?;
    serde_json::from_slice(&bytes).context("UNREADABLE_OPERATION_RECORD")
}
fn save(paths: &Paths, r: &Record) -> Result<()> {
    private_dir(&dir(paths))?;
    atomic_write(&file(paths, &r.id)?, &serde_json::to_vec_pretty(r)?)
}
fn list(paths: &Paths) -> Result<Vec<Record>> {
    let mut all = vec![];
    if !dir(paths).is_dir() {
        return Ok(all);
    }
    for entry in crate::paths::journal_paths(&dir(paths))? {
        if let Some(id) = entry
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
            && let Ok(r) = read(paths, id)
        {
            all.push(r);
        }
    }
    all.sort_by_key(|r| (r.created_at, r.id.clone()));
    Ok(all)
}
fn journal_lock(paths: &Paths) -> Result<fs::File> {
    private_dir(&dir(paths))?;
    let f = lock_file(&dir(paths).join("journal.lock"))?;
    f.lock_exclusive()?;
    Ok(f)
}
fn update(paths: &Paths, id: &str, edit: impl FnOnce(&mut Record) -> bool) -> Result<Record> {
    let _lock = journal_lock(paths)?;
    let mut r = read(paths, id)?;
    if edit(&mut r) {
        r.updated_at = crate::daemon::now();
        save(paths, &r)?;
    }
    Ok(r)
}
fn end(paths: &Paths, id: &str, phase: &str, reason: &str, detail: Option<String>) {
    let _ = update(paths, id, |r| {
        if r.terminal() {
            return false;
        }
        r.phase = phase.into();
        r.reason = Some(reason.into());
        if detail.is_some() {
            r.detail = detail;
        }
        if matches!(r.brief.state.as_str(), "pending" | "held") {
            r.brief.state = "not_sent".into();
        }
        true
    });
    if let Ok(f) = brief_file(paths, id) {
        let _ = fs::remove_file(f);
    }
}

fn public(paths: &Paths, r: &Record) -> Value {
    let session = r
        .session_id
        .as_deref()
        .and_then(|id| crate::catalog::find(paths, id).ok());
    let receipt = launch::status(paths, &r.id).ok();
    let observed = r.observed.as_ref();
    json!({"id":r.id,"kind":"session_launch","request_id":r.id,"phase":r.phase,"terminal":r.terminal(),
        "reason":r.reason,"detail":r.detail,"provider":r.provider,"account_id":r.request.account_id,
        "session_id":r.session_id,"provider_conversation_id":r.conversation_id,
        "generation":session.as_ref().map(|s| s.generation()),
        "space":session.as_ref().and_then(|s| s.space.clone()),
        "name":session.as_ref().map(|s| s.name.clone()),
        "launch":receipt.as_ref().map(|x| json!({"phase":x.phase,"detail":x.detail,"workspace_id":x.workspace_id,"tab_id":x.tab_id,"pane_id":x.pane_id})),
        "model":{"requested":r.model_requested,"applied":observed.map(|o| o.model.clone()),"confirmed":r.model_confirmed,
            "source":observed.map(|o| o.source.clone()),"observed_at":observed.map(|o| o.observed_at)},
        "effort":{"requested":r.effort_requested,"applied":observed.and_then(|o| o.effort.clone()),"confirmed":r.effort_confirmed},
        "brief":{"delivery":r.brief.delivery,"state":r.brief.state,"sha256":r.brief.sha256,"bytes":r.brief.bytes,
            "attempts":r.brief.attempts,"message_id":r.brief.message_id,"message_ids":r.brief.message_ids,
            "receipt_state":r.brief.receipt_state,"reason":r.brief.reason},
        "created_at":r.created_at,"updated_at":r.updated_at,"deadline_at":r.deadline_at,
        "can_cancel":!r.terminal() && matches!(r.brief.state.as_str(),"pending"|"held"),
        "can_respond":false,"can_recover":false})
}

pub struct Args {
    pub space: String,
    pub create_space: bool,
    pub name: String,
    pub account: String,
    pub model: String,
    pub effort: Option<String>,
    pub brief_file: PathBuf,
    pub when_ready: bool,
    pub initial_prompt: bool,
    pub directory: Option<PathBuf>,
    pub create_directory: bool,
    pub permissions: Option<crate::config::LaunchPermissions>,
    pub herdr_session: Option<String>,
    pub herdr_socket: Option<PathBuf>,
    pub request_id: Option<String>,
    pub deadline: Option<i64>,
    pub wait: Option<u64>,
    pub dry_run: bool,
}

fn read_brief(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if path == Path::new("-") {
        std::io::stdin().take(65537).read_to_end(&mut bytes)?;
    } else {
        let meta = fs::metadata(path).context("INVALID_ARGUMENT: --brief-file is not readable")?;
        ensure!(
            meta.is_file(),
            "INVALID_ARGUMENT: --brief-file is not a regular file"
        );
        fs::File::open(path)?.take(65537).read_to_end(&mut bytes)?;
    }
    ensure!(bytes.len() <= 65536, "INPUT_TOO_LARGE");
    Ok(bytes)
}

/// Brief rules per delivery (checked before anything is created).
fn check_brief(bytes: &[u8], delivery: &str, paths: &Paths) -> Result<String> {
    let text = String::from_utf8(bytes.to_vec())
        .map_err(|_| anyhow::anyhow!("INVALID_ARGUMENT: the brief is not UTF-8"))?;
    ensure!(
        !text.trim().is_empty() && !text.contains('\0'),
        "INVALID_ARGUMENT: the brief is empty or contains NUL"
    );
    match delivery {
        "initial_prompt" => {
            ensure!(text.len() <= CLAUDE_INITIAL_MAX, "INPUT_TOO_LARGE");
            ensure!(
                !text.trim_start().starts_with(['/', '!', '#']),
                "INVALID_ARGUMENT: an initial prompt starting with / ! or # would run as a command"
            );
        }
        "held" => {
            ensure!(
                crate::messaging::claude_steer::terminal::enabled(paths),
                "{}",
                crate::messaging::claude_steer::terminal::DISABLED
            );
            ensure!(text.len() <= CLAUDE_HELD_MAX, "TERMINAL_TEXT_TOO_LONG");
            crate::messaging::claude_steer::terminal::deliverable(&text)?;
        }
        _ => ensure!(text.len() <= CODEX_MAX, "INPUT_TOO_LARGE"),
    }
    Ok(text)
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Build the launch request from the flags (no I/O beyond the directory check).
fn request(args: &Args, id: &str) -> Result<launch::Request> {
    let socket = args
        .herdr_socket
        .clone()
        .or_else(|| std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from))
        .context("INVALID_ARGUMENT: --herdr-socket is required outside a HERDR pane")?;
    let session = args
        .herdr_session
        .clone()
        .or_else(|| crate::terminal::herdr_session_from(|k| std::env::var(k).ok()))
        .context("INVALID_ARGUMENT: --herdr-session is required outside a HERDR pane")?;
    let wanted = match &args.directory {
        Some(d) => d.clone(),
        None => std::env::current_dir()?,
    };
    ensure!(wanted.is_absolute(), "INVALID_DIRECTORY");
    let directory = if wanted.is_dir() {
        launch::Directory {
            path: fs::canonicalize(&wanted)?,
            create: vec![],
            confirmed_existing: true,
        }
    } else {
        ensure!(
            args.create_directory,
            "DIRECTORY_CONFIRMATION_REQUIRED: the directory does not exist (add --create-directory)"
        );
        // Missing components from the first missing one down to the target.
        let mut create = vec![];
        let mut p = wanted.as_path();
        while !p.exists() {
            create.push(p.to_path_buf());
            p = p.parent().context("INVALID_DIRECTORY")?;
        }
        create.reverse();
        launch::Directory {
            path: wanted.clone(),
            create,
            confirmed_existing: false,
        }
    };
    Ok(launch::Request {
        version: 1,
        request_id: id.to_owned(),
        account_id: args.account.clone(),
        name: args.name.clone(),
        host: launch::Host { session, socket },
        space: if args.create_space {
            launch::Space::New {
                label: args.space.clone(),
            }
        } else {
            launch::Space::Existing {
                id: args.space.clone(),
            }
        },
        directory,
        hosted: None,
        permissions: args.permissions,
        model: Some(args.model.clone()),
        reasoning_effort: args.effort.clone(),
    })
}

pub async fn launch(paths: &Paths, args: Args) -> Result<Value> {
    let id = args
        .request_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    ensure!(
        uuid::Uuid::parse_str(&id).is_ok_and(|u| u.to_string() == id),
        "INVALID_REQUEST_ID"
    );
    let account = AccountRef::parse(&args.account)?;
    ensure!(
        matches!(account.provider, ProviderId::Claude | ProviderId::Codex),
        "UNSUPPORTED_PROVIDER_OPERATION: session launch supports codex and claude accounts"
    );
    session_model::validate(account.provider, &args.model, args.effort.as_deref())?;
    let delivery = match account.provider {
        ProviderId::Claude if args.initial_prompt => {
            ensure!(
                !args.when_ready,
                "INVALID_ARGUMENT: --when-ready and --brief-delivery initial-prompt exclude each other"
            );
            "initial_prompt"
        }
        ProviderId::Claude => "held",
        _ => {
            ensure!(
                !args.initial_prompt,
                "INVALID_ARGUMENT: initial-prompt delivery is Claude-only"
            );
            "first_turn"
        }
    };
    let deadline = args.deadline.unwrap_or(DEFAULT_DEADLINE);
    ensure!(
        (MIN_DEADLINE..=MAX_DEADLINE).contains(&deadline),
        "INVALID_ARGUMENT: --deadline must be {MIN_DEADLINE}..{MAX_DEADLINE} seconds"
    );
    let wait = args.wait.unwrap_or(0);
    ensure!(
        wait <= MAX_WAIT,
        "INVALID_ARGUMENT: --wait is at most {MAX_WAIT} seconds"
    );
    let bytes = read_brief(&args.brief_file)?;
    check_brief(&bytes, delivery, paths)?;
    let digest = sha(&bytes);
    let request = request(&args, &id)?;
    if args.dry_run {
        let mut v = launch::admit(paths, request, true).await?;
        v["brief"] = json!({"delivery":delivery,"bytes":bytes.len(),"sha256":digest});
        v["model"] = json!({"requested":args.model});
        v["effort"] = json!({"requested":args.effort});
        return Ok(v);
    }
    {
        let _admission = {
            private_dir(&dir(paths))?;
            let f = lock_file(&dir(paths).join("admission.lock"))?;
            f.lock_exclusive()?;
            f
        };
        if exists(paths, &id) {
            let r = read(paths, &id)?;
            ensure!(
                r.request == request && r.brief.sha256 == digest,
                "REQUEST_ID_CONFLICT"
            );
            let mut v = public(paths, &r);
            v["existing"] = json!(true);
            return finish_wait(paths, &id, v, wait).await;
        }
        let now = crate::daemon::now();
        let r = Record {
            schema_version: 1,
            id: id.clone(),
            request: request.clone(),
            provider: account.provider.as_str().to_owned(),
            brief: Brief {
                delivery: delivery.into(),
                state: if delivery == "initial_prompt" {
                    "pending"
                } else {
                    "held"
                }
                .into(),
                sha256: digest,
                bytes: bytes.len(),
                attempts: 0,
                message_id: None,
                message_ids: vec![],
                receipt_state: None,
                reason: None,
                next_attempt_at: 0,
            },
            model_requested: args.model.clone(),
            effort_requested: args.effort.clone(),
            observed: None,
            model_confirmed: false,
            effort_confirmed: false,
            phase: "admitted".into(),
            reason: None,
            detail: None,
            session_id: None,
            conversation_id: None,
            created_at: now,
            updated_at: now,
            deadline_at: now + deadline,
        };
        private_dir(&dir(paths))?;
        let brief = brief_file(paths, &id)?;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&brief)
            .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes))?;
        save(paths, &r)?;
        if let Err(e) = launch::admit(paths, request, false).await {
            // Nothing was created: the refusal is the answer.
            let _ = fs::remove_file(&brief);
            let _ = fs::remove_file(file(paths, &id)?);
            return Err(e);
        }
        let _ = update(paths, &id, |r| {
            if r.phase != "admitted" {
                return false;
            }
            r.phase = "launching".into();
            true
        });
    }
    let mut v = public(paths, &read(paths, &id)?);
    v["existing"] = json!(false);
    finish_wait(paths, &id, v, wait).await
}

/// `--wait N`: drive and poll the record until it ends or N seconds passed. A non-`complete`
/// end is an error envelope carrying the record.
async fn finish_wait(paths: &Paths, id: &str, first: Value, wait: u64) -> Result<Value> {
    if wait == 0 {
        return Ok(first);
    }
    let until = std::time::Instant::now() + std::time::Duration::from_secs(wait);
    loop {
        advance(paths, id).await;
        let r = read(paths, id)?;
        if r.terminal() || std::time::Instant::now() >= until {
            let mut v = public(paths, &r);
            v["existing"] = first["existing"].clone();
            if r.phase == "complete" {
                return Ok(v);
            }
            let reason = r
                .reason
                .clone()
                .unwrap_or_else(|| "not finished within --wait".into());
            let code = match reason.as_str() {
                "model_not_confirmed" | "effort_not_confirmed" | "effort_unsupported" => {
                    "MODEL_NOT_CONFIRMED"
                }
                "launch_failed" => "PROVIDER_LAUNCH_FAILED",
                "launch_uncertain" => "LAUNCH_ACK_UNCERTAIN",
                _ => "PROMPT_NOT_DELIVERED",
            };
            return Err(session_model::NotConfirmed {
                code,
                message: format!("session launch {} is {}: {reason}", r.id, r.phase),
                operation: v,
            }
            .into());
        }
        tokio::time::sleep(crate::timing::scaled(std::time::Duration::from_millis(500))).await;
    }
}

pub fn status(paths: &Paths, id: &str) -> Result<Value> {
    Ok(public(paths, &read(paths, id)?))
}

/// Cancel while the brief has not been handed over; a started session keeps running.
pub fn cancel(paths: &Paths, id: &str) -> Result<Value> {
    let r = read(paths, id)?;
    if r.phase == "cancelled" {
        return Ok(public(paths, &r));
    }
    ensure!(!r.terminal(), "OPERATION_ALREADY_TERMINAL");
    let r = update(paths, id, |r| {
        if r.terminal() || !matches!(r.brief.state.as_str(), "pending" | "held") {
            return false;
        }
        r.phase = "cancelled".into();
        r.reason = Some("cancelled".into());
        r.brief.state = "not_sent".into();
        true
    })?;
    ensure!(
        r.phase == "cancelled",
        "OPERATION_WORKER_ACTIVE: the brief is already being delivered"
    );
    if let Ok(f) = brief_file(paths, id) {
        let _ = fs::remove_file(f);
    }
    let mut v = public(paths, &r);
    v["cancellation_requested"] = json!(true);
    Ok(v)
}

pub fn operations(paths: &Paths) -> Result<Vec<Value>> {
    let now = crate::daemon::now();
    Ok(list(paths)?
        .iter()
        .filter(|r| !r.terminal() || now - r.updated_at < 3600)
        .map(|r| public(paths, r))
        .collect())
}

/// For `launch-run` (Claude, initial-prompt delivery only): the brief to pass as Claude's
/// positional prompt, journaled as handed over first. None for every other launch.
pub fn initial_prompt(paths: &Paths, id: &str) -> Result<Option<String>> {
    if !exists(paths, id) {
        return Ok(None);
    }
    let r = read(paths, id)?;
    if r.brief.delivery != "initial_prompt" || r.terminal() {
        return Ok(None);
    }
    let text = fs::read_to_string(brief_file(paths, id)?)?;
    update(paths, id, |r| {
        r.brief.state = "handed_to_provider".into();
        true
    })?;
    Ok(Some(text))
}

/// Daemon pass (run by the model monitor) over pending one-pass launches.
pub async fn tick(paths: &Paths) {
    let Ok(all) = list(paths) else { return };
    for r in all.iter().filter(|r| !r.terminal()) {
        advance(paths, &r.id).await;
    }
}

async fn advance(paths: &Paths, id: &str) {
    let Ok(lock_path) = file(paths, id).map(|f| f.with_extension("advance.lock")) else {
        return;
    };
    let Ok(lock) = lock_file(&lock_path) else {
        return;
    };
    if lock.try_lock_exclusive().is_err() {
        return;
    }
    let Ok(r) = read(paths, id) else { return };
    if r.terminal() {
        if let Ok(f) = brief_file(paths, id) {
            let _ = fs::remove_file(f);
        }
        return;
    }
    let now = crate::daemon::now();
    if now >= r.deadline_at {
        if r.brief.state == "sending" {
            // A send is in flight: one last read; never reported as not sent.
            if r.phase == "delivering_brief" {
                deliver(paths, &r).await;
                match read(paths, id) {
                    Ok(x) if x.terminal() => return,
                    // The read proved nothing was written: an ordinary timeout.
                    Ok(x) if x.brief.state != "sending" => {
                        return end(paths, id, "failed", "ready_timeout", None);
                    }
                    _ => {}
                }
            }
            let _ = update(paths, id, |x| {
                x.brief.state = "delivery_unknown".into();
                x.brief.reason = Some("brief_delivery_unknown".into());
                true
            });
            return end(paths, id, "failed", "ready_timeout", None);
        }
        let reason = match r.phase.as_str() {
            "launching" if r.reason.as_deref() == Some("launch_uncertain") => "launch_uncertain",
            _ => "ready_timeout",
        };
        return end(paths, id, "failed", reason, None);
    }
    match r.phase.as_str() {
        "admitted" => {
            if launch::status(paths, id).is_ok() {
                let _ = update(paths, id, |r| {
                    r.phase = "launching".into();
                    true
                });
            } else if now - r.created_at > ADMIT_GRACE_SECONDS {
                // `launch::admit` runs in the admitting process and writes the receipt
                // before dispatching anything; no receipt this long after admission means it
                // never got that far (same bound as the launch ack).
                end(
                    paths,
                    id,
                    "failed",
                    "launch_failed",
                    Some("the launch was never admitted".into()),
                );
            }
        }
        "launching" => follow_launch(paths, &r),
        "verifying_model" => verify(paths, &r).await,
        "delivering_brief" => deliver(paths, &r).await,
        _ => {}
    }
}

fn follow_launch(paths: &Paths, r: &Record) {
    let Ok(receipt) = launch::reconcile_status(paths, &r.id) else {
        return;
    };
    match receipt.phase.as_str() {
        "failed" | "abandoned" => end(paths, &r.id, "failed", "launch_failed", receipt.detail),
        "uncertain" => {
            let _ = update(paths, &r.id, |x| {
                let changed = x.reason.as_deref() != Some("launch_uncertain");
                x.reason = Some("launch_uncertain".into());
                x.detail = receipt.detail.clone();
                changed
            });
        }
        "attached" | "running" if receipt.session_id.is_some() => {
            let _ = update(paths, &r.id, |x| {
                if x.phase != "launching" {
                    return false;
                }
                x.phase = "verifying_model".into();
                x.reason = None;
                x.detail = None;
                x.session_id = receipt.session_id.clone();
                x.conversation_id = receipt.conversation_id.clone();
                true
            });
        }
        _ => {}
    }
}

fn claude_home(paths: &Paths, r: &Record) -> Option<PathBuf> {
    let account = AccountRef::parse(&r.request.account_id).ok()?;
    crate::provider::claude::account(paths, &account.name)
        .ok()
        .map(|a| a.home)
}

async fn verify(paths: &Paths, r: &Record) {
    let Some(conversation) = r.conversation_id.clone() else {
        return;
    };
    let observed = if r.provider == "claude" {
        // The first status-line record of the brand-new conversation proves the TUI mounted
        // (folder trust accepted) and carries the resolved model and effort.
        claude_home(paths, r)
            .and_then(|h| session_model::hook_record(&h, &conversation))
            .as_ref()
            .and_then(Observed::from_hook)
    } else {
        crate::messaging::model_change::codex_settings(paths, &conversation)
            .await
            .unwrap_or_default()
    };
    let Some(o) = observed else {
        let _ = update(paths, &r.id, |x| {
            let changed = x.reason.as_deref() != Some("waiting_for_session");
            x.reason = Some("waiting_for_session".into());
            changed
        });
        return;
    };
    let _ = session_model::store(paths, &conversation, o.clone());
    let model_ok = session_model::same_model(&o.model, &r.model_requested);
    let effort_ok = match (&r.effort_requested, &o.effort) {
        (None, _) => true,
        (Some(want), Some(got)) => session_model::same(want, got),
        (Some(_), None) => false,
    };
    let reason = if !model_ok {
        Some("model_not_confirmed")
    } else if !effort_ok && o.effort.is_none() {
        Some("effort_unsupported")
    } else if !effort_ok {
        Some("effort_not_confirmed")
    } else {
        None
    };
    let _ = update(paths, &r.id, |x| {
        x.observed = Some(o.clone());
        x.model_confirmed = model_ok;
        x.effort_confirmed = effort_ok && x.effort_requested.is_some();
        if reason.is_none() {
            x.phase = "delivering_brief".into();
            x.reason = None;
        }
        true
    });
    if let Some(reason) = reason {
        // The session keeps running; a held brief is never sent to the wrong model.
        end(paths, &r.id, "failed", reason, None);
    }
}

async fn deliver(paths: &Paths, r: &Record) {
    let now = crate::daemon::now();
    let Some(conversation) = r.conversation_id.clone() else {
        return;
    };
    if r.brief.delivery == "initial_prompt" {
        let found = claude_home(paths, r)
            .and_then(|h| crate::provider::claude::find_transcript(&h, &conversation))
            .zip(fs::read_to_string(brief_file(paths, &r.id).unwrap_or_default()).ok())
            .and_then(|(t, text)| {
                crate::messaging::claude_steer::terminal::find(&t, 0, text.trim(), 0).ok()
            })
            .is_some_and(|f| f.uuid.is_some());
        if found {
            let _ = update(paths, &r.id, |x| {
                x.brief.state = "delivered".into();
                x.phase = "complete".into();
                x.reason = Some("completed".into());
                true
            });
            if let Ok(f) = brief_file(paths, &r.id) {
                let _ = fs::remove_file(f);
            }
        }
        return;
    }
    if let Some(mid) = &r.brief.message_id {
        let receipt = crate::messaging::receipt_public(paths, mid, false).await;
        let (state, accepted) = match &receipt {
            Ok(v) => (
                v["state"].as_str().unwrap_or("unknown").to_owned(),
                v["facts"]["accepted_native"] == true,
            ),
            // Only a receipt that provably does not exist means the send never happened.
            // Any other read error is unknown: read again later, never resend.
            Err(e) if not_found(e) => ("not_found".to_owned(), false),
            Err(_) => return,
        };
        if accepted {
            let _ = update(paths, &r.id, |x| {
                x.brief.state = "delivered".into();
                x.brief.receipt_state = Some(state.clone());
                x.phase = "complete".into();
                x.reason = Some("completed".into());
                true
            });
            if let Ok(f) = brief_file(paths, &r.id) {
                let _ = fs::remove_file(f);
            }
            return;
        }
        match state.as_str() {
            "delivery_unknown" => {
                let _ = update(paths, &r.id, |x| {
                    x.brief.state = "delivery_unknown".into();
                    x.brief.receipt_state = Some(state.clone());
                    true
                });
                return end(paths, &r.id, "failed", "brief_delivery_unknown", None);
            }
            // Resent only when nothing was written to the session (a pre-submit refusal or a
            // send that never created its receipt): the brief goes out at most once.
            s if resend_allowed(s, receipt.as_ref().ok()) => {
                let reason = receipt
                    .as_ref()
                    .ok()
                    .and_then(|v| v["reason"].as_str().map(str::to_owned))
                    .or_else(|| receipt.as_ref().err().map(|e| e.to_string()));
                if r.brief.attempts >= MAX_BRIEF_ATTEMPTS {
                    let _ = update(paths, &r.id, |x| {
                        x.brief.receipt_state = Some(state.clone());
                        x.brief.reason = reason.clone();
                        true
                    });
                    return end(paths, &r.id, "failed", "brief_not_delivered", reason);
                }
                let _ = update(paths, &r.id, |x| {
                    x.brief.message_id = None;
                    x.brief.state = "held".into();
                    x.brief.receipt_state = Some(state.clone());
                    x.brief.reason = reason.clone();
                    x.brief.next_attempt_at = now + RETRY_SECONDS;
                    true
                });
            }
            _ => {
                let _ = update(paths, &r.id, |x| {
                    let changed = x.brief.receipt_state.as_deref() != Some(state.as_str());
                    x.brief.receipt_state = Some(state.clone());
                    changed
                });
            }
        }
        return;
    }
    if now < r.brief.next_attempt_at {
        return;
    }
    let Ok(text) = fs::read_to_string(brief_file(paths, &r.id).unwrap_or_default()) else {
        return end(
            paths,
            &r.id,
            "failed",
            "brief_not_delivered",
            Some("the brief file is gone".into()),
        );
    };
    let Some(session_id) = r.session_id.clone() else {
        return;
    };
    let Ok(session) = crate::catalog::find(paths, &session_id) else {
        return;
    };
    // Journal the new message id before the send: never two sends for one id.
    let mid = uuid::Uuid::new_v4().to_string();
    let Ok(_) = update(paths, &r.id, |x| {
        x.brief.message_id = Some(mid.clone());
        x.brief.message_ids.push(mid.clone());
        x.brief.attempts += 1;
        x.brief.state = "sending".into();
        true
    }) else {
        return;
    };
    let mut send = json!({"api_version":1,"message_id":mid,"session_id":session.id,
        "expected_generation":session.generation(),"text":text,"expires_at":r.deadline_at,"origin":"operator"});
    if r.provider == "claude" {
        send["mode"] = json!("submit");
        send["delivery"] = json!("terminal_input");
    } else {
        send["mode"] = json!("submit");
        send["if_busy"] = json!("queue");
    }
    if let Err(e) = crate::messaging::headless::dispatch(paths, send.to_string().as_bytes()).await {
        let _ = update(paths, &r.id, |x| {
            x.brief.reason = Some(e.to_string().chars().take(300).collect());
            true
        });
    }
}

/// The brief may be sent again under a new id only when the attempt provably wrote nothing:
/// a pre-submit refusal, or a receipt that does not exist.
fn resend_allowed(state: &str, receipt: Option<&Value>) -> bool {
    matches!(
        state,
        "rejected" | "failed" | "expired" | "cancelled" | "not_found"
    ) && receipt
        .is_none_or(|v| v["facts"]["written"] != true && v["facts"]["accepted_native"] != true)
}
fn not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<crate::messaging::Fault>()
        .is_some_and(|f| f.0 == "receipt_not_found")
}

/// `api capabilities.session_model.launch`.
pub fn capability() -> Value {
    json!({"command":"session launch","status":"session launch-status","cancel_operation":"cancel-operation",
        "idempotent":"request_id","request_providers":["codex","claude"],"model_required":true,
        "brief":{"default":"held until the model (and requested effort) is verified",
            "claude":{"default":"held","opt_in":"initial_prompt","max_bytes":{"initial_prompt":CLAUDE_INITIAL_MAX,"held":CLAUDE_HELD_MAX}},
            "codex":{"delivery":"first_turn","max_bytes":CODEX_MAX},
            "states":["pending","held","handed_to_provider","sending","delivered","not_sent","delivery_unknown"],
            "retries":MAX_BRIEF_ATTEMPTS,"retry_seconds":RETRY_SECONDS},
        "when_ready":"hold the brief until the model is verified (the default; explicit flag accepted)",
        "wait":{"flag":"--wait","default":0,"max":MAX_WAIT,"blocks_until":"complete or failed"},
        "deadline":{"default":DEFAULT_DEADLINE,"min":MIN_DEADLINE,"max":MAX_DEADLINE},
        "phases":["admitted","launching","verifying_model","delivering_brief","complete","failed","cancelled"],
        "errors":["MODEL_NOT_CONFIRMED","PROMPT_NOT_DELIVERED","PROVIDER_LAUNCH_FAILED","LAUNCH_ACK_UNCERTAIN"],
        "journal":crate::layout::SESSION_LAUNCHES})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_rules_follow_the_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
        assert!(check_brief(b"", "first_turn", &paths).is_err());
        assert!(check_brief(b"a\0b", "first_turn", &paths).is_err());
        assert!(check_brief(&[0xff, 0xfe], "first_turn", &paths).is_err());
        check_brief(b"do the thing", "first_turn", &paths).unwrap();
        assert!(
            check_brief(&vec![b'a'; CODEX_MAX + 1], "first_turn", &paths)
                .unwrap_err()
                .to_string()
                .starts_with("INPUT_TOO_LARGE")
        );
        assert!(check_brief(b"/model x", "initial_prompt", &paths).is_err());
        check_brief(b"plain brief", "initial_prompt", &paths).unwrap();
        assert!(
            check_brief(&vec![b'a'; CLAUDE_HELD_MAX + 1], "held", &paths)
                .unwrap_err()
                .to_string()
                .starts_with("TERMINAL_TEXT_TOO_LONG")
        );
        assert!(check_brief(b"a\tb", "held", &paths).is_err());
    }

    fn args(dir: &Path) -> Args {
        Args {
            space: "w1".into(),
            create_space: false,
            name: "n".into(),
            account: "claude:a".into(),
            model: "claude-fable-5-2".into(),
            effort: Some("high".into()),
            brief_file: dir.join("brief"),
            when_ready: false,
            initial_prompt: false,
            directory: Some(dir.to_path_buf()),
            create_directory: false,
            permissions: None,
            herdr_session: Some("default".into()),
            herdr_socket: Some(dir.join("herdr.sock")),
            request_id: None,
            deadline: None,
            wait: None,
            dry_run: false,
        }
    }

    #[test]
    fn request_carries_exact_model_and_a_confirmed_directory() {
        let dir = tempfile::tempdir().unwrap();
        let a = args(dir.path());
        let id = "33333333-3333-4333-8333-333333333333";
        let r = request(&a, id).unwrap();
        assert_eq!(r.model.as_deref(), Some("claude-fable-5-2"));
        assert_eq!(r.reasoning_effort.as_deref(), Some("high"));
        assert!(r.directory.confirmed_existing && r.directory.create.is_empty());
        assert_eq!(r.space, launch::Space::Existing { id: "w1".into() });
        let mut missing = args(dir.path());
        missing.directory = Some(dir.path().join("x/y"));
        assert!(
            request(&missing, id)
                .unwrap_err()
                .to_string()
                .starts_with("DIRECTORY_CONFIRMATION_REQUIRED")
        );
        missing.create_directory = true;
        let r = request(&missing, id).unwrap();
        assert_eq!(
            r.directory.create,
            [dir.path().join("x"), dir.path().join("x/y")]
        );
        assert!(!r.directory.confirmed_existing);
    }

    #[tokio::test]
    async fn admission_refuses_aliases_and_bad_combinations_before_any_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
        fs::write(dir.path().join("brief"), "brief").unwrap();
        let mut a = args(dir.path());
        a.model = "fable".into();
        let e = launch(&paths, a).await.unwrap_err().to_string();
        assert!(e.starts_with("INVALID_MODEL"), "{e}");
        let mut a = args(dir.path());
        a.when_ready = true;
        a.initial_prompt = true;
        assert!(
            launch(&paths, a)
                .await
                .unwrap_err()
                .to_string()
                .starts_with("INVALID_ARGUMENT")
        );
        let mut a = args(dir.path());
        a.account = "codex:c".into();
        a.model = "gpt-6-astra".into();
        a.effort = None;
        a.initial_prompt = true;
        assert!(
            launch(&paths, a)
                .await
                .unwrap_err()
                .to_string()
                .starts_with("INVALID_ARGUMENT")
        );
        let mut a = args(dir.path());
        a.account = "gemini:g".into();
        assert!(
            launch(&paths, a)
                .await
                .unwrap_err()
                .to_string()
                .starts_with("UNSUPPORTED_PROVIDER_OPERATION")
        );
        let mut a = args(dir.path());
        a.deadline = Some(5);
        assert!(
            launch(&paths, a)
                .await
                .unwrap_err()
                .to_string()
                .starts_with("INVALID_ARGUMENT")
        );
        let mut a = args(dir.path());
        a.request_id = Some("not-a-uuid".into());
        assert!(
            launch(&paths, a)
                .await
                .unwrap_err()
                .to_string()
                .starts_with("INVALID_REQUEST_ID")
        );
        assert!(
            !dir.path()
                .join("root")
                .join(crate::layout::SESSION_LAUNCHES)
                .exists()
        );
    }

    fn sending(paths: &Paths, deadline_in: i64) -> (String, String) {
        let id = uuid::Uuid::new_v4().to_string();
        let mid = uuid::Uuid::new_v4().to_string();
        let dir = tempfile::tempdir().unwrap();
        let now = crate::daemon::now();
        let r = Record {
            schema_version: 1,
            id: id.clone(),
            request: request(&args(dir.path()), &id).unwrap(),
            provider: "codex".into(),
            brief: Brief {
                delivery: "first_turn".into(),
                state: "sending".into(),
                sha256: "x".into(),
                bytes: 5,
                attempts: 1,
                message_id: Some(mid.clone()),
                message_ids: vec![mid.clone()],
                receipt_state: None,
                reason: None,
                next_attempt_at: 0,
            },
            model_requested: "gpt-6".into(),
            effort_requested: None,
            observed: None,
            model_confirmed: true,
            effort_confirmed: false,
            phase: "delivering_brief".into(),
            reason: None,
            detail: None,
            session_id: Some("s".into()),
            conversation_id: Some("c".into()),
            created_at: now,
            updated_at: now,
            deadline_at: now + deadline_in,
        };
        save(paths, &r).unwrap();
        fs::write(brief_file(paths, &id).unwrap(), "brief").unwrap();
        (id, mid)
    }

    #[test]
    fn a_brief_is_resent_only_when_provably_nothing_was_written() {
        for s in ["rejected", "failed", "expired", "cancelled"] {
            assert!(resend_allowed(
                s,
                Some(&json!({"state":s,"facts":{"written":false}}))
            ));
            assert!(!resend_allowed(
                s,
                Some(&json!({"state":s,"facts":{"written":true}}))
            ));
            assert!(!resend_allowed(
                s,
                Some(&json!({"state":s,"facts":{"accepted_native":true}}))
            ));
        }
        assert!(resend_allowed("not_found", None));
        for s in [
            "pending_local",
            "dispatching",
            "delivery_unknown",
            "unknown",
        ] {
            assert!(!resend_allowed(s, Some(&json!({"state":s}))));
        }
    }

    #[tokio::test]
    async fn an_unreadable_receipt_is_read_again_never_resent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
        let (id, mid) = sending(&paths, 300);
        // The ledger cannot be opened: the read fails for a reason other than "not found".
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(paths.root.join("messaging"), "not a directory").unwrap();
        advance(&paths, &id).await;
        let r = read(&paths, &id).unwrap();
        assert_eq!(r.brief.message_id.as_deref(), Some(mid.as_str()));
        assert_eq!((r.brief.state.as_str(), r.brief.attempts), ("sending", 1));
        assert_eq!(r.brief.message_ids.len(), 1, "no second send");
        assert_eq!(r.phase, "delivering_brief");
    }

    #[tokio::test]
    async fn a_receipt_that_does_not_exist_allows_one_new_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
        let (id, _) = sending(&paths, 300);
        advance(&paths, &id).await;
        let r = read(&paths, &id).unwrap();
        assert_eq!(r.brief.message_id, None);
        assert_eq!(r.brief.state, "held");
        assert_eq!(r.brief.receipt_state.as_deref(), Some("not_found"));
        assert!(r.brief.next_attempt_at > 0);
    }

    #[tokio::test]
    async fn a_deadline_during_a_send_is_delivery_unknown_not_unsent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
        let (id, _) = sending(&paths, -1);
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(paths.root.join("messaging"), "not a directory").unwrap();
        advance(&paths, &id).await;
        let r = read(&paths, &id).unwrap();
        assert_eq!(
            (r.phase.as_str(), r.reason.as_deref()),
            ("failed", Some("ready_timeout"))
        );
        assert_eq!(r.brief.state, "delivery_unknown");
        assert_eq!(r.brief.reason.as_deref(), Some("brief_delivery_unknown"));
        // Provably unsent at the deadline: an ordinary timeout with the brief not sent.
        let dir2 = tempfile::tempdir().unwrap();
        let paths2 = Paths::new(Some(dir2.path().join("root"))).unwrap();
        let (id2, _) = sending(&paths2, -1);
        advance(&paths2, &id2).await;
        let r2 = read(&paths2, &id2).unwrap();
        assert_eq!(r2.reason.as_deref(), Some("ready_timeout"));
        assert_eq!(r2.brief.state, "not_sent");
    }

    #[tokio::test]
    async fn an_admitted_record_waits_for_the_launch_ack_window() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
        let (id, _) = sending(&paths, 300);
        let mut r = read(&paths, &id).unwrap();
        r.phase = "admitted".into();
        r.created_at = crate::daemon::now() - 60;
        save(&paths, &r).unwrap();
        advance(&paths, &id).await;
        assert_eq!(read(&paths, &id).unwrap().phase, "admitted");
        r.created_at = crate::daemon::now() - ADMIT_GRACE_SECONDS - 1;
        save(&paths, &r).unwrap();
        advance(&paths, &id).await;
        let r = read(&paths, &id).unwrap();
        assert_eq!(
            (r.phase.as_str(), r.reason.as_deref()),
            ("failed", Some("launch_failed"))
        );
    }
}
