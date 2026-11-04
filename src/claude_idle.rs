//! Native idle notices for live Claude sessions (contract 1.40.0).
//!
//! `api idle-notify` asks a live interactive Claude session, over its
//! cross-session inbox socket, for ONE notice when it next goes idle or exits
//! (`notify_when_idle` / `peer_idle_notice`, see
//! [`crate::provider::claude::native_peer`]). The subscription is journaled in
//! `operations/claude-idle-notify/<id>.json` and owned by a detached worker that
//! keeps the reply socket for up to 12 hours (Claude drops the subscription
//! after that). A notice becomes an event in the turn-notify journal
//! (`api turn-events`, kind `idle` or `exited`), optionally also queued as a
//! message to another session. Nothing is replayed; a worker lost to a reboot
//! leaves the record `stalled` and the operator subscribes again. The watched
//! session sends no model request for this; only its own inbound controls apply.
use crate::{
    catalog, daemon,
    paths::{Paths, atomic_write, lock_file, private_dir},
    provider::{
        ProviderId, claude,
        claude::native_peer::{self, Inbound},
    },
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, time::Duration};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub schema_version: u32,
    pub id: String,
    pub session_id: String,
    pub conversation_id: String,
    pub account: String,
    pub name: String,
    pub pid: u32,
    pub process_start: u64,
    pub socket: PathBuf,
    pub notify_msg_id: String,
    pub notify_session: Option<String>,
    /// admitted -> subscribed -> complete
    pub phase: String,
    /// pending_local | waiting | idle_notified | exited | expired | refused_or_unanswered | failed
    pub state: String,
    pub reason: Option<String>,
    pub reply_socket: Option<String>,
    pub event_seq: Option<u64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub expires_at: i64,
}

fn dir(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::CLAUDE_IDLE_NOTIFY)
}
fn file(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_OPERATION_ID")?;
    Ok(dir(paths).join(format!("{id}.json")))
}
fn lock_path(paths: &Paths, id: &str) -> Result<PathBuf> {
    Ok(file(paths, id)?.with_extension("worker.lock"))
}
fn read(paths: &Paths, id: &str) -> Result<Record> {
    serde_json::from_slice(&fs::read(file(paths, id)?).context("OPERATION_NOT_FOUND")?)
        .context("UNREADABLE_OPERATION_RECORD")
}
fn save(paths: &Paths, r: &mut Record) -> Result<()> {
    r.updated_at = daemon::now();
    private_dir(&dir(paths))?;
    atomic_write(&file(paths, &r.id)?, &serde_json::to_vec_pretty(r)?)
}
fn is_terminal(r: &Record) -> bool {
    r.phase == "complete"
}
fn finish(paths: &Paths, r: &mut Record, state: &str, reason: Option<&str>) -> Result<()> {
    r.phase = "complete".into();
    r.state = state.into();
    r.reason = reason.map(str::to_owned);
    save(paths, r)
}
fn all(paths: &Paths) -> Vec<Record> {
    let Ok(entries) = fs::read_dir(dir(paths)) else {
        return vec![];
    };
    let mut records: Vec<Record> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| serde_json::from_slice(&fs::read(p).ok()?).ok())
        .collect();
    records.sort_by_key(|r| (r.created_at, r.id.clone()));
    records
}
fn stalled(paths: &Paths, r: &Record) -> bool {
    !is_terminal(r)
        && daemon::now() - r.updated_at > 10
        && lock_path(paths, &r.id)
            .and_then(|p| crate::background::worker_stopped(&p))
            .unwrap_or(false)
}

pub fn public(paths: &Paths, r: &Record) -> Value {
    json!({"id":r.id,"session_id":r.session_id,"provider":"claude","kind":"idle_notice",
        "phase":r.phase,"state":r.state,"reason":r.reason,"terminal":is_terminal(r),
        "stalled":stalled(paths,r),"notify_session":r.notify_session,"event_seq":r.event_seq,
        "peer_message":{"reply_socket":r.reply_socket,"protocol":{"peer_protocol":native_peer::PEER_PROTOCOL,
            "verified_versions":native_peer::VERIFIED_VERSIONS}},
        "created_at":r.created_at,"updated_at":r.updated_at,"expires_at":r.expires_at,
        "replays":false,
        "next":if is_terminal(r) {"api turn-events --after CURSOR carries the notice"} else if stalled(paths,r) {"the worker stopped: subscribe again"} else {"api turn-events --wait N blocks until the notice arrives"}})
}
pub fn operations(paths: &Paths) -> Vec<Value> {
    all(paths).iter().map(|r| public(paths, r)).collect()
}

/// `api idle-notify`: subscribe to one idle notice of a running Claude session.
/// Idempotent: an active subscription of the same session is returned as is.
pub async fn start(
    paths: &Paths,
    session: &str,
    expected_generation: &str,
    notify_session: Option<String>,
) -> Result<Value> {
    let s = catalog::resolve(paths, session, expected_generation)?;
    ensure!(
        s.provider == ProviderId::Claude.as_str(),
        "UNSUPPORTED_PROVIDER_OPERATION: native idle notices are Claude-only; Codex sessions use turn-notify"
    );
    if let Some(target) = &notify_session {
        let t = catalog::find(paths, target).context("SESSION_NOT_FOUND: --notify-session")?;
        ensure!(
            t.id != s.id,
            "INVALID_ARGUMENT: --notify-session must be another session"
        );
    }
    let account =
        crate::messaging::headless::usable_account(paths, ProviderId::Claude, &s.account)?;
    crate::lifecycle::ensure_available(paths, &s.conversation_id, None)?;
    claude::hosted::ensure_not_hosted(paths, &s.conversation_id)?;
    let observation = claude::observe(&account.home, &s.conversation_id, s.attachment.as_ref());
    ensure!(
        observation.availability == "running",
        "SESSION_NOT_RUNNING: an idle notice needs a live interactive Claude session"
    );
    let live = claude::holder(&claude::live_sessions(&account.home)?, &s.conversation_id)
        .cloned()
        .context("SESSION_NOT_RUNNING")?;
    ensure!(
        claude::native_peer::accepting(&account.home, live.pid),
        "{}",
        claude::peer::DISABLED
    );
    let endpoint = claude::peer::endpoint(&account.home, &live)?;
    private_dir(&dir(paths))?;
    let admission = lock_file(&dir(paths).join("admission.lock"))?;
    admission.lock_exclusive()?;
    if let Some(existing) = all(paths).into_iter().find(|r| {
        !is_terminal(r)
            && r.session_id == s.id
            && r.pid == endpoint.pid
            && r.process_start == endpoint.process_start
            && !stalled(paths, r)
            && r.notify_session == notify_session
    }) {
        return Ok(public(paths, &existing));
    }
    let now = daemon::now();
    let mut r = Record {
        schema_version: 1,
        id: uuid::Uuid::new_v4().to_string(),
        session_id: s.id.clone(),
        conversation_id: s.conversation_id.clone(),
        account: account.name.clone(),
        name: s.name.clone(),
        pid: endpoint.pid,
        process_start: endpoint.process_start,
        socket: endpoint.socket,
        notify_msg_id: uuid::Uuid::new_v4().to_string(),
        notify_session,
        phase: "admitted".into(),
        state: "pending_local".into(),
        reason: None,
        reply_socket: None,
        event_seq: None,
        created_at: now,
        updated_at: now,
        expires_at: now + native_peer::IDLE_NOTICE_EXPIRY_SECONDS,
    };
    save(paths, &mut r)?;
    drop(admission);
    let spawned = crate::background::detach(&mut std::process::Command::new(
        crate::self_executable::path()?,
    ))
    .arg("--data-dir")
    .arg(&paths.root)
    .args(["claude-idle-worker", &r.id])
    .spawn();
    if spawned.is_err() {
        finish(paths, &mut r, "failed", Some("WORKER_COULD_NOT_START"))?;
        return Ok(public(paths, &r));
    }
    for _ in 0..60 {
        r = read(paths, &r.id)?;
        if r.phase != "admitted" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(public(paths, &r))
}

fn alive(r: &Record) -> bool {
    claude::proc_start(r.pid)
        .is_some_and(|(state, start)| start == r.process_start && !matches!(state, 'Z' | 'X'))
}

/// Detached worker: subscribe, wait for the notice (or the process's end, or the
/// 12-hour expiry), journal the event, optionally message another session.
pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let lock = lock_file(&lock_path(paths, id)?)?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(());
    }
    let mut r = read(paths, id)?;
    if r.phase != "admitted" {
        return Ok(());
    }
    if !alive(&r) {
        return finish(paths, &mut r, "failed", Some("STALE_TARGET"));
    }
    let reply = native_peer::bind_reply(&r.socket, &paths.runtime.join("peer"))?;
    r.reply_socket = Some(
        if reply.namespaced {
            "namespaced"
        } else {
            "private"
        }
        .into(),
    );
    let frame = native_peer::notify_idle_frame(&reply.address(), &r.notify_msg_id);
    let bytes = native_peer::encode(&[frame])?;
    let connected = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::UnixStream::connect(&r.socket),
    )
    .await;
    let Ok(Ok(stream)) = connected else {
        return finish(paths, &mut r, "failed", Some("PEER_SOCKET_UNREACHABLE"));
    };
    let (read_half, mut write_half) = stream.into_split();
    if write_half.write_all(&bytes).await.is_err() || write_half.flush().await.is_err() {
        return finish(paths, &mut r, "failed", Some("PEER_WRITE_FAILED"));
    }
    r.phase = "subscribed".into();
    r.state = "waiting".into();
    save(paths, &mut r)?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(16);
    let lines = |reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
                 tx: tokio::sync::mpsc::Sender<Value>| async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.len() <= 64 * 1024
                && let Ok(v) = serde_json::from_str::<Value>(&line)
                && tx.send(v).await.is_err()
            {
                break;
            }
        }
    };
    tokio::spawn(lines(Box::new(read_half), tx.clone()));
    let listener = reply.listener.clone();
    let accept_tx = tx.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            tokio::spawn(lines(Box::new(s), accept_tx.clone()));
        }
    });
    drop(tx);
    let mut outcome: Option<(&str, Value)> = None;
    while outcome.is_none() {
        let remaining = (r.expires_at - daemon::now()).max(0) as u64;
        let step = Duration::from_secs(remaining.min(15));
        tokio::select! {
            v = rx.recv() => {
                if let Some(v) = v
                    && let Inbound::IdleNotice { orig, state, finished_at, detail } = native_peer::classify(&v)
                    && orig == r.notify_msg_id
                {
                    outcome = Some((notice_kind(&state), json!({"state":state,"finished_at":finished_at,"status":detail})));
                }
            }
            _ = tokio::time::sleep(step) => {
                if !alive(&r) {
                    // A notice sent as the process exits may still be in flight.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    while let Ok(v) = rx.try_recv() {
                        if let Inbound::IdleNotice { orig, state, finished_at, detail } = native_peer::classify(&v)
                            && orig == r.notify_msg_id
                        {
                            outcome = Some((notice_kind(&state), json!({"state":state,"finished_at":finished_at,"status":detail})));
                        }
                    }
                    if outcome.is_none() {
                        outcome = Some(("exited", Value::Null));
                    }
                } else if daemon::now() >= r.expires_at {
                    return finish(paths, &mut r, "expired", Some("IDLE_NOTICE_EXPIRED"));
                }
            }
        }
    }
    let (kind, detail) = outcome.expect("loop ends with an outcome");
    let event = crate::turn_notify::record_external(
        paths,
        &r.session_id,
        &r.name,
        kind,
        Some(detail).filter(|d| !d.is_null()),
    )?;
    r.event_seq = Some(event.seq);
    finish(
        paths,
        &mut r,
        if kind == "idle" {
            "idle_notified"
        } else {
            "exited"
        },
        None,
    )?;
    if let Some(target) = r.notify_session.clone() {
        crate::turn_notify::deliver(paths, &event, &target).await;
    }
    drop(reply);
    Ok(())
}

pub fn capability() -> Value {
    json!({"since_contract":"1.40.0","version":1,"command":"idle-notify",
        "args":["--session","--expected-generation","--notify-session"],
        "providers":["claude"],"requires":"live interactive session with inbound peer messages accepted",
        "transport":"claude_cross_session_notify_when_idle","expiry_seconds":native_peer::IDLE_NOTICE_EXPIRY_SECONDS,
        "events":"api turn-events (kind idle|exited, detail:{state,finished_at,status})",
        "journal":"operations/claude-idle-notify","replays":false,"model_requests":false,
        "refusal_codes":["UNSUPPORTED_PROVIDER_OPERATION","SESSION_NOT_RUNNING","LIVE_STEERING_DISABLED","PEER_ENDPOINT_UNAVAILABLE","PEER_SOCKET_UNLINKED","STALE_TARGET"]})
}

/// Event kind of a received notice: a notice reporting `state:"exited"` is the session's exit
/// (`exited`, as the contract says), any other state is `idle`.
fn notice_kind(state: &str) -> &'static str {
    if state == "exited" { "exited" } else { "idle" }
}

#[cfg(test)]
mod kind_tests {
    use super::notice_kind;

    #[test]
    fn an_exit_notice_is_an_exited_event() {
        assert_eq!(notice_kind("exited"), "exited");
        assert_eq!(notice_kind("idle"), "idle");
        assert_eq!(notice_kind("anything-else"), "idle");
    }
}
