//! Submit, Queue and Steer for Claude sessions (contract revisions 16 and 17).
//!
//! Three delivery paths, chosen by the target session (hosted first, then
//! terminal input, then peer message; `delivery` in the request narrows it):
//! - **hosted** (`delivery_semantics:"hosted_stream_json"`): the session is run
//!   by a Sessionsation worker (`provider::claude::hosted`). The message is a
//!   stream-json stdin frame carrying `uuid` = message id and a priority
//!   (submit=`next`, queue=`later`, steer=`next`, optional interrupt first).
//!   Claude Code folds a `next` input into a running turn at its next tool
//!   round (or runs it as the next turn); `now` would CANCEL the running turn,
//!   so it is never sent. Correlation uses `command_lifecycle` events for that
//!   uuid and the `user_message_uuids` of the `result` that closes its turn;
//!   the reply is the assistant text streamed between `started` and that
//!   result. The terminal state needs both: a cut turn still emits a
//!   `success` result and only its `cancelled` lifecycle (after the result)
//!   tells it apart, so such inputs end `interrupted` with a non-final reply.
//! - **peer message** (`delivery_semantics:"peer_message"`): a live interactive
//!   session of an account that opted in (`crossSessionInbound: accept`). A
//!   detached worker owns a private listening socket, sends one inbound frame
//!   to the session's messaging socket and records `peer_message_status` /
//!   `peer_idle_notice` frames. Claude presents the text as a message from
//!   another session, which the model may only partially follow. The reply is
//!   read from the session transcript after the injected record (found by the
//!   message id), never claimed without that anchor.
//! - **terminal input** (`delivery_semantics:"terminal_input"`, revision 17):
//!   a tracked interactive session with a verified HERDR pane attachment. The
//!   text is typed into the pane as the user's own input; see [`terminal`].
//!
//! All paths journal before any write, keep durable receipts in
//! `DATA_ROOT/claude-steer/`, share the message-id namespace with the other
//! stores, and never replay an uncertain send.
use crate::{
    catalog,
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::{ProviderId, claude, claude::hosted},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, Read, Seek},
    path::{Path, PathBuf},
    time::Duration,
};

pub mod gemini;
pub mod terminal;

pub const PEER_SEMANTICS: &str = "peer_message";
pub const PEER_LABEL: &str = "Delivered as a Claude Code peer message: the session sees it as a message from another Claude session and may only partially comply";
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 600;
const MAX_REPLY_TEXT: usize = 64 * 1024;
const MAX_TRANSCRIPT_SCAN: u64 = 16 * 1024 * 1024;
const ADMITTED_GRACE_SECONDS: i64 = 10;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Steering {
    pub version: u32,
    #[serde(default)]
    pub interrupt: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub api_version: u32,
    pub message_id: String,
    pub mode: String,
    pub session_id: String,
    pub expected_generation: String,
    pub text: String,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering: Option<Steering>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    /// Delivery path: `auto` (default; terminal_input before peer_message),
    /// `terminal_input`, `peer_message` or `hosted_stream_json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
    /// Codex recovery intents do not apply; accepted only as absent/null.
    #[serde(default, skip_serializing)]
    pub recovery: Option<Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Facts {
    pub written: bool,
    pub lifecycle: Vec<String>,
    pub input_recorded: bool,
    pub folded: bool,
    pub shared_with_turn: bool,
    pub interrupt_requested: bool,
    pub peer_status: Vec<Value>,
    pub idle_notice: Option<Value>,
    /// The `result` naming this input arrived; the terminal state waits for
    /// its terminal `command_lifecycle` (`completed` or `cancelled`).
    #[serde(default)]
    pub result_seen: bool,
    /// An interrupt was requested while the consuming turn ran.
    #[serde(default)]
    pub turn_interrupted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub pid: u32,
    pub process_start: u64,
    pub socket: PathBuf,
    pub transcript: Option<PathBuf>,
    pub anchor_offset: Option<u64>,
    pub notify_msg_id: String,
    pub anchor_found: bool,
    pub turn_ended: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub schema_version: u32,
    /// hosted | peer | terminal
    pub kind: String,
    pub request: Request,
    pub session_id: String,
    pub conversation_id: String,
    pub account: String,
    pub accepted_generation: String,
    pub priority: String,
    pub host_epoch: Option<u64>,
    /// admitted -> dispatching -> written -> complete
    pub phase: String,
    pub state: String,
    pub reason: Option<String>,
    pub facts: Facts,
    pub reply_text: String,
    pub reply_truncated: bool,
    pub result: Option<Value>,
    pub peer: Option<Peer>,
    /// `gemini` for Gemini CLI sessions (terminal input only); absent for Claude.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Terminal-input delivery state (kind `terminal`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<terminal::Terminal>,
    pub observation_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub dispatched_at: Option<i64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub evidence_at: Option<i64>,
}

fn dir(paths: &Paths) -> PathBuf {
    paths.root.join("claude-steer")
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
fn records_lock(paths: &Paths) -> Result<fs::File> {
    private_dir(&dir(paths))?;
    let f = lock_file(&dir(paths).join("records.lock"))?;
    f.lock_exclusive()?;
    Ok(f)
}
/// Read-modify-write of one record under the shared records lock.
fn change<T>(paths: &Paths, id: &str, f: impl FnOnce(&mut Record) -> Result<T>) -> Result<T> {
    let _lock = records_lock(paths)?;
    let mut r = read(paths, id)?;
    let out = f(&mut r)?;
    save(paths, &mut r)?;
    Ok(out)
}
fn all(paths: &Paths) -> Vec<Record> {
    let Ok(entries) = fs::read_dir(dir(paths)) else {
        return vec![];
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| serde_json::from_slice(&fs::read(p).ok()?).ok())
        .collect()
}
fn is_terminal(r: &Record) -> bool {
    r.phase == "complete"
}
fn finish(r: &mut Record, state: &str, reason: Option<&str>) {
    if is_terminal(r) {
        return;
    }
    r.phase = "complete".into();
    r.state = state.into();
    r.reason = reason.map(str::to_owned);
    r.finished_at = Some(crate::daemon::now());
}
fn priority(mode: &str) -> &'static str {
    match mode {
        // Never `now`: Claude Code cancels the running turn for it.
        "queue" => "later",
        _ => "next",
    }
}
fn append_reply(r: &mut Record, text: &str) {
    if r.reply_truncated {
        return;
    }
    if !r.reply_text.is_empty() {
        r.reply_text.push_str("\n\n");
    }
    r.reply_text.push_str(text);
    if r.reply_text.len() > MAX_REPLY_TEXT {
        let mut end = MAX_REPLY_TEXT;
        while !r.reply_text.is_char_boundary(end) {
            end -= 1;
        }
        r.reply_text.truncate(end);
        r.reply_truncated = true;
    }
}

// ---------------------------------------------------------------- admission

fn shape(r: &Request) -> Result<()> {
    ensure!(
        r.api_version == crate::api::VERSION,
        "UNSUPPORTED_API_VERSION"
    );
    uuid::Uuid::parse_str(&r.message_id).context("INVALID_REQUEST")?;
    ensure!(
        matches!(r.mode.as_str(), "submit" | "queue" | "steer"),
        "INVALID_REQUEST"
    );
    ensure!(
        !r.text.is_empty() && r.text.len() <= 16384,
        "INVALID_REQUEST"
    );
    ensure!(
        r.recovery.as_ref().is_none_or(Value::is_null),
        "UNSUPPORTED_PROVIDER_OPERATION: Claude sessions take no Codex recovery intent"
    );
    if let Some(s) = &r.steering {
        ensure!(
            s.version == 1,
            "INVALID_REQUEST: steering.version must be 1"
        );
        ensure!(r.mode == "steer", "STEERING_ONLY_VALID_WITH_STEER");
    }
    ensure!(
        matches!(
            r.delivery.as_deref(),
            None | Some("auto" | "hosted_stream_json" | "terminal_input" | "peer_message")
        ),
        "INVALID_REQUEST: delivery must be auto, hosted_stream_json, terminal_input or peer_message"
    );
    ensure!(
        (1..=3600).contains(&r.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS)),
        "INVALID_TIMEOUT: timeout_seconds must be 1..=3600"
    );
    Ok(())
}

/// Admit one Claude submit/queue/steer request (raw `api send` JSON).
pub async fn send(paths: &Paths, v: Value) -> Result<Value> {
    let request: Request = serde_json::from_value(v.clone()).map_err(|e| {
        if v["steering"].get("expected_turn_id").is_some() {
            anyhow::anyhow!("INVALID_REQUEST: Claude steering takes {{\"version\":1,\"interrupt\":bool}}, not a Codex turn target")
        } else if exists(paths, v["message_id"].as_str().unwrap_or_default()) {
            anyhow::anyhow!("MESSAGE_ID_CONFLICT")
        } else {
            anyhow::anyhow!("INVALID_REQUEST: {e}")
        }
    })?;
    shape(&request)?;
    let id = request.message_id.clone();
    private_dir(&dir(paths))?;
    let admission = lock_file(&dir(paths).join("admission.lock"))?;
    admission.lock_exclusive()?;
    if exists(paths, &id) {
        let old = read(paths, &id)?;
        ensure!(old.request == request, "MESSAGE_ID_CONFLICT");
        drop(admission);
        return receipt(paths, &id, false).await;
    }
    ensure!(!super::headless::exists(paths, &id), "MESSAGE_ID_CONFLICT");
    super::headless::ensure_unused_elsewhere(paths, &id)?;
    let now = crate::daemon::now();
    ensure!(
        request.expires_at > now && request.expires_at <= now + 86400,
        "INVALID_EXPIRY"
    );
    ensure!(all(paths).len() < 10000, "RECEIPT_CAPACITY_REACHED");
    let session = catalog::resolve(paths, &request.session_id, &request.expected_generation)?;
    let is_gemini = session.provider == ProviderId::Gemini.as_str();
    ensure!(
        is_gemini || session.provider == ProviderId::Claude.as_str(),
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
    let account = super::headless::usable_account(
        paths,
        if is_gemini {
            ProviderId::Gemini
        } else {
            ProviderId::Claude
        },
        &session.account,
    )?;
    crate::lifecycle::ensure_available(paths, &session.conversation_id, None)?;
    let c = session.conversation_id.clone();
    let interrupt = request.steering.as_ref().is_some_and(|s| s.interrupt);
    let wanted = request.delivery.as_deref().unwrap_or("auto");
    let mut typed = None;
    let (kind, epoch, peer) = if is_gemini {
        ensure!(terminal::enabled(paths), "{}", terminal::DISABLED);
        typed = Some(gemini::admit(&request, &session, &account.home)?);
        ("terminal", None, None)
    } else if let Some(h) = hosted::accepting(paths, &c) {
        ensure!(
            matches!(wanted, "auto" | "hosted_stream_json"),
            "DELIVERY_UNAVAILABLE: this session is hosted; use delivery hosted_stream_json or auto"
        );
        ensure!(
            h.session_id == session.id,
            "STALE_TARGET: the hosted worker serves another catalog entry"
        );
        ("hosted", Some(h.epoch), None)
    } else {
        ensure!(
            !hosted::live(paths, &c),
            "SESSION_OPERATION_IN_PROGRESS: the hosted worker of this session is starting or stopping"
        );
        ensure!(
            wanted != "hosted_stream_json",
            "DELIVERY_UNAVAILABLE: this session is not hosted"
        );
        let observation = claude::observe(&account.home, &c, session.attachment.as_ref());
        ensure!(
            observation.availability == "running",
            "SESSION_NOT_RUNNING: submit/queue/steer need a hosted or live Claude session; use mode resume for a stopped session"
        );
        let attached = if terminal::enabled(paths) {
            terminal::eligible(&observation)
        } else {
            Err("TERMINAL_INPUT_DISABLED")
        };
        let peer_on = claude::peer::enabled(&account.home);
        // Terminal input is preferred over a peer message. Queue (`later`) and
        // interrupts cannot be typed: with `auto` a queue uses the peer path
        // when it is enabled.
        let use_terminal = match wanted {
            "terminal_input" => true,
            "peer_message" => false,
            _ => attached.is_ok() && !(request.mode == "queue" && peer_on),
        };
        if use_terminal {
            let t = attached.map_err(|code| {
                if code == "TERMINAL_INPUT_DISABLED" {
                    anyhow::anyhow!("{}", terminal::DISABLED)
                } else {
                    anyhow::anyhow!(
                        "{code}: terminal input needs a running session attached to a verified HERDR pane"
                    )
                }
            })?;
            ensure!(
                request.mode != "queue",
                "TERMINAL_QUEUE_UNSUPPORTED: typed input runs at the next tool boundary or as the next turn; queue needs a peer message (account live-steering --enable)"
            );
            ensure!(
                !interrupt,
                "TERMINAL_INTERRUPT_UNSUPPORTED: terminal input never sends Esc or Ctrl+C; omit steering.interrupt"
            );
            typed = Some(terminal::prepare(&request, t)?);
            ("terminal", None, None)
        } else {
            ensure!(
                !interrupt,
                "PEER_INTERRUPT_UNSUPPORTED: peer messages cannot interrupt a live session; omit steering.interrupt"
            );
            ensure!(peer_on, "{}", claude::peer::DISABLED);
            let live = claude::live_sessions(&account.home)?
                .into_iter()
                .find(|l| l.session_id == c)
                .context("SESSION_NOT_RUNNING")?;
            let endpoint = claude::peer::endpoint(&account.home, &live)?;
            let transcript = claude::find_transcript(&account.home, &c);
            (
                "peer",
                None,
                Some(Peer {
                    pid: endpoint.pid,
                    process_start: endpoint.process_start,
                    socket: endpoint.socket,
                    anchor_offset: None,
                    transcript,
                    notify_msg_id: uuid::Uuid::new_v4().to_string(),
                    anchor_found: false,
                    turn_ended: false,
                }),
            )
        }
    };
    let mut record = Record {
        schema_version: 1,
        kind: kind.into(),
        priority: priority(&request.mode).into(),
        session_id: session.id.clone(),
        conversation_id: c.clone(),
        account: account.name.clone(),
        accepted_generation: session.generation(),
        host_epoch: epoch,
        phase: "admitted".into(),
        state: "pending_local".into(),
        reason: None,
        facts: Facts {
            interrupt_requested: interrupt,
            ..Default::default()
        },
        reply_text: String::new(),
        reply_truncated: false,
        result: None,
        peer,
        provider: is_gemini.then(|| "gemini".to_owned()),
        terminal: typed,
        observation_error: None,
        created_at: now,
        updated_at: now,
        dispatched_at: None,
        started_at: None,
        finished_at: None,
        evidence_at: None,
        request,
    };
    // Journal before any write to a provider.
    save(paths, &mut record)?;
    drop(admission);
    if kind == "hosted" {
        let answer = hosted::call(
            paths,
            &c,
            json!({"op":"deliver","message_id":id}),
            Duration::from_secs(10),
        )
        .await;
        if answer.is_err() {
            // Unsent unless the worker already took it (then its own journal rules).
            abandon_unsent(paths, &id, "HOST_UNREACHABLE_NOT_SENT")?;
        }
    } else {
        let spawned = crate::background::detach(&mut std::process::Command::new(
            crate::self_executable::path()?,
        ))
        .arg("--data-dir")
        .arg(&paths.root)
        .arg(if is_gemini {
            "gemini-terminal-worker"
        } else if kind == "terminal" {
            "claude-terminal-worker"
        } else {
            "claude-peer-worker"
        })
        .arg(&id)
        .spawn();
        if spawned.is_err() {
            abandon_unsent(paths, &id, "WORKER_COULD_NOT_START_NO_PROVIDER_ACTION")?;
        }
        for _ in 0..40 {
            if read(paths, &id)?.phase != "admitted" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    receipt(paths, &id, false).await
}

/// A provably unsent (still `admitted`) request becomes terminal `rejected`.
pub fn abandon_unsent(paths: &Paths, id: &str, reason: &str) -> Result<()> {
    change(paths, id, |r| {
        if r.phase == "admitted" {
            finish(r, "rejected", Some(reason));
        }
        Ok(())
    })
}

// ------------------------------------------------------------ hosted events

/// Worker: claim an admitted request for writing; returns the stdin frames.
pub fn hosted_begin(paths: &Paths, id: &str, c: &str, epoch: u64) -> Result<Option<Vec<Value>>> {
    change(paths, id, |r| {
        ensure!(r.kind == "hosted" && r.conversation_id == c, "STALE_TARGET");
        if r.phase != "admitted" {
            return Ok(None);
        }
        if r.request.expires_at <= crate::daemon::now() {
            finish(r, "expired", Some("EXPIRED_BEFORE_DELIVERY"));
            return Ok(None);
        }
        r.phase = "dispatching".into();
        r.host_epoch = Some(epoch);
        r.dispatched_at = Some(crate::daemon::now());
        let mut frames = vec![];
        if r.facts.interrupt_requested {
            frames.push(
                json!({"type":"control_request","request_id":uuid::Uuid::new_v4().to_string(),
                "request":{"subtype":"interrupt"}}),
            );
        }
        frames.push(json!({"type":"user","message":{"role":"user","content":r.request.text},
            "uuid":r.request.message_id,"priority":r.priority,"parent_tool_use_id":null,"session_id":c}));
        Ok(Some(frames))
    })
}
pub fn hosted_written(paths: &Paths, id: &str, ok: bool) -> Result<()> {
    change(paths, id, |r| {
        if r.phase == "dispatching" {
            if ok {
                r.phase = "written".into();
                r.facts.written = true;
            } else {
                // Part of the frame may have reached the provider.
                finish(r, "delivery_unknown", Some("STDIN_WRITE_FAILED"));
            }
        }
        Ok(())
    })
}
fn hosted_record(paths: &Paths, c: &str, id: &str) -> Option<Record> {
    uuid::Uuid::parse_str(id).ok()?;
    read(paths, id)
        .ok()
        .filter(|r| r.kind == "hosted" && r.conversation_id == c)
}
pub(crate) fn hosted_lifecycle(
    paths: &Paths,
    c: &str,
    id: &str,
    state: &str,
    turns: &mut hosted::Turns,
) -> Result<()> {
    if state == "started" {
        // A new turn (or fold) proves the earlier turns' lifecycles were sent.
        settle_awaiting(paths, c, turns, Some(id))?;
    }
    if hosted_record(paths, c, id).is_none() {
        return Ok(());
    }
    let folded = !turns.active.is_empty();
    if state == "started" && !turns.active.iter().any(|a| a == id) {
        turns.active.push(id.to_owned());
    }
    if matches!(state, "completed" | "cancelled" | "discarded" | "refused") {
        turns.awaiting.retain(|a| a != id);
    }
    if matches!(state, "cancelled" | "discarded" | "refused") {
        turns.active.retain(|a| a != id);
    }
    let interrupted = turns.interrupted;
    change(paths, id, |r| {
        r.facts.lifecycle.push(state.into());
        r.evidence_at = Some(crate::daemon::now());
        if is_terminal(r) {
            return Ok(());
        }
        let started = r.facts.lifecycle.iter().any(|s| s == "started");
        match state {
            "queued" if r.state == "pending_local" => r.state = "native_accepted".into(),
            "started" => {
                r.state = "execution_started".into();
                r.started_at = Some(crate::daemon::now());
                r.facts.input_recorded = true;
                r.facts.folded = folded;
            }
            // Folded inputs complete before the result; wait for it.
            "completed" if r.facts.result_seen => settle(r),
            // Consumed by a turn that was cut (interrupt, a `now` input of
            // another client) or died: never `completed`, reply not final.
            "cancelled" | "discarded" if started => {
                r.facts.turn_interrupted |= interrupted;
                interrupt_end(r);
            }
            "cancelled" => finish(r, "cancelled", Some("NATIVE_CANCELLED")),
            "discarded" => finish(r, "cancelled", Some("NATIVE_DISCARDED")),
            "refused" => finish(r, "refused", Some("NATIVE_REFUSED")),
            _ => {}
        }
        Ok(())
    })
}
fn result_is_error(r: &Record) -> bool {
    r.result.as_ref().is_some_and(|v| {
        v["is_error"] == true
            || v["subtype"]
                .as_str()
                .is_some_and(|s| s.starts_with("error"))
    })
}
/// Terminal state of an input whose consuming turn was cut after it started.
fn interrupt_end(r: &mut Record) {
    if r.facts.turn_interrupted {
        finish(r, "interrupted", Some("TURN_INTERRUPTED"));
    } else if result_is_error(r) {
        finish(r, "failed", Some("PROVIDER_REPORTED_ERROR"));
    } else {
        finish(r, "interrupted", Some("TURN_CANCELLED_BY_PROVIDER"));
    }
}
/// Terminal state once both the result and a `completed` lifecycle are known.
fn settle(r: &mut Record) {
    if r.facts.turn_interrupted && result_is_error(r) {
        finish(r, "interrupted", Some("TURN_INTERRUPTED"));
    } else if result_is_error(r) {
        finish(r, "failed", Some("PROVIDER_REPORTED_ERROR"));
    } else if r.reply_text.is_empty() {
        finish(r, "completed_without_reply", None);
    } else {
        finish(r, "completed_with_reply", None);
    }
}
/// Inputs whose result arrived but whose terminal lifecycle never did, once a
/// later turn shows that none will come (a turn that threw, older producers):
/// settled from the result. A cut turn always sends `cancelled` first.
fn settle_awaiting(
    paths: &Paths,
    c: &str,
    turns: &mut hosted::Turns,
    except: Option<&str>,
) -> Result<()> {
    let waiting: Vec<String> = turns
        .awaiting
        .iter()
        .filter(|a| Some(a.as_str()) != except)
        .cloned()
        .collect();
    turns.awaiting.retain(|a| Some(a.as_str()) == except);
    for id in waiting {
        if hosted_record(paths, c, &id).is_some() {
            change(paths, &id, |r| {
                if !is_terminal(r) && r.facts.result_seen {
                    settle(r);
                }
                Ok(())
            })?;
        }
    }
    Ok(())
}
pub(crate) fn hosted_echo(paths: &Paths, c: &str, id: &str) -> Result<()> {
    if hosted_record(paths, c, id).is_none() {
        return Ok(());
    }
    change(paths, id, |r| {
        r.facts.input_recorded = true;
        if r.state == "pending_local" && !is_terminal(r) {
            r.state = "native_accepted".into();
        }
        Ok(())
    })
}
pub(crate) fn hosted_assistant(
    paths: &Paths,
    c: &str,
    text: &str,
    turns: &mut hosted::Turns,
) -> Result<()> {
    let shared = turns.active.len() > 1;
    for id in turns.active.clone() {
        if hosted_record(paths, c, &id).is_none() {
            continue;
        }
        change(paths, &id, |r| {
            if !is_terminal(r) {
                append_reply(r, text);
                r.facts.shared_with_turn |= shared;
            }
            Ok(())
        })?;
    }
    Ok(())
}
pub(crate) fn hosted_result(
    paths: &Paths,
    c: &str,
    v: &Value,
    turns: &mut hosted::Turns,
) -> Result<()> {
    let listed: Option<Vec<String>> = v["user_message_uuids"].as_array().map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    });
    let ids = listed.unwrap_or_else(|| turns.active.clone());
    // Inputs of an earlier result still without a lifecycle terminal.
    settle_awaiting(paths, c, turns, None)?;
    let interrupted = std::mem::take(&mut turns.interrupted);
    let result = json!({"subtype":v["subtype"],"is_error":v["is_error"],"num_turns":v["num_turns"],
        "duration_ms":v["duration_ms"],"usage":v["usage"],"session_id":v["session_id"],
        "user_message_uuids":v["user_message_uuids"]});
    let shared = ids.len() > 1;
    for id in &ids {
        turns.active.retain(|a| a != id);
        if hosted_record(paths, c, id).is_none() {
            continue;
        }
        change(paths, id, |r| {
            if is_terminal(r) {
                return Ok(());
            }
            r.result = Some(result.clone());
            r.facts.result_seen = true;
            r.facts.turn_interrupted |= interrupted;
            r.facts.shared_with_turn |= shared;
            r.facts.input_recorded = true;
            r.evidence_at = Some(crate::daemon::now());
            if v["session_id"].as_str().is_some_and(|s| s != c) {
                finish(r, "delivery_unknown", Some("SESSION_ID_MISMATCH"));
                return Ok(());
            }
            if r.reply_text.is_empty()
                && ids.len() == 1
                && !result_is_error(r)
                && let Some(text) = v["result"].as_str().filter(|t| !t.is_empty())
            {
                append_reply(r, text);
            }
            // A success result alone is not completion: a turn cut by an
            // interrupt or a `now` input also ends with one, followed by
            // `cancelled`. A fresh turn's `completed` follows the result; a
            // folded input's `completed` came before it.
            if r.facts.lifecycle.iter().any(|s| s == "completed") {
                settle(r);
            } else if !turns.awaiting.iter().any(|a| a == id) {
                turns.awaiting.push(id.clone());
            }
            Ok(())
        })?;
    }
    Ok(())
}
/// The host of epoch `epoch` ended: in-flight deliveries are uncertain (or
/// failed when their turn had started), never replayed.
pub fn hosted_exited(paths: &Paths, c: &str, epoch: u64, reason: &str) -> Result<()> {
    for r in all(paths) {
        if r.kind != "hosted"
            || r.conversation_id != c
            || r.host_epoch != Some(epoch)
            || is_terminal(&r)
        {
            continue;
        }
        change(paths, &r.request.message_id, |r| {
            if is_terminal(r) || r.phase == "admitted" {
                return Ok(());
            }
            if r.facts.result_seen {
                // The turn ended but its completed/cancelled never arrived.
                let why = format!("{reason}_BEFORE_TURN_OUTCOME");
                finish(r, "delivery_unknown", Some(&why));
            } else if r.state == "execution_started" {
                let why = format!("{reason}_DURING_TURN");
                finish(r, "failed", Some(&why));
            } else {
                finish(r, "delivery_unknown", Some(reason));
            }
            Ok(())
        })?;
    }
    Ok(())
}

// ------------------------------------------------------------- peer worker

/// Detached worker for one peer message: owns the listening socket, sends the
/// frame once and records the session's status/idle frames.
pub async fn peer_worker(paths: &Paths, id: &str) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let lock = lock_file(&worker_lock(paths, id)?)?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    let r = read(paths, id)?;
    if r.phase != "admitted" || r.kind != "peer" {
        return Ok(());
    }
    if r.request.expires_at <= crate::daemon::now() {
        return change(paths, id, |r| {
            finish(r, "expired", Some("EXPIRED_BEFORE_DELIVERY"));
            Ok(())
        });
    }
    let checked = peer_recheck(paths, &r);
    let peer = match checked {
        Ok(p) => p,
        Err(e) => {
            let reason = e
                .to_string()
                .split(':')
                .next()
                .unwrap_or("REJECTED")
                .to_owned();
            return abandon_unsent(paths, id, &reason);
        }
    };
    let listen = paths.runtime.join("peer").join(format!("{id}.sock"));
    private_dir(listen.parent().context("socket parent")?)?;
    let _ = fs::remove_file(&listen);
    let listener = tokio::net::UnixListener::bind(&listen)?;
    let from = format!("uds:{}", listen.display());
    // Anchor: transcript size before the write; the injected record follows it.
    let anchor = peer
        .transcript
        .as_ref()
        .and_then(|p| fs::metadata(p).ok())
        .map(|m| m.len());
    change(paths, id, |r| {
        r.phase = "dispatching".into();
        r.dispatched_at = Some(crate::daemon::now());
        if let Some(p) = r.peer.as_mut() {
            p.transcript = peer.transcript.clone();
            p.anchor_offset = anchor;
        }
        Ok(())
    })?;
    let stream = match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::UnixStream::connect(&peer.socket),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => {
            // Nothing was written: the connection never opened.
            let _ = fs::remove_file(&listen);
            return change(paths, id, |r| {
                finish(r, "rejected", Some("PEER_SOCKET_UNREACHABLE"));
                Ok(())
            });
        }
    };
    let (read_half, mut write_half) = stream.into_split();
    let frame = json!({"type":"user","message":{"role":"user","content":r.request.text},
        "uuid":id,"msg_id":id,"from":from,"priority":r.priority,"session_id":r.conversation_id});
    let notify = json!({"type":"notify_when_idle","from":from,"msg_id":peer.notify_msg_id});
    let wrote = async {
        write_half
            .write_all(format!("{frame}\n{notify}\n").as_bytes())
            .await?;
        write_half.flush().await
    }
    .await;
    change(paths, id, |r| {
        if wrote.is_ok() {
            r.phase = "written".into();
            r.facts.written = true;
        } else {
            finish(r, "delivery_unknown", Some("PEER_WRITE_FAILED"));
        }
        Ok(())
    })?;
    if wrote.is_err() {
        let _ = fs::remove_file(&listen);
        return Ok(());
    }
    // Control frames may arrive on our listener (`from`) or the open connection.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(32);
    let lines = |reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
                 tx: tokio::sync::mpsc::Sender<Value>| async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.len() > 64 * 1024 {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && tx.send(v).await.is_err()
            {
                break;
            }
        }
    };
    tokio::spawn(lines(Box::new(read_half), tx.clone()));
    let accept_tx = tx.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            tokio::spawn(lines(Box::new(s), accept_tx.clone()));
        }
    });
    drop(tx);
    let timeout = r.request.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    loop {
        let frame = tokio::select! {
            v = rx.recv() => v,
            _ = tokio::time::sleep_until(deadline) => None,
        };
        let Some(v) = frame else { break };
        peer_frame(paths, id, &peer.notify_msg_id, &v)?;
        // Frames arrive on separate connections in any order: an idle notice
        // ends listening only together with a status (or after a short grace).
        let r = read(paths, id)?;
        if is_terminal(&r) || (r.facts.idle_notice.is_some() && !r.facts.peer_status.is_empty()) {
            break;
        }
        if r.facts.idle_notice.is_some() {
            deadline = deadline.min(tokio::time::Instant::now() + Duration::from_secs(3));
        }
    }
    let _ = peer_observe(paths, id);
    drop(write_half);
    let _ = fs::remove_file(&listen);
    Ok(())
}

/// Recheck the target before sending: same verified process and endpoint, the
/// opt-in still on, no hosted worker. Returns the endpoint data.
fn peer_recheck(paths: &Paths, r: &Record) -> Result<Peer> {
    let account = super::headless::usable_account(paths, ProviderId::Claude, &r.account)?;
    let session = catalog::find(paths, &r.session_id)?;
    ensure!(session.conversation_id == r.conversation_id, "STALE_TARGET");
    hosted::ensure_not_hosted(paths, &r.conversation_id)?;
    ensure!(
        claude::peer::enabled(&account.home),
        "{}",
        claude::peer::DISABLED
    );
    let recorded = r.peer.clone().context("STALE_TARGET")?;
    let live = claude::live_sessions(&account.home)?
        .into_iter()
        .find(|l| l.session_id == r.conversation_id)
        .context("SESSION_NOT_RUNNING")?;
    ensure!(
        live.pid == recorded.pid && live.process_start == recorded.process_start,
        "STALE_TARGET: the live Claude process changed"
    );
    let endpoint = claude::peer::endpoint(&account.home, &live)?;
    ensure!(endpoint.socket == recorded.socket, "STALE_TARGET");
    Ok(Peer {
        transcript: claude::find_transcript(&account.home, &r.conversation_id),
        ..recorded
    })
}

/// Record one control frame.
fn peer_frame(paths: &Paths, id: &str, notify: &str, v: &Value) -> Result<()> {
    let ty = v["type"].as_str().unwrap_or_default();
    let orig = v["orig_msg_id"].as_str().unwrap_or_default();
    match ty {
        "peer_message_status" if orig == id => {
            let status = v["status"].as_str().unwrap_or_default().to_owned();
            change(paths, id, |r| {
                if r.facts.peer_status.len() < 32 {
                    r.facts
                        .peer_status
                        .push(json!({"status":status,"at":crate::daemon::now()}));
                }
                r.evidence_at = Some(crate::daemon::now());
                if is_terminal(r) {
                    return Ok(());
                }
                match status.as_str() {
                    "delivered" if matches!(r.state.as_str(), "pending_local" | "held") => {
                        r.state = "native_accepted".into();
                    }
                    "held" if r.state == "pending_local" => r.state = "held".into(),
                    "denied" => finish(r, "refused", Some("PEER_MESSAGE_DENIED")),
                    "refused" => finish(r, "refused", Some("PEER_MESSAGE_REFUSED")),
                    "dropped" => finish(r, "refused", Some("PEER_MESSAGE_DROPPED")),
                    "expired" => finish(r, "expired", Some("PEER_MESSAGE_EXPIRED")),
                    _ => {}
                }
                Ok(())
            })?;
            Ok(())
        }
        "peer_idle_notice" if orig == notify || orig == id => {
            change(paths, id, |r| {
                r.facts.idle_notice =
                    Some(json!({"state":v["state"],"finished_at":v["finished_at"],
                    "received_at":crate::daemon::now()}));
                Ok(())
            })?;
            peer_observe(paths, id)
        }
        _ => Ok(()),
    }
}

/// Result of reading the transcript after the anchor.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Scan {
    pub anchor_found: bool,
    pub reply: Vec<String>,
    pub turn_ended: bool,
}

/// Read the session transcript from `offset`: find the record that carries the
/// message id (the injected peer message), then collect assistant text until
/// the next real input. Reads at most 16 MiB; user text is never retained.
pub(crate) fn scan(path: &Path, offset: u64, id: &str) -> Result<Scan> {
    let mut f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = if offset <= len { offset } else { 0 };
    f.seek(std::io::SeekFrom::Start(start))?;
    let reader = std::io::BufReader::new(f.take(MAX_TRANSCRIPT_SCAN));
    let mut out = Scan::default();
    for line in reader.split(b'\n') {
        let line = line?;
        if !out.anchor_found {
            let hit = line.windows(id.len()).any(|w| w == id.as_bytes());
            if hit
                && let Ok(v) = serde_json::from_slice::<Value>(&line)
                && v["type"] == "user"
            {
                out.anchor_found = true;
            }
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match v["type"].as_str() {
            Some("assistant") => {
                let text = hosted::text_blocks(&v["message"]["content"]).join("");
                if !text.is_empty() {
                    out.reply.push(text);
                }
            }
            Some("user") => {
                let content = &v["message"]["content"];
                let tool_only = content
                    .as_array()
                    .is_some_and(|a| !a.is_empty() && a.iter().all(|b| b["type"] == "tool_result"));
                if !tool_only {
                    out.turn_ended = true;
                    break;
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Positive transcript evidence for a delivered peer message (never regressed).
fn peer_observe(paths: &Paths, id: &str) -> Result<()> {
    let r = read(paths, id)?;
    let Some(peer) = r.peer.clone() else {
        return Ok(());
    };
    if is_terminal(&r) || !r.facts.written {
        return Ok(());
    }
    let transcript = peer.transcript.clone().or_else(|| {
        let account = claude::account(paths, &r.account).ok()?;
        claude::find_transcript(&account.home, &r.conversation_id)
    });
    let Some(path) = transcript else {
        return change(paths, id, |r| {
            r.observation_error = Some("transcript_unavailable".into());
            Ok(())
        });
    };
    let scanned = scan(&path, peer.anchor_offset.unwrap_or(0), id);
    change(paths, id, |r| {
        if is_terminal(r) {
            return Ok(());
        }
        let Ok(s) = scanned else {
            r.observation_error = Some("transcript_unreadable".into());
            return Ok(());
        };
        let Some(p) = r.peer.as_mut() else {
            return Ok(());
        };
        p.transcript = Some(path.clone());
        if !s.anchor_found {
            r.observation_error = Some("peer_anchor_not_found".into());
            return Ok(());
        }
        r.observation_error = None;
        p.anchor_found = true;
        p.turn_ended |= s.turn_ended;
        let ended = p.turn_ended || r.facts.idle_notice.is_some();
        r.facts.input_recorded = true;
        r.evidence_at = Some(crate::daemon::now());
        r.reply_text.clear();
        r.reply_truncated = false;
        for text in &s.reply {
            append_reply(r, text);
        }
        if r.started_at.is_none() {
            r.started_at = Some(crate::daemon::now());
        }
        if ended {
            if r.reply_text.is_empty() {
                finish(r, "completed_without_reply", None);
            } else {
                finish(r, "completed_with_reply", None);
            }
        } else {
            r.state = "execution_started".into();
        }
        Ok(())
    })
}

// ------------------------------------------------------------------ receipt

/// Projection of a stale record (worker gone) to an honest state, persisted.
fn reconcile(paths: &Paths, id: &str) -> Result<Record> {
    let r = read(paths, id)?;
    if is_terminal(&r) {
        return Ok(r);
    }
    let age = crate::daemon::now() - r.updated_at;
    match r.kind.as_str() {
        "hosted" => {
            hosted::reconcile(paths, &r.conversation_id)?;
            let r = read(paths, id)?;
            if r.phase == "admitted" && age > ADMITTED_GRACE_SECONDS {
                let accepting = hosted::accepting(paths, &r.conversation_id)
                    .is_some_and(|h| Some(h.epoch) == r.host_epoch);
                if !accepting || r.request.expires_at <= crate::daemon::now() {
                    abandon_unsent(paths, id, "HOST_STOPPED_BEFORE_DELIVERY")?;
                }
            }
        }
        _ => {
            let stopped = crate::background::worker_stopped(&worker_lock(paths, id)?)?;
            let typed = r.kind == "terminal";
            if r.facts.written && typed && gemini::is_gemini(&r) {
                gemini::observe(paths, id, false)?;
                if stopped {
                    gemini::unconfirmed_after_exit(paths, id)?;
                }
            } else if r.facts.written && typed {
                terminal::observe(paths, id, false)?;
                if stopped {
                    terminal::unconfirmed_after_exit(paths, id)?;
                }
            } else if r.facts.written {
                peer_observe(paths, id)?;
            } else if stopped && age > ADMITTED_GRACE_SECONDS {
                change(paths, id, |r| {
                    // Enter is journaled (`submit`) before it is sent.
                    let submitting = r
                        .terminal
                        .as_ref()
                        .is_some_and(|t| t.steps.iter().any(|s| s == "submit"));
                    match r.phase.as_str() {
                        "admitted" => finish(r, "rejected", Some("WORKER_EXITED_BEFORE_DELIVERY")),
                        "dispatching" if typed && !submitting => {
                            finish(r, "rejected", Some("TERMINAL_WORKER_EXITED_BEFORE_SUBMIT"))
                        }
                        "dispatching" => {
                            finish(r, "delivery_unknown", Some("WORKER_EXITED_DURING_DELIVERY"))
                        }
                        _ => {}
                    }
                    Ok(())
                })?;
            }
        }
    }
    read(paths, id)
}

fn semantics_of(r: &Record) -> &'static str {
    match r.kind.as_str() {
        "peer" => PEER_SEMANTICS,
        "terminal" => terminal::SEMANTICS,
        _ => hosted::SEMANTICS,
    }
}

pub fn receipt_value(r: &Record) -> Value {
    let provider = r.provider.as_deref().unwrap_or("claude");
    let peer = r.kind == "peer";
    let completed = matches!(
        r.state.as_str(),
        "completed_with_reply" | "completed_without_reply"
    );
    let accepted = r.facts.input_recorded
        || r.facts
            .lifecycle
            .iter()
            .any(|s| matches!(s.as_str(), "queued" | "started"))
        || r.facts
            .peer_status
            .iter()
            .any(|s| s["status"] == "delivered");
    let reply_observed = !r.reply_text.is_empty()
        && (r.facts.input_recorded || r.peer.as_ref().is_some_and(|p| p.anchor_found));
    let consumed = if r.facts.input_recorded
        && (completed
            || matches!(
                r.state.as_str(),
                "execution_started" | "failed" | "interrupted"
            )) {
        json!(true)
    } else {
        json!("unknown")
    };
    let correlation = if peer {
        r.peer.as_ref().map(|p| {
            json!({"kind":"transcript_anchor","message_id":r.request.message_id,
            "anchor_found":p.anchor_found,"anchor_offset":p.anchor_offset,"turn_ended":p.turn_ended,
            "notify_msg_id":p.notify_msg_id})
        })
    } else {
        (r.facts.written || !r.facts.lifecycle.is_empty()).then(|| json!({"kind":"command_uuid","command_uuid":r.request.message_id,
            "lifecycle":r.facts.lifecycle,"user_message_uuids":r.result.as_ref().map(|x| x["user_message_uuids"].clone())}))
    };
    let reply = reply_observed.then(|| {
        let mut v = json!({"text":r.reply_text,"text_truncated":r.reply_truncated,"final":completed,
            "shared_with_turn":r.facts.shared_with_turn});
        if let Some(result) = &r.result {
            for k in ["is_error", "subtype", "num_turns", "duration_ms", "usage"] {
                v[k] = result[k].clone();
            }
        }
        v
    });
    let observation_state = if is_terminal(r) {
        "cached"
    } else if r.observation_error.is_some() {
        "unavailable"
    } else {
        "observed"
    };
    let mut v = json!({"message_id":r.request.message_id,"session_id":r.session_id,"accepted_generation":r.accepted_generation,
        "provider":provider,"provider_conversation_id":r.conversation_id,"account_id":format!("{provider}:{}",r.account),
        "mode":r.request.mode,"priority":r.priority,
        "delivery_semantics":semantics_of(r),
        "state":r.state,"reason":r.reason,
        "facts":{"accepted_local":true,"written":r.facts.written,"accepted_native":accepted,"input_recorded":r.facts.input_recorded,
            "input_consumed":consumed,"reply_observed":reply_observed,"reply_final":reply_observed && completed,
            "folded":r.facts.folded,"shared_with_turn":r.facts.shared_with_turn,"interrupt_requested":r.facts.interrupt_requested,
            "lifecycle":r.facts.lifecycle,"held":r.facts.peer_status.iter().any(|s| s["status"]=="held"),
            "peer_status":if peer {json!(r.facts.peer_status)} else {Value::Null},
            "idle_notice":r.facts.idle_notice.is_some()},
        "correlation":correlation,"reply":reply,
        "execution":{"phase":r.phase,"created_at":r.created_at,"dispatched_at":r.dispatched_at,"started_at":r.started_at,
            "finished_at":r.finished_at,"host_epoch":r.host_epoch,"automatic_replay":false},
        "observation":{"state":observation_state,"checked_at":crate::daemon::now(),"evidence_observed_at":r.evidence_at,
            "error_code":r.observation_error}});
    if peer {
        v["peer_message"] = json!({"label":PEER_LABEL,"status":r.facts.peer_status.last().map(|s| s["status"].clone())});
    }
    if r.kind == "terminal" {
        terminal::receipt_fields(r, &mut v);
        if gemini::is_gemini(r) {
            gemini::receipt_fields(r, &mut v);
        }
    }
    v
}

pub async fn receipt(paths: &Paths, id: &str, cancelling: bool) -> Result<Value> {
    if cancelling {
        let cancelled = change(paths, id, |r| {
            if r.phase == "admitted" {
                finish(r, "cancelled", Some("CANCELLED_BEFORE_DELIVERY"));
                return Ok(true);
            }
            Ok(is_terminal(r))
        })?;
        if !cancelled {
            bail!(
                "STEERING_CANCELLATION_UNSUPPORTED: this Claude message was already written to the session; read its receipt (a hosted turn can be interrupted with api hosted stop --interrupt)"
            );
        }
    }
    let r = reconcile(paths, id)?;
    Ok(receipt_value(&r))
}

// ------------------------------------------------------- session capability

/// What a Claude steer does to a running turn (all paths deliver at priority
/// `next`): it never interrupts; Claude Code folds it into the running turn
/// at the turn's next tool round, or runs it as the next turn when the turn
/// ends without one. `steering.interrupt:true` (hosted only) cancels the
/// running turn first; that turn's inputs then end `interrupted`.
pub fn steer_semantics() -> Value {
    json!({"priority":"next","interrupts":false,"delivery":"next_tool_boundary_or_next_turn",
        "interrupt_option":"steering.interrupt","interrupted_inputs_state":"interrupted"})
}
/// Typed input: Claude Code queues text submitted mid-turn as a human message
/// read at the next tool round (`queued_command`), or starts a turn when idle.
pub fn terminal_steer_semantics() -> Value {
    json!({"priority":"next","interrupts":false,"delivery":"next_tool_boundary_or_next_turn",
        "interrupt_option":null,"interrupted_inputs_state":"interrupted"})
}
fn path_label(path: Option<&str>) -> Option<&'static str> {
    match path {
        Some(PEER_SEMANTICS) => Some(PEER_LABEL),
        Some(terminal::SEMANTICS) => Some(terminal::LABEL),
        _ => None,
    }
}

/// Whether terminal input applies to a row: the operator switch is on and the
/// attachment is the verified HERDR pane of its running process (see
/// [`terminal::row_reason`]).
fn terminal_reason(value: &Value, enabled: bool) -> Option<&'static str> {
    if !enabled {
        return Some("TERMINAL_INPUT_DISABLED");
    }
    terminal::row_reason(value)
}

/// Per-session submit/queue/steer support (and the hosted host summary) for a
/// Claude session row. Hosted sessions also report `availability:"running"`.
/// Path choice: hosted, else terminal input (submit/steer), else peer message.
pub fn apply_session_input(paths: &Paths, value: &mut Value, home: Option<&Path>) {
    let Some(c) = value["conversation_id"].as_str().map(str::to_owned) else {
        return;
    };
    let host = hosted::reconcile(paths, &c).ok().flatten();
    let accepting = host
        .as_ref()
        .is_some_and(|h| h.state == "running" && hosted::worker_alive(paths, &c));
    if let Some(h) = &host {
        let mut summary = hosted::public(paths, h);
        summary["pending_permission_count"] = json!(h.pending_permissions.len());
        value["hosted"] = summary;
        if accepting && value["active_since"].is_null() {
            // Worker-recorded start of a live (worker_alive) hosted session.
            value["active_since"] = json!(h.started_at);
        }
        if !matches!(h.state.as_str(), "stopped" | "failed") {
            value["availability"] = json!("running");
            value["resume_block_reason"] = json!("already_running");
            value["capabilities"]["resume"] = json!(false);
        }
    } else {
        value["hosted"] = Value::Null;
    }
    let running = value["availability"] == "running";
    let busy_host = host
        .as_ref()
        .is_some_and(|h| !matches!(h.state.as_str(), "stopped" | "failed"));
    // (mode -> path or refusal reason)
    let mut modes: [(&str, Option<&str>, Option<String>); 3] = [
        ("submit", None, None),
        ("queue", None, None),
        ("steer", None, None),
    ];
    let mut terminal_status: (bool, Option<String>) = (false, None);
    if accepting {
        for m in &mut modes {
            m.1 = Some(hosted::SEMANTICS);
        }
        terminal_status.1 = Some("SESSION_HOSTED".into());
    } else if busy_host || !running {
        let reason = if busy_host {
            "SESSION_OPERATION_IN_PROGRESS: the hosted worker is starting or stopping"
        } else {
            "SESSION_NOT_RUNNING: use mode resume (headless) for a stopped session, or start it hosted"
        };
        for m in &mut modes {
            m.2 = Some(reason.into());
        }
        terminal_status.1 = Some(reason.split(':').next().unwrap_or(reason).into());
    } else {
        let peer: Result<(), String> = if !home.is_some_and(claude::peer::enabled) {
            Err(claude::peer::DISABLED.into())
        } else {
            let endpoint = home.and_then(|home| {
                let pid = value["pid"].as_u64()? as u32;
                let live = claude::live_sessions(home)
                    .ok()?
                    .into_iter()
                    .find(|l| l.pid == pid && l.session_id == c)?;
                Some(claude::peer::endpoint(home, &live))
            });
            match endpoint {
                Some(Ok(_)) => Ok(()),
                Some(Err(e)) if e.to_string().starts_with("PEER_SOCKET_UNLINKED") => {
                    Err(e.to_string())
                }
                _ => Err(claude::peer::NO_ENDPOINT.into()),
            }
        };
        let typed = terminal_reason(value, terminal::enabled(paths));
        terminal_status = (typed.is_none(), typed.map(str::to_owned));
        for m in &mut modes {
            if typed.is_none() && m.0 != "queue" {
                m.1 = Some(terminal::SEMANTICS);
            } else if peer.is_ok() {
                m.1 = Some(PEER_SEMANTICS);
            } else if typed.is_none() {
                m.2 = Some("TERMINAL_QUEUE_UNSUPPORTED: typed input cannot wait for a later turn; enable live steering for peer queue".into());
            } else {
                m.2 = peer.clone().err();
            }
        }
    }
    for (k, path, reason) in &modes {
        value["capabilities"][*k] = json!(path.is_some());
        value["capability_reasons"][*k] = json!(reason);
    }
    // The primary path is the one submit and steer use.
    let path = modes[0].1;
    let mut available: Vec<&str> = vec![];
    if accepting {
        available.push(hosted::SEMANTICS);
    }
    if terminal_status.0 {
        available.push(terminal::SEMANTICS);
    }
    if modes.iter().any(|m| m.1 == Some(PEER_SEMANTICS))
        || (terminal_status.0 && modes[1].1.is_some())
    {
        available.push(PEER_SEMANTICS);
    }
    available.dedup();
    value["input_delivery"] = json!({"semantics":path,"interrupt":accepting,
        "label":path_label(path),
        "steer":path.map(|p| if p == terminal::SEMANTICS {terminal_steer_semantics()} else {steer_semantics()}),
        "modes":{"submit":modes[0].1,"queue":modes[1].1,"steer":modes[2].1},
        "available":available,
        "terminal_input":{"available":terminal_status.0,"reason":terminal_status.1}});
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transcript_scan_needs_the_anchor_and_stops_at_the_next_input() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.jsonl");
        let id = "22222222-2222-4222-8222-222222222222";
        let lines = [
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"before"}]}}),
            json!({"type":"user","isMeta":true,"origin":{"msg_id":id},"message":{"content":"peer text"}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"one"},{"type":"tool_use","name":"Bash"}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"two"}]}}),
            json!({"type":"user","message":{"content":"next input"}}),
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"after"}]}}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        fs::write(&p, &text).unwrap();
        let s = scan(&p, 0, id).unwrap();
        assert!(s.anchor_found && s.turn_ended);
        assert_eq!(s.reply, ["one", "two"]);
        // Anchor absent: nothing is attributed.
        let s = scan(&p, 0, "33333333-3333-4333-8333-333333333333").unwrap();
        assert_eq!(s, Scan::default());
        // An unfinished turn is not ended; an assistant record naming the id is no anchor.
        fs::write(
            &p,
            format!(
                "{}\n{}\n{}\n",
                json!({"type":"assistant","message":{"content":[{"type":"text","text":id}]}}),
                lines[1],
                lines[2]
            ),
        )
        .unwrap();
        let s = scan(&p, 0, id).unwrap();
        assert!(s.anchor_found && !s.turn_ended);
        assert_eq!(s.reply, ["one"]);
    }
    #[test]
    fn requests_accept_only_claude_steering_options() {
        let base = json!({"api_version":1,"message_id":uuid::Uuid::new_v4().to_string(),"mode":"steer",
            "session_id":uuid::Uuid::new_v4().to_string(),"expected_generation":"a".repeat(64),"text":"x","expires_at":1});
        let ok: Request = serde_json::from_value(base.clone()).unwrap();
        assert!(shape(&ok).is_ok());
        let mut v = base.clone();
        v["steering"] = json!({"version":1,"interrupt":true});
        assert!(shape(&serde_json::from_value(v.clone()).unwrap()).is_ok());
        v["mode"] = json!("queue");
        assert!(shape(&serde_json::from_value(v).unwrap()).is_err());
        let mut codex = base.clone();
        codex["steering"] = json!({"version":1,"expected_turn_id":"t"});
        assert!(serde_json::from_value::<Request>(codex).is_err());
        assert_eq!(priority("steer"), "next");
        assert_eq!(priority("submit"), "next");
        assert_eq!(priority("queue"), "later");
    }
}
